"""Command-line interface for colab-llm."""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import shlex
import signal
import sys
import time
from collections.abc import Sequence
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

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
    LaunchConfig,
    format_duration,
    parse_duration,
    resolve_hf_token,
    state_dir,
    validate_session_name,
)
from colab_llm.engines import engines_dir, get_engine, list_engines
from colab_llm.proxy import (
    append_jsonl,
    probe_http_health,
    read_jsonl_tail,
    run_supervised_session,
)


def _session_state_path(session: str) -> Path:
    validate_session_name(session)
    return state_dir() / f"{session}.json"


def _control_socket_path(session: str) -> Path:
    validate_session_name(session)
    # Keep Unix domain socket paths short (<104 chars on macOS).
    runtime_base = Path(os.environ.get("XDG_RUNTIME_DIR") or "/tmp")
    sock_dir = runtime_base / f"colab-llm-{os.getuid()}"
    sock_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    try:
        os.chmod(sock_dir, 0o700)
    except OSError:
        pass
    return sock_dir / f"{session}.sock"


def _ledger_path() -> Path:
    return state_dir() / "sessions.jsonl"


def _try_get_usage(client: ColabClient) -> UsageSnapshot | None:
    try:
        return client.get_usage()
    except ColabError as exc:
        print(f"[colab-llm] Warning: could not query `colab usage`: {exc}", file=sys.stderr)
        return None


