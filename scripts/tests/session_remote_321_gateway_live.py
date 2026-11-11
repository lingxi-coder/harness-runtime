#!/usr/bin/env python3
"""Run disk-backed Remote Control handlers with an owned loopback RTDB emulator."""
import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

from session_remote_321_live import digest, dump, free_port, stop


def cleanup_owned_metadata_workers(ledger, root):
    """Reap exact recorded metadata children, never their inherited process group."""
    results = []
    for worker in ledger.get("metadataWorkers", []):
        pid = worker["pid"]
        assert isinstance(pid, int) and pid > 1 and pid != ledger["runnerPid"]
        assert worker["purpose"] == "session-catalog-metadata"
        assert worker["argv"] == ["--session-catalog-worker-json"]
        assert worker["parentPid"] == ledger["runnerPid"]
        assert worker["pgid"] == ledger["runnerPid"]
        assert worker["workspace"] == str(root / "workspace")
        assert worker["execPath"] in (ledger["binary"], str(root / "resources/bin/bridge-server"))
        result = {"pid": pid, "purpose": worker["purpose"], "signals": [], "childCloseRecorded": bool(worker.get("closed"))}

        def snapshot():
            if worker.get("retired") and worker.get("closed"):
                # ChildProcess close proves the original child was reaped. A
                # subsequently reused numeric PID must never be signaled.
                return False, True
            completed = subprocess.run(["/bin/ps", "-ww", "-p", str(pid), "-o", "pid=,ppid=,pgid=,lstart=,command="], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=2)
            rows = [line.split() for line in completed.stdout.splitlines() if line.strip()]
            if not rows:
                worker["retired"] = True
                return False, True
            parts = rows[0]
            if len(parts) < 9 or not all(value.isdigit() for value in parts[:3]):
                return True, False
            birth = " ".join(parts[3:8])
            if worker.get("birth") and birth != worker["birth"]:
                worker["retired"] = True
                result["pidReused"] = True
                return False, True
            command = " ".join(parts[8:])
            owned = bool(worker.get("birth") == birth and int(parts[0]) == pid and int(parts[2]) == worker["pgid"] and command.startswith(worker["execPath"] + " ") and command.endswith(" --session-catalog-worker-json"))
            return True, owned

        for sig, budget in [(signal.SIGTERM, 1.0), (signal.SIGKILL, 2.0)]:
            alive, proved = snapshot()
            if not alive:
                break
            if not proved:
                result["unverified"] = "live metadata child lacks recorded PID/birth/parent/purpose identity"
                break
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                worker["retired"] = True
                break
            result["signals"].append(sig.name)
            deadline = time.monotonic() + budget
            while time.monotonic() < deadline:
                alive, proved = snapshot()
                if not alive or not proved:
                    break
                time.sleep(0.05)
        result["exited"] = bool(worker.get("retired"))
        results.append(result)
    return results


def cleanup_owned_bridges(ledger_path):
    """Signal only fresh groups proven by this run's immutable spawn ledger."""
    if not ledger_path.exists():
        return {"verified": True, "groups": [], "reason": "no bridge launch recorded"}
    ledger = json.loads(ledger_path.read_text())
    root = ledger_path.parent
    expected_workspace = str(root / "workspace")
    groups = ledger["groups"]

    def snapshot(group):
        pgid = group["pgid"]
        assert pgid == group["pid"] and isinstance(pgid, int) and pgid > 1
        assert group["workspace"] == expected_workspace
        assert Path(group["bridgeDir"]).is_relative_to(root / "bridges")
        if group.get("retired"):
            return [], False
        try:
            os.killpg(pgid, 0)
        except ProcessLookupError:
            group["retired"] = True
            return [], False
        except PermissionError:
            return [], False
        try:
            completed = subprocess.run(["/bin/ps", "-ww", "-g", str(pgid), "-o", "pid=,ppid=,pgid=,lstart=,command="], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=2)
        except (OSError, subprocess.TimeoutExpired):
            return [], False
        rows = []
        for line in completed.stdout.splitlines():
            parts = line.split()
            if len(parts) < 9 or not all(value.isdigit() for value in parts[:3]):
                continue
            pid, ppid, actual_group = map(int, parts[:3])
            if actual_group != pgid:
                continue
            rows.append({"pid": pid, "ppid": ppid, "birth": " ".join(parts[3:8]), "command": " ".join(parts[8:])})
        leader = next((row for row in rows if row["pid"] == pgid), None)
        known = {member["pid"]: member for member in group.get("members", [])}
        leader_owned = bool(leader and leader["command"].startswith((group["execPath"] + " ", ledger["binary"] + " ")) and group["bridgeDir"] in leader["command"] and expected_workspace in leader["command"] and (pgid not in known or known[pgid]["birth"] == leader["birth"]))
        # An orphan is eligible only if the same PID + birth identity was
        # observed in this owned group while its lineage was still provable.
        known_alive = any(row["pid"] in known and known[row["pid"]]["birth"] == row["birth"] for row in rows if row["pid"] != pgid)
        return rows, leader_owned or known_alive

    results = []
    for group in groups:
        result = {"pid": group["pid"], "pgid": group["pgid"], "signals": []}
        for sig, budget in [(signal.SIGTERM, 1.0), (signal.SIGKILL, 2.0)]:
            rows, proved = snapshot(group)
            if group.get("retired"):
                break
            if not proved:
                result["unverified"] = "live group lacks current owned leader or recorded descendant identity"
                break
            try:
                os.killpg(group["pgid"], sig)
            except ProcessLookupError:
                group["retired"] = True
                break
            result["signals"].append(sig.name)
            deadline = time.monotonic() + budget
            while time.monotonic() < deadline:
                try:
                    os.killpg(group["pgid"], 0)
                except ProcessLookupError:
                    group["retired"] = True
                    break
                time.sleep(0.05)
        result["exited"] = bool(group.get("retired"))
        results.append(result)
    metadata_workers = cleanup_owned_metadata_workers(ledger, root)
    temporary_ledger = ledger_path.with_name(ledger_path.name + ".pending")
    temporary_ledger.write_text(json.dumps(ledger, indent=2) + "\n")
    temporary_ledger.replace(ledger_path)
    report = {"verified": all(group["exited"] for group in results) and all(worker["exited"] for worker in metadata_workers), "groups": results, "metadataWorkers": metadata_workers}
    dump(root / "owned-bridge-cleanup.json", report)
    return report


