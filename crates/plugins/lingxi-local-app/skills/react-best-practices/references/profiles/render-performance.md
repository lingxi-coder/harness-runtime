# Render performance

- Prefer stable domain keys, local state ownership, and smaller component
  surfaces over broad memoization.
- Keep expensive formatting, filtering, and sorting bounded and derived close to
  where they are used.
- Use transitions or deferred values for large lists and search-like UI when
  responsiveness would otherwise degrade.
- Keep React responsible for coarse UI state; high-frequency simulation and draw
  loops stay in the engine layer.
- Surface recoverable errors through UI and error boundaries rather than letting
  the tree crash silently.

## Sources

Reviewed: 2026-08-27

- Managing State: https://react.dev/learn/managing-state
- Preserving and Resetting State: https://react.dev/learn/preserving-and-resetting-state
- Render and Commit: https://react.dev/learn/render-and-commit
