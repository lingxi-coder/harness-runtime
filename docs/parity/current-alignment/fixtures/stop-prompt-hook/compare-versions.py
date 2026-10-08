#!/usr/bin/env python3
"""Compare captured raw system/messages fields for Native 2.1.293 and 2.1.294."""

import hashlib
import json
from pathlib import Path
import difflib
import re


ROOT = Path(__file__).resolve().parent
BASELINE = ROOT / "native-baseline"


def capture_dir(version: str) -> Path:
    saved = BASELINE / version
    if (saved / "requests" / "request-02.system.raw.json").exists():
        return saved
    return ROOT / "run-output" / version


def sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def strings(value: object, path: str = "$") -> dict[str, str]:
    found: dict[str, str] = {}
    if isinstance(value, str):
        found[path] = value
    elif isinstance(value, list):
        for index, child in enumerate(value):
            found.update(strings(child, f"{path}[{index}]"))
    elif isinstance(value, dict):
        for key, child in value.items():
            found.update(strings(child, f"{path}.{key}"))
    return found


def compact_delta(before: str, after: str) -> list[dict]:
    edits = []
    matcher = difflib.SequenceMatcher(None, before, after, autojunk=False)
    for tag, i1, i2, j1, j2 in matcher.get_opcodes():
        if tag == "equal":
            continue
        edits.append({
            "operation": tag,
            "removed": before[i1:i2],
            "added": after[j1:j2],
        })
    return edits


def field_record(version: str, field: str) -> tuple[bytes, dict]:
    path = capture_dir(version) / "requests" / f"request-02.{field}.raw.json"
    raw = path.read_bytes()
    return raw, {
        "path": str(path.relative_to(ROOT)),
        "bytes": len(raw),
        "sha256": sha256(raw),
    }


def main() -> None:
    comparison: dict = {"versions": ["2.1.293", "2.1.294"], "fields": {}}
    for field in ("system", "messages"):
        old, old_info = field_record("2.1.293", field)
        new, new_info = field_record("2.1.294", field)
        comparison["fields"][field] = {
            "raw_bytes_equal": old == new,
            "2.1.293": old_info,
            "2.1.294": new_info,
        }
        try:
            old_strings = strings(json.loads(old))
            new_strings = strings(json.loads(new))
        except (UnicodeDecodeError, json.JSONDecodeError):
            comparison["fields"][field]["parsed_string_delta"] = "unavailable"
            continue
        paths = sorted(set(old_strings) | set(new_strings))
        deltas = []
        prompt_ids: dict[str, list[dict]] = {"2.1.293": [], "2.1.294": []}
        for path in paths:
            before, after = old_strings.get(path), new_strings.get(path)
            for version, value in (("2.1.293", before), ("2.1.294", after)):
                if value is not None:
                    for prompt_id in re.findall(r'"prompt_id":"([^"]+)"', value):
                        prompt_ids[version].append({
                            "path": path,
                            "value": prompt_id,
                            "uuid_v4_shape": bool(re.fullmatch(
                                r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}",
                                prompt_id,
                            )),
                        })
            if before == after:
                continue
            before_ids = re.findall(r'"prompt_id":"([^"]+)"', before or "")
            after_ids = re.findall(r'"prompt_id":"([^"]+)"', after or "")
            if before_ids or after_ids:
                delta = {
                    "path": path,
                    "difference": "dynamic prompt_id reference differs; raw field remains unequal",
                    "prompt_id_values": {"2.1.293": before_ids, "2.1.294": after_ids},
                }
            else:
                delta = {"path": path, "edits": compact_delta(before or "", after or "")}
            if before is not None and after is not None:
                delta["raw_string_sha256"] = {
                    "2.1.293": sha256(before.encode("utf-8")),
                    "2.1.294": sha256(after.encode("utf-8")),
                }
            deltas.append(delta)
        comparison["fields"][field]["parsed_string_delta"] = deltas
        if field == "messages":
            comparison["fields"][field]["dynamic_prompt_id_constraints"] = {
                "values": prompt_ids,
                "raw_bytes_are_compared_without_normalization": True,
                "expected_shape": "UUIDv4; validate format/reference, not byte equality across Native runs",
            }

    output = BASELINE / "comparison-2.1.293-vs-2.1.294.json"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(comparison, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(comparison, ensure_ascii=True, indent=2))


if __name__ == "__main__":
    main()
