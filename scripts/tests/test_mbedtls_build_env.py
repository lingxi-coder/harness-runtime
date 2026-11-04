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
            upstream_config = include / "mbedtls_config.h"
            upstream_config.write_text("#define FIXTURE_LAYOUT 1\n")
            (include / "fixture.h").write_text(
                '#include "mbedtls/mbedtls_config.h"\n'
                '#ifndef LINGXI_FIXTURE_OFFSET\n#define LINGXI_FIXTURE_OFFSET 0\n#endif\n'
                'struct fixture_abi { char bytes[FIXTURE_LAYOUT + LINGXI_FIXTURE_OFFSET]; };\n'
            )
            (library / "x509.c").write_text(
                '#include "mbedtls/fixture.h"\n'
                'int fixture_x509_size(void) { return sizeof(struct fixture_abi); }\n'
            )
            (library / "debug.c").write_text(
                '#include "mbedtls/fixture.h"\n'
                'int fixture_tls_size(void) { return sizeof(struct fixture_abi); }\n'
            )
            (library / "fixture.c").write_text(
                '#include "mbedtls/fixture.h"\n'
                "#ifndef LINGXI_TEST_VALUE\n#define LINGXI_TEST_VALUE 0\n#endif\n"
                "unsigned int mbedtls_version_get_number(void) { return 0; }\n"
                "int mbedtls_fixture_value(void) { return LINGXI_TEST_VALUE; }\n"
                "int fixture_crypto_size(void) { return sizeof(struct fixture_abi); }\n"
            )
            (consumer / "Cargo.toml").write_text(
                '[package]\nname = "mbedtls-env-consumer"\nversion = "0.1.0"\n'
                'edition = "2021"\n[dependencies]\nmbedtls-sys = { path = "../mbedtls-sys" }\n'
                '[build-dependencies]\ncc = "=1.2.62"\n'
            )
            (consumer / "build.rs").write_text(
                'fn main() { cc::Build::new().include(std::env::var("DEP_MBEDTLS_INCLUDE").unwrap())'
                '.file("probe.c").compile("fixture_consumer"); }\n'
            )
            (consumer / "probe.c").write_text(
                '#include "mbedtls/fixture.h"\n'
                'int fixture_consumer_size(void) { return sizeof(struct fixture_abi); }\n'
            )
            (consumer / "src" / "main.rs").write_text(
                'extern "C" { fn mbedtls_fixture_value() -> i32; fn fixture_consumer_size() -> i32; '
                'fn fixture_tls_size() -> i32; fn fixture_x509_size() -> i32; '
                'fn fixture_crypto_size() -> i32; }\n'
                'fn main() { unsafe { (mbedtls_sys::mbedtls_link_anchor)(); '
                'println!("{} {} {} {} {}", mbedtls_fixture_value(), fixture_consumer_size(), '
                'fixture_tls_size(), fixture_x509_size(), fixture_crypto_size()); } }\n'
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

            def build(expected, layout=1):
                result = subprocess.run(command, env=environment, text=True, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                actual = subprocess.run([binary], check=True, text=True,
                                        capture_output=True).stdout.strip()
                self.assertEqual(actual, " ".join(map(str, [expected] + [layout] * 4)))
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

            # Both the source configuration and the LingXi wrapper affect the
            # ABI. Cargo must rebuild the three archives and the downstream
            # consumer using the newly generated, shared header tree.
            upstream_config.write_text("#define FIXTURE_LAYOUT 7\n")
            build(2, layout=7)
            wrapper_config = seam / "lingxi_config.h"
            wrapper_config.write_text(wrapper_config.read_text().replace(
                '#endif /* LINGXI_MBEDTLS_CONFIG_H */',
                '#define LINGXI_FIXTURE_OFFSET 3\n#endif /* LINGXI_MBEDTLS_CONFIG_H */'))
            build(2, layout=10)

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
                build(2, layout=10)
                self.assertTrue(any(" -c " in line for line in marker.read_text().splitlines()))
                marker.unlink()
                # The raw triple spelling is separate from the underscore form.
                environment["CC_" + target] = str(wrapper)
                build(2, layout=10)
                self.assertTrue(marker.exists())
                marker.unlink()
                del environment["CC_" + target]
                del environment["CC_" + normalized]
                environment["LINGXI_TEST_TOOL"] = shutil.which("ar")
                environment["AR_" + normalized] = str(wrapper)
                build(2, layout=10)
                self.assertTrue(marker.exists())


if __name__ == "__main__":
    unittest.main()
