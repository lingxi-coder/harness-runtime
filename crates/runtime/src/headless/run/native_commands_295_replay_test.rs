//! Regression replay for the separately captured Native 2.1.295 command/Skill fixture.
//! The version-labeled 2.1.293 baseline remains in its own fixture directory.
//! The 2.1.293 baseline remains in its own version-labeled fixture directory.

use super::*;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::SessionId;
use orchestrator::prompt::{LingxiMdTier, MemoryFile};
use orchestrator::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockStreamingApiClient,
    NoOpPermissionGate, StaticMemoryProvider,
};
use platform_posix::fs::PosixFileSystem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const COMMAND_FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/native_commands_295"
);
const NATIVE_295_SHA256: &str = "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70";
const NATIVE_295_RECEIPT_SHA256: &str =
    "352550db4b9c77d45392e246c5a09ef4acc3396c1cd2621cc0643ad5afabba08";
const NATIVE_295_MANIFEST_SHA256: &str =
    "d933facf6652cb733417d965268d50c6c9ea331b8ca8b2436cb77082dbf0039a";
// Current host user-skill snapshot: its full description matches Native's raw
// listing, but the capture manifest did not record the source path or hash.
const GRAPHIFY_USER_SKILL_SNAPSHOT_SHA256: &str =
    "a8c19806704b002d1b588c1cc89ade9e97453c622e157c57f9617569185f1ad2";
const NATIVE_GRAPHIFY_SKILL_DESCRIPTION: &str = "any input (code, docs, papers, images) → knowledge graph → clustered communities → HTML + JSON + audit report";
const NATIVE_UNKNOWN_COMMAND_NAMES: &[&str] = &[
    "graphify",
    "dataviz",
    "update-config",
    "code-review",
    "simplify",
    "fewer-permission-prompts",
    "loop",
    "claude-api",
    "workflow-authoring",
    "run",
    "plugin-authoring",
    "init",
    "security-review",
];
const NATIVE_UNKNOWN_ATTACHMENT_ORDER: &[&str] = &[
    "environment",
    "model",
    "agent_listing_delta",
    "skill_listing",
    "total_tokens_reminder",
    "budget_usd",
    "unknown_command_fallback",
    "instructions",
    "session_context",
    "date",
    "remote_session_change",
    "prompt_snapshot",
    "prompt_snapshot",
];

struct FixtureCase {
    id: &'static str,
    session_id: &'static str,
    prompt: &'static str,
    expected_outbound_texts: &'static [&'static str],
    persisted_command_text: Option<&'static str>,
    native_input_sha256: &'static str,
    native_request_sha256: &'static str,
    native_session_sha256: &'static str,
    native_response_sha256: &'static str,
    unknown_command_fallback: bool,
    skill_name: Option<&'static str>,
    skill_input_sha256: Option<&'static str>,
    user_skill_name: Option<&'static str>,
    user_skill_fixture_sha256: Option<&'static str>,
}

const CASES: &[FixtureCase] = &[
    FixtureCase {
        id: "slash-only",
        session_id: "00000000-0000-4000-8000-000000002951",
        prompt: "/",
        expected_outbound_texts: &["/"],
        persisted_command_text: None,
        native_input_sha256: "e8dc01dd75f8c75df532779efd158557014858f1de4418bfe78dc3836d1e2057",
        native_request_sha256: "31ba833cfe2a882d05482e6bf8713a3f84bde6d8b14b8e57bb869264d3c54752",
        native_session_sha256: "5da3a68ff37f296d60b980342a4997e9552ddbc76b60430d639f0b3098d548f8",
        native_response_sha256: "522f9a30e8842f3f4c339bb73a798d09935e6cb8b449c56ceec6714f2ef36665",
        unknown_command_fallback: false,
        skill_name: None,
        skill_input_sha256: None,
        user_skill_name: None,
        user_skill_fixture_sha256: None,
    },
    FixtureCase {
        id: "unknown-slash-command",
        session_id: "00000000-0000-4000-8000-000000002952",
        prompt: "/__native_unknown_295__ fixed-argument",
        expected_outbound_texts: &["/__native_unknown_295__ fixed-argument"],
        persisted_command_text: None,
        native_input_sha256: "ba77436293ed3c86e385342de26bc6acca7845d592e67adfd29f8ca3db8b09e4",
        native_request_sha256: "9a4ec3bbb1db85404cb0f481c3136da6602b056788fe259d80746a3822d88a7c",
        native_session_sha256: "42bb4d54e8ee1b718b2f9b06f6552be9f3dada7c14316fc68271273007e9f276",
        native_response_sha256: "522f9a30e8842f3f4c339bb73a798d09935e6cb8b449c56ceec6714f2ef36665",
        unknown_command_fallback: true,
        skill_name: None,
        skill_input_sha256: None,
        user_skill_name: Some("graphify"),
        user_skill_fixture_sha256: Some(GRAPHIFY_USER_SKILL_SNAPSHOT_SHA256),
    },
    FixtureCase {
        id: "unicode-skill-name",
        session_id: "00000000-0000-4000-8000-000000002953",
        prompt: "/skill-🧪-probe",
        expected_outbound_texts: &[
            "<command-message>skill-🧪-probe</command-message>\n<command-name>/skill-🧪-probe</command-name>\n",
        ],
        persisted_command_text: Some(
            "<command-message>skill-🧪-probe</command-message>\n<command-name>/skill-🧪-probe</command-name>",
        ),
        native_input_sha256: "4ef2b684faad3dc3d78a4a441d064f1292822aefc4dc04d5ea0023cc3ee7ad20",
        native_request_sha256: "626534f7959a9f43a711f302b5c97bd0b95bca536e8729be51f8aa7a2972a7c0",
        native_session_sha256: "ea797f2a54b944833e60bbb1bcf8c894d0097442ef2a24d82288f0ef89bd3fbb",
        native_response_sha256: "522f9a30e8842f3f4c339bb73a798d09935e6cb8b449c56ceec6714f2ef36665",
        unknown_command_fallback: false,
        skill_name: Some("skill-🧪-probe"),
        skill_input_sha256: Some(
            "8b0cb4a4180c9a2f6681a035718a462e8d97acfab63113a77c7d231ffd34151f",
        ),
        user_skill_name: None,
        user_skill_fixture_sha256: None,
    },
    FixtureCase {
        id: "punctuation-skill-name",
        session_id: "00000000-0000-4000-8000-000000002954",
        prompt: "/skill.alpha+beta",
        expected_outbound_texts: &[
            "<command-message>skill.alpha+beta</command-message>\n<command-name>/skill.alpha+beta</command-name>\n",
        ],
        persisted_command_text: Some(
            "<command-message>skill.alpha+beta</command-message>\n<command-name>/skill.alpha+beta</command-name>",
        ),
        native_input_sha256: "347fdd02b948c00d0698487a86565cf92fdbf5e4dd9e4bf778f8d5f8683bff6d",
        native_request_sha256: "fc92636072174a971ab06178a8dd528875cc44507166bb2e687001fa0a00fb86",
        native_session_sha256: "3b166395684a0e5b1267daabde211d14a9ab43e8e172ac5835a7433e45da17ae",
        native_response_sha256: "522f9a30e8842f3f4c339bb73a798d09935e6cb8b449c56ceec6714f2ef36665",
        unknown_command_fallback: false,
        skill_name: Some("skill.alpha+beta"),
        skill_input_sha256: Some(
            "15aec457a133adfe88aaa44eeb01592b33a53c4800adcb251cf8ff962f701071",
        ),
        user_skill_name: None,
        user_skill_fixture_sha256: None,
    },
];

