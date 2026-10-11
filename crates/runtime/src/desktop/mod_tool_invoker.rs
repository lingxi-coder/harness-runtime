//! Mod tool.call middleware around a subagent's inherited tool invoker.
//!
//! The child runner still owns its allow-list and the wrapped invoker still
//! owns schema validation, tool checks, and permission prompts. A Mod reaches
//! that exact core through next(e); a direct answer replaces the child tool
//! result without invoking it.

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;
use hooks::mods::ModError;
use hooks::HookRegistry;
use lingxi_core::host::tool_invoker::{
    tool_call_ref_index, SubagentInvocationContext, ToolInvocationResult, ToolInvoker,
    ToolInvokerError,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
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

fn empty_array() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("[]").expect("empty JSON array is valid")
}

fn projection_error(error: impl std::fmt::Display) -> ToolInvokerError {
    ToolInvokerError::Internal(error.to_string().into())
}

fn model_text(value: &Utf16JsonProjection) -> Utf16JsonProjection {
    if value.string_units("").is_some() {
        return value.clone();
    }
    value
        .to_json_string()
        .unwrap_or_else(|_| "null".to_owned())
        .into()
}

fn model_context(value: &Utf16JsonProjection) -> Utf16JsonProjection {
    let Ok(context) = value.subprojection("/context") else {
        return empty_array();
    };
    let Some(length) = context.array_len("") else {
        return empty_array();
    };
    let mut items = Vec::new();
    for index in 0..length {
        let pointer = format!("/{index}");
        if context
            .string_units(&pointer)
            .is_some_and(|units| !units.is_empty())
        {
            if let Ok(item) = context.subprojection(&pointer) {
                items.push(item);
            }
        }
    }
    Utf16JsonProjection::array(items).expect("context strings form a valid source array")
}

