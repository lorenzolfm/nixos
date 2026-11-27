"""Remote deadman watchdog running inside the Colab VM.

Secondary safety net in case the user's workstation loses network or power while
a billable GPU runtime is active. Stops the inference server and attempts
`google.colab.runtime.unassign()` if either:
  1. Hard wall-clock cap (max_duration + grace) is exceeded, or
  2. The heartbeat file (refreshed by the local supervisor over SSH) goes stale
     for longer than idle_timeout + grace.
"""

from __future__ import annotations

import argparse
import os
import signal
import sys
import time
from pathlib import Path


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
    if pid <= 1:
        return
    try:
        os.kill(pid, signal.SIGTERM)
        time.sleep(5)
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except OSError:
        pass


def _try_unassign_colab_runtime() -> None:
    try:
        from google.colab import runtime  # type: ignore[import-not-found]

        _log("Calling google.colab.runtime.unassign()...")
        runtime.unassign()
    except Exception as exc:  # noqa: BLE001
        _log(f"google.colab.runtime.unassign() unavailable or failed: {exc}")


def should_trigger(
    *,
    now: float,
    started_at: float,
    last_heartbeat: float,
    max_duration_s: int,
    heartbeat_timeout_s: int,
) -> str | None:
    if now - started_at >= max_duration_s:
        return "hard_cap"
    if now - last_heartbeat >= heartbeat_timeout_s:
        return "heartbeat_stale"
    return None


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Remote Colab deadman watchdog")
    ap.add_argument("--pid-file", required=True, type=Path)
    ap.add_argument("--heartbeat-file", required=True, type=Path)
    ap.add_argument("--max-duration", required=True, type=int)
    ap.add_argument("--heartbeat-timeout", required=True, type=int)
    ap.add_argument("--poll-interval", type=int, default=30)
    args = ap.parse_args(argv)

    started_at = time.time()
    args.heartbeat_file.parent.mkdir(parents=True, exist_ok=True)
    args.heartbeat_file.write_text(f"{int(started_at)}\n", encoding="utf-8")

    _log(
        f"Started remote deadman: max_duration={args.max_duration}s, "
        f"heartbeat_timeout={args.heartbeat_timeout}s"
    )
    while True:
        time.sleep(max(1, args.poll_interval))
        now = time.time()
        try:
            last_hb = args.heartbeat_file.stat().st_mtime
        except OSError:
            last_hb = started_at

        reason = should_trigger(
            now=now,
            started_at=started_at,
            last_heartbeat=last_hb,
            max_duration_s=args.max_duration,
            heartbeat_timeout_s=args.heartbeat_timeout,
        )
        if reason is not None:
            _log(f"Triggered ({reason}): terminating inference server and unassigning runtime.")
            _terminate_pid(args.pid_file)
            _try_unassign_colab_runtime()
            return 0


if __name__ == "__main__":
    raise SystemExit(main())
