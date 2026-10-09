# Native Skill description width boundary probe

This fixture gives Native 2.1.293 one project Skill and sends a single fixed
prompt through a loopback-only Messages stub. The default input has a
double-quoted YAML description containing literal `\uD800` followed by 250
ASCII `a` characters. Native's first request includes the Skill name and its
first body line, but not that frontmatter description. The runner captures a
narrow raw request excerpt and decoded listing observation; it keeps the full
request only in ignored output. The run uses an isolated HOME/config/cwd, a
dummy key, and no external service.

Run from the Harness repository root:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude \
python3 docs/parity/current-alignment/fixtures/commands/skill-description-width/run-native.py
```

The default run remains the original lone-surrogate YAML escape probe. To
separate that input-reachability result from listing visibility, the runner
also accepts valid ASCII and Unicode scalar controls:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude \
python3 docs/parity/current-alignment/fixtures/commands/skill-description-width/run-native.py \
  --description-mode ascii --output-name ascii-250

CLAUDE_NATIVE_BINARY=/path/to/claude \
python3 docs/parity/current-alignment/fixtures/commands/skill-description-width/run-native.py \
  --description-mode ascii --ascii-tail-length 1200 --output-name ascii-1200

CLAUDE_NATIVE_BINARY=/path/to/claude \
python3 docs/parity/current-alignment/fixtures/commands/skill-description-width/run-native.py \
  --description-mode unicode --output-name valid-unicode
```

The default tail length is 250. The Unicode control uses a standalone
combining mark, an emoji ZWJ sequence, U+FFFD, and the same ASCII tail. Native
2.1.293 included the complete 250-character ASCII description, the complete
Unicode description, and a 1200-character ASCII description in the prompt
listing, with no ellipsis. The 1200-character Native line is also preserved as
the exact byte slice `../native-baseline/2.1.293/skill-description-width/control-ascii-1200/skill-listing-line.raw.txt`;
its receipt records the original request SHA and line offset/length/hash. The
older `skill-bearing-request.raw-excerpt.bin` remains capped and should not be
used as a complete-line oracle. Each run writes to a separate ignored
`run-output/<name>/` directory. These public prompt observations do not directly
call the hidden width function or establish interactive Skill-menu width
behavior.
