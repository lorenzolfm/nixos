//! Unit tests for disk-cleanup core safety invariants, scanners, IPC, and subprocess bounds.

use std::ffi::{CString, OsString};
use std::fs;
use std::num::{NonZeroI32, NonZeroU32, NonZeroU64, NonZeroUsize};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::cleanup::{
    NixGcOutcome, TmpCleanupStats, TriggerDecision, UserCleanupCutoffs, UserPhaseConfig,
    apply_user_cleanup, build_notification, build_summary_parts, clean_nix_tmp_dirs,
    decide_trigger, format_age, format_bytes, parse_nix_gc_freed_line, remove_stale_tmp_dirs,
    run_user_phase_in_process, try_parse_size_to_bytes,
};
use crate::cli::{CliCommand, ParseOutcome, parse_args_with_user_resolver};
use crate::constants::{
    AGE_DAYS_MAX, CACHEDIR_TAG_SIGNATURE_BYTES, CLI_ARGS_MAX, ERROR_MESSAGE_BYTES_MAX, ERRORS_MAX,
    NANOS_PER_SECOND, SUBPROCESS_OUTPUT_BYTES_MAX, TARGET_MTIME_DEPTH_MAX,
};
use crate::ipc::{UserPhaseStats, encode_user_phase_stats_json, parse_user_phase_stats_json};
use crate::process::{
    CommandOptions, CommandOutcome, ProcessExit, StderrConfig, StdoutCapture, StdoutConfig,
    run_command_streaming,
};
use crate::scan::{
    DiskUsage, ProjectScanConfig, TreeFreshness, compute_disk_usage, default_lock_path,
    inspect_tmp_tree, latest_target_mtime_ns, parse_nix_develop_pid, probe_cargo_target_lock,
    scan_projects, scan_stale_nix_tmp_dirs,
};
use crate::sys::{
    self, AdvisoryLock, CargoLockProbe, FilesystemStat, RmtreeError, safe_rmtree,
    safe_rmtree_with_batch_max,
};
use crate::types::{
    AbsPath, AgeDurationNs, BoundedErrors, CleanupMode, DiskPercent, EntryName, NixGcAgeDays,
    NonRootAbsPath, RemovedBatch, SafeProjectsDir, ThresholdPercent, TimestampNs, UserName,
    finite_non_negative_seconds_to_ns,
};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestFixture {
    root: NonRootAbsPath,
    now_ns: TimestampNs,
}

impl TestFixture {
    fn new(name: &str) -> Self {
        let id = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir_name = format!("disk-cleanup-test-{}-{}-{id}", std::process::id(), name);
        let raw_path = std::env::temp_dir().join(dir_name);
        fs::create_dir_all(&raw_path).expect("create test fixture dir");
        let canon = fs::canonicalize(&raw_path).expect("canonicalize test fixture dir");
        let root = NonRootAbsPath::from_absolute(&canon).expect("fixture root is NonRootAbsPath");
        Self {
            root,
            now_ns: TimestampNs::now(),
        }
    }

    fn projects_dir(&self) -> SafeProjectsDir {
        let path = self
            .root
            .as_abs()
            .join_relative(Path::new("Projects"))
            .expect("join Projects");
        SafeProjectsDir::try_from_abs(path).expect("valid SafeProjectsDir")
    }

    fn make_cargo_project(
        &self,
        rel_path: &str,
        age_hours: f64,
        marker: Option<&str>,
        with_cargo_toml: bool,
    ) -> NonRootAbsPath {
        let project = self.root.as_path().join("Projects").join(rel_path);
        let target_raw = project.join("target");
        fs::create_dir_all(target_raw.join("debug").join("deps"))
            .expect("create cargo target tree");
        if with_cargo_toml {
            fs::write(project.join("Cargo.toml"), "[package]\nname = \"demo\"\n")
                .expect("write Cargo.toml");
        }
        if let Some(marker_name) = marker {
            if marker_name == "CACHEDIR.TAG" {
                let mut content = CACHEDIR_TAG_SIGNATURE_BYTES.to_vec();
                content.push(b'\n');
                fs::write(target_raw.join("CACHEDIR.TAG"), content).expect("write CACHEDIR.TAG");
            } else if !marker_name.is_empty() {
                fs::write(target_raw.join(marker_name), "").expect("write cargo marker");
            }
        }
        fs::write(
            target_raw.join("debug").join("deps").join("artifact.rlib"),
            vec![b'x'; 8192],
        )
        .expect("write artifact");

        let delta_ns = i128::try_from(finite_non_negative_seconds_to_ns(age_hours * 3600.0))
            .expect("delta fits i128");
        let mtime_ns = self.now_ns.as_i128() - delta_ns;
        set_tree_mtime_ns(&target_raw, mtime_ns);
        NonRootAbsPath::from_absolute(&target_raw).expect("target path is NonRootAbsPath")
    }
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        if self.root.as_path().exists() {
            restore_tree_permissions(self.root.as_path());
            let _ = safe_rmtree(&self.root);
        }
    }
}

fn tester_user() -> UserName {
    UserName::try_new("tester".to_string()).expect("valid test user")
}

fn set_mtime_ns(path: &Path, mtime_ns: i128) {
    let secs = i64::try_from(mtime_ns.div_euclid(NANOS_PER_SECOND)).expect("mtime secs fit i64");
    let nsec = i64::try_from(mtime_ns.rem_euclid(NANOS_PER_SECOND)).expect("mtime nsec fit i64");
    let ts = libc::timespec {
        tv_sec: libc::time_t::try_from(secs).expect("tv_sec fits time_t"),
        tv_nsec: libc::c_long::try_from(nsec).expect("tv_nsec fits c_long"),
    };
    let times = [ts, ts];
    let c_path = CString::new(path.as_os_str().as_bytes()).expect("path has no NUL");
    // SAFETY: `c_path` is NUL-terminated and `times` holds 2 valid `timespec` structs.
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    assert_eq!(
        rc,
        0,
        "utimensat failed on {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

fn set_tree_mtime_ns(root: &Path, mtime_ns: i128) {
    // Collect entries iteratively and apply mtime bottom-up so parent dirs keep `mtime_ns`.
    let mut visited = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        visited.push(dir.clone());
        if let Ok(read_dir) = fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let p = entry.path();
                if let Ok(meta) = fs::symlink_metadata(&p) {
                    if meta.file_type().is_dir() {
                        stack.push(p);
                    } else {
                        visited.push(p);
                    }
                }
            }
        }
    }
    for path in visited.into_iter().rev() {
        set_mtime_ns(&path, mtime_ns);
    }
}

fn restore_tree_permissions(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
        if let Ok(read_dir) = fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let p = entry.path();
                if let Ok(meta) = fs::symlink_metadata(&p) {
                    if meta.file_type().is_dir() {
                        stack.push(p);
                    } else if !meta.file_type().is_symlink() {
                        let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o644));
                    }
                }
            }
        }
    }
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod failed");
}

fn os_args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

