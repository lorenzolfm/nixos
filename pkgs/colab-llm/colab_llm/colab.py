"""Official google-colab-cli and OpenSSH ProxyCommand transport wrapper."""

from __future__ import annotations

import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

from colab_llm.config import (
    validate_loopback_host,
    validate_port,
    validate_session_name,
)

SSH_HOST = "root@colab-runtime"
_DEFAULT_KEY_CANDIDATES = ("id_ed25519", "id_ecdsa")
# Colab rejects `shape=hm` (HTTP 400) on single-shape GPU tiers such as G4 and L4.
_SINGLE_SHAPE_GPUS = frozenset({"G4", "L4"})

_BALANCE_RE = re.compile(r"Current balance:\s*([0-9]+(?:\.[0-9]+)?)", re.IGNORECASE)
_RATE_RE = re.compile(r"Usage rate:\s*([0-9]+(?:\.[0-9]+)?)/hr", re.IGNORECASE)
_ASSIGNMENTS_RE = re.compile(r"Active assignments:\s*(\d+)", re.IGNORECASE)
_SESSION_LINE_RE = re.compile(
    r"^\[(?P<name>[^\]]+)\]\s+(?P<endpoint>\S+)\s*\|\s*"
    r"Hardware:\s*(?P<hardware>[^|]+?)\s*\|\s*"
    r"Shape:\s*(?P<shape>[^|]+?)\s*\|\s*"
    r"Variant:\s*(?P<variant>[^|]+?)(?:\s*\|\s*Status:\s*(?P<status>.+))?$"
)


class ColabError(RuntimeError):
    """Raised when google-colab-cli or OpenSSH operations fail."""


@dataclass(frozen=True)
class UsageSnapshot:
    balance_cu: float
    rate_cu_per_hour: float
    active_assignments: int


@dataclass(frozen=True)
class ColabSessionInfo:
    name: str
    endpoint: str
    hardware: str
    shape: str
    variant: str
    status: str | None = None


def parse_usage_output(text: str) -> UsageSnapshot:
    """Parse `colab usage` output into structured Compute Unit numbers."""
    bal = _BALANCE_RE.search(text)
    rate = _RATE_RE.search(text)
    count = _ASSIGNMENTS_RE.search(text)
    if not (bal and rate and count):
        raise ColabError(f"Could not parse `colab usage` output:\n{text.strip()}")
    return UsageSnapshot(
        balance_cu=float(bal.group(1)),
        rate_cu_per_hour=float(rate.group(1)),
        active_assignments=int(count.group(1)),
    )


def parse_sessions_output(text: str) -> list[ColabSessionInfo]:
    """Parse `colab sessions` output into structured session entries."""
    sessions: list[ColabSessionInfo] = []
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        match = _SESSION_LINE_RE.match(line)
        if not match:
            # `[colab] ...` lines are the CLI's own notices. Anything else is a
            # format this parser does not know: fail rather than report a
            # running (billable) session as absent.
            if line.startswith("[colab]"):
                continue
            raise ColabError(f"Could not parse `colab sessions` line: {line!r}")
        sessions.append(
            ColabSessionInfo(
                name=match.group("name").strip(),
                endpoint=match.group("endpoint").strip(),
                hardware=match.group("hardware").strip(),
                shape=match.group("shape").strip(),
                variant=match.group("variant").strip(),
                status=match.group("status").strip() if match.group("status") else None,
            )
        )
    return sessions


def _pubkey_algorithm(path: Path) -> str | None:
    pub_path = path if path.suffix == ".pub" else Path(f"{path}.pub")
    if pub_path.is_file():
        first_line = pub_path.read_text(encoding="utf-8", errors="ignore").strip()
        if first_line:
            return first_line.split()[0]
    return None


