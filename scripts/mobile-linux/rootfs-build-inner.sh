#!/bin/sh
#
# Runs INSIDE a digest-pinned Alpine container. Builds the local-app Alpine
# rootfs for one architecture and emits it as a tarball plus a closure manifest.
#
# The vendored OpenMinis `prepare_alpine_rootfs.sh` only ever extracts a bare
# minirootfs and runs fakefsify -- it never installs a single APK, which is why
# `--local-app-runtime` could never satisfy `rootfs_tool.py verify-tree`. This
# script is the missing step: it downloads the committed hashed closure,
# installs it OFFLINE, and hands back a tree with node/npm/git/python.
#
# Inputs (environment):
#   LINGXI_ARCH             Alpine arch (aarch64 | x86_64)
#   LINGXI_ALPINE_VERSION   e.g. 3.24.1
#   LINGXI_ALPINE_BRANCH    e.g. v3.24
#   LINGXI_PACKAGES         space-separated `name=version` list
#   LINGXI_ROOTFS_SHA256    expected minirootfs digest (empty to trust the CDN's)
#   LINGXI_PNPM_VERSION/URL/SHA512  pinned pnpm CLI package metadata
#   LINGXI_TYPESCRIPT_*     pinned native TypeScript package metadata
#
# Outputs (under /out/<arch>/):
#   rootfs.tar.gz       installed rootfs, ready for fakefsify
#   closure.json        every APK in the closure with url + sha256 + repo
#   installed.txt       `apk info -v` of the finished tree
#
set -eu

ARCH="${LINGXI_ARCH:?LINGXI_ARCH required}"
ALPINE_VERSION="${LINGXI_ALPINE_VERSION:?LINGXI_ALPINE_VERSION required}"
ALPINE_BRANCH="${LINGXI_ALPINE_BRANCH:?LINGXI_ALPINE_BRANCH required}"
PKGS="${LINGXI_PACKAGES:?LINGXI_PACKAGES required}"
EXPECTED_ROOTFS_SHA="${LINGXI_ROOTFS_SHA256:-}"
PNPM_VERSION="${LINGXI_PNPM_VERSION:?LINGXI_PNPM_VERSION required}"
PNPM_URL="${LINGXI_PNPM_URL:?LINGXI_PNPM_URL required}"
PNPM_SHA512="${LINGXI_PNPM_SHA512:?LINGXI_PNPM_SHA512 required}"
TYPESCRIPT_VERSION="${LINGXI_TYPESCRIPT_VERSION:?LINGXI_TYPESCRIPT_VERSION required}"
TYPESCRIPT_INSTALL_ROOT="${LINGXI_TYPESCRIPT_INSTALL_ROOT:?LINGXI_TYPESCRIPT_INSTALL_ROOT required}"
TYPESCRIPT_LICENSE="${LINGXI_TYPESCRIPT_LICENSE:?LINGXI_TYPESCRIPT_LICENSE required}"
TYPESCRIPT_PACKAGE_NAME="${LINGXI_TYPESCRIPT_PACKAGE_NAME:?LINGXI_TYPESCRIPT_PACKAGE_NAME required}"
TYPESCRIPT_URL="${LINGXI_TYPESCRIPT_URL:?LINGXI_TYPESCRIPT_URL required}"
TYPESCRIPT_SHA512="${LINGXI_TYPESCRIPT_SHA512:?LINGXI_TYPESCRIPT_SHA512 required}"
TYPESCRIPT_TSC_SHA256="${LINGXI_TYPESCRIPT_TSC_SHA256:?LINGXI_TYPESCRIPT_TSC_SHA256 required}"
PINS_JSON=/pins.json
CDN=https://dl-cdn.alpinelinux.org/alpine

OUT="/out/${ARCH}"
TARGET="/target-${ARCH}"
REPO="${OUT}/repo"

# Builder-side tools only; none of this reaches the rootfs being assembled.
# python3 is needed for the closure manifest and is not in the base image.
#
# Cached under the mounted output directory rather than fetched fresh each run.
# Keyed by arch: an .apk filename carries no architecture, so `gcc-15.2.0-r5.apk`
# is the same name for aarch64 and x86_64. One shared cache directory would let
# an x86_64 build pick up the aarch64 build's package.
BUILDER_CACHE="/out/.builder-cache/${ARCH}"
mkdir -p "${BUILDER_CACHE}"
apk add --cache-dir "${BUILDER_CACHE}" curl python3 >/dev/null 2>&1

