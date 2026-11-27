#!/usr/bin/env bash
# Modular Colab bootstrap for llama.cpp (`llama-server`) GGUF serving.
# Never mounts Google Drive (preserves #455 backup isolation).
set -euo pipefail

export PATH="/usr/local/cuda/bin:/usr/local/bin:${PATH}"

WORK_DIR="/content/colab-llm"
LLAMA_DIR="${WORK_DIR}/llama.cpp"
LOG_FILE="${WORK_DIR}/server.log"
PID_FILE="${WORK_DIR}/server.pid"
DEADMAN_PID_FILE="${WORK_DIR}/deadman.pid"
HEARTBEAT_FILE="${WORK_DIR}/heartbeat"
HF_TOKEN_PATH="${WORK_DIR}/.hf_token"

LLAMA_HF_REPO="${LLAMA_HF_REPO:?LLAMA_HF_REPO must be set (e.g. owner/repo:QUANT)}"
LLAMA_PARALLEL="${LLAMA_PARALLEL:-1}"
REMOTE_PORT="${COLAB_LLM_REMOTE_PORT:-18100}"
CONTEXT_LENGTH="${COLAB_LLM_CONTEXT_LENGTH:-131072}"
IDLE_TIMEOUT_S="${COLAB_LLM_IDLE_TIMEOUT_S:-1800}"
MAX_DURATION_S="${COLAB_LLM_MAX_DURATION_S:-14400}"
RECONFIGURE="${COLAB_LLM_RECONFIGURE:-0}"

cleanup_secrets() {
  rm -f "${HF_TOKEN_PATH}"
}
trap cleanup_secrets EXIT

log() {
  printf '[llama-cpp-bootstrap %s] %s\n' "$(date -u +%H:%M:%S)" "$*"
}

mkdir -p "${WORK_DIR}"

if [[ "${RECONFIGURE}" != "1" ]] && [[ -f "${PID_FILE}" ]]; then
  existing_pid="$(cat "${PID_FILE}")"
  if kill -0 "${existing_pid}" 2>/dev/null \
    && curl -fsS --max-time 2 "http://127.0.0.1:${REMOTE_PORT}/health" >/dev/null 2>&1; then
    log "llama-server already healthy on 127.0.0.1:${REMOTE_PORT} (pid ${existing_pid}); reusing."
    date +%s > "${HEARTBEAT_FILE}"
    exit 0
  fi
fi

for pf in "${PID_FILE}" "${DEADMAN_PID_FILE}"; do
  if [[ -f "${pf}" ]]; then
    old_pid="$(cat "${pf}")"
    if [[ -n "${old_pid}" ]] && kill -0 "${old_pid}" 2>/dev/null; then
      kill "${old_pid}" 2>/dev/null || true
    fi
    rm -f "${pf}"
  fi
done

if [[ ! -x "${LLAMA_DIR}/build/bin/llama-server" ]]; then
  log "Building llama.cpp with CUDA support..."
  if [[ ! -d "${LLAMA_DIR}/.git" ]]; then
    git clone --depth 1 https://github.com/ggml-org/llama.cpp.git "${LLAMA_DIR}"
  fi
  cmake -S "${LLAMA_DIR}" -B "${LLAMA_DIR}/build" \
    -DGGML_CUDA=ON \
    -DLLAMA_CURL=ON \
    -DCMAKE_BUILD_TYPE=Release
  cmake --build "${LLAMA_DIR}/build" --target llama-server -j "$(nproc)"
fi

log "Starting llama-server (-hf ${LLAMA_HF_REPO}) on 127.0.0.1:${REMOTE_PORT}..."
: > "${LOG_FILE}"
date +%s > "${HEARTBEAT_FILE}"

(
  if [[ -f "${HF_TOKEN_PATH}" ]]; then
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

cleanup_secrets

nohup python3 "${WORK_DIR}/deadman.py" \
  --pid-file "${PID_FILE}" \
  --heartbeat-file "${HEARTBEAT_FILE}" \
  --max-duration "$(( MAX_DURATION_S + 300 ))" \
  --heartbeat-timeout "$(( IDLE_TIMEOUT_S + 600 ))" \
  >> "${WORK_DIR}/deadman.log" 2>&1 < /dev/null &
echo $! > "${DEADMAN_PID_FILE}"

log "llama-server launched (pid=$(cat "${PID_FILE}"))."
