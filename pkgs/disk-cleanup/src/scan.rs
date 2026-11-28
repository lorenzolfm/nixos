//! Filesystem usage calculation, lock probing, and bounded tree inspection.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::num::NonZeroU32;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::cleanup::log;
use crate::constants::{
    CACHEDIR_TAG_SIGNATURE_BYTES, CANDIDATES_MAX, ERRORS_MAX, INODES_TRACKED_MAX, NANOS_PER_SECOND,
    NIX_TMP_PREFIXES, PROJECTS_WALK_ENTRIES_MAX, SKIP_WALK_DIRS, TARGET_MTIME_DEPTH_MAX,
    TARGET_SKIP_MTIME_CHILDREN, TMP_DIR_ENTRIES_MAX, TREE_WALK_ENTRIES_MAX, WALK_DEPTH_MAX,
    WATCH_PATHS_MAX,
};
use crate::sys::{self, CargoLockProbe, FilesystemStat};
use crate::types::{
    AbsPath, AgeDurationNs, BoundedErrors, DiskPercent, EntryName, NonRootAbsPath, SafeProjectsDir,
    TimestampNs, UserName,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskUsage {
    pub path: AbsPath,
    pub device_id: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub used_percent: DiskPercent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectScanConfig {
    pub now_ns: TimestampNs,
    pub result_age: AgeDurationNs,
    pub target_age: AgeDurationNs,
}

impl ProjectScanConfig {
    pub fn result_mtime_max_ns(&self) -> i128 {
        self.result_age.cutoff_before(self.now_ns)
    }

    pub fn target_mtime_max_ns(&self) -> i128 {
        self.target_age.cutoff_before(self.now_ns)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetCandidate {
    pub path: NonRootAbsPath,
    pub mtime_latest_ns: i128,
    pub size_bytes: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProjectScanResult {
    pub stale_results: Vec<(NonRootAbsPath, i128)>,
    pub target_candidates: Vec<TargetCandidate>,
    pub targets_skipped_active_count: u64,
    pub targets_skipped_recent_count: u64,
    pub targets_skipped_unreadable_count: u64,
    pub errors: BoundedErrors,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TmpTreeSummary {
    pub mtime_latest_ns: i128,
    pub size_bytes: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TmpScanResult {
    pub stale_dirs: Vec<(NonRootAbsPath, u64)>,
    pub dirs_skipped_active_count: u64,
    pub dirs_skipped_recent_count: u64,
    pub dirs_skipped_unreadable_count: u64,
    pub errors: BoundedErrors,
}

/// Distinct fail-closed outcomes for inspecting a directory tree against a recency cutoff.
#[derive(Debug)]
pub enum TreeFreshness<T> {
    /// Every entry in the inspected tree is at or older than `cutoff_ns`.
    Stale(T),
    /// At least one entry was modified more recently than `cutoff_ns`.
    Recent,
    /// An entry or subdirectory could not be read/stat-ed (or changed type).
    Unreadable(io::Error),
    /// The walk hit `WALK_DEPTH_MAX` or `TREE_WALK_ENTRIES_MAX`.
    LimitExceeded,
}

/// Extract exact modification timestamp in nanoseconds since Unix epoch.
pub fn metadata_mtime_ns(metadata: &fs::Metadata) -> i128 {
    let secs = i128::from(metadata.mtime());
    let nsec = i128::from(metadata.mtime_nsec());
    assert!(nsec >= 0, "mtime_nsec must be non-negative: {nsec}");
    assert!(nsec < NANOS_PER_SECOND, "mtime_nsec out of range: {nsec}");
    secs * NANOS_PER_SECOND + nsec
}

/// Return the single canonical host lockfile path shared by root daemons and manual runs.
pub fn default_lock_path() -> NonRootAbsPath {
    #[cfg(target_os = "linux")]
    let raw = Path::new("/run/disk-cleanup/lock");
    #[cfg(not(target_os = "linux"))]
    let raw = Path::new("/var/db/disk-cleanup/lock");

    NonRootAbsPath::from_absolute(raw).expect("canonical default lockfile path is valid")
}

/// Pure calculation of filesystem usage matching `df`'s non-root usable percentage.
///
/// Uses exact `u128` ceiling division (`div_ceil`) so large block counts (`> 2^53`)
/// never suffer floating-point precision loss or integer overflow.
pub fn compute_disk_usage(path: &AbsPath, fs_stat: FilesystemStat) -> DiskUsage {
    let used_blocks = fs_stat.blocks_total.saturating_sub(fs_stat.blocks_free);
    let available_blocks = fs_stat.blocks_available;
    let usable_blocks_total = used_blocks.saturating_add(available_blocks);

    let used_percent = if usable_blocks_total == 0 {
        DiskPercent::FULL
    } else {
        let numerator = u128::from(used_blocks) * 100;
        let ceiling_percent = numerator.div_ceil(u128::from(usable_blocks_total)).min(100);
        let pct_u8 = u8::try_from(ceiling_percent).expect("clamped percentage in 0..=100 fits u8");
        DiskPercent::try_new(pct_u8).expect("clamped percentage fits DiskPercent")
    };

    let frag_bytes = fs_stat.fragment_size_bytes.get();
    let used_bytes = used_blocks.saturating_mul(frag_bytes);
    let available_bytes = available_blocks.saturating_mul(frag_bytes);

    DiskUsage {
        path: path.clone(),
        device_id: fs_stat.device_id,
        used_bytes,
        available_bytes,
        used_percent,
    }
}

/// Query filesystem usage via `statvfs` + `stat`.
pub fn stat_filesystem(path: &AbsPath) -> io::Result<DiskUsage> {
    let fs_stat = sys::statvfs_path(path)?;
    Ok(compute_disk_usage(path, fs_stat))
}

/// Return the highest-usage filesystem among existing candidate paths.
pub fn worst_disk_usage(paths: &[AbsPath]) -> io::Result<DiskUsage> {
    assert!(
        paths.len() <= WATCH_PATHS_MAX,
        "watch paths count {} exceeds WATCH_PATHS_MAX ({WATCH_PATHS_MAX})",
        paths.len()
    );

    let mut seen_devices = HashSet::with_capacity(paths.len());
    let mut usages = Vec::with_capacity(paths.len());

    for path in paths {
        if !path.as_path().exists() {
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

    match usages
        .into_iter()
        .max_by_key(|usage| (usage.used_percent, Reverse(usage.available_bytes)))
    {
        Some(val) => Ok(val),
        None => stat_filesystem(&AbsPath::root()),
    }
}

/// Measure allocated disk bytes of a directory tree without following links.
pub fn dir_disk_usage(root: &NonRootAbsPath) -> u64 {
    let mut total_bytes: u64 = fs::symlink_metadata(root.as_path())
        .map(|meta| meta.blocks().saturating_mul(512))
        .unwrap_or(0);
    let mut seen_inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut entries_visited_count: usize = 0;
    let mut stack: Vec<(PathBuf, usize)> = Vec::with_capacity(WALK_DEPTH_MAX);
    stack.push((root.as_path().to_path_buf(), 0));

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
            if meta.file_type().is_dir() && depth + 1 < WALK_DEPTH_MAX {
                stack.push((entry_path, depth + 1));
            }
        }
    }

    assert!(entries_visited_count <= TREE_WALK_ENTRIES_MAX);
    total_bytes
}

/// Verify `target_dir` is genuinely a Cargo build output directory.
pub fn is_cargo_target_dir(target_dir: &NonRootAbsPath) -> bool {
    let parent_dir = target_dir.parent();

    let Ok(dir_meta) = fs::symlink_metadata(target_dir.as_path()) else {
        return false;
    };
    if !dir_meta.file_type().is_dir() {
        return false;
    }

    let cargo_toml = parent_dir.as_path().join("Cargo.toml");
    let Ok(cargo_meta) = fs::symlink_metadata(&cargo_toml) else {
        return false;
    };
    if !cargo_meta.file_type().is_file() {
        return false;
    }

    if let Ok(cachedir_tag) = target_dir.join_relative(Path::new("CACHEDIR.TAG")) {
        let mut header_buf = [0u8; CACHEDIR_TAG_SIGNATURE_BYTES.len()];
        if let Ok(Some(bytes_read)) =
            sys::read_regular_prefix_nofollow(&cachedir_tag, &mut header_buf)
            && bytes_read == CACHEDIR_TAG_SIGNATURE_BYTES.len()
            && header_buf == CACHEDIR_TAG_SIGNATURE_BYTES
        {
            return true;
        }
    }

    for marker in [".cargo-lock", ".rustc_info.json"] {
        if let Ok(marker_meta) = fs::symlink_metadata(target_dir.as_path().join(marker))
            && marker_meta.file_type().is_file()
        {
            return true;
        }
    }

    false
}

/// Attempt to acquire Cargo's `.cargo-lock` for `target_dir`, returning the full
/// probe state so callers can hold the lock across re-verification and deletion
/// and log warnings on symlink/unreadable/unexpected errors.
pub fn probe_cargo_target_lock(target_dir: &NonRootAbsPath) -> CargoLockProbe {
    let lock_file = match target_dir.join_relative(Path::new(".cargo-lock")) {
        Ok(path) => path,
        Err(err) => return CargoLockProbe::SymlinkOrUnreadable(err),
    };
    let probe = sys::try_acquire_cargo_lock(&lock_file);
    match &probe {
        CargoLockProbe::SymlinkOrUnreadable(err) => {
            log(&format!(
                "warning: cannot safely open {}: {err}; treating target as locked",
                lock_file.display()
            ));
        }
        CargoLockProbe::UnexpectedError(err) => {
            log(&format!(
                "warning: flock check failed on {}: {err}; treating target as locked",
                lock_file.display()
            ));
        }
        CargoLockProbe::NoLockFile | CargoLockProbe::Acquired(_) | CargoLockProbe::Locked => {}
    }
    probe
}

/// Find the most recent mtime inside a Cargo `target/` directory (fails closed).
///
/// Skips recursing into `deps/` and `examples/` while still stat-ing those
/// directories themselves. Distinguishes recent modifications from unreadable
/// entries and walk-limit overflows so failures are never miscounted as recent.
pub fn latest_target_mtime_ns(
    target_dir: &NonRootAbsPath,
    cutoff_ns: i128,
    depth_max: usize,
) -> TreeFreshness<i128> {
    assert!(depth_max >= 1, "depth_max must be at least 1");
    assert!(
        depth_max <= WALK_DEPTH_MAX,
        "depth_max exceeds WALK_DEPTH_MAX"
    );

    let root_meta = match fs::symlink_metadata(target_dir.as_path()) {
        Ok(meta) => meta,
        Err(err) => return TreeFreshness::Unreadable(err),
    };
    let mut newest_ns = metadata_mtime_ns(&root_meta);
    if newest_ns > cutoff_ns {
        return TreeFreshness::Recent;
    }

    let mut entries_visited_count: usize = 0;
    let mut stack: Vec<(PathBuf, usize)> = Vec::with_capacity(depth_max);
    stack.push((target_dir.as_path().to_path_buf(), 0));

    let mut dirs_popped_count: usize = 0;
    while let Some((current, depth)) = stack.pop() {
        dirs_popped_count = dirs_popped_count.saturating_add(1);
        if dirs_popped_count > TREE_WALK_ENTRIES_MAX {
            return TreeFreshness::LimitExceeded;
        }
        if depth >= depth_max {
            continue;
        }
        // Fail closed on unreadable subdirectory inside `target/`.
        let read_dir = match fs::read_dir(&current) {
            Ok(iter) => iter,
            Err(err) => return TreeFreshness::Unreadable(err),
        };
        for entry_result in read_dir {
            entries_visited_count = entries_visited_count.saturating_add(1);
            if entries_visited_count > TREE_WALK_ENTRIES_MAX {
                return TreeFreshness::LimitExceeded;
            }
            // Fail closed: disappearing/unreadable child could belong to an active build.
            let entry = match entry_result {
                Ok(val) => val,
                Err(err) => return TreeFreshness::Unreadable(err),
            };
            let entry_path = entry.path();
            let entry_meta = match fs::symlink_metadata(&entry_path) {
                Ok(meta) => meta,
                Err(err) => return TreeFreshness::Unreadable(err),
            };
            let entry_mtime_ns = metadata_mtime_ns(&entry_meta);

            if entry_mtime_ns > cutoff_ns {
                return TreeFreshness::Recent;
            }
            if entry_mtime_ns > newest_ns {
                newest_ns = entry_mtime_ns;
            }
            if entry_meta.file_type().is_dir() {
                let is_skipped_child = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| TARGET_SKIP_MTIME_CHILDREN.contains(&name));
                if !is_skipped_child && depth + 1 < depth_max {
                    stack.push((entry_path, depth + 1));
                }
            }
        }
    }

    assert!(newest_ns <= cutoff_ns, "returned mtime must satisfy cutoff");
    TreeFreshness::Stale(newest_ns)
}

/// Single-pass mtime + disk-usage inspection for a `/tmp/nix-*` tree.
///
/// Fails closed and distinguishes recent modifications from unreadable entries,
/// type changes, and walk depth/entry limits.
pub fn inspect_tmp_tree(root: &NonRootAbsPath, cutoff_ns: i128) -> TreeFreshness<TmpTreeSummary> {
    let root_meta = match fs::symlink_metadata(root.as_path()) {
        Ok(meta) => meta,
        Err(err) => return TreeFreshness::Unreadable(err),
    };
    if !root_meta.file_type().is_dir() {
        return TreeFreshness::Unreadable(io::Error::new(
            io::ErrorKind::InvalidInput,
            "entry is not a directory",
        ));
    }
    let mut newest_ns = metadata_mtime_ns(&root_meta);
    if newest_ns > cutoff_ns {
        return TreeFreshness::Recent;
    }

    let mut total_bytes: u64 = root_meta.blocks().saturating_mul(512);
    let mut seen_inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut entries_visited_count: usize = 0;
    let mut stack: Vec<(PathBuf, usize)> = Vec::with_capacity(WALK_DEPTH_MAX);
    stack.push((root.as_path().to_path_buf(), 0));

    let mut dirs_popped_count: usize = 0;
    while let Some((current, depth)) = stack.pop() {
        dirs_popped_count = dirs_popped_count.saturating_add(1);
        if dirs_popped_count > TREE_WALK_ENTRIES_MAX {
            return TreeFreshness::LimitExceeded;
        }
        if depth >= WALK_DEPTH_MAX {
            return TreeFreshness::LimitExceeded;
        }
        let read_dir = match fs::read_dir(&current) {
            Ok(iter) => iter,
            Err(err) => return TreeFreshness::Unreadable(err),
        };
        for entry_result in read_dir {
            entries_visited_count = entries_visited_count.saturating_add(1);
            if entries_visited_count > TREE_WALK_ENTRIES_MAX {
                return TreeFreshness::LimitExceeded;
            }
            let entry = match entry_result {
                Ok(val) => val,
                Err(err) => return TreeFreshness::Unreadable(err),
            };
            let entry_path = entry.path();
            let entry_meta = match fs::symlink_metadata(&entry_path) {
                Ok(meta) => meta,
                Err(err) => return TreeFreshness::Unreadable(err),
            };
            let entry_mtime_ns = metadata_mtime_ns(&entry_meta);
            if entry_mtime_ns > cutoff_ns {
                return TreeFreshness::Recent;
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
    TreeFreshness::Stale(TmpTreeSummary {
        mtime_latest_ns: newest_ns,
        size_bytes: total_bytes,
    })
}

/// Walk `projects_dir` once to discover stale Nix `result*` links and Rust `target/` dirs.
pub fn scan_projects(
    projects_dir: &SafeProjectsDir,
    config: &ProjectScanConfig,
) -> ProjectScanResult {
    let mut result = ProjectScanResult::default();
    if !projects_dir.as_path().is_dir() {
        return result;
    }

    let result_mtime_max_ns = config.result_mtime_max_ns();
    let target_mtime_max_ns = config.target_mtime_max_ns();

    let mut entries_visited_count: usize = 0;
    let mut candidates_limit_reported = false;
    let mut stack: Vec<(AbsPath, usize)> = Vec::with_capacity(WALK_DEPTH_MAX);
    stack.push((projects_dir.as_abs().clone(), 0));

    let mut dirs_popped_count: usize = 0;
    while let Some((current, depth)) = stack.pop() {
        dirs_popped_count = dirs_popped_count.saturating_add(1);
        if dirs_popped_count > PROJECTS_WALK_ENTRIES_MAX {
            result.errors.push_truncated(format!(
                "scan {}: exceeded {PROJECTS_WALK_ENTRIES_MAX} entries limit",
                projects_dir.display()
            ));
            break;
        }
        if depth >= WALK_DEPTH_MAX {
            continue;
        }

        let read_dir = match fs::read_dir(current.as_path()) {
            Ok(iter) => iter,
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                // Unreadable subtrees (e.g. Docker bind-mounted data dirs) are not
                // cleanup errors; skip them.
                continue;
            }
            Err(err) => {
                result
                    .errors
                    .push_truncated(format!("scan {}: {err}", current.display()));
                continue;
            }
        };

        for entry_result in read_dir {
            entries_visited_count = entries_visited_count.saturating_add(1);
            if entries_visited_count > PROJECTS_WALK_ENTRIES_MAX {
                result.errors.push_truncated(format!(
                    "scan {}: exceeded {PROJECTS_WALK_ENTRIES_MAX} entries limit",
                    projects_dir.display()
                ));
                stack.clear();
                break;
            }

            let Ok(entry) = entry_result else {
                continue;
            };
            let file_name_os = entry.file_name();
            let Ok(entry_name) = EntryName::try_from_os_str(&file_name_os) else {
                continue;
            };
            let Ok(entry_path) = current.join_entry(&entry_name) else {
                continue;
            };
            let Ok(entry_meta) = fs::symlink_metadata(entry_path.as_path()) else {
                continue;
            };
            let file_name_lossy = file_name_os.to_string_lossy();
            let file_type = entry_meta.file_type();

            if file_type.is_symlink() {
                if (file_name_lossy == "result" || file_name_lossy.starts_with("result-"))
                    && let Ok(link_target) = fs::read_link(entry_path.as_path())
                {
                    let mtime_ns = metadata_mtime_ns(&entry_meta);
                    let target_bytes = link_target.as_os_str().as_bytes();
                    let is_stale_nix_link = target_bytes.len() > b"/nix/store/".len()
                        && target_bytes.starts_with(b"/nix/store/")
                        && mtime_ns <= result_mtime_max_ns;
                    if is_stale_nix_link {
                        if result.stale_results.len() < CANDIDATES_MAX {
                            result.stale_results.push((entry_path, mtime_ns));
                        } else if !candidates_limit_reported {
                            candidates_limit_reported = true;
                            result.errors.push_truncated(format!(
                                "scan {}: exceeded {CANDIDATES_MAX} candidate limit",
                                projects_dir.display()
                            ));
                        }
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
                    match probe_cargo_target_lock(&entry_path) {
                        CargoLockProbe::NoLockFile | CargoLockProbe::Acquired(_) => {
                            match latest_target_mtime_ns(
                                &entry_path,
                                target_mtime_max_ns,
                                TARGET_MTIME_DEPTH_MAX,
                            ) {
                                TreeFreshness::Stale(mtime_latest_ns) => {
                                    if result.target_candidates.len() < CANDIDATES_MAX {
                                        let size_bytes = dir_disk_usage(&entry_path);
                                        result.target_candidates.push(TargetCandidate {
                                            path: entry_path,
                                            mtime_latest_ns,
                                            size_bytes,
                                        });
                                    } else if !candidates_limit_reported {
                                        candidates_limit_reported = true;
                                        result.errors.push_truncated(format!(
                                            "scan {}: exceeded {CANDIDATES_MAX} candidate limit",
                                            projects_dir.display()
                                        ));
                                    }
                                }
                                TreeFreshness::Recent => {
                                    result.targets_skipped_recent_count += 1;
                                }
                                TreeFreshness::Unreadable(err) => {
                                    result.targets_skipped_unreadable_count += 1;
                                    log(&format!(
                                        "warning: skipped Rust target/ (unreadable): {} ({err})",
                                        entry_path.display()
                                    ));
                                }
                                TreeFreshness::LimitExceeded => {
                                    result.targets_skipped_unreadable_count += 1;
                                    log(&format!(
                                        "warning: skipped Rust target/ (walk limit exceeded): {}",
                                        entry_path.display()
                                    ));
                                }
                            }
                        }
                        CargoLockProbe::Locked
                        | CargoLockProbe::SymlinkOrUnreadable(_)
                        | CargoLockProbe::UnexpectedError(_) => {
                            result.targets_skipped_active_count += 1;
                        }
                    }
                }
                // Never descend into any `target` directory.
                continue;
            }

            if depth + 1 < WALK_DEPTH_MAX {
                stack.push((entry_path.as_abs().clone(), depth + 1));
            }
        }
    }

    result.stale_results.sort_by_key(|&(_, mtime_ns)| mtime_ns);
    result
        .target_candidates
        .sort_by_key(|candidate| candidate.mtime_latest_ns);

    assert!(result.stale_results.len() <= CANDIDATES_MAX);
    assert!(result.target_candidates.len() <= CANDIDATES_MAX);
    assert!(result.errors.len() <= ERRORS_MAX);
    result
}

/// Pure discovery of stale `/tmp/nix-*` directories using single-pass inspection.
pub fn scan_stale_nix_tmp_dirs(
    tmp_dir: &NonRootAbsPath,
    tmp_age: AgeDurationNs,
    now_ns: TimestampNs,
    owner_filter_uid: Option<u32>,
) -> TmpScanResult {
    let mut result = TmpScanResult::default();
    if !tmp_dir.as_path().is_dir() {
        return result;
    }

    let cutoff_ns = tmp_age.cutoff_before(now_ns);

    let read_dir = match fs::read_dir(tmp_dir.as_path()) {
        Ok(iter) => iter,
        Err(err) => {
            result
                .errors
                .push_truncated(format!("scan {}: {err}", tmp_dir.display()));
            return result;
        }
    };

    let mut entries_seen_count: usize = 0;
    for entry_result in read_dir {
        entries_seen_count = entries_seen_count.saturating_add(1);
        if entries_seen_count > TMP_DIR_ENTRIES_MAX {
            result.errors.push_truncated(format!(
                "scan {}: exceeded {TMP_DIR_ENTRIES_MAX} entries limit",
                tmp_dir.display()
            ));
            break;
        }

        let Ok(entry) = entry_result else {
            continue;
        };
        let Ok(entry_name) = EntryName::try_from_os_str(&entry.file_name()) else {
            continue;
        };
        if !is_nix_tmp_dir_name(&entry_name) {
            continue;
        }

        let Ok(entry_path) = tmp_dir.join_entry(&entry_name) else {
            continue;
        };
        let Ok(entry_meta) = fs::symlink_metadata(entry_path.as_path()) else {
            continue;
        };
        if !entry_meta.file_type().is_dir() {
            continue;
        }
        assert!(
            !entry_meta.file_type().is_symlink(),
            "is_dir on symlink_metadata must exclude symlinks"
        );
        if let Some(required_uid) = owner_filter_uid
            && entry_meta.uid() != required_uid
        {
            continue;
        }

        if let Some(pid) = parse_nix_develop_pid(&entry_name)
            && sys::is_pid_alive(pid)
        {
            result.dirs_skipped_active_count += 1;
            continue;
        }

        match inspect_tmp_tree(&entry_path, cutoff_ns) {
            TreeFreshness::Stale(tree) => {
                if result.stale_dirs.len() >= CANDIDATES_MAX {
                    result.errors.push_truncated(format!(
                        "scan {}: exceeded {CANDIDATES_MAX} candidate limit",
                        tmp_dir.display()
                    ));
                    break;
                }
                result.stale_dirs.push((entry_path, tree.size_bytes));
            }
            TreeFreshness::Recent => {
                result.dirs_skipped_recent_count += 1;
            }
            TreeFreshness::Unreadable(err) => {
                result.dirs_skipped_unreadable_count += 1;
                log(&format!(
                    "warning: skipped {} (unreadable): {err}",
                    entry_path.display()
                ));
            }
            TreeFreshness::LimitExceeded => {
                result.dirs_skipped_unreadable_count += 1;
                log(&format!(
                    "warning: skipped {} (walk depth or entry limit exceeded)",
                    entry_path.display()
                ));
            }
        }
    }

    assert!(result.stale_dirs.len() <= CANDIDATES_MAX);
    assert!(result.errors.len() <= ERRORS_MAX);
    result
}

/// Return existing per-user Nix profile paths to prune generations on.
pub fn user_nix_profiles(user_name: &UserName, user_home: &AbsPath) -> Vec<NonRootAbsPath> {
    let mut candidates = Vec::with_capacity(2);
    if let Ok(xdg_profile) = user_home.join_relative(Path::new(".local/state/nix/profiles/profile"))
        && let Ok(non_root) = NonRootAbsPath::try_from_abs(xdg_profile)
    {
        candidates.push(non_root);
    }
    let per_user_raw = Path::new("/nix/var/nix/profiles/per-user")
        .join(user_name.as_str())
        .join("profile");
    if let Ok(non_root) = NonRootAbsPath::from_absolute(&per_user_raw) {
        candidates.push(non_root);
    }

    let mut profiles = Vec::with_capacity(candidates.len());
    let mut seen: HashSet<PathBuf> = HashSet::with_capacity(candidates.len());

    for path in candidates {
        // `symlink_metadata` succeeds for both existing targets and dangling symlinks.
        if fs::symlink_metadata(path.as_path()).is_err() {
            continue;
        }
        let parent = path.parent();
        let file_name = path.file_name();
        let resolved = match fs::canonicalize(parent.as_path()) {
            Ok(canon_parent) => canon_parent.join(file_name.as_os_str()),
            Err(_) => path.as_path().to_path_buf(),
        };
        if seen.insert(resolved) {
            profiles.push(path);
        }
    }

    profiles
}

pub fn is_nix_tmp_dir_name(name: &EntryName) -> bool {
    let Some(text) = name.to_str() else {
        return false;
    };
    NIX_TMP_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

/// Extract non-zero PID from `nix-develop-<pid>-...` directory names.
pub fn parse_nix_develop_pid(name: &EntryName) -> Option<NonZeroU32> {
    let text = name.to_str()?;
    let rest = text.strip_prefix("nix-develop-")?;
    let dash_index = rest.find('-')?;
    let digits = rest.get(..dash_index)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let pid = digits.parse::<u32>().ok()?;
    NonZeroU32::new(pid)
}
