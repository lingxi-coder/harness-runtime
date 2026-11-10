Content search built on ripgrep. Prefer this over `grep`/`rg` via a registered shell tool, because its results integrate with the permission UI and file links in LingXi.

- Full regex syntax (e.g. "log.*Error", "function\s+\w+"). Ripgrep, not grep, so escape literal braces (`interface\{\}`).
- Filter with `glob` (e.g. "**/*.tsx") or `type` (e.g. "js", "py", "rust").
- `output_mode`: "content" (matching lines), "files_with_matches" (paths only, default), or "count".
- `multiline: true` for patterns that span lines.