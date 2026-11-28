//! End-to-end integration tests exercising the compiled `disk-cleanup` binary.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const CACHEDIR_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n";
static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "disk-cleanup-it-{}-{label}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        Self {
            path: fs::canonicalize(&dir).expect("canonicalize temp dir"),
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn write_executable(path: &Path, script: &str) {
    fs::write(path, script).expect("write script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod +x");
}

fn set_mtime_secs(path: &Path, epoch_secs: i64) {
    let ts = libc::timespec {
        tv_sec: libc::time_t::try_from(epoch_secs).expect("tv_sec fits time_t"),
        tv_nsec: 0,
    };
    let times = [ts, ts];
    let c_path = CString::new(path.as_os_str().as_bytes()).expect("no NUL");
    // SAFETY: `c_path` is NUL-terminated and `times` is a valid 2-element array.
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    assert_eq!(rc, 0, "utimensat failed on {}", path.display());
}

fn now_epoch_secs() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();
    i64::try_from(secs).expect("epoch secs fit i64")
}

#[test]
fn cli_help_and_invalid_args_exit_codes() {
    let bin = env!("CARGO_BIN_EXE_disk-cleanup");

    let help = Command::new(bin)
        .arg("--help")
        .output()
        .expect("run --help");
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("usage: disk-cleanup"));

    let bad = Command::new(bin)
        .args(["--threshold", "0"])
        .output()
        .expect("run invalid");
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn cli_force_cleanup_and_notification_warning_end_to_end() {
    let bin = env!("CARGO_BIN_EXE_disk-cleanup");
    let fx = TempDir::new("e2e-force");
    let projects = fx.path.join("Projects");
    let fake_tmp = fx.path.join("fake-tmp");
    let fake_bin = fx.path.join("bin");
    let lock_file = fx.path.join("disk-cleanup.lock");
    fs::create_dir_all(&projects).expect("mkdir projects");
    fs::create_dir_all(&fake_tmp).expect("mkdir fake_tmp");
    fs::create_dir_all(&fake_bin).expect("mkdir fake_bin");

    let now = now_epoch_secs();
    let old_mtime = now - 96 * 3600;

    // Create an old Cargo target/ dir + an old Nix result symlink.
    let app = projects.join("old-app");
    let target = app.join("target");
    fs::create_dir_all(target.join("debug")).expect("mkdir target");
    fs::write(app.join("Cargo.toml"), "[package]\nname = \"old\"\n").expect("write Cargo.toml");
    fs::write(target.join("CACHEDIR.TAG"), CACHEDIR_TAG).expect("write CACHEDIR.TAG");
    let bin_artifact = target.join("debug").join("old-app");
    fs::write(&bin_artifact, vec![b'a'; 4096]).expect("write artifact");
    set_mtime_secs(&bin_artifact, old_mtime);
    set_mtime_secs(&target.join("debug"), old_mtime);
    set_mtime_secs(&target.join("CACHEDIR.TAG"), old_mtime);
    set_mtime_secs(&target, old_mtime);

    let result_link = app.join("result");
    symlink("/nix/store/aaaa-old-app", &result_link).expect("symlink result");
    set_mtime_secs(&result_link, now - 10 * 86_400);

    // Fake nix-env (no-op so real user profiles are never touched),
    // fake nix-collect-garbage reporting 42 paths & 2.0 GiB freed,
    // and fake notify-send returning non-zero with diagnostic stderr.
    write_executable(&fake_bin.join("nix-env"), "#!/bin/sh\nexit 0\n");
    write_executable(
        &fake_bin.join("nix-collect-garbage"),
        "#!/bin/sh\necho '42 store paths deleted, 2.0 GiB freed'\n",
    );
    write_executable(
        &fake_bin.join("notify-send"),
        "#!/bin/sh\necho 'dbus connection refused' >&2\nexit 1\n",
    );

    let output = Command::new(bin)
        .args([
            "--force",
            "--projects-dir",
            projects.to_str().expect("utf-8"),
            "--tmp-dir",
            fake_tmp.to_str().expect("utf-8"),
            "--lock-file",
            lock_file.to_str().expect("utf-8"),
        ])
        .env("PATH", &fake_bin)
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/tmp/fake")
        .env("JOURNAL_STREAM", "1")
        .output()
        .expect("run disk-cleanup");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected exit 0, stderr:\n{stderr}"
    );
    assert!(!target.exists(), "old target/ should be deleted");
    assert!(
        fs::symlink_metadata(&result_link).is_err(),
        "stale result symlink should be deleted"
    );
    assert!(stderr.contains("Nix GC (2.0 GiB)"), "stderr:\n{stderr}");
    if cfg!(target_os = "linux") {
        assert!(
            stderr.contains("dbus connection refused"),
            "expected notify-send failure warning in stderr:\n{stderr}"
        );
    }
}

#[test]
fn cli_dry_run_and_user_phase_json_end_to_end() {
    let bin = env!("CARGO_BIN_EXE_disk-cleanup");
    let fx = TempDir::new("e2e-dry");
    let projects = fx.path.join("Projects");
    let fake_tmp = fx.path.join("fake-tmp");
    let fake_bin = fx.path.join("bin");
    let lock_file = fx.path.join("disk-cleanup.lock");
    fs::create_dir_all(&projects).expect("mkdir projects");
    fs::create_dir_all(&fake_tmp).expect("mkdir fake_tmp");
    fs::create_dir_all(&fake_bin).expect("mkdir fake_bin");

    write_executable(&fake_bin.join("nix-env"), "#!/bin/sh\nexit 0\n");
    write_executable(
        &fake_bin.join("nix-store"),
        "#!/bin/sh\necho /nix/store/1111-a\necho /nix/store/2222-b\n",
    );

    let dry = Command::new(bin)
        .args([
            "--dry-run",
            "--force",
            "--no-notify",
            "--projects-dir",
            projects.to_str().expect("utf-8"),
            "--tmp-dir",
            fake_tmp.to_str().expect("utf-8"),
            "--lock-file",
            lock_file.to_str().expect("utf-8"),
        ])
        .env("PATH", &fake_bin)
        .output()
        .expect("run dry-run");

    let stderr = String::from_utf8_lossy(&dry.stderr);
    assert!(dry.status.success(), "dry-run failed:\n{stderr}");
    assert!(
        stderr.contains("2 currently dead Nix store path(s)"),
        "stderr:\n{stderr}"
    );

    // Exercise `--_user-phase` IPC JSON output directly.
    let phase = Command::new(bin)
        .args([
            "--_user-phase",
            "--dry-run",
            "--projects-dir",
            projects.to_str().expect("utf-8"),
            "--_now-seconds",
            "1764322500.0",
        ])
        .env("PATH", &fake_bin)
        .output()
        .expect("run --_user-phase");
    assert!(phase.status.success());
    let stdout = String::from_utf8_lossy(&phase.stdout);
    assert!(
        stdout.trim().starts_with('{') && stdout.contains("\"targets_removed_count\":0"),
        "unexpected IPC stdout: {stdout}"
    );
}
