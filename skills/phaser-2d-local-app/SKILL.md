---
name: phaser-2d-local-app
description: Build LingXi Local Apps whose primary scene uses the persisted Phaser 2D runtime profile with explicit scene lifecycle, input parity, and host-safe bridge rules.
---

# Phaser 2D Local App

Use this skill when the confirmed Local App surface is canvas and
`runtime_profile.family=phaser_2d`.

Routing is load-mode aware: bundled runtimes must use the `Bundled resource`
section below and must not read from the app workspace (`references/router.md`
or its profiles). File-backed runtimes follow the markdown link
[references/router.md](references/router.md) first, then only the profiles
needed for the current scene work.

Use only the persisted host-managed Phaser runtime already shipped with the
app. If the host/runtime contract did not already confirm `phaser_2d`, do not
assume it exists and do not request package installs from this skill.

Always:

- use the profile-managed `createPhaserRuntime` from
  `lib/phaser-runtime.js`; it is the single Phaser `Game`, scheduler, resize,
  and teardown owner, so app code supplies scene callbacks and never creates a
  second `Game` or frame loop
- keep one explicit scene lifecycle through that adapter
- define both pointer or touch and keyboard behavior for every core action
- keep bridge, data, and error handling explicit rather than hiding them in
  Phaser globals
- support resize, DPR changes, pause or resume, reduced motion, and procedural
  asset fallback
- keep deterministic scene or simulation seams for QA and logic tests

Do not add npm installs, alternate engines, physics plugins, native wrappers,
or external asset-service dependencies.
