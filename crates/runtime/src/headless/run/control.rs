use super::suggestions::StreamFileSuggestionIndex;
use crate::headless::stream_json_input::ControlPlaneWriter;
use crate::headless::HeadlessRuntime as Runtime;
use lingxi_core::host::OrchestratorHandle;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use permission;
use serde_json::{json, Value};
use std::sync::Arc;

/// Resolution of a `set_model` control request's `model` field against the
/// session default, byte-faithful to claude-code 2.1.208's engine handler:
/// `if(fr!=null&&typeof fr!=="string"){…reject…} let or=model??"default",
/// Jr=or.trim().toLowerCase()==="default",vr=Jr?SE():or`.
pub(super) enum SetModelTarget {
    /// Apply this model: the raw requested string, or the session default when
    /// the request was absent / explicit `null` / case-insensitive `"default"`.
    Apply(String),
    /// The `model` field was present but neither a string nor null — reject.
    Reject,
}

pub(super) fn resolve_set_model_target(
    field: Option<&Value>,
    default_model: &str,
) -> SetModelTarget {
    // CC: `fr != null` — in JS `!= null` covers both `null` and `undefined`, so
    // an explicit JSON `null` is treated as absent (→ default), not a type
    // error. `model ?? "default"` collapses absent/null to `"default"`.
    let requested = match field {
        Some(Value::String(m)) => m.as_str(),
        None | Some(Value::Null) => "default",
        Some(_) => return SetModelTarget::Reject,
    };
    // CC: `or.trim().toLowerCase() === "default"` — trimmed, case-insensitive.
    if requested.trim().to_lowercase() == "default" {
        SetModelTarget::Apply(default_model.to_string())
    } else {
        // CC: `vr = Jr ? SE() : or` — the RAW requested string (untrimmed).
        SetModelTarget::Apply(requested.to_string())
    }
}

