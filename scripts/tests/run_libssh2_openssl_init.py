#!/usr/bin/env python3
"""Compile the real OpenSSL backend and test first-call reference ownership.

Requires pkg-config and OpenSSL development headers/libraries. The backend's
actual init/exit are wrapped only to count entry; both real functions still run.
"""
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile

from run_libssh2_mbedtls_init import ROOT, SSH, source_files, unix_defines, write_config

FIXTURE = ROOT / "scripts/tests/fixtures/libssh2_openssl_init.c"


def main():
    compiler = os.environ.get("CC", "cc")
    flags = shlex.split(subprocess.check_output(["pkg-config", "--cflags", "openssl"], text=True))
    libraries = shlex.split(subprocess.check_output(["pkg-config", "--libs", "openssl"], text=True))
    # The vendored ref10 implementation and its Mbed-specific glue are for
    # the Mbed backend. OpenSSL provides its own Ed25519 implementation.
    sources = [path for path in source_files() if path.parent.name != "ed25519"]
    backend = SSH / "libssh2/src/crypto.c"
    if backend not in sources:
        raise RuntimeError("libssh2 crypto.c is missing from the build source list")
    with tempfile.TemporaryDirectory(prefix="lingxi-libssh2-openssl-") as temporary:
        tmp = Path(temporary)
        write_config(tmp)
        defines = (*unix_defines("LIBSSH2_OPENSSL"), "HAVE_EVP_AES_128_CTR")
        common = [compiler, "-O1", "-g0", "-fno-omit-frame-pointer", "-fvisibility=hidden",
                  *("-D" + item for item in defines), "-I", str(tmp),
                  "-I", str(SSH / "libssh2/include"), "-I", str(SSH / "libssh2/src"), *flags]
        rename = ["-D_libssh2_openssl_crypto_init=test_real_openssl_crypto_init",
                  "-D_libssh2_openssl_crypto_exit=test_real_openssl_crypto_exit"]
        commands = [
            [*common, *rename, "-c", str(backend), "-o", str(tmp / "crypto.o")],
            [*common, *(str(path) for path in sources if path != backend), str(tmp / "crypto.o"),
             str(FIXTURE), *libraries, "-lz", "-lpthread", "-o", str(tmp / "libssh2-openssl-init")],
            [str(tmp / "libssh2-openssl-init")],
        ]
        for command in commands:
            print("+", shlex.join(command), flush=True)
            subprocess.run(command, cwd=ROOT, check=True)
        print(f"Compiled {len(sources)} vendored libssh2 source files with real OpenSSL.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"libssh2 OpenSSL initialization validation failed: {error}", file=sys.stderr)
        sys.exit(1)
