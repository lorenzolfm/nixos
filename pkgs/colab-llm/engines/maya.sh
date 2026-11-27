#!/usr/bin/env bash
# Remote Colab bootstrap for Project Maya (mw00/project-maya).
# Runs inside the ephemeral Colab VM over official `colab ssh --proxy-mode`.
# Never mounts Google Drive (preserves #455 backup isolation).
set -euo pipefail

# Non-login SSH sessions on Colab omit /usr/local/cuda/bin from PATH.
export PATH="/usr/local/cuda/bin:/usr/local/bin:${PATH}"

WORK_DIR="/content/colab-llm"
MAYA_DIR="${WORK_DIR}/project-maya"
MODELS_DIR="${WORK_DIR}/models"
LOG_FILE="${WORK_DIR}/server.log"
PID_FILE="${WORK_DIR}/server.pid"
DEADMAN_PID_FILE="${WORK_DIR}/deadman.pid"
HEARTBEAT_FILE="${WORK_DIR}/heartbeat"
HF_TOKEN_PATH="${WORK_DIR}/.hf_token"
HF_VENV="${WORK_DIR}/.hf-venv"

MAYA_REPO_URL="${MAYA_REPO_URL:-https://github.com/mw00/project-maya.git}"
MAYA_PINNED_TAG="${MAYA_PINNED_TAG:-v1.1.82}"
MAYA_PINNED_COMMIT="${MAYA_PINNED_COMMIT:-f84ec2db12bf96e7f2d106a2ebd91550903b859d}"
MAYA_MODEL="${MAYA_MODEL:-EXL3-3.05bpw}"
MAYA_DRAFT="${MAYA_DRAFT:-dflash2}"
MAYA_PARALLEL="${MAYA_PARALLEL:-1}"
MAYA_VISION="${MAYA_VISION:-1}"
export MAYA_WEB_UI="${MAYA_WEB_UI:-0}"
export MAYA_UPDATE_CHECK=0
MAYA_HF_REPO="${MAYA_HF_REPO:-}"
MAYA_HF_REVISION="${MAYA_HF_REVISION:-}"
MAYA_CUSTOM_MODEL_DIR="${MAYA_CUSTOM_MODEL_DIR:-0}"

REMOTE_PORT="${COLAB_LLM_REMOTE_PORT:-18100}"
CONTEXT_LENGTH="${COLAB_LLM_CONTEXT_LENGTH:-131072}"
IDLE_TIMEOUT_S="${COLAB_LLM_IDLE_TIMEOUT_S:-1800}"
MAX_DURATION_S="${COLAB_LLM_MAX_DURATION_S:-14400}"
FAST_HF="${COLAB_LLM_FAST_HF:-1}"
RECONFIGURE="${COLAB_LLM_RECONFIGURE:-0}"

cleanup_secrets() {
  rm -f "${HF_TOKEN_PATH}"
}
trap cleanup_secrets EXIT

log() {
  printf '[maya-bootstrap %s] %s\n' "$(date -u +%H:%M:%S)" "$*"
}

mkdir -p "${WORK_DIR}" "${MODELS_DIR}"

log "Hardware summary:"
nvidia-smi --query-gpu=name,memory.total,driver_version --format=csv,noheader || true
printf '  CPUs: %s | RAM: %s | Disk /content: %s\n' \
  "$(nproc)" \
  "$(free -h | awk '/^Mem:/ {print $2 " total, " $7 " avail"}')" \
  "$(df -h /content | awk 'NR==2 {print $4 " free / " $2}')"
nvcc --version | tail -n 1 || true

# Fast idempotent path: if Maya is already running and healthy, reuse it.
if [[ "${RECONFIGURE}" != "1" ]] && [[ -f "${PID_FILE}" ]]; then
  existing_pid="$(cat "${PID_FILE}")"
  if kill -0 "${existing_pid}" 2>/dev/null; then
    if curl -fsS --max-time 2 "http://127.0.0.1:${REMOTE_PORT}/health" >/dev/null 2>&1; then
      log "Project Maya is already healthy on 127.0.0.1:${REMOTE_PORT} (pid ${existing_pid}); reusing."
      date +%s > "${HEARTBEAT_FILE}"
      exit 0
    fi
  fi
fi

# Stop any prior server/deadman before reconfiguring.
for pf in "${PID_FILE}" "${DEADMAN_PID_FILE}"; do
  if [[ -f "${pf}" ]]; then
    old_pid="$(cat "${pf}")"
    if [[ -n "${old_pid}" ]] && kill -0 "${old_pid}" 2>/dev/null; then
      kill "${old_pid}" 2>/dev/null || true
      sleep 2
      kill -9 "${old_pid}" 2>/dev/null || true
    fi
    rm -f "${pf}"
  fi
done

