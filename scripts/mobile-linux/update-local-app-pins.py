#!/usr/bin/env python3
from pathlib import Path
import os
import sys
os.execv(sys.executable, [sys.executable, str(Path(__file__).with_name("sdk_delegate.py")), Path(__file__).name, *sys.argv[1:]])
