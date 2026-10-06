#!/usr/bin/env python3
"""Product-neutral crates must not name the Local App product (driver: scripts/checks/check-product-neutral.sh).

`core`, `mcp`, `tasks` and `permission` are engine crates: what they need from a product (the layout of its
workspaces, the rows of its tool table, the grammar of its server ids) is injected by the composition root, so a
Local App identifier in one of them is a coupling that the extraction removed and must not come back.

A hit is `LocalApp…`, `local_app…`, `local-app…`, `local app…` or `LOCAL_APP…` in a tracked source, manifest, JSON or
text file under one of these crates. The one way to keep a hit is a line in `product_neutral_keep.txt`:

    <path>\t<needle>\t<reason>

which allows that exact needle (case-insensitive substring of the matched text) in that file. A keep line that no
longer matches anything is itself a failure, so the list cannot rot into a blanket permission.
"""
import os
import re
import subprocess
import sys

CRATES = ("core", "mcp", "tasks", "permission")
EXTENSIONS = (".rs", ".toml", ".json", ".md", ".txt")
# `local_approval`, `local_application` and friends are ordinary words; the app family is `app`/`apps` followed by a
# non-letter (or the end).
PATTERN = re.compile(r"LocalApp|local[-_ ]apps?(?![a-z])", re.IGNORECASE)
HERE = os.path.dirname(os.path.abspath(__file__))


def tracked_files(root):
    out = subprocess.run(["git", "-C", root, "ls-files", "-z"], capture_output=True, check=True).stdout
    return [p for p in out.decode().split("\0") if p]


def load_keep(path):
    keep = []
    if not os.path.exists(path):
        return keep
    with open(path, encoding="utf-8") as f:
        for number, line in enumerate(f, 1):
            line = line.rstrip("\n")
            if not line or line.startswith("#"):
                continue
            parts = line.split("\t")
            if len(parts) != 3 or not all(p.strip() for p in parts):
                raise SystemExit("%s:%d: a keep line is `path<TAB>needle<TAB>reason`, with all three" % (path, number))
            keep.append((parts[0], parts[1].lower(), parts[2]))
    return keep


def scan(root, files, keep):
    used = set()
    hits = []
    for rel in files:
        parts = rel.split("/")
        if len(parts) < 3 or parts[0] != "crates" or parts[1] not in CRATES or not rel.endswith(EXTENSIONS):
            continue
        try:
            with open(os.path.join(root, rel), encoding="utf-8") as f:
                lines = f.read().splitlines()
        except (OSError, UnicodeDecodeError):
            continue
        for number, text in enumerate(lines, 1):
            for match in PATTERN.finditer(text):
                needle = match.group(0).lower()
                allowed = [i for i, (p, n, _) in enumerate(keep) if p == rel and n in text.lower()]
                if allowed:
                    used.update(allowed)
                else:
                    hits.append((rel, number, needle))
    return hits, used


def main(argv):
    root = os.path.realpath(argv[1]) if len(argv) > 1 else os.path.realpath(os.path.join(HERE, "..", ".."))
    keep_path = os.path.join(HERE, "product_neutral_keep.txt") if len(argv) <= 2 else argv[2]
    keep = load_keep(keep_path)
    files = tracked_files(root)
    scanned = [f for f in files if f.split("/")[1:2] and f.split("/")[1] in CRATES and f.endswith(EXTENSIONS)]
    if not scanned:
        sys.stderr.write("[deny] product-neutral: scanned 0 files under %s — the scan is broken, not the repo\n"
                         % ", ".join("crates/" + c for c in CRATES))
        return 1
    hits, used = scan(root, files, keep)
    stale = [k for i, k in enumerate(keep) if i not in used]
    if hits or stale:
        sys.stderr.write("[deny] product-neutral violations (%d):\n" % (len(hits) + len(stale)))
        by_file = {}
        for rel, number, needle in hits:
            by_file.setdefault(rel, []).append((number, needle))
        for rel, items in sorted(by_file.items()):
            shown = ", ".join("%d(%s)" % item for item in items[:6])
            more = "" if len(items) <= 6 else " … +%d" % (len(items) - 6)
            sys.stderr.write("  - %s names the Local App product at %s%s\n" % (rel, shown, more))
        for path, needle, _ in stale:
            sys.stderr.write("  - keep entry for %s (%s) matches nothing — remove it\n" % (path, needle))
        return 1
    print("check-product-neutral: OK — %d files under %s, no Local App identifiers (%d kept)"
          % (len(scanned), ", ".join("crates/" + c for c in CRATES), len(keep)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
