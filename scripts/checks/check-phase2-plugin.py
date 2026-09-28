#!/usr/bin/env python3
"""Validate the checked-in Phase 2 Local App Plugin migration.

This gate is intentionally independent of the Rust packer: it checks the
source manifest, the copied bytes, the per-family inventories/catalog, the
skill roster, and the build inventory as one cross-referenced contract. A
successful Rust build alone cannot detect a stale migration manifest or an
orphan accidentally reintroduced into a template family.
"""

from __future__ import annotations

import hashlib
import json
import re
import sys
from pathlib import Path


REPO = Path(__file__).resolve().parents[2]
CODE = REPO / "crates"
PLUGIN = CODE / "plugins" / "lingxi-local-app"
MANIFEST = REPO / "docs" / "local-apps" / "harness" / "template-migration-manifest.json"
INVENTORY = CODE / "harness-runtime" / "builtin-plugin-inventory.txt"
PROFILE_RS = CODE / "harness-runtime" / "src" / "mobile" / "local_app_runtime_profiles.rs"
PERMISSIONS_RS = CODE / "local-apps" / "src" / "permissions.rs"
PERMISSIONS_ASSET = CODE / "local-apps" / "assets" / "default-workspace-settings.local.json"
TASKS_PHASE2 = REPO / "docs" / "local-apps" / "harness" / "tasks-phase-2.json"

EXPECTED_SKILLS = {
    "accessibility",
    "apple-design",
    "babylon-3d-local-app",
    "canvas-2d-local-app",
    "create-local-app",
    "device",
    "expose-as-mcp",
    "frontend-design",
    "frontend-qa",
    "ionic-react-local-app",
    "llm-agent",
    "llm-sidequery",
    "local-app-background",
    "local-app-capture-view",
    "local-app-data",
    "local-app-debug",
    "local-app-inspect-view",
    "local-app-interact",
    "local-app-run",
    "local-app-test",
    "local-app-use",
    "mcp-flow-binding",
    "mcp-qa",
    "mcp-tool-design",
    "phaser-2d-local-app",
    "react-best-practices",
    "threejs-local-app",
}
# The subset of EXPECTED_SKILLS that the top-level `skills/` tree mirrors
# byte-for-byte. It is a separate roster because only these are package-native
# copies of a root skill, but it must stay a subset: a skill that is renamed or
# dropped from the Plugin would otherwise silently leave the byte comparison.
MIRRORED_SKILLS = {
    "accessibility", "apple-design", "babylon-3d-local-app", "canvas-2d-local-app", "create-local-app",
    "frontend-design", "frontend-qa", "ionic-react-local-app", "phaser-2d-local-app",
    "react-best-practices", "threejs-local-app",
}
EXPECTED_COMPONENT_FILES = {
    "agents": {
        "mcp-designer.md", "mcp-promoter.md", "operator.md", "tester.md", "verifier.md",
    },
    "workflows": {
        "local-app-mcp-authoring.js", "local-app-use-test.js",
    },
    "schemas": {
        "authoring-spec.schema.json", "mcp-proposal.schema.json", "qa-report.schema.json", "workflow-agent-results.schema.json",
        "use-test-report.schema.json",
    },
}
EXPECTED_FAMILIES = {
    "react-dom": "react_dom",
    "canvas-2d": "canvas_2d",
    "three-3d": "three_3d",
    "phaser-2d": "phaser_2d",
    "babylon-3d": "babylon_3d",
}
CURRENT_REVISION = 4
EXPECTED_TEMPLATE_REVISIONS = {family: CURRENT_REVISION for family in EXPECTED_FAMILIES}
CURRENT_WIDGET_ASSETS = {
    f"shared/mcp-widget/r4/{name}" for name in (
        "index.html", "package.json", "pnpm-lock.yaml", "src/main.jsx", "src/widget.jsx", "vite.config.mjs"
    )
}


