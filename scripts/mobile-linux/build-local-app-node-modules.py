#!/usr/bin/env python3
import argparse
from pathlib import Path
import subprocess
import sys
sys.dont_write_bytecode = True
from sdk_source import ROOT, effective_pins, resolve_sdk

parser = argparse.ArgumentParser()
parser.add_argument("--sdk-root")
parser.add_argument("--arch", required=True, choices=("aarch64", "x86_64"))
parser.add_argument("--rootfs", required=True)
parser.add_argument("--output", "--output-dir", dest="output", required=True)
parser.add_argument("--cache-dir", required=True)
args = parser.parse_args()
sdk = resolve_sdk(args.sdk_root)
pins = effective_pins(sdk_root=sdk)
profile = pins["local_app_runtime"]
command = ["bash", str(sdk / "scripts/mobile-linux/build-node-modules.sh"), "--arch", args.arch,
           "--rootfs", args.rootfs, "--bundle-dir", str(ROOT / profile["template"]),
           "--lock-sha256", profile["lockfile_sha256"], "--output-dir", args.output,
           "--cache-dir", args.cache_dir]
subprocess.run(command, check=True)
# Keep product requirements here; the SDK never names a renderer or template.
modules = Path(args.output) / "node_modules"
if not (modules / "vite/bin/vite.js").is_file():
    raise SystemExit("Local App dependency seed lacks Vite")
arch = "arm64" if args.arch == "aarch64" else "x64"
for relative in (f"@rolldown/binding-linux-{arch}-musl", f"@rollup/rollup-linux-{arch}-musl", f"lightningcss-linux-{arch}-musl"):
    if not (modules / relative).is_dir():
        raise SystemExit(f"Local App dependency seed lacks {relative}")
for relative in ("three", "phaser", "@babylonjs"):
    if (modules / relative).exists():
        raise SystemExit(f"base Local App seed must remain renderer-free: {relative}")
