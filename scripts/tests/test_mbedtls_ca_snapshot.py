#!/usr/bin/env python3
"""Exercise actual snapshot bodies with real, explicitly supplied Mbed TLS.

Set MBEDTLS_TEST_INCLUDE, MBEDTLS_TEST_LIB_DIR and MBEDTLS_TEST_CERT_A/B to a
matching host build and two distinct local authorities. MBEDTLS_TEST_SANITIZER may be
address or thread; thread requires all supplied native archives instrumented.
Set MBEDTLS_TEST_KEY_A/B to also run TLS 1.2/1.3 with matching certificate/key
pairs valid for expected.test. Certificates must be currently valid.
Set MBEDTLS_TEST_REQUIRE=1 to fail instead of skip when any input is missing.
This is a host component test, not an Android or full product acceptance test.
"""
from pathlib import Path
import os
import subprocess
import tempfile
import unittest

from test_mbedtls_stream_ownership import SOURCE, function


def setUpModule():
    if os.environ.get("MBEDTLS_TEST_REQUIRE") == "1":
        required = ("MBEDTLS_TEST_INCLUDE", "MBEDTLS_TEST_LIB_DIR", "MBEDTLS_TEST_CERT_A",
                    "MBEDTLS_TEST_CERT_B", "MBEDTLS_TEST_KEY_A", "MBEDTLS_TEST_KEY_B")
        missing = [name for name in required if not os.environ.get(name)]
        if missing:
            raise RuntimeError("Missing required Mbed TLS test inputs: " + ", ".join(missing))