def current_inventory(template_root: Path, family: str, family_key: str, base: list[dict]) -> dict:
    records = {item["path"]: dict(item) for item in base}
    for name in sorted(CURRENT_WIDGET_ASSETS):
        path = require_file(template_root / name, "current MCP widget")
        relative = "app/mcp-widget/" + Path(name).relative_to("shared/mcp-widget/r4").as_posix()
        records[relative] = {"path": relative, "bytes": path.stat().st_size, "sha256": sha256(path)}
    files = sorted(records.values(), key=lambda item: item["path"])
    return {
        "schemaVersion": 2, "family": family_key, "sourceFamily": family,
        "revision": CURRENT_REVISION, "sharedOverlay": "shared/mcp-widget/r4", "files": files,
        "totalBytes": sum(item["bytes"] for item in files),
    }

ORPHAN_NAMES = {
    "app/screens/detail-screen.jsx",
    "app/screens/home-screen.jsx",
    "src/stores/app-store.js",
}


def fail(message: str) -> None:
    print(f"PHASE2-PLUGIN FAIL: {message}", file=sys.stderr)
    raise SystemExit(1)


def require_file(path: Path, label: str) -> Path:
    if not path.is_file():
        fail(f"{label} missing: {path}")
    return path


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def canonical_json(value: object) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()


def read_json(path: Path, label: str) -> dict:
    require_file(path, label)
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        fail(f"{label} is not valid JSON: {error}")
    if not isinstance(value, dict):
        fail(f"{label} must be a JSON object")
    return value


def check_manifest_and_skills() -> tuple[dict, list[dict]]:
    manifest = read_json(PLUGIN / ".lingxi-plugin" / "plugin.json", "Plugin manifest")
    required = {
        "name": "lingxi-local-app",
        "displayName": "LingXi Local App",
        "version": "1.0.0",
        "defaultEnabled": True,
        "skills": "./skills/",
        "agents": "./agents/",
        "workflows": "./workflows/",
    }
    for key, expected in required.items():
        if manifest.get(key) != expected:
            fail(f"Plugin manifest {key!r} must be {expected!r}, got {manifest.get(key)!r}")
    if manifest.get("author", {}).get("name") != "LingXi":
        fail("Plugin manifest author.name must preserve the LingXi brand")

    skill_root = PLUGIN / "skills"
    if not skill_root.is_dir():
        fail(f"Plugin must contain exactly {len(EXPECTED_SKILLS)} skills; skill root is missing")
    skill_dirs = {path.name for path in skill_root.iterdir() if path.is_dir()}
    if skill_dirs != EXPECTED_SKILLS:
        fail(
            f"Plugin must contain exactly {len(EXPECTED_SKILLS)} skills; "
            f"missing={sorted(EXPECTED_SKILLS - skill_dirs)}, extra={sorted(skill_dirs - EXPECTED_SKILLS)}"
        )

    if not MIRRORED_SKILLS <= EXPECTED_SKILLS:
        fail(
            "root skill mirror roster names skills the Plugin does not ship: "
            f"{sorted(MIRRORED_SKILLS - EXPECTED_SKILLS)}"
        )
    root_skills = REPO / "skills"
    for name in sorted(MIRRORED_SKILLS):
        source = root_skills / name
        destination = skill_root / name
        if not source.is_dir():
            fail(f"root Local App skill source missing: {source}")
        source_files = {path.relative_to(source) for path in source.rglob("*") if path.is_file()}
        destination_files = {path.relative_to(destination) for path in destination.rglob("*") if path.is_file()}
        if source_files != destination_files:
            fail(f"skill migration file set differs for {name}: source/destination are not equal")
        for relative in source_files:
            source_file = source / relative
            destination_file = destination / relative
            if source_file.read_bytes() != destination_file.read_bytes():
                fail(f"skill migration bytes differ: {source_file} vs {destination_file}")
        if not (destination / "SKILL.md").is_file() or not (destination / "agents" / "openai.yaml").is_file():
            fail(f"{name} must include SKILL.md and inert agents/openai.yaml metadata")

    for name in sorted(EXPECTED_SKILLS):
        skill_file = skill_root / name / "SKILL.md"
        lines = skill_file.read_text(encoding="utf-8").splitlines()
        if len(lines) < 3 or lines[0].strip() != "---" or "---" not in lines[1:]:
            fail(f"{skill_file} does not have parseable YAML frontmatter")
        closing = lines[1:].index("---") + 1
        frontmatter = "\n".join(lines[1:closing])
        if not re.search(r"(?m)^name:\s*\S", frontmatter) or not re.search(r"(?m)^description:\s*\S", frontmatter):
            fail(f"{skill_file} frontmatter must contain non-empty name and description")

    for directory, expected in EXPECTED_COMPONENT_FILES.items():
        entries = {path.name for path in (PLUGIN / directory).iterdir() if path.is_file()}
        if entries != expected:
            fail(
                f"Plugin {directory} roster differs: "
                f"missing={sorted(expected - entries)}, extra={sorted(entries - expected)}"
            )
    return manifest, []