fn tool_call_event(
    name: &str,
    tool_use_id: &str,
    agent_id: &str,
    input: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, ToolInvokerError> {
    let mut fields = vec![
        ("tool".encode_utf16().collect(), name.into()),
        ("tool_use_id".encode_utf16().collect(), tool_use_id.into()),
        ("agentId".encode_utf16().collect(), agent_id.into()),
    ];
    if let Some(entries) = input.object_entries() {
        let pinned = [
            "tool".encode_utf16().collect::<Vec<_>>(),
            "tool_use_id".encode_utf16().collect(),
            "agentId".encode_utf16().collect(),
        ];
        for entry in entries {
            if pinned.iter().any(|key| key == entry.code_units) {
                continue;
            }
            let value = input
                .subprojection_property_units(entry.code_units)
                .map_err(projection_error)?;
            fields.push((entry.code_units.to_vec(), value));
        }
    }
    Utf16JsonProjection::object_units(fields).map_err(projection_error)
}

fn tool_call_arguments(forwarded: &Utf16JsonProjection) -> Result<Utf16JsonProjection, ModError> {
    let entries = forwarded
        .object_entries()
        .ok_or_else(|| ModError::Hook("tool.call input must be an object".into()))?;
    let pinned = [
        "tool".encode_utf16().collect::<Vec<_>>(),
        "tool_use_id".encode_utf16().collect(),
        "agentId".encode_utf16().collect(),
    ];
    let mut fields = Vec::new();
    for entry in entries {
        if pinned.iter().any(|key| key == entry.code_units) {
            continue;
        }
        let value = forwarded
            .subprojection_property_units(entry.code_units)
            .map_err(|error| ModError::Hook(error.to_string()))?;
        fields.push((entry.code_units.to_vec(), value));
    }
    Utf16JsonProjection::object_units(fields).map_err(|error| ModError::Hook(error.to_string()))
}

fn wrap_tool_call_error(name: &str, error: &ToolInvokerError) -> ToolInvocationResult {
    let detail = source_text_with_prefix(
        &format!(
            "tool.call step resolved {name} with a result that does not match its output shape: "
        ),
        &error.model_facing_message(),
        "",
    );
    ToolInvocationResult {
        is_error: true,
        data: source_text_with_prefix("Error: ", &detail, ""),
        model_content: Some(source_text_with_prefix(
            "<tool_use_error>",
            &detail,
            "</tool_use_error>",
        )),
        turn_end: None,
        new_messages: Vec::new(),
        context_modifier: None,
        mcp_meta: None,
        context: empty_array(),
        context_state: None,
    }
}

fn source_text_with_prefix(
    prefix: &str,
    text: &Utf16JsonProjection,
    suffix: &str,
) -> Utf16JsonProjection {
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.extend(
        text.string_units("")
            .expect("model-facing text is a root-string projection"),
    );
    units.extend(suffix.encode_utf16());
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("wrapped model-facing text remains a root string")
}

#[async_trait]
impl ToolInvoker for ModSubagentToolInvoker {
    async fn cleanup_computer_inputs(
        &self,
        agent_id: lingxi_core::types::AgentId,
        origin_session_id: Option<lingxi_core::types::SessionId>,
    ) -> Result<(), ToolInvokerError> {
        self.inner
            .cleanup_computer_inputs(agent_id, origin_session_id)
            .await
    }

    fn permission_mode(&self) -> Option<String> {
        self.inner.permission_mode()
    }

    fn tool_is_concurrency_safe(&self, name: &str, input: &Utf16JsonProjection) -> Option<bool> {
        self.inner.tool_is_concurrency_safe(name, input)
    }

    fn map_result_text(
        &self,
        name: &str,
        result: &Utf16JsonProjection,
    ) -> Option<Utf16JsonProjection> {
        self.inner.map_result_text(name, result)
    }

    fn map_result_is_error(&self, name: &str, result: &Utf16JsonProjection) -> Option<bool> {
        self.inner.map_result_is_error(name, result)
    }

    async fn parse_output(
        &self,
        name: &str,
        output: &Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ToolInvokerError> {
        self.inner.parse_output(name, output).await
    }

    async fn invoke(
        &self,
        name: &str,
        input: Utf16JsonProjection,
        ctx: SubagentInvocationContext,
    ) -> Result<Utf16JsonProjection, ToolInvokerError> {
        self.invoke_detailed(name, input, ctx, None)
            .await
            .map(|result| result.data)
    }

    async fn invoke_with_workspace_lease(
        &self,
        name: &str,
        input: Utf16JsonProjection,
        ctx: SubagentInvocationContext,
        lease: Option<u64>,
    ) -> Result<Utf16JsonProjection, ToolInvokerError> {
        self.invoke_detailed(name, input, ctx, lease)
            .await
            .map(|result| result.data)
    }

    async fn invoke_detailed(
        &self,
        name: &str,
        input: Utf16JsonProjection,
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
        let event = tool_call_event(name, &tool_use_id, &agent_id, &input)?;
        let original_input = input.clone();
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
                event,
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
                        let Some(forwarded) = forwarded else {
                            return Err(ModError::Hook("tool.call input must be an object".into()));
                        };
                        if forwarded.string_at("/tool") != Some(name.as_str())
                            || forwarded.string_at("/tool_use_id") != Some(id.as_str())
                            || forwarded.string_at("/agentId") != Some(agent.as_str())
                        {
                            return Err(ModError::Hook(
                                "tool, tool_use_id, and agentId are pinned".into(),
                            ));
                        }
                        let args = tool_call_arguments(&forwarded)?;
                        let result = match inner.invoke_detailed(&name, args, ctx, lease).await {
                            Ok(result) => result,
                            Err(error) => {
                                let source = error.model_facing_message();
                                *core_error.lock().await = Some(error);
                                return Err(ModError::HookSource(source));
                            }
                        };
                        let text = result
                            .model_content
                            .clone()
                            .unwrap_or_else(|| model_text(&result.data));
                        let mut completed = completed.lock().await;
                        let run_ref = completed.len().saturating_add(1);
                        let answer = Utf16JsonProjection::object(vec![
                            ("result".into(), result.data.clone()),
                            ("text".into(), text),
                            (
                                "isError".into(),
                                Utf16JsonProjection::from_serde_value(&serde_json::json!(
                                    result.is_error
                                ))
                                .expect("generated boolean is valid JSON"),
                            ),
                            (
                                "ref".into(),
                                Utf16JsonProjection::from_serde_value(&serde_json::json!(run_ref))
                                    .expect("generated run reference is valid JSON"),
                            ),
                            ("context".into(), result.context.clone()),
                        ])
                        .map_err(|error| ModError::Hook(error.to_string()))?;
                        completed.push(result);
                        Ok(Some(answer))
                    }
                },
                move |plugin, text| {
                    let session = log_session.clone();
                    async move { session.emit_mod_log(&plugin, text).await }
                },
                move |plugin, text, timeout_ms| {
                    let session = toast_session.clone();
                    async move { session.emit_mod_toast(&plugin, text, timeout_ms).await }
                },
                move |plugin, text| {
                    let session = status_session.clone();
                    async move { session.emit_mod_status(&plugin, text).await }
                },
            )
            .await;
        let answer = match answer {
            Ok(answer) => answer.result_projection,
            Err(error) => {
                if let Some(core_error) = core_error.lock().await.take() {
                    return Err(core_error);
                }
                if let Some(core) = completed.lock().await.last().cloned() {
                    return Ok(core);
                }
                tracing::warn!(tool = name, error = %error, "nested Mod tool.call failed");
                return self
                    .inner
                    .invoke_detailed(name, original_input, ctx, lease)
                    .await;
            }
        };

        if answer.string_at("/deny").is_some() {
            let reason = answer.subprojection("/deny").map_err(projection_error)?;
            return Err(ToolInvokerError::Validation(reason));
        }

        let selected_core = if let Some(reference) = answer.at_pointer("/ref") {
            let completed = completed.lock().await;
            tool_call_ref_index(reference, completed.len())
                .and_then(|index| completed.get(index).cloned())
        } else {
            None
        };
        let replacement = answer.subprojection("/result").ok();

        if let Some(mut core) = selected_core {
            core.context = model_context(&answer);
            let changed_result = replacement
                .as_ref()
                .is_some_and(|value| value != &core.data);
            if changed_result {
                let raw = replacement.as_ref().expect("changed result is present");
                let parsed = match self.inner.parse_output(name, raw).await {
                    Ok(parsed) => parsed,
                    Err(error) => return Ok(wrap_tool_call_error(name, &error)),
                };
                core.data = parsed;
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
                            .at_pointer("/isError")
                            .and_then(|value| value.as_bool())
                            .unwrap_or(false)
                    });
            }
            return Ok(core);
        }

        if let Some(raw) = replacement {
            let parsed = match self.inner.parse_output(name, &raw).await {
                Ok(parsed) => parsed,
                Err(error) => return Ok(wrap_tool_call_error(name, &error)),
            };
            return Ok(ToolInvocationResult {
                is_error: self
                    .inner
                    .map_result_is_error(name, &parsed)
                    .unwrap_or_else(|| {
                        answer
                            .at_pointer("/isError")
                            .and_then(|value| value.as_bool())
                            .unwrap_or(false)
                    }),
                data: parsed.clone(),
                model_content: Some(
                    self.inner
                        .map_result_text(name, &parsed)
                        .unwrap_or_else(|| model_text(&parsed)),
                ),
                turn_end: None,
                new_messages: Vec::new(),
                context_modifier: None,
                mcp_meta: None,
                context: model_context(&answer),
                context_state: None,
            });
        }
        let model_content = answer
            .subprojection("/text")
            .ok()
            .filter(|text| text.string_units("").is_some());
        Ok(ToolInvocationResult {
            is_error: answer
                .at_pointer("/isError")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            data: Utf16JsonProjection::parse("null").expect("null is valid JSON"),
            model_content,
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
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    fn source(value: &Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(value).expect("test JSON projects to flat source")
    }

    fn source_json(source: &Utf16JsonProjection) -> Value {
        serde_json::from_str(&source.to_json_string().expect("valid JSON source"))
            .expect("ordinary test source parses as JSON")
    }

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
            _input: Utf16JsonProjection,
            cwd: &std::path::Path,
        ) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
            self.ancestor_cwds.lock().unwrap().push(cwd.to_path_buf());
            Ok(source(&json!([{
                "dir": cwd, "name": "AGENTS.md", "content": "instructions", "parts": []
            }])))
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
        calls: StdMutex<Vec<Utf16JsonProjection>>,
    }

    #[async_trait]
    impl ToolInvoker for Core {
        fn tool_is_concurrency_safe(
            &self,
            name: &str,
            input: &Utf16JsonProjection,
        ) -> Option<bool> {
            (name == "Read").then(|| {
                !input
                    .at_pointer("/force_serial")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
            })
        }

        fn map_result_text(
            &self,
            _name: &str,
            result: &Utf16JsonProjection,
        ) -> Option<Utf16JsonProjection> {
            result
                .string_at("/mapped")
                .map(|text| format!("child-mapped:{text}").into())
        }

        fn map_result_is_error(&self, _name: &str, result: &Utf16JsonProjection) -> Option<bool> {
            result
                .at_pointer("/mappedError")
                .and_then(|value| value.as_bool())
        }

        async fn parse_output(
            &self,
            _name: &str,
            output: &Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, ToolInvokerError> {
            if output.object_entries().is_some() {
                Ok(output.clone())
            } else {
                Err(ToolInvokerError::Validation("expected an object".into()))
            }
        }

        async fn invoke(
            &self,
            _name: &str,
            input: Utf16JsonProjection,
            _ctx: SubagentInvocationContext,
        ) -> Result<Utf16JsonProjection, ToolInvokerError> {
            self.calls.lock().unwrap().push(input.clone());
            Ok(input)
        }

        async fn invoke_detailed(
            &self,
            _name: &str,
            input: Utf16JsonProjection,
            _ctx: SubagentInvocationContext,
            _lease: Option<u64>,
        ) -> Result<ToolInvocationResult, ToolInvokerError> {
            self.calls.lock().unwrap().push(input.clone());
            let marker = input.string_at("/marker").map(str::to_owned);
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
            let mcp_meta = marker
                .as_ref()
                .map(|marker| source(&json!({"run": marker})));
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
                context: empty_array(),
                context_state,
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn child_context(agent_id: AgentId) -> SubagentInvocationContext {
        SubagentInvocationContext {
            cancellation_token: lingxi_core::host::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: Some(agent_id),
            native_query_depth: None,
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

    async fn setup(source_text: &str) -> (ModSubagentToolInvoker, Arc<Core>, Arc<Session>) {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("nested.js");
        std::fs::write(&module, source_text).unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load(
            "nested",
            dir.path(),
            &module,
            Utf16JsonProjection::from_serde_value(&json!({})).unwrap(),
        )
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
                source(&json!({"file_path":"original.txt"})),
                child_context(child),
                None,
            )
            .await
            .unwrap();
        let child_id = child.as_uuid().to_string();
        let calls = core.calls.lock().unwrap();
        assert_eq!(
            source_json(&calls[0]),
            json!({"file_path":"/tmp/child-agent/read.txt"})
        );
        assert_eq!(calls.len(), 1);
        drop(calls);
        assert_eq!(result.data.string_at("/agentId"), Some(child_id.as_str()));
        let mapped_text = result.data.to_json_string().unwrap();
        assert_eq!(
            result
                .model_content
                .as_ref()
                .and_then(|text| text.string_at("")),
            Some(mapped_text.as_str())
        );
    }

    #[tokio::test]
    async fn tool_concurrency_resolution_forwards_to_the_wrapped_registry() {
        let (invoker, _, _session) = setup("export function register(on) {}").await;
        assert_eq!(
            invoker.tool_is_concurrency_safe("Read", &source(&json!({}))),
            Some(true)
        );
        assert_eq!(
            invoker.tool_is_concurrency_safe("Read", &source(&json!({"force_serial": true}))),
            Some(false)
        );
        assert_eq!(
            invoker.tool_is_concurrency_safe("Unknown", &source(&json!({}))),
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
            invoker.invoke_detailed("Read", source(&json!({"file_path":"a"})), context_a, None),
            invoker.invoke_detailed("Read", source(&json!({"file_path":"b"})), context_b, None),
        );
        result_a.unwrap();
        result_b.unwrap();

        let mut seen = core
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|call| call.string_at("/file_path").map(str::to_owned))
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
                source(&json!({"file_path":"original.txt"})),
                child_context(AgentId::new()),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolInvokerError::Validation(reason) if reason.string_at("") == Some("blocked by Mod"))
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
                source(&json!({"file_path":"read.txt"})),
                child_context(AgentId::new()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(core.calls.lock().unwrap().len(), 1);
        assert_eq!(
            result
                .model_content
                .as_ref()
                .and_then(|text| text.string_at("")),
            Some("core text")
        );
        assert_eq!(
            source_json(&result.context),
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
                source(&json!({"file_path":"read.txt"})),
                child_context(AgentId::new()),
                None,
            )
            .await
            .unwrap();

        assert_eq!(core.calls.lock().unwrap().len(), 1);
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
    async fn child_input_lone_units_survive_the_mod_next_boundary() {
        let (invoker, core, _session) = setup(
            r#"export function register(on) {
            on('tool.call', { tool: 'Read' }, async ($, e, next) => await next(e));
        }"#,
        )
        .await;
        let input = Utf16JsonProjection::parse(r#"{"file_path":"lone\ud800.txt"}"#).unwrap();
        invoker
            .invoke_detailed("Read", input, child_context(AgentId::new()), None)
            .await
            .unwrap();
        assert_eq!(
            core.calls.lock().unwrap()[0].string_units("/file_path"),
            Some(vec![
                0x006c, 0x006f, 0x006e, 0x0065, 0xd800, 0x002e, 0x0074, 0x0078, 0x0074
            ])
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
                source(&json!({"file_path":"read.txt"})),
                child_context(AgentId::new()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(core.calls.lock().unwrap().len(), 1);
        assert_eq!(
            result
                .model_content
                .as_ref()
                .and_then(|text| text.string_at("")),
            Some("core text")
        );
    }

    #[tokio::test]
    async fn child_tool_call_rejects_a_result_outside_the_output_schema() {
        for (source_text, core_should_run) in [
            (
                "export function register(on) { on('tool.call', () => ({ result: 42 })); }",
                false,
            ),
            (
                "export function register(on) { on('tool.call', async ($, e, next) => ({ ...await next(e), result: 42 })); }",
                true,
            ),
        ] {
            let (invoker, core, _session) = setup(source_text).await;
            let result = invoker
                .invoke_detailed(
                    "Read",
                    source(&json!({"file_path":"read.txt"})),
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
                .as_ref()
                .and_then(|text| text.string_at(""))
                .is_some_and(|text| text.starts_with(
                    "<tool_use_error>tool.call step resolved Read with a result that does not match its output shape:"
                )));
            assert_eq!(source_json(&result.context), Value::Array(Vec::new()));
        }
    }

    #[tokio::test]
    async fn child_tool_call_uses_the_tools_mapper_for_a_replacement() {
        for source_text in [
            "export function register(on) { on('tool.call', () => ({ result: { mapped: 'direct' } })); }",
            "export function register(on) { on('tool.call', async ($, e, next) => ({ ...await next(e), result: { mapped: 'direct' } })); }",
        ] {
            let (invoker, _core, _session) = setup(source_text).await;
            let result = invoker
                .invoke_detailed(
                    "Read",
                    source(&json!({"file_path":"read.txt"})),
                    child_context(AgentId::new()),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(source_json(&result.data), json!({"mapped":"direct"}));
            assert_eq!(
                result
                    .model_content
                    .as_ref()
                    .and_then(|text| text.string_at("")),
                Some("child-mapped:direct")
            );
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
                source(&json!({"file_path":"read.txt"})),
                child_context(AgentId::new()),
                None,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert_eq!(
            result
                .model_content
                .as_ref()
                .and_then(|text| text.string_at("")),
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
                source(&json!({"file_path":"original.txt"})),
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
            source_json(&core.calls.lock().unwrap()[0]),
            json!({"file_path":"/tmp/child-agent/read.txt"})
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
                  throw new Error('unexpected native run refs: ' + first.ref + ', ' + second.ref);
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
                source(&json!({"file_path":"read.txt"})),
                child_context(AgentId::new()),
                None,
            )
            .await
            .expect("the selected native run resolves");

        let calls = core.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].string_at("/marker"), Some("one"));
        assert_eq!(calls[1].string_at("/marker"), Some("two"));
        drop(calls);
        assert_eq!(result.data.string_at("/marker"), Some("one"));
        assert_eq!(
            result
                .data
                .at_pointer("/selectedRewrite")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        let expected_model_content = result.data.to_json_string().unwrap();
        assert_eq!(
            result
                .model_content
                .as_ref()
                .and_then(|text| text.string_at("")),
            Some(expected_model_content.as_str())
        );
        assert_eq!(source_json(&result.context), json!(["selected context"]));
        assert_eq!(
            result.mcp_meta.as_ref().map(source_json),
            Some(json!({"run":"one"}))
        );
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
                source(&json!({"file_path":"read.txt"})),
                child_context(AgentId::new()),
                None,
            )
            .await
            .expect_err("deny is terminal even when a selected ref is present");

        assert!(
            matches!(error, ToolInvokerError::Validation(reason) if reason.string_at("") == Some("blocked by Mod"))
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
                source(&json!({"file_path":"read.txt"})),
                child_context(AgentId::new()),
                None,
            )
            .await
            .expect("an invalid ref can still carry a synthetic replacement result");

        assert_eq!(core.calls.lock().unwrap().len(), 1);
        assert_eq!(result.data.string_at("/marker"), Some("one"));
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
