---
name: ionic-react-local-app
description: Implement LingXi Local Apps inside the pinned React 19, Ionic 9, and Vite 8 scaffold with the native WebView bridge and platform adapter constraints intact.
---

# Ionic React Local App

Use this skill when writing or repairing source inside a scaffolded LingXi Local
App.

When `runtime_profile.family=react_dom`, this skill owns the routed shell,
screen structure, Ionic navigation, and bridge-facing UI. When a canvas runtime
is already confirmed, this skill is only for overlays, forms, and bridge-safe
HUD UI around the scene; scene lifecycle stays in the matching runtime
specialist.

Routing is load-mode aware: bundled runtimes must use the `Bundled resource`
section below and must not read from the app workspace (`references/router.md`
or its profiles). File-backed runtimes follow the markdown link
[references/router.md](references/router.md) first, then only the profiles
needed for the current work.

Always preserve these project facts:

- React `19.2.8`
- Ionic React `9.0.0`
- Vite `8.2.1`
- React Router `6.30.6`
- bridge entrypoint: `window.lingxi.v2`

Implement the app through the checked-in Ionic shell, route containers, bridge
helpers, and platform adapter. Treat `deviceContext` as live host input,
surface bridge failures explicitly, and keep platform differences behind the
adapter boundary instead of scattering them through feature JSX.

Do not add or swap dependencies, invent a second host bridge, rely on
Capacitor, Tailwind, React Native, SwiftUI, Jetpack Compose, or any alternate
native-plugin runtime, or require a native reimplementation path.
