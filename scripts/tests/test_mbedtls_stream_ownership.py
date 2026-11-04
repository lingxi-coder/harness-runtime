#!/usr/bin/env python3
"""Check real stream lifecycle bodies with deterministic injected failures.

The small C harness substitutes allocation and TLS setup, not the product cleanup
branches. Hostname tests also inject setter failures before handshake. These
checks do not test TLS protocol correctness or the CA configuration lifecycle.
"""
from pathlib import Path
import os
import re
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "third_party/git2-rs/libgit2-sys/libgit2/src/libgit2/streams/mbedtls.c"


def function(text, name):
    match = re.search(r"^(?:static )?(?:int|void) " + name + r"\(", text, re.M)
    if match is None:
        raise AssertionError(f"Missing product function {name}")
    opening = text.index("{", match.start())
    depth = 0
    for position in range(opening, len(text)):
        if text[position] == "{":
            depth += 1
        elif text[position] == "}":
            depth -= 1
            if depth == 0:
                return text[match.start():position + 1]
    raise AssertionError(f"Unclosed product function {name}")


PREFIX = r'''
#include <assert.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
typedef struct { int initialized; const void *config; } mbedtls_ssl_context;
typedef struct { int config; } mbedtls_ca_snapshot;
typedef struct { void *data; } git_cert_x509;
typedef struct { int klass; const char *message; } git_error;
typedef struct git_stream git_stream;
struct git_stream {
 int version, encrypted, proxy_support;
 int (*connect)(git_stream*);
 int (*certificate)(void*, git_stream*);
 int (*set_proxy)(git_stream*, void*);
 ssize_t (*read)(git_stream*, void*, size_t);
 ssize_t (*write)(git_stream*, const char*, size_t, int);
 int (*close)(git_stream*);
 void (*free)(git_stream*);
};
#define GIT_STREAM_VERSION 1
#define GIT_ERROR_SSL 1
#define GIT_ASSERT_ARG(x) assert(x)
#define GIT_ERROR_CHECK_ALLOC(x) do { if (!(x)) return -1; } while (0)
#define git_error_set(klass, ...) do { if (emit_failure_diagnostic) set_diagnostic(klass, "TLS setup"); } while (0)
static int attempts, fail_at, outstanding, closes, frees, setup_failure, socket_failure;
static int acquire_failure, snapshot_refs = 1;
static mbedtls_ca_snapshot global_snapshot;
static int teardown_failure, close_failure, free_overwrites, emit_failure_diagnostic;
static bool diagnostic_present;
static git_error diagnostic, saved_error;
static void set_diagnostic(int klass, const char *message) {
 diagnostic.klass = klass; diagnostic.message = message; diagnostic_present = true;
}
static int git_error_save(git_error **out) {
 assert(diagnostic_present);
 saved_error = diagnostic; *out = &saved_error; return 0;
}
static int git_error_restore(git_error *error) {
 diagnostic = *error; diagnostic_present = true; return 0;
}
static bool git_error_exists(void) { return diagnostic_present; }
static void git_error_clear(void) {
 diagnostic.klass = 0; diagnostic.message = NULL; diagnostic_present = false;
}
static int ca_snapshot_acquire(mbedtls_ca_snapshot **out) {
 if (acquire_failure) {
  if (emit_failure_diagnostic) set_diagnostic(GIT_ERROR_SSL, "snapshot acquisition");
  return -1;
 }
 *out = &global_snapshot; ++snapshot_refs; return 0;
}
static void ca_snapshot_release(mbedtls_ca_snapshot *snapshot) {
 if (snapshot) { assert(snapshot == &global_snapshot && snapshot_refs > 1); --snapshot_refs; }
}
static void *allocate(size_t n) {
 if (++attempts == fail_at) {
  if (emit_failure_diagnostic) set_diagnostic(3, "allocation");
  return NULL;
 }
 void *p = calloc(1, n);
 assert(p); ++outstanding; return p;
}
static void release(void *p) { if (p) { --outstanding; free(p); } }
static void *git__calloc(size_t n, size_t s) { return allocate(n*s); }
static void *git__malloc(size_t n) { return allocate(n); }
static char *git__strdup(const char *s) {
 char *p = allocate(strlen(s)+1); if (p) strcpy(p, s); return p;
}
#define git__free release
static void mbedtls_ssl_init(mbedtls_ssl_context *s) { s->initialized = 1; }
static void mbedtls_ssl_free(mbedtls_ssl_context *s) {
 assert(s && s->initialized);
 if (s->config) assert(snapshot_refs > 1);
}
static int mbedtls_ssl_setup(mbedtls_ssl_context *s, void *config) {
 assert(s->initialized && config == &global_snapshot.config);
 s->config = config; return setup_failure ? -1 : 0;
}
static int socket_close(git_stream *s) {
 assert(s); ++closes;
 if (close_failure) set_diagnostic(2, "socket close");
 return close_failure;
}
static void socket_free(git_stream *s) {
 assert(s); ++frees;
 if (free_overwrites) set_diagnostic(4, "socket free");
 free(s);
}
static int git_stream_close(git_stream *s) { return s->close(s); }
static void git_stream_free(git_stream *s) { if (s) s->free(s); }
static git_stream *socket_create(void) {
 git_stream *s = calloc(1, sizeof(*s)); assert(s);
 s->close = socket_close; s->free = socket_free; return s;
}
static int git_socket_stream_new(git_stream **out, const char *host, const char *port) {
 (void)host; (void)port;
 if (socket_failure) return -7;
 *out = socket_create(); return 0;
}
static int git_stream_supports_proxy(git_stream *s) { (void)s; return 1; }
static int mbedtls_connect(git_stream *s) { (void)s; return 0; }
static int mbedtls_certificate(void *p, git_stream *s) { (void)p; (void)s; return 0; }
static int mbedtls_set_proxy(git_stream *s, void *p) { (void)s; (void)p; return 0; }
static ssize_t mbedtls_stream_read(git_stream *s, void *p, size_t n) {
 (void)s; (void)p; (void)n; return 0;
}
static ssize_t mbedtls_stream_write(git_stream *s, const char *p, size_t n, int f) {
 (void)s; (void)p; (void)n; (void)f; return 0;
}
static int ssl_teardown(mbedtls_ssl_context *s) {
 (void)s;
 if (teardown_failure) set_diagnostic(GIT_ERROR_SSL, "TLS teardown");
 return teardown_failure;
}
'''

