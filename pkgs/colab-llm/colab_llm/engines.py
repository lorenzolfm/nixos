"""Modular inference engine registry and bootstrap environment builders."""

from __future__ import annotations

import os
import re
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

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
    LaunchConfig,
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
    name: str
    description: str
    health_path: str
    script_filename: str
    env_builder: Callable[[LaunchConfig], dict[str, str]]

    def script_path(self) -> Path:
        path = engines_dir() / self.script_filename
        if not path.is_file():
            raise FileNotFoundError(f"Engine bootstrap script missing: {path}")
        return path

    @property
    def default_model(self) -> str:
        return DEFAULT_ENGINE_MODELS[self.name]

    def build_env(self, config: LaunchConfig) -> dict[str, str]:
        assert config.engine == self.name
        env = {
            "COLAB_LLM_ENGINE": self.name,
            "COLAB_LLM_REMOTE_PORT": str(config.remote_port),
            "COLAB_LLM_CONTEXT_LENGTH": str(config.context_length),
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
        engine_env = self.env_builder(config)
        assert not (env.keys() & engine_env.keys()), "engine env must not shadow shared env"
        env.update(engine_env)
        for name, value in env.items():
            assert _ENV_NAME_RE.fullmatch(name), name
            assert isinstance(value, str), name
        return env


def engines_dir() -> Path:
    override = os.environ.get("COLAB_LLM_ENGINES_DIR")
    if override:
        return Path(override)
    return Path(__file__).resolve().parent.parent / "engines"


def _maya_env(config: LaunchConfig) -> dict[str, str]:
    model = config.effective_model
    hf_repo = config.hf_repo or ""
    hf_revision = config.hf_revision or ""
    custom_model_dir = "0"

    if config.hf_repo:
        custom_model_dir = "1"
    elif model in KNOWN_MAYA_HF_PREFETCH:
        hf_repo, hf_revision = KNOWN_MAYA_HF_PREFETCH[model]

    return {
        "MAYA_REPO_URL": MAYA_REPO_URL,
        "MAYA_PINNED_TAG": MAYA_PINNED_TAG,
        "MAYA_PINNED_COMMIT": MAYA_PINNED_COMMIT,
        "MAYA_MODEL": model,
        "MAYA_DRAFT": config.draft,
        "MAYA_PARALLEL": str(config.parallel),
        "MAYA_VISION": "1" if config.vision else "0",
        "MAYA_WEB_UI": "1" if config.web_ui else "0",
        "MAYA_HF_REPO": hf_repo,
        "MAYA_HF_REVISION": hf_revision,
        "MAYA_CUSTOM_MODEL_DIR": custom_model_dir,
        "HF_HUB_PINNED_VERSION": HF_HUB_PINNED_VERSION,
        "HF_TRANSFER_PINNED_VERSION": HF_TRANSFER_PINNED_VERSION,
    }


def _vllm_env(config: LaunchConfig) -> dict[str, str]:
    return {
        "VLLM_MODEL": config.effective_model,
        "VLLM_REVISION": config.hf_revision or "",
        "VLLM_PINNED_VERSION": VLLM_PINNED_VERSION,
    }


def _llama_cpp_env(config: LaunchConfig) -> dict[str, str]:
    return {
        "LLAMA_HF_REPO": config.hf_repo or config.effective_model,
        "LLAMA_PARALLEL": str(config.parallel),
        "LLAMA_CPP_REPO_URL": LLAMA_CPP_REPO_URL,
        "LLAMA_CPP_PINNED_TAG": LLAMA_CPP_PINNED_TAG,
        "LLAMA_CPP_PINNED_COMMIT": LLAMA_CPP_PINNED_COMMIT,
    }


def _sglang_env(config: LaunchConfig) -> dict[str, str]:
    return {
        "SGLANG_MODEL": config.effective_model,
        "SGLANG_REVISION": config.hf_revision or "",
        "SGLANG_PINNED_VERSION": SGLANG_PINNED_VERSION,
    }


ENGINES: dict[str, EngineSpec] = {
    "maya": EngineSpec(
        name="maya",
        description=(
            "Project Maya (mw00/project-maya v1.1.82) — GLM-5.3-Flash EXL3 + "
            "DFlash2 with OpenAI /v1 and Anthropic /v1/messages APIs"
        ),
        health_path="/health",
        script_filename="maya.sh",
        env_builder=_maya_env,
    ),
    "vllm": EngineSpec(
        name="vllm",
        description="vLLM OpenAI-compatible server for Hugging Face checkpoints",
        health_path="/health",
        script_filename="vllm.sh",
        env_builder=_vllm_env,
    ),
    "llama-cpp": EngineSpec(
        name="llama-cpp",
        description="llama.cpp server (llama-server) for GGUF checkpoints",
        health_path="/health",
        script_filename="llama-cpp.sh",
        env_builder=_llama_cpp_env,
    ),
    "sglang": EngineSpec(
        name="sglang",
        description="SGLang OpenAI-compatible runtime for Hugging Face checkpoints",
        health_path="/health",
        script_filename="sglang.sh",
        env_builder=_sglang_env,
    ),
}


def get_engine(name: str) -> EngineSpec:
    try:
        return ENGINES[name]
    except KeyError as exc:
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
