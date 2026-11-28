//! Phase execution (user phase, `/tmp/nix-*`, Nix GC), notifications, and formatting.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::{Duration, Instant};

use crate::constants::{
    CANDIDATES_MAX, DEAD_PATHS_COUNT_MAX, ENV_VARS_MAX, NANOS_PER_SECOND_U128,
    NIX_ENV_LOG_LINES_MAX, NIX_ENV_TIMEOUT_SECONDS, NOTIFY_FREED_BYTES_MIN, NOTIFY_TIMEOUT_SECONDS,
    SECONDS_PER_DAY, SECONDS_PER_HOUR, SUBPROCESS_LINE_BYTES_MAX, SUBPROCESS_OUTPUT_BYTES_MAX,
    SUBPROCESS_STDERR_BYTES_MAX, TARGET_MTIME_DEPTH_MAX, USER_PHASE_TIMEOUT_SECONDS,
    USER_PROFILES_MAX,
};
use crate::ipc::{self, UserPhaseStats};
use crate::process::{
    self, CommandOptions, CommandOutcome, PrivilegeDrop, ProcessExit, StderrConfig, StdoutCapture,
    StdoutConfig,
};
use crate::scan::{
    DiskUsage, ProjectScanConfig, ProjectScanResult, TreeFreshness, inspect_tmp_tree,
    is_cargo_target_dir, latest_target_mtime_ns, parse_nix_develop_pid, probe_cargo_target_lock,
    scan_projects, scan_stale_nix_tmp_dirs, user_nix_profiles,
};
use crate::sys::{self, CargoLockProbe, ExecutionPrivilege, UnlinkStaleLinkOutcome};
use crate::types::{
    AbsPath, AgeDurationNs, BoundedErrors, CleanupMode, NixGcAgeDays, NonRootAbsPath, RemovedBatch,
    SafeProjectsDir, ThresholdPercent, TimestampNs, UserName,
};

/// `2^64` in `f64` (exact); any non-negative `f64` strictly below this fits in `u64`.
const U64_UPPER_BOUND_F64: f64 = 18_446_744_073_709_551_616.0;

const DEAD_PATHS_STREAM_LINES_MAX: NonZeroU64 =
    NonZeroU64::new(DEAD_PATHS_COUNT_MAX).expect("DEAD_PATHS_COUNT_MAX > 0");
const NOTIFY_STREAM_LINES_MAX: NonZeroU64 = NonZeroU64::new(64).expect("64 > 0");
const IPC_STDOUT_BYTES_MAX: NonZeroUsize =
    NonZeroUsize::new(SUBPROCESS_OUTPUT_BYTES_MAX).expect("SUBPROCESS_OUTPUT_BYTES_MAX > 0");

/// Distinct outcome of Phase 2b (`nix-store --gc --print-dead` or `nix-collect-garbage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NixGcOutcome {
    DryRun {
        dead_paths_count: u64,
        errors: BoundedErrors,
    },
    Executed {
        pruned_system_profiles: bool,
        reclaimed: RemovedBatch,
        errors: BoundedErrors,
    },
    Skipped {
        errors: BoundedErrors,
    },
}

impl NixGcOutcome {
    pub fn errors(&self) -> &BoundedErrors {
        match self {
            Self::DryRun { errors, .. }
            | Self::Executed { errors, .. }
            | Self::Skipped { errors } => errors,
        }
    }

    pub const fn reclaimed(&self) -> RemovedBatch {
        match self {
            Self::Executed { reclaimed, .. } => *reclaimed,
            Self::DryRun { .. } | Self::Skipped { .. } => RemovedBatch::None,
        }
    }

    pub const fn dead_paths_count(&self) -> u64 {
        match self {
            Self::DryRun {
                dead_paths_count, ..
            } => *dead_paths_count,
            Self::Executed { .. } | Self::Skipped { .. } => 0,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TmpCleanupStats {
    pub dirs_removed: RemovedBatch,
    pub dirs_skipped_active_count: u64,
    pub dirs_skipped_recent_count: u64,
    pub dirs_skipped_unreadable_count: u64,
    pub errors: BoundedErrors,
}

#[derive(Debug, Clone)]
pub struct UserPhaseConfig<'a> {
    pub projects_dir: &'a SafeProjectsDir,
    pub result_age: AgeDurationNs,
    pub target_age: AgeDurationNs,
    pub nix_gc_age_days: NixGcAgeDays,
    pub mode: CleanupMode,
    pub now_ns: TimestampNs,
    pub deadline: Instant,
}

/// Verified pre-delete cutoffs guaranteed to satisfy `cutoff <= now_ns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserCleanupCutoffs {
    now_ns: TimestampNs,
    result_mtime_max_ns: i128,
    target_mtime_max_ns: i128,
}

impl UserCleanupCutoffs {
    pub fn new(now_ns: TimestampNs, result_age: AgeDurationNs, target_age: AgeDurationNs) -> Self {
        Self {
            now_ns,
            result_mtime_max_ns: result_age.cutoff_before(now_ns),
            target_mtime_max_ns: target_age.cutoff_before(now_ns),
        }
    }

    pub const fn now_ns(self) -> TimestampNs {
        self.now_ns
    }

    pub const fn result_mtime_max_ns(self) -> i128 {
        self.result_mtime_max_ns
    }