rm -rf "${OUT}" "${TARGET}"
mkdir -p "${OUT}" "${TARGET}" "${REPO}/${ARCH}"

echo "[rootfs:${ARCH}] fetching minirootfs ${ALPINE_VERSION}"
curl -sSfL -o "${OUT}/minirootfs.tar.gz" \
  "${CDN}/${ALPINE_BRANCH}/releases/${ARCH}/alpine-minirootfs-${ALPINE_VERSION}-${ARCH}.tar.gz"
curl -sSfL -o "${OUT}/minirootfs.sha256" \
  "${CDN}/${ALPINE_BRANCH}/releases/${ARCH}/alpine-minirootfs-${ALPINE_VERSION}-${ARCH}.tar.gz.sha256"
ROOTFS_SHA="$(sha256sum "${OUT}/minirootfs.tar.gz" | awk '{print $1}')"
CDN_SHA="$(awk '{print $1}' "${OUT}/minirootfs.sha256")"
if [ "${ROOTFS_SHA}" != "${CDN_SHA}" ]; then
  echo "[rootfs:${ARCH}] minirootfs digest does not match the CDN checksum" >&2
  exit 1
fi
if [ -n "${EXPECTED_ROOTFS_SHA}" ] && [ "${ROOTFS_SHA}" != "${EXPECTED_ROOTFS_SHA}" ]; then
  echo "[rootfs:${ARCH}] minirootfs digest ${ROOTFS_SHA} diverged from the pin ${EXPECTED_ROOTFS_SHA}" >&2
  exit 1
fi

tar -xzf "${OUT}/minirootfs.tar.gz" -C "${TARGET}"
printf '%s/%s/main\n%s/%s/community\n' "${CDN}" "${ALPINE_BRANCH}" "${CDN}" "${ALPINE_BRANCH}" \
  > "${TARGET}/etc/apk/repositories"
cp /etc/resolv.conf "${TARGET}/etc/resolv.conf" 2>/dev/null || true
echo "[rootfs:${ARCH}] fetching the pinned APK closure"
# Never ask the moving Alpine index to resolve dependencies here. The committed
# pins already contain the complete, hashed closure; resolving again can mix a
# newer transitive package (for example python3) with an older exact primary
# pin and make the supposedly reproducible offline install impossible.
python3 - "${PINS_JSON}" "${ARCH}" > "${OUT}/pinned-artifacts.tsv" <<'PY'
import json, pathlib, sys

pins = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
arch = sys.argv[2]
abi = {"aarch64": "arm64-v8a", "x86_64": "x86_64"}[arch]
record = pins.get("apk_artifacts", {}).get(abi)
if not isinstance(record, dict) or record.get("closure_status") != "complete":
    raise SystemExit(f"pinned APK closure is not complete for {abi}")
artifacts = record.get("artifacts")
if not isinstance(artifacts, list) or not artifacts:
    raise SystemExit(f"pinned APK closure is empty for {abi}")
for artifact in artifacts:
    if artifact.get("availability") != "available" or artifact.get("arch") != arch:
        raise SystemExit(f"invalid pinned artifact for {abi}: {artifact!r}")
    url, digest = artifact.get("url"), artifact.get("sha256")
    if not isinstance(url, str) or not isinstance(digest, str):
        raise SystemExit(f"incomplete pinned artifact for {abi}: {artifact!r}")
    print(url, digest, pathlib.PurePosixPath(url).name, sep="\t")
PY

: > "${OUT}/fetch.log"
while IFS="$(printf '\t')" read -r url expected_sha filename; do
  destination="${REPO}/${ARCH}/${filename}"
  fetched=0
  for attempt in 1 2 3; do
    if curl -sSfL -o "${destination}.tmp" "${url}" >> "${OUT}/fetch.log" 2>&1; then
      actual_sha="$(sha256sum "${destination}.tmp" | awk '{print $1}')"
      if [ "${actual_sha}" != "${expected_sha}" ]; then
        echo "[rootfs:${ARCH}] APK digest mismatch: ${filename}" >&2
        exit 1
      fi
      mv "${destination}.tmp" "${destination}"
      fetched=1
      break
    fi
    echo "[rootfs:${ARCH}] fetch attempt ${attempt} failed: ${filename}" >&2
    sleep 2
  done
  if [ "${fetched}" != "1" ]; then
    echo "[rootfs:${ARCH}] failed to fetch pinned APK: ${filename}" >&2
    exit 1
  fi
