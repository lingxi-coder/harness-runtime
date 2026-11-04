from pathlib import Path
import os
import sys

rooted = Path("crates/rooted-fs/src/lib.rs")
owner = Path("crates/platforms/windows/src/process/supervisor_native.rs")
backups = {
    rooted: Path(os.environ["RUNNER_TEMP"]) / "rooted-fs-fixed.rs",
    owner: Path(os.environ["RUNNER_TEMP"]) / "supervisor-native-fixed.rs",
}

if sys.argv[1] == "baseline":
    changes = {
        rooted: (
            b"            desired_access | FILE_READ_ATTRIBUTES,\n            share_access,\n            disposition,\n",
            b"            desired_access,\n            share_access,\n            disposition,\n",
        ),
        owner: (
            b'    let user = current_user_sid()?;\n    let owner = sid_string(user[0] as Handle)?;\n'
            b'    // TOKEN_OWNER can be an administrators group even when TOKEN_USER is an\n'
            b'    // individual account. Specify the user at creation; validation stays strict.\n'
            b'    let descriptor = descriptor_from_sddl(&format!("O:{owner}{PRIVATE_DACL}"))?;\n',
            b'    let descriptor = descriptor_from_sddl(PRIVATE_DACL)?;\n',
        ),
    }
    for path, (needle, old) in changes.items():
        fixed = path.read_bytes()
        # Git on Windows may check out CRLF: normalize only the matching bytes.
        if b"\r\n" in fixed:
            needle, old = needle.replace(b"\n", b"\r\n"), old.replace(b"\n", b"\r\n")
        if fixed.count(needle) != 1:
            raise RuntimeError(f"expected the fixed code exactly once in {path}")
        backups[path].write_bytes(fixed)
        path.write_bytes(fixed.replace(needle, old, 1))
elif sys.argv[1] == "fixed":
    for path, backup in backups.items():
        path.write_bytes(backup.read_bytes())
else:
    raise ValueError("expected baseline or fixed")
