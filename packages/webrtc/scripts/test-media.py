#!/usr/bin/env python3
"""Run native media tests with only the declared plugins, without host scanning."""
import argparse
import os
from pathlib import Path
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--workspace", action="store_true", help="test all embedded WebRTC packages")
args = parser.parse_args()
root = Path(__file__).resolve().parents[1]
plugins = Path(subprocess.check_output(
    ["pkg-config", "--variable=pluginsdir", "gstreamer-1.0"], text=True,
).strip())
names = ["app", "audioconvert", "audioresample", "coreelements", "opus", "rtp", "rtpmanager", "vpx", "videoconvertscale"]
with tempfile.TemporaryDirectory(prefix="lingxi-webrtc-media-") as temp:
    directory = Path(temp) / "plugins"
    directory.mkdir()
    for name in names:
        source = plugins / ("libgst" + name + (".dylib" if os.uname().sysname == "Darwin" else ".so"))
        if not source.is_file():
            raise SystemExit("Required media plugin missing: " + str(source))
        (directory / source.name).symlink_to(source)
    env = os.environ.copy()
    env["LINGXI_MEDIA_TEST_PLUGIN_DIR"] = str(directory)
    for suffix in ["", "_1_0"]:
        env["GST_PLUGIN_PATH" + suffix] = ""
        env["GST_PLUGIN_SYSTEM_PATH" + suffix] = str(directory)
        env["GST_REGISTRY" + suffix] = str(Path(temp) / "registry.bin")
    command = ["cargo", "test", "--manifest-path", str(root / "Cargo.toml"), "--locked", "--offline"]
    if args.workspace:
        command += ["--workspace", "--no-fail-fast", "--features", "lingxi-webrtc-media/desktop-devices,lingxi-webrtc-media/diagnostics"]
    else:
        command += ["-p", "lingxi-webrtc-media", "-p", "lingxi-webrtc-mobile", "--features", "desktop-devices"]
    subprocess.run(command, env=env, check=True)
