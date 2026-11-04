# Provider accounts and execution bindings

Provider accounts are Host-global, not User/Project/Local-session settings
layers. Use native Providers/Accounts UI to add/login, select a default account,
inspect health and model permissions. Account metadata and availability are
separate from credentials held by the native broker/secure store. Do not copy
tokens into settings, shared Projects, generated Apps, logs or MCP definitions.

A logical provider profile names a connection/catalog. An account identifies
a credential identity; an execution binding selects the concrete route. Model
permissions, health/cooldown and selection policy constrain each request. The
runtime may choose another eligible account for a retry; do not assume a model
label, parent profile or startup CLI default proves the request's final route.
Storage unavailable is distinct from rejected/expired authentication; do not
clear credentials to recover a storage or network failure.

Desktop owns credential capture and supplies request credentials to Bridge over
the trusted host channel. Apps only receive their approved model/tool catalog.
SDK capabilities do not prove native UI/device rollout or a real inference pass.

On the CLI, an account is added with
`lingxi provider accounts add PROVIDER --name NAME` and either
`--api-key-stdin` or `--api-key-secret-ref`. The key is never an argument and
is not printed back. OAuth uses `lingxi provider accounts login PROVIDER
--name NAME`, with `--no-browser` to print the address and `--callback-stdin`
for a remote shell. `list`, `logout`, and `remove` are the other account
commands. This does not create a LingXi subscription, and the key is not a
LingXi password.

Source anchors: product apps/electron/src/main/provider-accounts.ts; Harness
crates/runtime/src/provider_accounts.rs, crates/llm-runtime/src/accounts,
crates/llm-runtime/src/service.rs and client.rs.
