#!/usr/bin/env python3
"""Pinned SDK identity checks include inactive platform dependencies."""
import copy
from pathlib import Path
import unittest
from sdk_source import inspect_metadata, REPOSITORY


class SdkSourceTests(unittest.TestCase):
    def setUp(self):
        self.rev = "b" * 40
        self.dependency = {"git": REPOSITORY, "rev": self.rev}
        self.source = f"git+{REPOSITORY}?rev={self.rev}#{self.rev}"
        self.metadata = {"packages": [
            {"name": name, "source": self.source,
             "manifest_path": f"/tmp/locked-sdk/crates/{name}/Cargo.toml", "dependencies": []}
            for name in ("mobile-linux-api", "platform-pty")
        ]}

    def inspect(self):
        return inspect_metadata(self.metadata, self.dependency)

    def test_exact_identity(self):
        self.assertEqual(self.inspect()["root"], str(Path("/tmp/locked-sdk").resolve()))

    def test_local_and_mixed_revision_rejected(self):
        for source in (None, self.source.replace(self.rev, "c" * 40),
                       self.source.replace("mobile-linux-runtime", "harness-runtime")):
            with self.subTest(source=source):
                self.metadata["packages"][1]["source"] = source
                with self.assertRaises(ValueError):
                    self.inspect()

    def test_duplicate_package(self):
        self.metadata["packages"].append(copy.deepcopy(self.metadata["packages"][1]))
        with self.assertRaises(ValueError):
            self.inspect()

    def test_missing_anchor(self):
        self.metadata["packages"].pop(0)
        with self.assertRaisesRegex(ValueError, "does not contain"):
            self.inspect()

    def test_resource_escape(self):
        self.metadata["packages"][1]["manifest_path"] = "/tmp/local/pty/Cargo.toml"
        with self.assertRaises(ValueError):
            self.inspect()

    def test_host_path_even_into_checkout_is_rejected(self):
        self.metadata["packages"].append({"name": "platform-api", "source": None, "dependencies": [
            {"name": "platform-pty", "path": "/tmp/locked-sdk/crates/platform-pty",
             "target": "cfg(target_os = ios)", "kind": "build"}
        ]})
        with self.assertRaisesRegex(ValueError, "local SDK"):
            self.inspect()

    def test_internal_contained_path(self):
        self.metadata["packages"][0]["dependencies"] = [
            {"name": "platform-pty", "path": "/tmp/locked-sdk/crates/platform-pty"}
        ]
        self.assertEqual(self.inspect()["revision"], self.rev)

    def test_inactive_alternate_dev_edge(self):
        self.metadata["packages"].append({"name": "platform-api", "dependencies": [
            {"name": "platform-pty", "source": self.source.replace(self.rev, "d" * 40),
             "target": "cfg(windows)", "kind": "dev"}
        ]})
        with self.assertRaisesRegex(ValueError, "inactive"):
            self.inspect()

    def test_unpinned_and_machine_alias(self):
        for dependency in ({"git": REPOSITORY, "rev": "main"},
                           {"git": "ssh://git@github.com-lingxi-coder/lingxi-coder/mobile-linux-runtime.git", "rev": self.rev}):
            with self.subTest(dependency=dependency), self.assertRaises(ValueError):
                inspect_metadata(self.metadata, dependency)


if __name__ == "__main__":
    unittest.main()
