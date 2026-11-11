//! Explicitly invoked live acceptance through the production SDK print driver.
//! The signed test host reads selected Desktop accounts into memory; only
//! secret-free metadata, public frames and test-local accounting reach disk.
use super::*;
use crate::headless::io::Output;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

#[derive(Clone, Deserialize)]
struct LiveCase {
    label: String,
    account_id: String,
    model: String,
    #[serde(default)]
    tool_probe: bool,
}

#[derive(Deserialize)]
struct LiveConfig {
    output: PathBuf,
    account_home: PathBuf,
    store_scope: String,
    broker_client: PathBuf,
    channel: String,
    cases: Vec<LiveCase>,
}

async fn credential(
    config: &LiveConfig,
    account: &lingxi_core::host::provider_accounts::AccountMetadata,
) -> secret::AccountCredential {
    let mut child = tokio::process::Command::new(&config.broker_client)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start installed signed credential client");
    let service = if config.channel == "production" {
        "com.lingxi.provider-credentials.v1"
    } else {
        assert_eq!(config.channel, "development");
        "com.lingxi.provider-credentials.v1.development"
    };
    let request =
        json!({"op":"retrieve", "service":service, "account":account.credential_ref.secret_ref});
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(serde_json::to_string(&request).unwrap().as_bytes())
        .await
        .unwrap();
    input.shutdown().await.unwrap();
    drop(input);
    let output = tokio::time::timeout(std::time::Duration::from_secs(90), child.wait_with_output())
        .await
        .expect("credential client deadline")
        .expect("credential client completion");
    assert!(output.stdout.len() <= 128 * 1024, "bounded broker response");
    let response: Value = serde_json::from_slice(&output.stdout).expect("broker response JSON");
    // Never include the response, payload, credential or provider error in a panic.
    assert!(
        output.status.success() && response["ok"] == true && response["present"] == true,
        "selected account is unavailable through the signed broker"
    );
    let mut record: Value = serde_json::from_str(
        response["payload"]
            .as_str()
            .expect("broker payload present"),
    )
    .expect("stored account record JSON");
    if record["kind"]["kind"] == "oauth" {
        let grant = &record["kind"];
        record["kind"] = json!({
            "kind":"oauth", "access_token":grant["access_token"], "refresh_token":null,
            "expires_at":{"secs_since_epoch":grant["expires_at"],"nanos_since_epoch":0},
            "scopes":grant["scopes"], "issuer":grant["issuer"], "subject":grant["subject"],
            "organization_id":grant["account_id"], "email":null,
            "fedramp":grant.get("fedramp").and_then(Value::as_bool).unwrap_or(false)
        });
    }
    let credential: secret::AccountCredential = serde_json::from_value(record)
        .unwrap_or_else(|_| panic!("stored account record has unsupported shape"));
    assert_eq!(credential.identity_generation, account.identity_generation);
    credential
}

async fn write_request(writer: &mut tokio::io::DuplexStream, value: Value) {
    writer
        .write_all(serde_json::to_string(&value).unwrap().as_bytes())
        .await
        .unwrap();
    writer.write_all(b"\n").await.unwrap();
}

async fn read_frame(
    reader: &mut tokio::io::BufReader<tokio::io::DuplexStream>,
    log: &mut std::fs::File,
) -> Value {
    use std::io::Write;
    let mut bytes = Vec::new();
    let n = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        reader.read_until(b'\n', &mut bytes),
    )
    .await
    .expect("live output deadline")
    .expect("read public output");
    assert!(
        n > 0 && bytes.ends_with(b"\n"),
        "complete live output frame"
    );
    log.write_all(&bytes).unwrap();
    serde_json::from_slice(&bytes).expect("public output JSON")
}

