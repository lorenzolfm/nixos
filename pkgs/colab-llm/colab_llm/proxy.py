"""Loopback activity-tracking TCP proxy, SSH tunnel supervisor, and CU watchdog."""

from __future__ import annotations

import asyncio
import json
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, assert_never

from colab_llm.colab import (
    ColabError,
    build_proxy_command,
    build_ssh_mux_args,
    build_ssh_tunnel_args,
)
from colab_llm.config import (
    HEARTBEAT_INTERVAL_S,
    SIGNAL_STOP_REASONS,
    LaunchConfig,
    SessionName,
    StopReason,
    deadman_heartbeat_timeout_s,
    format_duration,
    validate_loopback_host,
    validate_port,
)
from colab_llm.state import Ready, StateWriter, Stopping, Traffic, WaitingHealth

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
# How often a ready engine's process is checked on the VM.
ENGINE_ALIVE_CHECK_INTERVAL_S = 60.0
STATUS_LOG_INTERVAL_S = 300.0


def pick_free_loopback_port() -> int:
    """Allocate an ephemeral unused TCP port on 127.0.0.1."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        port = int(sock.getsockname()[1])
    assert 0 < port <= 65535
    return port


def bind_loopback_listener(bind_host: str, port: int) -> socket.socket:
    """Bind the user-facing loopback port now, so a port in use fails early.

    The caller hands the bound socket to `ActivityTrackingProxy`. Holding it is
    what makes "the port is ours" true for the rest of the run.
    """
    host = validate_loopback_host(bind_host)
    validate_port(port, "local_port")
    family, _, _, _, address = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM)[0]
    listener = socket.socket(family, socket.SOCK_STREAM)
    try:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(address)
        # Listen now: a bound socket that is not listening does not stop a
        # second process from binding the same port. Until the proxy starts,
        # early clients wait in the backlog instead of being refused.
        listener.listen()
    except OSError:
        listener.close()
        raise
    return listener


@dataclass(frozen=True)
class Healthy:
    """The engine answered 200 and is serving. `info` is its own health payload."""

    info: dict[str, Any]


@dataclass(frozen=True)
class Loading:
    """The engine answered, but says it is not serving yet."""

    status: str


@dataclass(frozen=True)
class Unreachable:
    """No usable answer: nothing is listening, or the tunnel is down."""


Health = Healthy | Loading | Unreachable

_LOADING_STATUSES = frozenset({"loading", "reloading", "updating"})


def probe_http_health(
    *,
    port: int,
    health_path: str = "/health",
    timeout_s: float = 3.0,
) -> Health:
    """Query the remote inference server through the internal loopback tunnel port."""
    url = f"http://127.0.0.1:{port}{health_path}"
    req = urllib.request.Request(url, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            if resp.status != 200:
                return Unreachable()
            raw = resp.read().decode("utf-8", errors="replace").strip()
    except (urllib.error.URLError, TimeoutError, OSError):
        return Unreachable()
    if not raw:
        return Healthy(info={})
    try:
        data = json.loads(raw)
    except ValueError:
        return Healthy(info={"raw": raw[:200]})
    if not isinstance(data, dict):
        return Unreachable()
    status = data.get("status")
    if isinstance(status, str) and status in _LOADING_STATUSES:
        return Loading(status=status)
    if data.get("unloaded") is True:
        return Loading(status="unloaded")
    return Healthy(info=data)


@dataclass
class ProxyStats:
    started_monotonic: float = field(default_factory=time.monotonic)
    last_activity_monotonic: float = field(default_factory=time.monotonic)
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
        listener: socket.socket,
        upstream_port: int,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        # Already bound by `bind_loopback_listener`, which checked it is loopback.
        self._listener = listener
        self.listen_port = int(listener.getsockname()[1])
        self.upstream_port = validate_port(upstream_port, "upstream_port")
        assert self.listen_port != self.upstream_port, "proxy would connect to itself"
        self._clock = clock
        now = clock()
        self.stats = ProxyStats(started_monotonic=now, last_activity_monotonic=now)
        self._server: asyncio.Server | None = None
        self._handlers: set[asyncio.Task[Any]] = set()
        self._closing = False

    @property
    def active_connections(self) -> int:
        """Connections being relayed. Derived, so it cannot drift from the handlers."""
        return len(self._handlers)

    def traffic(self, *, tunnel_port: int, now: float) -> Traffic:
        return Traffic(
            tunnel_port=tunnel_port,
            idle_s=round(self.stats.idle_s(now), 1),
            elapsed_s=round(self.stats.elapsed_s(now), 1),
            active_connections=self.active_connections,
            total_connections=self.stats.total_connections,
            bytes_in=self.stats.bytes_in,
            bytes_out=self.stats.bytes_out,
        )

    async def start(self) -> None:
        assert self._server is None, "proxy already started"
        self._closing = False
        self._server = await asyncio.start_server(self._handle_client, sock=self._listener)

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
        if self.active_connections < CLIENT_CONNECTIONS_MAX:
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
        self.stats.total_connections += 1
        self.stats.last_activity_monotonic = self._clock()
        assert self.active_connections <= CLIENT_CONNECTIONS_MAX

        upstream_writer: asyncio.StreamWriter | None = None
        pipes: list[asyncio.Task[None]] = []
        try:
            upstream_reader, connected_writer = await asyncio.wait_for(
                asyncio.open_connection("127.0.0.1", self.upstream_port),
                timeout=UPSTREAM_CONNECT_TIMEOUT_S,
            )
            upstream_writer = connected_writer
            pipes = [
                asyncio.create_task(self._pipe(client_reader, connected_writer, inbound=True)),
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
            assert handler in self._handlers
            self._handlers.discard(handler)
            self.stats.last_activity_monotonic = self._clock()
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
        session: SessionName,
        tunnel_port: int,
        remote_port: int,
        identity: Path,
        control_path: Path,
        rm_on_disconnect: bool = False,
        colab_config: Path | None = None,
        session_listed: Callable[[], bool] | None = None,
    ) -> None:
        # Whether Colab still lists the session. Asked before every reconnect:
        # `colab ssh --proxy-mode` allocates a new runtime under a name it no
        # longer knows, and a tunnel to that empty VM would look healthy.
        self._session_listed = session_listed
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
        assert self._task is None, "tunnel supervisor already started"
        self._stopping = False
        self._task = asyncio.create_task(self._supervise_loop())

    @property
    def ended(self) -> bool:
        """The supervise loop is over without `stop()`; the session must then end.

        Read from the task itself, so a loop that gave up and a loop that died
        on an unexpected error are the same state and neither can be missed.
        """
        return self._task is not None and self._task.done()

    def end_error(self) -> BaseException | None:
        """The error that ended the loop, if it did not return on its own."""
        if self._task is None or not self._task.done() or self._task.cancelled():
            return None
        return self._task.exception()

    def _remove_control_socket(self) -> None:
        # A stale socket would make the next `ssh -M` refuse to start.
        self.control_path.unlink(missing_ok=True)

    async def _session_still_listed(self) -> bool | None:
        """True or False as Colab reports it; None when the listing failed."""
        if self._session_listed is None:
            return True
        try:
            return await asyncio.to_thread(self._session_listed)
        except ColabError:
            return None

    async def _supervise_loop(self) -> None:
        backoff_s = TUNNEL_BACKOFF_S_MIN
        failures_consecutive = 0
        reconnecting = False
        # Bounded by TUNNEL_FAILURES_CONSECUTIVE_MAX failures in a row; a tunnel
        # that keeps working runs until `stop()`, which the hard cap guarantees.
        while not self._stopping:
            started = time.monotonic()
            # The first connection follows the bootstrap, which just used the
            # session. Later ones must not assume it still exists.
            listed = await self._session_still_listed() if reconnecting else True
            if self._stopping:
                break
            if listed is False:
                print(
                    f"[colab-llm] Session {self.session!r} is no longer listed on Colab; "
                    "not reconnecting the SSH tunnel.",
                    file=sys.stderr,
                )
                break
            exit_code: int | None = None
            err_msg = "could not confirm that the session is still listed on Colab"
            if listed is True:
                reconnecting = True
                self._remove_control_socket()
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
                break

            self.reconnect_count += 1
            print(
                f"[colab-llm] SSH tunnel dropped (rc={exit_code}); "
                f"reconnecting in {backoff_s:.0f}s" + (f": {err_msg}" if err_msg else ""),
                file=sys.stderr,
            )
            await asyncio.sleep(backoff_s)
            backoff_s = min(backoff_s * 2.0, TUNNEL_BACKOFF_S_MAX)

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


@dataclass(frozen=True)
class SessionOutcome:
    # Says by itself whether the engine was ever ready: see `is_failed_start`.
    stop_reason: StopReason
    total_connections: int


async def run_supervised_session(
    *,
    config: LaunchConfig,
    listener: socket.socket,
    health_path: str,
    state: StateWriter,
    control_path: Path,
    colab_bin: str,
    ssh_bin: str,
    identity: Path,
    on_ready: Callable[[dict[str, Any]], None] | None = None,
    check_remote_alive: Callable[[], bool] | None = None,
    session_listed: Callable[[], bool] | None = None,
) -> SessionOutcome:
    """Run the SSH tunnel, loopback activity proxy, health check, and idle/cap watchdog.

    `config` is already validated and `listener` already bound, so nothing here
    re-checks them. Caller is responsible for executing `colab stop -s <session>`
    after this coroutine returns.
    """
    assert health_path.startswith("/")
    session = config.session
    engine = config.engine.kind.value
    remote_port = config.remote_port
    idle_timeout_s = config.idle_timeout_s
    max_duration_s = config.max_duration_s
    health_timeout_s = config.bootstrap_timeout_s

    tunnel_port = pick_free_loopback_port()
    assert tunnel_port != config.local_port
    tunnel = TunnelSupervisor(
        colab_bin=colab_bin,
        ssh_bin=ssh_bin,
        session=session,
        tunnel_port=tunnel_port,
        remote_port=remote_port,
        identity=identity,
        control_path=control_path,
        rm_on_disconnect=config.rm_on_disconnect,
        colab_config=config.colab_config,
        session_listed=session_listed,
    )
    proxy = ActivityTrackingProxy(listener=listener, upstream_port=tunnel_port)

    stop_event = asyncio.Event()
    # Stays ERROR unless a branch below names the reason, so an exception that
    # escapes this coroutine is recorded as one.
    stop_reason = StopReason.ERROR
    ready = False

    def _request_stop(reason: StopReason) -> None:
        nonlocal stop_reason
        if not stop_event.is_set():
            stop_reason = reason
            stop_event.set()

    loop = asyncio.get_running_loop()
    # `remove_signal_handler` resets a signal to its default action, not to the
    # handler it replaced. The caller's handlers are what route SIGTERM and
    # SIGHUP through teardown, so they are saved here and put back on the way out.
    caller_handlers = {sig: signal.getsignal(sig) for sig in SIGNAL_STOP_REASONS}
    installed_signals: list[signal.Signals] = []
    for sig, reason in SIGNAL_STOP_REASONS.items():
        try:
            loop.add_signal_handler(sig, _request_stop, reason)
            installed_signals.append(sig)
        except NotImplementedError:
            # Platform without loop signal support: the caller's handlers remain.
            pass

    def _traffic() -> Traffic:
        return proxy.traffic(tunnel_port=tunnel_port, now=time.monotonic())

    def _outcome() -> SessionOutcome:
        return SessionOutcome(
            stop_reason=stop_reason,
            total_connections=proxy.stats.total_connections,
        )

    def _stop_for_ended_tunnel(reason: StopReason) -> None:
        error = tunnel.end_error()
        if error is not None:
            print(f"[colab-llm] SSH tunnel supervisor failed: {error!r}", file=sys.stderr)
        _request_stop(reason)

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

    try:
        # Inside the `try`: once the tunnel task exists, only the `finally`
        # below stops it.
        await tunnel.start()
        await proxy.start()
        state.write(WaitingHealth(traffic=_traffic()))

        # Phase 1: wait for inference engine health through the internal tunnel port.
        # Bounded by `deadline`.
        deadline = time.monotonic() + health_timeout_s
        last_log = time.monotonic()
        health_info: dict[str, Any] = {}
        while not stop_event.is_set():
            health = await asyncio.to_thread(
                probe_http_health,
                port=tunnel_port,
                health_path=health_path,
                timeout_s=HEALTH_PROBE_INTERVAL_S,
            )
            status_hint = ""
            match health:
                case Healthy(info=info):
                    ready = True
                    health_info = info
                    break
                case Loading(status=status):
                    status_hint = f" (engine status: {status})"
                case Unreachable():
                    pass
                case _:
                    assert_never(health)
            now = time.monotonic()
            if now >= deadline:
                _request_stop(StopReason.HEALTH_TIMEOUT)
                break
            if tunnel.ended:
                _stop_for_ended_tunnel(StopReason.TUNNEL_FAILED)
                break
            await _heartbeat_if_due(now, tunnel_expected=False)
            if now - last_log >= HEALTH_LOG_INTERVAL_S:
                remaining_s = max(0.0, deadline - now)
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
            return _outcome()

        # Both clocks start at the moment the endpoint becomes ready, so a long
        # model load counts neither as idle time nor against the hard cap. The
        # remote deadman's cap already allows for the load (see
        # `deadman_duration_s_max`).
        ready_at = time.monotonic()
        proxy.stats.last_activity_monotonic = ready_at
        proxy.stats.started_monotonic = ready_at
        state.write(Ready(traffic=_traffic(), health=health_info))
        if on_ready is not None:
            on_ready(health_info)

        # Phase 2: serve traffic and enforce idle timeout + hard max duration.
        # Bounded by the hard cap, which is checked first on every tick.
        last_state_write = 0.0
        last_status_log = time.monotonic()
        last_alive_check = time.monotonic()
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
            if tunnel.ended:
                _stop_for_ended_tunnel(StopReason.TUNNEL_DISCONNECTED)
                break

            if now - last_state_write >= STATE_WRITE_INTERVAL_S:
                state.write(Ready(traffic=_traffic(), health=health_info))
                last_state_write = now

            await _heartbeat_if_due(now, tunnel_expected=True)

            # A dead engine serves nothing, yet a client that keeps retrying
            # would keep the idle timer from ever expiring.
            if (
                check_remote_alive is not None
                and now - last_alive_check >= ENGINE_ALIVE_CHECK_INTERVAL_S
                and control_path.exists()
            ):
                last_alive_check = now
                if not await asyncio.to_thread(check_remote_alive):
                    print(
                        f"[colab-llm] The {engine} process on the VM has exited. "
                        "Stopping Colab runtime.",
                        file=sys.stderr,
                    )
                    _request_stop(StopReason.ENGINE_EXITED)
                    break

            if now - last_status_log >= STATUS_LOG_INTERVAL_S:
                idle_left_s = max(0.0, idle_timeout_s - proxy.stats.idle_s(now))
                cap_left_s = max(0.0, max_duration_s - proxy.stats.elapsed_s(now))
                print(
                    f"[colab-llm] Alive ({session}): "
                    f"active={proxy.active_connections}, "
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
        return _outcome()
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
                    previous = caller_handlers[sig]
                    # None is a handler that was not installed from Python;
                    # there is nothing of the caller's to put back.
                    if previous is not None:
                        signal.signal(sig, previous)
                # Not "stopped": the runtime is only stopped once the caller's
                # teardown has verified it.
                state.write(Stopping(stop_reason=stop_reason))
