#!/usr/bin/env bash
# Runtime verification of the bwrap shapes the Rust sandbox builder produces.
# Requires Docker (LinuxKit). bwrap needs --privileged (Docker's default seccomp
# blocks unprivileged userns). arm64 host → arm64v8/debian.
#
# Usage: scripts/tests/verify-bwrap.sh            (runs all checks)
#        scripts/tests/verify-bwrap.sh net|userns|deny|scrub|netbridge|socks|seccomp   (one group)
#
# The `netbridge` group proves the socat/bwrap/env PLUMBING that
# wrap_command_with_sandbox_linux emits: a host stand-in HTTP CONNECT proxy +
# host socat UNIX-LISTEN->TCP:proxy + a bwrap child (--unshare-net, sock bound,
# sandbox-side socat TCP-LISTEN:3128->UNIX-CONNECT, HTTP_PROXY=localhost:3128).
# The in-container python CONNECT proxy is a STAND-IN for the Rust hyper proxy
# (P3b, separately tested) that only allows one host — the e2e proves the
# socat/bwrap/env bridge, NOT the proxy's domain-filtering logic.
#
# The `socks` group is the SOCKS5 analogue of `netbridge` for the Rust
# serve_socks proxy (P5): a host stand-in RFC-1928 SOCKS5 proxy (no-auth,
# CONNECT, one allowed host) reached through the SAME socat/bwrap bridge on a
# :1080 listener, with `curl --socks5-hostname` so the DOMAINNAME path is
# exercised. Stand-in == not the Rust allowlist; the Rust pre-connect filter +
# wire parse are proven by the tokio integration tests in socks_proxy.rs.
#
# The `seccomp` group proves the P7 `apply-seccomp` AF_UNIX block: a bwrap child
# wrapped with the `apply-seccomp` helper runs a tiny C probe that asserts
# `socket(AF_UNIX, SOCK_STREAM)` fails (EPERM/EACCES) while
# `socket(AF_INET, SOCK_STREAM)` still succeeds -> UNIX_BLOCKED + INET_OK. This
# is the defense-in-depth seccomp filter that forces all egress through the TCP
# proxy listeners (the workload cannot create its own Unix sockets to reach the
# bridge sockets directly). The `apply-seccomp` binary is cross-compiled ON THE
# HOST as a static aarch64-musl ELF (pure-Rust deps + rust-lld, no C linker) and
# mounted into the arm64v8 container — this AVOIDS the in-container cargo build
# that OOM'd the LinuxKit VM in P4-2c/P5. If the host cross-build is unavailable
# (e.g. the aarch64-unknown-linux-musl target is not installed), the group is
# SKIPPED with a documented note + the runnable build/probe invocation; the
# seccompiler filter itself is unit-tested for compilation in the apply-seccomp
# crate (x86_64 + aarch64).
#
# NOTE: there is no `mitm` group. The P6b TLS-terminating MITM proxy
# (tls_terminate.rs) is pure in-process Rust — it adds no socat/bwrap/env
# PLUMBING for this script to exercise (it is what the bridge tunnels TO). Its
# runtime proof is the in-process, REAL-TLS integration tests in
# sandbox-runtime/src/tls_terminate.rs and src/http_proxy.rs: a CA-trusting
# tokio-rustls client drives a CONNECT + TLS + GET through the live hyper proxy
# to a stand-in HTTPS origin, asserting the decrypted request is re-issued
# upstream over real TLS (system roots + upstream_ca; verification NOT disabled)
# and round-trips, the served leaf SAN matches the host, and a filterRequest
# denial returns the byte-exact 403. Cross-compiling the Rust proxy into the
# container would be needed to add a Docker group and buys nothing over those.
set -euo pipefail
GROUP="${1:-all}"
IMG="arm64v8/debian:stable-slim"

if ! docker info >/dev/null 2>&1; then
  echo "FATAL: Docker daemon not running (open -a Docker)"; exit 2
fi

# The in-container script. Installs bubblewrap then runs the requested checks.
read -r -d '' INNER <<'EOS' || true
set -u
apt-get update -qq >/dev/null 2>&1
apt-get install -y -qq bubblewrap >/dev/null 2>&1
GROUP="$1"
fail=0
ok()   { echo "PASS: $1"; }
bad()  { echo "FAIL: $1"; fail=1; }

