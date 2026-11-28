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
mod types;

use std::env;
use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::cleanup::{
    TmpCleanupStats, TriggerDecision, UserPhaseConfig, build_notification, build_summary_parts,
    clean_nix_tmp_dirs, decide_trigger, format_bytes, log, remaining_budget, run_nix_gc,
    run_user_phase, run_user_phase_in_process, send_notification,
};
use crate::cli::{CliCommand, OrchestratorArgs, ParseOutcome, UserPhaseWorkerArgs};
use crate::constants::{NIX_GC_TIMEOUT_SECONDS, TOTAL_RUN_TIMEOUT_SECONDS};
use crate::scan::{default_lock_path, stat_filesystem, worst_disk_usage};
use crate::sys::{AdvisoryLock, ExecutionPrivilege};
use crate::types::{AbsPath, BoundedErrors, CleanupMode, SafeProjectsDir, TimestampNs};

fn main() -> ExitCode {
    let args_os: Vec<OsString> = env::args_os().skip(1).collect();
    let code = run(&args_os);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn run(args_os: &[OsString]) -> i32 {
    let command = match cli::parse_args(args_os) {
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

    let target_user = match &command {
        CliCommand::Orchestrator(args) => &args.user,
        CliCommand::UserPhaseWorker(args) => &args.user,
    };

    let passwd = match sys::passwd_by_name(target_user) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            log(&format!("error: unknown user {:?}", target_user.as_str()));
            return 2;
        }
        Err(err) => {
            log(&format!(
                "error: failed to look up user {:?}: {err}",
                target_user.as_str()
            ));
            return 2;
        }
    };

    let privilege = match ExecutionPrivilege::resolve(passwd) {
        Ok(resolved) => resolved,
        Err(msg) => {
            log(&format!("error: {msg}"));
            return 2;
        }
    };

    let deadline = Instant::now() + Duration::from_secs(TOTAL_RUN_TIMEOUT_SECONDS);

    match command {
        CliCommand::UserPhaseWorker(worker_args) => {
            run_user_phase_entrypoint(&privilege, &worker_args, deadline)
        }
        CliCommand::Orchestrator(orch_args) => {
            let projects_dir = match &orch_args.projects_dir {
                Some(custom) => custom.clone(),
                None => {
                    let default_abs =
                        match privilege.target_home().join_relative(Path::new("Projects")) {
                            Ok(path) => path,
                            Err(err) => {
                                log(&format!("error: invalid default Projects path: {err}"));
                                return 2;
                            }
                        };
                    match SafeProjectsDir::try_from_abs(default_abs) {
                        Ok(safe) => safe,
                        Err(msg) => {
                            log(&format!("error: invalid default Projects path: {msg}"));
                            return 2;
                        }
                    }
                }
            };

            let lock_path = orch_args
                .lock_file
                .clone()
                .unwrap_or_else(default_lock_path);

            let lock_guard = match AdvisoryLock::try_acquire(&lock_path) {
                Ok(Some(guard)) => guard,
                Ok(None) => {
                    if orch_args.verbose || orch_args.mode.is_dry_run() || orch_args.force {
                        log(&format!(
                            "another disk-cleanup run is already in progress ({}); skipping",
                            lock_path.display()
                        ));
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

            let now_ns = TimestampNs::now();
            run_cleanup_orchestrator(
                &orch_args,
                &privilege,
                &projects_dir,
                &lock_guard,
                now_ns,
                deadline,
            )
        }
    }
}

fn run_user_phase_entrypoint(
    privilege: &ExecutionPrivilege,
    args: &UserPhaseWorkerArgs,
    deadline: Instant,
) -> i32 {
    let config = UserPhaseConfig {
        projects_dir: &args.projects_dir,
        result_age: args.result_age,
        target_age: args.target_age,
        nix_gc_age_days: args.nix_gc_age_days,
        mode: args.mode,
        now_ns: args.now_ns,
        deadline,
    };
    let stats =
        run_user_phase_in_process(privilege.target_name(), privilege.target_home(), &config);
    let encoded = ipc::encode_user_phase_stats_json(&stats);
    println!("{encoded}");
    0
}

fn run_cleanup_orchestrator(
    args: &OrchestratorArgs,
    privilege: &ExecutionPrivilege,
    projects_dir: &SafeProjectsDir,
    _lock_guard: &AdvisoryLock,
    now_ns: TimestampNs,
    deadline: Instant,
) -> i32 {
    let primary_watch = if projects_dir.as_path().exists() {
        projects_dir.as_abs().clone()
    } else {
        privilege.target_home().clone()
    };
    let nix_path = AbsPath::from_absolute(Path::new("/nix")).expect("/nix is valid AbsPath");
    let watch_paths = [primary_watch, nix_path];
    let before = match worst_disk_usage(&watch_paths) {
        Ok(usage) => usage,
        Err(err) => {
            log(&format!("error: failed to inspect disk usage: {err}"));
            return 1;
        }
    };

    let trigger = match decide_trigger(&before, args.threshold, args.force) {
        TriggerDecision::SkipBelowThreshold => {
            if args.verbose || args.mode.is_dry_run() {
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

    let mode_label = match args.mode {
        CleanupMode::DryRun => "dry-run",
        CleanupMode::Apply => "cleanup",
    };
    log(&format!(
        "starting {mode_label} ({trigger}, {} free on {})",
        format_bytes(before.available_bytes),
        before.path.display(),
    ));

    // Phase 1 (dropped to target user): unpin stale result* symlinks, delete
    // idle Rust target/ dirs (giving Nix SQLite room if disk is nearly full),
    // and prune per-user Nix profile generations.
    let user_phase_config = UserPhaseConfig {
        projects_dir,
        result_age: args.result_age,
        target_age: args.target_age,
        nix_gc_age_days: args.nix_gc_age_days,
        mode: args.mode,
        now_ns,
        deadline,
    };
    let user_stats = run_user_phase(privilege, &user_phase_config);

    // Phase 2a (Linux only): clean stale /tmp/nix-* shell/build dirs before GC.
    let owner_filter_uid = if privilege.is_root() {
        None
    } else {
        Some(privilege.target_uid())
    };
    let tmp_stats = if cfg!(target_os = "linux") {
        clean_nix_tmp_dirs(
            &args.tmp_dir,
            args.tmp_age,
            args.mode,
            now_ns,
            owner_filter_uid,
        )
    } else {
        TmpCleanupStats::default()
    };

    // Phase 2b: prune system generations (when root) and collect dead store paths.
    let nix_gc_timeout = remaining_budget(deadline, Duration::from_secs(NIX_GC_TIMEOUT_SECONDS));
    let nix_outcome = run_nix_gc(
        args.nix_gc_age_days,
        args.mode,
        privilege.is_root(),
        nix_gc_timeout,
    );

    let mut all_errors = BoundedErrors::new();
    all_errors.extend_from(&user_stats.errors);
    all_errors.extend_from(&tmp_stats.errors);
    all_errors.extend_from(nix_outcome.errors());

    for error_text in all_errors.iter() {
        log(&format!("error: {error_text}"));
    }
    if all_errors.dropped_count() > 0 {
        log(&format!(
            "warning: {} additional error(s) omitted after reaching cap",
            all_errors.dropped_count()
        ));
    }

    if args.mode.is_dry_run() {
        let estimated_bytes = user_stats
            .targets_removed
            .freed_bytes()
            .saturating_add(tmp_stats.dirs_removed.freed_bytes());
        log(&format!(
            "[dry-run] summary: {} stale result link(s), {} Rust target/ dir(s) ({}), \
             {} /tmp/nix-* dir(s) ({}), {} currently dead Nix store path(s); \
             skipped {} locked, {} recent, and {} unreadable target/ dir(s); \
             estimated non-GC reclaim: {}",
            user_stats.result_links_removed_count,
            user_stats.targets_removed.count(),
            format_bytes(user_stats.targets_removed.freed_bytes()),
            tmp_stats.dirs_removed.count(),
            format_bytes(tmp_stats.dirs_removed.freed_bytes()),
            nix_outcome.dead_paths_count(),
            user_stats.targets_skipped_active_count,
            user_stats.targets_skipped_recent_count,
            user_stats.targets_skipped_unreadable_count,
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
        .targets_removed
        .freed_bytes()
        .saturating_add(tmp_stats.dirs_removed.freed_bytes())
        .saturating_add(nix_outcome.reclaimed().freed_bytes());
    let summary_parts = build_summary_parts(&user_stats, &nix_outcome, &tmp_stats);

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
        send_notification(privilege, &notification);
    }

    if all_errors.is_empty() { 0 } else { 1 }
}

#[cfg(test)]
mod tests;
