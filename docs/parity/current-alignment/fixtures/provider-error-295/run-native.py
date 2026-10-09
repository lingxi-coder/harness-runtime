#!/usr/bin/env python3
"""Replay the existing fixed provider-error inputs against frozen Native 2.1.295."""
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import hashlib
import json
import os
import subprocess
import tempfile
import threading

ROOT = Path(__file__).resolve().parent
SOURCE = ROOT.parent / "provider-error"
BINARY = Path(os.environ["CLAUDE_NATIVE_BINARY"]).resolve()
SHA = "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def main():
    assert digest(BINARY.read_bytes()) == SHA, "frozen Native binary hash"
    version = subprocess.check_output([str(BINARY), "--version"], timeout=15).decode().strip()
    assert version.startswith("2.1.295 "), version
    original = json.loads((SOURCE / "cases.json").read_bytes())
    cases = [{"id": c["id"], "status": 400, "body": bytes.fromhex(c["response_body_hex"])}
             for c in original["cases"]]
    source_body = cases[0]["body"]
    for status in [403, 404, 413, 401, 500]:
        cases.append({"id": f"http-{status}", "status": status, "body": source_body,
                      "no_retry_header": status in [401, 500]})
    output = ROOT / "run-output"
    output.mkdir(parents=True, exist_ok=True)
    run = Path(tempfile.mkdtemp(prefix="native-provider-error-295-", dir=output))
    active = {}

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, *_args):
            pass

        def do_HEAD(self):
            self.send_response(200 if self.path == "/api/hello" else 404)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def do_POST(self):
            assert self.path == "/v1/messages?beta=true", self.path
            assert self.headers.get("x-api-key") == "native-provider-error-295-dummy"
            body = self.rfile.read(int(self.headers["content-length"]))
            index = len(active["requests"])
            (active["dir"] / f"request-{index}.body.raw.json").write_bytes(body)
            parsed = json.loads(body)
            active["requests"].append({"model": parsed.get("model"), "stream": parsed.get("stream"),
                                       "body_sha256": digest(body), "body_bytes": len(body)})
            case = active["case"]
            self.send_response(case["status"])
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(case["body"])))
            if case.get("no_retry_header"):
                self.send_header("x-should-retry", "false")
            self.end_headers()
            self.wfile.write(case["body"])

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    receipts = []
    try:
        for index, case in enumerate(cases):
            case_dir = run / case["id"]
            for name in ["config", "home", "tmp", "cwd"]:
                (case_dir / name).mkdir(parents=True)
            active.update(case=case, dir=case_dir, requests=[])
            session_id = f"00000000-0000-4000-8000-{29500 + index:012d}"
            env = {"PATH": "/usr/bin:/bin", "HOME": str(case_dir / "home"),
                   "TMPDIR": str(case_dir / "tmp"), "LANG": "C.UTF-8", "TZ": "UTC", "TERM": "dumb",
                   "CLAUDE_CONFIG_DIR": str(case_dir / "config"),
                   "ANTHROPIC_API_KEY": "native-provider-error-295-dummy",
                   "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
                   "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "DISABLE_TELEMETRY": "1", "DO_NOT_TRACK": "1"}
            if not case.get("no_retry_header"):
                env["ANTHROPIC_MAX_RETRIES"] = "0"
            argv = [str(BINARY), "--setting-sources", "project", "--strict-mcp-config",
                    "--print", "--output-format", "json", "--tools", "", "--max-budget-usd", "1",
                    "--model", original["model"], "--session-id", session_id, original["prompt"]]
            result = subprocess.run(argv, cwd=case_dir / "cwd", env=env, capture_output=True, timeout=45)
            (case_dir / "native.stdout.raw").write_bytes(result.stdout)
            (case_dir / "native.stderr.raw").write_bytes(result.stderr)
            (case_dir / "http-response.body.raw").write_bytes(case["body"])
            parsed = json.loads(result.stdout)
            rows = []
            for path in (case_dir / "config").rglob(f"{session_id}.jsonl"):
                for raw in path.read_bytes().splitlines(keepends=True):
                    row = json.loads(raw)
                    if row.get("type") == "assistant":
                        name = f"assistant-{len(rows)}.jsonl.raw"
                        (case_dir / name).write_bytes(raw)
                        rows.append(name)
            receipt = {"case": case["id"], "status": case["status"], "exit_code": result.returncode,
                       "session_id": session_id, "requests": active["requests"], "assistant_rows": rows,
                       "stdout_sha256": digest(result.stdout), "response_sha256": digest(case["body"]),
                       "no_retry_header": case.get("no_retry_header", False),
                       "result": parsed.get("result"), "is_error": parsed.get("is_error"),
                       "stop_reason": parsed.get("stop_reason"), "terminal_reason": parsed.get("terminal_reason")}
            receipts.append(receipt)
            print(json.dumps({"case": case["id"], "exit": result.returncode,
                              "requests": len(active["requests"]), "assistant_rows": len(rows)}), flush=True)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
    receipt = {"native_version": version, "native_sha256": SHA,
               "runner_sha256": digest(Path(__file__).read_bytes()),
               "gateway_authority": f"127.0.0.1:{server.server_port}", "cases": receipts}
    (run / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n")
    print(run, flush=True)


if __name__ == "__main__":
    main()
