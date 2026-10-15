# Claude Code 2.1.289 refusal formatter inputs

This note traces the values consumed by the server-refusal API-error formatter
to their Native producers. The source-pinned executable probe and its receipt
are [mods_refusal_formatter_inputs_289_probe.mjs](../../scripts/tests/mods_refusal_formatter_inputs_289_probe.mjs)
and [mods-refusal-formatter-inputs-2.1.289.json](mods-refusal-formatter-inputs-2.1.289.json).
Run it with:

```sh
node scripts/tests/mods_refusal_formatter_inputs_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
```

The pinned binary SHA-256 is
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`.
The JSON receipt records every tested source byte range and hash, plus the
controlled execution outputs. No Native CLI process, account, network, or
provider request is used.

The server-fallback caller already pinned in
[the text probe](mods-refusal-fallback-text-2.1.289.md) passes `ge` to `$dt`.
That is the model selected for the current query. The query model state is
derived from `refusalOverride ?? fableConsentOverride ?? liveSwitchOverride ??
chainModel ?? primaryModel`; the query reducer then applies `Jf` with the
current permission mode and the 200k-token condition. The model can change
during a live switch, and the reducer updates `ge` before a later refusal is
formatted. The source is pinned at `[195413357,195413473)` for the precedence,
`[195432246,195433666)` for the `br`/`qs`/`ge` producer, and
`[182036250,182037196)` for the `Jf` plan clamp. This makes the formatter input
the current request model, not the fallback candidate that `KI` may return.

`eYe(model)` is exactly `KI(model) !== undefined`. The decision does not use
the organization `availableModels` allowlist. `KI` first obtains the model
identity and checks `CLAUDE_CODE_MODEL_CAPABILITIES`; an explicit matching
`false` immediately rejects the model. Without that explicit deny, the model
can qualify through the refusal capability, the Fable family, the
`fable_5_mitigations` capability (including the hardcoded `claude-mythos-5`
case), the `-eap` suffix, an exact match with
`ANTHROPIC_DEFAULT_FABLE_MODEL`, or the GrowthBook `tengu_loggia_roster`.
Mythos IDs have an earlier roster gate. The probe executes the exact `KI`,
`eYe`, capability resolver, and Fable/Mythos predicates with explicit
settings, catalog, and roster inputs.

Native's `fy(model, capability, originalSpelling)` precedence is:

1. `CLAUDE_CODE_MODEL_CAPABILITIES` segments, where semicolon-separated
   entries may be unscoped capability lists or `model=capability,...`
   selectors. A trailing `*` is a prefix selector. Capability items prefixed
   by `-` map to false. The last matching item for a capability wins, and
   false is preserved by the nullish fallback.
2. The served-catalog runtime callback. Its current producer looks up runtime
   capability names case-insensitively and returns true or undefined; it does
   not return false as a catalog veto.
3. The baked `Hsr` catalog's capability list.

For this capability parser, model normalization removes `[1m]` case-insensitively
before matching; it does not itself strip `[2m]`. `refusal_fallback` is not in
Native's feature-gated `S` map, so an unrelated disabled feature-gate callback
does not block a served `refusal_fallback` true. The executed vectors show an
environment deny overriding both the served and baked sources, a served true
adding a capability absent from the baked row, and a served absence falling
through to baked metadata.

The distinction between dynamic and baked data matters. `Hsr` is explicitly
commented as the hand-maintained baked-in model catalog. `La` indexes only
that object; it supplies the family, built-in display name, and baked
capabilities. The separate `WQe`/`Oy` path reads active or deactivated served
catalog rows and can supply dynamic custom-model names and runtime capability
lists. `$o` consults the baked provider-ID table first: a known provider ID
uses the baked display label, while an unrecognized custom ID can use the
served-catalog `name`. The probe executes both the baked built-in label path
and a controlled served custom-model label path. The live catalog fetch and
model-identity knowledge cache are not recreated.

Family-specific wording uses `La(Nm(model))?.family`, then the static `$I`
family table. In this binary the Sonnet family has the cyber first-time copy;
`claude-opus-5-5` is an explicit additional first-time model even though its
family is Opus. `Dw(model)` suppresses this wording for a Fable identity, the
`fable`/`fable[1m]` aliases, catalog-recognized Fable IDs, and the configured
default Fable model. The probe executes `v7`, `Lw`, `Dw`, and `b1r` against the
baked catalog. As a useful boundary, Opus 5.5 gets the first-time model
classification but its help-link selector still uses the generic link; the
Opus-specific link checks canonical Opus 5.

`KI` also needs a usable Opus fallback candidate. For accepted provider kinds,
it uses `GI(E9r())`; `E9r` reads `ANTHROPIC_DEFAULT_OPUS_MODEL` when present and
otherwise reads the provider-resolved model list `jS()[Gd()]`. `Gd` defaults to
the `opus5` position in Native's ordered alias list
`[opus55, opus5, opus48, opus47, opus46, opus45]`. When `ad()` accepts the
provider, `Gd` looks up its `opus` alias and advances from `opus5` only if the
provider alias is later in that list. The controlled source execution returns
`opus5` for first-party and Anthropic AWS/Google Cloud, and `opus47` for the
gateway fixture. This is the alias key used by `jS()[Gd()]`, not a claim that
those IDs are the resolved model names. The default key/order are pinned at
`[182032468,182032490)` and `[180331314,180331491)`. The probe does not execute
the live `jS()` provider model list when `ANTHROPIC_DEFAULT_OPUS_MODEL` is
absent, so it does not claim a universal selected model. The remote
compatibility gate may map Opus 5 to Opus 4.8 unless the active availability
fallback chain already contains Opus 5. For other providers, `KI` uses
`jI("claude-opus-4-8")`,
which looks first at the configured family default, then at current model
picker options. The candidate must pass Native's baked Opus-family check. The
eligibility vectors supply an explicit canonical Opus default and controlled
remote-gate inputs; the separate provider vectors execute the exact `Gd` alias
selection from Native's `Dqt`, `UI`, and `Yb` sources.

The special cyber copy's provider fact is `ad()`, not a model or allowlist
check. `He()` selects gateway first when any of three host credential slots is
present or required; otherwise it checks Bedrock, Foundry, Anthropic AWS,
Anthropic Google Cloud, Mantle, then Vertex environment flags, and defaults to
first-party. `ad()` accepts first-party, Anthropic AWS, Anthropic Google Cloud,
and gateway. The exact host credential-slot methods and provider classifier are
source-pinned and executed with controlled values.

The interaction fact is read from
`host.launchOptions.isInteractive()` (`Ce()` is its negation). The feedback
gate is not another GrowthBook Boolean: `QA()` is `Nre() === null`, and
`Nre('/feedback')` checks, in order, `DISABLE_FEEDBACK_COMMAND`,
`DISABLE_BUG_COMMAND`, the nonessential-traffic mode, then
`dp('allow_product_feedback', '/feedback', 'is')`.

That final `dp` calls the Native effective policy decision `sn`. The built-in
`allow_product_feedback` metadata says `serverPopulated: true`,
`onCacheMiss: "deny"`, and `deniedUnder: ["hipaa", "zdr"]`. With a loaded
policy snapshot, `sn` considers its restriction, compliance taints, and hinted
policy state; with an enabled policy service but no cached policy, the
deny-on-cache-miss path makes `QA()` false. A successful permission decision
makes `dp` return null; a denial returns a non-null reason. The probe executes
the exact `sn`, `dp`, `Nre`, and `QA` helpers with explicit policy-service and
session-policy snapshots. It does not fetch a real account verdict. The
`DISABLE_*` fields are evaluated with JavaScript truthiness, so the strings
`"false"` and `"0"` suppress feedback when they reach `aI` unchanged.

For a pure Host formatter, the required snapshot is therefore the actual
serving model `ge`, Native's resolved `$o(ge)` label, `eYe(ge)`,
`La(Nm(ge))?.family`, `Dw(ge)`, the current provider kind `He()`, the host
interactive flag, and the current `QA()` result. The runtime should source the
eligibility and feedback facts from their corresponding policy/capability
resolvers. A user/org available-model list is not an equivalent model-refusal
decision, and an assumed current Opus/Fable default is not an equivalent
candidate.

The current source probe deliberately leaves these live inputs explicit:
the model capability environment string, GrowthBook roster, served model
catalog and identity-spelling cache, model-picker options, provider credential
slots, resolved account feedback policy, and current model-switch state. Those
are evidence boundaries, not Native defaults. The earlier text formatter
probe still executes `$dt` itself; this probe establishes where its dynamic
inputs come from and executes the gate helpers under controlled snapshots.
