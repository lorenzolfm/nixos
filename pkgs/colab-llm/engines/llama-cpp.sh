#!/usr/bin/env bash
# Colab bootstrap for llama.cpp (`llama-server`) GGUF serving.
# Never mounts Google Drive (preserves #455 backup isolation).
set -euo pipefail

# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

LLAMA_DIR="${WORK_DIR}/llama.cpp"
LLAMA_HF_REPO="${LLAMA_HF_REPO:?}"
LLAMA_PARALLEL="${LLAMA_PARALLEL:?}"
LLAMA_CPP_REPO_URL="${LLAMA_CPP_REPO_URL:?}"
LLAMA_CPP_PINNED_TAG="${LLAMA_CPP_PINNED_TAG:?}"
LLAMA_CPP_PINNED_COMMIT="${LLAMA_CPP_PINNED_COMMIT:?}"

bootstrap_begin

if [[ ! -x "${LLAMA_DIR}/build/bin/llama-server" ]]; then
  checkout_pinned_commit \
    "${LLAMA_DIR}" "${LLAMA_CPP_REPO_URL}" "${LLAMA_CPP_PINNED_TAG}" "${LLAMA_CPP_PINNED_COMMIT}"
  log "Building llama.cpp with CUDA support..."
  cmake -S "${LLAMA_DIR}" -B "${LLAMA_DIR}/build" \
    -DGGML_CUDA=ON \
    -DLLAMA_CURL=ON \
    -DCMAKE_BUILD_TYPE=Release
  cmake --build "${LLAMA_DIR}/build" --target llama-server -j "$(nproc)"
fi

log "Starting llama-server (-hf ${LLAMA_HF_REPO}) on 127.0.0.1:${REMOTE_PORT}..."
: > "${LOG_FILE}"

(
  if [[ -f "${HF_TOKEN_PATH}" ]]; then
    # llama-server downloads the model itself, so the token has to live in its environment.
    HF_TOKEN="$(cat "${HF_TOKEN_PATH}")"
    export HF_TOKEN
  fi
  nohup "${LLAMA_DIR}/build/bin/llama-server" \
    -hf "${LLAMA_HF_REPO}" \
    --host 127.0.0.1 \
    --port "${REMOTE_PORT}" \
    --ctx-size "${CONTEXT_LENGTH}" \
    --parallel "${LLAMA_PARALLEL}" \
    --n-gpu-layers 999 \
    --flash-attn on \
    > "${LOG_FILE}" 2>&1 < /dev/null &
  echo $! > "${PID_FILE}"
)

bootstrap_finish
