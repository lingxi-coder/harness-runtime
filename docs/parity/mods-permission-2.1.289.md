# Claude Code 2.1.289 Mods permission oracle

The oracle pins 2.1.288 and 2.1.289 native executables and runs the relevant
plain-JavaScript ranges extracted from each binary. The 2.1.289 executable was
downloaded from the official [v2.1.289 release](https://github.com/anthropics/claude-code/releases/tag/v2.1.289).
The release asset manifest `SHASUMS256.txt` had SHA-256
`9f1136be59ac6ca562d8e3075c4adfc887ae4d72dc260e829e992c5a61385cc3`; its
`claude-darwin-arm64.tar.gz` entry and the official asset digest both equal
`20acfc89a32ed7260b63fd2f3fd71a33de7b2d74b82c724d25941ad73d2e0e96`. The
extracted Mach-O arm64 executable reports `2.1.289 (Claude Code)` and has SHA-256
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`. The
2.1.288 baseline is pinned to
`bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750`.

Run the source-pinned oracle with both reference binaries available:

```sh
node scripts/tests/mods_permission_289_oracle.mjs \
  /path/to/claude-code-2.1.288-darwin-arm64 \
  /path/to/claude-code-2.1.289-darwin-arm64 --check
```

It fails closed if either binary is missing or its SHA changes, if a native
source range moves or changes, or if the checked-in fixture differs. The checked
in fixture is
[`mods_nested_rule_2_1_289.json`](../../crates/permission/tests/fixtures/mods_nested_rule_2_1_289.json).
No extracted native source or binary is checked into the repository.

The evidence has three separate boundaries. First, the native compound Bash
producer `m6o` records each subcommand result under
`decisionReason.type = "subcommandResults"` and a `Map` of child results. The
oracle executes that producer with an injected child ask and deny. Second,
2.1.289 `Ast`/`wEe` recursively follows matching-behavior child results and
returns the first rule; 2.1.288 `Yrt`/`Vrt` checks only a direct matching child.
The actual 2.1.289 `Pst`/`Pn` projection turns the resulting nested rule into a
core `tool.check` row, for example
`{ decision: "deny", reason: "Native child deny", rule: "Bash(rm:*)" }`.
The fixture also covers direct rules, one, two, and three nested result levels,
behavior mismatches, first-match order, root-rule priority, matched-ask fallback,
and allow/deny/no-rule inputs.

Third, the Native consumers are distinct. The actual main `$Ln` caller uses
`Uoe` to force a core permission evaluation and retain its result only for
`WebFetch` or `WebSearch` when the Projects/session gate is active (including
its explicit-state and payload exceptions). It is not a generic Bash ask guard.
The fixture runs the exact `$Ln`, `ELn`, `Dst`, and `Pst` functions with the
aggregate Mod decision and permission callback injected: for Projects
`WebFetch`, a Mod `allow` does not replace a forced core ask, deny, or allow; a
same-behavior ask keeps the core decision. In a conditional Bash case where a
core ask has already been evaluated but the aggregate Mod result is `allow`,
`Uoe("Bash")` is false and the `$Ln` path returns the Mod allow. This is a
source-level arbitration case, not a full run of Native's plugin dispatcher;
the actual aggregate result and how the plugin runner reached it remain outside
this fixture.

Separately, the actual `sec-default` `ue` handler treats a rule as a hold only
when `decision === "deny"` and `rule` is defined. With an earlier user-Mod
allow, the handler's appended managed deny-plus-rule result wins; appended
ask-plus-rule and deny-without-rule results do not. An explicit
`allowModsToOverrideDenyRules: true` skips the hold, and a settings-read failure
fails closed for the deny case.

These source and callback-level tests do not prove a complete installed-Mod
execution, real configuration acquisition, or a universal managed Bash ask
hold. The v2.1.289 release note describes a managed compound Bash ask/deny
permission fix; the callback-injected evidence here establishes recursive rule
extraction and the exact Native consumers above, while the full plugin
dispatcher/lifecycle remains open.

## Pinned source ranges

The byte offsets below are half-open ranges in the corresponding executable.
The oracle verifies each range hash before evaluating it.

| Version | Source | Byte range | SHA-256 |
| --- | --- | ---: | --- |
| 2.1.288 | `Vrt` / `Yrt` direct-only rule lookup | `187327473..187327796` | `85fea4ee43fd9dfcbcbc1f55223cdab35f98e328f1d91be45673c6d4f401b203` |
| 2.1.289 | `wEe` / `Ast` recursive rule lookup | `187628314..187628661` | `e1613f5a8ad7e8c0b33cee74af4b6a1646cc5a52c09ace9b8e511c8222e47191` |
| 2.1.289 | compound Bash producer `m6o` | `189692731..189695182` | `782f6076975014582b28413b34b6c6e91d9520cebd4353080b3dbe8f611017a2` |
| 2.1.289 | `Pst` core decision projection | `187628661..187628952` | `1087d88f782be664074cd9229366e8c69bff407434f4d3fd3de82bf20f5e0a1d` |
| 2.1.289 | `$Ln` main `tool.check` caller | `187630030..187630770` | `c6f5e6c8bf158ed514906506da208fad09e6254dd6d5e6e0048f3652dd9d0167` |
| 2.1.289 | `Uoe` Projects-session predicate | `183805687..183805858` | `7507090d046e83385390644695e89b695b66ec7b919048cc1dda2e69e98fa3cb` |
| 2.1.289 | `_r = "WebFetch"` | `183190392..183190459` | `f73e84506163c032831f0b853bd2ed0ba92b5a10880827926f57f0c517a69f5e` |
| 2.1.289 | `Pm = "WebSearch"` | `183304712..183304731` | `191c8bb713b7a0e2b2a27fef9c3c7d2003e35eb736a850a6e86d0244ad06c5d9` |
| 2.1.289 | `sec-default` managed `tool.check` consumer | `213011071..213014666` | `8d1bc3c4ea247bff06104cccf6389a242e626db59f26ba1105954f683838a2ae` |