/// Dispatch a single `control_request` frame using initialization data collected
/// before the asynchronous dispatcher task starts.
#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_control_request(
    subtype: &str,
    request_id: &str,
    frame: &Utf16JsonProjection,
    writer: &ControlPlaneWriter,
    cancel_tx: &tokio::sync::watch::Sender<bool>,
    lifecycle: &crate::headless::queued_commands::QueueLifecycle,
    orchestrator: &Arc<orchestrator::ConversationOrchestrator>,
    task_registry: &Arc<tasks::registry::TaskRegistry>,
    session_cwd: &Arc<tool_api::SessionCwd>,
    control_plane: &Arc<crate::headless::control_plane::StdioControlPlane>,
    end_notify: &Arc<tokio::sync::Notify>,
    init_commands: &[serde_json::Value],
    init_agents: &[serde_json::Value],
    init_models: &[serde_json::Value],
    init_account: &serde_json::Value,
    init_fast_mode_state: &'static str,
    init_fast_mode_disabled_reason: Option<&'static str>,
    file_suggestions: &StreamFileSuggestionIndex,
    host: &dyn crate::headless::host::HeadlessHostServices,
    auxiliary_tasks: &Arc<super::PrintAuxTaskGroup>,
    execution_errors: &Arc<std::sync::Mutex<Vec<String>>>,
) -> Result<(), crate::headless::stream_json_input::InputError> {
    // The writer wrapper keeps the exact native request identity beside display
    // JSON; serde Value cannot represent an unpaired UTF-16 request id.
    let request_projection = frame
        .subprojection("/request")
        .unwrap_or_else(|_| Utf16JsonProjection::plain(json!({})));
    let sdk_configs = (subtype == "initialize")
        .then(|| crate::headless::sdk_mcp_transport::sdk_servers_from_initialize(frame));
    let writer = RequestReplyWriter {
        writer,
        delivery_error: std::sync::Mutex::new(None),
        request_id: frame
            .subprojection("/request_id")
            .unwrap_or_else(|_| Utf16JsonProjection::plain(json!(request_id))),
    };
    let frame = &frame.value;
    // Request body fields live at `frame.request.<field>` (already key-normalized).
    let field = |k: &str| frame.get("request").and_then(|r| r.get(k));

    match subtype {
        "initialize" => {
            let mut payload = initialize_response_payload(
                init_commands,
                init_agents,
                init_models,
                init_account,
                host.process_id(),
                init_fast_mode_state,
                init_fast_mode_disabled_reason,
            );
            let configs = sdk_configs.unwrap_or_default();
            let names = configs
                .iter()
                .map(|config| config.name.clone())
                .collect::<Vec<_>>();
            if let Some(transport) = control_plane.sdk_mcp_transport() {
                if let Some(statuses) = transport.park_manifests(
                    &names,
                    request_projection
                        .subprojection("/sdkMcpServerManifests")
                        .ok()
                        .as_ref(),
                ) {
                    payload["sdk_mcp_manifests_parked"] = statuses;
                }
            }
            writer.reply_initialize(
                payload,
                control_plane.pending_permission_requests().await,
                control_plane.pending_user_dialog_requests().await,
            );
            // The host must see this reply before receiving MCP handshake
            // requests; all later catalog changes use the session registry.
            writer.finish()?;
            if !configs.is_empty() {
                let orchestrator = orchestrator.clone();
                auxiliary_tasks.push(tokio::spawn(async move {
                    orchestrator.connect_sdk_mcp_servers(configs).await;
                }));
            }
        }
        "interrupt" => {
            let cancel_queued = field("cancel_queued").and_then(Value::as_bool) == Some(true);
            if !cancel_queued && field("send_now").and_then(Value::as_bool) == Some(true) {
                let waiting = lifecycle.queued.still_queued();
                let named = field("message_uuid")
                    .filter(|value| !value.is_null() && value.as_str() != Some(""));
                let has_waiting = match named {
                    None => !waiting.is_empty(),
                    Some(Value::String(uuid)) => waiting.iter().any(|queued| queued == uuid),
                    Some(_) => false,
                };
                let outcome = if !has_waiting {
                    "nothing_waiting"
                } else if !control_plane.is_busy().await {
                    "delivering"
                } else if control_plane.cancel_active_turn().await {
                    let _ = cancel_tx.send(true);
                    "stopped"
                } else {
                    "delivering"
                };
                writer.reply_success(
                    request_id,
                    Some(json!({"still_queued":waiting,"send_now":outcome})),
                );
                return writer.finish();
            }
            let aim = if cancel_queued {
                None
            } else {
                parse_interrupt_turn_aim(field("for_user_message_uuids"))
            };
            if aim
                .as_ref()
                .is_some_and(|aim| lifecycle.queued.current_turn_matches(aim) == Some(false))
            {
                writer.reply_success(
                    request_id,
                    Some(json!({"still_queued":lifecycle.queued.still_queued(),"turn":"spared"})),
                );
                return writer.finish();
            }
            let stopped = control_plane.cancel_active_turn().await;
            let _ = cancel_tx.send(stopped);
            if cancel_queued {
                let cancelled = lifecycle.queued.cancel_all_queued();
                for uuid in &cancelled {
                    writer.record(
                        lifecycle.emit(uuid, crate::headless::queued_commands::LIFECYCLE_CANCELLED),
                    );
                }
                writer.reply_success(
                    request_id,
                    Some(json!({"still_queued": [], "cancelled": cancelled})),
                );
            } else {
                let mut receipt = json!({"still_queued": lifecycle.queued.still_queued()});
                if aim.is_some() {
                    receipt["turn"] = json!(if stopped { "stopped" } else { "idle" });
                }
                writer.reply_success(request_id, Some(receipt));
            }
        }
        "cancel_async_message" => {
            let cancelled = field("message_uuid")
                .and_then(Value::as_str)
                .is_some_and(|uuid| lifecycle.queued.cancel_one(uuid));
            if cancelled {
                writer.record(
                    lifecycle.emit(
                        field("message_uuid")
                            .and_then(Value::as_str)
                            .expect("matched string UUID"),
                        crate::headless::queued_commands::LIFECYCLE_CANCELLED,
                    ),
                );
            }
            writer.reply_success(request_id, Some(json!({"cancelled":cancelled})));
        }
        "set_model" => {
            // §2.2 #5: `"default"` (or an absent model) resolves to the session
            // default model and APPLIES it — so a client can revert a prior
            // `set_model` override (claude-code re-resolves via
            // getDefaultMainLoopModel() and calls setMainLoopModelOverride).
            let default_model = orchestrator.default_model();
            let target = match resolve_set_model_target(field("model"), default_model.as_str()) {
                SetModelTarget::Apply(t) => t,
                SetModelTarget::Reject => {
                    // CC 2.1.208: `set_model: model must be a string`.
                    writer.reply_error(request_id, "set_model: model must be a string");
                    return writer.finish();
                }
            };
            match orchestrator
                .switch_model_with_source(&target, None, "sdk")
                .await
            {
                Ok(()) => writer.reply_success(request_id, None),
                Err(e) => writer.reply_error(request_id, &e.to_string()),
            }
        }
        "set_max_thinking_tokens" => {
            let max_tokens = match field("max_thinking_tokens") {
                Some(Value::Null) => None,
                Some(Value::Number(value)) => value.as_u64().and_then(|v| u32::try_from(v).ok()),
                None | Some(_) => None,
            };
            let max_tokens_valid =
                matches!(field("max_thinking_tokens"), Some(Value::Null)) || max_tokens.is_some();
            let display_valid = match field("thinking_display") {
                None | Some(Value::Null) => true,
                Some(Value::String(value)) => value == "summarized" || value == "omitted",
                Some(_) => false,
            };
            if !max_tokens_valid || !display_valid {
                writer.reply_error(
                    request_id,
                    "set_max_thinking_tokens: max_thinking_tokens must be an integer or null and thinking_display must be \"summarized\", \"omitted\", or null",
                );
                return writer.finish();
            }
            let thinking = match max_tokens {
                Some(0) => llm_runtime::model::thinking::ThinkingConfig::Disabled,
                Some(budget_tokens) => {
                    llm_runtime::model::thinking::ThinkingConfig::Enabled { budget_tokens }
                }
                None => llm_runtime::model::thinking::ThinkingConfig::Adaptive,
            };
            orchestrator.set_thinking_config(thinking);
            orchestrator.set_thinking_display(field("thinking_display").and_then(Value::as_str));
            writer.reply_success(request_id, None);
        }
        "rename_session" => {
            let title = field("title").and_then(Value::as_str).unwrap_or("");
            if title.trim().is_empty() {
                writer.reply_error(request_id, "title must be non-empty");
                return writer.finish();
            }
            match orchestrator.rename_session(title.to_string()).await {
                Ok(()) => writer.reply_success(request_id, None),
                Err(err) => writer.reply_error(request_id, &format!("rename_session: {err}")),
            }
        }
        "mcp_status" => match orchestrator.sdk_mcp_status().await {
            Ok(status) => writer.reply_projected_payload(status),
            Err(error) => writer.reply_error(request_id, &error),
        },
        "mcp_message" => {
            let result = match (
                field("server_name").and_then(Value::as_str),
                request_projection.subprojection("/message"),
                control_plane.sdk_mcp_transport(),
            ) {
                (Some(server), Ok(message), Some(transport)) => transport.deliver(server, &message),
                _ => Err("Invalid MCP message".to_owned()),
            };
            match result {
                Ok(()) => writer.reply_success(request_id, None),
                Err(error) => writer.reply_error(request_id, &error),
            }
        }
        "mcp_read_resource" => {
            let result = match (
                field("serverName").and_then(Value::as_str),
                field("uri").and_then(Value::as_str),
            ) {
                (None, _) => Err("mcp_read_resource: serverName must be a string".to_owned()),
                (_, None) => Err("mcp_read_resource: uri must be a string".to_owned()),
                (Some(_), Some(uri)) if !uri.starts_with("ui://") => {
                    Err("mcp_read_resource: uri must use the ui:// scheme".to_owned())
                }
                (Some(server), Some(uri)) => orchestrator.read_sdk_mcp_resource(server, uri).await,
            };
            match result {
                Ok(payload) => writer.reply_projected_payload(payload),
                Err(error) => writer.reply_error(request_id, &error),
            }
        }
        "ui_attach" => match validate_ui_attach(&request_projection.value) {
            Ok((client, surface, viewport)) => {
                let answers = field("answers").and_then(Value::as_array).map(|answers| {
                    answers
                        .iter()
                        .filter_map(Value::as_str)
                        .filter_map(lingxi_core::host::ModRemoteUiAnswer::parse)
                        .collect()
                });
                let (surfaces, event) =
                    orchestrator.prepare_sdk_ui_surface_attach(client, surface, viewport, answers);
                writer.reply_success(request_id, Some(json!({"surfaces":surfaces})));
                writer.finish()?;
                spawn_ui_surface_event(
                    auxiliary_tasks,
                    orchestrator.clone(),
                    "session.attach",
                    event,
                );
            }
            Err(error) => writer.reply_error(request_id, error),
        },
        "ui_detach" => {
            if let Some(client) = field("client_id")
                .and_then(Value::as_str)
                .filter(|client| orchestrator::mod_surface_roster::valid_client_id(client))
            {
                let (detached, surfaces, event) =
                    orchestrator.prepare_sdk_ui_surface_detach(client);
                writer.reply_success(
                    request_id,
                    Some(json!({"detached":detached,"surfaces":surfaces})),
                );
                writer.finish()?;
                spawn_ui_surface_event(
                    auxiliary_tasks,
                    orchestrator.clone(),
                    "session.detach",
                    event,
                );
            } else {
                writer.reply_error(request_id, "ui_detach: client_id must be 1-64 of letters, digits, . _ - (the colon is the engine's)");
            }
        }
        "ui_render" | "ui_client_module" | "ui_message" | "ui_client_fault" | "ui_client_press"
        | "ui_press" | "ui_input" | "ui_select" => {
            let request =
                match hooks::mods::normalize_mod_ui_control_request(request_projection.clone()) {
                    Ok(request) => request,
                    Err(error) => {
                        writer.reply_error(request_id, &error.to_string());
                        return writer.finish();
                    }
                };
            // Native ui_client_module is synchronous. The other UI operations
            // are admitted tasks, so their Mod awaits cannot block Stop/cancel.
            if subtype == "ui_client_module" {
                match orchestrator.mod_ui_control(request).await {
                    Ok(outcome) => writer.reply_projected_payload(outcome.response),
                    Err(error) => writer.reply_error(request_id, &error.to_string()),
                }
            } else {
                let inbound = if subtype == "ui_render" {
                    // Validation above precedes replacement of an existing ID.
                    let guard = control_plane.begin_inbound_ui_request(&writer.request_id)?;
                    let event = orchestrator.prepare_sdk_ui_render_attach(&request);
                    spawn_ui_surface_event(
                        auxiliary_tasks,
                        orchestrator.clone(),
                        "session.attach",
                        event,
                    );
                    Some(guard)
                } else {
                    None
                };
                let orchestrator = orchestrator.clone();
                let reply = control_plane.outbound_writer();
                let request_id = writer.request_id.clone();
                let plane = control_plane.clone();
                let end_notify = end_notify.clone();
                let execution_errors = execution_errors.clone();
                auxiliary_tasks.push(tokio::spawn(async move {
                    // Cancellation suppresses the reply, but never drops the
                    // Mod owner future. Native hGe.run receives no abort signal.
                    let outcome = orchestrator.mod_ui_control(request).await;
                    let send_reply = || match outcome {
                        Ok(outcome) => {
                            reply.reply_success_projected(request_id, Some(outcome.response))
                        }
                        Err(error) => reply.reply_error_projected(request_id, &error.to_string()),
                    };
                    let result = match inbound {
                        Some(guard) => guard.reply_if_live(send_reply),
                        None => send_reply(),
                    };
                    if let Err(error) = result {
                        execution_errors
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(error.to_string());
                        plane.shutdown(&error.to_string()).await;
                        end_notify.notify_one();
                    }
                }));
            }
        }
        "get_context_usage" => {
            // §2.2 #9: token-budget breakdown (shape inferred — not byte-dumped).
            let (used, total) = orchestrator.context_window_usage().await;
            writer.reply_success(
                request_id,
                Some(json!({
                    "usedTokens": used,
                    "maxTokens": total
                })),
            );
        }
        "get_session_cost" => {
            // §2.2 #10: `{text}` (format inferred — not byte-dumped).
            let cost = orchestrator.snapshot_cost().await;
            writer.reply_success(
                request_id,
                Some(json!({"text": format!("Total cost: ${:.4}", cost.total_usd)})),
            );
        }
        "get_usage" => {
            // §2.2 #11: usage snapshot (shape inferred — not byte-dumped).
            let cost = orchestrator.snapshot_cost().await;
            writer.reply_success(
                request_id,
                Some(json!({
                    "input_tokens": cost.input_tokens,
                    "output_tokens": cost.output_tokens,
                    "cache_read_tokens": cost.cache_read_tokens,
                    "cache_creation_tokens": cost.cache_creation_tokens,
                    "total_tokens": cost.total_tokens
                })),
            );
        }
        "stop_task" => {
            // §2.2 #38: best-effort kill; not_found/not_running ⇒ success `{}`.
            if let Some(task_id) = field("task_id").and_then(|v| v.as_str()) {
                // The control-channel `stopTask` — claude-code's `source:"user"`
                // caller, which inherits `killedBy = "user"`.
                let _ = task_registry.kill_with_reason(task_id, "user").await;
            }
            writer.reply_success(request_id, Some(json!({})));
        }
        "background_tasks" => {
            // claude-code's SDK/bridge `background_tasks` request:
            // `if(D.toolUseId!==void 0){let ue=Ode(D.toolUseId,F);Xe(r,{backgrounded:ue})}
            //  else zM(F),Xe(r,{})`.
            //
            // `K4t` validation runs FIRST, before the disabled gate: absent,
            // null or empty means "background everything", a string means
            // "background that one tool call", and any other JSON type is a
            // hard error.
            enum Target {
                All,
                One(String),
                Invalid,
            }
            let target = match field("tool_use_id") {
                None | Some(Value::Null) => Target::All,
                Some(Value::String(id)) if id.is_empty() => Target::All,
                Some(Value::String(id)) => Target::One(id.clone()),
                Some(_) => Target::Invalid,
            };
            match target {
                Target::Invalid => {
                    writer.reply_error(request_id, "background_tasks: tool_use_id must be a string")
                }
                _ if lingxi_core::host::env::is_env_truthy(
                    host.environment_variable("LINGXI_DISABLE_BACKGROUND_TASKS")
                        .as_deref(),
                ) =>
                {
                    writer.reply_error(request_id, "Background tasks are disabled in this session.")
                }
                Target::One(id) => {
                    let backgrounded = task_registry.background_task_for_tool_use(&id).await;
                    writer.reply_success(request_id, Some(json!({"backgrounded": backgrounded})));
                }
                Target::All => {
                    task_registry.background_all_tasks().await;
                    writer.reply_success(request_id, Some(json!({})));
                }
            }
        }
        "register_repo_root" => {
            let request_value = frame.get("request").cloned().unwrap_or_else(|| json!({}));
            match serde_json::from_value::<lingxi_core::host::RegisterRepoRootRequest>(
                request_value,
            ) {
                Ok(request) if !request.path.trim().is_empty() => {
                    match orchestrator.register_repo_root(request).await {
                        Ok(outcome) => match serde_json::to_value(outcome) {
                            Ok(value) => writer.reply_success(request_id, Some(value)),
                            Err(error) => writer.reply_error(
                                request_id,
                                &format!("register_repo_root: failed to encode response: {error}"),
                            ),
                        },
                        Err(error) => {
                            writer.reply_error(request_id, &format!("register_repo_root: {error}"));
                        }
                    }
                }
                Ok(_) => {
                    writer.reply_error(request_id, "register_repo_root: path must not be empty")
                }
                Err(error) => writer.reply_error(
                    request_id,
                    &format!("register_repo_root: invalid request: {error}"),
                ),
            }
        }
        "set_cwd" => {
            // Move the live session to another directory. This crosses the
            // TRUST boundary — the target's files become readable and writable
            // under the session's rules — so an untrusted directory is
            // confirmed by the client before the move, via the
            // `needs_trust` → `trust_accepted` + `trusted_directory` echo
            // handshake. The decision (and every byte-exact rejection) lives in
            // `permission::set_cwd`; this arm only gathers the facts and
            // performs the move.
            let request = permission::set_cwd::SetCwdRequest {
                path: field("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                trust_accepted: field("trust_accepted").and_then(serde_json::Value::as_bool),
                trusted_directory: field("trusted_directory")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            };
            let trimmed = request.path.trim().to_string();
            let raw = std::path::PathBuf::from(&trimmed);
            let target = if raw.is_absolute() {
                raw
            } else {
                session_cwd.cwd().join(raw)
            };
            // Canonicalise when we can; a path that cannot be canonicalised is
            // reported at the path the user typed, not at a half-resolved one.
            let display = std::fs::canonicalize(&target).unwrap_or(target.clone());
            let display_str = display.to_string_lossy().into_owned();
            let resolved = if !display.exists() {
                permission::set_cwd::ResolvedPath::NotFound(display_str.clone())
            } else if display.is_dir() {
                permission::set_cwd::ResolvedPath::Directory(display_str.clone())
            } else {
                permission::set_cwd::ResolvedPath::NotADirectory(display_str.clone())
            };
            // A relocation must own the same lease as admitted operations,
            // not just sample the busy token before an async transcript move.
            let _cwd_operation = control_plane.try_lock_operation();
            let ctx = permission::set_cwd::SetCwdContext {
                resolved,
                current_cwd: session_cwd.cwd().to_string_lossy().into_owned(),
                // `Cd(…)` rules have no port representation yet, so no rule can
                // block. Left explicit rather than implied: when Cd rules land,
                // this is the one line that has to change.
                blocking_cd_rule: None,
                // No config path (no resolvable home) ⇒ nothing can be
                // recorded as trusted, so the handshake runs — fail closed.
                trusted: host.trust_config_path().is_some_and(|p| {
                    migrations::global_config::check_has_trust_dialog_accepted(&p, &display)
                }),
                project_root: permission::set_cwd::project_root_of(&display)
                    .map(|p| p.to_string_lossy().into_owned()),
                // The control channel is served off the turn loop, so a
                // concurrently running turn is exactly what this guards.
                busy: _cwd_operation.is_none() || control_plane.is_busy().await,
            };
            match permission::set_cwd::decide_set_cwd(&request, &ctx) {
                permission::set_cwd::SetCwdDecision::Respond(
                    permission::set_cwd::SetCwdResponse::Invalid(message),
                ) => writer.reply_error(request_id, &message),
                permission::set_cwd::SetCwdDecision::Respond(
                    permission::set_cwd::SetCwdResponse::Rejected { reason, message },
                ) => writer.reply_success(
                    request_id,
                    Some(json!({
                        "status": "rejected",
                        "reason": reason.as_str(),
                        "message": message,
                    })),
                ),
                permission::set_cwd::SetCwdDecision::Respond(
                    permission::set_cwd::SetCwdResponse::NeedsTrust {
                        directory,
                        trust_root,
                    },
                ) => {
                    let mut payload = serde_json::Map::new();
                    payload.insert("status".into(), json!("needs_trust"));
                    payload.insert("directory".into(), json!(directory));
                    // Omitted, not null, when there is nothing useful to offer.
                    if let Some(root) = trust_root {
                        payload.insert("trust_root".into(), json!(root));
                    }
                    writer.reply_success(request_id, Some(Value::Object(payload)));
                }
                permission::set_cwd::SetCwdDecision::Respond(
                    permission::set_cwd::SetCwdResponse::AlreadyThere { cwd },
                ) => writer.reply_success(
                    request_id,
                    Some(json!({
                        "status": "ok",
                        "cwd": cwd,
                        "changed": false,
                        "transcript_relocated": true,
                    })),
                ),
                permission::set_cwd::SetCwdDecision::Proceed {
                    directory,
                    mark_trusted,
                } => {
                    // Record the trust BEFORE the move, so a crash in between
                    // leaves a trusted directory the user did approve rather
                    // than a session sitting in one it never confirmed.
                    if mark_trusted {
                        if let Some(cfg) = host.trust_config_path() {
                            migrations::global_config::record_trust_accept(&cfg, &display);
                        }
                    }
                    let dir = std::path::PathBuf::from(&directory);
                    let previous = session_cwd.snapshot();
                    // The new cwd becomes the SOLE trusted directory, matching
                    // what `EnterWorktree` and the worktree restore already do.
                    // Any `--add-dir` extras are dropped rather than carried
                    // across: narrowing a trust boundary on a move is the safe
                    // direction, and inheriting the old session's extras into a
                    // directory the user has just been asked to trust would
                    // grant more than the prompt described.
                    let trusted = vec![dir.clone()];
                    session_cwd.swap(dir.clone(), trusted);
                    let transcript_path = match orchestrator.retarget_transcript_for_cwd(&dir).await
                    {
                        Ok(path) => path,
                        Err(error) => {
                            // Do not acknowledge a cwd move if its transcript could
                            // not be rehomed. Restore both cwd and trusted roots so
                            // a subsequent request cannot run against a different
                            // directory while still reading the old session file.
                            session_cwd.swap(previous.0, previous.1);
                            tracing::warn!(%error, "failed to retarget transcript after set_cwd; rolled back cwd");
                            writer.reply_error(
                                request_id,
                                &format!("Could not change directory: {error}"),
                            );
                            return writer.finish();
                        }
                    };
                    if let Some(path) = transcript_path {
                        if let Err(error) = host.refresh_launch_identity(&dir, &path) {
                            let body = rollback_cd_after_launch_identity_failure(
                                orchestrator,
                                session_cwd,
                                previous,
                                &error.to_string(),
                                host,
                            )
                            .await;
                            tracing::warn!(
                                %error,
                                "rejected set_cwd because background launch identity is stale"
                            );
                            writer.reply_error(request_id, &body);
                            return writer.finish();
                        }
                    }
                    writer.reply_success(
                        request_id,
                        Some(json!({
                            "status": "ok",
                            "cwd": directory,
                            "changed": true,
                            "transcript_relocated": true,
                        })),
                    );
                }
            }
        }
        "set_permission_mode" => {
            // §2.2 #4: the net-new runtime mode-mutation surface. The gate
            // parses + validates the wire mode and applies it live; success
            // echoes `{mode}`, an invalid/disallowed mode returns an error frame.
            let mode = field("mode").and_then(|v| v.as_str()).unwrap_or("default");
            match orchestrator.set_permission_mode(mode).await {
                Ok(()) => {
                    host.remember_permission_mode(mode);
                    writer.reply_success(request_id, Some(json!({"mode": mode})));
                }
                Err(e) => writer.reply_error(request_id, &e),
            }
        }
        "set_mcp_permission_mode_override" => {
            let server_name = field("serverName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mode = match field("mode") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if s == "default" || s == "auto" => Some(s.as_str()),
                Some(Value::String(s))
                    if matches!(
                        s.as_str(),
                        "acceptEdits"
                            | "auto"
                            | "bypassPermissions"
                            | "default"
                            | "dontAsk"
                            | "plan"
                    ) =>
                {
                    writer.reply_error(
                        request_id,
                        &format!(
                            "Permission mode override over the control channel is tighten-only ('default', 'auto', or null); rejected '{s}'"
                        ),
                    );
                    return writer.finish();
                }
                _ => {
                    writer.reply_error(
                        request_id,
                        "Cannot set permission mode: must be one of acceptEdits, auto, bypassPermissions, default, dontAsk, plan",
                    );
                    return writer.finish();
                }
            };
            let known = orchestrator
                .list_mcp_servers()
                .await
                .into_iter()
                .any(|s| s.name == server_name);
            if !known {
                if let Err(e) = orchestrator
                    .set_mcp_permission_mode_override(&server_name, mode)
                    .await
                {
                    writer.reply_error(request_id, &e);
                    return writer.finish();
                }
                let warning = match mode {
                    Some(_) => format!(
                        "MCP server '{server_name}' is not yet known; override stored but will not apply until a server with that exact name connects."
                    ),
                    None => format!(
                        "MCP server '{server_name}' is not known; no override was present to clear."
                    ),
                };
                writer.reply_success(request_id, Some(json!({"warning": warning})));
                return writer.finish();
            }

            match orchestrator
                .set_mcp_permission_mode_override(&server_name, mode)
                .await
            {
                Ok(()) => writer.reply_success(request_id, None),
                Err(e) => writer.reply_error(request_id, &e),
            }
        }
        "end_session" => {
            // Cancel the actual owner too: idle notification turns do not use
            // the normal input turn's watch bridge.
            control_plane.shutdown("Session ended").await;
            let _ = cancel_tx.send(true);
            writer.reply_success(request_id, None);
            end_notify.notify_one();
        }
        "file_suggestions" => {
            let query = field("query").and_then(Value::as_str).unwrap_or("");
            let suggestions = file_suggestions
                .suggestions(&session_cwd.cwd(), query)
                .await
                .into_iter()
                .map(|path| json!({"path": path}))
                .collect::<Vec<_>>();
            writer.reply_success(request_id, Some(json!({"suggestions": suggestions})));
        }
        "seed_read_state" => {
            if let (Some(path), Some(mtime)) = (
                field("path").and_then(Value::as_str),
                field("mtime").and_then(Value::as_f64),
            ) {
                let _ = orchestrator.seed_read_state_from_host(path, mtime).await;
            }
            writer.reply_success(request_id, None);
        }
        "mcp_authenticate" | "mcp_reconnect" => {
            // ORACLE (2.1.201 `-p` handler): both branches first resolve the
            // MCP server config by `serverName`; when no server matches, they
            // reply `error: "Server not found: {serverName}"` (verified live —
            // `mcp_authenticate`/`mcp_reconnect` for an unknown server both
            // return that exact string). A fresh `-p` session has no MCP
            // servers, so this is the dominant observable path.
            //
            // DEFERRED (found-server path): the live handler then starts an
            // OAuth flow (mcp_authenticate → `{authUrl, requiresUserAction,…}`)
            // or tears down + reconnects the transport (mcp_reconnect → bare
            // success ack). The port's stream-json server has no live OAuth /
            // reconnect seam wired here, so a matched server is acked
            // best-effort: `mcp_reconnect` → bare success (mirrors the binary's
            // `Ur(_t)`), `mcp_authenticate` → success `{}`. Full flows tracked
            // as a follow-up.
            let server_name = field("serverName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let known: Vec<String> = orchestrator
                .list_mcp_servers()
                .await
                .into_iter()
                .map(|s| s.name)
                .collect();
            if !known.iter().any(|n| n == &server_name) {
                writer.reply_error(request_id, &format!("Server not found: {server_name}"));
            } else if subtype == "mcp_reconnect" {
                writer.reply_success(request_id, None);
            } else {
                writer.reply_success(request_id, Some(json!({})));
            }
        }
        // The orchestrator-free arms (get_binary_version, message_rated,
        // mcp_oauth_callback_url), the CLI-originated guard subtypes (no-reply),
        // and the byte-exact `Unsupported control request subtype` fallthrough
        // are pure — classified by `pure_control_response` so the wire shapes
        // are unit-testable without a live orchestrator.
        other => match pure_control_response(other, frame) {
            PureControlReply::Success(payload) => writer.reply_success(request_id, payload),
            PureControlReply::Error(msg) => writer.reply_error(request_id, &msg),
            // CLI-originated subtype seen inbound — no control_response (see below).
            PureControlReply::Ignore => {}
        },
    }
    writer.finish()
}

/// Reply for a pure (orchestrator-free) control arm.
#[derive(Debug, PartialEq)]
pub(super) enum PureControlReply {
    /// `control_response` success; `None` ⇒ inner `response` key omitted.
    Success(Option<serde_json::Value>),
    /// `control_response` error with this message.
    Error(String),
    /// No `control_response` at all — a CLI-originated subtype seen inbound that
    /// the binary handles as a top-of-chain guard, never in the server switch.
    Ignore,
}

/// Classify the control arms that need no async orchestrator/registry access,
/// including the byte-exact `Unsupported control request subtype` fallthrough.
///
pub(super) fn pure_control_response(subtype: &str, frame: &serde_json::Value) -> PureControlReply {
    let field = |k: &str| frame.get("request").and_then(|r| r.get(k));
    match subtype {
        // §2.2 #8: `{version, buildTime}`.
        "get_binary_version" => PureControlReply::Success(Some(json!({
            "version": lingxi_core::host::CLAUDE_CODE_VERSION,
            "buildTime": ""
        }))),
        // §2.2 #45: telemetry-only; ack with `{}`.
        "message_rated" => PureControlReply::Success(Some(json!({}))),
        // ORACLE (2.1.201 `-p` handler): `mcp_oauth_callback_url` looks up the
        // in-flight OAuth flow for `serverName`; with no active flow it replies
        // `error: "No active OAuth flow for server: {serverName}"` (verified
        // live). The port keeps no active-flow registry in the stream-json
        // server, so this is always the faithful reply.
        "mcp_oauth_callback_url" => {
            let server_name = field("serverName").and_then(|v| v.as_str()).unwrap_or("");
            PureControlReply::Error(format!("No active OAuth flow for server: {server_name}"))
        }
        // CLI-ORIGINATED subtypes: `can_use_tool` / `request_user_dialog` /
        // `elicitation` are CLIENT→SERVER frames the CLI itself SENDS (their
        // `control_response` is handled by the resolver task). The binary checks
        // them as top-of-chain GUARDS routed to the StructuredIO pending-request
        // path — they NEVER enter this server switch nor reach the Unsupported
        // fallthrough. A well-behaved host never sends them inbound as a
        // control_request, so we emit NO control_response rather than erroring.
        "can_use_tool" | "request_user_dialog" | "elicitation" => PureControlReply::Ignore,
        // The binary fallthrough for every unhandled / deep [D] subtype.
        _ => PureControlReply::Error(format!("Unsupported control request subtype: {subtype}")),
    }
}

/// Build the `initialize` control_response `response` payload.
///
/// ORACLE (2.1.201, verified live via
/// `{"subtype":"initialize"} | claude -p --input-format stream-json \
///   --output-format stream-json --verbose`): the `-p` handler replies with
/// `{commands, agents, output_style, available_output_styles, models, account,
/// pid}` where `output_style` is `"default"` and `available_output_styles` is
/// the 4-item list `["default","Proactive","Explanatory","Learning"]`.
/// 2.1.220 (live re-capture) extends the tail with the remote-control gate
/// booleans and `fast_mode_state` / `fast_mode_disabled_reason`.
/// (NOTE: the separate REPL-bridge handler defaults these to `"normal"` /
/// `["normal"]`, but that bridge is NOT the `-p --input-format stream-json`
/// role this dispatcher models — the observable `-p` truth is the 4-item list.)
pub(super) fn initialize_response_payload(
    commands: &[serde_json::Value],
    agents: &[serde_json::Value],
    models: &[serde_json::Value],
    account: &serde_json::Value,
    pid: u32,
    fast_mode_state: &str,
    fast_mode_disabled_reason: Option<&str>,
) -> serde_json::Value {
    // 2.1.220 live capture appends five keys after `pid`:
    // `remote_control_auto_enable`, `remote_control_auto_on_by_default`,
    // `ide_rc_auto_enable_gate` (all `false` in a clean sandbox — LingXi has
    // no remote-control feature, an accepted divergence, so `false` is always
    // truthful), then `fast_mode_state` + optional `fast_mode_disabled_reason`.
    let mut payload = json!({
        "commands": commands,
        "agents": agents,
        "output_style": "default",
        "available_output_styles": ["default", "Proactive", "Explanatory", "Learning"],
        "models": models,
        "account": account,
        "pid": pid,
        "remote_control_auto_enable": false,
        "remote_control_auto_on_by_default": false,
        "ide_rc_auto_enable_gate": false,
        "fast_mode_state": fast_mode_state,
    });
    if let Some(reason) = fast_mode_disabled_reason {
        payload["fast_mode_disabled_reason"] = json!(reason);
    }
    payload
}

/// Map the raw inner `control_response.response` permission payload onto a
/// [`permission::gate::PermissionOutcome`] for orphaned-tool recovery.
///
/// Mirrors the allow/deny shape of `StdioControlPermissionGate::map_payload`
/// but is deliberately LENIENT where the live gate is strict: an `allow`
/// WITHOUT `updatedInput` is honoured (falling back to the original tool input)
/// rather than rejected — matching claude-code's `handleOrphanedPermission`,
/// which logs a warning and uses the original input when `updatedInput` is
/// `undefined` (queryHelpers.ts:262-272), instead of `map_payload`'s strict
/// §3.3 "missing updatedInput" deny used for live responses.
pub(super) fn orphan_decision_from_payload(
    projection: &Utf16JsonProjection,
) -> permission::gate::PermissionOutcome {
    use permission::gate::PermissionOutcome;
    let payload = &projection.value;
    match payload.get("behavior").and_then(serde_json::Value::as_str) {
        Some("allow") => {
            // Carry `updatedInput` only when it is a non-empty object (claude-code
            // applies it "when it has keys"); otherwise fall back to the original.
            let updated_input = match payload.get("updatedInput") {
                Some(serde_json::Value::Object(m)) if !m.is_empty() => {
                    projection.subprojection("/updatedInput").ok()
                }
                _ => None,
            };
            let permission_updates = payload
                .get("updatedPermissions")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let decision_classification = payload
                .get("decisionClassification")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| match value {
                    "user_temporary" => {
                        Some(lingxi_core::host::permission_gate::ToolDecisionClassification::UserTemporary)
                    }
                    "user_permanent" => {
                        Some(lingxi_core::host::permission_gate::ToolDecisionClassification::UserPermanent)
                    }
                    "user_reject" => {
                        Some(lingxi_core::host::permission_gate::ToolDecisionClassification::UserReject)
                    }
                    _ => None,
                });
            PermissionOutcome::Allow {
                updated_input,
                permission_updates,
                decision_classification,
            }
        }
        Some("deny") => PermissionOutcome::Deny {
            reason: payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Tool permission denied")
                .to_string(),
        },
        // Any non-allow/deny behaviour is a schema-invalid result; deny safely
        // rather than execute on a malformed recovered decision.
        _ => PermissionOutcome::Deny {
            reason: "Tool permission request failed: malformed orphaned control_response"
                .to_string(),
        },
    }
}

/// Re-run a single ORPHANED tool: dequeued between turns, this looks the
/// unresolved `tool_use` up in the (resumed) session history and executes it
/// with the recovered permission decision. 1:1 with claude-code's
/// `handleOrphanedPermission` (queryHelpers.ts:224-343). Deduped per-toolUseID
/// via `handled_orphans` (twin of `handledOrphanedToolUseIds`, print.ts:2766):
/// a given id recovers once, but DISTINCT orphans each recover. An id is marked
/// handled ONLY on a real recovery (`Ok(true)`, which also covers the
/// unknown-tool case where the gate is consumed but nothing runs), so a
/// not-found orphan (`Ok(false)`) leaves a later same-id delivery able to
/// recover — matching claude-code, which adds to the Set only when
/// `findUnresolvedToolUse` succeeds.
pub(super) async fn recover_orphaned_permission(
    runtime: &Runtime,
    cmd: crate::headless::control_plane::OrphanedPermission,
    handled_orphans: &mut std::collections::HashSet<lingxi_core::types::ToolUseId>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let crate::headless::control_plane::OrphanedPermission {
        tool_use_id,
        permission_decision,
    } = cmd;
    if handled_orphans.contains(&tool_use_id) {
        tracing::debug!(
            "ignoring duplicate orphaned permission for toolUseID={} (already handled)",
            tool_use_id.as_str()
        );
        return;
    }
    let decision = orphan_decision_from_payload(&permission_decision);
    match runtime
        .orchestrator
        .run_orphaned_permission_with_cancel(&tool_use_id, decision, cancel)
        .await
    {
        Ok(true) => {
            handled_orphans.insert(tool_use_id.clone());
            tracing::info!(
                "recovered orphaned permission for toolUseID={}",
                tool_use_id.as_str()
            );
        }
        Ok(false) => {
            tracing::debug!(
                "orphaned permission toolUseID={} had no unresolved tool_use; skipped",
                tool_use_id.as_str()
            );
        }
        Err(e) => {
            tracing::warn!(
                "orphaned permission recovery failed for toolUseID={}: {e}",
                tool_use_id.as_str()
            );
        }
    }
}

/// Port of the 2.1.219 fast-mode reason resolver `JW()` (binary @227890982),
/// narrowed to the inputs reachable on the print/stream-json surface:
///
/// ```js
/// function JW(e){
///   if(!El())return xn()!=="firstParty"?"not_first_party":"disabled_by_env";
///   if(Ke("tengu_penguins_off",null)!==null)return"unknown";
///   if(!Hl(jkt())){…}                                    // model_not_allowed
///   let t=Hr("flagSettings")?.fastMode===!0;
///   if(_n()&&LVt()&&!t)return"sdk_opt_in_required";
///   if(mB.status==="pending"&&…)return"pending";
///   if(mB.status==="disabled"&&…)return mB.reason;       // free|preference|…
///   return null}
/// ```
///
/// * `El()` = firstParty provider && `!CLAUDE_CODE_DISABLE_FAST_MODE` (raw JS
///   truthiness — any non-empty value disables).
/// * `tengu_penguins_off` is a dynamic-config STRING read (`Ke(key,null)`);
///   with no fetcher wired the shipped binary resolves `null` there too, so
///   the port's flag-absent default falls through identically.
/// * `Hl` (org allowed-models policy) has no port surface — managed
///   `allowedModels` is unported, so `model_not_allowed` is unreachable.
/// * `_n()&&LVt()` — the SDK/non-interactive entrypoint check — is
///   constitutively TRUE here: this resolver only runs on the `-p`
///   stream-json/json paths, which ARE the Agent-SDK surface.
/// * The availability prober (`mB`) is unported; its `pending` and
///   `free|preference|extra_usage_disabled|network_error|unknown` arms are
///   unreachable, matching the fall-through `null` of an active status.
pub(super) fn resolve_fast_mode_disabled_reason(
    first_party: bool,
    sdk_fast_mode_opt_in: bool,
    host: &dyn crate::headless::host::HeadlessHostServices,
) -> Option<&'static str> {
    if !first_party {
        return Some("not_first_party");
    }
    if host
        .environment_variable(branding::DISABLE_FAST_MODE_ENV)
        .is_some_and(|v| !v.is_empty())
    {
        return Some("disabled_by_env");
    }
    if !sdk_fast_mode_opt_in {
        return Some("sdk_opt_in_required");
    }
    None
}

