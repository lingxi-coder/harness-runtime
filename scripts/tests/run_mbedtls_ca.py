#!/usr/bin/env python3
"""Build the current native seam and run isolated two-authority CA regressions.

The fixture is intentionally tiny: it builds only mbedtls-sys and its pinned
cc dependency. Every native C file and both C test executables use the selected
sanitizer. Temporary CA keys and the build target are always removed on exit.
Optional output retains only logs and hashes, never private keys.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
SEAM = ROOT / "third_party/mbedtls-sys"
TEST = ROOT / "scripts/tests/test_mbedtls_ca_snapshot.py"
CC_VERSION = "1.2.62"

LOCK = '''# Generated fixture lock; dependency versions and checksums are deliberate.
version = 4

[[package]]
name = "cc"
version = "1.2.62"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "a1dce859f0832a7d088c4f1119888ab94ef4b5d6795d1ce05afb7fe159d79f98"
dependencies = ["find-msvc-tools", "shlex"]

[[package]]
name = "find-msvc-tools"
version = "0.1.9"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "5baebc0774151f905a1a2cc41989300b1e6fbb29aff0ceffa1064fdd3088d582"

[[package]]
name = "lingxi-mbedtls-ca-fixture"
version = "0.1.0"
dependencies = ["cc", "mbedtls-sys"]

[[package]]
name = "mbedtls-sys"
version = "{version}"
dependencies = ["cc"]

[[package]]
name = "shlex"
version = "1.3.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0fda2ff0d084019ba4d7c6f371c95d8fd75ce3524c3cb8fb653a3023f6323e64"
'''

OPENSSL_CONFIG = '''[req]
distinguished_name = dn
x509_extensions = extensions
prompt = no
[dn]
CN = expected.test
[extensions]
subjectAltName = DNS:expected.test
basicConstraints = critical,CA:TRUE
keyUsage = critical,digitalSignature,keyCertSign,cRLSign
extendedKeyUsage = serverAuth
'''


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(command, *, env, cwd, log):
    print("+", shlex.join(str(item) for item in command), flush=True)
    result = subprocess.run(command, cwd=cwd, env=env, text=True, capture_output=True)
    log.write_text(result.stdout + result.stderr)
    if result.returncode:
        sys.stderr.write(result.stdout + result.stderr)
        raise RuntimeError(f"command failed with exit {result.returncode}: {command[0]}")
    return result


def native_env(compiler, sanitizer):
    env = os.environ.copy()
    # cc-rs gives target-specific flags precedence over CFLAGS. Remove inherited
    # variants so all native objects use this run's single sanitizer choice.
    names = ("CC", "CFLAGS", "AR", "ARFLAGS", "RANLIB", "RANLIBFLAGS")
    for name in list(env):
        if any(name == key or name.startswith(key + "_") or
               name in ("HOST_" + key, "TARGET_" + key) for key in names):
            env.pop(name)
    # O1 keeps the ECDSA/TSan stress test within its timeout on CI runners.
    flags = ["-O1", "-g", "-fno-omit-frame-pointer"]
    if sanitizer != "none":
        flags.append("-fsanitize=" + sanitizer)
    env.update(CC=compiler, CFLAGS=" ".join(flags), CC_ENABLE_DEBUG_OUTPUT="1",
               CARGO_INCREMENTAL="0", CARGO_PROFILE_DEV_DEBUG="0",
               CARGO_BUILD_JOBS="2", PYTHONDONTWRITEBYTECODE="1")
    return env


def fixture_version():
    text = (SEAM / "Cargo.toml").read_text()
    package = text.split("[package]", 1)[1].split("[", 1)[0]
    match = re.search(r'^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"', package, re.M)
    if not match:
        raise RuntimeError("cannot determine mbedtls-sys package version")
    return match.group(1)


def prepare_fixture(directory):
    fixture = directory / "fixture"
    (fixture / "src").mkdir(parents=True)
    manifest = (f'[package]\nname = "lingxi-mbedtls-ca-fixture"\nversion = "0.1.0"\n'
                f'edition = "2021"\nbuild = "build.rs"\n'
                f'[dependencies]\nmbedtls-sys = {{ path = {json.dumps(str(SEAM))} }}\n'
                f'[build-dependencies]\ncc = "={CC_VERSION}"\n[workspace]\n')
    (fixture / "Cargo.toml").write_text(manifest)
    (fixture / "Cargo.lock").write_text(LOCK.format(version=fixture_version()))
    (fixture / "build.rs").write_text('fn main() {}\n')
    (fixture / "src/lib.rs").write_text(
        'pub fn anchor() -> unsafe extern "C" fn() -> u32 { mbedtls_sys::mbedtls_link_anchor }\n')
    return fixture


def locate_native(stdout):
    candidates = set()
    for line in stdout.splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if item.get("reason") != "build-script-executed":
            continue
        path = Path(item["out_dir"])
        if ((path / "lingxi-mbedtls-include").is_dir() and
                all((path / name).is_file() for name in
                    ("libmbedtls.a", "libmbedx509.a", "libmbedcrypto.a"))):
            candidates.add(path)
    if len(candidates) != 1:
        raise RuntimeError(f"expected one mbedtls-sys output directory, found {len(candidates)}")
    return candidates.pop()


def verify_compile_commands(log, sanitizer):
    expected = {str(path.resolve()) for path in (ROOT / "third_party/mbedtls/library").glob("*.c")}
    compiled = []
    flag = None if sanitizer == "none" else "-fsanitize=" + sanitizer
    for line in log.splitlines():
        marker = line.find("running: ")
        if marker < 0:
            continue
        command = shlex.split(line[marker + len("running: "):])
        if "-c" not in command:
            continue
        sources = [str(Path(item).resolve()) for item in command if item.endswith(".c")]
        for source in sources:
            if source in expected:
                if flag and flag not in command:
                    raise RuntimeError(f"missing {flag} in native compile for {source}")
                compiled.append(source)
    if not expected or len(compiled) != len(expected) or set(compiled) != expected:
        raise RuntimeError(f"native compile coverage mismatch: {len(compiled)} commands for {len(expected)} sources")
    return len(compiled)


def make_certificate(directory, algorithm, name, env):
    cert = directory / f"{algorithm}-{name}.pem"
    key = directory / f"{algorithm}-{name}.key"
    config = directory / "openssl.cnf"
    if algorithm == "rsa":
        key_args = ["-newkey", "rsa:2048", "-nodes"]
    else:
        subprocess.run(["openssl", "ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", str(key)],
                       cwd=directory, env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        key_args = ["-key", str(key)]
    command = ["openssl", "req", "-new", "-x509", *key_args, "-sha256", "-days", "2",
               "-config", str(config), "-out", str(cert)]
    if algorithm == "rsa":
        command += ["-keyout", str(key)]
    subprocess.run(command, cwd=directory, env=env, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return cert, key


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sanitizer", choices=("none", "address", "thread"), required=True)
    parser.add_argument("--output-dir", type=Path, help="retain logs and hashes under a new run directory")
    args = parser.parse_args()
    compiler = shutil.which(os.environ.get("CC", "cc"))
    if compiler is None:
        raise RuntimeError("C compiler not found; set CC to its executable")
    for name in ("cargo", "openssl"):
        if shutil.which(name) is None:
            raise RuntimeError(f"required executable not found: {name}")
    if args.output_dir:
        args.output_dir.mkdir(parents=True, exist_ok=True)
        logs = Path(tempfile.mkdtemp(prefix=f"ca-{args.sanitizer}-", dir=args.output_dir))
    else:
        logs = None
    with tempfile.TemporaryDirectory(prefix="lingxi-mbedtls-ca-") as temporary:
        directory = Path(temporary)
        logdir = logs or directory
        fixture = prepare_fixture(directory)
        env = native_env(compiler, args.sanitizer)
        command = ["cargo", "+1.94.0", "build", "--locked", "-vv", "--manifest-path",
                   str(fixture / "Cargo.toml"), "--target-dir", str(directory / "target"),
                   "--message-format=json"]
        build = run(command, env=env, cwd=fixture, log=logdir / "native-build.log")
        (logdir / "native-messages.jsonl").write_text(build.stdout)
        native = locate_native(build.stdout)
        count = verify_compile_commands(build.stdout + build.stderr, args.sanitizer)
        include = native / "lingxi-mbedtls-include"
        macros = run([compiler, "-E", "-dM", "-I", str(include), "-include", "mbedtls/build_info.h",
                      "-x", "c", os.devnull], env=env, cwd=fixture, log=logdir / "native-macros.log").stdout
        for required in ("MBEDTLS_THREADING_C", "MBEDTLS_THREADING_PTHREAD", "MBEDTLS_SSL_PROTO_TLS1_3"):
            if not re.search(r"^#define " + required + r"\b", macros, re.M):
                raise RuntimeError(f"native configuration is missing {required}")
        # PSA must not be used by libssh2's earlier legacy DRBG initialization;
        # enabling either option changes the ordering assumption under test.
        for forbidden in ("MBEDTLS_SELF_TEST", "MBEDTLS_USE_PSA_CRYPTO", "MBEDTLS_CTR_DRBG_USE_PSA_CRYPTO"):
            if re.search(r"^#define " + forbidden + r"\b", macros, re.M):
                raise RuntimeError(f"native configuration unexpectedly enables {forbidden}")
        (directory / "openssl.cnf").write_text(OPENSSL_CONFIG)
        results = []
        for algorithm in ("rsa", "ec"):
            cert_a, key_a = make_certificate(directory, algorithm, "A", env)
            cert_b, key_b = make_certificate(directory, algorithm, "B", env)
            if digest(cert_a) == digest(cert_b):
                raise RuntimeError("the generated authorities are not distinct")
            test_env = env.copy()
            test_env.update(MBEDTLS_TEST_REQUIRE="1", MBEDTLS_TEST_INCLUDE=str(include),
                            MBEDTLS_TEST_LIB_DIR=str(native), MBEDTLS_TEST_CERT_A=str(cert_a),
                            MBEDTLS_TEST_CERT_B=str(cert_b), MBEDTLS_TEST_KEY_A=str(key_a),
                            MBEDTLS_TEST_KEY_B=str(key_b))
            test_env.pop("MBEDTLS_TEST_SANITIZER", None)
            if args.sanitizer != "none":
                test_env["MBEDTLS_TEST_SANITIZER"] = args.sanitizer
            test = run([sys.executable, str(TEST), "-v"], env=test_env, cwd=ROOT,
                       log=logdir / f"{algorithm}.log")
            output = test.stdout + test.stderr
            if "Ran 2 tests" not in output or "\nOK\n" not in output or "skipped" in output:
                raise RuntimeError(f"{algorithm} run was incomplete or skipped")
            if sys.platform == "darwin" and "native PSA entropy failure=" not in output:
                raise RuntimeError(f"{algorithm} run did not exercise native PSA entropy failure")
            results.append({"algorithm": algorithm, "cert_a_sha256": digest(cert_a),
                            "cert_b_sha256": digest(cert_b), "log_sha256": digest(logdir / f"{algorithm}.log")})
        manifest = {"sanitizer": args.sanitizer, "rust_toolchain": "1.94.0", "cc_version": CC_VERSION,
                    "native_source_files": count, "native_archives": {name: digest(native / name) for name in
                    ("libmbedtls.a", "libmbedx509.a", "libmbedcrypto.a")},
                    "native_config_sha256": digest(include / "mbedtls/mbedtls_config.h"),
                    "native_upstream_config_sha256": digest(include / "mbedtls/mbedtls_config_upstream.h"),
                    "build_script_sha256": digest(SEAM / "build.rs"),
                    "ca_source_sha256": digest(ROOT / "third_party/git2-rs/libgit2-sys/libgit2/src/libgit2/streams/mbedtls.c"),
                    "test_sha256": digest(TEST),
                    "handshake_fixture_sha256": digest(ROOT / "scripts/tests/fixtures/mbedtls_ca_handshake.c"),
                    "runner_sha256": digest(Path(__file__).resolve()), "results": results}
        (logdir / "results.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"Passed RSA and ECDSA CA tests ({args.sanitizer}); {count} native sources compiled.")
        if logs:
            print(f"Logs: {logs}")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        print(f"Mbed TLS CA validation failed: {error}", file=sys.stderr)
        sys.exit(1)
