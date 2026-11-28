#!/usr/bin/env python3
"""Threshold-triggered cleanup for safe Nix and Rust build artifacts.

When disk usage reaches the configured threshold (default 85%), this script
reclaims space across four safe targets:
  1. Stale Nix `result` / `result-*` symlinks under ~/Projects (unpins GC roots)
  2. Old Rust `target/` build directories under ~/Projects (oldest first,
     skipping locked or recently active builds)
  3. Stale `/tmp/nix-*` shell/build directories older than 24h (Linux only)
  4. Nix profile generation pruning + `nix-collect-garbage`

When invoked as root (via systemd or launchd), all operations inside the
target user's home directory and desktop notifications run dropped to that
user's UID/GID.
"""

from __future__ import annotations

import argparse
import dataclasses
import errno
import fcntl
import json
import math
import os
import pwd
import re
import shutil
import stat
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path
from typing import Callable, Iterable

CACHEDIR_TAG_SIGNATURE = "Signature: 8a477f597d28d172789f06886806bc55"
SKIP_WALK_DIRS = frozenset({"node_modules", "__pycache__", "Pods", "DerivedData"})
TARGET_SKIP_MTIME_CHILDREN = frozenset({"deps", "examples"})
NIX_TMP_PREFIXES = (
    "nix-shell.",
    "nix-shell-",
    "nix-develop-",
    "nix-build-",
    "nixos-rebuild.",
)
NIX_DEVELOP_PID_RE = re.compile(r"^nix-develop-(\d+)-")
NIX_GC_FREED_RE = re.compile(
    r"(\d+)\s+store paths deleted,\s+([0-9.]+)\s+([KMGT]?i?B)\s+freed",
    re.IGNORECASE,
)
NOTIFY_MIN_FREED_BYTES = 1024 * 1024  # 1 MiB


@dataclasses.dataclass
class DiskUsage:
    path: str
    dev: int
    used_bytes: int
    avail_bytes: int
    percent: int


@dataclasses.dataclass
class TargetCandidate:
    path: Path
    latest_mtime: float
    size_bytes: int


@dataclasses.dataclass
class UserPhaseStats:
    result_links_removed: int = 0
    targets_removed: int = 0
    targets_bytes: int = 0
    targets_skipped_active: int = 0
    targets_skipped_recent: int = 0
    user_profiles_pruned: int = 0
    errors: list[str] = dataclasses.field(default_factory=list)


@dataclasses.dataclass
class NixGcStats:
    ran: bool = False
    paths_deleted: int = 0
    freed_bytes: int = 0
    dead_paths_dry_run: int = 0
    system_profiles_pruned: bool = False
    errors: list[str] = dataclasses.field(default_factory=list)


@dataclasses.dataclass
class TmpCleanupStats:
    dirs_removed: int = 0
    bytes_freed: int = 0
    skipped_active: int = 0
    errors: list[str] = dataclasses.field(default_factory=list)


def log(msg: str) -> None:
    """Emit a log line to stderr; add timestamps when not under journald."""
    if os.environ.get("JOURNAL_STREAM"):
        prefix = "disk-cleanup:"
    else:
        ts = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        prefix = f"{ts} disk-cleanup:"
    print(f"{prefix} {msg}", file=sys.stderr, flush=True)


def format_bytes(num_bytes: int) -> str:
    if num_bytes < 1024:
        return f"{num_bytes} B"
    units = ("KiB", "MiB", "GiB", "TiB")
    value = float(num_bytes)
    for unit in units:
        value /= 1024.0
        if value < 1024.0 or unit == units[-1]:
            return f"{value:.1f} {unit}"
    return f"{num_bytes} B"


def format_age(seconds: float) -> str:
    if seconds < 3600:
        return f"{int(seconds // 60)}m"
    if seconds < 86400:
        return f"{seconds / 3600:.1f}h"
    return f"{seconds / 86400:.1f}d"


def parse_size_to_bytes(value: float, unit: str) -> int:
    multipliers = {
        "B": 1,
        "KB": 1000,
        "KIB": 1024,
        "MB": 1000**2,
        "MIB": 1024**2,
        "GB": 1000**3,
        "GIB": 1024**3,
        "TB": 1000**4,
        "TIB": 1024**4,
    }
    return int(value * multipliers.get(unit.upper(), 1))


