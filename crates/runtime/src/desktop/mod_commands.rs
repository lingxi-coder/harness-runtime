//! Desktop execution bridge for slash commands registered by Mods.

use command_api::{
    ModCommandExecutor, ModCommandRunContext, ModCommandRunInterceptor, RegistrySlashDispatcher,
};
use hooks::mods::ModSessionContext;
use hooks::HookRegistry;
use lingxi_core::host::SlashDispatchResult;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonBuilder, Utf16JsonProjection};
use orchestrator::ConversationOrchestrator;
use serde_json::Value;
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

fn source_from_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("command projection is a root string")
}

fn source_string(value: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::from(value)
}

fn source_object(fields: Vec<(String, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(fields).expect("command event object is valid JSON")
}

fn join_source_strings(parts: &[Utf16JsonProjection]) -> Utf16JsonProjection {
    let mut units = Vec::new();
    for part in parts {
        units.extend(
            part.string_units("")
                .expect("composed text is a root string"),
        );
    }
    source_from_units(units)
}

fn source_bool(value: bool) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&Value::Bool(value))
        .expect("generated boolean is valid JSON")
}

fn source_number(value: usize) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&Value::from(value))
        .expect("generated integer is valid JSON")
}

fn copy_view(view: JsonValueView<'_>) -> Utf16JsonProjection {
    enum Work<'a> {
        Visit(JsonValueView<'a>),
        FinishArray(usize),
        FinishObject(Vec<Vec<u16>>),
    }

    let mut builder = Utf16JsonBuilder::new();
    let mut work = vec![Work::Visit(view)];
    let mut values = Vec::<usize>::new();
    while let Some(frame) = work.pop() {
        match frame {
            Work::Visit(view) => {
                if let Some(units) = view.code_units() {
                    values.push(builder.push_string_units(units.to_vec()));
                } else if let Some(length) = view.array_len() {
                    work.push(Work::FinishArray(length));
                    for index in (0..length).rev() {
                        if let Some(child) = view.get_index(index) {
                            work.push(Work::Visit(child));
                        }
                    }
                } else if let Some(entries) = view.object_entries() {
                    let keys = entries
                        .iter()
                        .map(|entry| entry.code_units.to_vec())
                        .collect::<Vec<_>>();
                    work.push(Work::FinishObject(keys));
                    for entry in entries.into_iter().rev() {
                        work.push(Work::Visit(entry.value));
                    }
                } else if view.is_null() {
                    values.push(builder.push_null());
                } else if let Some(value) = view.as_bool() {
                    values.push(builder.push_bool(value));
                } else if let Some(value) = view.as_number() {
                    values.push(builder.push_number(value));
                } else {
                    unreachable!("JSON view must identify a JSON node")
                }
            }
            Work::FinishArray(length) => {
                let start = values
                    .len()
                    .checked_sub(length)
                    .expect("array children copied");
                let children = values.split_off(start);
                values.push(
                    builder
                        .push_array(children)
                        .expect("copied array is valid JSON"),
                );
            }
            Work::FinishObject(keys) => {
                let start = values
                    .len()
                    .checked_sub(keys.len())
                    .expect("object children copied");
                let children = values.split_off(start);
                let fields = keys.into_iter().zip(children).collect();
                values.push(
                    builder
                        .push_object_units(fields)
                        .expect("copied object is valid JSON"),
                );
            }
        }
    }
    debug_assert_eq!(values.len(), 1);
    builder
        .finish(values.pop().expect("one root value copied"))
        .expect("copied command result subtree is valid JSON")
}

fn source_value_at(value: &Utf16JsonProjection, pointer: &str) -> Option<Utf16JsonProjection> {
    value.at_pointer(pointer).map(copy_view)
}

fn source_string_at(value: &Utf16JsonProjection, pointer: &str) -> Option<Utf16JsonProjection> {
    let units = value.at_pointer(pointer)?.code_units()?.to_vec();
    Some(source_from_units(units))
}

fn display_text(value: &Utf16JsonProjection) -> String {
    String::from_utf16_lossy(&value.string_units("").unwrap_or_default())
}

fn command_line(command: &Utf16JsonProjection, args: &Utf16JsonProjection) -> Utf16JsonProjection {
    let command_units = command
        .string_units("")
        .expect("slash command name is a root string");
    let args_units = args
        .string_units("")
        .expect("slash command arguments are a root string");
    let mut units = Vec::with_capacity(1 + command_units.len() + args_units.len() + 1);
    units.push(u16::from(b'/'));
    units.extend(command_units);
    if !args_units.is_empty() {
        units.push(u16::from(b' '));
        units.extend(args_units);
    }
    source_from_units(units)
}

