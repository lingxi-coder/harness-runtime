# Router

Read this file first.

- App shell, routing, and page-container work ->
  `profiles/app-shell-and-routing.md`
- Bridge, device context, data, or permissions work ->
  `profiles/bridge-and-data.md`

## Scope

This skill assumes the scaffolded Local App template already ships:

- writable source under `app/`, `src/`, `components/`, `lib/`, `styles/`, and
  `public/`
- pinned dependencies for React 19, Ionic 9, Vite 8, zod 4, Zustand 5, and
  Three `0.185.1`
- host-managed bridge and platform-adapter files

This skill applies to routed DOM apps and to canvas apps that use Ionic only
for overlays. Keep the existing scaffold shape; do not create a second shell.

Do not rewrite host-controlled root files or dependency manifests.

## Sources

Reviewed: 2026-08-27

- Ionic React docs: https://ionicframework.com/docs/react
- LingXi Local Apps handoff: `docs/local-apps/HANDOFF.md`
- Local App runtime package: `lingxi-code/plugins/lingxi-local-app/assets/templates/react-dom/r4/package.json`
- LingXi provider: `lingxi-code/plugins/lingxi-local-app/assets/templates/react-dom/r4/lib/lingxi-provider.jsx`
