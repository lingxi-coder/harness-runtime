# Claude Code 2.1.289 server-refusal API-error text

This note records the text emitted when an accepted server-fallback response
is later declined for reason "refusal". The executable source probe is
[mods_refusal_fallback_text_289_probe.mjs](../../scripts/tests/mods_refusal_fallback_text_289_probe.mjs);
its captured hashes and six formatter outputs are in
[mods-refusal-fallback-text-2.1.289.json](mods-refusal-fallback-text-2.1.289.json).
Run it with:

~~~sh
node scripts/tests/mods_refusal_fallback_text_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
~~~

The pinned binary SHA-256 is
03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69. The
probe executes the exact $dt function and its text helper bodies with
explicit provider, interactivity, feedback, model-eligibility, model-label,
and model-family facts. Those catalog/policy facts are inputs to the pure text
projection; the probe does not launch the Native CLI or execute a provider
request. Low-level sanitizer imports are stubbed only for the exercised ASCII
categories (cyber and bio).

## Server-fallback call and row shape

The declined-refusal call is pinned at [195455728,195456130), SHA-256
af0bc32abd5dfa44b69a773d6f4ccd44a63fb9e9de57ffa025433f46f3a5c149:

~~~js
$dt("refusal", {
  type: "refusal",
  category: M.apiRefusalCategory ?? null,
  explanation: null,
  fallback_credit_token: null,
  fallback_has_prefill_claim: null,
  recommended_model: null
}, M.requestId, ge)
~~~

The $dt body is [188298052,188299837), SHA-256
7a19dc31283968e16b1583ddc200d3b9ad0097f094e9de501f61733083242455. It
returns the API-error assistant row with error "invalid_request",
message.stop_reason "refusal", and message.stop_details set to the
supplied refusal object. In this caller, explanation is always null and no
message ID argument is supplied. The displayed prefix is the exact string
API Error ([181462894,181462922)).

The formatter's inputs are not just the refusal category. It reads the
serving-model identity ge, host.launchOptions.isInteractive(), current
provider mode, current feedback eligibility, and current model-catalog family
and labels. A Rust formatter can stay pure by receiving those resolved facts
as explicit inputs. In particular, the eYe(ge) refusal-model decision and
$o(ge) display label are separate facts: $o is a model-label resolver that
consults Native model/catalog sources and can return a public label instead of
the wire ID. The value returned by eYe is used as a gate; $dt still formats
the supplied ge model, not the candidate returned by KI. Do not use an
organization availableModels membership test as a substitute for eYe.

## Exact text branches

The leading label is $o(ge) when ge is present; when it is absent the
generic no-model branch uses Claude. The model-labelled branch runs only
when ge != null and eYe(ge).

When that branch is taken, Native first asks v1r(label, ge, category, suffix)
for family-specific refusal copy. The suffix is the literal
" (https://www.anthropic.com/legal/aup)" including the leading space. If v1r returns text, the first
sentence is:

~~~text
{label}'s safeguards flagged this session (https://www.anthropic.com/legal/aup). {family-specific copy}
~~~

That branch continues with:

~~~text
Claude Code can't respond to your last message with {label}.
~~~

If v1r has no copy, the first sentence instead is:

~~~text
{label}'s safeguards flagged this message (https://www.anthropic.com/legal/aup). {category copy}
~~~

For cyber and bio, the category copy is the exact E1r string in the
fixture. Other categories use:

~~~text
This sometimes happens with safe, normal conversations.
~~~

That branch ends its refusal sentence with:

~~~text
Claude Code can't respond to this message with {label}.
~~~

The ordinary model-labelled suffix is exe(ge): when /feedback is
eligible, it is Send feedback with /feedback or learn more: {help URL};
otherwise it is You can learn more: {help URL}. Non-interactive mode instead
uses Learn more: {help URL}. The help URL selector returns
https://support.claude.com/en/articles/16049681 for canonical Opus 5
identities, https://support.claude.com/en/articles/15363606 for Fable
IDs, the fable/fable[1m] aliases, catalog-recognized Fable IDs, or the
configured default Fable model, and
https://support.claude.com/en/articles/8106465 otherwise. The interactive recovery line is
Double press esc to edit your last message, or try a different model with /model.
The non-interactive line is
Try rephrasing the request in a new session or change your model.

When no displayable refusal model is available, the special cyber wording
applies only when category is cyber and ad() is true. The provider gate
ad() is true for firstParty, anthropicAws, anthropicGoogleCloud, and
gateway. In that branch the body is:

~~~text
API Error: {label}'s safeguards flagged this message. Our intentionally broad safeguards allow us to deliver more capabilities faster, but can sometimes flag legitimate cybersecurity work. Apply to the Cyber Verification Program to reduce these interruptions. {learn-more-or-feedback-link}
~~~

The link is
https://support.claude.com/en/articles/14604842-real-time-cyber-safeguards-on-claude.
For every other no-displayable-model case, the body is:

