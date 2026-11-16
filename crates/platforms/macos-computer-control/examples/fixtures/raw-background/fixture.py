#!/usr/bin/env python3
"""Build and manage only the exact two temporary raw-background fixtures."""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import plistlib
import signal
import subprocess
import time

HERE = Path(__file__).resolve().parent
BUNDLES = {"target": "org.lingxi.raw-probe.target", "shadow": "org.lingxi.raw-probe.shadow"}


class BsdInfo(ctypes.Structure):
    # Darwin sys/proc_info.h: PROC_PIDTBSDINFO, exact start timestamp (not ps text).
    _fields_ = [("prefix", ctypes.c_uint32 * 12), ("comm", ctypes.c_char * 16),
                ("name", ctypes.c_char * 32), ("suffix", ctypes.c_uint32 * 6),
                ("start_sec", ctypes.c_uint64), ("start_usec", ctypes.c_uint64)]


def identity(pid):
    lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    lib.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
    lib.proc_pidpath.restype = ctypes.c_int
    lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64,
                                ctypes.c_void_p, ctypes.c_int]
    lib.proc_pidinfo.restype = ctypes.c_int
    path = ctypes.create_string_buffer(4096)
    info = BsdInfo()
    if lib.proc_pidpath(pid, path, len(path)) <= 0:
        return None
    if lib.proc_pidinfo(pid, 3, 0, ctypes.byref(info), ctypes.sizeof(info)) != ctypes.sizeof(info):
        # Exit can occur between the path and start-time queries. Only an
        # independently absent path is treated as gone; live inspection errors
        # remain failures and cannot authorize signalling.
        if lib.proc_pidpath(pid, path, len(path)) <= 0:
            return None
        raise RuntimeError(f"cannot obtain precise process start time for owned PID {pid}")
    return {"pid": pid, "executable": str(Path(os.fsdecode(path.value)).resolve()),
            "startTime": [info.start_sec, info.start_usec]}


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + ".new")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def executable(root, name="target"):
    return (root / f"{name}.app/Contents/MacOS/RawFixture").resolve()


def query(root, option):
    result = json.loads(subprocess.check_output([str(executable(root)), option], text=True))
    if isinstance(result, dict) and "error" in result:
        raise RuntimeError(result["error"])
    return result


def wait_for(description, predicate, seconds=15):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (FileNotFoundError, json.JSONDecodeError):
            pass
        time.sleep(.05)
    raise RuntimeError(f"timed out: {description}")


def build(root):
    if root.exists() and any(root.iterdir()):
        raise RuntimeError("build requires an empty output directory; nothing was removed")
    root.mkdir(parents=True, exist_ok=True)
    sdk = subprocess.check_output(["xcrun", "--show-sdk-path"], text=True).strip()
    output = root / "RawFixture"
    command = ["xcrun", "swiftc", "-sdk", sdk, "-module-cache-path", str(root / "swift-module-cache"),
               str(HERE / "RawFixture.swift"), "-o", str(output)]
    subprocess.run(command, check=True)
    for name, bundle in BUNDLES.items():
        contents = root / f"{name}.app/Contents"
        (contents / "MacOS").mkdir(parents=True)
        (contents / "MacOS/RawFixture").write_bytes(output.read_bytes())
        (contents / "MacOS/RawFixture").chmod(0o755)
        with (contents / "Info.plist").open("wb") as file:
            plistlib.dump({"CFBundleIdentifier": bundle, "CFBundleExecutable": "RawFixture",
                          "CFBundleName": f"Owned raw probe {name}", "CFBundlePackageType": "APPL",
                          "NSHighResolutionCapable": True}, file)
        subprocess.run(["codesign", "--force", "--sign", "-", "--identifier", bundle,
                        str(root / f"{name}.app")], check=True)
    write_json(root / "build.json", {"source": str(HERE / "RawFixture.swift"),
               "sourceSha256": hashlib.sha256((HERE / "RawFixture.swift").read_bytes()).hexdigest(),
               "command": command, "bundles": BUNDLES})


