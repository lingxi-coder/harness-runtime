#!/usr/bin/env python3
"""Probes for scripts/checks/check_product_neutral.py, each in a throwaway git repository."""
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ENGINE = Path(__file__).resolve().parents[1] / "checks/check_product_neutral.py"


class ProductNeutral(unittest.TestCase):
    def repo(self, files, keep=""):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        for rel, text in files.items():
            path = root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        subprocess.run(["git", "-C", str(root), "add", "-A"], check=True)
        keep_path = root / "keep.txt"
        keep_path.write_text(keep, encoding="utf-8")
        return root, keep_path

    def run_gate(self, files, keep=""):
        root, keep_path = self.repo(files, keep)
        return subprocess.run([sys.executable, str(ENGINE), str(root), str(keep_path)], text=True, capture_output=True)

    def test_a_clean_tree_passes(self):
        result = self.run_gate({"crates/core/src/a.rs": "fn main() {}\n"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("1 files", result.stdout)

    def test_each_spelling_in_each_crate_is_found(self):
        for crate in ("core", "mcp", "tasks", "permission"):
            for text in ("LocalAppBuild", "local_app_id", "local-app-build", "// a Local App", "LOCAL_APP_ROOT",
                         "local_apps::ids"):
                with self.subTest(crate=crate, text=text):
                    result = self.run_gate({f"crates/{crate}/src/a.rs": f"x\n{text}\n"})
                    self.assertEqual(result.returncode, 1, text)
                    self.assertIn(f"crates/{crate}/src/a.rs names the Local App product at 2(", result.stderr)

    def test_ordinary_words_that_start_the_same_way_are_not_hits(self):
        result = self.run_gate({"crates/mcp/src/a.rs": "fn policy_loads_migrated_local_approval() {}\nlocal_application\n"})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_manifests_and_json_are_scanned_too(self):
        for name in ("Cargo.toml", "snapshot.json", "NOTES.md"):
            with self.subTest(name=name):
                result = self.run_gate({"crates/tasks/" + name: "local-app-contracts = 1\n", "crates/tasks/src/a.rs": ""})
                self.assertEqual(result.returncode, 1)

    def test_a_crate_that_may_name_the_product_is_not_scanned(self):
        result = self.run_gate({"crates/core/src/a.rs": "", "crates/client/src/a.rs": "LocalAppBuild\n",
                                "crates/runtime/src/a.rs": "LocalAppBuild\n"})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_an_untracked_file_is_not_scanned_so_the_driver_says_to_stage_first(self):
        root, keep = self.repo({"crates/core/src/a.rs": ""})
        (root / "crates/core/src/b.rs").write_text("LocalAppBuild\n", encoding="utf-8")
        result = subprocess.run([sys.executable, str(ENGINE), str(root), str(keep)], text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, "documents that only tracked files are scanned")

    def test_a_keep_line_allows_exactly_that_needle_in_that_file(self):
        keep = "crates/core/src/a.rs\tlocal-app-build\tthe on-disk name old data carries\n"
        files = {"crates/core/src/a.rs": "let p = \"/var/lingxi/local-app-build\";\n",
                 "crates/core/src/b.rs": "let p = \"/var/lingxi/local-app-build\";\n"}
        result = self.run_gate(files, keep)
        self.assertEqual(result.returncode, 1)
        self.assertIn("crates/core/src/b.rs", result.stderr)
        self.assertNotIn("crates/core/src/a.rs names", result.stderr)
        other = self.run_gate({"crates/core/src/a.rs": "LocalAppBuild\nlocal-app-build\n"}, keep)
        self.assertEqual(other.returncode, 1, "a kept needle does not excuse a different one on another line")
        self.assertIn("a.rs names the Local App product at 1(", other.stderr)

    def test_a_keep_line_that_matches_nothing_fails(self):
        keep = "crates/core/src/a.rs\tlocal-app-build\told reason\n"
        result = self.run_gate({"crates/core/src/a.rs": "fn main() {}\n"}, keep)
        self.assertEqual(result.returncode, 1)
        self.assertIn("matches nothing", result.stderr)

    def test_a_malformed_keep_line_is_refused(self):
        result = self.run_gate({"crates/core/src/a.rs": ""}, "crates/core/src/a.rs\tneedle\n")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("path<TAB>needle<TAB>reason", result.stderr)

    def test_scanning_nothing_is_a_failure_not_a_pass(self):
        result = self.run_gate({"README.md": "LocalApp\n"})
        self.assertEqual(result.returncode, 1)
        self.assertIn("scanned 0 files", result.stderr)


if __name__ == "__main__":
    unittest.main()