CASES = r'''
static void reset(int allocation, int setup) {
 assert(outstanding == 0 && snapshot_refs == 1);
 attempts = closes = frees = socket_failure = 0;
 teardown_failure = close_failure = free_overwrites = acquire_failure = 0;
 emit_failure_diagnostic = 1; git_error_clear();
 fail_at = allocation; setup_failure = setup;
}
int main(void) {
 git_stream sentinel = {0};
 /* Each allocation, SSL setup and CA snapshot acquisition can fail. */
 for (int scenario = 1; scenario <= 5; ++scenario) {
  reset(scenario <= 3 ? scenario : 0, scenario == 4);
  acquire_failure = scenario == 5;
  git_stream *input = socket_create(), *out = &sentinel;
  assert(git_mbedtls_stream_wrap(&out, input, "example.test") == -1);
  assert(out == &sentinel && closes == 0 && frees == 0 && outstanding == 0);
  git_stream_close(input); git_stream_free(input);
  assert(closes == 1 && frees == 1);

  reset(scenario <= 3 ? scenario : 0, scenario == 4);
  acquire_failure = scenario == 5;
  out = &sentinel;
  assert(git_mbedtls_stream_new(&out, "example.test", "443") == -1);
  assert(out == &sentinel && closes == 1 && frees == 1 && outstanding == 0);
 }
 /* A successful borrowed wrapper never owns the caller's stream. */
 reset(0, 0);
 git_stream *input = socket_create(), *out = NULL;
 assert(git_mbedtls_stream_wrap(&out, input, "example.test") == 0);
 assert(out && out->encrypted && out->proxy_support);
 assert(out->close(out) == 0); out->free(out);
 assert(closes == 0 && frees == 0 && outstanding == 0);
 git_stream_close(input); git_stream_free(input);
 /* A successful newly created wrapper owns exactly one stream. */
 reset(0, 0); out = NULL;
 assert(git_mbedtls_stream_new(&out, "example.test", "443") == 0);
 assert(out->close(out) == 0); out->free(out);
 assert(closes == 1 && frees == 1 && outstanding == 0);
 reset(0, 0); socket_failure = 1; out = &sentinel;
 assert(git_mbedtls_stream_new(&out, "example.test", "443") == -7);
 assert(out == &sentinel && closes == 0 && frees == 0 && outstanding == 0);
 /* Failure cleanup may invoke a registered socket that changes the error. */
 for (int mode = 0; mode <= 2; ++mode) {
  reset(mode == 2 ? 0 : 3, mode == 2); out = &sentinel;
  emit_failure_diagnostic = mode != 1;
  close_failure = -7; free_overwrites = 1;
  assert(git_mbedtls_stream_new(&out, "example.test", "443") == -1);
  assert(out == &sentinel && closes == 1 && frees == 1 && outstanding == 0);
  if (mode == 1) {
   assert(!diagnostic_present);
  } else {
   assert(diagnostic_present && diagnostic.klass == (mode == 2 ? GIT_ERROR_SSL : 3));
   assert(strcmp(diagnostic.message, mode == 2 ? "TLS setup" : "allocation") == 0);
  }
 }
 /* TLS shutdown failure must still release an owned socket. Preserve the
    TLS error when the socket's close reports a second error. */
 for (int owned = 0; owned <= 1; ++owned) {
  for (int failure = 0; failure <= 1; ++failure) {
   reset(0, 0); input = socket_create(); out = NULL;
   assert(mbedtls_stream_wrap(&out, input, "example.test", owned) == 0);
   ((mbedtls_stream *)out)->connected = true;
   teardown_failure = -5; close_failure = failure ? -7 : 0;
   assert(out->close(out) == -1);
   assert(diagnostic.klass == GIT_ERROR_SSL);
   assert(strcmp(diagnostic.message, "TLS teardown") == 0);
   assert(!((mbedtls_stream *)out)->connected && closes == owned);
   out->free(out);
   assert(frees == owned && outstanding == 0);
   if (!owned) { git_stream_close(input); git_stream_free(input); }
  }
 }
 reset(0, 0); out = NULL;
 assert(git_mbedtls_stream_new(&out, "example.test", "443") == 0);
 ((mbedtls_stream *)out)->connected = true; close_failure = -7;
 assert(out->close(out) == -7); out->free(out);
 assert(diagnostic.klass == 2 && strcmp(diagnostic.message, "socket close") == 0);
 assert(closes == 1 && frees == 1 && outstanding == 0);
 assert(snapshot_refs == 1);
 return 0;
}
'''