/// Port of `cK(model, fastModeOptIn)` (binary @227895153) — the
/// `fast_mode_state` carried by `system/init`, the `initialize`
/// control_response and every `result` frame:
///
/// ```js
/// function cK(e,t){let r=El()&&QN()&&!!t&&fE(e);
///   if(r&&z0e())return"cooldown";if(r)return"on";return"off"}
/// ```
///
/// * `QN()` is `El()&&fde(undefined)===null` i.e. `El()&&JW()===null`, so
///   `El()&&QN()` collapses to "the disabled reason resolved to null" — the
///   value this function is handed.
/// * `fE(model)` (@227892311) is the canonical registry's `fast_mode`
///   capability. The UI state, initialize response, and request path all
///   consume the same table.
/// * `z0e()` (`"cooldown"`) rides the unported availability prober `mB` — the
///   same dead arm as `JW`'s `pending` / `disabled` branches.
pub(super) fn resolve_fast_mode_state(
    model: &str,
    fast_mode_disabled_reason: Option<&str>,
    sdk_fast_mode_opt_in: bool,
) -> &'static str {
    let model_supports_fast_mode = lingxi_core::host::model_capabilities::has_capability(
        model,
        lingxi_core::host::model_capabilities::ModelCapability::FastMode,
    );
    if fast_mode_disabled_reason.is_none() && sdk_fast_mode_opt_in && model_supports_fast_mode {
        "on"
    } else {
        "off"
    }
}