async fn drive(
    config: &LiveConfig,
    runtime: &Runtime,
    plane: Arc<StdioControlPlane>,
    mut input: tokio::io::DuplexStream,
    output: tokio::io::DuplexStream,
    session_id: String,
    mut done: tokio::sync::oneshot::Receiver<()>,
) -> Vec<Value> {
    use std::io::Write;
    let mut reader = tokio::io::BufReader::new(output);
    let mut log = std::fs::File::create(config.output.join("stdout.jsonl")).unwrap();
    write_request(
        &mut input,
        json!({"type":"control_request","request_id":"live-init",
        "request":{"subtype":"initialize","hooks":{},"sdkMcpServers":[]}}),
    )
    .await;
    loop {
        let row = read_frame(&mut reader, &mut log).await;
        if row["type"] == "control_response" && row["response"]["request_id"] == "live-init" {
            assert_eq!(row["response"]["subtype"], "success");
            break;
        }
    }
    let mut outcomes = Vec::new();
    for case in &config.cases {
        // Account profiles are deliberately hidden from user model selection.
        // The trusted host pins the route through its lifecycle API, under
        // the same operation fence as SDK model selection.
        let selected = {
            let _operation = plane.lock_operation().await;
            runtime
                .orchestrator
                .switch_model(&case.model, Some(&format!("account.{}", case.account_id)))
                .await
                .is_ok()
        };
        if !selected {
            outcomes.push(json!({"label":case.label,"account_id":case.account_id,"model":case.model,"selection_failed":true}));
            continue;
        }
        let prompt = if case.tool_probe {
            format!("Output-format acceptance probe {}. Use Bash to run exactly `printf LXI_LIVE_322_TOOL` once. After receiving its output, reply with exactly LXI_LIVE_322_OK. Use no other tools.", case.label)
        } else {
            format!("Output-format acceptance probe {}. Reply with exactly LXI_LIVE_322_OK. Do not use tools.", case.label)
        };
        write_request(
            &mut input,
            json!({"type":"user","session_id":session_id,
            "parent_tool_use_id":null,"message":{"role":"user","content":prompt}}),
        )
        .await;
        loop {
            let row = read_frame(&mut reader, &mut log).await;
            if row["type"] == "result" {
                outcomes.push(json!({"label":case.label,"account_id":case.account_id,"model":case.model,
                    "result_index":row["result_index"],"subtype":row["subtype"],"is_error":row["is_error"]}));
                break;
            }
        }
    }
    input.shutdown().await.unwrap();
    loop {
        tokio::select! {
            _ = &mut done => break,
            _ = read_frame(&mut reader, &mut log) => {},
        }
    }
    log.flush().unwrap();
    outcomes
}