if [ "$GROUP" = net ] || [ "$GROUP" = all ]; then
  # LoopbackOnly/Disabled shape == --unshare-net : external blocked, loopback up.
  if bwrap --unshare-user-try --unshare-net --ro-bind / / --proc /proc --dev /dev -- \
       /bin/sh -c 'getent hosts example.com >/dev/null 2>&1' ; then
    bad "unshare-net leaked external DNS"
  else ok "unshare-net blocks external"; fi
  # Read the kernel's per-netns interface list directly (no iproute2 in the
  # slim image): a fresh netns from --unshare-net always exposes `lo` here.
  if bwrap --unshare-user-try --unshare-net --ro-bind / / --proc /proc --dev /dev -- \
       /bin/sh -c 'grep -qw "lo:" /proc/net/dev && echo lo-ok' | grep -q lo-ok ; then
    ok "unshare-net keeps loopback ns"
  else bad "unshare-net loopback missing"; fi
  # Allowed shape == --share-net : external resolvable (host has net).
  if bwrap --unshare-user-try --share-net --ro-bind / / --proc /proc --dev /dev -- \
       /bin/sh -c 'getent hosts example.com >/dev/null 2>&1' ; then
    ok "share-net allows external"
  else echo "WARN: share-net external unresolved (host offline?) — not a sandbox failure"; fi
fi

if [ "$GROUP" = userns ] || [ "$GROUP" = all ]; then
  if bwrap --unshare-user-try --unshare-pid --ro-bind / / --proc /proc --dev /dev -- \
       /bin/sh -c 'echo up' | grep -q up ; then ok "unshare-user-try starts"; else bad "unshare-user-try start"; fi
  sysctl -w user.max_user_namespaces=0 >/dev/null 2>&1 || true
  if bwrap --unshare-user-try --unshare-pid --unshare-net --ro-bind / / --proc /proc --dev /dev -- \
       /bin/sh -c 'echo deg' | grep -q deg ; then ok "unshare-user-try degrades (userns=0)"; else bad "degrade"; fi
  sysctl -w user.max_user_namespaces=15000 >/dev/null 2>&1 || true
fi

if [ "$GROUP" = deny ] || [ "$GROUP" = all ]; then
  mkdir -p /w && echo orig > /w/HEAD
  out=$(bwrap --unshare-user-try --ro-bind / / --bind /w /w --ro-bind /w/HEAD /w/HEAD --proc /proc --dev /dev -- \
       /bin/sh -c 'echo x > /w/HEAD 2>/dev/null && echo WROTE || echo DENIED')
  [ "$out" = DENIED ] && ok "ro-bind-in-place denies write (overrides --bind parent)" || bad "ro-bind-in-place write $out"
  [ "$(cat /w/HEAD)" = orig ] && ok "host file unchanged" || bad "host file mutated"
fi

if [ "$GROUP" = scrub ] || [ "$GROUP" = all ]; then
  mkdir -p /s && cd /s
  # The wrapped-string shape: inner plants HEAD in host cwd; suffix scrubs it, preserves rc.
  bash -c 'bwrap --unshare-user-try --ro-bind / / --bind /s /s --proc /proc --dev /dev -- \
            /bin/sh -c "echo planted > /s/HEAD; exit 7"
           rc=$?; rm -rf -- /s/HEAD 2>/dev/null; exit $rc' ; rc=$?
  [ "$rc" = 7 ] && ok "scrub suffix preserves inner exit code" || bad "exit code $rc != 7"
  [ ! -e /s/HEAD ] && ok "planted bare-repo file scrubbed" || bad "HEAD not scrubbed"
  # Pre-existing file must NOT be scrubbed (it would be in ro_bind_in_place, never the scrub list).
  echo keep > /s/config
  bash -c 'true; rc=$?; rm -rf -- /s/HEAD 2>/dev/null; exit $rc' >/dev/null 2>&1 || true
  [ -e /s/config ] && ok "pre-existing file not in scrub list (untouched)" || bad "config wrongly scrubbed"
fi