HOSTNAME_PREFIX = r'''
#include <assert.h>
#include <stdbool.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

typedef struct { int marker; } git_stream;
typedef struct { int marker; } git_cert_x509;
typedef struct { int marker; } mbedtls_ssl_context;
typedef struct { int marker; } mbedtls_ca_snapshot;
#define GIT_ASSERT(x) assert(x)
#define GIT_ERROR_SSL 1
#define GIT_ECERTIFICATE -17
#define MBEDTLS_ERR_SSL_BAD_INPUT_DATA -0x7100
#define MBEDTLS_ERR_SSL_ALLOC_FAILED -0x7f00
#define MBEDTLS_ERR_SSL_WANT_READ -0x6900
#define MBEDTLS_ERR_SSL_WANT_WRITE -0x6880
#define MBEDTLS_ERR_X509_CERT_VERIFY_FAILED -0x2700

static int socket_status, hostname_status, handshake_status, verify_status;
static int socket_calls, hostname_calls, bio_calls, handshake_calls, verify_calls;
static int strerror_arg, diagnostic_class;
static char diagnostic[512];
static git_stream *expected_io;
static mbedtls_ssl_context *expected_ssl;
static const char *expected_host;

static void mbedtls_strerror(int error, char *buffer, size_t length) {
 strerror_arg = error;
 snprintf(buffer, length, "mock mbedtls reason");
}
static int mbedtls_ssl_get_verify_result(mbedtls_ssl_context *ssl) {
 assert(ssl == expected_ssl); return 0;
}
static void git_error_set(int klass, const char *format, ...) {
 va_list args;
 diagnostic_class = klass;
 va_start(args, format);
 vsnprintf(diagnostic, sizeof(diagnostic), format, args);
 va_end(args);
}
static int git_stream_connect(git_stream *io) {
 assert(io == expected_io); ++socket_calls; return socket_status;
}
static int mbedtls_ssl_set_hostname(mbedtls_ssl_context *ssl, const char *host) {
 assert(ssl == expected_ssl && host == expected_host);
 ++hostname_calls; return hostname_status;
}
static int bio_write(void *io, const unsigned char *buffer, size_t length) {
 (void)io; (void)buffer; (void)length; return 0;
}
static int bio_read(void *io, unsigned char *buffer, size_t length) {
 (void)io; (void)buffer; (void)length; return 0;
}
static void mbedtls_ssl_set_bio(mbedtls_ssl_context *ssl, void *io,
 int (*send)(void *, const unsigned char *, size_t),
 int (*receive)(void *, unsigned char *, size_t), void *timeout) {
 assert(ssl == expected_ssl && io == expected_io);
 assert(send == bio_write && receive == bio_read && timeout == NULL);
 ++bio_calls;
}
static int mbedtls_ssl_handshake(mbedtls_ssl_context *ssl) {
 assert(ssl == expected_ssl); ++handshake_calls; return handshake_status;
}
static int verify_server_cert(mbedtls_ssl_context *ssl) {
 assert(ssl == expected_ssl); ++verify_calls; return verify_status;
}
'''

