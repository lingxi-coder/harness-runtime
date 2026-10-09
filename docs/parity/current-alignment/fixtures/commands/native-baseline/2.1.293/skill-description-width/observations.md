# Native Skill description visibility boundary

Native 2.1.293's first loopback model request included the Skill listing in
`$.messages[0].content[3].text`. For the fixed lone-surrogate YAML input, the
listing line was:

```text
- native-width-probe: Return the fixed skill-width response.
```

That is the first body line. The exact source in `SKILL.md.raw` has a
double-quoted frontmatter description with literal `\uD800` plus 250 ASCII
`a` characters. Neither the description nor its tail appears in the request.
This observation does not establish why Native used the body line; no parse
error was surfaced in stdout or stderr. The raw request remains in ignored run
output; only the narrow wire excerpt, observation, stub SSE, stdout/stderr,
source and hash-bearing receipt are retained here.

Three paired valid-scalar controls are in `control-ascii-250/`,
`control-unicode-250/`, and `control-ascii-1200/`. Native included the complete
250-`a` description, the complete standalone-combining-mark + emoji-ZWJ +
U+FFFD description, and the complete 1200-`a` description; none had an
ellipsis. The 1200-`a` line is preserved as an exact no-newline byte slice in
`control-ascii-1200/skill-listing-line.raw.txt`; its receipt records the
original full-request hash, byte offset/length, and line hash. The earlier
`skill-bearing-request.raw-excerpt.bin` remains capped and is not the full-line
oracle. The observations show the request location and decoded code points.
These results show no truncation in this headless prompt
listing at those input lengths. They do not call the hidden width function and
do not establish interactive Skill-menu width behavior. Native width remains
Unverified. The separate plugin marker fixture establishes only Native
`Intl.Segmenter` behavior; host Bun output is not substituted.
