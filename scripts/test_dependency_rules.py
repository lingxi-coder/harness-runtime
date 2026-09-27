#!/usr/bin/env python3
"""Negative probes for the relocated dependency boundary, using Cargo-shaped data."""
import json
from pathlib import Path
import subprocess
import sys
import unittest

ENGINE = Path(__file__).with_name("check_deps.py")

class DependencyRules(unittest.TestCase):
    def run_gate(self, paths, edges):
        # The production gate traverses these four required runtime roots.
        paths = {**{n: f"crates/{n}" for n in
                   ("llm-runtime", "harness-runtime", "tool-api", "permission")}, **paths}
        metadata = {"workspace_root": "/runtime", "packages": [
            {"name": name, "manifest_path": f"/runtime/{path}/Cargo.toml",
             "dependencies": [{"name": dep} for dep in edges.get(name, [])]}
            for name, path in paths.items()
        ]}
        return subprocess.run([sys.executable, str(ENGINE)], input=json.dumps(metadata),
                              text=True, capture_output=True)

    def test_tool_sibling_is_rejected_under_crates(self):
        result = self.run_gate({"tool-web": "crates/tools/web", "tool-file": "crates/tools/file"},
                               {"tool-web": ["tool-file"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("tool-web (tool) depends on tool-file (tool)", result.stderr)

    def test_component_cannot_depend_on_harness(self):
        result = self.run_gate({"core": "crates/core"}, {"core": ["harness-runtime"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("core depends on the Harness composition root", result.stderr)

    def test_runtime_cannot_reach_tui_transitively(self):
        result = self.run_gate({"helper": "crates/helper", "tui": "crates/tui"},
                               {"harness-runtime": ["helper"], "helper": ["tui"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("harness-runtime -> helper -> tui", result.stderr)

    def test_tool_can_depend_on_abstract_api(self):
        result = self.run_gate({"tool-web": "crates/tools/web"}, {"tool-web": ["tool-api"]})
        self.assertEqual(result.returncode, 0, result.stderr)

if __name__ == "__main__":
    unittest.main()