HOSTNAME_CASES = r'''
static void reset(void) {
 socket_status = hostname_status = handshake_status = verify_status = 0;
 socket_calls = hostname_calls = bio_calls = handshake_calls = verify_calls = 0;
 strerror_arg = diagnostic_class = 0; diagnostic[0] = '\0';
}
int main(void) {
 git_stream io = {0}; mbedtls_ssl_context ssl = {0};
 char host[] = "example.test";
 expected_io = &io; expected_ssl = &ssl; expected_host = host;

 for (int owned = 0; owned <= 1; ++owned) {
  const int failures[] = {MBEDTLS_ERR_SSL_BAD_INPUT_DATA, MBEDTLS_ERR_SSL_ALLOC_FAILED};
  for (size_t i = 0; i < sizeof(failures) / sizeof(failures[0]); ++i) {
   reset(); mbedtls_stream st = {0}; st.io = &io; st.ssl = &ssl;
   st.host = host; st.owned = owned; hostname_status = failures[i];
   assert(mbedtls_connect(&st.parent) == -1);
   assert(socket_calls == owned && hostname_calls == 1 && st.connected);
   assert(bio_calls == 0 && handshake_calls == 0 && verify_calls == 0);
   assert(strerror_arg == failures[i] && diagnostic_class == GIT_ERROR_SSL);
   assert(strstr(diagnostic, "mock mbedtls reason") != NULL);
  }
  reset(); mbedtls_stream st = {0}; st.io = &io; st.ssl = &ssl;
  st.host = host; st.owned = owned;
  assert(mbedtls_connect(&st.parent) == 0);
  assert(socket_calls == owned && hostname_calls == 1 && bio_calls == 1);
  assert(handshake_calls == 1 && verify_calls == 1 && st.connected);

  reset(); st.connected = false; handshake_status = -0x7200;
  assert(mbedtls_connect(&st.parent) == -1);
  assert(strerror_arg == handshake_status && diagnostic_class == GIT_ERROR_SSL);
  assert(handshake_calls == 1 && verify_calls == 0);

  reset(); st.connected = false; verify_status = -33;
  assert(mbedtls_connect(&st.parent) == -33);
  assert(handshake_calls == 1 && verify_calls == 1);
 }
 reset(); mbedtls_stream st = {0}; st.io = &io; st.ssl = &ssl;
 st.host = host; st.owned = 1; socket_status = -7;
 assert(mbedtls_connect(&st.parent) == -7 && !st.connected);
 assert(socket_calls == 1 && hostname_calls == 0 && bio_calls == 0);
 assert(handshake_calls == 0 && verify_calls == 0);
 return 0;
}
'''

CIPHER_HEADER = r'''
#include <assert.h>
#include <stdbool.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
'''