def fixture_state(root, name):
    return json.loads((root / f"{name}.json").read_text())


def activate_shadow(root, saved):
    expected = {key: saved[key] for key in ["pid", "executable", "startTime"]}
    if identity(saved["pid"]) != expected:
        raise RuntimeError("shadow changed before owned-only activation")
    activation = json.loads(subprocess.check_output([str(executable(root)), "--activate-owned-shadow",
                            str(saved["pid"]), saved["executable"]], text=True))
    if not activation.get("activated"):
        raise RuntimeError("owned shadow activation refused")


def prepare(root, space_source):
    manifest_path = root / "owned.json"
    if manifest_path.exists() or any((root / f"{name}.json").exists() for name in BUNDLES):
        raise RuntimeError("prepare requires fresh fixture state; cleanup existing owned run first")
    running = query(root, "--running-fixtures")
    if running:
        raise RuntimeError(f"exact fixture bundles already running; preserving them: {running}")
    before = query(root, "--probe-state")
    if before["screenLocked"]:
        raise RuntimeError("prepare requires an unlocked screen; no Space was switched")
    manifest = {"version": 1, "outdir": str(root), "spaceSource": space_source,
                "preExistingSpaceIDs": before["spaceIDs"], "initialActiveSpace": before["activeSpace"],
                "processes": {}, "ownedSpaceIDs": [], "status": "preparing"}
    write_json(manifest_path, manifest)
    try:
        # Only the exact fresh temporary bundles are launched. Shadow is first,
        # so target's auxiliary can later move to this known owned desktop.
        for name in ["shadow", "target"]:
            args = [str(root / f"{name}.json")]
            if name == "shadow":
                args.append("shadow")
            elif space_source:
                args.append("space-source")
            with (root / f"{name}.stderr").open("w") as stderr:
                launcher = subprocess.Popen(["open", "-n", "-W", str(root / f"{name}.app"), "--args", *args],
                                            stdout=subprocess.DEVNULL, stderr=stderr)
            state = wait_for(f"{name} PID publication", lambda: fixture_state(root, name))
            process = identity(state["pid"])
            if process is None or process["executable"] != str(executable(root, name)):
                raise RuntimeError(f"launched fixture identity mismatch: {process}")
            process.update(bundleId=BUNDLES[name], launcherPid=launcher.pid)
            manifest["processes"][name] = process
            write_json(manifest_path, manifest)
            if name == "shadow":
                def shadow_ready():
                    current = fixture_state(root, name)
                    destination = current.get("spaces", [])
                    if current["pid"] == process["pid"] and destination and set(destination).issubset(before["spaceIDs"]):
                        return current
                    return None
                state = wait_for("owned shadow existing desktop membership", shadow_ready)
                destination = state["spaces"]
                manifest["shadowDestinationSpaceIDs"] = destination
                write_json(manifest_path, manifest)
                activate_shadow(root, process)
                wait_for("owned shadow regular desktop before source creation", lambda:
                         fixture_state(root, name) if fixture_state(root, name)["frontmost"] == process["pid"]
                         and fixture_state(root, name)["activeSpace"] in destination
                         and fixture_state(root, name)["activeSpaceType"] == 0 else None)
            if name == "target" and space_source:
                def source_ready():
                    state = fixture_state(root, name)
                    if state["phase"] == "space-ready" and set(state.get("spaces", [])) - set(before["spaceIDs"]):
                        return state
                    return None
                source = wait_for("owned fullscreen auxiliary Space membership", source_ready)
                manifest["ownedSpaceIDs"] = sorted(set(source["spaces"]) - set(before["spaceIDs"]))
                if not manifest["ownedSpaceIDs"]:
                    raise RuntimeError("owned fullscreen source did not publish a new Space ID")
                write_json(manifest_path, manifest)
        shadow = manifest["processes"]["shadow"]
        activate_shadow(root, shadow)
        wait_for("owned shadow frontmost on its regular desktop", lambda:
                 fixture_state(root, "target") if fixture_state(root, "target")["frontmost"] == shadow["pid"]
                 and fixture_state(root, "target")["activeSpace"] in manifest["shadowDestinationSpaceIDs"]
                 and fixture_state(root, "target")["activeSpaceType"] == 0 else None)
        time.sleep(.2)
        after = query(root, "--probe-state")
        if not set(manifest["ownedSpaceIDs"]).issubset(after["spaceIDs"]):
            raise RuntimeError("owned source Space disappeared before acceptance")
        manifest["status"] = "ready"
        write_json(manifest_path, manifest)
        write_json(root / "space-before.json", {name: fixture_state(root, name) for name in BUNDLES})
        print(json.dumps(manifest, indent=2))
    except BaseException:
        # Keep the exact latched ownership manifest for review/cleanup. Never
        # broaden process selection to bundle names, process names or pgrep.
        manifest["status"] = "prepare_failed"
        write_json(manifest_path, manifest)
        raise


