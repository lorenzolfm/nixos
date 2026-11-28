"""Unit and async loopback integration tests for colab-llm."""

from __future__ import annotations

import asyncio
import importlib.util
import json
import os
import stat
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

import pytest

from colab_llm import cli
from colab_llm.cli import main as cli_main
from colab_llm.colab import (
    ColabClient,
    ColabError,
    ColabSessionInfo,
    build_proxy_command,
    build_ssh_tunnel_args,
    parse_sessions_output,
    parse_usage_output,
    render_ssh_config_block,
    resolve_ssh_identity,
)
from colab_llm.config import (
    DEADMAN_HEARTBEAT_GRACE_S,
    DEFAULT_MAYA_MODEL,
    HEARTBEAT_INTERVAL_S,
    MAYA_PINNED_COMMIT,
    Attach,
    Draft,
    Engine,
    HfModel,
    LaunchConfig,
    LlamaCppConfig,
    MayaConfig,
    StopReason,
    VllmConfig,
    build_engine_config,
    exit_code_for,
    parse_duration,
    resolve_hf_token,
    validate_loopback_host,
    validate_session_name,
)
from colab_llm.engines import engines_dir, get_engine, list_engines
from colab_llm.proxy import (
    ActivityTrackingProxy,
    TunnelSupervisor,
    bind_loopback_listener,
    pick_free_loopback_port,
    probe_http_health,
)
from colab_llm.state import (
    Allocating,
    Ready,
    RunInfo,
    SessionBusyError,
    SessionState,
    StateError,
    Stopped,
    Traffic,
    acquire_session_lock,
    append_jsonl,
    live_supervisor_pid,
    parse_state,
    read_jsonl_tail,
    state_to_json,
    write_json_atomic,
)


def _load_deadman_module():
    path = engines_dir() / "deadman.py"
    spec = importlib.util.spec_from_file_location("colab_llm_deadman", path)
    assert spec and spec.loader
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


# --------------------------------------------------------------------------- config


def test_default_launch_config_matches_issue_457_defaults() -> None:
    cfg = LaunchConfig()
    assert cfg.gpu == "G4"
    assert isinstance(cfg.engine, MayaConfig)
    assert cfg.engine.kind == "maya"
    assert cfg.engine.served_model == DEFAULT_MAYA_MODEL == "EXL3-3.05bpw"
    assert cfg.engine.draft == "dflash2"
    assert cfg.context_length == 131072
    assert cfg.bind_host == "127.0.0.1"
    assert cfg.local_port == 18100
    assert cfg.remote_port == 18100
    assert cfg.idle_timeout_s == 1800
    assert cfg.max_duration_s == 14400
    assert cfg.engine.web_ui is False
    assert cfg.engine.vision is True


@pytest.mark.parametrize("bad_host", ["0.0.0.0", "::", "10.0.1.4", "example.com"])
def test_non_loopback_bind_host_rejected(bad_host: str) -> None:
    with pytest.raises(ValueError, match="loopback"):
        validate_loopback_host(bad_host)
    with pytest.raises(ValueError, match="loopback"):
        LaunchConfig(bind_host=bad_host)


@pytest.mark.parametrize("bad_name", ["", "../escape", "bad name", "a;rm -rf /", "-lead"])
def test_invalid_session_name_rejected(bad_name: str) -> None:
    with pytest.raises(ValueError, match="Invalid session name"):
        validate_session_name(bad_name)


def test_timeout_invariants_enforced() -> None:
    with pytest.raises(ValueError, match="cannot exceed"):
        LaunchConfig(idle_timeout_s=3600, max_duration_s=1800)
    with pytest.raises(ValueError, match="out of bounds"):
        LaunchConfig(idle_timeout_s=10)
    with pytest.raises(ValueError, match="Unsupported GPU"):
        LaunchConfig(gpu="RTX9999")
    # Colab rejects the high-RAM shape on G4 after allocation was requested;
    # the flag is refused up front instead of being dropped.
    with pytest.raises(ValueError, match="--high-mem is not available on G4"):
        LaunchConfig(gpu="G4", high_mem=True)
    assert LaunchConfig(gpu="A100", high_mem=True).high_mem is True


@pytest.mark.parametrize("engine", list(Engine))
def test_every_engine_resolves_a_model_before_any_gpu_is_allocated(engine: Engine) -> None:
    # A config that constructs must be launchable: a missing model may not
    # surface later, after a billable runtime exists.
    config = LaunchConfig(engine=build_engine_config(engine))
    assert config.engine.kind == engine
    assert config.engine.served_model == get_engine(engine).default_model
    assert get_engine(engine).build_env(config)
    custom = build_engine_config(engine, model="owner/custom")
    assert custom.served_model == "owner/custom"


