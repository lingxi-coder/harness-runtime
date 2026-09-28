//! r1-backlog-protocol-mirrors-05 — the positional lock `variant_ordinal_lock_test.rs`
//! does not cover.
//!
//! # What this catches that `variant_ordinal_lock_test.rs` cannot
//!
//! That file freezes `ClientCommand` and `ClientEvent` declaration order. It
//! says nothing about `local_apps.rs`, whose own doc comments make the same
//! claim about their own types:
//!
//! - `AppRecordDto` (a `#[derive(uniffi::Record)]` struct) — `scaffolded`'s
//!   doc comment: "Appended LAST: UniFFI encodes record fields POSITIONALLY,
//!   so a field inserted above `workspace_rel` would be reinterpreted by a
//!   client built against the previous bindings."
//! - `AppEventDto` (a `#[derive(uniffi::Enum)]` enum) — `AppCreated`'s doc
//!   comment: "Appended at the END, and that is load-bearing rather than
//!   tidy: UniFFI encodes this enum POSITIONALLY, so a variant inserted
//!   anywhere above renumbers every variant after it."
//! - `PluginCommandDto` (also `#[derive(uniffi::Enum)]`) — its own doc:
//!   "Future §17.1 additions … extend this enum, not `ClientCommand` again",
//!   i.e. it is a real UniFFI enum subject to the same ordinal rule, just
//!   never given a lock.
//!
//! - `LocalAppVerificationStatusDto` (`#[derive(uniffi::Enum)]`,
//!   `#[non_exhaustive]`) — five variants both mobile clients switch on.
//! - `LocalAppCreateConfirmationRequestDto` and
//!   `LocalAppMcpProposalApprovalRequestDto` (both `#[derive(uniffi::Record)]`)
//!   — the two native approval sheets' payloads.
//!
//! The last three were added here rather than to a fourth parser inside
//! `src/local_apps.rs`'s own `mod tests`: `cargo test -p client
//! --test local_apps_ordinal_lock_test` is the command a lane runs after
//! touching these types, and a lock it does not execute is not a lock.
//!
//! Before this file, nothing would fail if a field were spliced above
//! `workspace_rel`, or a variant spliced into the middle of `AppEventDto` or
//! `PluginCommandDto`: `version_guard_test.rs` fingerprints an UNORDERED
//! map of `"<type>::<member>" -> "<wire type>"`, so it is as blind to
//! position as the commands/events guard `variant_ordinal_lock_test.rs`
//! describes itself fixing for those two enums.
//!
//! # The rule this enforces
//!
//! The members frozen below are `AppRecordDto`'s fields, and
//! `PluginCommandDto` / `AppEventDto`'s variants, as declared at the
//! `CLIENT_PROTOCOL_VERSION` 12.0.0 bless (commit `0c313e1b9`,
//! `client/src/protocol/local_apps.rs`). Each must still sit at exactly the
//! index recorded here.
//!
//! - Appending AFTER the frozen prefix: allowed, no bump. Do not add it here
//!   — this is the 12.0.0 snapshot, not a mirror of the current type.
//! - Inserting, removing or reordering anything inside the frozen prefix:
//!   this test goes red and names the ordinal. Move it to the end instead.
//!   If it genuinely cannot go last, that is a MAJOR `CLIENT_PROTOCOL_VERSION`
//!   bump plus a re-bless of `snapshots/blessed_major.txt`, and the frozen
//!   lists here are then re-cut from the new major.

const LOCAL_APPS_SRC: &str = include_str!("../src/protocol/local_apps.rs");

/// `AppRecordDto`'s fields in declaration order as of the 12.0.0 bless
/// (`0c313e1b9`). 11 entries.
const APP_RECORD_DTO_FIELDS_AT_12_0_0: &[&str] = &[
    "id",
    "name",
    "brief",
    "git_enabled",
    "created_at_ms",
    "updated_at_ms",
    "workflow_state",
    "conversation_id",
    "init_session_id",
    "workspace_rel",
    "scaffolded",
];

/// `PluginCommandDto`'s variants in declaration order as of the 12.0.0 bless
/// (`0c313e1b9`). 10 entries.
const PLUGIN_COMMAND_DTO_ORDINALS_AT_12_0_0: &[&str] = &[
    "SetEnabled",
    "GetStatus",
    "GetInventory",
    "ResolveCreateConfirmation",
    "ResolveMcpProposalApproval",
    "StartLocalAppMcpAuthoring",
    "SetLocalAppMcpEnabled",
    "SetLocalAppMcpToolEnabled",
    "SetLocalAppMcpConversationPinned",
    "GetManagedMcpInventory",
];

