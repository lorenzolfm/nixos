//! Threshold-triggered cleanup for safe Nix and Rust build artifacts.
//!
//! When disk usage reaches the configured threshold (default 85%), this binary
//! reclaims space across four safe targets:
//!   1. Stale Nix `result` / `result-*` symlinks under `~/Projects` (unpins GC roots)
//!   2. Old Rust `target/` build directories under `~/Projects` (oldest first,
//!      skipping locked or recently active builds)
//!   3. Stale `/tmp/nix-*` shell/build directories older than 24h (Linux only)
//!   4. Nix profile generation pruning + `nix-collect-garbage`
//!
//! When invoked as root (via systemd or launchd), all operations inside the
//! target user's home directory and desktop notifications run dropped to that
//! user's UID/GID.

mod cleanup;
mod cli;
mod constants;
mod ipc;
mod process;
mod scan;
mod sys;

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::cleanup::{
    TmpCleanupStats, TriggerDecision, UserPhaseConfig, append_error, build_notification,
    build_summary_parts, clean_nix_tmp_dirs, decide_trigger, format_bytes, log, remaining_budget,
    run_nix_gc, run_user_phase, run_user_phase_in_process, send_notification,
};
use crate::cli::{CliArgs, ParseOutcome};
use crate::constants::{NIX_GC_TIMEOUT_SECONDS, TOTAL_RUN_TIMEOUT_SECONDS};
use crate::scan::{default_lock_path, stat_filesystem, worst_disk_usage};
use crate::sys::{AdvisoryLock, PasswdEntry};

fn main() -> ExitCode {
    let args_os: Vec<OsString> = env::args_os().skip(1).collect();
    let code = run(&args_os);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn run(args_os: &[OsString]) -> i32 {
    let args = match cli::parse_args(args_os) {
        ParseOutcome::Parsed(parsed) => parsed,
        ParseOutcome::Help(text) => {
            print!("{text}");
            return 0;
        }
        ParseOutcome::Error(message) => {
            eprintln!("disk-cleanup: error: {message}");
            return 2;
        }
    };

    let passwd = match sys::passwd_by_name(&args.user) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            log(&format!("error: unknown user {:?}", args.user));
            return 2;
        }
        Err(err) => {
            log(&format!(
                "error: failed to look up user {:?}: {err}",
                args.user
            ));
            return 2;
        }
    };

    let user_home = match sys::absolutize_path(&passwd.home_dir) {
        Ok(path) => path,
        Err(err) => {
            log(&format!("error: invalid user home directory: {err}"));
            return 2;
        }
    };
    let projects_dir = match &args.projects_dir {
        Some(custom) => match sys::absolutize_path(custom) {
            Ok(path) => path,
            Err(err) => {
                log(&format!("error: invalid --projects-dir: {err}"));
                return 2;
            }
        },
        None => user_home.join("Projects"),
    };
    let tmp_dir = match sys::absolutize_path(&args.tmp_dir) {
        Ok(path) => path,
        Err(err) => {
            log(&format!("error: invalid --tmp-dir: {err}"));
            return 2;
        }
    };

    let now_ns = args.now_ns.unwrap_or_else(cleanup::default_now_ns);
    let deadline = Instant::now() + Duration::from_secs(TOTAL_RUN_TIMEOUT_SECONDS);

    assert!(user_home.is_absolute(), "user_home must be absolute");
    assert!(projects_dir.is_absolute(), "projects_dir must be absolute");
    assert!(tmp_dir.is_absolute(), "tmp_dir must be absolute");
    assert!(now_ns > 0, "now_ns must be positive");

    if args.user_phase {
        return run_user_phase_entrypoint(
            &passwd,
            &user_home,
            &projects_dir,
            &args,
            now_ns,
            deadline,
        );
    }

    let euid = sys::effective_uid();
    if euid != 0 && passwd.uid != euid {
        log(&format!(
            "error: --user {} requires root privileges",
            args.user
        ));
        return 2;
    }

    let lock_path = match &args.lock_file {
        Some(custom) => match sys::absolutize_path(custom) {
            Ok(path) => path,
            Err(err) => {
                log(&format!("error: invalid --lock-file: {err}"));
                return 2;
            }
        },
        None => default_lock_path(&user_home),
    };

    let _lock_guard = match AdvisoryLock::try_acquire(&lock_path) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            if args.verbose || args.dry_run || args.force {
                log("another disk-cleanup run is already in progress; skipping");
            }
            return 0;
        }
        Err(err) => {
            log(&format!(
                "error: failed to acquire lockfile {}: {err}",
                lock_path.display()
            ));
            return 1;
        }
    };

    run_cleanup_orchestrator(
        &args,
        &passwd,
        &user_home,
        &projects_dir,
        &tmp_dir,
        now_ns,
        deadline,
    )
}

fn run_user_phase_entrypoint(
    passwd: &PasswdEntry,
    user_home: &Path,
    projects_dir: &Path,
    args: &CliArgs,
    now_ns: i128,
    deadline: Instant,
) -> i32 {
    assert!(!passwd.name.is_empty(), "passwd.name must be non-empty");
    let euid = sys::effective_uid();
    if euid != 0 {
        assert_eq!(
            passwd.uid, euid,
            "privilege-dropped child euid must match target user"
        );
    }

    let config = UserPhaseConfig {
        projects_dir,
        result_age_days: args.result_age_days,
        target_age_hours: args.target_age_hours,
        nix_gc_age_days: args.nix_gc_age_days,
        dry_run: args.dry_run,
        now_ns,
        deadline,
    };
    let stats = run_user_phase_in_process(&passwd.name, user_home, &config);
    ipc::assert_user_phase_stats_valid(&stats);
    let encoded = ipc::encode_user_phase_stats_json(&stats);
    println!("{encoded}");
    0
}

