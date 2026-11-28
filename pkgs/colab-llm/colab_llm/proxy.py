"""Loopback activity-tracking TCP proxy, SSH tunnel supervisor, and CU watchdog."""

from __future__ import annotations

import asyncio
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from colab_llm.colab import (
    build_proxy_command,
    build_ssh_mux_args,
    build_ssh_tunnel_args,
)
from colab_llm.config import (
    HEARTBEAT_INTERVAL_S,
    StopReason,
    deadman_heartbeat_timeout_s,
    format_duration,
    validate_loopback_host,
    validate_port,
)

# Loopback clients are few (one harness, a handful of pooled sockets); the cap
# exists so a runaway client cannot grow the task set without bound.
CLIENT_CONNECTIONS_MAX = 64
PIPE_CHUNK_SIZE_BYTES = 65536
UPSTREAM_CONNECT_TIMEOUT_S = 15.0
PROXY_CLOSE_TIMEOUT_S = 5.0
WRITER_CLOSE_TIMEOUT_S = 5.0

TUNNEL_BACKOFF_S_MIN = 2.0
TUNNEL_BACKOFF_S_MAX = 15.0
# A tunnel that stayed up this long was a working tunnel, so the next drop
# starts a fresh failure streak.
TUNNEL_STABLE_S = 60.0
# About five minutes of back-to-back failures at maximum backoff: the runtime is
# gone or unreachable, so stop supervising and let the session tear down.
TUNNEL_FAILURES_CONSECUTIVE_MAX = 20

HEALTH_PROBE_INTERVAL_S = 3.0
HEALTH_LOG_INTERVAL_S = 15.0
WATCHDOG_TICK_S = 5.0
STATE_WRITE_INTERVAL_S = 10.0
STATUS_LOG_INTERVAL_S = 300.0