def stat_filesystem(path: Path) -> DiskUsage:
    """Compute filesystem usage matching df's non-root usable percentage.

    Using used / (used + avail) matches df on Linux ext4 (excluding the 5%
    root reserve) and measures true APFS container utilization on macOS
    (where f_bfree == f_bavail == container free blocks).
    """
    st_fs = os.statvfs(path)
    st_file = os.stat(path)
    frsize = st_fs.f_frsize
    used_blocks = max(0, st_fs.f_blocks - st_fs.f_bfree)
    avail_blocks = st_fs.f_bavail
    usable_total = used_blocks + avail_blocks
    if usable_total <= 0:
        percent = 100
    else:
        percent = min(100, math.ceil(used_blocks * 100 / usable_total))
    return DiskUsage(
        path=str(path),
        dev=st_file.st_dev,
        used_bytes=used_blocks * frsize,
        avail_bytes=avail_blocks * frsize,
        percent=percent,
    )


def worst_disk_usage(paths: Iterable[Path]) -> DiskUsage:
    """Return the highest-usage filesystem among existing candidate paths."""
    seen_devs: set[int] = set()
    usages: list[DiskUsage] = []
    for path in paths:
        if not path.exists():
            continue
        try:
            usage = stat_filesystem(path)
        except OSError:
            continue
        if usage.dev in seen_devs:
            continue
        seen_devs.add(usage.dev)
        usages.append(usage)
    if not usages:
        return stat_filesystem(Path("/"))
    return max(usages, key=lambda u: (u.percent, -u.avail_bytes))


def acquire_lock(lock_path: Path) -> int | None:
    """Acquire a non-blocking exclusive lockfile. Returns fd, or None if busy."""
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(fd)
        return None
    return fd


def default_lock_path(user_home: Path) -> Path:
    if os.geteuid() == 0:
        if sys.platform.startswith("linux"):
            run_dir = Path("/run/disk-cleanup")
            if run_dir.is_dir():
                return run_dir / "lock"
            return Path("/run/disk-cleanup.lock")
        return Path("/var/run/disk-cleanup.lock")
    xdg_runtime = os.environ.get("XDG_RUNTIME_DIR")
    if xdg_runtime:
        return Path(xdg_runtime) / "disk-cleanup.lock"
    return user_home / ".cache" / "disk-cleanup.lock"


def _rmtree_handle_readonly(func: Callable[..., object], path: str, exc: BaseException) -> None:
    """Retry unlink/rmdir inside a build dir if Nix/Cargo left read-only files."""
    if not isinstance(exc, PermissionError):
        raise exc
    try:
        st = os.lstat(path)
    except OSError:
        raise exc
    if stat.S_ISLNK(st.st_mode):
        raise exc
    parent = os.path.dirname(path)
    try:
        if parent:
            parent_st = os.lstat(parent)
            if not stat.S_ISLNK(parent_st.st_mode):
                os.chmod(parent, parent_st.st_mode | stat.S_IWUSR | stat.S_IXUSR)
        if stat.S_ISDIR(st.st_mode):
            os.chmod(path, st.st_mode | stat.S_IWUSR | stat.S_IXUSR)
        else:
            os.chmod(path, st.st_mode | stat.S_IWUSR)
    except OSError:
        raise exc
    func(path)


def safe_rmtree(path: Path) -> None:
    """Delete a real directory tree (never a symlink), fixing read-only perms."""
    st = os.lstat(path)
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
        raise ValueError(f"refusing to rmtree non-directory: {path}")
    shutil.rmtree(path, onexc=_rmtree_handle_readonly)


def dir_disk_usage(root: Path) -> int:
    """Measure allocated disk bytes of a directory tree without following links."""
    total = 0
    seen_inodes: set[tuple[int, int]] = set()
    stack = [root]
    while stack:
        current = stack.pop()
        try:
            with os.scandir(current) as it:
                for entry in it:
                    try:
                        st = entry.stat(follow_symlinks=False)
                    except OSError:
                        continue
                    key = (st.st_dev, st.st_ino)
                    if st.st_nlink > 1:
                        if key in seen_inodes:
                            continue
                        seen_inodes.add(key)
                    # st_blocks is always in 512-byte units on POSIX.
                    total += st.st_blocks * 512
                    if stat.S_ISDIR(st.st_mode):
                        stack.append(Path(entry.path))
        except OSError:
            continue
    return total


