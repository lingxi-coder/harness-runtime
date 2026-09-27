# Client state and effects

- Keep components pure during render; derive values instead of mirroring props
  into state.
- Put user actions in event handlers, not in effects.
- Use effects only to synchronize with external systems such as the bridge,
  timers, subscriptions, media, or imperative browser APIs.
- Every async effect must have cancellation or stale-result protection and must
  render pending, success, and failure states explicitly.
- Centralize platform-specific shell state at the adapter boundary rather than
  scattering platform conditionals through feature JSX.
- Use `useEffectEvent`, `startTransition`, or `useDeferredValue` only when they
  solve a real interaction or scheduling problem; do not add them mechanically.

Exclude:

- per-frame simulation, particle, physics, or camera state in React or Zustand;
- using effects to repair state that could be derived in render;
- optimistic success UI that hides bridge or network failure.

## Sources

Reviewed: 2026-08-27

- Keeping Components Pure: https://react.dev/learn/keeping-components-pure
- You Might Not Need an Effect: https://react.dev/learn/you-might-not-need-an-effect
- Lifecycle of Reactive Effects: https://react.dev/learn/lifecycle-of-reactive-effects
- Synchronizing with Effects: https://react.dev/learn/synchronizing-with-effects
