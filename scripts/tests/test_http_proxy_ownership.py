#!/usr/bin/env python3
"""Run the actual HTTP connect function against borrowed-stream test callbacks.

This tests callback lifetime and diagnostic preservation without network, TLS
handshake, certificates, or the independent shared CA configuration paths.
"""
from pathlib import Path
import os
import subprocess
import tempfile
import unittest

from test_mbedtls_stream_ownership import function

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "third_party/git2-rs/libgit2-sys/libgit2/src/libgit2/transports/httpclient.c"

HARNESS = r'''
#include <assert.h>
#include <stdbool.h>
#include <stddef.h>
typedef struct { int code; } git_error;
typedef struct { const char *host, *port; } url;
typedef struct { url url; bool stream; } git_http_server;
typedef struct {
 git_http_server server, proxy;
 bool connected, proxy_connected, keepalive;
 int state, request_count;
} git_http_client;
typedef struct { int unused; } git_http_request;
enum { NONE, DONE };
#define GIT_RETRY -42
#define GIT_TRACE_DEBUG 0
#define git_trace(...) ((void)0)
static int mode, error_state, server_closes, proxy_closes;
static bool diagnostic_present;
static bool proxy_alive, borrowed;
static git_error saved;
static git_http_client *active;
static int git_error_save(git_error **out) {
 saved.code = error_state; *out = &saved; return 0;
}
static int git_error_restore(git_error *error) {
 error_state = error->code; diagnostic_present = true; return 0;
}
static bool git_error_exists(void) { return diagnostic_present; }
static void git_error_clear(void) { error_state = 0; diagnostic_present = false; }
static int setup_hosts(git_http_client *client, git_http_request *request) {
 (void)request; active = client;
 if (mode == 1 || mode == 6) {
  error_state = mode == 1 ? 11 : 0; diagnostic_present = mode == 1;
  return -11;
 }
 return 0;
}
static void close_stream(git_http_server *server) {
 if (!server->stream) return;
 if (server == &active->server) {
  /* A borrowed TLS stream can write close_notify through its proxy BIO. */
  if (borrowed) assert(proxy_alive);
  ++server_closes;
 } else {
  assert(server == &active->proxy && proxy_alive);
  proxy_alive = false; ++proxy_closes;
 }
 server->stream = false;
 /* Registered transports may replace the thread-local diagnostic. */
 error_state = 99;
 diagnostic_present = true;
}
static void reset_auth_connection(git_http_server *server) { (void)server; }
static void reset_parser(git_http_client *client) { (void)client; }
static bool use_connect_proxy(git_http_client *client) { (void)client; return mode != 4; }
static int proxy_connect(git_http_client *client, git_http_request *request) {
 (void)request; client->proxy.stream = true; proxy_alive = true;
 if (mode == 2 || mode == 5) {
  error_state = mode == 5 ? 0 : 12;
  diagnostic_present = mode != 5;
  return mode == 5 ? GIT_RETRY : -12;
 }
 return 0;
}
static int server_connect(git_http_client *client) {
 client->server.stream = true; borrowed = client->proxy.stream;
 error_state = 13; diagnostic_present = true; return -13;
}
'''

CASES = r'''
int main(void) {
 for (mode = 1; mode <= 6; ++mode) {
  git_http_client client = {0}; git_http_request request = {0};
  server_closes = proxy_closes = 0; proxy_alive = borrowed = false;
  error_state = 0; diagnostic_present = false;
  if (mode == 1 || mode == 6) {
   /* Host setup can fail while an earlier borrowed connection still exists. */
   client.server.stream = client.proxy.stream = true;
   proxy_alive = borrowed = true;
  }
  int result = http_client_connect(&client, &request);
  int expected = mode == 1 || mode == 6 ? -11 : (mode == 2 ? -12 : (mode == 5 ? GIT_RETRY : -13));
  assert(result == expected);
  assert(error_state == (mode == 1 ? 11 : (mode == 2 ? 12 : (mode == 5 || mode == 6 ? 0 : 13))));
  assert(diagnostic_present == (mode != 5 && mode != 6));
  assert(!client.server.stream);
  assert(server_closes == (mode == 2 || mode == 5 ? 0 : 1));
  if (mode == 5) {
   assert(client.proxy.stream && proxy_alive && proxy_closes == 0);
   close_stream(&client.proxy);
  } else {
   assert(!client.proxy.stream && !proxy_alive);
   assert(proxy_closes == (mode == 4 ? 0 : 1));
  }
 }
 return 0;
}
'''


class ProxyOwnership(unittest.TestCase):
    def test_error_cleanup_respects_borrowed_proxy(self):
        body = function(SOURCE.read_text(), "http_client_connect")
        with tempfile.TemporaryDirectory(prefix="http-proxy-ownership-") as directory:
            source, binary = Path(directory) / "proxy.c", Path(directory) / "proxy"
            source.write_text(HARNESS + body + CASES)
            command = [os.environ.get("CC", "cc"), "-std=c99", "-Wall", "-Wextra", "-Werror"]
            if os.environ.get("MBEDTLS_OWNERSHIP_ASAN") == "1":
                command += ["-fsanitize=address", "-fno-omit-frame-pointer"]
            subprocess.run(command + [str(source), "-o", str(binary)], check=True)
            subprocess.run([str(binary)], check=True)


if __name__ == "__main__":
    unittest.main()