def is_cargo_target_dir(target_dir: Path) -> bool:
    """Verify `target_dir` is genuinely a Cargo build output directory."""
    try:
        st = os.lstat(target_dir)
    except OSError:
        return False
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
        return False

    cargo_toml = target_dir.parent / "Cargo.toml"
    try:
        cargo_st = os.lstat(cargo_toml)
    except OSError:
        return False
    if not stat.S_ISREG(cargo_st.st_mode):
        return False

    cachedir_tag = target_dir / "CACHEDIR.TAG"
    try:
        tag_st = os.lstat(cachedir_tag)
        if stat.S_ISREG(tag_st.st_mode):
            with open(cachedir_tag, "r", encoding="utf-8", errors="ignore") as f:
                if f.readline().startswith(CACHEDIR_TAG_SIGNATURE):
                    return True
    except OSError:
        pass

    for marker in (".cargo-lock", ".rustc_info.json"):
        try:
            marker_st = os.lstat(target_dir / marker)
            if stat.S_ISREG(marker_st.st_mode):
                return True
        except OSError:
            continue

    return False


def is_cargo_target_locked(target_dir: Path) -> bool:
    """Return True if Cargo currently holds an exclusive flock on .cargo-lock."""
    lock_file = target_dir / ".cargo-lock"
    try:
        fd = os.open(lock_file, os.O_RDONLY | os.O_NOFOLLOW)
    except FileNotFoundError:
        return False
    except OSError:
        # Symlink or unreadable lock file: err on the side of caution.
        return True
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        except OSError as exc:
            if exc.errno in (errno.EAGAIN, errno.EACCES, errno.EWOULDBLOCK):
                return True
            return False
        else:
            fcntl.flock(fd, fcntl.LOCK_UN)
            return False
    finally:
        os.close(fd)


def latest_target_mtime(target_dir: Path, max_depth: int = 3) -> float:
    """Find the most recent mtime inside a Cargo target/ directory.

    Skips recursing into `deps/` and `examples/` (which can hold thousands of
    object files) while still stat-ing those directories themselves, plus
    top-level lock/info files, `.fingerprint`, `incremental`, `build`, and
    target-triple subdirectories.
    """
    try:
        newest = os.lstat(target_dir).st_mtime
    except OSError:
        return time.time()

    stack: list[tuple[Path, int]] = [(target_dir, 0)]
    while stack:
        current, depth = stack.pop()
        if depth >= max_depth:
            continue
        try:
            with os.scandir(current) as it:
                for entry in it:
                    try:
                        st = entry.stat(follow_symlinks=False)
                    except OSError:
                        continue
                    if st.st_mtime > newest:
                        newest = st.st_mtime
                    if stat.S_ISDIR(st.st_mode) and entry.name not in TARGET_SKIP_MTIME_CHILDREN:
                        stack.append((Path(entry.path), depth + 1))
        except OSError:
            continue
    return newest


def recursive_max_mtime(root: Path) -> float:
    """Compute the newest lstat mtime across every entry under `root`."""
    newest = os.lstat(root).st_mtime
    stack = [root]
    while stack:
        current = stack.pop()
        try:
            with os.scandir(current) as it:
                for entry in it:
                    try:
                        st = entry.stat(follow_symlinks=False)
                    except OSError:
                        continue
                    if st.st_mtime > newest:
                        newest = st.st_mtime
                    if stat.S_ISDIR(st.st_mode):
                        stack.append(Path(entry.path))
        except OSError:
            continue
    return newest


def scan_projects(
    projects_dir: Path,
    now: float,
    result_age_days: float,
    target_age_hours: float,
    stats: UserPhaseStats,
) -> tuple[list[tuple[Path, float]], list[TargetCandidate]]:
    """Walk `projects_dir` once to discover stale Nix result* links and Rust target/ dirs."""
    stale_results: list[tuple[Path, float]] = []
    target_candidates: list[TargetCandidate] = []

    if not projects_dir.is_dir():
        return stale_results, target_candidates

    result_cutoff = now - result_age_days * 86400.0
    target_cutoff = now - target_age_hours * 3600.0

    stack = [projects_dir]
    while stack:
        current = stack.pop()
        try:
            with os.scandir(current) as it:
                entries = list(it)
        except PermissionError:
            # Unreadable subtrees (e.g. Docker bind-mounted data dirs) are not
            # cleanup errors; just skip them.
            continue
        except OSError as exc:
            stats.errors.append(f"scan {current}: {exc}")
            continue

        for entry in entries:
            name = entry.name
            entry_path = Path(entry.path)
            try:
                st = entry.stat(follow_symlinks=False)
            except OSError:
                continue

            if stat.S_ISLNK(st.st_mode):
                if name == "result" or name.startswith("result-"):
                    try:
                        link_target = os.readlink(entry_path)
                    except OSError:
                        continue
                    if link_target.startswith("/nix/store/") and st.st_mtime <= result_cutoff:
                        stale_results.append((entry_path, st.st_mtime))
                continue

            if not stat.S_ISDIR(st.st_mode):
                continue

            if name.startswith(".") or name in SKIP_WALK_DIRS:
                continue

            if name == "target":
                if is_cargo_target_dir(entry_path):
                    if is_cargo_target_locked(entry_path):
                        stats.targets_skipped_active += 1
                    else:
                        newest = latest_target_mtime(entry_path)
                        if newest > target_cutoff:
                            stats.targets_skipped_recent += 1
                        else:
                            size = dir_disk_usage(entry_path)
                            target_candidates.append(
                                TargetCandidate(
                                    path=entry_path,
                                    latest_mtime=newest,
                                    size_bytes=size,
                                )
                            )
                # Never descend into any `target` directory.
                continue

            stack.append(entry_path)

    stale_results.sort(key=lambda item: item[1])
    target_candidates.sort(key=lambda c: c.latest_mtime)
    return stale_results, target_candidates


