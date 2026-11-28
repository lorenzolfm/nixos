//! Phase execution (user phase, `/tmp/nix-*`, Nix GC), notifications, and formatting.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::constants::{
    AGE_DAYS_MAX, AGE_HOURS_MAX, DEAD_PATHS_COUNT_MAX, ERRORS_MAX, NIX_ENV_LOG_LINES_MAX,
    NIX_ENV_TIMEOUT_SECONDS, NOTIFY_FREED_BYTES_MIN, NOTIFY_TIMEOUT_SECONDS,
    SUBPROCESS_OUTPUT_BYTES_MAX, SUBPROCESS_STDERR_BYTES_MAX, TARGET_MTIME_DEPTH_MAX,
    USER_PHASE_TIMEOUT_SECONDS,
};
use crate::ipc::{self, UserPhaseStats};
use crate::process::{self, CommandOptions, PrivilegeDrop};
use crate::scan::{
    self, DiskUsage, NANOS_PER_SECOND, ProjectScanResult, is_cargo_target_dir,
    is_cargo_target_locked, latest_target_mtime_ns, non_negative_seconds_to_ns, scan_projects,
    scan_stale_nix_tmp_dirs, user_nix_profiles,
};
use crate::sys::{self, PasswdEntry};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NixGcStats {
    pub ran: bool,
    pub paths_deleted_count: u64,
    pub freed_bytes: u64,
    pub dead_paths_count: u64,
    pub system_profiles_pruned: bool,
    pub errors: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TmpCleanupStats {
    pub dirs_removed_count: u64,
    pub freed_bytes: u64,
    pub dirs_skipped_active_count: u64,
    pub dirs_skipped_recent_count: u64,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct UserPhaseConfig<'a> {
    pub projects_dir: &'a Path,
    pub result_age_days: f64,
    pub target_age_hours: f64,
    pub nix_gc_age_days: u32,
    pub dry_run: bool,
    pub now_ns: i128,
    pub deadline: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerDecision {
    SkipBelowThreshold,
    Run { trigger_description: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationMessage {
    pub title: String,
    pub body: String,
}

/// Append an error string to a bounded list without exceeding `ERRORS_MAX`.
pub fn append_error(errors: &mut Vec<String>, message: String) {
    assert!(!message.is_empty(), "error message must be non-empty");
    if errors.len() < ERRORS_MAX {
        errors.push(message);
    }
    assert!(errors.len() <= ERRORS_MAX);
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
pub fn decide_trigger(before: &DiskUsage, threshold: u8, force: bool) -> TriggerDecision {
    assert!(threshold >= 1, "threshold must be >= 1");
    assert!(threshold <= 100, "threshold must be <= 100");
    assert!(before.used_percent <= 100, "used_percent must be <= 100");

    if before.used_percent < threshold {
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
    assert!(before.used_percent <= 100);
    assert!(after.used_percent <= 100);

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
    assert!(!title.is_empty());
    assert!(!body.is_empty());
    Some(NotificationMessage { title, body })
}

/// Run Phase 1 directly or in a privilege-dropped subprocess when invoked as root.
pub fn run_user_phase(passwd: &PasswdEntry, config: &UserPhaseConfig<'_>) -> UserPhaseStats {
    assert!(!passwd.name.is_empty(), "passwd.name must be non-empty");
    assert!(
        config.projects_dir.is_absolute(),
        "projects_dir must be absolute"
    );
    assert!(config.now_ns > 0, "now_ns must be positive");

    let should_drop_privileges = sys::effective_uid() == 0 && passwd.uid != 0;
    if !should_drop_privileges {
        let user_home =
            sys::absolutize_path(&passwd.home_dir).unwrap_or_else(|_| passwd.home_dir.clone());
        return run_user_phase_in_process(&passwd.name, &user_home, config);
    }

    let Some(timeout) = remaining_budget(
        config.deadline,
        Duration::from_secs(USER_PHASE_TIMEOUT_SECONDS),
    ) else {
        let mut stats = UserPhaseStats::default();
        append_error(
            &mut stats.errors,
            "user phase skipped: total run timeout exhausted".to_string(),
        );
        return stats;
    };

    let current_exe = match env::current_exe() {
        Ok(path) => path,
        Err(err) => {
            let mut stats = UserPhaseStats::default();
            append_error(
                &mut stats.errors,
                format!("user phase failed to locate executable: {err}"),
            );
            return stats;
        }
    };
    assert!(current_exe.is_absolute(), "current_exe must be absolute");

    let whole_secs = config.now_ns / NANOS_PER_SECOND;
    let rem_nanos = config.now_ns % NANOS_PER_SECOND;
    assert!(whole_secs >= 0);
    assert!(rem_nanos >= 0);
    let now_seconds_arg = format!("{whole_secs}.{rem_nanos:09}");

    let mut args: Vec<OsString> = vec![
        OsString::from("--_user-phase"),
        OsString::from("--user"),
        OsString::from(&passwd.name),
        OsString::from("--projects-dir"),
        config.projects_dir.as_os_str().to_os_string(),
        OsString::from("--result-age-days"),
        OsString::from(format!("{}", config.result_age_days)),
        OsString::from("--target-age-hours"),
        OsString::from(format!("{}", config.target_age_hours)),
        OsString::from("--nix-gc-age-days"),
        OsString::from(format!("{}", config.nix_gc_age_days)),
        OsString::from("--_now-seconds"),
        OsString::from(now_seconds_arg),
    ];
    if config.dry_run {
        args.push(OsString::from("--dry-run"));
    }

    let mut env_pairs: Vec<(OsString, OsString)> = vec![
        (
            OsString::from("PATH"),
            env::var_os("PATH").unwrap_or_default(),
        ),
        (
            OsString::from("HOME"),
            passwd.home_dir.as_os_str().to_os_string(),
        ),
        (OsString::from("USER"), OsString::from(&passwd.name)),
        (OsString::from("LOGNAME"), OsString::from(&passwd.name)),
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

    let extra_groups = match sys::user_groups(&passwd.name, passwd.gid) {
        Ok(groups) => groups,
        Err(err) => {
            log(&format!(
                "warning: getgrouplist failed for {}: {err}; using primary GID",
                passwd.name
            ));
            vec![passwd.gid]
        }
    };

    let options = CommandOptions {
        program: current_exe.into_os_string(),
        args,
        timeout,
        env_override: Some(env_pairs),
        privilege_drop: Some(PrivilegeDrop {
            user_uid: passwd.uid,
            group_gid: passwd.gid,
            extra_groups,
        }),
        stdout_lines_max: DEAD_PATHS_COUNT_MAX,
        capture_stdout_bytes_max: SUBPROCESS_OUTPUT_BYTES_MAX,
        capture_stderr_bytes_max: 0,
        inherit_stderr: true,
    };

    let result = process::run_command_streaming::<fn(&str), fn(&str)>(&options, None, None);
    if let Some(err_msg) = result.error_message {
        let mut stats = UserPhaseStats::default();
        append_error(
            &mut stats.errors,
            format!("user phase failed to execute: {err_msg}"),
        );
        return stats;
    }
    if result.timed_out {
        let mut stats = UserPhaseStats::default();
        append_error(
            &mut stats.errors,
            format!("user phase timed out after {}s", timeout.as_secs()),
        );
        return stats;
    }
    if result.returncode != 0 {
        let mut stats = UserPhaseStats::default();
        append_error(
            &mut stats.errors,
            format!("user phase exited with code {}", result.returncode),
        );
        return stats;
    }
    if result.stdout_truncated {
        let mut stats = UserPhaseStats::default();
        append_error(
            &mut stats.errors,
            "user phase IPC output exceeded byte limit".to_string(),
        );
        return stats;
    }

    match ipc::parse_user_phase_stats_json(&result.stdout_text) {
        Ok(stats) => {
            ipc::assert_user_phase_stats_valid(&stats);
            stats
        }
        Err(err) => {
            let mut stats = UserPhaseStats::default();
            append_error(
                &mut stats.errors,
                format!("failed to parse user phase output: {err}"),
            );
            stats
        }
    }
}

/// Discover and clean stale Nix `result*` links, Rust `target/` dirs, and user profiles.
pub fn run_user_phase_in_process(
    user_name: &str,
    user_home: &Path,
    config: &UserPhaseConfig<'_>,
) -> UserPhaseStats {
    assert!(!user_name.is_empty(), "user_name must be non-empty");
    assert!(user_home.is_absolute(), "user_home must be absolute");
    assert!(
        config.projects_dir.is_absolute(),
        "projects_dir must be absolute"
    );
    assert!(config.now_ns > 0, "now_ns must be positive");

    let scan = scan_projects(
        config.projects_dir,
        config.now_ns,
        config.result_age_days,
        config.target_age_hours,
    );
    let profiles = user_nix_profiles(user_name, user_home);

    let stats = if config.dry_run {
        log_user_dry_run(&scan, &profiles, config.nix_gc_age_days, config.now_ns)
    } else {
        let target_mtime_max_ns =
            config.now_ns - non_negative_seconds_to_ns(config.target_age_hours * 3600.0);
        apply_user_cleanup(
            &scan,
            &profiles,
            config.nix_gc_age_days,
            config.now_ns,
            target_mtime_max_ns,
            config.deadline,
        )
    };

    ipc::assert_user_phase_stats_valid(&stats);
    stats
}

/// Report what Phase 1 would remove without mutating the filesystem.
pub fn log_user_dry_run(
    scan: &ProjectScanResult,
    profiles: &[PathBuf],
    nix_gc_age_days: u32,
    now_ns: i128,
) -> UserPhaseStats {
    assert!(now_ns > 0, "now_ns must be positive");
    assert!(
        nix_gc_age_days <= AGE_DAYS_MAX,
        "nix_gc_age_days out of bounds"
    );

    let mut stats = UserPhaseStats {
        targets_skipped_active_count: scan.targets_skipped_active_count,
        targets_skipped_recent_count: scan.targets_skipped_recent_count,
        errors: scan.errors.iter().take(ERRORS_MAX).cloned().collect(),
        ..UserPhaseStats::default()
    };

    for (link_path, mtime_ns) in &scan.stale_results {
        let age = format_age(age_ns_since(now_ns, *mtime_ns));
        log(&format!(
            "[dry-run] would remove stale Nix result symlink: {} (age {age})",
            link_path.display()
        ));
        stats.result_links_removed_count += 1;
    }

    for candidate in &scan.target_candidates {
        let age = format_age(age_ns_since(now_ns, candidate.mtime_latest_ns));
        let size_text = format_bytes(candidate.size_bytes);
        log(&format!(
            "[dry-run] would remove Rust target/: {} ({size_text}, idle {age})",
            candidate.path.display()
        ));
        stats.targets_removed_count += 1;
        stats.targets_freed_bytes = stats
            .targets_freed_bytes
            .saturating_add(candidate.size_bytes);
    }

    if !profiles.is_empty() {
        if process::find_executable_in_path("nix-env").is_none() {
            append_error(&mut stats.errors, "nix-env not found on PATH".to_string());
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

    ipc::assert_user_phase_stats_valid(&stats);
    stats
}

/// Apply Phase 1 deletions after re-verifying safety preconditions at use time.
pub fn apply_user_cleanup(
    scan: &ProjectScanResult,
    profiles: &[PathBuf],
    nix_gc_age_days: u32,
    now_ns: i128,
    target_mtime_max_ns: i128,
    deadline: Instant,
) -> UserPhaseStats {
    assert!(now_ns > 0, "now_ns must be positive");
    assert!(
        nix_gc_age_days <= AGE_DAYS_MAX,
        "nix_gc_age_days out of bounds"
    );

    let mut stats = UserPhaseStats {
        targets_skipped_active_count: scan.targets_skipped_active_count,
        targets_skipped_recent_count: scan.targets_skipped_recent_count,
        errors: scan.errors.iter().take(ERRORS_MAX).cloned().collect(),
        ..UserPhaseStats::default()
    };

    for (link_path, mtime_ns) in &scan.stale_results {
        let age = format_age(age_ns_since(now_ns, *mtime_ns));
        // Re-verify symlink target immediately before unlink (closes POCPOU gap).
        let link_meta = match fs::symlink_metadata(link_path) {
            Ok(meta) => meta,
            Err(err) => {
                append_error(
                    &mut stats.errors,
                    format!("unlink {}: {err}", link_path.display()),
                );
                continue;
            }
        };
        if !link_meta.file_type().is_symlink() {
            continue;
        }
        let link_target = match fs::read_link(link_path) {
            Ok(target) => target,
            Err(err) => {
                append_error(
                    &mut stats.errors,
                    format!("unlink {}: {err}", link_path.display()),
                );
                continue;
            }
        };
        if !link_target
            .as_os_str()
            .as_bytes()
            .starts_with(b"/nix/store/")
        {
            continue;
        }
        match fs::remove_file(link_path) {
            Ok(()) => {
                log(&format!(
                    "removed stale Nix result symlink: {} (age {age})",
                    link_path.display()
                ));
                stats.result_links_removed_count += 1;
            }
            Err(err) => {
                append_error(
                    &mut stats.errors,
                    format!("unlink {}: {err}", link_path.display()),
                );
            }
        }
    }

    for candidate in &scan.target_candidates {
        // Re-verify markers, lock, and recency immediately before deletion.
        if !is_cargo_target_dir(&candidate.path) {
            log(&format!(
                "skipped Rust target/ (no longer a Cargo target): {}",
                candidate.path.display()
            ));
            continue;
        }
        if is_cargo_target_locked(&candidate.path) {
            stats.targets_skipped_active_count += 1;
            log(&format!(
                "skipped Rust target/ (build started): {}",
                candidate.path.display()
            ));
            continue;
        }
        let refreshed_mtime_ns = latest_target_mtime_ns(
            &candidate.path,
            now_ns,
            Some(target_mtime_max_ns),
            TARGET_MTIME_DEPTH_MAX,
        );
        let is_still_stale =
            refreshed_mtime_ns.is_some_and(|mtime_ns| mtime_ns <= target_mtime_max_ns);
        if !is_still_stale {
            stats.targets_skipped_recent_count += 1;
            log(&format!(
                "skipped Rust target/ (modified or unreadable during scan): {}",
                candidate.path.display()
            ));
            continue;
        }

        let age = format_age(age_ns_since(now_ns, candidate.mtime_latest_ns));
        let size_text = format_bytes(candidate.size_bytes);
        match sys::safe_rmtree(&candidate.path) {
            Ok(()) => {
                log(&format!(
                    "removed Rust target/: {} ({size_text}, idle {age})",
                    candidate.path.display()
                ));
                stats.targets_removed_count += 1;
                stats.targets_freed_bytes = stats
                    .targets_freed_bytes
                    .saturating_add(candidate.size_bytes);
            }
            Err(err) => {
                append_error(
                    &mut stats.errors,
                    format!("rmtree {}: {err}", candidate.path.display()),
                );
            }
        }
    }

    if !profiles.is_empty() {
        let Some(nix_env) = process::find_executable_in_path("nix-env") else {
            append_error(&mut stats.errors, "nix-env not found on PATH".to_string());
            ipc::assert_user_phase_stats_valid(&stats);
            return stats;
        };

        for profile in profiles {
            let Some(timeout) =
                remaining_budget(deadline, Duration::from_secs(NIX_ENV_TIMEOUT_SECONDS))
            else {
                append_error(
                    &mut stats.errors,
                    format!("nix-env {}: timeout budget exhausted", profile.display()),
                );
                break;
            };

            let profile_name = profile
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| profile.display().to_string());
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
                stdout_lines_max: DEAD_PATHS_COUNT_MAX,
                capture_stdout_bytes_max: 0,
                capture_stderr_bytes_max: SUBPROCESS_STDERR_BYTES_MAX,
                inherit_stderr: false,
            };

            let result = process::run_command_streaming(
                &options,
                Some(|line: &str| {
                    let stripped = line.trim();
                    if !stripped.is_empty() && logged_lines_count < NIX_ENV_LOG_LINES_MAX {
                        log(&format!("nix-env ({profile_name}): {stripped}"));
                        logged_lines_count += 1;
                    }
                }),
                None::<fn(&str)>,
            );

            if let Some(err_msg) = result.error_message {
                append_error(
                    &mut stats.errors,
                    format!("nix-env {}: {err_msg}", profile.display()),
                );
            } else if result.timed_out {
                append_error(
                    &mut stats.errors,
                    format!(
                        "nix-env {}: timed out after {}s",
                        profile.display(),
                        timeout.as_secs()
                    ),
                );
            } else if result.returncode == 0 {
                stats.user_profiles_pruned_count += 1;
            } else {
                let trimmed = result.stderr_tail.trim();
                let detail = if trimmed.is_empty() {
                    format!("exit {}", result.returncode)
                } else {
                    trimmed.to_string()
                };
                append_error(
                    &mut stats.errors,
                    format!("nix-env {}: {detail}", profile.display()),
                );
            }
        }
    }

    ipc::assert_user_phase_stats_valid(&stats);
    stats
}

/// Remove stale `/tmp/nix-*` shell and build directories (Linux only).
pub fn clean_nix_tmp_dirs(
    tmp_dir: &Path,
    tmp_age_hours: f64,
    dry_run: bool,
    now_ns: i128,
) -> TmpCleanupStats {
    assert!(tmp_dir.is_absolute(), "tmp_dir must be absolute");
    assert!(now_ns > 0, "now_ns must be positive");
    assert!(tmp_age_hours.is_finite());
    assert!(tmp_age_hours <= f64::from(AGE_HOURS_MAX));

    let scan = scan_stale_nix_tmp_dirs(tmp_dir, tmp_age_hours, now_ns);
    let mut stats = TmpCleanupStats {
        dirs_skipped_active_count: scan.dirs_skipped_active_count,
        dirs_skipped_recent_count: scan.dirs_skipped_recent_count,
        errors: scan.errors.into_iter().take(ERRORS_MAX).collect(),
        ..TmpCleanupStats::default()
    };

    remove_stale_tmp_dirs(&scan.stale_dirs, dry_run, &mut stats);

    if stats.dirs_removed_count > 0 {
        let verb = if dry_run {
            "[dry-run] would remove"
        } else {
            "removed"
        };
        log(&format!(
            "{verb} {} stale {}/nix-* dir(s) ({})",
            stats.dirs_removed_count,
            tmp_dir.display(),
            format_bytes(stats.freed_bytes)
        ));
    }

    stats
}

/// Apply deletion (or dry-run tally) over discovered stale `/tmp/nix-*` directories.
pub fn remove_stale_tmp_dirs(
    stale_dirs: &[(PathBuf, u64)],
    dry_run: bool,
    stats: &mut TmpCleanupStats,
) {
    assert!(stats.errors.len() <= ERRORS_MAX);
    if dry_run {
        for &(_, size_bytes) in stale_dirs {
            stats.dirs_removed_count += 1;
            stats.freed_bytes = stats.freed_bytes.saturating_add(size_bytes);
        }
        return;
    }

    for (entry_path, size_bytes) in stale_dirs {
        match sys::safe_rmtree(entry_path) {
            Ok(()) => {
                stats.dirs_removed_count += 1;
                stats.freed_bytes = stats.freed_bytes.saturating_add(*size_bytes);
            }
            Err(err) => {
                append_error(
                    &mut stats.errors,
                    format!("rmtree {}: {err}", entry_path.display()),
                );
            }
        }
    }
}

/// Prune old Nix generations and collect unreferenced `/nix/store` paths.
pub fn run_nix_gc(nix_gc_age_days: u32, dry_run: bool, timeout: Option<Duration>) -> NixGcStats {
    assert!(
        nix_gc_age_days <= AGE_DAYS_MAX,
        "nix_gc_age_days out of bounds"
    );

    let mut stats = NixGcStats::default();
    let Some(timeout) = timeout else {
        append_error(
            &mut stats.errors,
            "nix GC skipped: total run timeout exhausted".to_string(),
        );
        return stats;
    };

    if dry_run {
        let Some(nix_store) = process::find_executable_in_path("nix-store") else {
            append_error(&mut stats.errors, "nix-store not found on PATH".to_string());
            return stats;
        };

        let mut dead_paths_count: u64 = 0;
        let options = CommandOptions {
            program: nix_store.into_os_string(),
            args: vec![OsString::from("--gc"), OsString::from("--print-dead")],
            timeout,
            env_override: None,
            privilege_drop: None,
            stdout_lines_max: DEAD_PATHS_COUNT_MAX,
            capture_stdout_bytes_max: 0,
            capture_stderr_bytes_max: SUBPROCESS_STDERR_BYTES_MAX,
            inherit_stderr: false,
        };
        let result = process::run_command_streaming(
            &options,
            Some(|line: &str| {
                if !line.trim().is_empty() && dead_paths_count < DEAD_PATHS_COUNT_MAX {
                    dead_paths_count += 1;
                }
            }),
            None::<fn(&str)>,
        );

        if let Some(err_msg) = result.error_message {
            append_error(
                &mut stats.errors,
                format!("nix-store --gc --print-dead: {err_msg}"),
            );
        } else if result.timed_out {
            append_error(
                &mut stats.errors,
                format!(
                    "nix-store --gc --print-dead: timed out after {}s",
                    timeout.as_secs()
                ),
            );
        } else if result.returncode == 0 {
            stats.dead_paths_count = dead_paths_count;
            log(&format!(
                "[dry-run] would run nix-collect-garbage --delete-older-than {nix_gc_age_days}d \
                 ({dead_paths_count} currently dead store paths; additional paths will become \
                 dead after pruning result* links and generations)"
            ));
        } else {
            let trimmed = result.stderr_tail.trim();
            let detail = if trimmed.is_empty() {
                format!("exit {}", result.returncode)
            } else {
                trimmed.to_string()
            };
            append_error(
                &mut stats.errors,
                format!("nix-store --gc --print-dead: {detail}"),
            );
        }
        return stats;
    }

    let Some(nix_gc) = process::find_executable_in_path("nix-collect-garbage") else {
        append_error(
            &mut stats.errors,
            "nix-collect-garbage not found on PATH".to_string(),
        );
        return stats;
    };

    if sys::effective_uid() == 0 {
        stats.system_profiles_pruned = true;
    } else {
        log("running non-root: system profile generations will not be pruned");
    }

    let mut matched_summary: Option<(u64, u64)> = None;
    let options = CommandOptions {
        program: nix_gc.into_os_string(),
        args: vec![
            OsString::from("--delete-older-than"),
            OsString::from(format!("{nix_gc_age_days}d")),
        ],
        timeout,
        env_override: None,
        privilege_drop: None,
        stdout_lines_max: DEAD_PATHS_COUNT_MAX,
        capture_stdout_bytes_max: 0,
        capture_stderr_bytes_max: SUBPROCESS_STDERR_BYTES_MAX,
        inherit_stderr: false,
    };

    // Inspect both stdout and stderr because different Nix versions emit the GC
    // summary on either stream.
    let mut stdout_match: Option<(u64, u64)> = None;
    let mut stderr_match: Option<(u64, u64)> = None;
    let result = process::run_command_streaming(
        &options,
        Some(|line: &str| {
            if let Some(parsed) = parse_nix_gc_freed_line(line) {
                stdout_match = Some(parsed);
            }
        }),
        Some(|line: &str| {
            if let Some(parsed) = parse_nix_gc_freed_line(line) {
                stderr_match = Some(parsed);
            }
        }),
    );
    if stdout_match.is_some() {
        matched_summary = stdout_match;
    }
    if stderr_match.is_some() {
        matched_summary = stderr_match;
    }

    stats.ran = true;

    if let Some((paths_deleted, freed_bytes)) = matched_summary {
        stats.paths_deleted_count = paths_deleted;
        stats.freed_bytes = freed_bytes;
        log(&format!(
            "nix-collect-garbage: {} store paths deleted, {} freed",
            stats.paths_deleted_count,
            format_bytes(stats.freed_bytes)
        ));
    } else if result.returncode == 0 && !result.timed_out && result.error_message.is_none() {
        log("warning: nix-collect-garbage succeeded without a recognized freed-bytes summary line");
    }

    if let Some(err_msg) = result.error_message {
        append_error(&mut stats.errors, format!("nix-collect-garbage: {err_msg}"));
    } else if result.timed_out {
        append_error(
            &mut stats.errors,
            format!(
                "nix-collect-garbage: timed out after {}s",
                timeout.as_secs()
            ),
        );
    } else if result.returncode != 0 {
        let trimmed = result.stderr_tail.trim();
        let detail = if trimmed.is_empty() {
            format!("exit {}", result.returncode)
        } else {
            trimmed.to_string()
        };
        append_error(&mut stats.errors, format!("nix-collect-garbage: {detail}"));
    }

    stats
}

/// Best-effort desktop notification on NixOS (`notify-send`) and macOS (`terminal-notifier` / `osascript`).
pub fn send_notification(passwd: &PasswdEntry, title: &str, body: &str) {
    assert!(!title.is_empty(), "notification title must be non-empty");
    assert!(!body.is_empty(), "notification body must be non-empty");

    let Some(options) = build_notification_command(passwd, title, body) else {
        return;
    };

    let result = process::run_command_streaming::<fn(&str), fn(&str)>(&options, None, None);
    if let Some(err_msg) = result.error_message {
        log(&format!("warning: desktop notification failed: {err_msg}"));
    } else if result.timed_out {
        log(&format!(
            "warning: desktop notification timed out after {NOTIFY_TIMEOUT_SECONDS}s"
        ));
    } else if result.returncode != 0 {
        let trimmed = result.stderr_tail.trim();
        let detail = if trimmed.is_empty() {
            format!("exit {}", result.returncode)
        } else {
            trimmed.to_string()
        };
        log(&format!(
            "warning: desktop notification exited non-zero: {detail}"
        ));
    }
}

#[cfg(target_os = "linux")]
fn build_notification_command(
    passwd: &PasswdEntry,
    title: &str,
    body: &str,
) -> Option<CommandOptions> {
    let notify_send = process::find_executable_in_path("notify-send")?;
    let bus_path = PathBuf::from(format!("/run/user/{}/bus", passwd.uid));
    let should_drop_privileges = sys::effective_uid() == 0 && passwd.uid != 0;

    let (env_override, privilege_drop) = if should_drop_privileges {
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
                passwd.home_dir.as_os_str().to_os_string(),
            ),
            (OsString::from("USER"), OsString::from(&passwd.name)),
            (
                OsString::from("XDG_RUNTIME_DIR"),
                OsString::from(format!("/run/user/{}", passwd.uid)),
            ),
            (
                OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                OsString::from(format!("unix:path={}", bus_path.display())),
            ),
        ];
        let drop_cfg = PrivilegeDrop {
            user_uid: passwd.uid,
            group_gid: passwd.gid,
            extra_groups: vec![passwd.gid],
        };
        (Some(env_pairs), Some(drop_cfg))
    } else {
        let mut env_pairs: Vec<(OsString, OsString)> = env::vars_os().collect();
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
    };

    Some(CommandOptions {
        program: notify_send.into_os_string(),
        args: vec![
            OsString::from("--app-name=disk-cleanup"),
            OsString::from(title),
            OsString::from(body),
        ],
        timeout: Duration::from_secs(NOTIFY_TIMEOUT_SECONDS),
        env_override,
        privilege_drop,
        stdout_lines_max: 64,
        capture_stdout_bytes_max: 0,
        capture_stderr_bytes_max: 4096,
        inherit_stderr: false,
    })
}

#[cfg(target_os = "macos")]
fn build_notification_command(
    passwd: &PasswdEntry,
    title: &str,
    body: &str,
) -> Option<CommandOptions> {
    let base_command: Vec<OsString> = if let Some(notifier) =
        process::find_executable_in_path("terminal-notifier")
    {
        vec![
            notifier.into_os_string(),
            OsString::from("-title"),
            OsString::from(title),
            OsString::from("-message"),
            OsString::from(body),
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
            OsString::from(body),
            OsString::from(title),
        ]
    };

    let should_drop_privileges = sys::effective_uid() == 0 && passwd.uid != 0;
    let (program, args) = if should_drop_privileges {
        let mut wrapped = vec![
            OsString::from("asuser"),
            OsString::from(format!("{}", passwd.uid)),
            OsString::from("/usr/bin/sudo"),
            OsString::from("--non-interactive"),
            OsString::from("--user"),
            OsString::from(&passwd.name),
        ];
        wrapped.extend(base_command);
        (OsString::from("/bin/launchctl"), wrapped)
    } else {
        let mut iter = base_command.into_iter();
        let prog = iter.next().expect("base_command is non-empty");
        (prog, iter.collect())
    };

    Some(CommandOptions {
        program,
        args,
        timeout: Duration::from_secs(NOTIFY_TIMEOUT_SECONDS),
        env_override: None,
        privilege_drop: None,
        stdout_lines_max: 64,
        capture_stdout_bytes_max: 0,
        capture_stderr_bytes_max: 4096,
        inherit_stderr: false,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn build_notification_command(
    _passwd: &PasswdEntry,
    _title: &str,
    _body: &str,
) -> Option<CommandOptions> {
    None
}

pub fn build_summary_parts(
    user_stats: &UserPhaseStats,
    nix_stats: &NixGcStats,
    tmp_stats: &TmpCleanupStats,
) -> Vec<String> {
    ipc::assert_user_phase_stats_valid(user_stats);

    let mut parts = Vec::with_capacity(4);
    if user_stats.result_links_removed_count > 0 {
        parts.push(format!(
            "{} stale result link(s)",
            user_stats.result_links_removed_count
        ));
    }
    if user_stats.targets_removed_count > 0 {
        parts.push(format!(
            "{} Rust target/ dir(s) ({})",
            user_stats.targets_removed_count,
            format_bytes(user_stats.targets_freed_bytes)
        ));
    }
    if tmp_stats.dirs_removed_count > 0 {
        parts.push(format!(
            "{} /tmp/nix-* dir(s) ({})",
            tmp_stats.dirs_removed_count,
            format_bytes(tmp_stats.freed_bytes)
        ));
    }
    if nix_stats.paths_deleted_count > 0 {
        parts.push(format!("Nix GC ({})", format_bytes(nix_stats.freed_bytes)));
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
    format!("{size_bytes} B")
}

pub fn format_age(age_ns: u128) -> String {
    let age_seconds_whole = age_ns / 1_000_000_000;
    if age_seconds_whole < 3600 {
        let minutes = age_seconds_whole / 60;
        return format!("{minutes}m");
    }
    #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
    let age_seconds = (age_ns as f64) / 1_000_000_000.0;
    if age_seconds < 86_400.0 {
        return format!("{:.1}h", age_seconds / 3600.0);
    }
    format!("{:.1}d", age_seconds / 86_400.0)
}

/// Convert a Nix GC size value + unit string (`B`, `KB`, `KiB`, ..., `TiB`) to bytes.
pub fn parse_size_to_bytes(value: f64, unit: &str) -> u64 {
    assert!(value.is_finite(), "size value must be finite: {value}");
    assert!(value >= 0.0, "size value must be non-negative: {value}");

    let multiplier =
        size_unit_multiplier(unit).unwrap_or_else(|| panic!("unrecognized size unit: {unit:?}"));
    #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
    let product = value * (multiplier as f64);
    assert!(product.is_finite(), "computed byte size must be finite");
    assert!(product >= 0.0, "computed byte size must be non-negative");

    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let bytes = product as u64;
    bytes
}

/// Parse a `nix-collect-garbage` summary line such as:
/// `"42 store paths deleted, 12.50 MiB freed"` -> `Some((42, 13_107_200))`.
pub fn parse_nix_gc_freed_line(line: &str) -> Option<(u64, u64)> {
    // Tokenize on ASCII whitespace and search for the sequence:
    //   <u64> "store" "paths" "deleted," <decimal> <unit> "freed"
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.len() < 7 {
        return None;
    }

    for window in tokens.windows(7) {
        let Some(&count_tok) = window.first() else {
            continue;
        };
        let Some(&store_tok) = window.get(1) else {
            continue;
        };
        let Some(&paths_tok) = window.get(2) else {
            continue;
        };
        let Some(&deleted_tok) = window.get(3) else {
            continue;
        };
        let Some(&amount_tok) = window.get(4) else {
            continue;
        };
        let Some(&unit_tok) = window.get(5) else {
            continue;
        };
        let Some(&freed_tok) = window.get(6) else {
            continue;
        };

        if !store_tok.eq_ignore_ascii_case("store")
            || !paths_tok.eq_ignore_ascii_case("paths")
            || !deleted_tok.eq_ignore_ascii_case("deleted,")
        {
            continue;
        }
        // Allow trailing punctuation like `freed.` or `freed` cleanly.
        let freed_prefix_ok = freed_tok.len() >= 5
            && freed_tok
                .get(..5)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("freed"))
            && freed_tok.get(5..).is_some_and(|tail| {
                tail.is_empty() || !tail.chars().next().is_some_and(char::is_alphanumeric)
            });
        if !freed_prefix_ok {
            continue;
        }

        if count_tok.is_empty() || !count_tok.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(paths_deleted) = count_tok.parse::<u64>() else {
            continue;
        };

        if amount_tok.is_empty() || !amount_tok.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
            continue;
        }
        let Ok(amount) = amount_tok.parse::<f64>() else {
            continue;
        };
        if !amount.is_finite() || amount < 0.0 {
            continue;
        }

        if size_unit_multiplier(unit_tok).is_none() {
            continue;
        }
        let freed_bytes = parse_size_to_bytes(amount, unit_tok);
        return Some((paths_deleted, freed_bytes));
    }

    None
}

fn size_unit_multiplier(unit: &str) -> Option<u64> {
    if unit.eq_ignore_ascii_case("B") {
        Some(1)
    } else if unit.eq_ignore_ascii_case("KB") {
        Some(1_000)
    } else if unit.eq_ignore_ascii_case("KIB") {
        Some(1_024)
    } else if unit.eq_ignore_ascii_case("MB") {
        Some(1_000_u64.pow(2))
    } else if unit.eq_ignore_ascii_case("MIB") {
        Some(1_024_u64.pow(2))
    } else if unit.eq_ignore_ascii_case("GB") {
        Some(1_000_u64.pow(3))
    } else if unit.eq_ignore_ascii_case("GIB") {
        Some(1_024_u64.pow(3))
    } else if unit.eq_ignore_ascii_case("TB") {
        Some(1_000_u64.pow(4))
    } else if unit.eq_ignore_ascii_case("TIB") {
        Some(1_024_u64.pow(4))
    } else {
        None
    }
}

fn age_ns_since(now_ns: i128, mtime_ns: i128) -> u128 {
    let diff = now_ns.saturating_sub(mtime_ns);
    if diff <= 0 {
        0
    } else {
        u128::try_from(diff).expect("positive i128 fits u128")
    }
}

/// Convert `now_ns` helper re-exported for callers needing `scan::current_time_ns`.
pub fn default_now_ns() -> i128 {
    scan::current_time_ns()
}
