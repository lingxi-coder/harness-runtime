//! Mod `tool.call` middleware around a subagent's inherited tool invoker.
//!
//! The child runner still owns its allow-list and the wrapped invoker still
//! owns schema validation, tool checks, and permission prompts. A Mod reaches
//! that exact core through `next(e)`; a direct answer replaces the child tool
//! result without invoking it.

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;
use hooks::HookRegistry;
use hooks::mods::ModError;
use lingxi_core::host::tool_invoker::{
    SubagentInvocationContext, ToolInvocationResult, ToolInvoker, ToolInvokerError,
    tool_call_ref_index,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};

pub(super) struct ModSubagentToolInvoker {
    inner: Arc<dyn ToolInvoker>,
    hooks: Arc<RwLock<HookRegistry>>,
}

impl ModSubagentToolInvoker {
    pub(super) fn new(inner: Arc<dyn ToolInvoker>, hooks: Arc<RwLock<HookRegistry>>) -> Self {
        Self { inner, hooks }
    }
}

fn model_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn model_context(
    value: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
    let Ok(context) = value.subprojection("/context") else {
        return lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
            Value::Array(Vec::new()),
        );
    };
    let Some(items) = context.value.as_array() else {
        return lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
            Value::Array(Vec::new()),
        );
    };
    let mut result =
        lingxi_core::types::utf16_json::Utf16JsonProjection::plain(Value::Array(Vec::new()));
    for (index, item) in items.iter().enumerate() {
        let Some(text) = item.as_str().filter(|text| !text.is_empty()) else {
            continue;
        };
        let Some(units) = context.string_units(&format!("/{index}")) else {
            continue;
        };
        let next_index = result.value.as_array().map_or(0, Vec::len);
        result
            .value
            .as_array_mut()
            .expect("array projection")
            .push(Value::String(text.into()));
        if String::from_utf16(&units).is_err() {
            result
                .strings
                .push(lingxi_core::types::utf16_json::Utf16JsonString {
                    pointer: format!("/{next_index}"),
                    code_units: units,
                });
        }
    }
    result
}

#[async_trait]
impl ToolInvoker for ModSubagentToolInvoker {
    async fn cleanup_computer_inputs(
        &self,
        agent_id: lingxi_core::types::AgentId,
        origin_session_id: Option<lingxi_core::types::SessionId>,
    ) -> Result<(), lingxi_core::host::tool_invoker::ToolInvokerError> {
        self.inner
            .cleanup_computer_inputs(agent_id, origin_session_id)
            .await
    }

    fn tool_is_concurrency_safe(&self, name: &str, input: &Value) -> Option<bool> {
        self.inner.tool_is_concurrency_safe(name, input)
    }

    fn map_result_text(&self, name: &str, result: &Value) -> Option<String> {
        self.inner.map_result_text(name, result)
    }

    fn map_result_is_error(&self, name: &str, result: &Value) -> Option<bool> {
        self.inner.map_result_is_error(name, result)
    }

    fn validate_output(&self, name: &str, output: &Value) -> Result<(), String> {
        self.inner.validate_output(name, output)
    }

    async fn invoke(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
    ) -> Result<Value, ToolInvokerError> {
        self.invoke_detailed(name, input, ctx, None)
            .await
            .map(|result| result.data)
    }

    async fn invoke_with_workspace_lease(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
        lease: Option<u64>,
    ) -> Result<Value, ToolInvokerError> {
        self.invoke_detailed(name, input, ctx, lease)
            .await
            .map(|result| result.data)
    }

