"""Validated configuration types, defaults, and state paths for colab-llm."""

from __future__ import annotations

import os
import re
import signal
from dataclasses import dataclass, field
from enum import StrEnum
from pathlib import Path
from typing import assert_never

DEFAULT_SESSION = "colab-llm"
DEFAULT_GPU = "G4"
ALLOWED_GPUS = ("G4", "A100", "H100", "L4", "T4")
# Colab rejects `shape=hm` (HTTP 400) on single-shape GPU tiers.
SINGLE_SHAPE_GPUS = frozenset({"G4", "L4"})
assert SINGLE_SHAPE_GPUS <= set(ALLOWED_GPUS)

# Colab AUP guardrail: bind strictly to loopback so endpoints are never exposed
# publicly or accidentally bound to wildcard interfaces.
DEFAULT_BIND_HOST = "127.0.0.1"
LOOPBACK_HOSTS = frozenset({"127.0.0.1", "localhost", "::1"})

DEFAULT_LOCAL_PORT = 18100
DEFAULT_REMOTE_PORT = 18100

DEFAULT_IDLE_TIMEOUT_S = 1800  # 30 minutes
IDLE_TIMEOUT_S_MIN = 60
IDLE_TIMEOUT_S_MAX = 14400  # 4 hours

DEFAULT_MAX_DURATION_S = 14400  # 4 hours hard cap
DURATION_S_MIN = 300  # 5 minutes
DURATION_S_MAX = 43200  # 12 hours

DEFAULT_BOOTSTRAP_TIMEOUT_S = 1800  # 30 minutes (download + compile + load)
BOOTSTRAP_TIMEOUT_S_MIN = 60
BOOTSTRAP_TIMEOUT_S_MAX = 7200  # 2 hours

# Heartbeats keep the remote deadman from firing while the workstation is alive.
# The local supervisor sends one every HEARTBEAT_INTERVAL_S over the tunnel; the
# bootstrap script sends its own every BOOTSTRAP_HEARTBEAT_INTERVAL_S because no
# tunnel exists yet. The deadman tolerates idle_timeout_s + DEADMAN_HEARTBEAT_GRACE_S
# of silence, so the grace must cover at least two missed supervisor heartbeats.
HEARTBEAT_INTERVAL_S = 300
BOOTSTRAP_HEARTBEAT_INTERVAL_S = 60
DEADMAN_HEARTBEAT_GRACE_S = 600
DEADMAN_DURATION_GRACE_S = 300
assert HEARTBEAT_INTERVAL_S * 2 <= DEADMAN_HEARTBEAT_GRACE_S
assert BOOTSTRAP_HEARTBEAT_INTERVAL_S <= HEARTBEAT_INTERVAL_S

class Engine(StrEnum):
    MAYA = "maya"
    VLLM = "vllm"
    LLAMA_CPP = "llama-cpp"
    SGLANG = "sglang"


class Draft(StrEnum):
    """Project Maya drafting mode."""

    DFLASH2 = "dflash2"
    MTP = "mtp"
    OFF = "off"


DEFAULT_ENGINE = Engine.MAYA
ALLOWED_ENGINES = tuple(engine.value for engine in Engine)

DEFAULT_MAYA_MODEL = "EXL3-3.05bpw"
DEFAULT_MAYA_DRAFT = Draft.DFLASH2
ALLOWED_MAYA_DRAFTS = tuple(draft.value for draft in Draft)
DEFAULT_CONTEXT_LENGTH = 131072
CONTEXT_LENGTH_MIN = 4096
CONTEXT_LENGTH_MAX = 1048576
DEFAULT_PARALLEL = 1
PARALLEL_MAX = 8

# Model used when neither --model nor --hf-repo is given, per engine. Resolved at
# config construction so a missing model can never surface after GPU allocation.
DEFAULT_ENGINE_MODELS: dict[Engine, str] = {
    Engine.MAYA: DEFAULT_MAYA_MODEL,
    Engine.VLLM: "Qwen/Qwen3-Coder-30B-A3B-Instruct",
    Engine.LLAMA_CPP: "unsloth/GLM-5.3-Flash-GGUF:UD-IQ2_XXS",
    Engine.SGLANG: "Qwen/Qwen3-Coder-30B-A3B-Instruct",
}
assert set(DEFAULT_ENGINE_MODELS) == set(Engine)

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

