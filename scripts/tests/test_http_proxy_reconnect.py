#!/usr/bin/env python3
"""Exercise proxy reconnect and authentication retry without a network."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from test_mbedtls_stream_ownership import function

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "third_party/git2-rs/libgit2-sys/libgit2/src/libgit2/transports/httpclient.c"

PREFIX = r'''
#include <assert.h>
#include <stdbool.h>
#include <stdio.h>
typedef struct { int unused; } git_error;
typedef struct { const char *host, *port; } url;
typedef struct { url url; void *stream; } git_http_server;
typedef struct { int status; } git_http_response;
typedef struct { int unused; } git_http_request;
typedef struct {
 git_http_server server, proxy;
 struct { void *proxy_certificate_check_cb, *proxy_certificate_check_payload; } opts;
 bool connected, proxy_connected, keepalive;
 int state, current_server, request_count;
} git_http_client;
enum { NONE, DONE, SENDING_REQUEST, SENT_REQUEST, PROXY, HAS_EARLY_RESPONSE };
#define GIT_RETRY -42
#define GIT_TRACE_DEBUG 0
#define GIT_ERROR_HTTP 1
#define GIT_HTTP_STATUS_PROXY_AUTHENTICATION_REQUIRED 407
#define GIT_HTTP_STATUS_OK 200
#define GIT_ASSERT(x) assert(x)
#define git_trace(...) ((void)0)
#define git_error_set(...) ((void)0)
#define CHECK(x) do { if (!(x)) { \
 fprintf(stderr, "check failed at line %d: %s\n", __LINE__, #x); return 1; \
} } while (0)

static int opens, connect_requests, server_closes, proxy_closes;
static int setup_error, server_error, proxy_status;
static int proxy_identities[4];
static git_http_client *active;
static int setup_hosts(git_http_client *c, git_http_request *r) {
 (void)r; active = c; return setup_error;
}
static void close_stream(git_http_server *s) {
 if (!s->stream) return;
 if (s == &active->server) {
  assert(active->proxy.stream); ++server_closes;
 } else {
  assert(s == &active->proxy && !active->server.stream); ++proxy_closes;
 }
 s->stream = NULL;
}
static void reset_auth_connection(git_http_server *s) { (void)s; }
static void reset_parser(git_http_client *c) { (void)c; }
static bool use_connect_proxy(git_http_client *c) { (void)c; return true; }
static int server_create_stream(git_http_server *s) {
 assert(opens < 4); s->stream = &proxy_identities[opens++]; return 0;
}
static int server_connect_stream(git_http_server *s, void *cb, void *payload) {
 (void)cb; (void)payload; assert(s->stream); return 0;
}
static int generate_connect_request(git_http_client *c, git_http_request *r) {
 (void)c; (void)r; return 0;
}
static int client_write_request(git_http_client *c) {
 assert(c->proxy.stream); ++connect_requests; return 0;
}
static int git_http_client_read_response(git_http_response *r, git_http_client *c) {
 r->status = proxy_status; c->state = DONE; c->keepalive = true; return 0;
}
static int git_http_client_skip_body(git_http_client *c) { (void)c; return 0; }
static void save_early_response(git_http_client *c, git_http_response *r) {
 (void)r; c->state = HAS_EARLY_RESPONSE;
}
static void git_http_response_dispose(git_http_response *r) { (void)r; }
static int server_connect(git_http_client *c) {
 c->server.stream = &c->server; return server_error;
}
static bool git_error_exists(void) { return false; }
static int git_error_save(git_error **e) { (void)e; return 0; }
static int git_error_restore(git_error *e) { (void)e; return 0; }
static void git_error_clear(void) {}
'''

CASES = r'''
static void reset(void) {
 opens = connect_requests = server_closes = proxy_closes = 0;
 setup_error = server_error = 0; proxy_status = GIT_HTTP_STATUS_OK;
}
int main(void) {
 git_http_request request = {0};
 git_http_client client = {0};

 /* The first proxy CONNECT succeeds, then the server TLS connection fails. */
 reset(); server_error = -13;
 CHECK(http_client_connect(&client, &request) == -13);
 CHECK(opens == 1 && connect_requests == 1 && client.keepalive);
 CHECK(!client.server.stream && !client.proxy.stream && !client.proxy_connected);
 CHECK(server_closes == 1 && proxy_closes == 1);
 /* The same client must open a new proxy and send another CONNECT. */
 server_error = 0;
 CHECK(http_client_connect(&client, &request) == 0);
 CHECK(opens == 2 && connect_requests == 2 && client.connected);
 CHECK(client.proxy_connected && client.proxy.stream && client.server.stream);

 /* Setup failure with an earlier borrowed connection closes both in order. */
 reset(); setup_error = -19;
 CHECK(http_client_connect(&client, &request) == -19);
 CHECK(!client.proxy_connected && !client.proxy.stream && !client.server.stream);
 CHECK(server_closes == 1 && proxy_closes == 1);

 /* Authentication retry keeps the proxy open and sends CONNECT on it again. */
 client = (git_http_client){0}; reset();
 proxy_status = GIT_HTTP_STATUS_PROXY_AUTHENTICATION_REQUIRED;
 CHECK(http_client_connect(&client, &request) == GIT_RETRY);
 CHECK(client.state == HAS_EARLY_RESPONSE && client.proxy_connected && client.proxy.stream);
 CHECK(opens == 1 && connect_requests == 1 && server_closes == 0 && proxy_closes == 0);
 void *first_proxy = client.proxy.stream;
 /* The caller consumes the 407 response before retrying with credentials. */
 client.state = DONE; proxy_status = GIT_HTTP_STATUS_OK;
 CHECK(http_client_connect(&client, &request) == 0);
 CHECK(client.proxy.stream == first_proxy && client.proxy_connected && client.connected);
 CHECK(opens == 1 && connect_requests == 2 && proxy_closes == 0);
 puts("proxy failure reconnect and 407 keepalive retry passed");
 return 0;
}
'''


class ProxyReconnect(unittest.TestCase):
    def test_failure_reopens_proxy_but_retry_reuses_it(self):
        text = SOURCE.read_text()
        bodies = [function(text, name) for name in ("proxy_connect", "http_client_connect")]
        with tempfile.TemporaryDirectory(prefix="http-proxy-reconnect-") as directory:
            source, binary = Path(directory) / "reconnect.c", Path(directory) / "reconnect"
            source.write_text(PREFIX + "\n".join(bodies) + CASES)
            command = [os.environ.get("CC", "cc"), "-std=c99", "-Wall", "-Wextra", "-Werror"]
            if os.environ.get("MBEDTLS_OWNERSHIP_ASAN") == "1":
                command += ["-fsanitize=address", "-fno-omit-frame-pointer"]
            subprocess.run(command + [str(source), "-o", str(binary)], check=True)
            subprocess.run([str(binary)], check=True)


if __name__ == "__main__":
    unittest.main()
