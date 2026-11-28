"""Remote deadman watchdog running inside the Colab VM.

Secondary safety net in case the user's workstation loses network or power while
a billable GPU runtime is active. Started before the bootstrap does any work, so
the bootstrap itself is covered. Stops the watched processes and attempts
`google.colab.runtime.unassign()` if either:
  1. Hard wall-clock cap (--max-duration) is exceeded, or
  2. The heartbeat file (refreshed by the bootstrap, then by the local supervisor
     over SSH) goes stale for longer than --heartbeat-timeout.

Exits 0 only if the runtime was unassigned. A non-zero exit means the processes
were stopped but the VM may still be assigned and billing.
"""

from __future__ import annotations

import argparse
import math
import os
import signal
import sys
import time
from pathlib import Path

TERMINATE_GRACE_S = 5


def _log(msg: str) -> None:
    ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    sys.stderr.write(f"[deadman {ts}] {msg}\n")
    sys.stderr.flush()


def _terminate_pid(pid_file: Path) -> None:
    if not pid_file.is_file():
        return
    try:
        pid = int(pid_file.read_text(encoding="utf-8").strip())
    except (ValueError, OSError):
        return
    if pid <= 1 or pid == os.getpid():
        return
    try:
        os.kill(pid, signal.SIGTERM)
        time.sleep(TERMINATE_GRACE_S)
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        # Already gone, which is the goal.
        pass
    except OSError as exc:
        _log(f"Could not signal pid {pid} from {pid_file}: {exc}")


def _try_unassign_colab_runtime() -> bool:
    """Ask Colab to release this VM. Returns whether the request went through."""
    try:
        from google.colab import runtime  # type: ignore[import-not-found]

        _log("Calling google.colab.runtime.unassign()...")
        runtime.unassign()
    except Exception as exc:  # noqa: BLE001
        _log(f"google.colab.runtime.unassign() unavailable or failed: {exc!r}")
        return False
    return True


def should_trigger(
    *,
    now: float,
    started_at: float,
    last_heartbeat: float,
    max_duration_s: int,
    heartbeat_timeout_s: int,
) -> str | None:
    assert max_duration_s > 0
    assert heartbeat_timeout_s > 0
    if now - started_at >= max_duration_s:
        return "hard_cap"
    if now - last_heartbeat >= heartbeat_timeout_s:
        return "heartbeat_stale"
    return None


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Remote Colab deadman watchdog")
    parser.add_argument("--pid-file", required=True, type=Path, action="append")
    parser.add_argument("--heartbeat-file", required=True, type=Path)
    parser.add_argument("--max-duration", required=True, type=int)
    parser.add_argument("--heartbeat-timeout", required=True, type=int)
    parser.add_argument("--poll-interval", type=int, default=30)
    args = parser.parse_args(argv)
    if args.max_duration <= 0:
        parser.error("--max-duration must be positive")
    if args.heartbeat_timeout <= 0:
        parser.error("--heartbeat-timeout must be positive")
    if args.poll_interval <= 0:
        parser.error("--poll-interval must be positive")

    started_at = time.time()
    args.heartbeat_file.parent.mkdir(parents=True, exist_ok=True)
    args.heartbeat_file.write_text(f"{int(started_at)}\n", encoding="utf-8")

    _log(
        f"Started remote deadman: max_duration={args.max_duration}s, "
        f"heartbeat_timeout={args.heartbeat_timeout}s"
    )
    # Bounded: each poll sleeps at least poll_interval, so after polls_max polls
    # the hard cap has passed. If the wall clock stepped backwards and hid that,
    # running out of polls is itself treated as the hard cap.
    polls_max = math.ceil(args.max_duration / args.poll_interval) + 1
    reason: str | None = None
    for _ in range(polls_max):
        time.sleep(args.poll_interval)
        try:
            last_heartbeat = args.heartbeat_file.stat().st_mtime
        except OSError:
            # No heartbeat file means no heartbeat since we started.
            last_heartbeat = started_at
        reason = should_trigger(
            now=time.time(),
            started_at=started_at,
            last_heartbeat=last_heartbeat,
            max_duration_s=args.max_duration,
            heartbeat_timeout_s=args.heartbeat_timeout,
        )
        if reason is not None:
            break
    if reason is None:
        reason = "hard_cap"

    _log(f"Triggered ({reason}): terminating watched processes and unassigning runtime.")
    for pid_file in args.pid_file:
        _terminate_pid(pid_file)
    if _try_unassign_colab_runtime():
        return 0
    _log("ERROR: runtime was NOT unassigned; this VM may still be billing.")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
