"""Command-line interface for colab-llm."""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import shlex
import signal
import socket
import stat
import sys
import time
from collections.abc import Sequence
from dataclasses import asdict
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, assert_never

from colab_llm import __version__
from colab_llm.colab import (
    ColabClient,
    ColabError,
    UsageSnapshot,
    find_colab_bin,
    render_ssh_config_block,
    resolve_ssh_identity,
)
from colab_llm.config import (
    ALLOWED_ENGINES,
    ALLOWED_GPUS,
    ALLOWED_MAYA_DRAFTS,
    DEFAULT_BIND_HOST,
    DEFAULT_BOOTSTRAP_TIMEOUT_S,
    DEFAULT_CONTEXT_LENGTH,
    DEFAULT_ENGINE,
    DEFAULT_GPU,
    DEFAULT_IDLE_TIMEOUT_S,
    DEFAULT_LOCAL_PORT,
    DEFAULT_MAX_DURATION_S,
    DEFAULT_MAYA_DRAFT,
    DEFAULT_PARALLEL,
    DEFAULT_REMOTE_PORT,
    DEFAULT_SESSION,
    SIGNAL_STOP_REASONS,
    Attach,
    Engine,
    LaunchConfig,
    StopReason,
    build_engine_config,
    exit_code_for,
    format_duration,
    parse_duration,
    resolve_hf_token,
    state_dir,
    validate_session_name,
)
from colab_llm.engines import get_engine, list_engines, support_script_paths
from colab_llm.proxy import (
    bind_loopback_listener,
    probe_http_health,
    run_supervised_session,
)
from colab_llm.state import (
    LEDGER_VERSION,
    Allocating,
    Bootstrapping,
    Ready,
    RunInfo,
    SessionBusyError,
    SessionState,
    StateError,
    StateWriter,
    Stopped,
    Stopping,
    Traffic,
    WaitingHealth,
    acquire_session_lock,
    append_jsonl,
    ledger_path,
    live_supervisor_pid,
    phase_name,
    read_jsonl_tail,
    read_session_state,
    session_state_path,
    state_to_json,
)

REMOTE_WORK_DIR = "/content/colab-llm"

# Unix domain socket paths are limited to 104 bytes on macOS.
SOCKET_PATH_BYTES_MAX = 103

SUPERVISOR_STOP_POLLS_MAX = 10
SUPERVISOR_STOP_POLL_S = 0.2

TEARDOWN_ATTEMPTS_MAX = 3
TEARDOWN_RETRY_PAUSE_S = 5.0

_TERMINATION_SIGNALS = (signal.SIGTERM, signal.SIGHUP)


def _control_socket_dir() -> Path:
    """Private directory for the SSH ControlMaster socket.

    Without XDG_RUNTIME_DIR this lives in shared /tmp, where another local user
    can create the directory first. A socket inside a directory someone else
    owns would hand them our SSH multiplexing channel, so ownership and mode
    are verified rather than assumed.
    """
    runtime_base = Path(os.environ.get("XDG_RUNTIME_DIR") or "/tmp")
    socket_dir = runtime_base / f"colab-llm-{os.getuid()}"
    try:
        socket_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    except FileExistsError as exc:
        raise ColabError(f"Control socket path {socket_dir} is not a directory.") from exc
    info = os.lstat(socket_dir)
    if not stat.S_ISDIR(info.st_mode):
        raise ColabError(f"Control socket path {socket_dir} is not a directory.")
    if info.st_uid != os.getuid():
        raise ColabError(
            f"Control socket directory {socket_dir} is owned by uid {info.st_uid}, "
            "not by you. Remove it or set XDG_RUNTIME_DIR to a private directory."
        )
    if stat.S_IMODE(info.st_mode) != 0o700:
        os.chmod(socket_dir, 0o700)
    assert stat.S_IMODE(os.lstat(socket_dir).st_mode) == 0o700
    return socket_dir


def _control_socket_path(session: str) -> Path:
    validate_session_name(session)
    path = _control_socket_dir() / f"{session}.sock"
    if len(os.fsencode(path)) > SOCKET_PATH_BYTES_MAX:
        raise ColabError(f"Control socket path is too long for a Unix socket: {path}")
    return path


def _try_get_usage(client: ColabClient) -> UsageSnapshot | None:
    try:
        return client.get_usage()
    except ColabError as exc:
        print(f"[colab-llm] Warning: could not query `colab usage`: {exc}", file=sys.stderr)
        return None


