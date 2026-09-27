#!/usr/bin/env python3
"""No tracked symlink may point at an absolute path.

WHY THIS EXISTS. Twice in one session a worktree symlink — created so the
worktree could reuse the primary checkout's 20MB-per-ABI build inputs instead of
rebuilding them — ended up COMMITTED, because:

  1. the ignore rules that were supposed to cover those paths were written with a
     trailing slash (`Generated/`, `app/src/main/jniLibs/`), which matches a
     DIRECTORY ONLY, so a symlink at the same path slipped past and surfaced as
     untracked; and
  2. a broad `git add clients/` then swept it into the index.

The blob that lands is mode 120000 whose content is an absolute path on one
machine. For `clients/android/app/libs` the target was its own checked-out path,
so checking it out replaced the real build inputs with a self-referential link
and every tree walk afterwards died with ELOOP — `check-brand-leaks.sh` stopped
reporting and started crashing, which masked 21 real findings until the loop was
removed.

Discipline did not prevent the second occurrence: the ignore rules were already
fixed for Android, the lesson was already written down, and it still happened on
the iOS path an hour later. So this is a gate rather than a rule.

An absolute symlink is never correct in this repository: it cannot survive a
clone, a fresh worktree, or another machine. A RELATIVE symlink is fine and is
left alone.
"""

import subprocess
import sys


def main() -> int:
    # From the REPO ROOT, not the caller's cwd. The wrapper runs this from
    # `crates/`, and `git ls-files` is scoped to the current directory: the
    # first version of this gate therefore never looked at `clients/`, which is
    # where both real incidents happened, and printed "OK: 0 tracked symlinks"
    # while the offending symlink sat in the index. A comparator whose coverage
    # was silently truncated reports all-clear in exactly the same words as a
    # real pass, so the count below is printed to make the coverage checkable.
    root = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True
    ).stdout.strip()
    entries = subprocess.run(
        ["git", "-C", root, "ls-files", "-s"], capture_output=True, text=True, check=True
    ).stdout.splitlines()
    if not entries:
        print("ABSOLUTE-SYMLINK FAIL: git ls-files returned nothing — the gate is blind", file=sys.stderr)
        return 1

    symlinks = []
    for line in entries:
        meta, _, path = line.partition("\t")
        fields = meta.split()
        if len(fields) >= 2 and fields[0] == "120000":
            symlinks.append((fields[1], path))

    offenders = []
    for blob, path in symlinks:
        target = subprocess.run(
            ["git", "-C", root, "cat-file", "-p", blob], capture_output=True, text=True, check=True
        ).stdout
        if target.startswith("/"):
            offenders.append((path, target, target.rstrip("/").endswith(path)))

    if offenders:
        print("ABSOLUTE-SYMLINK FAIL: tracked symlinks point at absolute paths:", file=sys.stderr)
        for path, target, self_referential in offenders:
            note = "  <-- SELF-REFERENTIAL: checking this out destroys the real path" if self_referential else ""
            print(f"  {path} -> {target}{note}", file=sys.stderr)
        print(
            "\nThese are almost always a worktree build-input symlink swept in by a broad\n"
            "`git add <dir>`. Fix BOTH halves, or it recurs:\n"
            "  1. git rm --cached <path>          (an ignore rule never untracks)\n"
            "  2. make the ignore rule match a SYMLINK — drop the trailing slash, since\n"
            "     `Foo/` matches a directory only and misses a symlink named Foo.\n"
            "Then add the path per-name, never `git add <dir>`.",
            file=sys.stderr,
        )
        return 1

    print(f"OK: {len(entries)} tracked path(s) scanned, {len(symlinks)} symlink(s), none absolute")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
