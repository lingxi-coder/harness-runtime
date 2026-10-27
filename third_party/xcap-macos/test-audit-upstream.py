#!/usr/bin/env python3
"""Offline checks for the upstream-only xcap advisory audit."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import patch


sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("audit_xcap", Path(__file__).with_name("audit-upstream.py"))
assert spec and spec.loader
audit_xcap = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit_xcap)


class UpstreamAuditTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.vendor = self.root / "xcap-macos"
        self.vendor.mkdir()
        self.archive = self.root / "xcap.crate"
        self.archive.write_bytes(b"fixture upstream archive")
        self.digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        (self.vendor / "UPSTREAM.json").write_text(json.dumps({
            "name": "xcap", "version": "0.5.2", "artifact_sha256": self.digest,
        }))
        self.package = {"name": "xcap", "version": "0.5.2", "manifest_path": str(self.vendor / "Cargo.toml")}
        self.packages = [self.package]
        self.calls = []
        self.verifier_error = None
        self.audit_error = None
        self.metadata_error = None
        self.addCleanup(patch.stopall)
        patch.object(audit_xcap, "VENDOR_ROOT", self.vendor).start()
        patch.object(audit_xcap, "ARCHIVE_SHA256", self.digest).start()
        patch.object(audit_xcap.subprocess, "run", side_effect=self.fake_run).start()

    def fake_run(self, command, **kwargs):
        self.calls.append(command)
        if command[:2] == ["cargo", "metadata"]:
            self.assertEqual(command[2:], ["--locked", "--format-version=1"])
            if self.metadata_error:
                raise subprocess.CalledProcessError(self.metadata_error, command)
            return subprocess.CompletedProcess(command, 0, stdout=json.dumps({"packages": self.packages}))
        if command[0] == sys.executable:
            self.assertEqual(command[1:], [str(self.vendor / "verify-upstream.py"), "--archive", str(self.archive)])
            if self.verifier_error:
                raise subprocess.CalledProcessError(self.verifier_error, command)
            return subprocess.CompletedProcess(command, 0)
        self.assertEqual(command[:5], ["cargo", "audit", "--deny", "warnings", "--file"])
        directory = Path(kwargs["cwd"])
        self.assertEqual(Path(command[5]), directory / "Cargo.lock")
        with Path(command[5]).open("rb") as lock:
            self.assertEqual(tomllib.load(lock), {"version": 4, "package": [{
                "name": "xcap", "version": "0.5.2",
                "source": "registry+https://github.com/rust-lang/crates.io-index", "checksum": self.digest,
            }]})
        with (directory / ".cargo/audit.toml").open("rb") as config:
            self.assertEqual(tomllib.load(config), {"advisories": {"ignore": []}})
        if self.audit_error:
            raise subprocess.CalledProcessError(self.audit_error, command)
        return subprocess.CompletedProcess(command, 0)

    def test_checks_exact_package_and_audits_only_registry_xcap(self):
        audit_xcap.audit(self.root, self.archive)
        self.assertEqual(len(self.calls), 3)

    def test_missing_multiple_wrong_version_or_unexpected_source_fails_closed(self):
        variants = [[], [self.package, self.package], [{**self.package, "version": "0.9.8"}],
                    [{**self.package, "manifest_path": str(self.root / "other/Cargo.toml")}]]
        for packages in variants:
            with self.subTest(packages=packages):
                self.packages = packages
                self.calls.clear()
                with self.assertRaises(ValueError):
                    audit_xcap.audit(self.root, self.archive)
                self.assertEqual(len(self.calls), 1)

    def test_changed_archive_or_identity_fails_before_audit(self):
        self.archive.write_bytes(b"corrupt archive")
        with self.assertRaisesRegex(ValueError, "archive checksum"):
            audit_xcap.audit(self.root, self.archive)
        self.assertEqual(len(self.calls), 1)
        self.archive.write_bytes(b"fixture upstream archive")
        identity = self.vendor / "UPSTREAM.json"
        identity.write_text(json.dumps({"name": "xcap", "version": "0.5.2", "artifact_sha256": "0" * 64}))
        with self.assertRaisesRegex(ValueError, "identity differs"):
            audit_xcap.audit(self.root, self.archive)

    def test_metadata_verifier_and_advisory_failures_propagate(self):
        for attribute, status, expected_calls in (("metadata_error", 2, 1), ("verifier_error", 3, 2), ("audit_error", 7, 3)):
            with self.subTest(stage=attribute):
                setattr(self, attribute, status)
                self.calls.clear()
                with self.assertRaises(subprocess.CalledProcessError) as failure:
                    audit_xcap.audit(self.root, self.archive)
                self.assertEqual(failure.exception.returncode, status)
                self.assertEqual(len(self.calls), expected_calls)
                setattr(self, attribute, None)

    def test_registry_download_failure_does_not_fall_back(self):
        with patch.dict(os.environ, {"CARGO_HOME": str(self.root / "empty-cargo-home")}):
            with patch.object(audit_xcap.urllib.request, "urlopen", side_effect=OSError("registry unavailable")):
                with self.assertRaisesRegex(OSError, "registry unavailable"):
                    audit_xcap.audit(self.root)
        self.assertEqual(len(self.calls), 1)


if __name__ == "__main__":
    unittest.main()
