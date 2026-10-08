# Native Skill description visibility boundary

Native 2.1.293 saw the single local Skill by name, but the model request's
`<system-reminder>` entry was:

```text
- native-width-probe: Return the fixed skill-width response.
```

That is the first body line. The exact SKILL.md source in `SKILL.md.raw` has a
double-quoted frontmatter description with literal `\uD800` plus 250 ASCII
`a` characters. Neither that literal description nor the 250-character tail
appears in any string in the captured provider request. The raw request remains
in ignored run output; the checked-in wire excerpt, observation, stub SSE,
stdout/stderr and hash-bearing receipt are retained here.

This public Skill-listing input therefore does not reach a width measurement.
The capture does not distinguish why Native selected the body line, and does
not establish a width value. Width remains Unverified; do not substitute the
local Bun runtime's result. The separate plugin marker fixture establishes only
Native `Intl.Segmenter` behavior.