#[test]
fn stale_nix_result_symlink_safety() {
    let fx = TestFixture::new("result-symlinks");
    let projects = fx.projects_dir();
    let proj = projects.as_path().join("demo");
    fs::create_dir_all(&proj).expect("mkdir proj");

    // 1. Old Nix result symlinks (even if dangling -> /nix/store/...) => remove.
    let old_result = proj.join("result");
    symlink("/nix/store/aaaa-demo", &old_result).expect("symlink old_result");
    set_mtime_ns(
        &old_result,
        fx.now_ns.as_i128() - 10 * 86_400 * NANOS_PER_SECOND,
    );

    let old_result_named = proj.join("result-fmt");
    symlink("/nix/store/bbbb-fmt", &old_result_named).expect("symlink old_result_named");
    set_mtime_ns(
        &old_result_named,
        fx.now_ns.as_i128() - 8 * 86_400 * NANOS_PER_SECOND,
    );

    // 2. Fresh Nix result symlink => MUST NOT remove.
    let fake_store_target = fx.root.as_path().join("fake-store-item");
    fs::write(&fake_store_target, "store").expect("write fake target");
    set_mtime_ns(&fake_store_target, NANOS_PER_SECOND);
    let fresh_result = proj.join("result-fresh");
    symlink("/nix/store/cccc-fresh", &fresh_result).expect("symlink fresh");
    set_mtime_ns(
        &fresh_result,
        fx.now_ns.as_i128() - 2 * 86_400 * NANOS_PER_SECOND,
    );

    // 3. Old symlink pointing OUTSIDE /nix/store/ (including bare /nix/store and /nix/store/) => MUST NOT remove.
    let non_nix_result = proj.join("result-local");
    symlink(&fake_store_target, &non_nix_result).expect("symlink non_nix");
    set_mtime_ns(
        &non_nix_result,
        fx.now_ns.as_i128() - 30 * 86_400 * NANOS_PER_SECOND,
    );

    let bare_store_result = proj.join("result-bare");
    symlink("/nix/store", &bare_store_result).expect("symlink bare store");
    set_mtime_ns(
        &bare_store_result,
        fx.now_ns.as_i128() - 30 * 86_400 * NANOS_PER_SECOND,
    );

    let trailing_slash_store = proj.join("result-slash");
    symlink("/nix/store/", &trailing_slash_store).expect("symlink bare store slash");
    set_mtime_ns(
        &trailing_slash_store,
        fx.now_ns.as_i128() - 30 * 86_400 * NANOS_PER_SECOND,
    );

    // 4. Old regular file named `result` => MUST NOT remove.
    let regular_dir = projects.as_path().join("other");
    fs::create_dir_all(&regular_dir).expect("mkdir other");
    let regular_result = regular_dir.join("result");
    fs::write(&regular_result, "important").expect("write regular result");
    set_mtime_ns(
        &regular_result,
        fx.now_ns.as_i128() - 30 * 86_400 * NANOS_PER_SECOND,
    );

    let config = UserPhaseConfig {
        projects_dir: &projects,
        result_age: AgeDurationNs::DEFAULT_RESULT,
        target_age: AgeDurationNs::DEFAULT_TARGET,
        nix_gc_age_days: NixGcAgeDays::DEFAULT,
        mode: CleanupMode::Apply,
        now_ns: fx.now_ns,
        deadline: Instant::now() + Duration::from_secs(60),
    };
    let stats = run_user_phase_in_process(&tester_user(), fx.root.as_abs(), &config);

    assert_eq!(stats.result_links_removed_count, 2);
    assert!(fs::symlink_metadata(&old_result).is_err());
    assert!(fs::symlink_metadata(&old_result_named).is_err());
    assert!(fs::symlink_metadata(&fresh_result).is_ok());
    assert!(fs::symlink_metadata(&non_nix_result).is_ok());
    assert!(fs::symlink_metadata(&bare_store_result).is_ok());
    assert!(fs::symlink_metadata(&trailing_slash_store).is_ok());
    assert!(regular_result.is_file());
}

