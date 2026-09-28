#!/usr/bin/env python3
import os
from pathlib import Path
import sys
sys.dont_write_bytecode = True
from sdk_source import resolve_sdk

args = sys.argv[1:]
tool = args.pop(0)
explicit = None
if "--sdk-root" in args:
    index = args.index("--sdk-root")
    explicit = args[index + 1]
    del args[index:index + 2]
root = resolve_sdk(explicit)
path = root / "scripts/mobile-linux" / tool
if not path.is_file():
    raise SystemExit(f"required SDK tool missing: {path}")
command = sys.executable if path.suffix == ".py" else "/bin/bash"
os.execv(command, [command, str(path), *args])
