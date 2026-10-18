#!/usr/bin/env python3
"""Generate a source-only proof for the r8-to-r9 single-file Harness delta.

This records source snapshot identity and file/blob closure only. It does not
validate gate exits or claim that any r9 phase passed.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
BASE = HERE.parents[1]
HARNESS = BASE / "harness-runtime"
BASELINE = "4d6d0d6c428e6624a303f5e2a5c96eb4a392f1a1"
EXPECTED_CANDIDATE = "b3864c6ca6c16ed682a333ad96f7ad59cd05fb0b"
EXPECTED_R9_PATCH_SHA256 = "8470e956a493f59b7bbbd932af78b849c612822f3d6da342abd8e89006a2e51b"
DELTA_PATH = "crates/runtime/src/headless/run.rs"
SCHEMA = "r376-r8-r9-single-production-file-proof-v1"


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def sha_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha_file(path: Path) -> str:
    return sha_bytes(path.read_bytes())


def git(*args: str, binary: bool = False) -> str | bytes:
    result = subprocess.run(
        ["git", "-C", str(HARNESS), *args],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode:
        raise ValueError(
            f"git command failed: {' '.join(args)}: "
            f"{result.stderr.decode('utf-8', errors='replace')}"
        )
    return result.stdout if binary else result.stdout.decode("utf-8").strip()


def diff_sections(patch: bytes) -> dict[str, bytes]:
    sections: dict[str, bytes] = {}
    chunks = re.split(rb"(?=^diff --git a/)", patch, flags=re.MULTILINE)
    for chunk in chunks:
        if not chunk:
            continue
        match = re.match(rb"diff --git a/(.*?) b/(.*?)\n", chunk)
        require(match is not None, "source patch has bytes outside a standard diff section")
        old_path, new_path = (value.decode("utf-8") for value in match.groups())
        require(old_path == new_path, f"source patch renames are not allowed: {old_path} -> {new_path}")
        require(old_path not in sections, f"duplicate source patch section: {old_path}")
        sections[old_path] = chunk
    return sections


def run() -> dict[str, Any]:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--r8-dir", type=Path, default=HERE / "harness-repair-gates-r8")
    parser.add_argument("--r9-dir", type=Path, default=HERE / "harness-repair-gates-r9")
    parser.add_argument("--delta-patch-out", type=Path, default=HERE / "r8-r9-production-delta.patch")
    parser.add_argument("--proof-out", type=Path, default=HERE / "r8-r9-source-followup-proof.json")
    args = parser.parse_args()

    r8_dir = args.r8_dir.resolve(strict=True)
    r9_dir = args.r9_dir.resolve(strict=True)
    r8_patch_path = r8_dir / "source.patch"
    r9_patch_path = r9_dir / "source.patch"
    r8_current_path = r8_dir / "source-current.patch"
    r9_current_path = r9_dir / "source-current.patch"
    for path in (r8_patch_path, r9_patch_path, r8_current_path, r9_current_path):
        require(path.is_file() and not path.is_symlink(), f"source snapshot is missing or symlinked: {path}")

    r8_base = (r8_dir / "base-head.txt").read_text(encoding="utf-8").strip()
    r9_base = (r9_dir / "base-head.txt").read_text(encoding="utf-8").strip()
    candidate = (r9_dir / "candidate-head.txt").read_text(encoding="utf-8").strip()
    require(r8_base == r9_base == BASELINE, "r8/r9 snapshots do not use the frozen Harness baseline")
    require(candidate == EXPECTED_CANDIDATE and git("rev-parse", "HEAD") == candidate,
            "r9 candidate HEAD differs from the currently pinned pre-follow-up commit")

    r8_patch = r8_patch_path.read_bytes()
    r9_patch = r9_patch_path.read_bytes()
    r8_current = r8_current_path.read_bytes()
    r9_current = r9_current_path.read_bytes()
    require(r8_current == r8_patch, "r8 source changed after its gate snapshot")
    require(r9_current == r9_patch, "r9 source changed after its gate snapshot")
    require(sha_bytes(r9_patch) == EXPECTED_R9_PATCH_SHA256,
            "r9 source patch SHA differs from the source snapshot selected by Root")

    for name, directory, patch_path, patch_bytes in (
        ("r8", r8_dir, r8_patch_path, r8_patch),
        ("r9", r9_dir, r9_patch_path, r9_patch),
    ):
        sidecar_path = directory / "source.patch.sha256"
        require(sidecar_path.is_file() and not sidecar_path.is_symlink(), f"{name} source patch SHA sidecar is missing")
        sidecar_tokens = sidecar_path.read_text(encoding="utf-8").split()
        require(sidecar_tokens and sidecar_tokens[0] == sha_bytes(patch_bytes),
                f"{name} source patch SHA sidecar does not match its patch")

    r8_sections = diff_sections(r8_patch)
    r9_sections = diff_sections(r9_patch)
    changed_paths = sorted(
        path for path in set(r8_sections) | set(r9_sections)
        if r8_sections.get(path) != r9_sections.get(path)
    )
    require(changed_paths == [DELTA_PATH], f"r8/r9 source delta exceeds the one authorized path: {changed_paths}")
    require(DELTA_PATH not in r8_sections and DELTA_PATH in r9_sections,
            "r9 run.rs change is not a single newly added patch section")

    committed_r8_patch = git("diff", "--binary", f"{BASELINE}..{candidate}", "--", "crates", "Cargo.toml", "Cargo.lock", binary=True)
    require(committed_r8_patch == r8_patch,
            "r8 tested patch does not exactly represent the frozen source through the r9 candidate HEAD")
    base_blob = str(git("rev-parse", f"{BASELINE}:{DELTA_PATH}"))
    previous_blob = str(git("rev-parse", f"{candidate}:{DELTA_PATH}"))
    require(base_blob == previous_blob,
            "the r8 candidate already differs from baseline in run.rs; a single-file added-section proof is unsafe")
    old_bytes = git("show", f"{candidate}:{DELTA_PATH}", binary=True)
    current_bytes = (HARNESS / DELTA_PATH).read_bytes()
    hash_result = subprocess.run(
        ["git", "-C", str(HARNESS), "hash-object", "--stdin"],
        input=current_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    require(hash_result.returncode == 0,
            f"could not identify current run.rs blob: {hash_result.stderr.decode('utf-8', errors='replace')}")
    current_blob = hash_result.stdout.decode("ascii").strip()
    delta_patch = git("diff", "--binary", "--full-index", candidate, "--", DELTA_PATH, binary=True)
    r9_delta = re.sub(
        rb"(?m)^index [0-9a-f]+\.\.([0-9a-f]+)(.*)$",
        lambda match: b"index " + previous_blob.encode("ascii") + b".." + current_blob.encode("ascii") + match.group(2),
        r9_sections[DELTA_PATH],
    )
    require(delta_patch == r9_delta,
            "r9 run.rs patch section does not equal the exact candidate-to-worktree production delta")
    index_match = re.search(rb"(?m)^index [0-9a-f]+\.\.([0-9a-f]+)", delta_patch)
    require(index_match is not None, "r9 delta patch has no Git index blob header")
    require(current_blob == index_match.group(1).decode("ascii"),
            "r9 delta patch new blob does not match the current run.rs source")

    delta_out = args.delta_patch_out
    proof_out = args.proof_out
    for path in (delta_out, proof_out):
        require(path.is_absolute() or path.resolve().is_relative_to(HERE.resolve()),
                f"proof output must be under final-ledger-r376: {path}")
        if not path.is_absolute():
            path = (HERE / path).resolve()
        require(not path.exists() and not path.is_symlink(), f"proof output already exists: {path}")
    delta_out = delta_out if delta_out.is_absolute() else HERE / delta_out
    proof_out = proof_out if proof_out.is_absolute() else HERE / proof_out
    require(delta_out.resolve().parent.is_relative_to(HERE.resolve())
            and proof_out.resolve().parent.is_relative_to(HERE.resolve()),
            "proof outputs must remain under final-ledger-r376")

    delta_out.write_bytes(delta_patch)
    proof = {
        "schema": SCHEMA,
        "status": "verified_source_delta",
        "gate_status": "not_assessed_by_source_proof",
        "previous_snapshot": {
            "name": "r8",
            "base_head": r8_base,
            "candidate_head": candidate,
            "source_patch": os.path.relpath(r8_patch_path, HERE).replace(os.sep, "/"),
            "source_patch_sha256": sha_bytes(r8_patch),
            "source_current_patch_sha256": sha_bytes(r8_current),
        },
        "current_snapshot": {
            "name": "r9",
            "base_head": r9_base,
            "candidate_head": candidate,
            "source_patch": os.path.relpath(r9_patch_path, HERE).replace(os.sep, "/"),
            "source_patch_sha256": sha_bytes(r9_patch),
            "source_current_patch_sha256": sha_bytes(r9_current),
        },
        "all_other_source_diff_sections_identical": True,
        "changed_paths": changed_paths,
        "unchanged_source_section_sha256": {
            path: sha_bytes(section) for path, section in sorted(r8_sections.items()) if path != DELTA_PATH
        },
        "delta": {
            "path": DELTA_PATH,
            "patch": os.path.relpath(delta_out, HERE).replace(os.sep, "/"),
            "patch_sha256": sha_bytes(delta_patch),
            "previous_git_blob": previous_blob,
            "current_git_blob": current_blob,
            "previous_content_sha256": sha_bytes(old_bytes),
            "current_content_sha256": sha_bytes(current_bytes),
            "baseline_git_blob": base_blob,
        },
        "required_r9_gates": ["fmt", "runtime-lib", "changed-clippy"],
    }
    proof_out.write_text(json.dumps(proof, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({
        "status": proof["status"],
        "gate_status": proof["gate_status"],
        "changed_paths": changed_paths,
        "r8_source_patch_sha256": sha_bytes(r8_patch),
        "r9_source_patch_sha256": sha_bytes(r9_patch),
        "delta_patch_sha256": sha_bytes(delta_patch),
        "proof": str(proof_out),
    }, ensure_ascii=False, indent=2))
    return proof


if __name__ == "__main__":
    try:
        run()
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        raise SystemExit(f"make-r8-r9-source-proof: {error}") from error