# Pinned remote build inputs for the other engines. These run as root on the VM,
# so they are pinned for the same reason Maya is. Transitive pip dependencies are
# still resolved at install time.
LLAMA_CPP_REPO_URL = "https://github.com/ggml-org/llama.cpp.git"
LLAMA_CPP_PINNED_TAG = "b12826"
LLAMA_CPP_PINNED_COMMIT = "8e23278c7c36f4429fb237467d50e2935a91e655"
VLLM_PINNED_VERSION = "0.34.0"
SGLANG_PINNED_VERSION = "0.5.23"
HF_HUB_PINNED_VERSION = "2.5.0"
HF_TRANSFER_PINNED_VERSION = "0.1.9"


class StopReason(StrEnum):
    """Why a supervised session ended. Recorded in the state file and ledger."""

    ERROR = "error"
    SIGNAL_SIGINT = "signal:SIGINT"
    SIGNAL_SIGTERM = "signal:SIGTERM"
    SIGNAL_SIGHUP = "signal:SIGHUP"
    HEALTH_TIMEOUT = "startup_timeout"
    REMOTE_PROCESS_EXITED = "remote_process_exited"
    IDLE_TIMEOUT = "idle_timeout"
    MAX_DURATION = "max_duration"
    TUNNEL_DISCONNECTED = "tunnel_disconnected"


SIGNAL_STOP_REASONS: dict[signal.Signals, StopReason] = {
    signal.SIGINT: StopReason.SIGNAL_SIGINT,
    signal.SIGTERM: StopReason.SIGNAL_SIGTERM,
    signal.SIGHUP: StopReason.SIGNAL_SIGHUP,
}


def exit_code_for(stop_reason: StopReason, *, was_ready: bool) -> int:
    """Process exit code for a finished session. Total: every reason is decided here."""
    match stop_reason:
        case StopReason.SIGNAL_SIGINT | StopReason.SIGNAL_SIGTERM | StopReason.SIGNAL_SIGHUP:
            return 0
        case StopReason.IDLE_TIMEOUT | StopReason.MAX_DURATION:
            return 0
        case StopReason.TUNNEL_DISCONNECTED:
            # A tunnel that never carried a ready engine is a failed start.
            return 0 if was_ready else 1
        case StopReason.HEALTH_TIMEOUT | StopReason.REMOTE_PROCESS_EXITED | StopReason.ERROR:
            return 1
        case _:
            assert_never(stop_reason)