/// `AppEventDto`'s variants in declaration order as of the 12.0.0 bless
/// (`0c313e1b9`). 20 entries.
const APP_EVENT_DTO_ORDINALS_AT_12_0_0: &[&str] = &[
    "AppDetailsChanged",
    "AppBridgeResponse",
    "AppUiRequest",
    "AppCapabilityRequested",
    "AppCheckpointsChanged",
    "AppLlmActivityChanged",
    "AppAgentEventPosted",
    "AppBridgeStreamFrame",
    "AppRecordChanged",
    "AppProfileProposal",
    "AppBackgroundTaskChanged",
    "AppCreated",
    "AppDependencyChangeConfirmationRequested",
    "PluginStatusChanged",
    "PluginInventoryChanged",
    "CreateConfirmationRequested",
    "McpProposalApprovalRequested",
    "ManagedMcpInventoryChanged",
    "VerificationSummaryChanged",
    "LocalAppOperationFailed",
];

/// `LocalAppVerificationStatusDto`'s variants in declaration order as of the
/// 12.0.0 bless (`0c313e1b9`, where the body is byte-identical to today's).
/// 5 entries.
///
/// r3-never-wired-05: `Failed` sits at ordinal 2 and, for a long time, had no
/// production producer at all — which is exactly what makes "just delete the
/// dead variant" look free. It is not: dropping it renumbers `Unverified` and
/// `Unavailable`. (It does have a producer now,
/// `runtime/src/mobile/local_apps_host.rs:2016-2036`, but this lock does
/// not depend on that and must outlive it.)
const LOCAL_APP_VERIFICATION_STATUS_DTO_ORDINALS_AT_12_0_0: &[&str] =
    &["Pending", "Passed", "Failed", "Unverified", "Unavailable"];

/// `LocalAppCreateConfirmationRequestDto`'s fields in declaration order as of
/// the 12.0.0 bless (`0c313e1b9`), minus the `receipt` field removed by
/// r1-backlog-native-confirmation-13. 10 entries.
///
/// The removal was ordinal-safe because `receipt` was LAST in both this record
/// and `LocalAppMcpProposalApprovalRequestDto`: nothing that survives moved.
/// Every entry below still sits at the ordinal the 12.0.0 bless gave it, so
/// this list is still a valid frozen prefix against installed clients.
const LOCAL_APP_CREATE_CONFIRMATION_REQUEST_DTO_FIELDS_AT_12_0_0: &[&str] = &[
    "request_id",
    "app_id",
    "name",
    "brief",
    "selected_template",
    "runtime_profile",
    "reason",
    "rejected",
    "initial_tools",
    "required_gates",
];

/// `LocalAppMcpProposalApprovalRequestDto`'s fields in declaration order as of
/// the 12.0.0 bless (`0c313e1b9`), minus the `receipt` field removed by
/// r1-backlog-native-confirmation-13. 11 entries.
const LOCAL_APP_MCP_PROPOSAL_APPROVAL_REQUEST_DTO_FIELDS_AT_12_0_0: &[&str] = &[
    "request_id",
    "app_id",
    "workflow_run_id",
    "summary",
    "proposal_sha256",
    "approval_contract_sha256",
    "tool_surface_sha256",
    "tool_diffs",
    "required_flow_changes",
    "excluded_capabilities",
    "pending_gates",
];

/// Strip `//`-comments so brace counting is not thrown off by prose.
fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(at) => &line[..at],
        None => line,
    }
}

/// Extract the top-level variant identifiers of `enum <name>` from Rust
/// source, in declaration order. Identical approach to
/// `variant_ordinal_lock_test.rs::variants_in_declaration_order` (kept as an
/// independent copy rather than a shared `#[path]` include, so a bug fixed
/// in one does not silently start passing the other without review).
fn variants_in_declaration_order(src: &str, enum_name: &str) -> Vec<String> {
    let header = format!("pub enum {enum_name} {{");
    let start = src
        .find(&header)
        .unwrap_or_else(|| panic!("`{header}` not found in the source"))
        + header.len();

    let mut depth: i32 = 1;
    let mut out = Vec::new();
    for line in src[start..].lines() {
        let code = strip_line_comment(line);

        if depth == 1 {
            let trimmed = code.trim_start();
            let ident: String = trimmed
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !ident.is_empty() && ident.starts_with(|c: char| c.is_ascii_uppercase()) {
                let rest = trimmed[ident.len()..].trim_start();
                if rest.starts_with('{')
                    || rest.starts_with('(')
                    || rest.starts_with(',')
                    || rest.is_empty()
                {
                    out.push(ident);
                }
            }
        }

        for c in code.chars() {
            match c {
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' => depth -= 1,
                _ => {}
            }
        }
        if depth <= 0 {
            break;
        }
    }
    out
}