def resolve_ssh_identity(
    identity: Path | None = None,
    ssh_dir: Path | None = None,
) -> Path:
    """Resolve an ed25519/ecdsa SSH key for `colab ssh`.

    Google Colab rejects `ssh-rsa` keys server-side, so RSA keys are rejected
    upfront with an actionable message instead of failing after VM allocation.
    """
    if identity is not None:
        expanded = identity.expanduser()
        if not expanded.exists():
            raise ColabError(f"SSH identity not found: {expanded}")
        alg = _pubkey_algorithm(expanded)
        if alg == "ssh-rsa":
            raise ColabError(
                f"SSH key {expanded} is ssh-rsa, which Colab rejects. "
                "Generate an ed25519 key: ssh-keygen -t ed25519"
            )
        # Prefer private key path when both <key> and <key>.pub exist so OpenSSH
        # `-i` and `colab ssh -i` both work cleanly.
        if expanded.suffix == ".pub":
            priv = expanded.with_suffix("")
            if priv.exists():
                return priv
        return expanded

    base = ssh_dir if ssh_dir is not None else Path.home() / ".ssh"
    for name in _DEFAULT_KEY_CANDIDATES:
        priv = base / name
        pub = base / f"{name}.pub"
        if priv.exists() and pub.exists():
            return priv

    raise ColabError(
        "No supported SSH key (~/.ssh/id_ed25519 or ~/.ssh/id_ecdsa) found. "
        "Google Colab rejects RSA keys; create one with: ssh-keygen -t ed25519"
    )


def find_colab_bin() -> str:
    env_bin = os.environ.get("COLAB_LLM_COLAB_BIN")
    if env_bin and Path(env_bin).exists():
        return env_bin
    found = shutil.which("colab")
    if not found:
        raise ColabError(
            "`colab` (google-colab-cli) not found on PATH. "
            "Ensure `hosts/common/colab-llm.nix` is imported in your NixOS/nix-darwin config."
        )
    return found


def find_ssh_bin() -> str:
    env_bin = os.environ.get("COLAB_LLM_SSH_BIN")
    if env_bin and Path(env_bin).exists():
        return env_bin
    found = shutil.which("ssh")
    if not found:
        raise ColabError("`ssh` (OpenSSH client) not found on PATH.")
    return found


def build_proxy_command(
    *,
    colab_bin: str,
    session: str,
    identity: Path | None = None,
    rm_on_disconnect: bool = False,
    force: bool = False,
    colab_config: Path | None = None,
) -> str:
    """Build a shell-quoted OpenSSH ProxyCommand invoking official `colab ssh --proxy-mode`."""
    validate_session_name(session)
    parts: list[str] = [colab_bin]
    if colab_config is not None:
        parts.extend(["--config", str(colab_config.expanduser().resolve())])
    parts.extend(["ssh", "--proxy-mode", "-s", session])
    if identity is not None:
        parts.extend(["-i", str(identity.expanduser())])
    if rm_on_disconnect:
        parts.append("--rm")
    if force:
        parts.append("--force")
    return " ".join(shlex.quote(p) for p in parts)


# Options for a fresh connection through `colab ssh --proxy-mode`. The VM's host
# key changes on every allocation and the transport is Colab's authenticated
# WebSocket, so host-key pinning is deliberately off.
_SSH_TRANSPORT_OPTIONS = (
    "StrictHostKeyChecking=no",
    "UserKnownHostsFile=/dev/null",
    "LogLevel=ERROR",
    "IdentitiesOnly=yes",
    "ServerAliveInterval=30",
    "ServerAliveCountMax=3",
    "ConnectTimeout=30",
)


def _ssh_option_args(options: Sequence[str]) -> list[str]:
    args: list[str] = []
    for option in options:
        assert "=" in option, option
        args.extend(["-o", option])
    return args


def build_ssh_mux_args(
    *,
    ssh_bin: str,
    control_path: Path,
    batch_mode: bool = True,
    connect_timeout_s: int | None = None,
) -> list[str]:
    """Arguments that reuse the live ControlMaster socket (no new Colab SSH session)."""
    options = ["LogLevel=ERROR"]
    if batch_mode:
        options.insert(0, "BatchMode=yes")
    if connect_timeout_s is not None:
        assert connect_timeout_s > 0
        options.append(f"ConnectTimeout={connect_timeout_s}")
    return [ssh_bin, *_ssh_option_args(options), "-S", str(control_path), SSH_HOST]


