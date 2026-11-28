#!/usr/bin/env python3
"""Unit tests for disk_cleanup.py."""

from __future__ import annotations

import os
import pwd
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

import disk_cleanup


def set_mtime(path: Path, mtime: float) -> None:
    os.utime(path, (mtime, mtime), follow_symlinks=False)


class DiskCleanupTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.now = time.time()

    def tearDown(self) -> None:
        # Ensure any read-only test fixtures can be cleaned up.
        for dirpath, dirnames, filenames in os.walk(self.root):
            for name in dirnames:
                p = Path(dirpath) / name
                if not p.is_symlink():
                    os.chmod(p, 0o755)
            for name in filenames:
                p = Path(dirpath) / name
                if not p.is_symlink():
                    os.chmod(p, 0o644)
        self.tmp.cleanup()

    def _make_cargo_project(
        self,
        rel_path: str,
        age_hours: float,
        *,
        marker: str = "CACHEDIR.TAG",
        with_cargo_toml: bool = True,
    ) -> Path:
        project = self.root / "Projects" / rel_path
        target = project / "target"
        (target / "debug" / "deps").mkdir(parents=True)
        if with_cargo_toml:
            (project / "Cargo.toml").write_text('[package]\nname = "demo"\n', encoding="utf-8")
        if marker == "CACHEDIR.TAG":
            (target / "CACHEDIR.TAG").write_text(
                f"{disk_cleanup.CACHEDIR_TAG_SIGNATURE}\n", encoding="utf-8"
            )
        elif marker:
            (target / marker).write_text("", encoding="utf-8")
        (target / "debug" / "deps" / "artifact.rlib").write_bytes(b"x" * 8192)

        mtime = self.now - age_hours * 3600.0
        for p in sorted(target.rglob("*"), reverse=True):
            set_mtime(p, mtime)
        set_mtime(target, mtime)
        return target

    def test_stale_nix_result_symlink_safety(self) -> None:
        projects = self.root / "Projects"
        proj = projects / "demo"
        proj.mkdir(parents=True)

        # 1. Old Nix result symlink (even if broken -> /nix/store/...) => should remove.
        old_result = proj / "result"
        old_result.symlink_to("/nix/store/aaaa-demo")
        set_mtime(old_result, self.now - 10 * 86400)

        old_result_named = proj / "result-fmt"
        old_result_named.symlink_to("/nix/store/bbbb-fmt")
        set_mtime(old_result_named, self.now - 8 * 86400)

        # 2. Fresh Nix result symlink pointing to epoch-0 target => MUST NOT remove!
        #    (Verifies lstat mtime is checked, not followed target mtime.)
        fake_store_target = self.root / "fake-store-item"
        fake_store_target.write_text("store", encoding="utf-8")
        set_mtime(fake_store_target, 1.0)
        fresh_result = proj / "result-fresh"
        fresh_result.symlink_to("/nix/store/cccc-fresh")
        set_mtime(fresh_result, self.now - 2 * 86400)

        # 3. Old symlink named `result` pointing OUTSIDE /nix/store => MUST NOT remove!
        non_nix_result = proj / "result-local"
        non_nix_result.symlink_to(str(fake_store_target))
        set_mtime(non_nix_result, self.now - 30 * 86400)

        # 4. Old regular file named `result` => MUST NOT remove!
        regular_result_dir = projects / "other"
        regular_result_dir.mkdir()
        regular_result = regular_result_dir / "result"
        regular_result.write_text("important", encoding="utf-8")
        set_mtime(regular_result, self.now - 30 * 86400)

        stats = disk_cleanup.run_user_phase_in_process(
            user_name="tester",
            user_home=self.root,
            projects_dir=projects,
            result_age_days=7.0,
            target_age_hours=24.0,
            nix_gc_age_days=14,
            dry_run=False,
            now=self.now,
        )

        self.assertEqual(stats.result_links_removed, 2)
        self.assertFalse(old_result.is_symlink())
        self.assertFalse(old_result_named.is_symlink())
        self.assertTrue(fresh_result.is_symlink())
        self.assertTrue(non_nix_result.is_symlink())
        self.assertTrue(regular_result.is_file())

    def test_rust_target_cleanup_order_recency_and_markers(self) -> None:
        projects = self.root / "Projects"

        older = self._make_cargo_project("repo-old", age_hours=96)
        middle = self._make_cargo_project("repo-mid", age_hours=48, marker=".rustc_info.json")
        recent = self._make_cargo_project("repo-recent", age_hours=2)
        no_toml = self._make_cargo_project("not-rust", age_hours=96, with_cargo_toml=False)
        no_marker = self._make_cargo_project("src-target", age_hours=96, marker="")

        # Symlink named target inside a Cargo project must never be followed/removed.
        symlink_proj = projects / "repo-symlink"
        symlink_proj.mkdir(parents=True)
        (symlink_proj / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
        real_outside = self.root / "outside-keep"
        real_outside.mkdir()
        (symlink_proj / "target").symlink_to(real_outside)

        stats = disk_cleanup.UserPhaseStats()
        _, candidates = disk_cleanup.scan_projects(
            projects_dir=projects,
            now=self.now,
            result_age_days=7.0,
            target_age_hours=24.0,
            stats=stats,
        )
        # Oldest first!
        self.assertEqual([c.path for c in candidates], [older, middle])
        self.assertEqual(stats.targets_skipped_recent, 1)

        run_stats = disk_cleanup.run_user_phase_in_process(
            user_name="tester",
            user_home=self.root,
            projects_dir=projects,
            result_age_days=7.0,
            target_age_hours=24.0,
            nix_gc_age_days=14,
            dry_run=False,
            now=self.now,
        )
        self.assertEqual(run_stats.targets_removed, 2)
        self.assertFalse(older.exists())
        self.assertFalse(middle.exists())
        self.assertTrue(recent.exists())
        self.assertTrue(no_toml.exists())
        self.assertTrue(no_marker.exists())
        self.assertTrue(real_outside.exists())
        self.assertTrue((symlink_proj / "target").is_symlink())

    def test_rust_target_skips_active_cargo_lock(self) -> None:
        projects = self.root / "Projects"
        locked_target = self._make_cargo_project("repo-building", age_hours=72, marker=".cargo-lock")
        lock_path = locked_target / ".cargo-lock"

        # Hold an exclusive flock from a child process (just like cargo does).
        holder = subprocess.Popen(
            [
                sys.executable,
                "-c",
                (
                    "import fcntl, sys, time; "
                    "f = open(sys.argv[1], 'r+'); "
                    "fcntl.flock(f, fcntl.LOCK_EX); "
                    "print('locked', flush=True); "
                    "time.sleep(30)"
                ),
                str(lock_path),
            ],
            stdout=subprocess.PIPE,
            text=True,
        )
        try:
            assert holder.stdout is not None
            self.assertEqual(holder.stdout.readline().strip(), "locked")
            self.assertTrue(disk_cleanup.is_cargo_target_locked(locked_target))

            stats = disk_cleanup.run_user_phase_in_process(
                user_name="tester",
                user_home=self.root,
                projects_dir=projects,
                result_age_days=7.0,
                target_age_hours=24.0,
                nix_gc_age_days=14,
                dry_run=False,
                now=self.now,
            )
            self.assertEqual(stats.targets_removed, 0)
            self.assertEqual(stats.targets_skipped_active, 1)
            self.assertTrue(locked_target.exists())
        finally:
            if holder.stdout is not None:
                holder.stdout.close()
            holder.terminate()
            holder.wait(timeout=5)

        # Once unlocked, the same target dir becomes eligible.
        self.assertFalse(disk_cleanup.is_cargo_target_locked(locked_target))

    def test_tmp_nix_cleanup_handles_readonly_age_and_active_pid(self) -> None:
        tmp_dir = self.root / "fake-tmp"
        tmp_dir.mkdir()

        # 1. Stale nix-shell dir with read-only nested file => should remove.
        stale_shell = tmp_dir / "nix-shell.abc123"
        nested = stale_shell / "sub"
        nested.mkdir(parents=True)
        ro_file = nested / "readonly.txt"
        ro_file.write_text("store copy", encoding="utf-8")
        old_mtime = self.now - 48 * 3600
        set_mtime(ro_file, old_mtime)
        set_mtime(nested, old_mtime)
        set_mtime(stale_shell, old_mtime)
        os.chmod(ro_file, 0o444)
        os.chmod(nested, 0o555)

        # 2. Old top-level dir whose child was touched 1h ago => MUST NOT remove!
        active_build = tmp_dir / "nix-build-demo.drv-0"
        active_build.mkdir()
        recent_file = active_build / "progress.log"
        recent_file.write_text("building", encoding="utf-8")
        set_mtime(active_build, old_mtime)
        set_mtime(recent_file, self.now - 3600)

        # 3. Old nix-develop dir belonging to our own live PID => MUST NOT remove!
        live_develop = tmp_dir / f"nix-develop-{os.getpid()}-999999"
        live_develop.mkdir()
        set_mtime(live_develop, old_mtime)

        # 4. Old non-nix dir => MUST NOT remove!
        unrelated = tmp_dir / "claude-other"
        unrelated.mkdir()
        set_mtime(unrelated, old_mtime)

        # 5. Symlink named nix-shell.link => MUST NOT remove!
        symlink_entry = tmp_dir / "nix-shell.link"
        symlink_entry.symlink_to(unrelated)
        set_mtime(symlink_entry, old_mtime)

        stats = disk_cleanup.clean_nix_tmp_dirs(
            tmp_dir=tmp_dir,
            tmp_age_hours=24.0,
            dry_run=False,
            now=self.now,
        )

        self.assertEqual(stats.dirs_removed, 1)
        self.assertFalse(stale_shell.exists())
        self.assertTrue(active_build.exists())
        self.assertTrue(live_develop.exists())
        self.assertTrue(unrelated.exists())
        self.assertTrue(symlink_entry.is_symlink())

    def _fake_passwd(self) -> pwd.struct_passwd:
        return pwd.struct_passwd(
            ("tester", "x", os.geteuid(), os.getegid(), "Tester", str(self.root), "/bin/sh")
        )

    def test_threshold_skips_silently_below_limit(self) -> None:
        lock_file = self.root / "test.lock"
        fake_usage = disk_cleanup.DiskUsage(
            path="/",
            dev=1,
            used_bytes=500 * 1024**3,
            avail_bytes=500 * 1024**3,
            percent=50,
        )
        with (
            mock.patch.object(disk_cleanup.pwd, "getpwnam", return_value=self._fake_passwd()),
            mock.patch.object(disk_cleanup, "worst_disk_usage", return_value=fake_usage),
            mock.patch.object(disk_cleanup, "run_user_phase") as mock_user,
            mock.patch.object(disk_cleanup, "run_nix_gc") as mock_gc,
        ):
            rc = disk_cleanup.main(
                [
                    "--user",
                    "tester",
                    "--threshold",
                    "85",
                    "--projects-dir",
                    str(self.root / "Projects"),
                    "--lock-file",
                    str(lock_file),
                ]
            )
        self.assertEqual(rc, 0)
        mock_user.assert_not_called()
        mock_gc.assert_not_called()

    def test_threshold_triggers_cleanup_and_notifies_when_space_freed(self) -> None:
        lock_file = self.root / "test.lock"
        projects = self.root / "Projects"
        target = self._make_cargo_project("old-app", age_hours=72)

        before_usage = disk_cleanup.DiskUsage(
            path=str(self.root),
            dev=1,
            used_bytes=900 * 1024**3,
            avail_bytes=100 * 1024**3,
            percent=90,
        )
        after_usage = disk_cleanup.DiskUsage(
            path=str(self.root),
            dev=1,
            used_bytes=800 * 1024**3,
            avail_bytes=200 * 1024**3,
            percent=80,
        )
        fake_gc = disk_cleanup.NixGcStats(
            ran=True,
            paths_deleted=42,
            freed_bytes=50 * 1024**3,
        )
        with (
            mock.patch.object(disk_cleanup.pwd, "getpwnam", return_value=self._fake_passwd()),
            mock.patch.object(disk_cleanup, "worst_disk_usage", return_value=before_usage),
            mock.patch.object(disk_cleanup, "stat_filesystem", return_value=after_usage),
            mock.patch.object(disk_cleanup, "run_nix_gc", return_value=fake_gc),
            mock.patch.object(
                disk_cleanup, "clean_nix_tmp_dirs", return_value=disk_cleanup.TmpCleanupStats()
            ),
            mock.patch.object(disk_cleanup, "send_notification") as mock_notify,
        ):
            rc = disk_cleanup.main(
                [
                    "--user",
                    "tester",
                    "--threshold",
                    "85",
                    "--projects-dir",
                    str(projects),
                    "--lock-file",
                    str(lock_file),
                ]
            )
        self.assertEqual(rc, 0)
        self.assertFalse(target.exists())
        mock_notify.assert_called_once()
        _, title, body = mock_notify.call_args.args
        self.assertIn("100.0 GiB", title)
        self.assertIn("90% → 80%", body)
        self.assertIn("1 Rust target/", body)
        self.assertIn("Nix GC", body)

    def test_no_notification_when_nothing_freed(self) -> None:
        lock_file = self.root / "test.lock"
        projects = self.root / "Projects"
        projects.mkdir()

        steady_usage = disk_cleanup.DiskUsage(
            path=str(self.root),
            dev=1,
            used_bytes=900 * 1024**3,
            avail_bytes=100 * 1024**3,
            percent=90,
        )
        with (
            mock.patch.object(disk_cleanup.pwd, "getpwnam", return_value=self._fake_passwd()),
            mock.patch.object(disk_cleanup, "worst_disk_usage", return_value=steady_usage),
            mock.patch.object(disk_cleanup, "stat_filesystem", return_value=steady_usage),
            mock.patch.object(
                disk_cleanup, "run_nix_gc", return_value=disk_cleanup.NixGcStats(ran=True)
            ),
            mock.patch.object(
                disk_cleanup, "clean_nix_tmp_dirs", return_value=disk_cleanup.TmpCleanupStats()
            ),
            mock.patch.object(disk_cleanup, "send_notification") as mock_notify,
        ):
            rc = disk_cleanup.main(
                [
                    "--user",
                    "tester",
                    "--threshold",
                    "85",
                    "--projects-dir",
                    str(projects),
                    "--lock-file",
                    str(lock_file),
                ]
            )
        self.assertEqual(rc, 0)
        mock_notify.assert_not_called()

    def test_dry_run_preserves_everything(self) -> None:
        projects = self.root / "Projects"
        target = self._make_cargo_project("dry-run-app", age_hours=72)
        result_link = target.parent / "result"
        result_link.symlink_to("/nix/store/dddd-demo")
        set_mtime(result_link, self.now - 10 * 86400)

        stats = disk_cleanup.run_user_phase_in_process(
            user_name="tester",
            user_home=self.root,
            projects_dir=projects,
            result_age_days=7.0,
            target_age_hours=24.0,
            nix_gc_age_days=14,
            dry_run=True,
            now=self.now,
        )
        self.assertEqual(stats.result_links_removed, 1)
        self.assertEqual(stats.targets_removed, 1)
        self.assertTrue(target.exists())
        self.assertTrue(result_link.is_symlink())

    def test_skips_hidden_and_unreadable_directories(self) -> None:
        projects = self.root / "Projects"
        hidden_dir = projects / "repo" / ".volumes"
        hidden_dir.mkdir(parents=True)
        hidden_result = hidden_dir / "result"
        hidden_result.symlink_to("/nix/store/eeee-hidden")
        set_mtime(hidden_result, self.now - 30 * 86400)

        unreadable = projects / "repo" / "locked-data"
        unreadable.mkdir(parents=True)
        os.chmod(unreadable, 0o000)

        stats = disk_cleanup.run_user_phase_in_process(
            user_name="tester",
            user_home=self.root,
            projects_dir=projects,
            result_age_days=7.0,
            target_age_hours=24.0,
            nix_gc_age_days=14,
            dry_run=False,
            now=self.now,
        )
        self.assertEqual(stats.result_links_removed, 0)
        self.assertEqual(stats.errors, [])
        self.assertTrue(hidden_result.is_symlink())


if __name__ == "__main__":
    unittest.main()
