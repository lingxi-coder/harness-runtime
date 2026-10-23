//! Positional variant locks for the current Client protocol 29.0.0 baseline.
//!
//! UniFFI encodes each enum variant by declaration index, so inserting, removing,
//! or reordering a variant can change the mobile ABI even when JSON tags remain
//! stable. The arrays below retain the full enum prefix frozen at protocol 27.0.0.
//! Protocol 28 added a required field and protocol 29 changed a field carrier;
//! both leave all enum declarations in place, so the positional ABI prefix stays
//! unchanged. A later append remains
//! compatible with this frozen prefix; any ordinal change requires a deliberate
//! major-version re-cut. The wire contract major is separately recorded in
//! snapshots/blessed_major.txt (currently 29).

const COMMANDS_SRC: &str = include_str!("../src/protocol/commands.rs");
const EVENTS_SRC: &str = include_str!("../src/protocol/events.rs");

const CLIENT_COMMAND_ORDINALS_AT_27_0_0: &[&str] = &[
    "StartRealtimeAudio",
    "RealtimeAudioInput",
    "StopRealtimeAudio",
    "GetAudioSessionContext",
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
    "RequestExit",
    "GetConversationControls",
    "SetReasoningSelection",
    "SetFastMode",
    "SkillAdmin",
    "McpAdmin",
    "PluginAdmin",
    "HookAdmin",
    "UpdateSettings",
    "UpdatePermissionRules",
    "SetDefaultPermissionMode",
    "UpdateWorkspaceDirectories",
    "UpsertMcpServer",
    "RemoveMcpServer",
    "AudioResponse",
    "AttachTurn",
    "ResumeTurn",
    "PauseTurn",
    "SetTypescriptLspMode",
    "ForkSession",
    "TestProviderConnection",
    "CronManage",
    "TaskMessage",
    "CronRunStarted",
    "ScheduledRunTurn",
    "CronRunCompleted",
    "UpdateAudioCapabilities",
    "UiAttach",
    "UiDetach",
    "UiRender",
    "UiClientModule",
    "UiMessage",
    "UiClientFault",
    "UiClientPress",
    "UiPress",
    "UiInput",
    "UiSelect",
    "UiClientOperation",
];

const CLIENT_EVENT_ORDINALS_AT_27_0_0: &[&str] = &[
    "RealtimeAudioEvent",
    "AudioSessionContext",
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
    "SessionAgentTombstone",
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
    "AudioRequest",
    "TurnRecoveryState",
    "TurnEventReplay",
    "Skills",
    "TypescriptLspModeChanged",
    "SessionForked",
    "ProviderConnectionTested",
    "ConfigurationOperation",
    "SkillCatalog",
    "SkillDocument",
    "McpConfigurationSnapshot",
    "PluginCatalog",
    "ProviderModelCatalog",
    "CompactionStatus",
    "CronResult",
    "MessageRetracted",
    "MessageIdentity",
    "LoopWakeup",
    "TaskLifecycle",
    "OpenAiOAuthUpdated",
    "CronRunBound",
    "ScheduledRunFinished",
    "CronRunRequested",
    "ScheduledTaskFire",
    "TaskListComplete",
    "AudioCancel",
    "AudioCapabilitiesChanged",
    "QueryModelChange",
    "AssistantBlockStart",
    "AssistantBlockIdentity",
    "Tombstone",
    "RefusalContinuation",
    "UserTranscriptRowIdentity",
    "AssistantTranscriptRowUuids",
    "UiLog",
    "UiToast",
    "UiStatus",
    "UiControlResult",
    "UiClientFrame",
    "UiInvalidate",
    "TerminalApiError",
    "UiNotify",
    "VisualizationBlock",
];

/// Extract variant names from a literal enum used by parser tests, or from the
/// current `define_client_command!` / `define_client_event!` invocation.
fn variants_in_declaration_order(src: &str, enum_name: &str) -> Vec<String> {
    let file = syn::parse_file(src).expect("protocol source is valid Rust syntax");
    if let Some(item) = file.items.iter().find_map(|item| match item {
        syn::Item::Enum(item) if item.ident == enum_name => Some(item),
        _ => None,
    }) {
        return item
            .variants
            .iter()
            .map(|variant| variant.ident.to_string())
            .collect();
    }

    let macro_name = match enum_name {
        "ClientCommand" => "define_client_command",
        "ClientEvent" => "define_client_event",
        _ => panic!("no protocol macro registered for {enum_name}"),
    };
    let invocation = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Macro(item) if item.mac.path.is_ident(macro_name) => Some(&item.mac),
            _ => None,
        })
        .unwrap_or_else(|| panic!("`{macro_name}!` invocation not found"));
    let enum_source = format!("enum {enum_name} {{ {} }}", invocation.tokens);
    let item: syn::ItemEnum = syn::parse_str(&enum_source)
        .unwrap_or_else(|e| panic!("parse {enum_name} macro body: {e}"));
    item.variants
        .iter()
        .map(|variant| variant.ident.to_string())
        .collect()
}

