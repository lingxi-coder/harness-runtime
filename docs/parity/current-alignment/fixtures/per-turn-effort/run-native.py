#!/usr/bin/env python3
"""Observe effort markers across two public CLI processes and cold resume."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer


ROOT = Path(__file__).resolve().parent
BINARY = Path(os.environ["CLAUDE_NATIVE_BINARY"]).resolve()
NATIVE_HASHES = {
    "2.1.293": "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e",
    "2.1.295": "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70",
}
SESSION = "87788778-4321-4321-8321-123456789abc"
KEY = "native-effort-cold-resume-dummy-key"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def raw_array_items(raw):
    text = raw.decode("utf-8")
    decoder = json.JSONDecoder()
    pos = 1
    while True:
        while text[pos] in " \t\r\n,":
            pos += 1
        if text[pos] == "]":
            return
        value, end = decoder.raw_decode(text, pos)
        yield value, text[pos:end].encode("utf-8")
        pos = end


def raw_member(raw, key):
    text = raw.decode("utf-8")
    decoder = json.JSONDecoder()
    pos = 1
    while True:
        while text[pos] in " \t\r\n,":
            pos += 1
        if text[pos] == "}":
            raise KeyError(key)
        name, pos = decoder.raw_decode(text, pos)
        while text[pos] in " \t\r\n:":
            pos += 1
        start = pos
        _, pos = decoder.raw_decode(text, pos)
        if name == key:
            return text[start:pos].encode("utf-8")


def main():
    version = subprocess.check_output([str(BINARY), "--version"]).decode().strip()
    number = version.split(" ", 1)[0]
    expected_sha = NATIVE_HASHES[number]
    assert version == f"{number} (Claude Code)", version
    assert sha(BINARY.read_bytes()) == expected_sha
    run = Path(tempfile.mkdtemp(prefix=f"claude-alignment-native-effort-{number}-", dir="/private/tmp"))
    for name in ("home", "config", "cwd", "tmp"):
        (run / name).mkdir()
    response = (ROOT.parent.parent.parent.parent.parent / "crates/llm-runtime/tests/fixtures/http-layout-293/native-offered-response.raw").read_bytes().split(b"\r\n\r\n", 1)[1]
    records = []
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass
        def do_POST(self):
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            accepted = self.path == "/v1/messages?beta=true" and self.headers.get("x-api-key") == KEY
            index = len(records) + 1
            (run / f"request-{index:02}.raw.json").write_bytes(raw)
            records.append({"index": index, "accepted": accepted, "sha256": sha(raw), "bytes": len(raw)})
            body = response if accepted else b"unexpected loopback request"
            self.send_response(200 if accepted else 400)
            self.send_header("Content-Type", "text/event-stream" if accepted else "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
    server = HTTPServer(("127.0.0.1", 0), Handler)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    env = {"PATH": "/usr/bin:/bin", "HOME": str(run / "home"), "TMPDIR": str(run / "tmp"),
           "CLAUDE_CONFIG_DIR": str(run / "config"), "LANG": "C.UTF-8", "TZ": "UTC", "TERM": "dumb",
           "ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
           "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "DISABLE_TELEMETRY": "1", "DO_NOT_TRACK": "1"}
    base = [str(BINARY), "--setting-sources", "project", "--strict-mcp-config", "--print", "--tools", "",
            "--model", "claude-sonnet-5-5", "--output-format", "json", "--system-prompt", "Fixed effort fixture."]
    runs = []
    try:
        for index, (effort, prompt) in enumerate((("low", "First fixed prompt."), ("high", "Second fixed prompt.")), 1):
            argv = base + ["--session-id" if index == 1 else "--resume", SESSION, "--effort", effort, prompt]
            process = subprocess.run(argv, cwd=run / "cwd", env=env, capture_output=True, timeout=45)
            (run / f"process-{index}.stdout").write_bytes(process.stdout)
            (run / f"process-{index}.stderr").write_bytes(process.stderr)
            runs.append({"index": index, "effort": effort, "exit_code": process.returncode,
                         "stdout_sha256": sha(process.stdout), "stderr_bytes": len(process.stderr)})
            if process.returncode:
                break
    finally:
        server.shutdown()
        server.server_close()
        worker.join()
    baseline = run / "curated" / number
    baseline.mkdir(parents=True, exist_ok=True)
    marker_records = []
    for record in records:
        index = record["index"]
        raw = (run / f"request-{index:02}.raw.json").read_bytes()
        messages_raw = raw_member(raw, "messages")
        (baseline / f"request-{index:02}.messages.raw.json").write_bytes(messages_raw)
        markers = []
        for message_index, (value, source) in enumerate(raw_array_items(messages_raw)):
            if value.get("role") == "system" and "output_config" in value:
                name = f"request-{index:02}.message-{message_index:02}.raw.json"
                (baseline / name).write_bytes(source)
                markers.append({"message_index": message_index, "effort": value["output_config"].get("effort"),
                                "file": name, "sha256": sha(source)})
        marker_records.append({"request": index, "markers": markers})
    sessions = sorted((run / "config").rglob(f"{SESSION}.jsonl"))
    assistant_rows = []
    if sessions:
        for line in sessions[0].read_bytes().splitlines():
            row = json.loads(line)
            if row.get("type") == "assistant":
                assistant_rows.append(line)
        (baseline / "assistant-rows.raw.jsonl").write_bytes(b"\n".join(assistant_rows) + b"\n")
    receipt = {"native_version": version, "native_sha256": expected_sha, "run_directory": str(run),
               "runner_sha256": sha(Path(__file__).read_bytes()), "session_id": SESSION,
               "scope": "Native-only public CLI initial request and cold resume; dummy loopback; exact source slices",
               "runs": runs, "requests": records, "markers": marker_records,
               "assistant_efforts": [json.loads(row).get("perTurnEffort") for row in assistant_rows]}
    (baseline / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))
    assert len(runs) == 2 and all(run["exit_code"] == 0 for run in runs)
    assert len(records) == 2 and all(record["accepted"] for record in records)


if __name__ == "__main__":
    main()
