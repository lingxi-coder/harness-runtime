#!/usr/bin/env python3
"""The diagnostic must not report success after scanning an empty source set."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "checks" / "prompt_drift_scan.py"
CONTROL = "Execute a workflow script that orchestrates multiple subagents deterministically."
LITERAL = "This is a deliberately long model-visible instruction used to verify that the prompt audit actually visits matching source files."


class PromptDriftScanTests(unittest.TestCase):
    def test_unmatched_pattern_fails_instead_of_reporting_zero_candidates(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "oracle.js").write_text(CONTROL)
            result = subprocess.run(
                [sys.executable, str(SCRIPT), tmp, str(root / "missing" / "*.rs")],
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("matched no files", result.stderr)

    def test_no_comparable_literals_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "oracle.js").write_text(CONTROL)
            source = root / "short.rs"
            source.write_text('const TEXT: &str = "too short";')
            result = subprocess.run(
                [sys.executable, str(SCRIPT), tmp, str(source)],
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("no comparable prompt literals", result.stderr)

    def test_overlapping_patterns_do_not_inflate_coverage(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "oracle.js").write_text(CONTROL + "\n" + LITERAL)
            source = root / "prompt.rs"
            source.write_text(f'const TEXT: &str = "{LITERAL}";')
            result = subprocess.run(
                [sys.executable, str(SCRIPT), tmp, str(source), str(root / "*.rs")],
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("checked 1 literals; 0 candidate(s)", result.stdout)


if __name__ == "__main__":
    unittest.main()
