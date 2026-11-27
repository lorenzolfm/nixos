"""Validated configuration types, defaults, and state paths for colab-llm."""

from __future__ import annotations

import os
import re
from dataclasses import dataclass
from pathlib import Path

DEFAULT_SESSION = "colab-llm"
DEFAULT_GPU = "G4"
ALLOWED_GPUS = ("G4", "A100", "H100", "L4", "T4")

# Colab AUP guardrail: bind strictly to loopback so endpoints are never exposed
# publicly or accidentally bound to wildcard interfaces.
DEFAULT_BIND_HOST = "127.0.0.1"
LOOPBACK_HOSTS = frozenset({"127.0.0.1", "localhost", "::1"})

DEFAULT_LOCAL_PORT = 18100
DEFAULT_REMOTE_PORT = 18100

DEFAULT_IDLE_TIMEOUT_S = 1800  # 30 minutes
MIN_IDLE_TIMEOUT_S = 60
MAX_IDLE_TIMEOUT_S = 14400  # 4 hours

DEFAULT_MAX_DURATION_S = 14400  # 4 hours hard cap
MIN_MAX_DURATION_S = 300  # 5 minutes
MAX_MAX_DURATION_S = 43200  # 12 hours

DEFAULT_BOOTSTRAP_TIMEOUT_S = 1800  # 30 minutes (download + compile + load)

DEFAULT_ENGINE = "maya"
ALLOWED_ENGINES = ("maya", "vllm", "llama-cpp", "sglang")

DEFAULT_MAYA_MODEL = "EXL3-3.05bpw"
DEFAULT_MAYA_DRAFT = "dflash2"
ALLOWED_MAYA_DRAFTS = ("dflash2", "mtp", "off")
DEFAULT_CONTEXT_LENGTH = 131072
DEFAULT_PARALLEL = 1

# Pinned Project Maya upstream release (audited commit SHA).
MAYA_REPO_URL = "https://github.com/mw00/project-maya.git"
MAYA_PINNED_TAG = "v1.1.82"
MAYA_PINNED_COMMIT = "f84ec2db12bf96e7f2d106a2ebd91550903b859d"

# Exact pinned HF repo + commit revisions matching Project Maya v1.1.82's
# built-in EXL3 verification tables, enabling fast Xet/hf_transfer prefetch
# before Maya runs its own SHA256 verification and pack step.
KNOWN_MAYA_HF_PREFETCH: dict[str, tuple[str, str]] = {
    "EXL3-3.05bpw": (
        "turboderp/GLM-5.3-Flash-exl3",
        "332ab457b709b7ba30dd9a448be5de03b80a7ac9",
    ),
    "EXL3-4.05bpw": (
        "turboderp/GLM-5.3-Flash-exl3",
        "2a30229e67012798ba9f0cd832bb78abf4c363d5",
    ),
}

_SESSION_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
_DURATION_TOKEN_RE = re.compile(r"(\d+)\s*([hms]?)", re.IGNORECASE)
_HF_REPO_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
_HF_REV_RE = re.compile(r"^[A-Za-z0-9_./-]{1,128}$")
_MODEL_NAME_RE = re.compile(r"^[A-Za-z0-9_./:-]{1,200}$")


def validate_session_name(name: str) -> str:
    if not name or not _SESSION_RE.fullmatch(name):
        raise ValueError(
            f"Invalid session name {name!r}: must match {_SESSION_RE.pattern}"
        )
    return name


def validate_gpu(gpu: str) -> str:
    upper = gpu.strip().upper()
    if upper not in ALLOWED_GPUS:
        allowed = ", ".join(ALLOWED_GPUS)
        raise ValueError(f"Unsupported GPU {gpu!r}: allowed values are {allowed}")
    return upper


def validate_loopback_host(host: str) -> str:
    cleaned = host.strip().lower()
    if cleaned not in LOOPBACK_HOSTS:
        raise ValueError(
            f"Refusing non-loopback bind host {host!r}: only loopback addresses "
            f"({', '.join(sorted(LOOPBACK_HOSTS))}) are permitted."
        )
    return cleaned


def validate_port(port: int, label: str = "port") -> int:
    if not (1024 <= port <= 65535):
        raise ValueError(f"Invalid {label} {port}: must be in range 1024..65535")
    return port


def validate_timeouts(idle_timeout_s: int, max_duration_s: int) -> tuple[int, int]:
    if not (MIN_IDLE_TIMEOUT_S <= idle_timeout_s <= MAX_IDLE_TIMEOUT_S):
        raise ValueError(
            f"idle_timeout_s={idle_timeout_s} out of bounds "
            f"[{MIN_IDLE_TIMEOUT_S}, {MAX_IDLE_TIMEOUT_S}]"
        )
    if not (MIN_MAX_DURATION_S <= max_duration_s <= MAX_MAX_DURATION_S):
        raise ValueError(
            f"max_duration_s={max_duration_s} out of bounds "
            f"[{MIN_MAX_DURATION_S}, {MAX_MAX_DURATION_S}]"
        )
    if idle_timeout_s > max_duration_s:
        raise ValueError(
            f"idle_timeout_s ({idle_timeout_s}s) cannot exceed "
            f"max_duration_s ({max_duration_s}s)"
        )
    return idle_timeout_s, max_duration_s


