#!/usr/bin/env python3
"""Check production agent text and compile the production background renderer."""
import json
import re
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = json.loads((ROOT / "scripts/tests/fixtures/agent_reminders_319.json").read_text())
assert FIXTURE["version"] == "2.1.319"
assert FIXTURE["artifactSha256"] == "1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd"


def constant(relative, name):
    source_path = ROOT / relative
    source = source_path.read_text()
    head = re.search(r"\bconst\s+" + re.escape(name) + r"\s*:[^=]+?=\s*", source)
    assert head, (relative, name)
    value = source[head.end():]
    included = re.match(r'include_str!\("([^"]+)"\)', value)
    if included:
        return (source_path.parent / included.group(1)).read_text()
    raw = re.match(r'r"(.*?)";', value, re.S)
    if raw:
        return raw.group(1)
    quoted = re.match(r'"(?:[^"\\]|\\.)*"', value, re.S)
    assert quoted, (relative, name)
    literal = re.sub(r"\\\n[ \t]*", "", quoted.group(0))
    literal = re.sub(r"\\u\{([0-9A-Fa-f]+)\}", lambda match: chr(int(match.group(1), 16)), literal)
    return json.loads(literal)


agents = FIXTURE["agents"]
builtins = "crates/agent/src/builtins.rs"
for name, expected in [
    ("GENERAL_PURPOSE_PROMPT", agents["general"]),
    ("EXPLORE_PROMPT", agents["explore"][0]["body"]),
    ("PLAN_PROMPT", agents["plan"][0]["body"]),
    ("WEB_FETCH_PROMPT", agents["webFetch"]["body"]),
    ("WEB_FETCH_WHEN_TO_USE", agents["webFetch"]["description"]),
]:
    assert constant(builtins, name) == expected, name

standard = FIXTURE["subagentAppendices"][0]["parts"]
for name, expected in zip(["SUBAGENT_CONSENT_PARAGRAPH", "SUBAGENT_NOTES_TRAILER"], standard):
    assert constant("crates/agent/src/handle.rs", name) == expected, name
rewritten = FIXTURE["subagentAppendices"][1]["parts"]
for filename, expected in zip(["subagent_consent_rewritten.md", "subagent_notes_rewritten.md"], rewritten):
    assert (ROOT / "crates/agent/src/templates" / filename).read_text() == expected, filename
assert constant("crates/orchestrator/src/prompt/silent_turn.rs", "SILENT_TURN_REMINDER_TEXT") == FIXTURE["silentTurn"]["body"]
assert constant("crates/orchestrator/src/prompt/bash_output_note.rs", "BASH_OUTPUT_AUDIENCE_NOTE") == FIXTURE["bashAudienceNote"]
for name, field in [("NON_USER_INPUT_HEADER", "normal"), ("IN_HUMAN_TURN_HEADER", "inHumanTurn")]:
    assert constant("crates/core/src/host/task_notification.rs", name) == FIXTURE["notificationHeaders"][field], name

for case in FIXTURE["skill"]:
    if case["coordinator"]:
        continue
    filename = "skill_rewritten_319.md" if case["rewrite"] else "skill_standard_319.md"
    assert (ROOT / "crates/tools/skill/src/templates" / filename).read_text() == case["body"], filename


def rust_string(value):
    assert '"######' not in value
    return 'r######"' + value + '"######'


skill_source = (ROOT / "crates/tools/skill/src/skill.rs").read_text()
skill_source = skill_source[skill_source.index("pub(crate) fn skill_prompt_for_rewrite"):skill_source.index("/// Tool name byte-lock.")]
skill_source = re.sub(r'include_str!\("([^"]+)"\)', lambda match: 'include_str!(' + rust_string(str(ROOT / "crates/tools/skill/src" / match.group(1))) + ')', skill_source)
source = [
    'extern crate self as branding;',
    'pub const DOT_DIR: &str = ".lingxi";',
    '#[path = ' + rust_string(str(ROOT / "crates/orchestrator/src/prompt/bg_session.rs")) + '] mod background;',
    skill_source,
    'fn main() {',
]
for case in FIXTURE["background"]:
    isolation = {"in-place": 'Some("none")', "worktree-first": 'Some("worktree")'}.get(case["isolation"], "None")
    source.append('assert_eq!(background::section_with_rewrite(Some("bg"), Some("/jobs/example"), ' + isolation + ', ' + str(case["rewrite"]).lower() + ').unwrap(), ' + rust_string(case["body"]) + ');')
for case in FIXTURE["skill"]:
    if not case["coordinator"]:
        expected = case["body"] + "\nWhen a skill explicitly loads reference topics through `args`, calling it with a topic not yet loaded retrieves new material; do not repeat a topic already present.\n"
        source.append('assert_eq!(skill_prompt_for_rewrite(' + str(case["rewrite"]).lower() + '), ' + rust_string(expected) + ');')
source += ['assert!(background::section(None, Some("/jobs/example"), None).is_none());', '}']
with tempfile.TemporaryDirectory(prefix="agent-reminders-319-") as directory:
    script = Path(directory) / "check.rs"
    script.write_text("\n".join(source))
    executable = Path(directory) / "check"
    subprocess.run(["rustc", "--edition=2021", "-A", "warnings", str(script), "-o", str(executable)], check=True)
    subprocess.run([str(executable)], check=True)
print("Claude Code 2.1.319: agent text, notes, provenance headers and compiled background/Skill renderers match")
