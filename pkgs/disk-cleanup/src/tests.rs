//! Unit tests for disk-cleanup core safety invariants, scanners, IPC, and subprocess bounds.

use std::ffi::{CString, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::cleanup::{
    NixGcStats, TmpCleanupStats, TriggerDecision, UserCleanupCutoffs, UserPhaseConfig,
    append_error, apply_user_cleanup, build_notification, build_summary_parts, clean_nix_tmp_dirs,
    decide_trigger, format_age, format_bytes, parse_nix_gc_freed_line, remove_stale_tmp_dirs,
    run_user_phase_in_process, try_parse_size_to_bytes,
};
use crate::cli::{ParseOutcome, parse_args_with_user_resolver};
use crate::constants::{
    AGE_DAYS_MAX, CACHEDIR_TAG_SIGNATURE_BYTES, CLI_ARGS_MAX, ERROR_MESSAGE_BYTES_MAX, ERRORS_MAX,
    NANOS_PER_SECOND, SUBPROCESS_OUTPUT_BYTES_MAX, TARGET_MTIME_DEPTH_MAX,
};
use crate::ipc::{
    UserPhaseStats, assert_user_phase_stats_valid, encode_user_phase_stats_json,
    parse_user_phase_stats_json,
};
use crate::process::{CommandOptions, run_command_streaming};
use crate::scan::{
    DiskUsage, ProjectScanConfig, compute_disk_usage, current_time_ns, inspect_tmp_tree,
    is_cargo_target_locked, latest_target_mtime_ns, non_negative_seconds_to_ns,
    parse_nix_develop_pid, scan_projects, scan_stale_nix_tmp_dirs,
};
use crate::sys::{
    self, AdvisoryLock, FilesystemStat, RmtreeError, safe_rmtree, safe_rmtree_with_batch_max,
};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestFixture {
    root: PathBuf,
    now_ns: i128,
}

impl TestFixture {
    fn new(name: &str) -> Self {
        let id = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir_name = format!("disk-cleanup-test-{}-{}-{id}", std::process::id(), name);
        let raw_path = std::env::temp_dir().join(dir_name);
        fs::create_dir_all(&raw_path).expect("create test fixture dir");
        let root = fs::canonicalize(&raw_path).expect("canonicalize test fixture dir");
        Self {
            root,
            now_ns: current_time_ns(),
        }
    }

    fn make_cargo_project(
        &self,
        rel_path: &str,
        age_hours: f64,
        marker: Option<&str>,
        with_cargo_toml: bool,
    ) -> PathBuf {
        let project = self.root.join("Projects").join(rel_path);
        let target = project.join("target");
        fs::create_dir_all(target.join("debug").join("deps")).expect("create cargo target tree");
        if with_cargo_toml {
            fs::write(project.join("Cargo.toml"), "[package]\nname = \"demo\"\n")
                .expect("write Cargo.toml");
        }
        if let Some(marker_name) = marker {
            if marker_name == "CACHEDIR.TAG" {
                let mut content = CACHEDIR_TAG_SIGNATURE_BYTES.to_vec();
                content.push(b'\n');
                fs::write(target.join("CACHEDIR.TAG"), content).expect("write CACHEDIR.TAG");
            } else if !marker_name.is_empty() {
                fs::write(target.join(marker_name), "").expect("write cargo marker");
            }
        }
        fs::write(
            target.join("debug").join("deps").join("artifact.rlib"),
            vec![b'x'; 8192],
        )
        .expect("write artifact");

        let mtime_ns = self.now_ns - non_negative_seconds_to_ns(age_hours * 3600.0);
        set_tree_mtime_ns(&target, mtime_ns);
        target
    }
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        if self.root.exists() {
            restore_tree_permissions(&self.root);
            let _ = safe_rmtree(&self.root);
        }
    }
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
    let projects = fx.root.join("Projects");
    let proj = projects.join("demo");
    fs::create_dir_all(&proj).expect("mkdir proj");

    // 1. Old Nix result symlinks (even if dangling -> /nix/store/...) => remove.
    let old_result = proj.join("result");
    symlink("/nix/store/aaaa-demo", &old_result).expect("symlink old_result");
    set_mtime_ns(&old_result, fx.now_ns - 10 * 86_400 * NANOS_PER_SECOND);

    let old_result_named = proj.join("result-fmt");
    symlink("/nix/store/bbbb-fmt", &old_result_named).expect("symlink old_result_named");
    set_mtime_ns(&old_result_named, fx.now_ns - 8 * 86_400 * NANOS_PER_SECOND);

    // 2. Fresh Nix result symlink => MUST NOT remove.
    let fake_store_target = fx.root.join("fake-store-item");
    fs::write(&fake_store_target, "store").expect("write fake target");
    set_mtime_ns(&fake_store_target, NANOS_PER_SECOND);
    let fresh_result = proj.join("result-fresh");
    symlink("/nix/store/cccc-fresh", &fresh_result).expect("symlink fresh");
    set_mtime_ns(&fresh_result, fx.now_ns - 2 * 86_400 * NANOS_PER_SECOND);

    // 3. Old symlink pointing OUTSIDE /nix/store/ (including bare /nix/store) => MUST NOT remove.
    let non_nix_result = proj.join("result-local");
    symlink(&fake_store_target, &non_nix_result).expect("symlink non_nix");
    set_mtime_ns(&non_nix_result, fx.now_ns - 30 * 86_400 * NANOS_PER_SECOND);

    let bare_store_result = proj.join("result-bare");
    symlink("/nix/store", &bare_store_result).expect("symlink bare store");
    set_mtime_ns(
        &bare_store_result,
        fx.now_ns - 30 * 86_400 * NANOS_PER_SECOND,
    );

    // 4. Old regular file named `result` => MUST NOT remove.
    let regular_dir = projects.join("other");
    fs::create_dir_all(&regular_dir).expect("mkdir other");
    let regular_result = regular_dir.join("result");
    fs::write(&regular_result, "important").expect("write regular result");
    set_mtime_ns(&regular_result, fx.now_ns - 30 * 86_400 * NANOS_PER_SECOND);

    let config = UserPhaseConfig {
        projects_dir: &projects,
        result_age_days: 7.0,
        target_age_hours: 24.0,
        nix_gc_age_days: 14,
        dry_run: false,
        now_ns: fx.now_ns,
        deadline: Instant::now() + Duration::from_secs(60),
    };
    let stats = run_user_phase_in_process("tester", &fx.root, &config);

    assert_eq!(stats.result_links_removed_count, 2);
    assert!(fs::symlink_metadata(&old_result).is_err());
    assert!(fs::symlink_metadata(&old_result_named).is_err());
    assert!(fs::symlink_metadata(&fresh_result).is_ok());
    assert!(fs::symlink_metadata(&non_nix_result).is_ok());
    assert!(fs::symlink_metadata(&bare_store_result).is_ok());
    assert!(regular_result.is_file());
}