fn fixture_root() -> PathBuf {
    PathBuf::from(COMMAND_FIXTURES)
}

fn exact_text_needle(text: &str) -> Vec<u8> {
    let encoded = serde_json::to_string(text).expect("fixture text is valid UTF-8");
    format!("\"text\":{encoded}").into_bytes()
}

fn exact_json_string_needle(text: &str) -> Vec<u8> {
    serde_json::to_string(text)
        .expect("fixture text is valid UTF-8")
        .into_bytes()
}

fn native_user_text_blocks(raw: &[u8]) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_slice(raw).expect("parse raw Native request");
    value["messages"][0]["content"]
        .as_array()
        .expect("Native first request message has content blocks")
        .iter()
        .map(|block| {
            block["text"]
                .as_str()
                .expect("Native request content block has text")
                .to_owned()
        })
        .collect()
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn sha256(raw: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(raw))
}

fn row_has_text_units(row: &Utf16JsonProjection, expected: &[u16]) -> bool {
    if row.string_units("/message/content").as_deref() == Some(expected) {
        return true;
    }
    let Some(length) = row.array_len("/message/content") else {
        return false;
    };
    (0..length).any(|index| {
        row.string_units(&format!("/message/content/{index}/text"))
            .as_deref()
            == Some(expected)
    })
}

fn transcript_prompt_index(raw: &[u8], prompt: &str) -> Option<usize> {
    let Ok(source) = std::str::from_utf8(raw) else {
        return None;
    };
    let expected: Vec<u16> = prompt.encode_utf16().collect();
    source.lines().enumerate().find_map(|(index, line)| {
        let Ok(row) = Utf16JsonProjection::parse(line) else {
            return None;
        };
        (row.string_at("/type") == Some("user") && row_has_text_units(&row, &expected))
            .then_some(index)
    })
}

fn transcript_type_index(raw: &[u8], message_type: &str) -> Option<usize> {
    let source = std::str::from_utf8(raw).ok()?;
    source.lines().enumerate().find_map(|(index, line)| {
        let row = Utf16JsonProjection::parse(line).ok()?;
        (row.string_at("/type") == Some(message_type)).then_some(index)
    })
}

fn parsed_transcript_rows(raw: &[u8]) -> Vec<Utf16JsonProjection> {
    let Ok(source) = std::str::from_utf8(raw) else {
        return Vec::new();
    };
    source
        .lines()
        .filter_map(|line| Utf16JsonProjection::parse(line).ok())
        .collect()
}

fn transcript_attachment_types(raw: &[u8]) -> Vec<String> {
    parsed_transcript_rows(raw)
        .into_iter()
        .filter(|row| row.string_at("/type") == Some("attachment"))
        .filter_map(|row| row.string_at("/attachment/type").map(str::to_owned))
        .collect()
}

fn assert_attachment_subsequence(actual: &[String], expected: &[&str], label: &str) {
    let mut cursor = 0;
    for expected_type in expected {
        let offset = actual[cursor..]
            .iter()
            .position(|actual_type| actual_type.as_str() == *expected_type)
            .unwrap_or_else(|| {
                panic!("{label} is missing ordered attachment {expected_type:?}: {actual:?}")
            });
        cursor += offset + 1;
    }
}