/// Extract the top-level field identifiers of `struct <name>` from Rust
/// source, in declaration order. `pub struct` bodies here hold only
/// `pub <field>: <Type>,` lines (plus attributes/doc comments, which never
/// start a line with a lowercase identifier followed by `:`), so — unlike
/// the enum parser — depth tracking only needs to stop at the matching
/// close-brace; nested `{`/`}` inside a field's own type (e.g.
/// `Vec<AppDataFieldDto>`) never occurs because Rust field types don't use
/// braces.
fn fields_in_declaration_order(src: &str, struct_name: &str) -> Vec<String> {
    let header = format!("pub struct {struct_name} {{");
    let start = src
        .find(&header)
        .unwrap_or_else(|| panic!("`{header}` not found in the source"))
        + header.len();

    let mut depth: i32 = 1;
    let mut out = Vec::new();
    for line in src[start..].lines() {
        let code = strip_line_comment(line);
        let trimmed = code.trim_start();

        if depth == 1 {
            if let Some(rest) = trimmed.strip_prefix("pub ") {
                let ident: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !ident.is_empty() && rest[ident.len()..].trim_start().starts_with(':') {
                    out.push(ident);
                }
            }
        }

        for c in code.chars() {
            match c {
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' => depth -= 1,
                _ => {}
            }
        }
        if depth <= 0 {
            break;
        }
    }
    out
}

/// The shared assertion, mirroring
/// `variant_ordinal_lock_test.rs::assert_frozen_prefix`: reports the first
/// divergence by ordinal and name.
fn assert_frozen_prefix(type_name: &str, actual: &[String], frozen: &[&str]) {
    assert!(
        actual.len() >= frozen.len(),
        "`{type_name}` (client/src/protocol/local_apps.rs) now has {} members, fewer than the \
         {} frozen at the 12.0.0 bless. A REMOVED member shifts every later ordinal on the \
         positional UniFFI mobile bindings and is a breaking change: it needs a MAJOR \
         CLIENT_PROTOCOL_VERSION bump and a re-bless of snapshots/blessed_major.txt.\n\
         Current order: {actual:?}",
        actual.len(),
        frozen.len(),
    );

    for (ordinal, expected) in frozen.iter().enumerate() {
        let found = actual[ordinal].as_str();
        assert_eq!(
            found,
            *expected,
            "`{type_name}` ordinal {ordinal} (UniFFI ordinal {uniffi}) is `{found}`, but \
             `{expected}` was frozen there at the 12.0.0 bless.\n\
             \n\
             UniFFI encodes a variant/field as its DECLARATION INDEX, so a member inserted, \
             removed or reordered ahead of ordinal {ordinal} shifts this and every later member. \
             An old mobile client and a new host would both still advertise \
             CLIENT_PROTOCOL_VERSION 12.0.0, the handshake would accept, and `{expected}` would \
             decode as `{found}`.\n\
             \n\
             FIX: move the new member to the END of `{type_name}` in \
             client/src/protocol/local_apps.rs. Appending is additive and needs no bump. Only \
             if it truly cannot go last is this a MAJOR CLIENT_PROTOCOL_VERSION bump plus a \
             re-bless of snapshots/blessed_major.txt — and then re-cut the frozen list in this \
             file from the new major.\n\
             \n\
             Full current order: {actual:?}",
            uniffi = ordinal + 1,
        );
    }
}

#[test]
fn app_record_dto_keeps_every_12_0_0_field_at_its_ordinal() {
    let actual = fields_in_declaration_order(LOCAL_APPS_SRC, "AppRecordDto");
    assert_frozen_prefix("AppRecordDto", &actual, APP_RECORD_DTO_FIELDS_AT_12_0_0);
}

#[test]
fn plugin_command_dto_keeps_every_12_0_0_variant_at_its_ordinal() {
    let actual = variants_in_declaration_order(LOCAL_APPS_SRC, "PluginCommandDto");
    assert_frozen_prefix(
        "PluginCommandDto",
        &actual,
        PLUGIN_COMMAND_DTO_ORDINALS_AT_12_0_0,
    );
}

#[test]
fn app_event_dto_keeps_every_12_0_0_variant_at_its_ordinal() {
    let actual = variants_in_declaration_order(LOCAL_APPS_SRC, "AppEventDto");
    assert_frozen_prefix("AppEventDto", &actual, APP_EVENT_DTO_ORDINALS_AT_12_0_0);
}