def build_ssh_base_args(
    *,
    ssh_bin: str,
    proxy_command: str,
    identity: Path | None = None,
    control_path: Path | None = None,
    control_master: bool = False,
    batch_mode: bool = True,
) -> list[str]:
    """Construct OpenSSH arguments using `colab ssh --proxy-mode` as ProxyCommand."""
    assert proxy_command, "a ProxyCommand is the only supported transport"
    options = list(_SSH_TRANSPORT_OPTIONS)
    if batch_mode:
        options.insert(0, "BatchMode=yes")
    options.append(f"ProxyCommand={proxy_command}")
    args: list[str] = [ssh_bin, *_ssh_option_args(options)]
    if identity is not None:
        priv = (
            identity.with_suffix("")
            if identity.suffix == ".pub" and identity.with_suffix("").exists()
            else identity
        )
        args.extend(["-i", str(priv)])
    if control_path is not None:
        args.extend(["-o", f"ControlPath={control_path}"])
        if control_master:
            args.extend(["-o", "ControlMaster=auto", "-o", "ControlPersist=no"])
    return args


def build_ssh_tunnel_args(
    *,
    ssh_bin: str,
    proxy_command: str,
    bind_host: str,
    local_forward_port: int,
    remote_port: int,
    identity: Path | None = None,
    control_path: Path | None = None,
) -> list[str]:
    """Construct OpenSSH local-port-forwarding tunnel arguments (loopback-only)."""
    host = validate_loopback_host(bind_host)
    validate_port(local_forward_port, "local_forward_port")
    validate_port(remote_port, "remote_port")
    args = build_ssh_base_args(
        ssh_bin=ssh_bin,
        proxy_command=proxy_command,
        identity=identity,
        control_path=control_path,
        control_master=control_path is not None,
    )
    args.extend(
        [
            "-N",
            "-o",
            "ExitOnForwardFailure=yes",
            "-L",
            f"{host}:{local_forward_port}:127.0.0.1:{remote_port}",
            SSH_HOST,
        ]
    )
    return args


def render_ssh_config_block(
    *,
    colab_bin: str,
    session: str,
    local_port: int,
    remote_port: int,
    identity: Path | None = None,
    rm_on_disconnect: bool = False,
    colab_config: Path | None = None,
) -> str:
    """Render an auditable ~/.ssh/config snippet for official Colab ProxyCommand."""
    validate_session_name(session)
    validate_port(local_port, "local_port")
    validate_port(remote_port, "remote_port")
    proxy_cmd = build_proxy_command(
        colab_bin=colab_bin,
        session=session,
        identity=identity,
        rm_on_disconnect=rm_on_disconnect,
        colab_config=colab_config,
    )
    lines = [
        f"Host {session}",
        "    HostName colab-runtime",
        "    User root",
        f"    ProxyCommand {proxy_cmd}",
        f"    LocalForward 127.0.0.1:{local_port} 127.0.0.1:{remote_port}",
        "    StrictHostKeyChecking no",
        "    UserKnownHostsFile /dev/null",
        "    IdentitiesOnly yes",
        "    ServerAliveInterval 30",
        "    ServerAliveCountMax 3",
    ]
    if identity is not None:
        lines.insert(4, f"    IdentityFile {identity}")
    return "\n".join(lines) + "\n"