PREFIX = r'''
#define _POSIX_C_SOURCE 200809L
#include <assert.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <stdarg.h>
#include <pthread.h>
#include <sched.h>
#include <time.h>
#include <sys/stat.h>
#include <mbedtls/ssl.h>
#include <mbedtls/error.h>
#include <mbedtls/entropy.h>
#include <mbedtls/ctr_drbg.h>
#include <mbedtls/platform.h>
#include <psa/crypto.h>
#if defined(MBEDTLS_SSL_PROTO_TLS1_3)
static pthread_mutex_t psa_test_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t psa_test_condition = PTHREAD_COND_INITIALIZER;
static bool psa_inject_failure, psa_init_entered, psa_init_release;
#if defined(__APPLE__) && defined(MBEDTLS_PLATFORM_HAVE_DEV_RANDOM)
static bool psa_native_failure_observed;
#endif
static psa_status_t test_psa_crypto_init(void) {
 pthread_mutex_lock(&psa_test_mutex);
 if (psa_inject_failure) {
  psa_init_entered = true;
  pthread_cond_broadcast(&psa_test_condition);
  while (!psa_init_release) pthread_cond_wait(&psa_test_condition, &psa_test_mutex);
  psa_inject_failure = false;
  pthread_mutex_unlock(&psa_test_mutex);
#if defined(__APPLE__) && defined(MBEDTLS_PLATFORM_HAVE_DEV_RANDOM)
  /* On macOS the native entropy poll reads this configurable device path.
     Only this isolated test process is affected; waiting readers are joined. */
  const char *saved_random = mbedtls_platform_dev_random;
  mbedtls_platform_dev_random = "/nonexistent/lingxi-psa-entropy-test";
  psa_status_t result = psa_crypto_init();
  mbedtls_platform_dev_random = saved_random;
  assert(result != PSA_SUCCESS);
  psa_native_failure_observed = true;
  printf("native PSA entropy failure=%d observed\n", (int)result);
  return result;
#else
  return PSA_ERROR_INSUFFICIENT_ENTROPY;
#endif
 }
 pthread_mutex_unlock(&psa_test_mutex);
 return psa_crypto_init();
}
#define psa_crypto_init test_psa_crypto_init
#endif
typedef struct { _Atomic int val; } git_atomic32;
static void git_atomic32_set(git_atomic32 *a, int n) { atomic_store(&a->val, n); }
static int git_atomic32_inc(git_atomic32 *a) { return atomic_fetch_add(&a->val, 1)+1; }
static int git_atomic32_dec(git_atomic32 *a) { return atomic_fetch_sub(&a->val, 1)-1; }
#define git_mutex pthread_mutex_t
#define git_mutex_init(m) pthread_mutex_init(m, NULL)
static _Thread_local int fail_lock;
static int test_mutex_lock(pthread_mutex_t *mutex) {
 if (fail_lock) { fail_lock = 0; return -1; }
 return pthread_mutex_lock(mutex);
}
#define git_mutex_lock test_mutex_lock
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
static const char *certificate_paths[2];
static const char *missing_path;
#if defined(MBEDTLS_SSL_PROTO_TLS1_3)
static void *initialize_with_failure(void *argument) {
 *(int *)argument = git_mbedtls_stream_global_init();
 return NULL;
}
static void *acquire_before_publication(void *unused) {
 (void)unused;
 mbedtls_ca_snapshot *snapshot = NULL;
 assert(ca_snapshot_acquire(&snapshot) < 0 && snapshot == NULL);
 return NULL;
}
#endif
static int trusted(mbedtls_x509_crt *certificate, mbedtls_ca_snapshot *snapshot) {
 uint32_t flags = 0;
 int result = mbedtls_x509_crt_verify(certificate, &snapshot->ca_chain,
                                      NULL, "expected.test", &flags, NULL, NULL);
 return result == 0 && flags == 0;
}
static void *reader(void *unused) {
 (void)unused;
 mbedtls_x509_crt cert_a, cert_b;
 mbedtls_x509_crt_init(&cert_a); mbedtls_x509_crt_init(&cert_b);
 assert(mbedtls_x509_crt_parse_file(&cert_a, certificate_paths[0]) == 0);
 assert(mbedtls_x509_crt_parse_file(&cert_b, certificate_paths[1]) == 0);
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
  /* A snapshot must trust exactly one of the distinct authorities. */
  assert(trusted(&cert_a, a) != trusted(&cert_b, a));
  assert(trusted(&cert_a, b) != trusted(&cert_b, b));
  mbedtls_ssl_free(&ssl);
  ca_snapshot_release(a); ca_snapshot_release(b);
 }
 mbedtls_x509_crt_free(&cert_a); mbedtls_x509_crt_free(&cert_b);
 return NULL;
}
static void *writer(void *unused) {
 (void)unused;
 for (int i=0; i<80; ++i) {
  assert(git_mbedtls__set_cert_location(certificate_paths[i % 2], NULL) == 0);
  assert(git_mbedtls__set_cert_location(missing_path, NULL) < 0);
 }
 return NULL;
}
int main(int argc, char **argv) {
 assert(argc == 8);
 certificate_paths[0] = argv[1]; certificate_paths[1] = argv[2]; missing_path = argv[3];
 mbedtls_x509_crt cert_a, cert_b;
 mbedtls_x509_crt_init(&cert_a); mbedtls_x509_crt_init(&cert_b);
 assert(mbedtls_x509_crt_parse_file(&cert_a, certificate_paths[0]) == 0);
 assert(mbedtls_x509_crt_parse_file(&cert_b, certificate_paths[1]) == 0);
 assert(cert_a.raw.len != cert_b.raw.len || memcmp(cert_a.raw.p, cert_b.raw.p, cert_a.raw.len));
#if defined(MBEDTLS_SSL_PROTO_TLS1_3)
 /* Keep initialization inside the PSA call while other threads try to acquire
    a snapshot. No stream may receive one before success is published. */
 psa_inject_failure = true;
 int init_result = 0;
 pthread_t initializer, early_readers[8];
 assert(pthread_create(&initializer, NULL, initialize_with_failure, &init_result) == 0);
 pthread_mutex_lock(&psa_test_mutex);
 struct timespec deadline;
 assert(clock_gettime(CLOCK_REALTIME, &deadline) == 0);
 deadline.tv_sec += 10;
 int wait_result = 0;
 while (!psa_init_entered && wait_result == 0)
  wait_result = pthread_cond_timedwait(&psa_test_condition, &psa_test_mutex, &deadline);
 assert(psa_init_entered);
 pthread_mutex_unlock(&psa_test_mutex);
 for (int i=0; i<8; ++i) assert(pthread_create(&early_readers[i], NULL, acquire_before_publication, NULL) == 0);
 for (int i=0; i<8; ++i) assert(pthread_join(early_readers[i], NULL) == 0);
 pthread_mutex_lock(&psa_test_mutex); psa_init_release = true;
 pthread_cond_broadcast(&psa_test_condition); pthread_mutex_unlock(&psa_test_mutex);
 assert(pthread_join(initializer, NULL) == 0);
 assert(init_result == -1 && current_ca == NULL && !ca_lock_initialized);
 assert(atomic_load(&owned_allocations) == 0);
#if defined(__APPLE__) && defined(MBEDTLS_PLATFORM_HAVE_DEV_RANDOM)
 assert(psa_native_failure_observed);
#endif
#endif
 for (int cycle=0; cycle<2; ++cycle) {
  assert(git_mbedtls_stream_global_init() == 0);
  assert(git_mbedtls__set_cert_location(certificate_paths[0], NULL) == 0);
  mbedtls_ca_snapshot *old = current_ca, *held;
  assert(ca_snapshot_acquire(&held) == 0 && held != old);
  assert(trusted(&cert_a, held) && !trusted(&cert_b, held));
  const size_t length = held->ca_chain.raw.len;
  assert(git_mbedtls__set_cert_location(missing_path, NULL) < 0 && current_ca == old);
  assert(git_mbedtls__set_cert_location(NULL, argv[4]) < 0 && current_ca == old);
  assert(git_mbedtls__set_cert_location(NULL, argv[5]) < 0 && current_ca == old);
  assert(git_mbedtls__set_cert_location(certificate_paths[0], argv[5]) < 0 && current_ca == old);
  assert(git_mbedtls__set_cert_location(NULL, argv[6]) < 0 && current_ca == old);
  assert(git_mbedtls__set_cert_location(argv[7], argv[4]) < 0 && current_ca == old);
  fail_allocation = 1;
  assert(git_mbedtls__set_cert_location(certificate_paths[1], NULL) < 0 && current_ca == old);
  fail_allocation = 1;
  mbedtls_ca_snapshot *out = NULL;
  assert(ca_snapshot_acquire(&out) < 0 && out == NULL && current_ca == old);
  fail_lock = 1;
  assert(git_mbedtls__set_cert_location(certificate_paths[1], NULL) < 0 && current_ca == old);
  fail_lock = 1;
  assert(ca_snapshot_acquire(&out) < 0 && out == NULL && current_ca == old);
  assert(git_mbedtls__set_cert_location(certificate_paths[1], NULL) == 0);
  mbedtls_ca_snapshot *newer = NULL;
  assert(ca_snapshot_acquire(&newer) == 0);
  assert(trusted(&cert_a, held) && !trusted(&cert_b, held));
  assert(!trusted(&cert_a, newer) && trusted(&cert_b, newer));
  assert(held->ca_chain.raw.len == length && held->ca_chain.version);
  ca_snapshot_release(held); ca_snapshot_release(newer);
  pthread_t threads[5];
  for (int i=0; i<4; ++i) assert(pthread_create(&threads[i], NULL, reader, NULL) == 0);
  assert(pthread_create(&threads[4], NULL, writer, NULL) == 0);
  for (int i=0; i<5; ++i) assert(pthread_join(threads[i], NULL) == 0);
  shutdown_ssl();
  assert(atomic_load(&owned_allocations) == 0 && current_ca == NULL);
 }
 mbedtls_x509_crt_free(&cert_a); mbedtls_x509_crt_free(&cert_b);
 return 0;
}
'''


