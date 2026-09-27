//! F1-09 companion — the POSITIONAL half of the version guard.
//!
//! # What this catches that `version_guard_test.rs` cannot
//!
//! `version_guard_test.rs` fingerprints the contract as an unordered map of
//! `"<enum>::<Variant>::<field>" -> "<type>"`. That map is blind to ORDER, so
//! `adding_a_variant_is_compatible` passes whether the new variant lands at the
//! end of the enum or is spliced into the middle of it. On the JSON wire that
//! really is the same thing — the variant is tagged by name (`#[serde(tag =
//! "type")]`), so position is irrelevant.
//!
//! It is NOT the same thing on mobile. `#[derive(uniffi::Enum)]` encodes a
//! variant as its 1-based DECLARATION INDEX; the generated Swift and Kotlin
//! read that integer and switch on it. Insert a variant in the middle and every
//! variant after it silently shifts. A client built against the old ordinals
//! and a host built against the new ones both still advertise the same
//! `CLIENT_PROTOCOL_VERSION`, the handshake accepts, and then the host decodes
//! one command as an entirely different command. That is the exact rule
//! `version.rs` writes down for 9.0.0: "The wire JSON is additive, but the
//! mobile bindings are positional and must version-lock with the host."
//!
//! This is not hypothetical. On 2026-08-28, `AttachTurn` / `ResumeTurn` /
//! `PauseTurn` were inserted at `ClientCommand` ordinals 2/3/4 (between
//! `Cancel` and `ApprovePermission`), `TurnRecoveryState` / `TurnEventReplay`
//! at `ClientEvent` ordinals 12/13, and `Skills` at `ClientEvent` ordinal 29 —
//! shifting 48 and 45 pre-existing variants respectively — with
//! `CLIENT_PROTOCOL_VERSION` left at `9.0.0` and `snapshots/blessed_major.txt`
//! left at `9`. Every gate in the repo was green.
//!
//! # The rule this enforces
//!
//! The variants frozen below are the ones that existed when 9.0.0 was blessed
//! (commit `391d89fff`, `client-protocol/src/{commands,events}.rs`). Each must
//! still sit at exactly the index recorded here.
//!
//! - Appending a variant AFTER the frozen prefix: allowed, no bump. Do not add
//!   it to the frozen lists — they are the 9.0.0 snapshot, not a mirror of the
//!   current enum.
//! - Inserting, removing, or reordering anything inside the frozen prefix: this
//!   test goes red and NAMES the ordinal. Move the variant to the end instead.
//!   If it genuinely cannot go at the end, that is a MAJOR
//!   `CLIENT_PROTOCOL_VERSION` bump plus a re-bless of
//!   `snapshots/blessed_major.txt` — and the frozen lists here are then
//!   re-cut from the new major.
//!
//! # Why it parses the source instead of reading UniFFI metadata
//!
//! The metadata constants only exist under `--features uniffi`, which the
//! desktop gates never enable — that blindness is what let the 2026-08-27
//! metadata overflow and this ordinal shift both land. Declaration order in the
//! source IS what the derive reads, so this file reads the same thing, and it
//! runs under default features on every `cargo test -p client-protocol`.

const COMMANDS_SRC: &str = include_str!("../src/commands.rs");
const EVENTS_SRC: &str = include_str!("../src/events.rs");

