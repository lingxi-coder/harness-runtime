use super::*;
use crate::desktop::{DesktopConfig, DesktopMcpServices, DesktopSessionComposition};
use crate::headless::control_plane::StdioControlPlane;
use crate::headless::io::Output;
use crate::headless::sdk_mcp_transport::{sdk_servers_from_initialize, SdkMcpTransport};
use crate::headless::stream_json::StreamJsonStream;

async fn runtime(root: &Path) -> crate::desktop::DesktopRuntime {
    runtime_with_plane(root).await.0
}

async fn runtime_with_plane(
    root: &Path,
) -> (crate::desktop::DesktopRuntime, Arc<StdioControlPlane>) {
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let peer = root.join("peer.py");
    std::fs::write(&peer, r#"
import sys,json,time,threading
from pathlib import Path
lock=threading.Lock()
def send(f):
 with lock:sys.stdout.write(json.dumps(f,separators=(',',':'))+'\n');sys.stdout.flush()
def handle(f):
 m=f.get('method');p=f.get('params',{});i=f.get('id')
 if i is None:return
 if m=='initialize':r={'protocolVersion':p['protocolVersion'],'capabilities':{'tools':{'listChanged':True}},'serverInfo':{'name':'ordinary','version':'1'}}
 elif m=='tools/list':r={'tools':[{'name':n,'inputSchema':{'type':'object'},'annotations':{'readOnlyHint':True}} for n in ['echo','slow','logical','rpc','files']]}
 elif m=='tools/call':
  n=p['name'];a=p.get('arguments',{})
  if n=='rpc':send({'jsonrpc':'2.0','id':i,'error':{'code':-32003,'message':a.get('message','fixture rpc error')}});return
  if n=='slow':time.sleep(2)
  if n=='files':
   if a.get('out'):Path(a['out']).write_bytes(b'STAGED_OUTPUT')
   t='files complete'
  else:t=json.dumps(a,separators=(',',':'))
  r={'content':[{'type':'text','text':t}],'structuredContent':{'kind':n},'_meta':{'fixture':True}}
  if n=='logical':
   r['isError']=True
   if isinstance(a,dict) and 'message' in a:r['content'][0]['text']=a['message']
 else:r={}
 send({'jsonrpc':'2.0','id':i,'result':r})
for l in sys.stdin:
 with open(sys.argv[1],'a') as capture:capture.write(l)
 try:f=json.loads(l)
 except Exception:continue
 if f.get('method')=='tools/call':threading.Thread(target=handle,args=(f,),daemon=True).start()
 else:handle(f)
"#).unwrap();
    let mut config = DesktopConfig::default();
    config.cwd = cwd.clone();
    config.lingxi_home = root.join("home");
    config.isolated_credential_storage = true;
    config.use_noop_permission_gate = true;
    config.composition = Some(DesktopSessionComposition::HeadlessCli);
    let raw = serde_json::json!({"ordinary":{"command":"/usr/bin/python3","args":[peer,root.join("peer.stdin.jsonl")]}});
    config.cli_mcp_servers =
        ::mcp::json_config::parse_mcp_json_string(&raw.to_string(), ::mcp::ConfigScope::Dynamic)
            .unwrap();
    let prepared =
        crate::headless::session::prepare(&mut config, &crate::headless::SessionStart::New, false)
            .await
            .unwrap();
    let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(
        tokio::io::sink(),
    )));
    let plane = StdioControlPlane::new(stream.outbound_tx());
    let factory_plane = plane.clone();
    config.mcp_services_factory = Some(Arc::new(move |base| {
        let transport = SdkMcpTransport::new(base, factory_plane.clone(), cwd.clone());
        DesktopMcpServices {
            transport: transport.clone(),
            raw_connections: transport,
        }
    }));
    let runtime = crate::headless::session::build_prepared(
        config,
        prepared,
        stream,
        Arc::new(crate::headless::HeadlessPermissionRequestSink),
    )
    .await
    .unwrap();
    // These cases record explicit permission approval. The production default
    // is Auto and must continue to reject when its real classifier is absent;
    // the fixture does not manufacture a classifier or server verdict.
    runtime.enforcing_permission_gate.as_ref().unwrap().apply_permission_updates(&[
        Json::parse(r#"{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"mcp__ordinary__echo"},{"toolName":"mcp__ordinary__rpc"}]}"#).unwrap(),
    ]);
    let initialize = Json::parse(r#"{"request":{"sdkMcpServers":["fixture"]}}"#).unwrap();
    let configs = sdk_servers_from_initialize(&initialize);
    let transport = plane.sdk_mcp_transport().unwrap();
    transport.configure_server_names(&configs);
    let manifest = Json::parse(r#"{"fixture":{"initializeResult":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}},"toolsListResult":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}}"#).unwrap();
    assert_eq!(
        transport
            .park_manifests(&["fixture".into()], Some(&manifest))
            .unwrap()
            .string_at("/fixture"),
        Some("parked")
    );
    runtime.orchestrator.connect_sdk_mcp_servers(configs).await;
    (runtime, plane)
}

#[cfg(unix)]
#[tokio::test]
async fn native_322_direct_mcp_calls_replay_process_fixture_through_real_registry() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path()).await;
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/mcp_sdk_gap_control_2_1_322.json"
    ))
    .unwrap();
    let mut compared = 0;
    for case in fixture["cases"].as_array().unwrap() {
        let request = Json::from_serde_value(&case["request"]).unwrap();
        let Some(tool) = request.string_at("/tool") else {
            continue;
        };
        if request.contains_property("check_permissions")
            && request
                .at_pointer("/check_permissions")
                .and_then(|v| v.as_bool())
                .is_none()
        {
            continue;
        }
        let environment = CallEnvironment {
            essential_traffic_only: true,
            program_result: request
                .at_pointer("/result_not_for_model")
                .and_then(|v| v.as_bool())
                == Some(true),
            checked: request
                .at_pointer("/check_permissions")
                .and_then(|v| v.as_bool())
                == Some(true),
            file_store: None,
        };
        let actual = execute(
            &runtime.orchestrator,
            tool,
            &request,
            environment,
            CancellationToken::new(),
        )
        .await;
        let response = match actual {
            Ok(body) => {
                serde_json::json!({"subtype":"success","response":serde_json::from_str::<serde_json::Value>(&body.to_json_string().unwrap()).unwrap()})
            }
            Err(detail) => {
                serde_json::json!({"subtype":"error","error":serde_json::from_str::<serde_json::Value>(&detail.to_json_string().unwrap()).unwrap()})
            }
        };
        assert_eq!(response, case["response"], "{}", case["name"]);
        compared += 1;
    }
    assert_eq!(compared, 22);
    let wire = std::fs::read_to_string(root.path().join("peer.stdin.jsonl")).unwrap();
    let requests = wire
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|frame| frame.get("id").is_some())
        .collect::<Vec<_>>();
    assert_eq!(requests[0]["method"], "initialize");
    assert_eq!(requests[0]["id"], 0);
    let list = requests
        .iter()
        .find(|frame| frame["method"] == "tools/list")
        .unwrap();
    assert_eq!(list["id"], 1);
    let call = requests
        .iter()
        .find(|frame| frame["method"] == "tools/call")
        .unwrap();
    // Cached SDK-only publication must not re-discover the ordinary peer.
    let prelude = requests
        .iter()
        .take(3)
        .map(|frame| serde_json::json!([frame["method"], frame["id"]]))
        .collect::<Vec<_>>();
    assert_eq!(
        prelude,
        vec![
            serde_json::json!(["initialize", 0]),
            serde_json::json!(["tools/list", 1]),
            serde_json::json!(["tools/call", 2])
        ]
    );
    assert_eq!(call["id"], 2);
    assert_eq!(
        call["params"]["_meta"],
        serde_json::json!({"progressToken":2})
    );
    runtime.mcp_registry.disconnect("ordinary").await.unwrap();
    runtime.mcp_registry.disconnect("fixture").await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn staged_mcp_call_publishes_outputs_through_host_owned_cas_store() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path()).await;
    let store = Arc::new(LocalMcpStagedFileStore::new(root.path().join("project")));
    let original = store
        .compare_and_write("result.txt", b"original".to_vec(), None)
        .await
        .unwrap();
    let request = Json::from_serde_value(&serde_json::json!({"subtype":"mcp_call","tool":"mcp__ordinary__files","arguments":{"out":"{{out:result}}"},"expires_at":"2099-01-01T00:00:00Z","output_files":[{"name":"result","lane_path":"/working/result.txt","if_match":original}]})).unwrap();
    let make_environment = || CallEnvironment {
        essential_traffic_only: false,
        program_result: true,
        checked: false,
        file_store: Some(store.clone()),
    };
    let result = execute(
        &runtime.orchestrator,
        "mcp__ordinary__files",
        &request,
        make_environment(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        result.at_pointer("/staging/ok").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(result.number_at("/staging/outputs/0/bytes"), Some(13.0));
    assert_eq!(
        store.read("result.txt").await.unwrap().unwrap().bytes,
        b"STAGED_OUTPUT"
    );
    let replay = execute(
        &runtime.orchestrator,
        "mcp__ordinary__files",
        &request,
        make_environment(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        replay.string_at("/staging/error_code"),
        Some("output_conflict")
    );
    runtime.mcp_registry.disconnect("ordinary").await.unwrap();
    runtime.mcp_registry.disconnect("fixture").await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn native_322_raw_peer_errors_keep_utf16_through_unchecked_checked_and_staged_controls() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path()).await;
    let fixture = Json::parse(include_str!(
        "../../tests/fixtures/mcp_sdk_raw_errors_2_1_322.json"
    ))
    .unwrap();
    assert_eq!(fixture.array_len("/cases"), Some(8));
    for index in 0..fixture.array_len("/cases").unwrap() {
        let request = fixture
            .subprojection(&format!("/cases/{index}/request"))
            .unwrap();
        let expected = fixture
            .subprojection(&format!("/cases/{index}/response"))
            .unwrap();
        let result = execute(
            &runtime.orchestrator,
            request.string_at("/tool").unwrap(),
            &request,
            CallEnvironment {
                essential_traffic_only: true,
                checked: request
                    .get_property("check_permissions")
                    .and_then(|value| value.as_bool())
                    == Some(true),
                program_result: false,
                file_store: None,
            },
            CancellationToken::new(),
        )
        .await;
        let response = match result {
            Ok(body) => Json::object(vec![
                ("subtype".into(), "success".into()),
                ("response".into(), body),
            ])
            .unwrap(),
            Err(detail) => Json::object(vec![
                ("subtype".into(), "error".into()),
                ("error".into(), detail),
            ])
            .unwrap(),
        };
        assert_eq!(
            response.to_json_string().unwrap(),
            expected.to_json_string().unwrap(),
            "{}",
            fixture.string_at(&format!("/cases/{index}/name")).unwrap()
        );
    }
    runtime.mcp_registry.disconnect("ordinary").await.unwrap();
    runtime.mcp_registry.disconnect("fixture").await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn native_322_set_servers_reconciles_pending_sdk_identity_tools_and_remote_ownership() {
    let root = tempfile::tempdir().unwrap();
    let (runtime, plane) = runtime_with_plane(root.path()).await;
    let transport = plane.sdk_mcp_transport().unwrap();
    let retire = |names: &[String]| transport.retire_server_keys(names);
    let operators = vec![::mcp::McpServerName::from("ordinary")];
    let parsed = super::super::sdk_mcp_server_controls::parse_set_servers(
        &Json::parse(
            r#"{"servers":{"third":{"type":"sdk","connectorId":"first","timeout":1234}}}"#,
        )
        .unwrap(),
        false,
    )
    .unwrap();
    let (reply, pending) = runtime
        .orchestrator
        .set_mcp_servers_from_control(parsed.configs, parsed.errors, false, &operators, &retire)
        .await
        .unwrap();
    assert_eq!(reply.string_at("/added/0"), Some("third"));
    assert_eq!(reply.string_at("/removed/0"), Some("fixture"));
    assert_eq!(
        runtime
            .orchestrator
            .sdk_mcp_status()
            .await
            .unwrap()
            .string_at("/mcpServers/1/status"),
        Some("pending")
    );
    transport.configure_server_names(&pending);
    let stale = pending.clone();
    let owner = runtime.orchestrator.clone();
    let connecting = tokio::spawn(async move { owner.connect_control_mcp_servers(pending).await });
    transport
        .wait_for_handshake_admission(&["third".into()])
        .await;
    let update = super::super::sdk_mcp_server_controls::parse_set_servers(
        &Json::parse(r#"{"servers":{"third":{"type":"sdk","connectorId":"replacement","timeout":9999,"tools":[{"name":"write","permission_policy":"always_deny"}]}}}"#).unwrap(), false,
    ).unwrap();
    let (reply, pending) = tokio::time::timeout(
        Duration::from_secs(2),
        runtime.orchestrator.set_mcp_servers_from_control(
            update.configs,
            update.errors,
            false,
            &operators,
            &retire,
        ),
    )
    .await
    .expect("pending SDK tool update must not wait for its initialize reply")
    .unwrap();
    assert_eq!(reply.array_len("/added"), Some(0));
    assert!(pending.is_empty());
    let current = runtime
        .mcp_registry
        .get_config_by_key("third")
        .await
        .unwrap();
    assert_eq!(current.timeout_ms, Some(1234));
    assert_eq!(
        current
            .metadata
            .sdk_connector_id
            .as_ref()
            .unwrap()
            .string_at(""),
        Some("first")
    );
    assert_eq!(current.tools.len(), 1);
    let (reply, _) = runtime
        .orchestrator
        .set_mcp_servers_from_control(
            vec![],
            Json::parse("{}").unwrap(),
            true,
            &operators,
            &retire,
        )
        .await
        .unwrap();
    assert_eq!(
        reply.array_len("/removed"),
        Some(0),
        "remote empty payload retains caller SDK servers"
    );
    assert!(runtime
        .mcp_registry
        .get_config_by_key("third")
        .await
        .is_some());
    let (reply, _) = tokio::time::timeout(
        Duration::from_secs(2),
        runtime.orchestrator.set_mcp_servers_from_control(
            vec![],
            Json::parse("{}").unwrap(),
            false,
            &operators,
            &retire,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(reply.string_at("/removed/0"), Some("third"));
    tokio::time::timeout(Duration::from_secs(2), connecting)
        .await
        .unwrap()
        .unwrap();
    runtime
        .orchestrator
        .connect_control_mcp_servers(stale)
        .await;
    assert!(runtime
        .mcp_registry
        .get_config_by_key("third")
        .await
        .is_none());
    assert!(runtime
        .mcp_registry
        .get_config_by_key("ordinary")
        .await
        .is_some());
    runtime.mcp_registry.disconnect("ordinary").await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn native_322_sdk_toggle_reconnect_rejection_and_operator_protection_are_real_lifecycle_operations(
) {
    let root = tempfile::tempdir().unwrap();
    let (runtime, plane) = runtime_with_plane(root.path()).await;
    let transport = plane.sdk_mcp_transport().unwrap();
    let retire = |names: &[String]| transport.retire_server_keys(names);
    assert_eq!(
        runtime
            .orchestrator
            .reconnect_mcp_from_control(&"fixture".into())
            .await
            .unwrap_err()
            .string_at(""),
        Some("SDK servers should be handled in print.ts")
    );
    runtime
        .orchestrator
        .toggle_mcp_from_control(&"fixture".into(), &Json::parse("false").unwrap(), &retire)
        .await
        .unwrap();
    assert!(
        runtime
            .mcp_registry
            .get_config_by_key("fixture")
            .await
            .unwrap()
            .disabled
    );
    let manifest = Json::parse(r#"{"fixture":{"initializeResult":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"2"}},"toolsListResult":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}}"#).unwrap();
    transport.park_manifests(&["fixture".into()], Some(&manifest));
    transport.admit_server_name_units(&"fixture".encode_utf16().collect::<Vec<_>>());
    runtime
        .orchestrator
        .toggle_mcp_from_control(&"fixture".into(), &Json::parse("true").unwrap(), &retire)
        .await
        .unwrap();
    assert!(
        !runtime
            .mcp_registry
            .get_config_by_key("fixture")
            .await
            .unwrap()
            .disabled
    );
    assert!(runtime
        .mcp_registry
        .get_client_by_key("fixture")
        .await
        .is_some());
    let parsed = super::super::sdk_mcp_server_controls::parse_set_servers(
        &Json::parse(r#"{"servers":{"ordinary":{"type":"sdk"}}}"#).unwrap(),
        false,
    )
    .unwrap();
    let (reply, pending) = runtime
        .orchestrator
        .set_mcp_servers_from_control(
            parsed.configs,
            parsed.errors,
            true,
            &["ordinary".into()],
            &retire,
        )
        .await
        .unwrap();
    assert_eq!(
        reply.string_at("/errors/ordinary"),
        Some("Configured via --mcp-config on this host (operator-supplied); ignored")
    );
    assert!(pending.is_empty());
    assert!(matches!(
        runtime
            .mcp_registry
            .get_config_by_key("ordinary")
            .await
            .unwrap()
            .spec,
        lingxi_core::host::McpTransportSpec::Stdio { .. }
    ));
    runtime.mcp_registry.disconnect("ordinary").await.unwrap();
    runtime.mcp_registry.disconnect("fixture").await.unwrap();
}
