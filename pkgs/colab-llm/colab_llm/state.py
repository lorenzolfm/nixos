"""Local per-session records: the supervisor lock, the state file, the ledger.

The lock is the authority on "is a supervisor running": the kernel releases it
when the process dies, so it cannot go stale the way a recorded PID can. The
state file only describes what that supervisor last reported.
"""

from __future__ import annotations

import fcntl
import json
import math
import os
import sys
import tempfile
import time
from collections.abc import Mapping
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, assert_never

from colab_llm.config import (
    Engine,
    Gpu,
    SessionName,
    StopReason,
    parse_gpu,
    state_dir,
    validate_loopback_host,
    validate_port,
    validate_session_name,
    validate_timeouts,
)

STATE_VERSION = 1
LEDGER_VERSION = 1
LOCK_PID_BYTES_MAX = 32
# `probe_supervisor` holds a shared lock for an instant. A `start` that meets it
# retries instead of mistaking the probe for a running supervisor.
LOCK_ACQUIRE_ATTEMPTS_MAX = 5
LOCK_ACQUIRE_RETRY_S = 0.02


class SessionBusyError(RuntimeError):
    """Another supervisor already holds this session."""


class StateError(ValueError):
    """A stored record (state file or ledger line) that is not a legal value."""


# --------------------------------------------------------------------------- files


def write_json_atomic(path: Path, payload: dict[str, Any]) -> None:
    """Atomically write JSON with owner-only (0600) permissions."""
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, tmp_name = tempfile.mkstemp(
        prefix=f".{path.name}.",
        suffix=".tmp",
        dir=str(path.parent),
    )
    tmp_path = Path(tmp_name)
    try:
        os.chmod(tmp_path, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, indent=2, sort_keys=True)
            fh.write("\n")
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp_path, path)
    except Exception:
        tmp_path.unlink(missing_ok=True)
        raise


def append_jsonl(path: Path, record: dict[str, Any]) -> None:
    """Append a single JSON record to an owner-only JSONL ledger."""
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    flags = os.O_WRONLY | os.O_CREAT | os.O_APPEND
    fd = os.open(path, flags, 0o600)
    try:
        os.chmod(path, 0o600)
    except OSError:
        pass
    with os.fdopen(fd, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(record, sort_keys=True) + "\n")
        fh.flush()


def session_state_path(session: SessionName) -> Path:
    return state_dir() / f"{session}.json"


def session_lock_path(session: SessionName) -> Path:
    return state_dir() / f"{session}.lock"


def ledger_path() -> Path:
    return state_dir() / "sessions.jsonl"


# --------------------------------------------------------------------------- lock


@dataclass(frozen=True)
class SessionLock:
    """Proof that this process is the only supervisor of `session`.

    Hold it for as long as the session is supervised. Closing the descriptor,
    or the process dying, releases it.
    """

    session: SessionName
    fd: int

    def release(self) -> None:
        try:
            os.ftruncate(self.fd, 0)
        except OSError:
            pass
        os.close(self.fd)


def _try_flock(fd: int, operation: int) -> bool:
    try:
        fcntl.flock(fd, operation | fcntl.LOCK_NB)
    except BlockingIOError:
        return False
    return True


def acquire_session_lock(session: SessionName) -> SessionLock:
    """Become the supervisor of `session`, or fail if there already is one."""
    path = session_lock_path(session)
    # Python opens descriptors non-inheritable, so the ssh children never hold it.
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        # Bounded: a real supervisor holds the lock for its whole run, so it
        # still holds it after the last attempt.
        for attempt in range(1, LOCK_ACQUIRE_ATTEMPTS_MAX + 1):
            if _try_flock(fd, fcntl.LOCK_EX):
                break
            if attempt == LOCK_ACQUIRE_ATTEMPTS_MAX:
                raise SessionBusyError(
                    f"Session {session!r} already has a running supervisor. Stop it with "
                    f"`colab-llm stop -s {session}` or choose a different --session."
                )
            time.sleep(LOCK_ACQUIRE_RETRY_S)
        # The PID is only for `stop` to signal. Truncate first so a reader never
        # pairs our lock with the PID of an earlier holder.
        os.ftruncate(fd, 0)
        os.pwrite(fd, f"{os.getpid()}\n".encode("ascii"), 0)
    except BaseException:
        os.close(fd)
        raise
    return SessionLock(session=session, fd=fd)