# Ensure minimal build toolchain exists.
missing_pkgs=()
command -v git >/dev/null 2>&1 || missing_pkgs+=(git)
command -v curl >/dev/null 2>&1 || missing_pkgs+=(curl)
command -v cmake >/dev/null 2>&1 || missing_pkgs+=(cmake)
command -v ninja >/dev/null 2>&1 || missing_pkgs+=(ninja-build)
command -v g++ >/dev/null 2>&1 || missing_pkgs+=(g++)
if ! command -v uv >/dev/null 2>&1 \
  && ! python3 -c 'import sys, venv, ensurepip; sys.exit(0 if sys.version_info >= (3, 10) else 1)' 2>/dev/null; then
  missing_pkgs+=(python3-venv python3-pip)
fi
if (( ${#missing_pkgs[@]} > 0 )); then
  log "Installing missing build tools: ${missing_pkgs[*]}"
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends "${missing_pkgs[@]}"
fi

# Checkout pinned Project Maya commit (audited supply chain).
if [[ ! -d "${MAYA_DIR}/.git" ]]; then
  log "Cloning Project Maya (${MAYA_PINNED_TAG} @ ${MAYA_PINNED_COMMIT})..."
  git clone --depth 1 --branch "${MAYA_PINNED_TAG}" "${MAYA_REPO_URL}" "${MAYA_DIR}"
fi
actual_commit="$(git -C "${MAYA_DIR}" rev-parse HEAD)"
if [[ "${actual_commit}" != "${MAYA_PINNED_COMMIT}" ]]; then
  log "Fetching pinned commit ${MAYA_PINNED_COMMIT}..."
  git -C "${MAYA_DIR}" fetch --depth 1 origin "${MAYA_PINNED_COMMIT}"
  git -C "${MAYA_DIR}" checkout --detach "${MAYA_PINNED_COMMIT}"
  actual_commit="$(git -C "${MAYA_DIR}" rev-parse HEAD)"
fi
if [[ "${actual_commit}" != "${MAYA_PINNED_COMMIT}" ]]; then
  echo "ERROR: Project Maya commit mismatch: got ${actual_commit}, expected ${MAYA_PINNED_COMMIT}" >&2
  exit 1
fi
log "Verified Project Maya HEAD=${actual_commit}"

# Check for split CUDA 13 headers (cuda_profiler_api.h / nvtx3/nvToolsExt.h)
# per Project Maya docs/SETUP.md.
CUDA_CPATH=""
if ! find /usr/local/cuda* /usr/include -maxdepth 4 -name "cuda_profiler_api.h" 2>/dev/null | grep -q .; then
  cuda_major="$(nvcc --version 2>/dev/null | sed -n 's/.*release \([0-9]*\)\..*/\1/p' | head -n 1 || true)"
  if [[ "${cuda_major}" == "13" ]]; then
    log "Split CUDA 13 headers missing; installing nvidia-cuda-profiler-api + nvidia-nvtx locally..."
    python3 -m pip install -q --target "${MAYA_DIR}/.cuda-headers" \
      nvidia-cuda-profiler-api nvidia-nvtx
    if [[ -d "${MAYA_DIR}/.cuda-headers/nvidia/cu13/include" ]]; then
      CUDA_CPATH="${MAYA_DIR}/.cuda-headers/nvidia/cu13/include"
    fi
  fi
fi

# Fast Hugging Face Xet / hf_transfer pre-download into Maya's expected directory
# so Maya skips slow single-stream curl while still running full SHA256 checks.
CUSTOM_MODEL_PATH=""
if [[ -n "${MAYA_HF_REPO}" ]] && [[ "${FAST_HF}" == "1" || "${MAYA_CUSTOM_MODEL_DIR}" == "1" ]]; then
  if [[ "${MAYA_CUSTOM_MODEL_DIR}" == "1" ]]; then
    slug="$(printf '%s' "${MAYA_HF_REPO}${MAYA_HF_REVISION:+--${MAYA_HF_REVISION}}" | tr '/:@' '---')"
    target_dir="${MODELS_DIR}/custom/${slug}"
    CUSTOM_MODEL_PATH="${target_dir}"
  else
    target_dir="${MODELS_DIR}/${MAYA_MODEL}"
  fi
  mkdir -p "${target_dir}"

  if [[ ! -x "${HF_VENV}/bin/python" ]]; then
    log "Preparing fast Hugging Face downloader venv..."
    if command -v uv >/dev/null 2>&1; then
      uv venv -q "${HF_VENV}"
      uv pip install -q --python "${HF_VENV}/bin/python" "huggingface_hub[hf_xet]" hf_transfer
    else
      python3 -m venv "${HF_VENV}"
      "${HF_VENV}/bin/pip" install -q --no-input "huggingface_hub[hf_xet]" hf_transfer
    fi
  fi

  log "Prefetching ${MAYA_HF_REPO}${MAYA_HF_REVISION:+ @ ${MAYA_HF_REVISION}} into ${target_dir} via HF Xet..."
  HF_HUB_ENABLE_HF_TRANSFER=1 \
  HF_REPO="${MAYA_HF_REPO}" \
  HF_REVISION="${MAYA_HF_REVISION}" \
  HF_LOCAL_DIR="${target_dir}" \
  HF_TOKEN_FILE="${HF_TOKEN_PATH}" \
  "${HF_VENV}/bin/python" - <<'PY'
import os
from pathlib import Path
from huggingface_hub import snapshot_download

token = None
tf = Path(os.environ["HF_TOKEN_FILE"])
if tf.is_file():
    token = tf.read_text(encoding="utf-8").strip() or None

rev = os.environ["HF_REVISION"].strip() or None
snapshot_download(
    repo_id=os.environ["HF_REPO"],
    revision=rev,
    local_dir=os.environ["HF_LOCAL_DIR"],
    token=token,
)
PY
fi

# Delete HF token immediately after download phase.
cleanup_secrets

# Run non-interactive Project Maya setup (compiles engine, verifies SHA256s,
# builds pack, writes maya-<model>.json, does not start server yet).
setup_args=(
  --setup
  --yes
  --plain
  --no-start
  --models-dir "${MODELS_DIR}"
  --context "${CONTEXT_LENGTH}"
  --host 127.0.0.1
  --port "${REMOTE_PORT}"
)
if (( MAYA_PARALLEL > 1 )); then
  setup_args+=(--parallel "${MAYA_PARALLEL}")
fi

if [[ -n "${CUSTOM_MODEL_PATH}" ]]; then
  setup_args+=(--model-dir "${CUSTOM_MODEL_PATH}")
else
  setup_args+=(--download-model --model "${MAYA_MODEL}")
fi

case "${MAYA_DRAFT}" in
  dflash2)
    setup_args+=(--draft dflash2 --drafting on)
    ;;
  mtp)
    setup_args+=(--draft mtp --drafting on)
    ;;
  off)
    setup_args+=(--drafting off)
    ;;
  *)
    echo "ERROR: unsupported MAYA_DRAFT=${MAYA_DRAFT}" >&2
    exit 1
    ;;