def user_nix_profiles(user_name: str, user_home: Path) -> list[Path]:
    """Return existing per-user Nix profile paths to prune generations on."""
    candidates = [
        user_home / ".local" / "state" / "nix" / "profiles" / "profile",
        Path("/nix/var/nix/profiles/per-user") / user_name / "profile",
    ]
    profiles: list[Path] = []
    seen: set[Path] = set()
    for path in candidates:
        if not path.exists() and not path.is_symlink():
            continue
        try:
            resolved = path.parent.resolve() / path.name
        except OSError:
            resolved = path
        if resolved in seen:
            continue
        seen.add(resolved)
        profiles.append(path)
    return profiles


def run_user_phase_in_process(
    user_name: str,
    user_home: Path,
    projects_dir: Path,
    result_age_days: float,
    target_age_hours: float,
    nix_gc_age_days: int,
    dry_run: bool,
    now: float | None = None,
) -> UserPhaseStats:
    """Clean stale Nix result* links, Rust target/ dirs, and user Nix generations."""
    stats = UserPhaseStats()
    now_ts = time.time() if now is None else now

    stale_results, target_candidates = scan_projects(
        projects_dir=projects_dir,
        now=now_ts,
        result_age_days=result_age_days,
        target_age_hours=target_age_hours,
        stats=stats,
    )

    for link_path, mtime in stale_results:
        age = format_age(max(0.0, now_ts - mtime))
        if dry_run:
            log(f"[dry-run] would remove stale Nix result symlink: {link_path} (age {age})")
            stats.result_links_removed += 1
            continue
        try:
            # Re-verify symlink before unlink.
            st = os.lstat(link_path)
            if not stat.S_ISLNK(st.st_mode):
                continue
            if not os.readlink(link_path).startswith("/nix/store/"):
                continue
            os.unlink(link_path)
            log(f"removed stale Nix result symlink: {link_path} (age {age})")
            stats.result_links_removed += 1
        except OSError as exc:
            stats.errors.append(f"unlink {link_path}: {exc}")

    for candidate in target_candidates:
        age = format_age(max(0.0, now_ts - candidate.latest_mtime))
        size_str = format_bytes(candidate.size_bytes)
        if dry_run:
            log(f"[dry-run] would remove Rust target/: {candidate.path} ({size_str}, idle {age})")
            stats.targets_removed += 1
            stats.targets_bytes += candidate.size_bytes
            continue
        # Re-check lock right before deletion to avoid racing with a just-started build.
        if is_cargo_target_locked(candidate.path):
            stats.targets_skipped_active += 1
            log(f"skipped Rust target/ (build started): {candidate.path}")
            continue
        try:
            safe_rmtree(candidate.path)
            log(f"removed Rust target/: {candidate.path} ({size_str}, idle {age})")
            stats.targets_removed += 1
            stats.targets_bytes += candidate.size_bytes
        except OSError as exc:
            stats.errors.append(f"rmtree {candidate.path}: {exc}")

    nix_env = shutil.which("nix-env")
    if nix_env:
        for profile in user_nix_profiles(user_name, user_home):
            if dry_run:
                log(
                    f"[dry-run] would prune Nix profile generations older than "
                    f"{nix_gc_age_days}d: {profile}"
                )
                stats.user_profiles_pruned += 1
                continue
            proc = subprocess.run(
                [nix_env, "--profile", str(profile), "--delete-generations", f"{nix_gc_age_days}d"],
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
            )
            if proc.returncode == 0:
                stats.user_profiles_pruned += 1
                if proc.stdout.strip():
                    for line in proc.stdout.strip().splitlines():
                        log(f"nix-env ({profile.name}): {line}")
            else:
                err = proc.stderr.strip() or f"exit {proc.returncode}"
                stats.errors.append(f"nix-env {profile}: {err}")

    return stats


