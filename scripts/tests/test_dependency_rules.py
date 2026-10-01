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

    def test_shared_core_may_depend_on_a_shared_primitive(self):
        result = self.run_gate({"core": "crates/core", "mcp-wire": "crates/mcp-wire"},
                               {"core": ["mcp-wire"]})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_shared_primitive_cannot_depend_on_the_engine(self):
        result = self.run_gate({"core": "crates/core", "mcp-wire": "crates/mcp-wire"},
                               {"mcp-wire": ["core"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("mcp-wire depends on core — shared primitives have no workspace dependencies",
                      result.stderr)

    def test_a_standalone_service_may_use_the_shared_primitives(self):
        result = self.run_gate(
            {"local-apps": "crates/local-apps", "mcp-wire": "crates/mcp-wire",
             "rooted-fs": "crates/rooted-fs"},
            {"local-apps": ["mcp-wire", "rooted-fs"]})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_standalone_service_cannot_depend_on_the_engine(self):
        for engine in ("core", "tasks", "branding"):
            with self.subTest(engine=engine):
                result = self.run_gate(
                    {"local-apps": "crates/local-apps", engine: f"crates/{engine}"},
                    {"local-apps": [engine]})
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"local-apps depends on {engine} — a standalone service may reach",
                              result.stderr)

    def test_a_standalone_service_keeps_libgit2_behind_its_feature(self):
        gated = self.run_gate({"local-apps": "crates/local-apps"},
                              {"local-apps": [{"name": "git2", "optional": True}]})
        self.assertEqual(gated.returncode, 0, gated.stderr)
        unconditional = self.run_gate({"local-apps": "crates/local-apps"},
                                      {"local-apps": [{"name": "git2", "optional": False}]})
        self.assertEqual(unconditional.returncode, 1)
        self.assertIn("local-apps depends on git2 unconditionally", unconditional.stderr)

    def test_the_service_may_sit_on_the_core_with_its_default_features_off(self):
        result = self.run_gate(
            {"local-app-service": "crates/local-app-service", "local-apps": "crates/local-apps",
             "mcp-wire": "crates/mcp-wire"},
            {"local-app-service": [{"name": "local-apps", "uses_default_features": False},
                                   "mcp-wire"]})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_the_service_cannot_depend_on_the_engine(self):
        for engine in ("core", "client", "mcp", "lsp", "tasks", "permission"):
            with self.subTest(engine=engine):
                result = self.run_gate(
                    {"local-app-service": "crates/local-app-service", engine: f"crates/{engine}"},
                    {"local-app-service": [engine]})
                self.assertEqual(result.returncode, 1)
                self.assertIn(
                    f"local-app-service depends on {engine} — a standalone service may reach",
                    result.stderr)

    def test_the_device_api_is_a_shared_primitive_the_service_and_core_may_use(self):
        ok = self.run_gate(
            {"local-app-service": "crates/local-app-service", "local-apps": "crates/local-apps",
             "device-api": "crates/device-api", "core": "crates/core"},
            {"local-app-service": [{"name": "local-apps", "uses_default_features": False},
                                   "device-api"],
             "core": ["device-api"]})
        self.assertEqual(ok.returncode, 0, ok.stderr)
        bad = self.run_gate({"device-api": "crates/device-api", "core": "crates/core"},
                            {"device-api": ["core"]})
        self.assertEqual(bad.returncode, 1)
        self.assertIn("device-api depends on core — shared primitives have no workspace dependencies",
                      bad.stderr)

    def test_the_core_cannot_depend_on_the_service_above_it(self):
        result = self.run_gate(
            {"local-apps": "crates/local-apps", "local-app-service": "crates/local-app-service"},
            {"local-apps": ["local-app-service"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("local-apps depends on local-app-service — a standalone service may reach",
                      result.stderr)

    def test_the_service_cannot_pull_the_core_in_with_its_default_features(self):
        result = self.run_gate(
            {"local-app-service": "crates/local-app-service", "local-apps": "crates/local-apps"},
            {"local-app-service": [{"name": "local-apps", "uses_default_features": True}]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("local-app-service depends on local-apps with its default features",
                      result.stderr)

    def test_core_may_name_the_primitives_of_the_local_app_project(self):
        result = self.run_gate({"core": "crates/core"},
                               {"core": ["mcp-wire", "rooted-fs", "device-api", "local-app-contracts"]})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_core_cannot_name_the_crates_above_the_primitives(self):
        for crate in ("local-apps", "local-app-service", "local-app-plugin"):
            with self.subTest(crate=crate):
                result = self.run_gate({"core": "crates/core"}, {"core": [crate]})
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"core depends on {crate} — only", result.stderr)

    def test_no_other_engine_crate_may_name_the_local_app_crates(self):
        for engine in ("permission", "client", "lsp", "session"):
            with self.subTest(engine=engine):
                result = self.run_gate({engine: f"crates/{engine}"}, {engine: ["rooted-fs"]})
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"{engine} depends on rooted-fs — only", result.stderr)

    def test_the_consumers_the_table_lists_are_let_through(self):
        for consumer, crate in (("mcp", "mcp-wire"), ("tasks", "local-app-contracts"),
                                ("tasks", "local-app-plugin"), ("agent", "local-app-plugin"),
                                ("workflow", "local-app-plugin")):
            with self.subTest(consumer=consumer, crate=crate):
                result = self.run_gate({consumer: f"crates/{consumer}"}, {consumer: [crate]})
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_the_composition_root_may_name_every_local_app_crate(self):
        result = self.run_gate({}, {"harness-runtime": ["local-apps", "local-app-service",
                                                         "local-app-plugin", "local-app-contracts",
                                                         "rooted-fs"]})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_the_rule_by_name_holds_when_the_crates_are_not_workspace_members(self):
        # After the move the project's crates are not in `paths`: the member-based rules cannot see these edges, this one can.
        result = self.run_gate({"core": "crates/core", "tasks": "crates/tasks"},
                               {"core": ["local-app-service"], "tasks": ["local-app-contracts"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("core depends on local-app-service — only", result.stderr)
        self.assertNotIn("tasks depends on", result.stderr)

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
