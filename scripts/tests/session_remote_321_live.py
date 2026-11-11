#!/usr/bin/env python3
"""Validate native V5 availability and LingXi's real loopback RTDB transport.

The reference executable and cached emulator are never modified. The task owns
all subprocesses and all output/configuration directories. No cloud namespace
or real credentials are used.
"""

import argparse
import hashlib
import json
import mmap
import os
import re
import shutil
import signal
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request
from http.server import ThreadingHTTPServer
from pathlib import Path

from compact_live_oracle import isolated_env
from live_requests_287_oracle import make_handler

NATIVE_SHA = "abc9feeeca8091d51b623291e044cf0528c2f6b74e0c1d6ef671d52c73f7cd3c"


def dump(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=True) + "\n")


def digest(path):
    hasher = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(block)
    return hasher.hexdigest()


def stop(process):
    if process.poll() is not None:
        return
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=5)


def native_receipts(binary):
    receipts = []
    with binary.open("rb") as source:
        data = mmap.mmap(source.fileno(), 0, access=mmap.ACCESS_READ)
        start = data.find(b"function bDs(e=aCo){", 277000000)
        assert start >= 0
        for name, first, following in [
            ("environment-pinned backend selection", b"function bDs(", b"function wDs("),
            ("public-build V5 backend factory", b"function aCo(", b"export{bDs,"),
            ("default backend selection", b"function Wz(", b"\nexport{Wz"),
        ]:
            at = data.find(first, start)
            end = data.find(following, at + len(first))
            assert start <= at < end and end - at < 2000
            payload = data[at:end]
            receipts.append({
                "name": name, "start": at, "end": end,
                "sha256": hashlib.sha256(payload).hexdigest(),
            })
            if name == "public-build V5 backend factory":
                # Fail closed if this precise public-build limitation changes.
                assert hashlib.sha256(payload).hexdigest() == "1ad043a764e7d9c0c51370e297cc472182523f80a0beb132c5f8bea9b7d6394f"
        data.close()
    return receipts


