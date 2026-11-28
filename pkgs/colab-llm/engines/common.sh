#!/usr/bin/env bash
# shellcheck disable=SC2034  # Variables here are read by the engine script that sources this file.
#
# Shared preamble for every colab-llm engine bootstrap. Sourced, never executed.
# Never mounts Google Drive (preserves #455 backup isolation).
#
# Order matters for billing safety: `bootstrap_begin` starts the deadman watchdog
# before any download or build, so a workstation that disappears mid-bootstrap
# still gets its runtime stopped.

# Non-login SSH sessions on Colab omit /usr/local/cuda/bin from PATH.
export PATH="/usr/local/cuda/bin:/usr/local/bin:${PATH}"

WORK_DIR="/content/colab-llm"
LOG_FILE="${WORK_DIR}/server.log"
PID_FILE="${WORK_DIR}/server.pid"
BOOTSTRAP_PID_FILE="${WORK_DIR}/bootstrap.pid"
DEADMAN_PID_FILE="${WORK_DIR}/deadman.pid"
HEARTBEAT_FILE="${WORK_DIR}/heartbeat"
HF_TOKEN_PATH="${WORK_DIR}/.hf_token"
# Identifies the launch values the running server was started from.
FINGERPRINT_FILE="${WORK_DIR}/server.fingerprint"

# Every value is set by the launcher (colab_llm/engines.py). There are no
# defaults here on purpose: a fallback would only ever hide a launcher bug by
# running with a stale copy of the real value.
ENGINE_NAME="${COLAB_LLM_ENGINE:?}"
REMOTE_PORT="${COLAB_LLM_REMOTE_PORT:?}"
CONTEXT_LENGTH="${COLAB_LLM_CONTEXT_LENGTH:?}"
CONFIG_FINGERPRINT="${COLAB_LLM_CONFIG_FINGERPRINT:?}"
BOOTSTRAP_TIMEOUT_S="${COLAB_LLM_BOOTSTRAP_TIMEOUT_S:?}"
BOOTSTRAP_HEARTBEAT_INTERVAL_S="${COLAB_LLM_BOOTSTRAP_HEARTBEAT_INTERVAL_S:?}"
DEADMAN_HEARTBEAT_TIMEOUT_S="${COLAB_LLM_DEADMAN_HEARTBEAT_TIMEOUT_S:?}"
DEADMAN_DURATION_S_MAX="${COLAB_LLM_DEADMAN_DURATION_S_MAX:?}"
FAST_HF="${COLAB_LLM_FAST_HF:?}"
RECONFIGURE="${COLAB_LLM_RECONFIGURE:?}"

STOP_WAIT_S_MAX=10
BOOTSTRAP_HEARTBEAT_PID=""

log() {
  printf '[%s-bootstrap %s] %s\n' "${ENGINE_NAME}" "$(date -u +%H:%M:%S)" "$*"
}

die() {
  echo "ERROR: $*" >&2
  exit 1
}

cleanup_secrets() {
  rm -f "${HF_TOKEN_PATH}"
}

bootstrap_exit() {
  cleanup_secrets
  if [[ -n "${BOOTSTRAP_HEARTBEAT_PID}" ]]; then
    kill "${BOOTSTRAP_HEARTBEAT_PID}" 2>/dev/null || true
  fi
  rm -f "${BOOTSTRAP_PID_FILE}"
}
trap bootstrap_exit EXIT

if (( DEADMAN_HEARTBEAT_TIMEOUT_S <= BOOTSTRAP_HEARTBEAT_INTERVAL_S )); then
  die "deadman heartbeat timeout (${DEADMAN_HEARTBEAT_TIMEOUT_S}s) must exceed the heartbeat interval (${BOOTSTRAP_HEARTBEAT_INTERVAL_S}s)"
fi
if (( DEADMAN_DURATION_S_MAX <= BOOTSTRAP_TIMEOUT_S )); then
  die "deadman hard cap (${DEADMAN_DURATION_S_MAX}s) must exceed the bootstrap timeout (${BOOTSTRAP_TIMEOUT_S}s)"
fi

# Stop the process named in a pid file and remove the file.
# Bounded: waits at most STOP_WAIT_S_MAX seconds before SIGKILL.
stop_pid_file() {
  local pid_file="$1"
  local old_pid waited_s
  if [[ ! -f "${pid_file}" ]]; then
    return 0
  fi
  old_pid="$(cat "${pid_file}")"
  if [[ "${old_pid}" =~ ^[0-9]+$ ]] && kill -0 "${old_pid}" 2>/dev/null; then
    kill "${old_pid}" 2>/dev/null || true
    for (( waited_s = 0; waited_s < STOP_WAIT_S_MAX; waited_s++ )); do
      kill -0 "${old_pid}" 2>/dev/null || break
      sleep 1
    done
    kill -9 "${old_pid}" 2>/dev/null || true
  fi
  rm -f "${pid_file}"
}

server_is_healthy() {
  local server_pid
  [[ -f "${PID_FILE}" ]] || return 1
  server_pid="$(cat "${PID_FILE}")"
  [[ "${server_pid}" =~ ^[0-9]+$ ]] || return 1
  kill -0 "${server_pid}" 2>/dev/null || return 1
  curl -fsS --max-time 2 "http://127.0.0.1:${REMOTE_PORT}/health" >/dev/null 2>&1
}

