#!/usr/bin/env python3
"""Negative probes for the relocated dependency boundary, using Cargo-shaped data."""
import json
from pathlib import Path
import subprocess
import sys
import unittest

ENGINE = Path(__file__).resolve().parents[1] / "checks/check_deps.py"

class DependencyRules(unittest.TestCase):
    def run_gate(self, paths, edges):
        # The production gate traverses these four required runtime roots.
        paths = {**{n: f"crates/{n}" for n in
                   ("llm-runtime", "harness-runtime", "tool-api", "permission")}, **paths}
        metadata = {"workspace_root": "/runtime", "packages": [
            {"name": name, "manifest_path": f"/runtime/{path}/Cargo.toml",
             "dependencies": [dep if isinstance(dep, dict) else {"name": dep}
                              for dep in edges.get(name, [])]}
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

    def test_core_may_name_the_shared_primitives(self):
        result = self.run_gate({"core": "crates/core"},
                               {"core": ["mcp-wire", "rooted-fs", "device-api"]})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_nothing_may_name_the_crates_that_implement_local_app(self):
        for consumer in ("core", "harness-runtime", "platform-android"):
            for crate in ("local-apps", "local-app-builder-service", "local-app-builder-plugin",
                          "local-app-builder-contracts"):
                with self.subTest(consumer=consumer, crate=crate):
                    result = self.run_gate({consumer: f"crates/{consumer}"}, {consumer: [crate]})
                    self.assertEqual(result.returncode, 1)
                    self.assertIn(f"{consumer} depends on {crate} — only", result.stderr)

    def test_no_other_engine_crate_may_name_the_primitives(self):
        for engine in ("permission", "client", "lsp", "session", "tasks", "agent", "workflow"):
            with self.subTest(engine=engine):
                result = self.run_gate({engine: f"crates/{engine}"}, {engine: ["rooted-fs"]})
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"{engine} depends on rooted-fs — only", result.stderr)

    def test_the_consumers_the_table_lists_are_let_through(self):
        for consumer, crate in (("mcp", "mcp-wire"), ("harness-runtime", "rooted-fs"), ("harness-runtime", "mcp-wire")):
            with self.subTest(consumer=consumer, crate=crate):
                result = self.run_gate({consumer: f"crates/{consumer}"}, {consumer: [crate]})
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_the_rule_by_name_holds_when_the_crates_are_not_workspace_members(self):
        # The project's crates are not in `paths`: the member-based rules cannot see these edges, this one can.
        result = self.run_gate({"core": "crates/core", "mcp": "crates/mcp"},
                               {"core": ["local-app-builder-service"], "mcp": ["mcp-wire"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("core depends on local-app-builder-service — only", result.stderr)
        self.assertNotIn("mcp depends on", result.stderr)

    def test_shared_core_cannot_depend_on_telemetry(self):
        result = self.run_gate({"core": "crates/core", "telemetry": "crates/telemetry"},
                               {"core": ["telemetry"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("shared contracts must stay below domain and telemetry crates", result.stderr)

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