/// `Hr("flagSettings")?.fastMode===!0` — the Agent-SDK fast-mode opt-in
/// carried by `--settings` (inline JSON or a settings-file path). Strictly
/// boolean `true`, like the oracle's `===!0`.
pub(super) fn flag_settings_fast_mode_opt_in(settings: Option<&str>) -> bool {
    let Some(raw) = settings else { return false };
    let trimmed = raw.trim();
    let text = if trimmed.starts_with('{') {
        trimmed.to_string()
    } else {
        match std::fs::read_to_string(trimmed) {
            Ok(t) => t,
            Err(_) => return false,
        }
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("fastMode").and_then(serde_json::Value::as_bool))
        == Some(true)
}

/// Map a model's `request_model` string to its capability flags.
///
/// Returns `(supportsEffort, supportedEffortLevels, supportsAdaptiveThinking,
///           supportsFastMode, supportsAutoMode)`.
///
/// Refreshed to the 2.1.220 registry truth. The binary's initialize models
/// builder (print.ts @223434963) computes per-row: effort = `iw` (registry
/// "effort" capability), levels = `UR = [low,medium,high,xhigh,max]` filtered
/// by `BIe` ("max_effort") and `Zne` ("xhigh_effort", which additionally
/// EXCLUDES opus-4-6/sonnet-4-6 by name), adaptive = `Vit`
/// ("adaptive_thinking"), fast = `_h` (registry "fast_mode" or the
/// opus-4-7/opus-4-8 pair, gated on firstParty via `lc()`), auto = `mTe`
/// (true on firstParty for every non-legacy model). Capability sets come from
/// the baked-in catalog (binary blob @207769000..207775500). Legacy Claude
/// ids (claude-3-*, opus-4-0/4-1/4-5, sonnet-4-0/4-5, haiku-4-5) are the
/// shared exclusion list in all four predicates → all-false. Unknown /
/// non-Anthropic models keep all-false / empty defaults (multi-provider
/// divergence: the binary's `RN(Fh(e))` non-1P fallback has no lingxi seam).
pub(super) fn model_capabilities(
    request_model: &str,
) -> (bool, Vec<&'static str>, bool, bool, bool) {
    // The "default" pseudo-model: the binary computes capabilities on the
    // RESOLVED model (`r = R_()` for the Default row); lingxi's default
    // resolves to claude-sonnet-5 (2.1.197/198, M1).
    if request_model.eq_ignore_ascii_case("default") {
        return model_capabilities("claude-sonnet-5");
    }
    let capabilities =
        lingxi_core::host::model_capabilities::initialization_capabilities_for(request_model);
    (
        capabilities.supports_effort,
        capabilities.supported_effort_levels.to_vec(),
        capabilities.supports_adaptive_thinking,
        capabilities.supports_fast_mode,
        capabilities.supports_auto_mode,
    )
}