@dataclass(frozen=True)
class Unsupervised:
    """Nobody holds the session lock."""


@dataclass(frozen=True)
class Supervised:
    """A live process holds the session lock.

    `pid` is None for the instant between a supervisor taking the lock and
    writing its PID: it exists, but cannot be signalled yet.
    """

    pid: int | None


Supervision = Unsupervised | Supervised


def probe_supervisor(session: SessionName) -> Supervision:
    """Whether a process supervises `session` right now, as the kernel sees it."""
    try:
        fd = os.open(session_lock_path(session), os.O_RDWR)
    except FileNotFoundError:
        return Unsupervised()
    try:
        # Shared, so two probes never see each other as a supervisor. A PID is
        # read only while an exclusive holder exists, hence it is that holder's.
        if _try_flock(fd, fcntl.LOCK_SH):
            fcntl.flock(fd, fcntl.LOCK_UN)
            return Unsupervised()
        raw = os.pread(fd, LOCK_PID_BYTES_MAX, 0).decode("ascii", errors="ignore").strip()
        if raw.isdigit() and int(raw) > 1:
            return Supervised(pid=int(raw))
        return Supervised(pid=None)
    finally:
        os.close(fd)


# --------------------------------------------------------------------------- state


@dataclass(frozen=True)
class RunInfo:
    """What stays the same for the whole run of one supervisor."""

    session: SessionName
    pid: int
    engine: Engine
    model: str
    gpu: Gpu
    bind_host: str
    local_port: int
    remote_port: int
    idle_timeout_s: int
    max_duration_s: int


@dataclass(frozen=True)
class Traffic:
    """Proxy counters. They exist only once the tunnel and proxy are up."""

    tunnel_port: int
    idle_s: float
    elapsed_s: float
    active_connections: int
    total_connections: int
    bytes_in: int
    bytes_out: int


@dataclass(frozen=True)
class Allocating:
    pass


@dataclass(frozen=True)
class Bootstrapping:
    pass


@dataclass(frozen=True)
class WaitingHealth:
    traffic: Traffic


@dataclass(frozen=True)
class Ready:
    traffic: Traffic
    health: dict[str, Any]


@dataclass(frozen=True)
class Stopping:
    """The supervisor has ended; the runtime stop has not been confirmed yet."""

    stop_reason: StopReason


@dataclass(frozen=True)
class Stopped:
    """Teardown finished. `runtime_stopped` False means it may still be billing."""

    stop_reason: StopReason
    runtime_stopped: bool


Phase = Allocating | Bootstrapping | WaitingHealth | Ready | Stopping | Stopped


@dataclass(frozen=True)
class SessionState:
    run: RunInfo
    phase: Phase
    updated_at: str


def phase_name(phase: Phase) -> str:
    match phase:
        case Allocating():
            return "allocating"
        case Bootstrapping():
            return "bootstrapping"
        case WaitingHealth():
            return "waiting_health"
        case Ready():
            return "ready"
        case Stopping():
            return "stopping"
        case Stopped():
            return "stopped"
        case _:
            assert_never(phase)


def state_to_json(state: SessionState) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "version": STATE_VERSION,
        "updated_at": state.updated_at,
        "phase": phase_name(state.phase),
        **asdict(state.run),
    }
    payload["engine"] = state.run.engine.value
    payload["gpu"] = state.run.gpu.value
    phase = state.phase
    match phase:
        case Allocating() | Bootstrapping():
            pass
        case WaitingHealth(traffic=traffic):
            payload.update(asdict(traffic))
        case Ready(traffic=traffic, health=health):
            payload.update(asdict(traffic))
            payload["health"] = health
        case Stopping(stop_reason=stop_reason):
            payload["stop_reason"] = stop_reason.value
        case Stopped(stop_reason=stop_reason, runtime_stopped=runtime_stopped):
            payload["stop_reason"] = stop_reason.value
            payload["runtime_stopped"] = runtime_stopped
        case _:
            assert_never(phase)
    return payload


def _field[T](data: Mapping[str, Any], key: str, kind: type[T]) -> T:
    value = data.get(key)
    # bool is an int subclass; a flag where a number belongs is still wrong.
    if not isinstance(value, kind) or (isinstance(value, bool) and kind is not bool):
        raise StateError(f"field {key!r} is missing or is not a {kind.__name__}")
    return value


