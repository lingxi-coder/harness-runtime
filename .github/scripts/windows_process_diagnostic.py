from pathlib import Path
import os
import sys

path = Path("crates/rooted-fs/src/lib.rs")
backup = Path(os.environ["RUNNER_TEMP"]) / "rooted-fs-fixed.rs"
needle = b"            desired_access | FILE_READ_ATTRIBUTES,\n            share_access,\n            disposition,\n"

if sys.argv[1] == "baseline":
    fixed = path.read_bytes()
    # Git on Windows may check out CRLF: normalize only the matching bytes.
    needle = needle.replace(b"\n", b"\r\n") if b"\r\n" in fixed else needle
    if fixed.count(needle) != 1:
        raise RuntimeError("expected the fixed Windows open_regular rights exactly once")
    backup.write_bytes(fixed)
    path.write_bytes(fixed.replace(needle, needle.replace(b" | FILE_READ_ATTRIBUTES", b""), 1))
elif sys.argv[1] == "fixed":
    path.write_bytes(backup.read_bytes())
else:
    raise ValueError("expected baseline or fixed")
