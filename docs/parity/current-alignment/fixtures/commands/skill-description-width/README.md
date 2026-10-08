# Native Skill description width boundary probe

This fixture gives Native 2.1.293 one project Skill whose double-quoted YAML
description contains the literal escape `\uD800` followed by 250 ASCII `a`
characters. A single fixed prompt is sent through a loopback-only Messages
stub. The runner enables the default tool catalog so Native includes the Skill
list, but the stub returns only fixed text and no tool is invoked. It captures
the raw request's exact `system` JSON field and a decoded listing excerpt, then
records whether the test description and 250-character tail were visible. It
uses an isolated HOME/config/cwd, a dummy key, and no external service. The
captured Native request contains the Skill name with its first body line, but
not the frontmatter description or 250-character tail, so this probe does not
produce a Native width value.

Run from the Harness repository root:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude \
python3 docs/parity/current-alignment/fixtures/commands/skill-description-width/run-native.py
```

The full provider request remains under ignored `run-output/`; the checked-in
Native baseline keeps the source SKILL.md, exact skill-bearing request excerpt,
stub SSE, stdout/stderr, receipt, and decoded excerpt observation. This is a
public prompt-listing observation, not a direct call to the hidden width
function.