if [ "$GROUP" = netbridge ] || [ "$GROUP" = all ]; then
  # Extra deps for the bridge e2e (curl + socat + a python CONNECT proxy + CA certs).
  apt-get install -y -qq socat curl python3 ca-certificates >/dev/null 2>&1
  ALLOWED=example.com
  SOCK=/tmp/claude-http-e2e.sock
  rm -f "$SOCK"

  # --- Stand-in HTTP CONNECT proxy (allows exactly one host). ---
  # This is a STAND-IN for the Rust hyper proxy (P3b). It proves the socat/bwrap
  # bridge plumbing, not the proxy's filtering logic.
  cat > /tmp/connproxy.py <<'PY'
import socket, threading, sys, select
ALLOWED = sys.argv[1] if len(sys.argv) > 1 else "example.com"
def pipe(a, b):
    try:
        while True:
            r,_,_ = select.select([a,b],[],[])
            for s in r:
                d = s.recv(65536)
                if not d: return
                (b if s is a else a).sendall(d)
    except OSError:
        pass
def handle(c):
    try:
        req = b""
        while b"\r\n\r\n" not in req:
            d = c.recv(4096)
            if not d: return
            req += d
        line = req.split(b"\r\n",1)[0].decode("latin1")
        parts = line.split()
        if len(parts) < 2 or parts[0] != "CONNECT":
            c.sendall(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n"); return
        hostport = parts[1]
        host = hostport.rsplit(":",1)[0]
        port = int(hostport.rsplit(":",1)[1]) if ":" in hostport else 443
        if host != ALLOWED:
            c.sendall(b"HTTP/1.1 403 Forbidden\r\n\r\n"); return
        try:
            u = socket.create_connection((host, port), timeout=10)
        except OSError:
            c.sendall(b"HTTP/1.1 502 Bad Gateway\r\n\r\n"); return
        c.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        pipe(c, u); u.close()
    finally:
        c.close()
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 8888)); srv.listen(64)
print("PROXY_UP", flush=True)
while True:
    c,_ = srv.accept()
    threading.Thread(target=handle, args=(c,), daemon=True).start()
PY
  python3 /tmp/connproxy.py "$ALLOWED" >/tmp/proxy.log 2>&1 &
  PROXY_PID=$!
  # Wait for PROXY_UP.
  for _ in $(seq 1 50); do grep -q PROXY_UP /tmp/proxy.log 2>/dev/null && break; sleep 0.1; done

  # --- Host socat: UNIX-LISTEN <sock> -> TCP:localhost:8888 (the proxy). ---
  # EXACT shape of initialize_linux_network_bridge's host-side socat.
  socat "UNIX-LISTEN:$SOCK,fork,reuseaddr" \
        "TCP:localhost:8888,keepalive,keepidle=10,keepintvl=5,keepcnt=3" \
        >/dev/null 2>&1 &
  SOCAT_PID=$!
  for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
  if [ -S "$SOCK" ]; then ok "host socat UNIX-LISTEN socket created"; else bad "host socat socket missing"; fi

  # --- bwrap child: EXACT shape wrap_command_with_sandbox_linux emits. ---
  # --unshare-net + --bind <sock> <sock> + sandbox-side socat TCP-LISTEN:3128
  # -> UNIX-CONNECT + HTTP_PROXY=http://localhost:3128. curl the ALLOWED host
  # THROUGH the bridge (-> NET_OK).
  SANDBOX_INNER='socat TCP-LISTEN:3128,fork,reuseaddr UNIX-CONNECT:'"$SOCK"' >/dev/null 2>&1 &