_SESSION_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
_DURATION_TOKEN_RE = re.compile(r"(\d+)\s*([hms]?)", re.IGNORECASE)
_HF_REPO_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
_HF_REV_RE = re.compile(r"^[A-Za-z0-9_./-]{1,128}$")
_MODEL_NAME_RE = re.compile(r"^[A-Za-z0-9_./:-]{1,200}$")
# What `llama-server -hf` accepts: owner/repo with an optional quant tag.
_GGUF_MODEL_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(:[A-Za-z0-9_.-]+)?$")


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
    if not (IDLE_TIMEOUT_S_MIN <= idle_timeout_s <= IDLE_TIMEOUT_S_MAX):
        raise ValueError(
            f"idle_timeout_s={idle_timeout_s} out of bounds "
            f"[{IDLE_TIMEOUT_S_MIN}, {IDLE_TIMEOUT_S_MAX}]"
        )
    if not (DURATION_S_MIN <= max_duration_s <= DURATION_S_MAX):
        raise ValueError(
            f"max_duration_s={max_duration_s} out of bounds "
            f"[{DURATION_S_MIN}, {DURATION_S_MAX}]"
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
    if pos != len(text):
        raise ValueError(f"Invalid duration syntax: {spec!r}")
    if total <= 0:
        raise ValueError(f"Duration must be positive: {spec!r}")
    return total


def deadman_heartbeat_timeout_s(idle_timeout_s: int) -> int:
    """Heartbeat silence the remote deadman tolerates before stopping the VM."""
    assert idle_timeout_s >= IDLE_TIMEOUT_S_MIN
    timeout_s = idle_timeout_s + DEADMAN_HEARTBEAT_GRACE_S
    assert timeout_s > HEARTBEAT_INTERVAL_S
    return timeout_s


def deadman_duration_s_max(max_duration_s: int, bootstrap_timeout_s: int) -> int:
    """Wall-clock cap for the remote deadman, which starts before the bootstrap.

    The local hard cap only starts once the engine is ready, after up to one
    bootstrap timeout of setup and one more of waiting for health. The remote
    cap must outlast all three so it never fires before the local one.
    """
    assert max_duration_s >= DURATION_S_MIN
    assert bootstrap_timeout_s >= BOOTSTRAP_TIMEOUT_S_MIN
    return max_duration_s + 2 * bootstrap_timeout_s + DEADMAN_DURATION_GRACE_S


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


def _validate_parallel(parallel: int) -> None:
    if not (1 <= parallel <= PARALLEL_MAX):
        raise ValueError(f"parallel={parallel} out of bounds [1, {PARALLEL_MAX}]")


@dataclass(frozen=True)
class HfModel:
    """A Hugging Face repository, optionally pinned to a branch, tag or commit."""

    repo: str
    revision: str | None = None

    def __post_init__(self) -> None:
        if not _HF_REPO_RE.fullmatch(self.repo):
            raise ValueError(f"Invalid Hugging Face repo {self.repo!r}: expected 'owner/repo'")
        if self.revision is not None and not _HF_REV_RE.fullmatch(self.revision):
            raise ValueError(f"Invalid Hugging Face revision: {self.revision!r}")


# One variant per engine, each holding only the options that engine has. An
# option for the wrong engine is therefore not a value that can be built.


@dataclass(frozen=True)
class MayaConfig:
    # A named Maya quant, or a custom EXL3 repo served from its own directory.
    model: str | HfModel = DEFAULT_MAYA_MODEL
    draft: Draft = DEFAULT_MAYA_DRAFT
    parallel: int = DEFAULT_PARALLEL
    vision: bool = True
    web_ui: bool = False

    def __post_init__(self) -> None:
        if isinstance(self.model, str) and not _MODEL_NAME_RE.fullmatch(self.model):
            raise ValueError(f"Invalid model identifier: {self.model!r}")
        _validate_parallel(self.parallel)

    @property
    def kind(self) -> Engine:
        return Engine.MAYA

    @property
    def served_model(self) -> str:
        return self.model.repo if isinstance(self.model, HfModel) else self.model


@dataclass(frozen=True)
class VllmConfig:
    model: HfModel = field(default_factory=lambda: HfModel(DEFAULT_ENGINE_MODELS[Engine.VLLM]))

    @property
    def kind(self) -> Engine:
        return Engine.VLLM

    @property
    def served_model(self) -> str:
        return self.model.repo


@dataclass(frozen=True)
class LlamaCppConfig:
    # `owner/repo[:quant]`, as taken by `llama-server -hf`. It has no revision.
    model: str = DEFAULT_ENGINE_MODELS[Engine.LLAMA_CPP]
    parallel: int = DEFAULT_PARALLEL

    def __post_init__(self) -> None:
        if not _GGUF_MODEL_RE.fullmatch(self.model):
            raise ValueError(
                f"Invalid GGUF model {self.model!r}: expected 'owner/repo' or 'owner/repo:quant'"
            )
        _validate_parallel(self.parallel)

    @property
    def kind(self) -> Engine:
        return Engine.LLAMA_CPP

    @property
    def served_model(self) -> str:
        return self.model


@dataclass(frozen=True)
class SglangConfig:
    model: HfModel = field(default_factory=lambda: HfModel(DEFAULT_ENGINE_MODELS[Engine.SGLANG]))

    @property
    def kind(self) -> Engine:
        return Engine.SGLANG

    @property
    def served_model(self) -> str:
        return self.model.repo


EngineConfig = MayaConfig | VllmConfig | LlamaCppConfig | SglangConfig


def build_engine_config(
    engine: Engine,
    *,
    model: str | None = None,
    hf_repo: str | None = None,
    hf_revision: str | None = None,
    draft: str | None = None,
    parallel: int | None = None,
    no_vision: bool = False,
    web_ui: bool = False,
) -> EngineConfig:
    """Parse the engine flags of `start` into the one variant they describe.

    `None` and `False` mean the flag was not given. A flag the chosen engine
    does not have is rejected by name instead of being dropped.
    """

    def reject(**flags: object) -> None:
        for name, value in flags.items():
            if value is not None and value is not False:
                flag = "--" + name.replace("_", "-")
                raise ValueError(f"{flag} does not apply to --engine {engine.value}")

    if model is not None and hf_repo is not None:
        raise ValueError("--model and --hf-repo both name the model to serve: pass only one")

    match engine:
        case Engine.MAYA:
            if hf_revision is not None and hf_repo is None:
                raise ValueError("--hf-revision requires --hf-repo")
            maya_model: str | HfModel = DEFAULT_MAYA_MODEL
            if hf_repo is not None:
                maya_model = HfModel(hf_repo, hf_revision)
            elif model is not None:
                maya_model = model
            return MayaConfig(
                model=maya_model,
                draft=Draft(draft) if draft is not None else DEFAULT_MAYA_DRAFT,
                parallel=parallel if parallel is not None else DEFAULT_PARALLEL,
                vision=not no_vision,
                web_ui=web_ui,
            )
        case Engine.VLLM:
            reject(draft=draft, parallel=parallel, no_vision=no_vision, web_ui=web_ui)
            repo = model or hf_repo or DEFAULT_ENGINE_MODELS[engine]
            return VllmConfig(model=HfModel(repo, hf_revision))
        case Engine.SGLANG:
            reject(draft=draft, parallel=parallel, no_vision=no_vision, web_ui=web_ui)
            repo = model or hf_repo or DEFAULT_ENGINE_MODELS[engine]
            return SglangConfig(model=HfModel(repo, hf_revision))
        case Engine.LLAMA_CPP:
            reject(hf_revision=hf_revision, draft=draft, no_vision=no_vision, web_ui=web_ui)
            return LlamaCppConfig(
                model=model or hf_repo or DEFAULT_ENGINE_MODELS[engine],
                parallel=parallel if parallel is not None else DEFAULT_PARALLEL,
            )
        case _:
            assert_never(engine)


@dataclass(frozen=True)
class Attach:
    """Adopt an existing Colab runtime instead of allocating one.

    `endpoint` names the runtime; None means the single unnamed runtime with
    the requested GPU.
    """

    endpoint: str | None = None


@dataclass(frozen=True)
class LaunchConfig:
    session: str = DEFAULT_SESSION
    gpu: str = DEFAULT_GPU
    high_mem: bool = False
    engine: EngineConfig = field(default_factory=MayaConfig)
    context_length: int = DEFAULT_CONTEXT_LENGTH
    bind_host: str = DEFAULT_BIND_HOST
    local_port: int = DEFAULT_LOCAL_PORT
    remote_port: int = DEFAULT_REMOTE_PORT
    idle_timeout_s: int = DEFAULT_IDLE_TIMEOUT_S
    max_duration_s: int = DEFAULT_MAX_DURATION_S
    bootstrap_timeout_s: int = DEFAULT_BOOTSTRAP_TIMEOUT_S
    fast_hf: bool = True
    rm_on_disconnect: bool = False
    reconfigure: bool = False
    attach: Attach | None = None
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

        if self.high_mem and self.gpu in SINGLE_SHAPE_GPUS:
            raise ValueError(f"--high-mem is not available on {self.gpu}: it has a single shape")
        if not (CONTEXT_LENGTH_MIN <= self.context_length <= CONTEXT_LENGTH_MAX):
            raise ValueError(
                f"context_length={self.context_length} out of bounds "
                f"[{CONTEXT_LENGTH_MIN}, {CONTEXT_LENGTH_MAX}]"
            )
        if not (BOOTSTRAP_TIMEOUT_S_MIN <= self.bootstrap_timeout_s <= BOOTSTRAP_TIMEOUT_S_MAX):
            raise ValueError(
                f"bootstrap_timeout_s={self.bootstrap_timeout_s} out of bounds "
                f"[{BOOTSTRAP_TIMEOUT_S_MIN}, {BOOTSTRAP_TIMEOUT_S_MAX}]"
            )
