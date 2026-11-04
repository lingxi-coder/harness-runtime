#!/usr/bin/env python3
"""Exercise actual snapshot bodies with real, explicitly supplied Mbed TLS.

Set MBEDTLS_TEST_INCLUDE, MBEDTLS_TEST_LIB_DIR and MBEDTLS_TEST_CERT to a
matching host build and local test certificate. MBEDTLS_TEST_SANITIZER may be
address or thread; thread requires all supplied native archives instrumented.
Set MBEDTLS_TEST_KEY to also run TLS 1.2/1.3 with a matching certificate/key
valid for expected.test. The certificate must be valid at the current time.
This is a host component test, not an Android or full product acceptance test.
"""
from pathlib import Path
import os
import subprocess
import tempfile
import unittest

from test_mbedtls_stream_ownership import SOURCE, function

PREFIX = r'''
#include <assert.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <stdarg.h>
#include <pthread.h>
#include <sched.h>
#include <sys/stat.h>
#include <mbedtls/ssl.h>
#include <mbedtls/error.h>
#include <mbedtls/entropy.h>
#include <mbedtls/ctr_drbg.h>
typedef struct { _Atomic int val; } git_atomic32;
static void git_atomic32_set(git_atomic32 *a, int n) { atomic_store(&a->val, n); }
static int git_atomic32_inc(git_atomic32 *a) { return atomic_fetch_add(&a->val, 1)+1; }
static int git_atomic32_dec(git_atomic32 *a) { return atomic_fetch_sub(&a->val, 1)-1; }
#define git_mutex pthread_mutex_t
#define git_mutex_init(m) pthread_mutex_init(m, NULL)
#define git_mutex_lock pthread_mutex_lock
#define git_mutex_unlock pthread_mutex_unlock
#define git_mutex_free pthread_mutex_destroy
#define ARRAY_SIZE(a) (sizeof(a)/sizeof((a)[0]))
#define GIT_DEFAULT_CERT_LOCATION NULL
#define GIT_ERROR_SSL 1
#define GIT_ASSERT_ARG(x) assert(x)
#define GIT_ERROR_CHECK_ALLOC(x) do { if (!(x)) return -1; } while (0)
static _Atomic int owned_allocations;
static _Thread_local int fail_allocation;
static void *git__calloc(size_t n, size_t size) {
 if (fail_allocation) { fail_allocation = 0; return NULL; }
 void *p = calloc(n, size); if (p) atomic_fetch_add(&owned_allocations, 1); return p;
}
static char *git__strdup(const char *s) {
 char *p = git__calloc(strlen(s)+1, 1); if (p) strcpy(p, s); return p;
}
static void git__free(void *p) {
 if (p) { atomic_fetch_sub(&owned_allocations, 1); free(p); }
}
static char *git__strtok(char **s, const char *delimiter) {
 return strtok_r(*s, delimiter, s);
}
static void git_error_set(int kind, const char *format, ...) { (void)kind; (void)format; }
static int git_runtime_shutdown_register(void (*callback)(void)) { (void)callback; return 0; }
int git_mbedtls__set_cert_location(const char *file, const char *path);
'''

CASES = r'''
static const char *certificate_path;
static const char *missing_path;
static void *reader(void *unused) {
 (void)unused;
 for (int i=0; i<120; ++i) {
  mbedtls_ca_snapshot *a, *b;
  assert(ca_snapshot_acquire(&a) == 0);
  assert(ca_snapshot_acquire(&b) == 0);
  assert(a != b);
  assert(a->ca_chain.raw.p != b->ca_chain.raw.p);
  assert(a->ca_chain.pk.MBEDTLS_PRIVATE(pk_ctx) != b->ca_chain.pk.MBEDTLS_PRIVATE(pk_ctx));
  mbedtls_ssl_context ssl;
  mbedtls_ssl_init(&ssl);
  assert(mbedtls_ssl_setup(&ssl, &a->config) == 0);
  assert(mbedtls_ssl_set_hostname(&ssl, "expected.test") == 0);
  sched_yield();
  assert(a->ca_chain.version && a->ca_chain.raw.p && a->ca_chain.raw.len);
  mbedtls_ssl_free(&ssl);
  ca_snapshot_release(a); ca_snapshot_release(b);
 }
 return NULL;
}
static void *writer(void *unused) {
 (void)unused;
 for (int i=0; i<80; ++i) {
  assert(git_mbedtls__set_cert_location(certificate_path, NULL) == 0);
  assert(git_mbedtls__set_cert_location(missing_path, NULL) < 0);
 }
 return NULL;
}
int main(int argc, char **argv) {
 assert(argc == 7);
 certificate_path = argv[1]; missing_path = argv[2];
 for (int cycle=0; cycle<2; ++cycle) {
  assert(git_mbedtls_stream_global_init() == 0);
  assert(git_mbedtls__set_cert_location(certificate_path, NULL) == 0);
  mbedtls_ca_snapshot *old = current_ca, *held;
  assert(ca_snapshot_acquire(&held) == 0 && held != old);
  const size_t length = held->ca_chain.raw.len;
  assert(git_mbedtls__set_cert_location(missing_path, NULL) < 0 && current_ca == old);
  assert(git_mbedtls__set_cert_location(NULL, argv[3]) < 0 && current_ca == old);
  assert(git_mbedtls__set_cert_location(NULL, argv[4]) < 0 && current_ca == old);
  assert(git_mbedtls__set_cert_location(certificate_path, argv[4]) < 0 && current_ca == old);
  assert(git_mbedtls__set_cert_location(NULL, argv[5]) < 0 && current_ca == old);
  assert(git_mbedtls__set_cert_location(argv[6], argv[3]) < 0 && current_ca == old);
  fail_allocation = 1;
  assert(git_mbedtls__set_cert_location(certificate_path, NULL) < 0 && current_ca == old);
  fail_allocation = 1;
  mbedtls_ca_snapshot *out = NULL;
  assert(ca_snapshot_acquire(&out) < 0 && out == NULL && current_ca == old);
  assert(git_mbedtls__set_cert_location(certificate_path, NULL) == 0);
  assert(held->ca_chain.raw.len == length && held->ca_chain.version);
  ca_snapshot_release(held);
  pthread_t threads[5];
  for (int i=0; i<4; ++i) assert(pthread_create(&threads[i], NULL, reader, NULL) == 0);
  assert(pthread_create(&threads[4], NULL, writer, NULL) == 0);
  for (int i=0; i<5; ++i) assert(pthread_join(threads[i], NULL) == 0);
  shutdown_ssl();
  assert(atomic_load(&owned_allocations) == 0 && current_ca == NULL);
 }
 return 0;
}
'''


