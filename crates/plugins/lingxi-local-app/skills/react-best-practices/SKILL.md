---
name: react-best-practices
description: Apply React 19 client-side rules for LingXi local apps, focusing on component boundaries, state and effect discipline, cleanup, error handling, and render performance.
---

# React best practices

This skill is only for the client-side React layer inside the pinned local-app
runtime.

Use it to keep the app:

- split into clear component, hook, and adapter boundaries;
- explicit about loading, empty, error, success, and permission states;
- disciplined about effects, cleanup, and bridge lifecycles;
- performant without pushing per-frame simulation state into React or Zustand.

Routing is load-mode aware: bundled runtimes must use the `Bundled resource`
section below and must not read from the app workspace (`references/router.md`
or its profiles). File-backed runtimes follow the markdown link
[references/router.md](references/router.md) first. Exclude Next.js, SSR, Server
Components, route loaders owned by other frameworks, and platform design rules
that belong in `$frontend-design`.
