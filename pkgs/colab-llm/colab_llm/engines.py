"""Modular inference engine registry and bootstrap environment builders."""

from __future__ import annotations

import os
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

from colab_llm.config import (
    DEFAULT_MAYA_MODEL,
    KNOWN_MAYA_HF_PREFETCH,
    MAYA_PINNED_COMMIT,
    MAYA_PINNED_TAG,
    MAYA_REPO_URL,
    LaunchConfig,
)


@dataclass(frozen=True)
class EngineSpec:
    name: str
    description: str
    default_model: str
    health_path: str
    script_filename: str
    env_builder: Callable[[LaunchConfig], dict[str, str]]

    def script_path(self) -> Path:
        path = engines_dir() / self.script_filename
        if not path.is_file():
            raise FileNotFoundError(f"Engine bootstrap script missing: {path}")
        return path

    def build_env(self, cfg: LaunchConfig) -> dict[str, str]:
        env = {
            "COLAB_LLM_ENGINE": self.name,
            "COLAB_LLM_REMOTE_PORT": str(cfg.remote_port),
            "COLAB_LLM_CONTEXT_LENGTH": str(cfg.context_length),
            "COLAB_LLM_IDLE_TIMEOUT_S": str(cfg.idle_timeout_s),
            "COLAB_LLM_MAX_DURATION_S": str(cfg.max_duration_s),
            "COLAB_LLM_FAST_HF": "1" if cfg.fast_hf else "0",
            "COLAB_LLM_RECONFIGURE": "1" if cfg.reconfigure else "0",
        }
        env.update(self.env_builder(cfg))
        return env


def engines_dir() -> Path:
    override = os.environ.get("COLAB_LLM_ENGINES_DIR")
    if override:
        return Path(override)
    return Path(__file__).resolve().parent.parent / "engines"


def _maya_env(cfg: LaunchConfig) -> dict[str, str]:
    model = cfg.effective_model
    hf_repo = cfg.hf_repo or ""
    hf_revision = cfg.hf_revision or ""
    custom_model_dir = "0"

    if cfg.hf_repo:
        custom_model_dir = "1"
    elif model in KNOWN_MAYA_HF_PREFETCH:
        hf_repo, hf_revision = KNOWN_MAYA_HF_PREFETCH[model]

    return {
        "MAYA_REPO_URL": MAYA_REPO_URL,
        "MAYA_PINNED_TAG": MAYA_PINNED_TAG,
        "MAYA_PINNED_COMMIT": MAYA_PINNED_COMMIT,
        "MAYA_MODEL": model,
        "MAYA_DRAFT": cfg.draft,
        "MAYA_PARALLEL": str(cfg.parallel),
        "MAYA_VISION": "1" if cfg.vision else "0",
        "MAYA_WEB_UI": "1" if cfg.web_ui else "0",
        "MAYA_HF_REPO": hf_repo,
        "MAYA_HF_REVISION": hf_revision,
        "MAYA_CUSTOM_MODEL_DIR": custom_model_dir,
    }


def _vllm_env(cfg: LaunchConfig) -> dict[str, str]:
    return {
        "VLLM_MODEL": cfg.effective_model,
        "VLLM_REVISION": cfg.hf_revision or "",
    }


def _llama_cpp_env(cfg: LaunchConfig) -> dict[str, str]:
    return {
        "LLAMA_HF_REPO": cfg.hf_repo or cfg.effective_model,
        "LLAMA_MODEL_SPEC": cfg.effective_model,
        "LLAMA_PARALLEL": str(cfg.parallel),
    }


def _sglang_env(cfg: LaunchConfig) -> dict[str, str]:
    return {
        "SGLANG_MODEL": cfg.effective_model,
        "SGLANG_REVISION": cfg.hf_revision or "",
    }


ENGINES: dict[str, EngineSpec] = {
    "maya": EngineSpec(
        name="maya",
        description=(
            "Project Maya (mw00/project-maya v1.1.82) — GLM-5.3-Flash EXL3 + "
            "DFlash2 with OpenAI /v1 and Anthropic /v1/messages APIs"
        ),
        default_model=DEFAULT_MAYA_MODEL,
        health_path="/health",
        script_filename="maya.sh",
        env_builder=_maya_env,
    ),
    "vllm": EngineSpec(
        name="vllm",
        description="vLLM OpenAI-compatible server for Hugging Face checkpoints",
        default_model="Qwen/Qwen3-Coder-30B-A3B-Instruct",
        health_path="/health",
        script_filename="vllm.sh",
        env_builder=_vllm_env,
    ),
    "llama-cpp": EngineSpec(
        name="llama-cpp",
        description="llama.cpp server (llama-server) for GGUF checkpoints",
        default_model="unsloth/GLM-5.3-Flash-GGUF:UD-IQ2_XXS",
        health_path="/health",
        script_filename="llama-cpp.sh",
        env_builder=_llama_cpp_env,
    ),
    "sglang": EngineSpec(
        name="sglang",
        description="SGLang OpenAI-compatible runtime for Hugging Face checkpoints",
        default_model="Qwen/Qwen3-Coder-30B-A3B-Instruct",
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


def list_engines() -> list[EngineSpec]:
    return list(ENGINES.values())
