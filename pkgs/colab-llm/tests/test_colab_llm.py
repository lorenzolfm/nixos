"""Unit and async loopback integration tests for colab-llm."""

from __future__ import annotations

import asyncio
import importlib.util
import json
import stat
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

import pytest

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
    DEFAULT_MAYA_MODEL,
    MAYA_PINNED_COMMIT,
    LaunchConfig,
    parse_duration,
    resolve_hf_token,
    validate_loopback_host,
    validate_session_name,
)
from colab_llm.engines import engines_dir, get_engine, list_engines
from colab_llm.proxy import (
    ActivityTrackingProxy,
    append_jsonl,
    pick_free_loopback_port,
    probe_http_health,
    read_jsonl_tail,
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
    assert cfg.engine == "maya"
    assert cfg.effective_model == DEFAULT_MAYA_MODEL == "EXL3-3.05bpw"
    assert cfg.draft == "dflash2"
    assert cfg.context_length == 131072
    assert cfg.bind_host == "127.0.0.1"
    assert cfg.local_port == 18100
    assert cfg.remote_port == 18100
    assert cfg.idle_timeout_s == 1800
    assert cfg.max_duration_s == 14400
    assert cfg.web_ui is False
    assert cfg.vision is True


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
    with pytest.raises(ValueError, match="requires --hf-repo"):
        LaunchConfig(hf_revision="main")


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
    assert snap.rate_per_hour == pytest.approx(6.71)
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


def test_ensure_session_omits_high_mem_on_single_shape_g4(
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

    info, created = client.ensure_session(session="colab-llm", gpu="G4", high_mem=True)
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
                    name="?",
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
    info, created = client.ensure_session(session="colab-llm", gpu="G4", attach=True)
    assert created is False
    assert info.name == "colab-llm"
    assert info.endpoint == "m-s-webui123"
    saved = json.loads(sessions_file.read_text(encoding="utf-8"))
    assert saved["colab-llm"]["endpoint"] == "m-s-webui123"
    assert saved["colab-llm"]["accelerator"] == "G4"
    assert saved["colab-llm"]["machine_shape"] == "HIGH_RAM"
    assert stat.S_IMODE(sessions_file.stat().st_mode) == 0o600


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
                name="?",
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
    monkeypatch.setattr(client, "stop_session", lambda name: stopped.append(name) or True)

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

    custom_env = eng.build_env(
        LaunchConfig(hf_repo="turboderp/GLM-5.3-Flash-exl3", hf_revision="sc_2.60bpw")
    )
    assert custom_env["MAYA_CUSTOM_MODEL_DIR"] == "1"
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
            bind_host="127.0.0.1",
            listen_port=proxy_port,
            upstream_port=upstream_port,
            clock=lambda: fake_now[0],
        )
        await proxy.start()
        try:
            # Open a connection and hold it open past the idle timeout:
            # active_connections > 0 must prevent idle expiration.
            reader, writer = await asyncio.open_connection("127.0.0.1", proxy_port)
            writer.write(b"hello colab-llm")
            await writer.drain()
            echoed = await reader.readexactly(len(b"hello colab-llm"))
            assert echoed == b"hello colab-llm"

            fake_now[0] = 5000.0
            assert proxy.stats.active_connections == 1
            assert proxy.stats.idle_seconds(fake_now[0]) == 0.0
            assert not proxy.stats.is_idle_expired(1800, fake_now[0])

            # Close connection; activity timestamp updates to 5000.0.
            writer.close()
            await writer.wait_closed()
            await asyncio.sleep(0.05)

            assert proxy.stats.active_connections == 0
            assert proxy.stats.total_connections == 1
            assert proxy.stats.bytes_in == len(b"hello colab-llm")
            assert proxy.stats.bytes_out == len(b"hello colab-llm")
            assert not proxy.stats.is_idle_expired(1800, 6799.0)
            assert proxy.stats.is_idle_expired(1800, 6800.0)
            assert proxy.stats.is_hard_cap_expired(14400, 15400.0)
        finally:
            await proxy.close()
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


def test_atomic_state_and_jsonl_ledger_permissions(tmp_path: Path) -> None:
    state_file = tmp_path / "colab-llm.json"
    write_json_atomic(state_file, {"session": "colab-llm", "phase": "ready"})
    assert stat.S_IMODE(state_file.stat().st_mode) == 0o600
    assert json.loads(state_file.read_text(encoding="utf-8"))["phase"] == "ready"

    ledger = tmp_path / "sessions.jsonl"
    append_jsonl(ledger, {"session": "colab-llm", "cu_delta": 1.25})
    append_jsonl(ledger, {"session": "colab-llm", "cu_delta": 0.50})
    assert stat.S_IMODE(ledger.stat().st_mode) == 0o600
    rows = read_jsonl_tail(ledger, limit=1)
    assert len(rows) == 1
    assert rows[0]["cu_delta"] == 0.50


def test_cli_non_network_subcommands(capsys: pytest.CaptureFixture[str]) -> None:
    assert cli_main(["engines"]) == 0
    out = capsys.readouterr().out
    assert "maya" in out
    assert "EXL3-3.05bpw" in out

    assert cli_main(["ssh-config", "--colab-bin", "/usr/bin/colab"]) == 0
    cfg_out = capsys.readouterr().out
    assert "Host colab-llm" in cfg_out
    assert "ProxyCommand /usr/bin/colab ssh --proxy-mode -s colab-llm" in cfg_out