done < "${OUT}/pinned-artifacts.tsv"

# Alpine's published per-arch index rewrites noarch packages onto the concrete
# arch and serves them from <repo>/<arch>/; there is no <repo>/noarch/ on the
# CDN, it 404s. Plain `apk index` preserves each .apk's own `A:noarch`, which
# sends the installer looking in a directory that does not exist and fails every
# pure-Python package with "package mentioned in index not found".
echo "[rootfs:${ARCH}] indexing offline repo"
( cd "${REPO}/${ARCH}" && apk index --rewrite-arch "${ARCH}" -o APKINDEX.tar.gz ./*.apk >/dev/null 2>&1 )

echo "[rootfs:${ARCH}] installing closure offline (--no-network)"
# shellcheck disable=SC2086
apk --root "${TARGET}" --arch "${ARCH}" add \
    --no-network --allow-untrusted --repository "${REPO}" $PKGS \
    > "${OUT}/install.log" 2>&1 || {
  echo "[rootfs:${ARCH}] offline install failed" >&2; grep -i error "${OUT}/install.log" | head -20 >&2; exit 1; }
if grep -qi '^ERROR' "${OUT}/install.log"; then
  echo "[rootfs:${ARCH}] offline install reported errors" >&2
  grep -i '^ERROR' "${OUT}/install.log" | head -20 >&2
  exit 1
fi

echo "[rootfs:${ARCH}] configuring guest shell"
# The vendored OpenMinis rootfs script appends an unconditional `cd ~` to
# /etc/profile, which silently discards the cwd every `sh -lc` invocation asks
# for -- including the local-app build directory. We build our own rootfs, so we
# author the profile instead of inheriting that. Nothing here changes directory.
cat > "${TARGET}/etc/profile" <<'PROFILE'
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export HOME=${HOME:-/root}
export TMPDIR=${TMPDIR:-/tmp}
export SSL_CERT_FILE=${SSL_CERT_FILE:-/etc/ssl/cert.pem}
export SSL_CERT_DIR=${SSL_CERT_DIR:-/etc/ssl/certs}
export GIT_SSL_CAINFO=${GIT_SSL_CAINFO:-/etc/ssl/cert.pem}
export NPM_CONFIG_CACHE=${NPM_CONFIG_CACHE:-$HOME/.npm}
export PIP_CACHE_DIR=${PIP_CACHE_DIR:-$HOME/.cache/pip}
export XDG_CACHE_HOME=${XDG_CACHE_HOME:-$HOME/.cache}
export PS1='\w \$ '
PROFILE

# Root's shell, and the directories the runtime expects to exist.
sed -i 's|^root:.*|root:x:0:0:root:/root:/bin/sh|' "${TARGET}/etc/passwd"
mkdir -p "${TARGET}/dev" "${TARGET}/proc" "${TARGET}/sys" "${TARGET}/run" \
         "${TARGET}/tmp" "${TARGET}/var/tmp" "${TARGET}/root" "${TARGET}/home" \
         "${TARGET}/workspace" "${TARGET}/var/lingxi" "${TARGET}/opt/lingxi"
chmod 1777 "${TARGET}/tmp" "${TARGET}/var/tmp"

# resolv.conf is replaced at boot by the native bridge's refreshDns(); ship a
# resolvable default so a first command before the first path update still works.
printf 'nameserver 1.1.1.1\nnameserver 8.8.8.8\n' > "${TARGET}/etc/resolv.conf"

echo "[rootfs:${ARCH}] rewriting absolute symlinks as relative"
# ca-certificates (and a few others) install links like
#   /etc/ssl/certs/ca-cert-X.pem -> /usr/share/ca-certificates/mozilla/X.crt
# An absolute target is interpreted against the HOST root by anything reading
# the tree outside the guest, so the rootfs policy forbids them outright and
# rootfs_tool.py rejects the archive. Rewrite each one relative to its own
# directory, which resolves identically inside the guest and survives packaging.
python3 - "${TARGET}" <<'PY'
import os
import pathlib
import sys

root = pathlib.Path(sys.argv[1]).resolve()
rewritten = 0
for dirpath, dirnames, filenames in os.walk(root):
    for name in list(dirnames) + list(filenames):
        path = pathlib.Path(dirpath) / name
        if not path.is_symlink():
            continue
        target = os.readlink(path)
        if not target.startswith("/"):
            continue
        destination = root / target.lstrip("/")
        # A link escaping the rootfs cannot be made relative safely; fail rather
        # than silently emit a `../..` chain that climbs out of the guest.
        try:
            destination.resolve().relative_to(root)
        except ValueError:
            raise SystemExit(f"absolute symlink escapes the rootfs: {path} -> {target}")
        relative = os.path.relpath(destination, path.parent)
        os.remove(path)
        os.symlink(relative, path)
        rewritten += 1
print(f"   rewrote {rewritten} absolute symlinks")
PY

python3 /install-toolchains.py "${TARGET}" "${ARCH}" /node-source-cache

echo "[rootfs:${ARCH}] verifying required binaries"
MISSING=""
for f in usr/bin/node usr/bin/npm usr/bin/npx usr/bin/git usr/bin/python3 usr/bin/pip3 usr/bin/virtualenv usr/bin/ssh; do
  [ -e "${TARGET}/$f" ] || [ -L "${TARGET}/$f" ] || MISSING="${MISSING} $f"
done
if [ -n "${MISSING}" ]; then
  echo "[rootfs:${ARCH}] required binaries missing:${MISSING}" >&2
  exit 1
fi

apk --root "${TARGET}" info -v 2>/dev/null | sort > "${OUT}/installed.txt"

case "${TYPESCRIPT_INSTALL_ROOT}" in
  /opt/lingxi/toolchains/typescript/${TYPESCRIPT_VERSION}) ;;
  *)
    echo "[rootfs:${ARCH}] unsafe TypeScript install root: ${TYPESCRIPT_INSTALL_ROOT}" >&2
    exit 1
    ;;
esac

echo "[rootfs:${ARCH}] installing pinned native TypeScript ${TYPESCRIPT_VERSION}"
TYPESCRIPT_TARBALL="${OUT}/typescript-native-${TYPESCRIPT_VERSION}.tgz"
TYPESCRIPT_STAGE="${OUT}/typescript-native-stage"
curl -sSfL -o "${TYPESCRIPT_TARBALL}" "${TYPESCRIPT_URL}"
actual_typescript_sha512="$(sha512sum "${TYPESCRIPT_TARBALL}" | awk '{print $1}')"
expected_typescript_sha512="$(printf '%s' "${TYPESCRIPT_SHA512}" | base64 -d | od -An -tx1 | tr -d ' \n')"
if [ "$(printf '%s' "${actual_typescript_sha512}" | tr '[:lower:]' '[:upper:]')" != \
     "$(printf '%s' "${expected_typescript_sha512}" | tr '[:lower:]' '[:upper:]')" ]; then
  echo "[rootfs:${ARCH}] native TypeScript tarball SHA-512 mismatch" >&2
  exit 1
fi
rm -rf "${TYPESCRIPT_STAGE}" "${TARGET}${TYPESCRIPT_INSTALL_ROOT}"
mkdir -p "${TYPESCRIPT_STAGE}" "${TARGET}${TYPESCRIPT_INSTALL_ROOT}"
tar -xzf "${TYPESCRIPT_TARBALL}" -C "${TYPESCRIPT_STAGE}" --strip-components=1
test "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["name"])' "${TYPESCRIPT_STAGE}/package.json")" = "${TYPESCRIPT_PACKAGE_NAME}"
test "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "${TYPESCRIPT_STAGE}/package.json")" = "${TYPESCRIPT_VERSION}"
test "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["license"])' "${TYPESCRIPT_STAGE}/package.json")" = "${TYPESCRIPT_LICENSE}"
cp -a "${TYPESCRIPT_STAGE}/lib/." "${TARGET}${TYPESCRIPT_INSTALL_ROOT}/"
cp "${TYPESCRIPT_STAGE}/LICENSE" "${TYPESCRIPT_STAGE}/NOTICE.txt" \
   "${TYPESCRIPT_STAGE}/README.md" "${TYPESCRIPT_STAGE}/package.json" \
   "${TARGET}${TYPESCRIPT_INSTALL_ROOT}/"
chmod 0755 "${TARGET}${TYPESCRIPT_INSTALL_ROOT}/tsc"
actual_tsc_sha256="$(sha256sum "${TARGET}${TYPESCRIPT_INSTALL_ROOT}/tsc" | awk '{print $1}')"
if [ "${actual_tsc_sha256}" != "${TYPESCRIPT_TSC_SHA256}" ]; then
  echo "[rootfs:${ARCH}] native TypeScript tsc SHA-256 mismatch" >&2
  exit 1
fi

# Probe the installed binary DIRECTLY, not through `chroot "${TARGET}"`.
#
# `tsc` here is typescript-go: a statically linked Go binary whose `osvfs`
# package calls `os.Executable()` in its `init()`, which on Linux reads
# `/proc/self/exe`. Nothing mounts `/proc` inside "${TARGET}" — the directory is
# created at the `mkdir -p` above and left empty — so every chroot'd invocation
# dies before `main` with:
#
#   panic: vfs: failed to get executable path: readlink /proc/self/exe:
#          no such file or directory
#
# Mounting proc into the target is the other repair, but this build container
# runs without CAP_SYS_ADMIN (`build-local-app-rootfs.sh` passes no
# `--cap-add`/`--privileged`), so `mount -t proc` there fails with EPERM.
#
# Running it directly costs nothing: the binary is `statically linked` (verified
# with `file`), so it needs no interpreter, no shared library and no other file
# from "${TARGET}" — only the container's own `/proc`, which is mounted. It is
# byte-for-byte the file just installed into the target and hash-checked above,
# so the probe still proves that exact artifact runs.
typescript_version_output="$("${TARGET}${TYPESCRIPT_INSTALL_ROOT}/tsc" --version)"
if [ "${typescript_version_output}" != "Version ${TYPESCRIPT_VERSION}" ]; then
  echo "[rootfs:${ARCH}] native TypeScript version probe failed: ${typescript_version_output}" >&2
  exit 1
fi

# Same reason as the version probe above: direct exec, not chroot.
python3 - <<'PY' | "${TARGET}${TYPESCRIPT_INSTALL_ROOT}/tsc" --lsp --stdio > "${OUT}/typescript-lsp-initialize.out" 2> "${OUT}/typescript-lsp-initialize.err"
import json
import sys

messages = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"processId": None, "rootUri": "file:///tmp", "capabilities": {}}},
    {"jsonrpc": "2.0", "method": "initialized", "params": {}},
    {"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": None},
    {"jsonrpc": "2.0", "method": "exit", "params": None},
]
for message in messages:
    payload = json.dumps(message, separators=(",", ":")).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(payload)}\r\n\r\n".encode() + payload)
PY
grep -q '"capabilities"' "${OUT}/typescript-lsp-initialize.out" || {
  echo "[rootfs:${ARCH}] native TypeScript LSP initialize failed" >&2
  cat "${OUT}/typescript-lsp-initialize.err" >&2
  exit 1
}
rm -rf "${TYPESCRIPT_TARBALL}" "${TYPESCRIPT_STAGE}"

echo "[rootfs:${ARCH}] emitting closure manifest"
python3 - "${PINS_JSON}" "${ARCH}" "${OUT}/closure.json" <<'PY'
import json, pathlib, sys

pins = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
arch, out_path = sys.argv[2:4]
abi = {"aarch64": "arm64-v8a", "x86_64": "x86_64"}[arch]
artifacts = pins["apk_artifacts"][abi]["artifacts"]
pathlib.Path(out_path).write_text(
    json.dumps({"arch": arch, "artifacts": artifacts}, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
print(f"   {len(artifacts)} pinned artifacts")
PY

echo "[rootfs:${ARCH}] packing rootfs tarball"
tar -czf "${OUT}/rootfs.tar.gz" -C "${TARGET}" .
sha256sum "${OUT}/rootfs.tar.gz" | awk '{print "[rootfs] tarball sha256="$1}'
du -sh "${TARGET}" | awk '{print "[rootfs] installed size "$1}'
echo "[rootfs:${ARCH}] done"
