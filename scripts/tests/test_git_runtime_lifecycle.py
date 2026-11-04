#!/usr/bin/env python3
"""Compile the actual libgit2 runtime and test init failure/retry lifecycles."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
GIT2 = ROOT / "third_party/git2-rs/libgit2-sys/libgit2"
FIXTURE = ROOT / "scripts/tests/fixtures/git_runtime_lifecycle.c"


class RuntimeLifecycle(unittest.TestCase):
    def test_init_failure_retry_and_concurrency(self):
        with tempfile.TemporaryDirectory(prefix="git-runtime-lifecycle-") as temp:
            binary = Path(temp) / "runtime-lifecycle"
            cmd = [os.environ.get("CC", "cc"), "-std=c11", "-Wall", "-Wextra", "-Werror",
                   "-D_POSIX_C_SOURCE=200809L", "-DLIBGIT2_NO_FEATURES_H", "-DGIT_THREADS",
                   "-DGIT_ARCH_64", "-pthread",
                   "-I" + str(GIT2 / "src/util"), "-I" + str(GIT2 / "include"),
                   str(GIT2 / "src/util/runtime.c"), str(FIXTURE), "-o", str(binary)]
            sanitizer = os.environ.get("GIT_RUNTIME_SANITIZER")
            if sanitizer:
                self.assertIn(sanitizer, ("address", "thread"))
                cmd += ["-fsanitize=" + sanitizer, "-fno-omit-frame-pointer"]
            subprocess.run(cmd, check=True, timeout=60)
            subprocess.run([str(binary)], check=True, timeout=30)


if __name__ == "__main__":
    unittest.main(verbosity=2)