def _print_ready_banner(config: LaunchConfig, health: dict[str, Any]) -> None:
    base = f"http://{config.bind_host}:{config.local_port}"
    model_id = health.get("model") or config.engine.served_model
    label = health.get("model_label") or model_id
    ctx = health.get("max_context") or config.context_length
    print()
    print("=" * 72)
    print(f"[colab-llm] READY — {label} on Colab {config.gpu} (session: {config.session})")
    print(f"  OpenAI API:     {base}/v1")
    print(f"  Anthropic API:  {base}  (endpoint: {base}/v1/messages)")
    print(f"  Health:         {base}{get_engine(config.engine.kind).health_path}")
    print(f"  Context:        {ctx} tokens")
    print(
        f"  Auto-teardown:  idle={format_duration(config.idle_timeout_s)}, "
        f"hard_cap={format_duration(config.max_duration_s)}"
    )
    print()
    print("  Quick check:")
    print(f"    curl -s {base}/v1/models | jq .")
    print("  Claude Code (Anthropic-compatible):")
    print(f"    ANTHROPIC_BASE_URL={base} ANTHROPIC_AUTH_TOKEN=local claude --model {model_id}")
    print("  OpenAI-compatible harness:")
    print(f"    OPENAI_BASE_URL={base}/v1 OPENAI_API_KEY=local")
    print()
    print("  Press Ctrl+C (or run `colab-llm stop`) to tear down and stop CU billing.")
    print("=" * 72)
    print(flush=True)


def _say(message: str, *, error: bool = False) -> None:
    """Print without ever raising: teardown must proceed when the terminal is gone."""
    try:
        print(message, file=sys.stderr if error else sys.stdout, flush=True)
    except (OSError, ValueError):
        pass


def _upload_and_bootstrap(
    client: ColabClient,
    config: LaunchConfig,
    identity: Path,
) -> None:
    engine = get_engine(config.engine.kind)
    uploads = {"bootstrap.sh": engine.script_path().read_bytes()}
    for support_path in support_script_paths():
        assert support_path.name not in uploads, support_path.name
        uploads[support_path.name] = support_path.read_bytes()

    print(f"[colab-llm] Uploading {engine.name} bootstrap + deadman watchdog to VM...")
    for remote_name, content in uploads.items():
        assert content, f"{remote_name} is empty"
        remote_path = shlex.quote(f"{REMOTE_WORK_DIR}/{remote_name}")
        client.ssh_exec(
            session=config.session,
            identity=identity,
            remote_command=(
                f"install -d -m 700 {REMOTE_WORK_DIR} "
                f"&& cat > {remote_path} && chmod 700 {remote_path}"
            ),
            stdin_bytes=content,
            timeout=120,
        )

    hf_token = resolve_hf_token(config.hf_token_file)
    if hf_token is not None:
        # Maya only needs the token for its prefetch. The other engines download
        # inside the server process, so the token stays in that process's
        # environment after the file is removed.
        print(
            "[colab-llm] Staging HF token privately on VM "
            "(0600; the file is removed once the engine has started)..."
        )
        client.ssh_exec(
            session=config.session,
            identity=identity,
            remote_command=f"umask 077 && cat > {REMOTE_WORK_DIR}/.hf_token",
            stdin_bytes=(hf_token + "\n").encode("utf-8"),
            timeout=60,
        )

    env_vars = engine.build_env(config)
    env_assignments = " ".join(
        f"{k}={shlex.quote(v)}" for k, v in sorted(env_vars.items())
    )
    remote_cmd = f"env {env_assignments} bash {REMOTE_WORK_DIR}/bootstrap.sh"
    print(f"[colab-llm] Running {engine.name} bootstrap on Colab {config.gpu}...")
    client.ssh_exec(
        session=config.session,
        identity=identity,
        remote_command=remote_cmd,
        stream_output=True,
        timeout=config.bootstrap_timeout_s,
    )


class _Terminated(KeyboardInterrupt):
    """SIGTERM or SIGHUP arrived; unwinds like Ctrl+C so teardown still runs."""

    def __init__(self, reason: StopReason) -> None:
        super().__init__(reason.value)
        self.reason = reason


def _raise_terminated(signum: int, _frame: object) -> None:
    raise _Terminated(SIGNAL_STOP_REASONS[signal.Signals(signum)])


def _teardown_runtime(client: ColabClient, session: str, stop_reason: StopReason) -> bool:
    """Stop the billable runtime. Returns whether the stop was verified.

    Signals are ignored for the duration: the child `colab stop` shares our
    process group, so a second Ctrl+C would otherwise abort the stop itself.
    Bounded by TEARDOWN_ATTEMPTS_MAX attempts, each with its own timeouts.
    """
    ignored = (signal.SIGINT, *_TERMINATION_SIGNALS)
    previous = {sig: signal.signal(sig, signal.SIG_IGN) for sig in ignored}
    try:
        _say(
            f"[colab-llm] Tearing down Colab session {session!r} ({stop_reason}); "
            "Ctrl+C is ignored until the runtime is stopped..."
        )
        for attempt in range(1, TEARDOWN_ATTEMPTS_MAX + 1):
            try:
                client.teardown_session(session)
                return True
            except ColabError as exc:
                _say(
                    f"[colab-llm] WARNING: stop attempt {attempt}/{TEARDOWN_ATTEMPTS_MAX} "
                    f"for session {session!r} failed: {exc}",
                    error=True,
                )
            if attempt < TEARDOWN_ATTEMPTS_MAX:
                time.sleep(TEARDOWN_RETRY_PAUSE_S)
        _say(
            f"[colab-llm] WARNING: session {session!r} may still be running and billing.\n"
            f"  Run manually: colab stop -s {session}",
            error=True,
        )
        return False
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