trap "kill %1 2>/dev/null; exit" EXIT
sleep 0.5
curl -sS --max-time 20 -o /dev/null https://'"$ALLOWED"'/ && echo NET_OK || echo NET_FAIL'
  OUT=$(bwrap --unshare-user-try --unshare-net \
          --bind "$SOCK" "$SOCK" \
          --ro-bind / / --proc /proc --dev /dev --unshare-pid \
          --setenv HTTP_PROXY http://localhost:3128 \
          --setenv HTTPS_PROXY http://localhost:3128 \
          -- /bin/bash -c "$SANDBOX_INNER" 2>/dev/null || true)
  echo "[netbridge] bridged curl -> $OUT"
  case "$OUT" in
    *NET_OK*) ok "allowed host reachable THROUGH the socat bridge (NET_OK)";;
    *)        bad "allowed host NOT reachable through bridge: $OUT";;
  esac

  # --- DIRECT curl in a fresh netns (no proxy) must be blocked. ---
  DOUT=$(bwrap --unshare-user-try --unshare-net \
           --ro-bind / / --proc /proc --dev /dev --unshare-pid \
           -- /bin/bash -c \
           'curl -sS --noproxy "*" --max-time 8 -o /dev/null https://'"$ALLOWED"'/ && echo NET_LEAK || echo NET_BLOCKED' \
           2>/dev/null || true)
  echo "[netbridge] direct curl -> $DOUT"
  case "$DOUT" in
    *NET_BLOCKED*) ok "direct egress blocked in fresh netns (NET_BLOCKED)";;
    *)             bad "direct egress NOT blocked: $DOUT";;
  esac

  kill "$SOCAT_PID" "$PROXY_PID" 2>/dev/null || true
fi

if [ "$GROUP" = socks ] || [ "$GROUP" = all ]; then
  # SOCKS5 analogue of netbridge for the Rust serve_socks proxy (P5).
  apt-get install -y -qq socat curl python3 ca-certificates >/dev/null 2>&1
  ALLOWED=example.com
  SSOCK=/tmp/claude-socks-e2e.sock
  rm -f "$SSOCK"

  # --- Stand-in RFC 1928 SOCKS5 proxy (no-auth, CONNECT, one allowed host). ---
  # STAND-IN for the Rust serve_socks proxy; proves the socat/bwrap bridge for a
  # :1080 SOCKS listener, NOT the Rust allowlist (that is the tokio tests' job).
  cat > /tmp/socksproxy.py <<'PY'
import socket, threading, sys, select, struct
ALLOWED = sys.argv[1] if len(sys.argv) > 1 else "example.com"
def pipe(a, b):
    try:
        while True:
            r,_,_ = select.select([a,b],[],[])
            for s in r:
                d = s.recv(65536)
                if not d: return
                (b if s is a else a).sendall(d)
    except OSError:
        pass
def recvn(c, n):
    buf = b""
    while len(buf) < n:
        d = c.recv(n - len(buf))
        if not d: return None
        buf += d
    return buf
def handle(c):
    try:
        # Greeting: VER NMETHODS METHODS[]
        head = recvn(c, 2)
        if not head or head[0] != 5: return
        nm = head[1]
        if recvn(c, nm) is None: return
        c.sendall(b"\x05\x00")  # no-auth
        # Request: VER CMD RSV ATYP ...
        req = recvn(c, 4)
        if not req or req[0] != 5 or req[1] != 1:
            c.sendall(b"\x05\x01\x00\x01\x00\x00\x00\x00\x00\x00"); return
        atyp = req[3]
        if atyp == 1:
            raw = recvn(c, 4); host = socket.inet_ntoa(raw)
        elif atyp == 3:
            ln = recvn(c, 1)[0]; host = recvn(c, ln).decode("idna")
        elif atyp == 4:
            raw = recvn(c, 16); host = socket.inet_ntop(socket.AF_INET6, raw)
        else:
            c.sendall(b"\x05\x08\x00\x01\x00\x00\x00\x00\x00\x00"); return
        port = struct.unpack("!H", recvn(c, 2))[0]
        if host != ALLOWED:
            c.sendall(b"\x05\x02\x00\x01\x00\x00\x00\x00\x00\x00"); return  # NOT_ALLOWED
        try:
            u = socket.create_connection((host, port), timeout=10)
        except OSError:
            c.sendall(b"\x05\x04\x00\x01\x00\x00\x00\x00\x00\x00"); return  # HOST_UNREACHABLE
        c.sendall(b"\x05\x00\x00\x01\x00\x00\x00\x00\x00\x00")  # GRANTED
        pipe(c, u); u.close()
    finally:
        c.close()
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 9999)); srv.listen(64)
print("SOCKS_UP", flush=True)
while True:
    c,_ = srv.accept()
    threading.Thread(target=handle, args=(c,), daemon=True).start()
