//! Desktop execution bridge for slash commands registered by Mods.

use command_api::{
    ModCommandExecutor, ModCommandRunContext, ModCommandRunInterceptor, RegistrySlashDispatcher,
};
use hooks::mods::ModSessionContext;
use hooks::HookRegistry;
use lingxi_core::host::SlashDispatchResult;
use orchestrator::ConversationOrchestrator;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use tokio::sync::RwLock;

pub(super) struct DesktopModCommandExecutor {
    orchestrator: Weak<ConversationOrchestrator>,
    hooks: Arc<RwLock<HookRegistry>>,
}

impl DesktopModCommandExecutor {
    pub(super) fn new(
        orchestrator: Weak<ConversationOrchestrator>,
        hooks: Arc<RwLock<HookRegistry>>,
    ) -> Self {
        Self {
            orchestrator,
            hooks,
        }
    }
}

#[async_trait::async_trait]
impl ModCommandExecutor for DesktopModCommandExecutor {
    async fn run(
        &self,
        plugin: &str,
        command: &str,
        args: &str,
        context: ModCommandRunContext,
    ) -> Result<String, String> {
        let orch = self.orchestrator.upgrade().ok_or("Mod session ended")?;
        let host = self
            .hooks
            .read()
            .await
            .mod_host()
            .ok_or("Mod command host is unavailable")?;
        let name = command.to_owned();
        let core_name = name.clone();
        let core_plugin = plugin.to_owned();
        let log_orch = orch.clone();
        let toast_orch = orch.clone();
        let status_orch = orch.clone();
        let outcome = host
            .dispatch_with_ui_meta_at_session(
                "command.run",
                json!({
                    "command":name,
                    "args":args,
                    "origin":context.origin,
                    "presentation":{
                        "isFullscreen":context.is_fullscreen,
                        "columns":context.columns
                    }
                }),
                orch.as_ref(),
                move |event| {
                    let name = core_name.clone();
                    let plugin = core_plugin.clone();
                    async move {
                        if event.get("command").and_then(serde_json::Value::as_str)
                            != Some(name.as_str())
                        {
                            return Err(hooks::mods::ModError::Hook(
                                "command.run command is pinned".into(),
                            ));
                        }
                        Ok(json!({"text":format!(
                            "{plugin} registered /{name} but no command.run hook answered it: add on(\"command.run\", {{ command: \"{name}\" }}, ($, e) => ({{ text: ... }})) to the plugin."
                        )}))
                    }
                },
                move |plugin, text| {
                    let orch = log_orch.clone();
                    async move {
                        orch.emit_mod_log(&plugin, &text).await;
                    }
                },
                move |plugin, text, timeout_ms| {
                    let orch = toast_orch.clone();
                    async move {
                        orch.emit_mod_toast(&plugin, &text, timeout_ms).await;
                    }
                },
                move |plugin, text| {
                    let orch = status_orch.clone();
                    async move {
                        orch.emit_mod_status(&plugin, text.as_deref()).await;
                    }
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        command_api::record_mod_command_settlement(raw_command_settlement(&outcome.result, None));
        let text = outcome
            .result
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        Ok(attributed_text(
            text,
            &outcome.hooked,
            outcome.all_hooked_builtin,
        ))
    }
}

fn command_line(command: &str, args: &str) -> String {
    if args.is_empty() {
        format!("/{command}")
    } else {
        format!("/{command} {args}")
    }
}

fn core_result_value(result: &SlashDispatchResult, run_ref: usize) -> Value {
    match result {
        SlashDispatchResult::Handled { display } | SlashDispatchResult::Unknown { display, .. } => {
            json!({"text":display,"ref":run_ref})
        }
        SlashDispatchResult::RunAsTurn { .. } | SlashDispatchResult::NotASlashCommand => {
            json!({"ref":run_ref})
        }
    }
}

fn attributed_text(text: &str, hooked: &[String], all_hooked_builtin: bool) -> String {
    if hooked.is_empty() || all_hooked_builtin {
        text.to_owned()
    } else {
        format!("{}: {text}", hooked.join("+"))
    }
}

fn raw_command_settlement(answer: &Value, fallback_text: Option<&str>) -> Value {
    let mut settled = serde_json::Map::new();
    if let Some(text) = answer.get("text").and_then(Value::as_str).or(fallback_text) {
        settled.insert("text".into(), Value::String(text.to_owned()));
    }
    if let Some(context) = answer.get("context") {
        settled.insert("context".into(), context.clone());
    }
    if let Some(exit_code) = answer.get("exitCode") {
        settled.insert("exitCode".into(), exit_code.clone());
    }
    Value::Object(settled)
}

fn answer_to_slash_result(
    answer: &Value,
    runs: &[SlashDispatchResult],
    hooked: &[String],
    all_hooked_builtin: bool,
) -> SlashDispatchResult {
    let referenced = answer
        .get("ref")
        .and_then(Value::as_u64)
        .and_then(|reference| usize::try_from(reference).ok())
        .and_then(|reference| reference.checked_sub(1))
        .and_then(|index| runs.get(index));
    let selected = referenced.or_else(|| runs.last());
    match selected {
        Some(SlashDispatchResult::Handled { display }) => SlashDispatchResult::Handled {
            display: answer
                .get("text")
                .and_then(Value::as_str)
                .map(|text| {
                    if referenced.is_some() && text == display {
                        text.to_owned()
                    } else {
                        attributed_text(text, hooked, all_hooked_builtin)
                    }
                })
                .unwrap_or_else(|| display.clone()),
        },
        Some(other) => other.clone(),
        None => SlashDispatchResult::Handled {
            display: answer
                .get("text")
                .and_then(Value::as_str)
                .map(|text| attributed_text(text, hooked, all_hooked_builtin))
                .unwrap_or_default(),
        },
    }
}

#[async_trait::async_trait]
impl ModCommandRunInterceptor for DesktopModCommandExecutor {
    async fn run(
        &self,
        dispatcher: &RegistrySlashDispatcher,
        command: &str,
        args: &str,
        context: ModCommandRunContext,
    ) -> SlashDispatchResult {
        let original = command_line(command, args);
        let Some(orch) = self.orchestrator.upgrade() else {
            return dispatcher.dispatch_without_mod_hooks(&original).await;
        };
        let Some(host) = self.hooks.read().await.mod_host() else {
            return dispatcher.dispatch_without_mod_hooks(&original).await;
        };
        let runs = Arc::new(StdMutex::new(Vec::<SlashDispatchResult>::new()));
        let core_name = command.to_owned();
        let log_orch = orch.clone();
        let toast_orch = orch.clone();
        let status_orch = orch.clone();
        let answer = host
            .dispatch_with_ui_meta_at_session(
                "command.run",
                json!({
                    "command":command,
                    "args":args,
                    "origin":context.origin,
                    "presentation":{
                        "isFullscreen":context.is_fullscreen,
                        "columns":context.columns
                    }
                }),
                orch.as_ref(),
                {
                    let runs = runs.clone();
                    move |event| {
                        let name = core_name.clone();
                        let runs = runs.clone();
                        async move {
                            if event.get("command").and_then(Value::as_str) != Some(name.as_str()) {
                                return Err(hooks::mods::ModError::Hook(
                                    "command.run command is pinned".into(),
                                ));
                            }
                            let args =
                                event.get("args").and_then(Value::as_str).ok_or_else(|| {
                                    hooks::mods::ModError::Hook(
                                        "command.run args must be a string".into(),
                                    )
                                })?;
                            let result = dispatcher
                                .dispatch_without_mod_hooks(&command_line(&name, args))
                                .await;
                            let run_ref = {
                                let mut runs = runs
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                runs.push(result.clone());
                                runs.len()
                            };
                            Ok(core_result_value(&result, run_ref))
                        }
                    }
                },
                move |plugin, text| {
                    let orch = log_orch.clone();
                    async move { orch.emit_mod_log(&plugin, &text).await }
                },
                move |plugin, text, timeout_ms| {
                    let orch = toast_orch.clone();
                    async move { orch.emit_mod_toast(&plugin, &text, timeout_ms).await }
                },
                move |plugin, text| {
                    let orch = status_orch.clone();
                    async move { orch.emit_mod_status(&plugin, text.as_deref()).await }
                },
            )
            .await;
        let runs = runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match answer {
            Ok(outcome) => {
                let result = answer_to_slash_result(
                    &outcome.result,
                    &runs,
                    &outcome.hooked,
                    outcome.all_hooked_builtin,
                );
                if let SlashDispatchResult::Handled { display } = &result {
                    command_api::record_mod_command_settlement(raw_command_settlement(
                        &outcome.result,
                        Some(display),
                    ));
                }
                result
            }
            Err(error) => {
                tracing::warn!(command, %error, "Mod command.run dispatch failed");
                match runs.last() {
                    Some(result) => result.clone(),
                    None => dispatcher.dispatch_without_mod_hooks(&original).await,
                }
            }
        }
    }
}