def parse_duration(spec: str) -> int:
    """Parse a duration like '30m', '4h', '1h30m', or raw seconds '1800'."""
    text = spec.strip()
    if not text:
        raise ValueError("Duration cannot be empty")
    if text.isdigit():
        return int(text)

    total = 0
    pos = 0
    multipliers = {"": 1, "s": 1, "m": 60, "h": 3600}
    for match in _DURATION_TOKEN_RE.finditer(text):
        if match.start() != pos:
            raise ValueError(f"Invalid duration syntax: {spec!r}")
        amount = int(match.group(1))
        unit = match.group(2).lower()
        total += amount * multipliers[unit]
        pos = match.end()
    if pos != len(text) or total <= 0:
        raise ValueError(f"Invalid duration syntax: {spec!r}")
    return total


def format_duration(seconds: float) -> str:
    secs = max(0, round(seconds))
    hours, rem = divmod(secs, 3600)
    minutes, secs = divmod(rem, 60)
    parts: list[str] = []
    if hours:
        parts.append(f"{hours}h")
    if minutes or hours:
        parts.append(f"{minutes}m")
    parts.append(f"{secs}s")
    return "".join(parts)


def state_dir() -> Path:
    base = os.environ.get("XDG_STATE_HOME")
    root = Path(base) if base else Path.home() / ".local" / "state"
    path = root / "colab-llm"
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    try:
        os.chmod(path, 0o700)
    except OSError:
        pass
    return path


def resolve_hf_token(token_file: Path | None = None) -> str | None:
    """Resolve an optional Hugging Face token without ever printing it.

    Checks, in order: explicit --hf-token-file, $HF_TOKEN_FILE (sops-nix
    compatible), then $HF_TOKEN. Returns None for public-only usage.
    """
    file_path = token_file
    if file_path is None:
        env_path = os.environ.get("HF_TOKEN_FILE", "").strip()
        if env_path:
            file_path = Path(env_path).expanduser()

    if file_path is not None:
        if not file_path.is_file():
            raise FileNotFoundError(f"HF token file not found: {file_path}")
        token = file_path.read_text(encoding="utf-8").strip()
        return token or None

    env_token = os.environ.get("HF_TOKEN", "").strip()
    return env_token or None


@dataclass(frozen=True)
class LaunchConfig:
    session: str = DEFAULT_SESSION
    gpu: str = DEFAULT_GPU
    high_mem: bool = False
    engine: str = DEFAULT_ENGINE
    model: str | None = None
    hf_repo: str | None = None
    hf_revision: str | None = None
    draft: str = DEFAULT_MAYA_DRAFT
    context_length: int = DEFAULT_CONTEXT_LENGTH
    parallel: int = DEFAULT_PARALLEL
    vision: bool = True
    web_ui: bool = False
    bind_host: str = DEFAULT_BIND_HOST
    local_port: int = DEFAULT_LOCAL_PORT
    remote_port: int = DEFAULT_REMOTE_PORT
    idle_timeout_s: int = DEFAULT_IDLE_TIMEOUT_S
    max_duration_s: int = DEFAULT_MAX_DURATION_S
    bootstrap_timeout_s: int = DEFAULT_BOOTSTRAP_TIMEOUT_S
    fast_hf: bool = True
    rm_on_disconnect: bool = False
    reconfigure: bool = False
    attach: bool = False
    attach_endpoint: str | None = None
    identity: Path | None = None
    colab_config: Path | None = None
    hf_token_file: Path | None = None

    def __post_init__(self) -> None:
        object.__setattr__(self, "session", validate_session_name(self.session))
        object.__setattr__(self, "gpu", validate_gpu(self.gpu))
        object.__setattr__(self, "bind_host", validate_loopback_host(self.bind_host))
        validate_port(self.local_port, "local_port")
        validate_port(self.remote_port, "remote_port")
        validate_timeouts(self.idle_timeout_s, self.max_duration_s)

        if self.engine not in ALLOWED_ENGINES:
            raise ValueError(
                f"Unsupported engine {self.engine!r}: "
                f"choose one of {', '.join(ALLOWED_ENGINES)}"
            )
        if self.draft not in ALLOWED_MAYA_DRAFTS:
            raise ValueError(
                f"Unsupported draft mode {self.draft!r}: "
                f"choose one of {', '.join(ALLOWED_MAYA_DRAFTS)}"
            )
        if not (4096 <= self.context_length <= 1048576):
            raise ValueError(
                f"context_length={self.context_length} out of bounds [4096, 1048576]"
            )
        if not (1 <= self.parallel <= 8):
            raise ValueError(f"parallel={self.parallel} out of bounds [1, 8]")
        if not (60 <= self.bootstrap_timeout_s <= 7200):
            raise ValueError(
                f"bootstrap_timeout_s={self.bootstrap_timeout_s} out of bounds [60, 7200]"
            )
        if self.model is not None and not _MODEL_NAME_RE.fullmatch(self.model):
            raise ValueError(f"Invalid model identifier: {self.model!r}")
        if self.hf_repo is not None and not _HF_REPO_RE.fullmatch(self.hf_repo):
            raise ValueError(
                f"Invalid Hugging Face repo {self.hf_repo!r}: expected 'owner/repo'"
            )
        if self.hf_revision is not None and not _HF_REV_RE.fullmatch(self.hf_revision):
            raise ValueError(f"Invalid Hugging Face revision: {self.hf_revision!r}")
        if self.hf_revision is not None and self.hf_repo is None:
            raise ValueError("--hf-revision requires --hf-repo")

    @property
    def effective_model(self) -> str:
        if self.model:
            return self.model
        if self.engine == "maya":
            return DEFAULT_MAYA_MODEL
        if self.hf_repo:
            return self.hf_repo
        raise ValueError(f"Engine {self.engine!r} requires --model (or --hf-repo)")