PY
  python3 /tmp/socksproxy.py "$ALLOWED" >/tmp/socks.log 2>&1 &
  SPROXY_PID=$!
  for _ in $(seq 1 50); do grep -q SOCKS_UP /tmp/socks.log 2>/dev/null && break; sleep 0.1; done

  # --- Host socat: UNIX-LISTEN <sock> -> TCP:localhost:9999 (the SOCKS proxy). ---
  socat "UNIX-LISTEN:$SSOCK,fork,reuseaddr" \
        "TCP:localhost:9999,keepalive,keepidle=10,keepintvl=5,keepcnt=3" \
        >/dev/null 2>&1 &
  SSOCAT_PID=$!
  for _ in $(seq 1 50); do [ -S "$SSOCK" ] && break; sleep 0.1; done
  if [ -S "$SSOCK" ]; then ok "host socat UNIX-LISTEN socket created (socks)"; else bad "host socat socks socket missing"; fi

  # --- bwrap child: sandbox-side socat TCP-LISTEN:1080 -> UNIX-CONNECT, then
  # curl --socks5-hostname localhost:1080 (DOMAINNAME path) THROUGH the bridge. ---
  SOCKS_INNER='socat TCP-LISTEN:1080,fork,reuseaddr UNIX-CONNECT:'"$SSOCK"' >/dev/null 2>&1 &
trap "kill %1 2>/dev/null; exit" EXIT
sleep 0.5
curl -sS --max-time 20 --socks5-hostname localhost:1080 -o /dev/null https://'"$ALLOWED"'/ && echo SOCKS_OK || echo SOCKS_FAIL'
  SOUT=$(bwrap --unshare-user-try --unshare-net \
          --bind "$SSOCK" "$SSOCK" \
          --ro-bind / / --proc /proc --dev /dev --unshare-pid \
          -- /bin/bash -c "$SOCKS_INNER" 2>/dev/null || true)
  echo "[socks] bridged curl --socks5-hostname -> $SOUT"
  case "$SOUT" in
    *SOCKS_OK*) ok "allowed host reachable THROUGH the SOCKS5 bridge (SOCKS_OK)";;
    *)          bad "allowed host NOT reachable through SOCKS bridge: $SOUT";;
  esac

  # --- DIRECT curl in a fresh netns (no proxy) must be blocked. ---
  SDOUT=$(bwrap --unshare-user-try --unshare-net \
           --ro-bind / / --proc /proc --dev /dev --unshare-pid \
           -- /bin/bash -c \
           'curl -sS --noproxy "*" --max-time 8 -o /dev/null https://'"$ALLOWED"'/ && echo NET_LEAK || echo NET_BLOCKED' \
           2>/dev/null || true)
  echo "[socks] direct curl -> $SDOUT"
  case "$SDOUT" in
    *NET_BLOCKED*) ok "direct egress blocked in fresh netns (socks group)";;
    *)             bad "direct egress NOT blocked (socks group): $SDOUT";;
  esac

  kill "$SSOCAT_PID" "$SPROXY_PID" 2>/dev/null || true
fi

if [ "$GROUP" = seccomp ] || [ "$GROUP" = all ]; then
  # The host-cross-built apply-seccomp ELF is bind-mounted at /opt/apply-seccomp.
  APPLY=/opt/apply-seccomp
  if [ ! -x "$APPLY" ]; then
    echo "WARN: $APPLY not present (host cross-build skipped) — seccomp group SKIPPED."
    echo "       To run: rustup target add aarch64-unknown-linux-musl &&"
    echo "       CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \\"
    echo "         cargo build -p apply-seccomp --target aarch64-unknown-linux-musl"
  else
    apt-get install -y -qq gcc libc6-dev >/dev/null 2>&1
    # Probe: AF_UNIX socket MUST fail (the seccomp EPERM), AF_INET MUST succeed.
    cat > /tmp/probe.c <<'PROBE'
