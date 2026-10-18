#!/usr/bin/env python3
"""Generate a source-only proof for the r8-to-final-round single-file H delta.

This records source snapshot identity and file/blob closure only. It does not
validate gate exits or claim that any final-round phase passed. Intermediate r9
source identity is retained separately when finalizing r10.
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
DELTA_PATH = "crates/runtime/src/headless/run.rs"
SCHEMA = "r376-r8-final-single-production-file-proof-v1"


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
    parser.add_argument("--intermediate-dir", type=Path, default=HERE / "harness-repair-gates-r9")
    parser.add_argument("--final-dir", type=Path, default=HERE / "harness-repair-gates-r10")
    parser.add_argument("--delta-patch-out", type=Path, default=HERE / "r8-r10-production-delta.patch")
    parser.add_argument("--proof-out", type=Path, default=HERE / "r8-r10-source-followup-proof.json")
    args = parser.parse_args()

    r8_dir = args.r8_dir.resolve(strict=True)
    intermediate_dir = args.intermediate_dir.resolve(strict=True)
    final_dir = args.final_dir.resolve(strict=True)
    r8_patch_path = r8_dir / "source.patch"
    intermediate_patch_path = intermediate_dir / "source.patch"
    final_patch_path = final_dir / "source.patch"
    r8_current_path = r8_dir / "source-current.patch"
    intermediate_current_path = intermediate_dir / "source-current.patch"
    final_current_path = final_dir / "source-current.patch"
    for path in (r8_patch_path, intermediate_patch_path, final_patch_path,
                 r8_current_path, intermediate_current_path, final_current_path):
        require(path.is_file() and not path.is_symlink(), f"source snapshot is missing or symlinked: {path}")

    r8_base = (r8_dir / "base-head.txt").read_text(encoding="utf-8").strip()
    intermediate_base = (intermediate_dir / "base-head.txt").read_text(encoding="utf-8").strip()
    final_base = (final_dir / "base-head.txt").read_text(encoding="utf-8").strip()
    intermediate_candidate = (intermediate_dir / "candidate-head.txt").read_text(encoding="utf-8").strip()
    final_candidate = (final_dir / "candidate-head.txt").read_text(encoding="utf-8").strip()
    require(r8_base == intermediate_base == final_base == BASELINE,
            "r8/intermediate/final snapshots do not use the frozen Harness baseline")
    require(intermediate_candidate == final_candidate == git("rev-parse", "HEAD"),
            "intermediate/final candidate HEADs differ from the source baseline commit")
    candidate = final_candidate

    r8_patch = r8_patch_path.read_bytes()
    intermediate_patch = intermediate_patch_path.read_bytes()
    final_patch = final_patch_path.read_bytes()
    r8_current = r8_current_path.read_bytes()
    intermediate_current = intermediate_current_path.read_bytes()
    final_current = final_current_path.read_bytes()
    require(r8_current == r8_patch, "r8 source changed after its gate snapshot")
    require(intermediate_current == intermediate_patch, "intermediate r9 source changed after its gate snapshot")
    require(final_current == final_patch, "final source changed after its gate snapshot")

    for name, directory, patch_path, patch_bytes in (
        ("r8", r8_dir, r8_patch_path, r8_patch),
        ("r9", intermediate_dir, intermediate_patch_path, intermediate_patch),
        ("final", final_dir, final_patch_path, final_patch),
    ):
        sidecar_path = directory / "source.patch.sha256"
        require(sidecar_path.is_file() and not sidecar_path.is_symlink(), f"{name} source patch SHA sidecar is missing")
        sidecar_tokens = sidecar_path.read_text(encoding="utf-8").split()
        require(sidecar_tokens and sidecar_tokens[0] == sha_bytes(patch_bytes),
                f"{name} source patch SHA sidecar does not match its patch")

    r8_sections = diff_sections(r8_patch)
    intermediate_sections = diff_sections(intermediate_patch)
    final_sections = diff_sections(final_patch)
    changed_paths_r8_intermediate = sorted(
        path for path in set(r8_sections) | set(intermediate_sections)
        if r8_sections.get(path) != intermediate_sections.get(path)
    )
    require(changed_paths_r8_intermediate == [DELTA_PATH],
            f"r8/intermediate source delta exceeds the one authorized path: {changed_paths_r8_intermediate}")
    changed_paths_r8_final = sorted(
        path for path in set(r8_sections) | set(final_sections)
        if r8_sections.get(path) != final_sections.get(path)
    )
    require(changed_paths_r8_final == [DELTA_PATH],
            f"r8/final source delta exceeds the one authorized path: {changed_paths_r8_final}")
    changed_paths_intermediate_final = sorted(
        path for path in set(intermediate_sections) | set(final_sections)
        if intermediate_sections.get(path) != final_sections.get(path)
    )
    require(changed_paths_intermediate_final == [DELTA_PATH],
            f"intermediate/final source delta exceeds the one authorized path: {changed_paths_intermediate_final}")
    require(DELTA_PATH not in r8_sections and DELTA_PATH in intermediate_sections and DELTA_PATH in final_sections,
            "the source snapshots must isolate run.rs as the only added file section")

    committed_r8_patch = git("diff", "--binary", f"{BASELINE}..{candidate}", "--", "crates", "Cargo.toml", "Cargo.lock", binary=True)
    require(committed_r8_patch == r8_patch,
            "r8 tested patch does not exactly represent the frozen source through the final candidate HEAD")
    base_blob = str(git("rev-parse", f"{BASELINE}:{DELTA_PATH}"))
    previous_blob = str(git("rev-parse", f"{candidate}:{DELTA_PATH}"))
    require(base_blob == previous_blob,
            "the final candidate already differs from baseline in run.rs; a single-file added-section proof is unsafe")
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
    final_delta_section = re.sub(
        rb"(?m)^index [0-9a-f]+\.\.([0-9a-f]+)(.*)$",
        lambda match: b"index " + previous_blob.encode("ascii") + b".." + current_blob.encode("ascii") + match.group(2),
        final_sections[DELTA_PATH],
    )
    require(delta_patch == final_delta_section,
            "final run.rs patch section does not equal the exact candidate-to-worktree production delta")
    index_match = re.search(rb"(?m)^index [0-9a-f]+\.\.([0-9a-f]+)", delta_patch)
    require(index_match is not None, "final delta patch has no Git index blob header")
    require(current_blob == index_match.group(1).decode("ascii"),
            "final delta patch new blob does not match the current run.rs source")

    outputs = []
    for raw_path in (args.delta_patch_out, args.proof_out):
        path = raw_path if raw_path.is_absolute() else HERE / raw_path
        resolved_parent = path.parent.resolve()
        require(resolved_parent.is_relative_to(HERE.resolve()),
                f"proof output must be under final-ledger-r376: {path}")
        resolved = resolved_parent / path.name
        require(not resolved.exists() and not resolved.is_symlink(),
                f"proof output already exists: {resolved}")
        outputs.append(resolved)
    delta_out, proof_out = outputs

    delta_out.write_bytes(delta_patch)
    intermediate_name = intermediate_dir.name.removeprefix("harness-repair-gates-")
    final_name = final_dir.name.removeprefix("harness-repair-gates-")
    proof = {
        "schema": SCHEMA,
        "status": "verified_source_delta",
        "gate_status": "not_assessed_by_source_proof",
        "intermediate_deltas": {
            "r8_to_r9_changed_paths": changed_paths_r8_intermediate,
            "r9_to_final_changed_paths": changed_paths_intermediate_final,
        },
        "previous_snapshot": {
            "name": "r8",
            "base_head": r8_base,
            "candidate_head": candidate,
            "source_patch": os.path.relpath(r8_patch_path, HERE).replace(os.sep, "/"),
            "source_patch_sha256": sha_bytes(r8_patch),
            "source_current_patch_sha256": sha_bytes(r8_current),
        },
        "intermediate_snapshots": [{
            "name": intermediate_name,
            "base_head": intermediate_base,
            "candidate_head": candidate,
            "source_patch": os.path.relpath(intermediate_patch_path, HERE).replace(os.sep, "/"),
            "source_patch_sha256": sha_bytes(intermediate_patch),
            "source_current_patch_sha256": sha_bytes(intermediate_current),
        }],
        "current_snapshot": {
            "name": final_name,
            "base_head": final_base,
            "candidate_head": candidate,
            "source_patch": os.path.relpath(final_patch_path, HERE).replace(os.sep, "/"),
            "source_patch_sha256": sha_bytes(final_patch),
            "source_current_patch_sha256": sha_bytes(final_current),
        },
        "all_other_source_diff_sections_identical": True,
        "changed_paths": changed_paths_r8_final,
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
        "required_final_gates": ["fmt", "runtime-lib", "changed-clippy"],
    }
    proof_out.write_text(json.dumps(proof, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({
        "status": proof["status"],
        "gate_status": proof["gate_status"],
        "changed_paths": changed_paths_r8_final,
        "r8_source_patch_sha256": sha_bytes(r8_patch),
        "intermediate_source_patch_sha256": sha_bytes(intermediate_patch),
        "final_source_patch_sha256": sha_bytes(final_patch),
        "delta_patch_sha256": sha_bytes(delta_patch),
        "proof": str(proof_out),
    }, ensure_ascii=False, indent=2))
    return proof


if __name__ == "__main__":
    try:
        run()
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        raise SystemExit(f"make-r8-final-source-proof: {error}") from error