~~~text
API Error: {label} can't help with this. Start a new session to continue.

{learn-more-or-feedback-link}
~~~

Here {label} is $o(ge) if a serving model was supplied and Claude
otherwise. In this no-model branch, Ce() || !QA() selects
Learn more: https://www.anthropic.com/legal/aup; only interactive mode with
QA() === true selects
Send feedback with /feedback or learn more: https://www.anthropic.com/legal/aup.

After the main body, $1e(category) appends
two newlines, Details: followed by a U+0060 grave accent, bracketed category,
and a second U+0060, for a non-empty sanitized category. It removes grave
accents and square brackets from the category, collapses whitespace, trims,
then truncates to 255 code units. For cyber the exact suffix is:

~~~text

Details: &#96;[cyber]&#96;
~~~

Native uses a 255-code-unit cap. The optional request suffix is
two newlines, Request ID: {id} only when the ID matches
^[A-Za-z0-9_-]{1,255}$. The row still keeps the caller's requestId value even
when that textual suffix is omitted. A message-ID suffix exists in $dt for a
fifth argument, but this server-fallback caller supplies four arguments, so
it is absent here.

## Mode, provider, feedback, and family facts

Ce() is exactly the negation of
host.launchOptions.isInteractive() ([179307203,179307263)). Thus the branch
variable named ye in $dt means non-interactive.

He() resolves gateway first, then provider environment flags in this
order: Bedrock, Foundry, Anthropic AWS, Anthropic Google Cloud, Mantle, Vertex,
and finally first-party. ad() accepts first-party, Anthropic AWS, Anthropic
Google Cloud, and gateway only ([180332633,180333499)). The source therefore
does not use the Cyber Verification copy for Bedrock, Foundry, Mantle, or
Vertex.

QA() is Nre() === null. The probe executes Native's Ivt/Tt environment
helper and Nre/QA bodies: Nre() first checks whether /feedback is
disabled by DISABLE_FEEDBACK_COMMAND, DISABLE_BUG_COMMAND, or the
nonessential-traffic mode; if none applies, it asks the allow_product_feedback
policy/settings gate. The nonessential-traffic mode is sourced from
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC; DISABLE_TELEMETRY and
DO_NOT_TRACK resolve to no-telemetry, not that mode. Therefore feedback
eligibility is a live policy fact, not a simple provider or interactive-mode
check.

For first-time wording, Native obtains La(Nm(servingModel))?.family. Its
$I family table in this build gives the sonnet family only the cyber
category and the suffix " on a Sonnet model"; the explicit model exception set
also includes claude-opus-5-5. For example, the source-probe vector for
claude-sonnet-5 / cyber produces the exact "You may be seeing this for the
first time on a Sonnet model: ..." sentence in the JSON fixture. Dw(model)
can suppress this family-specific copy and selects the Fable help page for
Fable family IDs/aliases and the configured default Fable model. $o and
La/Nm depend on Native's live model catalog and labels; the probe injects
these values instead of fabricating a static catalog.

The pinned eYe body delegates to KI and only returns whether its
refusal_fallback route resolved. KI reads the model's refusal_fallback policy,
the provider kind, the current fallback candidate, and model-identity
validation. $dt uses this as a boolean gate; it does not print KI's candidate.

## Tool-returned rows in je

The same output projection confirms that je is broader than native
tool_result user rows. Tn iterates each completed executor result and calls
xr(A.message, ...) ([195439205,195439671)). xr pushes attachment rows
directly into je; for other messages it projects with Lv and adds the
projected user rows ([195429764,195430089)). The executor's zyo producer
queues each tool-returned _o.newMessages row; user rows are copied with
turnCompanion: true, and other row types are queued unchanged
([188849094,188849265)). Thus tool-returned user messages and attachment
rows can join je; non-user rows are not automatically added by xr.

On accepted server fallback, when tombstonedToolUse is true, Native captures
to = [...je], resets the query's tool-result accumulator, and rebuilds the
streaming executor. The exact branch is [195457049,195457553), SHA-256
620a9fbe09bfbd2bc31b4de0331018e69829feff4bb46c7ea51cd2c881f422af.
Therefore that cleanup covers the current je contents as a whole, including
projected tool-result users, tool-returned user messages, and attachments. It
is not a per-tool-use-ID filter. The formatter and accumulator source windows
and executed output are recorded in the JSON fixture.

## Scope boundary

This source probe supplies explicit booleans/labels for the KI-based model
eligibility decision, live model label/family lookup, and feedback policy. It
does not dynamically recreate Native's settings, policy service, model catalog,
or provider environment. These are required inputs to a Host pure formatter;
they must come from existing trusted Host facts if exact runtime behavior is
required. The six formatter vectors demonstrate the exact text construction
for the supplied facts, not a full Native server-fallback runtime session.
