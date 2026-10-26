"""Exercise the real installer transactions with fake assets and no elevation.

Run from the project folder: python3 -m unittest discover -s scripts/tests -v
macOS filesystem operations are mocked on Linux; native dialogs need a Mac.
"""
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
LINUX = (ROOT / "scripts/install-linux.sh").read_text()
MACOS = (ROOT / "scripts/install-macos.sh").read_text()
LINUX_FUNCTIONS = LINUX[LINUX.index("install_files() ("):LINUX.index("if needs_admin; then")]
MACOS_FUNCTIONS = MACOS[MACOS.index("shell_quote() {"):MACOS.index('\nif [ -w "/Applications" ]')]


class Installers(unittest.TestCase):
    def setUp(self):
        fixtures = ROOT / "target/installer-tests"
        fixtures.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(dir=fixtures)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.commands = self.root / "commands"
        self.commands.mkdir()
        self.work = self.root / "download's files"
        self.work.mkdir()
        self.prefix = self.root / "installed app's prefix"
        self.prefix.mkdir()
        self.env = dict(os.environ, PATH=f"{self.commands}:{os.environ['PATH']}")
        self.env.update(WORK_DIR=str(self.work), PREFIX=str(self.prefix))
        for name in ("update-desktop-database", "gtk-update-icon-cache"):
            self.command(name, "exit 1\n")  # Cache failure must not fail a committed update.
        self.command("mv", f"""
for arg in "$@"; do
    if [[ "${{FAIL_RENAME:-}}" == linux && "$arg" == */slintyt.desktop ]]; then exit 1; fi
    if [[ "${{FAIL_RENAME:-}}" == mac && "$arg" == */new.app ]]; then exit 1; fi
done
exec {shlex.quote(shutil.which('mv'))} "$@"
""")
        self.command("ditto", """
cp -a "$1" "$2"
if [[ "${FAIL_COPY:-}" == yes ]]; then exit 1; fi
""")

    def command(self, name, body):
        path = self.commands / name
        path.write_text("#!/bin/bash\nset -euo pipefail\n" + body)
        path.chmod(0o755)

    def shell(self, code):
        return subprocess.run(["bash", "-c", "set -euo pipefail\n" + code],
                              env=self.env, cwd=ROOT, capture_output=True, text=True, timeout=10)

    def linux_assets(self):
        paths = ["bin/slintyt", "share/applications/slintyt.desktop",
                 "share/icons/hicolor/256x256/apps/slintyt.png",
                 "share/icons/hicolor/scalable/apps/slintyt.svg"]
        sources = ["slintyt-linux-x86_64", "slintyt.desktop", "slintyt.png", "slintyt.svg"]
        for path, source in zip(paths, sources):
            target = self.prefix / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text("old " + source)
            (self.work / source).write_text("new " + source)
        return [self.prefix / path for path in paths]

    def linux(self):
        return self.shell(LINUX_FUNCTIONS + '\ninstall_files "$WORK_DIR" "$PREFIX"')

    def assert_clean(self):
        self.assertEqual(list(self.prefix.rglob(".slintyt-*")), [])
        self.assertEqual(list(self.prefix.glob(".SlintYT-*")), [])

    def test_linux_success_and_nonfatal_cache_failure(self):
        paths = self.linux_assets()
        result = self.linux()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(all(path.read_text().startswith("new ") for path in paths))
        self.assertTrue(os.access(paths[0], os.X_OK))
        self.assert_clean()

    def test_linux_stage_failure_preserves_old_files(self):
        paths = self.linux_assets()
        (self.work / "slintyt.svg").unlink()
        result = self.linux()
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(all(path.read_text().startswith("old ") for path in paths))
        self.assert_clean()

    def test_linux_partial_commit_rolls_back_binary(self):
        paths = self.linux_assets()
        self.env["FAIL_RENAME"] = "linux"
        result = self.linux()
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(all(path.read_text().startswith("old ") for path in paths))
        self.assert_clean()

    def test_linux_elevated_invocation_preserves_quoted_paths(self):
        paths = self.linux_assets()
        self.command("pkexec", 'echo called > "$PREFIX/prompt"\nexec "$@"\n')
        self.env["SLINTYT_ELEVATE"] = "gui"
        dispatch = LINUX[LINUX.index("if needs_admin; then"):LINUX.index("if ! command -v yt-dlp")]
        result = self.shell(LINUX_FUNCTIONS + "\nneeds_admin() { return 0; }\n" + dispatch)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.prefix / "prompt").exists())
        self.assertTrue(all(path.read_text().startswith("new ") for path in paths))

    def test_linux_cancelled_prompt_preserves_files(self):
        paths = self.linux_assets()
        self.command("pkexec", 'exit 126\n')
        self.env["SLINTYT_ELEVATE"] = "gui"
        dispatch = LINUX[LINUX.index("if needs_admin; then"):LINUX.index("if ! command -v yt-dlp")]
        result = self.shell(LINUX_FUNCTIONS + "\nneeds_admin() { return 0; }\n" + dispatch)
        self.assertEqual(result.returncode, 126)
        self.assertTrue(all(path.read_text().startswith("old ") for path in paths))

    def test_linux_missing_polkit_agent_is_not_reported_as_denied(self):
        paths = self.linux_assets()
        self.command("pkexec", 'exit 127\n')
        self.env["SLINTYT_ELEVATE"] = "gui"
        dispatch = LINUX[LINUX.index("if needs_admin; then"):LINUX.index("if ! command -v yt-dlp")]
        result = self.shell(LINUX_FUNCTIONS + "\nneeds_admin() { return 0; }\n" + dispatch)
        self.assertEqual(result.returncode, 1)
        self.assertIn("polkit authentication agent", result.stderr)
        self.assertNotIn("Permission was not granted", result.stderr)
        self.assertTrue(all(path.read_text().startswith("old ") for path in paths))

    def test_linux_failed_backup_cleanup_still_releases_the_lock(self):
        paths = self.linux_assets()
        self.command("rm", f"""
for arg in "$@"; do
    if [[ "$arg" == */.slintyt-update.* ]]; then exit 1; fi
done
exec {shlex.quote(shutil.which('rm'))} "$@"
""")
        result = self.linux()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("could not remove", result.stderr)
        self.assertTrue(all(path.read_text().startswith("new ") for path in paths))
        self.assertFalse((self.prefix / ".slintyt-install-lock").exists())

    @unittest.skipIf(os.geteuid() == 0, "Root bypasses directory write permissions")
    def test_linux_writable_prefix_with_protected_destination_needs_admin(self):
        self.linux_assets()
        result = self.shell(LINUX_FUNCTIONS + "\nneeds_admin")
        self.assertEqual(result.returncode, 1)
        directory = self.prefix / "share/applications"
        directory.chmod(0o555)
        try:
            result = self.shell(LINUX_FUNCTIONS + "\nneeds_admin")
            self.assertEqual(result.returncode, 0, result.stderr)
        finally:
            directory.chmod(0o755)

    def mac_assets(self):
        for parent, content in ((self.work, "new"), (self.prefix, "old")):
            binary = parent / "SlintYT.app/Contents/MacOS/slintyt"
            binary.parent.mkdir(parents=True)
            binary.write_text(content)
            binary.chmod(0o755)
        self.env.update(MOUNT_POINT=str(self.work), APP_NAME="SlintYT.app",
                        APP_DEST=str(self.prefix / "SlintYT.app"))
        return self.prefix / "SlintYT.app/Contents/MacOS/slintyt"

    def mac(self):
        # Use the exact serialized command handed to osascript/sudo.
        return self.shell(MACOS_FUNCTIONS + '\n/bin/sh -c "$REPLACE"')

    def test_mac_success_and_quoted_paths(self):
        binary = self.mac_assets()
        result = self.mac()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(binary.read_text(), "new")
        self.assert_clean()

    def test_mac_copy_failure_preserves_old_bundle(self):
        binary = self.mac_assets()
        self.env["FAIL_COPY"] = "yes"
        result = self.mac()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(binary.read_text(), "old")
        self.assert_clean()

    def test_mac_final_rename_failure_restores_old_bundle(self):
        binary = self.mac_assets()
        self.env["FAIL_RENAME"] = "mac"
        result = self.mac()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(binary.read_text(), "old")
        self.assert_clean()

    def test_mac_failed_backup_cleanup_still_releases_the_lock(self):
        binary = self.mac_assets()
        self.command("rm", f"""
for arg in "$@"; do
    if [[ "$arg" == */.SlintYT-update.* ]]; then exit 1; fi
done
exec {shlex.quote(shutil.which('rm'))} "$@"
""")
        result = self.mac()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("could not remove", result.stderr)
        self.assertEqual(binary.read_text(), "new")
        self.assertFalse((self.prefix / ".SlintYT-install-lock").exists())

    def test_mac_concurrent_install_is_rejected(self):
        binary = self.mac_assets()
        lock = self.prefix / ".SlintYT-install-lock"
        lock.mkdir()
        result = self.mac()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(binary.read_text(), "old")
        self.assertTrue(lock.exists())


if __name__ == "__main__":
    unittest.main()
