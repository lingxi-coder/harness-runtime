#!/usr/bin/env python3
"""Real filesystem/Cargo fixtures for the project source-identity supplement."""
from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "lib"))
import vendored_source as gate


class SourceGateTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="source-gate-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name) / "repo"
        self.root.mkdir()
        self.manifest = self.root / "vendor/demo/Cargo.toml"
        self.write("vendor/demo/Cargo.toml", '[package]\nname = "demo-fork"\nversion = "0.5.2+upstream.7"\nedition = "2021"\n[lib]\npath = "src/lib.rs"\n')
        self.write("vendor/demo/src/lib.rs", "pub fn value() -> u8 { 1 }\n")
        self.write("native/library/core.c", "int value(void) { return 1; }\n")
        self.write("native/include/core.h", "int value(void);\n")
        self.write("vendor/demo/.editorconfig", "root=true\r\n")
        self.doc = {"format": 1, "git_repository": "https://example.invalid/repo.git", "capsules": [{
            "name": "demo-fork", "base_version": "0.5.2+upstream.7", "manifest": "vendor/demo/Cargo.toml",
            "input_roots": ["vendor/demo", "native"],
            "provenance": {"upstream_repository": "https://example.invalid/upstream", "description": "Synthetic fixture, not a real audit"},
            "files": [], "digest": "",
        }]}
        gate.validate_specs(self.doc["capsules"], production=False)
        gate.propose(self.root, self.doc)
        self.store = Path(self.tmp.name) / "store"
        self.store.mkdir()
        self.write_store(config='[policy.demo-fork]\naudit-as-crates-io=true\n', audits='', imports='')

    def write(self, rel, data):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data.encode() if isinstance(data, str) else data)
        return path

    def write_store(self, config=None, audits=None, imports=None):
        for file, value in (("config.toml", config), ("audits.toml", audits), ("imports.lock", imports)):
            if value is not None:
                (self.store / file).write_text(value, encoding="utf-8")

    def tag(self):
        spec = self.doc["capsules"][0]
        return gate.marked_version(spec["base_version"], spec["digest"])

    def stamp(self):
        gate.stamp(self.root, self.doc)
        gate.verify(self.root, self.doc)

    def path_graph(self):
        return {"packages": [{"name": "demo-fork", "version": self.tag(), "manifest_path": str(self.manifest), "source": None}]}

    def full(self, version, private=True):
        return f'[[audits.demo-fork]]\nwho = "Synthetic fixture reviewer"\ncriteria = "safe-to-deploy"\nversion = "{version}"\nimportable = {str(not private).lower()}\n'

    def git_init(self, root):
        subprocess.run(['git', 'init', '-q', str(root)], check=True, capture_output=True)
        subprocess.run(['git', '-C', str(root), 'add', 'vendor', 'native'], check=True, capture_output=True)
        env = dict(os.environ, GIT_AUTHOR_NAME='Fixture', GIT_AUTHOR_EMAIL='fixture@example.invalid',
                   GIT_COMMITTER_NAME='Fixture', GIT_COMMITTER_EMAIL='fixture@example.invalid')
        subprocess.run(['git', '-C', str(root), 'commit', '-qm', 'synthetic fixture'], env=env, check=True, capture_output=True)
        return subprocess.check_output(['git', '-C', str(root), 'rev-parse', 'HEAD'], text=True).strip()

    def assert_rejected(self, callback):
        with self.assertRaises(gate.GateError):
            callback()

    def test_stamp_and_real_cargo_path_metadata(self):
        self.stamp()
        tag = self.tag()
        self.assertTrue(tag.startswith("0.5.2+upstream.7.lingxi.h"))
        self.assertIn(f'version = "{tag}"', self.manifest.read_text())
        self.assertEqual((self.root / "vendor/demo/.editorconfig").read_bytes(), b"root=true\r\n")
        app = self.write('app/Cargo.toml', '[package]\nname="app"\nversion="1.0.0"\nedition="2021"\n[dependencies]\ndemo-fork={path="../vendor/demo"}\n')
        self.write('app/src/lib.rs', '')
        subprocess.run(["cargo", "generate-lockfile", "--offline", "--manifest-path", str(app)], check=True, capture_output=True, text=True)
        graph = gate.cargo_metadata(app, app.parent)
        p = next(p for p in graph["packages"] if p["name"] == "demo-fork")
        self.assertEqual((p["version"], p["source"]), (tag, None))
        self.write_store(audits=self.full(tag))
        self.assertEqual(gate.verify_audits(self.root, self.doc, self.store, [graph]), 1)

    def test_optional_official_cargo_vet_with_real_fixture(self):
        binary_dir = os.environ.get('SOURCE_GATE_CARGO_VET_DIR')
        if not binary_dir:
            self.skipTest('set SOURCE_GATE_CARGO_VET_DIR for official cargo-vet integration')
        self.stamp()
        app = self.write('app/Cargo.toml', '[package]\nname="app"\nversion="1.0.0"\nedition="2021"\n[dependencies]\ndemo-fork={path="../vendor/demo"}\n')
        self.write('app/src/lib.rs', '')
        subprocess.run(['cargo', 'generate-lockfile', '--offline', '--manifest-path', str(app)], check=True, capture_output=True, text=True)
        self.write_store(config='[cargo-vet]\nversion="0.10"\n[policy.demo-fork]\naudit-as-crates-io=true\n',
                         audits=self.full(self.tag()).replace('Synthetic fixture reviewer', 'Codex AI-assisted (synthetic fixture only)'),
                         imports='# cargo-vet imports lock\n')
        env = dict(os.environ, PATH=binary_dir + os.pathsep + os.environ['PATH'])
        fmt = subprocess.run(['cargo', 'vet', 'fmt', '--manifest-path', str(app), '--store-path', str(self.store)],
                             env=env, text=True, capture_output=True)
        self.assertEqual(fmt.returncode, 0, fmt.stdout + fmt.stderr)
        command = ['cargo', 'vet', 'check', '--manifest-path', str(app), '--store-path', str(self.store),
                   '--cache-dir', str(Path(self.tmp.name) / 'vet-cache'), '--locked', '--frozen',
                   '--no-registry-suggestions', '--no-minimize-exemptions']
        result = subprocess.run(command, env=env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        graph = gate.cargo_metadata(app, app.parent)
        gate.verify_audits(self.root, self.doc, self.store, [graph])
        self.write('native/library/core.c', 'int changed;\n')
        result = subprocess.run(command, env=env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_rejected(lambda: gate.verify_audits(self.root, self.doc, self.store, [graph]))

    def test_changed_added_missing_c_and_mode_rejected(self):
        self.stamp()
        cfile = self.root / "native/library/core.c"
        original = cfile.read_bytes()
        cfile.write_text("int value(void) { return 2; }\n")
        self.assert_rejected(lambda: gate.verify(self.root, self.doc))
        cfile.write_bytes(original)
        added = self.write("native/library/new.c", "int added;\n")
        self.assert_rejected(lambda: gate.verify(self.root, self.doc))
        added.unlink()
        cfile.unlink()
        self.assert_rejected(lambda: gate.verify(self.root, self.doc))
        cfile.write_bytes(original)
        cfile.chmod(0o755)
        self.assert_rejected(lambda: gate.verify(self.root, self.doc))
        cfile.chmod(0o644)
        gate.verify(self.root, self.doc)

    def test_modified_tag_and_non_version_manifest_bytes_rejected(self):
        self.stamp()
        original = self.manifest.read_bytes()
        self.manifest.write_bytes(original.replace(self.tag().encode(), self.tag()[:-1].encode() + b"f"))
        self.assert_rejected(lambda: gate.verify(self.root, self.doc))
        self.manifest.write_bytes(original + b"# changed\n")
        self.assert_rejected(lambda: gate.verify(self.root, self.doc))

    def test_symlink_unknown_manifest_and_nested_target_are_rejected(self):
        self.stamp()
        link = self.root / "vendor/demo/leak"
        link.symlink_to(self.root / "native", target_is_directory=True)
        self.assert_rejected(lambda: gate.verify(self.root, self.doc))
        link.unlink()
        unknown = self.write("vendor/demo/nested/Cargo.toml", '[package]\nname="sneak"\nversion="1.0.0"\n')
        self.assert_rejected(lambda: gate.verify(self.root, self.doc))
        unknown.unlink()
        unknown.parent.rmdir()
        extra = self.write("vendor/demo/subdir/target/input.c", "int leaked;\n")
        self.assert_rejected(lambda: gate.verify(self.root, self.doc))
        extra.unlink()
        extra.parent.rmdir()
        extra.parent.parent.rmdir()
        self.write("vendor/demo/target/generated.o", "transient")
        gate.verify(self.root, self.doc)

    def test_untracked_empty_source_directories_are_rejected(self):
        self.stamp()
        feature = self.root / 'vendor/demo/enable_feature'
        feature.mkdir()
        self.assert_rejected(lambda: gate.verify(self.root, self.doc))
        self.assert_rejected(lambda: gate.propose(self.root, copy.deepcopy(self.doc)))
        feature.rmdir()
        gate.verify(self.root, self.doc)

    def test_missing_review_old_bare_importable_and_alias_rejected(self):
        self.stamp()
        run = lambda: gate.verify_audits(self.root, self.doc, self.store, [self.path_graph()])
        self.assert_rejected(run)
        self.write_store(audits=self.full("0.5.2+upstream.7"))
        self.assert_rejected(run)
        self.write_store(audits=self.full(self.tag(), private=False))
        self.assert_rejected(run)
        self.write_store(audits=self.full(self.tag()), imports=f'[[unpublished.demo-fork]]\nversion="{self.tag()}"\naudited-as="0.5.2+upstream.7"\n')
        self.assert_rejected(run)
        self.write_store(imports='')
        run()
        for config, audits, imports in [
            ('[policy.demo-fork]\naudit-as-crates-io=true\n[[exemptions.demo-fork]]\nversion="0.5.2"\ncriteria="safe-to-deploy"\n', self.full(self.tag()), ''),
            ('[policy.demo-fork]\naudit-as-crates-io=true\n', self.full(self.tag()) + '\n[[trusted.demo-fork]]\nuser-id=1\ncriteria="safe-to-deploy"\n', ''),
            ('[policy.demo-fork]\naudit-as-crates-io=true\n', self.full(self.tag()) + '\n[[wildcard-audits.demo-fork]]\nuser-id=1\ncriteria="safe-to-deploy"\n', ''),
            ('[policy.demo-fork]\naudit-as-crates-io=true\n', self.full(self.tag()), '[[audits.example.wildcard-audits.demo-fork]]\nuser-id=1\ncriteria="safe-to-deploy"\n'),
            ('[policy.demo-fork]\naudit-as-crates-io=false\n', self.full(self.tag()), ''),
            ('', self.full(self.tag()), ''),
        ]:
            self.write_store(config=config, audits=audits, imports=imports)
            self.assert_rejected(run)

    def test_git_and_path_records_cannot_be_interchanged(self):
        self.write('vendor/demo/.editorconfig', 'root=true\n')
        gate.propose(self.root, self.doc)
        self.stamp()
        self.write_store(audits=self.full(self.tag() + '@git:' + 'a' * 40))
        self.assert_rejected(lambda: gate.verify_audits(self.root, self.doc, self.store, [self.path_graph()]))
        self.write_store(audits=self.full(self.tag()))
        git_copy = Path(self.tmp.name) / "git-checkout"
        import shutil
        shutil.copytree(self.root, git_copy)
        sha = self.git_init(git_copy)
        graph = {"packages": [{"name": "demo-fork", "version": self.tag(), "manifest_path": str(git_copy / 'vendor/demo/Cargo.toml'),
                 "source": 'git+https://example.invalid/repo.git?rev=' + sha + '#' + sha}]}
        self.assert_rejected(lambda: gate.verify_audits(self.root, self.doc, self.store, [graph]))
        self.write_store(audits=self.full(self.tag() + '@git:' + sha))
        gate.verify_audits(self.root, self.doc, self.store, [graph])
        (git_copy / 'native/library/core.c').write_text('int changed;\n')
        self.assert_rejected(lambda: gate.verify_audits(self.root, self.doc, self.store, [graph]))

    def test_real_cargo_git_metadata_and_source_audit(self):
        # Git attributes can normalize CRLF on checkout; freeze its actual LF
        # bytes first, as required for a portable source identity.
        self.write('vendor/demo/.editorconfig', 'root=true\n')
        gate.propose(self.root, self.doc)
        self.stamp()
        sha = self.git_init(self.root)
        repo_url = self.root.as_uri()
        self.doc['git_repository'] = repo_url
        app = Path(self.tmp.name) / 'app'
        (app / 'src').mkdir(parents=True)
        (app / 'Cargo.toml').write_text('[package]\nname="synthetic-app"\nversion="1.0.0"\nedition="2021"\n'
                                        f'[dependencies]\ndemo-fork={{git="{repo_url}",rev="{sha}"}}\n')
        (app / 'src/lib.rs').write_text('')
        cargo_home = Path(self.tmp.name) / 'cargo-home'
        with patch.dict(os.environ, {'CARGO_HOME': str(cargo_home)}):
            subprocess.run(['cargo', 'generate-lockfile', '--manifest-path', str(app / 'Cargo.toml')],
                           check=True, capture_output=True, text=True)
            graph = gate.cargo_metadata(app / 'Cargo.toml', app)
        actual = next(p for p in graph['packages'] if p['name'] == 'demo-fork')
        self.assertEqual(actual['source'], f'git+{repo_url}?rev={sha}#{sha}')
        self.write_store(audits=self.full(self.tag()))
        self.assert_rejected(lambda: gate.verify_audits(self.root, self.doc, self.store, [graph]))
        self.write_store(audits=self.full(self.tag() + '@git:' + sha))
        self.assertEqual(gate.verify_audits(self.root, self.doc, self.store, [graph]), 1)
        self.write_store(config=f'[policy."demo-fork:{self.tag()}@git:{sha}"]\naudit-as-crates-io=true\n')
        self.assertEqual(gate.verify_audits(self.root, self.doc, self.store, [graph]), 1)

    def test_git_head_and_committed_blob_cannot_be_spoofed_by_worktree(self):
        self.write('vendor/demo/.editorconfig', 'root=true\n')
        gate.propose(self.root, self.doc)
        self.stamp()
        import shutil
        checkout = Path(self.tmp.name) / 'checkout'
        shutil.copytree(self.root, checkout)
        original_sha = self.git_init(checkout)
        gate.verify_git_tree(checkout, original_sha, self.doc)
        cfile = checkout / 'native/library/core.c'
        original = cfile.read_bytes()
        cfile.write_text('int value(void) { return 999; }\n')
        subprocess.run(['git', '-C', str(checkout), 'add', 'native/library/core.c'], check=True, capture_output=True)
        env = dict(os.environ, GIT_AUTHOR_NAME='Fixture', GIT_AUTHOR_EMAIL='fixture@example.invalid',
                   GIT_COMMITTER_NAME='Fixture', GIT_COMMITTER_EMAIL='fixture@example.invalid')
        subprocess.run(['git', '-C', str(checkout), 'commit', '-qm', 'different committed source'], env=env, check=True, capture_output=True)
        new_sha = subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip()
        cfile.write_bytes(original)
        # The worktree matches the inventory while neither the original HEAD nor
        # the new commit's C blob matches that source claim.
        gate.verify(checkout, self.doc)
        self.assert_rejected(lambda: gate.verify_git_tree(checkout, original_sha, self.doc))
        self.assert_rejected(lambda: gate.verify_git_tree(checkout, new_sha, self.doc))

    def test_git_tree_rejects_uncommitted_input_and_wrong_blob_mode(self):
        self.write('vendor/demo/.editorconfig', 'root=true\n')
        gate.propose(self.root, self.doc)
        self.stamp()
        import shutil
        checkout = Path(self.tmp.name) / 'checkout'
        shutil.copytree(self.root, checkout)
        self.git_init(checkout)
        env = dict(os.environ, GIT_AUTHOR_NAME='Fixture', GIT_AUTHOR_EMAIL='fixture@example.invalid',
                   GIT_COMMITTER_NAME='Fixture', GIT_COMMITTER_EMAIL='fixture@example.invalid')
        subprocess.run(['git', '-C', str(checkout), 'update-index', '--chmod=+x', 'native/library/core.c'], check=True, capture_output=True)
        subprocess.run(['git', '-C', str(checkout), 'commit', '-qm', 'different mode'], env=env, check=True, capture_output=True)
        mode_sha = subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip()
        gate.verify(checkout, self.doc)
        self.assert_rejected(lambda: gate.verify_git_tree(checkout, mode_sha, self.doc))
        subprocess.run(['git', '-C', str(checkout), 'rm', '--cached', '-q', 'native/library/core.c'], check=True, capture_output=True)
        subprocess.run(['git', '-C', str(checkout), 'commit', '-qm', 'missing C blob'], env=env, check=True, capture_output=True)
        missing_sha = subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip()
        gate.verify(checkout, self.doc)
        self.assert_rejected(lambda: gate.verify_git_tree(checkout, missing_sha, self.doc))

    def test_active_graph_scope_and_unknown_registry(self):
        self.write('vendor/other/Cargo.toml', '[package]\nname="other"\nversion="1.2.3"\n')
        other = {"name": "other", "base_version": "1.2.3", "manifest": "vendor/other/Cargo.toml", "input_roots": ["vendor/other"],
                 "provenance": {"upstream_repository": None, "description": "synthetic"}, "files": [], "digest": ""}
        self.doc['capsules'].append(other)
        gate.propose(self.root, self.doc)
        gate.stamp(self.root, self.doc)
        graph = self.path_graph()
        self.write_store(audits=self.full(self.tag()))
        self.assertEqual(gate.verify_audits(self.root, self.doc, self.store, [graph]), 1)
        self.assert_rejected(lambda: gate.verify_audits(self.root, self.doc, self.store, [{'packages': []}]))
        graph['packages'].append({'name': 'demo-fork', 'version': '0.4.0', 'source': 'registry+https://untrusted.example/index',
                                  'manifest_path': '/cache/registry/demo-fork-0.4.0/Cargo.toml'})
        self.assert_rejected(lambda: gate.verify_audits(self.root, self.doc, self.store, [graph]))
        (self.root / 'vendor/other/Cargo.toml').write_text('[package]\nname="other"\nversion="1.2.3"\n# changed\n')
        self.assert_rejected(lambda: gate.verify_audits(self.root, self.doc, self.store, [self.path_graph()]))

    def test_actual_cargo_graph_with_foreign_same_name_path_fork_fails(self):
        self.stamp()
        self.write('foreign/demo/Cargo.toml', '[package]\nname="demo-fork"\nversion="0.5.3"\nedition="2021"\n')
        self.write('foreign/demo/src/lib.rs', 'pub fn foreign() {}\n')
        app = self.write('app/Cargo.toml', '[package]\nname="app"\nversion="1.0.0"\nedition="2021"\n[dependencies]\n'
                         'first={package="demo-fork",path="../vendor/demo"}\nsecond={package="demo-fork",path="../foreign/demo"}\n')
        self.write('app/src/lib.rs', '')
        subprocess.run(['cargo', 'generate-lockfile', '--offline', '--manifest-path', str(app)], check=True, capture_output=True, text=True)
        graph = gate.cargo_metadata(app, app.parent)
        self.assertEqual(len([p for p in graph['packages'] if p['name'] == 'demo-fork']), 2)
        self.write_store(audits=self.full(self.tag()))
        self.assert_rejected(lambda: gate.verify_audits(self.root, self.doc, self.store, [graph]))

    def test_actual_cargo_graph_with_same_name_registry_and_path_fork(self):
        # cfg-if 1.0.0 must already be cached; no network or source downloads.
        self.manifest.write_text(self.manifest.read_text().replace('demo-fork', 'cfg-if'))
        self.doc['capsules'][0]['name'] = 'cfg-if'
        gate.propose(self.root, self.doc)
        gate.stamp(self.root, self.doc)
        app = self.write('app/Cargo.toml', '[package]\nname="app"\nversion="1.0.0"\nedition="2021"\n[dependencies]\n'
                         'first={package="cfg-if",path="../vendor/demo"}\nsecond={package="cfg-if",version="=1.0.0"}\n')
        self.write('app/src/lib.rs', '')
        result = subprocess.run(['cargo', 'generate-lockfile', '--offline', '--manifest-path', str(app)], capture_output=True, text=True)
        if result.returncode and 'no matching package named' in result.stderr:
            self.skipTest('cfg-if 1.0.0 not available in local Cargo cache')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        graph = gate.cargo_metadata(app, app.parent)
        packages = [p for p in graph['packages'] if p['name'] == 'cfg-if']
        self.assertEqual(len(packages), 2)
        self.assertEqual(len([p for p in packages if p['source'] and p['source'].startswith('registry+')]), 1)
        self.write_store(config='[policy.cfg-if]\naudit-as-crates-io=true\n', audits=self.full(self.tag()).replace('demo-fork', 'cfg-if'))
        self.assertEqual(gate.verify_audits(self.root, self.doc, self.store, [graph]), 1)

    def test_cargo_metadata_uses_explicit_build_cwd(self):
        self.stamp()
        self.write('foreign/demo/Cargo.toml', f'[package]\nname="demo-fork"\nversion="{self.tag()}"\nedition="2021"\n')
        self.write('foreign/demo/src/lib.rs', 'pub fn changed() {}\n')
        app = self.write('app/Cargo.toml', '[package]\nname="app"\nversion="1.0.0"\nedition="2021"\n[dependencies]\ndemo-fork="=0.5.2"\n')
        self.write('app/src/lib.rs', '')
        self.write('.cargo/config.toml', '[patch.crates-io]\ndemo-fork={path=' + json.dumps(str(self.manifest.parent)) + '}\n')
        self.write('app/.cargo/config.toml', '[patch.crates-io]\ndemo-fork={path=' + json.dumps(str(self.root / 'foreign/demo')) + '}\n')
        cargo_home = Path(self.tmp.name) / 'cargo-cwd-home'
        with patch.dict(os.environ, {'CARGO_HOME': str(cargo_home)}):
            subprocess.run(['cargo', 'generate-lockfile', '--offline', '--manifest-path', str(app)], cwd=self.root,
                           check=True, capture_output=True, text=True)
            good = gate.cargo_metadata(app, self.root)
            bad = gate.cargo_metadata(app, app.parent)
        self.write_store(audits=self.full(self.tag()))
        self.assertEqual(gate.verify_audits(self.root, self.doc, self.store, [good]), 1)
        self.assert_rejected(lambda: gate.verify_audits(self.root, self.doc, self.store, [bad]))

    def test_real_cargo_registry_replacement_and_mutated_cache_rejected(self):
        import hashlib
        import shutil
        import tarfile
        self.manifest.write_text(self.manifest.read_text().replace('demo-fork', 'cfg-if'))
        self.doc['capsules'][0]['name'] = 'cfg-if'
        gate.propose(self.root, self.doc)
        gate.stamp(self.root, self.doc)
        app = self.write('app/Cargo.toml', '[package]\nname="app"\nversion="1.0.0"\nedition="2021"\n[dependencies]\n'
                         'first={package="cfg-if",path="../vendor/demo"}\nsecond={package="cfg-if",version="=1.0.0"}\n')
        self.write('app/src/lib.rs', '')
        result = subprocess.run(['cargo', 'generate-lockfile', '--offline', '--manifest-path', str(app)], cwd=app.parent, capture_output=True, text=True)
        if result.returncode and 'no matching package named' in result.stderr:
            self.skipTest('cfg-if 1.0.0 not available in local Cargo cache')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        original_graph = gate.cargo_metadata(app, app.parent)
        registry_pkg = next(p for p in original_graph['packages'] if p['name'] == 'cfg-if' and p['source'])
        cargo_home = Path(original_graph['_source_gate_evidence']['cargo_home'])
        index = Path(registry_pkg['manifest_path']).relative_to(cargo_home / 'registry/src').parts[0]
        archive = cargo_home / 'registry/cache' / index / 'cfg-if-1.0.0.crate'
        replacement = self.root / 'replacement/cfg-if-1.0.0'
        with tarfile.open(archive, 'r:gz') as stream:
            for item in stream:
                if item.isfile():
                    dest = replacement.joinpath(*Path(item.name).parts[1:])
                    dest.parent.mkdir(parents=True, exist_ok=True)
                    dest.write_bytes(stream.extractfile(item).read())
                    dest.chmod(item.mode & 0o777)
        lib = replacement / 'src/lib.rs'
        original = lib.read_bytes()
        lib.write_bytes(original + b'\npub const MODIFIED_SOURCE_SENTINEL: u8 = 42;\n')
        files = {p.relative_to(replacement).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest() for p in replacement.rglob('*') if p.is_file()}
        (replacement / '.cargo-checksum.json').write_text(json.dumps({'package': hashlib.sha256(archive.read_bytes()).hexdigest(), 'files': files}))
        self.write('app/.cargo/config.toml', '[source.crates-io]\nreplace-with="fixture"\n[source.fixture]\ndirectory=' + json.dumps(str(replacement.parent)) + '\n')
        replaced = gate.cargo_metadata(app, app.parent)
        actual = next(p for p in replaced['packages'] if p['name'] == 'cfg-if' and p['source'])
        self.assertEqual(actual['source'], 'registry+https://github.com/rust-lang/crates.io-index')
        self.assertEqual(Path(actual['manifest_path']).resolve(), (replacement / 'Cargo.toml').resolve())
        self.write_store(config='[policy.cfg-if]\naudit-as-crates-io=true\n', audits=self.full(self.tag()).replace('demo-fork', 'cfg-if'))
        self.assert_rejected(lambda: gate.verify_audits(self.root, self.doc, self.store, [replaced]))
        # Even if replacement bytes are placed under a Cargo-looking cache path,
        # the lock-pinned archive, not the directory's checksum JSON, controls.
        home = Path(self.tmp.name) / 'fake-cargo-home'
        cached = home / 'registry/src' / index / 'cfg-if-1.0.0'
        cached.parent.mkdir(parents=True)
        shutil.copytree(replacement, cached)
        (cached / '.cargo-checksum.json').unlink()
        dest_archive = home / 'registry/cache' / index / archive.name
        dest_archive.parent.mkdir(parents=True)
        shutil.copyfile(archive, dest_archive)
        fake_graph = copy.deepcopy(original_graph)
        fake_graph['_source_gate_evidence']['cargo_home'] = str(home)
        fake_pkg = next(p for p in fake_graph['packages'] if p['name'] == 'cfg-if' and p['source'])
        fake_pkg['manifest_path'] = str(cached / 'Cargo.toml')
        self.assert_rejected(lambda: gate.verify_registry_source(fake_pkg, fake_graph))
        (cached / 'src/lib.rs').write_bytes(original)
        gate.verify_registry_source(fake_pkg, fake_graph)
        empty = cached / 'src/empty-feature'
        empty.mkdir()
        self.assert_rejected(lambda: gate.verify_registry_source(fake_pkg, fake_graph))
        empty.rmdir()
        extra = cached / 'src/injected.c'
        extra.write_text('int injected;\n')
        self.assert_rejected(lambda: gate.verify_registry_source(fake_pkg, fake_graph))
        extra.unlink()
        (cached / 'src/lib.rs').chmod(0o755)
        self.assert_rejected(lambda: gate.verify_registry_source(fake_pkg, fake_graph))
        (cached / 'src/lib.rs').chmod(0o644)
        (cached / 'src/lib.rs').unlink()
        self.assert_rejected(lambda: gate.verify_registry_source(fake_pkg, fake_graph))
        (cached / 'src/lib.rs').write_bytes(original)
        dest_archive.write_bytes(dest_archive.read_bytes() + b'corrupt')
        self.assert_rejected(lambda: gate.verify_registry_source(fake_pkg, fake_graph))

    def test_registry_archive_rejects_path_traversal_and_links(self):
        import hashlib
        import io
        import tarfile
        home = Path(self.tmp.name) / 'cargo-home'
        index = 'synthetic-registry'
        source = home / 'registry/src' / index / 'cfg-if-1.0.0'
        source.mkdir(parents=True)
        (source / 'Cargo.toml').write_text('[package]\nname="cfg-if"\nversion="1.0.0"\n')
        archive = home / 'registry/cache' / index / 'cfg-if-1.0.0.crate'
        archive.parent.mkdir(parents=True)
        lock = Path(self.tmp.name) / 'Cargo.lock'
        graph = {'_source_gate_evidence': {'cargo_home': str(home), 'lock': str(lock), 'cwd': str(self.root)}}
        pkg = {'name': 'cfg-if', 'version': '1.0.0', 'source': 'registry+https://github.com/rust-lang/crates.io-index',
               'manifest_path': str(source / 'Cargo.toml')}
        for kind in ('traversal', 'absolute', 'symlink', 'hardlink', 'duplicate'):
            with tarfile.open(archive, 'w:gz') as stream:
                names = ['cfg-if-1.0.0/../escape'] if kind == 'traversal' else ['/absolute'] if kind == 'absolute' else ['cfg-if-1.0.0/bad']
                if kind == 'duplicate':
                    names *= 2
                for name in names:
                    info = tarfile.TarInfo(name)
                    if kind in ('symlink', 'hardlink'):
                        info.type = tarfile.SYMTYPE if kind == 'symlink' else tarfile.LNKTYPE
                        info.linkname = 'elsewhere'
                        stream.addfile(info)
                    else:
                        info.size = 1
                        stream.addfile(info, io.BytesIO(b'x'))
            sha = hashlib.sha256(archive.read_bytes()).hexdigest()
            lock.write_text('[[package]]\nname="cfg-if"\nversion="1.0.0"\nsource="registry+https://github.com/rust-lang/crates.io-index"\nchecksum="' + sha + '"\n')
            with self.assertRaisesRegex(gate.GateError, 'unsafe|unsupported|duplicate'):
                gate.verify_registry_source(pkg, graph)

    def test_claimed_canonical_git_with_wrong_manifest_never_skipped(self):
        self.stamp()
        sha = 'a' * 40
        graph = self.path_graph()
        graph['packages'].append({'name': 'demo-fork', 'version': self.tag(),
                                  'source': 'git+https://example.invalid/repo.git?rev=' + sha + '#' + sha,
                                  'manifest_path': str(self.root / 'somewhere-else/Cargo.toml')})
        self.write_store(audits=self.full(self.tag()))
        self.assert_rejected(lambda: gate.verify_audits(self.root, self.doc, self.store, [graph]))

    def test_delta_must_have_full_base_and_private_exact_git_target(self):
        version = self.tag() + '@git:' + 'a'*40
        target = f'[[audits.demo-fork]]\nwho="Synthetic"\ncriteria="safe-to-deploy"\ndelta="0.5.2 -> {version}"\nimportable=false\n'
        def parse(text):
            import tomllib
            return tomllib.loads(text)['audits']['demo-fork']
        self.assertFalse(gate.has_local_audit(parse(target), self.tag(), 'a'*40))
        self.assertTrue(gate.has_local_audit(parse(self.full('0.5.2') + target), self.tag(), 'a'*40))
        self.assertFalse(gate.has_local_audit(parse(self.full('0.5.2') + target.replace('importable=false', 'importable=true')), self.tag(), 'a'*40))
        trick = f'[[audits.demo-fork]]\nwho="Synthetic"\ncriteria="safe-to-deploy"\nversion="{version}"\nimportable=true\n' + target.replace('0.5.2 ->', version + ' ->')
        self.assertFalse(gate.has_local_audit(parse(trick), self.tag(), 'a'*40))

    def test_shared_manifest_versions_normalized_before_stamp(self):
        self.write('vendor/other/Cargo.toml', '[package]\nname="other"\nversion="1.2.3"\n')
        other = {"name": "other", "base_version": "1.2.3", "manifest": "vendor/other/Cargo.toml", "input_roots": ["vendor/other"],
                 "provenance": {"upstream_repository": None, "description": "synthetic"}, "files": [], "digest": ""}
        self.doc["capsules"][0]["input_roots"].append('vendor/other')
        self.doc["capsules"].append(other)
        gate.propose(self.root, self.doc)
        before = [c['digest'] for c in self.doc['capsules']]
        gate.stamp(self.root, self.doc)
        after, _ = gate.collect(self.root, self.doc['capsules'])
        self.assertEqual(before, [x[1] for x in after])
        gate.verify(self.root, self.doc)

    def test_stamp_refuses_unreviewed_change_without_partial_writes(self):
        self.write('native/library/core.c', 'int changed;\n')
        before = self.manifest.read_bytes()
        self.assert_rejected(lambda: gate.stamp(self.root, self.doc))
        self.assertEqual(self.manifest.read_bytes(), before)

    def test_walk_errors_fail_closed_for_proposal_check_and_stamp(self):
        import errno
        real_scandir = os.scandir
        blocked = (self.root / 'native/library').resolve()
        for err in (errno.EACCES, errno.EIO):
            def scan(path):
                if Path(path) == blocked:
                    raise OSError(err, os.strerror(err), str(path))
                return real_scandir(path)
            with patch.object(gate.os, 'scandir', side_effect=scan):
                self.assert_rejected(lambda: gate.propose(self.root, copy.deepcopy(self.doc)))
                self.assert_rejected(lambda: gate.verify(self.root, self.doc, stamped=False))
                self.assert_rejected(lambda: gate.stamp(self.root, self.doc))
        gate.verify(self.root, self.doc, stamped=False)

    def test_reviewed_source_update_requires_prior_identity(self):
        self.stamp()
        old = copy.deepcopy(self.doc)
        self.write('native/library/core.c', 'int value(void) { return 4; }\n')
        new = gate.propose(self.root, copy.deepcopy(old))
        before = self.manifest.read_bytes()
        self.assert_rejected(lambda: gate.stamp(self.root, new))
        self.assertEqual(self.manifest.read_bytes(), before)
        wrong = copy.deepcopy(old)
        wrong['capsules'][0]['digest'] = '0' * 64
        self.assert_rejected(lambda: gate.stamp(self.root, new, previous=wrong))
        gate.stamp(self.root, new, previous=old)
        gate.verify(self.root, new)

    def test_base_version_upgrade_with_shared_stamped_inputs(self):
        other_manifest = self.write('vendor/other/Cargo.toml', '[package]\nname="other"\nversion="1.2.3"\n')
        other = {"name": "other", "base_version": "1.2.3", "manifest": "vendor/other/Cargo.toml", "input_roots": ["vendor/other"],
                 "provenance": {"upstream_repository": None, "description": "synthetic"}, "files": [], "digest": ""}
        self.doc['capsules'][0]['input_roots'].append('vendor/other')
        self.doc['capsules'].append(other)
        gate.propose(self.root, self.doc)
        gate.stamp(self.root, self.doc)
        old = copy.deepcopy(self.doc)
        old_tag = gate.marked_version('1.2.3', old['capsules'][1]['digest'])
        other_manifest.write_bytes(gate.rewrite_version(other_manifest.read_bytes(), old_tag, '1.2.4', 'fixture'))
        new = copy.deepcopy(old)
        new['capsules'][1]['base_version'] = '1.2.4'
        gate.propose(self.root, new)
        self.assert_rejected(lambda: gate.stamp(self.root, new))
        gate.stamp(self.root, new, previous=old)
        gate.verify(self.root, new)

    def test_cli_proposes_only_explicit_new_base(self):
        import importlib.util
        cli_path = Path(__file__).resolve().parents[1] / 'checks/check-vendored-sources.py'
        module_spec = importlib.util.spec_from_file_location('source_gate_cli_test', cli_path)
        cli = importlib.util.module_from_spec(module_spec)
        module_spec.loader.exec_module(cli)
        self.manifest.write_text(self.manifest.read_text().replace('0.5.2+upstream.7', '0.5.3'))
        inventory = Path(self.tmp.name) / 'old.json'
        inventory.write_text(json.dumps(self.doc))
        out = Path(self.tmp.name) / 'proposal.json'
        args = ['gate', '--inventory', str(inventory), '--propose', str(out), '--base-version', 'vendor/demo/Cargo.toml=0.5.3']
        with patch.object(cli, 'ROOT', self.root), patch.object(sys, 'argv', args), patch.object(cli.gate, 'read_doc', return_value=copy.deepcopy(self.doc)):
            cli.main()
        proposed = json.loads(out.read_text())
        self.assertEqual(proposed['capsules'][0]['base_version'], '0.5.3')
        gate.verify(self.root, proposed, stamped=False)
        self.assertEqual(json.loads(inventory.read_text()), self.doc)


if __name__ == '__main__':
    unittest.main(verbosity=2)
