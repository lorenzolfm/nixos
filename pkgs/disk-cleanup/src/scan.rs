//! Filesystem usage calculation, lock probing, and bounded tree inspection.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cleanup::{append_error, log};
use crate::constants::{
    AGE_DAYS_MAX, AGE_HOURS_MAX, CACHEDIR_TAG_SIGNATURE_BYTES, CANDIDATES_MAX, INODES_TRACKED_MAX,
    NIX_TMP_PREFIXES, PROJECTS_WALK_ENTRIES_MAX, SKIP_WALK_DIRS, TARGET_MTIME_DEPTH_MAX,
    TARGET_SKIP_MTIME_CHILDREN, TMP_DIR_ENTRIES_MAX, TREE_WALK_ENTRIES_MAX, WALK_DEPTH_MAX,
    WATCH_PATHS_MAX,
};
use crate::sys::{self, FilesystemStat, LockProbe};

pub const NANOS_PER_SECOND: i128 = 1_000_000_000;
const SECONDS_PER_HOUR: f64 = 3600.0;
const SECONDS_PER_DAY: f64 = 86_400.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskUsage {
    pub path: PathBuf,
    pub device_id: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub used_percent: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetCandidate {
    pub path: PathBuf,
    pub mtime_latest_ns: i128,
    pub size_bytes: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProjectScanResult {
    pub stale_results: Vec<(PathBuf, i128)>,
    pub target_candidates: Vec<TargetCandidate>,
    pub targets_skipped_active_count: u64,
    pub targets_skipped_recent_count: u64,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TmpTreeSummary {
    pub mtime_latest_ns: i128,
    pub size_bytes: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TmpScanResult {
    pub stale_dirs: Vec<(PathBuf, u64)>,
    pub dirs_skipped_active_count: u64,
    pub dirs_skipped_recent_count: u64,
    pub errors: Vec<String>,
}

/// Convert a finite non-negative number of seconds to exact nanoseconds.
pub fn non_negative_seconds_to_ns(seconds: f64) -> i128 {
    assert!(seconds.is_finite(), "seconds must be finite");
    assert!(seconds >= 0.0, "seconds must be non-negative");
    assert!(seconds <= 1.0e13, "seconds exceeds safe conversion bound");

    let whole_seconds = seconds.trunc();
    let frac_seconds = seconds - whole_seconds;
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    let whole_i128 = whole_seconds as i128;
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    let frac_ns = (frac_seconds * 1_000_000_000.0).round() as i128;

    let result = whole_i128 * NANOS_PER_SECOND + frac_ns;
    assert!(result >= 0, "converted nanoseconds must be non-negative");
    result
}

/// Current wall-clock Unix time in nanoseconds.
pub fn current_time_ns() -> i128 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after UNIX_EPOCH");
    let nanos = i128::try_from(duration.as_nanos()).expect("system time fits i128");
    assert!(nanos > 0, "current_time_ns must be positive");
    nanos
}

/// Extract exact modification timestamp in nanoseconds since Unix epoch.
pub fn metadata_mtime_ns(metadata: &fs::Metadata) -> i128 {
    let secs = i128::from(metadata.mtime());
    let nsec = i128::from(metadata.mtime_nsec());
    secs * NANOS_PER_SECOND + nsec
}

/// Compute default lockfile location for root vs non-root invocations.
pub fn default_lock_path(user_home: &Path) -> PathBuf {
    assert!(user_home.is_absolute(), "user_home must be absolute");
    let result = if sys::effective_uid() == 0 {
        if cfg!(target_os = "linux") {
            let run_dir = Path::new("/run/disk-cleanup");
            if run_dir.is_dir() {
                run_dir.join("lock")
            } else {
                PathBuf::from("/run/disk-cleanup.lock")
            }
        } else {
            PathBuf::from("/var/run/disk-cleanup.lock")
        }
    } else if let Some(xdg_runtime) = env::var_os("XDG_RUNTIME_DIR")
        .filter(|val| !val.is_empty())
        .and_then(|val| sys::absolutize_path(Path::new(&val)).ok())
    {
        xdg_runtime.join("disk-cleanup.lock")
    } else {
        user_home.join(".cache").join("disk-cleanup.lock")
    };

    assert!(
        result.is_absolute(),
        "default_lock_path must return an absolute path"
    );
    result
}

/// Pure calculation of filesystem usage matching `df`'s non-root usable percentage.
///
/// Uses exact `u128` ceiling division (`div_ceil`) so large block counts (`> 2^53`)
/// never suffer floating-point precision loss or integer overflow.
pub fn compute_disk_usage(path: &Path, fs_stat: FilesystemStat) -> DiskUsage {
    assert!(path.is_absolute(), "path must be absolute");
    assert!(
        fs_stat.fragment_size_bytes > 0,
        "f_frsize must be positive: {}",
        fs_stat.fragment_size_bytes
    );

    let used_blocks = fs_stat.blocks_total.saturating_sub(fs_stat.blocks_free);
    let available_blocks = fs_stat.blocks_available;
    let usable_blocks_total = used_blocks.saturating_add(available_blocks);

    let used_percent: u8 = if usable_blocks_total == 0 {
        100
    } else {
        let numerator = u128::from(used_blocks) * 100;
        let ceiling_percent = numerator.div_ceil(u128::from(usable_blocks_total)).min(100);
        u8::try_from(ceiling_percent).expect("clamped percentage in 0..=100 fits u8")
    };

    let used_bytes = used_blocks.saturating_mul(fs_stat.fragment_size_bytes);
    let available_bytes = available_blocks.saturating_mul(fs_stat.fragment_size_bytes);
    assert!(
        used_percent <= 100,
        "used_percent out of range: {used_percent}"
    );

    DiskUsage {
        path: path.to_path_buf(),
        device_id: fs_stat.device_id,
        used_bytes,
        available_bytes,
        used_percent,
    }
}

/// Query filesystem usage via `statvfs` + `stat`.
pub fn stat_filesystem(path: &Path) -> io::Result<DiskUsage> {
    assert!(path.is_absolute(), "path must be absolute");
    let fs_stat = sys::statvfs_path(path)?;
    Ok(compute_disk_usage(path, fs_stat))
}

/// Return the highest-usage filesystem among existing candidate paths.
pub fn worst_disk_usage(paths: &[PathBuf]) -> io::Result<DiskUsage> {
    assert!(
        paths.len() <= WATCH_PATHS_MAX,
        "watch paths count {} exceeds WATCH_PATHS_MAX ({WATCH_PATHS_MAX})",
        paths.len()
    );

    let mut seen_devices = HashSet::with_capacity(paths.len());
    let mut usages = Vec::with_capacity(paths.len());

    for path in paths {
        if !path.exists() {
            continue;
        }
        let usage = match stat_filesystem(path) {
            Ok(val) => val,
            Err(err) => {
                log(&format!(
                    "warning: statvfs failed for {}: {err}",
                    path.display()
                ));
                continue;
            }
        };
        if !seen_devices.insert(usage.device_id) {
            continue;
        }
        usages.push(usage);
    }

    let worst = match usages
        .into_iter()
        .max_by_key(|usage| (usage.used_percent, Reverse(usage.available_bytes)))
    {
        Some(val) => val,
        None => stat_filesystem(Path::new("/"))?,
    };
    assert!(worst.used_percent <= 100, "worst.used_percent out of range");
    Ok(worst)
}

/// Measure allocated disk bytes of a directory tree without following links.
pub fn dir_disk_usage(root: &Path) -> u64 {
    assert!(root.is_absolute(), "dir_disk_usage requires absolute path");

    let mut total_bytes: u64 = 0;
    let mut seen_inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut entries_visited_count: usize = 0;
    let mut stack: Vec<(PathBuf, usize)> = Vec::with_capacity(WALK_DEPTH_MAX);
    stack.push((root.to_path_buf(), 0));

    let mut dirs_popped_count: usize = 0;
    while let Some((current, depth)) = stack.pop() {
        dirs_popped_count = dirs_popped_count.saturating_add(1);
        if dirs_popped_count > TREE_WALK_ENTRIES_MAX {
            return total_bytes;
        }
        if depth >= WALK_DEPTH_MAX {
            continue;
        }
        let read_dir = match fs::read_dir(&current) {
            Ok(iter) => iter,
            Err(_) => continue,
        };
        for entry_result in read_dir {
            entries_visited_count = entries_visited_count.saturating_add(1);
            if entries_visited_count > TREE_WALK_ENTRIES_MAX {
                return total_bytes;
            }
            let Ok(entry) = entry_result else {
                continue;
            };
            let entry_path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&entry_path) else {
                continue;
            };
            if meta.nlink() > 1 {
                let inode_key = (meta.dev(), meta.ino());
                if seen_inodes.contains(&inode_key) {
                    continue;
                }
                if seen_inodes.len() < INODES_TRACKED_MAX {
                    seen_inodes.insert(inode_key);
                }
            }
            // `st_blocks` is always in 512-byte units on POSIX.
            total_bytes = total_bytes.saturating_add(meta.blocks().saturating_mul(512));
            if meta.file_type().is_dir() {
                stack.push((entry_path, depth + 1));
            }
        }
    }

    total_bytes
}

/// Verify `target_dir` is genuinely a Cargo build output directory.
pub fn is_cargo_target_dir(target_dir: &Path) -> bool {
    assert!(target_dir.is_absolute(), "target_dir must be absolute");
    let Some(parent_dir) = target_dir.parent() else {
        panic!("target_dir cannot be filesystem root");
    };
    assert_ne!(
        parent_dir, target_dir,
        "target_dir cannot be filesystem root"
    );

    let Ok(dir_meta) = fs::symlink_metadata(target_dir) else {
        return false;
    };
    if !dir_meta.file_type().is_dir() {
        return false;
    }

    let cargo_toml = parent_dir.join("Cargo.toml");
    let Ok(cargo_meta) = fs::symlink_metadata(&cargo_toml) else {
        return false;
    };
    if !cargo_meta.file_type().is_file() {
        return false;
    }

    let cachedir_tag = target_dir.join("CACHEDIR.TAG");
    let mut header_buf = [0u8; CACHEDIR_TAG_SIGNATURE_BYTES.len()];
    if let Ok(Some(bytes_read)) = sys::read_regular_prefix_nofollow(&cachedir_tag, &mut header_buf)
        && bytes_read == CACHEDIR_TAG_SIGNATURE_BYTES.len()
        && header_buf == CACHEDIR_TAG_SIGNATURE_BYTES
    {
        return true;
    }

    for marker in [".cargo-lock", ".rustc_info.json"] {
        if let Ok(marker_meta) = fs::symlink_metadata(target_dir.join(marker))
            && marker_meta.file_type().is_file()
        {
            return true;
        }
    }

    false
}

/// Return `true` if Cargo holds an exclusive `flock` on `.cargo-lock` (fails closed).
pub fn is_cargo_target_locked(target_dir: &Path) -> bool {
    assert!(target_dir.is_absolute(), "target_dir must be absolute");
    let lock_file = target_dir.join(".cargo-lock");
    match sys::probe_exclusive_flock(&lock_file) {
        LockProbe::Unlocked => false,
        LockProbe::Locked => true,
        LockProbe::UnexpectedError(err) => {
            log(&format!(
                "warning: flock check failed on {}: {err}; treating as locked",
                lock_file.display()
            ));
            // Fail closed on any unexpected flock error so active builds are never deleted.
            true
        }
    }
}

/// Find the most recent mtime inside a Cargo `target/` directory (fails closed).
///
/// Skips recursing into `deps/` and `examples/` while still stat-ing those
/// directories themselves. Returns `None` if any entry is unreadable, if walk
/// bounds are exceeded, or if `cutoff_ns` is given and any entry is newer.
pub fn latest_target_mtime_ns(
    target_dir: &Path,
    now_ns: i128,
    cutoff_ns: Option<i128>,
    depth_max: usize,
) -> Option<i128> {
    assert!(target_dir.is_absolute(), "target_dir must be absolute");
    assert!(now_ns > 0, "now_ns must be positive");
    assert!(depth_max >= 1, "depth_max must be at least 1");
    assert!(
        depth_max <= WALK_DEPTH_MAX,
        "depth_max exceeds WALK_DEPTH_MAX"
    );

    let root_meta = fs::symlink_metadata(target_dir).ok()?;
    let mut newest_ns = metadata_mtime_ns(&root_meta);

    if let Some(limit) = cutoff_ns
        && newest_ns > limit
    {
        return None;
    }

    let mut entries_visited_count: usize = 0;
    let mut stack: Vec<(PathBuf, usize)> = Vec::with_capacity(depth_max);
    stack.push((target_dir.to_path_buf(), 0));

    let mut dirs_popped_count: usize = 0;
    while let Some((current, depth)) = stack.pop() {
        dirs_popped_count = dirs_popped_count.saturating_add(1);
        if dirs_popped_count > TREE_WALK_ENTRIES_MAX {
            return None;
        }
        if depth >= depth_max {
            continue;
        }
        // Fail closed on unreadable subdirectory inside `target/`.
        let read_dir = fs::read_dir(&current).ok()?;
        for entry_result in read_dir {
            entries_visited_count = entries_visited_count.saturating_add(1);
            if entries_visited_count > TREE_WALK_ENTRIES_MAX {
                return None;
            }
            // Fail closed: disappearing/unreadable child could belong to an active build.
            let entry = entry_result.ok()?;
            let entry_path = entry.path();
            let entry_meta = fs::symlink_metadata(&entry_path).ok()?;
            let entry_mtime_ns = metadata_mtime_ns(&entry_meta);

            if let Some(limit) = cutoff_ns
                && entry_mtime_ns > limit
            {
                return None;
            }
            if entry_mtime_ns > newest_ns {
                newest_ns = entry_mtime_ns;
            }
            if entry_meta.file_type().is_dir() {
                let is_skipped_child = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| TARGET_SKIP_MTIME_CHILDREN.contains(&name));
                if !is_skipped_child {
                    stack.push((entry_path, depth + 1));
                }
            }
        }
    }

    if let Some(limit) = cutoff_ns {
        assert!(newest_ns <= limit, "returned mtime must satisfy cutoff");
    }
    Some(newest_ns)
}

/// Single-pass mtime + disk-usage inspection for a `/tmp/nix-*` tree.
///
/// Returns `None` (fail closed / skip) if any entry is newer than `cutoff_ns`,
/// if any subdirectory/entry cannot be stat-ed, or if walk bounds are exceeded.
pub fn inspect_tmp_tree(root: &Path, now_ns: i128, cutoff_ns: i128) -> Option<TmpTreeSummary> {
    assert!(root.is_absolute(), "root must be absolute");
    assert!(now_ns > 0, "now_ns must be positive");

    let root_meta = fs::symlink_metadata(root).ok()?;
    if !root_meta.file_type().is_dir() {
        return None;
    }
    let mut newest_ns = metadata_mtime_ns(&root_meta);
    if newest_ns > cutoff_ns {
        return None;
    }

    let mut total_bytes: u64 = root_meta.blocks().saturating_mul(512);
    let mut seen_inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut entries_visited_count: usize = 0;
    let mut stack: Vec<(PathBuf, usize)> = Vec::with_capacity(WALK_DEPTH_MAX);
    stack.push((root.to_path_buf(), 0));

    let mut dirs_popped_count: usize = 0;
    while let Some((current, depth)) = stack.pop() {
        dirs_popped_count = dirs_popped_count.saturating_add(1);
        if dirs_popped_count > TREE_WALK_ENTRIES_MAX {
            return None;
        }
        if depth >= WALK_DEPTH_MAX {
            return None;
        }
        let read_dir = fs::read_dir(&current).ok()?;
        for entry_result in read_dir {
            entries_visited_count = entries_visited_count.saturating_add(1);
            if entries_visited_count > TREE_WALK_ENTRIES_MAX {
                return None;
            }
            let entry = entry_result.ok()?;
            let entry_path = entry.path();
            let entry_meta = fs::symlink_metadata(&entry_path).ok()?;
            let entry_mtime_ns = metadata_mtime_ns(&entry_meta);
            if entry_mtime_ns > cutoff_ns {
                return None;
            }
            if entry_mtime_ns > newest_ns {
                newest_ns = entry_mtime_ns;
            }

            let count_blocks = if entry_meta.nlink() > 1 {
                let inode_key = (entry_meta.dev(), entry_meta.ino());
                if seen_inodes.contains(&inode_key) {
                    false
                } else {
                    if seen_inodes.len() < INODES_TRACKED_MAX {
                        seen_inodes.insert(inode_key);
                    }
                    true
                }
            } else {
                true
            };
            if count_blocks {
                total_bytes = total_bytes.saturating_add(entry_meta.blocks().saturating_mul(512));
            }

            if entry_meta.file_type().is_dir() {
                stack.push((entry_path, depth + 1));
            }
        }
    }

    assert!(
        newest_ns <= cutoff_ns,
        "returned tmp tree must satisfy cutoff"
    );
    Some(TmpTreeSummary {
        mtime_latest_ns: newest_ns,
        size_bytes: total_bytes,
    })
}

/// Walk `projects_dir` once to discover stale Nix `result*` links and Rust `target/` dirs.
pub fn scan_projects(
    projects_dir: &Path,
    now_ns: i128,
    result_age_days: f64,
    target_age_hours: f64,
) -> ProjectScanResult {
    assert!(projects_dir.is_absolute(), "projects_dir must be absolute");
    assert!(
        !sys::is_forbidden_delete_path(projects_dir),
        "projects_dir cannot be a system root"
    );
    assert!(now_ns > 0, "now_ns must be positive");
    assert!(result_age_days.is_finite());
    assert!(result_age_days >= 0.0);
    assert!(result_age_days <= f64::from(AGE_DAYS_MAX));
    assert!(target_age_hours.is_finite());
    assert!(target_age_hours >= 0.0);
    assert!(target_age_hours <= f64::from(AGE_HOURS_MAX));

    let mut result = ProjectScanResult::default();
    if !projects_dir.is_dir() {
        return result;
    }

    let result_mtime_max_ns =
        now_ns - non_negative_seconds_to_ns(result_age_days * SECONDS_PER_DAY);
    let target_mtime_max_ns =
        now_ns - non_negative_seconds_to_ns(target_age_hours * SECONDS_PER_HOUR);

    let mut entries_visited_count: usize = 0;
    let mut stack: Vec<(PathBuf, usize)> = Vec::with_capacity(WALK_DEPTH_MAX);
    stack.push((projects_dir.to_path_buf(), 0));

    let mut dirs_popped_count: usize = 0;
    while let Some((current, depth)) = stack.pop() {
        dirs_popped_count = dirs_popped_count.saturating_add(1);
        if dirs_popped_count > PROJECTS_WALK_ENTRIES_MAX {
            append_error(
                &mut result.errors,
                format!(
                    "scan {}: exceeded {PROJECTS_WALK_ENTRIES_MAX} entries limit",
                    projects_dir.display()
                ),
            );
            break;
        }
        if depth >= WALK_DEPTH_MAX {
            continue;
        }

        let read_dir = match fs::read_dir(&current) {
            Ok(iter) => iter,
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                // Unreadable subtrees (e.g. Docker bind-mounted data dirs) are not
                // cleanup errors; skip them.
                continue;
            }
            Err(err) => {
                append_error(
                    &mut result.errors,
                    format!("scan {}: {err}", current.display()),
                );
                continue;
            }
        };

        let mut subdirs_to_push: Vec<PathBuf> = Vec::new();
        for entry_result in read_dir {
            entries_visited_count = entries_visited_count.saturating_add(1);
            if entries_visited_count > PROJECTS_WALK_ENTRIES_MAX {
                append_error(
                    &mut result.errors,
                    format!(
                        "scan {}: exceeded {PROJECTS_WALK_ENTRIES_MAX} entries limit",
                        projects_dir.display()
                    ),
                );
                stack.clear();
                subdirs_to_push.clear();
                break;
            }

            let Ok(entry) = entry_result else {
                continue;
            };
            let entry_path = entry.path();
            let Ok(entry_meta) = fs::symlink_metadata(&entry_path) else {
                continue;
            };
            let file_name_os = entry.file_name();
            let file_name_lossy = file_name_os.to_string_lossy();
            let file_type = entry_meta.file_type();

            if file_type.is_symlink() {
                if (file_name_lossy == "result" || file_name_lossy.starts_with("result-"))
                    && let Ok(link_target) = fs::read_link(&entry_path)
                {
                    let mtime_ns = metadata_mtime_ns(&entry_meta);
                    if link_target
                        .as_os_str()
                        .as_bytes()
                        .starts_with(b"/nix/store/")
                        && mtime_ns <= result_mtime_max_ns
                        && result.stale_results.len() < CANDIDATES_MAX
                    {
                        result.stale_results.push((entry_path, mtime_ns));
                    }
                }
                continue;
            }

            if !file_type.is_dir() {
                continue;
            }

            if file_name_lossy.starts_with('.')
                || SKIP_WALK_DIRS.contains(&file_name_lossy.as_ref())
            {
                continue;
            }

            if file_name_lossy == "target" {
                if is_cargo_target_dir(&entry_path) {
                    if is_cargo_target_locked(&entry_path) {
                        result.targets_skipped_active_count += 1;
                    } else {
                        let newest_ns = latest_target_mtime_ns(
                            &entry_path,
                            now_ns,
                            Some(target_mtime_max_ns),
                            TARGET_MTIME_DEPTH_MAX,
                        );
                        if let Some(mtime_latest_ns) = newest_ns {
                            assert!(mtime_latest_ns <= target_mtime_max_ns);
                            if result.target_candidates.len() < CANDIDATES_MAX {
                                let size_bytes = dir_disk_usage(&entry_path);
                                result.target_candidates.push(TargetCandidate {
                                    path: entry_path,
                                    mtime_latest_ns,
                                    size_bytes,
                                });
                            }
                        } else {
                            result.targets_skipped_recent_count += 1;
                        }
                    }
                }
                // Never descend into any `target` directory.
                continue;
            }

            subdirs_to_push.push(entry_path);
        }

        for subdir in subdirs_to_push {
            stack.push((subdir, depth + 1));
        }
    }

    result.stale_results.sort_by_key(|&(_, mtime_ns)| mtime_ns);
    result
        .target_candidates
        .sort_by_key(|candidate| candidate.mtime_latest_ns);

    assert!(
        result.stale_results.len() <= CANDIDATES_MAX,
        "stale_results exceeded CANDIDATES_MAX"
    );
    assert!(
        result.target_candidates.len() <= CANDIDATES_MAX,
        "target_candidates exceeded CANDIDATES_MAX"
    );
    result
}