def test_engine_flags_for_the_wrong_engine_are_rejected_not_dropped() -> None:
    # Each of these used to be accepted and silently ignored, so the run was
    # not the one the command line described.
    with pytest.raises(ValueError, match="--hf-revision does not apply to --engine llama-cpp"):
        build_engine_config(Engine.LLAMA_CPP, hf_repo="owner/repo", hf_revision="main")
    with pytest.raises(ValueError, match="--draft does not apply to --engine vllm"):
        build_engine_config(Engine.VLLM, draft="mtp")
    with pytest.raises(ValueError, match="--parallel does not apply to --engine sglang"):
        build_engine_config(Engine.SGLANG, parallel=2)
    with pytest.raises(ValueError, match="--no-vision does not apply to --engine llama-cpp"):
        build_engine_config(Engine.LLAMA_CPP, no_vision=True)
    with pytest.raises(ValueError, match="--hf-revision requires --hf-repo"):
        build_engine_config(Engine.MAYA, hf_revision="main")
    for engine in Engine:
        with pytest.raises(ValueError, match="pass only one"):
            build_engine_config(engine, model="owner/a", hf_repo="owner/b")


def test_the_recorded_model_is_the_served_model() -> None:
    # One value feeds both the remote environment and the banner/state/ledger.
    llama = build_engine_config(Engine.LLAMA_CPP, hf_repo="owner/gguf")
    assert isinstance(llama, LlamaCppConfig)
    env = get_engine(Engine.LLAMA_CPP).build_env(LaunchConfig(engine=llama))
    assert env["LLAMA_HF_REPO"] == llama.served_model == "owner/gguf"

    vllm = build_engine_config(Engine.VLLM, model="owner/model", hf_revision="v2")
    assert vllm == VllmConfig(model=HfModel("owner/model", "v2"))
    env = get_engine(Engine.VLLM).build_env(LaunchConfig(engine=vllm))
    assert (env["VLLM_MODEL"], env["VLLM_REVISION"]) == ("owner/model", "v2")


def test_config_fingerprint_follows_what_is_served_and_nothing_else() -> None:
    # A healthy server left on the VM is reused only when the fingerprint
    # matches, so it must change with the model and not with the timeouts.
    spec = get_engine(Engine.MAYA)

    def fingerprint(config: LaunchConfig) -> str:
        return spec.build_env(config)["COLAB_LLM_CONFIG_FINGERPRINT"]

    base = fingerprint(LaunchConfig())
    assert fingerprint(LaunchConfig(idle_timeout_s=600, reconfigure=True)) == base
    assert fingerprint(LaunchConfig(engine=MayaConfig(draft=Draft.OFF))) != base
    assert fingerprint(LaunchConfig(engine=MayaConfig(model="EXL3-4.05bpw"))) != base
    assert fingerprint(LaunchConfig(context_length=8192)) != base


def test_exit_code_is_decided_by_the_stop_reason() -> None:
    # A tunnel that never came up used to exit 0 through a catch-all branch.
    assert exit_code_for(StopReason.TUNNEL_DISCONNECTED, was_ready=False) == 1
    assert exit_code_for(StopReason.TUNNEL_DISCONNECTED, was_ready=True) == 0
    assert exit_code_for(StopReason.HEALTH_TIMEOUT, was_ready=False) == 1
    assert exit_code_for(StopReason.IDLE_TIMEOUT, was_ready=True) == 0
    assert exit_code_for(StopReason.SIGNAL_SIGINT, was_ready=False) == 0
    assert exit_code_for(StopReason.ERROR, was_ready=True) == 1


@pytest.mark.parametrize(
    ("spec", "expected"),
    [
        ("30m", 1800),
        ("4h", 14400),
        ("1h30m", 5400),
        ("90s", 90),
        ("1800", 1800),
        ("2h15m10s", 8110),
    ],
)
def test_parse_duration_valid(spec: str, expected: int) -> None:
    assert parse_duration(spec) == expected


@pytest.mark.parametrize("bad", ["", "0m", "-5m", "30d", "abc"])
def test_parse_duration_invalid(bad: str) -> None:
    with pytest.raises(ValueError):
        parse_duration(bad)


def test_resolve_hf_token_precedence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    explicit = tmp_path / "explicit"
    explicit.write_text("tok_explicit\n", encoding="utf-8")
    env_file = tmp_path / "env_file"
    env_file.write_text("tok_env_file\n", encoding="utf-8")

    monkeypatch.setenv("HF_TOKEN", "tok_env")
    monkeypatch.setenv("HF_TOKEN_FILE", str(env_file))
    assert resolve_hf_token(explicit) == "tok_explicit"
    assert resolve_hf_token(None) == "tok_env_file"

    monkeypatch.delenv("HF_TOKEN_FILE")
    assert resolve_hf_token(None) == "tok_env"

    monkeypatch.delenv("HF_TOKEN")
    assert resolve_hf_token(None) is None


# --------------------------------------------------------------------------- colab & ssh


