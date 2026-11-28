//! Zero-dependency CLI argument parser and boundary validator.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::constants::{AGE_DAYS_MAX, AGE_HOURS_MAX, CLI_ARGS_MAX, NOW_NS_MAX};
use crate::sys;

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

#[derive(Debug, Clone, PartialEq)]
pub struct CliArgs {
    pub threshold: u8,
    pub user: String,
    pub projects_dir: Option<PathBuf>,
    pub result_age_days: f64,
    pub target_age_hours: f64,
    pub nix_gc_age_days: u32,
    pub tmp_age_hours: f64,
    pub tmp_dir: PathBuf,
    pub lock_file: Option<PathBuf>,
    pub dry_run: bool,
    pub force: bool,
    pub no_notify: bool,
    pub verbose: bool,
    pub user_phase: bool,
    pub now_ns: Option<i128>,
}

#[derive(Debug, PartialEq)]
pub enum ParseOutcome {
    Parsed(CliArgs),
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
    F: FnOnce() -> Result<String, String>,
{
    if args.len() > CLI_ARGS_MAX {
        return ParseOutcome::Error(format!("too many arguments (max {CLI_ARGS_MAX})"));
    }

    let mut threshold: i64 = 85;
    let mut user: Option<String> = None;
    let mut projects_dir: Option<PathBuf> = None;
    let mut result_age_days: f64 = 7.0;
    let mut target_age_hours: f64 = 24.0;
    let mut nix_gc_age_days: i64 = 14;
    let mut tmp_age_hours: f64 = 24.0;
    let mut tmp_dir = PathBuf::from("/tmp");
    let mut lock_file: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut force = false;
    let mut no_notify = false;
    let mut verbose = false;
    let mut user_phase = false;
    let mut now_ns: Option<i128> = None;

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
                dry_run = true;
            }
            "--force" => {
                if inline_value.is_some() {
                    return ParseOutcome::Error("--force does not take a value".to_string());
                }
                force = true;
            }
            "--no-notify" => {
                if inline_value.is_some() {
                    return ParseOutcome::Error("--no-notify does not take a value".to_string());
                }
                no_notify = true;
            }
            "--verbose" => {
                if inline_value.is_some() {
                    return ParseOutcome::Error("--verbose does not take a value".to_string());
                }
                verbose = true;
            }
            "--_user-phase" => {
                if inline_value.is_some() {
                    return ParseOutcome::Error("--_user-phase does not take a value".to_string());
                }
                user_phase = true;
            }
            "--threshold" => {
                let raw = match take_value("--threshold", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                threshold = match parse_i64_arg("--threshold", &raw) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
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
                if text.is_empty() {
                    return ParseOutcome::Error("--user must be non-empty".to_string());
                }
                user = Some(text);
            }
            "--projects-dir" => {
                let raw = match take_value("--projects-dir", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                if raw.is_empty() {
                    return ParseOutcome::Error("--projects-dir must be non-empty".to_string());
                }
                projects_dir = Some(PathBuf::from(raw));
            }
            "--result-age-days" => {
                let raw = match take_value("--result-age-days", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                result_age_days = match parse_f64_arg("--result-age-days", &raw) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
            }
            "--target-age-hours" => {
                let raw = match take_value("--target-age-hours", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                target_age_hours = match parse_f64_arg("--target-age-hours", &raw) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
            }
            "--nix-gc-age-days" => {
                let raw = match take_value("--nix-gc-age-days", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                nix_gc_age_days = match parse_i64_arg("--nix-gc-age-days", &raw) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
            }
            "--tmp-age-hours" => {
                let raw = match take_value("--tmp-age-hours", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                tmp_age_hours = match parse_f64_arg("--tmp-age-hours", &raw) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
            }
            "--tmp-dir" => {
                let raw = match take_value("--tmp-dir", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                if raw.is_empty() {
                    return ParseOutcome::Error("--tmp-dir must be non-empty".to_string());
                }
                tmp_dir = PathBuf::from(raw);
            }
            "--lock-file" => {
                let raw = match take_value("--lock-file", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                if raw.is_empty() {
                    return ParseOutcome::Error("--lock-file must be non-empty".to_string());
                }
                lock_file = Some(PathBuf::from(raw));
            }
            "--_now-ns" => {
                let raw = match take_value("--_now-ns", inline_value, args, &mut index) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                };
                now_ns = Some(match parse_i128_arg("--_now-ns", &raw) {
                    Ok(val) => val,
                    Err(msg) => return ParseOutcome::Error(msg),
                });
            }
            unknown => {
                return ParseOutcome::Error(format!("unrecognized argument: {unknown}"));
            }
        }
    }

    let resolved_user = match user {
        Some(name) => name,
        None => match resolve_current_user() {
            Ok(name) => name,
            Err(msg) => return ParseOutcome::Error(msg),
        },
    };
    if resolved_user.is_empty() {
        return ParseOutcome::Error("--user must be non-empty".to_string());
    }

    let threshold_u8 = if (1..=100).contains(&threshold) {
        u8::try_from(threshold).expect("validated threshold in 1..=100 fits u8")
    } else {
        return ParseOutcome::Error("--threshold must be between 1 and 100".to_string());
    };

    if let Err(msg) = validate_f64_range("--result-age-days", result_age_days, AGE_DAYS_MAX) {
        return ParseOutcome::Error(msg);
    }
    if let Err(msg) = validate_f64_range("--target-age-hours", target_age_hours, AGE_HOURS_MAX) {
        return ParseOutcome::Error(msg);
    }
    let nix_gc_age_days_u32 = if (0..=i64::from(AGE_DAYS_MAX)).contains(&nix_gc_age_days) {
        u32::try_from(nix_gc_age_days).expect("validated nix_gc_age_days fits u32")
    } else {
        return ParseOutcome::Error(format!(
            "--nix-gc-age-days must be an integer in [0, {AGE_DAYS_MAX}]"
        ));
    };
    if let Err(msg) = validate_f64_range("--tmp-age-hours", tmp_age_hours, AGE_HOURS_MAX) {
        return ParseOutcome::Error(msg);
    }

    if let Some(now_val) = now_ns {
        let valid_now = (1..=NOW_NS_MAX).contains(&now_val);
        if !valid_now {
            return ParseOutcome::Error(format!(
                "--_now-ns must be an integer in [1, {NOW_NS_MAX}]"
            ));
        }
    }

    if let Some(ref proj_dir) = projects_dir {
        let resolved_projects = match sys::absolutize_path(proj_dir) {
            Ok(path) => path,
            Err(err) => {
                return ParseOutcome::Error(format!("invalid --projects-dir: {err}"));
            }
        };
        if sys::is_forbidden_delete_path(&resolved_projects) {
            return ParseOutcome::Error(format!(
                "--projects-dir cannot be system root {}",
                resolved_projects.display()
            ));
        }
    }

    let resolved_tmp = match sys::absolutize_path(&tmp_dir) {
        Ok(path) => path,
        Err(err) => return ParseOutcome::Error(format!("invalid --tmp-dir: {err}")),
    };
    if resolved_tmp == Path::new("/") {
        return ParseOutcome::Error("--tmp-dir cannot be /".to_string());
    }

    assert!(threshold_u8 >= 1);
    assert!(threshold_u8 <= 100);
    assert!(result_age_days.is_finite());
    assert!(result_age_days >= 0.0);
    assert!(target_age_hours.is_finite());
    assert!(target_age_hours >= 0.0);
    assert!(nix_gc_age_days_u32 <= AGE_DAYS_MAX);
    assert!(tmp_age_hours.is_finite());
    assert!(tmp_age_hours >= 0.0);
    if let Some(ns) = now_ns {
        assert!(ns > 0);
        assert!(ns <= NOW_NS_MAX);
    }
    assert!(!resolved_user.is_empty());

    ParseOutcome::Parsed(CliArgs {
        threshold: threshold_u8,
        user: resolved_user,
        projects_dir,
        result_age_days,
        target_age_hours,
        nix_gc_age_days: nix_gc_age_days_u32,
        tmp_age_hours,
        tmp_dir,
        lock_file,
        dry_run,
        force,
        no_notify,
        verbose,
        user_phase,
        now_ns,
    })
}

fn validate_f64_range(flag: &str, value: f64, max: u32) -> Result<(), String> {
    assert!(!flag.is_empty(), "flag name must be non-empty");
    assert!(max > 0, "max bound must be positive");
    let in_range = value.is_finite() && (0.0..=f64::from(max)).contains(&value);
    if in_range {
        Ok(())
    } else {
        Err(format!("{flag} must be a finite number in [0, {max}]"))
    }
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
    assert!(!flag.is_empty(), "flag name must be non-empty");
    let text = raw
        .to_str()
        .ok_or_else(|| format!("invalid integer for {flag}"))?;
    text.parse::<i64>()
        .map_err(|_| format!("invalid integer for {flag}: {text:?}"))
}

fn parse_i128_arg(flag: &str, raw: &OsString) -> Result<i128, String> {
    assert!(!flag.is_empty(), "flag name must be non-empty");
    let text = raw
        .to_str()
        .ok_or_else(|| format!("invalid integer for {flag}"))?;
    text.parse::<i128>()
        .map_err(|_| format!("invalid integer for {flag}: {text:?}"))
}

fn parse_f64_arg(flag: &str, raw: &OsString) -> Result<f64, String> {
    assert!(!flag.is_empty(), "flag name must be non-empty");
    let text = raw
        .to_str()
        .ok_or_else(|| format!("invalid number for {flag}"))?;
    text.parse::<f64>()
        .map_err(|_| format!("invalid number for {flag}: {text:?}"))
}
