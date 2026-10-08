#!/usr/bin/env python3
"""Capture Native 2.1.293 tool catalogs for three `isDeferred` inputs."""
from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import threading


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output"
NATIVE_PATH = os.environ.get("CLAUDE_NATIVE_BINARY")
if not NATIVE_PATH:
    raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
BINARY = Path(NATIVE_PATH).expanduser().resolve()
EXPECTED_SHA256 = "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e"
DUMMY_KEY = "native-tool-register-fixture-dummy-key"
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
CASES = tuple(item["id"] for item in FIXTURE["cases"])
SCHEMA = FIXTURE["tool"]["inputSchema"]


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def raw_top_level_value(raw: bytes, wanted: str) -> bytes:
    """Return the exact UTF-8 byte slice for one top-level JSON member."""
    text = raw.decode("utf-8")
    decoder = json.JSONDecoder()

    def skip_space(position: int) -> int:
        while position < len(text) and text[position] in " \t\r\n":
            position += 1
        return position

    index = skip_space(0)
    if text[index] != "{":
        raise ValueError("request JSON root is not an object")
    index += 1
    while True:
        index = skip_space(index)
        if index >= len(text) or text[index] == "}":
            break
        key, key_end = decoder.raw_decode(text, index)
        if not isinstance(key, str):
            raise ValueError("request JSON object key is not a string")
        index = skip_space(key_end)
        if text[index] != ":":
            raise ValueError("request JSON key has no colon")
        value_start = skip_space(index + 1)
        _value, value_end = decoder.raw_decode(text, value_start)
        if key == wanted:
            return text[value_start:value_end].encode("utf-8")
        index = skip_space(value_end)
        if text[index] == ",":
            index += 1
            continue
        if text[index] != "}":
            raise ValueError("request JSON object has invalid separator")
        break
    raise KeyError(f"missing top-level {wanted!r} member")


def native_module(case: str) -> str:
    case_data = next(item for item in FIXTURE["cases"] if item["id"] == case)
    deferred_field = (
        "" if "isDeferred" not in case_data
        else f", isDeferred: {json.dumps(case_data['isDeferred'])}"
    )
    tool_spec = {
        "name": FIXTURE["tool"]["name"],
        "description": FIXTURE["tool"]["description"],
        "inputSchema": SCHEMA,
    }
    return f'''export function register(on) {{
  on("session.start", async ($, event, next) => {{
    await $.tool.register({json.dumps(tool_spec, ensure_ascii=True, separators=(",", ":"))[:-1]}{deferred_field}}});
    return await next(event);
  }});
}}
'''


def prepare_plugin(case: str) -> Path:
    plugin = OUTPUT / "plugins" / "current"
    (plugin / ".claude-plugin").mkdir(parents=True, exist_ok=True)
    (plugin / "hooks").mkdir(parents=True, exist_ok=True)
    manifest = {"name": FIXTURE["plugin_name"]}
    (plugin / ".claude-plugin" / "plugin.json").write_text(
        json.dumps(manifest, separators=(",", ":")) + "\n", encoding="utf-8"
    )
    (plugin / "hooks" / "hooks.json").write_text(
        '{"hooks":{},"modules":["./register.js"]}\n', encoding="utf-8"
    )
    (plugin / "hooks" / "register.js").write_text(native_module(case), encoding="utf-8")
    return plugin