fn transcript_string_array(row: &Utf16JsonProjection, pointer: &str) -> Vec<String> {
    let Some(length) = row.array_len(pointer) else {
        return Vec::new();
    };
    (0..length)
        .map(|index| {
            row.string_at(&format!("{pointer}/{index}"))
                .unwrap_or_else(|| panic!("{pointer}/{index} is not a string"))
                .to_owned()
        })
        .collect()
}

fn attachment_row(raw: &[u8], attachment_type: &str) -> Option<Utf16JsonProjection> {
    parsed_transcript_rows(raw).into_iter().find(|row| {
        row.string_at("/type") == Some("attachment")
            && row.string_at("/attachment/type") == Some(attachment_type)
    })
}

fn assert_message_parent_chain(raw: &[u8], label: &str) {
    let messages = parsed_transcript_rows(raw)
        .into_iter()
        .filter(|row| {
            matches!(
                row.string_at("/type"),
                Some("user" | "assistant" | "system" | "attachment")
            )
        })
        .collect::<Vec<_>>();
    for pair in messages.windows(2) {
        assert_eq!(
            pair[1].string_at("/parentUuid"),
            pair[0].string_at("/uuid"),
            "{label} JSONL parent chain at {} -> {}",
            pair[0].string_at("/type").unwrap_or("?"),
            pair[1].string_at("/type").unwrap_or("?")
        );
    }
}

fn unknown_fallback_attachment_index(raw: &[u8], command_name: &str) -> Option<usize> {
    let Ok(source) = std::str::from_utf8(raw) else {
        return None;
    };
    let expected_rendered = format!(
        "<system-reminder>\nThe user's message starts with the slash command /{command_name}, but no command with that name is available in this session"
    );
    source.lines().enumerate().find_map(|(index, line)| {
        let Ok(row) = Utf16JsonProjection::parse(line) else {
            return None;
        };
        let rendered = row.string_at("/rendered/0/content")?;
        let available_names = row.array_len("/attachment/availableCommandNames")?;
        let available_count = row.number_at("/attachment/availableCommandCount")?;
        (row.string_at("/type") == Some("attachment")
            && row.string_at("/attachment/type") == Some("unknown_command_fallback")
            && row.string_at("/attachment/commandName") == Some(command_name)
            && available_names > 0
            && available_count >= available_names as f64
            && row.array_len("/rendered") == Some(1)
            && row.string_at("/renderedRole") == Some("user")
            && rendered.starts_with(&expected_rendered))
        .then_some(index)
    })
}

fn install_native_skill_input(cwd: &Path, name: &str, expected_sha256: &str) {
    let source = fixture_root()
        .join("skill-inputs")
        .join(name)
        .join("SKILL.md");
    let raw = std::fs::read(&source).expect("read captured Native Skill input bytes");
    assert_eq!(
        sha256(&raw),
        expected_sha256,
        "Native Skill input fixture hash"
    );
    let destination = cwd
        .join(branding::DOT_DIR)
        .join("skills")
        .join(name)
        .join("SKILL.md");
    std::fs::create_dir_all(destination.parent().unwrap()).expect("create isolated Skill root");
    std::fs::write(destination, raw).expect("copy checked-in Native Skill input bytes");
}

fn install_user_skill_fixture(lingxi_home: &Path, name: &str, expected_sha256: &str) {
    let source = fixture_root()
        .join("skill-inputs")
        .join(name)
        .join("SKILL.md");
    let raw = std::fs::read(&source).expect("read user Skill environment fixture bytes");
    assert_eq!(
        sha256(&raw),
        expected_sha256,
        "user Skill environment fixture hash"
    );
    let destination = lingxi_home.join("skills").join(name).join("SKILL.md");
    std::fs::create_dir_all(destination.parent().unwrap())
        .expect("create isolated user Skill root");
    std::fs::write(destination, raw).expect("copy checked-in user Skill environment fixture");
}