/// Find the body opening in a direct enum fixture or protocol macro invocation.
fn variant_body_start(src: &str, enum_name: &str) -> usize {
    let macro_name = match enum_name {
        "ClientCommand" => Some("define_client_command"),
        "ClientEvent" => Some("define_client_event"),
        _ => None,
    };
    if let Some(macro_name) = macro_name {
        let opener = format!("{macro_name}! {{");
        if let Some(start) = src.find(&opener) {
            return start + opener.len();
        }
    }

    let header = format!("pub enum {enum_name} {{");
    src.find(&header)
        .unwrap_or_else(|| panic!("`{header}` not found in the source"))
        + header.len()
}

/// A deliberately independent source-line scan used to verify that the AST
/// parser did not omit a top-level variant from the literal enum or macro body.
fn variants_by_flat_scan(src: &str, enum_name: &str) -> Vec<String> {
    let start = variant_body_start(src, enum_name);

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
         27.0.0 bless. A REMOVED variant shifts every later ordinal on the positional UniFFI \
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
             `{expected}` was frozen there at the 27.0.0 bless.\n\
             \n\
             UniFFI encodes a variant as its DECLARATION INDEX, so a variant inserted, removed \
             or reordered ahead of ordinal {ordinal} shifts this and every later variant. An \
             old mobile client and a new host would both still advertise \
             CLIENT_PROTOCOL_VERSION 27.0.0, the handshake would accept, and `{expected}` would \
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
fn client_command_keeps_every_27_0_0_variant_at_its_ordinal() {
    let actual = variants_in_declaration_order(COMMANDS_SRC, "ClientCommand");
    assert_frozen_prefix(
        "ClientCommand",
        "client/src/protocol/commands.rs",
        &actual,
        CLIENT_COMMAND_ORDINALS_AT_27_0_0,
    );
}

#[test]
fn client_event_keeps_every_27_0_0_variant_at_its_ordinal() {
    let actual = variants_in_declaration_order(EVENTS_SRC, "ClientEvent");
    assert_frozen_prefix(
        "ClientEvent",
        "client/src/protocol/events.rs",
        &actual,
        CLIENT_EVENT_ORDINALS_AT_27_0_0,
    );
}

#[test]
fn side_chat_variants_are_appended_after_the_protocol_25_prefix() {
    let commands = variants_in_declaration_order(COMMANDS_SRC, "ClientCommand");
    let events = variants_in_declaration_order(EVENTS_SRC, "ClientEvent");
    assert!(commands.ends_with(&[
        "SideChatCreate".into(),
        "SideChatList".into(),
        "SideChatGet".into(),
        "SideChatEnd".into(),
        "SideChatSubmit".into(),
    ]));
    assert!(events.ends_with(&[
        "SideChatCreated".into(),
        "SideChatList".into(),
        "SideChatSnapshot".into(),
        "SideChatEvent".into(),
        "SideChatPermission".into(),
        "SideChatSubmitted".into(),
        "SideChatEnded".into(),
        "SideChatError".into(),
        "SessionStorageState".into(),
        "WorkspaceChanged".into(),
    ]));
}

/// Cross-check the AST parser against an independent scan of the original
/// macro invocation body, so a parser that omits a variant cannot pass by
/// agreeing only with the frozen prefix.
#[test]
fn the_parser_reads_the_whole_enum_body() {
    let commands = variants_in_declaration_order(COMMANDS_SRC, "ClientCommand");
    let events = variants_in_declaration_order(EVENTS_SRC, "ClientEvent");

    assert!(
        commands.len() >= CLIENT_COMMAND_ORDINALS_AT_27_0_0.len(),
        "parsed only {} ClientCommand variants",
        commands.len()
    );
    assert!(
        events.len() >= CLIENT_EVENT_ORDINALS_AT_27_0_0.len(),
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
