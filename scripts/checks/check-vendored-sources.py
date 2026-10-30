#!/usr/bin/env python3
"""Check the project's vendored source identity; this does not audit the code.

Usage: python3.11 scripts/checks/check-vendored-sources.py
       python3.11 scripts/checks/check-vendored-sources.py --propose /outside/inputs/new.json
       python3.11 scripts/checks/check-vendored-sources.py --stamp
       python3.11 scripts/checks/check-vendored-sources.py --audit-store supply-chain --cargo-manifest Cargo.toml --cargo-cwd .

Proposal is a review artifact, never an approval or an audit. --stamp changes all
covered package manifests and requires an already-reviewed matching inventory;
Cargo lockfiles must subsequently be regenerated. Audit mode runs cargo metadata
locked and offline, rejects aliases/exemptions/trust for covered names, and
requires explicit private exact records for all capsules present in the graph(s).
At least one covered path/Git fork must be present. Unused capsules are source
identity-checked but are not claimed as audit-verified for that graph.
Each --cargo-cwd must match the working directory of the corresponding build.
This is a project source gate, not cargo-vet's native path identity proof.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts" / "lib"))
try:
    import vendored_source as gate
except ModuleNotFoundError as exc:
    if exc.name == "tomllib":
        sys.exit("Python 3.11 or later is required")
    raise


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    action = parser.add_mutually_exclusive_group()
    action.add_argument("--propose", type=Path, metavar="OUTPUT", help="write a reviewable replacement inventory outside all input roots")
    action.add_argument("--stamp", action="store_true", help="explicitly stamp matching reviewed inventory into package manifests")
    parser.add_argument("--inventory", type=Path, default=ROOT / "supply-chain" / "vendored-sources.json")
    parser.add_argument("--base-version", action="append", default=[], metavar="MANIFEST=VERSION",
                        help="explicit new base version for a proposed, reviewed source update; repeat as needed")
    parser.add_argument("--previous-inventory", type=Path, help="prior reviewed inventory when stamping a newly reviewed source update")
    parser.add_argument("--audit-store", type=Path, help="also require exact local private cargo-vet audit records")
    parser.add_argument("--cargo-manifest", action="append", type=Path, default=[], help="manifest for an actual locked, offline Cargo graph; repeat if needed")
    parser.add_argument("--cargo-cwd", action="append", type=Path, default=[], help="actual build working directory for each --cargo-manifest, in the same order")
    args = parser.parse_args()
    if bool(args.audit_store) != bool(args.cargo_manifest) or len(args.cargo_manifest) != len(args.cargo_cwd):
        parser.error("--audit-store requires one or more matching --cargo-manifest and --cargo-cwd pairs")
    if (args.propose or args.stamp) and args.audit_store:
        parser.error("proposal/stamp and audit verification must be separate commands")
    if args.previous_inventory and not args.stamp:
        parser.error("--previous-inventory is only valid with --stamp")
    if args.base_version and not args.propose:
        parser.error("--base-version is only valid with --propose")
    doc = gate.read_doc(args.inventory)
    if args.base_version:
        specs = {spec["manifest"]: spec for spec in doc["capsules"]}
        changed = set()
        for item in args.base_version:
            if "=" not in item:
                parser.error("--base-version requires MANIFEST=VERSION")
            manifest, version = item.split("=", 1)
            if manifest not in specs or manifest in changed:
                parser.error(f"unknown or duplicate manifest in --base-version: {manifest}")
            gate.marked_version(version, "0" * 64)  # Validate an unmarked semver base.
            specs[manifest]["base_version"] = version
            changed.add(manifest)
    inventory = args.inventory.resolve(strict=True)
    for spec in doc["capsules"]:
        for input_root in spec["input_roots"]:
            p = (ROOT / input_root).resolve(strict=True)
            if inventory == p or p in inventory.parents:
                gate.die("inventory must be outside all hashed input roots")
    if args.propose:
        out = args.propose.resolve()
        if out == inventory:
            gate.die("proposal cannot overwrite the current inventory")
        for spec in doc["capsules"]:
            for input_root in spec["input_roots"]:
                p = (ROOT / input_root).resolve(strict=True)
                if out == p or p in out.parents:
                    gate.die("proposal must be outside all hashed input roots")
        proposed = gate.propose(ROOT, doc)
        with open(out, "x", encoding="utf-8", newline="\n") as stream:
            json.dump(proposed, stream, indent=2, ensure_ascii=False)
            stream.write("\n")
        print(f"Wrote unapproved source inventory proposal: {out}")
        return
    if args.stamp:
        # The tracked Git text must be deliberately normalized before freezing.
        if b"\r\n" in (ROOT / "third_party/xcap/.editorconfig").read_bytes():
            gate.die("xcap/.editorconfig has CRLF; normalize and review a new inventory before stamping")
        previous = gate.read_doc(args.previous_inventory) if args.previous_inventory else None
        gate.stamp(ROOT, doc, previous=previous)
        print("Stamped reviewed source identities; regenerate Cargo lockfiles and run cargo-vet separately.")
        return
    gate.verify(ROOT, doc)
    print(f"Verified source identities for {len(doc['capsules'])} capsules; source identity is not a safety audit.")
    if args.audit_store:
        metadata = []
        for manifest, cwd in zip(args.cargo_manifest, args.cargo_cwd):
            cwd = cwd.resolve(strict=True)
            manifest = (manifest if manifest.is_absolute() else cwd / manifest).resolve(strict=True)
            print(f"Checking Cargo graph: manifest={manifest} cwd={cwd}")
            metadata.append(gate.cargo_metadata(manifest, cwd))
        n = gate.verify_audits(ROOT, doc, args.audit_store, metadata)
        print(f"Verified exact private local audit records for {n} graph identities; other capsules are not audit-verified for these graphs. This does not prove a review occurred.")


if __name__ == "__main__":
    try:
        main()
    except (gate.GateError, OSError, ValueError) as exc:
        print(f"vendored source gate: {exc}", file=sys.stderr)
        sys.exit(1)
