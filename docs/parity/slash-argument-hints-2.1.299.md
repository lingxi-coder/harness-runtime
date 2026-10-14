# Slash argument hints: Claude Code 2.1.299

The installed native artifact is the oracle for this feature. `claude --version`
reported `2.1.299 (Claude Code)` on 2026-10-14. REA `open_binary` identified
`~/.local/share/claude/versions/2.1.299` as an arm64 Mach-O executable with SHA-256
`ba5ab7b8c195fa03ab8d5bf3915c76153b576a2c40e79a5c46e265e7ed9744b3`.
The REA session was closed after artifact identification. The behavior below is
established by recovered embedded JavaScript and controlled execution of its
pure functions, not by a recorded interactive terminal session.

## Reproduce

```sh
node scripts/tests/slash_argument_hint_299_oracle.mjs --check
```

The script rejects a different binary hash. It extracts and executes native
functions, regenerates the JSON fixture, and checks the standalone parser bytes.
It does not start the interactive application, read credentials, access the
network, or call a model. Run without `--check` to regenerate the two artifacts.

- `slash-argument-hints-2.1.299.json`: 51 selector cases, 83 shell argument cases,
  10 metadata cases, 7 render gate cases, and both dynamic code-review hint texts.
- `slash-argument-parser-2.1.299.js`: native shell parser plus native AST argument
  traversal and fallback, with unchanged function bodies inside an isolated
  wrapper. It exports `slashCommandParseArguments` and
  `slashCommandArgumentCount` on `globalThis`. The only host adaptation is a
  scoped `performance.now` fallback to `Date.now` when the host lacks Performance.

The JSON records half-open byte ranges and SHA-256 for the original modules and
the focused function slices. The standalone generated parser has SHA-256
`f3a36c719f973c818c61068cb1f1c4db61de84647ad49395f8da532b370256ed`.

## Observed selector contract

The command hint is a separate display value, never a draft completion or text
to submit. Its source is the resolved command's `argumentHint`, or remaining
declared argument names for a prompt command.

1. The composer must be in prompt mode. The draft starts with `/`, and the
   command token before the first ASCII space contains only ASCII letters,
   digits, `.`, `:`, `_`, and `-`. Leading whitespace and a tab in place of the
   command separator do not enter this branch.
2. The cursor must be greater than zero. The branch does **not** require the
   cursor to be at the end. For example `/review ` at cursor 3 still produces
   its argument hint; cursor 0 does not.
3. `/review` without its following ASCII space has no argument hint. For
   `/review `, a truthy `argumentHint` is returned verbatim. The exact initial
   boundary is `draft.length === draft.indexOf(" ") + 1`.
4. An additional space or argument removes the static hint. For a `prompt`
   command with `argNames`, a draft ending in ASCII space instead displays the
   unconsumed names as `[name]` joined with ASCII spaces. It works even without
   an `argumentHint`. A local command does not use the name fallback.
5. Resolution is case-sensitive. Any exact primary name takes precedence over
   an earlier alias match. Otherwise the first `userFacingName()` or alias
   match wins. The lookup itself does not filter `isHidden`.
6. Named arguments are parsed with the native shell parser; their count selects
   `argNames.slice(count)`. Exhausting the names yields no hint.

Native names in the artifact: `dR`/`lse` validate slash tokens, `aBe` gates argument
entry, `Ns`/`OC`/`Ro` resolve names, `fJr` parses the arguments, `Kae` parses the
`arguments` metadata, and `Jes` constructs the remaining names. The selector
slice is byte range `[225732150, 225732667)` with SHA-256
`51265fa66bba18e4f1185e40ad9d9673969c03df5878418d2c29c4f7161604cd`.

Selecting a normal command completion through `HK` writes `/selected-name `
with a trailing ASCII space and moves the caret to its end. Tab uses
`shouldExecute: false`; this trailing space applies to every command. Candidate
construction chooses the first alias whose lowercase form starts with the
lowercase query, even if the canonical name also matches. Acceptance retains
that alias only when resolving it still selects the same command; an exact
primary name belonging to another command defeats the alias and falls back to
the selected command's canonical name. Enter also writes the trailing space
first, then submits for a non-prompt command or a prompt command with no declared
argument names. These completion facts are static source observations; they are
not included in the controlled selector fixture count.

## Shell parser details