def cleanup(root):
    manifest = json.loads((root / "owned.json").read_text())
    if manifest["outdir"] != str(root):
        raise RuntimeError("ownership manifest output directory mismatch")
    mismatches = []
    # Signal only exact PID + proc_pidpath + precise start-time matches.
    for name in ["target", "shadow"]:
        saved = manifest["processes"].get(name)
        if saved is None:
            continue
        expected = {key: saved[key] for key in ["pid", "executable", "startTime"]}
        if expected["executable"] != str(executable(root, name)) or saved["bundleId"] != BUNDLES[name]:
            mismatches.append(name)
            continue
        current = identity(saved["pid"])
        if current is None:
            continue
        if current != expected:
            mismatches.append(name)
            continue
        if name == "target" and manifest["spaceSource"]:
            # Also latch a source published during an interrupted prepare,
            # after verifying the exact target process still owns this state.
            try:
                state = fixture_state(root, name)
                if state["pid"] == saved["pid"]:
                    manifest["ownedSpaceIDs"] = sorted(set(manifest["ownedSpaceIDs"]) |
                        (set(state.get("spaces", [])) - set(manifest["preExistingSpaceIDs"])))
            except (FileNotFoundError, json.JSONDecodeError):
                pass
        if identity(saved["pid"]) != expected:
            mismatches.append(name)
            continue
        os.kill(saved["pid"], signal.SIGTERM)
        wait_for(f"owned {name} process exit", lambda: identity(saved["pid"]) != expected, seconds=10)
    if mismatches:
        raise RuntimeError(f"refused to signal changed process identities: {mismatches}")
    def spaces_settled():
        state = query(root, "--probe-state")
        current = set(state["spaceIDs"])
        if set(manifest["ownedSpaceIDs"]) & current:
            return None
        return state
    after = wait_for("owned temporary fullscreen Space removed by fixture exit", spaces_settled, seconds=15)
    missing = set(manifest["preExistingSpaceIDs"]) - set(after["spaceIDs"])
    if missing:
        raise RuntimeError(f"pre-existing Spaces changed externally; no Space delete was attempted: {sorted(missing)}")
    manifest.update(status="cleaned", cleanupState=after,
                    cleanupPolicy="Exact latched PID/path/start only; no Space deletion API or foreground restoration")
    write_json(root / "owned.json", manifest)
    print(json.dumps(manifest, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["build", "prepare", "cleanup", "probe"])
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--space-source", action="store_true")
    args = parser.parse_args()
    if args.out.is_symlink():
        parser.error("output directory must not be a symlink")
    root = args.out.expanduser().resolve()
    if args.command == "build":
        build(root)
    elif args.command == "prepare":
        prepare(root, args.space_source)
    elif args.command == "cleanup":
        cleanup(root)
    else:
        print(json.dumps({"state": query(root, "--probe-state"),
                          "runningFixtures": query(root, "--running-fixtures")}, indent=2))


if __name__ == "__main__":
    main()
