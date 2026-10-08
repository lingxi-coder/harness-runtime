#!/usr/bin/env python3
"""Capture Native slash-command fallback, Skill-name and persistence cases."""
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
DUMMY_KEY = "native-commands-fixture-dummy-key"
CASES = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))["cases"]


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def response_stream() -> bytes:
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": "msg_native_commands_fixture", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [], "stop_reason": None,
            "stop_sequence": None,
            "usage": {"input_tokens": 5, "output_tokens": 0,
                      "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "text", "text": ""}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "text_delta", "text": "fixed native stub result"}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                            "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                            "usage": {"output_tokens": 4}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("utf-8")


def create_skills(cwd: Path) -> None:
    skill_specs = (
        ("skill-🧪-probe", "Unicode-scalar Skill fixture"),
        ("skill.alpha+beta", "Punctuation Skill fixture"),
    )
    for name, description in skill_specs:
        directory = cwd / ".claude" / "skills" / name
        directory.mkdir(parents=True, exist_ok=True)
        content = (
            f"---\nname: {name}\ndescription: {description}\n---\n"
            f"Return the fixed Skill body for {name}.\n"
        )
        (directory / "SKILL.md").write_bytes(content.encode("utf-8"))


def encode_prompt(case: dict) -> tuple[list[str], bytes]:
    if case["kind"] == "argv":
        return [case["prompt"]], b""
    raw_units = bytes.fromhex(case["prompt_utf16le_hex"])
    prompt = raw_units.decode("utf-16-le", errors="surrogatepass")
    line = json.dumps({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": prompt}]},
    }, ensure_ascii=True, separators=(",", ":")).encode("ascii") + b"\n"
    return [], line


def run_case(case: dict, server: HTTPServer, request_records: list[dict], cwd: Path, config: Path, temp: Path) -> dict:
    case_id = case["id"]
    prompt_argv, stdin_bytes = encode_prompt(case)
    argv = [
        str(BINARY), "--setting-sources", "project", "--strict-mcp-config",
        "--print", "--tools", "", "--max-budget-usd", "1",
        "--model", "claude-sonnet-4-5", "--session-id", case["session_id"],
    ]
    if case["kind"] == "stream-json":
        argv.extend(["--input-format", "stream-json", "--output-format", "stream-json", "--verbose"])
    argv.extend(prompt_argv)
    env = {
        "PATH": "/usr/bin:/bin",
        "TMPDIR": str(temp),
        "LANG": "C.UTF-8",
        "TZ": "UTC",
        "TERM": "dumb",
        "HOME": str(config.parent / "home"),
        "CLAUDE_CONFIG_DIR": str(config),
        "ANTHROPIC_API_KEY": DUMMY_KEY,
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DO_NOT_TRACK": "1",
    }
    try:
        completed = subprocess.run(
            argv, cwd=cwd, env=env, input=stdin_bytes if case["kind"] == "stream-json" else None,
            capture_output=True, timeout=60, check=False,
        )
        stdout, stderr, exit_code = completed.stdout, completed.stderr, completed.returncode
        timed_out = False
    except subprocess.TimeoutExpired as error:
        stdout, stderr, exit_code = error.stdout or b"", error.stderr or b"", None
        timed_out = True

    (OUTPUT / f"{case_id}.stdout").write_bytes(stdout)
    (OUTPUT / f"{case_id}.stderr").write_bytes(stderr)

    session_paths = sorted(config.rglob(f"{case['session_id']}.jsonl"))
    session_record = None
    if session_paths:
        source = session_paths[0]
        raw_session = source.read_bytes()
        archived_session = OUTPUT / "sessions" / f"{case_id}.jsonl"
        archived_session.write_bytes(raw_session)
        session_record = {
            "native_path_relative_to_config": str(source.relative_to(config)),
            "capture_file": str(archived_session.relative_to(ROOT)),
            "bytes": len(raw_session),
            "sha256": sha256(raw_session),
        }

    case_requests = [record for record in request_records if record.get("case") == case_id]
    return {
        "id": case_id,
        "kind": case["kind"],
        "session_id": case["session_id"],
        "prompt_code_units_hex": case.get("prompt_utf16le_hex"),
        "input_file": f"inputs/{case_id}.stdin.jsonl" if stdin_bytes else None,
        "argv": argv,
        "native_exit_code": exit_code,
        "timed_out": timed_out,
        "stdout_file": f"{case_id}.stdout",
        "stdout_bytes": len(stdout),
        "stdout_sha256": sha256(stdout),
        "stderr_file": f"{case_id}.stderr",
        "stderr_bytes": len(stderr),
        "stderr_sha256": sha256(stderr),
        "loopback_requests": case_requests,
        "session_persistence": session_record,
    }


def main() -> int:
    actual_binary_sha = sha256(BINARY.read_bytes())
    if actual_binary_sha != EXPECTED_SHA256:
        raise SystemExit(f"Native SHA mismatch: {actual_binary_sha}")
    if OUTPUT.exists():
        shutil.rmtree(OUTPUT)
    OUTPUT.mkdir(parents=True)
    run_root = OUTPUT / "run"
    (OUTPUT / "sessions").mkdir(parents=True)
    (OUTPUT / "requests").mkdir(parents=True)
    if run_root.exists():
        shutil.rmtree(run_root)
    config = run_root / "config"
    cwd = run_root / "cwd"
    temp = run_root / "tmp"
    for path in (config, cwd, temp, run_root / "home"):
        path.mkdir(parents=True)
    create_skills(cwd)
    for path in (OUTPUT / "requests").glob("*.request-*.json"):
        path.unlink()
    for path in (OUTPUT / "sessions").glob("*.jsonl"):
        path.unlink()

    request_records: list[dict] = []

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
            request_records.append({"case": active_case[0], "method": "HEAD", "path": self.path,
                                   "accepted": accepted})
            self.reply(200 if accepted else 404)

        def do_POST(self) -> None:
            length = int(self.headers.get("content-length", "0"))
            if length > 2 * 1024 * 1024:
                request_records.append({"case": active_case[0], "method": "POST", "path": self.path,
                                       "accepted": False, "reason": "body-too-large"})
                self.reply(413)
                return
            raw = self.rfile.read(length)
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            record = {
                "case": active_case[0], "method": "POST", "path": self.path,
                "accepted": accepted,
                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "bytes": len(raw), "sha256": sha256(raw),
            }
            if not accepted:
                request_records.append(record)
                self.reply(404 if self.path != "/v1/messages?beta=true" else 401)
                return
            index = sum(item.get("case") == active_case[0] and item.get("method") == "POST"
                        for item in request_records)
            request_file = OUTPUT / "requests" / f"{active_case[0]}.request-{index:02}.json"
            request_file.write_bytes(raw)
            record["raw_file"] = str(request_file.relative_to(ROOT))
            try:
                request = json.loads(raw)
                record["stream"] = request.get("stream") is True
                record["tools"] = [item.get("name") for item in request.get("tools", [])
                                   if isinstance(item, dict)]
            except (UnicodeDecodeError, json.JSONDecodeError):
                record["json_parseable"] = False
                self.reply(400)
                request_records.append(record)
                return
            request_records.append(record)
            self.reply(200, response_stream(), "text/event-stream")

    active_case = [""]
    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    case_results = []
    try:
        for case in CASES:
            active_case[0] = case["id"]
            case_results.append(run_case(case, server, request_records, cwd, config, temp))
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    receipt = {
        "native_version": "2.1.293",
        "native_sha256": actual_binary_sha,
        "base_url_scope": "127.0.0.1 only, dummy key, no relay; telemetry disabled in fixture env",
        "fixed_inputs": {"cwd": str(cwd), "config_dir": str(config), "model": "claude-sonnet-4-5",
                         "locale": "C.UTF-8", "timezone": "UTC", "skills": [
                             "skill-🧪-probe", "skill.alpha+beta"]},
        "cases": case_results,
        "all_requests_accepted_locally": all(
            record.get("accepted") and record.get("dummy_key_matched")
            for record in request_records if record.get("method") == "POST"
        ),
        "no_upstream_relay": True,
        "scope": "Native command parse/fallback, Unicode/punctuation/unpaired-surrogate Skill-name and session persistence fixtures; no Harness result implied",
    }
    (OUTPUT / "native-receipt.json").write_text(
        json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps(receipt, ensure_ascii=False, indent=2))
    return 0 if all(not case["timed_out"] for case in case_results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
