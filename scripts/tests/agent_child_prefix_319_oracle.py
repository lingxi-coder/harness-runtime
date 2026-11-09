#!/usr/bin/env python3
"""Derive child prefix/context fixtures from unmodified 2.1.319 API captures."""
import hashlib
import json
import re
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[2]
capture_root = Path(sys.argv[1])
cases = []


def normalize(text):
    text = text.replace("Claude Code, Anthropic's official CLI for Claude", "LingXi")
    text = text.replace("CLAUDE.md", "LINGXI.md").replace("between bash calls", "between shell tool calls")
    text = re.sub(r"(?m)^ - Primary working directory: .*", " - Primary working directory: <CWD>", text)
    text = re.sub(r"(?m)^ - OS Version: .*", " - OS Version: <OS_VERSION>", text)
    return re.sub(r"Today's date is \d{4}-\d{2}-\d{2}\.", "Today's date is <DATE>.", text)


for filename in sorted(capture_root.glob("*/requests.json")):
    requests = json.loads(filename.read_text())
    children = [request["body"] for request in requests
                if any("cc_is_subagent=true" in block.get("text", "")
                       for block in request["body"].get("system", []))]
    assert children, filename
    child = children[0]
    assert any("cc_version=2.1.319." in block.get("text", "") for block in child["system"]), filename
    prefix = next(block["text"] for block in child["system"] if block.get("text", "").startswith("WIRE_CHILD_AGENT\n\n"))
    system_rows = [message for message in child["messages"] if message["role"] == "system"]
    if system_rows:
        placement = "system_after_task"
        sections = system_rows[0]["content"][0]["text"].split("\n\n")
    else:
        placement = "user_before_task"
        blocks = child["messages"][0]["content"]
        sections = [block["text"].removeprefix("<system-reminder>\n").removesuffix("\n").removesuffix("\n</system-reminder>")
                    for block in blocks if block.get("text", "").startswith("<system-reminder>\n")]
    assert len(sections) == 3, (filename, sections)
    suffix = re.search(r"<total_tokens>.*?</total_tokens>$", prefix)
    cases.append({
        "model": child["model"],
        "captureSha256": hashlib.sha256(filename.read_bytes()).hexdigest(),
        "systemBody": normalize(prefix),
        "systemSuffix": suffix.group(0) if suffix else None,
        "contextPlacement": placement,
        "contextSections": [normalize(section) for section in sections],
    })
assert {case["model"] for case in cases} == {"claude-sonnet-4-6", "claude-opus-5", "claude-sonnet-5-5"}
fixture = {
    "version": "2.1.319",
    "artifactSha256": "1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd",
    "normalization": ["LingXi brand and portable shell label", "working directory", "OS version", "date"],
    "cases": cases,
}
destination = root / "scripts/tests/fixtures/agent_child_prefix_319.json"
encoded = json.dumps(fixture, indent=2, ensure_ascii=False) + "\n"
if "--check" in sys.argv:
    assert destination.read_text() == encoded
    print("Claude Code 2.1.319: three captured child prefixes and context placements match")
else:
    destination.write_text(encoded)
    print(destination)
