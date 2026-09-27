# Router

Read this file first, then load both profiles:

- `profiles/client-state-and-effects.md`
- `profiles/render-performance.md`

## Scope

- Runtime: React 19 client components inside LingXi local apps
- In scope: component boundaries, state ownership, effects, cleanup, bridge
  integration, error states, async lifecycles, render performance
- Out of scope: Next.js, SSR, RSC, backend fetching architecture, platform
  visual design

## Sources

Reviewed: 2026-08-27

- React docs: https://react.dev/learn
- `useEffect`: https://react.dev/reference/react/useEffect
- `useDeferredValue`: https://react.dev/reference/react/useDeferredValue