/// Pure discovery of stale `/tmp/nix-*` directories using single-pass inspection.
pub fn scan_stale_nix_tmp_dirs(tmp_dir: &Path, tmp_age_hours: f64, now_ns: i128) -> TmpScanResult {
    assert!(tmp_dir.is_absolute(), "tmp_dir must be absolute");
    assert_ne!(tmp_dir, Path::new("/"), "tmp_dir must not be /");
    assert!(now_ns > 0, "now_ns must be positive");
    assert!(tmp_age_hours.is_finite());
    assert!(tmp_age_hours >= 0.0);
    assert!(tmp_age_hours <= f64::from(AGE_HOURS_MAX));

    let mut result = TmpScanResult::default();
    if !tmp_dir.is_dir() {
        return result;
    }

    let cutoff_ns = now_ns - non_negative_seconds_to_ns(tmp_age_hours * SECONDS_PER_HOUR);
    let euid = sys::effective_uid();

    let read_dir = match fs::read_dir(tmp_dir) {
        Ok(iter) => iter,
        Err(err) => {
            append_error(
                &mut result.errors,
                format!("scan {}: {err}", tmp_dir.display()),
            );
            return result;
        }
    };

    let mut entries_seen_count: usize = 0;
    for entry_result in read_dir {
        entries_seen_count = entries_seen_count.saturating_add(1);
        if entries_seen_count > TMP_DIR_ENTRIES_MAX {
            append_error(
                &mut result.errors,
                format!(
                    "scan {}: exceeded {TMP_DIR_ENTRIES_MAX} entries limit",
                    tmp_dir.display()
                ),
            );
            break;
        }

        let Ok(entry) = entry_result else {
            continue;
        };
        let name_os = entry.file_name();
        let Some(name_str) = name_os.to_str() else {
            continue;
        };
        if !is_nix_tmp_dir_name(name_str) {
            continue;
        }

        let entry_path = entry.path();
        let Ok(entry_meta) = fs::symlink_metadata(&entry_path) else {
            continue;
        };
        if !entry_meta.file_type().is_dir() {
            continue;
        }
        assert!(
            !entry_meta.file_type().is_symlink(),
            "is_dir on symlink_metadata must exclude symlinks"
        );
        if euid != 0 && entry_meta.uid() != euid {
            continue;
        }

        if let Some(pid) = parse_nix_develop_pid(name_str)
            && sys::is_pid_alive(pid)
        {
            result.dirs_skipped_active_count += 1;
            continue;
        }

        let Some(tree) = inspect_tmp_tree(&entry_path, now_ns, cutoff_ns) else {
            result.dirs_skipped_recent_count += 1;
            continue;
        };

        if result.stale_dirs.len() >= CANDIDATES_MAX {
            append_error(
                &mut result.errors,
                format!(
                    "scan {}: exceeded {CANDIDATES_MAX} candidate limit",
                    tmp_dir.display()
                ),
            );
            break;
        }
        result.stale_dirs.push((entry_path, tree.size_bytes));
    }

    assert!(result.stale_dirs.len() <= CANDIDATES_MAX);
    result
}

