#!/usr/bin/env bash
# Single entry point for every checked-in gate script — the execution
# trigger for P1.12 (see docs/local-apps/... review history: three of these
# gates shipped with 0 automation triggers and only human memory ran them).
#
# DISCOVERY, NOT A HARDCODED LIST. Gates are found by walking scripts/
# (this directory only — not lap_gate_fixtures/, not mobile-linux/) for
# executable files matching the naming convention every gate wrapper in
# this repo already follows: `check-*.sh` or `*-gate.sh`. That convention
# is the whole point: a fifth gate written tomorrow by someone who has
# never heard of this finding gets picked up automatically, because it is
# discovered from the filesystem, not looked up in a list this file would
# need editing to extend. scripts/test_gate_triggers.py enumerates the same
# directory independently and cross-checks it against what actually ran.
#
# Each gate is invoked for real — `exec`'d as a child process, its stdout
# captured and echoed — never just named in a comment. A trigger that only
# MENTIONS a filename (e.g. a stray comment, a doc reference) is exactly
# the failure mode this script exists to not be; grep for evidence of that
# distinction in test_gate_triggers.py.
set -euo pipefail

# All five gate wrappers resolve their own engine path via `cd
# "$(dirname "$0")/.."` + a path relative to the NEW cwd. That only
# resolves correctly when invoked as `./scripts/<name>.sh` from the
# crates/ root (the same convention .github/workflows/ci.yml already
# uses for check-deps.sh) — not from inside scripts/ itself, and not via
# an absolute path. So: cd to crates/ once, then always invoke
# `./scripts/<name>` — never `./"$name"` from inside this directory.
cd "$(dirname "$0")/.." || exit 1

shopt -s nullglob
gates=()
non_executable=()
seen=$'\n'
for f in scripts/check-*.sh scripts/*-gate.sh; do
    base="$(basename "$f")"
    [[ "$base" == "check-all.sh" ]] && continue
    [[ -f "$f" ]] || continue
    case "$seen" in
        *$'\n'"$base"$'\n'*) continue ;;
    esac
    seen+="$base"$'\n'
    if [[ ! -x "$f" ]]; then
        non_executable+=("$base")
        continue
    fi
    gates+=("$base")
done

if [[ ${#gates[@]} -eq 0 ]]; then
    echo "check-all: discovered 0 gate scripts under scripts/ — discovery is broken, not the repo" >&2
    exit 1
fi

status=0
if [[ ${#non_executable[@]} -ne 0 ]]; then
    printf 'check-all: matching gate is not executable: %s\n' "${non_executable[@]}" >&2
    status=1
fi

# Stable, deterministic order regardless of glob/filesystem ordering.
sorted_gates=()
while IFS= read -r gate; do
    sorted_gates+=("$gate")
done < <(printf '%s\n' "${gates[@]}" | sort)
gates=("${sorted_gates[@]}")

ran=()
for g in "${gates[@]}"; do
    echo "=== RUNNING: $g ==="
    case "$g" in
        lap-gate.sh)
            # lap-gate is a criterion *library*: every subcommand wants a
            # specific --run/--baseline/--range argument for a specific
            # task under review. There is no "check this repo" mode. The
            # closest thing an unattended trigger can run is the engine's
            # own self-test: it exercises all ten planted criteria in both
            # directions and fails if the judging logic itself regresses.
            if ./scripts/lap-gate.sh selftest 2>&1; then rc=0; else rc=$?; fi
            ;;
        *)
            if ./scripts/"$g" 2>&1; then rc=0; else rc=$?; fi
            ;;
    esac
    echo "=== RESULT: $g exit=$rc ==="
    ran+=("$g")
    if [[ $rc -ne 0 ]]; then
        status=$rc
    fi
done

echo "check-all: ran ${#ran[@]} gate(s): ${ran[*]}"
exit "$status"
