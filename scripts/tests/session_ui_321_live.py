#!/usr/bin/env python3
"""Owned real-PTY session picker probes with an exclusively loopback provider.

Never overrides HOME or changes real sessions. Raw terminal output, keyboard
events, provider requests and on-disk transcripts remain under --output.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import termios
import time
import uuid
from pathlib import Path

import pexpect

from session_cli_321_live import FAKE_KEY, MODEL, NATIVE_SHA, Loopback, dump, isolated_env, profile, remove_lingxi_account, setup_lingxi_account, text_content

IDS = {"Alpha": "11111111-1111-4111-8111-111111111111", "Beta": "22222222-2222-4222-8222-222222222222"}
ANSI = re.compile(r"\x1b\][^\x07]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]|\x1b[()][A-Z0-9]|\x1b[=>]")


def seed(root: Path, engine: str, sessions=True):
    profile(root, engine)
    work = root / "work"
    config = root / "config"
    dotfile = config / (".claude.json" if engine == "native" else ".lingxi.json")
    dump(dotfile, {"hasCompletedOnboarding": True, "customApiKeyResponses": {"approved": [FAKE_KEY[-20:]], "rejected": []}, "projects": {str(work): {"hasTrustDialogAccepted": True, "hasCompletedProjectOnboarding": True, "projectOnboardingSeenCount": 1}}})
    slug = re.sub(r"[^a-zA-Z0-9]", "-", str(work))
    project = config / "projects" / slug
    if sessions:
        project.mkdir(parents=True, exist_ok=True)
    for name, sid in (IDS.items() if sessions else []):
        user, assistant = str(uuid.uuid4()), str(uuid.uuid4())
        header = {"isSidechain": False, "userType": "external", "cwd": str(work), "sessionId": sid, "version": "2.1.321", "gitBranch": ""}
        rows = [{"parentUuid": None, **header, "type": "user", "message": {"role": "user", "content": f"SESSION_UI_{name.upper()}_REQUEST"}, "uuid": user, "timestamp": "2026-11-09T12:00:00.000Z"}, {"parentUuid": user, **header, "type": "assistant", "message": {"id": f"msg_ui_{name}", "type": "message", "role": "assistant", "model": MODEL, "content": [{"type": "text", "text": f"SESSION_UI_{name.upper()}_ANSWER"}], "stop_reason": "end_turn", "stop_sequence": None, "usage": {"input_tokens": 1, "output_tokens": 1}}, "uuid": assistant, "timestamp": "2026-11-09T12:00:01.000Z"}, {"type": "custom-title", "sessionId": sid, "customTitle": f"Session321 UI {name}"}]
        if engine == "lingxi":
            # Explicit product authority boundary, not a native transcript row.
            # The picker must never weaken this header to accept a QA fixture.
            rows.insert(0, {"type": "fixed-session-mode", "sessionId": sid, "sessionMode": "code", "instructionEpoch": 0})
        target = project / f"{sid}.jsonl"
        target.write_text("".join(json.dumps(row, separators=(",", ":")) + "\n" for row in rows))
        os.utime(target, (1794225600 + (name == "Beta"),) * 2)


class Terminal:
    def __init__(self, binary: Path, engine: str, root: Path, api: Loopback, cols: int, entry="--resume"):
        self.root = root
        self.events = []
        self.cols = cols
        self.output = (root / "terminal.raw").open("wb")
        env = isolated_env(engine, root / "config", root / "tmp", api.base_url)
        argv = ["--bare", "--model", MODEL, "--system-prompt", "You are a deterministic local session fixture.", "--setting-sources", "", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}', "--tools", "", entry]
        dump(root / "invocation.json", {"binary": str(binary), "argv": argv, "cwd": str(root / "work"), "environment": env, "dimensions": [32, cols], "overrides_HOME": False, "inherits_credentials": False})
        self.child = pexpect.spawn(str(binary), argv, cwd=str(root / "work"), env=env, dimensions=(32, cols), timeout=15)
        self.child.logfile_read = self.output
        self.child.delaybeforesend = 0.15

    def read(self, seconds=2):
        end = time.monotonic() + seconds
        data = b""
        while time.monotonic() < end:
            try:
                data += self.child.read_nonblocking(65536, timeout=min(0.25, max(0.01, end-time.monotonic())))
            except pexpect.TIMEOUT:
                continue
            except pexpect.EOF:
                break
        return data

    def wait_for(self, marker: bytes, timeout=30):
        self.output.flush()
        observed = (self.root / "terminal.raw").read_bytes()
        end = time.monotonic() + timeout
        while marker not in observed and self.child.isalive() and time.monotonic() < end:
            observed += self.read(min(0.5, max(0.01, end - time.monotonic())))
        if marker not in observed:
            raise RuntimeError(f"Terminal did not display {marker.decode()} within {timeout}s")

    def send(self, data: bytes, label: str):
        self.events.append({"key": label, "bytes_hex": data.hex(), "monotonic": time.monotonic()})
        self.child.send(data)

    def type(self, value: str, label: str):
        # A single unbracketed multi-character write is paste-like to native
        # Ink's key parser. Exercise real typing as separate key deliveries.
        self.events.append({"key": label, "text": value, "delivery": "individual UTF-8 keys", "monotonic": time.monotonic()})
        for char in value:
            self.child.send(char.encode())
            # A real terminal drains output while typing. Without this read,
            # repaint traffic can fill the PTY queue and make later spaced
            # keys arrive together after rendering unblocks, mimicking paste.
            self.read(0.05)

    def snapshot(self, name: str):
        self.output.flush()
        renderer = r"""
