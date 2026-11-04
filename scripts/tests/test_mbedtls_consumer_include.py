"""Exercise the actual Cargo consumer build scripts with small native mocks.

These tests invoke both build.rs entry points, not copied include-handling
snippets. The mock cc crate records paths without compiling the vendor trees;
Cargo's own handling of unusual target directories is outside this fixture.
"""

from pathlib import Path
import os
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
LIBSSH2 = ROOT / "third_party/libssh2-sys/build.rs"
LIBGIT2 = ROOT / "third_party/git2-rs/libgit2-sys/build.rs"

CC_MOCK = r'''
use std::{env, fs::OpenOptions, io::Write, path::Path};
pub struct Build;
impl Build {
    pub fn new() -> Self { Self }
    pub fn include<P: AsRef<Path>>(&mut self, path: P) -> &mut Self {
        let mut f = OpenOptions::new().create(true).append(true)
            .open(env::var_os("TEST_INCLUDE_LOG").unwrap()).unwrap();
        f.write_all(path.as_ref().as_os_str().as_encoded_bytes()).unwrap();
        f.write_all(&[0]).unwrap();
        self
    }
    pub fn file<P: AsRef<Path>>(&mut self, _: P) -> &mut Self { self }
    pub fn out_dir<P: AsRef<Path>>(&mut self, _: P) -> &mut Self { self }
    pub fn flag(&mut self, _: &str) -> &mut Self { self }
    pub fn warnings(&mut self, _: bool) -> &mut Self { self }
    pub fn define<'a, V: Into<Option<&'a str>>>(&mut self, _: &str, _: V) -> &mut Self { self }
    pub fn compile(&mut self, _: &str) {}
}
'''
PKG_CONFIG_MOCK = r'''
use std::{env, fmt, path::PathBuf};
#[derive(Debug)] pub struct Error;
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "mock unavailable") }
}
impl std::error::Error for Error {}
pub struct Library { pub include_paths: Vec<PathBuf> }
pub struct Config;
impl Config {
    pub fn new() -> Self { Self }
    pub fn range_version<R>(&mut self, _: R) -> &mut Self { self }
    pub fn probe(&mut self, _: &str) -> Result<Library, Error> { find_library("") }
}
pub fn find_library(_: &str) -> Result<Library, Error> {
    if env::var_os("TEST_USE_SYSTEM").is_some() {
        Ok(Library { include_paths: Vec::new() })
    } else { Err(Error) }
}
'''


@unittest.skipUnless(os.name == "posix" and shutil.which("rustc"),
                     "requires a Unix host and Rust compiler")
class MbedtlsConsumerIncludeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix="mbedtls-consumers-")
        cls.root = Path(cls.tmp.name)
        for name, source in (("cc", CC_MOCK), ("pkg_config", PKG_CONFIG_MOCK)):
            rs = cls.root / (name + ".rs")
            rs.write_text(source)
            subprocess.run(["rustc", "--edition=2021", "--crate-name", name,
                            "--crate-type=rlib", str(rs), "-o",
                            str(cls.root / ("lib" + name + ".rlib"))], check=True)
        cls.binaries = {}
        for name, source in (("ssh", LIBSSH2), ("git", LIBGIT2)):
            binary = cls.root / (name + "-build")
            subprocess.run(["rustc", "--edition=2021", str(source),
                            "--extern", "cc=" + str(cls.root / "libcc.rlib"),
                            "--extern", "pkg_config=" + str(cls.root / "libpkg_config.rlib"),
                            "-o", str(binary)], check=True)
            cls.binaries[name] = binary

        cls.ssh = cls.root / "ssh"
        (cls.ssh / "libssh2/include").mkdir(parents=True)
        (cls.ssh / "libssh2/src").mkdir(parents=True)
        (cls.ssh / "libssh2/include/libssh2.h").write_text('#define LIBSSH2_VERSION "1.2.3"\n')
        for name in ("libssh2_publickey.h", "libssh2_sftp.h"):
            (cls.ssh / "libssh2/include" / name).write_text("/* fixture */\n")
        (cls.ssh / "libssh2/src/libssh2_config_cmake.h.in").write_text("/* fixture */\n")
        (cls.ssh / "libssh2/libssh2.pc.in").write_text("prefix=@prefix@\n")

        cls.git = cls.root / "git"
        for name in ("libgit2/include", "libgit2/src/libgit2/transports",
                     "libgit2/src/libgit2/streams", "libgit2/src/util/unix",
                     "libgit2/src/util/win32", "libgit2/deps/llhttp",
                     "libgit2/deps/xdiff", "libgit2/deps/pcre"):
            (cls.git / name).mkdir(parents=True)
        (cls.git / "libgit2/include/git2.h").write_text("/* fixture */\n")

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def setUp(self):
        self.case = tempfile.TemporaryDirectory(dir=self.root, prefix="case-")
        self.case_path = Path(self.case.name)

    def tearDown(self):
        self.case.cleanup()

    def run_build(self, kind, include=None, *, target="x86_64-unknown-linux-gnu",
                  https=True, system=False):
        out = self.case_path / ("out-" + kind)
        out.mkdir(exist_ok=True)
        log = self.case_path / (kind + "-includes.bin")
        log.unlink(missing_ok=True)
        env = os.environ.copy()
        for key in list(env):
            if key.startswith(("DEP_", "CARGO_FEATURE_", "TEST_")) or key in (
                    "LIBGIT2_NO_VENDOR", "LIBSSH2_SYS_USE_PKG_CONFIG"):
                env.pop(key)
        env.update({"TARGET": target, "PROFILE": "debug", "OUT_DIR": str(out),
                    "CARGO_CFG_TARGET_POINTER_WIDTH": "64", "TEST_INCLUDE_LOG": str(log)})
        if include is not None:
            env["DEP_MBEDTLS_INCLUDE"] = os.fspath(include)
        if kind == "git":
            if https:
                env["CARGO_FEATURE_HTTPS"] = "1"
            if system:
                env["LIBGIT2_NO_VENDOR"] = "1"
            else:
                env["CARGO_FEATURE_VENDORED"] = "1"
        elif system:
            env["LIBSSH2_SYS_USE_PKG_CONFIG"] = "1"
        if system:
            env["TEST_USE_SYSTEM"] = "1"
        result = subprocess.run([self.binaries[kind]], cwd=self.git if kind == "git" else self.ssh,
                                env=env, capture_output=True)
        paths = log.read_bytes().split(b"\0")[:-1] if log.exists() else []
        return result, paths

    def assert_include_reaches_both_consumers(self, include):
        (include / "mbedtls").mkdir(parents=True)
        (include / "mbedtls/mbedtls_config.h").write_text("/* fixture */\n")
        for kind in ("ssh", "git"):
            with self.subTest(consumer=kind):
                result, paths = self.run_build(kind, include)
                self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
                self.assertIn(os.fsencode(include), paths)
                self.assertEqual(paths.count(os.fsencode(include)), 1)

    def test_single_path_containing_separator_reaches_both_consumers(self):
        # Injected directly as DEP metadata: this does not claim Cargo accepts
        # the separator in CARGO_TARGET_DIR.
        self.assert_include_reaches_both_consumers(self.case_path / "include:single")

    def test_non_utf8_path_reaches_both_consumers_when_supported_by_filesystem(self):
        include = self.case_path / os.fsdecode(b"include-\xff")
        try:
            include.mkdir()
        except OSError as error:
            self.skipTest(f"filesystem cannot create non-UTF-8 path: {error}")
        self.assert_include_reaches_both_consumers(include)

    def test_missing_empty_and_invalid_metadata_fail_closed(self):
        invalid = self.case_path / "without-config"
        invalid.mkdir()
        non_utf8_missing = self.case_path / os.fsdecode(b"missing-\xff")
        for kind in ("ssh", "git"):
            for include in (None, "", invalid, non_utf8_missing):
                with self.subTest(consumer=kind, include=include):
                    result, _ = self.run_build(kind, include)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(b"DEP_MBEDTLS_INCLUDE", result.stderr)

    def test_paths_without_mbedtls_backend_do_not_require_metadata(self):
        for target, https in (("x86_64-unknown-linux-gnu", False),
                              ("aarch64-apple-darwin", True),
                              ("x86_64-pc-windows-msvc", True)):
            with self.subTest(target=target, https=https):
                result, _ = self.run_build("git", target=target, https=https)
                self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        for kind in ("ssh", "git"):
            with self.subTest(consumer=kind, backend="system"):
                result, _ = self.run_build(kind, system=True)
                self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))

    def test_vendored_windows_ssh_still_requires_the_include(self):
        target = "x86_64-pc-windows-msvc"
        result, _ = self.run_build("ssh", target=target)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"DEP_MBEDTLS_INCLUDE", result.stderr)
        include = self.case_path / "valid-include"
        (include / "mbedtls").mkdir(parents=True)
        (include / "mbedtls/mbedtls_config.h").write_text("/* fixture */\n")
        result, paths = self.run_build("ssh", include, target=target)
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        self.assertIn(os.fsencode(include), paths)


if __name__ == "__main__":
    unittest.main()
