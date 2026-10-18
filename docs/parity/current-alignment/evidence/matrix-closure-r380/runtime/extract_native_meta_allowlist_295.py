#!/usr/bin/env python3
"""Extract Native 2.1.295's `I4e` allowlist from the frozen bundled source."""

from __future__ import annotations

import argparse
import ast
import hashlib
import json
import re
from pathlib import Path


NATIVE_SHA256 = "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70"
NATIVE_DEFAULT = Path("/Users/luolingfeng/.local/share/claude/versions/2.1.295")


def skip_space(source: bytes, index: int) -> int:
    while source[index] in b" \t\r\n":
        index += 1
    return index


def parse_atom(source: bytes, index: int, env: dict[str, object]):
    index = skip_space(source, index)
    quote = source[index]
    if quote in (ord('"'), ord("'"), ord("`")):
        cursor = index + 1
        escaped = False
        while cursor < len(source):
            char = source[cursor]
            if escaped:
                escaped = False
            elif char == ord("\\"):
                escaped = True
            elif char == quote:
                break
            cursor += 1
        raw = source[index : cursor + 1].decode("utf-8")
        if quote == ord("`"):
            body = raw[1:-1]
            if "${" in body:
                raise ValueError("template interpolation is outside the extracted allowlist")
            value = ast.literal_eval('"""' + body + '"""')
        else:
            value = ast.literal_eval(raw)
        return value, cursor + 1

    if quote == ord("["):
        values = []
        index = skip_space(source, index + 1)
        while source[index] != ord("]"):
            value, index = parse_expression(source, index, env)
            values.append(value)
            index = skip_space(source, index)
            if source[index] == ord(","):
                index = skip_space(source, index + 1)
            else:
                break
        if source[index] != ord("]"):
            raise ValueError(f"unterminated source array at byte {index}")
        return values, index + 1

    match = re.match(rb"[$A-Za-z_][$\w]*", source[index:])
    if match is None:
        raise ValueError(f"unsupported JavaScript token at byte {index}: {source[index:index+50]!r}")
    name = match.group().decode("ascii")
    return env[name], index + len(match.group())


def parse_expression(source: bytes, index: int, env: dict[str, object]):
    value, index = parse_atom(source, index, env)
    index = skip_space(source, index)
    while source[index : index + 1] == b"+":
        right, index = parse_atom(source, index + 1, env)
        value = value + right
        index = skip_space(source, index)
    return value, index


def assignment(source: bytes, name: str, start: int, end: int, env: dict[str, object]):
    pattern = rb"(?<![$\w])" + re.escape(name.encode("ascii")) + rb"="
    match = re.search(pattern, source[start:end])
    if match is None:
        raise ValueError(f"source assignment not found: {name}")
    value, _ = parse_expression(source, start + match.end(), env)
    env[name] = value
    return value


def utf16_units(value: str) -> list[int]:
    raw = value.encode("utf-16-le", "surrogatepass")
    return [raw[index] | (raw[index + 1] << 8) for index in range(0, len(raw), 2)]


def span(source: bytes, start: int, end: int) -> dict[str, object]:
    value = source[start:end]
    return {
        "offset": start,
        "bytes": len(value),
        "sha256": hashlib.sha256(value).hexdigest(),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=NATIVE_DEFAULT)
    args = parser.parse_args()

    source = args.binary.read_bytes()
    binary_sha = hashlib.sha256(source).hexdigest()
    if binary_sha != NATIVE_SHA256:
        raise SystemExit(f"frozen binary SHA mismatch: {binary_sha}")

    env: dict[str, object] = {}
    assignment(source, "hen", 186813898, 186814300, env)
    declaration_start = source.find(b"var $kr=", 186861500)
    declaration_end = source.find(b"function HAs(", declaration_start)
    if declaration_start < 0 or declaration_end < 0:
        raise SystemExit("the frozen `ce`/`le` declarations were not found")
    for name in [
        "uOt",
        "Sbo",
        "ict",
        "HZt",
        "bbo",
        "MZt",
        "DZt",
        "Ukr",
        "wbo",
        "LZt",
        "Ebo",
        "vbo",
        "pOt",
        "fOt",
        "NZt",
        "$kr",
        "_bo",
        "ce",
        "le",
    ]:
        assignment(source, name, declaration_start, declaration_end, env)

    prefixes = [value for value in env["ce"] if value]
    exact = env["le"]
    if prefixes != [env["uOt"], env["ict"], env["hen"], env["NZt"]]:
        raise SystemExit("Native `ce` evaluation changed")
    if exact != [env[name] for name in ["HZt", "bbo", "MZt", "DZt", "Ukr", "wbo", "LZt", "Ebo", "vbo", "pOt", "fOt"]]:
        raise SystemExit("Native `le` evaluation changed")

    i4e_offset = 186866210
    i4e_bytes = 228
    if source[i4e_offset : i4e_offset + 15] != b"function I4e(e)":
        raise SystemExit("the pinned `I4e` offset no longer matches")
    i4e = span(source, i4e_offset, i4e_offset + i4e_bytes)
    if i4e["sha256"] != "45aa5349661a7e8ea971e78ab67a1512977a96c1af2615d24e0626e930703a21":
        raise SystemExit("the pinned `I4e` source fingerprint changed")

    fixture = {
        "schema": "lingxi.claude-native-meta-allowlist.v1",
        "native": {"version": "2.1.295", "binarySha256": binary_sha},
        "source": {
            "declarationsAndI4e": span(source, declaration_start, declaration_end),
            "i4e": i4e,
            "selection": "`ce` entries with nonempty UTF-16 string length; every `le` entry; hook event names and `$kr` suffix.",
        },
        "classification": {
            "prefixes": [{"text": value, "utf16CodeUnits": utf16_units(value)} for value in prefixes],
            "exact": [{"text": value, "utf16CodeUnits": utf16_units(value)} for value in exact],
            "hookEvents": list(env["_bo"]),
            "hookFeedbackSuffix": {
                "text": env["$kr"],
                "utf16CodeUnits": utf16_units(env["$kr"]),
            },
            "requiresIsMeta": True,
        },
    }

    aggregate = Path(__file__).resolve().parents[3]
    outputs = [
        Path(__file__).with_name("native-meta-allowlist-295.json"),
        aggregate / "harness-runtime/crates/core/src/fixtures/native-meta-allowlist-295.json",
    ]
    rendered = json.dumps(fixture, ensure_ascii=False, indent=2) + "\n"
    for output in outputs:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(rendered, encoding="utf-8")
        print(f"wrote {output}")
    print(f"fixture_sha256={hashlib.sha256(rendered.encode()).hexdigest()}")


if __name__ == "__main__":
    main()