    pub const fn target_mtime_max_ns(self) -> i128 {
        self.target_mtime_max_ns
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerDecision {
    SkipBelowThreshold,
    Run { trigger_description: String },
}

/// Validated non-empty notification payload produced only by `build_notification`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationMessage {
    title: String,
    body: String,
}

impl NotificationMessage {
    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn body(&self) -> &str {
        &self.body
    }
}

/// Closed set of recognized Nix GC size units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeUnit {
    B,
    KB,
    KiB,
    MB,
    MiB,
    GB,
    GiB,
    TB,
    TiB,
}

impl SizeUnit {
    fn parse(raw: &str) -> Option<Self> {
        if raw.eq_ignore_ascii_case("B") {
            Some(Self::B)
        } else if raw.eq_ignore_ascii_case("KB") {
            Some(Self::KB)
        } else if raw.eq_ignore_ascii_case("KIB") {
            Some(Self::KiB)
        } else if raw.eq_ignore_ascii_case("MB") {
            Some(Self::MB)
        } else if raw.eq_ignore_ascii_case("MIB") {
            Some(Self::MiB)
        } else if raw.eq_ignore_ascii_case("GB") {
            Some(Self::GB)
        } else if raw.eq_ignore_ascii_case("GIB") {
            Some(Self::GiB)
        } else if raw.eq_ignore_ascii_case("TB") {
            Some(Self::TB)
        } else if raw.eq_ignore_ascii_case("TIB") {
            Some(Self::TiB)
        } else {
            None
        }
    }