CIPHER_PREFIX = r'''
typedef struct { int unused; } fake_context;
typedef struct { int unused; } mbedtls_ca_snapshot;
static fake_context mbedtls_entropy, mbedtls_rng;
static mbedtls_ca_snapshot snapshot, *current_ca;
static int ca_lock;
static bool ca_lock_initialized;
static bool initialized;
static int mode, names_seen, configured, consumed, registrations, ca_calls;
static int psa_result, psa_calls, diagnostic_class, psa_diagnostic;
typedef int psa_status_t;
#define PSA_SUCCESS 0
#define MBEDTLS_SSL_PROTO_TLS1_3 1
#define GIT_DEFAULT_CERT_LOCATION NULL
#define GIT_ERROR_SSL 1
#define GIT_ERROR_CHECK_ALLOC(x) do { if (!(x)) return -1; } while (0)
#define ARRAY_SIZE(x) (sizeof(x) / sizeof((x)[0]))
#define MBEDTLS_SSL_IS_CLIENT 0
#define MBEDTLS_SSL_TRANSPORT_STREAM 0
#define MBEDTLS_SSL_PRESET_DEFAULT 0
#define MBEDTLS_SSL_VERIFY_OPTIONAL 0

static void git_error_set(int klass, const char *format, ...) {
 diagnostic_class = klass;
 if (strcmp(format, "failed to initialize PSA crypto: %d") == 0) {
  va_list args; va_start(args, format); psa_diagnostic = va_arg(args, int); va_end(args);
 }
}
static psa_status_t psa_crypto_init(void) { ++psa_calls; return psa_result; }
static void mbedtls_entropy_init(fake_context *p) { (void)p; }
static void mbedtls_ctr_drbg_init(fake_context *p) { (void)p; }
static int git_mutex_init(int *lock) { (void)lock; return 0; }
static char *git__strdup(const char *s) {
 char *copy = malloc(strlen(s) + 1); assert(copy); strcpy(copy, s); return copy;
}
static char *git__strtok(char **cursor, const char *delimiters) {
 char *start = *cursor, *end;
 if (!start) return NULL;
 end = strpbrk(start, delimiters);
 if (end) { *end = '\0'; *cursor = end + 1; } else { *cursor = NULL; }
 return start;
}
#define git__free free
static int mbedtls_ssl_get_ciphersuite_id(const char *name) {
 assert(*name); ++names_seen;
 return mode == 2 || (mode == 1 && names_seen != 1) ? 0 : names_seen;
}
static int ca_snapshot_create(mbedtls_ca_snapshot **out, const char *file, const char *path) {
 assert(!file && !path); ++configured; consumed = 0;
 /* The real snapshot config passes this array to a zero-terminated consumer. */
 while (ciphers_list[consumed] != 0) ++consumed;
 *out = &snapshot; return 0;
}
static int mbedtls_entropy_func(void *p, unsigned char *out, size_t n) {
 (void)p; (void)out; (void)n; return 0;
}
static int mbedtls_ctr_drbg_seed(fake_context *p,
 int (*f)(void *, unsigned char *, size_t), void *entropy,
 const unsigned char *custom, size_t length) {
 (void)p; (void)f; (void)entropy; (void)custom; (void)length; return 0;
}
static int git_mbedtls__set_cert_location(const char *file, const char *path) {
 (void)file; (void)path; ++ca_calls; return 0;
}
static void shutdown_ssl(void) { initialized = false; ca_lock_initialized = false; current_ca = NULL; }
static int git_runtime_shutdown_register(void (*callback)(void)) {
 assert(callback == shutdown_ssl); ++registrations; return 0;
}
'''

CIPHER_CASES = r'''
int main(void) {
 psa_result = -148;
 assert(git_mbedtls_stream_global_init() == -1);
 assert(psa_calls == 1 && diagnostic_class == GIT_ERROR_SSL && psa_diagnostic == -148);
 assert(!initialized && !ca_lock_initialized && current_ca == NULL && configured == 0);
 psa_result = 0;
 mode = 0;
 assert(git_mbedtls_stream_global_init() == 0);
 assert(names_seen == GIT_SSL_DEFAULT_CIPHERS_COUNT);
 assert(configured == 1 && consumed == GIT_SSL_DEFAULT_CIPHERS_COUNT);
 assert(initialized && registrations == 1 && ca_calls == 0 && psa_calls == 2);

 /* A later shorter list must not retain nonzero entries from the first. */
 shutdown_ssl();
 mode = 1; names_seen = 0;
 assert(git_mbedtls_stream_global_init() == 0);
 assert(names_seen == GIT_SSL_DEFAULT_CIPHERS_COUNT);
 assert(configured == 2 && consumed == 1 && registrations == 2 && ca_calls == 0);

 shutdown_ssl(); mode = 2; names_seen = 0;
 assert(git_mbedtls_stream_global_init() == -1);
 assert(names_seen == GIT_SSL_DEFAULT_CIPHERS_COUNT);
 assert(configured == 2 && registrations == 2 && ca_calls == 0);
 return 0;
}
'''