#[test]
fn rust_target_cleanup_order_recency_and_markers() {
    let fx = TestFixture::new("rust-targets");
    let projects = fx.projects_dir();

    let older = fx.make_cargo_project("repo-old", 96.0, Some("CACHEDIR.TAG"), true);
    let middle = fx.make_cargo_project("repo-mid", 48.0, Some(".rustc_info.json"), true);
    let recent = fx.make_cargo_project("repo-recent", 2.0, Some("CACHEDIR.TAG"), true);
    let no_toml = fx.make_cargo_project("not-rust", 96.0, Some("CACHEDIR.TAG"), false);
    let no_marker = fx.make_cargo_project("src-target", 96.0, None, true);

    // Symlink named `target` inside a Cargo project must never be followed/removed.
    let symlink_proj = projects.as_path().join("repo-symlink");
    fs::create_dir_all(&symlink_proj).expect("mkdir symlink_proj");
    fs::write(symlink_proj.join("Cargo.toml"), "[package]\n").expect("write Cargo.toml");
    let real_outside = fx.root.as_path().join("outside-keep");
    fs::create_dir_all(&real_outside).expect("mkdir real_outside");
    symlink(&real_outside, symlink_proj.join("target")).expect("symlink target");

    let scan = scan_projects(
        &projects,
        &ProjectScanConfig {
            now_ns: fx.now_ns,
            result_age: AgeDurationNs::DEFAULT_RESULT,
            target_age: AgeDurationNs::DEFAULT_TARGET,
        },
    );
    let candidate_paths: Vec<NonRootAbsPath> = scan
        .target_candidates
        .iter()
        .map(|c| c.path.clone())
        .collect();
    // Oldest first!
    assert_eq!(candidate_paths, vec![older.clone(), middle.clone()]);
    assert_eq!(scan.targets_skipped_recent_count, 1);

    let config = UserPhaseConfig {
        projects_dir: &projects,
        result_age: AgeDurationNs::DEFAULT_RESULT,
        target_age: AgeDurationNs::DEFAULT_TARGET,
        nix_gc_age_days: NixGcAgeDays::DEFAULT,
        mode: CleanupMode::Apply,
        now_ns: fx.now_ns,
        deadline: Instant::now() + Duration::from_secs(60),
    };
    let stats = run_user_phase_in_process(&tester_user(), fx.root.as_abs(), &config);
    assert_eq!(stats.targets_removed.count(), 2);
    assert!(!older.as_path().exists());
    assert!(!middle.as_path().exists());
    assert!(recent.as_path().exists());
    assert!(no_toml.as_path().exists());
    assert!(no_marker.as_path().exists());
    assert!(real_outside.exists());
    assert!(
        fs::symlink_metadata(symlink_proj.join("target"))
            .expect("symlink remains")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn rust_target_holds_and_skips_active_cargo_lock() {
    let fx = TestFixture::new("cargo-lock");
    let projects = fx.projects_dir();
    let locked_target = fx.make_cargo_project("repo-building", 72.0, Some(".cargo-lock"), true);
    let lock_path = locked_target
        .join_relative(Path::new(".cargo-lock"))
        .expect("lock path");

    // Hold an independent kernel flock on `.cargo-lock` via AdvisoryLock.
    let guard = AdvisoryLock::try_acquire(&lock_path)
        .expect("acquire flock")
        .expect("lock should succeed");
    assert!(matches!(
        probe_cargo_target_lock(&locked_target),
        CargoLockProbe::Locked
    ));

    let config = UserPhaseConfig {
        projects_dir: &projects,
        result_age: AgeDurationNs::DEFAULT_RESULT,
        target_age: AgeDurationNs::DEFAULT_TARGET,
        nix_gc_age_days: NixGcAgeDays::DEFAULT,
        mode: CleanupMode::Apply,
        now_ns: fx.now_ns,
        deadline: Instant::now() + Duration::from_secs(60),
    };
    let stats = run_user_phase_in_process(&tester_user(), fx.root.as_abs(), &config);
    assert_eq!(stats.targets_removed, RemovedBatch::None);
    assert_eq!(stats.targets_skipped_active_count, 1);
    assert!(locked_target.as_path().exists());

    drop(guard);

    // Acquiring via `probe_cargo_target_lock` keeps `.cargo-lock` locked until dropped.
    let held = match probe_cargo_target_lock(&locked_target) {
        CargoLockProbe::Acquired(lock) => lock,
        other => panic!("expected Acquired, got {other:?}"),
    };
    assert!(
        AdvisoryLock::try_acquire(&lock_path)
            .expect("contend lock")
            .is_none(),
        "HeldCargoLock must remain locked until dropped"
    );
    drop(held);

    // Now that `.cargo-lock` is free, cleanup acquires/holds it and deletes `target/` cleanly.
    let stats_after = run_user_phase_in_process(&tester_user(), fx.root.as_abs(), &config);
    assert_eq!(stats_after.targets_removed.count(), 1);
    assert!(!locked_target.as_path().exists());
}

#[test]
fn tmp_nix_cleanup_handles_readonly_age_and_active_pid() {
    let fx = TestFixture::new("tmp-nix");
    let tmp_dir = fx
        .root
        .join_relative(Path::new("fake-tmp"))
        .expect("fake-tmp path");
    fs::create_dir_all(tmp_dir.as_path()).expect("mkdir fake-tmp");
    let old_mtime_ns = fx.now_ns.as_i128() - 48 * 3600 * NANOS_PER_SECOND;

    // 1. Stale nix-shell dir with read-only nested dir & file => remove.
    let stale_shell = tmp_dir.as_path().join("nix-shell.abc123");
    let nested = stale_shell.join("sub");
    fs::create_dir_all(&nested).expect("mkdir nested");
    let ro_file = nested.join("readonly.txt");
    fs::write(&ro_file, "store copy").expect("write ro_file");
    set_mtime_ns(&ro_file, old_mtime_ns);
    set_mtime_ns(&nested, old_mtime_ns);
    set_mtime_ns(&stale_shell, old_mtime_ns);
    chmod(&ro_file, 0o444);
    chmod(&nested, 0o555);

    // 2. Old top-level dir whose child was touched 1h ago => MUST NOT remove.
    let active_build = tmp_dir.as_path().join("nix-build-demo.drv-0");
    fs::create_dir_all(&active_build).expect("mkdir active_build");
    let recent_file = active_build.join("progress.log");
    fs::write(&recent_file, "building").expect("write progress.log");
    set_mtime_ns(&active_build, old_mtime_ns);
    set_mtime_ns(&recent_file, fx.now_ns.as_i128() - 3600 * NANOS_PER_SECOND);

    // 3. Old nix-develop dir belonging to our own live PID => MUST NOT remove.
    let live_develop = tmp_dir
        .as_path()
        .join(format!("nix-develop-{}-999999", std::process::id()));
    fs::create_dir_all(&live_develop).expect("mkdir live_develop");
    set_mtime_ns(&live_develop, old_mtime_ns);

    // 4. Old non-nix dir => MUST NOT remove.
    let unrelated = tmp_dir.as_path().join("claude-other");
    fs::create_dir_all(&unrelated).expect("mkdir unrelated");
    set_mtime_ns(&unrelated, old_mtime_ns);

    // 5. Symlink named nix-shell.link => MUST NOT remove.
    let symlink_entry = tmp_dir.as_path().join("nix-shell.link");
    symlink(&unrelated, &symlink_entry).expect("symlink nix-shell.link");
    set_mtime_ns(&symlink_entry, old_mtime_ns);

    let stats = clean_nix_tmp_dirs(
        &tmp_dir,
        AgeDurationNs::DEFAULT_TMP,
        CleanupMode::Apply,
        fx.now_ns,
        None,
    );
    assert_eq!(stats.dirs_removed.count(), 1);
    assert_eq!(stats.dirs_skipped_active_count, 1);
    assert_eq!(stats.dirs_skipped_recent_count, 1);
    assert_eq!(stats.dirs_skipped_unreadable_count, 0);
    assert!(!stale_shell.exists());
    assert!(active_build.exists());
    assert!(live_develop.exists());
    assert!(unrelated.exists());
    assert!(
        fs::symlink_metadata(&symlink_entry)
            .expect("symlink kept")
            .file_type()
            .is_symlink()
    );

    assert_eq!(
        parse_nix_develop_pid(&EntryName::try_from_bytes(b"nix-develop-12345-abc").expect("name")),
        NonZeroU32::new(12345)
    );
    assert_eq!(
        parse_nix_develop_pid(&EntryName::try_from_bytes(b"nix-develop-0-abc").expect("name")),
        None
    );
    assert_eq!(
        parse_nix_develop_pid(
            &EntryName::try_from_bytes(b"nix-develop-notapid-abc").expect("name")
        ),
        None
    );
    assert_eq!(
        parse_nix_develop_pid(&EntryName::try_from_bytes(b"nix-shell.123").expect("name")),
        None
    );
}

#[test]
fn dry_run_preserves_everything() {
    let fx = TestFixture::new("dry-run");
    let projects = fx.projects_dir();
    let target = fx.make_cargo_project("dry-run-app", 72.0, Some("CACHEDIR.TAG"), true);
    let result_link = target.parent().as_path().join("result");
    symlink("/nix/store/dddd-demo", &result_link).expect("symlink result");
    set_mtime_ns(
        &result_link,
        fx.now_ns.as_i128() - 10 * 86_400 * NANOS_PER_SECOND,
    );

    let config = UserPhaseConfig {
        projects_dir: &projects,
        result_age: AgeDurationNs::DEFAULT_RESULT,
        target_age: AgeDurationNs::DEFAULT_TARGET,
        nix_gc_age_days: NixGcAgeDays::DEFAULT,
        mode: CleanupMode::DryRun,
        now_ns: fx.now_ns,
        deadline: Instant::now() + Duration::from_secs(60),
    };
    let stats = run_user_phase_in_process(&tester_user(), fx.root.as_abs(), &config);
    assert_eq!(stats.mode, CleanupMode::DryRun);
    assert_eq!(stats.result_links_removed_count, 1);
    assert_eq!(stats.targets_removed.count(), 1);
    assert!(target.as_path().exists());
    assert!(
        fs::symlink_metadata(&result_link)
            .expect("result link preserved")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn skips_hidden_and_unreadable_directories() {
    let fx = TestFixture::new("hidden-unreadable");
    let projects = fx.projects_dir();
    let hidden_dir = projects.as_path().join("repo").join(".volumes");
    fs::create_dir_all(&hidden_dir).expect("mkdir .volumes");
    let hidden_result = hidden_dir.join("result");
    symlink("/nix/store/eeee-hidden", &hidden_result).expect("symlink hidden");
    set_mtime_ns(
        &hidden_result,
        fx.now_ns.as_i128() - 30 * 86_400 * NANOS_PER_SECOND,
    );

    let unreadable = projects.as_path().join("repo").join("locked-data");
    fs::create_dir_all(&unreadable).expect("mkdir locked-data");
    chmod(&unreadable, 0o000);

    let config = UserPhaseConfig {
        projects_dir: &projects,
        result_age: AgeDurationNs::DEFAULT_RESULT,
        target_age: AgeDurationNs::DEFAULT_TARGET,
        nix_gc_age_days: NixGcAgeDays::DEFAULT,
        mode: CleanupMode::Apply,
        now_ns: fx.now_ns,
        deadline: Instant::now() + Duration::from_secs(60),
    };
    let stats = run_user_phase_in_process(&tester_user(), fx.root.as_abs(), &config);
    assert_eq!(stats.result_links_removed_count, 0);
    assert!(stats.errors.is_empty());
    assert!(fs::symlink_metadata(&hidden_result).is_ok());
}

#[test]
fn safe_rmtree_refuses_symlinks_forbidden_paths_preserves_parent_perms_and_batches() {
    let fx = TestFixture::new("safe-rmtree");
    let outside = fx.root.as_path().join("keep-outside");
    fs::create_dir_all(&outside).expect("mkdir outside");
    let sentinel = outside.join("precious.txt");
    fs::write(&sentinel, "keep").expect("write sentinel");

    // Forbidden system path returns Err(UnsafePath) instead of panicking.
    let tmp_system_path = NonRootAbsPath::from_absolute(Path::new("/tmp")).expect("/tmp");
    assert!(matches!(
        safe_rmtree(&tmp_system_path),
        Err(RmtreeError::UnsafePath(_))
    ));

    let link_path = fx
        .root
        .join_relative(Path::new("sneaky-link"))
        .expect("link path");
    symlink(&outside, link_path.as_path()).expect("symlink sneaky-link");
    assert!(matches!(
        safe_rmtree(&link_path),
        Err(RmtreeError::UnsafePath(_))
    ));
    assert!(sentinel.exists());

    let regular_file = fx
        .root
        .join_relative(Path::new("not-a-dir"))
        .expect("file path");
    fs::write(regular_file.as_path(), "file").expect("write regular_file");
    assert!(matches!(
        safe_rmtree(&regular_file),
        Err(RmtreeError::UnsafePath(_))
    ));
    assert!(regular_file.as_path().exists());

    // Multi-batch deletion + read-only nested files must never chmod parent.
    let parent = fx.root.as_path().join("parent-box");
    fs::create_dir_all(&parent).expect("mkdir parent");
    chmod(&parent, 0o750);
    let parent_mode_before = fs::symlink_metadata(&parent)
        .expect("lstat parent")
        .permissions()
        .mode()
        & 0o7777;

    let victim = NonRootAbsPath::from_absolute(&parent.join("victim")).expect("victim path");
    let nested = victim.as_path().join("a").join("b");
    fs::create_dir_all(&nested).expect("mkdir nested");
    for index in 0..10 {
        let file_path = victim.as_path().join(format!("f{index}.txt"));
        fs::write(&file_path, "data").expect("write victim file");
        chmod(&file_path, 0o444);
    }
    let leaf = nested.join("leaf.txt");
    fs::write(&leaf, "leaf").expect("write leaf");
    chmod(&leaf, 0o444);
    chmod(&nested, 0o555);
    chmod(&victim.as_path().join("a"), 0o555);

    safe_rmtree_with_batch_max(&victim, 3).expect("safe_rmtree_with_batch_max");

    assert!(!victim.as_path().exists());
    let parent_mode_after = fs::symlink_metadata(&parent)
        .expect("lstat parent after")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(parent_mode_after, parent_mode_before);
}

#[test]
fn safe_rmtree_unsafe_path_error_is_recorded_without_aborting_phase() {
    let fx = TestFixture::new("rmtree-race");
    let tmp_dir = fx
        .root
        .join_relative(Path::new("race-tmp"))
        .expect("race-tmp");
    fs::create_dir_all(tmp_dir.as_path()).expect("mkdir race-tmp");

    let outside = fx.root.as_path().join("outside-target");
    fs::create_dir_all(&outside).expect("mkdir outside");
    // Simulate a TOCTOU swap: first entry became a symlink before deletion.
    let first_swapped = tmp_dir
        .join_relative(Path::new("nix-shell.first"))
        .expect("first");
    symlink(&outside, first_swapped.as_path()).expect("symlink swapped");
    let second_real = tmp_dir
        .join_relative(Path::new("nix-shell.second"))
        .expect("second");
    fs::create_dir_all(second_real.as_path()).expect("mkdir second");

    let mut stats = TmpCleanupStats::default();
    remove_stale_tmp_dirs(
        &tmp_dir,
        &[(first_swapped.clone(), 4096), (second_real.clone(), 4096)],
        CleanupMode::Apply,
        fx.now_ns.as_i128() + NANOS_PER_SECOND,
        &mut stats,
    );

    assert_eq!(stats.dirs_removed.count(), 1);
    assert_eq!(stats.errors.len(), 1);
    assert!(
        stats
            .errors
            .iter()
            .next()
            .is_some_and(|e| e.as_str().contains("non-directory or symlink"))
    );
    assert!(fs::symlink_metadata(first_swapped.as_path()).is_ok());
    assert!(outside.exists());
    assert!(!second_real.as_path().exists());
}

#[test]
fn parse_args_rejects_invalid_bounds_cross_role_flags_and_system_roots() {
    let max_days_plus_one = format!("{}", AGE_DAYS_MAX + 1);
    let invalid_invocations: Vec<Vec<&str>> = vec![
        vec!["--user", "tester", "--target-age-hours", "nan"],
        vec!["--user", "tester", "--result-age-days", "inf"],
        vec!["--user", "tester", "--tmp-age-hours", "-inf"],
        vec!["--user", "tester", "--tmp-age-hours", "-1"],
        vec!["--user", "tester", "--nix-gc-age-days", &max_days_plus_one],
        vec!["--user", "tester", "--threshold", "0"],
        vec!["--user", "tester", "--threshold", "101"],
        vec!["--user", "tester", "--projects-dir", "/"],
        vec!["--user", "tester", "--tmp-dir", "/"],
        vec!["--user", "../root"],
        vec!["--user", "foo/bar"],
        // Worker-only internal flags rejected in Orchestrator mode.
        vec!["--user", "tester", "--_now-ns", "1764322500123456789"],
        vec!["--user", "tester", "--_result-age-ns", "1000"],
        // Invalid `--_now-ns` values in worker mode.
        vec![
            "--_user-phase",
            "--user",
            "tester",
            "--projects-dir",
            "/tmp/p",
            "--_now-ns",
            "0",
        ],
        vec![
            "--_user-phase",
            "--user",
            "tester",
            "--projects-dir",
            "/tmp/p",
            "--_now-ns",
            "-1",
        ],
        vec![
            "--_user-phase",
            "--user",
            "tester",
            "--projects-dir",
            "/tmp/p",
            "--_now-ns",
            "nan",
        ],
        // Worker mode requires `--projects-dir` and `--_now-ns`.
        vec![
            "--_user-phase",
            "--user",
            "tester",
            "--_now-ns",
            "1764322500123456789",
        ],
        vec![
            "--_user-phase",
            "--user",
            "tester",
            "--projects-dir",
            "/tmp/p",
        ],
        // Orchestrator-only flags rejected in Worker mode (in either flag order).
        vec![
            "--_user-phase",
            "--user",
            "tester",
            "--projects-dir",
            "/tmp/p",
            "--_now-ns",
            "100",
            "--force",
        ],
        vec![
            "--threshold",
            "90",
            "--_user-phase",
            "--user",
            "tester",
            "--projects-dir",
            "/tmp/p",
            "--_now-ns",
            "100",
        ],
        vec![
            "--_user-phase",
            "--user",
            "tester",
            "--projects-dir",
            "/tmp/p",
            "--_now-ns",
            "100",
            "--lock-file",
            "/tmp/x",
        ],
    ];

    for argv in invalid_invocations {
        let outcome = parse_args_with_user_resolver(&os_args(&argv), || Ok(tester_user()));
        assert!(
            matches!(outcome, ParseOutcome::Error(_)),
            "expected error for argv={argv:?}, got {outcome:?}"
        );
    }

    // Excessive argument counts are rejected by CLI_ARGS_MAX.
    let too_many_args: Vec<OsString> = vec![OsString::from("--force"); CLI_ARGS_MAX + 1];
    assert!(matches!(
        parse_args_with_user_resolver(&too_many_args, || Ok(tester_user())),
        ParseOutcome::Error(_)
    ));

    // Explicit `--user` must not require current UID passwd lookup to succeed.
    let orch = parse_args_with_user_resolver(
        &os_args(&["--user=tester", "--force", "--threshold", "90"]),
        || Err("missing uid".to_string()),
    );
    match orch {
        ParseOutcome::Parsed(CliCommand::Orchestrator(args)) => {
            assert_eq!(args.user.as_str(), "tester");
            assert!(args.force);
            assert_eq!(args.threshold.get(), 90);
        }
        other => panic!("expected Parsed(Orchestrator), got {other:?}"),
    }

    // Worker mode preserves exact nanosecond `TimestampNs` and `AgeDurationNs`.
    let worker = parse_args_with_user_resolver(
        &os_args(&[
            "--_user-phase",
            "--user=tester",
            "--projects-dir",
            "/tmp/test-projects",
            "--_now-ns",
            "1764322500123456789",
            "--_result-age-ns",
            "86400000000000",
        ]),
        || Err("missing uid".to_string()),
    );
    match worker {
        ParseOutcome::Parsed(CliCommand::UserPhaseWorker(args)) => {
            assert_eq!(args.user.as_str(), "tester");
            assert_eq!(args.now_ns.as_i128(), 1_764_322_500_123_456_789);
            assert_eq!(args.result_age.as_ns(), 86_400_000_000_000);
        }
        other => panic!("expected Parsed(UserPhaseWorker), got {other:?}"),
    }
}

#[test]
fn fail_closed_on_flock_and_mtime_errors_with_distinct_outcomes() {
    let fx = TestFixture::new("fail-closed");
    let projects = fx.projects_dir();
    let target = fx.make_cargo_project("repo-err", 72.0, Some(".cargo-lock"), true);

    // Symlink `.cargo-lock` must fail closed AND report `SymlinkOrUnreadable` (not silent `Locked`).
    let lock_path = target.as_path().join(".cargo-lock");
    fs::remove_file(&lock_path).expect("remove lock file");
    symlink("/etc/passwd", &lock_path).expect("symlink .cargo-lock");
    assert!(matches!(
        probe_cargo_target_lock(&target),
        CargoLockProbe::SymlinkOrUnreadable(_)
    ));
    fs::remove_file(&lock_path).expect("remove symlink");
    fs::write(&lock_path, "").expect("restore regular .cargo-lock");
    set_mtime_ns(
        &lock_path,
        fx.now_ns.as_i128() - 72 * 3600 * NANOS_PER_SECOND,
    );
    set_mtime_ns(
        target.as_path(),
        fx.now_ns.as_i128() - 72 * 3600 * NANOS_PER_SECOND,
    );

    if sys::effective_uid() != 0 {
        // Unreadable `.cargo-lock` must fail closed as `SymlinkOrUnreadable`.
        chmod(&lock_path, 0o000);
        assert!(matches!(
            probe_cargo_target_lock(&target),
            CargoLockProbe::SymlinkOrUnreadable(_)
        ));
        chmod(&lock_path, 0o644);
        set_mtime_ns(
            &lock_path,
            fx.now_ns.as_i128() - 72 * 3600 * NANOS_PER_SECOND,
        );
        set_mtime_ns(
            target.as_path(),
            fx.now_ns.as_i128() - 72 * 3600 * NANOS_PER_SECOND,
        );

        // Unreadable inner subdirectory inside target/ must fail closed as `Unreadable`
        // and count under `targets_skipped_unreadable_count` (NOT `recent`).
        let locked_subdir = target.as_path().join("debug").join("unreadable-sub");
        fs::create_dir_all(&locked_subdir).expect("mkdir unreadable-sub");
        set_mtime_ns(
            &locked_subdir,
            fx.now_ns.as_i128() - 72 * 3600 * NANOS_PER_SECOND,
        );
        set_mtime_ns(
            &target.as_path().join("debug"),
            fx.now_ns.as_i128() - 72 * 3600 * NANOS_PER_SECOND,
        );
        chmod(&locked_subdir, 0o000);

        assert!(matches!(
            latest_target_mtime_ns(&target, fx.now_ns.as_i128(), TARGET_MTIME_DEPTH_MAX),
            TreeFreshness::Unreadable(_)
        ));
        let scan = scan_projects(
            &projects,
            &ProjectScanConfig {
                now_ns: fx.now_ns,
                result_age: AgeDurationNs::DEFAULT_RESULT,
                target_age: AgeDurationNs::try_from_ns(0).expect("0ns"),
            },
        );
        assert!(scan.target_candidates.is_empty());
        assert_eq!(scan.targets_skipped_recent_count, 0);
        assert_eq!(scan.targets_skipped_unreadable_count, 1);

        // Same distinct `Unreadable` behavior for unreadable subdirectory inside `/tmp/nix-*`.
        let tmp_dir = fx
            .root
            .join_relative(Path::new("tmp-unreadable"))
            .expect("tmp-unreadable");
        let stale = tmp_dir
            .join_relative(Path::new("nix-shell.unreadable"))
            .expect("stale");
        let inner = stale.as_path().join("blocked");
        fs::create_dir_all(&inner).expect("mkdir inner");
        let old_mtime_ns = fx.now_ns.as_i128() - 72 * 3600 * NANOS_PER_SECOND;
        set_mtime_ns(&inner, old_mtime_ns);
        set_mtime_ns(stale.as_path(), old_mtime_ns);
        chmod(&inner, 0o000);

        assert!(matches!(
            inspect_tmp_tree(&stale, fx.now_ns.as_i128() - 24 * 3600 * NANOS_PER_SECOND),
            TreeFreshness::Unreadable(_)
        ));
        let tmp_scan =
            scan_stale_nix_tmp_dirs(&tmp_dir, AgeDurationNs::DEFAULT_TMP, fx.now_ns, None);
        assert!(tmp_scan.stale_dirs.is_empty());
        assert_eq!(tmp_scan.dirs_skipped_recent_count, 0);
        assert_eq!(tmp_scan.dirs_skipped_unreadable_count, 1);
    }
}

#[test]
fn pocpou_skips_target_result_symlink_and_tmp_dir_modified_between_scan_and_delete() {
    let fx = TestFixture::new("pocpou");
    let projects = fx.projects_dir();
    let target = fx.make_cargo_project("repo-race", 72.0, Some("CACHEDIR.TAG"), true);
    let result_link = target.parent().as_path().join("result");
    symlink("/nix/store/aaaa-old", &result_link).expect("symlink old result");
    set_mtime_ns(
        &result_link,
        fx.now_ns.as_i128() - 10 * 86_400 * NANOS_PER_SECOND,
    );

    let scan_config = ProjectScanConfig {
        now_ns: fx.now_ns,
        result_age: AgeDurationNs::DEFAULT_RESULT,
        target_age: AgeDurationNs::DEFAULT_TARGET,
    };
    let scan = scan_projects(&projects, &scan_config);
    assert_eq!(scan.stale_results.len(), 1);
    assert_eq!(scan.target_candidates.len(), 1);

    // 1. Simulate a fresh `nix build` replacing `result` between scan and unlink.
    fs::remove_file(&result_link).expect("remove old result");
    symlink("/nix/store/bbbb-new", &result_link).expect("symlink new result");
    let recent_mtime_ns = fx.now_ns.as_i128() - 10 * NANOS_PER_SECOND;
    set_mtime_ns(&result_link, recent_mtime_ns);

    // 2. Simulate a Cargo build running and finishing after `scan_projects` returned.
    let fresh_artifact = target.as_path().join("debug").join("fresh-bin");
    fs::write(&fresh_artifact, "just built").expect("write fresh-bin");
    set_mtime_ns(&fresh_artifact, recent_mtime_ns);
    set_mtime_ns(&target.as_path().join("debug"), recent_mtime_ns);

    let stats = apply_user_cleanup(
        &projects,
        &scan,
        &[],
        NixGcAgeDays::DEFAULT,
        UserCleanupCutoffs::new(
            fx.now_ns,
            AgeDurationNs::DEFAULT_RESULT,
            AgeDurationNs::DEFAULT_TARGET,
        ),
        Instant::now() + Duration::from_secs(60),
    );
    assert_eq!(stats.result_links_removed_count, 0);
    assert_eq!(stats.targets_removed, RemovedBatch::None);
    assert_eq!(stats.targets_skipped_recent_count, 1);
    assert!(fs::symlink_metadata(&result_link).is_ok());
    assert!(target.as_path().exists());

    // 3. Simulate a `/tmp/nix-*` directory modified between scan and deletion.
    let tmp_dir = fx
        .root
        .join_relative(Path::new("pocpou-tmp"))
        .expect("pocpou-tmp");
    let stale_tmp = tmp_dir
        .join_relative(Path::new("nix-shell.race"))
        .expect("stale_tmp");
    fs::create_dir_all(stale_tmp.as_path()).expect("mkdir stale_tmp");
    let old_mtime_ns = fx.now_ns.as_i128() - 48 * 3600 * NANOS_PER_SECOND;
    set_mtime_ns(stale_tmp.as_path(), old_mtime_ns);

    let tmp_scan = scan_stale_nix_tmp_dirs(&tmp_dir, AgeDurationNs::DEFAULT_TMP, fx.now_ns, None);
    assert_eq!(tmp_scan.stale_dirs.len(), 1);

    let fresh_tmp_file = stale_tmp.as_path().join("active.log");
    fs::write(&fresh_tmp_file, "active").expect("write active.log");
    set_mtime_ns(&fresh_tmp_file, recent_mtime_ns);

    let mut tmp_stats = TmpCleanupStats::default();
    let tmp_cutoff_ns = fx.now_ns.as_i128() - 24 * 3600 * NANOS_PER_SECOND;
    remove_stale_tmp_dirs(
        &tmp_dir,
        &tmp_scan.stale_dirs,
        CleanupMode::Apply,
        tmp_cutoff_ns,
        &mut tmp_stats,
    );
    assert_eq!(tmp_stats.dirs_removed, RemovedBatch::None);
    assert_eq!(tmp_stats.dirs_skipped_recent_count, 1);
    assert!(stale_tmp.as_path().exists());
}

#[test]
fn ipc_json_pair_validation_rejects_empty_errors_mode_mismatch_and_tracks_drops() {
    let mut valid = UserPhaseStats::new(CleanupMode::Apply);
    valid.result_links_removed_count = 2;
    valid.targets_removed.record(4096);
    valid.targets_skipped_active_count = 1;
    valid.targets_skipped_unreadable_count = 3;
    valid.user_profiles_pruned_count = 1;
    valid.errors.push_truncated("minor \"quoted\"\nerror\\path");

    let encoded = encode_user_phase_stats_json(&valid);
    let parsed =
        parse_user_phase_stats_json(&encoded, CleanupMode::Apply).expect("round-trip JSON");
    assert_eq!(parsed, valid);

    // Mode mismatch must be rejected at the IPC boundary.
    assert!(parse_user_phase_stats_json(&encoded, CleanupMode::DryRun).is_err());

    // Worst-case error budget: ERRORS_MAX oversized UTF-8 + control-char error strings + drops
    // must truncate cleanly, track `dropped_count`, fit within `SUBPROCESS_OUTPUT_BYTES_MAX`, and round-trip.
    let mut worst_case = UserPhaseStats::new(CleanupMode::Apply);
    let noisy_message = format!("{}\u{0001}\"\\é", "é".repeat(ERROR_MESSAGE_BYTES_MAX));
    for _ in 0..(ERRORS_MAX + 10) {
        worst_case.errors.push_truncated(noisy_message.clone());
    }
    assert_eq!(worst_case.errors.len(), ERRORS_MAX);
    assert_eq!(worst_case.errors.dropped_count(), 10);
    assert!(
        worst_case
            .errors
            .iter()
            .all(|msg| !msg.as_str().is_empty() && msg.as_str().len() <= ERROR_MESSAGE_BYTES_MAX)
    );
    let worst_encoded = encode_user_phase_stats_json(&worst_case);
    assert!(worst_encoded.len() <= SUBPROCESS_OUTPUT_BYTES_MAX);
    let worst_parsed = parse_user_phase_stats_json(&worst_encoded, CleanupMode::Apply)
        .expect("round-trip worst-case JSON");
    assert_eq!(worst_parsed, worst_case);

    // Cross-privilege semantic mismatches (including empty error string `""`) must return Err, never panic root.
    let invalid_payloads = [
        "[]",
        "{\"unexpected\": 1}",
        // Empty error string previously panicked root in `append_error`:
        "{\"mode\":\"apply\",\"result_links_removed_count\":0,\"targets_removed_count\":0,\"targets_freed_bytes\":0,\"targets_skipped_active_count\":0,\"targets_skipped_recent_count\":0,\"targets_skipped_unreadable_count\":0,\"user_profiles_pruned_count\":0,\"errors_dropped_count\":0,\"errors\":[\"\"]}",
        // `targets_removed_count == 0` with `targets_freed_bytes > 0`:
        "{\"mode\":\"apply\",\"result_links_removed_count\":0,\"targets_removed_count\":0,\"targets_freed_bytes\":1024,\"targets_skipped_active_count\":0,\"targets_skipped_recent_count\":0,\"targets_skipped_unreadable_count\":0,\"user_profiles_pruned_count\":0,\"errors_dropped_count\":0,\"errors\":[]}",
        // `errors_dropped_count > 0` when `errors` is not full:
        "{\"mode\":\"apply\",\"result_links_removed_count\":0,\"targets_removed_count\":0,\"targets_freed_bytes\":0,\"targets_skipped_active_count\":0,\"targets_skipped_recent_count\":0,\"targets_skipped_unreadable_count\":0,\"user_profiles_pruned_count\":0,\"errors_dropped_count\":1,\"errors\":[]}",
        // Invalid mode string:
        "{\"mode\":\"other\",\"result_links_removed_count\":0,\"targets_removed_count\":0,\"targets_freed_bytes\":0,\"targets_skipped_active_count\":0,\"targets_skipped_recent_count\":0,\"targets_skipped_unreadable_count\":0,\"user_profiles_pruned_count\":0,\"errors_dropped_count\":0,\"errors\":[]}",
        // Negative integer:
        "{\"mode\":\"apply\",\"result_links_removed_count\":0,\"targets_removed_count\":-1,\"targets_freed_bytes\":0,\"targets_skipped_active_count\":0,\"targets_skipped_recent_count\":0,\"targets_skipped_unreadable_count\":0,\"user_profiles_pruned_count\":0,\"errors_dropped_count\":0,\"errors\":[]}",
        // Non-string error item:
        "{\"mode\":\"apply\",\"result_links_removed_count\":0,\"targets_removed_count\":0,\"targets_freed_bytes\":0,\"targets_skipped_active_count\":0,\"targets_skipped_recent_count\":0,\"targets_skipped_unreadable_count\":0,\"user_profiles_pruned_count\":0,\"errors_dropped_count\":0,\"errors\":[123]}",
        // Trailing data:
        "{\"mode\":\"apply\",\"result_links_removed_count\":0,\"targets_removed_count\":0,\"targets_freed_bytes\":0,\"targets_skipped_active_count\":0,\"targets_skipped_recent_count\":0,\"targets_skipped_unreadable_count\":0,\"user_profiles_pruned_count\":0,\"errors_dropped_count\":0,\"errors\":[]} trailing",
    ];
    for raw in invalid_payloads {
        assert!(
            parse_user_phase_stats_json(raw, CleanupMode::Apply).is_err(),
            "expected Err for payload: {raw}"
        );
    }
}

#[test]
fn streaming_subprocess_outcomes_timeout_truncation_and_signal_separation() {
    let lines_max = NonZeroU64::new(1000).expect("1000 > 0");

    // 1. Timeout enforcement produces `CommandOutcome::TimedOut` (no overlapping fields).
    let timed_opts = CommandOptions {
        program: OsString::from("/bin/sh"),
        args: os_args(&["-c", "sleep 30"]),
        timeout: Duration::from_millis(250),
        env_override: None,
        privilege_drop: None,
        stream_lines_max: lines_max,
    };
    let timed = run_command_streaming::<fn(&str), fn(&str)>(
        &timed_opts,
        StdoutConfig::StreamOnly { on_line: None },
        StderrConfig::Piped {
            tail_bytes_max: 0,
            on_line: None,
        },
    );
    assert!(matches!(timed, CommandOutcome::TimedOut { .. }));

    // 2. Byte capture cap produces `StdoutCapture::Truncated` distinctly from `Complete`.
    let capped_opts = CommandOptions {
        program: OsString::from("/bin/sh"),
        args: os_args(&["-c", "printf '%0500d\\n' 0"]),
        timeout: Duration::from_secs(5),
        env_override: None,
        privilege_drop: None,
        stream_lines_max: lines_max,
    };
    let capped = run_command_streaming::<fn(&str), fn(&str)>(
        &capped_opts,
        StdoutConfig::Capture {
            bytes_max: NonZeroUsize::new(32).expect("32 > 0"),
            on_line: None,
        },
        StderrConfig::Piped {
            tail_bytes_max: 0,
            on_line: None,
        },
    );
    match capped {
        CommandOutcome::Completed {
            exit: ProcessExit::Success,
            stdout: StdoutCapture::Truncated { text, .. },
            ..
        } => assert_eq!(text.len(), 32),
        other => panic!("expected Completed(Success, Truncated), got {other:?}"),
    }

    // 3. Line streaming callback works with `StdoutCapture::NotCaptured`.
    let mut seen_lines: Vec<String> = Vec::new();
    let stream_opts = CommandOptions {
        program: OsString::from("/bin/sh"),
        args: os_args(&["-c", "for i in 0 1 2 3 4; do echo line-$i; done"]),
        timeout: Duration::from_secs(5),
        env_override: None,
        privilege_drop: None,
        stream_lines_max: lines_max,
    };
    let streamed = run_command_streaming(
        &stream_opts,
        StdoutConfig::StreamOnly {
            on_line: Some(|line: &str| seen_lines.push(line.to_string())),
        },
        StderrConfig::<fn(&str)>::Piped {
            tail_bytes_max: 0,
            on_line: None,
        },
    );
    assert!(matches!(
        streamed,
        CommandOutcome::Completed {
            exit: ProcessExit::Success,
            stdout: StdoutCapture::NotCaptured { lines_count: 5 },
            ..
        }
    ));
    assert_eq!(
        seen_lines,
        vec!["line-0", "line-1", "line-2", "line-3", "line-4"]
    );

    // 4. SIGHUP (signal 1) maps to `ProcessExit::Signaled(1)`, distinct from `CommandOutcome::Failed`.
    let sighup_opts = CommandOptions {
        program: OsString::from("/bin/sh"),
        args: os_args(&["-c", "kill -HUP $$"]),
        timeout: Duration::from_secs(5),
        env_override: None,
        privilege_drop: None,
        stream_lines_max: lines_max,
    };
    let sighup = run_command_streaming::<fn(&str), fn(&str)>(
        &sighup_opts,
        StdoutConfig::StreamOnly { on_line: None },
        StderrConfig::Piped {
            tail_bytes_max: 256,
            on_line: None,
        },
    );
    assert!(matches!(
        sighup,
        CommandOutcome::Completed {
            exit: ProcessExit::Signaled(sig),
            ..
        } if sig == NonZeroI32::new(libc::SIGHUP).expect("SIGHUP != 0")
    ));

    let missing_opts = CommandOptions {
        program: OsString::from("/nonexistent/disk-cleanup-test-binary"),
        args: Vec::new(),
        timeout: Duration::from_secs(5),
        env_override: None,
        privilege_drop: None,
        stream_lines_max: lines_max,
    };
    let missing = run_command_streaming::<fn(&str), fn(&str)>(
        &missing_opts,
        StdoutConfig::StreamOnly { on_line: None },
        StderrConfig::Piped {
            tail_bytes_max: 256,
            on_line: None,
        },
    );
    assert!(matches!(missing, CommandOutcome::Failed { .. }));
}

#[test]
fn advisory_lock_contention_regular_file_and_size_parsing_invariants() {
    let fx = TestFixture::new("lock-size");
    let lock_file = fx
        .root
        .join_relative(Path::new("sub/disk.lock"))
        .expect("lock_file");

    // Canonical default lock path is single and deterministic.
    #[cfg(target_os = "linux")]
    assert_eq!(
        default_lock_path().as_path(),
        Path::new("/run/disk-cleanup/lock")
    );
    #[cfg(not(target_os = "linux"))]
    assert_eq!(
        default_lock_path().as_path(),
        Path::new("/var/db/disk-cleanup/lock")
    );

    let first = AdvisoryLock::try_acquire(&lock_file).expect("first acquire");
    assert!(first.is_some());
    let second = AdvisoryLock::try_acquire(&lock_file).expect("second acquire");
    assert!(second.is_none());
    drop(first);

    let third = AdvisoryLock::try_acquire(&lock_file).expect("third acquire after drop");
    assert!(third.is_some());
    drop(third);

    // Directory or symlink as lockfile must be rejected.
    assert!(AdvisoryLock::try_acquire(&fx.root).is_err());

    assert_eq!(
        try_parse_size_to_bytes(2.0, "GiB"),
        Some(2 * 1024_u64.pow(3))
    );
    assert_eq!(try_parse_size_to_bytes(500.0, "B"), Some(500));
    assert_eq!(try_parse_size_to_bytes(10.0, "iB"), None);
    assert_eq!(try_parse_size_to_bytes(f64::NAN, "MiB"), None);
    assert_eq!(try_parse_size_to_bytes(-1.0, "MiB"), None);
    assert_eq!(try_parse_size_to_bytes(1.0e305, "TiB"), None);
    assert_eq!(
        parse_nix_gc_freed_line("1 store paths deleted, 5.0 iB freed"),
        None
    );
    // Inconsistent `0 paths` claiming `> 0` bytes must be rejected.
    assert_eq!(
        parse_nix_gc_freed_line("0 store paths deleted, 5.0 MiB freed"),
        None
    );
    assert_eq!(
        parse_nix_gc_freed_line("0 store paths deleted, 0.00 MiB freed"),
        Some(RemovedBatch::None)
    );
    // Overflowing floats from external stdout must return None (never panic or saturate).
    assert_eq!(
        parse_nix_gc_freed_line("1 store paths deleted, 999999999999999999999999999999 TiB freed"),
        None
    );
    assert_eq!(
        parse_nix_gc_freed_line("1 store paths deleted, 18446744073709551616 B freed"),
        None
    );
    assert_eq!(
        parse_nix_gc_freed_line("1 store paths deleted, 5.0 MiB freed"),
        RemovedBatch::from_parts(1, 5 * 1024 * 1024).ok()
    );
    assert_eq!(
        parse_nix_gc_freed_line("42 store paths deleted, 50.0 GiB freed"),
        RemovedBatch::from_parts(42, 50 * 1024_u64.pow(3)).ok()
    );
}

#[test]
fn stat_filesystem_exact_integer_ceiling() {
    // Large block count (> 2^53) where IEEE-754 float division loses precision.
    let huge: u64 = 1_u64 << 54;
    let usage = compute_disk_usage(
        &AbsPath::root(),
        FilesystemStat {
            device_id: 42,
            fragment_size_bytes: NonZeroU64::new(4096).expect("4096 > 0"),
            blocks_total: huge,
            blocks_free: huge - 1,
            blocks_available: huge - 1,
        },
    );
    assert_eq!(usage.used_percent, DiskPercent::try_new(1).expect("1%"));
    assert_eq!(usage.used_bytes, 4096);
    assert_eq!(usage.device_id, 42);
}

#[test]
fn threshold_trigger_and_notification_decisions() {
    let gib = 1024_u64.pow(3);
    let below = DiskUsage {
        path: AbsPath::root(),
        device_id: 1,
        used_bytes: 500 * gib,
        available_bytes: 500 * gib,
        used_percent: DiskPercent::try_new(50).expect("50%"),
    };
    assert_eq!(
        decide_trigger(&below, ThresholdPercent::DEFAULT, false),
        TriggerDecision::SkipBelowThreshold
    );
    assert_eq!(
        decide_trigger(&below, ThresholdPercent::DEFAULT, true),
        TriggerDecision::Run {
            trigger_description: "forced at 50%".to_string()
        }
    );

    let before = DiskUsage {
        path: AbsPath::root(),
        device_id: 1,
        used_bytes: 900 * gib,
        available_bytes: 100 * gib,
        used_percent: DiskPercent::try_new(90).expect("90%"),
    };
    let after = DiskUsage {
        path: AbsPath::root(),
        device_id: 1,
        used_bytes: 800 * gib,
        available_bytes: 200 * gib,
        used_percent: DiskPercent::try_new(80).expect("80%"),
    };
    assert_eq!(
        decide_trigger(&before, ThresholdPercent::DEFAULT, false),
        TriggerDecision::Run {
            trigger_description: "triggered at 90% >= 85%".to_string()
        }
    );

    let mut user_stats = UserPhaseStats::new(CleanupMode::Apply);
    user_stats.targets_removed.record(8192);
    let nix_outcome = NixGcOutcome::Executed {
        pruned_system_profiles: true,
        reclaimed: RemovedBatch::from_parts(42, 50 * gib).expect("valid batch"),
        errors: BoundedErrors::new(),
    };
    let summary_parts = build_summary_parts(&user_stats, &nix_outcome, &TmpCleanupStats::default());
    let notification = build_notification(&before, &after, 50 * gib, &summary_parts)
        .expect("notification should fire");
    assert!(notification.title().contains("100.0 GiB"));
    assert!(notification.body().contains("90% → 80%"));
    assert!(notification.body().contains("1 Rust target/"));
    assert!(notification.body().contains("Nix GC"));

    // Nothing freed -> no notification.
    assert_eq!(build_notification(&before, &before, 0, &[]), None);

    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(2048), "2.0 KiB");
    assert_eq!(format_age(300 * 1_000_000_000), "5m");
    assert_eq!(format_age(7200 * 1_000_000_000), "2.0h");
    assert_eq!(format_age(172_800 * 1_000_000_000), "2.0d");
}

#[test]
fn validated_domain_types_reject_illegal_constructions() {
    assert!(AbsPath::from_absolute(Path::new("relative/path")).is_err());
    assert!(AbsPath::from_absolute(Path::new("")).is_err());
    assert!(NonRootAbsPath::from_absolute(Path::new("/")).is_err());
    assert!(ThresholdPercent::try_from_i64(0).is_err());
    assert!(ThresholdPercent::try_from_i64(101).is_err());
    assert!(DiskPercent::try_new(101).is_err());
    assert!(NixGcAgeDays::try_from_i64(-1).is_err());
    assert!(TimestampNs::try_new(0).is_err());
    assert!(RemovedBatch::from_parts(0, 1).is_err());
    assert!(EntryName::try_from_bytes(b".").is_err());
    assert!(EntryName::try_from_bytes(b"..").is_err());
    assert!(EntryName::try_from_bytes(b"a/b").is_err());
}