fn run_cleanup_orchestrator(
    args: &CliArgs,
    passwd: &PasswdEntry,
    user_home: &Path,
    projects_dir: &Path,
    tmp_dir: &Path,
    now_ns: i128,
    deadline: Instant,
) -> i32 {
    assert!((1..=100).contains(&args.threshold));
    assert!(now_ns > 0, "now_ns must be positive");

    let primary_watch = if projects_dir.exists() {
        projects_dir.to_path_buf()
    } else {
        user_home.to_path_buf()
    };
    let watch_paths = [primary_watch, PathBuf::from("/nix")];
    let before = match worst_disk_usage(&watch_paths) {
        Ok(usage) => usage,
        Err(err) => {
            log(&format!("error: failed to inspect disk usage: {err}"));
            return 1;
        }
    };

    let trigger = match decide_trigger(&before, args.threshold, args.force) {
        TriggerDecision::SkipBelowThreshold => {
            if args.verbose || args.dry_run {
                log(&format!(
                    "disk usage {}% < {}% threshold ({} free on {}); nothing to do",
                    before.used_percent,
                    args.threshold,
                    format_bytes(before.available_bytes),
                    before.path.display(),
                ));
            }
            return 0;
        }
        TriggerDecision::Run {
            trigger_description,
        } => trigger_description,
    };

    let mode = if args.dry_run { "dry-run" } else { "cleanup" };
    log(&format!(
        "starting {mode} ({trigger}, {} free on {})",
        format_bytes(before.available_bytes),
        before.path.display(),
    ));

    // Phase 1 (dropped to target user): unpin stale result* symlinks, delete
    // idle Rust target/ dirs (giving Nix SQLite room if disk is nearly full),
    // and prune per-user Nix profile generations.
    let user_phase_config = UserPhaseConfig {
        projects_dir,
        result_age_days: args.result_age_days,
        target_age_hours: args.target_age_hours,
        nix_gc_age_days: args.nix_gc_age_days,
        dry_run: args.dry_run,
        now_ns,
        deadline,
    };
    let user_stats = run_user_phase(passwd, user_home, &user_phase_config);

    // Phase 2a (Linux only): clean stale /tmp/nix-* shell/build dirs before GC.
    let tmp_stats = if cfg!(target_os = "linux") {
        clean_nix_tmp_dirs(tmp_dir, args.tmp_age_hours, args.dry_run, now_ns)
    } else {
        TmpCleanupStats::default()
    };

    // Phase 2b: prune system generations (when root) and collect dead store paths.
    let nix_gc_timeout = remaining_budget(deadline, Duration::from_secs(NIX_GC_TIMEOUT_SECONDS));
    let nix_stats = run_nix_gc(args.nix_gc_age_days, args.dry_run, nix_gc_timeout);

    let mut all_errors: Vec<String> = Vec::new();
    for source_errors in [&user_stats.errors, &tmp_stats.errors, &nix_stats.errors] {
        for error_text in source_errors {
            append_error(&mut all_errors, error_text.clone());
        }
    }
    for error_text in &all_errors {
        log(&format!("error: {error_text}"));
    }

    if args.dry_run {
        let estimated_bytes = user_stats
            .targets_freed_bytes
            .saturating_add(tmp_stats.freed_bytes);
        log(&format!(
            "[dry-run] summary: {} stale result link(s), {} Rust target/ dir(s) ({}), \
             {} /tmp/nix-* dir(s) ({}), {} currently dead Nix store path(s); \
             skipped {} locked and {} recent target/ dir(s); \
             estimated non-GC reclaim: {}",
            user_stats.result_links_removed_count,
            user_stats.targets_removed_count,
            format_bytes(user_stats.targets_freed_bytes),
            tmp_stats.dirs_removed_count,
            format_bytes(tmp_stats.freed_bytes),
            nix_stats.dead_paths_count,
            user_stats.targets_skipped_active_count,
            user_stats.targets_skipped_recent_count,
            format_bytes(estimated_bytes),
        ));
        return if all_errors.is_empty() { 0 } else { 1 };
    }

    let after = match stat_filesystem(&before.path) {
        Ok(usage) => usage,
        Err(err) => {
            log(&format!(
                "warning: post-cleanup statvfs failed for {}: {err}",
                before.path.display()
            ));
            before.clone()
        }
    };

    let net_freed_bytes = after.available_bytes.saturating_sub(before.available_bytes);
    let tracked_freed_bytes = user_stats
        .targets_freed_bytes
        .saturating_add(tmp_stats.freed_bytes)
        .saturating_add(nix_stats.freed_bytes);
    let summary_parts = build_summary_parts(&user_stats, &nix_stats, &tmp_stats);

    if summary_parts.is_empty() {
        log(&format!(
            "finished: nothing eligible to clean (disk still {}%, {} free)",
            after.used_percent,
            format_bytes(after.available_bytes),
        ));
    } else {
        log(&format!(
            "finished: {}% -> {}% (net freed {}, {} free); cleaned: {}",
            before.used_percent,
            after.used_percent,
            format_bytes(net_freed_bytes),
            format_bytes(after.available_bytes),
            summary_parts.join(", "),
        ));
    }

    if !args.no_notify
        && let Some(notification) =
            build_notification(&before, &after, tracked_freed_bytes, &summary_parts)
    {
        send_notification(passwd, &notification.title, &notification.body);
    }

    if all_errors.is_empty() { 0 } else { 1 }
}

#[cfg(test)]
mod tests;
