#!/usr/bin/env python3
"""Build, locate, and execute the real Linux seccomp helper in a custom target dir."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    if sys.platform != "linux":
        raise SystemExit("seccomp smoke requires a real Linux kernel")
    target = Path(os.environ["CARGO_TARGET_DIR"])
    if not target.is_absolute() or target == ROOT / "target":
        raise SystemExit("CARGO_TARGET_DIR must be an absolute custom build directory")
    target.mkdir(parents=True, exist_ok=True)
    artifacts = subprocess.check_output([
        "cargo", "build", "--locked", "-p", "apply-seccomp", "-p", "sandbox-runtime",
        "--message-format=json",
    ], cwd=ROOT, text=True)
    helper = None
    library = None
    for line in artifacts.splitlines():
        item = json.loads(line)
        if item.get("reason") != "compiler-artifact":
            continue
        if item["target"]["name"] == "apply-seccomp" and item.get("executable"):
            helper = Path(item["executable"]).resolve()
        if item["target"]["name"] == "sandbox_runtime" and "lib" in item["target"]["kind"]:
            library = next(Path(p) for p in item["filenames"] if p.endswith(".rlib"))
    assert helper == (target / "debug/apply-seccomp").resolve(), helper
    assert helper.is_file() and os.access(helper, os.X_OK), helper
    assert library is not None, "Cargo did not report the sandbox-runtime library"

    with tempfile.TemporaryDirectory(prefix="seccomp-locator-") as directory:
        scratch = Path(directory)
        probe = scratch / "locator.rs"
        probe.write_text(r"""
fn main() {
    let prefix = sandbox_runtime::linux::resolve_apply_seccomp_prefix(None, None)
        .expect("custom CARGO_TARGET_DIR must locate the built helper");
    print!("{prefix}");
}
""")
        binary = scratch / "locator"
        subprocess.run([
            "rustc", "--edition=2021", str(probe), "-o", str(binary),
            "--extern", f"sandbox_runtime={library}",
            "-L", f"dependency={target / 'debug/deps'}",
        ], cwd=ROOT, check=True)
        # The probe executable is outside target/debug, so its sibling lookup
        # cannot succeed: this exercises custom-target workspace discovery.
        prefix = subprocess.check_output([str(binary)], cwd=scratch, text=True)
        assert shlex.split(prefix) == [str(helper)], prefix
        print(f"PASS custom target discovery: {helper}", flush=True)

        control = """
import socket
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM):
    pass
left, right = socket.socketpair(socket.AF_UNIX)
left.close()
right.close()
"""
        subprocess.run([sys.executable, "-c", control], check=True)
        filtered = """
import errno
from pathlib import Path
import socket

for name, operation in (
    ("socket(AF_UNIX)", lambda: socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)),
    ("socketpair(AF_UNIX)", lambda: socket.socketpair(socket.AF_UNIX)),
):
    try:
        operation()
    except OSError as error:
        assert error.errno == errno.EPERM, (name, error)
    else:
        raise AssertionError(name + " was not blocked")
with socket.socket(socket.AF_INET, socket.SOCK_STREAM):
    pass
assert "NoNewPrivs:\\t1" in Path("/proc/self/status").read_text()
print("PASS real seccomp: AF_UNIX socket/socketpair denied, AF_INET allowed, NO_NEW_PRIVS set")
"""
        # Use the runtime-discovered prefix, not a separately hardcoded path.
        subprocess.run([*shlex.split(prefix), sys.executable, "-c", filtered], check=True)
        status = subprocess.run([*shlex.split(prefix), "sh", "-c", "exit 73"]).returncode
        assert status == 73, status
        print("PASS helper exec preserves child exit status", flush=True)


if __name__ == "__main__":
    main()