const {Terminal}=require(process.argv[1]);
const fs=require('node:fs');
const t=new Terminal({cols:Number(process.argv[2]),rows:32,allowProposedApi:true,scrollback:5000});
t.write(fs.readFileSync(0),()=>{
 const lines=[]; for(let i=0;i<t.buffer.active.length;i++) lines.push(t.buffer.active.getLine(i)?.translateToString(true)||'');
 process.stdout.write(JSON.stringify({cols:t.cols,rows:t.rows,lines})); t.dispose();
});
"""
        module = Path(__file__).resolve().parents[3] / "lingxi-app/apps/electron/node_modules/@xterm/xterm/lib/xterm.js"
        raw = (self.root / "terminal.raw").read_bytes()
        rendered = subprocess.run(["node", "-e", renderer, str(module), str(self.cols)], input=raw, capture_output=True, check=True)
        frame = json.loads(rendered.stdout)
        try:
            attributes = termios.tcgetattr(self.child.child_fd)
            frame["terminal_mode"] = {"canonical": bool(attributes[3] & termios.ICANON), "echo": bool(attributes[3] & termios.ECHO), "cr_to_nl": bool(attributes[0] & termios.ICRNL), "signals": bool(attributes[3] & termios.ISIG)}
        except (OSError, ValueError, termios.error) as error:
            frame["terminal_mode_unavailable"] = str(error)
        frame.update(source_raw_sha256=hashlib.sha256(raw).hexdigest(), renderer="Installed @xterm/xterm; terminal buffer reconstructed from actual captured PTY bytes")
        dump(self.root / f"{name}.frame.json", frame)
        text = "\n".join(frame["lines"])
        (self.root / f"{name}.frame.txt").write_text(text)
        return text

    def close(self):
        self.read(0.3)
        alive = self.child.isalive()
        if alive:
            self.child.terminate(force=True)
        self.child.close()
        self.output.close()
        dump(self.root / "events.json", self.events)
        raw = (self.root / "terminal.raw").read_bytes()
        (self.root / "terminal.plain.txt").write_text(ANSI.sub("", raw.decode(errors="replace")))
        return {"exit_code": self.child.exitstatus, "signal": self.child.signalstatus, "forced_cleanup": alive, "owned_pid": self.child.pid, "reaped": not self.child.isalive(), "raw_sha256": hashlib.sha256(raw).hexdigest()}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--engine", choices=("native", "lingxi"), default="native")
    parser.add_argument("--binary", type=Path, default=Path.home()/".local/share/claude/versions/2.1.321")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cols", type=int, default=100)
    parser.add_argument("--probe", action="store_true")
    parser.add_argument("--case", choices=("search", "search-down", "search-up", "cancel", "search-cancel", "commit-cancel", "ctrl-c", "down", "empty-continue", "empty-resume"), default="search")
    parser.add_argument("--setup-fake-account", action="store_true")
    parser.add_argument("--query", default="Alpha")
    args = parser.parse_args()
    binary_hash = hashlib.sha256(args.binary.read_bytes()).hexdigest()
    if args.engine == "native" and binary_hash != NATIVE_SHA:
        raise SystemExit("Native binary differs from the pinned Claude Code 2.1.321 reference")
    args.output.mkdir(parents=True, exist_ok=False)
    seeded = args.case not in ("empty-continue", "empty-resume")
    seed(args.output, args.engine, seeded)
    source_ids = IDS if seeded else {}
    source_hashes = {name: hashlib.sha256(next((args.output / "config/projects").rglob(f"{sid}.jsonl")).read_bytes()).hexdigest() for name, sid in source_ids.items()}
    with Loopback(args.output / "api", args.output / "work") as api:
        account = None
        if args.setup_fake_account:
            if args.engine != "lingxi":
                raise SystemExit("A fixture account is only applicable to LingXi")
            account = setup_lingxi_account(args.binary, args.output, api)
            if not account["passed"]:
                dump(args.output / "result.json", {"passed": False, "phase": "synthetic account setup", "details": account})
                raise SystemExit(1)
        term = None
        failure = None
        diagnostic_failure = None
        picker = ""
        committed = ""
        cleared = ""
        after_ctrl_c = ""
        unfocused = ""
        empty_after_enter = False
        case = "cancel" if args.probe else args.case
        try:
            term = Terminal(args.binary, args.engine, args.output, api, args.cols, "--continue" if case == "empty-continue" else "--resume")
            initial = term.read(3)
            (args.output / "initial.raw").write_bytes(initial)
            picker = term.snapshot("picker")
            expected = "Session321 UI Alpha" if seeded else "No conversation found to continue" if case == "empty-continue" else "No conversations found in this project."
            ready_deadline = time.monotonic() + 30
            while expected not in picker and term.child.isalive() and time.monotonic() < ready_deadline:
                term.read(0.5)
                picker = term.snapshot("picker")
            if expected not in picker:
                raise RuntimeError(f"Initial session UI did not display {expected!r} within readiness budget")
            term.output.flush()
            (args.output / "initial.raw").write_bytes((args.output / "terminal.raw").read_bytes())
            if case in ("search", "search-down", "search-up", "search-cancel", "commit-cancel"):
                term.type(args.query, "search query")
                term.read(0.7)
                term.snapshot("search")
            if case == "empty-continue":
                # Native print mode starts fresh, but the actual interactive
                # empty-continue path reports this error and exits with 1.
                term.read(0.5)
            elif case == "empty-resume":
                if term.child.isalive():
                    term.send(b"\r", "Enter on empty picker")
                    term.read(0.7)
                    empty_after_enter = term.child.isalive()
                    term.snapshot("empty-enter")
                    if empty_after_enter:
                        term.send(b"\x1b", "cancel empty picker")
                        term.read(1)
            elif case in ("search", "search-down", "search-up", "down"):
                if case in ("search-down", "search-up", "down"):
                    term.send(b"\x1b[A" if case == "search-up" else b"\x1b[B", "navigate sessions")
                    term.read(0.5)
                    if case in ("search-down", "search-up"):
                        committed = term.snapshot("search-arrow")
                term.send(b"\r", "select Alpha")
                term.read(0.7)
                if case == "search":
                    committed = term.snapshot("search-committed")
                    term.send(b"\r", "resume selected Alpha")
                term.wait_for(b"SESSION_UI_ALPHA_ANSWER")
                term.snapshot("selected")
                term.type("SESSION_UI_RESUMED_TURN", "type resumed turn")
                term.send(b"\r", "submit resumed turn")
                term.wait_for(b"MOCK_OK")
                term.snapshot("after-turn")
                if term.child.isalive():
                    term.type("/exit", "type exit command")
                    term.send(b"\r", "submit exit command")
                    term.read(2)
            elif case == "commit-cancel":
                term.send(b"\r", "commit search")
                term.read(0.7)
                committed = term.snapshot("search-committed")
                term.send(b"\x1b", "cancel committed search")
                term.read(1)
            elif case == "ctrl-c":
                term.send(b"\x03", "Ctrl+C")
                term.read(1)
                after_ctrl_c = term.snapshot("after-ctrl-c")
                if term.child.isalive():
                    term.send(b"\x1b", "cancel after Ctrl+C observation")
                    term.read(1)
            else:
                term.send(b"\x1b", "cancel")
                term.read(1)
                if case == "search-cancel":
                    cleared = term.snapshot("search-cleared")
                    term.send(b"\x1b", "cancel cleared picker")
                    term.read(1)
                    unfocused = term.snapshot("search-unfocused")
                    if term.child.isalive():
                        term.send(b"\x1b", "cancel list picker")
                        term.read(1)
            term.snapshot("final")
        except Exception as error:
            failure = f"{type(error).__name__}: {error}"
            if term:
                try:
                    term.snapshot("failure")
                except Exception as snapshot_error:
                    diagnostic_failure = f"{type(snapshot_error).__name__}: {snapshot_error}"
        finally:
            cleanup = None
            try:
                result = term.close() if term else {"reaped": True, "exit_code": None, "forced_cleanup": False}
            finally:
                if account and account.get("account_id"):
                    cleanup = remove_lingxi_account(args.binary, args.output, api, account["account_id"])
            if cleanup:
                result["synthetic_account_removed"] = cleanup["passed"]
        final_hashes = {name: hashlib.sha256(next((args.output / "config/projects").rglob(f"{sid}.jsonl")).read_bytes()).hexdigest() for name, sid in source_ids.items()}
        assertions = {"owned_cli_process_reaped": result["reaped"]}
        if seeded:
            assertions["picker_lists_both_titles"] = "Session321 UI Alpha" in picker and "Session321 UI Beta" in picker
        if account:
            assertions["synthetic_account_removed"] = result.get("synthetic_account_removed", False)
        if case in ("search", "search-down", "search-up", "down"):
            bodies = json.dumps([request["body"].get("messages", []) for request in api.requests])
            rows = [json.loads(line) for line in next((args.output / "config/projects").rglob(f"{IDS['Alpha']}.jsonl")).read_text().splitlines()]
            persisted = any(row.get("sessionId") == IDS["Alpha"] and "SESSION_UI_RESUMED_TURN" in text_content(row.get("message", {})) for row in rows)
            assertions.update(restored_selected_history="SESSION_UI_ALPHA_REQUEST" in bodies and "SESSION_UI_ALPHA_ANSWER" in bodies, excludes_other_history="SESSION_UI_BETA_REQUEST" not in bodies, resumed_turn_sent="SESSION_UI_RESUMED_TURN" in bodies, retained_original_session=persisted, unrelated_session_unchanged=source_hashes["Beta"] == final_hashes["Beta"], graceful_exit=result["exit_code"] == 0 and not result["forced_cleanup"])
            if case == "search":
                assertions["first_enter_keeps_picker_open"] = "Resume session" in committed and "SESSION_UI_ALPHA_ANSWER" not in committed
        if case == "cancel":
            assertions.update(cancel_has_no_provider_request=not api.requests, cancel_preserves_sources=source_hashes == final_hashes, native_cancel_status=result["exit_code"] == 1 and not result["forced_cleanup"])
        if case == "search-cancel":
            assertions.update(first_escape_clears_filter="Resume session" in cleared and all(f"Session321 UI {name}" in cleared for name in IDS), second_escape_keeps_selected_session=bool(re.search(r"[❯>] Session321 UI Alpha", unfocused)), third_escape_cancels=result["exit_code"] == 1 and not result["forced_cleanup"], cancel_preserves_sources=source_hashes == final_hashes)
        if case == "commit-cancel":
            assertions.update(commit_retains_picker="Resume session" in committed, escape_cancels_committed_search=result["exit_code"] == 1 and not result["forced_cleanup"], cancel_preserves_sources=source_hashes == final_hashes)
        if case == "ctrl-c":
            assertions.update(ctrl_c_keeps_picker="Resume session" in after_ctrl_c, escape_then_cancels=result["exit_code"] == 1 and not result["forced_cleanup"], cancel_preserves_sources=source_hashes == final_hashes)
        if case == "empty-continue":
            transcripts = list((args.output / "config/projects").rglob("*.jsonl"))
            assertions.update(exact_empty_continue_error=picker.strip() == "No conversation found to continue", no_provider_request=not api.requests, no_new_transcript=not transcripts, expected_error_exit=result["exit_code"] == 1 and not result["forced_cleanup"])
        if case == "empty-resume":
            assertions.update(empty_picker_message="No conversations found in this project." in picker, empty_enter_keeps_picker=empty_after_enter, no_provider_request=not api.requests, no_new_transcript=not list((args.output / "config/projects").rglob("*.jsonl")), expected_cancel_status=result["exit_code"] == 1 and not result["forced_cleanup"])
        result.update(engine=args.engine, binary_sha256=binary_hash, case=case, columns=args.cols, failure=failure, diagnostic_failure=diagnostic_failure, assertions=assertions, passed=failure is None and all(assertions.values()), requests=len(api.requests), refused_routes=api.refused, scope="Actual PTY process and raw keyboard input with synthetic local sessions and loopback provider; external routes refused by local proxy")
        dump(args.output / "result.json", result)
        print(json.dumps(result))
        raise SystemExit(0 if result["passed"] else 1)


if __name__ == "__main__":
    main()
