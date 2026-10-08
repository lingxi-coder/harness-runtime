#!/usr/bin/env python3
"""Compare the captured Native SubagentStop request fields without normalization."""

import difflib
import hashlib
import json
from pathlib import Path
import re


ROOT = Path(__file__).resolve().parent
BASELINE = ROOT / "native-baseline"


def sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def text_paths(value: object, path: str = "$") -> list[tuple[str, str]]:
    if isinstance(value, str):
        return [(path, value)]
    if isinstance(value, dict):
        return [pair for key, child in value.items() for pair in text_paths(child, f"{path}.{key}")]
    if isinstance(value, list):
        return [pair for index, child in enumerate(value) for pair in text_paths(child, f"{path}[{index}]")]
    return []


def capture(version: str) -> tuple[dict, dict[str, bytes]]:
    version_dir = BASELINE / version
    receipt = json.loads((version_dir / "native-receipt.json").read_text(encoding="utf-8"))
    hook = next(item for item in receipt["requests"] if item.get("classification") == "subagent_stop_hook")
    fields = {
        field: (version_dir / "requests" / hook[f"{field}_artifact"]).read_bytes()
        for field in ("system", "messages")
    }
    return {"receipt": receipt, "hook": hook}, fields


def raw_field_record(raw: bytes, version: str, field: str) -> dict:
    return {
        "path": f"native-baseline/{version}/requests/request-{capture_request_number(version):02d}.{field}.raw.json",
        "bytes": len(raw),
        "sha256": sha256(raw),
    }


def capture_request_number(version: str) -> int:
    details, _ = capture(version)
    return int(details["hook"]["sequence"])


def system_delta(old: bytes, new: bytes) -> list[dict]:
    old_strings = dict(text_paths(json.loads(old)))
    new_strings = dict(text_paths(json.loads(new)))
    changes = []
    for path in sorted(set(old_strings) | set(new_strings)):
        before, after = old_strings.get(path), new_strings.get(path)
        if before == after:
            continue
        edits = []
        matcher = difflib.SequenceMatcher(None, before or "", after or "", autojunk=False)
        for tag, i1, i2, j1, j2 in matcher.get_opcodes():
            if tag != "equal":
                edits.append({"operation": tag, "removed": (before or "")[i1:i2], "added": (after or "")[j1:j2]})
        changes.append({"path": path, "edits": edits})
    return changes


def main() -> None:
    loaded = {version: capture(version) for version in ("2.1.293", "2.1.294")}
    raw = {version: loaded[version][1] for version in loaded}
    comparisons = {}
    for field in ("system", "messages"):
        old, new = raw["2.1.293"][field], raw["2.1.294"][field]
        comparisons[field] = {
            "raw_bytes_equal": old == new,
            "2.1.293": raw_field_record(old, "2.1.293", field),
            "2.1.294": raw_field_record(new, "2.1.294", field),
        }
        old_value, new_value = json.loads(old), json.loads(new)
        comparisons[field]["parsed_string_delta"] = system_delta(old, new)
        if field == "messages":
            ids = {}
            for version, value in (("2.1.293", old_value), ("2.1.294", new_value)):
                all_text = "\n".join(text for _, text in text_paths(value))
                matches = re.findall(r'"prompt_id"\s*:\s*"([^"]+)"', all_text)
                ids[version] = [{
                    "value": prompt_id,
                    "uuid_v4_shape": bool(re.fullmatch(
                        r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}", prompt_id
                    )),
                } for prompt_id in matches]
            comparisons[field]["dynamic_prompt_id_constraint"] = {
                "values": ids,
                "raw_bytes_compared_without_normalization": True,
            }
    summaries = {}
    for version, (details, _) in loaded.items():
        receipt, hook = details["receipt"], details["hook"]
        raw_messages = json.loads((BASELINE / version / "requests" / hook["messages_artifact"]).read_bytes())
        all_text = "\n".join(text for _, text in text_paths(raw_messages))
        raw_system = json.loads((BASELINE / version / "requests" / hook["system_artifact"]).read_bytes())
        system_text = "\n".join(text for _, text in text_paths(raw_system))
        summaries[version] = {
            "native_sha256": receipt["native_sha256"],
            "classification_counts": receipt["classification_counts"],
            "event_name_in_captured_message": bool(re.search(r'"hook_event_name"\s*:\s*"SubagentStop"', all_text)),
            "subagent_system_marker": "cc_is_subagent=true" in system_text,
            "parent_completed_with_fixed_response": receipt["parent_completed_with_fixed_response"],
            "hook_request_sha256": hook["request_sha256"],
        }
    result = {"versions": summaries, "hook_request_fields": comparisons}
    output = BASELINE / "comparison-2.1.293-vs-2.1.294.json"
    output.write_text(json.dumps(result, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, ensure_ascii=True, indent=2))


if __name__ == "__main__":
    main()
