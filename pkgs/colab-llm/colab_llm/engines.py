"""Modular inference engine registry and bootstrap environment builders."""

from __future__ import annotations

import hashlib
import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import assert_never

from colab_llm.config import (
    BOOTSTRAP_HEARTBEAT_INTERVAL_S,
    DEFAULT_ENGINE_MODELS,
    HF_HUB_PINNED_VERSION,
    HF_TRANSFER_PINNED_VERSION,
    KNOWN_MAYA_HF_PREFETCH,
    LLAMA_CPP_PINNED_COMMIT,
    LLAMA_CPP_PINNED_TAG,
    LLAMA_CPP_REPO_URL,
    MAYA_PINNED_COMMIT,
    MAYA_PINNED_TAG,
    MAYA_REPO_URL,
    SGLANG_PINNED_VERSION,
    VLLM_PINNED_VERSION,
    Engine,
    EngineConfig,
    HfModel,
    LaunchConfig,
    LlamaCppConfig,
    MayaConfig,
    SglangConfig,
    VllmConfig,
    deadman_duration_s_max,
    deadman_heartbeat_timeout_s,
)

# Shared by every engine bootstrap; uploaded next to the engine script.
COMMON_SCRIPT_FILENAME = "common.sh"
DEADMAN_SCRIPT_FILENAME = "deadman.py"

# Environment names are interpolated unquoted into the remote command line, so
# they must be plain identifiers. Values are always shell-quoted by the caller.
_ENV_NAME_RE = re.compile(r"^[A-Z][A-Z0-9_]*$")


@dataclass(frozen=True)
class EngineSpec:
    name: Engine
    description: str
    health_path: str
    script_filename: str

    def script_path(self) -> Path:
        path = engines_dir() / self.script_filename
        if not path.is_file():
            raise FileNotFoundError(f"Engine bootstrap script missing: {path}")
        return path

    @property
    def default_model(self) -> str:
        return DEFAULT_ENGINE_MODELS[self.name]

    def build_env(self, config: LaunchConfig) -> dict[str, str]:
        assert config.engine.kind == self.name
        # What decides which server runs. A healthy server left on the VM is
        # reused only if it was started from the same values.
        serving_env = {
            "COLAB_LLM_ENGINE": self.name.value,
            "COLAB_LLM_REMOTE_PORT": str(config.remote_port),
            "COLAB_LLM_CONTEXT_LENGTH": str(config.context_length),
        }
        engine_env = _engine_env(config.engine)
        assert not (serving_env.keys() & engine_env.keys()), "engine env must not shadow shared env"
        serving_env.update(engine_env)

        env = {
            "COLAB_LLM_CONFIG_FINGERPRINT": _fingerprint(serving_env),
            "COLAB_LLM_BOOTSTRAP_TIMEOUT_S": str(config.bootstrap_timeout_s),
            "COLAB_LLM_BOOTSTRAP_HEARTBEAT_INTERVAL_S": str(BOOTSTRAP_HEARTBEAT_INTERVAL_S),
            "COLAB_LLM_DEADMAN_HEARTBEAT_TIMEOUT_S": str(
                deadman_heartbeat_timeout_s(config.idle_timeout_s)
            ),
            "COLAB_LLM_DEADMAN_DURATION_S_MAX": str(
                deadman_duration_s_max(config.max_duration_s, config.bootstrap_timeout_s)
            ),
            "COLAB_LLM_FAST_HF": "1" if config.fast_hf else "0",
            "COLAB_LLM_RECONFIGURE": "1" if config.reconfigure else "0",
        }
        assert not (env.keys() & serving_env.keys())
        env.update(serving_env)
        for name, value in env.items():
            assert _ENV_NAME_RE.fullmatch(name), name
            assert isinstance(value, str), name
        return env


def _fingerprint(serving_env: dict[str, str]) -> str:
    digest = hashlib.sha256()
    for name, value in sorted(serving_env.items()):
        digest.update(f"{name}={value}\0".encode())
    return digest.hexdigest()


def engines_dir() -> Path:
    override = os.environ.get("COLAB_LLM_ENGINES_DIR")
    if override:
        return Path(override)
    return Path(__file__).resolve().parent.parent / "engines"