#include <sys/socket.h>
#include <stdio.h>
#include <errno.h>
#include <string.h>
int main(void) {
    int u = socket(AF_UNIX, SOCK_STREAM, 0);
    if (u >= 0) { printf("UNIX_OPEN\n"); }   /* leak: filter failed */
    else        { printf("UNIX_BLOCKED errno=%d(%s)\n", errno, strerror(errno)); }
    int i = socket(AF_INET, SOCK_STREAM, 0);
    if (i >= 0) { printf("INET_OK\n"); }
    else        { printf("INET_FAIL errno=%d(%s)\n", errno, strerror(errno)); }
    return 0;
}
PROBE
    if gcc -static -O0 -o /tmp/probe /tmp/probe.c 2>/tmp/gcc.log; then :; else
      # Fall back to dynamic if -static libc is unavailable.
      gcc -O0 -o /tmp/probe /tmp/probe.c 2>>/tmp/gcc.log || true
    fi
    if [ ! -x /tmp/probe ]; then bad "could not compile seccomp probe"; cat /tmp/gcc.log; fi

    # --- SANITY: WITHOUT apply-seccomp, AF_UNIX must OPEN (proves the probe works). ---
    SANE=$(bwrap --unshare-user-try --unshare-pid --ro-bind / / --proc /proc --dev /dev \
             -- /tmp/probe 2>/dev/null || true)
    echo "[seccomp] sanity (no filter) -> $(echo "$SANE" | tr '\n' ' ')"
    case "$SANE" in
      *UNIX_OPEN*INET_OK*) ok "probe: AF_UNIX opens & AF_INET ok without the filter (sanity)";;
      *) echo "WARN: sanity probe unexpected (env may lack AF_UNIX/AF_INET): $SANE";;
    esac

    # --- WITH apply-seccomp: EXACT shape wrap_command_with_sandbox_linux emits in
    # the non-network branch: `<apply-seccomp> <shell> -c '<cmd>'`. Here the cmd is
    # the probe directly (apply-seccomp execs argv[1..]). ---
    OUT=$(bwrap --unshare-user-try --unshare-pid --ro-bind / / --proc /proc --dev /dev \
            -- "$APPLY" /tmp/probe 2>/dev/null || true)
    echo "[seccomp] filtered -> $(echo "$OUT" | tr '\n' ' ')"
    UB=0; IO=0
    case "$OUT" in *UNIX_BLOCKED*) UB=1;; esac
    case "$OUT" in *INET_OK*) IO=1;; esac
    [ "$UB" = 1 ] && ok "socket(AF_UNIX) BLOCKED by apply-seccomp (UNIX_BLOCKED)" \
                  || bad "socket(AF_UNIX) NOT blocked: $OUT"
    [ "$IO" = 1 ] && ok "socket(AF_INET) still allowed (INET_OK — proxy egress intact)" \
                  || bad "socket(AF_INET) wrongly blocked: $OUT"
  fi
fi

echo "=== $([ $fail = 0 ] && echo ALL-PASS || echo SOME-FAIL) ==="
exit $fail
EOS

# For the seccomp group, cross-build the apply-seccomp helper on the HOST as a
# static aarch64-musl ELF (pure-Rust deps + rust-lld → no C linker needed) and
# bind-mount it into the container. This sidesteps the in-container cargo build
# that OOM'd the LinuxKit VM in earlier phases.
MOUNT_ARGS=()
if [ "$GROUP" = seccomp ] || [ "$GROUP" = all ]; then
  HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/harness-bwrap-target}"
  TARGET=aarch64-unknown-linux-musl
  BIN="$CARGO_TARGET_DIR/$TARGET/debug/apply-seccomp"
  if rustup target list --installed 2>/dev/null | grep -q "$TARGET"; then
    echo "[seccomp] cross-building apply-seccomp ($TARGET) on the host…"
    ( cd "$HERE" && CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
        cargo build -p apply-seccomp --target "$TARGET" ) || \
        echo "WARN: host cross-build failed — seccomp group will SKIP in-container."
  else
    echo "WARN: rustup target $TARGET not installed — seccomp group will SKIP."
    echo "      Install with: rustup target add $TARGET"
  fi
  if [ -x "$BIN" ]; then
    MOUNT_ARGS=(-v "$BIN:/opt/apply-seccomp:ro")
  fi
fi

docker run --rm --privileged "${MOUNT_ARGS[@]}" "$IMG" /bin/bash -c "$INNER" _ "$GROUP"
