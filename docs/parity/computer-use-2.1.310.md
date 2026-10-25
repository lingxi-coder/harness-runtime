# Computer Use and Chrome reference: Claude Code 2.1.310

Status: reference audit and replay baseline, **not full implementation parity**.
Checked at 2026-10-25 04:18 UTC (2026-10-24 America/Los_Angeles).
The user requested inspection of latest Claude Code before connecting the
Desktop Computer Use and Browser settings. At the initial audit, functionality
was paused and the controls were disabled. Follow-up implementation now connects
the host-owned device policy, native execution/revocation checks and a LingXi
Chrome extension. The gap table below records the initial comparison, not the
current verification result. The product's
`docs/desktop-computer-use-permissions.md` documents the implementation and its
delivery/acceptance boundaries. Full byte-level parity remains unproven.

## Reference identity

- npm `@anthropic-ai/claude-code` latest resolved to **2.1.310** at verification.
  [Registry metadata](https://registry.npmjs.org/@anthropic-ai/claude-code/latest).
- Selected official platform package: `@anthropic-ai/claude-code-darwin-arm64@2.1.310`.
  [Pinned tarball](https://registry.npmjs.org/@anthropic-ai/claude-code-darwin-arm64/-/claude-code-darwin-arm64-2.1.310.tgz).
- Tarball integrity verified against registry metadata:
  `sha512-1HkmRv3tvMTjTrsW5J17S3NtjFjEDC8nnhduNT0klPStHes8/FRX1YFv7UwnfC6fuwd31kFuzWLyNn2IN07JtQ==`.
- Native executable: 274,068,224 bytes; SHA-256
  `ffb64331bc0ee1b69efb7c2c428bd3ab516bc31b75bf60886b356c97c1c25ffd`.
- The installed CLI was 2.1.308; it was neither used as latest nor upgraded.
- Comparison checkout: Harness `0e28910fd624638e6c5d1f92eca4899a7938d188`;
  LingXi Desktop settings presentation commit `0764ac6a2`.

“Latest” is a dated registry observation, not a floating parity guarantee. A
new release requires a newly pinned artifact and a new replay.

## Reproducible evidence

[Oracle runner](../../scripts/tests/computer_use_310_oracle.mjs) reads the binary,
verifies its complete digest and seven exact byte ranges, and executes only
reviewed bounded helper code in isolated Node VM contexts. Different bundled
modules keep separate scopes because their minified identifiers overlap.
It does not start Claude, import bundled modules, access credentials, connect
to Chrome, prompt for OS permissions, or inject native input.

[Generated fixture](../../crates/tools/computer-use/fixtures/claude-code-2.1.310.json)
SHA-256: `d3702f8c0458cfdd2487c63e528718c7d751515ab542dcff86ceb2c6e21e2e40`.
The fixture contains inputs, observed results and schema hashes, not extracted
application source. `--check` compares the complete regenerated JSON bytes.

```sh
node --check scripts/tests/computer_use_310_oracle.mjs
node scripts/tests/computer_use_310_oracle.mjs /path/to/claude-2.1.310 --check
```

Both checks passed. The 1,573 replay cases comprise:

| Group | Cases | Evidence scope |
| --- | ---: | --- |
| App classification and policy denial | 694 | All identifier and fallback-keyword entries, case and precedence probes |
| Grant normalization | 774 | Missing/read/click/full tiers, denied apps, restricted apps, explicit force-full option |
| Explicit grant lookup | 5 | Granted, user denied, policy denied and unknown targets |
| Clipboard and system-key classification | 22 | Native macOS and Windows classifier results from this macOS artifact |
| Sensitive-app sentinel classification | 44 | Shell, filesystem and system-settings identities |
| Session grant merge | 3 | Existing app identity, duplicate response entries and strict boolean flags |
| Tool schemas | 4 | Capability variants; structural schemas and exact full-tool JSON hashes |
| Chrome status | 27 | Controlled detection, connected state and default-enabled preference |

Chrome status replay supplies extension detection, configuration, account-policy
booleans and startup evaluation as controlled dependencies. It proves the
status helper's branching and output, not a real extension connection. Windows
classifier replay does not establish Windows native behavior. Schema hashes
are hashes of `JSON.stringify(tool)` output, not transport captures. No LingXi
production differential has passed against these fixtures yet.

REA recognized the executable as macOS arm64 with the same SHA-256. Its focused
JavaScript analysis parsed all four selected carriers (190,866 source bytes),
with no parse failures. Retained parent evidence:
`ev_60f61f72d0b1537e54ce4dfff8d1d3e34fe57d629fb84183a1fb35525a15f44d`.
That analysis is static and reports unresolved semantic relationships; it is
supporting navigation evidence rather than behavioral proof.

## Observed permission model and current gaps

| Surface | Observed 2.1.310 behavior | Current LingXi gap |
| --- | --- | --- |
| Request contract | `apps`, `reason`, three optional capability booleans; no model-selected tier | `computer` adds a caller-selected uniform `tier` |
| App tiers | Browser/trading `read`; terminal/IDE and Windows shell `click`; other apps `full`; explicit grants are clamped to category ceiling | No matching classifier/clamp in `handle_request_access` |
| Policy restrictions | Normalization drops user-denied and hard-policy-denied apps | Current session model has no matching policy denylist |
| Grant lifetime | CLI adapter owns session `allowedApps` and grant flags | Session state exists, but settings cannot revoke it |
| Merge | Existing bundle entries are preserved; only new bundle IDs append; only exact `true` enables a flag | `SessionState::grant_app` upgrades an existing tier, so its old “matches upstream” claim does not establish latest parity |
| Wildcard control | Shared engine has a separate wildcard policy with mandatory nonempty policy denylist; it grants allowed apps full tier | CLI adapter does not select wildcard; a Desktop global toggle must be explicitly wired to this policy, not interpreted as an ordinary session approval |
| Empty grants | Most calls fail before execution until an app is granted | Current tier gate exempts screenshot/zoom and other non-input actions |
| Capture isolation | Tool schema distinguishes native-filtered, masked and unfiltered capture; actual capability selects the claim | Current macOS backend captures the selected monitor without per-app filtering |
| Input authorization | Checks include actual target, app restrictions, grant flags, OS state and locks | Current main tier gate checks the frontmost app, insufficient for arbitrary coordinate targets |
| Clipboard | Three flags default false; copy/cut/paste chords require corresponding grants too | Existing raw clipboard gates do not establish the native key-chord matrix |
| OS permission setup | Missing TCC invokes a distinct panel and rechecks; success asks the model to call `request_access` again | Current request handler attaches TCC state but resumes into the same grant flow |
| Background vs display | `app_*` tools and screen takeover have distinct authorization and locks | No equivalent background tools or takeover consent contract in current `computer` action schema |
| Locked use | Background capture may proceed when permitted; locked macOS still blocks background writes | Cannot turn on a switch that promises click/type while locked |
| Stop/locks | CLI session wiring owns locks and Esc abort; background window locks and display takeover are separate | Existing lock/cancellation support does not establish this complete lifecycle |
| Chrome configuration | User-level enabled preference; detection/onboarding caches; actual connected MCP client status | Product Browser controls have no backing host service |
| Chrome permissions | `/chrome` manages permissions through the extension-owned route; site access is not granted by enabling generic computer control | No extension transport/site-policy enforcement path was found in the bounded product/runtime search |

Relevant current sources:

- [`handle_request_access`, schema, dispatch and tier checks](../../crates/tools/computer-use/src/lib.rs).
- [Session state](../../crates/tools/computer-use/src/permission_model.rs).
- [Shortcut approximation](../../crates/tools/computer-use/src/validate.rs).
- [macOS capture](../../crates/platforms/macos-computer-control/src/macos/mod.rs).
- [Desktop runtime assembly](../../crates/runtime/src/desktop/assembly.rs).
- Product `apps/electron/src/renderer/components/settings/pages/ComputerUse.tsx`
  and `ComputerUseBrowser.tsx`: presentation-only controls.
- Product `apps/electron/src/main/bridge.ts`: approval lookup checks pending
  request identity; request-subset validation must also be enforced before
  any backend grant is committed.

The engine's explicit `forceFullTier` option is a distinct host mode. Its
presence does not justify bypassing category limits in ordinary CLI sessions.
Likewise, subscription/account gating is an observed Anthropic product boundary,
not an instruction to remove LingXi's existing provider support.

## Chrome and the screenshot's settings

The pinned CLI's `/chrome` command offers connection status, installation,
reconnect, browser selection, enabled-by-default and Manage permissions. The
permissions route observed in the artifact is
`https://clau.de/chrome/permissions`. Setup checks live connection before marking
onboarding/default-enabled completion. Chrome context passes socket discovery,
client identity, pairing and connection/authentication handlers into its bridge.
Static context wiring does not establish every byte of that transport or of the
separately shipped extension's policy engine.

The supplied ChatGPT screenshot is the visual target, but it is not evidence
that Claude Code exposes the same History/Downloads/WebMCP/full-CDP/Excel or
always-allowed-app preferences. Those controls must be mapped to a verified
backend capability or remain unavailable. Do not write independent renderer
preferences and label them Claude-aligned browser permissions.

The installed Claude Desktop 3.6556.2 was examined only as supplementary
evidence, not verified as latest. ASAR analysis found a native-addon integrity
mismatch. Continued static results are untrusted and partial; none establishes
the authoritative CLI baseline or complete Desktop UI parity.

## Implementation acceptance gates

1. Consume the oracle cases in a differential suite while replacing the old
   caller-selected tier model. Keep protocol adapters explicit: LingXi's
   combined `computer` tool is not the reference's separate MCP tool surface.
2. Add a host-owned policy authority and authenticated IPC. Persist supported
   device settings atomically, publish changes to live runtimes, and reject
   approvals completed against an obsolete policy revision. Revocation must
   affect active sessions and pending requests.
3. Enforce policy in the execution path for tool, native-frame and batch
   operations, including screenshots, clipboard and open-app calls. Leave
   cleanup/release possible after disabling control. Validate approved apps
   and flags against the actual pending request at the trusted boundary.
4. Implement app-filtered capture and hit-target checks before claiming native
   Computer Use parity. Test unauthorized target windows, focus changes,
   revoked grants, batch steps, cancellation, OS denial and lock transitions.
5. Connect a real browser transport and extension policy owner. Verify site
   approval, redirect/subdomain handling, downloads/history/site-tools/CDP
   routing and reconnect before enabling corresponding Browser controls.
6. Check the signed Desktop package end to end. Helper replay, TypeScript
   compilation or a settings screenshot alone is not packaged acceptance.

## Static source ledger

Ranges below are half-open byte offsets in the pinned native executable.
The oracle embeds narrower independently hashed ranges for its replay.

| Carrier | Range | SHA-256 |
| --- | --- | --- |
| Permission engine | `[237496498,237637683)` | `d2eceedd3964628741285a44b82d88339e9fa4eb0d810c1e27262a7cb31a9010` |
| Tool definitions | `[237461171,237496497)` | `e3abf6716e2783d23ada560f9c3628eaf3cf5e8f397086d6495f80de0876b2bb` |
| Capability defaults/sentinels | `[237459100,237461170)` | `7b0290e5a60299a481f05b4467fae692224e4ab6fe19ccd478cc65dfebc3886f` |
| CLI session adapter | `[263885624,263897909)` | `4447192f1e2b0e1ebbcd886cbda4ccdba2663418e5a63d518974d2e33a099d06` |
| Chrome onboarding | `[235244104,235474913)` | `2e1031eb31bdf9604d2d1d84df689f9f117c43a6090c025c53ef1c7aca67ee1e` |
| Chrome bridge context | `[237800129,237828630)` | `17e04438187ead56f53bab0018602e01a59bc365b3d987113266def926c8b39a` |
| Chrome status/permissions route | `[262151704,262153194)` | `cbfa690fd9c13d958309ffc91293e1c17e9c0adde14ba4b7ab79b8f6ca31e362` |
| `/chrome` command | `[262153195,262181240)` | `6fcea72e876118b329240df7259410ba2bae4ae2b1ff7472001a3701ada2669d` |

Official documentation corroborates the architecture but is not a substitute
for this pinned evidence: [Computer Use](https://code.claude.com/docs/en/computer-use),
[Chrome](https://code.claude.com/docs/en/chrome),
[Desktop](https://code.claude.com/docs/en/desktop).