#[test]
fn rust_target_cleanup_order_recency_and_markers() {
    let fx = TestFixture::new("rust-targets");
    let projects = fx.root.join("Projects");

    let older = fx.make_cargo_project("repo-old", 96.0, Some("CACHEDIR.TAG"), true);
    let middle = fx.make_cargo_project("repo-mid", 48.0, Some(".rustc_info.json"), true);
    let recent = fx.make_cargo_project("repo-recent", 2.0, Some("CACHEDIR.TAG"), true);
    let no_toml = fx.make_cargo_project("not-rust", 96.0, Some("CACHEDIR.TAG"), false);
    let no_marker = fx.make_cargo_project("src-target", 96.0, None, true);

    // Symlink named `target` inside a Cargo project must never be followed/removed.
    let symlink_proj = projects.join("repo-symlink");
    fs::create_dir_all(&symlink_proj).expect("mkdir symlink_proj");
    fs::write(symlink_proj.join("Cargo.toml"), "[package]\n").expect("write Cargo.toml");
    let real_outside = fx.root.join("outside-keep");
    fs::create_dir_all(&real_outside).expect("mkdir real_outside");
    symlink(&real_outside, symlink_proj.join("target")).expect("symlink target");

    let scan = scan_projects(
        &projects,
        &ProjectScanConfig {
            now_ns: fx.now_ns,
            result_age_days: 7.0,
            target_age_hours: 24.0,
        },
    );
    let candidate_paths: Vec<PathBuf> = scan
        .target_candidates
        .iter()
        .map(|c| c.path.clone())
        .collect();
    // Oldest first!
    assert_eq!(candidate_paths, vec![older.clone(), middle.clone()]);
    assert_eq!(scan.targets_skipped_recent_count, 1);

    let config = UserPhaseConfig {
        projects_dir: &projects,
        result_age_days: 7.0,
        target_age_hours: 24.0,
        nix_gc_age_days: 14,
        dry_run: false,
        now_ns: fx.now_ns,
        deadline: Instant::now() + Duration::from_secs(60),
    };
    let stats = run_user_phase_in_process("tester", &fx.root, &config);
    assert_eq!(stats.targets_removed_count, 2);
    assert!(!older.exists());
    assert!(!middle.exists());
    assert!(recent.exists());
    assert!(no_toml.exists());
    assert!(no_marker.exists());
    assert!(real_outside.exists());
    assert!(
        fs::symlink_metadata(symlink_proj.join("target"))
            .expect("symlink remains")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn rust_target_skips_active_cargo_lock() {
    let fx = TestFixture::new("cargo-lock");
    let projects = fx.root.join("Projects");
    let locked_target = fx.make_cargo_project("repo-building", 72.0, Some(".cargo-lock"), true);
    let lock_path = locked_target.join(".cargo-lock");

    // Hold an independent kernel flock on `.cargo-lock` via AdvisoryLock.
    let guard = AdvisoryLock::try_acquire(&lock_path)
        .expect("acquire flock")
        .expect("lock should succeed");
    assert!(is_cargo_target_locked(&locked_target));

    let config = UserPhaseConfig {
        projects_dir: &projects,
        result_age_days: 7.0,
        target_age_hours: 24.0,
        nix_gc_age_days: 14,
        dry_run: false,
        now_ns: fx.now_ns,
        deadline: Instant::now() + Duration::from_secs(60),
    };
    let stats = run_user_phase_in_process("tester", &fx.root, &config);
    assert_eq!(stats.targets_removed_count, 0);
    assert_eq!(stats.targets_skipped_active_count, 1);
    assert!(locked_target.exists());

    drop(guard);
    assert!(!is_cargo_target_locked(&locked_target));
}

#[test]
fn tmp_nix_cleanup_handles_readonly_age_and_active_pid() {
    let fx = TestFixture::new("tmp-nix");
    let tmp_dir = fx.root.join("fake-tmp");
    fs::create_dir_all(&tmp_dir).expect("mkdir fake-tmp");
    let old_mtime_ns = fx.now_ns - 48 * 3600 * NANOS_PER_SECOND;

    // 1. Stale nix-shell dir with read-only nested dir & file => remove.
    let stale_shell = tmp_dir.join("nix-shell.abc123");
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
    let active_build = tmp_dir.join("nix-build-demo.drv-0");
    fs::create_dir_all(&active_build).expect("mkdir active_build");
    let recent_file = active_build.join("progress.log");
    fs::write(&recent_file, "building").expect("write progress.log");
    set_mtime_ns(&active_build, old_mtime_ns);
    set_mtime_ns(&recent_file, fx.now_ns - 3600 * NANOS_PER_SECOND);

    // 3. Old nix-develop dir belonging to our own live PID => MUST NOT remove.
    let live_develop = tmp_dir.join(format!("nix-develop-{}-999999", std::process::id()));
    fs::create_dir_all(&live_develop).expect("mkdir live_develop");
    set_mtime_ns(&live_develop, old_mtime_ns);

    // 4. Old non-nix dir => MUST NOT remove.
    let unrelated = tmp_dir.join("claude-other");
    fs::create_dir_all(&unrelated).expect("mkdir unrelated");
    set_mtime_ns(&unrelated, old_mtime_ns);

    // 5. Symlink named nix-shell.link => MUST NOT remove.
    let symlink_entry = tmp_dir.join("nix-shell.link");
    symlink(&unrelated, &symlink_entry).expect("symlink nix-shell.link");
    set_mtime_ns(&symlink_entry, old_mtime_ns);

    let stats = clean_nix_tmp_dirs(&tmp_dir, 24.0, false, fx.now_ns);
    assert_eq!(stats.dirs_removed_count, 1);
    assert_eq!(stats.dirs_skipped_active_count, 1);
    assert_eq!(stats.dirs_skipped_recent_count, 1);
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

    assert_eq!(parse_nix_develop_pid("nix-develop-12345-abc"), Some(12345));
    assert_eq!(parse_nix_develop_pid("nix-develop-notapid-abc"), None);
    assert_eq!(parse_nix_develop_pid("nix-shell.123"), None);
}

#[test]
fn dry_run_preserves_everything() {
    let fx = TestFixture::new("dry-run");
    let projects = fx.root.join("Projects");
    let target = fx.make_cargo_project("dry-run-app", 72.0, Some("CACHEDIR.TAG"), true);
    let result_link = target.parent().expect("parent").join("result");
    symlink("/nix/store/dddd-demo", &result_link).expect("symlink result");
    set_mtime_ns(&result_link, fx.now_ns - 10 * 86_400 * NANOS_PER_SECOND);

    let config = UserPhaseConfig {
        projects_dir: &projects,
        result_age_days: 7.0,
        target_age_hours: 24.0,
        nix_gc_age_days: 14,
        dry_run: true,
        now_ns: fx.now_ns,
        deadline: Instant::now() + Duration::from_secs(60),
    };
    let stats = run_user_phase_in_process("tester", &fx.root, &config);
    assert_eq!(stats.result_links_removed_count, 1);
    assert_eq!(stats.targets_removed_count, 1);
    assert!(target.exists());
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
    let projects = fx.root.join("Projects");
    let hidden_dir = projects.join("repo").join(".volumes");
    fs::create_dir_all(&hidden_dir).expect("mkdir .volumes");
    let hidden_result = hidden_dir.join("result");
    symlink("/nix/store/eeee-hidden", &hidden_result).expect("symlink hidden");
    set_mtime_ns(&hidden_result, fx.now_ns - 30 * 86_400 * NANOS_PER_SECOND);

    let unreadable = projects.join("repo").join("locked-data");
    fs::create_dir_all(&unreadable).expect("mkdir locked-data");
    chmod(&unreadable, 0o000);

    let config = UserPhaseConfig {
        projects_dir: &projects,
        result_age_days: 7.0,
        target_age_hours: 24.0,
        nix_gc_age_days: 14,
        dry_run: false,
        now_ns: fx.now_ns,
        deadline: Instant::now() + Duration::from_secs(60),
    };
    let stats = run_user_phase_in_process("tester", &fx.root, &config);
    assert_eq!(stats.result_links_removed_count, 0);
    assert!(stats.errors.is_empty());
    assert!(fs::symlink_metadata(&hidden_result).is_ok());
}

#[test]
fn safe_rmtree_refuses_symlinks_preserves_parent_perms_and_batches() {
    let fx = TestFixture::new("safe-rmtree");
    let outside = fx.root.join("keep-outside");
    fs::create_dir_all(&outside).expect("mkdir outside");
    let sentinel = outside.join("precious.txt");
    fs::write(&sentinel, "keep").expect("write sentinel");

    let link_path = fx.root.join("sneaky-link");
    symlink(&outside, &link_path).expect("symlink sneaky-link");
    assert!(matches!(
        safe_rmtree(&link_path),
        Err(RmtreeError::UnsafePath(_))
    ));
    assert!(sentinel.exists());

    let regular_file = fx.root.join("not-a-dir");
    fs::write(&regular_file, "file").expect("write regular_file");
    assert!(matches!(
        safe_rmtree(&regular_file),
        Err(RmtreeError::UnsafePath(_))
    ));
    assert!(regular_file.exists());

    // Multi-batch deletion + read-only nested files must never chmod parent.
    let parent = fx.root.join("parent-box");
    fs::create_dir_all(&parent).expect("mkdir parent");
    chmod(&parent, 0o750);
    let parent_mode_before = fs::symlink_metadata(&parent)
        .expect("lstat parent")
        .permissions()
        .mode()
        & 0o7777;

    let victim = parent.join("victim");
    let nested = victim.join("a").join("b");
    fs::create_dir_all(&nested).expect("mkdir nested");
    for index in 0..10 {
        let file_path = victim.join(format!("f{index}.txt"));
        fs::write(&file_path, "data").expect("write victim file");
        chmod(&file_path, 0o444);
    }
    let leaf = nested.join("leaf.txt");
    fs::write(&leaf, "leaf").expect("write leaf");
    chmod(&leaf, 0o444);
    chmod(&nested, 0o555);
    chmod(&victim.join("a"), 0o555);

    safe_rmtree_with_batch_max(&victim, 3).expect("safe_rmtree_with_batch_max");

    assert!(!victim.exists());
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
    let tmp_dir = fx.root.join("race-tmp");
    fs::create_dir_all(&tmp_dir).expect("mkdir race-tmp");

    let outside = fx.root.join("outside-target");
    fs::create_dir_all(&outside).expect("mkdir outside");
    // Simulate a TOCTOU swap: first entry became a symlink before deletion.
    let first_swapped = tmp_dir.join("nix-shell.first");
    symlink(&outside, &first_swapped).expect("symlink swapped");
    let second_real = tmp_dir.join("nix-shell.second");
    fs::create_dir_all(&second_real).expect("mkdir second");

    let mut stats = TmpCleanupStats::default();
    remove_stale_tmp_dirs(
        &[(first_swapped.clone(), 4096), (second_real.clone(), 4096)],
        false,
        fx.now_ns + NANOS_PER_SECOND,
        &mut stats,
    );

    assert_eq!(stats.dirs_removed_count, 1);
    assert_eq!(stats.errors.len(), 1);
    assert!(
        stats
            .errors
            .first()
            .is_some_and(|e| e.contains("non-directory or symlink"))
    );
    assert!(fs::symlink_metadata(&first_swapped).is_ok());
    assert!(outside.exists());
    assert!(!second_real.exists());
}

#[test]
fn parse_args_rejects_nan_inf_bounds_and_system_roots() {
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
        vec!["--user", "tester", "--_now-ns", "0"],
        vec!["--user", "tester", "--_now-ns", "-1"],
        vec!["--user", "tester", "--_now-ns", "nan"],
    ];

    for argv in invalid_invocations {
        let outcome = parse_args_with_user_resolver(&os_args(&argv), || Ok("unused".to_string()));
        assert!(
            matches!(outcome, ParseOutcome::Error(_)),
            "expected error for argv={argv:?}, got {outcome:?}"
        );
    }

    // Excessive argument counts are rejected by CLI_ARGS_MAX.
    let too_many_args: Vec<OsString> = vec![OsString::from("--force"); CLI_ARGS_MAX + 1];
    assert!(matches!(
        parse_args_with_user_resolver(&too_many_args, || Ok("unused".to_string())),
        ParseOutcome::Error(_)
    ));

    // Explicit `--user` must not require current UID passwd lookup to succeed,
    // and `--_now-ns` preserves exact nanosecond i128 precision.
    let parsed = parse_args_with_user_resolver(
        &os_args(&[
            "--user=tester",
            "--force",
            "--_now-ns",
            "1764322500123456789",
        ]),
        || Err("missing uid".to_string()),
    );
    match parsed {
        ParseOutcome::Parsed(args) => {
            assert_eq!(args.user, "tester");
            assert!(args.force);
            assert_eq!(args.now_ns, Some(1_764_322_500_123_456_789));
        }
        other => panic!("expected Parsed, got {other:?}"),
    }
}

#[test]
fn fail_closed_on_flock_and_mtime_errors() {
    let fx = TestFixture::new("fail-closed");
    let projects = fx.root.join("Projects");
    let target = fx.make_cargo_project("repo-err", 72.0, Some(".cargo-lock"), true);

    // Symlink `.cargo-lock` must fail closed (O_NOFOLLOW -> ELOOP -> treated as locked).
    let lock_path = target.join(".cargo-lock");
    fs::remove_file(&lock_path).expect("remove lock file");
    symlink("/etc/passwd", &lock_path).expect("symlink .cargo-lock");
    assert!(is_cargo_target_locked(&target));
    fs::remove_file(&lock_path).expect("remove symlink");
    fs::write(&lock_path, "").expect("restore regular .cargo-lock");
    set_mtime_ns(&lock_path, fx.now_ns - 72 * 3600 * NANOS_PER_SECOND);
    set_mtime_ns(&target, fx.now_ns - 72 * 3600 * NANOS_PER_SECOND);

    if sys::effective_uid() != 0 {
        // Unreadable `.cargo-lock` must fail closed as locked.
        chmod(&lock_path, 0o000);
        assert!(is_cargo_target_locked(&target));
        chmod(&lock_path, 0o644);
        set_mtime_ns(&lock_path, fx.now_ns - 72 * 3600 * NANOS_PER_SECOND);
        set_mtime_ns(&target, fx.now_ns - 72 * 3600 * NANOS_PER_SECOND);

        // Unreadable inner subdirectory inside target/ must fail closed even at 0h age.
        let locked_subdir = target.join("debug").join("unreadable-sub");
        fs::create_dir_all(&locked_subdir).expect("mkdir unreadable-sub");
        set_mtime_ns(&locked_subdir, fx.now_ns - 72 * 3600 * NANOS_PER_SECOND);
        set_mtime_ns(
            &target.join("debug"),
            fx.now_ns - 72 * 3600 * NANOS_PER_SECOND,
        );
        chmod(&locked_subdir, 0o000);

        assert_eq!(
            latest_target_mtime_ns(&target, Some(fx.now_ns), TARGET_MTIME_DEPTH_MAX),
            None
        );
        let scan = scan_projects(
            &projects,
            &ProjectScanConfig {
                now_ns: fx.now_ns,
                result_age_days: 7.0,
                target_age_hours: 0.0,
            },
        );
        assert!(scan.target_candidates.is_empty());
        assert_eq!(scan.targets_skipped_recent_count, 1);

        // Same fail-closed behavior for unreadable subdirectory inside /tmp/nix-*.
        let tmp_dir = fx.root.join("tmp-unreadable");
        let stale = tmp_dir.join("nix-shell.unreadable");
        let inner = stale.join("blocked");
        fs::create_dir_all(&inner).expect("mkdir inner");
        let old_mtime_ns = fx.now_ns - 72 * 3600 * NANOS_PER_SECOND;
        set_mtime_ns(&inner, old_mtime_ns);
        set_mtime_ns(&stale, old_mtime_ns);
        chmod(&inner, 0o000);

        assert_eq!(
            inspect_tmp_tree(&stale, fx.now_ns - 24 * 3600 * NANOS_PER_SECOND),
            None
        );
    }
}

#[test]
fn pocpou_skips_target_result_symlink_and_tmp_dir_modified_between_scan_and_delete() {
    let fx = TestFixture::new("pocpou");
    let projects = fx.root.join("Projects");
    let target = fx.make_cargo_project("repo-race", 72.0, Some("CACHEDIR.TAG"), true);
    let result_link = target.parent().expect("parent").join("result");
    symlink("/nix/store/aaaa-old", &result_link).expect("symlink old result");
    set_mtime_ns(&result_link, fx.now_ns - 10 * 86_400 * NANOS_PER_SECOND);

    let scan_config = ProjectScanConfig {
        now_ns: fx.now_ns,
        result_age_days: 7.0,
        target_age_hours: 24.0,
    };
    let scan = scan_projects(&projects, &scan_config);
    assert_eq!(scan.stale_results.len(), 1);
    assert_eq!(scan.target_candidates.len(), 1);

    // 1. Simulate a fresh `nix build` replacing `result` between scan and unlink.
    fs::remove_file(&result_link).expect("remove old result");
    symlink("/nix/store/bbbb-new", &result_link).expect("symlink new result");
    let recent_mtime_ns = fx.now_ns - 10 * NANOS_PER_SECOND;
    set_mtime_ns(&result_link, recent_mtime_ns);

    // 2. Simulate a Cargo build running and finishing after `scan_projects` returned.
    let fresh_artifact = target.join("debug").join("fresh-bin");
    fs::write(&fresh_artifact, "just built").expect("write fresh-bin");
    set_mtime_ns(&fresh_artifact, recent_mtime_ns);
    set_mtime_ns(&target.join("debug"), recent_mtime_ns);

    let stats = apply_user_cleanup(
        &scan,
        &[],
        14,
        UserCleanupCutoffs {
            now_ns: fx.now_ns,
            result_mtime_max_ns: scan_config.result_mtime_max_ns(),
            target_mtime_max_ns: scan_config.target_mtime_max_ns(),
        },
        Instant::now() + Duration::from_secs(60),
    );
    assert_eq!(stats.result_links_removed_count, 0);
    assert_eq!(stats.targets_removed_count, 0);
    assert_eq!(stats.targets_skipped_recent_count, 1);
    assert!(fs::symlink_metadata(&result_link).is_ok());
    assert!(target.exists());

    // 3. Simulate a `/tmp/nix-*` directory modified between scan and deletion.
    let tmp_dir = fx.root.join("pocpou-tmp");
    let stale_tmp = tmp_dir.join("nix-shell.race");
    fs::create_dir_all(&stale_tmp).expect("mkdir stale_tmp");
    let old_mtime_ns = fx.now_ns - 48 * 3600 * NANOS_PER_SECOND;
    set_mtime_ns(&stale_tmp, old_mtime_ns);

    let tmp_scan = scan_stale_nix_tmp_dirs(&tmp_dir, 24.0, fx.now_ns);
    assert_eq!(tmp_scan.stale_dirs.len(), 1);

    let fresh_tmp_file = stale_tmp.join("active.log");
    fs::write(&fresh_tmp_file, "active").expect("write active.log");
    set_mtime_ns(&fresh_tmp_file, recent_mtime_ns);

    let mut tmp_stats = TmpCleanupStats::default();
    let tmp_cutoff_ns = fx.now_ns - 24 * 3600 * NANOS_PER_SECOND;
    remove_stale_tmp_dirs(&tmp_scan.stale_dirs, false, tmp_cutoff_ns, &mut tmp_stats);
    assert_eq!(tmp_stats.dirs_removed_count, 0);
    assert_eq!(tmp_stats.dirs_skipped_recent_count, 1);
    assert!(stale_tmp.exists());
}

#[test]
fn ipc_json_pair_validation_and_worst_case_error_budget() {
    let valid = UserPhaseStats {
        result_links_removed_count: 2,
        targets_removed_count: 1,
        targets_freed_bytes: 4096,
        targets_skipped_active_count: 1,
        targets_skipped_recent_count: 0,
        user_profiles_pruned_count: 1,
        errors: vec!["minor \"quoted\"\nerror\\path".to_string()],
    };
    assert_user_phase_stats_valid(&valid);
    let encoded = encode_user_phase_stats_json(&valid);
    let parsed = parse_user_phase_stats_json(&encoded).expect("round-trip JSON");
    assert_eq!(parsed, valid);

    // Worst-case error budget: ERRORS_MAX oversized UTF-8 + control-char error strings
    // must truncate cleanly, fit within SUBPROCESS_OUTPUT_BYTES_MAX, and round-trip in O(N).
    let mut worst_case = UserPhaseStats::default();
    let noisy_message = format!("{}\u{0001}\"\\é", "é".repeat(ERROR_MESSAGE_BYTES_MAX));
    for _ in 0..(ERRORS_MAX + 10) {
        append_error(&mut worst_case.errors, noisy_message.clone());
    }
    assert_eq!(worst_case.errors.len(), ERRORS_MAX);
    assert!(
        worst_case
            .errors
            .iter()
            .all(|msg| msg.len() <= ERROR_MESSAGE_BYTES_MAX)
    );
    let worst_encoded = encode_user_phase_stats_json(&worst_case);
    assert!(worst_encoded.len() <= SUBPROCESS_OUTPUT_BYTES_MAX);
    let worst_parsed =
        parse_user_phase_stats_json(&worst_encoded).expect("round-trip worst-case JSON");
    assert_eq!(worst_parsed, worst_case);

    // Cross-privilege semantic mismatch must return Err (not panic root).
    let inconsistent = "{\"result_links_removed_count\":0,\"targets_removed_count\":0,\"targets_freed_bytes\":1024,\"targets_skipped_active_count\":0,\"targets_skipped_recent_count\":0,\"user_profiles_pruned_count\":0,\"errors\":[]}";
    assert!(parse_user_phase_stats_json(inconsistent).is_err());

    let invalid_payloads = [
        "[]",
        "{\"unexpected\": 1}",
        "{\"result_links_removed_count\":0,\"targets_removed_count\":-1,\"targets_freed_bytes\":0,\"targets_skipped_active_count\":0,\"targets_skipped_recent_count\":0,\"user_profiles_pruned_count\":0,\"errors\":[]}",
        "{\"result_links_removed_count\":0,\"targets_removed_count\":true,\"targets_freed_bytes\":0,\"targets_skipped_active_count\":0,\"targets_skipped_recent_count\":0,\"user_profiles_pruned_count\":0,\"errors\":[]}",
        "{\"result_links_removed_count\":0,\"targets_removed_count\":0,\"targets_freed_bytes\":0,\"targets_skipped_active_count\":0,\"targets_skipped_recent_count\":0,\"user_profiles_pruned_count\":0,\"errors\":[123]}",
        "{\"result_links_removed_count\":0,\"targets_removed_count\":0,\"targets_freed_bytes\":0,\"targets_skipped_active_count\":0,\"targets_skipped_recent_count\":0,\"user_profiles_pruned_count\":0,\"errors\":[]} trailing",
    ];
    for raw in invalid_payloads {
        assert!(
            parse_user_phase_stats_json(raw).is_err(),
            "expected Err for payload: {raw}"
        );
    }
}

#[test]
#[should_panic(expected = "targets_freed_bytes must be 0 when no targets were removed")]
fn ipc_json_inconsistent_targets_trips_write_assertion() {
    let inconsistent = UserPhaseStats {
        targets_removed_count: 0,
        targets_freed_bytes: 1024,
        ..UserPhaseStats::default()
    };
    let _ = encode_user_phase_stats_json(&inconsistent);
}

#[test]
fn streaming_subprocess_bounds_timeout_and_truncation() {
    // 1. Timeout enforcement kills child promptly.
    let timed_opts = CommandOptions {
        program: OsString::from("/bin/sh"),
        args: os_args(&["-c", "sleep 30"]),
        timeout: Duration::from_millis(250),
        env_override: None,
        privilege_drop: None,
        stream_lines_max: 1000,
        capture_stdout_bytes_max: 0,
        capture_stderr_bytes_max: 0,
        inherit_stderr: false,
    };
    let timed = run_command_streaming::<fn(&str), fn(&str)>(&timed_opts, None, None);
    assert!(timed.timed_out);
    assert!(timed.error_message.is_none());

    // 2. Byte capture cap sets stdout_truncated without unbounded buffering.
    let capped_opts = CommandOptions {
        program: OsString::from("/bin/sh"),
        args: os_args(&["-c", "printf '%0500d\\n' 0"]),
        timeout: Duration::from_secs(5),
        env_override: None,
        privilege_drop: None,
        stream_lines_max: 1000,
        capture_stdout_bytes_max: 32,
        capture_stderr_bytes_max: 0,
        inherit_stderr: false,
    };
    let capped = run_command_streaming::<fn(&str), fn(&str)>(&capped_opts, None, None);
    assert_eq!(capped.exit_code, 0);
    assert!(capped.stdout_truncated);
    assert_eq!(capped.stdout_text.len(), 32);

    // 3. Line streaming callback works with zero stdout buffering.
    let mut seen_lines: Vec<String> = Vec::new();
    let stream_opts = CommandOptions {
        program: OsString::from("/bin/sh"),
        args: os_args(&["-c", "for i in 0 1 2 3 4; do echo line-$i; done"]),
        timeout: Duration::from_secs(5),
        env_override: None,
        privilege_drop: None,
        stream_lines_max: 1000,
        capture_stdout_bytes_max: 0,
        capture_stderr_bytes_max: 0,
        inherit_stderr: false,
    };
    let streamed = run_command_streaming(
        &stream_opts,
        Some(|line: &str| seen_lines.push(line.to_string())),
        None::<fn(&str)>,
    );
    assert_eq!(streamed.exit_code, 0);
    assert_eq!(streamed.stdout_lines_count, 5);
    assert_eq!(
        seen_lines,
        vec!["line-0", "line-1", "line-2", "line-3", "line-4"]
    );
}

#[test]
fn advisory_lock_contention_and_size_parsing_invariants() {
    let fx = TestFixture::new("lock-size");
    let lock_file = fx.root.join("sub").join("disk.lock");

    let first = AdvisoryLock::try_acquire(&lock_file).expect("first acquire");
    assert!(first.is_some());
    let second = AdvisoryLock::try_acquire(&lock_file).expect("second acquire");
    assert!(second.is_none());
    drop(first);

    let third = AdvisoryLock::try_acquire(&lock_file).expect("third acquire after drop");
    assert!(third.is_some());

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
        Some((1, 5 * 1024 * 1024))
    );
    assert_eq!(
        parse_nix_gc_freed_line("42 store paths deleted, 50.0 GiB freed"),
        Some((42, 50 * 1024_u64.pow(3)))
    );
}