def _record_session_end(
    *,
    client: ColabClient,
    config: LaunchConfig,
    usage_before: UsageSnapshot | None,
    started_wall: datetime,
    started_mono: float,
    connections_total: int,
    stop_reason: StopReason,
    runtime_stopped: bool,
) -> None:
    """Append the session to the ledger and print the closing balance. Best effort."""
    usage_after = _try_get_usage(client)
    elapsed_s = round(time.monotonic() - started_mono, 1)
    assert elapsed_s >= 0
    cu_delta = (
        round(usage_before.balance_cu - usage_after.balance_cu, 3)
        if usage_before is not None and usage_after is not None
        else None
    )
    # `cu_delta` is not stored: it is the difference of the two balances below,
    # and `usage` computes it when it reads the record.
    record: dict[str, Any] = {
        "version": LEDGER_VERSION,
        "session": config.session,
        "gpu": config.gpu,
        "engine": config.engine.kind.value,
        "model": config.engine.served_model,
        "started_at": started_wall.isoformat(),
        "ended_at": datetime.now(UTC).isoformat(),
        "duration_s": elapsed_s,
        "connections": connections_total,
        "stop_reason": stop_reason.value,
        "runtime_stopped": runtime_stopped,
        "cu_before": usage_before.balance_cu if usage_before else None,
        "cu_after": usage_after.balance_cu if usage_after else None,
    }
    append_jsonl(ledger_path(), record)
    if usage_after is not None:
        delta_str = f" (burned ~{cu_delta:.2f} CU)" if cu_delta is not None else ""
        _say(
            f"[colab-llm] Session ended after {format_duration(elapsed_s)}. "
            f"Remaining balance: {usage_after.balance_cu:.2f} CU{delta_str}."
        )


def _cmd_start(args: argparse.Namespace) -> int:
    config = LaunchConfig(
        session=args.session,
        gpu=args.gpu,
        high_mem=args.high_mem,
        engine=build_engine_config(
            Engine(args.engine),
            model=args.model,
            hf_repo=args.hf_repo,
            hf_revision=args.hf_revision,
            draft=args.draft,
            parallel=args.parallel,
            no_vision=args.no_vision,
            web_ui=args.web_ui,
        ),
        context_length=args.context,
        bind_host=args.host,
        local_port=args.port,
        remote_port=args.remote_port,
        idle_timeout_s=parse_duration(args.idle_timeout),
        max_duration_s=parse_duration(args.max_duration),
        bootstrap_timeout_s=parse_duration(args.bootstrap_timeout),
        fast_hf=not args.no_fast_hf,
        rm_on_disconnect=args.rm_on_disconnect,
        reconfigure=args.reconfigure,
        attach=args.attach,
        identity=Path(args.identity) if args.identity else None,
        colab_config=Path(args.colab_config) if args.colab_config else None,
        hf_token_file=Path(args.hf_token_file) if args.hf_token_file else None,
    )

    # Everything that can fail locally is resolved here, before a billable GPU
    # exists: once it does, the only acceptable exits go through teardown.
    identity = resolve_ssh_identity(config.identity)
    client = ColabClient(colab_config=config.colab_config)
    engine = get_engine(config.engine.kind)
    engine.script_path()
    support_script_paths()
    control_path = _control_socket_path(config.session)

    # One supervisor per session, and the user-facing port is ours, from here
    # until the process exits. A second `start` fails on these two lines; it can
    # no longer reach the teardown of a session another supervisor is serving.
    lock = acquire_session_lock(config.session)
    try:
        try:
            listener = bind_loopback_listener(config.bind_host, config.local_port)
        except OSError as exc:
            raise ColabError(
                f"Cannot listen on {config.bind_host}:{config.local_port}: {exc}"
            ) from exc
        try:
            return _run_session(
                config=config,
                client=client,
                identity=identity,
                control_path=control_path,
                listener=listener,
            )
        finally:
            listener.close()
    finally:
        lock.release()