/// Restore transcript and cwd together after a host launch-identity failure.
async fn rollback_cd_after_launch_identity_failure(
    orch: &Arc<orchestrator::ConversationOrchestrator>,
    session_cwd: &Arc<tool_api::SessionCwd>,
    previous: (std::path::PathBuf, Vec<std::path::PathBuf>),
    refresh_error: &str,
    host: &dyn crate::headless::host::HeadlessHostServices,
) -> String {
    match orch.retarget_transcript_for_cwd(&previous.0).await {
        Ok(_) => {
            session_cwd.swap(previous.0, previous.1);
            format!("Could not change directory: {refresh_error}")
        }
        Err(rollback_error) => {
            host.launch_identity_rollback_failed(&format!(
                "background launch identity refresh failed and transcript rollback failed: {refresh_error}; rollback: {rollback_error}"
            ));
            tracing::error!(%refresh_error, %rollback_error, "failed to roll back /cd after background launch identity refresh failure; job disabled");
            format!("Could not change directory: {refresh_error}; background session disabled because transcript rollback failed: {rollback_error}")
        }
    }
}

fn spawn_ui_surface_event(
    owner: &Arc<super::PrintAuxTaskGroup>,
    orchestrator: Arc<orchestrator::ConversationOrchestrator>,
    event_name: &'static str,
    event: Option<Value>,
) {
    if event.is_some() {
        owner.push(tokio::spawn(async move {
            orchestrator
                .dispatch_sdk_ui_surface_event(event_name, event)
                .await;
        }));
    }
}

