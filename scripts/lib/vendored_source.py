"""Project-specific source identity supplement for cargo-vet, not a source review.

This hashes the checked-in capsule inputs, not system libraries, SDKs or build tools.
Only the package.version literal in each listed Cargo.toml is canonicalized; every
other file byte, pathname and executable bit is included. See the candidate report
for the external build-input contract and limitations.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess
import tarfile
import tempfile
import tomllib


class GateError(Exception):
    pass


# This is an allowlist, not an automatically discovered trust decision. Changes to
# the input boundary must be reviewed in both this file and the inventory.
PRODUCTION = {
    "third_party/git2-rs/git2": ("git2", ["third_party/git2-rs/git2"]),
    "third_party/git2-rs/libgit2-sys": ("libgit2-sys", ["third_party/git2-rs/libgit2-sys", "third_party/libssh2-sys", "third_party/mbedtls-sys", "third_party/mbedtls"]),
    "third_party/libssh2-sys": ("libssh2-sys", ["third_party/libssh2-sys", "third_party/mbedtls-sys", "third_party/mbedtls"]),
    "third_party/icu_collator": ("icu_collator", ["third_party/icu_collator"]),
    "third_party/syntect": ("syntect", ["third_party/syntect"]),
    "third_party/xcap": ("xcap", ["third_party/xcap"]),
    "third_party/mbedtls-sys": ("mbedtls-sys", ["third_party/mbedtls-sys", "third_party/mbedtls"]),
    "third_party/asn1-rs-impl": ("asn1-rs-impl", ["third_party/asn1-rs-impl"]),
    "third_party/num-threads": ("num_threads", ["third_party/num-threads"]),
    "third_party/openssl-probe": ("openssl-probe", ["third_party/openssl-probe"]),
    "third_party/windows-targets-0.48.5": ("windows-targets", ["third_party/windows-targets-0.48.5"]),
    "third_party/windows-targets-0.52.6": ("windows-targets", ["third_party/windows-targets-0.52.6"]),
    "packages/webrtc/vendor/webrtc": ("webrtc", ["packages/webrtc/vendor/webrtc"]),
    "packages/webrtc/vendor/nix": ("nix", ["packages/webrtc/vendor/nix"]),
    "third_party/rustls-native-certs": ("rustls-native-certs", ["third_party/rustls-native-certs"]),
}
REPO = "https://github.com/lingxi-coder/harness-runtime.git"
VERSION = re.compile(r"(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?\Z")
HASH = re.compile(r"[0-9a-f]{64}\Z")
GIT = re.compile(r"[0-9a-f]{40}\Z")
SECTION = re.compile(rb"^\s*\[([^\[\]]+)\]\s*(?:\#.*)?(?:\r?\n)?$")
VALUE = re.compile(rb'^(\s*version\s*=\s*)(["\'])([^"\'\r\n]+)\2(\s*(?:\#.*)?)(\r?\n)?$')


def die(message):
    raise GateError(message)


def relpath(value):
    if not isinstance(value, str) or not value or "\\" in value:
        die(f"invalid relative path: {value!r}")
    p = PurePosixPath(value)
    if p.is_absolute() or any(x in ("", ".", "..") for x in value.split("/")) or str(p) != value:
        die(f"invalid relative path: {value!r}")
    return value


def checked_path(root, path, must_exist=True):
    path = relpath(path)
    cur = root
    for component in path.split("/"):
        cur = cur / component
        try:
            st = cur.lstat()
        except FileNotFoundError:
            if must_exist:
                die(f"missing path: {path}")
            return cur
        if stat.S_ISLNK(st.st_mode):
            die(f"symlink rejected: {cur}")
    return cur


def parse_manifest(data, path):
    try:
        obj = tomllib.loads(data.decode("utf-8"))
        package = obj["package"]
        name, version = package["name"], package["version"]
    except (KeyError, ValueError, UnicodeDecodeError, TypeError) as exc:
        die(f"invalid package manifest {path}: {exc}")
    if not isinstance(name, str) or not isinstance(version, str) or not VERSION.fullmatch(version):
        die(f"invalid package identity in {path}")
    return name, version, package.get("repository")


def rewrite_version(data, old, new, path):
    """Replace only the single, literal [package] version, preserving all else."""
    lines = data.splitlines(keepends=True)
    active = False
    found = 0
    for i, line in enumerate(lines):
        section = SECTION.match(line)
        if section:
            active = section.group(1).strip() == b"package"
            continue
        if active:
            match = VALUE.match(line)
            if match:
                found += 1
                if match.group(3).decode("ascii") != old:
                    die(f"nonliteral or unexpected version in {path}")
                lines[i] = match.group(1) + match.group(2) + new.encode("ascii") + match.group(2) + match.group(4) + (match.group(5) or b"")
    if found != 1:
        die(f"expected one literal [package] version in {path}, found {found}")
    result = b"".join(lines)
    if parse_manifest(result, path)[1] != new:
        die(f"version rewrite failed for {path}")
    return result


def marked_version(base, digest):
    if not VERSION.fullmatch(base) or ".lingxi.h" in base or "+lingxi.h" in base or not HASH.fullmatch(digest):
        die(f"invalid baseline or digest: {base!r}")
    return base + ("." if "+" in base else "+") + "lingxi.h" + digest


def valid_current(base, current):
    prefix = base + ("." if "+" in base else "+") + "lingxi.h"
    return current == base or (current.startswith(prefix) and HASH.fullmatch(current[len(prefix):]) is not None)


def validate_specs(specs, production=True):
    if not isinstance(specs, list) or not specs:
        die("missing capsules")
    names = set()
    for item in specs:
        if set(item) != {"name", "base_version", "manifest", "input_roots", "provenance", "files", "digest"}:
            die("capsule has unknown or missing fields")
        manifest = relpath(item["manifest"])
        if not manifest.endswith("/Cargo.toml") or manifest in names:
            die(f"invalid or duplicate manifest {manifest}")
        names.add(manifest)
        roots = item["input_roots"]
        if not isinstance(roots, list) or not roots or len(set(roots)) != len(roots):
            die(f"invalid input roots for {manifest}")
        for root in roots:
            relpath(root)
        if not isinstance(item["name"], str) or not VERSION.fullmatch(item["base_version"]):
            die(f"invalid capsule identity for {manifest}")
        if not isinstance(item["provenance"], dict) or set(item["provenance"]) != {"upstream_repository", "description"}:
            die(f"invalid provenance for {manifest}")
    if production:
        actual = {s["manifest"][:-len("/Cargo.toml")]: (s["name"], s["input_roots"]) for s in specs}
        if actual != PRODUCTION:
            die("capsules or input roots differ from the code-reviewed allowlist")


def read_doc(path, production=True):
    try:
        doc = json.loads(Path(path).read_text("utf-8"))
    except (OSError, ValueError) as exc:
        die(f"cannot read inventory: {exc}")
    if set(doc) != {"format", "git_repository", "capsules"} or doc["format"] != 1:
        die("unknown inventory format")
    if not isinstance(doc["git_repository"], str) or (production and doc["git_repository"] != REPO):
        die("unexpected Git repository")
    validate_specs(doc["capsules"], production)
    return doc


def manifest_inputs(root, specs):
    manifest_paths = {x["manifest"] for x in specs}
    originals = {}
    for spec in specs:
        path = spec["manifest"]
        data = checked_path(root, path).read_bytes()
        name, actual, _ = parse_manifest(data, path)
        if name != spec["name"] or not valid_current(spec["base_version"], actual):
            die(f"unexpected name/version in {path}: {name} {actual}")
        originals[path] = (actual, data, rewrite_version(data, actual, spec["base_version"], path))
    return manifest_paths, originals


def collect(root, specs):
    """Compute all capsules with all package manifest versions normalized together."""
    root = Path(root).resolve(strict=True)
    if not root.is_dir():
        die("repository root must be a directory")
    manifest_paths, manifests = manifest_inputs(root, specs)
    generated_dirs = {p[:-len("Cargo.toml")] + "target" for p in manifest_paths}
    result = []
    def walk_error(exc):
        die(f"cannot scan source input: {exc}")
    for spec in specs:
        all_files = {}
        all_dirs = set()
        for rootname in spec["input_roots"]:
            start = checked_path(root, rootname)
            if not start.is_dir():
                die(f"input root is not a directory: {rootname}")
            for current, dirs, files in os.walk(start, topdown=True, onerror=walk_error, followlinks=False):
                current_path = Path(current)
                all_dirs.add(current_path.relative_to(root).as_posix())
                for name in sorted(dirs + files):
                    path = Path(current) / name
                    rel = path.relative_to(root).as_posix()
                    mode = path.lstat().st_mode
                    if stat.S_ISLNK(mode):
                        die(f"symlink rejected: {rel}")
                    if rel in generated_dirs:
                        if not stat.S_ISDIR(mode):
                            die(f"generated target is not a directory: {rel}")
                        dirs.remove(name)
                        continue
                    if stat.S_ISDIR(mode):
                        continue
                    if not stat.S_ISREG(mode):
                        die(f"special file rejected: {rel}")
                    if name == "Cargo.toml" and rel not in manifest_paths:
                        die(f"unknown package manifest inside source inputs: {rel}")
                    data = manifests[rel][2] if rel in manifests else path.read_bytes()
                    entry = {"path": rel, "size": len(data), "executable": mode & 0o111, "sha256": hashlib.sha256(data).hexdigest()}
                    if rel in all_files and all_files[rel] != entry:
                        die(f"inconsistent overlapping input: {rel}")
                    all_files[rel] = entry
        implied_dirs = {str(parent) for name in all_files for parent in PurePosixPath(name).parents if str(parent) != "."}
        unbound_dirs = all_dirs - implied_dirs
        if unbound_dirs:
            die("source input contains unbound empty directories: " + ", ".join(sorted(unbound_dirs)[:5]))
        files = [all_files[p] for p in sorted(all_files)]
        if spec["manifest"] not in all_files:
            die(f"manifest outside its source inputs: {spec['manifest']}")
        # Length-delimited fields make path/content/permission identities unambiguous.
        digest = hashlib.sha256(b"lingxi-vendored-source-v1\0")
        for entry in files:
            path_bytes = entry["path"].encode("utf-8")
            digest.update(len(path_bytes).to_bytes(8, "big"))
            digest.update(path_bytes)
            digest.update(entry["executable"].to_bytes(2, "big"))
            digest.update(entry["size"].to_bytes(8, "big"))
            digest.update(bytes.fromhex(entry["sha256"]))
        result.append((files, digest.hexdigest()))
    return result, manifests


def verify(root, doc, stamped=True, previous=None):
    results, manifests = collect(root, doc["capsules"])
    for spec, (files, digest) in zip(doc["capsules"], results):
        if spec["files"] != files or spec["digest"] != digest:
            die(f"inventory differs for {spec['manifest']}: expected {spec['digest']}, got {digest}; added, removed, or changed inputs require review")
        actual = manifests[spec["manifest"]][0]
        target = marked_version(spec["base_version"], digest)
        if stamped and actual != target:
            die(f"unstamped or wrong version: {spec['manifest']} has {actual}, expected {target}")
        valid = [spec["base_version"], target]
        if previous is not None and previous[spec["manifest"]]["base_version"] == spec["base_version"]:
            old = previous[spec["manifest"]]
            valid.append(marked_version(spec["base_version"], old["digest"]))
        if not stamped and actual not in valid:
            die(f"wrong existing version tag: {spec['manifest']}")
    return results, manifests


def propose(root, doc):
    results, manifests = collect(root, doc["capsules"])
    for spec, (files, digest) in zip(doc["capsules"], results):
        # A proposal may refresh file hashes, but cannot silently change identities.
        actual = manifests[spec["manifest"]][0]
        old = spec["digest"]
        if old and actual not in (spec["base_version"], marked_version(spec["base_version"], old)):
            die(f"unexpected current version for proposal: {spec['manifest']}")
        spec["files"], spec["digest"] = files, digest
    return doc


def stamp(root, doc, previous=None):
    """Preflight every capsule then atomically replace manifests; rollback on error."""
    root = Path(root).resolve(strict=True)
    previous_by_path = None
    if previous is not None:
        previous_by_path = {x["manifest"]: x for x in previous["capsules"]}
        if set(previous_by_path) != {x["manifest"] for x in doc["capsules"]}:
            die("previous inventory covers a different set of manifests")
        for current in doc["capsules"]:
            old = previous_by_path[current["manifest"]]
            if any(old[k] != current[k] for k in ("name", "input_roots")):
                die(f"previous capsule boundary differs: {current['manifest']}")
    results, manifests = verify(root, doc, stamped=False, previous=previous_by_path)
    replacements = []
    for spec, (_, digest) in zip(doc["capsules"], results):
        path = checked_path(root, spec["manifest"])
        old_version, old_data, _ = manifests[spec["manifest"]]
        new = marked_version(spec["base_version"], digest)
        replacements.append((path, old_data, rewrite_version(old_data, old_version, new, spec["manifest"]), path.stat().st_mode & 0o777))
    def replace(path, data, mode):
        fd, temp = tempfile.mkstemp(prefix=".vendored-version-", dir=path.parent)
        try:
            with os.fdopen(fd, "wb") as handle:
                handle.write(data)
                handle.flush()
                os.fsync(handle.fileno())
            os.chmod(temp, mode)
            os.replace(temp, path)
        finally:
            if os.path.exists(temp):
                os.unlink(temp)
    done = []
    try:
        for path, old, new, mode in replacements:
            if path.read_bytes() != old:
                die(f"source changed during stamp: {path}")
            if old != new:
                replace(path, new, mode)
                done.append((path, old, new, mode))
        verify(root, doc, stamped=True)
    except Exception as exc:
        conflicts = []
        for path, data, written, mode in reversed(done):
            if path.read_bytes() != written:
                conflicts.append(str(path))
            else:
                replace(path, data, mode)
        if conflicts:
            die(f"stamp failed ({exc}); concurrent edits prevented rollback of: {', '.join(conflicts)}")
        raise


def criterion(record):
    value = record.get("criteria", [])
    return "safe-to-deploy" in ([value] if isinstance(value, str) else value if isinstance(value, list) else [])


def read_toml(path):
    try:
        with open(path, "rb") as handle:
            return tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        die(f"cannot read cargo-vet store file {path}: {exc}")


def reject_aliases(doc, config, audits, imports):
    names = {x["name"] for x in doc["capsules"]}
    def reject(table, label):
        if not isinstance(table, dict):
            die(f"invalid {label} table")
        matches = names.intersection(table)
        if matches:
            die(f"{label} is forbidden for covered names: {', '.join(sorted(matches))}")
    reject(config.get("exemptions", {}), "exemptions")
    reject(audits.get("trusted", {}), "trusted publishers")
    reject(audits.get("wildcard-audits", {}), "wildcard audits")
    reject(imports.get("unpublished", {}), "unpublished aliases")
    for imported in imports.get("audits", {}).values():
        if not isinstance(imported, dict):
            die("invalid imported audit table")
        reject(imported.get("wildcard-audits", {}), "imported wildcard audits")
        reject(imported.get("trusted", {}), "imported trusted publishers")


def has_local_audit(records, version, git_sha):
    """Only accept explicit local private target records; delta chains are local."""
    target = version + ("@git:" + git_sha if git_sha else "")
    allowed = [x for x in records if isinstance(x, dict) and criterion(x) and x.get("who")]
    full = {x.get("version") for x in allowed if isinstance(x.get("version"), str) and "delta" not in x and x.get("version") != target}
    private_full = any(x.get("version") == target and "delta" not in x and x.get("importable") is False for x in allowed)
    if private_full:
        return True
    if not git_sha:
        return False
    edges = []
    for x in allowed:
        delta = x.get("delta")
        if "version" not in x and isinstance(delta, str) and delta.count(" -> ") == 1:
            before, after = delta.split(" -> ")
            edges.append((before, after, x.get("importable") is False))
    reachable = set(full)
    while True:
        expanded = reachable | {after for before, after, _ in edges if before in reachable and after != target}
        if expanded == reachable:
            break
        reachable = expanded
    return any(after == target and private and before in reachable for before, after, private in edges)


def cargo_metadata(manifest, cwd):
    # Cargo discovers .cargo/config.toml from cwd, not from --manifest-path.
    # Callers must pass the same cwd as the build whose graph is being checked.
    cwd = Path(cwd).resolve(strict=True)
    if not cwd.is_dir():
        die(f"Cargo working directory is not a directory: {cwd}")
    manifest = Path(manifest).resolve(strict=True)
    proc = subprocess.run(["cargo", "metadata", "--locked", "--offline", "--format-version", "1", "--all-features", "--manifest-path", str(manifest)], cwd=cwd, text=True, capture_output=True)
    if proc.returncode:
        die(f"cargo metadata failed for {manifest}: {proc.stderr.strip()}")
    try:
        graph = json.loads(proc.stdout)
    except ValueError as exc:
        die(f"cargo metadata returned invalid JSON: {exc}")
    workspace = graph.get("workspace_root")
    if not isinstance(workspace, str) or not Path(workspace).is_absolute():
        die("Cargo metadata did not identify its workspace root")
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    if not cargo_home.is_absolute():
        cargo_home = cwd / cargo_home
    graph["_source_gate_evidence"] = {"lock": str(Path(workspace) / "Cargo.lock"),
                                      "cargo_home": str(cargo_home.resolve()), "cwd": str(cwd)}
    return graph


def verify_git_tree(checkout, revision, doc):
    """Bind the working copy to the exact Cargo Git commit and its blob bytes/modes."""
    checkout = Path(checkout).resolve(strict=True)
    if not GIT.fullmatch(revision):
        die("Git revision must be a full SHA-1")
    # Do not let an inherited alternate Git directory, index, replacement
    # object, or user configuration redefine which commit/tree is examined.
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    env.update({"GIT_NO_REPLACE_OBJECTS": "1", "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull})
    def git(*args):
        proc = subprocess.run(["git", "-C", str(checkout), *args], capture_output=True, env=env)
        if proc.returncode:
            die(f"cannot verify Git checkout {checkout}: {proc.stderr.decode('utf-8', 'replace').strip()}")
        return proc.stdout
    if Path(os.fsdecode(git("rev-parse", "--show-toplevel").strip())).resolve(strict=True) != checkout:
        die(f"Git source is not the checkout root: {checkout}")
    if git("rev-parse", "--show-object-format").strip() != b"sha1":
        die("unsupported Git object format")
    if git("rev-parse", "--verify", "HEAD^{commit}").strip().decode("ascii") != revision:
        die(f"Git HEAD differs from metadata revision: {checkout}")
    results, _ = verify(checkout, doc, stamped=True)
    expected = {entry["path"] for files, _ in results for entry in files}
    roots = sorted({item for capsule in doc["capsules"] for item in capsule["input_roots"]})
    output = git("ls-tree", "-r", "-z", "--full-tree", revision, "--", *roots)
    tree = {}
    for record in output.split(b"\0"):
        if not record:
            continue
        try:
            meta, raw_path = record.split(b"\t", 1)
            mode, kind, oid = meta.split(b" ")
            path = raw_path.decode("utf-8")
        except (ValueError, UnicodeDecodeError):
            die("cannot parse Git tree entry")
        if path in tree or mode not in (b"100644", b"100755") or kind != b"blob":
            die(f"unsupported or duplicate Git tree entry: {path}")
        tree[path] = (mode, oid)
    if set(tree) != expected:
        missing = sorted(expected - set(tree))[:5]
        extra = sorted(set(tree) - expected)[:5]
        die(f"Git tree source inventory differs: uncommitted={missing}, extra committed={extra}")
    for rel, (mode, oid) in tree.items():
        path = checked_path(checkout, rel)
        data = path.read_bytes()
        content_hash = hashlib.sha1(b"blob " + str(len(data)).encode("ascii") + b"\0" + data).hexdigest().encode("ascii")
        executable = bool(path.stat().st_mode & 0o111)
        if content_hash != oid or executable != (mode == b"100755"):
            die(f"Git blob or mode differs from checkout: {rel}")


def verify_registry_source(package, graph):
    """Compare same-name registry source to its lock-pinned cached archive."""
    evidence = graph.get("_source_gate_evidence")
    if not isinstance(evidence, dict) or set(evidence) != {"lock", "cargo_home", "cwd"}:
        die("registry source has no Cargo graph/lock evidence")
    name, version, source = package.get("name"), package.get("version"), package.get("source")
    if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z0-9_-]+", name) or not isinstance(version, str) or not VERSION.fullmatch(version):
        die("invalid registry package identity")
    lock = read_toml(evidence["lock"])
    records = [x for x in lock.get("package", []) if isinstance(x, dict) and
               x.get("name") == name and x.get("version") == version and x.get("source") == source]
    if len(records) != 1 or not isinstance(records[0].get("checksum"), str) or not HASH.fullmatch(records[0]["checksum"]):
        die(f"registry package has no unique lock checksum: {name} {version}")
    checksum = records[0]["checksum"]
    cargo_home = Path(evidence["cargo_home"])
    src_root = cargo_home / "registry" / "src"
    manifest = Path(package.get("manifest_path", ""))
    try:
        parts = manifest.relative_to(src_root).parts
    except ValueError:
        die(f"registry package is not in the Cargo source cache: {manifest}")
    if len(parts) != 3 or parts[1] != f"{name}-{version}" or parts[2] != "Cargo.toml":
        die(f"unexpected registry source cache path: {manifest}")
    archive = cargo_home / "registry" / "cache" / parts[0] / f"{name}-{version}.crate"
    if not archive.is_file():
        die(f"registry archive missing: {archive}")
    with archive.open("rb") as handle:
        if hashlib.file_digest(handle, "sha256").hexdigest() != checksum:
            die(f"registry archive checksum does not match Cargo.lock: {archive}")
    source_root = manifest.parent.resolve(strict=True)
    source_files = {}
    source_dirs = set()
    def walk_error(exc):
        die(f"cannot scan registry source: {exc}")
    for current, dirs, files in os.walk(source_root, onerror=walk_error, followlinks=False):
        current_path = Path(current)
        source_dirs.add(current_path.relative_to(source_root).as_posix())
        for child in sorted(dirs + files):
            path = Path(current) / child
            rel = path.relative_to(source_root).as_posix()
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode) or not (stat.S_ISREG(mode) or stat.S_ISDIR(mode)):
                die(f"unsupported registry source entry: {path}")
            if stat.S_ISDIR(mode):
                continue
            data = path.read_bytes()
            if rel == ".cargo-ok" and data == b'{"v":1}' and not mode & 0o111:
                continue
            source_files[rel] = (len(data), mode & 0o111, hashlib.sha256(data).hexdigest())
    implied_dirs = {str(parent) for name in source_files for parent in PurePosixPath(name).parents}
    unbound_dirs = source_dirs - implied_dirs
    if unbound_dirs:
        die("registry source contains unbound empty directories: " + ", ".join(sorted(unbound_dirs)[:5]))
    archive_files = {}
    seen = set()
    prefix = f"{name}-{version}"
    try:
        with tarfile.open(archive, "r:gz") as stream:
            for member in stream:
                member_name = member.name.rstrip("/")
                pieces = member_name.split("/")
                if not pieces or pieces[0] != prefix or any(x in ("", ".", "..") for x in pieces):
                    die(f"unsafe registry archive path: {member.name}")
                if member_name in seen:
                    die(f"duplicate registry archive path: {member.name}")
                seen.add(member_name)
                if member.isdir():
                    continue
                if not member.isfile() or len(pieces) < 2:
                    die(f"unsupported registry archive entry: {member.name}")
                rel = "/".join(pieces[1:])
                if rel == ".cargo-ok":
                    die("registry archive may not contain Cargo's generated marker")
                contents = stream.extractfile(member)
                if contents is None:
                    die(f"cannot read registry archive file: {member.name}")
                with contents:
                    digest = hashlib.file_digest(contents, "sha256").hexdigest()
                archive_files[rel] = (member.size, member.mode & 0o111, digest)
    except (tarfile.TarError, OSError) as exc:
        die(f"cannot read registry archive {archive}: {exc}")
    if source_files != archive_files:
        die(f"registry source differs from lock-pinned archive: {name} {version}")


def verify_audits(root, doc, store, metadatas):
    root = Path(root).resolve(strict=True)
    verify(root, doc, stamped=True)
    store = Path(store).resolve(strict=True)
    config, audits, imports = (read_toml(store / x) for x in ("config.toml", "audits.toml", "imports.lock"))
    reject_aliases(doc, config, audits, imports)
    entries = audits.get("audits", {})
    if not isinstance(entries, dict):
        die("invalid local audit table")
    seen = set()
    checked_git_roots = set()
    covered_names = {spec["name"] for spec in doc["capsules"]}
    for graph in metadatas:
        if not isinstance(graph, dict) or not isinstance(graph.get("packages"), list):
            die("invalid cargo metadata")
        for package in graph["packages"]:
            name = package.get("name")
            if name not in covered_names:
                continue
            source = package.get("source")
            # A separately resolved crates.io version is not this path/Git
            # fork. Unknown registries and other sources are not assumed safe.
            if source in ("registry+https://github.com/rust-lang/crates.io-index", "sparse+https://index.crates.io/"):
                verify_registry_source(package, graph)
                continue
            manifest_path = package.get("manifest_path")
            if not isinstance(manifest_path, str) or not Path(manifest_path).is_absolute():
                die(f"invalid manifest path for covered name: {name}")
            path = Path(manifest_path)
            matches = [spec for spec in doc["capsules"] if spec["name"] == name and str(path).endswith("/" + spec["manifest"])]
            if len(matches) != 1:
                die(f"unknown same-name non-registry source for {name}: {source!r} {path}")
            for spec in matches:
                relative = Path(spec["manifest"])
                checkout = path
                for _ in relative.parts:
                    checkout = checkout.parent
                checkout = checkout.resolve(strict=True)
                actual = marked_version(spec["base_version"], spec["digest"])
                if package.get("version") != actual:
                    die(f"metadata uses unexpected version for {spec['manifest']}")
                git_sha = None
                if source is None:
                    if checkout != root:
                        die(f"unexpected path source root: {checkout}")
                else:
                    prefix = "git+" + doc["git_repository"] + "?rev="
                    if not isinstance(source, str) or not source.startswith(prefix) or source.count("#") != 1:
                        die(f"unrecognized source for {spec['manifest']}: {source!r}")
                    rev, git_sha = source[len(prefix):].split("#")
                    if rev != git_sha or not GIT.fullmatch(git_sha) or checkout == root:
                        die(f"Git source must name exact separate 40-character revision: {source}")
                    if (checkout, git_sha) not in checked_git_roots:
                        verify_git_tree(checkout, git_sha, doc)
                        checked_git_roots.add((checkout, git_sha))
                policy = config.get("policy", {})
                if not isinstance(policy, dict):
                    die("invalid cargo-vet policy")
                exact = spec["name"] + ":" + actual + ("@git:" + git_sha if git_sha else "")
                plain_entry, exact_entry = policy.get(spec["name"]), policy.get(exact)
                if plain_entry is not None and exact_entry is not None:
                    die(f"ambiguous cargo-vet policy for {exact}")
                entry = exact_entry if exact_entry is not None else plain_entry
                if not isinstance(entry, dict) or entry.get("audit-as-crates-io") is not True:
                    die(f"cargo-vet must classify this fork as third-party: {exact}")
                if not has_local_audit(entries.get(spec["name"], []), actual, git_sha):
                    die(f"missing local private exact {'Git' if git_sha else 'path'} safe-to-deploy audit: {spec['name']} {actual}")
                seen.add((spec["manifest"], source))
    if not seen:
        die("no covered path/Git fork appears in the supplied Cargo graphs")
    return len(seen)