def native_probe(binary, root, timeout):
    root.mkdir()
    for name in ("config", "work", "tmp"):
        (root / name).mkdir()
    dump(root / "config" / ".claude.json", {"hasCompletedOnboarding": True})
    requests, refused = [], []
    server = ThreadingHTTPServer(
        ("127.0.0.1", 0), make_handler(root, "prompt-body", requests, refused)
    )
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    env = isolated_env(root, f"http://127.0.0.1:{server.server_port}")
    env.update({
        "CLAUDE_CODE_HOVER_REST": "1",
        "CLAUDE_CODE_DISABLE_FEATURE_FLAGS": "1",
        "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1",
    })
    command = [
        str(binary), "--bare", "-p", "SESSION_REMOTE_321_STORAGE_PROBE",
        "--model", "claude-sonnet-4-6", "--system-prompt", "REMOTE_STORAGE_PROBE",
        "--output-format", "stream-json", "--verbose", "--setting-sources", "",
        "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
        "--tools", "", "--debug-file", str(root / "debug.log"),
    ]
    process = subprocess.Popen(
        command, cwd=root / "work", env=env, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, text=True, start_new_session=True,
    )
    try:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            stop(process)
            stdout, stderr = process.communicate(timeout=3)
            raise RuntimeError("native remote-storage bootstrap probe timed out")
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
    (root / "stdout.jsonl").write_text(stdout)
    (root / "stderr.txt").write_text(stderr)
    events = [json.loads(line) for line in stdout.splitlines() if line.strip()]
    result = next((event for event in reversed(events) if event.get("type") == "result"), None)
    assert process.returncode == 0 and result and not result.get("is_error"), stderr[-2000:]
    transcripts = sorted((root / "config" / "projects").rglob("*.jsonl"))
    assert transcripts, "the forced V5 pin must still use local transcript persistence"
    assert any("WIRE_DONE" in path.read_text() for path in transcripts)
    report = {
        "passed": True, "exitCode": process.returncode,
        "hoverRestEnvironment": "1", "apiRequests": len(requests),
        "refusedPaths": refused,
        "persistedTranscripts": [
            {"path": str(path.relative_to(root)), "sha256": digest(path), "bytes": path.stat().st_size}
            for path in transcripts
        ],
        "observation": "Actual pinned CLI completes a turn and writes local transcripts even with HOVER_REST explicitly enabled.",
        "limitation": "The exported default V5 factory returns no backend in this public build. No native remote-storage service was exercised.",
    }
    dump(root / "report.json", report)
    return report


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def emulator_probe(app, jar, root, timeout):
    root.mkdir()
    java = shutil.which("java")
    node = shutil.which("node")
    assert java and node, "existing Java and Node runtimes are required"
    port = free_port()
    command = [java, "-Xmx256m", "-jar", str(jar), "--host", "127.0.0.1", "--port", str(port)]
    emulator_log = (root / "emulator.log").open("w")
    emulator = subprocess.Popen(
        command, cwd=root, stdout=emulator_log, stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    try:
        deadline = time.monotonic() + 25
        while True:
            assert emulator.poll() is None, "emulator exited during startup"
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{port}/.json?ns=demo-session-321-health", timeout=1).close()
                break
            except urllib.error.HTTPError:
                break
            except (urllib.error.URLError, TimeoutError):
                assert time.monotonic() < deadline, "emulator did not become ready"
                time.sleep(0.15)
        env = {
            "PATH": os.environ.get("PATH", os.defpath),
            "FIREBASE_DATABASE_EMULATOR_HOST": f"127.0.0.1:{port}",
            "LANG": "en_US.UTF-8",
        }
        test_command = [node, "--import", "tsx", "--test", "--test-reporter=tap", "test/remote-rtdb-emulator.test.ts"]
        completed = subprocess.run(
            test_command, cwd=app / "packages/bridge-client", env=env,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=timeout,
        )
        (root / "tests.tap").write_text(completed.stdout)
        counters = {name: int(count) for name, count in re.findall(r"^# (tests|pass|fail|skipped) (\d+)$", completed.stdout, re.MULTILINE)}
        assert completed.returncode == 0, completed.stdout[-5000:]
        assert counters.get("tests") == 5 and counters.get("pass") == 5 and counters.get("skipped") == 0, counters
        report = {
            "passed": True, "exitCode": completed.returncode,
            "emulatorVersion": "4.11.2", "emulatorSha256": digest(jar),
            "endpoint": f"http://127.0.0.1:{port}",
            "namespace": "lingxi-5e4b2-default-rtdb (isolated owned emulator only)",
            "tests": counters,
            "coverage": [
                "Production REST client: JSON values, deletes, sparse arrays and timestamps",
                "Production SSE listener: initial snapshot, updates and explicit reconnect",
                "Production encrypted tunnel and RPC: both directions, 300KB payload, close cleanup",
                "Cross-user authorization refusal and bounded token refresh",
                "Live read-rule revocation: cancel and no reconnect",
            ],
            "limitation": "Actual HTTP/SSE server transport; no production cloud namespace, deployed host, peer WebRTC, engine session persistence, or Claude StorageV5 parity is claimed.",
        }
        dump(root / "report.json", report)
        return report
    finally:
        stop(emulator)
        emulator_log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", type=Path, default=Path.home() / ".local/share/claude/versions/2.1.321")
    parser.add_argument("--app", type=Path, default=Path(__file__).resolve().parents[3] / "lingxi-app")
    parser.add_argument("--emulator-jar", type=Path, default=Path.home() / ".cache/firebase/emulators/firebase-database-emulator-v4.11.2.jar")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=90)
    args = parser.parse_args()
    native = args.native.resolve(strict=True)
    assert digest(native) == NATIVE_SHA, "native artifact changed"
    args.output.mkdir(parents=True, exist_ok=False)
    receipts = native_receipts(native)
    dump(args.output / "native-source-receipts.json", receipts)
    native_result = native_probe(native, args.output / "native-v5-bootstrap", args.timeout)
    print("PASS actual native V5 availability probe", flush=True)
    transport = emulator_probe(args.app.resolve(strict=True), args.emulator_jar.resolve(strict=True), args.output / "rtdb-transport", args.timeout)
    print("PASS production LingXi HTTP/SSE transport: 5 tests", flush=True)
    dump(args.output / "report.json", {
        "referenceVersion": "2.1.321", "referenceSha256": NATIVE_SHA,
        "sourceByteRanges": receipts, "native": native_result, "lingxi": transport,
        "cloudStorageParity": "unavailable: public native backend factory returns undefined and LingXi owns local transcripts behind Remote Control",
    })


if __name__ == "__main__":
    main()