    const fn multiplier(self) -> u64 {
        match self {
            Self::B => 1,
            Self::KB => 1_000,
            Self::KiB => 1_024,
            Self::MB => 1_000_u64.pow(2),
            Self::MiB => 1_024_u64.pow(2),
            Self::GB => 1_000_u64.pow(3),
            Self::GiB => 1_024_u64.pow(3),
            Self::TB => 1_000_u64.pow(4),
            Self::TiB => 1_024_u64.pow(4),
        }
    }
}

/// Compute remaining wall-clock budget clamped to `cap`, or `None` if exhausted.
pub fn remaining_budget(deadline: Instant, cap: Duration) -> Option<Duration> {
    assert!(!cap.is_zero(), "cap duration must be positive");
    let now = Instant::now();
    if now >= deadline {
        return None;
    }
    let remaining = deadline.saturating_duration_since(now);
    let clamped = remaining.min(cap);
    if clamped.is_zero() {
        None
    } else {
        assert!(clamped <= cap);
        Some(clamped)
    }
}

/// Emit a log line to stderr; add local timestamps when not running under journald.
pub fn log(message: &str) {
    assert!(!message.is_empty(), "log message must be non-empty");
    let under_journald = env::var_os("JOURNAL_STREAM").is_some_and(|val| !val.is_empty());
    if under_journald {
        eprintln!("disk-cleanup: {message}");
    } else {
        let timestamp = sys::format_local_timestamp();
        eprintln!("{timestamp} disk-cleanup: {message}");
    }
}

/// Decide whether disk usage triggers cleanup or should exit quietly.
pub fn decide_trigger(
    before: &DiskUsage,
    threshold: ThresholdPercent,
    force: bool,
) -> TriggerDecision {
    if before.used_percent.is_below(threshold) {
        if force {
            TriggerDecision::Run {
                trigger_description: format!("forced at {}%", before.used_percent),
            }
        } else {
            TriggerDecision::SkipBelowThreshold
        }
    } else {
        TriggerDecision::Run {
            trigger_description: format!("triggered at {}% >= {threshold}%", before.used_percent),
        }
    }
}

/// Build the desktop notification payload if at least `NOTIFY_FREED_BYTES_MIN` was freed.
pub fn build_notification(
    before: &DiskUsage,
    after: &DiskUsage,
    tracked_freed_bytes: u64,
    summary_parts: &[String],
) -> Option<NotificationMessage> {
    let net_freed_bytes = after.available_bytes.saturating_sub(before.available_bytes);
    let reported_freed_bytes = if net_freed_bytes >= NOTIFY_FREED_BYTES_MIN {
        net_freed_bytes
    } else if tracked_freed_bytes >= NOTIFY_FREED_BYTES_MIN {
        tracked_freed_bytes
    } else {
        0
    };

    if reported_freed_bytes < NOTIFY_FREED_BYTES_MIN {
        return None;
    }

    let title = format!("Disk cleanup freed {}", format_bytes(reported_freed_bytes));
    let mut body = format!(
        "Disk: {}% → {}% ({} free)",
        before.used_percent,
        after.used_percent,
        format_bytes(after.available_bytes),
    );
    if !summary_parts.is_empty() {
        body.push_str("\nCleaned: ");
        body.push_str(&summary_parts.join(", "));
    }
    Some(NotificationMessage { title, body })
}

/// Run Phase 1 directly or in a privilege-dropped subprocess when invoked as root
/// targeting a non-root user.
pub fn run_user_phase(
    privilege: &ExecutionPrivilege,
    config: &UserPhaseConfig<'_>,
) -> UserPhaseStats {
    let Some(target) = privilege.drop_target() else {
        return run_user_phase_in_process(privilege.target_name(), privilege.target_home(), config);
    };

    let Some(timeout) = remaining_budget(
        config.deadline,
        Duration::from_secs(USER_PHASE_TIMEOUT_SECONDS),
    ) else {
        let mut stats = UserPhaseStats::new(config.mode);
        stats
            .errors
            .push_truncated("user phase skipped: total run timeout exhausted");
        return stats;
    };

    let current_exe = match env::current_exe().and_then(|p| AbsPath::from_absolute(&p)) {
        Ok(path) => path,
        Err(err) => {
            let mut stats = UserPhaseStats::new(config.mode);
            stats
                .errors
                .push_truncated(format!("user phase failed to locate executable: {err}"));
            return stats;
        }
    };

    let mut args: Vec<OsString> = vec![
        OsString::from("--_user-phase"),
        OsString::from("--user"),
        OsString::from(target.name.as_str()),
        OsString::from("--projects-dir"),
        config.projects_dir.as_os_str().to_os_string(),
        OsString::from("--_result-age-ns"),
        OsString::from(format!("{}", config.result_age.as_ns())),
        OsString::from("--_target-age-ns"),
        OsString::from(format!("{}", config.target_age.as_ns())),
        OsString::from("--nix-gc-age-days"),
        OsString::from(format!("{}", config.nix_gc_age_days)),
        OsString::from("--_now-ns"),
        OsString::from(format!("{}", config.now_ns)),
    ];
    if config.mode.is_dry_run() {
        args.push(OsString::from("--dry-run"));
    }

    let mut env_pairs: Vec<(OsString, OsString)> = vec![
        (
            OsString::from("PATH"),
            env::var_os("PATH").unwrap_or_default(),
        ),
        (
            OsString::from("HOME"),
            target.home_dir.as_os_str().to_os_string(),
        ),
        (OsString::from("USER"), OsString::from(target.name.as_str())),
        (
            OsString::from("LOGNAME"),
            OsString::from(target.name.as_str()),
        ),
    ];
    for key in [
        "NIX_REMOTE",
        "NIX_CONF_DIR",
        "JOURNAL_STREAM",
        "TZ",
        "TZDIR",
        "LOCALE_ARCHIVE",
    ] {
        if let Some(val) = env::var_os(key) {
            env_pairs.push((OsString::from(key), val));
        }
    }

    let extra_groups = match sys::user_groups(&target.name, target.gid) {
        Ok(groups) => groups,
        Err(err) => {
            log(&format!(
                "warning: getgrouplist failed for {}: {err}; using primary GID",
                target.name
            ));
            vec![target.gid]
        }
    };
    let privilege_drop = match PrivilegeDrop::for_target(target, extra_groups) {
        Ok(drop_cfg) => drop_cfg,
        Err(err) => {
            let mut stats = UserPhaseStats::new(config.mode);
            stats
                .errors
                .push_truncated(format!("user phase privilege drop setup failed: {err}"));
            return stats;
        }
    };

    let options = CommandOptions {
        program: current_exe.as_os_str().to_os_string(),
        args,
        timeout,
        env_override: Some(env_pairs),
        privilege_drop: Some(privilege_drop),
        stream_lines_max: DEAD_PATHS_STREAM_LINES_MAX,
    };

    let outcome = process::run_command_streaming::<fn(&str), fn(&str)>(
        &options,
        StdoutConfig::Capture {
            bytes_max: IPC_STDOUT_BYTES_MAX,
            on_line: None,
        },
        StderrConfig::Inherit,
    );

    match outcome {
        CommandOutcome::Completed {
            exit: ProcessExit::Success,
            stdout: StdoutCapture::Complete { text, .. },
            ..
        } => match ipc::parse_user_phase_stats_json(&text, config.mode) {
            Ok(stats) => stats,
            Err(err) => {
                let mut stats = UserPhaseStats::new(config.mode);
                stats
                    .errors
                    .push_truncated(format!("failed to parse user phase output: {err}"));
                stats
            }
        },
        CommandOutcome::Completed {
            exit: ProcessExit::Success,
            stdout: StdoutCapture::Truncated { .. },
            ..
        } => {
            let mut stats = UserPhaseStats::new(config.mode);
            stats
                .errors
                .push_truncated("user phase IPC output exceeded byte limit");
            stats
        }
        CommandOutcome::Completed {
            exit: ProcessExit::Success,
            stdout: StdoutCapture::NotCaptured { .. },
            ..
        } => unreachable!("run_user_phase requested StdoutConfig::Capture"),
        other => {
            let detail = other
                .failure_detail()
                .expect("non-success CommandOutcome always has failure_detail");
            let mut stats = UserPhaseStats::new(config.mode);
            stats
                .errors
                .push_truncated(format!("user phase failed: {detail}"));
            stats
        }
    }
}

/// Discover and clean stale Nix `result*` links, Rust `target/` dirs, and user profiles.
pub fn run_user_phase_in_process(
    user_name: &UserName,
    user_home: &AbsPath,
    config: &UserPhaseConfig<'_>,
) -> UserPhaseStats {
    let scan_config = ProjectScanConfig {
        now_ns: config.now_ns,
        result_age: config.result_age,
        target_age: config.target_age,
    };
    let scan = scan_projects(config.projects_dir, &scan_config);
    let profiles = user_nix_profiles(user_name, user_home);

    match config.mode {
        CleanupMode::DryRun => {
            log_user_dry_run(&scan, &profiles, config.nix_gc_age_days, config.now_ns)
        }
        CleanupMode::Apply => {
            let cutoffs =
                UserCleanupCutoffs::new(config.now_ns, config.result_age, config.target_age);
            apply_user_cleanup(
                config.projects_dir,
                &scan,
                &profiles,
                config.nix_gc_age_days,
                cutoffs,
                config.deadline,
            )
        }
    }
}

/// Report what Phase 1 would remove without mutating the filesystem.
pub fn log_user_dry_run(
    scan: &ProjectScanResult,
    profiles: &[NonRootAbsPath],
    nix_gc_age_days: NixGcAgeDays,
    now_ns: TimestampNs,
) -> UserPhaseStats {
    assert!(scan.stale_results.len() <= CANDIDATES_MAX);
    assert!(scan.target_candidates.len() <= CANDIDATES_MAX);
    assert!(profiles.len() <= USER_PROFILES_MAX);

    let mut stats = UserPhaseStats::new(CleanupMode::DryRun);
    stats.targets_skipped_active_count = scan.targets_skipped_active_count;
    stats.targets_skipped_recent_count = scan.targets_skipped_recent_count;
    stats.targets_skipped_unreadable_count = scan.targets_skipped_unreadable_count;
    stats.errors.extend_from(&scan.errors);

    for (link_path, mtime_ns) in &scan.stale_results {
        let age = format_age(now_ns.age_since(*mtime_ns));
        log(&format!(
            "[dry-run] would remove stale Nix result symlink: {} (age {age})",
            link_path.display()
        ));
        stats.result_links_removed_count += 1;
    }

    for candidate in &scan.target_candidates {
        let age = format_age(now_ns.age_since(candidate.mtime_latest_ns));
        let size_text = format_bytes(candidate.size_bytes);
        log(&format!(
            "[dry-run] would remove Rust target/: {} ({size_text}, idle {age})",
            candidate.path.display()
        ));
        stats.targets_removed.record(candidate.size_bytes);
    }

    if !profiles.is_empty() {
        if process::find_executable_in_path("nix-env").is_none() {
            stats.errors.push_truncated("nix-env not found on PATH");
        } else {
            for profile in profiles {
                log(&format!(
                    "[dry-run] would prune Nix profile generations older than {nix_gc_age_days}d: {}",
                    profile.display()
                ));
                stats.user_profiles_pruned_count += 1;
            }
        }
    }

    stats
}

/// Apply Phase 1 deletions after re-verifying safety preconditions at use time.
pub fn apply_user_cleanup(
    projects_dir: &SafeProjectsDir,
    scan: &ProjectScanResult,
    profiles: &[NonRootAbsPath],
    nix_gc_age_days: NixGcAgeDays,
    cutoffs: UserCleanupCutoffs,
    deadline: Instant,
) -> UserPhaseStats {
    assert!(scan.stale_results.len() <= CANDIDATES_MAX);
    assert!(scan.target_candidates.len() <= CANDIDATES_MAX);
    assert!(profiles.len() <= USER_PROFILES_MAX);

    let mut stats = UserPhaseStats::new(CleanupMode::Apply);
    stats.targets_skipped_active_count = scan.targets_skipped_active_count;
    stats.targets_skipped_recent_count = scan.targets_skipped_recent_count;
    stats.targets_skipped_unreadable_count = scan.targets_skipped_unreadable_count;
    stats.errors.extend_from(&scan.errors);

    for (link_path, _scanned_mtime_ns) in &scan.stale_results {
        match sys::verify_and_unlink_stale_nix_symlink(
            projects_dir.as_abs(),
            link_path,
            cutoffs.result_mtime_max_ns(),
        ) {
            Ok(UnlinkStaleLinkOutcome::Removed { mtime_ns }) => {
                let age = format_age(cutoffs.now_ns().age_since(mtime_ns));
                log(&format!(
                    "removed stale Nix result symlink: {} (age {age})",
                    link_path.display()
                ));
                stats.result_links_removed_count += 1;
            }
            Ok(UnlinkStaleLinkOutcome::SkippedFresh) => {
                log(&format!(
                    "skipped Nix result symlink (replaced during scan): {}",
                    link_path.display()
                ));
            }
            Ok(
                UnlinkStaleLinkOutcome::SkippedNotSymlink
                | UnlinkStaleLinkOutcome::SkippedNonNixTarget,
            ) => {}
            Err(err) => {
                stats
                    .errors
                    .push_truncated(format!("unlink {}: {err}", link_path.display()));
            }
        }
    }

    for candidate in &scan.target_candidates {
        // Acquire and HOLD `.cargo-lock` across marker re-verification, mtime
        // re-check, and `safe_rmtree_under` so a newly starting Cargo build
        // blocks on `.cargo-lock` instead of racing with deletion.
        let _held_cargo_lock = match probe_cargo_target_lock(&candidate.path) {
            CargoLockProbe::NoLockFile => None,
            CargoLockProbe::Acquired(guard) => Some(guard),
            CargoLockProbe::Locked
            | CargoLockProbe::SymlinkOrUnreadable(_)
            | CargoLockProbe::UnexpectedError(_) => {
                stats.targets_skipped_active_count += 1;
                log(&format!(
                    "skipped Rust target/ (build started or lock unreadable): {}",
                    candidate.path.display()
                ));
                continue;
            }
        };

        if !is_cargo_target_dir(&candidate.path) {
            log(&format!(
                "skipped Rust target/ (no longer a Cargo target): {}",
                candidate.path.display()
            ));
            continue;
        }

        let refreshed_mtime_ns = match latest_target_mtime_ns(
            &candidate.path,
            cutoffs.target_mtime_max_ns(),
            TARGET_MTIME_DEPTH_MAX,
        ) {
            TreeFreshness::Stale(mtime_ns) => mtime_ns,
            TreeFreshness::Recent => {
                stats.targets_skipped_recent_count += 1;
                log(&format!(
                    "skipped Rust target/ (modified during scan): {}",
                    candidate.path.display()
                ));
                continue;
            }
            TreeFreshness::Unreadable(err) => {
                stats.targets_skipped_unreadable_count += 1;
                log(&format!(
                    "warning: skipped Rust target/ (unreadable during re-check): {} ({err})",
                    candidate.path.display()
                ));
                continue;
            }
            TreeFreshness::LimitExceeded => {
                stats.targets_skipped_unreadable_count += 1;
                log(&format!(
                    "warning: skipped Rust target/ (walk limit exceeded during re-check): {}",
                    candidate.path.display()
                ));
                continue;
            }
        };

        let age = format_age(cutoffs.now_ns().age_since(refreshed_mtime_ns));
        let size_text = format_bytes(candidate.size_bytes);
        match sys::safe_rmtree_under(projects_dir.as_abs(), &candidate.path) {
            Ok(()) => {
                log(&format!(
                    "removed Rust target/: {} ({size_text}, idle {age})",
                    candidate.path.display()
                ));
                stats.targets_removed.record(candidate.size_bytes);
            }
            Err(err) => {
                stats
                    .errors
                    .push_truncated(format!("rmtree {}: {err}", candidate.path.display()));
            }
        }
    }

    if !profiles.is_empty() {
        let Some(nix_env) = process::find_executable_in_path("nix-env") else {
            stats.errors.push_truncated("nix-env not found on PATH");
            return stats;
        };

        for profile in profiles {
            let Some(timeout) =
                remaining_budget(deadline, Duration::from_secs(NIX_ENV_TIMEOUT_SECONDS))
            else {
                stats.errors.push_truncated(format!(
                    "nix-env {}: timeout budget exhausted",
                    profile.display()
                ));
                break;
            };

            let profile_name = profile.file_name();
            let mut logged_lines_count: usize = 0;

            let options = CommandOptions {
                program: nix_env.as_os_str().to_os_string(),
                args: vec![
                    OsString::from("--profile"),
                    profile.as_os_str().to_os_string(),
                    OsString::from("--delete-generations"),
                    OsString::from(format!("{nix_gc_age_days}d")),
                ],
                timeout,
                env_override: None,
                privilege_drop: None,
                stream_lines_max: DEAD_PATHS_STREAM_LINES_MAX,
            };

            let outcome = process::run_command_streaming(
                &options,
                StdoutConfig::StreamOnly {
                    on_line: Some(|line: &str| {
                        let stripped = line.trim();
                        if !stripped.is_empty() && logged_lines_count < NIX_ENV_LOG_LINES_MAX {
                            log(&format!("nix-env ({profile_name}): {stripped}"));
                            logged_lines_count += 1;
                        }
                    }),
                },
                StderrConfig::<fn(&str)>::Piped {
                    tail_bytes_max: SUBPROCESS_STDERR_BYTES_MAX,
                    on_line: None,
                },
            );

            if let Some(detail) = outcome.failure_detail() {
                stats
                    .errors
                    .push_truncated(format!("nix-env {}: {detail}", profile.display()));
            } else {
                stats.user_profiles_pruned_count += 1;
            }
        }
    }

    stats
}

/// Remove stale `/tmp/nix-*` shell and build directories (Linux only).
pub fn clean_nix_tmp_dirs(
    tmp_dir: &NonRootAbsPath,
    tmp_age: AgeDurationNs,
    mode: CleanupMode,
    now_ns: TimestampNs,
    owner_filter_uid: Option<u32>,
) -> TmpCleanupStats {
    let cutoff_ns = tmp_age.cutoff_before(now_ns);
    let scan = scan_stale_nix_tmp_dirs(tmp_dir, tmp_age, now_ns, owner_filter_uid);
    let mut stats = TmpCleanupStats {
        dirs_removed: RemovedBatch::None,
        dirs_skipped_active_count: scan.dirs_skipped_active_count,
        dirs_skipped_recent_count: scan.dirs_skipped_recent_count,
        dirs_skipped_unreadable_count: scan.dirs_skipped_unreadable_count,
        errors: scan.errors,
    };

    remove_stale_tmp_dirs(tmp_dir, &scan.stale_dirs, mode, cutoff_ns, &mut stats);

    if let RemovedBatch::Some { count, freed_bytes } = stats.dirs_removed {
        let verb = match mode {
            CleanupMode::DryRun => "[dry-run] would remove",
            CleanupMode::Apply => "removed",
        };
        log(&format!(
            "{verb} {count} stale {}/nix-* dir(s) ({})",
            tmp_dir.display(),
            format_bytes(freed_bytes)
        ));
    }

    stats
}

/// Apply deletion (or dry-run tally) over discovered stale `/tmp/nix-*` directories.
pub fn remove_stale_tmp_dirs(
    tmp_root: &NonRootAbsPath,
    stale_dirs: &[(NonRootAbsPath, u64)],
    mode: CleanupMode,
    cutoff_ns: i128,
    stats: &mut TmpCleanupStats,
) {
    assert!(stale_dirs.len() <= CANDIDATES_MAX);
    if mode.is_dry_run() {
        for &(_, size_bytes) in stale_dirs {
            stats.dirs_removed.record(size_bytes);
        }
        return;
    }

    for (entry_path, size_bytes) in stale_dirs {
        // Re-verify PID liveness and recency for real directories right before deletion
        // (closes POCPOU gap); symlink swaps still hit `safe_rmtree_under` so they are logged as errors.
        if let Ok(meta) = fs::symlink_metadata(entry_path.as_path())
            && meta.file_type().is_dir()
        {
            let entry_name = entry_path.file_name();
            if parse_nix_develop_pid(&entry_name).is_some_and(sys::is_pid_alive) {
                stats.dirs_skipped_active_count += 1;
                continue;
            }
            match inspect_tmp_tree(entry_path, cutoff_ns) {
                TreeFreshness::Stale(_) => {}
                TreeFreshness::Recent => {
                    stats.dirs_skipped_recent_count += 1;
                    continue;
                }
                TreeFreshness::Unreadable(err) => {
                    stats.dirs_skipped_unreadable_count += 1;
                    log(&format!(
                        "warning: skipped {} (unreadable during re-check): {err}",
                        entry_path.display()
                    ));
                    continue;
                }
                TreeFreshness::LimitExceeded => {
                    stats.dirs_skipped_unreadable_count += 1;
                    log(&format!(
                        "warning: skipped {} (walk limit exceeded during re-check)",
                        entry_path.display()
                    ));
                    continue;
                }
            }
        }

        match sys::safe_rmtree_under(tmp_root.as_abs(), entry_path) {
            Ok(()) => {
                stats.dirs_removed.record(*size_bytes);
            }
            Err(err) => {
                stats
                    .errors
                    .push_truncated(format!("rmtree {}: {err}", entry_path.display()));
            }
        }
    }
}

/// Prune old Nix generations and collect unreferenced `/nix/store` paths.
pub fn run_nix_gc(
    nix_gc_age_days: NixGcAgeDays,
    mode: CleanupMode,
    is_root: bool,
    timeout: Option<Duration>,
) -> NixGcOutcome {
    let mut errors = BoundedErrors::new();
    let Some(timeout) = timeout else {
        errors.push_truncated("nix GC skipped: total run timeout exhausted");
        return NixGcOutcome::Skipped { errors };
    };

    if mode.is_dry_run() {
        let Some(nix_store) = process::find_executable_in_path("nix-store") else {
            errors.push_truncated("nix-store not found on PATH");
            return NixGcOutcome::DryRun {
                dead_paths_count: 0,
                errors,
            };
        };

        let mut dead_paths_count: u64 = 0;
        let options = CommandOptions {
            program: nix_store.into_os_string(),
            args: vec![OsString::from("--gc"), OsString::from("--print-dead")],
            timeout,
            env_override: None,
            privilege_drop: None,
            stream_lines_max: DEAD_PATHS_STREAM_LINES_MAX,
        };
        let outcome = process::run_command_streaming(
            &options,
            StdoutConfig::StreamOnly {
                on_line: Some(|line: &str| {
                    if !line.trim().is_empty() && dead_paths_count < DEAD_PATHS_COUNT_MAX {
                        dead_paths_count += 1;
                    }
                }),
            },
            StderrConfig::<fn(&str)>::Piped {
                tail_bytes_max: SUBPROCESS_STDERR_BYTES_MAX,
                on_line: None,
            },
        );

        if let Some(detail) = outcome.failure_detail() {
            errors.push_truncated(format!("nix-store --gc --print-dead: {detail}"));
            return NixGcOutcome::DryRun {
                dead_paths_count: 0,
                errors,
            };
        }

        log(&format!(
            "[dry-run] would run nix-collect-garbage --delete-older-than {nix_gc_age_days}d \
             ({dead_paths_count} currently dead store paths; additional paths will become \
             dead after pruning result* links and generations)"
        ));
        return NixGcOutcome::DryRun {
            dead_paths_count,
            errors,
        };
    }

    let Some(nix_gc) = process::find_executable_in_path("nix-collect-garbage") else {
        errors.push_truncated("nix-collect-garbage not found on PATH");
        return NixGcOutcome::Skipped { errors };
    };

    if !is_root {
        log("running non-root: system profile generations will not be pruned");
    }

    let options = CommandOptions {
        program: nix_gc.into_os_string(),
        args: vec![
            OsString::from("--delete-older-than"),
            OsString::from(format!("{nix_gc_age_days}d")),
        ],
        timeout,
        env_override: None,
        privilege_drop: None,
        stream_lines_max: DEAD_PATHS_STREAM_LINES_MAX,
    };

    // Inspect both stdout and stderr because different Nix versions emit the GC
    // summary on either stream.
    let mut stdout_match: Option<RemovedBatch> = None;
    let mut stderr_match: Option<RemovedBatch> = None;
    let outcome = process::run_command_streaming(
        &options,
        StdoutConfig::StreamOnly {
            on_line: Some(|line: &str| {
                if let Some(parsed) = parse_nix_gc_freed_line(line) {
                    stdout_match = Some(parsed);
                }
            }),
        },
        StderrConfig::Piped {
            tail_bytes_max: SUBPROCESS_STDERR_BYTES_MAX,
            on_line: Some(|line: &str| {
                if let Some(parsed) = parse_nix_gc_freed_line(line) {
                    stderr_match = Some(parsed);
                }
            }),
        },
    );

    if matches!(outcome, CommandOutcome::Failed { .. }) {
        let detail = outcome
            .failure_detail()
            .expect("Failed outcome always has failure_detail");
        errors.push_truncated(format!("nix-collect-garbage: {detail}"));
        return NixGcOutcome::Skipped { errors };
    }

    let matched_summary = stderr_match.or(stdout_match);
    let failure_detail = outcome.failure_detail();

    let reclaimed = match matched_summary {
        Some(batch) => {
            log(&format!(
                "nix-collect-garbage: {} store paths deleted, {} freed",
                batch.count(),
                format_bytes(batch.freed_bytes())
            ));
            batch
        }
        None => {
            if failure_detail.is_none() {
                log(
                    "warning: nix-collect-garbage succeeded without a recognized freed-bytes summary line",
                );
            }
            RemovedBatch::None
        }
    };

    let succeeded = failure_detail.is_none();
    if let Some(detail) = failure_detail {
        errors.push_truncated(format!("nix-collect-garbage: {detail}"));
    }

    NixGcOutcome::Executed {
        pruned_system_profiles: is_root && succeeded,
        reclaimed,
        errors,
    }
}

/// Best-effort desktop notification on NixOS (`notify-send`) and macOS (`terminal-notifier` / `osascript`).
pub fn send_notification(privilege: &ExecutionPrivilege, notification: &NotificationMessage) {
    let Some(options) = build_notification_command(privilege, notification) else {
        return;
    };

    let outcome = process::run_command_streaming::<fn(&str), fn(&str)>(
        &options,
        StdoutConfig::StreamOnly { on_line: None },
        StderrConfig::Piped {
            tail_bytes_max: 4096,
            on_line: None,
        },
    );
    if let Some(detail) = outcome.failure_detail() {
        log(&format!("warning: desktop notification failed: {detail}"));
    }
}

#[cfg(target_os = "linux")]
fn build_notification_command(
    privilege: &ExecutionPrivilege,
    notification: &NotificationMessage,
) -> Option<CommandOptions> {
    let notify_send = process::find_executable_in_path("notify-send")?;
    let bus_path = std::path::PathBuf::from(format!("/run/user/{}/bus", privilege.target_uid()));

    let (env_override, privilege_drop) = match privilege.drop_target() {
        Some(target) => {
            if !bus_path.exists() {
                return None;
            }
            let env_pairs = vec![
                (
                    OsString::from("PATH"),
                    env::var_os("PATH").unwrap_or_default(),
                ),
                (
                    OsString::from("HOME"),
                    target.home_dir.as_os_str().to_os_string(),
                ),
                (OsString::from("USER"), OsString::from(target.name.as_str())),
                (
                    OsString::from("XDG_RUNTIME_DIR"),
                    OsString::from(format!("/run/user/{}", target.uid)),
                ),
                (
                    OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                    OsString::from(format!("unix:path={}", bus_path.display())),
                ),
            ];
            let drop_cfg = PrivilegeDrop::for_target(target, vec![target.gid]).ok()?;
            (Some(env_pairs), Some(drop_cfg))
        }
        None => {
            let mut env_pairs: Vec<(OsString, OsString)> =
                env::vars_os().take(ENV_VARS_MAX).collect();
            let has_dbus = env_pairs
                .iter()
                .any(|(key, _)| key == "DBUS_SESSION_BUS_ADDRESS");
            if !has_dbus {
                if !bus_path.exists() {
                    return None;
                }
                env_pairs.push((
                    OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                    OsString::from(format!("unix:path={}", bus_path.display())),
                ));
            }
            (Some(env_pairs), None)
        }
    };

    Some(CommandOptions {
        program: notify_send.into_os_string(),
        args: vec![
            OsString::from("--app-name=disk-cleanup"),
            OsString::from(notification.title()),
            OsString::from(notification.body()),
        ],
        timeout: Duration::from_secs(NOTIFY_TIMEOUT_SECONDS),
        env_override,
        privilege_drop,
        stream_lines_max: NOTIFY_STREAM_LINES_MAX,
    })
}

#[cfg(target_os = "macos")]
fn build_notification_command(
    privilege: &ExecutionPrivilege,
    notification: &NotificationMessage,
) -> Option<CommandOptions> {
    let base_command: Vec<OsString> = if let Some(notifier) =
        process::find_executable_in_path("terminal-notifier")
    {
        vec![
            notifier.into_os_string(),
            OsString::from("-title"),
            OsString::from(notification.title()),
            OsString::from("-message"),
            OsString::from(notification.body()),
            OsString::from("-group"),
            OsString::from("disk-cleanup"),
        ]
    } else {
        vec![
            OsString::from("/usr/bin/osascript"),
            OsString::from("-e"),
            OsString::from(
                "on run argv\ndisplay notification (item 1 of argv) with title (item 2 of argv)\nend run",
            ),
            OsString::from("--"),
            OsString::from(notification.body()),
            OsString::from(notification.title()),
        ]
    };

    let (program, args) = match privilege.drop_target() {
        Some(target) => {
            let mut wrapped = vec![
                OsString::from("asuser"),
                OsString::from(format!("{}", target.uid)),
                OsString::from("/usr/bin/sudo"),
                OsString::from("--non-interactive"),
                OsString::from("--user"),
                OsString::from(target.name.as_str()),
            ];
            wrapped.extend(base_command);
            (OsString::from("/bin/launchctl"), wrapped)
        }
        None => {
            let mut iter = base_command.into_iter();
            let prog = iter.next().expect("base_command is non-empty");
            (prog, iter.collect())
        }
    };

    Some(CommandOptions {
        program,
        args,
        timeout: Duration::from_secs(NOTIFY_TIMEOUT_SECONDS),
        env_override: None,
        privilege_drop: None,
        stream_lines_max: NOTIFY_STREAM_LINES_MAX,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn build_notification_command(
    _privilege: &ExecutionPrivilege,
    _notification: &NotificationMessage,
) -> Option<CommandOptions> {
    None
}

pub fn build_summary_parts(
    user_stats: &UserPhaseStats,
    nix_outcome: &NixGcOutcome,
    tmp_stats: &TmpCleanupStats,
) -> Vec<String> {
    let mut parts = Vec::with_capacity(4);
    if user_stats.result_links_removed_count > 0 {
        parts.push(format!(
            "{} stale result link(s)",
            user_stats.result_links_removed_count
        ));
    }
    if let RemovedBatch::Some { count, freed_bytes } = user_stats.targets_removed {
        parts.push(format!(
            "{count} Rust target/ dir(s) ({})",
            format_bytes(freed_bytes)
        ));
    }
    if let RemovedBatch::Some { count, freed_bytes } = tmp_stats.dirs_removed {
        parts.push(format!(
            "{count} /tmp/nix-* dir(s) ({})",
            format_bytes(freed_bytes)
        ));
    }
    if let RemovedBatch::Some { freed_bytes, .. } = nix_outcome.reclaimed() {
        parts.push(format!("Nix GC ({})", format_bytes(freed_bytes)));
    }
    assert!(parts.len() <= 4);
    parts
}

pub fn format_bytes(size_bytes: u64) -> String {
    if size_bytes < 1024 {
        return format!("{size_bytes} B");
    }
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
    let mut value = size_bytes as f64;
    for (index, unit) in UNITS.iter().enumerate() {
        value /= 1024.0;
        if value < 1024.0 || index + 1 == UNITS.len() {
            return format!("{value:.1} {unit}");
        }
    }
    unreachable!("UNITS is non-empty and the final iteration always returns")
}

pub fn format_age(age_ns: u128) -> String {
    let age_seconds_whole = age_ns.div_euclid(NANOS_PER_SECOND_U128);
    if age_seconds_whole < 3600 {
        let minutes = age_seconds_whole.div_euclid(60);
        return format!("{minutes}m");
    }
    #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
    let age_seconds = (age_ns as f64) / (NANOS_PER_SECOND_U128 as f64);
    if age_seconds < SECONDS_PER_DAY {
        return format!("{:.1}h", age_seconds / SECONDS_PER_HOUR);
    }
    format!("{:.1}d", age_seconds / SECONDS_PER_DAY)
}

/// Fallible conversion of a Nix GC size value + unit string (`B`..`TiB`) to bytes.
///
/// Rejects non-finite values, negative values, unknown units, and products that
/// overflow `u64` instead of panicking or saturating.
pub fn try_parse_size_to_bytes(value: f64, unit: &str) -> Option<u64> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let multiplier = SizeUnit::parse(unit)?.multiplier();
    #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
    let product = value * (multiplier as f64);
    if !product.is_finite() || product < 0.0 || product >= U64_UPPER_BOUND_F64 {
        return None;
    }
    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let bytes = product as u64;
    Some(bytes)
}

/// Parse a `nix-collect-garbage` summary line such as:
/// `"42 store paths deleted, 12.50 MiB freed"` -> `Some(RemovedBatch::Some { count: 42, freed_bytes: 13_107_200 })`.
///
/// Rejects inconsistent lines where `0` store paths claim `> 0` freed bytes.
/// Uses a zero-allocation 7-token stack sliding window on the per-line hot path.
pub fn parse_nix_gc_freed_line(line: &str) -> Option<RemovedBatch> {
    assert!(
        line.len() <= SUBPROCESS_LINE_BYTES_MAX,
        "line exceeded SUBPROCESS_LINE_BYTES_MAX"
    );

    let mut window: [&str; 7] = [""; 7];
    let mut filled: usize = 0;

    for token in line.split_whitespace() {
        if filled < 7 {
            if let Some(slot) = window.get_mut(filled) {
                *slot = token;
            }
            filled += 1;
        } else {
            let [_, w1, w2, w3, w4, w5, w6] = window;
            window = [w1, w2, w3, w4, w5, w6, token];
        }

        if filled == 7
            && let Some(parsed) = parse_gc_summary_window(window)
        {
            return Some(parsed);
        }
    }

    None
}

fn parse_gc_summary_window(window: [&str; 7]) -> Option<RemovedBatch> {
    let [
        count_tok,
        store_tok,
        paths_tok,
        deleted_tok,
        amount_tok,
        unit_tok,
        freed_tok,
    ] = window;

    let keywords_match = store_tok.eq_ignore_ascii_case("store")
        && paths_tok.eq_ignore_ascii_case("paths")
        && deleted_tok.eq_ignore_ascii_case("deleted,");
    if !keywords_match {
        return None;
    }

    // Allow trailing punctuation like `freed.` or `freed` cleanly.
    let freed_matches = freed_tok
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("freed"))
        && freed_tok.get(5..).is_some_and(|tail| {
            tail.is_empty() || tail.chars().next().is_some_and(|c| !c.is_alphanumeric())
        });
    if !freed_matches {
        return None;
    }

    let valid_count = !count_tok.is_empty() && count_tok.bytes().all(|b| b.is_ascii_digit());
    if !valid_count {
        return None;
    }
    let paths_deleted = count_tok.parse::<u64>().ok()?;

    let valid_amount =
        !amount_tok.is_empty() && amount_tok.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    if !valid_amount {
        return None;
    }
    let amount = amount_tok.parse::<f64>().ok()?;
    let freed_bytes = try_parse_size_to_bytes(amount, unit_tok)?;
    RemovedBatch::from_parts(paths_deleted, freed_bytes).ok()
}