/// `ClientCommand`'s variants in declaration order as of the 9.0.0 bless
/// (`391d89fff`), MINUS one row. 49 entries; ordinal N here is UniFFI ordinal
/// N+1.
///
/// 🚨 PROVENANCE, stated as fact rather than as policy: the 9.0.0 bless froze
/// 50 entries, with `ResolveAppRuntimeProfileSelection` at ordinal 38. Commit
/// `77d1ec7fb` deleted that one row from this list. It was NOT a re-cut: at
/// `77d1ec7fb^` and at `77d1ec7fb` alike `CLIENT_PROTOCOL_VERSION` reads
/// `"10.0.0"` and `snapshots/blessed_major.txt` reads `10`, so nothing was
/// bumped and nothing was re-blessed. Per this file's own rule above, dropping
/// a row from the frozen prefix is a MAJOR bump plus a re-bless, and a re-cut
/// then takes the FULL current order — not the old list minus a row. So this
/// 49-entry list is a hybrid that has never been sanctioned; do not read it as
/// precedent, and do not shrink it further to turn a red run green. Resolving
/// it (restore ordinal 38, or re-cut both lists from the blessed 11.0.0 and
/// rename them `_AT_11_0_0`) belongs with the owner of
/// `client-protocol/src/{commands,events}.rs`.
const CLIENT_COMMAND_ORDINALS_AT_9_0_0: &[&str] = &[
    "SendPrompt",
    "Cancel",
    "ApprovePermission",
    "DenyPermission",
    "ApproveComputerAccess",
    "DenyComputerAccess",
    "AnswerAskUserQuestion",
    "CancelAskUserQuestion",
    "SetPermissionMode",
    "ListProviderCredentials",
    "SetProviderCredential",
    "DeleteProviderCredential",
    "SetModel",
    "ListModels",
    "RunSlashCommand",
    "RefreshListings",
    "ListSessionAgents",
    "LoadSessionAgentTranscript",
    "NewSession",
    "ResumeSession",
    "ListSessions",
    "Login",
    "Logout",
    "ForceCompact",
    "ClearSession",
    "TaskList",
    "TaskOutput",
    "TaskStop",
    "ResumeWorkflow",
    "ListApps",
    "GetAppDetails",
    "CreateApp",
    "StartApp",
    "StopApp",
    "RestartApp",
    "ExecuteAppBridgeRequest",
    "ResolveAppUiRequest",
    "ResolveAppCapabilityRequest",
    "ResolveAppProfileProposal",
    "ResetAppPermissions",
    "ListAppSessions",
    "ListAppCheckpoints",
    "RestoreAppCheckpoint",
    "DeleteApp",
    "RequestExit",
    "GetConversationControls",
    "SetReasoningSelection",
    "SetFastMode",
    "ResolveAppDependencyChangeConfirmation",
];

/// `ClientEvent`'s variants in declaration order as of the 9.0.0 bless
/// (`391d89fff`). 57 entries; ordinal N here is UniFFI ordinal N+1.
const CLIENT_EVENT_ORDINALS_AT_9_0_0: &[&str] = &[
    "Error",
    "SystemNotice",
    "AskUserQuestion",
    "AskUserQuestionResolved",
    "PermissionRequestResolved",
    "TextDelta",
    "ToolUseStarted",
    "ToolHeartbeat",
    "ToolUseResult",
    "MessageComplete",
    "TurnStarted",
    "TurnEnded",
    "CostUpdate",
    "CompactionCompleted",
    "SessionStarted",
    "SessionEnded",
    "SessionResumed",
    "SessionAgentList",
    "SessionAgentTranscript",
    "SessionAgentUpdated",
    "SessionAgentMessage",
    "SessionList",
    "ModelList",
    "ModelChanged",
    "PermissionModeChanged",
    "ProviderCredentialStatus",
    "McpServers",
    "Hooks",
    "Agents",
    "SlashCommandCatalog",
    "SlashCommandResult",
    "MemoryEntries",
    "StatusSnapshot",
    "SettingsSnapshot",
    "AuthState",
    "DoctorReport",
    "TaskRow",
    "TaskOutputChunk",
    "TaskStatusChanged",
    "CommandsChanged",
    "AppsChanged",
    "AppEvent",
    "AppWorkflowChanged",
    "AppRuntimeChanged",
    "AppSessionsChanged",
    "AppCheckpointCreated",
    "AppOperationFailed",
    "CoordinatorStatus",
    "CoordinatorWorker",
    "ThinkingDelta",
    "UsageUpdate",
    "Attachment",
    "ApiRetry",
    "PlanUpdated",
    "WorkflowResumed",
    "ConversationControlsChanged",
    "FastModeChanged",
];