fn generated_event_input(
    command: &Utf16JsonProjection,
    args: &Utf16JsonProjection,
    context: ModCommandRunContext,
) -> Utf16JsonProjection {
    let origin = Utf16JsonProjection::from_serde_value(&context.origin)
        .expect("host-generated command origin is JSON");
    let presentation = source_object(vec![
        (
            "isFullscreen".to_owned(),
            source_bool(context.is_fullscreen),
        ),
        ("columns".to_owned(), source_number(context.columns.into())),
    ]);
    source_object(vec![
        ("command".to_owned(), command.clone()),
        ("args".to_owned(), args.clone()),
        ("origin".to_owned(), origin),
        ("presentation".to_owned(), presentation),
    ])
}

fn core_result_projection(result: &SlashDispatchResult, run_ref: usize) -> Utf16JsonProjection {
    let mut fields = Vec::new();
    match result {
        SlashDispatchResult::Handled { display } | SlashDispatchResult::Unknown { display, .. } => {
            fields.push(("text".to_owned(), display.clone()));
        }
        // The Mod `command.run` `next()` path has already pinned a non-empty
        // registered command name before dispatch, so ParseFailure is
        // unreachable there. Keep its result shape in the no-display branch;
        // the top-level host handles syntax failure and ordinary-turn fallback.
        SlashDispatchResult::RunAsTurn { .. }
        | SlashDispatchResult::NotASlashCommand
        | SlashDispatchResult::ParseFailure { .. } => {}
    }
    fields.push(("ref".to_owned(), source_number(run_ref)));
    source_object(fields)
}

fn attributed_text(
    text: &Utf16JsonProjection,
    hooked: &[String],
    all_hooked_builtin: bool,
) -> Utf16JsonProjection {
    if hooked.is_empty() || all_hooked_builtin {
        return text.clone();
    }
    let prefix = format!("{}: ", hooked.join("+"));
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.extend(
        text.string_units("")
            .expect("hook result text is a root string"),
    );
    source_from_units(units)
}

fn raw_command_settlement(
    answer: &Utf16JsonProjection,
    fallback_text: Option<&Utf16JsonProjection>,
) -> Utf16JsonProjection {
    let mut fields = Vec::new();
    if let Some(text) = source_string_at(answer, "/text").or_else(|| fallback_text.cloned()) {
        fields.push(("text".to_owned(), text));
    }
    if let Some(context) = source_value_at(answer, "/context") {
        fields.push(("context".to_owned(), context));
    }
    if let Some(exit_code) = source_value_at(answer, "/exitCode") {
        fields.push(("exitCode".to_owned(), exit_code));
    }
    source_object(fields)
}

fn answer_to_slash_result(
    answer: &Utf16JsonProjection,
    runs: &[SlashDispatchResult],
    hooked: &[String],
    all_hooked_builtin: bool,
) -> SlashDispatchResult {
    let referenced = answer
        .at_pointer("/ref")
        .and_then(|value| value.as_number())
        .filter(|value| value.is_finite() && *value >= 1.0 && value.fract() == 0.0)
        .and_then(|reference| usize::try_from(reference as u64).ok())
        .and_then(|reference| reference.checked_sub(1))
        .and_then(|index| runs.get(index));
    let selected = referenced.or_else(|| runs.last());
    match selected {
        Some(SlashDispatchResult::Handled { display }) => {
            let chosen = source_string_at(answer, "/text");
            SlashDispatchResult::Handled {
                display: chosen
                    .map(|text| {
                        if referenced.is_some() && text.string_units("") == display.string_units("")
                        {
                            display.clone()
                        } else {
                            attributed_text(&text, hooked, all_hooked_builtin)
                        }
                    })
                    .unwrap_or_else(|| display.clone()),
            }
        }
        Some(other) => other.clone(),
        None => SlashDispatchResult::Handled {
            display: source_string_at(answer, "/text")
                .map(|text| attributed_text(&text, hooked, all_hooked_builtin))
                .unwrap_or_else(|| source_string("")),
        },
    }
}

