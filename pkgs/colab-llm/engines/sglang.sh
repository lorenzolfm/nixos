#!/usr/bin/env bash
# Colab bootstrap for SGLang OpenAI-compatible serving.
# Never mounts Google Drive (preserves #455 backup isolation).
set -euo pipefail

# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

VENV_DIR="${WORK_DIR}/.sglang-venv"
SGLANG_MODEL="${SGLANG_MODEL:?}"
SGLANG_REVISION="${SGLANG_REVISION?}"
SGLANG_PINNED_VERSION="${SGLANG_PINNED_VERSION:?}"

bootstrap_begin

if [[ ! -x "${VENV_DIR}/bin/python" ]]; then
  log "Installing SGLang ${SGLANG_PINNED_VERSION} in ${VENV_DIR}..."
  packages=("sglang[all]==${SGLANG_PINNED_VERSION}" "huggingface_hub[hf_xet]" hf_transfer)
  if command -v uv >/dev/null 2>&1; then
    uv venv -q "${VENV_DIR}"
    uv pip install -q --python "${VENV_DIR}/bin/python" "${packages[@]}"
  else
    python3 -m venv "${VENV_DIR}"
    "${VENV_DIR}/bin/pip" install -q --no-input "${packages[@]}"
  fi
fi

serve_args=(
  -m sglang.launch_server
  --model-path "${SGLANG_MODEL}"
  --host 127.0.0.1
  --port "${REMOTE_PORT}"
  --context-length "${CONTEXT_LENGTH}"
)
if [[ -n "${SGLANG_REVISION}" ]]; then
  serve_args+=(--revision "${SGLANG_REVISION}")
fi

log "Starting SGLang (${SGLANG_MODEL}) on 127.0.0.1:${REMOTE_PORT}..."
: > "${LOG_FILE}"

(
  export HF_HUB_ENABLE_HF_TRANSFER=1
  if [[ -f "${HF_TOKEN_PATH}" ]]; then
    # SGLang downloads the model itself, so the token has to live in its environment.
    HF_TOKEN="$(cat "${HF_TOKEN_PATH}")"
    export HF_TOKEN
  fi
  nohup "${VENV_DIR}/bin/python" "${serve_args[@]}" > "${LOG_FILE}" 2>&1 < /dev/null &
  echo $! > "${PID_FILE}"
)

bootstrap_finish