/// A response writer bound to the input frame's exact JavaScript identity.
struct RequestReplyWriter<'a> {
    writer: &'a ControlPlaneWriter,
    delivery_error: std::sync::Mutex<Option<crate::headless::stream_json_input::InputError>>,
    request_id: Utf16JsonProjection,
}

fn validate_ui_attach(
    request: &Value,
) -> Result<(&str, orchestrator::config::ModRenderSurface, Option<Value>), &'static str> {
    const INVALID: &str = "ui_attach: surface must be \"desktop\", \"mobile\" or \"vscode\", client_id 1-64 of letters, digits, . _ - (the colon is the engine's), viewport (when given) positive integer columns and rows with isFullscreen (when given) a boolean, and answers (when given) a list of \"ui_copy\", \"ui_prompt_read\", \"ui_prompt_fill\", \"ui_prompt_suggest\", \"ui_read_selection\"";
    let surface = match request.get("surface").and_then(Value::as_str) {
        Some("desktop") => orchestrator::config::ModRenderSurface::Desktop,
        Some("mobile") => orchestrator::config::ModRenderSurface::Mobile,
        Some("vscode") => orchestrator::config::ModRenderSurface::Vscode,
        _ => return Err(INVALID),
    };
    let client = request
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|client| orchestrator::mod_surface_roster::valid_client_id(client))
        .ok_or(INVALID)?;
    let viewport = match request.get("viewport") {
        None => None,
        Some(value)
            if value.is_object()
                && ["columns", "rows"].iter().all(|key| {
                    value
                        .get(key)
                        .and_then(Value::as_f64)
                        .is_some_and(|number| number > 0.0 && number.fract() == 0.0)
                })
                && value.get("isFullscreen").is_none_or(Value::is_boolean) =>
        {
            let mut viewport = json!({"columns":value["columns"],"rows":value["rows"]});
            if let Some(fullscreen) = value.get("isFullscreen") {
                viewport["isFullscreen"] = fullscreen.clone();
            }
            Some(viewport)
        }
        Some(_) => return Err(INVALID),
    };
    if let Some(answers) = request.get("answers") {
        let Some(answers) = answers.as_array() else {
            return Err(INVALID);
        };
        if answers.len() > 5
            || !answers.iter().all(|answer| {
                matches!(
                    answer.as_str(),
                    Some(
                        "ui_copy"
                            | "ui_prompt_read"
                            | "ui_prompt_fill"
                            | "ui_prompt_suggest"
                            | "ui_read_selection"
                    )
                )
            })
        {
            return Err(INVALID);
        }
    }
    Ok((client, surface, viewport))
}

