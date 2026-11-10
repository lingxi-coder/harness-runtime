#!/usr/bin/env python3
"""Capture default 2.1.319 conversation prompts using an isolated loopback API."""

import argparse
import hashlib
import json
import os
import signal
import subprocess
import threading
from http.server import ThreadingHTTPServer
from pathlib import Path

from compact_live_oracle import isolated_env
from live_requests_287_oracle import make_handler

SHA256 = "1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd"
MODELS = ["claude-sonnet-4-6", "claude-opus-5", "claude-sonnet-5-5"]


def capture(binary, output, model, timeout, memory, child):
    root = output / model
    root.mkdir()
    for name in ["config", "work", "tmp"]:
        (root / name).mkdir()
    (root / "config" / ".claude.json").write_text(
        json.dumps({"hasCompletedOnboarding": True})
    )
    requests, refused = [], []
    server = ThreadingHTTPServer(
        ("127.0.0.1", 0), make_handler(root, "unavailable-agent" if child else "prompt-body", requests, refused)
    )
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    env = isolated_env(root, f"http://127.0.0.1:{server.server_port}")
    env.update({
        "CLAUDE_CODE_DISABLE_FEATURE_FLAGS": "1",
        "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "0" if memory else "1",
    })
    command = [
        str(binary), "-p", "WIRE_ROOT_AGENT" if child else "PROMPT_BODY_LOCAL_PROBE", "--model", model,
        "--output-format", "stream-json", "--verbose", "--setting-sources", "",
        "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
        "--disable-slash-commands", "--no-session-persistence",
        "--tools", "Read,Edit,Write,Glob,Grep,Bash,Agent,Skill,TaskCreate,TaskList,TaskUpdate,TaskGet",
    ]
    if child:
        command += [
            "--permission-mode", "bypassPermissions", "--dangerously-skip-permissions",
            "--agents", json.dumps({"wire-child": {
                "description": "Local prompt capture child", "prompt": "WIRE_CHILD_AGENT", "tools": [],
            }}),
        ]
    process = subprocess.Popen(
        command, cwd=root / "work", env=env, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, text=True, start_new_session=True,
    )
    try:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=5)
            raise RuntimeError(f"native prompt capture timed out: {model}")
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
    (root / "stdout.jsonl").write_text(stdout)
    (root / "stderr.txt").write_text(stderr)
    messages = [r for r in requests if "count_tokens" not in r["path"]]
    assert process.returncode == 0 and messages, (model, process.returncode, stderr[-1000:])
    body = messages[0]["body"]
    assert any("# Harness" in item.get("text", "") or "# System" in item.get("text", "")
               for item in body.get("system", [])), "default prompt was not captured"
    report = {
        "model": model, "memory_enabled": memory, "child": child,
        "passed": True, "requests": len(messages),
        "system_sections": len(body.get("system", [])),
        "tool_names": [tool.get("name") for tool in body.get("tools", [])],
        "refused_paths": refused,
    }
    (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--model", choices=MODELS)
    parser.add_argument("--timeout", type=float, default=40)
    parser.add_argument("--memory", action="store_true")
    parser.add_argument("--child", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    digest = hashlib.sha256()
    with binary.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    assert digest.hexdigest() == SHA256, "native artifact changed"
    args.output.mkdir(parents=True)
    reports = []
    for model in [args.model] if args.model else MODELS:
        report = capture(binary, args.output, model, args.timeout, args.memory, args.child)
        reports.append(report)
        print(f"PASS {model}: {report['system_sections']} system sections", flush=True)
    (args.output / "report.json").write_text(json.dumps({
        "version": "2.1.319", "sha256": SHA256, "cases": reports,
        "scope": "Unmodified native executable; isolated config; fake credentials; loopback scripted API; no model-quality claim.",
    }, indent=2) + "\n")


if __name__ == "__main__":
    main()
