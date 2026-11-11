#!/usr/bin/env python3
"""Run real isolated Claude Code 2.1.321 and LingXi CLI session probes.

The provider is a loopback HTTP/SSE server with scripted responses. All
configuration and transcripts belong to the requested output directory.
No HOME override, user credentials, remote model, or real user session is used.
Import Loopback and isolated_env for interactive UI probes.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import queue
import re
import signal
import subprocess
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

NATIVE_SHA = "abc9feeeca8091d51b623291e044cf0528c2f6b74e0c1d6ef671d52c73f7cd3c"
FAKE_KEY = "sk-ant-api03-fake-local-session-321-only"
MODEL = "claude-sonnet-4-6"
SESSION = "11111111-1111-4111-8111-111111111111"
SECOND = "22222222-2222-4222-8222-222222222222"
MISSING = "77777777-7777-4777-8777-777777777777"


def dump(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, ensure_ascii=True) + "\n")


def text_content(value: object) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "\n".join(text_content(item) for item in value)
    if isinstance(value, dict):
        return str(value.get("text", "")) + text_content(value.get("content", []))
    return ""


class Loopback:
    """Owned scripted API. No route forwards to any external service."""

    def __init__(self, root: Path, work: Path | None = None):
        self.root, self.work = Path(root), Path(work) if work else Path(root)
        self.root.mkdir(parents=True, exist_ok=True)
        self.requests: list[dict] = []
        self.refused: list[str] = []
        self.message_count = 0
        self.lock = threading.Lock()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_CONNECT(self):
                owner.refused.append("CONNECT")
                self.send_error(403, "Only the local fixture is available")

            def do_GET(self):
                owner.refused.append(self.path)
                self.send_error(403, "Only the local fixture is available")

            def respond_json(self, status, body):
                raw = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def do_POST(self):
                route = self.path.split("?")[0]
                if route not in ("/v1/messages", "/messages", "/v1/messages/count_tokens", "/messages/count_tokens"):
                    owner.refused.append(self.path)
                    self.send_error(403, "Only the local fixture is available")
                    return
                raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                body = json.loads(raw)
                with owner.lock:
                    ordinal = len(owner.requests) + 1
                    owner.requests.append({"path": self.path, "body": body})
                    (owner.root / f"request-{ordinal:03}.json").write_bytes(raw)
                    dump(owner.root / "requests.json", owner.requests)
                if "count_tokens" in route:
                    self.respond_json(200, {"input_tokens": 2500})
                    return
                with owner.lock:
                    owner.message_count += 1
                    message_id = f"msg_live_{owner.message_count}"
                messages = body.get("messages", [])
                last = text_content(messages[-1].get("content", [])) if messages else ""
                joined = json.dumps(messages)
                tool_names = [tool.get("name") for tool in body.get("tools", [])]
                use_tool = "SESSION_TOOL_SEED" in last and "Read" in tool_names and "toolu_live_read" not in joined
                summary = "CRITICAL: Respond with TEXT ONLY." in joined
                if summary:
                    content, stop = [{"type": "text", "text": "<analysis>Local scripted summary.</analysis>\n\n<summary>SESSION_COMPACT_RETAINED_MARKER_321</summary>"}], "end_turn"
                elif use_tool:
                    content = [{"type": "tool_use", "id": "toolu_live_read", "name": "Read", "input": {"file_path": str(owner.work / "marker.txt")}}]
                    stop = "tool_use"
                else:
                    marker = re.findall(r"SESSION_[A-Z_0-9]+", last)
                    text = "MOCK_OK " + (marker[-1] if marker else "SESSION_TOOL_DONE" if "toolu_live_read" in joined else "SESSION_REPLY")
                    content, stop = [{"type": "text", "text": text}], "end_turn"
                message = {"id": message_id, "type": "message", "role": "assistant", "model": body.get("model", MODEL), "content": [], "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 2500, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
                if not body.get("stream"):
                    message.update(content=content, stop_reason=stop)
                    message["usage"]["output_tokens"] = 20
                    self.respond_json(200, message)
                    return
                events = [{"type": "message_start", "message": message}]
                for index, block in enumerate(content):
                    initial = {**block, "input": {}} if block["type"] == "tool_use" else {"type": "text", "text": ""}
                    delta = {"type": "input_json_delta", "partial_json": json.dumps(block["input"])} if block["type"] == "tool_use" else {"type": "text_delta", "text": block["text"]}
                    events.extend([{"type": "content_block_start", "index": index, "content_block": initial}, {"type": "content_block_delta", "index": index, "delta": delta}, {"type": "content_block_stop", "index": index}])
                events.extend([{"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None}, "usage": {"output_tokens": 20}}, {"type": "message_stop"}])
                raw = "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def base_url(self):
        return f"http://127.0.0.1:{self.server.server_port}"

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_args):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


def isolated_env(engine: str, config: Path, tmp: Path, base_url: str) -> dict[str, str]:
    """Only explicit dummy credentials and loopback routes enter the child."""
    env = {"PATH": os.defpath, "TMPDIR": str(tmp), "LANG": "en_US.UTF-8", "TERM": "xterm-256color", "CLAUDE_CONFIG_DIR" if engine == "native" else "LINGXI_CONFIG_DIR": str(config), "ANTHROPIC_API_KEY": FAKE_KEY, "ANTHROPIC_BASE_URL": base_url, "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "CLAUDE_CODE_DISABLE_FEATURE_FLAGS": "1", "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1", "DISABLE_AUTOUPDATER": "1", "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1", "DO_NOT_TRACK": "1", "HTTP_PROXY": base_url, "HTTPS_PROXY": base_url, "ALL_PROXY": base_url, "NO_PROXY": "127.0.0.1,localhost"}
    if engine == "lingxi":
        env["LINGXI_API_BASE_URL"] = base_url
    return env


def profile(root: Path, engine: str):
    for name in ("config", "tmp", "work", "captures"):
        (root / name).mkdir(parents=True, exist_ok=True)
    (root / "config" / (".claude.json" if engine == "native" else ".lingxi.json")).write_text(json.dumps({"hasCompletedOnboarding": True}))
    (root / "work" / "marker.txt").write_text("SESSION_TOOL_FILE_MARKER_321\n")


def snapshot_transcripts(root: Path, capture: Path):
    """Retain exact owned JSONL bytes at each process boundary."""
    records = []
    for source in sorted((root / "config" / "projects").rglob("*.jsonl")):
        raw = source.read_bytes()
        relative = source.relative_to(root / "config")
        target = capture / "transcripts" / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(raw)
        rows = [json.loads(line) for line in raw.splitlines() if line]
        records.append({"path": str(relative), "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest(), "ends_in_lf": raw.endswith(b"\n"), "record_types": [row.get("type") for row in rows]})
    dump(capture / "transcript-inventory.json", records)


def snapshot_requests(api: Loopback, begin: int, capture: Path):
    records = []
    for ordinal in range(begin + 1, len(api.requests) + 1):
        raw = (api.root / f"request-{ordinal:03}.json").read_bytes()
        name = f"request-{ordinal - begin:03}.json"
        (capture / name).write_bytes(raw)
        records.append({"path": name, "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()})
    dump(capture / "request-inventory.json", records)


def setup_lingxi_account(binary: Path, root: Path, api: Loopback, timeout=30, capture_name="account-setup"):
    """Use the supported account CLI, dummy stdin key, and owned hashed scope."""
    target = root / "captures" / capture_name
    target.mkdir()
    command = [str(binary), "--bare", "provider", "accounts", "add", "anthropic", "--name", "SESSION_QA_LOOPBACK_ONLY_321", "--execution-binding", "anthropic", "--api-key-stdin", "--json"]
    env = isolated_env("lingxi", root / "config", root / "tmp", api.base_url)
    dump(target / "invocation.json", {"argv": command, "cwd": str(root / "work"), "environment": env, "stdin": "fixed synthetic API key only", "scope": "owned nondefault LINGXI_CONFIG_DIR; Broker service names are path-hashed"})
    process = subprocess.run(command, cwd=root / "work", env=env, input=(FAKE_KEY + "\n").encode(), stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
    (target / "stdout.json").write_bytes(process.stdout)
    (target / "stderr.txt").write_bytes(process.stderr)
    account_id = None
    if process.returncode == 0:
        snapshot = json.loads(process.stdout)
        accounts = snapshot.get("accounts", snapshot.get("snapshot", {}).get("accounts", []))
        matching = [account for account in accounts if account.get("displayName") == "SESSION_QA_LOOPBACK_ONLY_321"]
        if len(matching) == 1:
            account_id = matching[0]["accountId"]
    result = {"exit_code": process.returncode, "passed": process.returncode == 0 and account_id is not None, "account_id": account_id, "stderr": process.stderr.decode(errors="replace")}
    dump(target / "result.json", result)
    return result


def remove_lingxi_account(binary: Path, root: Path, api: Loopback, account_id: str, timeout=30, capture_name="account-remove"):
    """Remove only the exact synthetic account created by this fixture."""
    target = root / "captures" / capture_name
    target.mkdir()
    command = [str(binary), "--bare", "provider", "accounts", "remove", "anthropic", account_id, "--json"]
    process = subprocess.run(command, cwd=root / "work", env=isolated_env("lingxi", root / "config", root / "tmp", api.base_url), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
    (target / "stdout.json").write_bytes(process.stdout)
    (target / "stderr.txt").write_bytes(process.stderr)
    absent = False
    if process.returncode == 0:
        snapshot = json.loads(process.stdout)
        accounts = snapshot.get("accounts", snapshot.get("snapshot", {}).get("accounts", []))
        absent = all(account.get("accountId") != account_id for account in accounts)
    result = {"exit_code": process.returncode, "passed": process.returncode == 0 and absent, "account_id": account_id, "removed_from_owned_registry": absent}
    dump(target / "result.json", result)
    return result


def run_process(binary: Path, engine: str, root: Path, api: Loopback, name: str, flags: list[str], prompt: str | None, tools="", timeout=30, stdin_data: bytes | None = None, output_format="stream-json", verbose=True):
    capture = root / "captures" / name
    capture.mkdir()
    argv = [str(binary), "--bare", "--print", "--output-format", output_format, *(["--verbose"] if verbose else []), "--model", MODEL, "--system-prompt", "You are a deterministic local session fixture.", "--setting-sources", "", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}', "--tools", tools, *flags]
    if prompt is not None:
        argv.append(prompt)
    if tools:
        argv.extend(["--allowedTools", tools])
    env = isolated_env(engine, root / "config", root / "tmp", api.base_url)
    dump(capture / "invocation.json", {"argv": argv, "cwd": str(root / "work"), "environment": env, "inherits_credentials": False, "overrides_HOME": False})
    begin = len(api.requests)
    process = subprocess.Popen(argv, cwd=root / "work", env=env, stdin=subprocess.PIPE if stdin_data is not None else subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    if stdin_data is not None:
        (capture / "stdin.jsonl").write_bytes(stdin_data)
    timed_out = False
    try:
        stdout, stderr = process.communicate(input=stdin_data, timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        os.killpg(process.pid, signal.SIGTERM)
        try:
            stdout, stderr = process.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate(timeout=3)
    (capture / "stdout.jsonl").write_bytes(stdout)
    (capture / "stderr.txt").write_bytes(stderr)
    requests = api.requests[begin:]
    dump(capture / "requests.json", requests)
    snapshot_requests(api, begin, capture)
    rows = []
    for line in stdout.decode(errors="replace").splitlines():
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    ids = [row["session_id"] for row in rows if isinstance(row, dict) and isinstance(row.get("session_id"), str)]
    result = {"case": name, "exit_code": process.returncode, "timed_out": timed_out, "requests": len(requests), "session_ids": list(dict.fromkeys(ids)), "result_subtypes": [row.get("subtype") for row in rows if row.get("type") == "result"], "stderr": stderr.decode(errors="replace"), "stdout_sha256": hashlib.sha256(stdout).hexdigest()}
    dump(capture / "result.json", result)
    snapshot_transcripts(root, capture)
    return result, requests


def find_session(config: Path, session_id: str) -> Path:
    found = list(config.rglob(f"{session_id}.jsonl"))
    if len(found) != 1:
        raise AssertionError(f"Expected one owned session {session_id}; got {found}")
    return found[0]


def copy_session(source: Path, destination_id: str, title: str):
    rows = [json.loads(line) for line in source.read_text().splitlines()]
    for row in rows:
        if "sessionId" in row:
            row["sessionId"] = destination_id
    rows.append({"type": "custom-title", "sessionId": destination_id, "customTitle": title})
    target = source.with_name(f"{destination_id}.jsonl")
    target.write_text("".join(json.dumps(row, separators=(",", ":")) + "\n" for row in rows))
    return target


def request_with(requests, marker):
    return next((item["body"] for item in requests if "count_tokens" not in item["path"] and marker in json.dumps(item["body"].get("messages", []))), None)


def user_content_with(body, marker):
    return next((row.get("content") for row in (body or {}).get("messages", []) if row.get("role") == "user" and marker in text_content(row.get("content"))), None)


def run_error_engine(binary: Path, engine: str, root: Path, timeout: int, setup_fake_account=False):
    """Exercise public session rejection and empty-continue behavior."""
    profile(root, engine)
    cases = [
        ("invalid-session-id", ["--session-id", "invalid-session-id"]),
        ("prefixed-session-id", ["--session-id", f"sess:{SESSION}"]),
        ("session-id-resume-conflict", ["--session-id", SESSION, "--resume", MISSING]),
        ("session-id-continue-conflict", ["--session-id", SESSION, "--continue"]),
        ("resume-at-requires-resume", ["--resume-session-at", SESSION]),
        ("drops-turn-requires-at", ["--resume-drops-turn", SESSION]),
        ("rewind-requires-resume", ["--rewind-files", SESSION]),
        ("rewind-rejects-prompt", ["--rewind-files", SESSION, "--resume", MISSING]),
        ("missing-uuid", ["--resume", MISSING]),
        ("missing-name", ["--resume", "SESSION_MISSING_NAME"]),
        ("empty-continue", ["--continue"]),
    ]
    outcomes = []
    with Loopback(root / "provider", root / "work") as api:
        for name, flags in cases:
            case_root = root / "cases" / name
            profile(case_root, engine)
            expected_continue = name == "empty-continue"
            account = setup_lingxi_account(binary, case_root, api, max(timeout, 30)) if engine == "lingxi" and setup_fake_account and expected_continue else None
            try:
                result, _requests = run_process(binary, engine, case_root, api, name, flags, "SESSION_ERROR_PROMPT", timeout=timeout)
            finally:
                if account and account.get("account_id"):
                    remove_lingxi_account(binary, case_root, api, account["account_id"], max(timeout, 30))
            result["passed"] = result["exit_code"] == (0 if expected_continue else 1) and result["requests"] == (1 if expected_continue else 0) and not result["timed_out"]
            dump(case_root / "captures" / name / "result.json", result)
            outcomes.append(result)
            print(f"{engine}/{name}: exit={result['exit_code']} requests={result['requests']}", flush=True)
        # Existence is sufficient for a fresh --session-id collision. Its bytes
        # belong to this fixture; no production transcript is opened or edited.
        slug = re.sub(r"[^a-zA-Z0-9]", "-", str(root / "work"))
        occupied = root / "config" / "projects" / slug / f"{SESSION}.jsonl"
        occupied.parent.mkdir(parents=True, exist_ok=True)
        occupied.write_text(json.dumps({"type": "custom-title", "sessionId": SESSION, "customTitle": "SESSION_OWNED_OCCUPIED"}) + "\n")
        before = occupied.read_bytes()
        result, _requests = run_process(binary, engine, root, api, "occupied-session-id", ["--session-id", SESSION], "SESSION_ERROR_PROMPT", timeout=timeout)
        result["source_unchanged"] = occupied.read_bytes() == before
        result["passed"] = result["exit_code"] == 1 and result["requests"] == 0 and not result["timed_out"] and result["source_unchanged"]
        outcomes.append(result)
        report = {"engine": engine, "suite": "errors", "passed": all(row["passed"] for row in outcomes), "outcomes": outcomes, "refused_routes": api.refused}
    dump(root / "report.json", report)
    return report


def run_error_format_engine(binary: Path, engine: str, root: Path, timeout: int):
    """Verify early lookup output independently of runtime/credential startup."""
    profile(root, engine)
    outcomes = []
    with Loopback(root / "provider", root / "work") as api:
        for selection, value in (("missing-uuid", MISSING), ("missing-name", "SESSION_MISSING_NAME")):
            for format_name, output_format, verbose in (("text", "text", False), ("json", "json", False), ("json-verbose", "json", True), ("stream-json", "stream-json", True)):
                name = f"{selection}-{format_name}"
                case_root = root / "cases" / name
                profile(case_root, engine)
                result, _requests = run_process(binary, engine, case_root, api, name, ["--resume", value], "SESSION_ERROR_PROMPT", timeout=timeout, output_format=output_format, verbose=verbose)
                raw = (case_root / "captures" / name / "stdout.jsonl").read_bytes()
                result["output_contract"] = result["result_subtypes"] == ["error_during_execution"] if output_format == "stream-json" else raw == b""
                result["passed"] = result["exit_code"] == 1 and result["requests"] == 0 and not result["timed_out"] and result["output_contract"]
                dump(case_root / "captures" / name / "result.json", result)
                outcomes.append(result)
                print(f"{engine}/{name}: exit={result['exit_code']} requests={result['requests']}", flush=True)
        report = {"engine": engine, "suite": "error-formats", "passed": all(row["passed"] for row in outcomes), "outcomes": outcomes, "refused_routes": api.refused}
    dump(root / "report.json", report)
    return report


def compare_error_bytes(output: Path, reports):
    """Keep exact stderr differences visible; never normalize branding here."""
    if {report["engine"] for report in reports} != {"native", "lingxi"}:
        return None
    by_engine = {report["engine"]: {case["case"]: case for case in report["outcomes"]} for report in reports}
    comparisons = []
    for name, native in by_engine["native"].items():
        product = by_engine["lingxi"][name]
        exact = native["stderr"] == product["stderr"]
        branded = name.startswith("missing-name") and native["stderr"].replace("Usage: claude -p", "Usage: lingxi -p", 1) == product["stderr"]
        same_exit = native["exit_code"] == product["exit_code"]
        row = {"case": name, "same_exit_code": same_exit, "same_stderr_bytes": exact, "classification": "equal" if exact and same_exit else "intentional-usage-branding" if branded and same_exit else "mismatch", "native_stderr": native["stderr"], "lingxi_stderr": product["stderr"]}
        if name != "empty-continue":
            def raw_stdout(engine):
                capture = output / engine / "cases" / name / "captures" / name / "stdout.jsonl"
                if name == "occupied-session-id":
                    capture = output / engine / "captures" / name / "stdout.jsonl"
                return capture.read_text()
            native_stdout, product_stdout = raw_stdout("native"), raw_stdout("lingxi")
            def declared_dynamic_fields(raw):
                raw = re.sub(r'"uuid":"[a-fA-F0-9-]{36}"', '"uuid":"<generated-uuid>"', raw)
                if name.startswith("missing-name"):
                    raw = re.sub(r'"session_id":"[a-fA-F0-9-]{36}"', '"session_id":"<generated-session-id>"', raw)
                    raw = raw.replace("Usage: claude -p", "Usage: lingxi -p", 1)
                return raw
            row["same_stdout_bytes"] = native_stdout == product_stdout
            row["same_stdout_after_declared_dynamic_fields"] = declared_dynamic_fields(native_stdout) == declared_dynamic_fields(product_stdout)
            if not row["same_stdout_after_declared_dynamic_fields"]:
                row["classification"] = "mismatch"
            elif not row["same_stdout_bytes"] and row["classification"] == "equal":
                row["classification"] = "generated-uuid-only"
        comparisons.append(row)
    report = {"all_stderr_bytes_equal": all(row["same_stderr_bytes"] and row["same_exit_code"] for row in comparisons), "parity_passed": all(row["classification"] != "mismatch" for row in comparisons), "comparisons": comparisons, "scope": "Exact exit code/stderr and rejection stdout bytes, allowing only explicitly generated UUID fields and the usage executable name. Fresh-continue provider execution is semantic; no general terminal/request byte-equivalence claim."}
    dump(output / "error-byte-comparison.json", report)
    return report


def run_compact_stream(binary: Path, engine: str, root: Path, api: Loopback, timeout: int):
    """Real SDK process: two turns, manual /compact, then cold resume elsewhere."""
    sid = "33333333-3333-4333-8333-333333333333"
    target = root / "captures" / "compact-stream"
    target.mkdir()
    command = [str(binary), "--bare", "--print", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--model", MODEL, "--session-id", sid, "--system-prompt", "You are a deterministic local session fixture.", "--setting-sources", "", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}', "--tools", ""]
    env = isolated_env(engine, root / "config", root / "tmp", api.base_url)
    dump(target / "invocation.json", {"argv": command, "cwd": str(root / "work"), "environment": env})
    process = subprocess.Popen(command, cwd=root / "work", env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1, start_new_session=True)
    output: queue.Queue = queue.Queue()
    readers = []
    events, timed_out, failure = [], False, None
    begin = len(api.requests)

    def drain(pipe, filename):
        with (target / filename).open("w") as file:
            for line in pipe:
                file.write(line)
                file.flush()
                if filename == "stdout.jsonl":
                    output.put(line)

    for pipe, filename in ((process.stdout, "stdout.jsonl"), (process.stderr, "stderr.txt")):
        thread = threading.Thread(target=drain, args=(pipe, filename), daemon=True)
        thread.start()
        readers.append(thread)
    try:
        for prompt in ("SESSION_COMPACT_FIRST", "SESSION_COMPACT_SECOND", "/compact Preserve SESSION_COMPACT_RETAINED_MARKER_321"):
            process.stdin.write(json.dumps({"type": "user", "uuid": str(uuid.uuid4()), "message": {"role": "user", "content": prompt}}) + "\n")
            process.stdin.flush()
            deadline = time.monotonic() + timeout
            while True:
                try:
                    row = json.loads(output.get(timeout=0.2))
                except queue.Empty:
                    if process.poll() is not None:
                        raise RuntimeError(f"CLI exited before result: {process.returncode}")
                    if time.monotonic() > deadline:
                        timed_out = True
                        raise TimeoutError(f"No result for {prompt}")
                    continue
                events.append(row)
                if row.get("type") == "result":
                    break
        process.stdin.close()
        process.wait(timeout=timeout)
    except (RuntimeError, TimeoutError, subprocess.TimeoutExpired, BrokenPipeError) as error:
        failure = str(error)
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=3)
        for thread in readers:
            thread.join(timeout=2)
    requests = api.requests[begin:]
    dump(target / "requests.json", requests)
    snapshot_requests(api, begin, target)
    result = {"case": "compact-stream", "exit_code": process.returncode, "timed_out": timed_out, "failure": failure, "requests": len(requests), "session_id": sid, "compact_boundaries": len([event for event in events if event.get("subtype") == "compact_boundary"])}
    result["passed"] = result["exit_code"] == 0 and result["compact_boundaries"] == 1 and failure is None
    dump(target / "result.json", result)
    snapshot_transcripts(root, target)
    return result


def run_engine(binary: Path, engine: str, root: Path, timeout: int, setup_fake_account=False):
    profile(root, engine)
    outcomes = []
    account, report = None, None
    with Loopback(root / "provider", root / "work") as api:
        try:
            account = setup_lingxi_account(binary, root, api, timeout) if engine == "lingxi" and setup_fake_account else None
            if account and not account["passed"]:
                report = {"engine": engine, "passed": False, "blocker": "isolated fake account setup failed", "account_setup": account, "outcomes": [], "refused_routes": api.refused}
                dump(root / "report.json", report)
                return report
            def run(name, flags, prompt, tools="", stdin_data=None):
                result, requests = run_process(binary, engine, root, api, name, flags, prompt, tools, timeout, stdin_data)
                outcomes.append(result)
                print(f"{engine}/{name}: exit={result['exit_code']} requests={result['requests']}", flush=True)
                return result, requests

            result, requests = run("seed", ["--session-id", SESSION], "SESSION_SEED_ALPHA")
            if result["exit_code"] != 0 or not request_with(requests, "SESSION_SEED_ALPHA"):
                report = {"engine": engine, "passed": False, "blocker": "seed CLI failed", "account_setup": account, "account_cleanup": None, "outcomes": outcomes, "refused_routes": api.refused}
                dump(root / "report.json", report)
                return report
            source = find_session(root / "config", SESSION)
            (root / "seed.jsonl").write_bytes(source.read_bytes())
            seed_content = next(json.loads(line)["message"]["content"] for line in source.read_text().splitlines() if json.loads(line).get("type") == "user")
            result, requests = run("resume-uuid", ["--resume", SESSION], "SESSION_RESUME_UUID")
            body = request_with(requests, "SESSION_RESUME_UUID")
            result["history_contains_seed"] = body is not None and "SESSION_SEED_ALPHA" in json.dumps(body["messages"])
            result["same_session_id"] = SESSION in result["session_ids"]
            result["stored_content_root_preserved"] = type(user_content_with(body, "SESSION_SEED_ALPHA")) is type(seed_content)
            result, requests = run("continue", ["--continue"], "SESSION_CONTINUE")
            body = request_with(requests, "SESSION_CONTINUE")
            result["history_contains_resume"] = body is not None and "SESSION_RESUME_UUID" in json.dumps(body["messages"])
            before = source.read_bytes()
            result, requests = run("fork", ["--resume", SESSION, "--fork-session"], "SESSION_FORK")
            result["new_session_id"] = bool(result["session_ids"]) and all(sid != SESSION for sid in result["session_ids"])
            result["source_unchanged"] = source.read_bytes() == before
            body = request_with(requests, "SESSION_FORK")
            result["fork_history_present"] = body is not None and all(marker in json.dumps(body["messages"]) for marker in ("SESSION_SEED_ALPHA", "SESSION_CONTINUE"))
            result["stored_content_root_preserved"] = type(user_content_with(body, "SESSION_SEED_ALPHA")) is type(seed_content)
            result, requests = run("resume-file", ["--resume", str(source)], "SESSION_RESUME_FILE")
            result["history_contains_seed"] = (body := request_with(requests, "SESSION_RESUME_FILE")) is not None and "SESSION_SEED_ALPHA" in json.dumps(body["messages"])
            with source.open("a") as file:
                file.write(json.dumps({"type": "custom-title", "sessionId": SESSION, "customTitle": "SESSION_NAMED_ALPHA"}) + "\n")
            result, requests = run("resume-name", ["--resume", "SESSION_NAMED_ALPHA"], "SESSION_RESUME_NAME")
            result["history_contains_seed"] = (body := request_with(requests, "SESSION_RESUME_NAME")) is not None and "SESSION_SEED_ALPHA" in json.dumps(body["messages"])
            copy_session(source, SECOND, "SESSION_NAMED_ALPHA")
            run("ambiguous-name", ["--resume", "SESSION_NAMED_ALPHA"], "SESSION_AMBIGUOUS")
            run("missing-uuid", ["--resume", MISSING], "SESSION_MISSING")
            dump(root / "provider" / "refused.json", api.refused)
            source_records = [json.loads(line) for line in source.read_text().splitlines()]
            assistant = next((row for row in source_records if row.get("type") == "assistant"), None)
            if assistant:
                result, requests = run("resume-at", ["--resume", SESSION, "--resume-session-at", assistant["uuid"]], "SESSION_RESUME_AT")
                result["selected_message_uuid"] = assistant["uuid"]
                body = request_with(requests, "SESSION_RESUME_AT")
                result["selected_prefix_present"] = body is not None and all(marker in json.dumps(body["messages"]) for marker in ("SESSION_SEED_ALPHA", "MOCK_OK SESSION_SEED_ALPHA"))
                result["later_history_excluded"] = body is not None and all(marker not in json.dumps(body["messages"]) for marker in ("SESSION_RESUME_UUID", "SESSION_CONTINUE", "SESSION_RESUME_FILE", "SESSION_RESUME_NAME"))
            tool_sid = "44444444-4444-4444-8444-444444444444"
            result, requests = run("tool-seed", ["--session-id", tool_sid], "SESSION_TOOL_SEED", "Read")
            result["tool_result_persisted"] = (tool_source := find_session(root / "config", tool_sid)).is_file() and "SESSION_TOOL_FILE_MARKER_321" in tool_source.read_text()
            result, requests = run("tool-resume", ["--resume", tool_sid], "SESSION_TOOL_RESUME", "Read")
            body = request_with(requests, "SESSION_TOOL_RESUME")
            result["tool_history_present"] = body is not None and all(marker in json.dumps(body["messages"]) for marker in ("toolu_live_read", "SESSION_TOOL_FILE_MARKER_321", "tool_result"))
            array_sid = "66666666-6666-4666-8666-666666666666"
            array_input = (json.dumps({"type": "user", "uuid": str(uuid.uuid4()), "message": {"role": "user", "content": [{"type": "text", "text": "SESSION_ARRAY_SEED"}]}}) + "\n").encode()
            result, requests = run("array-seed", ["--session-id", array_sid, "--input-format", "stream-json"], None, stdin_data=array_input)
            array_source = find_session(root / "config", array_sid)
            saved_array = next((json.loads(line)["message"]["content"] for line in array_source.read_text().splitlines() if json.loads(line).get("type") == "user"), None)
            result["source_remains_array"] = isinstance(saved_array, list)
            result, requests = run("array-resume", ["--resume", array_sid], "SESSION_ARRAY_RESUME")
            body = request_with(requests, "SESSION_ARRAY_RESUME")
            result["array_history_root_preserved"] = isinstance(user_content_with(body, "SESSION_ARRAY_SEED"), list)
            result["array_history_content_matches_source"] = user_content_with(body, "SESSION_ARRAY_SEED") == saved_array
            compact = run_compact_stream(binary, engine, root, api, timeout)
            outcomes.append(compact)
            print(f"{engine}/compact-stream: {compact}", flush=True)
            if compact["passed"]:
                result, requests = run("compact-resume", ["--resume", compact["session_id"]], "SESSION_COMPACT_RESUME")
                body = request_with(requests, "SESSION_COMPACT_RESUME")
                result["summary_present"] = body is not None and "SESSION_COMPACT_RETAINED_MARKER_321" in json.dumps(body["messages"])
                result["retained_markers"] = [marker for marker in ("SESSION_COMPACT_FIRST", "SESSION_COMPACT_SECOND") if body and marker in json.dumps(body["messages"])]
                result["retained_turn_matches_native"] = result["retained_markers"] == ["SESSION_COMPACT_SECOND"]
            for case in outcomes:
                if case["case"] == "compact-stream":
                    continue
                expected_error = case["case"] in ("ambiguous-name", "missing-uuid")
                checks = [value for key, value in case.items() if isinstance(value, bool) and key != "timed_out"]
                case["passed"] = case["exit_code"] == (1 if expected_error else 0) and not case["timed_out"] and all(checks)
                if expected_error:
                    case["passed"] &= case["requests"] == 0 and ("matches 2 sessions" if case["case"] == "ambiguous-name" else "No conversation found with session ID") in case["stderr"]
                dump(root / "captures" / case["case"] / "result.json", case)
            report = {"engine": engine, "passed": all(case["passed"] for case in outcomes), "source_path": str(source), "account_setup": account, "account_cleanup": None, "outcomes": outcomes, "refused_routes": api.refused}
        finally:
            if account and account.get("account_id"):
                cleanup = remove_lingxi_account(binary, root, api, account["account_id"], timeout)
                if report is not None:
                    report["account_cleanup"] = cleanup
                    report["passed"] = report["passed"] and cleanup["passed"]
                    dump(root / "report.json", report)

    dump(root / "report.json", report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", type=Path, default=Path.home() / ".local/share/claude/versions/2.1.321")
    parser.add_argument("--lingxi", type=Path, default=Path("/Users/luolingfeng/lingxi/lingxi-app/target/debug/lingxi-cli"))
    parser.add_argument("--engine", choices=("native", "lingxi", "both"), default="both")
    parser.add_argument("--suite", choices=("sessions", "errors", "error-formats"), default="sessions")
    parser.add_argument("--setup-fake-account", action="store_true", help="Create then remove one dummy-key account through the signed CLI in this owned config scope")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout", type=int, default=30)
    args = parser.parse_args()
    assert not args.output.exists(), "Select a new evidence directory"
    assert hashlib.sha256(args.native.read_bytes()).hexdigest() == NATIVE_SHA, "Native binary identity changed"
    args.output.mkdir(parents=True)
    reports = []
    engines = ["native", "lingxi"] if args.engine == "both" else [args.engine]
    for engine in engines:
        binary = args.native if engine == "native" else args.lingxi
        if args.suite == "errors":
            reports.append(run_error_engine(binary, engine, args.output / engine, args.timeout, args.setup_fake_account))
        elif args.suite == "error-formats":
            reports.append(run_error_format_engine(binary, engine, args.output / engine, args.timeout))
        else:
            reports.append(run_engine(binary, engine, args.output / engine, args.timeout, args.setup_fake_account))
    comparison = compare_error_bytes(args.output, reports) if args.suite in ("errors", "error-formats") else None
    dump(args.output / "report.json", {"version": "2.1.321", "suite": args.suite, "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), "native_sha256": NATIVE_SHA, "binary_hashes": {engine: hashlib.sha256((args.native if engine == "native" else args.lingxi).read_bytes()).hexdigest() for engine in engines}, "reports": reports, "error_byte_comparison": comparison, "scope": "Real CLI process and owned local filesystem with loopback scripted model. Semantic session assertions plus exact error stderr comparisons; no full request/terminal byte equivalence, UI, live model or remote storage claim."})
    raise SystemExit(0 if all(report["passed"] for report in reports) and (comparison is None or comparison["parity_passed"]) else 1)


if __name__ == "__main__":
    main()