/// Strip `//`-comments so brace counting is not thrown off by prose. The enum
/// bodies here really do contain braces inside comments (`{media_type,
/// base64}`), and a miscount would silently truncate the variant list — a
/// parser that under-reads is a parser that passes for the wrong reason.
fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(at) => &line[..at],
        None => line,
    }
}

/// Extract the top-level variant identifiers of `enum <name>` from Rust source,
/// in declaration order.
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

/// A SECOND, deliberately different reading of the same enum body, used only by
/// `the_parser_reads_the_whole_enum_body` to catch a truncating
/// `variants_in_declaration_order`.
///
/// It shares no logic with that function: instead of tracking brace depth it
/// takes everything between the enum header and the first column-0 `}`, and
/// treats a line indented by exactly four spaces and starting with an uppercase
/// ASCII letter as a variant. `rustfmt` puts every top-level variant there and
/// nothing else (fields sit at eight, doc comments start with `/`, attributes
/// with `#`). A depth bug — an unbalanced brace inside a string or a comment —
/// truncates the depth parser without touching this one, so the two disagree.
fn variants_by_flat_scan(src: &str, enum_name: &str) -> Vec<String> {
    let header = format!("pub enum {enum_name} {{");
    let start = src
        .find(&header)
        .unwrap_or_else(|| panic!("`{header}` not found in the source"))
        + header.len();

    let mut out = Vec::new();
    for line in src[start..].lines() {
        if line == "}" {
            break;
        }
        let Some(rest) = line.strip_prefix("    ") else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_uppercase()) {
            continue;
        }
        let ident: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let tail = rest[ident.len()..].trim_start();
        if tail.starts_with('{')
            || tail.starts_with('(')
            || tail.starts_with(',')
            || tail.is_empty()
        {
            out.push(ident);
        }
    }
    out
}

/// The shared assertion. Reports the first divergence BY ORDINAL AND NAME, so a
/// red run says which variant moved and what now occupies its slot — never just
/// "not equal".
fn assert_frozen_prefix(enum_name: &str, source_path: &str, actual: &[String], frozen: &[&str]) {
    assert!(
        actual.len() >= frozen.len(),
        "`{enum_name}` ({source_path}) now has {} variants, fewer than the {} frozen at the \
         9.0.0 bless. A REMOVED variant shifts every later ordinal on the positional UniFFI \
         mobile bindings and is a breaking change: it needs a MAJOR CLIENT_PROTOCOL_VERSION \
         bump and a re-bless of snapshots/blessed_major.txt.\nCurrent order: {actual:?}",
        actual.len(),
        frozen.len(),
    );

    for (ordinal, expected) in frozen.iter().enumerate() {
        let found = actual[ordinal].as_str();
        assert_eq!(
            found,
            *expected,
            "`{enum_name}` ordinal {ordinal} (UniFFI ordinal {uniffi}) is `{found}`, but \
             `{expected}` was frozen there at the 9.0.0 bless.\n\
             \n\
             UniFFI encodes a variant as its DECLARATION INDEX, so a variant inserted, removed \
             or reordered ahead of ordinal {ordinal} shifts this and every later variant. An \
             old mobile client and a new host would both still advertise \
             CLIENT_PROTOCOL_VERSION 9.0.0, the handshake would accept, and `{expected}` would \
             decode as `{found}`.\n\
             \n\
             FIX: move the new variant to the END of `{enum_name}` in {source_path}. Appending \
             is additive and needs no bump. Only if it truly cannot go last is this a MAJOR \
             CLIENT_PROTOCOL_VERSION bump plus a re-bless of snapshots/blessed_major.txt — and \
             then re-cut the frozen list in this file from the new major.\n\
             \n\
             Full current order: {actual:?}",
            uniffi = ordinal + 1,
        );
    }
}

#[test]
fn client_command_keeps_every_9_0_0_variant_at_its_ordinal() {
    let actual = variants_in_declaration_order(COMMANDS_SRC, "ClientCommand");
    assert_frozen_prefix(
        "ClientCommand",
        "client-protocol/src/commands.rs",
        &actual,
        CLIENT_COMMAND_ORDINALS_AT_9_0_0,
    );
}