def response_stream() -> bytes:
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": "msg_mod_tool_register_fixture", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [], "stop_reason": None,
            "stop_sequence": None,
            "usage": {"input_tokens": 5, "output_tokens": 0,
                      "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "text", "text": ""}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "text_delta", "text": "fixture complete"}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                            "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                            "usage": {"output_tokens": 2}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("utf-8")


def serve_case(case: str, plugin: Path) -> dict:
    request_records = []

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def reply(self, status: int, body: bytes = b"", content_type: str = "application/json") -> None:
            self.send_response(status)
            self.send_header("Content-Length", str(len(body)))
            if body:
                self.send_header("Content-Type", content_type)
            self.end_headers()
            if body:
                self.wfile.write(body)

        def do_HEAD(self) -> None:
            accepted = self.client_address[0] == "127.0.0.1" and self.path == "/api/hello"
            request_records.append({"method": "HEAD", "path": self.path, "accepted": accepted})
            self.reply(200 if accepted else 404)

        def do_POST(self) -> None:
            length = int(self.headers.get("content-length", "0"))
            if length > 2 * 1024 * 1024:
                request_records.append({"method": "POST", "path": self.path, "accepted": False,
                                       "reason": "body-too-large"})
                self.reply(413)
                return
            raw = self.rfile.read(length)
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            record = {
                "method": "POST", "path": self.path, "accepted": accepted,
                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "request_bytes": len(raw), "request_sha256": sha256(raw),
                "stream": None,
            }
            if not accepted:
                request_records.append(record)
                self.reply(404 if self.path != "/v1/messages?beta=true" else 401)
                return
            request_index = sum(item.get("method") == "POST" for item in request_records)
            request_path = OUTPUT / "requests" / f"{case}.request-{request_index:02}.json"
            request_path.parent.mkdir(parents=True, exist_ok=True)
            request_path.write_bytes(raw)
            try:
                decoded = json.loads(raw)
            except (UnicodeDecodeError, json.JSONDecodeError):
                record["json_parseable"] = False
                request_records.append(record)
                self.reply(400)
                return
            record["stream"] = decoded.get("stream") is True
            record["raw_file"] = str(request_path.relative_to(ROOT))
            request_records.append(record)
            self.reply(200, response_stream(), "text/event-stream")

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    case_run = OUTPUT / "run" / "current"
    for child in ("cwd", "tmp", "home", "config"):
        child_path = case_run / child
        if child_path.exists():
            shutil.rmtree(child_path)
        child_path.mkdir(parents=True)
    config_path = case_run / "config"
    config_path.mkdir(parents=True, exist_ok=True)
    env = {
        "PATH": "/usr/bin:/bin",
        "TMPDIR": str(case_run / "tmp"),
        "LANG": "C.UTF-8",
        "TZ": "UTC",
        "TERM": "dumb",
        "HOME": str(case_run / "home"),
        "CLAUDE_CONFIG_DIR": str(config_path),
        "ANTHROPIC_API_KEY": DUMMY_KEY,
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DO_NOT_TRACK": "1",
    }
    argv = [
        str(BINARY), "--plugin-dir", str(plugin),
        "--setting-sources", "project", "--strict-mcp-config",
        "--print", "--no-session-persistence", "--tools", "default",
        "--max-budget-usd", "1", "--model", "claude-sonnet-4-5",
        "--debug", "hooks",
        "--session-id", FIXTURE["session_id"],
        FIXTURE["prompt"],
    ]
    try:
        completed = subprocess.run(argv, cwd=case_run / "cwd", env=env,
                                   capture_output=True, timeout=60, check=False)
        timed_out = False
        stdout, stderr, exit_code = completed.stdout, completed.stderr, completed.returncode
    except subprocess.TimeoutExpired as error:
        timed_out = True
        stdout, stderr, exit_code = error.stdout or b"", error.stderr or b"", None
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    (OUTPUT / f"{case}.stdout").write_bytes(stdout)
    (OUTPUT / f"{case}.stderr").write_bytes(stderr)
    requests = []
    for record in request_records:
        if record.get("method") != "POST" or "raw_file" not in record:
            continue
        raw = (ROOT / record["raw_file"]).read_bytes()
        decoded = json.loads(raw)
        tools = decoded.get("tools", [])
        matching = [tool for tool in tools if isinstance(tool, dict) and tool.get("name", "").endswith("__probe")]
        raw_tools = raw_top_level_value(raw, "tools")
        raw_tools_path = OUTPUT / "requests" / f"{case}.tools.raw.json"
        raw_tools_path.write_bytes(raw_tools)
        requests.append({
            "raw_file": record["raw_file"],
            "raw_bytes": len(raw),
            "raw_sha256": sha256(raw),
            "tools_raw_file": str(raw_tools_path.relative_to(ROOT)),
            "tools_raw_bytes": len(raw_tools),
            "tools_raw_sha256": sha256(raw_tools),
            "stream": decoded.get("stream"),
            "tool_count": len(tools) if isinstance(tools, list) else None,
            "tool_names": [tool.get("name") for tool in tools if isinstance(tool, dict)],
            "registered_tool_entries": matching,
        })
    return {
        "case": case,
        "input_spec": {"name": "probe", "isDeferred": None if case == "omitted" else case == "true",
                        "inputSchema": SCHEMA},
        "argv": argv,
        "cwd": str(case_run / "cwd"),
        "config_dir": str(config_path),
        "loopback_url": env["ANTHROPIC_BASE_URL"],
        "requests": request_records,
        "captured_requests": requests,
        "native_exit_code": exit_code,
        "timed_out": timed_out,
        "stdout_bytes": len(stdout),
        "stdout_sha256": sha256(stdout),
        "stderr_bytes": len(stderr),
        "stderr_sha256": sha256(stderr),
        "stdout_file": f"{case}.stdout",
        "stderr_file": f"{case}.stderr",
    }


binary_sha = sha256(BINARY.read_bytes())
if binary_sha != EXPECTED_SHA256:
    raise SystemExit(f"Native SHA mismatch: {binary_sha}")

if OUTPUT.exists():
    shutil.rmtree(OUTPUT)
OUTPUT.mkdir(parents=True)
run_root = OUTPUT / "run" / "current"
if run_root.exists():
    shutil.rmtree(run_root)
for case in CASES:
    for path in (OUTPUT / "requests").glob(f"{case}.*.json"):
        path.unlink()

case_results = []
for case in CASES:
    case_results.append(serve_case(case, prepare_plugin(case)))

captured_requests = [
    request for case in case_results for request in case["captured_requests"]
]
request_bodies = [(ROOT / request["raw_file"]).read_bytes() for request in captured_requests]
tool_arrays = [(ROOT / request["tools_raw_file"]).read_bytes() for request in captured_requests]
tool_entries = [request["registered_tool_entries"] for request in captured_requests]
by_case = {
    case["case"]: {
        "request": (ROOT / case["captured_requests"][0]["raw_file"]).read_bytes(),
        "tools_raw": (ROOT / case["captured_requests"][0]["tools_raw_file"]).read_bytes(),
        "decoded": json.loads((ROOT / case["captured_requests"][0]["raw_file"]).read_bytes()),
    }
    for case in case_results
}


def pairwise(left: str, right: str) -> dict:
    left_value, right_value = by_case[left], by_case[right]
    left_body, right_body = left_value["decoded"], right_value["decoded"]
    changed_fields = [key for key in left_body if left_body.get(key) != right_body.get(key)]
    changed_fields.extend(key for key in right_body if key not in left_body)
    left_names = [tool.get("name") for tool in left_body.get("tools", []) if isinstance(tool, dict)]
    right_names = [tool.get("name") for tool in right_body.get("tools", []) if isinstance(tool, dict)]
    return {
        "request_bytes_equal": left_value["request"] == right_value["request"],
        "tools_array_raw_bytes_equal": left_value["tools_raw"] == right_value["tools_raw"],
        "parsed_top_level_fields_with_different_values": changed_fields,
        "removed_tool_names_in_native_order": [name for name in left_names if name not in right_names],
        "added_tool_names_in_native_order": [name for name in right_names if name not in left_names],
        "registered_probe_entry_equal": next(
            (tool for tool in left_body.get("tools", [])
             if isinstance(tool, dict) and tool.get("name", "").endswith("__probe")), None
        ) == next(
            (tool for tool in right_body.get("tools", [])
             if isinstance(tool, dict) and tool.get("name", "").endswith("__probe")), None
        ),
    }

receipt = {
    "native_version": "2.1.293",
    "native_sha256": binary_sha,
    "cases": case_results,
    "all_requests_loopback_dummy_key_only": all(
        request.get("accepted") and request.get("dummy_key_matched")
        for case in case_results for request in case["requests"] if request.get("method") == "POST"
    ),
    "no_upstream_relay": True,
    "captured_original_request_bytes": all(
        case["captured_requests"] for case in case_results
    ),
    "registered_tool_present": {
        case["case"]: bool(case["captured_requests"] and case["captured_requests"][0]["registered_tool_entries"])
        for case in case_results
    },
    "all_cases_have_one_captured_request": all(len(case["captured_requests"]) == 1 for case in case_results),
    "tools_array_raw_bytes_identical_across_cases": len(tool_arrays) == len(CASES) and all(
        value == tool_arrays[0] for value in tool_arrays[1:]
    ),
    "registered_tool_schema_objects_equal_across_cases": len(tool_entries) == len(CASES) and all(
        value == tool_entries[0] for value in tool_entries[1:]
    ),
    "full_request_raw_bytes_identical_across_cases": len(request_bodies) == len(CASES) and all(
        value == request_bodies[0] for value in request_bodies[1:]
    ),
    "pairwise": {
        "omitted_vs_false": pairwise("omitted", "false"),
        "omitted_vs_true": pairwise("omitted", "true"),
        "false_vs_true": pairwise("false", "true"),
    },
    "scope": "Native 2.1.293 public plugin module session.start -> $.tool.register, first Messages request tool catalog; Harness comparison is separate and pending",
}
(OUTPUT / "native-receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n")
print(json.dumps(receipt, ensure_ascii=False, indent=2))
if not (receipt["captured_original_request_bytes"]
        and receipt["all_requests_loopback_dummy_key_only"]
        and receipt["all_cases_have_one_captured_request"]):
    raise SystemExit(1)