class CaSnapshot(unittest.TestCase):
    @unittest.skipUnless(all(os.environ.get(n) for n in (
        "MBEDTLS_TEST_INCLUDE", "MBEDTLS_TEST_LIB_DIR", "MBEDTLS_TEST_CERT", "MBEDTLS_TEST_KEY")),
        "requires a real native build and matching certificate/private key")
    def test_concurrent_real_handshakes(self):
        text = SOURCE.read_text()
        begin = text.index('#define GIT_SSL_DEFAULT_CIPHERS "')
        end = text.index("\nstatic int bio_read(", begin)
        includes = '''
#include <signal.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>
#include <mbedtls/pk.h>
#define GIT_ECERTIFICATE -17
'''
        fixture = Path(__file__).with_name("fixtures") / "mbedtls_ca_handshake.c"
        bodies = (PREFIX + includes + text[begin:end] +
                  function(text, "git_mbedtls__set_cert_location") +
                  function(text, "verify_server_cert") + fixture.read_text())
        with tempfile.TemporaryDirectory(prefix="mbedtls-ca-handshake-") as directory:
            source, binary = Path(directory) / "handshake.c", Path(directory) / "handshake"
            source.write_text(bodies)
            command = [os.environ.get("CC", "cc"), "-std=c11", "-Wall", "-Wextra", "-Werror"]
            sanitizer = os.environ.get("MBEDTLS_TEST_SANITIZER")
            if sanitizer:
                self.assertIn(sanitizer, ("address", "thread"))
                command += ["-fsanitize=" + sanitizer, "-fno-omit-frame-pointer"]
            command += ['-DTEST_CERT="' + os.environ["MBEDTLS_TEST_CERT"] + '"',
                        '-DTEST_KEY="' + os.environ["MBEDTLS_TEST_KEY"] + '"',
                        "-I", os.environ["MBEDTLS_TEST_INCLUDE"], str(source), "-L",
                        os.environ["MBEDTLS_TEST_LIB_DIR"], "-lmbedtls", "-lmbedx509",
                        "-lmbedcrypto", "-lpthread", "-o", str(binary)]
            subprocess.run(command, check=True)
            subprocess.run([str(binary)], check=True, timeout=90)

    @unittest.skipUnless(all(os.environ.get(n) for n in (
        "MBEDTLS_TEST_INCLUDE", "MBEDTLS_TEST_LIB_DIR", "MBEDTLS_TEST_CERT")),
        "requires an explicitly identified real native Mbed TLS build")
    def test_real_configuration_lifetimes(self):
        text = SOURCE.read_text()
        begin = text.index('#define GIT_SSL_DEFAULT_CIPHERS "')
        end = text.index("\nstatic int bio_read(", begin)
        bodies = text[begin:end] + function(text, "git_mbedtls__set_cert_location")
        with tempfile.TemporaryDirectory(prefix="mbedtls-ca-snapshot-") as directory:
            root = Path(directory)
            (root / "empty").mkdir()
            (root / "bad").mkdir()
            (root / "bad" / "invalid.pem").write_text("not a certificate")
            certificate = Path(os.environ["MBEDTLS_TEST_CERT"]).read_bytes()
            (root / "mixed").mkdir()
            (root / "mixed" / "valid.pem").write_bytes(certificate)
            (root / "mixed" / "invalid.pem").write_text("not a certificate")
            (root / "partial.pem").write_bytes(certificate + b"\n-----BEGIN CERTIFICATE-----\ninvalid\n-----END CERTIFICATE-----\n")
            source, binary = root / "snapshot.c", root / "snapshot"
            source.write_text(PREFIX + bodies + CASES)
            command = [os.environ.get("CC", "cc"), "-std=c11", "-Wall", "-Wextra", "-Werror"]
            sanitizer = os.environ.get("MBEDTLS_TEST_SANITIZER")
            if sanitizer:
                self.assertIn(sanitizer, ("address", "thread"))
                command += ["-fsanitize=" + sanitizer, "-fno-omit-frame-pointer"]
            command += ["-I", os.environ["MBEDTLS_TEST_INCLUDE"], str(source), "-L",
                        os.environ["MBEDTLS_TEST_LIB_DIR"], "-lmbedtls", "-lmbedx509",
                        "-lmbedcrypto", "-lpthread", "-o", str(binary)]
            subprocess.run(command, check=True)
            subprocess.run([str(binary), os.environ["MBEDTLS_TEST_CERT"], str(root / "missing"),
                            str(root / "empty"), str(root / "bad"), str(root / "mixed"),
                            str(root / "partial.pem")], check=True, timeout=90)


if __name__ == "__main__":
    unittest.main()