def _print_ready_banner(cfg: LaunchConfig, health: dict[str, Any]) -> None:
    base = f"http://{cfg.bind_host}:{cfg.local_port}"
    model_id = health.get("model") or cfg.effective_model
    label = health.get("model_label") or model_id
    ctx = health.get("max_context") or cfg.context_length
    print()
    print("=" * 72)
    print(f"[colab-llm] READY — {label} on Colab {cfg.gpu} (session: {cfg.session})")
    print(f"  OpenAI API:     {base}/v1")
    print(f"  Anthropic API:  {base}  (endpoint: {base}/v1/messages)")
    print(f"  Health:         {base}{get_engine(cfg.engine).health_path}")
    print(f"  Context:        {ctx} tokens")
    print(
        f"  Auto-teardown:  idle={format_duration(cfg.idle_timeout_s)}, "
        f"hard_cap={format_duration(cfg.max_duration_s)}"
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


def _upload_and_bootstrap(
    client: ColabClient,
    cfg: LaunchConfig,
    identity: Path,
) -> None:
    engine = get_engine(cfg.engine)
    script_bytes = engine.script_path().read_bytes()
    deadman_path = engines_dir() / "deadman.py"
    if not deadman_path.is_file():
        raise FileNotFoundError(f"Missing remote deadman script: {deadman_path}")
    deadman_bytes = deadman_path.read_bytes()

    print(f"[colab-llm] Uploading {engine.name} bootstrap + deadman watchdog to VM...")
    client.ssh_exec(
        session=cfg.session,
        identity=identity,
        remote_command=(
            "install -d -m 700 /content/colab-llm "
            "&& cat > /content/colab-llm/bootstrap.sh "
            "&& chmod 700 /content/colab-llm/bootstrap.sh"
        ),
        stdin_bytes=script_bytes,
        timeout=120,
    )
    client.ssh_exec(
        session=cfg.session,
        identity=identity,
        remote_command=(
            "cat > /content/colab-llm/deadman.py "
            "&& chmod 700 /content/colab-llm/deadman.py"
        ),
        stdin_bytes=deadman_bytes,
        timeout=120,
    )

    hf_token = resolve_hf_token(cfg.hf_token_file)
    if hf_token is not None:
        print("[colab-llm] Staging HF token privately on VM (0600, deleted after download)...")
        client.ssh_exec(
            session=cfg.session,
            identity=identity,
            remote_command="umask 077 && cat > /content/colab-llm/.hf_token",
            stdin_bytes=(hf_token + "\n").encode("utf-8"),
            timeout=60,
        )

    env_vars = engine.build_env(cfg)
    env_assignments = " ".join(
        f"{k}={shlex.quote(v)}" for k, v in sorted(env_vars.items())
    )
    remote_cmd = f"env {env_assignments} bash /content/colab-llm/bootstrap.sh"
    print(f"[colab-llm] Running {engine.name} bootstrap on Colab {cfg.gpu}...")
    client.ssh_exec(
        session=cfg.session,
        identity=identity,
        remote_command=remote_cmd,
        stream_output=True,
        timeout=cfg.bootstrap_timeout_s,
    )


def _cmd_start(args: argparse.Namespace) -> int:
    idle_s = parse_duration(args.idle_timeout)
    max_s = parse_duration(args.max_duration)
    boot_s = parse_duration(args.bootstrap_timeout)

    cfg = LaunchConfig(
        session=args.session,
        gpu=args.gpu,
        high_mem=args.high_mem,
        engine=args.engine,
        model=args.model,
        hf_repo=args.hf_repo,
        hf_revision=args.hf_revision,
        draft=args.draft,
        context_length=args.context,
        parallel=args.parallel,
        vision=not args.no_vision,
        web_ui=args.web_ui,
        bind_host=args.host,
        local_port=args.port,
        remote_port=args.remote_port,
        idle_timeout_s=idle_s,
        max_duration_s=max_s,
        bootstrap_timeout_s=boot_s,
        fast_hf=not args.no_fast_hf,
        rm_on_disconnect=args.rm_on_disconnect,
        reconfigure=args.reconfigure,
        attach=args.attach is not None,
        attach_endpoint=args.attach if args.attach not in (None, "AUTO") else None,
        identity=Path(args.identity) if args.identity else None,
        colab_config=Path(args.colab_config) if args.colab_config else None,
        hf_token_file=Path(args.hf_token_file) if args.hf_token_file else None,
    )

    identity = resolve_ssh_identity(cfg.identity)
    client = ColabClient(colab_config=cfg.colab_config)
    engine = get_engine(cfg.engine)

    # Preflight auth + CU balance before allocating any billable GPU.
    who = client.whoami()
    print(f"[colab-llm] Authenticated: {who}")
    usage_before = _try_get_usage(client)
    if usage_before is not None:
        print(
            f"[colab-llm] CU balance: {usage_before.balance_cu:.2f} CU "
            f"(rate: {usage_before.rate_per_hour:.2f}/hr, "
            f"active: {usage_before.active_assignments})"
        )

    started_wall = datetime.now(UTC)
    started_mono = time.monotonic()
    info, created = client.ensure_session(
        session=cfg.session,
        gpu=cfg.gpu,
        high_mem=cfg.high_mem,
        attach=cfg.attach,
        attach_endpoint=cfg.attach_endpoint,
    )
    action = (
        "Allocated new"
        if created
        else ("Attached to existing" if cfg.attach else "Reusing existing")
    )
    print(
        f"[colab-llm] {action} session [{info.name}] ({info.endpoint}) "
        f"Hardware={info.hardware} Shape={info.shape}"
    )

    stop_reason = "error"
    final_requests = 0
    control_path = _control_socket_path(cfg.session)
    state_file = _session_state_path(cfg.session)

    try:
        _upload_and_bootstrap(client, cfg, identity)

        def _check_remote_pid() -> bool:
            # Only multiplex over the active ControlMaster socket; never open a
            # second competing `colab ssh` WebSocket session (Colab caps active
            # SSH sessions and returns HTTP 429). Also distinguish an explicit
            # DEAD server PID from a transient SSH transport error.
            if not control_path.exists():
                return True
            try:
                proc = client.ssh_exec(
                    session=cfg.session,
                    identity=identity,
                    control_path=control_path,
                    remote_command=(
                        "if test -f /content/colab-llm/server.pid "
                        "&& kill -0 $(cat /content/colab-llm/server.pid) 2>/dev/null; "
                        "then echo ALIVE; else echo DEAD; fi"
                    ),
                    timeout=20,
                )
            except ColabError:
                return True
            return proc.stdout.decode("utf-8", errors="ignore").strip() != "DEAD"

        stop_reason, stats = asyncio.run(
            run_supervised_session(
                session=cfg.session,
                engine=cfg.engine,
                model=cfg.effective_model,
                gpu=cfg.gpu,
                bind_host=cfg.bind_host,
                local_port=cfg.local_port,
                remote_port=cfg.remote_port,
                health_path=engine.health_path,
                idle_timeout_s=cfg.idle_timeout_s,
                max_duration_s=cfg.max_duration_s,
                startup_timeout_s=cfg.bootstrap_timeout_s,
                state_file=state_file,
                control_path=control_path,
                colab_bin=client.colab_bin,
                ssh_bin=client.ssh_bin,
                identity=identity,
                rm_on_disconnect=cfg.rm_on_disconnect,
                colab_config=cfg.colab_config,
                on_ready=lambda h: _print_ready_banner(cfg, h),
                check_remote_alive=_check_remote_pid,
            )
        )
        final_requests = stats.total_connections
        if stop_reason in ("remote_process_exited", "startup_timeout"):
            print(
                f"[colab-llm] Engine did not reach ready state ({stop_reason}). "
                "Last 40 lines of remote server.log:",
                file=sys.stderr,
            )
            try:
                client.ssh_exec(
                    session=cfg.session,
                    identity=identity,
                    force=True,
                    remote_command="tail -n 40 /content/colab-llm/server.log || true",
                    stream_output=True,
                    timeout=30,
                )
            except ColabError:
                pass
            return 1
        return 0
    except KeyboardInterrupt:
        stop_reason = "signal:SIGINT"
        return 0
    finally:
        print(f"[colab-llm] Tearing down Colab session {cfg.session!r} ({stop_reason})...")
        try:
            client.stop_session(cfg.session)
        except ColabError as exc:
            print(
                f"[colab-llm] WARNING: failed to stop session {cfg.session!r}: {exc}\n"
                f"  Run manually: colab stop -s {cfg.session}",
                file=sys.stderr,
            )
        usage_after = _try_get_usage(client)
        elapsed_s = round(time.monotonic() - started_mono, 1)
        cu_delta = (
            round(usage_before.balance_cu - usage_after.balance_cu, 3)
            if usage_before is not None and usage_after is not None
            else None
        )
        record: dict[str, Any] = {
            "session": cfg.session,
            "gpu": cfg.gpu,
            "engine": cfg.engine,
            "model": cfg.effective_model,
            "started_at": started_wall.isoformat(),
            "ended_at": datetime.now(UTC).isoformat(),
            "duration_s": elapsed_s,
            "requests": final_requests,
            "stop_reason": stop_reason,
            "cu_before": usage_before.balance_cu if usage_before else None,
            "cu_after": usage_after.balance_cu if usage_after else None,
            "cu_delta": cu_delta,
        }
        append_jsonl(_ledger_path(), record)
        if usage_after is not None:
            delta_str = f" (burned ~{cu_delta:.2f} CU)" if cu_delta is not None else ""
            print(
                f"[colab-llm] Session ended after {format_duration(elapsed_s)}. "
                f"Remaining balance: {usage_after.balance_cu:.2f} CU{delta_str}."
            )


def _stop_local_supervisor(session: str) -> None:
    path = _session_state_path(session)
    if not path.is_file():
        return
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        pid = int(data.get("pid", 0))
    except (ValueError, OSError):
        return
    if pid > 1 and pid != os.getpid():
        try:
            os.kill(pid, signal.SIGTERM)
            # Give the supervisor a moment to close its SSH tunnel cleanly.
            for _ in range(10):
                time.sleep(0.2)
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
        except ProcessLookupError:
            pass
        except PermissionError:
            pass


def _cmd_stop(args: argparse.Namespace) -> int:
    client = ColabClient(
        colab_config=Path(args.colab_config) if args.colab_config else None
    )
    if args.all:
        for state_file in state_dir().glob("*.json"):
            _stop_local_supervisor(state_file.stem)
        client.stop_all()
        print("[colab-llm] Stopped all active Colab sessions.")
        return 0

    validate_session_name(args.session)
    _stop_local_supervisor(args.session)
    stopped = client.stop_session(args.session)
    if stopped:
        print(f"[colab-llm] Stopped Colab session {args.session!r}.")
    else:
        print(f"[colab-llm] Session {args.session!r} is not active on Colab.")
    return 0


def _cmd_status(args: argparse.Namespace) -> int:
    validate_session_name(args.session)
    client = ColabClient(
        colab_config=Path(args.colab_config) if args.colab_config else None
    )
    sessions = client.list_sessions()
    usage = _try_get_usage(client)

    local_state: dict[str, Any] | None = None
    state_file = _session_state_path(args.session)
    if state_file.is_file():
        try:
            local_state = json.loads(state_file.read_text(encoding="utf-8"))
        except ValueError:
            local_state = None

    pid_alive = False
    local_healthy = False
    if local_state:
        pid = int(local_state.get("pid", 0))
        if pid > 1:
            try:
                os.kill(pid, 0)
                pid_alive = True
            except OSError:
                pid_alive = False
        if pid_alive and local_state.get("tunnel_port"):
            eng_name = str(local_state.get("engine") or DEFAULT_ENGINE)
            health_path = get_engine(eng_name).health_path
            local_healthy, _ = probe_http_health(
                port=int(local_state["tunnel_port"]),
                health_path=health_path,
                timeout_s=2.0,
            )

    if args.json:
        print(
            json.dumps(
                {
                    "session": args.session,
                    "local_supervisor_alive": pid_alive,
                    "local_endpoint_healthy": local_healthy,
                    "local_state": local_state,
                    "remote_sessions": [s.__dict__ for s in sessions],
                    "usage": usage.__dict__ if usage else None,
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
            print(f"  [{s.name}] {s.endpoint} | {s.hardware} ({s.shape}) | {s.variant}")

    if usage is not None:
        print(
            f"Compute units: {usage.balance_cu:.2f} CU "
            f"| rate: {usage.rate_per_hour:.2f}/hr "
            f"| active: {usage.active_assignments}"
        )

    if local_state and pid_alive:
        idle_left = max(
            0.0,
            float(local_state["idle_timeout_s"]) - float(local_state.get("idle_seconds", 0)),
        )
        cap_left = max(
            0.0,
            float(local_state["max_duration_s"]) - float(local_state.get("elapsed_seconds", 0)),
        )
        print(
            f"Local supervisor [{args.session}]: pid={local_state['pid']} "
            f"phase={local_state.get('phase')} healthy={local_healthy} "
            f"endpoint=http://{local_state['bind_host']}:{local_state['local_port']} "
            f"idle_left={format_duration(idle_left)} cap_left={format_duration(cap_left)}"
        )
    else:
        print(f"Local supervisor [{args.session}]: not running")
    return 0


def _cmd_usage(args: argparse.Namespace) -> int:
    client = ColabClient(
        colab_config=Path(args.colab_config) if args.colab_config else None
    )
    usage = client.get_usage()
    print(f"Current balance:    {usage.balance_cu:.2f} CU")
    print(f"Usage rate:         {usage.rate_per_hour:.2f} CU/hr")
    print(f"Active assignments: {usage.active_assignments}")

    history = read_jsonl_tail(_ledger_path(), limit=args.history)
    if history:
        print(f"\nRecent sessions ({len(history)}):")
        for rec in history:
            dur = format_duration(float(rec.get("duration_s", 0)))
            delta = rec.get("cu_delta")
            delta_s = f"{delta:.2f} CU" if isinstance(delta, (int, float)) else "n/a"
            print(
                f"  {rec.get('started_at', '?')}  [{rec.get('session')}]  "
                f"{rec.get('gpu')}  {rec.get('engine')}:{rec.get('model')}  "
                f"dur={dur}  cu={delta_s}  reason={rec.get('stop_reason')}"
            )
    return 0


def _cmd_logs(args: argparse.Namespace) -> int:
    validate_session_name(args.session)
    identity = resolve_ssh_identity(Path(args.identity) if args.identity else None)
    client = ColabClient(
        colab_config=Path(args.colab_config) if args.colab_config else None
    )
    ctl = _control_socket_path(args.session)
    n = max(1, int(args.lines))
    tail_cmd = (
        f"tail -n {n} -f /content/colab-llm/server.log"
        if args.follow
        else f"tail -n {n} /content/colab-llm/server.log"
    )
    return client.ssh_interactive(
        session=args.session,
        identity=identity,
        remote_cmd=["bash", "-lc", tail_cmd],
        control_path=ctl if ctl.exists() else None,
    )


def _cmd_ssh(args: argparse.Namespace) -> int:
    validate_session_name(args.session)
    identity = resolve_ssh_identity(Path(args.identity) if args.identity else None)
    client = ColabClient(
        colab_config=Path(args.colab_config) if args.colab_config else None
    )
    ctl = _control_socket_path(args.session)
    cmd = list(args.cmd) if args.cmd else None
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]
    return client.ssh_interactive(
        session=args.session,
        identity=identity,
        remote_cmd=cmd or None,
        control_path=ctl if ctl.exists() else None,
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
    for eng in list_engines():
        print(f"{eng.name:10s}  default={eng.default_model}")
        print(f"            {eng.description}")
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
    p_start.add_argument("--high-mem", action="store_true", help="Request high-RAM shape when supported")
    p_start.add_argument("--engine", default=DEFAULT_ENGINE, choices=ALLOWED_ENGINES, help=f"Inference engine (default: {DEFAULT_ENGINE})")
    p_start.add_argument("--model", default=None, help="Model name/quant (Maya default: EXL3-3.05bpw)")
    p_start.add_argument("--hf-repo", default=None, help="Custom Hugging Face repo (owner/repo) downloaded into --model-dir")
    p_start.add_argument("--hf-revision", default=None, help="Optional branch/tag/commit for --hf-repo")
    p_start.add_argument("--draft", default=DEFAULT_MAYA_DRAFT, choices=ALLOWED_MAYA_DRAFTS, help=f"Maya drafting mode (default: {DEFAULT_MAYA_DRAFT})")
    p_start.add_argument("--context", type=int, default=DEFAULT_CONTEXT_LENGTH, help=f"Context length in tokens (default: {DEFAULT_CONTEXT_LENGTH})")
    p_start.add_argument("--parallel", type=int, default=DEFAULT_PARALLEL, help=f"Concurrent request slots (default: {DEFAULT_PARALLEL})")
    p_start.add_argument("--no-vision", action="store_true", help="Disable Maya image encoder build & download")
    p_start.add_argument("--web-ui", action="store_true", help="Enable Maya browser UI (default: API-only)")
    p_start.add_argument("--host", default=DEFAULT_BIND_HOST, help=f"Local loopback bind address (default: {DEFAULT_BIND_HOST})")
    p_start.add_argument("--port", type=int, default=DEFAULT_LOCAL_PORT, help=f"Local loopback port (default: {DEFAULT_LOCAL_PORT})")
    p_start.add_argument("--remote-port", type=int, default=DEFAULT_REMOTE_PORT, help=f"Remote loopback port on Colab VM (default: {DEFAULT_REMOTE_PORT})")
    p_start.add_argument("--idle-timeout", default=f"{DEFAULT_IDLE_TIMEOUT_S // 60}m", help="Auto-stop after idle duration with 0 active requests (default: 30m)")
    p_start.add_argument("--max-duration", default=f"{DEFAULT_MAX_DURATION_S // 3600}h", help="Hard max session duration before forced teardown (default: 4h)")
    p_start.add_argument("--bootstrap-timeout", default=f"{DEFAULT_BOOTSTRAP_TIMEOUT_S // 60}m", help="Timeout for download/build/load startup (default: 30m)")
    p_start.add_argument("-i", "--identity", default=None, help="SSH ed25519/ecdsa key path (default: ~/.ssh/id_ed25519)")
    p_start.add_argument("--colab-config", default=None, help="Override google-colab-cli sessions.json path")
    p_start.add_argument("--hf-token-file", default=None, help="File containing optional HF token (or set HF_TOKEN_FILE / HF_TOKEN)")
    p_start.add_argument("--no-fast-hf", action="store_true", help="Disable fast Hugging Face Xet prefetch before Maya setup")
    p_start.add_argument("--rm-on-disconnect", action="store_true", help="Pass --rm to `colab ssh --proxy-mode` (tears down on any SSH drop)")
    p_start.add_argument("--reconfigure", action="store_true", help="Force re-running engine bootstrap even if server is already listening")
    p_start.add_argument(
        "--attach",
        nargs="?",
        const="AUTO",
        default=None,
        metavar="ENDPOINT",
        help="Attach to an existing unnamed Colab runtime (e.g. allocated in the Web UI when CLI /tun/m/assign returns 503)",
    )
    p_start.set_defaults(handler=_cmd_start)

    # stop
    p_stop = sub.add_parser("stop", help="Stop Colab session and local tunnel")
    p_stop.add_argument("-s", "--session", default=DEFAULT_SESSION, help=f"Colab session name (default: {DEFAULT_SESSION})")
    p_stop.add_argument("--all", action="store_true", help="Stop all active Colab sessions")
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
    p_sshcfg = sub.add_parser("ssh-config", help="Print an auditable ~/.ssh/config snippet for Colab ProxyCommand")
    p_sshcfg.add_argument("-s", "--session", default=DEFAULT_SESSION, help=f"Session name (default: {DEFAULT_SESSION})")
    p_sshcfg.add_argument("--port", type=int, default=DEFAULT_LOCAL_PORT, help=f"Local forwarded port (default: {DEFAULT_LOCAL_PORT})")
    p_sshcfg.add_argument("--remote-port", type=int, default=DEFAULT_REMOTE_PORT, help=f"Remote port (default: {DEFAULT_REMOTE_PORT})")
    p_sshcfg.add_argument("-i", "--identity", default=None, help="SSH key path")
    p_sshcfg.add_argument("--rm", action="store_true", help="Include --rm in ProxyCommand")
    p_sshcfg.add_argument("--colab-bin", default=None, help="Explicit path to colab binary")
    p_sshcfg.add_argument("--colab-config", default=None, help="Override google-colab-cli sessions.json path")
    p_sshcfg.set_defaults(handler=_cmd_ssh_config)

    # engines
    p_eng = sub.add_parser("engines", help="List available modular inference engine adapters")
    p_eng.set_defaults(handler=_cmd_engines)

    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return int(args.handler(args))
    except (ValueError, FileNotFoundError, ColabError) as exc:
        print(f"[colab-llm] Error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
