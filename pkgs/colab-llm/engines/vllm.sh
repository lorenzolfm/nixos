#!/usr/bin/env bash
# Colab bootstrap for vLLM OpenAI-compatible serving.
# Never mounts Google Drive (preserves #455 backup isolation).
set -euo pipefail

# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

VENV_DIR="${WORK_DIR}/.vllm-venv"
VLLM_MODEL="${VLLM_MODEL:?}"
VLLM_REVISION="${VLLM_REVISION?}"
VLLM_PINNED_VERSION="${VLLM_PINNED_VERSION:?}"

bootstrap_begin

INSTALL_MARKER="${VENV_DIR}/.colab-llm-installed"
if ! install_marker_matches "${INSTALL_MARKER}" "${VLLM_PINNED_VERSION}"; then
  rm -rf "${VENV_DIR}"
  log "Installing vLLM ${VLLM_PINNED_VERSION} in ${VENV_DIR}..."
  packages=("vllm==${VLLM_PINNED_VERSION}" "huggingface_hub[hf_xet]" hf_transfer)
  if command -v uv >/dev/null 2>&1; then
    uv venv -q "${VENV_DIR}"
    uv pip install -q --python "${VENV_DIR}/bin/python" "${packages[@]}"
  else
    python3 -m venv "${VENV_DIR}"
    "${VENV_DIR}/bin/pip" install -q --no-input "${packages[@]}"
  fi
  printf '%s\n' "${VLLM_PINNED_VERSION}" > "${INSTALL_MARKER}"
fi

serve_args=(
  serve "${VLLM_MODEL}"
  --host 127.0.0.1
  --port "${REMOTE_PORT}"
  --max-model-len "${CONTEXT_LENGTH}"
)
if [[ -n "${VLLM_REVISION}" ]]; then
  serve_args+=(--revision "${VLLM_REVISION}")
fi

log "Starting vLLM (${VLLM_MODEL}) on 127.0.0.1:${REMOTE_PORT}..."
: > "${LOG_FILE}"

(
  export HF_HUB_ENABLE_HF_TRANSFER=1
  if [[ -f "${HF_TOKEN_PATH}" ]]; then
    # vLLM downloads the model itself, so the token has to live in its environment.
    HF_TOKEN="$(cat "${HF_TOKEN_PATH}")"
    export HF_TOKEN
  fi
  nohup "${VENV_DIR}/bin/vllm" "${serve_args[@]}" > "${LOG_FILE}" 2>&1 < /dev/null &
  echo $! > "${PID_FILE}"
)

bootstrap_finish