class ColabClient:
    """Subprocess client wrapping official `google-colab-cli` and OpenSSH."""

    def __init__(
        self,
        *,
        colab_bin: str | None = None,
        ssh_bin: str | None = None,
        colab_config: Path | None = None,
    ) -> None:
        self.colab_bin = colab_bin or find_colab_bin()
        self.ssh_bin = ssh_bin or find_ssh_bin()
        self.colab_config = colab_config

    def _colab_cmd(self, args: Sequence[str]) -> list[str]:
        cmd = [self.colab_bin]
        if self.colab_config is not None:
            cmd.extend(["--config", str(self.colab_config.expanduser().resolve())])
        cmd.extend(args)
        return cmd

    def _run_colab(
        self,
        args: Sequence[str],
        *,
        check: bool = True,
        capture: bool = True,
        timeout: int = 180,
    ) -> subprocess.CompletedProcess[str]:
        cmd = self._colab_cmd(args)
        try:
            proc = subprocess.run(
                cmd,
                check=False,
                capture_output=capture,
                text=True,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired as exc:
            raise ColabError(f"Timed out running `{' '.join(cmd)}` after {timeout}s") from exc

        if check and proc.returncode != 0:
            detail = (proc.stderr or proc.stdout or "").strip()
            raise ColabError(
                f"`{' '.join(cmd)}` failed (exit {proc.returncode})"
                + (f":\n{detail}" if detail else "")
            )
        return proc

    def whoami(self) -> str:
        proc = self._run_colab(["whoami"], timeout=60)
        return proc.stdout.strip()

    def get_usage(self) -> UsageSnapshot:
        proc = self._run_colab(["usage"], timeout=60)
        return parse_usage_output(proc.stdout)

    def list_sessions(self) -> list[ColabSessionInfo]:
        proc = self._run_colab(["sessions"], timeout=60)
        return parse_sessions_output(proc.stdout)

    def _sessions_file_path(self) -> Path:
        if self.colab_config is not None:
            return self.colab_config.expanduser().resolve()
        return Path.home() / ".config" / "colab-cli" / "sessions.json"

    def attach_session(
        self,
        *,
        session: str,
        gpu: str,
        endpoint: str | None = None,
    ) -> ColabSessionInfo:
        """Adopt an existing Colab assignment (e.g. created in the Web UI) into sessions.json.

        Registers the endpoint with an empty token and lets `colab sessions`
        refresh the live runtime proxy token/URL via official `list_assignments()`.
        """
        validate_session_name(session)
        listed = self.list_sessions()

        # Already tracked under this session name?
        for item in listed:
            if item.name == session:
                if item.hardware.upper() != gpu.upper():
                    raise ColabError(
                        f"Session {session!r} already exists on hardware {item.hardware!r}, "
                        f"not requested {gpu!r}."
                    )
                return item

        if endpoint is not None:
            candidates = [s for s in listed if s.endpoint == endpoint]
            if not candidates:
                raise ColabError(
                    f"Endpoint {endpoint!r} not found in `colab sessions`. "
                    "Allocate a runtime first in https://colab.research.google.com."
                )
            target = candidates[0]
            if target.hardware.upper() != gpu.upper():
                raise ColabError(
                    f"Endpoint {endpoint!r} has hardware {target.hardware!r}, "
                    f"not requested {gpu!r}."
                )
        else:
            candidates = [
                s for s in listed if s.name == "?" and s.hardware.upper() == gpu.upper()
            ]
            if not candidates:
                raise ColabError(
                    f"No unnamed {gpu} runtime found in `colab sessions` to attach to.\n"
                    f"Connect a {gpu} runtime in https://colab.research.google.com, "
                    f"verify it appears in `colab sessions`, then re-run `colab-llm start --attach`."
                )
            if len(candidates) > 1:
                eps = ", ".join(c.endpoint for c in candidates)
                raise ColabError(
                    f"Multiple unnamed {gpu} runtimes found ({eps}). "
                    "Pass one explicitly: `colab-llm start --attach <endpoint>`."
                )
            target = candidates[0]

        sessions_path = self._sessions_file_path()
        sessions_path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        existing_data: dict[str, object] = {}
        if sessions_path.is_file():
            raw = sessions_path.read_text(encoding="utf-8").strip()
            if raw:
                loaded = json.loads(raw)
                if isinstance(loaded, dict):
                    existing_data = loaded

        shape_enum = "HIGH_RAM" if "high" in target.shape.lower() else "STANDARD"
        existing_data[session] = {
            "name": session,
            "token": "",
            "url": "",
            "endpoint": target.endpoint,
            "token_expires_at": None,
            "variant": target.variant.upper(),
            "accelerator": target.hardware.upper(),
            "machine_shape": shape_enum,
            "kernel_id": None,
            "session_id": None,
            "last_execution": None,
            "running": None,
        }
        fd, tmp_name = tempfile.mkstemp(
            prefix=".sessions.", suffix=".tmp", dir=str(sessions_path.parent)
        )
        tmp_path = Path(tmp_name)
        try:
            os.chmod(tmp_path, 0o600)
            with os.fdopen(fd, "w", encoding="utf-8") as fh:
                json.dump(existing_data, fh, indent=2)
                fh.write("\n")
                fh.flush()
                os.fsync(fh.fileno())
            os.replace(tmp_path, sessions_path)
        except Exception:
            tmp_path.unlink(missing_ok=True)
            raise

        # Trigger `colab sessions` so google-colab-cli refreshes the live token/URL.
        refreshed = {s.name: s for s in self.list_sessions()}
        if session not in refreshed:
            raise ColabError(
                f"Attached endpoint {target.endpoint!r} disappeared before sync completed."
            )
        return refreshed[session]

    def ensure_session(
        self,
        *,
        session: str,
        gpu: str,
        high_mem: bool = False,
        attach: bool = False,
        attach_endpoint: str | None = None,
    ) -> tuple[ColabSessionInfo, bool]:
        """Ensure a Colab session with the requested GPU exists.

        Returns (session_info, created). Refuses to silently reuse a session
        allocated with a different accelerator.
        """
        if attach or attach_endpoint is not None:
            info = self.attach_session(
                session=session,
                gpu=gpu,
                endpoint=attach_endpoint,
            )
            return info, False

        existing = {s.name: s for s in self.list_sessions()}
        if session in existing:
            info = existing[session]
            if info.hardware.upper() != gpu.upper():
                raise ColabError(
                    f"Session {session!r} already exists on hardware {info.hardware!r}, "
                    f"not requested {gpu!r}. Stop it first with "
                    f"`colab-llm stop -s {session}` or choose a different --session."
                )
            return info, False

        args = ["new", "-s", session, "--gpu", gpu]
        if high_mem and gpu.upper() not in _SINGLE_SHAPE_GPUS:
            args.append("--high-mem")
        # From here a billable runtime may exist. Any failure, including Ctrl+C,
        # must stop it before propagating: the caller has no session to clean up.
        try:
            return self._create_session(session=session, gpu=gpu, args=args), True
        except BaseException:
            self._stop_after_failed_create(session)
            raise

    def _create_session(
        self, *, session: str, gpu: str, args: Sequence[str]
    ) -> ColabSessionInfo:
        try:
            self._run_colab(args, capture=False, timeout=300)
        except ColabError as exc:
            raise ColabError(
                f"{exc}\n"
                "Hint: if Colab returned HTTP 503 Service Unavailable on /tun/m/assign, "
                "no Compute Units were charged; this is a temporary Colab GPU pool / "
                "backend timeout (see googlecolab/google-colab-cli#282 and #292).\n"
                f"Workaround: connect a {gpu} runtime in https://colab.research.google.com "
                "and run `colab-llm start --attach` (or retry later / raise COLAB_CLI_ASSIGN_RETRIES)."
            ) from exc

        after = {s.name: s for s in self.list_sessions()}
        if session not in after:
            raise ColabError(
                f"Session {session!r} was created but did not appear in `colab sessions`."
            )
        info = after[session]
        if info.hardware.upper() != gpu.upper():
            raise ColabError(
                f"Session {session!r} was created on hardware {info.hardware!r}, "
                f"not requested {gpu!r}."
            )
        return info

    def _stop_after_failed_create(self, session: str) -> None:
        try:
            self.teardown_session(session)
        except ColabError as exc:
            # Do not mask the original failure; make the leak impossible to miss.
            print(
                f"[colab-llm] WARNING: session {session!r} may still be running and "
                f"billing: {exc}\n  Run manually: colab stop -s {session}",
                file=sys.stderr,
            )

    def stop_session(self, session: str) -> bool:
        """Stop a single session idempotently. Returns True if a session was stopped."""
        validate_session_name(session)
        existing = {s.name for s in self.list_sessions()}
        if session not in existing:
            return False
        self._run_colab(["stop", "-s", session], capture=False, timeout=120)
        remaining = {s.name for s in self.list_sessions()}
        if session in remaining:
            raise ColabError(f"Session {session!r} is still listed after `colab stop`.")
        return True

    def teardown_session(self, session: str) -> None:
        """Billing-safety stop: always issue `colab stop`, then verify it took effect.

        Unlike `stop_session`, this never trusts a prior listing to decide
        whether a stop is needed. Stopping an absent session is harmless;
        skipping the stop of a present one is not.
        """
        validate_session_name(session)
        stop = self._run_colab(["stop", "-s", session], check=False, timeout=120)
        remaining = {s.name for s in self.list_sessions()}
        if session in remaining:
            detail = (stop.stderr or stop.stdout or "").strip()
            raise ColabError(
                f"Session {session!r} is still listed after `colab stop` "
                f"(exit {stop.returncode})" + (f":\n{detail}" if detail else "")
            )

    def stop_all(self) -> None:
        self._run_colab(["stop", "--all"], capture=False, timeout=180)
        # `colab stop --all` only stops locally tracked sessions; also clean up
        # any remaining unnamed (`[?]`) server assignments (e.g. Web UI tabs).
        for index, leftover in enumerate(self.list_sessions()):
            if leftover.name == "?":
                cleanup_name = f"colab-llm-cleanup-{index}"
                self.attach_session(
                    session=cleanup_name,
                    gpu=leftover.hardware,
                    endpoint=leftover.endpoint,
                )
                self.stop_session(cleanup_name)
            else:
                self.stop_session(leftover.name)

    def ssh_exec(
        self,
        *,
        session: str,
        remote_command: str,
        identity: Path,
        stdin_bytes: bytes | None = None,
        control_path: Path | None = None,
        force: bool = False,
        stream_output: bool = False,
        timeout: int | None = None,
    ) -> subprocess.CompletedProcess[bytes]:
        """Run a remote command over official `colab ssh --proxy-mode` (or active ControlMaster)."""
        if control_path is not None and control_path.exists():
            cmd = build_ssh_mux_args(ssh_bin=self.ssh_bin, control_path=control_path)
            cmd.append(remote_command)
        else:
            proxy_cmd = build_proxy_command(
                colab_bin=self.colab_bin,
                session=session,
                identity=identity,
                force=force,
                colab_config=self.colab_config,
            )
            cmd = build_ssh_base_args(
                ssh_bin=self.ssh_bin,
                proxy_command=proxy_cmd,
                identity=identity,
            )
            cmd.extend([SSH_HOST, remote_command])

        try:
            proc = subprocess.run(
                cmd,
                input=stdin_bytes,
                capture_output=not stream_output,
                check=False,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired as exc:
            raise ColabError(f"SSH command timed out after {timeout}s") from exc

        if proc.returncode != 0:
            detail = ""
            if not stream_output:
                detail = (proc.stderr or proc.stdout or b"").decode(
                    "utf-8", errors="replace"
                ).strip()
            raise ColabError(
                f"Remote SSH command failed (exit {proc.returncode})"
                + (f":\n{detail}" if detail else "")
            )
        return proc

    def ssh_interactive(
        self,
        *,
        session: str,
        identity: Path,
        remote_cmd: Sequence[str] | None = None,
        control_path: Path | None = None,
    ) -> int:
        """Open an interactive shell or command over official `colab ssh --proxy-mode`."""
        if control_path is not None and control_path.exists():
            cmd = build_ssh_mux_args(
                ssh_bin=self.ssh_bin, control_path=control_path, batch_mode=False
            )
        else:
            proxy_cmd = build_proxy_command(
                colab_bin=self.colab_bin,
                session=session,
                identity=identity,
                colab_config=self.colab_config,
            )
            cmd = build_ssh_base_args(
                ssh_bin=self.ssh_bin,
                proxy_command=proxy_cmd,
                identity=identity,
                batch_mode=False,
            )
            cmd.append(SSH_HOST)
        if remote_cmd:
            cmd.append(" ".join(shlex.quote(arg) for arg in remote_cmd))
        proc = subprocess.run(cmd, check=False)
        return proc.returncode
