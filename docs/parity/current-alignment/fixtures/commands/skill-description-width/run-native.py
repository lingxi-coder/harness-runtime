#!/usr/bin/env python3
"""Observe Native Skill-description truncation using one fixed lone-surrogate input."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import threading


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output"
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
DUMMY_KEY = "native-skill-width-fixture-dummy-key"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def field_span(text: str, wanted: str) -> tuple[object, int, int]:
    decoder = json.JSONDecoder()
    position = 0
    while position < len(text) and text[position] in " \t\r\n":
        position += 1
    if text[position] != "{":
        raise ValueError("provider request JSON root is not an object")
    position += 1
    while True:
        while position < len(text) and text[position] in " \t\r\n":
            position += 1
        if text[position] == "}":
            raise KeyError(wanted)
        key, key_end = decoder.raw_decode(text, position)
        position = key_end
        while text[position] in " \t\r\n":
            position += 1
        if text[position] != ":":
            raise ValueError("request object member is missing colon")
        position += 1
        while text[position] in " \t\r\n":
            position += 1
        value_start = position
        value, value_end = decoder.raw_decode(text, value_start)
        if key == wanted:
            return value, value_start, value_end
        position = value_end
        while text[position] in " \t\r\n":
            position += 1
        if text[position] == ",":
            position += 1
        elif text[position] != "}":
            raise ValueError("request object member has invalid separator")


def string_leaves(value: object, path: str = "$"):
    if isinstance(value, str):
        yield path, value
    elif isinstance(value, list):
        for index, item in enumerate(value):
            yield from string_leaves(item, f"{path}[{index}]")
    elif isinstance(value, dict):
        for key, item in value.items():
            yield from string_leaves(item, f"{path}.{key}")


def response_sse() -> bytes:
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": "msg_native_skill_width_probe", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [], "stop_reason": None,
            "stop_sequence": None, "usage": {"input_tokens": 5, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": "fixed skill-width probe response"}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "fixed skill-width probe response"}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": None},
            "usage": {"output_tokens": 2}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("utf-8")


binary_text = os.environ.get("CLAUDE_NATIVE_BINARY")
if not binary_text:
    raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
binary = Path(binary_text).expanduser().resolve()
binary_sha = sha256(binary.read_bytes())
if binary_sha != FIXTURE["native_sha256"]:
    raise SystemExit(f"Native SHA mismatch: {binary_sha}")
version = subprocess.run([str(binary), "--version"], capture_output=True, check=False)
version_text = (version.stdout + version.stderr).decode("utf-8", errors="replace").strip()
if FIXTURE["native_version"] not in version_text:
    raise SystemExit(f"Native version mismatch: {version_text!r}")

if OUTPUT.exists():
    shutil.rmtree(OUTPUT)
for name in ("home", "config", "tmp", "cwd"):
    (OUTPUT / name).mkdir(parents=True, exist_ok=True)
skill_dir = OUTPUT / "cwd" / ".claude" / "skills" / FIXTURE["skill_name"]
skill_dir.mkdir(parents=True)
description = "\\uD800" + "a" * FIXTURE["ascii_tail_length"]
skill_source = (
    "---\n"
    f"name: {FIXTURE['skill_name']}\n"
    f'description: "{description}"\n'
    "---\n"
    "Return the fixed skill-width response.\n"
)
(skill_dir / "SKILL.md").write_bytes(skill_source.encode("ascii"))
request_records: list[dict] = []
response_body = response_sse()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def do_POST(self) -> None:
        raw = self.rfile.read(int(self.headers.get("content-length", "0")))
        accepted = (
            self.client_address[0] == "127.0.0.1"
            and self.path == "/v1/messages?beta=true"
            and self.headers.get("x-api-key") == DUMMY_KEY
        )
        request_records.append({"path": self.path, "accepted": accepted,
                                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                                "bytes": len(raw), "sha256": sha256(raw)})
        if accepted:
            (OUTPUT / "request.full.raw.json").write_bytes(raw)
            body, status, content_type = response_body, 200, "text/event-stream"
        else:
            body, status, content_type = b"{}", 404, "application/json"
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Type", content_type)
        self.end_headers()
        self.wfile.write(body)


server = HTTPServer(("127.0.0.1", 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
env = {
    "PATH": "/usr/bin:/bin",
    "TMPDIR": str(OUTPUT / "tmp"),
    "LANG": "C.UTF-8",
    "TZ": "UTC",
    "TERM": "dumb",
    "HOME": str(OUTPUT / "home"),
    "CLAUDE_CONFIG_DIR": str(OUTPUT / "config"),
    "ANTHROPIC_API_KEY": DUMMY_KEY,
    "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
    "ANTHROPIC_MAX_RETRIES": "0",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    "DISABLE_TELEMETRY": "1",
    "DO_NOT_TRACK": "1",
}
argv = [
    str(binary), "--setting-sources", "project", "--strict-mcp-config", "--print",
    "--no-session-persistence", "--tools", "default", "--model", FIXTURE["model"],
    "--session-id", FIXTURE["session_id"], FIXTURE["prompt"],
]
try:
    completed = subprocess.run(argv, cwd=OUTPUT / "cwd", env=env, capture_output=True,
                               timeout=60, check=False)
    stdout, stderr, exit_code, timed_out = completed.stdout, completed.stderr, completed.returncode, False
except subprocess.TimeoutExpired as error:
    stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=2)
(OUTPUT / "native.stdout.raw").write_bytes(stdout)
(OUTPUT / "native.stderr.raw").write_bytes(stderr)

request_raw = (OUTPUT / "request.full.raw.json").read_bytes() if request_records else b""
request_text = request_raw.decode("utf-8") if request_raw else ""
request_obj = json.loads(request_raw) if request_raw else {}
if request_records:
    (OUTPUT / "response.raw.sse").write_bytes(response_body)
system_value, system_start, system_end = field_span(request_text, "system") if request_raw else (None, 0, 0)
if request_raw:
    (OUTPUT / "system-field.raw.json").write_bytes(request_text[system_start:system_end].encode("utf-8"))
if isinstance(system_value, str):
    system_texts = [system_value]
elif isinstance(system_value, list):
    system_texts = [item.get("text", "") for item in system_value if isinstance(item, dict)]
else:
    system_texts = []
system_joined = "\n".join(text for text in system_texts if isinstance(text, str))
name_index = system_joined.find(FIXTURE["skill_name"])
matches = []
request_strings = list(string_leaves(request_obj))
source_description_present = any(description in value for _path, value in request_strings)
description_tail_present = any(("a" * FIXTURE["ascii_tail_length"]) in value
                               for _path, value in request_strings)
for path, value in request_strings:
    index = value.find(FIXTURE["skill_name"])
    if index < 0:
        continue
    line_start = value.rfind("\n", 0, index) + 1
    line_end = value.find("\n", index)
    if line_end < 0:
        line_end = min(len(value), index + 512)
    excerpt = value[line_start:line_end]
    matches.append({
        "json_path": path,
        "decoded_excerpt": excerpt,
        "excerpt_code_points": len(excerpt),
        "ascii_a_count": excerpt.count("a"),
        "max_ascii_a_run": max((len(run) for run in re.findall(r"a+", excerpt)), default=0),
        "replacement_character_count": excerpt.count("\ufffd"),
        "ellipsis_present": "…" in excerpt,
    })
listing = {
    "skill_name_found_in_system": name_index >= 0,
    "system_occurrences": system_joined.count(FIXTURE["skill_name"]),
    "request_occurrences": len(matches),
    "string_matches": matches,
    "source_description_literal_present_in_request": source_description_present,
    "250_ascii_tail_present_in_request": description_tail_present,
    "system_blocks": len(system_texts),
}
(OUTPUT / "skill-bearing-request.raw-excerpt.bin").write_bytes(
    request_raw[max(0, request_raw.find(FIXTURE["skill_name"].encode("ascii")) - 128):
                request_raw.find(FIXTURE["skill_name"].encode("ascii")) + 1024]
    if request_raw.find(FIXTURE["skill_name"].encode("ascii")) >= 0 else b""
)
(OUTPUT / "skill-listing-observation.json").write_text(
    json.dumps(listing, ensure_ascii=True, indent=2) + "\n", encoding="utf-8"
)
receipt = {
    "native_version": version_text,
    "native_sha256": binary_sha,
    "runner_sha256": sha256(Path(__file__).read_bytes()),
    "skill_source_sha256": sha256((skill_dir / "SKILL.md").read_bytes()),
    "skill_source_bytes": (skill_dir / "SKILL.md").stat().st_size,
    "fixed_input": {"skill_name": FIXTURE["skill_name"],
                    "description_yaml_escape": "\\uD800",
                    "description_ascii_a_count": FIXTURE["ascii_tail_length"],
                    "cwd_isolated": True, "home_config_isolated": True,
                    "model": FIXTURE["model"], "prompt": FIXTURE["prompt"]},
    "argv": ["$CLAUDE_NATIVE_BINARY", "--setting-sources", "project", "--strict-mcp-config",
             "--print", "--no-session-persistence", "--tools", "default", "--model", FIXTURE["model"],
             "--session-id", FIXTURE["session_id"], FIXTURE["prompt"]],
    "loopback_only_dummy_key": True,
    "timed_out": timed_out,
    "exit_code": exit_code,
    "requests": request_records,
    "response": {"path": "response.raw.sse", "bytes": len(response_body),
                  "sha256": sha256(response_body)} if request_records else None,
    "system_field_raw": {"path": "system-field.raw.json",
                          "bytes": system_end - system_start,
                          "sha256": sha256((OUTPUT / "system-field.raw.json").read_bytes())}
                         if request_raw else None,
    "listing_observation": listing,
    "source_description_literal_present_in_request": source_description_present,
    "ascii_description_tail_present_in_request": description_tail_present,
    "skill_bearing_request_excerpt": {
        "path": "skill-bearing-request.raw-excerpt.bin",
        "bytes": (OUTPUT / "skill-bearing-request.raw-excerpt.bin").stat().st_size,
        "sha256": sha256((OUTPUT / "skill-bearing-request.raw-excerpt.bin").read_bytes()),
    },
    "full_request_remains_in_ignored_run_output": True,
    "stdout": {"path": "native.stdout.raw", "bytes": len(stdout), "sha256": sha256(stdout)},
    "stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": sha256(stderr)},
}
(OUTPUT / "native-receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n")
print(json.dumps(receipt, ensure_ascii=True, indent=2))
if timed_out or exit_code != 0 or len(request_records) != 1 or not request_records[0].get("accepted"):
    raise SystemExit(1)
