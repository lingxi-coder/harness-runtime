#!/usr/bin/env python3
"""Exercise real libssh2/Mbed TLS initialization using the native CA build.

The include tree and libraries must come from the same pinned mbedtls-sys
build. This host test injects only OS entropy failure; it never replaces Mbed
TLS or libssh2 crypto. The test fails on unsupported entropy-injection hosts.
"""
import argparse
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
SSH = ROOT / "third_party/libssh2-sys"
FIXTURE = ROOT / "scripts/tests/fixtures/libssh2_mbedtls_init.c"


def source_files():
    build = (SSH / "build.rs").read_text()
    sources = re.findall(r'\.file\("(libssh2/src/[^\"]+\.c)"\)', build)
    sources = [source for source in sources if not source.endswith("/agent_win.c")]
    if len(sources) < 25 or len(sources) != len(set(sources)):
        raise RuntimeError("could not determine vendored libssh2 source set from build.rs")
    return [SSH / source for source in sources]


def unix_defines(backend):
    return ("HAVE_LONGLONG", "HAVE_SNPRINTF", "HAVE_UNISTD_H", "HAVE_INTTYPES_H",
            "HAVE_STDLIB_H", "HAVE_SYS_SELECT_H", "HAVE_SYS_SOCKET_H",
            "HAVE_SYS_IOCTL_H", "HAVE_SYS_TIME_H", "HAVE_SYS_UN_H", "HAVE_O_NONBLOCK",
            backend, "HAVE_LIBCRYPT32", "HAVE_POLL", "HAVE_GETTIMEOFDAY",
            "LIBSSH2_DH_GEX_NEW", "LIBSSH2_HAVE_ZLIB")


def write_config(directory):
    template = (SSH / "libssh2/src/libssh2_config_cmake.h.in").read_text()
    (directory / "libssh2_config.h").write_text("\n".join(
        line for line in template.splitlines() if "#cmakedefine" not in line) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--include", type=Path, required=True)
    parser.add_argument("--lib-dir", type=Path, required=True)
    parser.add_argument("--sanitizer", choices=("none", "address", "thread"), required=True)
    args = parser.parse_args()
    if sys.platform not in ("darwin", "linux"):
        raise RuntimeError("real OS entropy failure injection requires macOS or glibc Linux")
    for name in ("libmbedtls.a", "libmbedx509.a", "libmbedcrypto.a"):
        if not (args.lib_dir / name).is_file():
            raise RuntimeError(f"missing Mbed TLS archive: {name}")
    if not (args.include / "mbedtls/mbedtls_config.h").is_file():
        raise RuntimeError("missing generated Mbed TLS include tree")
    sources = source_files()
    defines = unix_defines("LIBSSH2_MBEDTLS")
    with tempfile.TemporaryDirectory(prefix="lingxi-libssh2-init-") as temporary:
        tmp = Path(temporary)
        write_config(tmp)
        compiler = os.environ.get("CC", "cc")
        flags = ["-O1", "-g0", "-fno-omit-frame-pointer", "-fvisibility=hidden"]
        if args.sanitizer != "none":
            flags += ["-fsanitize=" + args.sanitizer]
        command = [compiler, *flags, *("-D" + item for item in defines),
                   "-I", str(tmp), "-I", str(SSH / "libssh2/include"),
                   "-I", str(SSH / "libssh2/src"), "-I", str(args.include),
                   *(str(source) for source in sources), str(FIXTURE),
                   "-L", str(args.lib_dir), "-lmbedtls", "-lmbedx509", "-lmbedcrypto",
                   "-lz", "-lpthread"]
        if sys.platform == "linux":
            command += ["-Wl,--wrap=syscall"]
        command += ["-o", str(tmp / "libssh2-init")]
        print("+", shlex.join(command), flush=True)
        subprocess.run(command, cwd=ROOT, check=True)
        subprocess.run([str(tmp / "libssh2-init")], cwd=ROOT, check=True)
        print(f"Compiled {len(sources)} vendored libssh2 source files.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"libssh2 Mbed TLS initialization validation failed: {error}", file=sys.stderr)
        sys.exit(1)