def run_user_phase(
    pw: pwd.struct_passwd,
    projects_dir: Path,
    result_age_days: float,
    target_age_hours: float,
    nix_gc_age_days: int,
    dry_run: bool,
) -> UserPhaseStats:
    """Run Phase 1 directly or in a privilege-dropped subprocess when root."""
    if os.geteuid() != 0 or pw.pw_uid == 0:
        return run_user_phase_in_process(
            user_name=pw.pw_name,
            user_home=Path(pw.pw_dir),
            projects_dir=projects_dir,
            result_age_days=result_age_days,
            target_age_hours=target_age_hours,
            nix_gc_age_days=nix_gc_age_days,
            dry_run=dry_run,
        )

    cmd = [
        sys.executable,
        os.path.abspath(__file__),
        "--_user-phase",
        "--user",
        pw.pw_name,
        "--projects-dir",
        str(projects_dir),
        "--result-age-days",
        str(result_age_days),
        "--target-age-hours",
        str(target_age_hours),
        "--nix-gc-age-days",
        str(nix_gc_age_days),
    ]
    if dry_run:
        cmd.append("--dry-run")

    env = {
        "PATH": os.environ.get("PATH", ""),
        "HOME": pw.pw_dir,
        "USER": pw.pw_name,
        "LOGNAME": pw.pw_name,
    }
    for key in ("NIX_REMOTE", "NIX_CONF_DIR", "JOURNAL_STREAM", "TZ", "TZDIR", "LOCALE_ARCHIVE"):
        if key in os.environ:
            env[key] = os.environ[key]

    try:
        extra_groups = os.getgrouplist(pw.pw_name, pw.pw_gid)
    except OSError:
        extra_groups = [pw.pw_gid]

    proc = subprocess.run(
        cmd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        text=True,
        env=env,
        user=pw.pw_uid,
        group=pw.pw_gid,
        extra_groups=extra_groups,
    )
    if proc.returncode != 0:
        stats = UserPhaseStats()
        stats.errors.append(f"user phase exited with code {proc.returncode}")
        return stats
    try:
        payload = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        stats = UserPhaseStats()
        stats.errors.append(f"failed to parse user phase output: {exc}")
        return stats
    return UserPhaseStats(**payload)


def is_nix_tmp_dir_name(name: str) -> bool:
    return any(name.startswith(prefix) for prefix in NIX_TMP_PREFIXES)


def is_pid_alive(pid: int) -> bool:
    if pid <= 0:
        return False
    if Path("/proc").is_dir():
        return Path(f"/proc/{pid}").exists()
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def clean_nix_tmp_dirs(
    tmp_dir: Path,
    tmp_age_hours: float,
    dry_run: bool,
    now: float | None = None,
) -> TmpCleanupStats:
    """Remove stale `/tmp/nix-*` shell and build directories (Linux only)."""
    stats = TmpCleanupStats()
    if not tmp_dir.is_dir():
        return stats

    now_ts = time.time() if now is None else now
    cutoff = now_ts - tmp_age_hours * 3600.0
    euid = os.geteuid()

    try:
        with os.scandir(tmp_dir) as it:
            entries = list(it)
    except OSError as exc:
        stats.errors.append(f"scan {tmp_dir}: {exc}")
        return stats

    for entry in entries:
        if not is_nix_tmp_dir_name(entry.name):
            continue
        try:
            st = entry.stat(follow_symlinks=False)
        except OSError:
            continue
        if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
            continue
        if euid != 0 and st.st_uid != euid:
            continue

        pid_match = NIX_DEVELOP_PID_RE.match(entry.name)
        if pid_match and is_pid_alive(int(pid_match.group(1))):
            stats.skipped_active += 1
            continue

        entry_path = Path(entry.path)
        try:
            newest = recursive_max_mtime(entry_path)
        except OSError:
            continue
        if newest > cutoff:
            stats.skipped_active += 1
            continue

        size = dir_disk_usage(entry_path)
        if dry_run:
            stats.dirs_removed += 1
            stats.bytes_freed += size
            continue
        try:
            safe_rmtree(entry_path)
            stats.dirs_removed += 1
            stats.bytes_freed += size
        except OSError as exc:
            stats.errors.append(f"rmtree {entry_path}: {exc}")

    if stats.dirs_removed > 0:
        verb = "[dry-run] would remove" if dry_run else "removed"
        log(
            f"{verb} {stats.dirs_removed} stale {tmp_dir}/nix-* dir(s) "
            f"({format_bytes(stats.bytes_freed)})"
        )
    return stats


