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

from colab_llm.colab import build_proxy_command, build_ssh_tunnel_args
from colab_llm.config import format_duration, validate_loopback_host, validate_port


def pick_free_loopback_port() -> int:
    """Allocate an ephemeral unused TCP port on 127.0.0.1."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


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
    if not path.is_file():
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

    def idle_seconds(self, now: float | None = None) -> float:
        current = time.monotonic() if now is None else now
        if self.active_connections > 0:
            return 0.0
        return max(0.0, current - self.last_activity_monotonic)

    def elapsed_seconds(self, now: float | None = None) -> float:
        current = time.monotonic() if now is None else now
        return max(0.0, current - self.started_monotonic)

    def is_idle_expired(self, idle_timeout_s: int, now: float | None = None) -> bool:
        return self.active_connections == 0 and self.idle_seconds(now) >= idle_timeout_s

    def is_hard_cap_expired(self, max_duration_s: int, now: float | None = None) -> bool:
        return self.elapsed_seconds(now) >= max_duration_s


class ActivityTrackingProxy:
    """Asyncio loopback TCP relay tracking genuine client traffic for idle teardown.

    Health probes hit the internal SSH tunnel port directly so they never reset
    the idle timer; only real client connections to the user-facing loopback
    port count as activity, and in-flight streams pause idle expiration.
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
        self._clock = clock
        now = clock()
        self.stats = ProxyStats(started_monotonic=now, last_activity_monotonic=now)
        self._server: asyncio.Server | None = None
        self._tasks: set[asyncio.Task[Any]] = set()

    @property
    def bound_port(self) -> int:
        if self._server and self._server.sockets:
            return int(self._server.sockets[0].getsockname()[1])
        return self.listen_port

    async def start(self) -> None:
        self._server = await asyncio.start_server(
            self._handle_client,
            host=self.bind_host,
            port=self.listen_port,
        )

    async def close(self) -> None:
        if self._server is not None:
            self._server.close()
            await self._server.wait_closed()
            self._server = None
        for task in list(self._tasks):
            task.cancel()
        if self._tasks:
            await asyncio.gather(*self._tasks, return_exceptions=True)
            self._tasks.clear()

    async def _pipe(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
        *,
        inbound: bool,
    ) -> None:
        try:
            while True:
                data = await reader.read(65536)
                if not data:
                    break
                writer.write(data)
                await writer.drain()
                self.stats.last_activity_monotonic = self._clock()
                if inbound:
                    self.stats.bytes_in += len(data)
                else:
                    self.stats.bytes_out += len(data)
        except (ConnectionError, OSError, asyncio.CancelledError):
            pass
        finally:
            try:
                if writer.can_write_eof():
                    writer.write_eof()
            except OSError:
                pass

    async def _handle_client(
        self,
        client_reader: asyncio.StreamReader,
        client_writer: asyncio.StreamWriter,
    ) -> None:
        self.stats.active_connections += 1
        self.stats.total_connections += 1
        self.stats.last_activity_monotonic = self._clock()
        upstream_writer: asyncio.StreamWriter | None = None
        try:
            upstream_reader, upstream_writer = await asyncio.wait_for(
                asyncio.open_connection("127.0.0.1", self.upstream_port),
                timeout=15.0,
            )
            t_in = asyncio.create_task(
                self._pipe(client_reader, upstream_writer, inbound=True)
            )
            t_out = asyncio.create_task(
                self._pipe(upstream_reader, client_writer, inbound=False)
            )
            self._tasks.update({t_in, t_out})
            try:
                await asyncio.gather(t_in, t_out, return_exceptions=True)
            finally:
                self._tasks.discard(t_in)
                self._tasks.discard(t_out)
        except (TimeoutError, OSError, asyncio.CancelledError):
            pass
        finally:
            self.stats.active_connections = max(0, self.stats.active_connections - 1)
            self.stats.last_activity_monotonic = self._clock()
            for w in (upstream_writer, client_writer):
                if w is not None:
                    try:
                        w.close()
                        await w.wait_closed()
                    except OSError:
                        pass


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
        self._stopping = False
        self._task = asyncio.create_task(self._supervise_loop())

    async def _supervise_loop(self) -> None:
        backoff_s = 2.0
        while not self._stopping:
            if self.control_path.exists():
                try:
                    self.control_path.unlink()
                except OSError:
                    pass
            # Pass --force so a just-closed bootstrap SSH session slot on Colab
            # (which enforces a 1-session limit on some tiers) never causes a
            # spurious HTTP 429 when bringing up the ControlMaster tunnel.
            cmd = self._build_cmd(force=True)
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.DEVNULL,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            _, stderr_bytes = await self._proc.communicate()
            rc = self._proc.returncode
            self._proc = None
            if self._stopping:
                break
            err_msg = (stderr_bytes or b"").decode("utf-8", errors="replace").strip()
            if self.rm_on_disconnect:
                print(
                    f"[colab-llm] SSH tunnel exited (rc={rc}) with --rm-on-disconnect enabled.",
                    file=sys.stderr,
                )
                break
            self.reconnect_count += 1
            print(
                f"[colab-llm] SSH tunnel dropped (rc={rc}); reconnecting in {backoff_s:.0f}s"
                + (f": {err_msg}" if err_msg else ""),
                file=sys.stderr,
            )
            await asyncio.sleep(backoff_s)
            backoff_s = min(backoff_s * 2.0, 15.0)

    def touch_remote_heartbeat(self) -> None:
        """Touch the remote deadman heartbeat over the active ControlMaster socket."""
        if not self.control_path.exists():
            return
        try:
            subprocess.run(
                [
                    self.ssh_bin,
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "LogLevel=ERROR",
                    "-o",
                    "ConnectTimeout=10",
                    "-S",
                    str(self.control_path),
                    "root@colab-runtime",
                    "date +%s > /content/colab-llm/heartbeat",
                ],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=15,
            )
        except (OSError, subprocess.TimeoutExpired):
            pass

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
        if self.control_path.exists():
            try:
                self.control_path.unlink()
            except OSError:
                pass


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
    startup_timeout_s: int,
    state_file: Path,
    control_path: Path,
    colab_bin: str,
    ssh_bin: str,
    identity: Path,
    rm_on_disconnect: bool = False,
    colab_config: Path | None = None,
    on_ready: Callable[[dict[str, Any]], None] | None = None,
    check_remote_alive: Callable[[], bool] | None = None,
) -> tuple[str, ProxyStats]:
    """Run the SSH tunnel, loopback activity proxy, health check, and idle/cap watchdog.

    Returns (stop_reason, final_stats). Caller is responsible for executing
    `colab stop -s <session>` after this coroutine returns.
    """
    tunnel_port = pick_free_loopback_port()
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
    stop_reason = "signal"

    def _request_stop(reason: str) -> None:
        nonlocal stop_reason
        if not stop_event.is_set():
            stop_reason = reason
            stop_event.set()

    loop = asyncio.get_running_loop()
    installed_signals: list[signal.Signals] = []
    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, _request_stop, f"signal:{sig.name}")
            installed_signals.append(sig)
        except NotImplementedError:
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
            "idle_seconds": round(proxy.stats.idle_seconds(now), 1),
            "elapsed_seconds": round(proxy.stats.elapsed_seconds(now), 1),
            "active_connections": proxy.stats.active_connections,
            "total_connections": proxy.stats.total_connections,
            "bytes_in": proxy.stats.bytes_in,
            "bytes_out": proxy.stats.bytes_out,
            "updated_at": datetime.now(UTC).isoformat(),
        }
        if extra:
            payload.update(extra)
        write_json_atomic(state_file, payload)

    await tunnel.start()
    await proxy.start()
    _write_state("waiting_health")

    try:
        # Phase 1: wait for inference engine health through the internal tunnel port.
        deadline = time.monotonic() + startup_timeout_s
        last_log = time.monotonic()
        health_info: dict[str, Any] = {}
        ready = False
        while not stop_event.is_set():
            ok, data = await asyncio.to_thread(
                probe_http_health,
                port=tunnel_port,
                health_path=health_path,
                timeout_s=3.0,
            )
            if ok:
                ready = True
                health_info = data or {}
                break
            now = time.monotonic()
            if now >= deadline:
                _request_stop("startup_timeout")
                break
            if now - last_log >= 15.0:
                rem = max(0.0, deadline - now)
                status_hint = ""
                if data and data.get("status"):
                    status_hint = f" (engine status: {data['status']})"
                print(
                    f"[colab-llm] Waiting for {engine} on 127.0.0.1:{remote_port}{health_path}"
                    f"{status_hint} — {format_duration(rem)} remaining...",
                    flush=True,
                )
                last_log = now
                if check_remote_alive is not None and control_path.exists():
                    alive = await asyncio.to_thread(check_remote_alive)
                    if not alive:
                        _request_stop("remote_process_exited")
                        break
            try:
                await asyncio.wait_for(stop_event.wait(), timeout=3.0)
            except TimeoutError:
                pass

        if not ready:
            return stop_reason, proxy.stats

        # Reset idle timer at the moment the endpoint becomes ready so long model
        # loads never count as idle time.
        proxy.stats.last_activity_monotonic = time.monotonic()
        _write_state("ready", {"health": health_info})
        if on_ready is not None:
            on_ready(health_info)

        # Phase 2: serve traffic and enforce idle timeout + hard max duration.
        last_state_write = 0.0
        last_heartbeat = 0.0
        last_status_log = time.monotonic()
        while not stop_event.is_set():
            now = time.monotonic()
            if proxy.stats.is_hard_cap_expired(max_duration_s, now):
                print(
                    f"[colab-llm] Hard session cap reached "
                    f"({format_duration(max_duration_s)}). Stopping Colab runtime."
                )
                _request_stop("max_duration")
                break
            if proxy.stats.is_idle_expired(idle_timeout_s, now):
                print(
                    f"[colab-llm] Idle timeout reached "
                    f"({format_duration(idle_timeout_s)} with 0 active requests). "
                    "Stopping Colab runtime."
                )
                _request_stop("idle_timeout")
                break
            if rm_on_disconnect and not tunnel.is_running:
                _request_stop("tunnel_disconnected")
                break

            if now - last_state_write >= 10.0:
                _write_state("ready", {"health": health_info})
                last_state_write = now

            if now - last_heartbeat >= 300.0:
                await asyncio.to_thread(tunnel.touch_remote_heartbeat)
                last_heartbeat = now

            if now - last_status_log >= 300.0:
                idle_rem = max(0.0, idle_timeout_s - proxy.stats.idle_seconds(now))
                cap_rem = max(0.0, max_duration_s - proxy.stats.elapsed_seconds(now))
                print(
                    f"[colab-llm] Alive ({session}): "
                    f"active={proxy.stats.active_connections}, "
                    f"requests={proxy.stats.total_connections}, "
                    f"idle_left={format_duration(idle_rem)}, "
                    f"cap_left={format_duration(cap_rem)}"
                )
                last_status_log = now

            try:
                await asyncio.wait_for(stop_event.wait(), timeout=5.0)
            except TimeoutError:
                pass

        return stop_reason, proxy.stats
    finally:
        for sig in installed_signals:
            try:
                loop.remove_signal_handler(sig)
            except (NotImplementedError, RuntimeError):
                pass
        await proxy.close()
        await tunnel.stop()
        _write_state("stopped", {"stop_reason": stop_reason})