class StreamOwnership(unittest.TestCase):
    def test_failure_and_success_ownership(self):
        text = SOURCE.read_text()
        start = text.index("typedef struct {\n\tgit_stream parent;")
        end = text.index("} mbedtls_stream;", start) + len("} mbedtls_stream;")
        bodies = [function(text, name) for name in (
            "mbedtls_stream_close", "mbedtls_stream_free", "mbedtls_stream_wrap",
            "git_mbedtls_stream_wrap", "git_mbedtls_stream_new")]
        with tempfile.TemporaryDirectory(prefix="mbedtls-ownership-") as directory:
            source, binary = Path(directory) / "ownership.c", Path(directory) / "ownership"
            source.write_text(PREFIX + text[start:end] + "\n" + "\n".join(bodies) + CASES)
            command = [os.environ.get("CC", "cc"), "-std=c99", "-Wall", "-Wextra", "-Werror"]
            if os.environ.get("MBEDTLS_OWNERSHIP_ASAN") == "1":
                command += ["-fsanitize=address", "-fno-omit-frame-pointer"]
            subprocess.run(command + [str(source), "-o", str(binary)], check=True)
            subprocess.run([str(binary)], check=True)

    def test_hostname_failure_stops_before_handshake(self):
        text = SOURCE.read_text()
        start = text.index("typedef struct {\n\tgit_stream parent;")
        end = text.index("} mbedtls_stream;", start) + len("} mbedtls_stream;")
        bodies = [function(text, name) for name in ("ssl_set_error", "mbedtls_connect")]
        with tempfile.TemporaryDirectory(prefix="mbedtls-hostname-") as directory:
            source, binary = Path(directory) / "hostname.c", Path(directory) / "hostname"
            source.write_text(HOSTNAME_PREFIX + text[start:end] + "\n" + "\n".join(bodies) + HOSTNAME_CASES)
            command = [os.environ.get("CC", "cc"), "-std=c99", "-Wall", "-Wextra", "-Werror"]
            if os.environ.get("MBEDTLS_OWNERSHIP_ASAN") == "1":
                command += ["-fsanitize=address", "-fno-omit-frame-pointer"]
            subprocess.run(command + [str(source), "-o", str(binary)], check=True)
            subprocess.run([str(binary)], check=True)

    def test_cipher_consumer_stops_at_terminator(self):
        text = SOURCE.read_text()
        declaration = re.search(
            r"^#define GIT_SSL_DEFAULT_CIPHERS .+\n"
            r"#define GIT_SSL_DEFAULT_CIPHERS_COUNT .+\n\n"
            r"static int ciphers_list\[.+\];", text, re.M)
        self.assertIsNotNone(declaration)
        body = function(text, "git_mbedtls_stream_global_init")
        with tempfile.TemporaryDirectory(prefix="mbedtls-cipher-list-") as directory:
            source, binary = Path(directory) / "cipher.c", Path(directory) / "cipher"
            source.write_text(CIPHER_HEADER + declaration.group() + "\n" + CIPHER_PREFIX + body + CIPHER_CASES)
            command = [os.environ.get("CC", "cc"), "-std=c99", "-Wall", "-Wextra", "-Werror"]
            if os.environ.get("MBEDTLS_OWNERSHIP_ASAN") == "1":
                command += ["-fsanitize=address", "-fno-omit-frame-pointer"]
            subprocess.run(command + [str(source), "-o", str(binary)], check=True)
            subprocess.run([str(binary)], check=True)


if __name__ == "__main__":
    unittest.main()