def run_nix_gc(nix_gc_age_days: int, dry_run: bool) -> NixGcStats:
    """Prune old Nix generations and collect unreferenced /nix/store paths."""
    stats = NixGcStats()
    if dry_run:
        nix_store = shutil.which("nix-store")
        if not nix_store:
            stats.errors.append("nix-store not found on PATH")
            return stats
        proc = subprocess.run(
            [nix_store, "--gc", "--print-dead"],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
        )
        if proc.returncode == 0:
            dead_count = sum(1 for line in proc.stdout.splitlines() if line.strip())
            stats.dead_paths_dry_run = dead_count
            log(
                f"[dry-run] would run nix-collect-garbage --delete-older-than "
                f"{nix_gc_age_days}d ({dead_count} currently dead store paths; "
                f"additional paths will become dead after pruning result* links and generations)"
            )
        else:
            err = proc.stderr.strip() or f"exit {proc.returncode}"
            stats.errors.append(f"nix-store --gc --print-dead: {err}")
        return stats

    nix_gc = shutil.which("nix-collect-garbage")
    if not nix_gc:
        stats.errors.append("nix-collect-garbage not found on PATH")
        return stats

    if os.geteuid() != 0:
        log("running non-root: system profile generations will not be pruned")
    else:
        stats.system_profiles_pruned = True

    proc = subprocess.run(
        [nix_gc, "--delete-older-than", f"{nix_gc_age_days}d"],
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
    )
    stats.ran = True
    combined_output = "\n".join(part for part in (proc.stdout, proc.stderr) if part)
    match = NIX_GC_FREED_RE.search(combined_output)
    if match:
        stats.paths_deleted = int(match.group(1))
        stats.freed_bytes = parse_size_to_bytes(float(match.group(2)), match.group(3))
        log(
            f"nix-collect-garbage: {stats.paths_deleted} store paths deleted, "
            f"{format_bytes(stats.freed_bytes)} freed"
        )
    elif proc.returncode == 0:
        log("nix-collect-garbage: 0 store paths deleted")

    if proc.returncode != 0:
        err = proc.stderr.strip() or proc.stdout.strip() or f"exit {proc.returncode}"
        stats.errors.append(f"nix-collect-garbage: {err}")
    return stats


def send_notification(pw: pwd.struct_passwd, title: str, body: str) -> None:
    """Best-effort desktop notification on NixOS (libnotify) and macOS."""
    try:
        if sys.platform.startswith("linux"):
            notify_send = shutil.which("notify-send")
            if not notify_send:
                return
            bus_path = Path(f"/run/user/{pw.pw_uid}/bus")
            if os.geteuid() == 0:
                if not bus_path.exists():
                    return
                env = {
                    "PATH": os.environ.get("PATH", ""),
                    "HOME": pw.pw_dir,
                    "USER": pw.pw_name,
                    "XDG_RUNTIME_DIR": f"/run/user/{pw.pw_uid}",
                    "DBUS_SESSION_BUS_ADDRESS": f"unix:path={bus_path}",
                }
                subprocess.run(
                    [notify_send, "--app-name=disk-cleanup", title, body],
                    stdin=subprocess.DEVNULL,
                    capture_output=True,
                    env=env,
                    user=pw.pw_uid,
                    group=pw.pw_gid,
                    timeout=15,
                )
            else:
                env = os.environ.copy()
                if "DBUS_SESSION_BUS_ADDRESS" not in env:
                    if not bus_path.exists():
                        return
                    env["DBUS_SESSION_BUS_ADDRESS"] = f"unix:path={bus_path}"
                subprocess.run(
                    [notify_send, "--app-name=disk-cleanup", title, body],
                    stdin=subprocess.DEVNULL,
                    capture_output=True,
                    env=env,
                    timeout=15,
                )
        elif sys.platform == "darwin":
            notifier = shutil.which("terminal-notifier")
            if notifier:
                base_cmd = [
                    notifier,
                    "-title",
                    title,
                    "-message",
                    body,
                    "-group",
                    "disk-cleanup",
                ]
            else:
                base_cmd = [
                    "/usr/bin/osascript",
                    "-e",
                    "on run argv\ndisplay notification (item 1 of argv) with title (item 2 of argv)\nend run",
                    "--",
                    body,
                    title,
                ]
            if os.geteuid() == 0 and pw.pw_uid != 0:
                cmd = [
                    "/bin/launchctl",
                    "asuser",
                    str(pw.pw_uid),
                    "/usr/bin/sudo",
                    "-n",
                    "-u",
                    pw.pw_name,
                    *base_cmd,
                ]
            else:
                cmd = base_cmd
            subprocess.run(
                cmd,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                timeout=15,
            )
    except (OSError, subprocess.SubprocessError) as exc:
        log(f"warning: desktop notification failed: {exc}")