esac

if [[ "${MAYA_VISION}" != "1" ]]; then
  setup_args+=(--no-vision)
fi

# Pre-seed Project Maya's .venv with pip via uv when available (Colab ships
# Python 3.13 without Debian's ensurepip wheel, which maya.sh's own check needs).
if [[ ! -x "${MAYA_DIR}/.venv/bin/python" ]] \
  || ! "${MAYA_DIR}/.venv/bin/python" -m pip --version >/dev/null 2>&1; then
  if command -v uv >/dev/null 2>&1; then
    log "Seeding Project Maya .venv with pip via uv..."
    rm -rf "${MAYA_DIR}/.venv"
    uv venv -q --seed "${MAYA_DIR}/.venv"
  fi
fi

log "Running Project Maya setup: ./maya.sh ${setup_args[*]}"
(
  cd "${MAYA_DIR}"
  if [[ -n "${CUDA_CPATH}" ]]; then
    export CPATH="${CUDA_CPATH}${CPATH:+:${CPATH}}"
  fi
  ./maya.sh "${setup_args[@]}"
)

# Resolve the generated Maya config matching REMOTE_PORT (newest mtime wins).
CFG_PATH="$(
  MAYA_DIR="${MAYA_DIR}" REMOTE_PORT="${REMOTE_PORT}" python3 - <<'PY'
import json
import os
from pathlib import Path

maya_dir = Path(os.environ["MAYA_DIR"])
want_port = int(os.environ["REMOTE_PORT"])
candidates = sorted(
    maya_dir.glob("maya-*.json"),
    key=lambda p: p.stat().st_mtime,
    reverse=True,
)
for path in candidates:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        continue
    if int(data.get("port", 0)) == want_port:
        print(path)
        raise SystemExit(0)
if candidates:
    print(candidates[0])
    raise SystemExit(0)
raise SystemExit("No maya-*.json config generated by setup")
PY
)"

log "Starting Project Maya server in background (config: ${CFG_PATH}, log: ${LOG_FILE})..."
: > "${LOG_FILE}"
date +%s > "${HEARTBEAT_FILE}"

(
  cd "${MAYA_DIR}"
  nohup ./maya.sh --plain --config "${CFG_PATH}" > "${LOG_FILE}" 2>&1 < /dev/null &
  echo $! > "${PID_FILE}"
)

# Start secondary remote deadman watchdog (guards against workstation power loss).
deadman_hb_timeout=$(( IDLE_TIMEOUT_S + 600 ))
deadman_max_dur=$(( MAX_DURATION_S + 300 ))
nohup python3 "${WORK_DIR}/deadman.py" \
  --pid-file "${PID_FILE}" \
  --heartbeat-file "${HEARTBEAT_FILE}" \
  --max-duration "${deadman_max_dur}" \
  --heartbeat-timeout "${deadman_hb_timeout}" \
  >> "${WORK_DIR}/deadman.log" 2>&1 < /dev/null &
echo $! > "${DEADMAN_PID_FILE}"

sleep 2
server_pid="$(cat "${PID_FILE}")"
if ! kill -0 "${server_pid}" 2>/dev/null; then
  echo "ERROR: Project Maya exited immediately on startup. Last 40 lines of ${LOG_FILE}:" >&2
  tail -n 40 "${LOG_FILE}" >&2 || true
  exit 1
fi

log "Project Maya launched (pid=${server_pid}); local supervisor will poll /health over SSH tunnel."
