//! Zero-dependency CLI argument parser and boundary validator.
//!
//! Parses raw `OsString` flags once into either `OrchestratorArgs` or
//! `UserPhaseWorkerArgs` with validated Layer 3 types, rejecting cross-role
//! flag combinations at the edge.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use crate::constants::CLI_ARGS_MAX;
use crate::sys;
use crate::types::{
    AbsPath, AgeDurationNs, CleanupMode, NixGcAgeDays, NonRootAbsPath, SafeProjectsDir,
    ThresholdPercent, TimestampNs, UserName,
};

pub const USAGE: &str = "\
usage: disk-cleanup [OPTIONS]

Clean safe Nix and Rust artifacts when disk usage hits a threshold.

options:
  -h, --help                    Show this help message and exit
  --threshold PERCENT           Disk usage percentage that triggers cleanup (default: 85)
  --user USER                   Target user whose ~/Projects and Nix profile are cleaned
  --projects-dir PATH           Projects directory to scan (default: <user-home>/Projects)
  --result-age-days DAYS        Minimum age in days for Nix result/result-* symlinks (default: 7)
  --target-age-hours HOURS      Minimum idle time in hours before cleaning a Rust target/ dir (default: 24)
  --nix-gc-age-days DAYS        Delete Nix profile generations older than N days (default: 14)
  --tmp-age-hours HOURS         Minimum idle time in hours for /tmp/nix-* dirs on Linux (default: 24)
  --dry-run                     Report what would be cleaned without deleting anything
  --force                       Run cleanup regardless of current disk usage percentage
  --no-notify                   Disable desktop notification
  --verbose                     Log even when disk usage is below the threshold
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorArgs {
    pub threshold: ThresholdPercent,
    pub user: UserName,
    pub projects_dir: Option<SafeProjectsDir>,
    pub result_age: AgeDurationNs,
    pub target_age: AgeDurationNs,
    pub nix_gc_age_days: NixGcAgeDays,
    pub tmp_age: AgeDurationNs,
    pub tmp_dir: NonRootAbsPath,
    pub lock_file: Option<NonRootAbsPath>,
    pub mode: CleanupMode,
    pub force: bool,
    pub no_notify: bool,
    pub verbose: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPhaseWorkerArgs {
    pub user: UserName,
    pub projects_dir: SafeProjectsDir,
    pub result_age: AgeDurationNs,
    pub target_age: AgeDurationNs,
    pub nix_gc_age_days: NixGcAgeDays,
    pub mode: CleanupMode,
    pub now_ns: TimestampNs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Orchestrator(OrchestratorArgs),
    UserPhaseWorker(UserPhaseWorkerArgs),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseOutcome {
    Parsed(CliCommand),
    Help(&'static str),
    Error(String),
}

/// Parse CLI arguments (excluding `argv[0]`), resolving `--user` from the current UID if omitted.
pub fn parse_args(args: &[OsString]) -> ParseOutcome {
    parse_args_with_user_resolver(args, || {
        let uid = sys::real_uid();
        match sys::passwd_by_uid(uid) {
            Ok(Some(entry)) => Ok(entry.name),
            Ok(None) | Err(_) => {
                Err("current UID has no passwd entry; specify --user explicitly".to_string())
            }
        }
    })
}

/// Parse CLI arguments with an injectable current-user resolver (only called if `--user` is omitted).
pub fn parse_args_with_user_resolver<F>(args: &[OsString], resolve_current_user: F) -> ParseOutcome
where
    F: FnOnce() -> Result<UserName, String>,
{
    if args.len() > CLI_ARGS_MAX {
        return ParseOutcome::Error(format!("too many arguments (max {CLI_ARGS_MAX})"));
    }

    let mut threshold: Option<ThresholdPercent> = None;
    let mut user: Option<UserName> = None;
    let mut projects_dir: Option<SafeProjectsDir> = None;
    let mut result_age_days: Option<AgeDurationNs> = None;
    let mut result_age_ns: Option<AgeDurationNs> = None;
    let mut target_age_hours: Option<AgeDurationNs> = None;
    let mut target_age_ns: Option<AgeDurationNs> = None;
    let mut nix_gc_age_days = NixGcAgeDays::DEFAULT;
    let mut tmp_age: Option<AgeDurationNs> = None;
    let mut tmp_dir: Option<NonRootAbsPath> = None;
    let mut lock_file: Option<NonRootAbsPath> = None;
    let mut mode = CleanupMode::Apply;
    let mut force = false;
    let mut no_notify = false;
    let mut verbose = false;
    let mut user_phase = false;
    let mut now_ns: Option<TimestampNs> = None;

    let mut orchestrator_only_flag: Option<&'static str> = None;
    let mut worker_only_flag: Option<&'static str> = None;

    let mut index: usize = 0;
    // Bounded loop: at most `CLI_ARGS_MAX` iterations, index strictly increases each step.
    for _ in 0..args.len() {
        if index >= args.len() {
            break;
        }
        let raw_arg = match args.get(index) {
            Some(arg) => arg,
            None => break,
        };
        let (flag_name, inline_value) = split_flag_and_value(raw_arg);
        index += 1;

        match flag_name.as_str() {
            "-h" | "--help" => return ParseOutcome::Help(USAGE),
            "--dry-run" => {
                if inline_value.is_some() {
                    return ParseOutcome::Error("--dry-run does not take a value".to_string());
                }
                mode = CleanupMode::DryRun;
            }
            "--force" => {
                if inline_value.is_some() {
                    return ParseOutcome::Error("--force does not take a value".to_string());
                }
                orchestrator_only_flag.get_or_insert("--force");
                force = true;
            }
            "--no-notify" => {
                if inline_value.is_some() {
                    return ParseOutcome::Error("--no-notify does not take a value".to_string());
                }
                orchestrator_only_flag.get_or_insert("--no-notify");
                no_notify = true;
            }
            "--verbose" => {
                if inline_value.is_some() {
                    return ParseOutcome::Error("--verbose does not take a value".to_string());
                }
                orchestrator_only_flag.get_or_insert("--verbose");
                verbose = true;
            }
            "--_user-phase" => {
                if inline_value.is_some() {
                    return ParseOutcome::Error("--_user-phase does not take a value".to_string());
                }
                user_phase = true;
            }
            "--threshold" => {
                orchestrator_only_flag.get_or_insert("--threshold");
                let raw = match take_value("--threshold", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                let parsed = match parse_i64_arg("--threshold", &raw)
                    .and_then(ThresholdPercent::try_from_i64)
                {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                threshold = Some(parsed);
            }
            "--user" => {
                let raw = match take_value("--user", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                let text = match raw.into_string() {
                    Ok(val) => val,
                    Err(_) => return ParseOutcome::Error("--user must be valid UTF-8".to_string()),
                };
                let parsed = match UserName::try_new(text) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(format!("--user: {msg}")),
                };
                user = Some(parsed);
            }
            "--projects-dir" => {
                let raw = match take_value("--projects-dir", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                if raw.is_empty() {
                    return ParseOutcome::Error("--projects-dir must be non-empty".to_string());
                }
                let abs = match AbsPath::normalize(&PathBuf::from(raw)) {
                    Ok(path) => path,
                    Err(err) => {
                        return ParseOutcome::Error(format!("invalid --projects-dir: {err}"));
                    }
                };
                let safe = match SafeProjectsDir::try_from_abs(abs) {
                    Ok(path) => path,
                    Err(msg) => return ParseOutcome::Error(format!("--projects-dir {msg}")),
                };
                projects_dir = Some(safe);
            }
            "--result-age-days" => {
                let raw = match take_value("--result-age-days", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                let parsed = match parse_f64_arg("--result-age-days", &raw)
                    .and_then(|v| AgeDurationNs::from_days_f64("--result-age-days", v))
                {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                result_age_days = Some(parsed);
            }
            "--_result-age-ns" => {
                worker_only_flag.get_or_insert("--_result-age-ns");
                let raw = match take_value("--_result-age-ns", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                let parsed = match parse_u128_arg("--_result-age-ns", &raw)
                    .and_then(AgeDurationNs::try_from_ns)
                {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                result_age_ns = Some(parsed);
            }
            "--target-age-hours" => {
                let raw = match take_value("--target-age-hours", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                let parsed = match parse_f64_arg("--target-age-hours", &raw)
                    .and_then(|v| AgeDurationNs::from_hours_f64("--target-age-hours", v))
                {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                target_age_hours = Some(parsed);
            }
            "--_target-age-ns" => {
                worker_only_flag.get_or_insert("--_target-age-ns");
                let raw = match take_value("--_target-age-ns", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                let parsed = match parse_u128_arg("--_target-age-ns", &raw)
                    .and_then(AgeDurationNs::try_from_ns)
                {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                target_age_ns = Some(parsed);
            }
            "--nix-gc-age-days" => {
                let raw = match take_value("--nix-gc-age-days", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                nix_gc_age_days = match parse_i64_arg("--nix-gc-age-days", &raw)
                    .and_then(NixGcAgeDays::try_from_i64)
                {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
            }
            "--tmp-age-hours" => {
                orchestrator_only_flag.get_or_insert("--tmp-age-hours");
                let raw = match take_value("--tmp-age-hours", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                let parsed = match parse_f64_arg("--tmp-age-hours", &raw)
                    .and_then(|v| AgeDurationNs::from_hours_f64("--tmp-age-hours", v))
                {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                tmp_age = Some(parsed);
            }
            "--tmp-dir" => {
                orchestrator_only_flag.get_or_insert("--tmp-dir");
                let raw = match take_value("--tmp-dir", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                if raw.is_empty() {
                    return ParseOutcome::Error("--tmp-dir must be non-empty".to_string());
                }
                let parsed = match NonRootAbsPath::normalize(&PathBuf::from(raw)) {
                    Ok(path) => path,
                    Err(err) => return ParseOutcome::Error(format!("invalid --tmp-dir: {err}")),
                };
                tmp_dir = Some(parsed);
            }
            "--lock-file" => {
                orchestrator_only_flag.get_or_insert("--lock-file");
                let raw = match take_value("--lock-file", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                if raw.is_empty() {
                    return ParseOutcome::Error("--lock-file must be non-empty".to_string());
                }
                let parsed = match NonRootAbsPath::normalize(&PathBuf::from(raw)) {
                    Ok(path) => path,
                    Err(err) => return ParseOutcome::Error(format!("invalid --lock-file: {err}")),
                };
                lock_file = Some(parsed);
            }
            "--_now-ns" => {
                worker_only_flag.get_or_insert("--_now-ns");
                let raw = match take_value("--_now-ns", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                let parsed = match parse_i128_arg("--_now-ns", &raw).and_then(|val| {
                    TimestampNs::try_new(val).map_err(|_| {
                        format!(
                            "--_now-ns must be an integer in [1, {}]",
                            crate::constants::NOW_NS_MAX
                        )
                    })
                }) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                now_ns = Some(parsed);
            }
            unknown => {
                return ParseOutcome::Error(format!("unrecognized argument: {unknown}"));
            }
        }
    }

    if user_phase {
        if let Some(flag) = orchestrator_only_flag {
            return ParseOutcome::Error(format!("{flag} is not valid with --_user-phase"));
        }
        if result_age_days.is_some() && result_age_ns.is_some() {
            return ParseOutcome::Error(
                "--result-age-days and --_result-age-ns cannot both be specified".to_string(),
            );
        }
        if target_age_hours.is_some() && target_age_ns.is_some() {
            return ParseOutcome::Error(
                "--target-age-hours and --_target-age-ns cannot both be specified".to_string(),
            );
        }
        let Some(worker_projects_dir) = projects_dir else {
            return ParseOutcome::Error("--_user-phase requires --projects-dir".to_string());
        };
        let Some(worker_now_ns) = now_ns else {
            return ParseOutcome::Error("--_user-phase requires --_now-ns".to_string());
        };
        let resolved_user = match user {
            Some(name) => name,
            None => match resolve_current_user() {
                Ok(name) => name,
                Err(msg) => return ParseOutcome::Error(msg),
            },
        };
        return ParseOutcome::Parsed(CliCommand::UserPhaseWorker(UserPhaseWorkerArgs {
            user: resolved_user,
            projects_dir: worker_projects_dir,
            result_age: result_age_ns
                .or(result_age_days)
                .unwrap_or(AgeDurationNs::DEFAULT_RESULT),
            target_age: target_age_ns
                .or(target_age_hours)
                .unwrap_or(AgeDurationNs::DEFAULT_TARGET),
            nix_gc_age_days,
            mode,
            now_ns: worker_now_ns,
        }));
    }

    if let Some(flag) = worker_only_flag {
        return ParseOutcome::Error(format!("{flag} is only valid with --_user-phase"));
    }

    let resolved_user = match user {
        Some(name) => name,
        None => match resolve_current_user() {
            Ok(name) => name,
            Err(msg) => return ParseOutcome::Error(msg),
        },
    };
    let resolved_tmp_dir = match tmp_dir {
        Some(path) => path,
        None => NonRootAbsPath::from_absolute(std::path::Path::new("/tmp"))
            .expect("/tmp is a valid non-root absolute path"),
    };

    ParseOutcome::Parsed(CliCommand::Orchestrator(OrchestratorArgs {
        threshold: threshold.unwrap_or(ThresholdPercent::DEFAULT),
        user: resolved_user,
        projects_dir,
        result_age: result_age_days.unwrap_or(AgeDurationNs::DEFAULT_RESULT),
        target_age: target_age_hours.unwrap_or(AgeDurationNs::DEFAULT_TARGET),
        nix_gc_age_days,
        tmp_age: tmp_age.unwrap_or(AgeDurationNs::DEFAULT_TMP),
        tmp_dir: resolved_tmp_dir,
        lock_file,
        mode,
        force,
        no_notify,
        verbose,
    }))
}

fn split_flag_and_value(raw_arg: &OsString) -> (String, Option<OsString>) {
    let bytes = raw_arg.as_bytes();
    if bytes.starts_with(b"--")
        && let Some(eq_pos) = bytes.iter().position(|&b| b == b'=')
    {
        let name_bytes = bytes.get(..eq_pos).expect("eq_pos within slice");
        let val_bytes = bytes.get(eq_pos + 1..).expect("eq_pos + 1 within slice");
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        let value = std::ffi::OsStr::from_bytes(val_bytes).to_os_string();
        return (name, Some(value));
    }
    (raw_arg.to_string_lossy().into_owned(), None)
}

fn take_value(
    flag: &str,
    inline_value: Option<OsString>,
    args: &[OsString],
    index: &mut usize,
) -> Result<OsString, String> {
    assert!(!flag.is_empty(), "flag name must be non-empty");
    assert!(*index <= args.len(), "argument index out of bounds");
    if let Some(val) = inline_value {
        return Ok(val);
    }
    let Some(next) = args.get(*index) else {
        return Err(format!("option {flag} requires a value"));
    };
    *index += 1;
    assert!(*index <= args.len());
    Ok(next.clone())
}

fn parse_i64_arg(flag: &str, raw: &OsString) -> Result<i64, String> {
    let text = raw
        .to_str()
        .ok_or_else(|| format!("invalid integer for {flag}"))?;
    text.parse::<i64>()
        .map_err(|_| format!("invalid integer for {flag}: {text:?}"))
}

fn parse_i128_arg(flag: &str, raw: &OsString) -> Result<i128, String> {
    let text = raw
        .to_str()
        .ok_or_else(|| format!("invalid integer for {flag}"))?;
    text.parse::<i128>()
        .map_err(|_| format!("invalid integer for {flag}: {text:?}"))
}

fn parse_u128_arg(flag: &str, raw: &OsString) -> Result<u128, String> {
    let text = raw
        .to_str()
        .ok_or_else(|| format!("invalid integer for {flag}"))?;
    text.parse::<u128>()
        .map_err(|_| format!("invalid integer for {flag}: {text:?}"))
}

fn parse_f64_arg(flag: &str, raw: &OsString) -> Result<f64, String> {
    let text = raw
        .to_str()
        .ok_or_else(|| format!("invalid number for {flag}"))?;
    text.parse::<f64>()
        .map_err(|_| format!("invalid number for {flag}: {text:?}"))
}