def test_parse_usage_output() -> None:
    sample = (
        "Current balance: 398.42 compute units\n"
        "Usage rate: 6.71/hr\n"
        "Active assignments: 1\n"
    )
    snap = parse_usage_output(sample)
    assert snap.balance_cu == pytest.approx(398.42)
    assert snap.rate_cu_per_hour == pytest.approx(6.71)
    assert snap.active_assignments == 1


def test_parse_sessions_output() -> None:
    sample = (
        "[colab-llm] m-s-abc123 | Hardware: G4 | Shape: High-RAM | Variant: GPU\n"
        "[other] m-s-def456 | Hardware: L4 | Shape: Standard | Variant: GPU | Status: IDLE\n"
    )
    sessions = parse_sessions_output(sample)
    assert len(sessions) == 2
    assert sessions[0] == ColabSessionInfo(
        name="colab-llm",
        endpoint="m-s-abc123",
        hardware="G4",
        shape="High-RAM",
        variant="GPU",
        status=None,
    )
    assert sessions[1].status == "IDLE"
    # Hardware is normalised once here; an unnamed assignment has no name.
    unnamed = parse_sessions_output("[?] m-s-web | Hardware: g4 | Shape: Standard | Variant: GPU\n")
    assert (unnamed[0].name, unnamed[0].hardware) == (None, "G4")


def test_parse_sessions_output_skips_cli_notices_but_rejects_unknown_lines() -> None:
    assert parse_sessions_output("[colab] No active sessions found on server.\n") == []
    # An unrecognised line could be a running session in a new format; reporting
    # it as "no sessions" would let teardown skip the stop.
    with pytest.raises(ColabError, match="Could not parse"):
        parse_sessions_output("colab-llm  m-s-abc123  G4  running\n")


def _session(name: str) -> ColabSessionInfo:
    return ColabSessionInfo(
        name=name, endpoint="m-s-1", hardware="G4", shape="Standard", variant="GPU"
    )


def test_teardown_session_always_stops_and_verifies(monkeypatch: pytest.MonkeyPatch) -> None:
    client = ColabClient(colab_bin="/bin/true", ssh_bin="/bin/true")
    stops: list[list[str]] = []

    def _fake_run(args: list[str], **_kwargs: object) -> subprocess.CompletedProcess[str]:
        stops.append(list(args))
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(client, "_run_colab", _fake_run)

    monkeypatch.setattr(client, "list_sessions", list)
    client.teardown_session("colab-llm")
    assert stops == [["stop", "-s", "colab-llm"]]

    monkeypatch.setattr(client, "list_sessions", lambda: [_session("colab-llm")])
    with pytest.raises(ColabError, match="still listed"):
        client.teardown_session("colab-llm")