#[tokio::test]
async fn regular_headless_prompt_stages_skill_listing_from_live_command_registry() {
    let root = tempfile::tempdir().expect("isolated regular-prompt fixture");
    let cwd = root.path().join("project");
    let lingxi_home = root.path().join("home");
    std::fs::create_dir_all(&cwd).expect("create isolated project");
    let skill_path = lingxi_home.join("skills/headless-regular-skill/SKILL.md");
    std::fs::create_dir_all(skill_path.parent().unwrap()).expect("create user skill root");
    std::fs::write(
        &skill_path,
        "---\nname: headless-regular-skill\ndescription: Source-ordered regular prompt fixture.\n---\n\nUse this skill for the regular-prompt ingress regression.\n",
    )
    .expect("write real user skill input");

    let stream = Arc::new(StreamJsonStream::new_placeholder(
        crate::headless::io::Output::new(tokio::io::sink()),
    ));
    let mut runtime = super::tests::fixture_runtime(stream.clone(), root.path()).await;
    let catalog = runtime.headless_command_catalog().await;
    assert!(catalog
        .prompt_command_names
        .iter()
        .any(|name| { name.string_at("") == Some("headless-regular-skill") }));
    let batched = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "regular prompt response".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));
    let streaming = Arc::new(MockStreamingApiClient::with_turns(Vec::new()));
    let session_id = SessionId::from_uuid(
        uuid::Uuid::parse_str("00000000-0000-4000-8000-000000002955")
            .expect("fixture session UUID"),
    );
    let transcript_path = root.path().join("regular-prompt.jsonl");
    let filesystem: Arc<dyn lingxi_core::host::FileSystem> =
        Arc::new(PosixFileSystem::new(root.path().to_path_buf()));
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
        transcript_path.clone(),
        filesystem,
    ));

    // Reuse the real Headless tool registry and listing provider populated by
    // `fixture_runtime`; replace only network and persistence edges.
    let production_orchestrator = runtime.inner.orchestrator.clone();
    let production_tools = production_orchestrator.shared_tool_registry();
    let production_skill_listing = production_orchestrator
        .skill_listing_provider()
        .expect("headless composition wires the live Skill catalog provider");
    runtime.inner.orchestrator = orchestrator::ConversationOrchestrator::into_shared(
        orchestrator::ConversationOrchestrator::new_with_streaming(
            orchestrator::OrchestratorConfig {
                max_budget_nano_usd: Some(1_000_000_000),
                ..Default::default()
            },
            batched.clone(),
            streaming.clone(),
            production_tools,
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            stream,
            Arc::new(StaticMemoryProvider::empty()),
            cwd,
        )
        .with_task_registry(runtime.task_registry.clone())
        .with_jsonl_writer(writer)
        .with_skill_listing(production_skill_listing)
        .with_session_id(session_id),
    );

    let sink = crate::headless::output::PlainSink::new(
        crate::headless::io::Output::new(tokio::io::sink()),
        crate::headless::io::Output::new(tokio::io::sink()),
    );
    let code = run_oneshot(
        &Argv {
            prompt: Some("Summarize the project structure.".into()),
            max_budget_usd: Some(1.0),
            no_stream: true,
            ..Default::default()
        },
        &runtime,
        &sink,
    )
    .await;
    assert_eq!(code, exit_codes::SUCCESS);
    assert!(streaming.captured_calls().await.is_empty());
    let captured = batched.captured_msgs().await;
    assert_eq!(captured.len(), 1, "regular prompt reaches the model once");
    let outbound = captured[0]
        .iter()
        .map(|message| message.text_content())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(outbound.contains("headless-regular-skill"));
    assert!(outbound.contains("Source-ordered regular prompt fixture."));

    let transcript = std::fs::read(&transcript_path).expect("Headless JSONL transcript");
    let listing = attachment_row(&transcript, "skill_listing")
        .expect("regular prompt persists a typed skill listing");
    let rendered = listing
        .string_at("/attachment/content")
        .expect("typed listing keeps its rendered source field");
    assert!(rendered.contains("headless-regular-skill"));
    assert!(rendered.contains("Source-ordered regular prompt fixture."));
}

#[tokio::test]
async fn headless_catalog_rereads_live_native_usage_credits_entitlement() {
    let root = tempfile::tempdir().expect("isolated command-catalog fixture");
    let stream = Arc::new(StreamJsonStream::new_placeholder(
        crate::headless::io::Output::new(tokio::io::sink()),
    ));
    let runtime = super::tests::fixture_runtime(stream, root.path()).await;
    let before = runtime.headless_command_catalog().await;
    *runtime
        .subscription
        .write()
        .expect("live subscription snapshot lock") =
        Some(lingxi_core::host::subscription::SubscriptionSnapshot {
            is_subscriber: true,
            billing_type: Some("stripe_subscription_enterprise_self_serve".into()),
            ..Default::default()
        });

    let after = runtime.headless_command_catalog().await;
    assert_eq!(
        after.available_command_count,
        before.available_command_count + 1,
        "the live Native billing allowlist enables the non-interactive usage-credits twin"
    );
}

#[test]
fn native_295_commands_fixture_replay_through_headless_entry() {
    // This end-to-end fixture keeps a large replay frame live across many
    // awaits. Give only this test a known stack size; CI's RUST_MIN_STACK and
    // production runtime thread sizes remain unchanged.
    let test_thread = std::thread::Builder::new()
        .name("native-295-command-replay".to_owned())
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build current-thread Tokio runtime for Native replay fixture");
            runtime.block_on(native_295_commands_fixture_replay_body());
        })
        .expect("spawn Native replay fixture thread");
    test_thread
        .join()
        .expect("Native replay fixture test thread panicked");
}