def stop_node(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=25)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=Path, default=Path(__file__).resolve().parents[3] / "lingxi-app")
    parser.add_argument("--bridge-server", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--emulator-jar", type=Path, default=Path("/Users/luolingfeng/.cache/firebase/emulators/firebase-database-emulator-v4.11.2.jar"))
    args = parser.parse_args()
    root = args.output.resolve()
    root.mkdir(mode=0o700)
    java, node = shutil.which("java"), shutil.which("node")
    assert java and node
    assert args.bridge_server.is_file()
    port = free_port()
    origin = f"http://127.0.0.1:{port}"
    with (root / "emulator.log").open("w") as logfile:
        emulator = subprocess.Popen([java, "-Xmx256m", "-jar", str(args.emulator_jar), "--host", "127.0.0.1", "--port", str(port)], cwd=root, stdout=logfile, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            deadline = time.monotonic() + 25
            while True:
                assert emulator.poll() is None, "emulator exited"
                try:
                    urllib.request.urlopen(f"{origin}/.json?ns=demo-session-321-health", timeout=1).close()
                    break
                except urllib.error.HTTPError:
                    break
                except (urllib.error.URLError, TimeoutError):
                    assert time.monotonic() < deadline, "emulator startup timed out"
                    time.sleep(0.15)
            command = [node, "--import", str(args.app / "packages/bridge-client/node_modules/tsx/dist/loader.mjs"), str(Path(__file__).with_suffix(".mjs")), "--app", str(args.app), "--bridge-server", str(args.bridge_server.resolve()), "--output", str(root / "gateway"), "--emulator-origin", origin]
            dump(root / "invocation.json", {"command": command, "emulatorSha256": digest(args.emulator_jar), "bridgeServerSha256": digest(args.bridge_server)})
            # No HOME override or credential variables; the child uses explicit
            # owned configuration/global paths and an empty injected registry.
            env = {"PATH": os.environ.get("PATH", os.defpath), "LANG": "en_US.UTF-8", "TMPDIR": str(root), "LINGXI_CONFIG_DIR": str(root / "gateway/config"), "LINGXI_REMOTE_TEST_PYTHON": sys.executable}
            process = subprocess.Popen(command, cwd=args.app / "apps/electron", env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
            try:
                stdout, _ = process.communicate(timeout=240)
            except subprocess.TimeoutExpired:
                stop_node(process)
                stdout, _ = process.communicate(timeout=5)
                (root / "runner.log").write_text(stdout)
                raise RuntimeError("remote gateway validation timed out")
            finally:
                stop_node(process)
                cleanup = cleanup_owned_bridges(root / "gateway/owned-bridge-processes.json")
                dump(root / "supervisor-cleanup.json", cleanup)
            (root / "runner.log").write_text(stdout)
            assert cleanup["verified"], "owned bridge cleanup could not be verified; no unrelated group was signaled"
            assert process.returncode == 0, stdout[-4500:]
            report = json.loads((root / "gateway/report.json").read_text())
            assert report["passed"]
            print(f"PASS production disk-backed remote gateway: {len(report['checks'])} checks")
        finally:
            stop(emulator)


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--cleanup-ledger":
        print(json.dumps(cleanup_owned_bridges(Path(sys.argv[2]))))
    else:
        main()