def check_migration(manifest: dict) -> None:
    migration = read_json(MANIFEST, "template migration manifest")
    entries = migration.get("files")
    if not isinstance(entries, list) or len(entries) != 112:
        fail(f"template migration manifest must lock exactly 112 files, got {len(entries) if isinstance(entries, list) else entries!r}")
    if migration.get("callSites") != 112 or migration.get("entries") != 112:
        fail(f"template migration callSites/entries must both be 112, got {migration.get('callSites')!r}/{migration.get('entries')!r}")
    if migration.get("missingOnDisk") or migration.get("referencedButUntracked"):
        fail("template migration manifest contains missingOnDisk or referencedButUntracked entries")

    expected_destinations: set[str] = set()
    family_entries: dict[str, list[dict]] = {family: [] for family in EXPECTED_FAMILIES}
    for entry in entries:
        family = entry.get("family")
        relative = entry.get("path")
        if family not in EXPECTED_FAMILIES or not isinstance(relative, str) or not relative:
            fail(f"invalid template migration entry: {entry!r}")
        if Path(relative).is_absolute() or ".." in Path(relative).parts:
            fail(f"unsafe template migration path: {relative!r}")
        expected_source = (
            CODE / "local-apps" / "templates" / "runtime-profiles" / family / "r4" / relative
        )
        source = REPO / entry.get("source", "")
        if source != expected_source:
            fail(
                f"template migration source must be the current production asset for "
                f"{family}/{relative}, got {source}"
            )
        destination = PLUGIN / "assets" / "templates" / family / "r4" / relative
        expected_destinations.add(destination.relative_to(PLUGIN / "assets" / "templates").as_posix())
        family_entries[family].append(entry)
        require_file(source, f"template source {family}/{relative}")
        require_file(destination, f"template destination {family}/{relative}")
        source_bytes = source.read_bytes()
        destination_bytes = destination.read_bytes()
        if source_bytes != destination_bytes:
            fail(f"template migration bytes differ: {source} vs {destination}")
        if len(source_bytes) != entry.get("bytes") or sha256(source) != entry.get("sha256"):
            fail(f"template migration manifest digest is stale for {family}/{relative}")
    if {family: len(items) for family, items in family_entries.items()} != migration.get("perFamily"):
        fail(f"template migration perFamily counts do not match entries: {migration.get('perFamily')!r}")
    if migration.get("totalBytes") != sum(item["bytes"] for item in entries):
        fail("template migration totalBytes does not match its 112 entries")

    profile_text = PROFILE_RS.read_text(encoding="utf-8")
    template_root = PLUGIN / "assets" / "templates"
    actual_assets = {
        path.relative_to(template_root).as_posix() for path in template_root.rglob("*")
        if path.is_file() and path.name not in {"inventory.json", "catalog.json"}
    }
    expected_assets = expected_destinations | CURRENT_WIDGET_ASSETS
    if actual_assets != expected_assets:
        fail(f"current template assets differ: missing={sorted(expected_assets - actual_assets)}, extra={sorted(actual_assets - expected_assets)}")
    for family in EXPECTED_FAMILIES:
        for base in (template_root / family, CODE / "local-apps/templates/runtime-profiles" / family):
            if {path.name for path in base.iterdir() if path.is_dir()} != {"r4"}:
                fail(f"only the current template revision may remain: {base}")
        if family != "react-dom":
            for relative in ORPHAN_NAMES:
                if (template_root / family / "r4" / relative).exists():
                    fail(f"unrelated DOM source in canvas template: {family}/{relative}")
    if migration.get("orphanedTrackedFiles") != []:
        fail("removed template revisions must not remain as compatibility records")

    inventories: dict[str, dict] = {}
    for family, family_key in EXPECTED_FAMILIES.items():
        inventory_path = template_root / family / "r4" / "inventory.json"
        inventory = read_json(inventory_path, f"{family} inventory")
        inventories[family] = inventory
        family_files = family_entries[family]
        # Preserve the migration manifest's stable call-site order. The
        # inventory is consumed by the packer as an ordered declaration, so a
        # path-set-only comparison would allow an accidental reordering to
        # drift from the checked-in golden.
        expected_records = [
            {"path": item["path"], "bytes": item["bytes"], "sha256": item["sha256"]}
            for item in family_files
        ]
        if (
            inventory.get("schemaVersion") != 1
            or inventory.get("family") != family_key
            or inventory.get("sourceFamily") != family
            or inventory.get("revision") != CURRENT_REVISION
        ):
            fail(f"{inventory_path} has wrong schema/family/sourceFamily/revision")
        if inventory.get("files") != expected_records:
            fail(f"{inventory_path} does not equal the migration manifest records")
        if inventory.get("totalBytes") != sum(item["bytes"] for item in expected_records):
            fail(f"{inventory_path} totalBytes is stale")

    catalog = read_json(template_root / "catalog.json", "runtime profile catalog")
    toolchain_match = re.search(
        r'const RUNTIME_PROFILE_TOOLCHAIN_KEY: &str = "([^"]+)";', profile_text
    )
    if toolchain_match is None:
        fail("cannot resolve the production runtime profile toolchain key")
    if catalog.get("schemaVersion") != 2 or catalog.get("toolchainKey") != toolchain_match.group(1):
        fail("runtime profile catalog schema/toolchain differs from production")
    templates = catalog.get("templates")
    if not isinstance(templates, list) or len(templates) != 5:
        fail(f"runtime profile catalog must contain exactly 5 templates, got {templates!r}")
    seen_families = set()
    for template in templates:
        family = next((raw for raw, key in EXPECTED_FAMILIES.items() if key == template.get("family")), None)
        if family is None or template.get("revision") != EXPECTED_TEMPLATE_REVISIONS[family]:
            fail(f"catalog contains unknown family or revision: {template!r}")
        if template.get("templateId") != f"{family}-r{EXPECTED_TEMPLATE_REVISIONS[family]}":
            fail(f"catalog templateId is not canonical for {family}")
        expected_surface = "dom" if family == "react-dom" else "canvas"
        if template.get("surface") != expected_surface:
            fail(f"catalog surface is stale for {family}")
        if not re.fullmatch(r"[0-9a-f]{64}", str(template.get("contractSha256", ""))):
            fail(f"catalog contractSha256 is not a canonical SHA-256 for {family}")
        seen_families.add(family)
        if template.get("mcpDefaultEnabled") is not False:
            fail(f"catalog mcpDefaultEnabled must be false for {family}")
        suggestions = template.get("mcpSuggestions")
        if not isinstance(suggestions, list) or not suggestions or not all(isinstance(item, str) and item for item in suggestions):
            fail(f"catalog mcpSuggestions must be a non-empty string list for {family}")
        inventory_value = inventories[family]
        expected_digest = hashlib.sha256(canonical_json(current_inventory(
            template_root, family, EXPECTED_FAMILIES[family], inventory_value["files"]
        ))).hexdigest()
        if template.get("inventorySha256") != expected_digest:
            fail(f"catalog inventorySha256 is stale for {family}")
        if template.get("available") is not (family != "babylon-3d"):
            fail(f"catalog availability must keep Babylon fail-closed: {template!r}")
        if family == "babylon-3d" and "real-device" not in template.get("availabilityReason", ""):
            fail("Babylon catalog entry must explain the unavailable real-device validation")
    if seen_families != set(EXPECTED_FAMILIES):
        fail(f"catalog families differ from runtime profile families: {seen_families!r}")