    async fn invoke_detailed(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
        lease: Option<u64>,
    ) -> Result<ToolInvocationResult, ToolInvokerError> {
        let (host, session) = {
            let hooks = self.hooks.read().await;
            (hooks.mod_host(), hooks.mod_background_context())
        };
        let Some(host) = host else {
            return self.inner.invoke_detailed(name, input, ctx, lease).await;
        };
        let session = session.and_then(|weak| weak.upgrade()).ok_or_else(|| {
            ToolInvokerError::Internal("Mod session ended before the child tool call".into())
        })?;
        let cwd = ctx.cwd.clone().unwrap_or_else(|| session.cwd());
        let tool_use_id = ctx.tool_use_id.clone().unwrap_or_default();
        let inherited_hook_origin = ctx.agent_spawn_provenance.hook_origin.clone();
        let agent_id = ctx
            .parent_agent_id
            .map(|id| id.as_uuid().to_string())
            .ok_or_else(|| ToolInvokerError::Internal("child tool call has no agent id".into()))?;
        let mut event = serde_json::Map::new();
        event.insert("tool".into(), json!(name));
        event.insert("tool_use_id".into(), json!(tool_use_id));
        event.insert("agentId".into(), json!(agent_id));
        if let Some(args) = input.as_object() {
            for (key, value) in args {
                if !matches!(key.as_str(), "tool" | "tool_use_id" | "agentId") {
                    event.insert(key.clone(), value.clone());
                }
            }
        }
        let completed = Arc::new(Mutex::new(Vec::<ToolInvocationResult>::new()));
        let core_error = Arc::new(Mutex::new(None::<ToolInvokerError>));
        let core_inner = self.inner.clone();
        let core_name = name.to_owned();
        let core_id = tool_use_id.clone();
        let core_agent = agent_id.clone();
        let core_ctx = ctx.clone();
        let completed_for_core = completed.clone();
        let error_for_core = core_error.clone();
        let log_session = session.clone();
        let toast_session = session.clone();
        let status_session = session.clone();
        let origin = match &inherited_hook_origin {
            lingxi_core::host::task_registry::FieldPresence::Value(value) => Some(value.clone()),
            lingxi_core::host::task_registry::FieldPresence::Missing
            | lingxi_core::host::task_registry::FieldPresence::Null => None,
        };
        let api_origin = inherited_hook_origin;
        let answer = host
            .dispatch_with_utf16_at_context(
                "tool.call",
                hooks::mods::ModUtf16ValueProjection::plain(Value::Object(event)),
                &cwd,
                Some(session.as_ref()),
                Some(&cwd),
                origin,
                None,
                api_origin,
                move |forwarded| {
                    let inner = core_inner.clone();
                    let name = core_name.clone();
                    let id = core_id.clone();
                    let agent = core_agent.clone();
                    let ctx = core_ctx.clone();
                    let completed = completed_for_core.clone();
                    let core_error = error_for_core.clone();
                    async move {
                        if forwarded.value.get("tool").and_then(Value::as_str)
                            != Some(name.as_str())
                            || forwarded.value.get("tool_use_id").and_then(Value::as_str)
                                != Some(id.as_str())
                            || forwarded.value.get("agentId").and_then(Value::as_str)
                                != Some(agent.as_str())
                        {
                            return Err(ModError::Hook(
                                "tool, tool_use_id, and agentId are pinned".into(),
                            ));
                        }
                        let mut args = forwarded.value.as_object().cloned().ok_or_else(|| {
                            ModError::Hook("tool.call input must be an object".into())
                        })?;
                        args.remove("tool");
                        args.remove("tool_use_id");
                        args.remove("agentId");
                        let result = match inner
                            .invoke_detailed(&name, Value::Object(args), ctx, lease)
                            .await
                        {
                            Ok(result) => result,
                            Err(error) => {
                                let message = error.to_string();
                                *core_error.lock().await = Some(error);
                                return Err(ModError::Hook(message));
                            }
                        };
                        let text = result
                            .model_content
                            .clone()
                            .unwrap_or_else(|| model_text(&result.data));
                        let mut completed = completed.lock().await;
                        // Native XIe `keep` assigns 1-based run references;
                        // `of(ref)` later indexes `entries[ref - 1]`.
                        let run_ref = completed.len().saturating_add(1);
                        let context = result.context.clone();
                        let mut answer = hooks::mods::ModUtf16ValueProjection::plain(json!({
                            "result": result.data,
                            "text": text,
                            "isError": result.is_error,
                            "ref": run_ref,
                            "context": context.value,
                        }));
                        answer
                            .strings
                            .extend(context.strings.into_iter().map(|sidecar| {
                                hooks::mods::ModUtf16StringSidecar {
                                    pointer: format!("/context{}", sidecar.pointer),
                                    code_units: sidecar.code_units,
                                }
                            }));
                        answer.keys.extend(context.keys.into_iter().map(|sidecar| {
                            hooks::mods::ModUtf16KeySidecar {
                                pointer: format!("/context{}", sidecar.pointer),
                                placeholder: sidecar.placeholder,
                                code_units: sidecar.code_units,
                            }
                        }));
                        completed.push(result);
                        Ok(answer)
                    }
                },
                move |plugin, text| {
                    let session = log_session.clone();
                    async move { session.emit_mod_log(&plugin, &text).await }
                },
                move |plugin, text, timeout_ms| {
                    let session = toast_session.clone();
                    async move { session.emit_mod_toast(&plugin, &text, timeout_ms).await }
                },
                move |plugin, text| {
                    let session = status_session.clone();
                    async move { session.emit_mod_status(&plugin, text.as_deref()).await }
                },
            )
            .await;
        let answer = match answer {
            Ok(answer) => lingxi_core::types::utf16_json::Utf16JsonProjection {
                value: answer.result,
                strings: answer
                    .result_utf16_strings
                    .into_iter()
                    .map(|sidecar| lingxi_core::types::utf16_json::Utf16JsonString {
                        pointer: sidecar.pointer,
                        code_units: sidecar.code_units,
                    })
                    .collect(),
                keys: answer
                    .result_utf16_keys
                    .into_iter()
                    .map(|sidecar| lingxi_core::types::utf16_json::Utf16JsonKey {
                        pointer: sidecar.pointer,
                        placeholder: sidecar.placeholder,
                        code_units: sidecar.code_units,
                    })
                    .collect(),
            },
            Err(error) => {
                if let Some(core_error) = core_error.lock().await.take() {
                    return Err(core_error);
                }
                if let Some(core) = completed.lock().await.last().cloned() {
                    return Ok(core);
                }
                tracing::warn!(tool = name, error = %error, "nested Mod tool.call failed");
                return self.inner.invoke_detailed(name, input, ctx, lease).await;
            }
        };
        // Native DVt honors deny before looking up a selected run ref. The
        // public validator rejects a simultaneous own `result` property, but
        // keep the finalizer safe if an internal caller reaches this boundary.
        if let Some(reason) = answer.value.get("deny").and_then(Value::as_str) {
            return Err(ToolInvokerError::Validation(reason.to_owned()));
        }

        // A present run reference is 1-based. Invalid values never alias the
        // first completed call; without a selected row, an accompanying
        // `result` follows the ordinary synthetic-replacement path below.
        let selected_core = if let Some(run_ref) = answer.value.get("ref") {
            let completed = completed.lock().await;
            tool_call_ref_index(run_ref, completed.len())
                .and_then(|index| completed.get(index).cloned())
        } else {
            None
        };

        if let Some(result) = answer.value.get("result") {
            let reuses_core = selected_core
                .as_ref()
                .is_some_and(|core| core.data == *result);
            if !reuses_core {
                if let Err(detail) = self.inner.validate_output(name, result) {
                    let message = format!(
                        "tool.call step resolved {name} with a result that does not match its output shape: {detail}"
                    );
                    return Ok(ToolInvocationResult {
                        is_error: true,
                        data: Value::String(format!("Error: {message}")),
                        model_content: Some(format!("<tool_use_error>{message}</tool_use_error>")),
                        turn_end: None,
                        new_messages: Vec::new(),
                        context_modifier: None,
                        mcp_meta: None,
                        context: model_context(&answer),
                        context_state: None,
                    });
                }
            }
        }

        if let Some(mut core) = selected_core {
            core.context = model_context(&answer);
            let changed_result = answer
                .value
                .get("result")
                .is_some_and(|value| value != &core.data);
            if changed_result {
                core.data = answer.value.get("result").cloned().unwrap_or(core.data);
                core.model_content = Some(
                    self.inner
                        .map_result_text(name, &core.data)
                        .unwrap_or_else(|| model_text(&core.data)),
                );
                core.is_error = self
                    .inner
                    .map_result_is_error(name, &core.data)
                    .unwrap_or_else(|| {
                        answer
                            .value
                            .get("isError")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    });
            }
            return Ok(core);
        }
        if let Some(result) = answer.value.get("result") {
            return Ok(ToolInvocationResult {
                is_error: self
                    .inner
                    .map_result_is_error(name, result)
                    .unwrap_or_else(|| {
                        answer
                            .value
                            .get("isError")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    }),
                data: result.clone(),
                model_content: Some(
                    self.inner
                        .map_result_text(name, result)
                        .unwrap_or_else(|| model_text(result)),
                ),
                turn_end: None,
                new_messages: Vec::new(),
                context_modifier: None,
                mcp_meta: None,
                context: model_context(&answer),
                context_state: None,
            });
        }
        // No selected run and no explicit replacement becomes an ordinary
        // synthetic result. The internal runs' opaque metadata belongs only
        // to a valid selected row and must not leak from the last attempted run.
        Ok(ToolInvocationResult {
            is_error: answer
                .value
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            data: Value::Null,
            model_content: answer
                .value
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned),
            turn_end: None,
            new_messages: Vec::new(),
            context_modifier: None,
            mcp_meta: None,
            context: model_context(&answer),
            context_state: None,
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::tool_invoker::ToolExecutionPolicy;
    use lingxi_core::types::{AgentId, ConversationMessage, MessageId};
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    struct Session {
        cwd: PathBuf,
        ancestor_cwds: StdMutex<Vec<PathBuf>>,
    }

    #[async_trait]
    impl hooks::mods::ModSessionContext for Session {
        fn cwd(&self) -> PathBuf {
            self.cwd.clone()
        }

        fn root(&self) -> PathBuf {
            self.cwd.clone()
        }

        async fn fs_ancestors_at(
            &self,
            _input: Value,
            cwd: &std::path::Path,
        ) -> Result<Value, hooks::mods::ModError> {
            self.ancestor_cwds.lock().unwrap().push(cwd.to_path_buf());
            Ok(json!([{
                "dir": cwd, "name": "AGENTS.md", "content": "instructions", "parts": []
            }]))
        }

        async fn model(&self) -> String {
            "claude-sonnet".into()
        }

        async fn id(&self) -> String {
            "session-1".into()
        }

        async fn turns(&self) -> u64 {
            1
        }
    }

    struct Core {
        calls: StdMutex<Vec<Value>>,
    }

    #[async_trait]
    impl ToolInvoker for Core {
        fn tool_is_concurrency_safe(&self, name: &str, input: &Value) -> Option<bool> {
            (name == "Read").then(|| {
                !input
                    .get("force_serial")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
        }

        fn map_result_text(&self, _name: &str, result: &Value) -> Option<String> {
            result
                .get("mapped")
                .and_then(Value::as_str)
                .map(|text| format!("child-mapped:{text}"))
        }

        fn map_result_is_error(&self, _name: &str, result: &Value) -> Option<bool> {
            result.get("mappedError").and_then(Value::as_bool)
        }

        fn validate_output(&self, _name: &str, output: &Value) -> Result<(), String> {
            output
                .is_object()
                .then_some(())
                .ok_or_else(|| "expected an object".into())
        }

        async fn invoke(
            &self,
            _name: &str,
            input: Value,
            _ctx: SubagentInvocationContext,
        ) -> Result<Value, ToolInvokerError> {
            self.calls.lock().unwrap().push(input.clone());
            Ok(input)
        }

        async fn invoke_detailed(
            &self,
            _name: &str,
            input: Value,
            _ctx: SubagentInvocationContext,
            _lease: Option<u64>,
        ) -> Result<ToolInvocationResult, ToolInvokerError> {
            self.calls.lock().unwrap().push(input.clone());
            let marker = input
                .get("marker")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let new_messages = marker
                .as_ref()
                .map(|marker| {
                    vec![ConversationMessage::user(
                        MessageId::new(),
                        format!("message-{marker}"),
                    )]
                })
                .unwrap_or_default();
            let context_modifier = marker.as_ref().map(|marker| {
                let marker = marker.clone();
                lingxi_core::host::tool_invoker::ToolInvocationContextModifier::new(
                    move |context: String| format!("{context}-{marker}"),
                )
            });
            let mcp_meta = marker.as_ref().map(|marker| json!({"run": marker}));
            let turn_end =
                marker
                    .as_ref()
                    .map(|_| lingxi_core::host::tool_invoker::ToolResultTurnEnd {
                        source: lingxi_core::host::tool_invoker::ToolResultTurnEndSource::McpMeta,
                    });
            let context_state = marker.as_ref().map(|marker| {
                lingxi_core::host::tool_invoker::ToolInvocationContextState::new(Arc::new(
                    json!({"marker": marker}),
                ))
            });
            Ok(ToolInvocationResult {
                is_error: false,
                data: input,
                model_content: Some("core text".into()),
                turn_end,
                new_messages,
                context_modifier,
                mcp_meta,
                context: lingxi_core::types::utf16_json::Utf16JsonProjection::plain(Value::Array(
                    Vec::new(),
                )),
                context_state,
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn child_context(agent_id: AgentId) -> SubagentInvocationContext {
        SubagentInvocationContext {
            input_projection: None,
            cancellation_token: lingxi_core::host::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: Some(agent_id),
            origin_session_id: None,
            tool_execution_policy: ToolExecutionPolicy::Ordinary,
            agent_name: None,
            team_name: None,
            is_async: false,
            is_non_interactive_session: false,
            can_show_permission_prompts: false,
            cwd: Some(PathBuf::from("/tmp/child-agent")),
            tool_use_id: Some("toolu_child_read".into()),
            assistant_message_id: None,
            depth: 1,
            observer: None,
            parent_model: Some("claude-sonnet".into()),
            parent_model_profile: None,
            agent_spawn_provenance: Default::default(),
            tool_context_state: None,
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            current_history: Vec::new(),
            instruction_context: None,
            fork_context: None,
            mode_override: None,
            request_source: None,
            frozen_command_denies: Vec::new(),
        }
    }

    async fn setup(source: &str) -> (ModSubagentToolInvoker, Arc<Core>, Arc<Session>) {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("nested.js");
        std::fs::write(&module, source).unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load("nested", dir.path(), &module, json!({}))
            .await
            .unwrap();
        let session = Arc::new(Session {
            cwd: PathBuf::from("/tmp/main-session"),
            ancestor_cwds: StdMutex::new(Vec::new()),
        });
        let session_dyn: Arc<dyn hooks::mods::ModSessionContext> = session.clone();
        let mut registry = HookRegistry::new();
        registry.attach_mod_background_context(Arc::downgrade(&session_dyn));
        registry.set_mod_host(host);
        let core = Arc::new(Core {
            calls: StdMutex::new(Vec::new()),
        });
        let invoker = ModSubagentToolInvoker::new(core.clone(), Arc::new(RwLock::new(registry)));
        (invoker, core, session)
    }

    #[tokio::test]
    async fn child_tool_call_rewrites_input_and_preserves_agent_identity() {
        let (invoker, core, _session) = setup(r#"export function register(on) {
            on('tool.call', { tool: 'Read' }, async ($, e, next) => {
              const cwd = await $.session.cwd();
              const result = await next({ ...e, file_path: cwd + '/read.txt' });
              return { ...result, result: { ...result.result, agentId: e.agentId }, text: 'Mod text' };
            });
        }"#).await;
        let child = AgentId::new();
        let result = invoker
            .invoke_detailed(
                "Read",
                json!({"file_path":"original.txt"}),
                child_context(child),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            core.calls.lock().unwrap().as_slice(),
            &[json!({"file_path":"/tmp/child-agent/read.txt"})]
        );
        assert_eq!(result.data["agentId"], child.as_uuid().to_string());
        let mapped_text = result.data.to_string();
        assert_eq!(result.model_content.as_deref(), Some(mapped_text.as_str()));
    }

    #[tokio::test]
    async fn tool_concurrency_resolution_forwards_to_the_wrapped_registry() {
        let (invoker, _, _session) = setup("export function register(on) {}").await;
        assert_eq!(
            invoker.tool_is_concurrency_safe("Read", &json!({})),
            Some(true)
        );
        assert_eq!(
            invoker.tool_is_concurrency_safe("Read", &json!({"force_serial": true})),
            Some(false)
        );
        assert_eq!(
            invoker.tool_is_concurrency_safe("Unknown", &json!({})),
            None
        );
    }

    #[tokio::test]
    async fn child_tool_calls_keep_concurrent_inherited_origins_isolated() {
        let (invoker, core, _session) = setup(
            r#"export function register(on) {
            on('tool.call', { tool: 'Read' }, async ($, e, next) => {
              return await next({ ...e, file_path: JSON.stringify(next.origin) });
            });
        }"#,
        )
        .await;
        let parent_a = AgentId::new();
        let parent_b = AgentId::new();
        let mut context_a = child_context(parent_a);
        context_a.agent_spawn_provenance =
            lingxi_core::host::subagent_spawn::AgentSpawnProvenance {
                hook_caller: lingxi_core::host::task_registry::FieldPresence::Value(json!(
                    "plugin-a"
                )),
                hook_origin: lingxi_core::host::task_registry::FieldPresence::Value(json!([
                    "root", "agent-a"
                ])),
            };
        let mut context_b = child_context(parent_b);
        context_b.agent_spawn_provenance =
            lingxi_core::host::subagent_spawn::AgentSpawnProvenance {
                hook_caller: lingxi_core::host::task_registry::FieldPresence::Value(json!(
                    "plugin-b"
                )),
                hook_origin: lingxi_core::host::task_registry::FieldPresence::Value(json!([
                    "root", "agent-b"
                ])),
            };

        let (result_a, result_b) = tokio::join!(
            invoker.invoke_detailed("Read", json!({"file_path":"a"}), context_a, None),
            invoker.invoke_detailed("Read", json!({"file_path":"b"}), context_b, None),
        );
        result_a.unwrap();
        result_b.unwrap();

        let mut seen = core
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|call| call.get("file_path").and_then(Value::as_str))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        seen.sort();
        assert_eq!(
            seen,
            vec![
                r#"["root","agent-a"]"#.to_owned(),
                r#"["root","agent-b"]"#.to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn child_tool_call_mod_deny_does_not_invoke_core() {
        let (invoker, core, _session) = setup(
            r#"export function register(on) {
            on('tool.call', { tool: 'Read' }, () => ({ deny: 'blocked by Mod' }));
        }"#,
        )
        .await;
        let error = invoker
            .invoke_detailed(
                "Read",
                json!({"file_path":"original.txt"}),
                child_context(AgentId::new()),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolInvokerError::Validation(reason) if reason == "blocked by Mod")
        );
        assert!(core.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn child_tool_call_attaches_model_only_context_after_core() {
        let (invoker, core, _session) = setup(
            r#"export function register(on) {
              on('tool.call', { tool: 'Read' }, async ($, e, next) => {
                const result = await next(e);
                return { ...result, context: ['Contents of /tmp/child-agent/AGENTS.md:\n\nNested rule'] };
              });
            }"#,
        )
        .await;
        let result = invoker
            .invoke_detailed(
                "Read",
                json!({"file_path":"read.txt"}),
                child_context(AgentId::new()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(core.calls.lock().unwrap().len(), 1);
        assert_eq!(result.model_content.as_deref(), Some("core text"));
        assert_eq!(
            result.context.value,
            json!(["Contents of /tmp/child-agent/AGENTS.md:\n\nNested rule"])
        );
    }

    #[tokio::test]
    async fn child_tool_call_context_keeps_exact_js_utf16_units() {
        let (invoker, core, _session) = setup(
            r#"export function register(on) {
              on('tool.call', { tool: 'Read' }, async ($, e, next) => {
                const result = await next(e);
                return { ...result, context: ['kept\uD800'] };
              });
            }"#,
        )
        .await;
        let result = invoker
            .invoke_detailed(
                "Read",
                json!({"file_path":"read.txt"}),
                child_context(AgentId::new()),
                None,
            )
            .await
            .unwrap();

        assert_eq!(core.calls.lock().unwrap().len(), 1);
        assert_eq!(result.context.value, json!(["kept�"]));
        assert_eq!(
            result.context.string_units("/0"),
            Some(vec![0x006b, 0x0065, 0x0070, 0x0074, 0xd800])
        );
        assert_eq!(
            result.context.to_json_string().unwrap(),
            r#"["kept\ud800"]"#
        );
    }

    #[tokio::test]
    async fn child_tool_call_keeps_core_text_when_ref_and_result_are_unchanged() {
        let (invoker, core, _session) = setup(
            r#"export function register(on) {
              on('tool.call', { tool: 'Read' }, async ($, e, next) => {
                const result = await next(e);
                return { ...result, text: 'forged text' };
              });
            }"#,
        )
        .await;
        let result = invoker
            .invoke_detailed(
                "Read",
                json!({"file_path":"read.txt"}),
                child_context(AgentId::new()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(core.calls.lock().unwrap().len(), 1);
        assert_eq!(result.model_content.as_deref(), Some("core text"));
    }

    #[tokio::test]
    async fn child_tool_call_rejects_a_result_outside_the_output_schema() {
        for (source, core_should_run) in [
            (
                "export function register(on) { on('tool.call', () => ({ result: 42 })); }",
                false,
            ),
            (
                "export function register(on) { on('tool.call', async ($, e, next) => ({ ...await next(e), result: 42 })); }",
                true,
            ),
        ] {
            let (invoker, core, _session) = setup(source).await;
            let result = invoker
                .invoke_detailed(
                    "Read",
                    json!({"file_path":"read.txt"}),
                    child_context(AgentId::new()),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(
                core.calls.lock().unwrap().len(),
                usize::from(core_should_run)
            );
            assert!(result.is_error);
            assert!(result
                .model_content
                .as_deref()
                .is_some_and(|text| text.starts_with("<tool_use_error>tool.call step resolved Read with a result that does not match its output shape:")));
            assert_eq!(result.context.value, Value::Array(Vec::new()));
        }
    }

    #[tokio::test]
    async fn child_tool_call_uses_the_tools_mapper_for_a_replacement() {
        for source in [
            "export function register(on) { on('tool.call', () => ({ result: { mapped: 'direct' } })); }",
            "export function register(on) { on('tool.call', async ($, e, next) => ({ ...await next(e), result: { mapped: 'direct' } })); }",
        ] {
            let (invoker, _core, _session) = setup(source).await;
            let result = invoker
                .invoke_detailed(
                    "Read",
                    json!({"file_path":"read.txt"}),
                    child_context(AgentId::new()),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(result.data, json!({"mapped":"direct"}));
            assert_eq!(result.model_content.as_deref(), Some("child-mapped:direct"));
        }
    }

    #[tokio::test]
    async fn child_tool_call_uses_the_tools_error_bit_for_a_replacement() {
        let (invoker, _core, _session) = setup(
            "export function register(on) { on('tool.call', () => ({ result: { mapped: 'interrupted', mappedError: true } })); }",
        )
        .await;
        let result = invoker
            .invoke_detailed(
                "Read",
                json!({"file_path":"read.txt"}),
                child_context(AgentId::new()),
                None,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert_eq!(
            result.model_content.as_deref(),
            Some("child-mapped:interrupted")
        );
    }

    #[tokio::test]
    async fn child_fs_ancestors_uses_the_agent_cwd() {
        let (invoker, core, session) = setup(
            r#"export function register(on) {
            on('tool.call', { tool: 'Read' }, async ($, e, next) => {
              const ancestors = await $.fs.ancestors({ names: ['AGENTS.md'] });
              const root = await $.session.root();
              const cwd = await $.session.cwd();
              if (root !== ancestors[0].dir || cwd !== root) {
                throw new Error('child session root and cwd diverged');
              }
              return next({ ...e, file_path: ancestors[0].dir + '/read.txt' });
            });
        }"#,
        )
        .await;
        invoker
            .invoke_detailed(
                "Read",
                json!({"file_path":"original.txt"}),
                child_context(AgentId::new()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            session.ancestor_cwds.lock().unwrap().as_slice(),
            &[PathBuf::from("/tmp/child-agent")]
        );
        assert_eq!(
            core.calls.lock().unwrap().as_slice(),
            &[json!({"file_path":"/tmp/child-agent/read.txt"})]
        );
    }

    #[tokio::test]
    async fn child_tool_call_refs_are_one_based_and_retain_the_selected_core_effects() {
        let (invoker, core, _session) = setup(
            r#"export function register(on) {
              on('tool.call', { tool: 'Read' }, async ($, e, next) => {
                const first = await next({ ...e, marker: 'one' });
                const second = await next({ ...e, marker: 'two' });
                if (first.ref !== 1 || second.ref !== 2) {
                  throw new Error(`unexpected native run refs: ${first.ref}, ${second.ref}`);
                }
                return { ...second, ref: String(first.ref),
                  result: { ...first.result, selectedRewrite: true },
                  context: ['selected context'] };
              });
            }"#,
        )
        .await;
        let result = invoker
            .invoke_detailed(
                "Read",
                json!({"file_path":"read.txt"}),
                child_context(AgentId::new()),
                None,
            )
            .await
            .expect("the selected native run resolves");

        let calls = core.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["marker"], "one");
        assert_eq!(calls[1]["marker"], "two");
        drop(calls);
        assert_eq!(result.data["marker"], "one");
        assert_eq!(result.data["selectedRewrite"], true);
        let expected_model_content = result.data.to_string();
        assert_eq!(
            result.model_content.as_deref(),
            Some(expected_model_content.as_str())
        );
        assert_eq!(result.context.value, json!(["selected context"]));
        assert_eq!(result.mcp_meta, Some(json!({"run":"one"})));
        assert_eq!(
            result.turn_end.map(|end| end.source),
            Some(lingxi_core::host::tool_invoker::ToolResultTurnEndSource::McpMeta)
        );
        assert_eq!(result.new_messages.len(), 1);
        assert_eq!(result.new_messages[0].text_content(), "message-one");
        assert_eq!(
            result
                .context_state
                .as_ref()
                .unwrap()
                .downcast_arc::<Value>()
                .unwrap()
                .as_ref(),
            &json!({"marker":"one"})
        );
        assert_eq!(
            result
                .context_modifier
                .as_ref()
                .unwrap()
                .apply("seed".to_owned())
                .unwrap(),
            "seed-one"
        );
    }

    #[tokio::test]
    async fn child_tool_call_deny_wins_over_a_selected_ref_without_result() {
        let (invoker, core, _session) = setup(
            r#"export function register(on) {
              on('tool.call', { tool: 'Read' }, async ($, e, next) => {
                const selected = await next({ ...e, marker: 'one' });
                const { result, ...withoutResult } = selected;
                return { ...withoutResult, deny: 'blocked by Mod', ref: selected.ref };
              });
            }"#,
        )
        .await;
        let error = invoker
            .invoke_detailed(
                "Read",
                json!({"file_path":"read.txt"}),
                child_context(AgentId::new()),
                None,
            )
            .await
            .expect_err("deny is terminal even when a selected ref is present");

        assert!(
            matches!(error, ToolInvokerError::Validation(reason) if reason == "blocked by Mod")
        );
        assert_eq!(core.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn child_tool_call_ref_zero_uses_synthetic_result_without_aliasing_run_one() {
        let (invoker, core, _session) = setup(
            r#"export function register(on) {
              on('tool.call', { tool: 'Read' }, async ($, e, next) => {
                const first = await next({ ...e, marker: 'one' });
                return { ...first, ref: 0, result: first.result };
              });
            }"#,
        )
        .await;
        let result = invoker
            .invoke_detailed(
                "Read",
                json!({"file_path":"read.txt"}),
                child_context(AgentId::new()),
                None,
            )
            .await
            .expect("an invalid ref can still carry a synthetic replacement result");

        assert_eq!(core.calls.lock().unwrap().len(), 1);
        assert_eq!(result.data["marker"], "one");
        assert_eq!(
            result.mcp_meta, None,
            "ref zero does not select run one metadata"
        );
        assert_eq!(result.turn_end, None);
        assert!(result.new_messages.is_empty());
        assert!(result.context_modifier.is_none());
        assert!(result.context_state.is_none());
    }
}
