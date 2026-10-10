#!/usr/bin/env python3
"""P1.12 regression gate: every checked-in gate script in scripts/ must have
a real execution trigger — not a comment mentioning its name, not a
hardcoded list that agrees with itself.

WHY THIS EXISTS: a five-lens adversarial review of this branch found four
gate scripts (check-brand-leaks, check-skill-frontmatter, check-i18n-pairing)
checked in with ZERO automation trigger — nothing but a human (or
an agent) remembering to run them. CI now owns that trigger without making
every local commit wait for the full repository gate suite.
A unit test on the gate ENGINES cannot catch this — every one of them
passes today with zero production callers, which is the whole finding.

WHAT WOULD MAKE THIS TEST LIE TO YOU, AND WHY IT DOESN'T:

  * A hardcoded list of "the four gates" agrees with itself forever and
    goes blind the day a fifth gate is added by someone who has never
    heard of this finding — the exact defect shape this branch keeps
    finding elsewhere (a validator reached only by its own tests). So
    `discover_gates()` below walks scripts/ from the filesystem with the
    SAME naming convention scripts/check-all.sh uses (`check-*.sh` /
    `*-gate.sh`), reimplemented independently here rather than imported
    from check-all.sh, so a change to one cannot silently drag the other
    along uncaught.

  * A trigger that only MENTIONS a gate's filename — in a comment, in
    ci.yml, in this docstring — is indistinguishable from a real one
    under plain `grep`. This test never greps for a filename. It executes
    scripts/check-all.sh as a real subprocess, requires each discovered
    gate to appear in a "=== RUNNING: X ===" / "=== RESULT: X exit=N ==="
    marker pair in its REAL stdout, and then re-runs that same gate
    standalone, independent of check-all.sh, and requires the exit code
    to match. A stub that prints the markers without invoking anything
    would report an unconditional exit=0 and gets caught the moment the
    independent standalone run disagrees.

Run directly: `python3 scripts/tests/test_gate_triggers.py` from anywhere (paths
are resolved from this file's own location, not the caller's cwd).
"""
from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

SCRIPTS_DIR = Path(__file__).resolve().parents[1] / "checks"
REPO_ROOT = SCRIPTS_DIR.parents[1]
GATE_NAME_RE = re.compile(r"^(check-.*\.sh|.*-gate\.sh)$")
SELF = "check-all.sh"

# A gate that needs a non-empty argv to do anything meaningful unattended goes here.
# Any such script added later and missing from this table is reported as a
# mismatch (see `extra`/`missing` below), never silently swallowed.
ARGV_OVERRIDES: dict = {}

RUNNING_RE = re.compile(r"^=== RUNNING: (\S+) ===$", re.MULTILINE)
RESULT_RE = re.compile(r"^=== RESULT: (\S+) exit=(-?\d+) ===$", re.MULTILINE)
# Captures the gate's OWN output between its RUNNING and matching RESULT
# marker (backreference \1 ties the two ends to the SAME gate name). This
# is what makes the difference between "invoked for real" and "mentioned":
# a stub that prints the markers around `true` produces an EMPTY block
# (matched here as group(2) == "", NOT as a failure to match at all — the
# content group must be allowed to be zero-length so a no-op stub is
# classified as a real, empty, comparable block rather than silently
# falling through as "no marker pair found").
BLOCK_RE = re.compile(
    r"=== RUNNING: (\S+) ===\n(.*?)(?:^=== RESULT: \1 exit=(-?\d+) ===$)",
    re.MULTILINE | re.DOTALL,
)


def discover_gates() -> set:
    """Enumerate gate scripts straight from the filesystem — never a
    hardcoded list. Only scripts/checks/ entries, excluding fixture and build-helper directories; must be
    a regular file matching the naming convention every gate wrapper in this
    repo already follows. Executability is checked separately and fails the
    test instead of making a broken gate disappear from discovery."""
    gates = set()
    for entry in SCRIPTS_DIR.iterdir():
        if not entry.is_file():
            continue
        if entry.name == SELF:
            continue
        if not GATE_NAME_RE.match(entry.name):
            continue
        gates.add(entry.name)
    return gates