def check_build_inventory_and_permissions() -> None:
    inventory_lines = [line.strip() for line in require_file(INVENTORY, "build inventory").read_text().splitlines() if line.strip() and not line.lstrip().startswith("#")]
    if len(inventory_lines) != len(set(inventory_lines)):
        fail("build inventory contains duplicate paths")
    if inventory_lines != sorted(inventory_lines):
        fail("build inventory paths must stay sorted")
    actual = {path.relative_to(PLUGIN).as_posix() for path in PLUGIN.rglob("*") if path.is_file()}
    if set(inventory_lines) != actual:
        fail(
            "build inventory must equal the actual Plugin directory: "
            f"missing={sorted(actual - set(inventory_lines))[:4]}, extra={sorted(set(inventory_lines) - actual)[:4]}"
        )
    source = require_file(PERMISSIONS_ASSET, "permission settings asset")
    if "default-workspace-settings.local.json" not in PERMISSIONS_RS.read_text(encoding="utf-8"):
        fail("permissions.rs must include the migrated production settings asset")
    try:
        permission_payload = json.loads(source.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        fail(f"permission settings asset is not valid JSON: {error}")
    if not isinstance(permission_payload.get("permissions"), dict):
        fail("permission settings asset must retain the production permissions object")
    if (PLUGIN / "assets" / "templates" / "vite-react-static-v1").exists():
        fail("permission settings must not be copied into Plugin template assets")
    profile_text = PROFILE_RS.read_text(encoding="utf-8")
    if len(re.findall(r'profile_file!\(\s*"', profile_text)) < 112:
        fail("local_app_runtime_profiles.rs must retain at least the 112 current profile_file! call sites")
    if "plugins/lingxi-local-app/assets/templates/" not in profile_text or "runtime-profiles" in profile_text:
        fail("runtime profile production includes must point only at Plugin template assets")


def check_phase2_task_evidence() -> None:
    descriptor = read_json(TASKS_PHASE2, "Phase 2 task descriptor")
    tasks = descriptor.get("tasks")
    if not isinstance(tasks, list) or {task.get("id") for task in tasks} != {"P2.0", "P2.1", "P2.2", "P2.3"}:
        fail("Phase 2 task descriptor must contain exactly P2.0 through P2.3")
    for task in tasks:
        owned_sources: set[Path] = set()
        for owned in task.get("owns", []):
            path = REPO / owned
            if path.is_file() and path.suffix == ".rs":
                owned_sources.add(path)
            elif path.is_dir():
                owned_sources.update(path.rglob("*.rs"))
        rust_sources = "\n".join(
            path.read_text(encoding="utf-8", errors="ignore")
            for path in sorted(owned_sources)
        )
        for test_name in task.get("addedTests", []):
            if not re.search(rf"\bfn\s+{re.escape(test_name)}\b", rust_sources):
                fail(
                    f"Phase 2 task {task.get('id')} claims nonexistent addedTests symbol "
                    f"{test_name!r}"
                )


def main() -> int:
    check_manifest_and_skills()
    migration = read_json(MANIFEST, "template migration manifest")
    check_migration(migration)
    check_build_inventory_and_permissions()
    check_phase2_task_evidence()
    roster = ", ".join(
        f"{len(EXPECTED_COMPONENT_FILES[name])} {name}"
        for name in ("agents", "workflows", "schemas")
    )
    print(
        f"PHASE2-PLUGIN OK: {len(EXPECTED_SKILLS)} skills, {roster}, "
        "112 current profile assets, 6 shared MCP widget assets, "
        "no historical template revisions, and exact build inventory"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
