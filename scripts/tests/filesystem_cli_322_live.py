#!/usr/bin/env python3
"""Exercise real filesystem CLI calls and process resume with a local provider.

The server scripts tool requests; no remote model or external account is used.
LingXi may opt into a disposable local named account with a synthetic key.
Only a fresh output directory is writable by the fixture. Captures are semantic
runtime evidence, not a byte-parity verdict or evidence about a different build.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
import signal
import subprocess
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from session_cli_321_live import (MODEL, dump, isolated_env, snapshot_requests, snapshot_transcripts,
                                  setup_lingxi_account, remove_lingxi_account)

NATIVE_SHA = "a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450"
TOOLS = "Read,Write,Edit,NotebookEdit,Glob,Grep"


def blocks(value):
    """Walk request JSON without interpreting content as executable code."""
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from blocks(child)
    elif isinstance(value, list):
        for child in value:
            yield from blocks(child)


class FilesystemLoopback:
    """Own every listener, request and fixture file; never forward requests."""

    def __init__(self, root: Path, work: Path):
        self.root, self.work = root, work
        root.mkdir(parents=True)
        self.requests, self.refused, self.tool_errors = [], [], []
        self.lock = threading.Lock()
        self.resume_without_read = None
        owner = self
        notebook = json.dumps({
            "cells": [{"cell_type": "code", "id": "c1", "execution_count": None,
                       "metadata": {}, "outputs": [], "source": ["print('before')\n"]}],
            "metadata": {"language_info": {"name": "python"}},
            "nbformat": 4, "nbformat_minor": 5,
        })
        self.seed = [
            ("Write", {"file_path": str(work / "doc.txt"), "content": "FS_CLI_ALPHA_322\n"}),
            ("Read", {"file_path": str(work / "doc.txt")}),
            ("Edit", {"file_path": str(work / "doc.txt"), "old_string": "ALPHA", "new_string": "BETA"}),
            ("Glob", {"pattern": "*.txt", "path": str(work)}),
            ("Grep", {"pattern": "FS_CLI_BETA_322", "path": str(work / "doc.txt"), "output_mode": "content"}),
            ("Write", {"file_path": str(work / "notes.ipynb"), "content": notebook}),
            ("Read", {"file_path": str(work / "notes.ipynb")}),
            ("NotebookEdit", {"notebook_path": str(work / "notes.ipynb"), "cell_id": "c1", "new_source": "print('after')"}),
        ]

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_CONNECT(self):
                owner.refused.append("CONNECT " + self.path)
                self.send_error(403)

            def do_GET(self):
                owner.refused.append(self.path)
                self.send_error(403)

            def respond_json(self, status, value):
                raw = json.dumps(value).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def do_POST(self):
                route = self.path.split("?")[0]
                if route not in ("/v1/messages", "/messages", "/v1/messages/count_tokens", "/messages/count_tokens"):
                    owner.refused.append(self.path)
                    self.send_error(403)
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
                content, stop = owner.response(body)
                message = {"id": f"msg_fs_322_{ordinal}", "type": "message", "role": "assistant",
                           "model": body.get("model", MODEL), "content": [], "stop_reason": None,
                           "stop_sequence": None, "usage": {"input_tokens": 2500, "output_tokens": 0,
                           "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
                if not body.get("stream"):
                    message.update(content=content, stop_reason=stop)
                    message["usage"]["output_tokens"] = 20
                    self.respond_json(200, message)
                    return
                events = [{"type": "message_start", "message": message}]
                for index, block in enumerate(content):
                    is_tool = block["type"] == "tool_use"
                    initial = {**block, "input": {}} if is_tool else {"type": "text", "text": ""}
                    delta = {"type": "input_json_delta", "partial_json": json.dumps(block["input"])} if is_tool else {"type": "text_delta", "text": block["text"]}
                    events.extend([{"type": "content_block_start", "index": index, "content_block": initial},
                                   {"type": "content_block_delta", "index": index, "delta": delta},
                                   {"type": "content_block_stop", "index": index}])
                events.extend([{"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None},
                                "usage": {"output_tokens": 20}}, {"type": "message_stop"}])
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

    def response(self, body):
        messages = body.get("messages", [])
        results = {block["tool_use_id"]: block for block in blocks(messages)
                   if block.get("type") == "tool_result" and block.get("tool_use_id", "").startswith("fs322_")}
        resumed = "FS_CLI_RESUME_322" in json.dumps(messages)
        if resumed:
            first = results.get("fs322_resume_edit")
            if first is None:
                return self.tool("fs322_resume_edit", "Edit", {"file_path": str(self.work / "doc.txt"), "old_string": "BETA", "new_string": "GAMMA"})
            self.resume_without_read = {"succeeded": not bool(first.get("is_error")), "result": first.get("content")}
            if "fs322_resume_read" not in results:
                return self.tool("fs322_resume_read", "Read", {"file_path": str(self.work / "doc.txt")})
            if first.get("is_error") and "fs322_resume_retry" not in results:
                return self.tool("fs322_resume_retry", "Edit", {"file_path": str(self.work / "doc.txt"), "old_string": "BETA", "new_string": "GAMMA"})
            relevant = {key: result for key, result in results.items() if key.startswith("fs322_resume_") and key != "fs322_resume_edit"}
            errors = {key: value for key, value in relevant.items() if value.get("is_error")}
            if errors:
                self.tool_errors.extend(errors.values())
            return [{"type": "text", "text": "FS_CLI_RESUME_DONE_322" if not errors else "FS_CLI_TOOL_ERROR_322"}], "end_turn"
        for index, (name, value) in enumerate(self.seed):
            key = f"fs322_seed_{index:02}"
            if key not in results:
                return self.tool(key, name, value)
            if results[key].get("is_error"):
                self.tool_errors.append(results[key])
                return [{"type": "text", "text": "FS_CLI_TOOL_ERROR_322"}], "end_turn"
        return [{"type": "text", "text": "FS_CLI_DONE_322"}], "end_turn"

    @staticmethod
    def tool(identity, name, value):
        return [{"type": "tool_use", "id": identity, "name": name, "input": value}], "tool_use"


@contextmanager
def credential_fixture(binary, engine, root, api, timeout, enabled=False):
    """Opt-in dummy named account; remove its exact ID after the owned run."""
    info = {"local_named_account_created": False, "external_account_registration": False}
    account = None
    if enabled:
        if engine != "lingxi":
            raise ValueError("Dummy named accounts are only supported for LingXi")
        account = setup_lingxi_account(binary, root, api, max(timeout, 30))
        info.update(local_named_account_created=bool(account.get("passed")), setup=account)
        if not account.get("passed"):
            raise RuntimeError("Disposable named-account setup failed: " + str(account))
    try:
        yield info
    finally:
        if account and account.get("account_id"):
            cleanup = remove_lingxi_account(binary, root, api, account["account_id"], max(timeout, 30))
            info["cleanup"] = cleanup
            dump(root / "captures/credential-fixture.json", info)
            if not cleanup.get("passed"):
                raise RuntimeError("Disposable named-account cleanup failed: " + str(cleanup))


def run_process(binary, engine, root, api, name, flags, prompt, tools, timeout):
    # --bare intentionally exposes only a small catalog in Native322. Safe mode
    # disables customizations while retaining the six public filesystem tools.
    capture = root / "captures" / name
    capture.mkdir()
    argv = [str(binary), "--safe-mode", "--print", "--output-format", "stream-json", "--verbose",
            "--model", MODEL, "--system-prompt", "You are a deterministic local filesystem fixture.",
            "--setting-sources", "", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
            "--tools", tools, *flags, "--allowedTools", tools, "--", prompt]
    env = isolated_env(engine, root / "config", root / "tmp", api.base_url)
    dump(capture / "invocation.json", {"argv": argv, "cwd": str(root / "work"), "environment": env,
                                       "inherits_credentials": False, "overrides_HOME": False,
                                       "mode": "safe-mode; empty user/project/local setting sources"})
    begin = len(api.requests)
    process = subprocess.Popen(argv, cwd=root / "work", env=env, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=timeout)
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
    result = {"case": name, "exit_code": process.returncode, "timed_out": timed_out,
              "requests": len(requests), "session_ids": list(dict.fromkeys(ids)),
              "result_subtypes": [row.get("subtype") for row in rows if row.get("type") == "result"],
              "stderr": stderr.decode(errors="replace"), "stdout_sha256": hashlib.sha256(stdout).hexdigest(),
              "completed": any(row.get("type") == "result" and row.get("subtype") == "success" and
                               ("FS_CLI_RESUME_DONE_322" if name == "resume" else "FS_CLI_DONE_322") in str(row.get("result", "")) for row in rows)}
    dump(capture / "result.json", result)
    snapshot_transcripts(root, capture)
    return result, requests


def run(binary: Path, engine: str, root: Path, timeout: int, setup_fake_account=False):
    binary = binary.resolve(strict=True)
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    if engine == "native" and digest != NATIVE_SHA:
        raise ValueError("Native executable must match pinned Claude Code 2.1.322 SHA-256")
    for part in ("config", "tmp", "work", "captures"):
        (root / part).mkdir(parents=True)
    (root / "config" / (".claude.json" if engine == "native" else ".lingxi.json")).write_text(json.dumps({"hasCompletedOnboarding": True}))
    sid = str(uuid.uuid4())
    outcomes = []
    report = {"engine": engine, "binary": str(binary), "sha256": digest, "session_id": sid,
              "account_registration": False, "remote_provider": False, "source_build_verified": False,
              "meaning": "Real CLI filesystem/lifecycle semantics for exactly this binary, without byte normalization.",
              "passed": False}
    with FilesystemLoopback(root / "provider", root / "work") as api, credential_fixture(
        binary, engine, root, api, timeout, setup_fake_account
    ) as credentials:
        seed, _ = run_process(binary, engine, root, api, "seed", ["--session-id", sid, "--max-turns", "20"], "FS_CLI_SEED_322", TOOLS, timeout)
        outcomes.append(seed)
        seed["text_bytes_correct"] = (root / "work/doc.txt").read_bytes() == b"FS_CLI_BETA_322\n" if (root / "work/doc.txt").is_file() else False
        notebook = root / "work/notes.ipynb"
        if notebook.is_file():
            source = json.loads(notebook.read_text())["cells"][0]["source"]
            seed["notebook_source_correct"] = "".join(source) == "print('after')" if isinstance(source, list) else source == "print('after')"
        else:
            seed["notebook_source_correct"] = False
        if seed["exit_code"] == 0 and seed["completed"] and seed["text_bytes_correct"] and seed["notebook_source_correct"] and not api.tool_errors:
            resumed, _ = run_process(binary, engine, root, api, "resume", ["--resume", sid, "--max-turns", "8"], "FS_CLI_RESUME_322", TOOLS, timeout)
            outcomes.append(resumed)
            resumed["text_bytes_correct"] = (root / "work/doc.txt").read_bytes() == b"FS_CLI_GAMMA_322\n"
        report.update(outcomes=outcomes, refused_routes=api.refused, tool_errors=api.tool_errors,
                      resume_without_read=api.resume_without_read,
                      passed=len(outcomes) == 2 and all(item["exit_code"] == 0 and item["completed"] and not item["timed_out"] and item["text_bytes_correct"] for item in outcomes) and seed["notebook_source_correct"] and not api.tool_errors)
    report.update(credential_fixture=credentials, account_registration=credentials["local_named_account_created"],
                  external_account_registration=False)
    dump(root / "report.json", report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--engine", choices=("native", "lingxi"), required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--setup-fake-account", action="store_true", help="provision and remove one dummy account in the owned config")
    parser.add_argument("--timeout", type=int, default=45)
    args = parser.parse_args()
    if not 1 <= args.timeout <= 120:
        parser.error("--timeout must be 1..120 seconds")
    args.output.mkdir(parents=True, exist_ok=False)
    report = run(args.binary, args.engine, args.output.resolve(), args.timeout, args.setup_fake_account)
    print(json.dumps({"engine": report["engine"], "passed": report["passed"], "resume_without_read": report.get("resume_without_read"), "report": str(args.output / "report.json")}))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