def _number(value: object, key: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise StateError(f"field {key!r} is missing or is not a number")
    return float(value)


def _duration(data: Mapping[str, Any], key: str) -> float:
    value = _number(data.get(key), key)
    if not (math.isfinite(value) and value >= 0):
        raise StateError(f"field {key!r} is not a duration in seconds: {value}")
    return value


def _count(data: Mapping[str, Any], key: str) -> int:
    value = _field(data, key, int)
    if value < 0:
        raise StateError(f"field {key!r} is negative: {value}")
    return value


def _timestamp(data: Mapping[str, Any], key: str) -> str:
    raw = _field(data, key, str)
    try:
        datetime.fromisoformat(raw)
    except ValueError:
        raise StateError(f"field {key!r} is not an ISO 8601 timestamp: {raw!r}") from None
    return raw


def _session(data: Mapping[str, Any]) -> SessionName:
    return validate_session_name(_field(data, "session", str))


def _engine(data: Mapping[str, Any]) -> Engine:
    raw = _field(data, "engine", str)
    try:
        return Engine(raw)
    except ValueError:
        raise StateError(f"unknown engine {raw!r}") from None


def _stop_reason(data: Mapping[str, Any]) -> StopReason:
    raw = _field(data, "stop_reason", str)
    try:
        return StopReason(raw)
    except ValueError:
        raise StateError(f"unknown stop_reason {raw!r}") from None


def _traffic(data: Mapping[str, Any]) -> Traffic:
    return Traffic(
        tunnel_port=validate_port(_field(data, "tunnel_port", int), "tunnel_port"),
        idle_s=_duration(data, "idle_s"),
        elapsed_s=_duration(data, "elapsed_s"),
        active_connections=_count(data, "active_connections"),
        total_connections=_count(data, "total_connections"),
        bytes_in=_count(data, "bytes_in"),
        bytes_out=_count(data, "bytes_out"),
    )


def _run_info(data: Mapping[str, Any]) -> RunInfo:
    pid = _field(data, "pid", int)
    if pid <= 1:
        raise StateError(f"field 'pid' is not a supervisor PID: {pid}")
    bind_host = _field(data, "bind_host", str)
    if validate_loopback_host(bind_host) != bind_host:
        raise StateError(f"field 'bind_host' is not in its recorded form: {bind_host!r}")
    idle_timeout_s, max_duration_s = validate_timeouts(
        _field(data, "idle_timeout_s", int), _field(data, "max_duration_s", int)
    )
    return RunInfo(
        session=_session(data),
        pid=pid,
        engine=_engine(data),
        model=_field(data, "model", str),
        gpu=parse_gpu(_field(data, "gpu", str)),
        bind_host=bind_host,
        local_port=validate_port(_field(data, "local_port", int), "local_port"),
        remote_port=validate_port(_field(data, "remote_port", int), "remote_port"),
        idle_timeout_s=idle_timeout_s,
        max_duration_s=max_duration_s,
    )


def _phase(data: Mapping[str, Any]) -> Phase:
    name = _field(data, "phase", str)
    match name:
        case "allocating":
            return Allocating()
        case "bootstrapping":
            return Bootstrapping()
        case "waiting_health":
            return WaitingHealth(traffic=_traffic(data))
        case "ready":
            return Ready(traffic=_traffic(data), health=_field(data, "health", dict))
        case "stopping":
            return Stopping(stop_reason=_stop_reason(data))
        case "stopped":
            return Stopped(
                stop_reason=_stop_reason(data),
                runtime_stopped=_field(data, "runtime_stopped", bool),
            )
        case _:
            raise StateError(f"unknown phase {name!r}")


def parse_state(data: object) -> SessionState:
    """Parse a state file into one legal state, or raise StateError. No defaults.

    Every value goes through the parser that the launch configuration uses, so
    a state file cannot hold a session, GPU, port or host that `start` refuses.
    """
    if not isinstance(data, dict):
        raise StateError("state file is not a JSON object")
    version = data.get("version")
    if version != STATE_VERSION:
        raise StateError(f"unsupported state file version {version!r}")
    try:
        return SessionState(
            run=_run_info(data),
            phase=_phase(data),
            updated_at=_timestamp(data, "updated_at"),
        )
    except StateError:
        raise
    except ValueError as exc:
        # The shared validators speak ValueError; here it is a bad record.
        raise StateError(str(exc)) from exc


def read_session_state(session: SessionName) -> SessionState | None:
    """The last recorded state, None if there is none. Raises StateError if unreadable."""
    path = session_state_path(session)
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise StateError(f"cannot read {path}: {exc}") from exc
    try:
        data = json.loads(text)
    except ValueError as exc:
        raise StateError(f"{path} is not valid JSON: {exc}") from exc
    return parse_state(data)


class StateWriter:
    """The one writer of a session's state file, for one run."""

    def __init__(self, path: Path, run: RunInfo) -> None:
        self.path = path
        self.run = run

    def write(self, phase: Phase) -> None:
        state = SessionState(
            run=self.run, phase=phase, updated_at=datetime.now(UTC).isoformat()
        )
        write_json_atomic(self.path, state_to_json(state))


# --------------------------------------------------------------------------- ledger


@dataclass(frozen=True)
class LedgerRecord:
    """One finished session. A balance is None when `colab usage` could not be read."""

    session: SessionName
    gpu: Gpu
    engine: Engine
    model: str
    started_at: str
    ended_at: str
    duration_s: float
    connections: int
    stop_reason: StopReason
    # False means the runtime may have kept billing after this record was written.
    runtime_stopped: bool
    cu_before: float | None
    cu_after: float | None

    @property
    def cu_burned(self) -> float | None:
        """Derived, never stored: it is the difference of the two balances."""
        if self.cu_before is None or self.cu_after is None:
            return None
        return self.cu_before - self.cu_after


def ledger_to_json(record: LedgerRecord) -> dict[str, Any]:
    payload: dict[str, Any] = {"version": LEDGER_VERSION, **asdict(record)}
    payload["gpu"] = record.gpu.value
    payload["engine"] = record.engine.value
    payload["stop_reason"] = record.stop_reason.value
    return payload


def _balance(data: Mapping[str, Any], key: str) -> float | None:
    if key not in data:
        raise StateError(f"field {key!r} is missing")
    value = data[key]
    return None if value is None else _number(value, key)


def parse_ledger_record(data: object) -> LedgerRecord:
    """Parse one ledger line into a record, or raise StateError. No defaults."""
    if not isinstance(data, dict):
        raise StateError("ledger line is not a JSON object")
    version = data.get("version")
    if version != LEDGER_VERSION:
        raise StateError(f"unsupported ledger record version {version!r}")
    try:
        return LedgerRecord(
            session=_session(data),
            gpu=parse_gpu(_field(data, "gpu", str)),
            engine=_engine(data),
            model=_field(data, "model", str),
            started_at=_timestamp(data, "started_at"),
            ended_at=_timestamp(data, "ended_at"),
            duration_s=_duration(data, "duration_s"),
            connections=_count(data, "connections"),
            stop_reason=_stop_reason(data),
            runtime_stopped=_field(data, "runtime_stopped", bool),
            cu_before=_balance(data, "cu_before"),
            cu_after=_balance(data, "cu_after"),
        )
    except StateError:
        raise
    except ValueError as exc:
        raise StateError(str(exc)) from exc


def append_ledger(path: Path, record: LedgerRecord) -> None:
    append_jsonl(path, ledger_to_json(record))


def read_ledger_tail(path: Path, limit: int = 10) -> list[LedgerRecord]:
    """Return up to `limit` of the most recent records, oldest first.

    Lines that are not legal records are skipped, and counted on stderr so a
    damaged ledger is not mistaken for a short one.
    """
    assert limit >= 0, limit
    # `lines[-0:]` is the whole list, so a zero count needs its own branch.
    if limit == 0 or not path.is_file():
        return []
    lines = [line for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    records: list[LedgerRecord] = []
    skipped = 0
    for line in lines[-limit:]:
        try:
            records.append(parse_ledger_record(json.loads(line)))
        except ValueError:
            # Covers invalid JSON and StateError alike.
            skipped += 1
    if skipped:
        print(
            f"[colab-llm] Warning: skipped {skipped} unreadable line(s) in {path}.",
            file=sys.stderr,
        )
    assert len(records) <= limit
    return records