A whitespace or quote-only tokenizer is insufficient. The native parser reads
the argument remainder as a shell program, finds its first command AST node,
then collects its words. It skips assignment nodes, stops before later command
or process substitutions, and ignores later commands in a pipeline, list, or
newline-separated program. Empty quotes count as one argument. Incomplete
quotes are accepted as a token. If the parser produces no arguments, `fJr`
falls back to JavaScript whitespace splitting. An entirely whitespace string
returns an empty array before parsing. Inputs longer than 10,000 UTF-16 code
units take that fallback too.

Examples from the executable fixture:

| Argument remainder | Native parsed arguments |
| --- | --- |
| `"a b" ` | `["a b"]` |
| `"" ` | `[""]` |
| `a\ b ` | `["a b"]` |
| `A=b foo ` | `["foo"]` |
| `a; b ` | `["a"]` |
| `a \| b ` | `["a"]` |
| `a $(echo hi) b ` | `["a"]` |
| `a >out b ` | `["a"]` |
| `# comment ` | `["#", "comment"]` (fallback) |

The native shell module is `[198366413, 198428312)`. `que` selects the AST node
and `w5e` collects its arguments in `[198430294, 198431649)`; `iv` enforces the
input bound in `[201451545, 201451670)`. This parser performs no shell execution.

## Rendering and screenshot text

`jBt` renders the hint after the rendered draft with `dimColor: true` and
`wrap: "truncate-end"`. It only adds a separator when the original draft does
not already end in ASCII space. The input renderer itself reserves a blank
cursor cell when the caret is at the end; this accounts for the blue blank cell
before the hint in the supplied screenshot. With the caret moved into the
draft, the hint remains after the draft and the extra cursor cell is absent.

The terminal text layout dispatches `truncate-end` through `Nu` to `d1`/`XBt`.
If content exceeds its allocated display width, it keeps up to width minus one
display cells and appends U+2026 (`…`). Width one produces only `…`; width less
than one produces empty text. `Bun.sliceAnsi` and display-width measurement avoid
splitting wide terminal characters. This is static source evidence at the
native renderer near byte offset 208446636; terminal layout was not replayed.

`/code-review` in the screenshot is a bundled command, not a user skill. Its
dynamic native hint is:

```text
[low|medium|high|xhigh|max|ultra] [--fix] [--comment] [<pr#>|<branch>|<path>] [--max-findings <n>|all]
```

When the native ultra feature predicate is false, the `|ultra` member is omitted.
The predicate is `VSe()?.enabled === true && Yn() && !kD()`. These hint strings
describe Claude Code's command capabilities; they are not evidence that another
runtime implements ultra or `--max-findings`.

## Coverage boundary

The controlled selector replay extracts the hint-producing branch. Other native
completion branches can take precedence: suggestion suppression, an inline
slash menu, emoji or mention completion, directory/resume completion, and a
command's `getArgumentCompletions`. A custom argument completion provider clears
the hint even when it returns an empty list. The fixture does not execute those
asynchronous providers or the React/Ink component lifecycle.

The source parser is replayed, including its 10,000-unit input bound and internal
node/time budgets. The fixtures cover ordinary and adversarial shell syntax;
they are not an exhaustive grammar proof. No claim is made here of full native
terminal pixel parity, all command availability parity, or all client runtime
acceptance. Client checks must consume these fixtures and separately validate
their own rendering, selection, and submit paths.

## Client integration

`client::protocol::listings::SlashCommandDto.argument_names` carries the declared names through the
desktop bridge and mobile catalogs. Catalog lowering publishes names only for
prompt command kinds. Static `argument_hint` strings remain unmodified.

Desktop runs the extracted JavaScript parser directly. CLI, Android, and iOS
call `client::slash_command_argument_count`, which runs the same checked-in
parser through the existing QuickJS version used by the runtime. Its context
and last argument count are cached per thread. The host adds a 32 MiB heap
limit and a one-second interruption guard; engine failure suppresses only the
decorative hint. The native parser's own input and traversal limits remain in
place. The `slash-hints` feature is enabled by client presentation and UniFFI.

The renderers keep the suffix outside submitted text: Desktop uses an overlay
at the actual DOM range end, Android uses a measured and truncated visual
transformation, iOS uses the actual UITextView end caret and a sibling label,
and CLI reserves the terminal cursor cell before a dim, truncated suffix.

Mobile validation regenerated both binding languages from current UniFFI
metadata, including the original event-listener callback interface. Regenerated
artifacts were staged for compilation and real-FFI tests, then the original
ignored bindings were restored to remain paired with the existing native
libraries. Running the changed mobile app requires rebuilding native libraries
and bindings together against this Harness change. The product's existing
immutable dependency pin is not changed by an uncommitted source edit.
