#!/usr/bin/env python3
"""Create capture-reference parameters from the exact live disposable fixtures."""
import argparse
import ctypes
import importlib.util
import json
import math
from pathlib import Path
import re
import sys
import time

HERE = Path(__file__).resolve().parent


class Point(ctypes.Structure):
    _fields_ = [("x", ctypes.c_double), ("y", ctypes.c_double)]


class Size(ctypes.Structure):
    _fields_ = [("width", ctypes.c_double), ("height", ctypes.c_double)]


class Rect(ctypes.Structure):
    _fields_ = [("origin", Point), ("size", Size)]


def display_geometry():
    cg = ctypes.CDLL("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")
    cg.CGMainDisplayID.argtypes = []
    cg.CGMainDisplayID.restype = ctypes.c_uint32
    cg.CGDisplayBounds.argtypes = [ctypes.c_uint32]
    cg.CGDisplayBounds.restype = Rect
    for name in ["CGDisplayPixelsWide", "CGDisplayPixelsHigh"]:
        function = getattr(cg, name)
        function.argtypes = [ctypes.c_uint32]
        function.restype = ctypes.c_size_t
    display = cg.CGMainDisplayID()
    bounds = cg.CGDisplayBounds(display)
    values = [bounds.origin.x, bounds.origin.y, bounds.size.width, bounds.size.height]
    pixels = [cg.CGDisplayPixelsWide(display), cg.CGDisplayPixelsHigh(display)]
    if (not display or not all(math.isfinite(v) for v in values)
            or min(values[2:]) <= 0 or min(pixels) <= 0):
        raise RuntimeError("cannot query the main fixture display")
    return {"id": display, "bounds": values, "pixels": pixels}


def configure(fixtures, output, host_root):
    spec = importlib.util.spec_from_file_location("owned_raw_fixture", HERE.parent / "raw-background/fixture.py")
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    manifest = json.loads((fixtures / "owned.json").read_text())
    if manifest.get("outdir") != str(fixtures) or manifest.get("status") != "ready":
        raise RuntimeError("use a ready portable raw-background ownership manifest")
    if output.exists() and any(output.iterdir()):
        raise RuntimeError("capture output directory must be empty; nothing was removed")
    states = {}
    processes = {}
    for name, bundle in fixture.BUNDLES.items():
        saved = manifest["processes"][name]
        expected = {key: saved[key] for key in ["pid", "executable", "startTime"]}
        if saved.get("bundleId") != bundle or expected["executable"] != str(fixture.executable(fixtures, name)):
            raise RuntimeError(f"{name} manifest does not describe its exact owned bundle")
        if fixture.identity(saved["pid"]) != expected:
            raise RuntimeError(f"{name} process identity changed")
        path = fixtures / f"{name}.json"
        if time.time() - path.stat().st_mtime > 2:
            raise RuntimeError(f"{name} fixture state is stale")
        state = fixture.fixture_state(fixtures, name)
        point = state.get("canvasScreen")
        if (state.get("pid") != saved["pid"] or not isinstance(state.get("window"), int)
                or state["window"] <= 0 or not isinstance(point, list) or len(point) != 2
                or not all(isinstance(v, (int, float)) and math.isfinite(v) for v in point)):
            raise RuntimeError(f"{name} has no live owned canvas geometry")
        states[name] = state
        processes[name] = expected
    shadow_pid = processes["shadow"]["pid"]
    active = states["shadow"]["activeSpace"]
    for name, state in states.items():
        if (state.get("screenLocked") is not False
                or state.get("frontmost") != shadow_pid or state.get("activeSpaceType") != 0
                or state.get("activeSpace") != active or active not in state.get("spaces", [])):
            raise RuntimeError(f"{name} must be on the owned shadow's active regular desktop; finish the Space-move test first")
    geometry = display_geometry()
    origin_x, origin_y, width, height = geometry["bounds"]
    def local(point):
        return [point[0] - origin_x, point[1] - origin_y]
    foreign = local(states["shadow"]["canvasScreen"])
    canvas = local(states["target"]["canvasScreen"])
    # These offsets describe the checked-in fixture's canvas, not a user's
    # window: canvasScreen is at (100,80); the canvas starts at (20,20).
    edge = [canvas[0] - 80, canvas[1] - 60]
    region = [edge[0] - 9.75, edge[1] - 9.5, edge[0] + 180.75, edge[1] + 110.5]
    for name, point in [("foreign marker", foreign), ("owned edge", edge)]:
        if not (8 <= point[0] <= width - 8 and 8 <= point[1] <= height - 8):
            raise RuntimeError(f"{name} is outside the main display; reposition only the disposable fixtures")
    if not (0 <= region[0] < region[2] <= width and 0 <= region[1] < region[3] <= height):
        raise RuntimeError("owned fractional zoom region is outside the main display")
    for name, process in processes.items():
        if fixture.identity(process["pid"]) != process:
            raise RuntimeError(f"{name} identity changed during configuration")
    output.mkdir(parents=True, exist_ok=True)
    config = {"displayId": geometry["id"], "hostRoot": host_root,
              "allowedBundleId": fixture.BUNDLES["target"],
              "foreignBundleId": fixture.BUNDLES["shadow"],
              "foreignLogicalPoint": foreign, "ownedEdgeLogicalPoint": edge,
              "logicalRegion": region}
    fixture.write_json(output / "config.json", config)
    fixture.write_json(output / "owned-inputs.json", {
        "fixtureRoot": str(fixtures), "processes": processes, "display": geometry,
        "activeSpace": active, "windowIds": {name: state["window"] for name, state in states.items()},
        "scope": "read-only identity/geometry queries; no activation, input, capture or Space mutation"})
    print(json.dumps(config, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--host-root", required=True)
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("macOS is required")
    if args.fixtures.is_symlink() or args.out.is_symlink():
        parser.error("fixture and output roots must not be symlinks")
    if not re.fullmatch(r"[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+", args.host_root):
        parser.error("host root must be a bundle identifier")
    configure(args.fixtures.expanduser().resolve(), args.out.expanduser().resolve(), args.host_root)


if __name__ == "__main__":
    main()
