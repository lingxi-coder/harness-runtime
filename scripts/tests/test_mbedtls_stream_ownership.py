#!/usr/bin/env python3
"""Check real wrapper/new/free bodies with deterministic allocation failures.

The small C harness substitutes allocation and TLS setup, not the product cleanup
branches. It verifies stream ownership and local allocation balance, not TLS
protocol correctness or the independent CA configuration lifecycle.
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
typedef struct { void *data; } git_cert_x509;
typedef struct { int config; } mbedtls_ca_snapshot;
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
#define git_error_set(...) ((void)0)
static int attempts, fail_at, outstanding, closes, frees, setup_failure, socket_failure;
static int acquire_failure, snapshot_refs = 1;
static mbedtls_ca_snapshot global_snapshot;
static int teardown_failure, close_failure;
static git_error diagnostic, saved_error;
static void set_diagnostic(int klass, const char *message) {
 diagnostic.klass = klass; diagnostic.message = message;
}
static int git_error_save(git_error **out) {
 saved_error = diagnostic; *out = &saved_error; return 0;
}
static int git_error_restore(git_error *error) { diagnostic = *error; return 0; }
static int ca_snapshot_acquire(mbedtls_ca_snapshot **out) {
 if (acquire_failure) return -1;
 *out = &global_snapshot; ++snapshot_refs; return 0;
}
static void ca_snapshot_release(mbedtls_ca_snapshot *snapshot) {
 if (snapshot) { assert(snapshot == &global_snapshot && snapshot_refs > 1); --snapshot_refs; }
}
static void *allocate(size_t n) {
 if (++attempts == fail_at) return NULL;
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
static void socket_free(git_stream *s) { assert(s); ++frees; free(s); }
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
 teardown_failure = close_failure = acquire_failure = 0;
 diagnostic.klass = 0; diagnostic.message = NULL;
 fail_at = allocation; setup_failure = setup;
}
int main(void) {
 git_stream sentinel = {0};
 /* Each of the wrapper's three allocations, and SSL setup, can fail. */
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


if __name__ == "__main__":
    unittest.main()