#[test]
fn local_app_verification_status_dto_keeps_every_12_0_0_variant_at_its_ordinal() {
    let actual = variants_in_declaration_order(LOCAL_APPS_SRC, "LocalAppVerificationStatusDto");
    assert_frozen_prefix(
        "LocalAppVerificationStatusDto",
        &actual,
        LOCAL_APP_VERIFICATION_STATUS_DTO_ORDINALS_AT_12_0_0,
    );
}

#[test]
fn create_confirmation_request_dto_keeps_every_12_0_0_field_at_its_ordinal() {
    let actual =
        fields_in_declaration_order(LOCAL_APPS_SRC, "LocalAppCreateConfirmationRequestDto");
    assert_frozen_prefix(
        "LocalAppCreateConfirmationRequestDto",
        &actual,
        LOCAL_APP_CREATE_CONFIRMATION_REQUEST_DTO_FIELDS_AT_12_0_0,
    );
}

#[test]
fn mcp_proposal_approval_request_dto_keeps_every_12_0_0_field_at_its_ordinal() {
    let actual =
        fields_in_declaration_order(LOCAL_APPS_SRC, "LocalAppMcpProposalApprovalRequestDto");
    assert_frozen_prefix(
        "LocalAppMcpProposalApprovalRequestDto",
        &actual,
        LOCAL_APP_MCP_PROPOSAL_APPROVAL_REQUEST_DTO_FIELDS_AT_12_0_0,
    );
}

/// Proof the three locks above are not VACUOUS — i.e. that they read the real
/// `src/local_apps.rs` and would actually fire, rather than only proving the
/// parsers work on synthetic strings (which the two tests below already do).
///
/// Both mutations are applied to a COPY of the real source and each is
/// guarded by an `assert_ne!`: a needle that silently stops matching would
/// otherwise turn this proof into a no-op that keeps passing.
#[test]
fn the_local_app_ordinal_locks_fire_against_a_mutated_copy_of_the_real_source() {
    // (a) REORDER — swap the last two variants of the real enum.
    let swapped = LOCAL_APPS_SRC.replacen(
        "    Failed,\n    Unverified,\n    Unavailable,\n}",
        "    Failed,\n    Unavailable,\n    Unverified,\n}",
        1,
    );
    assert!(
        swapped != LOCAL_APPS_SRC,
        "the reorder mutation did not apply: `LocalAppVerificationStatusDto`'s tail no longer \
         reads `Failed, Unverified, Unavailable`, so this proof would be silently vacuous"
    );
    let actual = variants_in_declaration_order(&swapped, "LocalAppVerificationStatusDto");
    assert_eq!(
        actual,
        vec!["Pending", "Passed", "Failed", "Unavailable", "Unverified"],
        "the mutated copy must parse with the swap in place"
    );
    let message = capture_frozen_prefix_failure(
        "LocalAppVerificationStatusDto",
        &actual,
        LOCAL_APP_VERIFICATION_STATUS_DTO_ORDINALS_AT_12_0_0,
    );
    assert!(
        message.contains("ordinal 3")
            && message.contains("`Unavailable`")
            && message.contains("`Unverified`"),
        "the reorder failure must name the ordinal and both variants; got: {message}"
    );

    // (b) REMOVAL — drop the LAST field of the real create-confirmation
    // record. `assert_frozen_prefix`'s length guard must catch this by name
    // rather than panicking on an out-of-range index.
    let header = "pub struct LocalAppCreateConfirmationRequestDto {";
    let start = LOCAL_APPS_SRC
        .find(header)
        .expect("`LocalAppCreateConfirmationRequestDto` must exist in src/local_apps.rs");
    let end = start
        + LOCAL_APPS_SRC[start..]
            .find("\n}\n")
            .expect("the record body must terminate");
    let body = &LOCAL_APPS_SRC[start..end];
    // No trailing `\n` in the needle: `body` was cut at the record's closing
    // brace, so the last field line has no newline after it. Only the `pub`
    // line is removed; the `#[serde(...)]` above it may dangle, which
    // `fields_in_declaration_order` ignores because it counts `pub ` lines.
    //
    // Re-cut once already: this proof used to mutate `receipt`, which
    // r1-backlog-native-confirmation-13 removed. `required_gates` is the last
    // field now.
    let trimmed = body.replace("\n    pub required_gates: Vec<LocalAppGateStatusDto>,", "");
    assert!(
        trimmed != body,
        "the removal mutation did not apply: `pub required_gates: Vec<LocalAppGateStatusDto>,` is \
         no longer the last field of `LocalAppCreateConfirmationRequestDto`. If it was removed or \
         something was appended after it, drop/replace it in \
         LOCAL_APP_CREATE_CONFIRMATION_REQUEST_DTO_FIELDS_AT_12_0_0 and re-cut this proof against \
         the new last field — do not leave it matching nothing"
    );
    let mutated = format!("{trimmed}\n}}\n");
    let actual = fields_in_declaration_order(&mutated, "LocalAppCreateConfirmationRequestDto");
    assert_eq!(
        actual.len(),
        LOCAL_APP_CREATE_CONFIRMATION_REQUEST_DTO_FIELDS_AT_12_0_0.len() - 1,
        "the mutated copy must be exactly one field shorter: {actual:?}"
    );
    let message = capture_frozen_prefix_failure(
        "LocalAppCreateConfirmationRequestDto",
        &actual,
        LOCAL_APP_CREATE_CONFIRMATION_REQUEST_DTO_FIELDS_AT_12_0_0,
    );
    assert!(
        message.contains("fewer than the")
            && message.contains("LocalAppCreateConfirmationRequestDto"),
        "removing the last field must be reported as a member count shortfall, by type name; \
         got: {message}"
    );
}

