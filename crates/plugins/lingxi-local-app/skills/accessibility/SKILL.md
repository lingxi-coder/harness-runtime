---
name: accessibility
description: Enforce local-app accessibility for DOM and canvas surfaces across iOS VoiceOver, Android TalkBack, and desktop keyboard and ARIA behavior.
---

# Accessibility

Treat accessibility as part of the product contract, not a late polish pass.

Return or repair against an explicit `a11y_contract` that covers:

- semantic structure or canvas equivalents;
- focus order, visibility, and non-pointer reachability;
- screen-reader names, status updates, and error recovery;
- target sizes, contrast, zoom/large-type tolerance, and reduced motion;
- platform-specific behavior for iOS, Android, and desktop.

Routing is load-mode aware: bundled runtimes must use the `Bundled resource`
section below and must not read from the app workspace (`references/router.md`
or its profiles). File-backed runtimes follow the markdown link
[references/router.md](references/router.md) first, then only the surface and
platform profiles that apply. Do not file DOM-only findings against a canvas
surface; canvas has a different contract and must be evaluated as such.