impl RequestReplyWriter<'_> {
    fn record(&self, result: Result<(), crate::headless::stream_json_input::InputError>) {
        if let Err(error) = result {
            *self
                .delivery_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
        }
    }
    fn finish(&self) -> Result<(), crate::headless::stream_json_input::InputError> {
        self.delivery_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .map_or(Ok(()), Err)
    }
    fn reply_projected_payload(&self, payload: Utf16JsonProjection) {
        self.record(
            self.writer
                .reply_success_projected(self.request_id.clone(), Some(payload)),
        );
    }
    fn reply_initialize(
        &self,
        payload: Value,
        permissions: Vec<Utf16JsonProjection>,
        dialogs: Vec<Utf16JsonProjection>,
    ) {
        let mut response = Utf16JsonProjection::plain(json!({"subtype":"success"}));
        response
            .set_field("request_id", self.request_id.clone())
            .expect("valid request identity");
        response
            .set_field("response", Utf16JsonProjection::plain(payload))
            .expect("valid initialize payload");
        response
            .set_field("pending_permission_requests", projected_array(permissions))
            .expect("valid pending permission projections");
        response
            .set_field("pending_user_dialog_requests", projected_array(dialogs))
            .expect("valid pending dialog projections");
        let mut frame = Utf16JsonProjection::plain(json!({"type":"control_response"}));
        frame
            .set_field("response", response)
            .expect("valid initialization response");
        self.record(self.writer.send_projected(&frame));
    }
    fn reply_success(&self, _request_id: &str, payload: Option<Value>) {
        self.record(self.writer.reply_success_projected(
            self.request_id.clone(),
            payload.map(Utf16JsonProjection::plain),
        ));
    }

    fn reply_error(&self, _request_id: &str, message: &str) {
        self.record(
            self.writer
                .reply_error_projected(self.request_id.clone(), message),
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn attach_viewport_is_canonical_and_accepts_positive_integer_numbers() {
        let (_, _, viewport) = super::validate_ui_attach(&serde_json::json!({"surface":"mobile","client_id":"phone","viewport":{"columns":80.0,"rows":24,"isFullscreen":false,"unrecognized":true}})).unwrap();
        assert_eq!(
            viewport.unwrap(),
            serde_json::json!({"columns":80.0,"rows":24,"isFullscreen":false})
        );
        assert!(super::validate_ui_attach(&serde_json::json!({"surface":"mobile","client_id":"phone","viewport":{"columns":80.5,"rows":24}})).is_err());
    }

    use super::*;
    #[tokio::test]
    async fn request_reply_retains_utf16_identity_and_native_key_order() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let writer = ControlPlaneWriter::new(Arc::new(tx));
        let request_id = Utf16JsonProjection::parse(r#""\ud800""#).unwrap();
        let reply = RequestReplyWriter {
            writer: &writer,
            request_id,
            delivery_error: std::sync::Mutex::new(None),
        };
        reply.reply_error("display only", "unsupported");
        let Some(crate::headless::stream_json::OutboundMsg::Line(line)) = rx.recv().await else {
            panic!("expected reply");
        };
        assert_eq!(line, "{\"type\":\"control_response\",\"response\":{\"subtype\":\"error\",\"request_id\":\"\\ud800\",\"error\":\"unsupported\"}}\n");
    }

    fn apply(field: Option<serde_json::Value>) -> Option<String> {
        match resolve_set_model_target(field.as_ref(), "session-default") {
            SetModelTarget::Apply(t) => Some(t),
            SetModelTarget::Reject => None,
        }
    }

    #[test]
    fn set_model_absent_or_null_applies_session_default() {
        // CC `model ?? "default"`: both absent and explicit JSON null collapse
        // to the session default — never a type error.
        assert_eq!(apply(None).as_deref(), Some("session-default"));
        assert_eq!(
            apply(Some(serde_json::Value::Null)).as_deref(),
            Some("session-default")
        );
    }

    #[test]
    fn set_model_default_is_case_insensitive_and_trimmed() {
        // CC `or.trim().toLowerCase() === "default"`.
        for s in [
            "default",
            "DEFAULT",
            "Default",
            "  default  ",
            "\tdefault\n",
        ] {
            assert_eq!(
                apply(Some(serde_json::Value::String(s.into()))).as_deref(),
                Some("session-default"),
                "{s:?} must resolve to the session default"
            );
        }
    }

    #[test]
    fn set_model_named_model_passes_raw_string() {
        // CC `vr = Jr ? SE() : or` — the raw requested string, untrimmed.
        assert_eq!(
            apply(Some(serde_json::json!("claude-opus-4"))).as_deref(),
            Some("claude-opus-4")
        );
    }

    #[test]
    fn set_model_non_string_non_null_is_rejected() {
        // CC `if(fr!=null && typeof fr!=="string")` → reject.
        assert!(apply(Some(serde_json::json!(42))).is_none());
        assert!(apply(Some(serde_json::json!(true))).is_none());
        assert!(apply(Some(serde_json::json!({"a": 1}))).is_none());
        assert!(apply(Some(serde_json::json!(["x"]))).is_none());
    }

    fn req(subtype: &str, body: serde_json::Value) -> serde_json::Value {
        let mut request = body;
        request["subtype"] = json!(subtype);
        json!({"type": "control_request", "request_id": "r1", "request": request})
    }

    #[test]
    fn pure_unknown_subtype_falls_through_byte_exact() {
        let frame = req("totally_made_up", json!({}));
        assert_eq!(
            pure_control_response("totally_made_up", &frame),
            PureControlReply::Error(
                "Unsupported control request subtype: totally_made_up".to_string()
            )
        );
    }

    #[test]
    fn pure_cli_originated_subtypes_are_ignored_not_unsupported() {
        // #5: an inbound control_request for a CLI-originated subtype is a guard
        // case — no control_response, NOT an Unsupported error.
        for st in ["can_use_tool", "request_user_dialog", "elicitation"] {
            let frame = req(st, json!({}));
            assert_eq!(
                pure_control_response(st, &frame),
                PureControlReply::Ignore,
                "{st} must be ignored (top-of-chain guard), not Unsupported"
            );
        }
    }

    #[test]
    fn pure_mcp_oauth_callback_url_no_active_flow() {
        // ORACLE 2.1.201 `-p`: no in-flight OAuth flow ⇒ byte-exact error.
        let frame = req(
            "mcp_oauth_callback_url",
            json!({ "serverName": "s1", "callbackUrl": "http://x?code=1" }),
        );
        assert_eq!(
            pure_control_response("mcp_oauth_callback_url", &frame),
            PureControlReply::Error("No active OAuth flow for server: s1".to_string())
        );
    }

    #[test]
    fn initialize_payload_output_style_defaults_match_p_oracle() {
        // ORACLE 2.1.201 `-p`: output_style "default" + the 4-item list.
        // Locks the `-p` truth (NOT the REPL-bridge "normal"/["normal"]).
        // 2.1.220 re-capture appends the remote-control gates + fast-mode tail
        // (covered exhaustively by `initialize_payload_tail_matches_2_1_220`).
        let payload = initialize_response_payload(&[], &[], &[], &json!({}), 4242, "off", None);
        assert_eq!(payload["output_style"], "default");
        assert_eq!(
            payload["available_output_styles"],
            json!(["default", "Proactive", "Explanatory", "Learning"])
        );
        assert_eq!(payload["pid"], 4242);
        // Exact top-level key set (no fabricated keys).
        let keys: Vec<&str> = payload
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(
            keys,
            vec![
                "commands",
                "agents",
                "output_style",
                "available_output_styles",
                "models",
                "account",
                "pid",
                "remote_control_auto_enable",
                "remote_control_auto_on_by_default",
                "ide_rc_auto_enable_gate",
                "fast_mode_state",
            ]
        );
    }

    #[test]
    fn pure_get_binary_version_shape() {
        let frame = req("get_binary_version", json!({}));
        let PureControlReply::Success(Some(payload)) =
            pure_control_response("get_binary_version", &frame)
        else {
            panic!("expected success payload");
        };
        assert_eq!(payload["version"], lingxi_core::host::CLAUDE_CODE_VERSION);
        assert!(payload.get("buildTime").is_some());
    }

    #[test]
    fn initialize_payload_tail_matches_2_1_220() {
        let p = initialize_response_payload(
            &[],
            &[],
            &[],
            &json!({}),
            42,
            "off",
            Some("sdk_opt_in_required"),
        );
        let keys: Vec<&str> = p.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "commands",
                "agents",
                "output_style",
                "available_output_styles",
                "models",
                "account",
                "pid",
                "remote_control_auto_enable",
                "remote_control_auto_on_by_default",
                "ide_rc_auto_enable_gate",
                "fast_mode_state",
                "fast_mode_disabled_reason",
            ],
        );
        assert_eq!(p["fast_mode_state"], "off");
        assert_eq!(p["fast_mode_disabled_reason"], "sdk_opt_in_required");

        let bare = initialize_response_payload(&[], &[], &[], &json!({}), 42, "off", None);
        assert!(
            !bare
                .as_object()
                .unwrap()
                .contains_key("fast_mode_disabled_reason"),
            "no reason ⇒ key omitted"
        );
    }

    #[test]
    fn flag_settings_fast_mode_opt_in_parses_inline_and_file() {
        assert!(flag_settings_fast_mode_opt_in(Some(
            r#"{"fastMode": true}"#
        )));
        assert!(!flag_settings_fast_mode_opt_in(Some(
            r#"{"fastMode": "true"}"#
        )));
        assert!(!flag_settings_fast_mode_opt_in(Some(r#"{}"#)));
        assert!(!flag_settings_fast_mode_opt_in(None));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, r#"{"fastMode": true}"#).unwrap();
        assert!(flag_settings_fast_mode_opt_in(file.to_str()));
        assert!(!flag_settings_fast_mode_opt_in(Some(
            "/nonexistent/lingxi-settings.json"
        )));
    }
}

fn projected_array(values: Vec<Utf16JsonProjection>) -> Utf16JsonProjection {
    let mut projection = Utf16JsonProjection::plain(Value::Array(Vec::with_capacity(values.len())));
    for (index, child) in values.into_iter().enumerate() {
        projection
            .value
            .as_array_mut()
            .expect("array")
            .push(child.value);
        projection
            .strings
            .extend(child.strings.into_iter().map(|mut s| {
                s.pointer = format!("/{index}{}", s.pointer);
                s
            }));
        projection.keys.extend(child.keys.into_iter().map(|mut k| {
            k.pointer = format!("/{index}{}", k.pointer);
            k
        }));
    }
    projection
        .validate()
        .expect("valid request snapshot projections");
    projection
}

fn parse_interrupt_turn_aim(value: Option<&Value>) -> Option<Vec<String>> {
    let values = value?.as_array()?;
    if !(1..=8).contains(&values.len()) {
        return None;
    }
    values
        .iter()
        .map(|value| {
            let uuid = value.as_str()?;
            uuid::Uuid::parse_str(uuid).ok()?;
            Some(uuid.to_owned())
        })
        .collect()
}