async fn run_live(config: LiveConfig) {
    assert!(!config.cases.is_empty());
    assert!(
        !config.output.exists(),
        "live run never overwrites evidence"
    );
    std::fs::create_dir_all(&config.output).unwrap();
    let snapshot =
        crate::provider_accounts::load_account_snapshot(&config.account_home, &config.store_scope)
            .expect("read selected Desktop account metadata");
    let account_home = config.output.join("account-state");
    let directory = account_home.join("provider-accounts");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join(format!("{}.json", snapshot.store_scope)),
        serde_json::to_vec(&snapshot).unwrap(),
    )
    .unwrap();
    let home = config.output.join("session-state");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("settings.json"),
        br#"{"permissions":{"allow":["Bash(printf LXI_LIVE_322_TOOL)"]}}"#,
    )
    .unwrap();
    let mut shared = crate::desktop::build_shared_credential_stack_with_policy(
        &home,
        false,
        lingxi_core::host::CredentialStoragePolicy::NativeOrMemory,
    )
    .await
    .unwrap();
    let mut loaded = std::collections::HashSet::new();
    for case in &config.cases {
        if !loaded.insert(case.account_id.clone()) {
            continue;
        }
        let account = snapshot
            .accounts
            .iter()
            .find(|row| row.account_id == case.account_id && row.enabled)
            .expect("live account must be configured and enabled");
        let record = credential(&config, account).await;
        let version = crate::provider_accounts::AccountCredentialVersion {
            account_id: account.account_id.clone(),
            identity_generation: record.identity_generation,
            credential_revision: record.credential_revision,
        };
        assert_eq!(
            provider_config::read_account_credential_version(&config.account_home, account)
                .unwrap(),
            Some(record.credential_revision),
            "credential revision is published by the owning Desktop authority"
        );
        shared
            .credentials
            .set_account_credential_ephemeral(&account.credential_ref, Some(record))
            .await
            .unwrap();
        crate::provider_accounts::publish_account_credential_version(
            account_home.clone(),
            snapshot.store_scope.clone(),
            version,
        )
        .await
        .unwrap();
    }
    shared.provider_account_home = Some(account_home);
    let default_binding = snapshot
        .accounts
        .iter()
        .find(|row| row.account_id == config.cases[0].account_id)
        .unwrap()
        .execution_binding_id
        .clone();
    shared.provider_accounts = Some(snapshot);
    let (output_writer, output_reader) = tokio::io::duplex(1024 * 1024);
    let stdout = Output::new(output_writer);
    let stream = Arc::new(StreamJsonStream::new_placeholder(stdout.clone()));
    let services = Arc::new(super::tests::FixtureHost);
    let options = Argv {
        output_format: crate::headless::OutputFormat::StreamJson,
        input_format: crate::headless::InputFormat::StreamJson,
        verbose: true,
        include_partial_messages: true,
        max_budget_usd: Some(0.25),
        ..Default::default()
    };
    let mut cfg = crate::desktop::DesktopConfig::default();
    cfg.lingxi_home = home;
    cfg.cwd = config.output.join("project");
    std::fs::create_dir_all(&cfg.cwd).unwrap();
    cfg.customization_gates.bare = true;
    cfg.isolated_credential_storage = true;
    cfg.default_model = format!("{}/{}", default_binding, config.cases[0].model);
    cfg.default_model_explicit = true;
    cfg.max_budget_usd = options.max_budget_usd;
    cfg.use_noop_permission_gate = true;
    cfg.deny_unresolved_ask = true;
    crate::headless::host::configure_desktop_request_state(&mut cfg, services.as_ref(), &options);
    let user_agent = cfg.user_agent_environment.clone();
    let inner = crate::desktop::build_with_credential_stack(
        cfg,
        stream.clone(),
        Arc::new(crate::headless::HeadlessPermissionRequestSink),
        shared,
    )
    .await
    .expect("assemble live production runtime");
    let session_id = inner
        .orchestrator
        .session()
        .lock()
        .await
        .session_id
        .to_string();
    *stream.session_id_handle().lock().await = session_id.clone();
    let runtime = Runtime {
        inner,
        services,
        prompt_suggestions: suggestions::SdkPromptSuggestions::new(false),
        native_catalog_user_agent: user_agent,
        output: Output::new(tokio::io::sink()),
        stdout,
        shutdown: tokio_util::sync::CancellationToken::new(),
        active_turn: std::sync::Mutex::new(tokio_util::sync::CancellationToken::new()),
        cancel_reason: orchestrator::prompt::mid_turn_input::CancelReasonFlag::new(),
        failures: std::sync::Mutex::new(Vec::new()),
        execution_errors: Arc::new(std::sync::Mutex::new(Vec::new())),
        max_turns: Some(4),
        execution_code: std::sync::atomic::AtomicI32::new(0),
        execution_interrupted: std::sync::atomic::AtomicBool::new(false),
        shutdown_complete: std::sync::atomic::AtomicBool::new(false),
    };
    stream.share_permission_denials(runtime.orchestrator.permission_denials_handle());
    let plane = StdioControlPlane::new(stream.outbound_tx());
    let (input_writer, input_reader) = tokio::io::duplex(1024 * 1024);
    let prepared = crate::headless::stdio::PreparedStdio::start(
        Box::pin(input_reader),
        Output::new(tokio::io::sink()),
        false,
        session_id.clone(),
        stream.clone(),
        plane.clone(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let execution = async {
        let code = run_stream_json_input_loop(
            &options,
            &runtime,
            stream,
            permission::PermissionMode::Default,
            plane.clone(),
            prepared,
        )
        .await;
        let _ = done_tx.send(());
        code
    };
    let (code, outcomes) = tokio::join!(
        execution,
        drive(
            &config,
            &runtime,
            plane.clone(),
            input_writer,
            output_reader,
            session_id,
            done_rx
        )
    );
    std::fs::write(
        config.output.join("outcomes.json"),
        serde_json::to_vec_pretty(&json!({"exit_code":code,"cases":outcomes})).unwrap(),
    )
    .unwrap();
    assert_eq!(code, 0, "all live turns must succeed");
    assert!(
        outcomes
            .iter()
            .all(|row| row["subtype"] == "success" && row["is_error"] == false),
        "live cases are not all successful; inspect secret-free outcomes"
    );
}

#[test]
#[ignore = "Explicit live traffic; requires configured accounts and a signed CLI host"]
fn native_322_live_multi_provider_accounts() {
    let config = std::env::var_os("LINGXI_OUTPUT_322_LIVE_CONFIG")
        .expect("explicit live configuration path");
    let config: LiveConfig =
        serde_json::from_slice(&std::fs::read(Path::new(&config)).unwrap()).unwrap();
    std::thread::Builder::new()
        .name("output-322-live".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_live(config))
        })
        .unwrap()
        .join()
        .expect("live test host completed");
}