async fn native_295_commands_fixture_replay_body() {
    // Version guard: this is the separate public Native 2.1.295 capture.
    // The checked-in 2.1.293 baseline remains in its own fixture directory.
    let receipt = std::fs::read(fixture_root().join("native-receipt.json"))
        .expect("Native 2.1.295 command fixture receipt");
    assert_eq!(sha256(&receipt), NATIVE_295_RECEIPT_SHA256);
    let receipt: serde_json::Value =
        serde_json::from_slice(&receipt).expect("parse Native command fixture receipt");
    assert_eq!(receipt["native_version"], "2.1.295");
    assert_eq!(receipt["native_sha256"], NATIVE_295_SHA256);
    assert!(receipt["all_completed"].as_bool().unwrap_or(false));
    let manifest = std::fs::read(fixture_root().join("capture-manifest.json"))
        .expect("Native 2.1.295 capture manifest");
    assert_eq!(sha256(&manifest), NATIVE_295_MANIFEST_SHA256);
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest).expect("parse Native capture manifest");
    assert_eq!(manifest["target"]["native_version"], "2.1.295");
    assert_eq!(manifest["target"]["native_sha256"], NATIVE_295_SHA256);
    assert_eq!(manifest["capture"]["output_format"], "json");
    assert_eq!(
        manifest["cases"].as_array().map(Vec::len),
        Some(CASES.len())
    );
    let receipt_cases = receipt["cases"].as_array().expect("captured case receipts");
    assert_eq!(receipt_cases.len(), CASES.len());
    let manifest_cases = manifest["cases"].as_array().expect("manifest case inputs");
    assert_eq!(manifest_cases.len(), CASES.len());

    for case in CASES {
        let native_input = std::fs::read(
            fixture_root()
                .join("inputs")
                .join(format!("{}.input.json", case.id)),
        )
        .expect("read original Native input metadata");
        assert_eq!(
            sha256(&native_input),
            case.native_input_sha256,
            "case {} Native input metadata fixture",
            case.id
        );
        let native_input: serde_json::Value =
            serde_json::from_slice(&native_input).expect("parse original Native input metadata");
        assert_eq!(native_input["case"], case.id);
        assert_eq!(native_input["native_sha256"], NATIVE_295_SHA256);
        assert!(native_input["dummy_credential_used"]
            .as_bool()
            .unwrap_or(false));
        assert_eq!(
            native_input["argv"]
                .as_array()
                .and_then(|argv| argv.last())
                .and_then(serde_json::Value::as_str),
            Some(case.prompt)
        );

        let root = tempfile::tempdir().expect("isolated headless command fixture");
        let cwd = root.path().join("project");
        let lingxi_home = root.path().join("home");
        std::fs::create_dir_all(&cwd).expect("create isolated project");
        let instruction_body = "Native command replay fixture instructions";
        std::fs::write(cwd.join("LINGXI.md"), instruction_body)
            .expect("write isolated project instructions");
        if let (Some(skill_name), Some(input_sha256)) = (case.skill_name, case.skill_input_sha256) {
            install_native_skill_input(&cwd, skill_name, input_sha256);
        }
        if let (Some(skill_name), Some(input_sha256)) =
            (case.user_skill_name, case.user_skill_fixture_sha256)
        {
            install_user_skill_fixture(&lingxi_home, skill_name, input_sha256);
        }

        let stream = Arc::new(StreamJsonStream::new_placeholder(
            crate::headless::io::Output::new(tokio::io::sink()),
        ));
        let mut runtime = super::tests::fixture_runtime(stream.clone(), root.path()).await;

        let batched = Arc::new(MockApiClient::new(vec![mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "fixed native commands capture response".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        )]));
        let streaming = Arc::new(MockStreamingApiClient::with_turns(Vec::new()));
        let session_id = SessionId::from_uuid(
            uuid::Uuid::parse_str(case.session_id).expect("fixture session UUID"),
        );
        let transcript_path = root.path().join(format!("{}.jsonl", case.session_id));
        let filesystem: Arc<dyn lingxi_core::host::FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_path_buf()));
        let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
            transcript_path.clone(),
            filesystem,
        ));
        let orchestrator_config = orchestrator::OrchestratorConfig {
            max_budget_nano_usd: case.unknown_command_fallback.then_some(1_000_000_000),
            context_rendering: lingxi_core::host::instructions::InstructionRendering::Announced,
            ..Default::default()
        };
        // Keep the production headless tool registry and its lazy Skill
        // provider while replacing only the remote clients and transcript
        // writer. The captured installed-skill fixture is already loaded into
        // the shared command catalog by fixture_runtime; an empty registry
        // would make Native's Skill-tool gate false and could never produce
        // the expected `skill_listing` NAr row.
        let production_orchestrator = runtime.inner.orchestrator.clone();
        let production_tools = production_orchestrator.shared_tool_registry();
        let production_skill_listing = production_orchestrator
            .skill_listing_provider()
            .expect("headless composition wires the live Skill catalog provider");
        let memory = StaticMemoryProvider::with_files(vec![MemoryFile {
            source_content: None,
            parent: None,
            path: cwd.join("LINGXI.md"),
            body: instruction_body.into(),
            is_local_override: false,
            tier: LingxiMdTier::Project,
            globs: None,
            raw_content: instruction_body.into(),
            content_differs_from_disk: false,
        }]);
        runtime.inner.orchestrator = orchestrator::ConversationOrchestrator::into_shared(
            orchestrator::ConversationOrchestrator::new_with_streaming(
                orchestrator_config,
                batched.clone(),
                streaming.clone(),
                production_tools,
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                stream.clone(),
                Arc::new(memory),
                cwd.clone(),
            )
            .with_task_registry(runtime.task_registry.clone())
            .with_jsonl_writer(writer)
            .with_skill_listing(production_skill_listing)
            .with_session_id(session_id),
        );

        let sink = crate::headless::output::PlainSink::new(
            crate::headless::io::Output::new(tokio::io::sink()),
            crate::headless::io::Output::new(tokio::io::sink()),
        );
        let argv = Argv {
            prompt: Some(case.prompt.into()),
            max_budget_usd: Some(1.0),
            no_stream: true,
            ..Default::default()
        };
        let code = run_oneshot(&argv, &runtime, &sink).await;
        assert_eq!(code, exit_codes::SUCCESS, "case {}", case.id);
        assert!(
            streaming.captured_calls().await.is_empty(),
            "case {} uses batched request",
            case.id
        );

        let captured = batched.captured_msgs().await;
        assert_eq!(
            captured.len(),
            1,
            "case {} makes one outbound model request",
            case.id
        );
        let outbound_bytes =
            serde_json::to_vec(&captured[0]).expect("serialize captured message seam");
        // MockApiClient sees the separate Source rows before SDK conversion.
        // The captured Native request includes normalizeMessagesForAPI's
        // newline at the adjacent-user text seam, so compare that fixture only
        // after running the same production normalizer.
        let normalized_messages =
            llm_runtime::convert::normalize_messages_for_api(captured[0].clone());
        let normalized_outbound_bytes =
            serde_json::to_vec(&normalized_messages).expect("serialize normalized API messages");
        let native_request = std::fs::read(
            fixture_root()
                .join("requests")
                .join(format!("{}.request-00.json", case.id)),
        )
        .expect("read original Native 2.1.295 raw request body");
        let native_jsonl = std::fs::read(
            fixture_root()
                .join("sessions")
                .join(format!("{}.jsonl", case.id)),
        )
        .expect("read original Native 2.1.295 raw session JSONL");
        let native_response = std::fs::read(
            fixture_root()
                .join("responses")
                .join(format!("{}.response-00.raw", case.id)),
        )
        .expect("read original Native 2.1.295 raw SSE response body");
        assert_eq!(
            sha256(&native_request),
            case.native_request_sha256,
            "case {} Native request fixture",
            case.id
        );
        assert_eq!(
            sha256(&native_jsonl),
            case.native_session_sha256,
            "case {} Native session fixture",
            case.id
        );
        assert_eq!(
            sha256(&native_response),
            case.native_response_sha256,
            "case {} raw SSE response fixture",
            case.id
        );
        assert!(contains_bytes(
            &native_response,
            b"fixed native commands capture response"
        ));
        assert!(contains_bytes(&native_jsonl, case.session_id.as_bytes()));
        let case_receipt = receipt_cases
            .iter()
            .find(|captured| captured["id"] == case.id)
            .expect("case is present in Native capture receipt");
        let manifest_case = manifest_cases
            .iter()
            .find(|captured| captured["id"] == case.id)
            .expect("case is present in Native capture manifest");
        assert_eq!(manifest_case["session_id"], case.session_id);
        assert_eq!(manifest_case["prompt"], case.prompt);
        assert_eq!(case_receipt["session_id"], case.session_id);
        assert_eq!(case_receipt["status"], "complete");
        assert_eq!(
            case_receipt["loopback_requests"][0]["request_sha256"],
            case.native_request_sha256
        );
        assert_eq!(
            case_receipt["loopback_requests"][0]["response_sha256"],
            case.native_response_sha256
        );

        if let Some(skill_name) = case.skill_name {
            // Qe emits a plain user command header and a separate meta body;
            // its trailing LF is introduced only when adjacent user messages
            // are normalized for the API.
            let source_header = case.expected_outbound_texts[0]
                .strip_suffix('\n')
                .expect("Native API header fixture includes the adjacent-user seam newline");
            assert!(
                captured[0].iter().any(|message| {
                    matches!(
                        message,
                        lingxi_core::types::ConversationMessage::User {
                            is_meta: false,
                            ..
                        }
                    ) && message.text_content() == source_header
                }),
                "pre-SDK command header is a non-meta source row without the API merge newline for {}",
                case.id
            );
            let skill_dir = cwd.join(branding::DOT_DIR).join("skills").join(skill_name);
            let body_prefix = format!("Base directory for this skill: {}\n\n", skill_dir.display());
            let fixed_body = format!("Return the fixed Skill body for {skill_name}.");
            assert!(
                captured[0].iter().any(|message| {
                    matches!(
                        message,
                        lingxi_core::types::ConversationMessage::User {
                            is_meta: true,
                            turn_companion: Some(Some(true)),
                            ..
                        }
                    ) && message.text_content().starts_with(&body_prefix)
                        && message.text_content().contains(&fixed_body)
                }),
                "pre-SDK Skill body is a meta turn-companion row with its captured body for {}",
                case.id
            );
        }

        for expected in case.expected_outbound_texts {
            let needle = exact_text_needle(expected);
            assert!(
                contains_bytes(&native_request, &needle),
                "Native raw request missing fixed source text for {}: {expected:?}",
                case.id
            );
            assert!(
                contains_bytes(&normalized_outbound_bytes, &needle),
                "normalized API message source differs for {}: {expected:?}; raw normalized message JSON: {}",
                case.id,
                String::from_utf8_lossy(&normalized_outbound_bytes)
            );
        }

        if let Some(persisted_command_text) = case.persisted_command_text {
            let persisted_marker = exact_json_string_needle(persisted_command_text);
            assert!(
                contains_bytes(&native_jsonl, &persisted_marker),
                "Native raw JSONL missing command marker for {}",
                case.id
            );
            let harness_jsonl = std::fs::read(&transcript_path).expect("Harness JSONL transcript");
            assert!(
                contains_bytes(&harness_jsonl, &persisted_marker),
                "Harness JSONL missing command marker for {}",
                case.id
            );
        }

        if case.skill_name.is_none() {
            let native_prompt = exact_json_string_needle(case.prompt);
            assert!(
                contains_bytes(&native_jsonl, &native_prompt),
                "Native JSONL does not contain fixed prompt source for {}",
                case.id
            );
        }

        let native_attachment_types = transcript_attachment_types(&native_jsonl);
        assert_attachment_subsequence(
            &native_attachment_types,
            if case.unknown_command_fallback {
                NATIVE_UNKNOWN_ATTACHMENT_ORDER
            } else {
                &[
                    "instructions",
                    "session_context",
                    "date",
                    "remote_session_change",
                    "prompt_snapshot",
                ]
            },
            "Native 2.1.295 attachment order",
        );
        assert_message_parent_chain(&native_jsonl, "Native 2.1.295");
        let harness_jsonl = std::fs::read(&transcript_path).expect("Harness JSONL transcript");
        assert_message_parent_chain(&harness_jsonl, "Harness");
        let harness_attachment_types = transcript_attachment_types(&harness_jsonl);
        assert_attachment_subsequence(
            &harness_attachment_types,
            if case.unknown_command_fallback {
                &[
                    "environment",
                    "model",
                    "skill_listing",
                    "total_tokens_reminder",
                    "budget_usd",
                    "unknown_command_fallback",
                    "instructions",
                    "session_context",
                    "date",
                    "prompt_snapshot",
                ]
            } else {
                &["instructions", "session_context", "date", "prompt_snapshot"]
            },
            "Harness attachment order",
        );

        if case.unknown_command_fallback {
            // Native 2.1.295 persists this typed sidecar after the reminder
            // fan-out and before the common context announcements. The
            // announcement producer is shared by all turn types, not gated on
            // unknown-command dispatch.
            assert!(contains_bytes(
                &native_request,
                b"The user's message starts with the slash command"
            ));
            assert!(contains_bytes(
                &native_request,
                NATIVE_GRAPHIFY_SKILL_DESCRIPTION.as_bytes()
            ));
            let harness_skill_listing = attachment_row(&harness_jsonl, "skill_listing")
                .unwrap_or_else(|| {
                    panic!(
                        "Harness skill listing missing; attachment rows: {:?}",
                        transcript_attachment_types(&harness_jsonl)
                    )
                });
            assert!(harness_skill_listing
                .string_at("/attachment/content")
                .is_some_and(|content| content.contains(NATIVE_GRAPHIFY_SKILL_DESCRIPTION)));
            assert!(contains_bytes(
                &outbound_bytes,
                NATIVE_GRAPHIFY_SKILL_DESCRIPTION.as_bytes()
            ));
            let native_texts = native_user_text_blocks(&native_request);
            let native_index = |needle: &str| {
                native_texts
                    .iter()
                    .position(|text| text.contains(needle))
                    .unwrap_or_else(|| panic!("Native 2.1.295 request lacks {needle:?}"))
            };
            assert!(native_index("<total_tokens>") < native_index("USD budget"));
            assert!(native_index("Primary working directory:") < native_index("<total_tokens>"));
            let native_fallback_index = native_index(
                "The user's message starts with the slash command /__native_unknown_295__",
            );
            assert!(native_index("USD budget") < native_fallback_index);
            assert!(native_fallback_index < native_index("Codebase and user instructions"));
            assert!(
                native_index("Primary working directory:")
                    < native_index("Codebase and user instructions")
            );
            assert!(native_index("Primary working directory:") < native_index("Today's date is"));
            assert!(native_index("Today's date is") < native_index(case.prompt));
            let native_prompt_index = transcript_prompt_index(&native_jsonl, case.prompt)
                .expect("Native JSONL persists the exact unknown-command prompt");
            let native_fallback_index =
                unknown_fallback_attachment_index(&native_jsonl, "__native_unknown_295__")
                    .expect("Native JSONL persists the typed unknown-command fallback");
            assert!(native_fallback_index > native_prompt_index);
            let native_assistant_index = transcript_type_index(&native_jsonl, "assistant")
                .expect("Native JSONL persists the assistant response");
            assert!(native_fallback_index < native_assistant_index);

            let native_types = transcript_attachment_types(&native_jsonl);
            assert_eq!(
                native_types,
                NATIVE_UNKNOWN_ATTACHMENT_ORDER
                    .iter()
                    .map(|attachment_type| (*attachment_type).to_owned())
                    .collect::<Vec<_>>()
            );
            let native_attachment = attachment_row(&native_jsonl, "unknown_command_fallback")
                .expect("parse Native typed fallback attachment");
            let native_names =
                transcript_string_array(&native_attachment, "/attachment/availableCommandNames");
            assert_eq!(
                native_names,
                NATIVE_UNKNOWN_COMMAND_NAMES
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                native_attachment.number_at("/attachment/availableCommandCount"),
                Some(41.0),
                "Native catalog count is fixed fixture data"
            );
            let native_budget = attachment_row(&native_jsonl, "budget_usd")
                .expect("parse Native typed budget attachment");
            assert_eq!(
                native_attachment.string_at("/parentUuid"),
                native_budget.string_at("/uuid"),
                "Native fallback parent follows its preceding budget reminder"
            );

            let current_catalog = runtime.headless_command_catalog().await;
            let current_catalog_names = current_catalog
                .prompt_command_names
                .iter()
                .map(|name| {
                    name.string_at("")
                        .expect("catalog names must be valid source strings")
                        .to_owned()
                })
                .collect::<Vec<_>>();
            let command_registry = runtime.inner.shared_command_registry.read().await;
            // `run_oneshot` has completed its shutdown boundary here. Shutdown
            // retires builtin handler objects but preserves the ordinary
            // command catalog, so assert only that these LingXi facade rows
            // remain registered.
            for name in ["connect", "worktree", "fusion"] {
                assert!(
                    command_registry.resolve(&name.into()).is_some(),
                    "Product facade /{name} command remains in the ordinary registry"
                );
            }
            drop(command_registry);
            assert_eq!(
                current_catalog_names,
                NATIVE_UNKNOWN_COMMAND_NAMES
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect::<Vec<_>>(),
                "Harness command catalog must match the captured Native 2.1.295 list; differences remain a strict parity failure"
            );
            assert_eq!(
                current_catalog.available_command_count, 41,
                "Harness command catalog count must match Native 2.1.295"
            );

            let reminder_marker = "The user's message starts with the slash command";
            let outbound_reminders = captured[0]
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        lingxi_core::types::ConversationMessage::User { is_meta: true, .. }
                    ) && message.text_content().contains(reminder_marker)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                outbound_reminders.len(),
                1,
                "unknown slash reminder reaches outbound request once"
            );
            let outbound_index = |needle: &str| {
                captured[0]
                    .iter()
                    .position(|message| message.text_content().contains(needle))
                    .unwrap_or_else(|| panic!("Harness request is missing {needle:?}"))
            };
            let outbound_total_tokens = outbound_index("<total_tokens>");
            let outbound_fallback_index =
                outbound_index("The user's message starts with the slash command");
            let outbound_instructions = outbound_index("Codebase and user instructions");
            let outbound_session_context = outbound_index("Primary working directory:");
            let outbound_date = outbound_index("Today's date is");
            let outbound_prompt_index = captured[0]
                .iter()
                .position(|message| message.text_content() == case.prompt)
                .expect("current prompt is present in the outgoing history");
            assert!(
                outbound_session_context < outbound_total_tokens
                    && outbound_total_tokens < outbound_fallback_index
                    && outbound_fallback_index < outbound_instructions
                    && outbound_session_context < outbound_instructions
                    && outbound_session_context < outbound_date
                    && outbound_date < outbound_prompt_index,
                "Native 2.1.295 request rows must order per-turn reminders, typed fallback, shared context, and prompt"
            );
            let reminder_occurrences = captured[0]
                .iter()
                .map(|message| message.text_content().matches(reminder_marker).count())
                .sum::<usize>();
            assert_eq!(
                reminder_occurrences, 1,
                "the model request contains exactly one copy of the fallback reminder"
            );
            let outbound_reminder = outbound_reminders[0].text_content();
            let harness_prompt_index = transcript_prompt_index(&harness_jsonl, case.prompt)
                .expect("Harness JSONL persists the exact unknown-command prompt");
            let harness_fallback_index =
                unknown_fallback_attachment_index(&harness_jsonl, "__native_unknown_295__")
                    .expect("Harness JSONL persists the typed Native-compatible fallback");
            assert!(harness_fallback_index > harness_prompt_index);
            let harness_attachment = attachment_row(&harness_jsonl, "unknown_command_fallback")
                .expect("parse persisted fallback attachment row");
            assert_eq!(
                transcript_string_array(&harness_attachment, "/attachment/availableCommandNames"),
                current_catalog_names,
                "persisted catalog names come from the current runtime catalog"
            );
            assert_eq!(
                harness_attachment.number_at("/attachment/availableCommandCount"),
                Some(current_catalog.available_command_count as f64),
                "persisted catalog count comes from the current runtime catalog"
            );
            assert_eq!(
                harness_attachment.string_at("/rendered/0/content"),
                Some(outbound_reminder.as_str()),
                "typed transcript rendering preserves the exact outbound reminder"
            );
            assert!(
                harness_fallback_index
                    < transcript_type_index(&harness_jsonl, "assistant")
                        .expect("Harness JSONL persists the assistant response")
            );
        } else if case.skill_name.is_none() {
            let harness_jsonl = std::fs::read(&transcript_path).expect("Harness JSONL transcript");
            assert!(
                transcript_prompt_index(&harness_jsonl, case.prompt).is_some(),
                "case {}",
                case.id
            );
        }

        if let Some(skill_name) = case.skill_name {
            let skill_dir = cwd.join(branding::DOT_DIR).join("skills").join(skill_name);
            let dynamic_base = format!("Base directory for this skill: {}", skill_dir.display());
            assert!(
                contains_bytes(&outbound_bytes, dynamic_base.as_bytes()),
                "skill base directory is a dynamic path constraint for {}",
                case.id
            );
            let fixed_body = format!("Return the fixed Skill body for {skill_name}.");
            assert!(
                contains_bytes(&native_request, fixed_body.as_bytes()),
                "Native raw request missing fixed Skill body for {}",
                case.id
            );
            assert!(
                contains_bytes(&outbound_bytes, fixed_body.as_bytes()),
                "outbound message missing fixed Skill body for {}",
                case.id
            );
            assert!(
                contains_bytes(&native_jsonl, fixed_body.as_bytes()),
                "Native JSONL missing fixed Skill body for {}",
                case.id
            );
            let harness_jsonl = std::fs::read(&transcript_path).expect("Harness JSONL transcript");
            assert!(
                contains_bytes(&harness_jsonl, dynamic_base.as_bytes()),
                "Harness JSONL skill base path is a dynamic constraint for {}",
                case.id
            );
            assert!(
                contains_bytes(&harness_jsonl, fixed_body.as_bytes()),
                "Harness JSONL missing fixed Skill body for {}",
                case.id
            );
        }

        let native_fixed_prompt = exact_text_needle(case.expected_outbound_texts[0]);
        assert!(
            contains_bytes(&native_request, &native_fixed_prompt),
            "case {} Native fixed-field anchor",
            case.id
        );
        stream.finish().await.expect("close test stream");
    }
}
