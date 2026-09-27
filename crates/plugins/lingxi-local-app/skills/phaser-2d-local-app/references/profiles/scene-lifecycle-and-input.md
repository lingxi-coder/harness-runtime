# Scene lifecycle and input

- Keep Phaser setup explicit through the profile-managed
  `createPhaserRuntime` adapter: one game owner, one scene registration path,
  and deterministic create/frame responsibilities. Do not hand-roll or replace
  the adapter's scheduler, resize, or teardown path.
- Define every core action in both pointer or touch and keyboard terms so the
  host QA path can drive them without gesture-only assumptions.
- Keep bridge reads and writes outside ad hoc global mutation; surface native
  failures in the overlay UI or scene state.
- Handle resize, DPR, visibility pause, and resume without duplicating scenes
  or replaying an unbounded catch-up frame.
- Prefer straightforward collision and state transitions unless the brief
  clearly requires more.

## Sources

Reviewed: 2026-08-27

- Phaser concepts: https://docs.phaser.io/phaser/concepts
- Phaser scale manager: https://docs.phaser.io/api-documentation/class/scale-scalemanager