def _run_session(
    *,
    config: LaunchConfig,
    client: ColabClient,
    identity: Path,
    control_path: Path,
    listener: socket.socket,
) -> int:
    engine = get_engine(config.engine.kind)
    state = StateWriter(
        session_state_path(config.session),
        RunInfo(
            session=config.session,
            pid=os.getpid(),
            engine=config.engine.kind,
            model=config.engine.served_model,
            gpu=config.gpu,
            bind_host=config.bind_host,
            local_port=config.local_port,
            remote_port=config.remote_port,
            idle_timeout_s=config.idle_timeout_s,
            max_duration_s=config.max_duration_s,
        ),
    )

    # Preflight auth + CU balance before allocating any billable GPU.
    who = client.whoami()
    print(f"[colab-llm] Authenticated: {who}")
    usage_before = _try_get_usage(client)
    if usage_before is not None:
        print(
            f"[colab-llm] CU balance: {usage_before.balance_cu:.2f} CU "
            f"(rate: {usage_before.rate_cu_per_hour:.2f}/hr, "
            f"active: {usage_before.active_assignments})"
        )

    # A closed terminal or `kill` must unwind through teardown, like Ctrl+C.
    for sig in _TERMINATION_SIGNALS:
        signal.signal(sig, _raise_terminated)

    started_wall = datetime.now(UTC)
    started_mono = time.monotonic()
    state.write(Allocating())
    # `ensure_session` stops whatever it allocated if it fails part-way.
    info, created = client.ensure_session(
        session=config.session,
        gpu=config.gpu,
        high_mem=config.high_mem,
        attach=config.attach,
    )

    stop_reason = StopReason.ERROR
    was_ready = False
    connections_total = 0
    try:
        action = (
            "Allocated new"
            if created
            else ("Attached to existing" if config.attach is not None else "Reusing existing")
        )
        print(
            f"[colab-llm] {action} session [{info.name}] ({info.endpoint}) "
            f"Hardware={info.hardware} Shape={info.shape}"
        )
        state.write(Bootstrapping())
        _upload_and_bootstrap(client, config, identity)

        def _check_remote_pid() -> bool:
            # Only multiplex over the active ControlMaster socket; never open a
            # second competing `colab ssh` WebSocket session (Colab caps active
            # SSH sessions and returns HTTP 429). Also distinguish an explicit
            # DEAD server PID from a transient SSH transport error.
            if not control_path.exists():
                return True
            try:
                proc = client.ssh_exec(
                    session=config.session,
                    identity=identity,
                    control_path=control_path,
                    remote_command=(
                        f"if test -f {REMOTE_WORK_DIR}/server.pid "
                        f"&& kill -0 $(cat {REMOTE_WORK_DIR}/server.pid) 2>/dev/null; "
                        "then echo ALIVE; else echo DEAD; fi"
                    ),
                    timeout=20,
                )
            except ColabError:
                # Transport trouble is not evidence the server died.
                return True
            return proc.stdout.decode("utf-8", errors="ignore").strip() != "DEAD"

        outcome = asyncio.run(
            run_supervised_session(
                config=config,
                listener=listener,
                health_path=engine.health_path,
                state=state,
                control_path=control_path,
                colab_bin=client.colab_bin,
                ssh_bin=client.ssh_bin,
                identity=identity,
                on_ready=lambda h: _print_ready_banner(config, h),
                check_remote_alive=_check_remote_pid,
            )
        )
        stop_reason = outcome.stop_reason
        was_ready = outcome.was_ready
        connections_total = outcome.total_connections
        if not was_ready and exit_code_for(stop_reason, was_ready=False) != 0:
            print(
                f"[colab-llm] Engine did not reach ready state ({stop_reason}). "
                "Last 40 lines of remote server.log:",
                file=sys.stderr,
            )
            try:
                client.ssh_exec(
                    session=config.session,
                    identity=identity,
                    force=True,
                    remote_command=f"tail -n 40 {REMOTE_WORK_DIR}/server.log || true",
                    stream_output=True,
                    timeout=30,
                )
            except ColabError as exc:
                print(f"[colab-llm] Could not fetch server.log: {exc}", file=sys.stderr)
    except _Terminated as exc:
        stop_reason = exc.reason
    except KeyboardInterrupt:
        stop_reason = StopReason.SIGNAL_SIGINT
    finally:
        runtime_stopped = _teardown_runtime(client, config.session, stop_reason)
        try:
            # Written only now: "stopped" must say whether the runtime really is.
            state.write(Stopped(stop_reason=stop_reason, runtime_stopped=runtime_stopped))
        except OSError as exc:
            _say(f"[colab-llm] Warning: could not write the session state: {exc}", error=True)
        try:
            _record_session_end(
                client=client,
                config=config,
                usage_before=usage_before,
                started_wall=started_wall,
                started_mono=started_mono,
                connections_total=connections_total,
                stop_reason=stop_reason,
                runtime_stopped=runtime_stopped,
            )
        except OSError as exc:
            _say(f"[colab-llm] Warning: could not record session in ledger: {exc}", error=True)
    if not runtime_stopped:
        return 1
    return exit_code_for(stop_reason, was_ready=was_ready)


