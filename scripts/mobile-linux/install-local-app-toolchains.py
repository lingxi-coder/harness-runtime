#!/usr/bin/env python3
"""Install pinned non-APK toolchains inside the native Alpine builder."""
import base64
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tarfile
import urllib.request

pins = json.loads(pathlib.Path('/pins.json').read_text())
root, arch, cache = pathlib.Path(sys.argv[1]), sys.argv[2], pathlib.Path(sys.argv[3])
cache.mkdir(parents=True, exist_ok=True)


def fetch(pin, algorithm):
    destination = cache / pathlib.PurePosixPath(pin['url']).name
    expected = pin[algorithm]
    def valid():
        if not destination.is_file():
            return False
        digest = hashlib.new(algorithm, destination.read_bytes()).digest()
        actual = digest.hex() if algorithm == 'sha256' else base64.b64encode(digest).decode()
        return actual == expected
    if not valid():
        temporary = destination.with_suffix(destination.suffix + '.tmp')
        urllib.request.urlretrieve(pin['url'], temporary)
        temporary.replace(destination)
    if not valid():
        raise SystemExit(f'Artifact digest mismatch: {destination.name}')
    return destination


def extract(archive, destination):
    destination.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive) as tar:
        tar.extractall(destination, filter='data')


def npm_package(pin, destination):
    stage = cache / ('unpack-' + destination.name)
    shutil.rmtree(stage, ignore_errors=True)
    extract(fetch(pin, 'sha512'), stage)
    manifest = json.loads((stage / 'package/package.json').read_text())
    if (manifest.get('name') != pin.get('name', destination.name)
            or manifest.get('version') != pin.get('version', pins['pnpm']['version'])):
        raise SystemExit(f'Package identity mismatch: {destination.name}')
    shutil.rmtree(destination, ignore_errors=True)
    shutil.copytree(stage / 'package', destination)
    shutil.rmtree(stage)


node = pins['node_source']
source_archive = fetch(node, 'sha256')
source = cache / f"node-v{node['version']}"
if not source.exists():
    stage = cache / 'node-extract'
    shutil.rmtree(stage, ignore_errors=True)
    extract(source_archive, stage)
    (stage / source.name).rename(source)
    shutil.rmtree(stage)
binary = source / 'out/Release/node'
build_record_path = cache / 'node-build.json'
try:
    build_record = json.loads(build_record_path.read_text())
except (OSError, json.JSONDecodeError):
    build_record = {}
cache_valid = (
    build_record.get('pin') == node
    and build_record.get('architecture') == arch
    and binary.is_file()
    and build_record.get('binary_sha256') == hashlib.sha256(binary.read_bytes()).hexdigest()
)
print(f"[toolchains] {'reusing verified' if cache_valid else 'building'} Node {node['version']} ({arch})", flush=True)
if not cache_valid:
    # Builder dependencies never enter the guest. Pin direct build tools and
    # retain the resolved package list with the resulting binary's provenance.
    subprocess.run(['apk', 'add', '--no-cache', *[f'{name}={version}' for name, version in node['build_packages'].items()]], check=True)
    subprocess.run(['./configure', *node['configure_args']], cwd=source, check=True)
    subprocess.run(['make', '-j' + os.environ.get('LINGXI_NODE_BUILD_JOBS', '4')], cwd=source, check=True)
    subprocess.run(['strip', '--strip-unneeded', str(binary)], check=True)
    build_record = dict(pin=node, architecture=arch,
                        binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                        resolved_build_packages=sorted(subprocess.check_output(['apk', 'info', '-v'], text=True).splitlines()))
    build_record_path.write_text(json.dumps(build_record, indent=2) + '\n')
(root / 'usr/bin').mkdir(parents=True, exist_ok=True)
shutil.copy2(binary, root / 'usr/bin/node')
metadata_dir = root / 'opt/lingxi/toolchains/node'
metadata_dir.mkdir(parents=True, exist_ok=True)
shutil.copy2(source / 'LICENSE', metadata_dir / 'LICENSE')
provenance = dict(node, architecture=arch, binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                  resolved_build_packages=build_record['resolved_build_packages'])
(metadata_dir / 'provenance.json').write_text(json.dumps(provenance, indent=2) + '\n')
modules = root / 'usr/lib/node_modules'
npm_package(pins['npm'], modules / 'npm')
for name, target in [('npm', 'npm-cli.js'), ('npx', 'npx-cli.js')]:
    path = root / 'usr/bin' / name
    path.unlink(missing_ok=True)
    path.symlink_to('../lib/node_modules/npm/bin/' + target)
pnpm_package = pins['pnpm']['packages'][arch]
npm_package(pins['pnpm'], modules / 'pnpm')
npm_package(pnpm_package, modules / 'pnpm/node_modules' / pnpm_package['name'])
pnpm_binary = modules / 'pnpm/node_modules' / pnpm_package['name'] / 'pnpm'
# Native binary beside dist/ lets pnpm find its bundled node-gyp payload.
native_destination = modules / 'pnpm/pnpm'
native_destination.unlink(missing_ok=True)
shutil.copy2(pnpm_binary, native_destination)
pnpm_binary.unlink()
pnpm_binary.symlink_to('../../../pnpm')
path = root / 'usr/bin/pnpm'
path.unlink(missing_ok=True)
path.symlink_to('../lib/node_modules/pnpm/pnpm')
for name, expected in [('node', 'v' + node['version']), ('npm', pins['npm']['version']), ('pnpm', pins['pnpm']['version'])]:
    actual = subprocess.check_output(['chroot', str(root), '/usr/bin/' + name, '--version'], text=True).strip()
    if actual != expected:
        raise SystemExit(f'{name} version mismatch: {actual!r} != {expected!r}')
    print(f'[toolchains] {name}: {actual}', flush=True)
