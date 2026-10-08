#!/usr/bin/env python3
"""Run the real ComputerTool against a disposable local AppKit target.

No API calls or VerifiedComputerProfile entries are produced. A pass requires
events observed in the application, beyond successful input API returns.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import time


def source_identity(root):
    paths = [root / "Cargo.toml", root / "Cargo.lock", root / "crates/core/src/host/computer_control.rs"]
    for directory in ("crates/tools/computer-use", "crates/platforms/macos-computer-control", "crates/tool-api"):
        paths.extend((root / directory).rglob("*.rs"))
        paths.append(root / directory / "Cargo.toml")
    return {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(set(paths))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--manifest-path", type=Path)
    args = parser.parse_args()
    if not args.run:
        print(json.dumps({"status": "not_run", "reason": "pass --run for real desktop input"}))
        return 0
    if sys.platform != "darwin":
        parser.error("macOS is required")
    root = Path(__file__).resolve().parents[2]
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    bundle_id = f"com.lingxi.computer-use.acceptance.{os.getpid()}"
    bundle = output / "ComputerUseProbe.app" / "Contents"
    (bundle / "MacOS").mkdir(parents=True)
    (bundle / "Info.plist").write_bytes(plistlib.dumps({
        "CFBundleIdentifier": bundle_id, "CFBundleExecutable": "ComputerUseProbe",
        "CFBundleName": "LingXi Computer Use Acceptance", "CFBundlePackageType": "APPL",
        "NSHighResolutionCapable": True,
    }))
    swift_cache = output / "swift-cache"
    probe_source = output / "ComputerUseProbe.swift"
    shutil.copy2(root / "scripts/tests/fixtures/ComputerUseProbe.swift", probe_source)
    try:
        subprocess.run(["swiftc", "-module-cache-path", str(swift_cache),
                        str(probe_source), "-o",
                        str(bundle / "MacOS/ComputerUseProbe")], check=True)
    finally:
        shutil.rmtree(swift_cache, ignore_errors=True)
    # Prepare the binary before showing the target, so compilation never
    # leaves an acceptance window blocking ordinary work for several minutes.
    manifest = (args.manifest_path or root / "Cargo.toml").resolve()
    source_before = source_identity(manifest.parent)
    with (output / "build.log").open("w") as log:
        subprocess.run(["cargo", "build", "--offline", "--locked", "--manifest-path", str(manifest),
                        "-p", "tool-computer-use", "--example", "desktop_acceptance", "--message-format=json"],
                       stdout=log, stderr=log, check=True)
    executable = None
    for line in (output / "build.log").read_text().splitlines():
        try:
            artifact = json.loads(line)
        except ValueError:
            continue
        if artifact.get("reason") == "compiler-artifact" and artifact.get("target", {}).get("name") == "desktop_acceptance":
            executable = artifact.get("executable") or executable
    if executable is None:
        raise RuntimeError("Cargo did not report the desktop acceptance executable")
    # A different checkout can replace the shared Cargo target after building.
    # Hash and run an owned copy so the report identifies the bytes we execute.
    owned_executable = output / "desktop_acceptance"
    shutil.copy2(executable, owned_executable)
    identity = {"sources": source_before,
                "scope": "Selected Computer host sources and workspace manifests; not the full SDK or Agent dependency closure. Hash equality only compares the recorded snapshots.",
                "executable_sha256": hashlib.sha256(owned_executable.read_bytes()).hexdigest(),
                "probe_sha256": hashlib.sha256((bundle / "MacOS/ComputerUseProbe").read_bytes()).hexdigest(),
                "probe_source_sha256": hashlib.sha256(probe_source.read_bytes()).hexdigest(),
                "runner_source_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    (output / "source-identity.json").write_text(json.dumps(identity, indent=2))
    command = [str(owned_executable), "--run", str(output)]
    probe = None
    try:
        probe = subprocess.Popen([str(bundle / "MacOS/ComputerUseProbe"), str(output)],
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 15
        while not (output / "ready.json").exists() and time.monotonic() < deadline:
            time.sleep(0.1)
        ready = json.loads((output / "ready.json").read_text())
        if ready["pid"] != probe.pid:
            raise RuntimeError("probe process identity mismatch")
        completed = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        (output / "runner.log").write_text(completed.stdout + completed.stderr)
        if completed.returncode != 0:
            execution_path = output / "execution.json"
            execution = json.loads(execution_path.read_text()) if execution_path.exists() else {}
            raise RuntimeError(execution.get("error") or completed.stderr.strip() or "desktop execution failed")
        observed = json.loads((output / "observed.json").read_text())
        scroll = observed["scroll"]
        drag = observed["drag"]
        buttons = observed["buttons"]
        flags = observed["flags"]
        start_flags = len(json.loads((output / "pre-cancel.json").read_text())["flags"])
        cancel_flags = json.loads((output / "post-cancel.json").read_text())["flags"][start_flags:]
        expected_path = ready["window_path"]
        def near(event, point):
            return abs(event["x"] - point[0]) <= 2 and abs(event["y"] - point[1]) <= 2
        # Consume distinct events in order. The non-collinear middle waypoint
        # distinguishes full-path input from an endpoint-only drag.
        position = 0
        for event in drag:
            if position < len(expected_path) and event["kind"] in ("down", "move") and near(event, expected_path[position]):
                position += 1
        wheel = [e for e in scroll if not e["continuous"]]
        pixels = [e for e in scroll if e["continuous"]]
        checks = {
            "computer_source_identity": source_before == source_identity(manifest.parent),
            "target_window_geometry": ready["window_frame"] == observed["window_frame"],
            "execution": completed.returncode == 0,
            "retina": ready["scale"] > 1,
            "click": observed["clicks"] >= 1,
            "text": observed["text"] == "Computer Use desktop acceptance",
            "wheel_and_pixel_scroll": bool(wheel) and bool(pixels)
                and sum(e["ticks_y"] for e in wheel) == -3 and all(e["ticks_x"] == 0 for e in wheel)
                and sum(e["points_y"] for e in pixels) == -120 and all(e["points_x"] == 0 for e in pixels),
            "drag": position == len(expected_path) and any(e["kind"] == "up" and near(e, expected_path[-1]) for e in drag)
                and sum(e["kind"] == "up" for e in drag) >= 2,
            "side_buttons": all(any(e["button"] == b and e["kind"] == "down" for e in buttons)
                                and any(e["button"] == b and e["kind"] == "up" for e in buttons) for b in (3, 4)),
            "held_key_and_cancel_release": sum(e["shift"] for e in flags) >= 2 and bool(flags) and not flags[-1]["shift"]
                and len(cancel_flags) >= 2 and cancel_flags[0]["shift"] and not cancel_flags[-1]["shift"],
            "captures": (output / "before.png").exists() and (output / "after.png").exists(),
        }
        report = {"status": "passed" if all(checks.values()) else "failed", "checks": checks,
                  "bundle_id": bundle_id, "scale": ready["scale"],
                  "source_identity": "source-identity.json",
                  "boundary": "Real ComputerTool and macOS input/capture. No Provider API, Agent dispatcher/hooks/journal or second display acceptance."}
        (output / "report.json").write_text(json.dumps(report, indent=2))
        print(json.dumps(report, indent=2))
        return 0 if all(checks.values()) else 1
    except (OSError, ValueError, RuntimeError) as error:
        report = {"status": "failed", "reason": str(error), "scope": "desktop acceptance runner"}
        (output / "report.json").write_text(json.dumps(report, indent=2))
        print(json.dumps(report))
        return 1
    finally:
        if probe is not None:
            probe.terminate()
            try:
                probe.wait(timeout=3)
            except subprocess.TimeoutExpired:
                probe.kill()
                probe.wait()


if __name__ == "__main__":
    sys.exit(main())
