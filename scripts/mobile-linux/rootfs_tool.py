#!/usr/bin/env python3
from pathlib import Path
import os
import sys
os.execv(sys.executable, [sys.executable, str(Path(__file__).resolve().parents[1] / "lib/sdk_delegate.py"), Path(__file__).name, *sys.argv[1:]])