# A healthy server is only the requested one if it was started from the same
# engine, model and options. Without this, a different `start` would reuse it
# and report a model that is not the one being served.
server_matches_config() {
  [[ -f "${FINGERPRINT_FILE}" ]] || return 1
  [[ "$(cat "${FINGERPRINT_FILE}")" == "${CONFIG_FINGERPRINT}" ]]
}

# Start the remote deadman and refuse to continue without it: a bootstrap with
# no watchdog is a runtime that nothing will stop if the workstation vanishes.
start_deadman() {
  local deadman_pid
  date +%s > "${HEARTBEAT_FILE}"
  nohup python3 "${WORK_DIR}/deadman.py" \
    --pid-file "${PID_FILE}" \
    --pid-file "${BOOTSTRAP_PID_FILE}" \
    --heartbeat-file "${HEARTBEAT_FILE}" \
    --max-duration "${DEADMAN_DURATION_S_MAX}" \
    --heartbeat-timeout "${DEADMAN_HEARTBEAT_TIMEOUT_S}" \
    >> "${WORK_DIR}/deadman.log" 2>&1 < /dev/null &
  deadman_pid=$!
  echo "${deadman_pid}" > "${DEADMAN_PID_FILE}"
  sleep 1
  if ! kill -0 "${deadman_pid}" 2>/dev/null; then
    tail -n 20 "${WORK_DIR}/deadman.log" >&2 || true
    die "deadman watchdog failed to start"
  fi
}

# Keep the deadman's heartbeat fresh while the bootstrap runs; the local
# supervisor cannot, because its tunnel only comes up afterwards.
# Bounded: ends when the bootstrap process exits, and after at most one
# bootstrap timeout's worth of beats.
bootstrap_heartbeat_loop() {
  local bootstrap_pid="$1"
  local beats_max=$(( BOOTSTRAP_TIMEOUT_S / BOOTSTRAP_HEARTBEAT_INTERVAL_S + 1 ))
  local beat
  for (( beat = 0; beat < beats_max; beat++ )); do
    kill -0 "${bootstrap_pid}" 2>/dev/null || return 0
    date +%s > "${HEARTBEAT_FILE}"
    sleep "${BOOTSTRAP_HEARTBEAT_INTERVAL_S}"
  done
}

# Reuse a healthy server if allowed; otherwise clear prior processes and put the
# watchdog in place. Call before doing any work.
bootstrap_begin() {
  mkdir -p "${WORK_DIR}"

  if [[ "${RECONFIGURE}" != "1" ]] && server_is_healthy; then
    if server_matches_config; then
      log "Server already healthy on 127.0.0.1:${REMOTE_PORT}; reusing it under a fresh deadman."
      stop_pid_file "${DEADMAN_PID_FILE}"
      start_deadman
      exit 0
    fi
    log "A healthy server is running from a different configuration; replacing it."
  fi

  stop_pid_file "${PID_FILE}"
  rm -f "${FINGERPRINT_FILE}"
  stop_pid_file "${DEADMAN_PID_FILE}"

  echo "$$" > "${BOOTSTRAP_PID_FILE}"
  start_deadman
  # Detached from the SSH session's stdio so it can never hold the session open.
  bootstrap_heartbeat_loop "$$" > /dev/null 2>&1 < /dev/null &
  BOOTSTRAP_HEARTBEAT_PID=$!
}

# Whether an install finished and is the pinned one. The marker is written only
# after the install succeeds, so a venv left behind by a failed or interrupted
# install is rebuilt instead of reused.
install_marker_matches() {
  local marker="$1"
  local wanted="$2"
  [[ -f "${marker}" ]] && [[ "$(cat "${marker}")" == "${wanted}" ]]
}

# Check out exactly the pinned commit, or fail.
checkout_pinned_commit() {
  local repo_dir="$1"
  local repo_url="$2"
  local pinned_tag="$3"
  local pinned_commit="$4"
  local actual_commit
  if [[ ! -d "${repo_dir}/.git" ]]; then
    log "Cloning ${repo_url} (${pinned_tag} @ ${pinned_commit})..."
    git clone --depth 1 --branch "${pinned_tag}" "${repo_url}" "${repo_dir}"
  fi
  actual_commit="$(git -C "${repo_dir}" rev-parse HEAD)"
  if [[ "${actual_commit}" != "${pinned_commit}" ]]; then
    log "Fetching pinned commit ${pinned_commit}..."
    git -C "${repo_dir}" fetch --depth 1 origin "${pinned_commit}"
    git -C "${repo_dir}" checkout --detach "${pinned_commit}"
    actual_commit="$(git -C "${repo_dir}" rev-parse HEAD)"
  fi
  if [[ "${actual_commit}" != "${pinned_commit}" ]]; then
    die "commit mismatch in ${repo_dir}: got ${actual_commit}, expected ${pinned_commit}"
  fi
  log "Verified ${repo_dir} HEAD=${actual_commit}"
}

# Call after launching the server in the background and writing PID_FILE.
bootstrap_finish() {
  local server_pid
  cleanup_secrets
  sleep 2
  server_pid="$(cat "${PID_FILE}")"
  if ! kill -0 "${server_pid}" 2>/dev/null; then
    echo "ERROR: ${ENGINE_NAME} exited immediately on startup. Last 40 lines of ${LOG_FILE}:" >&2
    tail -n 40 "${LOG_FILE}" >&2 || true
    exit 1
  fi
  printf '%s\n' "${CONFIG_FINGERPRINT}" > "${FINGERPRINT_FILE}"
  log "${ENGINE_NAME} launched (pid=${server_pid}); local supervisor will poll /health over the SSH tunnel."
}