def _stop_local_supervisor(session: str) -> None:
    # Liveness comes from the session lock, so a stale or recycled PID is never
    # signalled: a dead supervisor holds no lock.
    pid = live_supervisor_pid(session)
    if pid is None:
        return
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    except PermissionError:
        print(f"[colab-llm] Warning: not permitted to signal supervisor pid {pid}.", file=sys.stderr)
        return
    # Give the supervisor a moment to close its SSH tunnel cleanly.
    for _ in range(SUPERVISOR_STOP_POLLS_MAX):
        time.sleep(SUPERVISOR_STOP_POLL_S)
        if live_supervisor_pid(session) is None:
            return
    print(
        f"[colab-llm] Warning: local supervisor pid {pid} is still running after "
        f"{SUPERVISOR_STOP_POLLS_MAX * SUPERVISOR_STOP_POLL_S:.0f}s; "
        "stopping the Colab session regardless.",
        file=sys.stderr,
    )


def _try_stop_local_supervisor(session: str) -> None:
    """Best effort: nothing on the local side may prevent the Colab stop."""
    try:
        _stop_local_supervisor(session)
    except (ValueError, OSError) as exc:
        print(
            f"[colab-llm] Warning: could not stop local supervisor of {session!r}: {exc}",
            file=sys.stderr,
        )


def _cmd_stop(args: argparse.Namespace) -> int:
    client = ColabClient(
        colab_config=Path(args.colab_config) if args.colab_config else None
    )
    if args.all:
        for lock_file in state_dir().glob("*.lock"):
            _try_stop_local_supervisor(lock_file.stem)
        client.stop_all()
        print("[colab-llm] Stopped all active Colab sessions.")
        return 0

    session = validate_session_name(args.session or DEFAULT_SESSION)
    _try_stop_local_supervisor(session)
    # Same stop as `start` uses on exit: issued unconditionally, then verified.
    client.teardown_session(session)
    print(f"[colab-llm] Stop issued; session {session!r} is not active on Colab.")
    return 0


def _describe_live_state(state: SessionState, *, healthy: bool) -> str:
    run = state.run
    head = f"pid={run.pid} phase={phase_name(state.phase)}"
    endpoint = f"endpoint=http://{run.bind_host}:{run.local_port}"

    def _serving(traffic: Traffic) -> str:
        idle_left_s = max(0.0, run.idle_timeout_s - traffic.idle_s)
        cap_left_s = max(0.0, run.max_duration_s - traffic.elapsed_s)
        return (
            f"{head} healthy={healthy} {endpoint} "
            f"idle_left={format_duration(idle_left_s)} cap_left={format_duration(cap_left_s)}"
        )

    phase = state.phase
    match phase:
        case Allocating() | Bootstrapping():
            return f"{head} ({endpoint} is not serving yet)"
        case WaitingHealth(traffic=traffic) | Ready(traffic=traffic):
            return _serving(traffic)
        case Stopping(stop_reason=stop_reason):
            return f"{head} reason={stop_reason.value} (stopping the Colab runtime)"
        case Stopped(stop_reason=stop_reason, runtime_stopped=runtime_stopped):
            return f"{head} reason={stop_reason.value} runtime_stopped={runtime_stopped}"
        case _:
            assert_never(phase)


def _describe_last_state(state: SessionState) -> str:
    phase = state.phase
    match phase:
        case Stopped(stop_reason=stop_reason, runtime_stopped=True):
            return f"last run ended ({stop_reason.value}); runtime stop verified"
        case Stopped(stop_reason=stop_reason):
            return (
                f"WARNING: last run ended ({stop_reason.value}) but the runtime stop was "
                f"NOT verified; it may still be billing. Run: colab-llm stop -s {state.run.session}"
            )
        case Allocating() | Bootstrapping() | WaitingHealth() | Ready() | Stopping():
            return (
                f"WARNING: last supervisor (pid {state.run.pid}) died in phase "
                f"{phase_name(phase)} without confirming a stop; check the sessions above. "
                f"Run: colab-llm stop -s {state.run.session}"
            )
        case _:
            assert_never(phase)