#[test]
fn client_event_keeps_every_9_0_0_variant_at_its_ordinal() {
    let actual = variants_in_declaration_order(EVENTS_SRC, "ClientEvent");
    assert_frozen_prefix(
        "ClientEvent",
        "client-protocol/src/events.rs",
        &actual,
        CLIENT_EVENT_ORDINALS_AT_9_0_0,
    );
}

/// The parser is the gate. If it silently under-reads — stopping early on a
/// brace inside a comment, or skipping a variant shape it does not recognise —
/// both tests above would pass while reading a truncated list. Pin it against
/// the real files by count, so a truncating parser is a red run rather than a
/// green one.
#[test]
fn the_parser_reads_the_whole_enum_body() {
    let commands = variants_in_declaration_order(COMMANDS_SRC, "ClientCommand");
    let events = variants_in_declaration_order(EVENTS_SRC, "ClientEvent");

    assert!(
        commands.len() >= CLIENT_COMMAND_ORDINALS_AT_9_0_0.len(),
        "parsed only {} ClientCommand variants",
        commands.len()
    );
    assert!(
        events.len() >= CLIENT_EVENT_ORDINALS_AT_9_0_0.len(),
        "parsed only {} ClientEvent variants",
        events.len()
    );

    // The TAIL of each enum, established by a second, independent reading of
    // the same source (see `variants_by_flat_scan`) rather than by a hardcoded
    // name: a hardcoded name silently stops covering the tail as soon as
    // anything is appended past it, which is precisely where every new variant
    // lands. The two readers disagree exactly when the depth-tracking parser
    // truncates.
    for (enum_name, src, parsed) in [
        ("ClientCommand", COMMANDS_SRC, &commands),
        ("ClientEvent", EVENTS_SRC, &events),
    ] {
        let flat = variants_by_flat_scan(src, enum_name);
        assert_eq!(
            parsed.len(),
            flat.len(),
            "the depth-tracking parser read {} `{enum_name}` variants but the independent \
             flat scan of {} reads {}. A truncating parser makes the ordinal locks above \
             pass on a short list.\nparser: {parsed:?}\nflat:   {flat:?}",
            parsed.len(),
            enum_name,
            flat.len(),
        );
        assert_eq!(
            parsed.last(),
            flat.last(),
            "the depth-tracking parser ends `{enum_name}` at {:?} but the independent flat \
             scan ends it at {:?}",
            parsed.last(),
            flat.last(),
        );
    }
}

/// Proof that the gate can go red: feed the parser a synthetic enum with a
/// variant spliced into the middle and confirm the assertion fires and names
/// the ordinal. Without this, a parser that returned the frozen list verbatim
/// would look identical to a working gate.
#[test]
fn a_mid_enum_insertion_is_detected() {
    const FROZEN: &[&str] = &["Alpha", "Beta", "Gamma"];

    let good = "pub enum Synthetic {\n    Alpha { a: u64 },\n\n    Beta {\n        // a brace in prose: {x, y}\n        b: u64,\n    },\n\n    Gamma,\n\n    Delta { d: u64 },\n}\n";
    let parsed = variants_in_declaration_order(good, "Synthetic");
    assert_eq!(
        parsed,
        vec!["Alpha", "Beta", "Gamma", "Delta"],
        "appending `Delta` must parse as ordinal 3"
    );
    assert_frozen_prefix("Synthetic", "synthetic.rs", &parsed, FROZEN);

    let spliced = "pub enum Synthetic {\n    Alpha { a: u64 },\n\n    Inserted { i: u64 },\n\n    Beta {\n        b: u64,\n    },\n\n    Gamma,\n}\n";
    let parsed = variants_in_declaration_order(spliced, "Synthetic");
    assert_eq!(parsed, vec!["Alpha", "Inserted", "Beta", "Gamma"]);

    // The deliberate panic below is expected; keep it off stderr so a PASSING
    // run does not print a scary backtrace that reads like a real failure.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let failure = std::panic::catch_unwind(|| {
        assert_frozen_prefix("Synthetic", "synthetic.rs", &parsed, FROZEN);
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
