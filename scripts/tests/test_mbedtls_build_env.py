"""Exercise Cargo's incremental rebuilds for the actual mbedTLS build script.

The fixture uses three tiny C files, not the vendored cryptographic sources.
It needs a host C toolchain and cc 1.2.62 available to Cargo in offline mode.
"""

from pathlib import Path
import os
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SEAM = ROOT / "third_party" / "mbedtls-sys"
CC_VARIABLES = ("CC", "CFLAGS", "AR", "ARFLAGS", "RANLIB", "RANLIBFLAGS")


class MbedtlsBuildEnvironmentTest(unittest.TestCase):
    @unittest.skipUnless(shutil.which("cargo") and shutil.which("cc") and shutil.which("ar"),
                         "requires Cargo, a C compiler and ar")
    def test_environment_changes_rebuild_native_archives(self):
        with tempfile.TemporaryDirectory(prefix="mbedtls-env-") as temp:
            root = Path(temp)
            seam = root / "mbedtls-sys"
            library = root / "mbedtls" / "library"
            include = root / "mbedtls" / "include" / "mbedtls"
            consumer = root / "consumer"
            for directory in (seam, library, include, consumer / "src"):
                directory.mkdir(parents=True)
            for name in ("build.rs", "lib.rs", "lingxi_config.h"):
                shutil.copyfile(SEAM / name, seam / name)
            (seam / "Cargo.toml").write_text(
                '[package]\nname = "mbedtls-sys"\nversion = "3.6.7"\n'
                'edition = "2021"\nlinks = "mbedtls"\nbuild = "build.rs"\n'
                '[lib]\npath = "lib.rs"\n[build-dependencies]\ncc = "=1.2.62"\n'
            )
            (include / "mbedtls_config.h").write_text("/* fixture configuration */\n")
            (library / "x509.c").write_text("int fixture_x509(void) { return 0; }\n")
            (library / "debug.c").write_text("int fixture_tls(void) { return 0; }\n")
            (library / "fixture.c").write_text(
                "#ifndef LINGXI_TEST_VALUE\n#define LINGXI_TEST_VALUE 0\n#endif\n"
                "unsigned int mbedtls_version_get_number(void) { return 0; }\n"
                "int mbedtls_fixture_value(void) { return LINGXI_TEST_VALUE; }\n"
            )
            (consumer / "Cargo.toml").write_text(
                '[package]\nname = "mbedtls-env-consumer"\nversion = "0.1.0"\n'
                'edition = "2021"\n[dependencies]\nmbedtls-sys = { path = "../mbedtls-sys" }\n'
            )
            (consumer / "src" / "main.rs").write_text(
                'extern "C" { fn mbedtls_fixture_value() -> i32; }\n'
                'fn main() { unsafe { (mbedtls_sys::mbedtls_link_anchor)(); '
                'println!("{}", mbedtls_fixture_value()); } }\n'
            )

            environment = os.environ.copy()
            # Do not let the invoking shell's toolchain overrides mask a case.
            for key in list(environment):
                if any(key == name or key.startswith(name + "_") or
                       key in ("HOST_" + name, "TARGET_" + name)
                       for name in CC_VARIABLES):
                    environment.pop(key)
            environment.pop("CC_FORCE_DISABLE", None)
            rustc_info = subprocess.run(["rustc", "-vV"], check=True, text=True,
                                        capture_output=True).stdout
            target = next(line.split(": ", 1)[1] for line in rustc_info.splitlines()
                          if line.startswith("host: "))
            normalized = target.replace("-", "_").replace(".", "_")
            target_dir = root / "target"
            command = ["cargo", "build", "--offline", "-vv", "--manifest-path",
                       str(consumer / "Cargo.toml"), "--target-dir", str(target_dir)]
            binary = target_dir / "debug" / ("mbedtls-env-consumer.exe"
                                             if os.name == "nt" else "mbedtls-env-consumer")

            def build(expected):
                result = subprocess.run(command, env=environment, text=True, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                actual = subprocess.run([binary], check=True, text=True,
                                        capture_output=True).stdout.strip()
                self.assertEqual(actual, str(expected))
                return result.stdout + result.stderr

            environment["CFLAGS"] = "-DLINGXI_TEST_VALUE=1"
            build(1)
            self.assertIn("Fresh mbedtls-sys", build(1))
            environment["CFLAGS"] = "-DLINGXI_TEST_VALUE=2"
            build(2)
            environment["CFLAGS_" + normalized] = "-DLINGXI_TEST_VALUE=3"
            build(3)
            del environment["CFLAGS_" + normalized]
            build(2)

            output_files = list(target_dir.glob("debug/build/mbedtls-sys-*/output"))
            self.assertEqual(len(output_files), 1)
            metadata = output_files[0].read_text().splitlines()
            self.assertIn("cargo:rerun-if-env-changed=CFLAGS_" + target, metadata)
            self.assertIn("cargo:rerun-if-env-changed=CFLAGS_" + normalized, metadata)
            self.assertIn("cargo:rerun-if-env-changed=HOST_CFLAGS", metadata)
            self.assertIn("cargo:rerun-if-env-changed=AR_" + normalized, metadata)
            for variable in ("RUSTC_WRAPPER", "RUSTC_LINKER", "ZERO_AR_DATE",
                             "WASI_SDK_PATH", "WASI_SYSROOT", "WASM_MUSL_SYSROOT"):
                self.assertIn("cargo:rerun-if-env-changed=" + variable, metadata)
            self.assertFalse(any(line.startswith("cargo:rustc-link-lib=") for line in metadata))

            if os.name != "nt":
                # A real compiler and archiver wrapper show that both selected
                # target-specific tools are executed after an incremental change.
                wrapper = root / "tool-wrapper"
                wrapper.write_text('#!/bin/sh\nprintf "%s\\n" "$0 $*" >> "$LINGXI_TEST_LOG"\n'
                                   'exec "$LINGXI_TEST_TOOL" "$@"\n')
                wrapper.chmod(0o755)
                marker = root / "tool-log"
                environment["LINGXI_TEST_LOG"] = str(marker)
                environment["LINGXI_TEST_TOOL"] = shutil.which("cc")
                environment["CC_" + normalized] = str(wrapper)
                build(2)
                self.assertTrue(any(" -c " in line for line in marker.read_text().splitlines()))
                marker.unlink()
                # The raw triple spelling is separate from the underscore form.
                environment["CC_" + target] = str(wrapper)
                build(2)
                self.assertTrue(marker.exists())
                marker.unlink()
                del environment["CC_" + target]
                del environment["CC_" + normalized]
                environment["LINGXI_TEST_TOOL"] = shutil.which("ar")
                environment["AR_" + normalized] = str(wrapper)
                build(2)
                self.assertTrue(marker.exists())


if __name__ == "__main__":
    unittest.main()