def _cmd_status(args: argparse.Namespace) -> int:
    validate_session_name(args.session)
    client = ColabClient(
        colab_config=Path(args.colab_config) if args.colab_config else None
    )
    sessions = client.list_sessions()
    usage = _try_get_usage(client)

    state_error: str | None = None
    local_state: SessionState | None = None
    try:
        local_state = read_session_state(args.session)
    except StateError as exc:
        state_error = str(exc)

    supervisor_pid = live_supervisor_pid(args.session)
    # A state file written by an earlier supervisor says nothing about this one.
    live_state = (
        local_state
        if local_state is not None and local_state.run.pid == supervisor_pid
        else None
    )
    local_healthy = False
    if live_state is not None:
        match live_state.phase:
            case WaitingHealth(traffic=traffic) | Ready(traffic=traffic):
                local_healthy, _ = probe_http_health(
                    port=traffic.tunnel_port,
                    health_path=get_engine(live_state.run.engine).health_path,
                    timeout_s=2.0,
                )
            case _:
                pass

    if args.json:
        print(
            json.dumps(
                {
                    "session": args.session,
                    "local_supervisor_alive": supervisor_pid is not None,
                    "local_supervisor_pid": supervisor_pid,
                    "local_endpoint_healthy": local_healthy,
                    "local_state": state_to_json(local_state) if local_state else None,
                    "local_state_error": state_error,
                    "remote_sessions": [asdict(s) for s in sessions],
                    "usage": asdict(usage) if usage else None,
                },
                indent=2,
            )
        )
        return 0

    print("Colab active sessions:")
    if not sessions:
        print("  (none)")
    else:
        for s in sessions:
            print(f"  [{s.name or '?'}] {s.endpoint} | {s.hardware} ({s.shape}) | {s.variant}")

    if usage is not None:
        print(
            f"Compute units: {usage.balance_cu:.2f} CU "
            f"| rate: {usage.rate_cu_per_hour:.2f}/hr "
            f"| active: {usage.active_assignments}"
        )

    label = f"Local supervisor [{args.session}]"
    if state_error is not None:
        print(f"{label}: state file unreadable: {state_error}", file=sys.stderr)
    if supervisor_pid is None:
        print(f"{label}: not running")
        if local_state is not None:
            print(f"  {_describe_last_state(local_state)}")
    elif live_state is None:
        print(f"{label}: pid={supervisor_pid} running; it has not recorded a state yet")
    else:
        print(f"{label}: {_describe_live_state(live_state, healthy=local_healthy)}")
    return 0


def _cmd_usage(args: argparse.Namespace) -> int:
    if args.history < 0:
        raise ValueError(f"--history must be 0 or greater, got {args.history}")
    client = ColabClient(
        colab_config=Path(args.colab_config) if args.colab_config else None
    )
    usage = client.get_usage()
    print(f"Current balance:    {usage.balance_cu:.2f} CU")
    print(f"Usage rate:         {usage.rate_cu_per_hour:.2f} CU/hr")
    print(f"Active assignments: {usage.active_assignments}")

    history = read_jsonl_tail(ledger_path(), limit=args.history)
    if history:
        print(f"\nRecent sessions ({len(history)}):")
        for rec in history:
            duration = format_duration(float(rec.get("duration_s", 0)))
            before, after = rec.get("cu_before"), rec.get("cu_after")
            delta_text = (
                f"{before - after:.2f} CU"
                if isinstance(before, (int, float)) and isinstance(after, (int, float))
                else "n/a"
            )
            print(
                f"  {rec.get('started_at', '?')}  [{rec.get('session')}]  "
                f"{rec.get('gpu')}  {rec.get('engine')}:{rec.get('model')}  "
                f"dur={duration}  cu={delta_text}  reason={rec.get('stop_reason')}"
            )
    return 0


def _cmd_logs(args: argparse.Namespace) -> int:
    validate_session_name(args.session)
    if args.lines < 1:
        raise ValueError(f"--lines must be 1 or greater, got {args.lines}")
    identity = resolve_ssh_identity(Path(args.identity) if args.identity else None)
    client = ColabClient(
        colab_config=Path(args.colab_config) if args.colab_config else None
    )
    control_path = _control_socket_path(args.session)
    line_count = int(args.lines)
    tail_cmd = (
        f"tail -n {line_count} -f {REMOTE_WORK_DIR}/server.log"
        if args.follow
        else f"tail -n {line_count} {REMOTE_WORK_DIR}/server.log"
    )
    return client.ssh_interactive(
        session=args.session,
        identity=identity,
        remote_cmd=["bash", "-lc", tail_cmd],
        control_path=control_path if control_path.exists() else None,
    )


def _cmd_ssh(args: argparse.Namespace) -> int:
    validate_session_name(args.session)
    identity = resolve_ssh_identity(Path(args.identity) if args.identity else None)
    client = ColabClient(
        colab_config=Path(args.colab_config) if args.colab_config else None
    )
    control_path = _control_socket_path(args.session)
    cmd = list(args.cmd) if args.cmd else None
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]
    return client.ssh_interactive(
        session=args.session,
        identity=identity,
        remote_cmd=cmd or None,
        control_path=control_path if control_path.exists() else None,
    )