/// Run `assert_frozen_prefix` expecting it to panic, and return its message.
fn capture_frozen_prefix_failure(type_name: &str, actual: &[String], frozen: &[&str]) -> String {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_frozen_prefix(type_name, actual, frozen);
    }));
    std::panic::set_hook(previous_hook);
    let failure = failure.expect_err("`assert_frozen_prefix` must reject this input");
    failure
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| failure.downcast_ref::<&str>().copied())
        .unwrap_or("")
        .to_string()
}

/// Proof this gate can go red: feed the field parser a synthetic struct with
/// a field spliced into the middle and confirm the assertion fires and names
/// the ordinal.
#[test]
fn a_mid_struct_field_insertion_is_detected() {
    const FROZEN: &[&str] = &["alpha", "beta", "gamma"];

    let good = "pub struct Synthetic {\n    pub alpha: String,\n    /// a brace in prose: {x, y}\n    pub beta: u64,\n    pub gamma: bool,\n    pub delta: Vec<Other>,\n}\n";
    let parsed = fields_in_declaration_order(good, "Synthetic");
    assert_eq!(
        parsed,
        vec!["alpha", "beta", "gamma", "delta"],
        "appending `delta` must parse as ordinal 3"
    );
    assert_frozen_prefix("Synthetic", &parsed, FROZEN);

    let spliced = "pub struct Synthetic {\n    pub alpha: String,\n    pub inserted: u64,\n    pub beta: u64,\n    pub gamma: bool,\n}\n";
    let parsed = fields_in_declaration_order(spliced, "Synthetic");
    assert_eq!(parsed, vec!["alpha", "inserted", "beta", "gamma"]);

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let failure = std::panic::catch_unwind(|| {
        assert_frozen_prefix("Synthetic", &parsed, FROZEN);
    });
    std::panic::set_hook(previous_hook);
    let failure = failure.expect_err("a mid-struct field insertion must fail assert_frozen_prefix");
    let message = failure
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| failure.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(
        message.contains("ordinal 1") && message.contains("inserted") && message.contains("beta"),
        "the failure must name the ordinal and both fields; got: {message}"
    );
}

/// Same proof for the enum-variant parser, reusing the shape already
/// validated in `variant_ordinal_lock_test.rs::a_mid_enum_insertion_is_detected`
/// but against this file's own copy of the parser so a regression here is
/// caught locally rather than only in the sibling file.
#[test]
fn a_mid_enum_variant_insertion_is_detected() {
    const FROZEN: &[&str] = &["Alpha", "Beta", "Gamma"];

    let spliced = "pub enum Synthetic {\n    Alpha { a: u64 },\n\n    Inserted { i: u64 },\n\n    Beta {\n        b: u64,\n    },\n\n    Gamma,\n}\n";
    let parsed = variants_in_declaration_order(spliced, "Synthetic");
    assert_eq!(parsed, vec!["Alpha", "Inserted", "Beta", "Gamma"]);

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let failure = std::panic::catch_unwind(|| {
        assert_frozen_prefix("Synthetic", &parsed, FROZEN);
    });
    std::panic::set_hook(previous_hook);
    let failure = failure.expect_err("a mid-enum insertion must fail assert_frozen_prefix");
    let message = failure
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| failure.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(
        message.contains("ordinal 1") && message.contains("Inserted") && message.contains("Beta"),
        "the failure must name the ordinal and both variants; got: {message}"
    );
}