def _maya_env(engine: MayaConfig) -> dict[str, str]:
    # Empty strings mean "absent" on the bash side; maya.sh checks that exactly
    # one of a named model and a custom repo arrives.
    model_name = ""
    hf_repo = ""
    hf_revision = ""
    custom_model_dir = "0"
    match engine.model:
        case HfModel(repo=repo, revision=revision):
            hf_repo = repo
            hf_revision = revision or ""
            custom_model_dir = "1"
        case str() as name:
            model_name = name
            if name in KNOWN_MAYA_HF_PREFETCH:
                hf_repo, hf_revision = KNOWN_MAYA_HF_PREFETCH[name]
        case _:
            assert_never(engine.model)

    return {
        "MAYA_REPO_URL": MAYA_REPO_URL,
        "MAYA_PINNED_TAG": MAYA_PINNED_TAG,
        "MAYA_PINNED_COMMIT": MAYA_PINNED_COMMIT,
        "MAYA_MODEL": model_name,
        "MAYA_DRAFT": engine.draft.value,
        "MAYA_PARALLEL": str(engine.parallel),
        "MAYA_VISION": "1" if engine.vision else "0",
        "MAYA_WEB_UI": "1" if engine.web_ui else "0",
        "MAYA_HF_REPO": hf_repo,
        "MAYA_HF_REVISION": hf_revision,
        "MAYA_CUSTOM_MODEL_DIR": custom_model_dir,
        "HF_HUB_PINNED_VERSION": HF_HUB_PINNED_VERSION,
        "HF_TRANSFER_PINNED_VERSION": HF_TRANSFER_PINNED_VERSION,
    }


def _vllm_env(engine: VllmConfig) -> dict[str, str]:
    return {
        "VLLM_MODEL": engine.model.repo,
        "VLLM_REVISION": engine.model.revision or "",
        "VLLM_PINNED_VERSION": VLLM_PINNED_VERSION,
    }


def _llama_cpp_env(engine: LlamaCppConfig) -> dict[str, str]:
    return {
        "LLAMA_HF_REPO": engine.model,
        "LLAMA_PARALLEL": str(engine.parallel),
        "LLAMA_CPP_REPO_URL": LLAMA_CPP_REPO_URL,
        "LLAMA_CPP_PINNED_TAG": LLAMA_CPP_PINNED_TAG,
        "LLAMA_CPP_PINNED_COMMIT": LLAMA_CPP_PINNED_COMMIT,
    }


def _sglang_env(engine: SglangConfig) -> dict[str, str]:
    return {
        "SGLANG_MODEL": engine.model.repo,
        "SGLANG_REVISION": engine.model.revision or "",
        "SGLANG_PINNED_VERSION": SGLANG_PINNED_VERSION,
    }


def _engine_env(engine: EngineConfig) -> dict[str, str]:
    match engine:
        case MayaConfig():
            return _maya_env(engine)
        case VllmConfig():
            return _vllm_env(engine)
        case LlamaCppConfig():
            return _llama_cpp_env(engine)
        case SglangConfig():
            return _sglang_env(engine)
        case _:
            assert_never(engine)


ENGINES: dict[Engine, EngineSpec] = {
    Engine.MAYA: EngineSpec(
        name=Engine.MAYA,
        description=(
            "Project Maya (mw00/project-maya v1.1.82) — GLM-5.3-Flash EXL3 + "
            "DFlash2 with OpenAI /v1 and Anthropic /v1/messages APIs"
        ),
        health_path="/health",
        script_filename="maya.sh",
    ),
    Engine.VLLM: EngineSpec(
        name=Engine.VLLM,
        description="vLLM OpenAI-compatible server for Hugging Face checkpoints",
        health_path="/health",
        script_filename="vllm.sh",
    ),
    Engine.LLAMA_CPP: EngineSpec(
        name=Engine.LLAMA_CPP,
        description="llama.cpp server (llama-server) for GGUF checkpoints",
        health_path="/health",
        script_filename="llama-cpp.sh",
    ),
    Engine.SGLANG: EngineSpec(
        name=Engine.SGLANG,
        description="SGLang OpenAI-compatible runtime for Hugging Face checkpoints",
        health_path="/health",
        script_filename="sglang.sh",
    ),
}
assert set(ENGINES) == set(Engine)
assert all(spec.name == key for key, spec in ENGINES.items())


def get_engine(name: Engine | str) -> EngineSpec:
    try:
        return ENGINES[Engine(name)]
    except ValueError as exc:
        supported = ", ".join(ENGINES)
        raise ValueError(f"Unknown engine {name!r}: supported engines are {supported}") from exc


def support_script_paths() -> list[Path]:
    """Files every engine bootstrap needs on the VM besides its own script."""
    paths = [engines_dir() / COMMON_SCRIPT_FILENAME, engines_dir() / DEADMAN_SCRIPT_FILENAME]
    for path in paths:
        if not path.is_file():
            raise FileNotFoundError(f"Missing remote support script: {path}")
    return paths


def list_engines() -> list[EngineSpec]:
    return list(ENGINES.values())
