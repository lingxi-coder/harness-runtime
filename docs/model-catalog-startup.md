# Official model catalog startup

The product supplies `StartupCatalog` with its actual platform, application
version and private LingXi home. Desktop and CLI use `DesktopConfig`; Android
and iOS use `MobileConfig`. The default embedder configuration performs no
catalog networking. This does not change the Claude Code 2.1.295 target.

An existing validated cache starts the runtime immediately while an eight-second
HTTP request revalidates it in the background. A cold owner overlaps its bounded
fetch with unrelated startup work; an unsuccessful cold fetch uses embedded SDK
definitions. Updated cache data is admitted by the next runtime owner. An active
runtime and its model list retain their original snapshot, including during
settings refreshes and in-process side chats.

Cache identity includes endpoint, platform, exact application version and schema.
Conditional requests use ETags; 304 requires a validated cache. Source hashes,
the complete body checksum, SDK parsing and execution-registry construction are
checked before admission. Invalid publications, redirects, oversized bodies and
publication rollback retain the last valid cache. Writes use a nonblocking
per-key lock, a new temporary file, sync and rename. Contention does not block
startup or overwrite a newer publication.

An omitted provider `executionDefinition` explicitly references the pinned SDK's
definition with that profile id. New providers or models require a complete SDK
preset TOML definition with its SHA-256. The SDK parses both source forms with
the same rules, including service-only model exclusions. The published rows
override their display facts, availability, limits and listed prices; protocol,
auth, endpoint controls, independent services and pricing schedules retain the
complete SDK definition. Changing protocol requires a matching definition.

The snapshot enters provider assembly before user settings. Custom profiles,
models, routing aliases, endpoint overrides and credential ids keep precedence.
No settings or credential store is written by catalog delivery, and public GETs
contain no account, workspace or provider credentials. Host-owned ChatGPT sign-in
is independent of the public OpenAI API inventory. Removing a primary protocol
format does not rebind a surviving format to another credential slot.

Availability decided by publication or explicit user models bypasses the binary's
embedded model shortlist through an internal, non-serialized metadata flag. The
same registry supplies routing, prices, provider settings and model pickers.
This also allows a fully defined new model to appear without updating a static
shortlist. A retired built-in boot default can choose an available route without
rewriting saved settings; managed model policies and unknown custom names are
not replaced by this fallback.

Validation commands:

```sh
cargo test --locked -p provider-config
cargo clippy --locked -p provider-config -p llm-runtime --all-targets -- -D warnings
cargo check --locked -p harness-runtime --features desktop,mobile,uniffi
```

`startup_catalog_probe` is a credential-free, read-only acceptance example. It
queries the public endpoint for four example app versions and uses only its
specified isolated directory. It never sends an inference request, deploys a
service or changes a published catalog. Host checks and public-query evidence
do not substitute for native application compilation or device acceptance.