#[async_trait::async_trait]
impl ModCommandExecutor for DesktopModCommandExecutor {
    async fn run(
        &self,
        plugin: &str,
        command: &Utf16JsonProjection,
        args: &Utf16JsonProjection,
        context: ModCommandRunContext,
    ) -> Result<Utf16JsonProjection, Utf16JsonProjection> {
        let orch = self
            .orchestrator
            .upgrade()
            .ok_or_else(|| source_string("Mod session ended"))?;
        let host = self
            .hooks
            .read()
            .await
            .mod_host()
            .ok_or_else(|| source_string("Mod command host is unavailable"))?;
        let name = command.clone();
        let plugin_id = plugin.to_owned();
        let log_orch = orch.clone();
        let toast_orch = orch.clone();
        let status_orch = orch.clone();
        let outcome = host
            .dispatch_with_ui_meta_at_session(
                "command.run",
                generated_event_input(command, args, context),
                orch.as_ref(),
                move |event| {
                    let command = name.clone();
                    let plugin = plugin_id.clone();
                    async move {
                        let Some(event) = event else {
                            return Err(hooks::mods::ModError::Hook(
                                "command.run event is missing".into(),
                            ));
                        };
                        if source_string_at(&event, "/command")
                            .and_then(|value| value.string_units(""))
                            != command.string_units("")
                        {
                            return Err(hooks::mods::ModError::Hook(
                                "command.run command is pinned".into(),
                            ));
                        }
                        let args = source_string_at(&event, "/args").ok_or_else(|| {
                            hooks::mods::ModError::Hook(
                                "command.run args must be a string".into(),
                            )
                        })?;
                        let prompt = join_source_strings(&[
                            source_string(&format!("{plugin} registered /")),
                            command.clone(),
                            source_string(
                                " but no command.run hook answered it: add on(\"command.run\", { command: \"",
                            ),
                            command.clone(),
                            source_string("\" }, ($, e) => ({ text: ... })) to the plugin."),
                        ]);
                        let result = source_object(vec![(
                            "text".to_owned(),
                            prompt,
                        )]);
                        let _ = args;
                        Ok(Some(result))
                    }
                },
                move |plugin, text| {
                    let orch = log_orch.clone();
                    async move { orch.emit_mod_log(&plugin, text).await }
                },
                move |plugin, text, timeout_ms| {
                    let orch = toast_orch.clone();
                    async move { orch.emit_mod_toast(&plugin, text, timeout_ms).await }
                },
                move |plugin, text| {
                    let orch = status_orch.clone();
                    async move { orch.emit_mod_status(&plugin, text).await }
                },
            )
            .await
            .map_err(|error| source_string(&error.to_string()))?;
        let result = answer_to_slash_result(
            &outcome.result_projection,
            &[],
            &outcome.hooked,
            outcome.all_hooked_builtin,
        );
        let settlement = raw_command_settlement(&outcome.result_projection, None);
        command_api::record_mod_command_settlement(settlement);
        let text =
            source_string_at(&outcome.result_projection, "/text").unwrap_or_else(
                || match &result {
                    SlashDispatchResult::Handled { display } => display.clone(),
                    _ => source_string(""),
                },
            );
        Ok(attributed_text(
            &text,
            &outcome.hooked,
            outcome.all_hooked_builtin,
        ))
    }
}

#[async_trait::async_trait]
impl ModCommandRunInterceptor for DesktopModCommandExecutor {
    async fn run(
        &self,
        dispatcher: &RegistrySlashDispatcher,
        command: &Utf16JsonProjection,
        args: &Utf16JsonProjection,
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
        let command_name = command.clone();
        let log_orch = orch.clone();
        let toast_orch = orch.clone();
        let status_orch = orch.clone();
        let answer = host
            .dispatch_with_ui_meta_at_session(
                "command.run",
                generated_event_input(command, args, context),
                orch.as_ref(),
                {
                    let runs = runs.clone();
                    move |event| {
                        let command_name = command_name.clone();
                        let runs = runs.clone();
                        async move {
                            let event = event.ok_or_else(|| {
                                hooks::mods::ModError::Hook("command.run event is missing".into())
                            })?;
                            if source_string_at(&event, "/command")
                                .and_then(|value| value.string_units(""))
                                != command_name.string_units("")
                            {
                                return Err(hooks::mods::ModError::Hook(
                                    "command.run command is pinned".into(),
                                ));
                            }
                            let args = source_string_at(&event, "/args").ok_or_else(|| {
                                hooks::mods::ModError::Hook(
                                    "command.run args must be a string".into(),
                                )
                            })?;
                            let result = dispatcher
                                .dispatch_without_mod_hooks(&command_line(&command_name, &args))
                                .await;
                            let run_ref = {
                                let mut runs = runs
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                runs.push(result.clone());
                                runs.len()
                            };
                            Ok(Some(core_result_projection(&result, run_ref)))
                        }
                    }
                },
                move |plugin, text| {
                    let orch = log_orch.clone();
                    async move { orch.emit_mod_log(&plugin, text).await }
                },
                move |plugin, text, timeout_ms| {
                    let orch = toast_orch.clone();
                    async move { orch.emit_mod_toast(&plugin, text, timeout_ms).await }
                },
                move |plugin, text| {
                    let orch = status_orch.clone();
                    async move { orch.emit_mod_status(&plugin, text).await }
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
                    &outcome.result_projection,
                    &runs,
                    &outcome.hooked,
                    outcome.all_hooked_builtin,
                );
                if let SlashDispatchResult::Handled { display } = &result {
                    command_api::record_mod_command_settlement(raw_command_settlement(
                        &outcome.result_projection,
                        Some(display),
                    ));
                }
                result
            }
            Err(error) => {
                tracing::warn!(command = %display_text(command), %error, "Mod command.run dispatch failed");
                match runs.last() {
                    Some(result) => result.clone(),
                    None => dispatcher.dispatch_without_mod_hooks(&original).await,
                }
            }
        }
    }
}
