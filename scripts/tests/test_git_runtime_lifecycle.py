#!/usr/bin/env python3
"""Compile the actual libgit2 runtime and test init failure/retry lifecycles."""
import os
from pathlib import Path
import platform
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

    def test_rand_initializer_faults_and_retry(self):
        with tempfile.TemporaryDirectory(prefix="git-rand-lifecycle-") as temp:
            temp = Path(temp)
            rand_object = temp / "rand.o"
            binary = temp / "rand-lifecycle"
            flags = [os.environ.get("CC", "cc"), "-std=c11", "-Wall", "-Wextra", "-Werror",
                     "-D_POSIX_C_SOURCE=200809L", "-D_DEFAULT_SOURCE", "-D_DARWIN_C_SOURCE",
                     "-DLIBGIT2_NO_FEATURES_H",
                     "-DGIT_THREADS", "-DGIT_ARCH_64", "-pthread",
                     "-I" + str(GIT2 / "src/util"), "-I" + str(GIT2 / "include")]
            sanitizer = os.environ.get("GIT_RUNTIME_SANITIZER")
            if sanitizer:
                self.assertIn(sanitizer, ("address", "thread"))
                flags += ["-fsanitize=" + sanitizer, "-fno-omit-frame-pointer"]
            injected = ["-DGIT_RAND_GETENTROPY", "-Dgetentropy=audit_rand_getentropy",
                        "-Dgettimeofday=audit_rand_gettimeofday",
                        "-include", str(ROOT / "scripts/tests/fixtures/git_rand_overrides.h"),
                        "-Dpthread_mutex_init=audit_rand_mutex_init",
                        "-Dpthread_mutex_destroy=audit_rand_mutex_destroy",
                        "-Dpthread_mutex_lock=audit_rand_mutex_lock",
                        "-Dpthread_mutex_unlock=audit_rand_mutex_unlock"]
            subprocess.run(flags + injected + ["-c", str(GIT2 / "src/util/rand.c"),
                                               "-o", str(rand_object)], check=True, timeout=60)
            subprocess.run(flags + [str(GIT2 / "src/util/runtime.c"),
                                    str(ROOT / "scripts/tests/fixtures/git_rand_lifecycle.c"),
                                    str(rand_object), "-o", str(binary)], check=True, timeout=60)
            for case in ("seed-zero", "seed-error", "mutex-error", "register-full", "retry"):
                with self.subTest(case=case):
                    subprocess.run([str(binary), case], check=True, timeout=30)

    def test_filter_initializer_faults_and_retry(self):
        with tempfile.TemporaryDirectory(prefix="git-filter-lifecycle-") as temp:
            temp = Path(temp)
            binary = temp / "filter-lifecycle"
            flags = [os.environ.get("CC", "cc"), "-std=c11", "-Wall", "-Wextra", "-Werror",
                     "-D_POSIX_C_SOURCE=200809L", "-D_DEFAULT_SOURCE", "-D_DARWIN_C_SOURCE",
                     "-DLIBGIT2_NO_FEATURES_H", "-DGIT_THREADS", "-DGIT_ARCH_64",
                     "-DGIT_SHA1_COLLISIONDETECT", "-DGIT_SHA256_BUILTIN",
                     "-Wno-missing-field-initializers",
                     "-ffunction-sections", "-fdata-sections", "-pthread",
                     "-I" + str(GIT2 / "src/util"), "-I" + str(GIT2 / "src/libgit2"),
                     "-I" + str(GIT2 / "include")]
            sanitizer = os.environ.get("GIT_RUNTIME_SANITIZER")
            if sanitizer:
                self.assertIn(sanitizer, ("address", "thread"))
                flags += ["-fsanitize=" + sanitizer, "-fno-omit-frame-pointer"]
            obj = temp / "filter.o"
            subprocess.run(flags + ["-Dpthread_rwlock_init=audit_filter_rwlock_init",
                                    "-Dpthread_rwlock_destroy=audit_filter_rwlock_destroy",
                                    "-c", str(GIT2 / "src/libgit2/filter.c"),
                                    "-o", str(obj)], check=True, timeout=60)
            sources = [GIT2 / "src/util/runtime.c", GIT2 / "src/util/vector.c",
                       GIT2 / "src/util/str.c", GIT2 / "src/util/alloc.c",
                       GIT2 / "src/util/allocators/failalloc.c",
                       ROOT / "scripts/tests/fixtures/git_filter_lifecycle.c"]
            strip_flag = "-Wl,-dead_strip" if platform.system() == "Darwin" else "-Wl,--gc-sections"
            subprocess.run(flags + [*(str(source) for source in sources), str(obj), strip_flag,
                                    "-o", str(binary)], check=True, timeout=60)
            result = subprocess.run([str(binary), "success"], check=True, timeout=30,
                                    capture_output=True, text=True)
            allocations = int(result.stdout.strip().split("=")[1])
            self.assertGreater(allocations, 0)
            for case in ("lock", "register", *(str(i) for i in range(1, allocations + 1))):
                with self.subTest(case=case):
                    subprocess.run([str(binary), case], check=True, timeout=30)

    def test_sysdir_initializer_faults_and_retry(self):
        with tempfile.TemporaryDirectory(prefix="git-sysdir-lifecycle-") as temp:
            temp = Path(temp)
            binary = temp / "sysdir-lifecycle"
            flags = [os.environ.get("CC", "cc"), "-std=c11", "-Wall", "-Wextra", "-Werror",
                     "-D_POSIX_C_SOURCE=200809L", "-D_DEFAULT_SOURCE", "-D_DARWIN_C_SOURCE",
                     "-DLIBGIT2_NO_FEATURES_H", "-DGIT_THREADS", "-DGIT_ARCH_64",
                     "-DGIT_SHA1_COLLISIONDETECT", "-DGIT_SHA256_BUILTIN",
                     "-Wno-missing-field-initializers", "-ffunction-sections", "-fdata-sections",
                     "-pthread", "-I" + str(GIT2 / "src/util"),
                     "-I" + str(GIT2 / "src/libgit2"), "-I" + str(GIT2 / "include")]
            sanitizer = os.environ.get("GIT_RUNTIME_SANITIZER")
            if sanitizer:
                self.assertIn(sanitizer, ("address", "thread"))
                flags += ["-fsanitize=" + sanitizer, "-fno-omit-frame-pointer"]
            obj = temp / "sysdir.o"
            injected = ["-Dgetenv=audit_sysdir_getenv", "-Dgetuid=audit_sysdir_getuid",
                        "-Dgeteuid=audit_sysdir_geteuid", "-Dsysconf=audit_sysdir_sysconf",
                        "-Dgetpwuid_r=audit_sysdir_getpwuid_r"]
            subprocess.run(flags + injected + ["-c", str(GIT2 / "src/libgit2/sysdir.c"),
                                               "-o", str(obj)], check=True, timeout=60)
            sources = [GIT2 / "src/util/runtime.c", GIT2 / "src/util/str.c",
                       GIT2 / "src/util/alloc.c", GIT2 / "src/util/allocators/failalloc.c",
                       ROOT / "scripts/tests/fixtures/git_sysdir_lifecycle.c"]
            strip_flag = "-Wl,-dead_strip" if platform.system() == "Darwin" else "-Wl,--gc-sections"
            subprocess.run(flags + [*(str(source) for source in sources), str(obj), strip_flag,
                                    "-o", str(binary)], check=True, timeout=60)
            for mode in ("env", "passwd"):
                result = subprocess.run([str(binary), mode, "success"], check=True, timeout=30,
                                        capture_output=True, text=True)
                allocations = int(result.stdout.strip().split("=")[1])
                self.assertGreater(allocations, 0)
                cases = ["register", *(str(i) for i in range(1, allocations + 1))]
                if mode == "passwd":
                    cases.append("no-entry")
                for case in cases:
                    with self.subTest(mode=mode, case=case):
                        subprocess.run([str(binary), mode, case], check=True, timeout=30)

    def test_merge_initializer_faults_and_retry(self):
        with tempfile.TemporaryDirectory(prefix="git-merge-lifecycle-") as temp:
            temp = Path(temp)
            binary = temp / "merge-lifecycle"
            flags = [os.environ.get("CC", "cc"), "-std=c11", "-Wall", "-Wextra", "-Werror",
                     "-D_POSIX_C_SOURCE=200809L", "-D_DEFAULT_SOURCE", "-D_DARWIN_C_SOURCE",
                     "-DLIBGIT2_NO_FEATURES_H", "-DGIT_THREADS", "-DGIT_ARCH_64",
                     "-DGIT_SHA1_COLLISIONDETECT", "-DGIT_SHA256_BUILTIN",
                     "-Wno-missing-field-initializers", "-ffunction-sections", "-fdata-sections",
                     "-pthread", "-I" + str(GIT2 / "src/util"),
                     "-I" + str(GIT2 / "src/libgit2"), "-I" + str(GIT2 / "include")]
            sanitizer = os.environ.get("GIT_RUNTIME_SANITIZER")
            if sanitizer:
                self.assertIn(sanitizer, ("address", "thread"))
                flags += ["-fsanitize=" + sanitizer, "-fno-omit-frame-pointer"]
            obj = temp / "merge.o"
            injected = ["-Dpthread_rwlock_init=audit_merge_rwlock_init",
                        "-Dpthread_rwlock_destroy=audit_merge_rwlock_destroy",
                        "-Dgit_vector_insert_sorted=audit_merge_vector_insert_sorted"]
            subprocess.run(flags + injected + ["-c", str(GIT2 / "src/libgit2/merge_driver.c"),
                                               "-o", str(obj)], check=True, timeout=60)
            sources = [GIT2 / "src/util/runtime.c", GIT2 / "src/util/vector.c",
                       GIT2 / "src/util/alloc.c", GIT2 / "src/util/allocators/failalloc.c",
                       ROOT / "scripts/tests/fixtures/git_merge_lifecycle.c"]
            strip_flag = "-Wl,-dead_strip" if platform.system() == "Darwin" else "-Wl,--gc-sections"
            subprocess.run(flags + [*(str(source) for source in sources), str(obj), strip_flag,
                                    "-o", str(binary)], check=True, timeout=60)
            result = subprocess.run([str(binary), "success"], check=True, timeout=30,
                                    capture_output=True, text=True)
            allocations = int(result.stdout.strip().split("=")[1])
            self.assertGreater(allocations, 0)
            for case in ("lock", "register", "insert1", "insert2", "insert3",
                         *(str(i) for i in range(1, allocations + 1))):
                with self.subTest(case=case):
                    subprocess.run([str(binary), case], check=True, timeout=30)

    def test_winsock_initializer_faults_and_retry(self):
        with tempfile.TemporaryDirectory(prefix="git-winsock-lifecycle-") as temp:
            temp = Path(temp)
            binary = temp / "winsock-lifecycle"
            source = (GIT2 / "src/libgit2/streams/socket.c").read_text()
            start = source.index("static void socket_stream_global_shutdown(void)")
            end = source.index("\n#else\n", start)
            snippet = source[start:end]
            self.assertIn("int git_socket_stream_global_init(void)", snippet)
            extracted = temp / "socket-windows-init.c"
            extracted.write_text(snippet)
            flags = [os.environ.get("CC", "cc"), "-std=c11", "-Wall", "-Wextra", "-Werror",
                     "-D_POSIX_C_SOURCE=200809L", "-DLIBGIT2_NO_FEATURES_H", "-DGIT_THREADS",
                     "-DGIT_ARCH_64", "-pthread", "-I" + str(GIT2 / "src/util"),
                     "-I" + str(GIT2 / "include"),
                     "-I" + str(ROOT / "scripts/tests/fixtures")]
            sanitizer = os.environ.get("GIT_RUNTIME_SANITIZER")
            if sanitizer:
                self.assertIn(sanitizer, ("address", "thread"))
                flags += ["-fsanitize=" + sanitizer, "-fno-omit-frame-pointer"]
            obj = temp / "winsock.o"
            subprocess.run(flags + ["-include", str(ROOT / "scripts/tests/fixtures/git_winsock_overrides.h"),
                                    "-c", str(extracted), "-o", str(obj)], check=True, timeout=60)
            subprocess.run(flags + [str(GIT2 / "src/util/runtime.c"),
                                    str(ROOT / "scripts/tests/fixtures/git_winsock_lifecycle.c"),
                                    str(obj), "-o", str(binary)], check=True, timeout=60)
            for case in ("startup", "version", "register"):
                with self.subTest(case=case):
                    subprocess.run([str(binary), case], check=True, timeout=30)


if __name__ == "__main__":
    unittest.main(verbosity=2)