def pick_free_loopback_port() -> int:
    """Allocate an ephemeral unused TCP port on 127.0.0.1."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        port = int(sock.getsockname()[1])
    assert 0 < port <= 65535
    return port


def write_json_atomic(path: Path, payload: dict[str, Any]) -> None:
    """Atomically write JSON with owner-only (0600) permissions."""
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, tmp_name = tempfile.mkstemp(
        prefix=f".{path.name}.",
        suffix=".tmp",
        dir=str(path.parent),
    )
    tmp_path = Path(tmp_name)
    try:
        os.chmod(tmp_path, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, indent=2, sort_keys=True)
            fh.write("\n")
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp_path, path)
    except Exception:
        tmp_path.unlink(missing_ok=True)
        raise


def append_jsonl(path: Path, record: dict[str, Any]) -> None:
    """Append a single JSON record to an owner-only JSONL ledger."""
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    flags = os.O_WRONLY | os.O_CREAT | os.O_APPEND
    fd = os.open(path, flags, 0o600)
    try:
        os.chmod(path, 0o600)
    except OSError:
        pass
    with os.fdopen(fd, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(record, sort_keys=True) + "\n")
        fh.flush()


def read_jsonl_tail(path: Path, limit: int = 10) -> list[dict[str, Any]]:
    """Return up to `limit` of the most recent records, oldest first."""
    assert limit >= 0, limit
    # `lines[-0:]` is the whole list, so a zero count needs its own branch.
    if limit == 0 or not path.is_file():
        return []
    lines = path.read_text(encoding="utf-8").splitlines()
    records: list[dict[str, Any]] = []
    for line in lines[-limit:]:
        line = line.strip()
        if not line:
            continue
        try:
            parsed = json.loads(line)
            if isinstance(parsed, dict):
                records.append(parsed)
        except ValueError:
            continue
    assert len(records) <= limit
    return records


def probe_http_health(
    *,
    port: int,
    health_path: str = "/health",
    timeout_s: float = 3.0,
) -> tuple[bool, dict[str, Any] | None]:
    """Query the remote inference server through the internal loopback tunnel port."""
    url = f"http://127.0.0.1:{port}{health_path}"
    req = urllib.request.Request(url, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            if resp.status != 200:
                return False, None
            raw = resp.read().decode("utf-8", errors="replace").strip()
            if not raw:
                return True, {}
            try:
                data = json.loads(raw)
                if isinstance(data, dict):
                    status = data.get("status")
                    if status in ("loading", "reloading", "updating"):
                        return False, data
                    if data.get("unloaded") is True:
                        return False, data
                    return True, data
            except ValueError:
                return True, {"raw": raw[:200]}
    except (urllib.error.URLError, TimeoutError, OSError):
        return False, None
    return False, None


@dataclass
class ProxyStats:
    started_monotonic: float = field(default_factory=time.monotonic)
    last_activity_monotonic: float = field(default_factory=time.monotonic)
    active_connections: int = 0
    total_connections: int = 0
    bytes_in: int = 0
    bytes_out: int = 0

    def idle_s(self, now: float | None = None) -> float:
        """Seconds since a byte last moved or a connection last opened or closed.

        An open connection is not activity by itself: a client that holds a
        silent socket must not keep a billable runtime alive.
        """
        current = time.monotonic() if now is None else now
        # Callers sample `now` before awaiting, so activity can be newer than it.
        return max(0.0, current - self.last_activity_monotonic)

    def elapsed_s(self, now: float | None = None) -> float:
        current = time.monotonic() if now is None else now
        return max(0.0, current - self.started_monotonic)

    def is_idle_expired(self, idle_timeout_s: int, now: float | None = None) -> bool:
        assert idle_timeout_s > 0
        return self.idle_s(now) >= idle_timeout_s

    def is_hard_cap_expired(self, max_duration_s: int, now: float | None = None) -> bool:
        assert max_duration_s > 0
        return self.elapsed_s(now) >= max_duration_s


class ActivityTrackingProxy:
    """Asyncio loopback TCP relay tracking genuine client traffic for idle teardown.

    Health probes hit the internal SSH tunnel port directly so they never reset
    the idle timer; only bytes moving through the user-facing loopback port
    count as activity.
    """

    def __init__(
        self,
        *,
        bind_host: str,
        listen_port: int,
        upstream_port: int,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self.bind_host = validate_loopback_host(bind_host)
        self.listen_port = validate_port(listen_port, "listen_port")
        self.upstream_port = validate_port(upstream_port, "upstream_port")
        assert self.listen_port != self.upstream_port, "proxy would connect to itself"
        self._clock = clock
        now = clock()
        self.stats = ProxyStats(started_monotonic=now, last_activity_monotonic=now)
        self._server: asyncio.Server | None = None
        self._handlers: set[asyncio.Task[Any]] = set()
        self._closing = False

    @property
    def bound_port(self) -> int:
        if self._server and self._server.sockets:
            return int(self._server.sockets[0].getsockname()[1])
        return self.listen_port

    async def start(self) -> None:
        assert self._server is None, "proxy already started"
        self._closing = False
        self._server = await asyncio.start_server(
            self._handle_client,
            host=self.bind_host,
            port=self.listen_port,
        )

    async def close(self) -> None:
        """Stop accepting, drop every client, and return within a bounded time.

        Teardown of the billable runtime waits on this call, so it must never
        depend on a client choosing to disconnect: `Server.wait_closed()` blocks
        until every connection is gone, hence connections are cancelled first.
        """
        self._closing = True
        server = self._server
        self._server = None
        if server is not None:
            server.close()

        handlers = list(self._handlers)
        for handler in handlers:
            handler.cancel()
        if handlers:
            await asyncio.gather(*handlers, return_exceptions=True)
        assert not self._handlers
        assert self.stats.active_connections == 0, self.stats.active_connections

        if server is not None:
            try:
                await asyncio.wait_for(server.wait_closed(), timeout=PROXY_CLOSE_TIMEOUT_S)
            except TimeoutError:
                print(
                    "[colab-llm] Warning: proxy listener did not close within "
                    f"{PROXY_CLOSE_TIMEOUT_S:.0f}s; continuing teardown.",
                    file=sys.stderr,
                )

    async def _pipe(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
        *,
        inbound: bool,
    ) -> None:
        # Runs until EOF, a transport error, or cancellation. `close()` cancels
        # every connection, so the session's hard cap bounds this loop.
        try:
            while True:
                data = await reader.read(PIPE_CHUNK_SIZE_BYTES)
                if not data:
                    break
                assert len(data) <= PIPE_CHUNK_SIZE_BYTES
                writer.write(data)
                await writer.drain()
                self.stats.last_activity_monotonic = self._clock()
                if inbound:
                    self.stats.bytes_in += len(data)
                else:
                    self.stats.bytes_out += len(data)
        except (ConnectionError, OSError):
            # The peer went away mid-stream; the other direction sees EOF below.
            pass
        finally:
            try:
                if writer.can_write_eof():
                    writer.write_eof()
            except (OSError, RuntimeError):
                pass

    async def _close_writer(self, writer: asyncio.StreamWriter) -> None:
        if self._closing:
            # Shutdown must not wait on a peer that has stopped reading.
            writer.transport.abort()
            return
        writer.close()
        try:
            await asyncio.wait_for(writer.wait_closed(), timeout=WRITER_CLOSE_TIMEOUT_S)
        except (TimeoutError, OSError):
            writer.transport.abort()

    async def _handle_client(
        self,
        client_reader: asyncio.StreamReader,
        client_writer: asyncio.StreamWriter,
    ) -> None:
        handler = asyncio.current_task()
        assert handler is not None
        if self._closing:
            client_writer.transport.abort()
            return
        if self.stats.active_connections < CLIENT_CONNECTIONS_MAX:
            await self._relay_client(handler, client_reader, client_writer)
        else:
            client_writer.transport.abort()

    async def _relay_client(
        self,
        handler: asyncio.Task[Any],
        client_reader: asyncio.StreamReader,
        client_writer: asyncio.StreamWriter,
    ) -> None:
        self._handlers.add(handler)
        self.stats.active_connections += 1
        self.stats.total_connections += 1
        self.stats.last_activity_monotonic = self._clock()
        assert self.stats.active_connections <= CLIENT_CONNECTIONS_MAX

        upstream_writer: asyncio.StreamWriter | None = None
        pipes: list[asyncio.Task[None]] = []
        try:
            upstream_reader, upstream_writer = await asyncio.wait_for(
                asyncio.open_connection("127.0.0.1", self.upstream_port),
                timeout=UPSTREAM_CONNECT_TIMEOUT_S,
            )
            pipes = [
                asyncio.create_task(self._pipe(client_reader, upstream_writer, inbound=True)),
                asyncio.create_task(self._pipe(upstream_reader, client_writer, inbound=False)),
            ]
            await asyncio.gather(*pipes)
        except (TimeoutError, OSError):
            # Upstream (the SSH tunnel) is unreachable: drop this client. The
            # tunnel supervisor reports and repairs the tunnel itself.
            pass
        finally:
            for pipe in pipes:
                pipe.cancel()
            if pipes:
                await asyncio.gather(*pipes, return_exceptions=True)
            assert self.stats.active_connections > 0
            self.stats.active_connections -= 1
            self.stats.last_activity_monotonic = self._clock()
            self._handlers.discard(handler)
            if upstream_writer is not None:
                await self._close_writer(upstream_writer)
            await self._close_writer(client_writer)


class TunnelSupervisor:
    """Supervises the OpenSSH local-forward tunnel over `colab ssh --proxy-mode`."""

    def __init__(
        self,
        *,
        colab_bin: str,
        ssh_bin: str,
        session: str,
        tunnel_port: int,
        remote_port: int,
        identity: Path,
        control_path: Path,
        rm_on_disconnect: bool = False,
        colab_config: Path | None = None,
    ) -> None:
        self.colab_bin = colab_bin
        self.ssh_bin = ssh_bin
        self.session = session
        self.tunnel_port = tunnel_port
        self.remote_port = remote_port
        self.identity = identity
        self.control_path = control_path
        self.rm_on_disconnect = rm_on_disconnect
        self.colab_config = colab_config
        self._proc: asyncio.subprocess.Process | None = None
        self._stopping = False
        self._task: asyncio.Task[None] | None = None
        self.reconnect_count = 0
        # Set when the supervisor stops trying; the session must then end.
        self.gave_up = False

    def _build_cmd(self, *, force: bool = False) -> list[str]:
        proxy_cmd = build_proxy_command(
            colab_bin=self.colab_bin,
            session=self.session,
            identity=self.identity,
            rm_on_disconnect=self.rm_on_disconnect,
            force=force,
            colab_config=self.colab_config,
        )
        return build_ssh_tunnel_args(
            ssh_bin=self.ssh_bin,
            proxy_command=proxy_cmd,
            bind_host="127.0.0.1",
            local_forward_port=self.tunnel_port,
            remote_port=self.remote_port,
            identity=self.identity,
            control_path=self.control_path,
        )

    async def start(self) -> None:
        assert self._task is None, "tunnel supervisor already started"
        self._stopping = False
        self.gave_up = False
        self._task = asyncio.create_task(self._supervise_loop())

    def _remove_control_socket(self) -> None:
        # A stale socket would make the next `ssh -M` refuse to start.
        self.control_path.unlink(missing_ok=True)

    async def _supervise_loop(self) -> None:
        backoff_s = TUNNEL_BACKOFF_S_MIN
        failures_consecutive = 0
        # Bounded by TUNNEL_FAILURES_CONSECUTIVE_MAX failures in a row; a tunnel
        # that keeps working runs until `stop()`, which the hard cap guarantees.
        while not self._stopping:
            self._remove_control_socket()
            # Pass --force so a just-closed bootstrap SSH session slot on Colab
            # (which enforces a 1-session limit on some tiers) never causes a
            # spurious HTTP 429 when bringing up the ControlMaster tunnel.
            cmd = self._build_cmd(force=True)
            started = time.monotonic()
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.DEVNULL,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            _, stderr_bytes = await self._proc.communicate()
            exit_code = self._proc.returncode
            self._proc = None
            if self._stopping:
                break
            err_msg = (stderr_bytes or b"").decode("utf-8", errors="replace").strip()
            if self.rm_on_disconnect:
                print(
                    f"[colab-llm] SSH tunnel exited (rc={exit_code}) "
                    "with --rm-on-disconnect enabled.",
                    file=sys.stderr,
                )
                self.gave_up = True
                break

            if time.monotonic() - started >= TUNNEL_STABLE_S:
                failures_consecutive = 0
                backoff_s = TUNNEL_BACKOFF_S_MIN
            failures_consecutive += 1
            if failures_consecutive > TUNNEL_FAILURES_CONSECUTIVE_MAX:
                print(
                    f"[colab-llm] SSH tunnel failed {failures_consecutive} times in a row "
                    f"(rc={exit_code}); giving up" + (f": {err_msg}" if err_msg else "."),
                    file=sys.stderr,
                )
                self.gave_up = True
                break

            self.reconnect_count += 1
            print(
                f"[colab-llm] SSH tunnel dropped (rc={exit_code}); "
                f"reconnecting in {backoff_s:.0f}s" + (f": {err_msg}" if err_msg else ""),
                file=sys.stderr,
            )
            await asyncio.sleep(backoff_s)
            backoff_s = min(backoff_s * 2.0, TUNNEL_BACKOFF_S_MAX)
        assert self._stopping or self.gave_up

    def touch_remote_heartbeat(self) -> bool:
        """Touch the remote deadman heartbeat over the live ControlMaster socket.

        Returns whether the VM acknowledged it. A heartbeat that is not
        delivered brings the remote deadman closer to firing, so the caller
        must be told.
        """
        if not self.control_path.exists():
            return False
        cmd = build_ssh_mux_args(
            ssh_bin=self.ssh_bin, control_path=self.control_path, connect_timeout_s=10
        )
        cmd.append("date +%s > /content/colab-llm/heartbeat")
        try:
            proc = subprocess.run(
                cmd,
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=15,
            )
        except (OSError, subprocess.TimeoutExpired):
            return False
        return proc.returncode == 0

    @property
    def is_running(self) -> bool:
        return self._proc is not None and self._proc.returncode is None

    async def stop(self) -> None:
        self._stopping = True
        if self._proc is not None and self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=5.0)
            except TimeoutError:
                self._proc.kill()
                await self._proc.wait()
        if self._task is not None:
            self._task.cancel()
            await asyncio.gather(self._task, return_exceptions=True)
            self._task = None
        self._remove_control_socket()


async def run_supervised_session(
    *,
    session: str,
    engine: str,
    model: str,
    gpu: str,
    bind_host: str,
    local_port: int,
    remote_port: int,
    health_path: str,
    idle_timeout_s: int,
    max_duration_s: int,
    health_timeout_s: int,
    state_file: Path,
    control_path: Path,
    colab_bin: str,
    ssh_bin: str,
    identity: Path,
    rm_on_disconnect: bool = False,
    colab_config: Path | None = None,
    on_ready: Callable[[dict[str, Any]], None] | None = None,
    check_remote_alive: Callable[[], bool] | None = None,
) -> tuple[StopReason, ProxyStats]:
    """Run the SSH tunnel, loopback activity proxy, health check, and idle/cap watchdog.

    Returns (stop_reason, final_stats). Caller is responsible for executing
    `colab stop -s <session>` after this coroutine returns.
    """
    assert 0 < idle_timeout_s <= max_duration_s
    assert health_timeout_s > 0
    assert health_path.startswith("/")

    tunnel_port = pick_free_loopback_port()
    assert tunnel_port != local_port
    tunnel = TunnelSupervisor(
        colab_bin=colab_bin,
        ssh_bin=ssh_bin,
        session=session,
        tunnel_port=tunnel_port,
        remote_port=remote_port,
        identity=identity,
        control_path=control_path,
        rm_on_disconnect=rm_on_disconnect,
        colab_config=colab_config,
    )
    proxy = ActivityTrackingProxy(
        bind_host=bind_host,
        listen_port=local_port,
        upstream_port=tunnel_port,
    )

    stop_event = asyncio.Event()
    # Stays ERROR unless a branch below names the reason, so an exception that
    # escapes this coroutine is recorded as one.
    stop_reason = StopReason.ERROR

    def _request_stop(reason: StopReason) -> None:
        nonlocal stop_reason
        if not stop_event.is_set():
            stop_reason = reason
            stop_event.set()

    loop = asyncio.get_running_loop()
    installed_signals: list[signal.Signals] = []
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        try:
            loop.add_signal_handler(sig, _request_stop, StopReason(f"signal:{sig.name}"))
            installed_signals.append(sig)
        except NotImplementedError:
            # Platform without loop signal support: the caller's handlers remain.
            pass

    def _write_state(phase: str, extra: dict[str, Any] | None = None) -> None:
        now = time.monotonic()
        payload: dict[str, Any] = {
            "session": session,
            "pid": os.getpid(),
            "phase": phase,
            "engine": engine,
            "model": model,
            "gpu": gpu,
            "bind_host": bind_host,
            "local_port": local_port,
            "remote_port": remote_port,
            "tunnel_port": tunnel_port,
            "control_path": str(control_path),
            "idle_timeout_s": idle_timeout_s,
            "max_duration_s": max_duration_s,
            "idle_s": round(proxy.stats.idle_s(now), 1),
            "elapsed_s": round(proxy.stats.elapsed_s(now), 1),
            "active_connections": proxy.stats.active_connections,
            "total_connections": proxy.stats.total_connections,
            "bytes_in": proxy.stats.bytes_in,
            "bytes_out": proxy.stats.bytes_out,
            "updated_at": datetime.now(UTC).isoformat(),
        }
        if extra:
            payload.update(extra)
        write_json_atomic(state_file, payload)

    heartbeat_last = float("-inf")
    heartbeat_failures_consecutive = 0
    heartbeat_timeout_s = deadman_heartbeat_timeout_s(idle_timeout_s)

    async def _heartbeat_if_due(now: float, *, tunnel_expected: bool) -> None:
        nonlocal heartbeat_last, heartbeat_failures_consecutive
        if now - heartbeat_last < HEARTBEAT_INTERVAL_S:
            return
        if not tunnel_expected and not control_path.exists():
            # The tunnel is still coming up; try again on the next tick.
            return
        heartbeat_last = now
        if await asyncio.to_thread(tunnel.touch_remote_heartbeat):
            heartbeat_failures_consecutive = 0
            return
        heartbeat_failures_consecutive += 1
        print(
            f"[colab-llm] Warning: heartbeat not delivered to the VM "
            f"({heartbeat_failures_consecutive} in a row). The remote deadman stops the "
            f"runtime after {format_duration(heartbeat_timeout_s)} without one.",
            file=sys.stderr,
        )

    await tunnel.start()
    await proxy.start()
    _write_state("waiting_health")

    try:
        # Phase 1: wait for inference engine health through the internal tunnel port.
        # Bounded by `deadline`.
        deadline = time.monotonic() + health_timeout_s
        last_log = time.monotonic()
        health_info: dict[str, Any] = {}
        ready = False
        while not stop_event.is_set():
            ok, data = await asyncio.to_thread(
                probe_http_health,
                port=tunnel_port,
                health_path=health_path,
                timeout_s=HEALTH_PROBE_INTERVAL_S,
            )
            if ok:
                ready = True
                health_info = data or {}
                break
            now = time.monotonic()
            if now >= deadline:
                _request_stop(StopReason.HEALTH_TIMEOUT)
                break
            if tunnel.gave_up:
                _request_stop(StopReason.TUNNEL_DISCONNECTED)
                break
            await _heartbeat_if_due(now, tunnel_expected=False)
            if now - last_log >= HEALTH_LOG_INTERVAL_S:
                remaining_s = max(0.0, deadline - now)
                status_hint = ""
                if data and data.get("status"):
                    status_hint = f" (engine status: {data['status']})"
                print(
                    f"[colab-llm] Waiting for {engine} on 127.0.0.1:{remote_port}{health_path}"
                    f"{status_hint} — {format_duration(remaining_s)} remaining...",
                    flush=True,
                )
                last_log = now
                if check_remote_alive is not None and control_path.exists():
                    alive = await asyncio.to_thread(check_remote_alive)
                    if not alive:
                        _request_stop(StopReason.REMOTE_PROCESS_EXITED)
                        break
            try:
                await asyncio.wait_for(stop_event.wait(), timeout=HEALTH_PROBE_INTERVAL_S)
            except TimeoutError:
                pass  # The timeout is the poll interval, not an error.

        if not ready:
            assert stop_event.is_set()
            return stop_reason, proxy.stats

        # Reset idle timer at the moment the endpoint becomes ready so long model
        # loads never count as idle time.
        proxy.stats.last_activity_monotonic = time.monotonic()
        _write_state("ready", {"health": health_info})
        if on_ready is not None:
            on_ready(health_info)

        # Phase 2: serve traffic and enforce idle timeout + hard max duration.
        # Bounded by the hard cap, which is checked first on every tick.
        last_state_write = 0.0
        last_status_log = time.monotonic()
        while not stop_event.is_set():
            now = time.monotonic()
            if proxy.stats.is_hard_cap_expired(max_duration_s, now):
                print(
                    f"[colab-llm] Hard session cap reached "
                    f"({format_duration(max_duration_s)}). Stopping Colab runtime."
                )
                _request_stop(StopReason.MAX_DURATION)
                break
            if proxy.stats.is_idle_expired(idle_timeout_s, now):
                print(
                    f"[colab-llm] Idle timeout reached "
                    f"({format_duration(idle_timeout_s)} with no traffic). "
                    "Stopping Colab runtime."
                )
                _request_stop(StopReason.IDLE_TIMEOUT)
                break
            if tunnel.gave_up:
                _request_stop(StopReason.TUNNEL_DISCONNECTED)
                break

            if now - last_state_write >= STATE_WRITE_INTERVAL_S:
                _write_state("ready", {"health": health_info})
                last_state_write = now

            await _heartbeat_if_due(now, tunnel_expected=True)

            if now - last_status_log >= STATUS_LOG_INTERVAL_S:
                idle_left_s = max(0.0, idle_timeout_s - proxy.stats.idle_s(now))
                cap_left_s = max(0.0, max_duration_s - proxy.stats.elapsed_s(now))
                print(
                    f"[colab-llm] Alive ({session}): "
                    f"active={proxy.stats.active_connections}, "
                    f"connections={proxy.stats.total_connections}, "
                    f"idle_left={format_duration(idle_left_s)}, "
                    f"cap_left={format_duration(cap_left_s)}"
                )
                last_status_log = now

            try:
                await asyncio.wait_for(stop_event.wait(), timeout=WATCHDOG_TICK_S)
            except TimeoutError:
                pass  # The timeout is the tick interval, not an error.

        assert stop_event.is_set()
        return stop_reason, proxy.stats
    finally:
        # Each step is bounded; none may prevent the caller's `colab stop`. The
        # signal handlers stay installed meanwhile: a repeated Ctrl+C is then an
        # idempotent stop request instead of an interruption of the cleanup.
        try:
            await proxy.close()
        finally:
            try:
                await tunnel.stop()
            finally:
                for sig in installed_signals:
                    try:
                        loop.remove_signal_handler(sig)
                    except (NotImplementedError, RuntimeError):
                        pass
                _write_state("stopped", {"stop_reason": stop_reason})
