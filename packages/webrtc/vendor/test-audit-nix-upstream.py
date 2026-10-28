#!/usr/bin/env python3
"""Fail-closed source and identity tests for the nix advisory sentinel."""

import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("audit_nix", Path(__file__).with_name("audit-nix-upstream.py"))
assert spec and spec.loader
nix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(nix)


class NixAuditTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.workspace = self.root / "webrtc"
        self.vendor = self.workspace / "vendor/nix"
        self.vendor.mkdir(parents=True)
        self.archive = self.root / "nix.crate"
        self.fixture = {
            "Cargo.toml": b"[package]\nname = 'nix'\nversion = '0.31.3'\n",
            "LICENSE": b"MIT fixture", "README.md": b"readme", "build.rs": b"fn main() {}",
            "src/lib.rs": b"pub mod fixture;",
        }
        for name, changes in nix.PATCHES.items():
            self.fixture[name] = b"\n".join(old for old, _ in changes) + b"\n"
        with tarfile.open(self.archive, "w:gz") as tar:
            for name, data in self.fixture.items():
                member = tarfile.TarInfo(f"nix-{nix.VERSION}/{name}")
                member.size = len(data)
                tar.addfile(member, io.BytesIO(data))
                for old, new in nix.PATCHES.get(name, []):
                    data = data.replace(old, new, 1)
                path = self.vendor / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
        self.checksum = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        (self.workspace / "Cargo.toml").write_text('[workspace]\n[patch.crates-io]\nnix = { path = "vendor/nix" }\n')
        self.package = {"name": "nix", "version": nix.VERSION, "source": None, "manifest_path": str(self.vendor / "Cargo.toml")}
        self.packages = [self.package]
        self.calls = []
        self.audit_error = None
        self.metadata_error = None
        self.addCleanup(patch.stopall)
        patch.object(nix, "VENDOR", self.vendor).start()
        patch.object(nix, "CHECKSUM", self.checksum).start()
        patch.object(nix.subprocess, "run", side_effect=self.fake_run).start()

    def fake_run(self, command, **kwargs):
        self.calls.append(command)
        if command[:2] == ["cargo", "metadata"]:
            self.assertEqual(command[2:], ["--locked", "--all-features", "--format-version=1"])
            if self.metadata_error:
                raise subprocess.CalledProcessError(self.metadata_error, command)
            return subprocess.CompletedProcess(command, 0, stdout=json.dumps({"packages": self.packages}))
        self.assertEqual(command[:5], ["cargo", "audit", "--deny", "warnings", "--file"])
        directory = Path(kwargs["cwd"])
        with Path(command[5]).open("rb") as lock:
            self.assertEqual(tomllib.load(lock), {"version": 4, "package": [{
                "name": "nix", "version": nix.VERSION, "source": nix.registry_audit.REGISTRY_SOURCE,
                "checksum": self.checksum,
            }]})
        with (directory / ".cargo/audit.toml").open("rb") as config:
            self.assertEqual(tomllib.load(config), {"advisories": {"ignore": []}})
        if self.audit_error:
            raise subprocess.CalledProcessError(self.audit_error, command)
        return subprocess.CompletedProcess(command, 0)

    def test_local_graph_and_single_registry_identity(self):
        nix.audit(self.workspace, self.archive)
        self.assertEqual(len(self.calls), 2)

    def test_pinned_git_graph_and_unrelated_nix_versions(self):
        workspace = self.root / "app"
        workspace.mkdir()
        revision = "a" * 40
        (workspace / "Cargo.toml").write_text(f'''[workspace.dependencies]
harness-runtime = {{ git = "{nix.REPOSITORY}", rev = "{revision}" }}
[patch.crates-io]
nix = {{ git = "{nix.REPOSITORY}", rev = "{revision}" }}
''')
        source = f"git+{nix.REPOSITORY}?rev={revision}#{revision}"
        self.packages = [{**self.package, "source": source}, {**self.package, "version": "0.28.0", "source": "registry"}]
        nix.audit(workspace, self.archive)
        bad = [
            [{**self.packages[0], "source": source.replace(revision, "b" * 40)}],
            [{**self.packages[0], "source": "registry+https://github.com/rust-lang/crates.io-index"}],
            [self.packages[0], self.packages[0]], [],
            [{**self.packages[0], "manifest_path": str(self.root / "foreign/nix/Cargo.toml")}],
        ]
        for variant in bad:
            with self.subTest(packages=variant):
                self.packages = variant
                self.calls.clear()
                with self.assertRaises(ValueError):
                    nix.audit(workspace, self.archive)
                self.assertEqual(len(self.calls), 1)
        self.packages = [{**self.package, "source": source}]
        (workspace / "Cargo.toml").write_text((workspace / "Cargo.toml").read_text().replace(revision, "c" * 40, 1))
        with self.assertRaisesRegex(ValueError, "same pinned"):
            nix.audit(workspace, self.archive)

    def test_wrong_local_workspace_or_patch_rejected(self):
        (self.root / "Cargo.toml").write_text('[workspace]\n[patch.crates-io]\nnix = { path = "vendor/nix" }\n')
        with self.assertRaisesRegex(ValueError, "embedded WebRTC"):
            nix.verify_identity({"packages": self.packages}, self.root)
        (self.workspace / "Cargo.toml").write_text('[workspace]\n[patch.crates-io]\nnix = { path = "../nix" }\n')
        with self.assertRaisesRegex(ValueError, "embedded WebRTC"):
            nix.verify_identity({"packages": self.packages}, self.workspace)

    def test_only_four_reviewed_cfg_changes_are_allowed(self):
        nix.verify_source(self.archive)
        for name in nix.PATCHES:
            target = self.vendor / name
            original = target.read_bytes()
            for _, new in nix.PATCHES[name]:
                with self.subTest(name=name, context=new):
                    target.write_bytes(original.replace(new, new.replace(b'macos', b'ios'), 1))
                    with self.assertRaisesRegex(ValueError, "reviewed upstream patch"):
                        nix.verify_source(self.archive)
                    target.write_bytes(original)
            target.write_bytes(original + b"\n// unrelated change\n")
            with self.assertRaisesRegex(ValueError, "reviewed upstream patch"):
                nix.verify_source(self.archive)
            target.write_bytes(original)

    def test_missing_added_modified_and_symlink_files_rejected(self):
        target = self.vendor / "src/lib.rs"
        original = target.read_bytes()
        target.unlink()
        with self.assertRaisesRegex(ValueError, "inventory differs"):
            nix.verify_source(self.archive)
        target.write_bytes(original)
        extra = self.vendor / "extra.rs"
        extra.write_text("unexpected")
        with self.assertRaisesRegex(ValueError, "inventory differs"):
            nix.verify_source(self.archive)
        extra.unlink()
        target.unlink()
        target.symlink_to(self.vendor / "README.md")
        with self.assertRaisesRegex(ValueError, "unsupported"):
            nix.verify_source(self.archive)
        target.unlink()
        target.write_bytes(original)
        manifest = self.vendor / "Cargo.toml"
        manifest.write_bytes(manifest.read_bytes() + b'\n[build-dependencies]\nunreviewed = "1"\n')
        with self.assertRaisesRegex(ValueError, "reviewed upstream patch"):
            nix.verify_source(self.archive)

    def test_bad_archive_and_command_failures_propagate(self):
        self.archive.write_bytes(b"corrupt")
        with self.assertRaisesRegex(ValueError, "archive differs"):
            nix.audit(self.workspace, self.archive)
        self.assertEqual(len(self.calls), 1)
        for attr, exit_code in (("metadata_error", 2), ("audit_error", 7)):
            with self.subTest(stage=attr):
                setattr(self, attr, exit_code)
                with patch.object(nix, "verify_source"):
                    with self.assertRaises(subprocess.CalledProcessError) as error:
                        nix.audit(self.workspace, self.archive)
                self.assertEqual(error.exception.returncode, exit_code)
                setattr(self, attr, None)
        with patch.object(nix, "audit", side_effect=subprocess.CalledProcessError(7, ["cargo", "audit"])):
            with patch.object(sys, "argv", ["audit-nix-upstream.py"]):
                self.assertEqual(nix.main(), 7)

    def test_download_failure_does_not_fall_back(self):
        with patch.dict(os.environ, {"CARGO_HOME": str(self.root / "empty-cargo-home")}):
            with patch.object(nix.urllib.request, "urlopen", side_effect=OSError("registry unavailable")):
                with self.assertRaisesRegex(OSError, "registry unavailable"):
                    nix.archive_path(self.root)


if __name__ == "__main__":
    unittest.main()