class CaSnapshot(unittest.TestCase):
    @unittest.skipUnless(all(os.environ.get(n) for n in (
        "MBEDTLS_TEST_INCLUDE", "MBEDTLS_TEST_LIB_DIR", "MBEDTLS_TEST_CERT_A",
        "MBEDTLS_TEST_CERT_B", "MBEDTLS_TEST_KEY_A", "MBEDTLS_TEST_KEY_B")),
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
            command += ['-DTEST_CERT_A="' + os.environ["MBEDTLS_TEST_CERT_A"] + '"',
                        '-DTEST_CERT_B="' + os.environ["MBEDTLS_TEST_CERT_B"] + '"',
                        '-DTEST_KEY_A="' + os.environ["MBEDTLS_TEST_KEY_A"] + '"',
                        '-DTEST_KEY_B="' + os.environ["MBEDTLS_TEST_KEY_B"] + '"',
                        "-I", os.environ["MBEDTLS_TEST_INCLUDE"], str(source), "-L",
                        os.environ["MBEDTLS_TEST_LIB_DIR"], "-lmbedtls", "-lmbedx509",
                        "-lmbedcrypto", "-lpthread", "-o", str(binary)]
            subprocess.run(command, check=True)
            subprocess.run([str(binary)], check=True, timeout=90)

    @unittest.skipUnless(all(os.environ.get(n) for n in (
        "MBEDTLS_TEST_INCLUDE", "MBEDTLS_TEST_LIB_DIR", "MBEDTLS_TEST_CERT_A", "MBEDTLS_TEST_CERT_B")),
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
            certificate = Path(os.environ["MBEDTLS_TEST_CERT_A"]).read_bytes()
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
            subprocess.run([str(binary), os.environ["MBEDTLS_TEST_CERT_A"], os.environ["MBEDTLS_TEST_CERT_B"], str(root / "missing"),
                            str(root / "empty"), str(root / "bad"), str(root / "mixed"),
                            str(root / "partial.pem")], check=True, timeout=90)


if __name__ == "__main__":
    unittest.main()
