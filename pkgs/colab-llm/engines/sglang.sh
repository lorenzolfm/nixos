#!/usr/bin/env bash
# Modular Colab bootstrap for SGLang OpenAI-compatible serving.
# Never mounts Google Drive (preserves #455 backup isolation).
set -euo pipefail

export PATH="/usr/local/cuda/bin:/usr/local/bin:${PATH}"

WORK_DIR="/content/colab-llm"
VENV_DIR="${WORK_DIR}/.sglang-venv"
LOG_FILE="${WORK_DIR}/server.log"
PID_FILE="${WORK_DIR}/server.pid"
DEADMAN_PID_FILE="${WORK_DIR}/deadman.pid"
HEARTBEAT_FILE="${WORK_DIR}/heartbeat"
HF_TOKEN_PATH="${WORK_DIR}/.hf_token"

SGLANG_MODEL="${SGLANG_MODEL:?SGLANG_MODEL must be set}"
SGLANG_REVISION="${SGLANG_REVISION:-}"
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
  printf '[sglang-bootstrap %s] %s\n' "$(date -u +%H:%M:%S)" "$*"
}

mkdir -p "${WORK_DIR}"

if [[ "${RECONFIGURE}" != "1" ]] && [[ -f "${PID_FILE}" ]]; then
  existing_pid="$(cat "${PID_FILE}")"
  if kill -0 "${existing_pid}" 2>/dev/null \
    && curl -fsS --max-time 2 "http://127.0.0.1:${REMOTE_PORT}/health" >/dev/null 2>&1; then
    log "SGLang already healthy on 127.0.0.1:${REMOTE_PORT} (pid ${existing_pid}); reusing."
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

if [[ ! -x "${VENV_DIR}/bin/python" ]]; then
  log "Installing SGLang in ${VENV_DIR}..."
  if command -v uv >/dev/null 2>&1; then
    uv venv -q "${VENV_DIR}"
    uv pip install -q --python "${VENV_DIR}/bin/python" "sglang[all]" "huggingface_hub[hf_xet]" hf_transfer
  else
    python3 -m venv "${VENV_DIR}"
    "${VENV_DIR}/bin/pip" install -q --no-input "sglang[all]" "huggingface_hub[hf_xet]" hf_transfer
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
date +%s > "${HEARTBEAT_FILE}"

(
  export HF_HUB_ENABLE_HF_TRANSFER=1
  if [[ -f "${HF_TOKEN_PATH}" ]]; then
    HF_TOKEN="$(cat "${HF_TOKEN_PATH}")"
    export HF_TOKEN
  fi
  nohup "${VENV_DIR}/bin/python" "${serve_args[@]}" > "${LOG_FILE}" 2>&1 < /dev/null &
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

log "SGLang launched (pid=$(cat "${PID_FILE}"))."