/// Return existing per-user Nix profile paths to prune generations on.
pub fn user_nix_profiles(user_name: &str, user_home: &Path) -> Vec<PathBuf> {
    assert!(!user_name.is_empty(), "user_name must be non-empty");
    assert!(user_home.is_absolute(), "user_home must be absolute");

    let candidates = [
        user_home
            .join(".local")
            .join("state")
            .join("nix")
            .join("profiles")
            .join("profile"),
        Path::new("/nix/var/nix/profiles/per-user")
            .join(user_name)
            .join("profile"),
    ];

    let mut profiles = Vec::with_capacity(candidates.len());
    let mut seen: HashSet<PathBuf> = HashSet::with_capacity(candidates.len());

    for path in &candidates {
        // `symlink_metadata` succeeds for both existing targets and dangling symlinks.
        if fs::symlink_metadata(path).is_err() {
            continue;
        }
        let resolved = match (
            path.parent().and_then(|p| fs::canonicalize(p).ok()),
            path.file_name(),
        ) {
            (Some(canon_parent), Some(name)) => canon_parent.join(name),
            _ => path.clone(),
        };
        if seen.insert(resolved) {
            profiles.push(path.clone());
        }
    }

    assert!(profiles.len() <= candidates.len());
    profiles
}

pub fn is_nix_tmp_dir_name(name: &str) -> bool {
    assert!(
        !name.contains('/'),
        "directory entry name must not contain '/'"
    );
    NIX_TMP_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Extract PID from `nix-develop-<pid>-...` directory names.
pub fn parse_nix_develop_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("nix-develop-")?;
    let dash_index = rest.find('-')?;
    let digits = rest.get(..dash_index)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok()
}