def run_standalone(gate: str) -> tuple:
    """Run one gate exactly as check-all.sh would — `./scripts/<gate>` from
    crates/ — in complete isolation from check-all.sh, and return its
    real (exit_code, combined_output). This is the independent reference
    each gate's recorded behaviour inside check-all.sh is checked against,
    both by exit code AND by the actual text it printed."""
    argv = ["./scripts/checks/" + gate] + ARGV_OVERRIDES.get(gate, [])
    proc = subprocess.run(argv, cwd=str(REPO_ROOT), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    return proc.returncode, proc.stdout


def run_check_all() -> tuple:
    proc = subprocess.run(
        ["./scripts/check-all.sh"], cwd=str(REPO_ROOT), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
    )
    return proc.returncode, proc.stdout


def parse_invocations(output: str) -> dict:
    """Pull {gate_name: (exit_code, own_output_block)} out of check-all.sh's
    REAL stdout by its RUNNING/RESULT marker pairs — never by grepping for
    a filename alone, which a comment would also satisfy. `own_output_block`
    is the text the gate itself printed between its markers; comparing it
    against an independent standalone run is what catches a stub that
    prints the markers around a no-op (see run_standalone / the exit-code-
    only version of this check that a `true`-stub with a currently-green
    gate slips past silently)."""
    running = set(RUNNING_RE.findall(output))
    blocks = {m.group(1): (int(m.group(3)), m.group(2)) for m in BLOCK_RE.finditer(output)}
    # A RUNNING marker with no matching RESULT marker means check-all.sh
    # started a gate and never reported what happened to it — don't let
    # that default itself into looking invoked.
    return {name: rc_block for name, rc_block in blocks.items() if name in running}



def comparable_gate_output(output: str) -> str:
    """Ignore unittest wall time while retaining its count, result, and diagnostics."""
    return re.sub(
        r"(?m)^(Ran \d+ tests? in )\d+(?:\.\d+)?s$",
        r"\1<elapsed>s",
        output.strip(),
    )


def output_comparison_preserves_gate_evidence() -> list[str]:
    reference = "Ran 9 tests in 0.001s\n\nOK\nlocked source verified"
    equivalent = "Ran 9 tests in 0.125s\n\nOK\nlocked source verified"
    failures = []
    if comparable_gate_output(reference) != comparable_gate_output(equivalent):
        failures.append("unit-test elapsed time must not change gate evidence")
    for altered in (
        "",
        reference.replace("9 tests", "8 tests"),
        reference.replace("OK", "FAILED (failures=1)"),
        reference.replace("locked source verified", "different source"),
    ):
        if comparable_gate_output(reference) == comparable_gate_output(altered):
            failures.append("gate output comparison discarded counts, failures, or source evidence")
    return failures


def every_checked_in_gate_has_an_execution_trigger() -> list:
    """Returns a list of failure messages; empty means the invariant holds."""
    failures = []

    discovered = discover_gates()
    if not discovered:
        return [
            "discover_gates() found ZERO gate scripts under scripts/ — the enumeration "
            "itself is broken; this must never be read as \"nothing to check\""
        ]

    non_executable = sorted(
        gate for gate in discovered if not (SCRIPTS_DIR.joinpath(gate).stat().st_mode & 0o111)
    )
    if non_executable:
        failures.append(
            "matching gate script(s) are not executable: " + ", ".join(non_executable)
        )

    executable = discovered - set(non_executable)
    check_all_rc, check_all_out = run_check_all()
    invoked = parse_invocations(check_all_out)

    missing = executable - invoked.keys()
    if missing:
        failures.append(
            "gate(s) present in scripts/ but never invoked by check-all.sh's real run: "
            + ", ".join(sorted(missing))
            + " -- this IS the P1.12 regression: a gate script checked in with no execution trigger"
        )

    extra = invoked.keys() - executable
    if extra:
        failures.append(
            "check-all.sh invoked name(s) discover_gates() does not recognise as a gate "
            "(naming-convention drift between the two): " + ", ".join(sorted(extra))
        )

    # The "not just a comment" check: re-run every discovered gate
    # standalone, independent of check-all.sh, and require BOTH the exit
    # code AND the actual printed output to match what check-all.sh
    # captured. Exit code alone is not enough: a stub that swaps a gate's
    # real invocation for a no-op (`true`) reports exit=0, which matches
    # the reference run whenever that gate is currently green in the repo
    # -- a vacuously-true comparison in the only state this repo is
    # usually in. Comparing the OUTPUT TEXT closes that hole: a no-op
    # prints nothing, where every real gate engine prints a distinctive
    # "OK: ..." / "FAIL: ..." line naming what it actually checked.
    standalone_results = {}
    for gate in sorted(executable & invoked.keys()):
        invoked_rc, invoked_text = invoked[gate]
        reference_rc, reference_text = run_standalone(gate)
        standalone_results[gate] = reference_rc
        if invoked_rc != reference_rc:
            failures.append(
                f"{gate}: check-all.sh reported exit={invoked_rc} but running the same "
                f"script standalone (independent of check-all.sh) gives exit={reference_rc} "
                "-- check-all.sh is not actually invoking this gate's real engine"
            )
        elif comparable_gate_output(invoked_text) != comparable_gate_output(reference_text):
            failures.append(
                f"{gate}: exit codes agree but the output check-all.sh captured for this gate "
                f"({invoked_text.strip()[:120]!r}) does not match an independent standalone run "
                f"({reference_text.strip()[:120]!r}) -- the trigger names this gate but is not "
                "running its real engine (e.g. a no-op stub that happens to share today's exit "
                "code)"
            )

    expected_success = not non_executable and all(
        rc == 0 for rc in standalone_results.values()
    ) and executable <= standalone_results.keys()
    if (check_all_rc == 0) != expected_success:
        failures.append(
            f"check-all.sh aggregate exit={check_all_rc} does not propagate the standalone "
            f"gate outcomes {standalone_results}"
        )

    # This regression test must itself be on the checked-in CI trigger path;
    # otherwise it can pass forever while nobody runs it. Local commits stay
    # hook-free so the complete repository gate suite is paid once in CI.
    ci = REPO_ROOT.joinpath(".github/workflows/ci.yml").read_text()
    executable_lines = [
        line.strip()
        for line in ci.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if not any("scripts/check-all.sh" in line for line in executable_lines):
        failures.append("ci.yml does not execute scripts/check-all.sh")
    if not any("scripts/tests/test_gate_triggers.py" in line for line in executable_lines):
        failures.append("ci.yml does not execute scripts/tests/test_gate_triggers.py")

    return failures


def main() -> int:
    failures = output_comparison_preserves_gate_evidence()
    failures.extend(every_checked_in_gate_has_an_execution_trigger())
    if failures:
        print("FAIL: every_checked_in_gate_has_an_execution_trigger")
        for f in failures:
            print("  - " + f)
        return 1
    discovered = sorted(discover_gates())
    print(
        "OK: every_checked_in_gate_has_an_execution_trigger -- %d gate(s) discovered under "
        "scripts/ (%s), each has a real execution trigger (ci.yml -> check-all.sh -> the "
        "gate)" % (len(discovered), ", ".join(discovered))
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