def test_ensure_session_stops_runtime_when_creation_fails_part_way(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = ColabClient(colab_bin="/bin/true", ssh_bin="/bin/true")
    commands: list[list[str]] = []

    def _fake_run(args: list[str], **_kwargs: object) -> subprocess.CompletedProcess[str]:
        commands.append(list(args))
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    # `colab new` succeeds but the session never shows up in the listing.
    monkeypatch.setattr(client, "_run_colab", _fake_run)
    monkeypatch.setattr(client, "list_sessions", list)
    with pytest.raises(ColabError, match="did not appear"):
        client.ensure_session(session="colab-llm", gpu="G4")
    assert commands == [
        ["new", "-s", "colab-llm", "--gpu", "G4"],
        ["stop", "-s", "colab-llm"],
    ]


def test_resolve_ssh_identity_prefers_ed25519_and_rejects_rsa(tmp_path: Path) -> None:
    (tmp_path / "id_rsa").write_text("priv", encoding="utf-8")
    (tmp_path / "id_rsa.pub").write_text("ssh-rsa AAAAB3 test@host\n", encoding="utf-8")
    with pytest.raises(ColabError, match="No supported SSH key"):
        resolve_ssh_identity(None, ssh_dir=tmp_path)
    with pytest.raises(ColabError, match="ssh-rsa"):
        resolve_ssh_identity(tmp_path / "id_rsa", ssh_dir=tmp_path)

    (tmp_path / "id_ed25519").write_text("priv", encoding="utf-8")
    (tmp_path / "id_ed25519.pub").write_text("ssh-ed25519 AAAAC3 test@host\n", encoding="utf-8")
    assert resolve_ssh_identity(None, ssh_dir=tmp_path) == tmp_path / "id_ed25519"
    assert (
        resolve_ssh_identity(tmp_path / "id_ed25519.pub", ssh_dir=tmp_path)
        == tmp_path / "id_ed25519"
    )


def test_proxy_command_and_tunnel_args_use_official_transport_only(tmp_path: Path) -> None:
    key = tmp_path / "id_ed25519"
    key.write_text("priv", encoding="utf-8")
    proxy_cmd = build_proxy_command(
        colab_bin="/nix/store/fake/bin/colab",
        session="colab-llm",
        identity=key,
        rm_on_disconnect=True,
    )
    assert "/nix/store/fake/bin/colab" in proxy_cmd
    assert "ssh --proxy-mode -s colab-llm" in proxy_cmd
    assert "--rm" in proxy_cmd

    args = build_ssh_tunnel_args(
        ssh_bin="/nix/store/fake/bin/ssh",
        proxy_command=proxy_cmd,
        bind_host="127.0.0.1",
        local_forward_port=19100,
        remote_port=18100,
        identity=key,
        control_path=tmp_path / "ctl.sock",
    )
    assert "-L" in args
    assert "127.0.0.1:19100:127.0.0.1:18100" in args
    assert "ExitOnForwardFailure=yes" in args
    assert "ControlMaster=auto" in args
    assert args[-1] == "root@colab-runtime"

    block = render_ssh_config_block(
        colab_bin="/nix/store/fake/bin/colab",
        session="colab-llm",
        local_port=18100,
        remote_port=18100,
        identity=key,
    )
    assert "LocalForward 127.0.0.1:18100 127.0.0.1:18100" in block
    assert "ProxyCommand /nix/store/fake/bin/colab ssh --proxy-mode -s colab-llm" in block


def test_ensure_session_rejects_gpu_mismatch(monkeypatch: pytest.MonkeyPatch) -> None:
    client = ColabClient(colab_bin="/bin/true", ssh_bin="/bin/true")
    monkeypatch.setattr(
        client,
        "list_sessions",
        lambda: [
            ColabSessionInfo(
                name="colab-llm",
                endpoint="m-s-1",
                hardware="T4",
                shape="Standard",
                variant="GPU",
            )
        ],
    )
    with pytest.raises(ColabError, match="already exists on hardware 'T4'"):
        client.ensure_session(session="colab-llm", gpu="G4")


def test_ensure_session_creates_the_requested_session(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = ColabClient(colab_bin="/bin/true", ssh_bin="/bin/true")
    seen_args: list[list[str]] = []
    calls = {"list": 0}

    def _fake_list() -> list[ColabSessionInfo]:
        calls["list"] += 1
        if calls["list"] == 1:
            return []
        return [
            ColabSessionInfo(
                name="colab-llm",
                endpoint="m-s-g4",
                hardware="G4",
                shape="Standard",
                variant="GPU",
            )
        ]

    def _fake_run(args: list[str], **_kwargs: object) -> None:
        seen_args.append(list(args))

    monkeypatch.setattr(client, "list_sessions", _fake_list)
    monkeypatch.setattr(client, "_run_colab", _fake_run)

    info, created = client.ensure_session(session="colab-llm", gpu="G4")
    assert created is True
    assert info.hardware == "G4"
    assert seen_args == [["new", "-s", "colab-llm", "--gpu", "G4"]]


def test_attach_session_adopts_unnamed_web_ui_runtime(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    sessions_file = tmp_path / "sessions.json"
    client = ColabClient(
        colab_bin="/bin/true",
        ssh_bin="/bin/true",
        colab_config=sessions_file,
    )
    calls = {"n": 0}

    def _fake_list() -> list[ColabSessionInfo]:
        calls["n"] += 1
        if calls["n"] == 1:
            return [
                ColabSessionInfo(
                    name=None,
                    endpoint="m-s-webui123",
                    hardware="G4",
                    shape="High-RAM",
                    variant="GPU",
                )
            ]
        return [
            ColabSessionInfo(
                name="colab-llm",
                endpoint="m-s-webui123",
                hardware="G4",
                shape="High-RAM",
                variant="GPU",
            )
        ]

    monkeypatch.setattr(client, "list_sessions", _fake_list)
    info, created = client.ensure_session(session="colab-llm", gpu="G4", attach=Attach())
    assert created is False
    assert info.name == "colab-llm"
    assert info.endpoint == "m-s-webui123"
    saved = json.loads(sessions_file.read_text(encoding="utf-8"))
    assert saved["colab-llm"]["endpoint"] == "m-s-webui123"
    assert saved["colab-llm"]["accelerator"] == "G4"
    assert saved["colab-llm"]["machine_shape"] == "HIGH_RAM"
    assert stat.S_IMODE(sessions_file.stat().st_mode) == 0o600


def test_attach_refuses_to_overwrite_a_sessions_file_it_does_not_understand(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The file belongs to google-colab-cli. Treating an unexpected shape as
    # empty would erase every session it tracks.
    sessions_file = tmp_path / "sessions.json"
    sessions_file.write_text('["not", "an", "object"]', encoding="utf-8")
    client = ColabClient(
        colab_bin="/bin/true", ssh_bin="/bin/true", colab_config=sessions_file
    )
    monkeypatch.setattr(
        client,
        "list_sessions",
        lambda: [
            ColabSessionInfo(
                name=None, endpoint="m-s-1", hardware="G4", shape="Standard", variant="GPU"
            )
        ],
    )
    with pytest.raises(ColabError, match="refusing to overwrite"):
        client.attach_session(session="colab-llm", gpu="G4")
    assert sessions_file.read_text(encoding="utf-8") == '["not", "an", "object"]'


def test_stop_all_also_terminates_unnamed_server_sessions(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    client = ColabClient(
        colab_bin="/bin/true",
        ssh_bin="/bin/true",
        colab_config=tmp_path / "sessions.json",
    )
    calls: list[list[str]] = []
    attached: list[str] = []
    stopped: list[str] = []

    monkeypatch.setattr(client, "_run_colab", lambda args, **_kw: calls.append(list(args)))
    monkeypatch.setattr(
        client,
        "list_sessions",
        lambda: [
            ColabSessionInfo(
                name=None,
                endpoint="m-s-stray-a100",
                hardware="A100",
                shape="High-RAM",
                variant="GPU",
            )
        ],
    )
    monkeypatch.setattr(
        client,
        "attach_session",
        lambda *, session, gpu, endpoint=None: attached.append(f"{session}:{gpu}:{endpoint}"),
    )
    monkeypatch.setattr(client, "teardown_session", stopped.append)

    client.stop_all()
    assert calls == [["stop", "--all"]]
    assert attached == ["colab-llm-cleanup-0:A100:m-s-stray-a100"]
    assert stopped == ["colab-llm-cleanup-0"]


# --------------------------------------------------------------------------- engines


def test_engines_scripts_exist_and_forbid_drive_mount_or_third_party_tunnels() -> None:
    engines = list_engines()
    assert {e.name for e in engines} == {"maya", "vllm", "llama-cpp", "sglang"}
    for eng in engines:
        text = eng.script_path().read_text(encoding="utf-8")
        # Security & AUP guardrails: never mount Google Drive (#455 isolation)
        # and never install/invoke third-party reverse tunnels.
        assert "drive.mount" not in text
        assert "drivemount" not in text
        assert "ngrok" not in text
        assert "cloudflared" not in text
        assert "/nix/store/" not in text
        assert "127.0.0.1" in text
    deadman_text = (engines_dir() / "deadman.py").read_text(encoding="utf-8")
    assert "/nix/store/" not in deadman_text


def test_maya_env_pins_commit_and_known_hf_prefetch() -> None:
    eng = get_engine("maya")
    env = eng.build_env(LaunchConfig())
    assert env["MAYA_PINNED_COMMIT"] == MAYA_PINNED_COMMIT
    assert env["MAYA_MODEL"] == "EXL3-3.05bpw"
    assert env["MAYA_DRAFT"] == "dflash2"
    assert env["MAYA_HF_REPO"] == "turboderp/GLM-5.3-Flash-exl3"
    assert env["MAYA_HF_REVISION"] == "332ab457b709b7ba30dd9a448be5de03b80a7ac9"
    assert env["MAYA_CUSTOM_MODEL_DIR"] == "0"
    assert env["MAYA_WEB_UI"] == "0"

    # The deadman must tolerate at least two missed supervisor heartbeats, and
    # its hard cap must outlast bootstrap + health wait + the local cap.
    config = LaunchConfig()
    heartbeat_timeout_s = int(env["COLAB_LLM_DEADMAN_HEARTBEAT_TIMEOUT_S"])
    assert heartbeat_timeout_s == config.idle_timeout_s + DEADMAN_HEARTBEAT_GRACE_S
    assert heartbeat_timeout_s >= config.idle_timeout_s + 2 * HEARTBEAT_INTERVAL_S
    assert int(env["COLAB_LLM_DEADMAN_DURATION_S_MAX"]) > (
        config.max_duration_s + 2 * config.bootstrap_timeout_s
    )

    custom_env = eng.build_env(
        LaunchConfig(
            engine=build_engine_config(
                Engine.MAYA, hf_repo="turboderp/GLM-5.3-Flash-exl3", hf_revision="sc_2.60bpw"
            )
        )
    )
    assert custom_env["MAYA_CUSTOM_MODEL_DIR"] == "1"
    assert custom_env["MAYA_MODEL"] == ""
    assert custom_env["MAYA_HF_REPO"] == "turboderp/GLM-5.3-Flash-exl3"
    assert custom_env["MAYA_HF_REVISION"] == "sc_2.60bpw"


# --------------------------------------------------------------------------- proxy & health


def test_activity_tracking_proxy_relays_traffic_and_tracks_idle() -> None:
    async def _run() -> None:
        fake_now = [1000.0]

        async def _echo_handler(
            reader: asyncio.StreamReader, writer: asyncio.StreamWriter
        ) -> None:
            try:
                while data := await reader.read(4096):
                    writer.write(data)
                    await writer.drain()
            finally:
                writer.close()
                await writer.wait_closed()

        upstream_port = pick_free_loopback_port()
        proxy_port = pick_free_loopback_port()
        upstream = await asyncio.start_server(_echo_handler, "127.0.0.1", upstream_port)
        proxy = ActivityTrackingProxy(
            listener=bind_loopback_listener("127.0.0.1", proxy_port),
            upstream_port=upstream_port,
            clock=lambda: fake_now[0],
        )
        await proxy.start()
        try:
            # Open a connection and hold it open, silent, past the idle timeout:
            # an open socket is not traffic, so it must not prevent expiration.
            reader, writer = await asyncio.open_connection("127.0.0.1", proxy_port)
            writer.write(b"hello colab-llm")
            await writer.drain()
            echoed = await reader.readexactly(len(b"hello colab-llm"))
            assert echoed == b"hello colab-llm"

            fake_now[0] = 5000.0
            assert proxy.active_connections == 1
            assert proxy.stats.idle_s(fake_now[0]) == 4000.0
            assert proxy.stats.is_idle_expired(1800, fake_now[0])

            # Bytes moving through the proxy are traffic and reset the timer.
            writer.write(b"again")
            await writer.drain()
            assert await reader.readexactly(5) == b"again"
            assert not proxy.stats.is_idle_expired(1800, fake_now[0])

            # Close connection; activity timestamp updates to 5000.0.
            writer.close()
            await writer.wait_closed()
            await asyncio.sleep(0.05)

            assert proxy.active_connections == 0
            assert proxy.stats.total_connections == 1
            assert proxy.stats.bytes_in == len(b"hello colab-llmagain")
            assert proxy.stats.bytes_out == len(b"hello colab-llmagain")
            assert not proxy.stats.is_idle_expired(1800, 6799.0)
            assert proxy.stats.is_idle_expired(1800, 6800.0)
            assert proxy.stats.is_hard_cap_expired(14400, 15400.0)
        finally:
            await proxy.close()
            upstream.close()
            await upstream.wait_closed()

    asyncio.run(_run())


def test_proxy_close_returns_while_a_client_is_still_connected() -> None:
    # Teardown of the billable runtime waits on close(), so a client that never
    # disconnects must not be able to block it.
    async def _run() -> None:
        async def _echo_handler(
            reader: asyncio.StreamReader, writer: asyncio.StreamWriter
        ) -> None:
            while data := await reader.read(4096):
                writer.write(data)
                await writer.drain()
            writer.close()

        upstream = await asyncio.start_server(_echo_handler, "127.0.0.1", 0)
        upstream_port = upstream.sockets[0].getsockname()[1]
        proxy = ActivityTrackingProxy(
            listener=bind_loopback_listener("127.0.0.1", pick_free_loopback_port()),
            upstream_port=upstream_port,
        )
        await proxy.start()
        reader, writer = await asyncio.open_connection("127.0.0.1", proxy.listen_port)
        writer.write(b"ping")
        await writer.drain()
        assert await reader.readexactly(4) == b"ping"
        assert proxy.active_connections == 1

        await asyncio.wait_for(proxy.close(), timeout=3.0)
        assert proxy.active_connections == 0
        # The held connection was dropped rather than waited for.
        assert await asyncio.wait_for(reader.read(), timeout=3.0) == b""

        writer.close()
        upstream.close()
        await upstream.wait_closed()

    asyncio.run(_run())


def test_probe_http_health_states() -> None:
    state: dict[str, object] = {"status": "reloading", "unloaded": False}

    class _Handler(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            body = json.dumps(state).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, format: str, *args: object) -> None:
            pass

    port = pick_free_loopback_port()
    srv = HTTPServer(("127.0.0.1", port), _Handler)
    thread = threading.Thread(target=srv.serve_forever, daemon=True)
    thread.start()
    try:
        ok, data = probe_http_health(port=port)
        assert ok is False
        assert data and data["status"] == "reloading"

        state["status"] = "ok"
        state["unloaded"] = True
        ok, _ = probe_http_health(port=port)
        assert ok is False

        state["unloaded"] = False
        ok, data = probe_http_health(port=port)
        assert ok is True
        assert data and data["status"] == "ok"
    finally:
        srv.shutdown()
        srv.server_close()


def test_deadman_trigger_logic() -> None:
    deadman = _load_deadman_module()
    assert (
        deadman.should_trigger(
            now=1000.0,
            started_at=0.0,
            last_heartbeat=900.0,
            max_duration_s=14400,
            heartbeat_timeout_s=2400,
        )
        is None
    )
    assert (
        deadman.should_trigger(
            now=3400.0,
            started_at=0.0,
            last_heartbeat=900.0,
            max_duration_s=14400,
            heartbeat_timeout_s=2400,
        )
        == "heartbeat_stale"
    )
    assert (
        deadman.should_trigger(
            now=15000.0,
            started_at=0.0,
            last_heartbeat=14999.0,
            max_duration_s=14400,
            heartbeat_timeout_s=2400,
        )
        == "hard_cap"
    )


def test_deadman_reports_failure_when_runtime_is_not_unassigned(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Outside a Colab VM the unassign call cannot succeed. The watchdog must
    # say so through its exit code instead of claiming the runtime was released.
    deadman = _load_deadman_module()
    monkeypatch.setattr(deadman.time, "sleep", lambda _seconds: None)
    exit_code = deadman.main(
        [
            "--pid-file", str(tmp_path / "server.pid"),
            "--pid-file", str(tmp_path / "bootstrap.pid"),
            "--heartbeat-file", str(tmp_path / "heartbeat"),
            "--max-duration", "1",
            "--heartbeat-timeout", "1",
            "--poll-interval", "1",
        ]
    )
    assert exit_code == 1


@pytest.fixture
def state_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    return tmp_path


def test_a_session_has_at_most_one_supervisor(state_home: Path) -> None:
    # A second `start` used to run to the port bind, fail, and then tear down
    # the runtime the first supervisor was serving.
    assert live_supervisor_pid("colab-llm") is None
    lock = acquire_session_lock("colab-llm")
    try:
        assert live_supervisor_pid("colab-llm") == os.getpid()
        with pytest.raises(SessionBusyError, match="already has a running supervisor"):
            acquire_session_lock("colab-llm")
        # Another session name is another lock.
        acquire_session_lock("other").release()
    finally:
        lock.release()
    assert live_supervisor_pid("colab-llm") is None
    acquire_session_lock("colab-llm").release()


def test_a_dead_supervisor_is_never_signalled(state_home: Path) -> None:
    # The lock dies with its holder, so a PID left behind by a killed
    # supervisor (and possibly recycled since) is never offered for signalling.
    script = (
        "import os, time\n"
        "from colab_llm.state import acquire_session_lock\n"
        "acquire_session_lock('colab-llm')\n"
        "print(os.getpid(), flush=True)\n"
        "time.sleep(30)\n"
    )
    env = {**os.environ, "PYTHONPATH": str(Path(cli.__file__).resolve().parent.parent)}
    holder = subprocess.Popen(
        [sys.executable, "-c", script], stdout=subprocess.PIPE, text=True, env=env
    )
    try:
        assert holder.stdout is not None
        assert int(holder.stdout.readline()) == holder.pid
        assert live_supervisor_pid("colab-llm") == holder.pid
    finally:
        holder.kill()
        holder.wait()
    assert (state_home / "colab-llm" / "colab-llm.lock").read_text().strip() == str(holder.pid)
    assert live_supervisor_pid("colab-llm") is None


def test_the_user_facing_port_is_claimed_before_anything_else() -> None:
    port = pick_free_loopback_port()
    listener = bind_loopback_listener("127.0.0.1", port)
    try:
        with pytest.raises(OSError):
            bind_loopback_listener("127.0.0.1", port)
    finally:
        listener.close()
    with pytest.raises(ValueError, match="loopback"):
        bind_loopback_listener("0.0.0.0", port)


_RUN = RunInfo(
    session="colab-llm",
    pid=4242,
    engine=Engine.MAYA,
    model="EXL3-3.05bpw",
    gpu="G4",
    bind_host="127.0.0.1",
    local_port=18100,
    remote_port=18100,
    idle_timeout_s=1800,
    max_duration_s=14400,
)
_TRAFFIC = Traffic(
    tunnel_port=40001,
    idle_s=12.5,
    elapsed_s=90.0,
    active_connections=1,
    total_connections=3,
    bytes_in=10,
    bytes_out=20,
)


@pytest.mark.parametrize(
    "phase",
    [
        Allocating(),
        Ready(traffic=_TRAFFIC, health={"status": "ok"}),
        Stopped(stop_reason=StopReason.IDLE_TIMEOUT, runtime_stopped=False),
    ],
)
def test_state_file_round_trips_each_phase(phase: object) -> None:
    state = SessionState(run=_RUN, phase=phase, updated_at="2026-11-28T00:00:00+00:00")  # type: ignore[arg-type]
    assert parse_state(json.loads(json.dumps(state_to_json(state)))) == state


def test_state_file_that_is_not_a_legal_state_is_an_error_not_a_default() -> None:
    ready = state_to_json(
        SessionState(run=_RUN, phase=Ready(traffic=_TRAFFIC, health={}), updated_at="t")
    )
    # A stopped session without a reason, a ready one without counters, an
    # unknown phase, and a file from before versioning are all refused.
    for broken in (
        {**ready, "phase": "stopped"},
        {k: v for k, v in ready.items() if k != "tunnel_port"},
        {**ready, "phase": "resting"},
        {**ready, "idle_timeout_s": "1800"},
        {k: v for k, v in ready.items() if k != "version"},
        ["not", "an", "object"],
    ):
        with pytest.raises(StateError):
            parse_state(broken)


def test_tunnel_supervisor_that_dies_is_seen_as_ended(tmp_path: Path) -> None:
    # A loop that crashed (here: the ssh binary cannot be executed) left both
    # of the old flags false, and the session kept billing with no tunnel.
    async def _run() -> None:
        tunnel = TunnelSupervisor(
            colab_bin="/bin/true",
            ssh_bin=str(tmp_path / "no-such-ssh"),
            session="colab-llm",
            tunnel_port=pick_free_loopback_port(),
            remote_port=18100,
            identity=tmp_path / "id_ed25519",
            control_path=tmp_path / "ctl.sock",
        )
        assert tunnel.ended is False
        await tunnel.start()
        for _ in range(100):
            if tunnel.ended:
                break
            await asyncio.sleep(0.01)
        assert tunnel.ended is True
        assert isinstance(tunnel.end_error(), FileNotFoundError)
        await tunnel.stop()

    asyncio.run(_run())


def test_stop_always_issues_the_colab_stop(
    state_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # `stop` used to list sessions first, so a listing it could not parse
    # aborted the command before any stop was sent.
    commands: list[list[str]] = []

    def _fake_run(
        self: ColabClient, args: list[str], **_kwargs: object
    ) -> subprocess.CompletedProcess[str]:
        commands.append(list(args))
        stdout = "an unexpected notice\n" if args == ["sessions"] else ""
        return subprocess.CompletedProcess(args, 0, stdout=stdout, stderr="")

    # Any existing file: every `colab` call is faked below.
    monkeypatch.setenv("COLAB_LLM_COLAB_BIN", sys.executable)
    monkeypatch.setenv("COLAB_LLM_SSH_BIN", sys.executable)
    monkeypatch.setattr(ColabClient, "_run_colab", _fake_run)
    assert cli_main(["stop"]) == 2  # the verification listing is unreadable
    assert commands[0] == ["stop", "-s", "colab-llm"]

    # A stray file in the state directory must not stop `stop --all` either.
    (state_home / "colab-llm").mkdir(exist_ok=True)
    (state_home / "colab-llm" / ".not a session.lock").write_text("", encoding="utf-8")
    commands.clear()
    cli_main(["stop", "--all"])
    assert commands[0] == ["stop", "--all"]


def test_control_socket_dir_must_be_a_real_private_directory(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # pytest's tmp_path is too deep for a Unix socket path, so use a short one.
    with tempfile.TemporaryDirectory(prefix="cl-", dir="/tmp") as runtime_dir:
        monkeypatch.setenv("XDG_RUNTIME_DIR", runtime_dir)
        socket_path = cli._control_socket_path("colab-llm")
        assert stat.S_IMODE(socket_path.parent.stat().st_mode) == 0o700

        # A symlink planted where the directory should be is refused, not followed.
        elsewhere = Path(runtime_dir) / "elsewhere"
        elsewhere.mkdir()
        socket_path.parent.rmdir()
        socket_path.parent.symlink_to(elsewhere)
        with pytest.raises(ColabError, match="not a directory"):
            cli._control_socket_path("colab-llm")


def test_atomic_state_and_jsonl_ledger_permissions(tmp_path: Path) -> None:
    state_file = tmp_path / "colab-llm.json"
    write_json_atomic(state_file, {"session": "colab-llm", "phase": "ready"})
    assert stat.S_IMODE(state_file.stat().st_mode) == 0o600
    assert json.loads(state_file.read_text(encoding="utf-8"))["phase"] == "ready"

    ledger = tmp_path / "sessions.jsonl"
    append_jsonl(ledger, {"session": "colab-llm", "cu_before": 10.0, "cu_after": 8.75})
    append_jsonl(ledger, {"session": "colab-llm", "cu_before": 8.75, "cu_after": 8.25})
    assert stat.S_IMODE(ledger.stat().st_mode) == 0o600
    rows = read_jsonl_tail(ledger, limit=1)
    assert len(rows) == 1
    assert rows[0]["cu_after"] == 8.25
    assert read_jsonl_tail(ledger, limit=0) == []
    assert len(read_jsonl_tail(ledger, limit=10)) == 2


def test_cli_non_network_subcommands(capsys: pytest.CaptureFixture[str]) -> None:
    assert cli_main(["engines"]) == 0
    out = capsys.readouterr().out
    assert "maya" in out
    assert "EXL3-3.05bpw" in out

    assert cli_main(["ssh-config", "--colab-bin", "/usr/bin/colab"]) == 0
    cfg_out = capsys.readouterr().out
    assert "Host colab-llm" in cfg_out
    assert "ProxyCommand /usr/bin/colab ssh --proxy-mode -s colab-llm" in cfg_out