def _cmd_ssh_config(args: argparse.Namespace) -> int:
    identity = Path(args.identity).expanduser() if args.identity else None
    colab_bin = args.colab_bin or find_colab_bin()
    block = render_ssh_config_block(
        colab_bin=colab_bin,
        session=args.session,
        local_port=args.port,
        remote_port=args.remote_port,
        identity=identity,
        rm_on_disconnect=args.rm,
        colab_config=Path(args.colab_config) if args.colab_config else None,
    )
    sys.stdout.write(block)
    return 0


def _cmd_engines(_args: argparse.Namespace) -> int:
    for engine in list_engines():
        print(f"{engine.name:10s}  default={engine.default_model}")
        print(f"            {engine.description}")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="colab-llm",
        description=(
            "Auditable, zero-idle on-demand LLM inference launcher for Google Colab G4 "
            "using official google-colab-cli SSH tunneling."
        ),
    )
    parser.add_argument("--version", action="version", version=f"colab-llm {__version__}")

    sub = parser.add_subparsers(dest="command", required=True)

    # start
    p_start = sub.add_parser("start", help="Allocate Colab GPU, bootstrap engine, tunnel locally")
    p_start.add_argument("-s", "--session", default=DEFAULT_SESSION, help=f"Colab session name (default: {DEFAULT_SESSION})")
    p_start.add_argument("--gpu", default=DEFAULT_GPU, choices=ALLOWED_GPUS, help=f"Colab GPU tier (default: {DEFAULT_GPU})")
    p_start.add_argument("--high-mem", action="store_true", help="Request the high-RAM shape (not available on single-shape GPUs: G4, L4)")
    p_start.add_argument("--engine", default=DEFAULT_ENGINE, choices=ALLOWED_ENGINES, help=f"Inference engine (default: {DEFAULT_ENGINE})")
    p_start.add_argument("--model", default=None, help="Model to serve: a Maya quant name, owner/repo, or owner/repo:quant for llama-cpp (default: per engine, see `colab-llm engines`)")
    p_start.add_argument("--hf-repo", default=None, help="Hugging Face repo (owner/repo) to serve instead of --model; for maya, a custom EXL3 repo")
    p_start.add_argument("--hf-revision", default=None, help="Branch/tag/commit of the served repo; not for llama-cpp")
    p_start.add_argument("--draft", default=None, choices=ALLOWED_MAYA_DRAFTS, help=f"Maya drafting mode; maya only (default: {DEFAULT_MAYA_DRAFT})")
    p_start.add_argument("--context", type=int, default=DEFAULT_CONTEXT_LENGTH, help=f"Context length in tokens (default: {DEFAULT_CONTEXT_LENGTH})")
    p_start.add_argument("--parallel", type=int, default=None, help=f"Concurrent request slots; maya and llama-cpp only (default: {DEFAULT_PARALLEL})")
    p_start.add_argument("--no-vision", action="store_true", help="Disable Maya image encoder build & download; maya only")
    p_start.add_argument("--web-ui", action="store_true", help="Enable Maya browser UI (default: API-only); maya only")
    p_start.add_argument("--host", default=DEFAULT_BIND_HOST, help=f"Local loopback bind address (default: {DEFAULT_BIND_HOST})")
    p_start.add_argument("--port", type=int, default=DEFAULT_LOCAL_PORT, help=f"Local loopback port (default: {DEFAULT_LOCAL_PORT})")
    p_start.add_argument("--remote-port", type=int, default=DEFAULT_REMOTE_PORT, help=f"Remote loopback port on Colab VM (default: {DEFAULT_REMOTE_PORT})")
    p_start.add_argument("--idle-timeout", default=f"{DEFAULT_IDLE_TIMEOUT_S // 60}m", help="Auto-stop after this long with no traffic through the proxy (default: 30m)")
    p_start.add_argument("--max-duration", default=f"{DEFAULT_MAX_DURATION_S // 3600}h", help="Hard max session duration before forced teardown (default: 4h)")
    p_start.add_argument("--bootstrap-timeout", default=f"{DEFAULT_BOOTSTRAP_TIMEOUT_S // 60}m", help="Timeout for the remote bootstrap, and again for the engine to report healthy (default: 30m)")
    p_start.add_argument("-i", "--identity", default=None, help="SSH ed25519/ecdsa key path (default: ~/.ssh/id_ed25519)")
    p_start.add_argument("--colab-config", default=None, help="Override google-colab-cli sessions.json path")
    p_start.add_argument("--hf-token-file", default=None, help="File containing optional HF token (or set HF_TOKEN_FILE / HF_TOKEN)")
    p_start.add_argument("--no-fast-hf", action="store_true", help="Disable fast Hugging Face Xet prefetch before Maya setup")
    p_start.add_argument("--rm-on-disconnect", action="store_true", help="Pass --rm to `colab ssh --proxy-mode` (tears down on any SSH drop)")
    p_start.add_argument("--reconfigure", action="store_true", help="Force re-running engine bootstrap even if server is already listening")
    p_start.add_argument(
        "--attach",
        nargs="?",
        const=Attach(),
        type=Attach,
        default=None,
        metavar="ENDPOINT",
        help="Attach to an existing unnamed Colab runtime (e.g. allocated in the Web UI when CLI /tun/m/assign returns 503)",
    )
    p_start.set_defaults(handler=_cmd_start)

    # stop
    p_stop = sub.add_parser("stop", help="Stop Colab session and local tunnel")
    stop_target = p_stop.add_mutually_exclusive_group()
    stop_target.add_argument("-s", "--session", default=None, help=f"Colab session name (default: {DEFAULT_SESSION})")
    stop_target.add_argument("--all", action="store_true", help="Stop all active Colab sessions")
    p_stop.add_argument("--colab-config", default=None, help="Override google-colab-cli sessions.json path")
    p_stop.set_defaults(handler=_cmd_stop)

    # status
    p_status = sub.add_parser("status", help="Show active Colab sessions, CU balance, and local watchdog status")
    p_status.add_argument("-s", "--session", default=DEFAULT_SESSION, help=f"Session name (default: {DEFAULT_SESSION})")
    p_status.add_argument("--json", action="store_true", help="Output machine-readable JSON")
    p_status.add_argument("--colab-config", default=None, help="Override google-colab-cli sessions.json path")
    p_status.set_defaults(handler=_cmd_status)

    # usage
    p_usage = sub.add_parser("usage", help="Show live Colab Compute Unit balance/rate and local session ledger")
    p_usage.add_argument("--history", type=int, default=10, help="Number of recent local sessions to display")
    p_usage.add_argument("--colab-config", default=None, help="Override google-colab-cli sessions.json path")
    p_usage.set_defaults(handler=_cmd_usage)

    # logs
    p_logs = sub.add_parser("logs", help="View or tail remote inference engine logs")
    p_logs.add_argument("-s", "--session", default=DEFAULT_SESSION, help=f"Session name (default: {DEFAULT_SESSION})")
    p_logs.add_argument("-n", "--lines", type=int, default=80, help="Number of log lines")
    p_logs.add_argument("-f", "--follow", action="store_true", help="Follow log output")
    p_logs.add_argument("-i", "--identity", default=None, help="SSH ed25519/ecdsa key path")
    p_logs.add_argument("--colab-config", default=None, help="Override google-colab-cli sessions.json path")
    p_logs.set_defaults(handler=_cmd_logs)

    # ssh
    p_ssh = sub.add_parser("ssh", help="Open SSH shell or run a command on the Colab session")
    p_ssh.add_argument("-s", "--session", default=DEFAULT_SESSION, help=f"Session name (default: {DEFAULT_SESSION})")
    p_ssh.add_argument("-i", "--identity", default=None, help="SSH ed25519/ecdsa key path")
    p_ssh.add_argument("--colab-config", default=None, help="Override google-colab-cli sessions.json path")
    p_ssh.add_argument("cmd", nargs=argparse.REMAINDER, help="Optional remote command")
    p_ssh.set_defaults(handler=_cmd_ssh)

    # ssh-config
    p_ssh_config = sub.add_parser("ssh-config", help="Print an auditable ~/.ssh/config snippet for Colab ProxyCommand")
    p_ssh_config.add_argument("-s", "--session", default=DEFAULT_SESSION, help=f"Session name (default: {DEFAULT_SESSION})")
    p_ssh_config.add_argument("--port", type=int, default=DEFAULT_LOCAL_PORT, help=f"Local forwarded port (default: {DEFAULT_LOCAL_PORT})")
    p_ssh_config.add_argument("--remote-port", type=int, default=DEFAULT_REMOTE_PORT, help=f"Remote port (default: {DEFAULT_REMOTE_PORT})")
    p_ssh_config.add_argument("-i", "--identity", default=None, help="SSH key path")
    p_ssh_config.add_argument("--rm", action="store_true", help="Include --rm in ProxyCommand")
    p_ssh_config.add_argument("--colab-bin", default=None, help="Explicit path to colab binary")
    p_ssh_config.add_argument("--colab-config", default=None, help="Override google-colab-cli sessions.json path")
    p_ssh_config.set_defaults(handler=_cmd_ssh_config)

    # engines
    p_engines = sub.add_parser("engines", help="List available modular inference engine adapters")
    p_engines.set_defaults(handler=_cmd_engines)

    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return int(args.handler(args))
    except (ValueError, FileNotFoundError, ColabError, SessionBusyError) as exc:
        print(f"[colab-llm] Error: {exc}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