def build_summary_parts(
    user_stats: UserPhaseStats,
    nix_stats: NixGcStats,
    tmp_stats: TmpCleanupStats,
) -> list[str]:
    parts: list[str] = []
    if user_stats.result_links_removed > 0:
        parts.append(f"{user_stats.result_links_removed} stale result link(s)")
    if user_stats.targets_removed > 0:
        parts.append(
            f"{user_stats.targets_removed} Rust target/ dir(s) "
            f"({format_bytes(user_stats.targets_bytes)})"
        )
    if tmp_stats.dirs_removed > 0:
        parts.append(
            f"{tmp_stats.dirs_removed} /tmp/nix-* dir(s) "
            f"({format_bytes(tmp_stats.bytes_freed)})"
        )
    if nix_stats.paths_deleted > 0:
        parts.append(f"Nix GC ({format_bytes(nix_stats.freed_bytes)})")
    return parts


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    default_user = pwd.getpwuid(os.getuid()).pw_name
    parser = argparse.ArgumentParser(
        prog="disk-cleanup",
        description="Clean safe Nix and Rust artifacts when disk usage hits a threshold.",
    )
    parser.add_argument(
        "--threshold",
        type=int,
        default=85,
        help="Disk usage percentage that triggers cleanup (default: 85).",
    )
    parser.add_argument(
        "--user",
        default=default_user,
        help="Target user whose ~/Projects and Nix profile are cleaned.",
    )
    parser.add_argument(
        "--projects-dir",
        type=Path,
        default=None,
        help="Projects directory to scan (default: <user-home>/Projects).",
    )
    parser.add_argument(
        "--result-age-days",
        type=float,
        default=7.0,
        help="Minimum age in days for Nix result/result-* symlinks (default: 7).",
    )
    parser.add_argument(
        "--target-age-hours",
        type=float,
        default=24.0,
        help="Minimum idle time in hours before cleaning a Rust target/ dir (default: 24).",
    )
    parser.add_argument(
        "--nix-gc-age-days",
        type=int,
        default=14,
        help="Delete Nix profile generations older than N days (default: 14).",
    )
    parser.add_argument(
        "--tmp-age-hours",
        type=float,
        default=24.0,
        help="Minimum idle time in hours for /tmp/nix-* dirs on Linux (default: 24).",
    )
    parser.add_argument(
        "--tmp-dir",
        type=Path,
        default=Path("/tmp"),
        help=argparse.SUPPRESS,
    )
    parser.add_argument(
        "--lock-file",
        type=Path,
        default=None,
        help=argparse.SUPPRESS,
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Report what would be cleaned without deleting anything.",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Run cleanup regardless of current disk usage percentage.",
    )
    parser.add_argument(
        "--no-notify",
        action="store_true",
        help="Disable desktop notification.",
    )
    parser.add_argument(
        "--verbose",
        action="store_true",
        help="Log even when disk usage is below the threshold.",
    )
    parser.add_argument(
        "--_user-phase",
        action="store_true",
        help=argparse.SUPPRESS,
    )
    args = parser.parse_args(argv)
    if not (1 <= args.threshold <= 100):
        parser.error("--threshold must be between 1 and 100")
    if args.result_age_days < 0 or args.target_age_hours < 0 or args.nix_gc_age_days < 0 or args.tmp_age_hours < 0:
        parser.error("age arguments must be non-negative")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        pw = pwd.getpwnam(args.user)
    except KeyError:
        log(f"error: unknown user {args.user!r}")
        return 2

    user_home = Path(pw.pw_dir)
    projects_dir = args.projects_dir if args.projects_dir is not None else (user_home / "Projects")

    if args._user_phase:
        stats = run_user_phase_in_process(
            user_name=pw.pw_name,
            user_home=user_home,
            projects_dir=projects_dir,
            result_age_days=args.result_age_days,
            target_age_hours=args.target_age_hours,
            nix_gc_age_days=args.nix_gc_age_days,
            dry_run=args.dry_run,
        )
        print(json.dumps(dataclasses.asdict(stats)))
        return 0

    if os.geteuid() != 0 and pw.pw_uid != os.geteuid():
        log(f"error: --user {args.user} requires root privileges")
        return 2

    lock_path = args.lock_file if args.lock_file is not None else default_lock_path(user_home)
    lock_fd = acquire_lock(lock_path)
    if lock_fd is None:
        if args.verbose or args.dry_run or args.force:
            log("another disk-cleanup run is already in progress; skipping")
        return 0

    try:
        watch_paths = [projects_dir if projects_dir.exists() else user_home, Path("/nix")]
        before = worst_disk_usage(watch_paths)

        if before.percent < args.threshold and not args.force:
            if args.verbose or args.dry_run:
                log(
                    f"disk usage {before.percent}% < {args.threshold}% threshold "
                    f"({format_bytes(before.avail_bytes)} free on {before.path}); nothing to do"
                )
            return 0

        mode = "dry-run" if args.dry_run else "cleanup"
        trigger = (
            f"forced at {before.percent}%"
            if args.force and before.percent < args.threshold
            else f"triggered at {before.percent}% >= {args.threshold}%"
        )
        log(
            f"starting {mode} ({trigger}, {format_bytes(before.avail_bytes)} free on {before.path})"
        )

        # Phase 1 (dropped to target user): unpin stale result* symlinks, delete
        # idle Rust target/ dirs (giving Nix SQLite room if disk is nearly full),
        # and prune per-user Nix profile generations.
        user_stats = run_user_phase(
            pw=pw,
            projects_dir=projects_dir,
            result_age_days=args.result_age_days,
            target_age_hours=args.target_age_hours,
            nix_gc_age_days=args.nix_gc_age_days,
            dry_run=args.dry_run,
        )

        # Phase 2a (Linux only): clean stale /tmp/nix-* shell/build dirs before GC.
        if sys.platform.startswith("linux"):
            tmp_stats = clean_nix_tmp_dirs(
                tmp_dir=args.tmp_dir,
                tmp_age_hours=args.tmp_age_hours,
                dry_run=args.dry_run,
            )
        else:
            tmp_stats = TmpCleanupStats()

        # Phase 2b: prune system generations (when root) and collect dead store paths.
        nix_stats = run_nix_gc(
            nix_gc_age_days=args.nix_gc_age_days,
            dry_run=args.dry_run,
        )

        all_errors = user_stats.errors + tmp_stats.errors + nix_stats.errors
        for err in all_errors:
            log(f"error: {err}")

        if args.dry_run:
            est_bytes = user_stats.targets_bytes + tmp_stats.bytes_freed
            log(
                f"[dry-run] summary: {user_stats.result_links_removed} stale result link(s), "
                f"{user_stats.targets_removed} Rust target/ dir(s) ({format_bytes(user_stats.targets_bytes)}), "
                f"{tmp_stats.dirs_removed} /tmp/nix-* dir(s) ({format_bytes(tmp_stats.bytes_freed)}), "
                f"{nix_stats.dead_paths_dry_run} currently dead Nix store path(s); "
                f"skipped {user_stats.targets_skipped_active} locked and "
                f"{user_stats.targets_skipped_recent} recent target/ dir(s); "
                f"estimated non-GC reclaim: {format_bytes(est_bytes)}"
            )
            return 1 if all_errors else 0

        after = stat_filesystem(Path(before.path))
        net_freed = max(0, after.avail_bytes - before.avail_bytes)
        tracked_freed = user_stats.targets_bytes + tmp_stats.bytes_freed + nix_stats.freed_bytes
        summary_parts = build_summary_parts(user_stats, nix_stats, tmp_stats)

        if summary_parts:
            log(
                f"finished: {before.percent}% -> {after.percent}% "
                f"(net freed {format_bytes(net_freed)}, {format_bytes(after.avail_bytes)} free); "
                f"cleaned: {', '.join(summary_parts)}"
            )
        else:
            log(
                f"finished: nothing eligible to clean "
                f"(disk still {after.percent}%, {format_bytes(after.avail_bytes)} free)"
            )

        if not args.no_notify and (
            net_freed >= NOTIFY_MIN_FREED_BYTES or tracked_freed >= NOTIFY_MIN_FREED_BYTES
        ):
            reported_freed = net_freed if net_freed >= NOTIFY_MIN_FREED_BYTES else tracked_freed
            title = f"Disk cleanup freed {format_bytes(reported_freed)}"
            lines = [
                f"Disk: {before.percent}% → {after.percent}% ({format_bytes(after.avail_bytes)} free)"
            ]
            if summary_parts:
                lines.append("Cleaned: " + ", ".join(summary_parts))
            send_notification(pw, title, "\n".join(lines))

        return 1 if all_errors else 0
    finally:
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_UN)
        finally:
            os.close(lock_fd)


if __name__ == "__main__":
    sys.exit(main())
