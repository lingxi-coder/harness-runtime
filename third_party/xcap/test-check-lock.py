#!/usr/bin/env python3
"""Exercise the fork-lock guard against source and checksum tampering."""

from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

FORK = Path(__file__).resolve().parent
ROOT = FORK.parents[1]
EXPECTED = '''name = "quick-error"
version = "2.0.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "a993555f31e5a609f617c12db6250dedcac1b0a85076912c436e6fc9b2c8e6a3"'''


class LockGuardTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        temp_root = Path(self.temp.name)
        self.fork = temp_root / "third_party" / "xcap"
        self.fork.mkdir(parents=True)
        shutil.copy2(ROOT / "Cargo.lock", temp_root / "Cargo.lock")
        shutil.copy2(FORK / "Cargo.lock", self.fork / "Cargo.lock")
        shutil.copy2(FORK / "check-lock.py", self.fork / "check-lock.py")

    def run_guard(self):
        return subprocess.run(
            [sys.executable, str(self.fork / "check-lock.py")],
            capture_output=True,
            text=True,
            check=False,
        )

    def tamper(self, changed):
        lock = self.fork / "Cargo.lock"
        original = lock.read_text()
        self.assertEqual(original.count(EXPECTED), 1)
        lock.write_text(original.replace(EXPECTED, changed, 1))
        result = self.run_guard()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("quick-error", result.stderr)

    def test_checked_in_locks_match(self):
        result = self.run_guard()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_changed_checksum(self):
        self.tamper(EXPECTED.replace("a993555f", "00000000"))

    def test_rejects_changed_source(self):
        self.tamper(EXPECTED.replace("registry+https://github.com/rust-lang/crates.io-index", "registry+https://example.invalid/index"))

    def test_rejects_changed_version(self):
        self.tamper(EXPECTED.replace('version = "2.0.1"', 'version = "2.0.2"'))

    def test_rejects_changed_identity_if_production_drops_package(self):
        root_lock = self.fork.parents[1] / "Cargo.lock"
        original = root_lock.read_text()
        marker = 'name = "quick-error"\nversion = "1.2.3"'
        self.assertEqual(original.count(marker), 1)
        root_lock.write_text(original.replace(marker, 'name = "other-quick-error"\nversion = "1.2.3"', 1))
        self.tamper(EXPECTED.replace("a993555f", "00000000"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
