"""Local per-session records: the supervisor lock, the state file, the ledger.

The lock is the authority on "is a supervisor running": the kernel releases it
when the process dies, so it cannot go stale the way a recorded PID can. The
state file only describes what that supervisor last reported.
"""

from __future__ import annotations

import fcntl
import json
import os
import sys
import tempfile
from collections.abc import Mapping
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, assert_never

from colab_llm.config import Engine, StopReason, state_dir, validate_session_name

STATE_VERSION = 1
LEDGER_VERSION = 1
LOCK_PID_BYTES_MAX = 32


class SessionBusyError(RuntimeError):
    """Another supervisor already holds this session."""


class StateError(ValueError):
    """A state file that does not describe a legal session state."""


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


def read_jsonl_tail(path: Path, limit: int = 10) -> list[dict[str, Any]]:
    """Return up to `limit` of the most recent records, oldest first.

    Lines that are not JSON objects are skipped, and counted on stderr so a
    damaged ledger is not mistaken for a short one.
    """
    assert limit >= 0, limit
    # `lines[-0:]` is the whole list, so a zero count needs its own branch.
    if limit == 0 or not path.is_file():
        return []
    lines = path.read_text(encoding="utf-8").splitlines()
    records: list[dict[str, Any]] = []
    skipped = 0
    for line in lines[-limit:]:
        line = line.strip()
        if not line:
            continue
        try:
            parsed = json.loads(line)
        except ValueError:
            skipped += 1
            continue
        if isinstance(parsed, dict):
            records.append(parsed)
        else:
            skipped += 1
    if skipped:
        print(
            f"[colab-llm] Warning: skipped {skipped} unreadable line(s) in {path}.",
            file=sys.stderr,
        )
    assert len(records) <= limit
    return records


def session_state_path(session: str) -> Path:
    validate_session_name(session)
    return state_dir() / f"{session}.json"


def session_lock_path(session: str) -> Path:
    validate_session_name(session)
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

    session: str
    fd: int

    def release(self) -> None:
        try:
            os.ftruncate(self.fd, 0)
        except OSError:
            pass
        os.close(self.fd)


def acquire_session_lock(session: str) -> SessionLock:
    """Become the supervisor of `session`, or fail if there already is one."""
    path = session_lock_path(session)
    # Python opens descriptors non-inheritable, so the ssh children never hold it.
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(fd)
        raise SessionBusyError(
            f"Session {session!r} already has a running supervisor. Stop it with "
            f"`colab-llm stop -s {session}` or choose a different --session."
        ) from None
    except BaseException:
        os.close(fd)
        raise
    try:
        # The PID is only for `stop` to signal. Truncate first so a reader never
        # pairs our lock with the PID of an earlier holder.
        os.ftruncate(fd, 0)
        os.pwrite(fd, f"{os.getpid()}\n".encode("ascii"), 0)
    except BaseException:
        os.close(fd)
        raise
    return SessionLock(session=session, fd=fd)


def live_supervisor_pid(session: str) -> int | None:
    """PID of the process supervising `session`, if one holds the lock."""
    try:
        fd = os.open(session_lock_path(session), os.O_RDWR)
    except FileNotFoundError:
        return None
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raw = os.pread(fd, LOCK_PID_BYTES_MAX, 0).decode("ascii", errors="ignore").strip()
            if raw.isdigit() and int(raw) > 1:
                return int(raw)
            # Held, but the holder has not written its PID yet.
            return None
        fcntl.flock(fd, fcntl.LOCK_UN)
        return None
    finally:
        os.close(fd)


# --------------------------------------------------------------------------- state


@dataclass(frozen=True)
class RunInfo:
    """What stays the same for the whole run of one supervisor."""

    session: str
    pid: int
    engine: Engine
    model: str
    gpu: str
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
        raise StateError(f"state field {key!r} is missing or is not a {kind.__name__}")
    return value


def _number(data: Mapping[str, Any], key: str) -> float:
    value = data.get(key)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise StateError(f"state field {key!r} is missing or is not a number")
    return float(value)


def _stop_reason(data: Mapping[str, Any]) -> StopReason:
    raw = _field(data, "stop_reason", str)
    try:
        return StopReason(raw)
    except ValueError:
        raise StateError(f"unknown stop_reason {raw!r}") from None


def _traffic(data: Mapping[str, Any]) -> Traffic:
    return Traffic(
        tunnel_port=_field(data, "tunnel_port", int),
        idle_s=_number(data, "idle_s"),
        elapsed_s=_number(data, "elapsed_s"),
        active_connections=_field(data, "active_connections", int),
        total_connections=_field(data, "total_connections", int),
        bytes_in=_field(data, "bytes_in", int),
        bytes_out=_field(data, "bytes_out", int),
    )


def parse_state(data: object) -> SessionState:
    """Parse a state file into one legal state, or raise StateError. No defaults."""
    if not isinstance(data, dict):
        raise StateError("state file is not a JSON object")
    version = data.get("version")
    if version != STATE_VERSION:
        raise StateError(f"unsupported state file version {version!r}")
    engine_raw = _field(data, "engine", str)
    try:
        engine = Engine(engine_raw)
    except ValueError:
        raise StateError(f"unknown engine {engine_raw!r}") from None
    run = RunInfo(
        session=_field(data, "session", str),
        pid=_field(data, "pid", int),
        engine=engine,
        model=_field(data, "model", str),
        gpu=_field(data, "gpu", str),
        bind_host=_field(data, "bind_host", str),
        local_port=_field(data, "local_port", int),
        remote_port=_field(data, "remote_port", int),
        idle_timeout_s=_field(data, "idle_timeout_s", int),
        max_duration_s=_field(data, "max_duration_s", int),
    )
    name = _field(data, "phase", str)
    phase: Phase
    match name:
        case "allocating":
            phase = Allocating()
        case "bootstrapping":
            phase = Bootstrapping()
        case "waiting_health":
            phase = WaitingHealth(traffic=_traffic(data))
        case "ready":
            phase = Ready(traffic=_traffic(data), health=_field(data, "health", dict))
        case "stopping":
            phase = Stopping(stop_reason=_stop_reason(data))
        case "stopped":
            phase = Stopped(
                stop_reason=_stop_reason(data),
                runtime_stopped=_field(data, "runtime_stopped", bool),
            )
        case _:
            raise StateError(f"unknown phase {name!r}")
    return SessionState(run=run, phase=phase, updated_at=_field(data, "updated_at", str))


def read_session_state(session: str) -> SessionState | None:
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