#[test]
fn stat_filesystem_exact_integer_ceiling() {
    // Large block count (> 2^53) where IEEE-754 float division loses precision.
    let huge: u64 = 1_u64 << 54;
    let usage = compute_disk_usage(
        Path::new("/"),
        FilesystemStat {
            device_id: 42,
            fragment_size_bytes: 4096,
            blocks_total: huge,
            blocks_free: huge - 1,
            blocks_available: huge - 1,
        },
    );
    assert_eq!(usage.used_percent, 1);
    assert_eq!(usage.used_bytes, 4096);
    assert_eq!(usage.device_id, 42);
}

#[test]
fn threshold_trigger_and_notification_decisions() {
    let gib = 1024_u64.pow(3);
    let below = DiskUsage {
        path: PathBuf::from("/"),
        device_id: 1,
        used_bytes: 500 * gib,
        available_bytes: 500 * gib,
        used_percent: 50,
    };
    assert_eq!(
        decide_trigger(&below, 85, false),
        TriggerDecision::SkipBelowThreshold
    );
    assert_eq!(
        decide_trigger(&below, 85, true),
        TriggerDecision::Run {
            trigger_description: "forced at 50%".to_string()
        }
    );

    let before = DiskUsage {
        path: PathBuf::from("/"),
        device_id: 1,
        used_bytes: 900 * gib,
        available_bytes: 100 * gib,
        used_percent: 90,
    };
    let after = DiskUsage {
        path: PathBuf::from("/"),
        device_id: 1,
        used_bytes: 800 * gib,
        available_bytes: 200 * gib,
        used_percent: 80,
    };
    assert_eq!(
        decide_trigger(&before, 85, false),
        TriggerDecision::Run {
            trigger_description: "triggered at 90% >= 85%".to_string()
        }
    );

    let summary_parts = build_summary_parts(
        &UserPhaseStats {
            targets_removed_count: 1,
            targets_freed_bytes: 8192,
            ..UserPhaseStats::default()
        },
        &NixGcStats {
            ran: true,
            paths_deleted_count: 42,
            freed_bytes: 50 * gib,
            ..NixGcStats::default()
        },
        &TmpCleanupStats::default(),
    );
    let notification = build_notification(&before, &after, 50 * gib, &summary_parts)
        .expect("notification should fire");
    assert!(notification.title.contains("100.0 GiB"));
    assert!(notification.body.contains("90% → 80%"));
    assert!(notification.body.contains("1 Rust target/"));
    assert!(notification.body.contains("Nix GC"));

    // Nothing freed -> no notification.
    assert_eq!(build_notification(&before, &before, 0, &[]), None);

    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(2048), "2.0 KiB");
    assert_eq!(format_age(300 * 1_000_000_000), "5m");
    assert_eq!(format_age(7200 * 1_000_000_000), "2.0h");
    assert_eq!(format_age(172_800 * 1_000_000_000), "2.0d");
}
