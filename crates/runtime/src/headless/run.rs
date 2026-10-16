//! One-shot conversation: feed the prompt, run the orchestrator, print
//! results, exit. Also hosts the resume entrypoint.
//!
//! (M7-12) `--resume` is a three-way split routed by the pure [`resume_route`]:
//!   - `--resume <uuid>`            → [`run_resume_by_id`] (load by id).
//!   - `--resume` (no id) + TTY     → [`run_resume_iocraft`] (the iocraft
//!     Resume screen over the M5-08 loader).
//!   - `--resume` (no id) + non-TTY → [`run_resume_stdio_picker`] (the
//!     unchanged M5-08 `select_session_interactive` stdio fallback).

use crate::headless::config::HeadlessOptions as Argv;
use crate::headless::control_plane::StdioControlPlane;
use crate::headless::exit_codes;
use crate::headless::output::OutputSink;
use crate::headless::stream_json::{
    build_init_params, permission_mode_str, StreamJsonInitHostMetadata, StreamJsonStream,
};
use crate::headless::stream_json_input::{
    content_to_prompt, emit_projected_frame_queued, emit_replay_ack_projected_queued,
    emit_replay_ack_queued, ControlPlaneWriter, StdinControlFrame, StdinReaderStatus, StreamInput,
};
use crate::headless::HeadlessRuntime as Runtime;
use control::{
    dispatch_control_request, flag_settings_fast_mode_opt_in, model_capabilities,
    recover_orphaned_permission, resolve_fast_mode_disabled_reason, resolve_fast_mode_state,
};
#[cfg(test)]
use fusion::await_local_fusion_result_bounded;
use fusion::{
    await_local_fusion_result, fusion_result_exit_code, fusion_spawn_failure_exit_code,
    local_fusion_task_id_to_await,
};
pub(crate) use lifecycle::PrintAuxTaskGroup;
use lifecycle::{
    finish_print_branch, run_print_owned, run_print_owned_with_cleanup,
    stop_background_agents_at_budget, stop_print_tasks, wind_down_print_tasks,
};
use lingxi_core::host::{McpStatus, OrchestratorHandle, SlashDispatchResult};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use permission;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use suggestions::{SdkPromptSuggestions, StreamFileSuggestionIndex};
mod control;
mod fusion;
#[path = "run/initialize_commands.rs"]
mod initialize_commands;
mod lifecycle;
mod result_holding;
pub(super) mod suggestions;

fn init_host_metadata(runtime: &Runtime) -> StreamJsonInitHostMetadata {
    let present = |name: &str| {
        runtime
            .services
            .environment_variable(name)
            .is_some_and(|value| !value.is_empty())
    };
    let essential_only = present(branding::DISABLE_NONESSENTIAL_TRAFFIC_ENV)
        || present("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC");
    let do_not_track = runtime.services.environment_variable("DO_NOT_TRACK");
    let cwd = runtime.session_cwd.cwd();
    let global = runtime.services.trust_config_path();
    StreamJsonInitHostMetadata {
        mcp_server_errors: crate::headless::stream_json::collect_mcp_server_errors(
            &cwd,
            global.as_deref(),
        ),
        cwd,
        analytics_disabled: essential_only
            || present("DISABLE_TELEMETRY")
            || lingxi_core::host::env::is_env_truthy(do_not_track.as_deref()),
        product_feedback_disabled: essential_only,
        per_turn_effort_active: Some(true),
        view_mode: Some("default".into()),
    }
}

async fn init_plugins(runtime: &Runtime) -> Vec<crate::headless::stream_json::StreamJsonPlugin> {
    let Some(plugins) = &runtime.plugin_runtime else {
        return Vec::new();
    };
    plugins
        .loaded_plugins()
        .await
        .into_iter()
        .map(|(manifest, path)| {
            let builtin = matches!(manifest.source, plugin::source::PluginSource::BuiltIn);
            // Installed marketplace plugins retain their canonical cache layout;
            // session directory plugins use the native inline source identity.
            let marketplace = path
                .parent()
                .and_then(std::path::Path::parent)
                .filter(|marketplace| {
                    marketplace
                        .parent()
                        .and_then(std::path::Path::file_name)
                        .is_some_and(|name| name == "cache")
                })
                .and_then(std::path::Path::file_name)
                .and_then(std::ffi::OsStr::to_str);
            let source = format!(
                "{}@{}",
                manifest.name,
                if builtin {
                    "builtin"
                } else {
                    marketplace.unwrap_or("inline")
                }
            );
            crate::headless::stream_json::StreamJsonPlugin {
                name: manifest.name,
                path: if builtin {
                    "builtin".into()
                } else {
                    path.to_string_lossy().into_owned()
                },
                source,
                version: (!manifest.version.is_empty()).then_some(manifest.version),
            }
        })
        .collect()
}

async fn init_output_style(
    orchestrator: &orchestrator::ConversationOrchestrator,
) -> Utf16JsonProjection {
    orchestrator
        .active_output_style_setting()
        .await
        .filter(|style| {
            style
                .string_units("")
                .is_some_and(|units| !units.is_empty())
        })
        .unwrap_or_else(|| "default".into())
}

async fn init_command_projections(
    runtime: &Runtime,
) -> Result<
    (
        Vec<Utf16JsonProjection>,
        Vec<Utf16JsonProjection>,
        Vec<Utf16JsonProjection>,
    ),
    String,
> {
    let visibility = runtime
        .command_visibility_snapshot()
        .map_err(|error| error.to_string())?;
    let registry = runtime.dispatcher.registry();
    let registry = registry.read().await;

    // Native init is `s7e({commands: obt(commands), skills})`: the slash list
    // comes from the non-interactive command pool, then applies the
    // user-invocable projection. Terminal metadata is taken from the same
    // descriptors and keeps their source order.
    let mut slash_commands = Vec::new();
    let mut terminal_slash_commands = Vec::new();
    for command in registry.headless_init_commands() {
        if command.user_invocable == Some(false) || visibility.command_is_off(command) {
            continue;
        }
        slash_commands.push(command.name.clone());
        if command.terminal_oriented {
            terminal_slash_commands.push(command.name.clone());
        }
    }

    // Native's skill list is built from the pre-bge prompt pool and then
    // projected by `userInvocable`; it is not the `loadedFrom === "skills"`
    // subset of slash commands. Keep exact source order and deduplicate by
    // UTF-16 identity, matching the source catalog merge without decoding
    // lone surrogates through UTF-8.
    let mut seen_skill_names = HashSet::new();
    let skills = registry
        .headless_init_skills()
        .into_iter()
        .filter(|command| !visibility.command_is_off(command))
        .filter(|command| {
            command
                .name
                .string_units("")
                .is_some_and(|units| seen_skill_names.insert(units))
        })
        .filter(|command| command.user_invocable != Some(false))
        .map(|command| command.name.clone())
        .collect();

    Ok((slash_commands, terminal_slash_commands, skills))
}

fn stream_json_error_subtype(err: &orchestrator::OrchestratorError) -> &'static str {
    match err {
        orchestrator::OrchestratorError::MaxTurnsReached { .. } => "error_max_turns",
        orchestrator::OrchestratorError::MaxBudgetReached { .. } => "error_max_budget_usd",
        orchestrator::OrchestratorError::MaxStructuredOutputRetries { .. } => {
            "error_max_structured_output_retries"
        }
        _ => "error_during_execution",
    }
}

/// Render a non-negative finite `f64` like JavaScript `Number#toFixed(2)`.
/// Rust's precision formatter uses ties-to-even (`1.125 -> 1.12`), while the
/// ECMAScript rule chooses the larger decimal integer on an exact tie
/// (`1.125 -> 1.13`). Work from the exact IEEE-754 rational so nearby values
/// such as `2.675` still produce JavaScript's `2.67`.
fn js_to_fixed_2(value: f64) -> String {
    debug_assert!(value.is_finite() && value >= 0.0);

    let bits = value.to_bits();
    let raw_exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1_u64 << 52) - 1);
    let (significand, exponent) = if raw_exponent == 0 {
        (u128::from(fraction), -1022 - 52)
    } else {
        (
            u128::from(fraction | (1_u64 << 52)),
            raw_exponent - 1023 - 52,
        )
    };
    let scaled = significand * 100;
    let cents = if exponent >= 0 {
        scaled << exponent.unsigned_abs()
    } else {
        let shift = exponent.unsigned_abs();
        if shift >= 128 {
            0
        } else {
            let whole = scaled >> shift;
            let remainder = scaled & ((1_u128 << shift) - 1);
            let halfway = 1_u128 << (shift - 1);
            whole + u128::from(remainder >= halfway)
        }
    };

    format!("{}.{:02}", cents / 100, cents % 100)
}

/// Per-print transport state shared by user queries and task-notification
/// queries. It publishes owner snapshots before starting an isolated SDK
/// prompt suggestion from the completed query cache.
struct NativeQueryResultPublisher<'a> {
    runtime: &'a Runtime,
    stream: &'a Arc<StreamJsonStream>,
    prompt_suggestions: Arc<SdkPromptSuggestions>,
    suggestion_turn_origin: Option<&'static str>,
    input_status: Option<tokio::sync::watch::Receiver<StdinReaderStatus>>,
    result_hold: result_holding::SdkResultHold,
    max_budget_usd: Option<f64>,
    fast_mode_state: &'a str,
    fast_mode_disabled_reason: Option<&'a str>,
    betas: &'a [String],
    next_index: u64,
    last_error_published: bool,
    last_result_failed: bool,
}

impl NativeQueryResultPublisher<'_> {
    async fn should_hold_results(&mut self) -> bool {
        let input_closed = self
            .input_status
            .as_ref()
            .is_none_or(|status| !matches!(*status.borrow(), StdinReaderStatus::Reading));
        let tasks = lingxi_core::host::task_registry::TaskRegistryHandle::list(
            self.runtime.task_registry.as_ref(),
            Default::default(),
        )
        .await
        .unwrap_or_default();
        self.result_hold
            .should_hold(input_closed, &tasks, suggestions::now_ms())
    }

    async fn release_held_results(&mut self, stopping: bool) {
        if self.runtime.policy_helper_refusal.current().is_some() {
            return;
        }
        let cost = self.runtime.orchestrator.snapshot_cost().await;
        self.stream.refresh_held_result_totals(
            &cost,
            self.betas,
            self.runtime
                .agent_session_statistics()
                .await
                .map(|stats| serde_json::to_value(stats).expect("agent statistics")),
        );
        if self.runtime.policy_helper_refusal.current().is_some() {
            return;
        }
        self.stream.release_sdk_results(stopping);
        self.prompt_suggestions.emit_pending(self.stream);
        let _ = self.stream.flush().await;
    }

    fn begin_query(&mut self) {
        self.last_error_published = false;
        self.last_result_failed = false;
        self.runtime
            .execution_interrupted
            .store(false, std::sync::atomic::Ordering::Release);
        self.stream.begin_query_timing();
    }

    async fn begin_notification_query(&mut self) {
        self.prompt_suggestions
            .on_command(self.runtime, true, None)
            .await;
        self.suggestion_turn_origin = Some("task_notification");
        self.begin_query();
        let _ = self.stream.begin_request_markers(None, Vec::new(), false);
        self.stream.emit_init().await;
    }

    async fn publish(
        &mut self,
        result: &Result<orchestrator::TurnOutcome, orchestrator::OrchestratorError>,
        queued: usize,
    ) {
        if self.runtime.policy_helper_refusal.current().is_some() {
            return;
        }
        if result.is_ok() {
            self.runtime.execution_interrupted.store(
                matches!(result, Ok(orchestrator::TurnOutcome::Cancelled)),
                std::sync::atomic::Ordering::Release,
            );
        }
        let limit_error = match result {
            Ok(orchestrator::TurnOutcome::MaxTurns) => {
                Some(Err(orchestrator::OrchestratorError::MaxTurnsReached {
                    max_turns: self.runtime.max_turns.unwrap_or_else(|| {
                        self.runtime
                            .orchestrator
                            .completed_turn_metrics()
                            .map(|metrics| metrics.num_turns)
                            .unwrap_or_default()
                    }),
                }))
            }
            _ => None,
        };
        let result = limit_error.as_ref().unwrap_or(result);
        stop_background_agents_at_budget(
            self.max_budget_usd,
            self.runtime.orchestrator.as_ref(),
            self.runtime.task_registry.as_ref(),
            &self.runtime.output,
        )
        .await;
        let cost = self.runtime.orchestrator.snapshot_cost().await;
        let hold_results = self.should_hold_results().await;
        self.stream.hold_sdk_results(hold_results);
        self.stream.refresh_held_result_totals(
            &cost,
            self.betas,
            self.runtime
                .agent_session_statistics()
                .await
                .map(|stats| serde_json::to_value(stats).expect("agent statistics")),
        );
        set_result_position(self.runtime, self.stream, queued, self.next_index).await;
        if self.runtime.policy_helper_refusal.current().is_some() {
            return;
        }
        self.last_result_failed = result.is_err()
            || self
                .runtime
                .orchestrator
                .completed_turn_metrics()
                .and_then(|metrics| metrics.api_error_stop_reason)
                .is_some();
        match result {
            Ok(_) => {
                let text = self.stream.get_last_result_text().await;
                if self.last_result_failed {
                    self.runtime.record_execution_error(
                        self.runtime
                            .orchestrator
                            .session()
                            .lock()
                            .await
                            .history
                            .last()
                            .map(|message| message.text_content())
                            .unwrap_or_default(),
                    );
                }
                self.stream
                    .emit_result_success(
                        &text,
                        "end_turn",
                        &cost,
                        self.fast_mode_state,
                        self.fast_mode_disabled_reason,
                        self.betas,
                    )
                    .await;
                self.last_error_published = false;
            }
            Err(error) => {
                self.runtime.record_execution_error(error);
                self.stream
                    .emit_result_error(
                        stream_json_error_subtype(error),
                        vec![error.to_string()],
                        &cost,
                        self.fast_mode_state,
                        self.fast_mode_disabled_reason,
                        self.betas,
                    )
                    .await;
                self.last_error_published = true;
            }
        }
        self.next_index = self.next_index.saturating_add(1);
        // Flush ordinary frames now; a result retained for delegated work stays
        // behind the SDK publication boundary with its pending suggestion.
        let _ = self.stream.flush().await;
        self.prompt_suggestions
            .after_result(self.runtime, self.stream, self.suggestion_turn_origin)
            .await;
    }
}

/// Drive a one-shot conversation: either a `/slash-command` or a normal
/// prompt that runs through the orchestrator turn loop.
pub fn run_oneshot<'a>(
    argv: &'a Argv,
    runtime: &'a Runtime,
    sink: &'a dyn OutputSink,
) -> futures::future::BoxFuture<'a, i32> {
    let operation = Box::pin(run_oneshot_inner(argv, runtime, sink));
    Box::pin(run_print_owned(runtime, operation))
}

async fn run_oneshot_inner(argv: &Argv, runtime: &Runtime, sink: &dyn OutputSink) -> i32 {
    runtime.activate_deferred_startup().await;
    let prompt = argv.prompt.clone().unwrap_or_default();
    if prompt.trim().is_empty() {
        // Byte-parity with claude-code print.ts: the empty-input error in print
        // mode is this exact line, then exit 1 (ARGV_ERROR == 1 post-flip).
        runtime.record_execution_error("Error: Input must be provided either through stdin or as a prompt argument when using --print");
        let _ = runtime.output.write_line("Error: Input must be provided either through stdin or as a prompt argument when using --print").await;
        return exit_codes::ARGV_ERROR;
    }
    if let Err(error) = runtime.flush_prepared_fork_history().await {
        runtime.record_execution_error(&error);
        if runtime.policy_helper_refusal.current().is_none() {
            sink.error("runtime", &error).await;
        }
        return exit_codes::RUNTIME_ERROR;
    }

    // This producer is the actual print-mode user prompt, before generic
    // slash dispatch (which is also used by non-human internal callers).
    let prompt_source = Utf16JsonProjection::from(prompt.as_str());
    let (command, args) = prompt
        .split_once(char::is_whitespace)
        .unwrap_or((&prompt, ""));
    if command == "/tasks" {
        if let Some(parsed) = lingxi_core::host::human_task_message::parse(args) {
            let result = match parsed {
                Ok((task_id, message)) if runtime.orchestrator.workspace_trusted().await => {
                    let message = Utf16JsonProjection::from(message);
                    runtime
                        .task_registry
                        .send_human_task_message(task_id, &message)
                        .await
                        .map(|()| format!("Message accepted for task {task_id}"))
                        .map_err(|error| error.to_string())
                }
                Ok(_) => Err("Trust this workspace before messaging a task".into()),
                Err(error) => Err(error.into()),
            };
            let code = match result {
                Ok(display) => {
                    sink.command_output(&prompt, &display).await;
                    exit_codes::SUCCESS
                }
                Err(error) => {
                    runtime.record_execution_error(&error);
                    if runtime.policy_helper_refusal.current().is_none() {
                        sink.error("task_message", &error).await;
                    }
                    exit_codes::RUNTIME_ERROR
                }
            };
            return finish_print_branch(runtime, argv.max_budget_usd, sink, code).await;
        }
    }
    // Local slash commands finish here; unknown or malformed slash input falls
    // back to a model turn through the same execution owner. Keep its large
    // child future on the heap to limit poll-time stack growth.
    if prompt.starts_with('/') {
        let code = Box::pin(run_slash_command_with_budget(
            &prompt_source,
            runtime,
            argv.max_budget_usd,
            argv.no_stream,
            sink,
        ))
        .await;
        return finish_print_branch(runtime, argv.max_budget_usd, sink, code).await;
    }

    // Structured-output branch (`--json-schema`): `crate::desktop::build` wires
    // the StructuredOutput tool and requests forced tool choice. Providers that
    // reject that request still receive an explicit prompt requirement here. We
    // validate the captured result against the schema and retry. Only active when
    // `build()` surfaced a capture slot (`--json-schema` + `--print`).
    if let Some(slot) = runtime.structured_output_slot.clone() {
        if argv.json_schema.is_some() {
            let code = run_structured_output(
                runtime,
                &prompt,
                &slot,
                argv.prompt_images.clone(),
                argv.max_budget_usd,
                sink,
            )
            .await;
            return finish_print_branch(runtime, argv.max_budget_usd, sink, code).await;
        }
    }

    // Non-slash branch — drive the orchestrator turn loop. Without a real
    // ANTHROPIC_API_KEY this returns 401; we surface the error verbatim.
    sink.turn_start().await;
    let turn_result = if argv.no_stream {
        if argv.prompt_images.is_empty() {
            orchestrator::native_user_message_source::with_sdk_source(
                orchestrator::mod_prompt_origin::with_origin(
                    serde_json::json!({"kind":"sdk"}),
                    runtime
                        .orchestrator
                        .run_turn_with_cancel(prompt_source.clone(), runtime.turn_cancel()),
                ),
            )
            .await
        } else {
            orchestrator::native_user_message_source::with_sdk_source(
                orchestrator::mod_prompt_origin::with_origin(
                    serde_json::json!({"kind":"sdk"}),
                    runtime.orchestrator.run_turn_with_cancel_image_sources(
                        prompt_source.clone(),
                        argv.prompt_images.clone(),
                        runtime.turn_cancel(),
                    ),
                ),
            )
            .await
        }
    } else {
        orchestrator::native_user_message_source::with_sdk_source(
            orchestrator::mod_prompt_origin::with_origin(
                serde_json::json!({"kind":"sdk"}),
                runtime
                    .orchestrator
                    .run_turn_streaming_with_cancel_image_sources(
                        prompt_source.clone(),
                        argv.prompt_images.clone(),
                        runtime.turn_cancel(),
                    ),
            ),
        )
        .await
    };
    runtime.execution_interrupted.store(
        matches!(&turn_result, Ok(orchestrator::TurnOutcome::Cancelled)),
        std::sync::atomic::Ordering::Release,
    );
    let turn_result = match turn_result {
        Ok(outcome) => wind_down_print_tasks(
            runtime,
            argv.max_budget_usd,
            runtime.shutdown.child_token(),
            None,
            None,
        )
        .await
        .map(|()| outcome),
        Err(error) => {
            stop_print_tasks(runtime).await;
            Err(error)
        }
    };
    stop_background_agents_at_budget(
        argv.max_budget_usd,
        runtime.orchestrator.as_ref(),
        runtime.task_registry.as_ref(),
        &runtime.output,
    )
    .await;
    match turn_result {
        Ok(_outcome) => exit_codes::SUCCESS,
        Err(e) => {
            runtime.record_execution_error(&e);
            if runtime.policy_helper_refusal.current().is_none() {
                sink.error("runtime", &e.to_string()).await;
            }
            exit_codes::RUNTIME_ERROR
        }
    }
}

/// Recognize only the built-in /compact command, preserving its focus text.
fn compact_command_instructions(prompt: &str) -> Option<&str> {
    let tail = prompt.trim().strip_prefix("/compact")?;
    (tail.is_empty() || tail.starts_with(char::is_whitespace)).then(|| tail.trim())
}

async fn run_stream_compact_command(
    argv: &Argv,
    runtime: &Runtime,
    stream: &StreamJsonStream,
    instructions: &str,
    command_uuid: Option<&str>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let started = std::time::Instant::now();
    let uuid = command_uuid.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string);
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let instructions_source = Utf16JsonProjection::from(instructions);
    let result = runtime
        .orchestrator
        .force_compact_with_instructions_and_cancel(
            (!instructions.is_empty()).then_some(&instructions_source),
            cancel,
        )
        .await;
    if runtime.policy_helper_refusal.current().is_some() {
        return;
    }
    // A successful boundary emits the init frame immediately before itself.
    // A failed attempt has no boundary but still completes a local SDK command.
    if result.is_err() {
        stream.emit_init().await;
    }
    let failure = result.err().map(|error| {
        let message = error.to_string();
        let mut display = command_api::builtins::compact::compact_failure_display(&message);
        if display == "No messages to compact" {
            display.insert_str(0, "Error: ");
        }
        (
            display,
            command_api::builtins::compact::compact_failure_is_error(&message),
        )
    });
    stream
        .emit_compact_command_output(
            instructions,
            &uuid,
            &timestamp,
            failure
                .as_ref()
                .map(|(display, is_error)| (display.as_str(), *is_error)),
            argv.verbose,
            argv.replay_user_messages,
        )
        .await;
    let cost = runtime.orchestrator.snapshot_cost().await;
    if runtime.policy_helper_refusal.current().is_some() {
        return;
    }
    stream
        .emit_compact_command_result(
            &cost,
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            failure.as_ref().map(|(display, _)| display.as_str()),
        )
        .await;
}

/// Drive a one-shot stream-json conversation or local /compact command.
/// The stream is already installed as the orchestrator's output sink.
pub async fn run_stream_json_print(
    argv: &Argv,
    runtime: &Runtime,
    stream: Arc<StreamJsonStream>,
    permission_mode: permission::PermissionMode,
) -> i32 {
    run_print_owned(runtime, async {
        let code =
            run_stream_json_print_inner(argv, runtime, stream.clone(), permission_mode).await;
        if runtime.policy_helper_refusal.current().is_some() {
            return exit_codes::RUNTIME_ERROR;
        }
        // Publish the native JSON result before entering the shared
        // shutdown barrier. Stream-json has already delivered each frame.
        let cost = runtime.orchestrator.snapshot_cost().await;
        stream.refresh_held_result_totals(
            &cost,
            &argv.betas.clone().unwrap_or_default(),
            runtime.agent_session_statistics().await.map(|stats| {
                serde_json::to_value(stats).expect("agent statistics are JSON scalars")
            }),
        );
        if runtime.policy_helper_refusal.current().is_some() {
            return exit_codes::RUNTIME_ERROR;
        }
        if stream.publish_json().await.is_err() {
            exit_codes::RUNTIME_ERROR
        } else {
            code
        }
    })
    .await
}

async fn run_stream_json_print_inner(
    argv: &Argv,
    runtime: &Runtime,
    stream: Arc<StreamJsonStream>,
    permission_mode: permission::PermissionMode,
) -> i32 {
    let prompt = argv.prompt.clone().unwrap_or_default();
    if prompt.trim().is_empty() {
        // Byte-parity with claude-code print.ts (see run_oneshot).
        runtime.record_execution_error("Error: Input must be provided either through stdin or as a prompt argument when using --print");
        let _ = runtime.output.write_line("Error: Input must be provided either through stdin or as a prompt argument when using --print").await;
        return exit_codes::ARGV_ERROR;
    }
    if let Err(error) = runtime.flush_prepared_fork_history().await {
        runtime.record_execution_error(&error);
        let _ = runtime.output.write_line(&error).await;
        return exit_codes::RUNTIME_ERROR;
    }
    stream.begin_query_timing();
    let _ = stream.begin_request_markers(None, Vec::new(), false);

    // /compact is a local SDK command with compaction status and replay frames.
    // Other local-command transports retain their existing dispatch policy.
    if prompt.starts_with('/') && compact_command_instructions(&prompt).is_none() {
        let _ = runtime
            .output
            .write_line("lingxi-cli: slash commands not supported in stream-json mode")
            .await;
        return exit_codes::ARGV_ERROR;
    }

    // Collect the real session_id and model from the orchestrator after build.
    let (session_id_str, model_str, model_profile) = {
        let session_handle = runtime.orchestrator.session();
        let session = session_handle.lock().await;
        (
            session.session_id.as_uuid().to_string(),
            session.model.clone(),
            session.model_profile.clone(),
        )
    };

    // Advertise the current registry tool names.
    let tool_names = runtime.orchestrator.advertised_tool_names().await;

    // ── P2b: real init-frame population ─────────────────────────────────────

    // MCP servers: name + status string from the live orchestrator registry.
    let mcp_servers: Vec<(String, String)> = runtime
        .orchestrator
        .list_mcp_servers()
        .await
        .into_iter()
        .map(|s| {
            let status_str = match s.status {
                McpStatus::Connected => "connected".to_string(),
                McpStatus::Disconnected => "disconnected".to_string(),
                McpStatus::Error(_) => "error".to_string(),
            };
            (s.name, status_str)
        })
        .collect();

    let (slash_commands, terminal_slash_commands, skills) =
        match init_command_projections(runtime).await {
            Ok(projections) => projections,
            Err(error) => {
                let message = format!("Error: Failed to load command visibility settings: {error}");
                runtime.record_execution_error(&message);
                let _ = runtime.output.write_line(&message).await;
                return exit_codes::RUNTIME_ERROR;
            }
        };

    // Agents: names from the agent catalog via the orchestrator handle.
    let agents: Vec<String> = runtime
        .orchestrator
        .list_agents()
        .await
        .into_iter()
        .map(|a| a.name)
        .collect();
    // list_agents already sorts; no re-sort needed.

    let plugins = init_plugins(runtime).await;

    // 2.1.219 `JW()`: why fast mode is unavailable here. The `-p` surface is
    // the Agent SDK, so without the `--settings` `fastMode:true` opt-in this
    // resolves `sdk_opt_in_required` (live 2.1.220 init/result capture).
    let sdk_fast_mode_opt_in = flag_settings_fast_mode_opt_in(argv.settings.as_deref());
    let fast_mode_disabled_reason = resolve_fast_mode_disabled_reason(
        runtime
            .orchestrator
            .model_is_first_party_route(&model_str, model_profile.as_deref()),
        sdk_fast_mode_opt_in,
        runtime.services.as_ref(),
    );
    let credential_source = runtime
        .orchestrator
        .model_credential_source(&model_str, model_profile.as_deref())
        .await
        .unwrap_or(llm_runtime::CredentialSource::Unknown);
    // `cK(mt,ce.fastMode)` — the state rides the SAME inputs as the reason, so
    // an opted-in first-party fast-mode model reports `on` instead of the
    // self-contradicting `off`-with-no-reason pair.
    let fast_mode_state =
        resolve_fast_mode_state(&model_str, fast_mode_disabled_reason, sdk_fast_mode_opt_in);

    // Build the init parameters now that the runtime is available.
    let mut init_params = build_init_params(
        &session_id_str,
        tool_names,
        mcp_servers,
        &model_str,
        &credential_source,
        permission_mode_str(permission_mode),
        slash_commands,
        agents,
        skills,
        plugins,
        "default", // output_style
        None,      // memory_auto_path
        fast_mode_state,
        fast_mode_disabled_reason,
        init_host_metadata(runtime),
    );
    init_params.terminal_slash_commands = terminal_slash_commands;
    init_params.output_style = init_output_style(&runtime.orchestrator).await;
    init_params.model_source = runtime
        .orchestrator
        .get_status_snapshot()
        .await
        .model_source;

    // Thread the real params + session_id into the stream.
    stream.set_init_params(init_params).await;

    // Phase 0 (0a): start the single-writer stdout drain task before any frames
    // are emitted. All subsequent emit_* calls push onto the mpsc channel;
    // the drain task is the sole stdout writer.
    stream.ensure_drain_started().await;

    if let Some(instructions) = compact_command_instructions(&prompt) {
        run_stream_compact_command(
            argv,
            runtime,
            &stream,
            instructions,
            None,
            runtime.turn_cancel(),
        )
        .await;
        runtime.activate_deferred_startup().await;
        let _ = stream.flush().await;
        return exit_codes::SUCCESS;
    }

    // ① system/init frame
    stream.emit_init().await;
    runtime.activate_deferred_startup().await;

    // Native 2.1.293 does not publish a requesting status before model input.

    // ③ Run the turn — streaming callbacks (emit_text / emit_tool_call /
    //    emit_message_start / emit_message_boundary) fire on the stream.
    let turn_result =
        if let (Some(slot), Some(_schema)) = (&runtime.structured_output_slot, &argv.json_schema) {
            match execute_structured_output(
                runtime,
                &prompt,
                argv.prompt_images.clone(),
                None,
                None,
                slot,
                argv.max_budget_usd,
                runtime.turn_cancel(),
                None,
            )
            .await
            {
                Ok(value) => {
                    stream.set_structured_output(value).await;
                    Ok(orchestrator::TurnOutcome::EndTurn)
                }
                Err(error) => Err(error),
            }
        } else if argv.no_stream {
            let prompt_source = Utf16JsonProjection::from(prompt.as_str());
            if argv.prompt_images.is_empty() {
                orchestrator::native_user_message_source::with_sdk_source(
                    orchestrator::mod_prompt_origin::with_origin(
                        json!({"kind":"sdk"}),
                        runtime
                            .orchestrator
                            .run_turn_with_cancel(prompt_source, runtime.turn_cancel()),
                    ),
                )
                .await
            } else {
                orchestrator::native_user_message_source::with_sdk_source(
                    orchestrator::mod_prompt_origin::with_origin(
                        json!({"kind":"sdk"}),
                        runtime.orchestrator.run_turn_with_cancel_image_sources(
                            prompt_source,
                            argv.prompt_images.clone(),
                            runtime.turn_cancel(),
                        ),
                    ),
                )
                .await
            }
        } else {
            orchestrator::native_user_message_source::with_sdk_source(
                orchestrator::mod_prompt_origin::with_origin(
                    json!({"kind":"sdk"}),
                    runtime
                        .orchestrator
                        .run_turn_streaming_with_cancel_image_sources(
                            &prompt,
                            argv.prompt_images.clone(),
                            runtime.turn_cancel(),
                        ),
                ),
            )
            .await
        };
    let betas = argv.betas.clone().unwrap_or_default();
    let prompt_suggestions = runtime.prompt_suggestions.clone();
    let mut publisher = NativeQueryResultPublisher {
        runtime,
        stream: &stream,
        prompt_suggestions: prompt_suggestions.clone(),
        suggestion_turn_origin: None,
        input_status: None,
        result_hold: Default::default(),
        max_budget_usd: argv.max_budget_usd,
        fast_mode_state,
        fast_mode_disabled_reason,
        betas: &betas,
        next_index: 0,
        last_error_published: false,
        last_result_failed: false,
    };
    // Native holds closed-input results while delegated agents/workflows or
    // their completion notifications remain; suggestions share this boundary.
    publisher.publish(&turn_result, 0).await;
    if publisher.last_result_failed {
        prompt_suggestions.discard();
        stop_print_tasks(runtime).await;
        publisher.release_held_results(true).await;
        prompt_suggestions.settle().await;
        return exit_codes::RUNTIME_ERROR;
    }
    let wind_down = wind_down_print_tasks(
        runtime,
        argv.max_budget_usd,
        runtime.shutdown.child_token(),
        None,
        Some(&mut publisher),
    )
    .await;
    publisher
        .release_held_results(wind_down.is_err() || runtime.shutdown.is_cancelled())
        .await;
    if runtime.policy_helper_refusal.current().is_none() {
        prompt_suggestions.settle().await;
        if runtime.policy_helper_refusal.current().is_none() {
            prompt_suggestions.emit_pending(&stream);
        }
    }
    match wind_down {
        Ok(()) if !publisher.last_result_failed => exit_codes::SUCCESS,
        Ok(()) => exit_codes::RUNTIME_ERROR,
        Err(_) => exit_codes::RUNTIME_ERROR,
    }
}

/// Drive a multi-turn `--input-format stream-json` conversation (P3).
///
/// Reads user turns from stdin (one JSON line per turn), deduplicates by uuid,
/// and feeds each turn sequentially through `run_turn`. Emits `system/init` +
/// `system/status` before the first turn and a `result` frame after the last.
///
/// Under `--replay-user-messages`, duplicate-uuid acks (`isReplay:true`) are
/// emitted when a dup is detected.
///
/// This function is called from `run_cli` when BOTH `--output-format stream-json`
/// AND `--input-format stream-json` are set. The stream is already installed as
/// the orchestrator's `OutputStream`.
pub async fn run_stream_json_input_loop(
    argv: &Argv,
    runtime: &Runtime,
    stream: Arc<StreamJsonStream>,
    permission_mode: permission::PermissionMode,
    control_plane: Arc<StdioControlPlane>,
    prepared_stdio: super::stdio::PreparedStdio,
) -> i32 {
    let auxiliary_tasks = Arc::new(PrintAuxTaskGroup::default());
    run_print_owned_with_cleanup(
        runtime,
        run_stream_json_input_loop_inner(
            argv,
            runtime,
            stream,
            permission_mode,
            control_plane.clone(),
            auxiliary_tasks.clone(),
            prepared_stdio,
        ),
        async {
            control_plane.shutdown("Session ended").await;
            auxiliary_tasks.abort_and_join().await;
        },
    )
    .await
}

async fn run_stream_json_input_loop_inner(
    argv: &Argv,
    runtime: &Runtime,
    stream: Arc<StreamJsonStream>,
    permission_mode: permission::PermissionMode,
    control_plane: Arc<StdioControlPlane>,
    auxiliary_tasks: Arc<PrintAuxTaskGroup>,
    prepared_stdio: super::stdio::PreparedStdio,
) -> i32 {
    control_plane.set_auxiliary_tasks(Arc::downgrade(&auxiliary_tasks));
    // Collect the real session_id and model from the orchestrator after build.
    let (session_id_str, model_str, model_profile) = {
        let session_handle = runtime.orchestrator.session();
        let session = session_handle.lock().await;
        (
            session.session_id.as_uuid().to_string(),
            session.model.clone(),
            session.model_profile.clone(),
        )
    };

    // Collect tool names, MCP servers, slash commands, agents, etc. — same as
    // run_stream_json_print's init-frame population.
    let tool_names = runtime.orchestrator.advertised_tool_names().await;

    let mcp_servers: Vec<(String, String)> = runtime
        .orchestrator
        .list_mcp_servers()
        .await
        .into_iter()
        .map(|s| {
            let status_str = match s.status {
                McpStatus::Connected => "connected".to_string(),
                McpStatus::Disconnected => "disconnected".to_string(),
                McpStatus::Error(_) => "error".to_string(),
            };
            (s.name, status_str)
        })
        .collect();

    let (slash_commands, terminal_slash_commands, skills) =
        match init_command_projections(runtime).await {
            Ok(projections) => projections,
            Err(error) => {
                let message = format!("Error: Failed to load command visibility settings: {error}");
                runtime.record_execution_error(&message);
                let _ = runtime.output.write_line(&message).await;
                return exit_codes::RUNTIME_ERROR;
            }
        };

    let agents: Vec<String> = runtime
        .orchestrator
        .list_agents()
        .await
        .into_iter()
        .map(|a| a.name)
        .collect();

    let plugins = init_plugins(runtime).await;

    // 2.1.219 `JW()`: fast-mode unavailability reason for this SDK surface —
    // threaded into system/init, the `initialize` control_response, and every
    // result frame (all live-verified 2.1.220 emission sites).
    let sdk_fast_mode_opt_in = flag_settings_fast_mode_opt_in(argv.settings.as_deref());
    let fast_mode_disabled_reason = resolve_fast_mode_disabled_reason(
        runtime
            .orchestrator
            .model_is_first_party_route(&model_str, model_profile.as_deref()),
        sdk_fast_mode_opt_in,
        runtime.services.as_ref(),
    );
    let credential_source = runtime
        .orchestrator
        .model_credential_source(&model_str, model_profile.as_deref())
        .await
        .unwrap_or(llm_runtime::CredentialSource::Unknown);
    // `cK(mt,ce.fastMode)` — same inputs as the reason (see
    // `resolve_fast_mode_state`), so the two never contradict each other.
    let fast_mode_state =
        resolve_fast_mode_state(&model_str, fast_mode_disabled_reason, sdk_fast_mode_opt_in);

    let mut init_params = build_init_params(
        &session_id_str,
        tool_names,
        mcp_servers,
        &model_str,
        &credential_source,
        permission_mode_str(permission_mode),
        slash_commands,
        agents,
        skills,
        plugins,
        "default",
        None,
        fast_mode_state,
        fast_mode_disabled_reason,
        init_host_metadata(runtime),
    );
    init_params.terminal_slash_commands = terminal_slash_commands;
    init_params.output_style = init_output_style(&runtime.orchestrator).await;
    init_params.model_source = runtime
        .orchestrator
        .get_status_snapshot()
        .await
        .model_source;

    stream.set_init_params(init_params).await;

    // Phase 0 (0a): start the single-writer stdout drain task before any frames
    // are emitted. All subsequent emit_* calls push onto the mpsc channel;
    // the drain task is the sole stdout writer.
    stream.ensure_drain_started().await;

    // Defer the initial conversation frames until the first model turn.
    // A leading /compact has its own status → init lifecycle and must not
    // acquire an unrelated "requesting" status before it starts.
    let mut emitted_initial_conversation_frames = false;

    // Phase 0 (0b): spawn the streaming stdin router. Frames arrive AS THEY
    // ARE SENT (not buffered to EOF), routed by type onto three channels:
    // - input_rx    → ordered user/history/bash frames consumed below.
    // - control_req_rx → control_request frames: dispatched to control-plane and cancel.
    // - control_resp_rx → control_response frames: resolved by `resolver_task`.
    //
    // The owned async reader remains cancellable and is joined on cleanup.
    // Data-plane intake is shared with the driver's post-tool fold; controls
    // retain their independent resolver/dispatcher lanes.
    let super::stdio::PreparedStdio {
        input_rx,
        input_pending,
        mut control_req_rx,
        reader_status,
        reader,
        queue_lifecycle,
        mut orphan_rx,
        auxiliary_tasks: prepared_tasks,
        stop: transport_stop,
        plane: _,
    } = prepared_stdio;
    for task in prepared_tasks {
        auxiliary_tasks.push(task);
    }
    let journal_ready =
        match queue_lifecycle.bind_journal(runtime.orchestrator.session_transcript_writer()) {
            Ok(()) => queue_lifecycle.flush_journal().await,
            Err(error) => Err(error),
        };
    if let Err(error) = journal_ready {
        reader.stop();
        let _ = reader.join().await;
        transport_stop.cancel();
        control_plane.shutdown("Session ended").await;
        auxiliary_tasks.abort_and_join().await;
        let _ = runtime.output.write_line(&error).await;
        return exit_codes::RUNTIME_ERROR;
    }
    let input_rx = Arc::new(super::queued_commands::SdkInputQueue::new(
        input_rx,
        input_pending,
        queue_lifecycle.clone(),
        stream.clone(),
        runtime.orchestrator.clone(),
        argv.replay_user_messages,
        session_id_str.clone(),
    ));
    runtime.orchestrator.set_mid_turn_input(input_rx.clone());

    // ③ Phase 1: pre-collect initialization data for the `initialize` handler.
    // These require async access to runtime — must be collected here before the
    // move into the spawned ctrl-dispatcher task.

    // Native Og invokes T5(getCommands()) for each initialize request.
    // Retain current registry/settings sources instead of a startup snapshot.
    let init_commands_registry = runtime.dispatcher.registry();
    let init_command_visibility = runtime.command_visibility_snapshot_loader();

    // Agents for the initialize response.
    let init_agents: Vec<serde_json::Value> = runtime
        .orchestrator
        .list_agents()
        .await
        .into_iter()
        .map(|a| json!({"name": a.name, "description": a.description}))
        .collect();

    // Models for the initialize response. Use the live model listings from the
    // orchestrator; map known request_model strings to capability flags.
    let init_models: Vec<serde_json::Value> = {
        let listings = runtime.orchestrator.list_model_listings().await;
        listings
            .into_iter()
            .map(|m| {
                // Capability mapping for known Anthropic models.
                let (
                    supports_effort,
                    supported_effort_levels,
                    supports_adaptive_thinking,
                    supports_fast_mode,
                    supports_auto_mode,
                ) = model_capabilities(&m.request_model);
                let mut obj = json!({
                    "value": m.request_model,
                    "displayName": m.display_model,
                    "description": m.provider_label,
                    "supportsEffort": supports_effort,
                    "supportsAdaptiveThinking": supports_adaptive_thinking,
                    "supportsFastMode": supports_fast_mode,
                    "supportsAutoMode": supports_auto_mode,
                });
                if !supported_effort_levels.is_empty() {
                    obj["supportedEffortLevels"] = serde_json::Value::Array(
                        supported_effort_levels
                            .into_iter()
                            .map(|s| serde_json::Value::String(s.to_string()))
                            .collect(),
                    );
                }
                obj
            })
            .collect()
    };

    // Account: emit what we can; full auth integration is deferred (Phase 3+).
    let init_account = runtime.services.account_metadata();

    // ③ Phase 1: cancel watch channel for interrupt support.
    // The cancel_tx is shared with the ctrl-dispatcher task; each turn
    // subscribes so it can abort an in-flight turn on `interrupt`.
    // `_cancel_anchor_rx` is never read — it exists because
    // `watch::Sender::send` is a no-op at zero receivers, and BETWEEN turns
    // (no subscriber) an `interrupt` would otherwise be silently dropped.
    let (cancel_tx, _cancel_anchor_rx) = tokio::sync::watch::channel(false);
    let cancel_tx_clone = cancel_tx.clone();

    // ③ Drain control-request/control-cancel and control-response channels
    //    concurrently with the turn loop.
    //
    // The dispatcher handles both server-initiated control requests and
    // inbound `control_cancel_request` frames for host-origin UI renders.
    let outbound_tx = stream.outbound_tx();
    let ctrl_plane = ControlPlaneWriter::new(outbound_tx.clone());
    let ctrl_orch = runtime.orchestrator.clone();
    let ctrl_tasks = runtime.task_registry.clone();
    // `set_cwd` moves the live session; it needs the cwd cell to swap and the
    // control plane to know whether a turn is in flight.
    let ctrl_session_cwd = runtime.session_cwd.clone();
    let ctrl_plane_busy = control_plane.clone();
    // `end_session` signals the turn loop to drain + exit (the loop selects on it).
    let end_notify = Arc::new(tokio::sync::Notify::new());
    let end_notify_ctrl = end_notify.clone();
    let resolver_plane_for_cancel = control_plane.clone();
    let ctrl_lifecycle = queue_lifecycle.clone();
    let ctrl_file_suggestions = StreamFileSuggestionIndex::default();
    let ctrl_services = runtime.services.clone();
    let ctrl_execution_errors = runtime.execution_errors.clone();
    let ctrl_auxiliary_tasks = auxiliary_tasks.clone();
    let prompt_suggestions = runtime.prompt_suggestions.clone();
    let ctrl_prompt_suggestions = prompt_suggestions.clone();
    auxiliary_tasks.push(tokio::spawn(async move {
        while let Some(frame) = control_req_rx.recv().await {
            match frame {
                StdinControlFrame::UserInputStarted => {
                    ctrl_prompt_suggestions.input_started();
                    continue;
                }
                StdinControlFrame::UpdateEnvironmentVariables(values) => {
                    if let Err(error) = ctrl_services.update_environment_variables(values) {
                        ctrl_execution_errors
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(error.clone());
                        ctrl_plane_busy.shutdown(&error).await;
                        end_notify_ctrl.notify_one();
                        break;
                    }
                    continue;
                }
                StdinControlFrame::Cancel(request_id) => {
                    resolver_plane_for_cancel.cancel_inbound_request(&request_id);
                    continue;
                }
                StdinControlFrame::Request(frame) => {
                    let request_id = frame.string_at("/request_id").unwrap_or("").to_owned();
                    let subtype = frame.string_at("/request/subtype").unwrap_or("").to_owned();
                    let init_commands = if subtype == "initialize" {
                        match init_command_visibility() {
                            Ok(visibility) => {
                                let registry = init_commands_registry.read().await;
                                initialize_commands::collect(&registry, &visibility)
                            }
                            Err(error) => {
                                let request_source = frame
                                    .subprojection("/request_id")
                                    .unwrap_or_else(|_| request_id.clone().into());
                                if let Err(delivery_error) = ctrl_plane.reply_error_projected(
                                    request_source,
                                    format!("Failed to load command visibility settings: {error}")
                                        .into(),
                                ) {
                                    ctrl_execution_errors
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .push(delivery_error.to_string());
                                    ctrl_plane_busy.shutdown(&delivery_error.to_string()).await;
                                    end_notify_ctrl.notify_one();
                                    break;
                                }
                                continue;
                            }
                        }
                    } else {
                        Vec::new()
                    };
                    if let Err(error) = dispatch_control_request(
                        &subtype,
                        &request_id,
                        &frame,
                        &ctrl_plane,
                        &cancel_tx_clone,
                        &ctrl_lifecycle,
                        &ctrl_orch,
                        &ctrl_tasks,
                        &ctrl_session_cwd,
                        &ctrl_plane_busy,
                        &end_notify_ctrl,
                        &init_commands,
                        &init_agents,
                        &init_models,
                        &init_account,
                        fast_mode_state,
                        fast_mode_disabled_reason,
                        &ctrl_file_suggestions,
                        &ctrl_prompt_suggestions,
                        ctrl_services.as_ref(),
                        &ctrl_auxiliary_tasks,
                        &ctrl_execution_errors,
                    )
                    .await
                    {
                        ctrl_execution_errors
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(error.to_string());
                        ctrl_plane_busy.shutdown(&error.to_string()).await;
                        end_notify_ctrl.notify_one();
                        break;
                    }
                    if subtype == "end_session" {
                        break;
                    }
                }
            }
        }
    }));

    // ④ Consume user turns sequentially through the orchestrator.
    let betas = argv.betas.clone().unwrap_or_default();
    let mut last_turn_err: Option<orchestrator::OrchestratorError> = None;
    let mut had_any_turn = false;
    let mut publisher = NativeQueryResultPublisher {
        runtime,
        stream: &stream,
        prompt_suggestions: prompt_suggestions.clone(),
        suggestion_turn_origin: None,
        input_status: Some(reader_status.clone()),
        result_hold: Default::default(),
        max_budget_usd: argv.max_budget_usd,
        fast_mode_state,
        fast_mode_disabled_reason,
        betas: &betas,
        next_index: 0,
        last_error_published: false,
        last_result_failed: false,
    };
    // Per-toolUseID orphaned-permission dedup (twin of claude-code's
    // `handledOrphanedToolUseIds` Set, print.ts:2766/5272/5287): each DISTINCT
    // unresolved tool_use recovers once; a same-id re-delivery is skipped. NOT a
    // session-wide single-shot — a `--resume` that lost several `can_use_tool`
    // requests recovers each of them, matching claude-code (whose
    // `hasHandledOrphanedPermission` boolean is a per-command QueryEngine field,
    // not a cross-command cap).
    let mut handled_orphans: std::collections::HashSet<lingxi_core::types::ToolUseId> =
        std::collections::HashSet::new();
    // Disable the orphan `select!` branch once its channel closes (all senders
    // dropped) so a perpetually-ready `recv() → None` can't busy-spin the loop.
    let mut orphan_closed = false;

    let mut explicit_end_session = false;
    loop {
        if let StdinReaderStatus::Failed(error) = reader_status.borrow().clone() {
            last_turn_err = Some(orchestrator::OrchestratorError::Internal(error.to_string()));
            break;
        }
        let turn = tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                if runtime.task_registry.has_pending_task_notifications_for(None).await {
                    let _operation = control_plane.lock_operation().await;
                    let cancel = runtime.turn_cancel();
                    publisher.begin_notification_query().await;
                    control_plane.set_active_turn(cancel.clone()).await;
                    let result = runtime.orchestrator.run_task_notification_rewake(runtime.task_registry.as_ref(), cancel).await;
                    control_plane.clear_active_turn().await;
                    publisher.publish(&result, input_rx.len().await).await;
                    if let Err(error) = result { last_turn_err = Some(error); break; }

                }
                continue;
            },
            // `end_session` (§2.2 #2): the host asked us to drain + exit.
            _ = end_notify.notified() => { explicit_end_session = true; break; },
            _ = runtime.shutdown.cancelled() => { explicit_end_session = true; break; },
            // ORPHANED PERMISSION recovery, drained BETWEEN turns (the `select!`
            // is not polled while `run_turn_streaming_with_cancel` runs, so an
            // orphan that arrives mid-turn is buffered and recovered after — never
            // concurrently with a turn, since both mutate `session.history`).
            recv = orphan_rx.recv(), if !orphan_closed => match recv {
                Some(cmd) => {
                    let _operation = control_plane.lock_operation().await;
                    let cancel = runtime.turn_cancel();
                    control_plane.set_active_turn(cancel.clone()).await;
                    recover_orphaned_permission(runtime, cmd, &mut handled_orphans, cancel).await;
                    // Cancel tools have unwound; Block tools and result writes
                    // have completed before this owner releases cwd/admission.
                    control_plane.clear_active_turn().await;
                    continue;
                }
                None => {
                    orphan_closed = true;
                    continue;
                }
            },
            recv = input_rx.recv() => match recv {
                Some(StreamInput::User(t)) => t,
                Some(StreamInput::History(history)) => {
                    runtime
                        .orchestrator
                        .append_external_history_message(history.message)
                        .await;
                    if argv.replay_user_messages && history.replay_frame.is_some() {
                        let _ = emit_projected_frame_queued(&outbound_tx, &history.frame_projection);
                    }
                    continue;
                }
                Some(StreamInput::Bash(command)) => {
                    let input = format!("<bash-input>{}</bash-input>", command.command);
                    let _operation = control_plane.lock_operation().await;
                    let cancel = runtime.turn_cancel();
                    control_plane.set_active_turn(cancel.clone()).await;
                    let output = runtime.bash_runner
                        .run_with_cancel(&command.command, cancel).await;
                    // Oracle frame:
                    // `<bash-stdout>..</bash-stdout><bash-stderr>..</bash-stderr>
                    //  <bash-exit-code>N</bash-exit-code>`
                    let result = format!(
                        "<bash-stdout>{}</bash-stdout><bash-stderr>{}</bash-stderr><bash-exit-code>{}</bash-exit-code>",
                        output.stdout, output.stderr, output.exit_code
                    );
                    for content in [input, result] {
                        runtime
                            .orchestrator
                            .append_external_history_message(lingxi_core::types::ConversationMessage::user(
                                lingxi_core::types::MessageId::new(),
                                content.clone(),
                            ))
                            .await;
                        // Match the other history replay paths: this echo is best-effort.
                        let _ = emit_replay_ack_queued(
                            &outbound_tx,
                            &uuid::Uuid::new_v4().to_string(),
                            &Value::String(content),
                            None,
                            &session_id_str,
                        );
                    }
                    // Retain busy and the cwd lease through child settlement
                    // AND its ordered transcript/output publication.
                    control_plane.clear_active_turn().await;
                    continue;
                }
                None => break, // stdin closed or fatal error — exit the loop.
            },
        };
        // interrupt_cancel_queued_v1: a uuid cancelled while queue-resident
        // must not run — its terminal `cancelled` lifecycle already went out
        // with the interrupt receipt. `on_dequeued` also retires the uuid from
        // the `still_queued` shadow registry.
        queue_lifecycle.record_dequeued();
        if let Err(error) = queue_lifecycle.flush_journal().await {
            last_turn_err = Some(orchestrator::OrchestratorError::Internal(error));
            break;
        }
        if let Some(uuid) = turn.uuid.as_deref() {
            if !queue_lifecycle.queued.on_dequeued(uuid) {
                continue;
            }
        }
        let prompt = content_to_prompt(&turn.content);
        let external_message_id = turn
            .uuid
            .as_deref()
            .and_then(lingxi_core::types::MessageId::parse_prefixed);

        if let Some(uuid) = turn.uuid.as_deref() {
            if runtime
                .orchestrator
                .session_contains_message_uuid(uuid)
                .await
            {
                if argv.replay_user_messages {
                    let _ = emit_replay_ack_projected_queued(
                        &outbound_tx,
                        &turn.frame_projection,
                        &session_id_str,
                    );
                }
                if let Err(error) = emit_dedup_skip_terminal(&queue_lifecycle, uuid) {
                    last_turn_err =
                        Some(orchestrator::OrchestratorError::Internal(error.to_string()));
                    break;
                }
                let _ = queue_lifecycle.queued.take_current_turn_uuids();
                continue;
            }
        }

        if let Err(error) = runtime.flush_prepared_fork_history().await {
            last_turn_err = Some(orchestrator::OrchestratorError::Internal(error));
            break;
        }
        publisher.begin_query();
        let primary = turn
            .frame_projection
            .subprojection("/uuid")
            .ok()
            .filter(|value| value.string_at("").is_some_and(|uuid| !uuid.is_empty()));
        let consumed = primary.iter().cloned().collect();
        if let Err(error) = stream.begin_request_markers(primary, consumed, false) {
            last_turn_err = Some(orchestrator::OrchestratorError::Internal(error));
            break;
        }

        // UUID replays are discarded by Claude Code before they reach the
        // query loop. Only a genuinely new query aborts the prior suggestion
        // and makes this a non-empty session.
        had_any_turn = true;
        prompt_suggestions
            .on_command(
                runtime,
                compact_command_instructions(&prompt).is_none(),
                Some(&turn.content),
            )
            .await;
        publisher.suggestion_turn_origin = None;

        if !emitted_initial_conversation_frames && compact_command_instructions(&prompt).is_none() {
            stream.emit_init().await;
            runtime.activate_deferred_startup().await;
            emitted_initial_conversation_frames = true;
        }

        // Under --replay-user-messages, re-emit the inbound user frame as
        // isReplay:true (the initial-prompt ack for each new turn). Echo the
        // ORIGINAL uuid + content so the host can correlate the ack.
        if argv.replay_user_messages && compact_command_instructions(&prompt).is_none() {
            let _ = emit_replay_ack_projected_queued(
                &outbound_tx,
                &turn.frame_projection,
                &session_id_str,
            );
        }

        // msg_lifecycle_v1: the dequeued command's turn is now dispatching.
        if let Some(uuid) = turn.uuid.as_deref() {
            if let Err(error) =
                queue_lifecycle.emit(uuid, crate::headless::queued_commands::LIFECYCLE_STARTED)
            {
                last_turn_err = Some(orchestrator::OrchestratorError::Internal(error.to_string()));
                break;
            }
        }

        // Phase 1: use cancel-aware turn entry point so `interrupt` can abort
        // the in-flight SSE stream. A watcher task bridges the watch channel
        // to the CancellationToken that `run_turn_streaming_with_cancel` consumes.
        //
        // `subscribe()` — NOT `cancel_rx.clone()`: a watch Receiver clone
        // inherits the version its source last SAW, and `cancel_rx` is never
        // awaited, so from turn 2 on (after the end-of-turn `send(false)`
        // below) a clone would satisfy `changed()` immediately with `false`,
        // the bridge would exit, and no later `interrupt` could reach the
        // token. Loop rather than await a single change so a `false` reset
        // racing the turn start does not retire the bridge either.
        let _operation = control_plane.lock_operation().await;
        let cancel = runtime.turn_cancel();
        let cancel_clone = cancel.clone();
        let mut cancel_rx2 = cancel_tx.subscribe();
        let cancel_bridge = tokio::spawn(async move {
            loop {
                if *cancel_rx2.borrow_and_update() {
                    cancel_clone.cancel();
                    return;
                }
                if cancel_rx2.changed().await.is_err() {
                    return;
                }
            }
        });

        // P5 Phase 2: register this turn's token so a `can_use_tool`
        // `deny+interrupt` response (§3.4) can abort the whole turn.
        control_plane.set_active_turn(cancel.clone()).await;

        if let Some(instructions) = compact_command_instructions(&prompt) {
            // This local command publishes a zeroed result, not an admitted
            // model query carrying the inbound SDK request marker.
            let _ = stream.begin_request_markers(None, Vec::new(), false);
            set_result_position(runtime, &stream, input_rx.len().await, publisher.next_index).await;
            run_stream_compact_command(
                argv,
                runtime,
                &stream,
                instructions,
                turn.uuid.as_deref(),
                cancel.clone(),
            )
            .await;
            runtime.activate_deferred_startup().await;
            publisher.next_index = publisher.next_index.saturating_add(1);
            control_plane.clear_active_turn().await;
            cancel_bridge.abort();
            let _ = queue_lifecycle.queued.take_current_turn_uuids();
            if let Some(uuid) = turn.uuid.as_deref() {
                if let Err(error) = queue_lifecycle
                    .emit(uuid, crate::headless::queued_commands::LIFECYCLE_COMPLETED)
                {
                    let _ = cancel_tx.send(false);
                    last_turn_err =
                        Some(orchestrator::OrchestratorError::Internal(error.to_string()));
                    break;
                }
            }
            let _ = cancel_tx.send(false);
            last_turn_err = None;
            continue;
        }

        // Probe handle: after the turn, `is_cancelled()` distinguishes an
        // interrupt-aborted turn from a completed one (binary `mCo(reason)`).
        let cancel_probe = cancel.clone();
        let turn_result = if let (Some(slot), Some(_schema)) =
            (&runtime.structured_output_slot, &argv.json_schema)
        {
            match execute_structured_output(
                runtime,
                &prompt,
                Vec::new(),
                Some(&turn.content_projection),
                external_message_id,
                slot,
                argv.max_budget_usd,
                cancel,
                None,
            )
            .await
            {
                Ok(value) => {
                    stream.set_structured_output(value).await;
                    Ok(orchestrator::TurnOutcome::EndTurn)
                }
                Err(error) => Err(error),
            }
        } else {
            orchestrator::native_user_message_source::with_sdk_source(
                orchestrator::mod_prompt_origin::with_origin(
                    json!({"kind":"sdk"}),
                    runtime
                        .orchestrator
                        .run_turn_streaming_with_cancel_projected_content(
                            &turn.content_projection,
                            cancel,
                            external_message_id,
                        ),
                ),
            )
            .await
        };
        // The control plane's token is also its busy flag. Release it only
        // after the orchestrator future has naturally completed so Block tools
        // remain protected, but always release it before accepting between-turn
        // control operations such as set_cwd.
        control_plane.clear_active_turn().await;
        // The bridge outlives the turn otherwise (it parks on `changed()`),
        // and the next turn subscribes its own.
        cancel_bridge.abort();
        let consumed_uuids = queue_lifecycle.queued.take_current_turn_uuids();
        let mut terminal_delivery_error = None;
        for uuid in consumed_uuids
            .iter()
            .filter(|uuid| turn.uuid.as_deref() != Some(uuid.as_str()))
        {
            if let Err(error) = emit_turn_terminal_lifecycle(
                &queue_lifecycle,
                Some(uuid),
                &turn_result,
                cancel_probe.is_cancelled(),
            ) {
                terminal_delivery_error = Some(error);
                break;
            }
        }
        if let Some(error) = terminal_delivery_error {
            let _ = cancel_tx.send(false);
            last_turn_err = Some(orchestrator::OrchestratorError::Internal(error.to_string()));
            break;
        }
        publisher.publish(&turn_result, input_rx.len().await).await;
        if let Err(error) = emit_turn_terminal_lifecycle(
            &queue_lifecycle,
            turn.uuid.as_deref(),
            &turn_result,
            cancel_probe.is_cancelled(),
        ) {
            let _ = cancel_tx.send(false);
            last_turn_err = Some(orchestrator::OrchestratorError::Internal(error.to_string()));
            break;
        }
        match turn_result {
            Ok(_) => {
                // Reset the cancel signal for the next turn.
                let _ = cancel_tx.send(false);
                last_turn_err = None;
            }
            Err(e) => {
                // Reset cancel state regardless.
                let _ = cancel_tx.send(false);
                last_turn_err = Some(e);
                break;
            }
        }
    }

    if let StdinReaderStatus::Failed(error) = reader_status.borrow().clone() {
        last_turn_err = Some(orchestrator::OrchestratorError::Internal(error.to_string()));
    }
    if explicit_end_session || last_turn_err.is_some() {
        prompt_suggestions.discard();
        stop_print_tasks(runtime).await;
    } else {
        let shutdown = runtime.shutdown.child_token();
        let winding_down = wind_down_print_tasks(
            runtime,
            argv.max_budget_usd,
            shutdown.clone(),
            Some(&control_plane),
            Some(&mut publisher),
        );
        tokio::pin!(winding_down);
        let result = tokio::select! {
            result = &mut winding_down => result,
            _ = end_notify.notified() => {
                shutdown.cancel();
                control_plane.cancel_active_turn().await;
                // Keep polling so Block tools unwind under their normal policy.
                winding_down.await
            }
        };
        if let Err(error) = result {
            last_turn_err = Some(error);
        }
    }

    publisher
        .release_held_results(
            explicit_end_session || last_turn_err.is_some() || runtime.shutdown.is_cancelled(),
        )
        .await;
    // Native HS.settle runs before auxiliary/control-owner teardown.
    if runtime.policy_helper_refusal.current().is_none() {
        prompt_suggestions.settle().await;
        if runtime.policy_helper_refusal.current().is_none() {
            prompt_suggestions.emit_pending(&stream);
        }
    }

    input_rx.close().await;
    reader.stop();
    let _ = reader.join().await;
    if let Err(error) = queue_lifecycle.flush_journal().await {
        last_turn_err = Some(orchestrator::OrchestratorError::Internal(error));
    }
    transport_stop.cancel();
    control_plane.shutdown("Session ended").await;
    if explicit_end_session || last_turn_err.is_some() {
        auxiliary_tasks.abort_and_join().await;
    } else {
        auxiliary_tasks.join().await;
    }

    // Stream teardown (binary `Hkm`): every uuid still queue-resident gets a
    // terminal `discarded` lifecycle — covers both stdin EOF and `end_session`.
    for uuid in queue_lifecycle.queued.drain_for_discard() {
        if let Err(error) =
            queue_lifecycle.emit(&uuid, crate::headless::queued_commands::LIFECYCLE_DISCARDED)
        {
            last_turn_err.get_or_insert_with(|| {
                orchestrator::OrchestratorError::Internal(error.to_string())
            });
        }
    }

    if runtime.policy_helper_refusal.current().is_none()
        && !had_any_turn
        && publisher.next_index == 0
        && last_turn_err.is_none()
    {
        // No user turns received — emit an empty-result envelope.
        let cost = runtime.orchestrator.snapshot_cost().await;
        if runtime.policy_helper_refusal.current().is_some() {
            return exit_codes::RUNTIME_ERROR;
        }
        stream
            .emit_result_success(
                &Utf16JsonProjection::from(""),
                "end_turn",
                &cost,
                fast_mode_state,
                fast_mode_disabled_reason,
                &betas,
            )
            .await;
        let _ = stream.flush().await;
        return exit_codes::SUCCESS;
    }

    if let Some(err) = last_turn_err {
        if !publisher.last_error_published {
            publisher.publish(&Err(err), input_rx.len().await).await;
        }
        exit_codes::RUNTIME_ERROR
    } else {
        // Every admitted query, including EOF notification wakes, was already
        // published. Closing input does not create a duplicate terminal frame.
        let _ = stream.flush().await;
        if publisher.last_result_failed {
            exit_codes::RUNTIME_ERROR
        } else {
            exit_codes::SUCCESS
        }
    }
}

/// The oracle terminal reason (`In` at the stream-json call site @240906553)
/// for a port turn outcome, so the `command_lifecycle` terminal can run
/// through the real `Njo`/`mCo` split instead of a bare cancel probe.
/// `Cancelled` is the interrupt arm `Wpt` reports as `aborted_streaming`;
/// `MaxTurns` and `EndTurn` are two of `Bxs`'s `completed` arms.
fn turn_outcome_terminal_reason(outcome: &orchestrator::TurnOutcome) -> &'static str {
    match outcome {
        orchestrator::TurnOutcome::Cancelled => "aborted_streaming",
        orchestrator::TurnOutcome::MaxTurns => "max_turns",
        orchestrator::TurnOutcome::EndTurn => "completed",
    }
}

/// msg_lifecycle_v1 terminal for a command the resume dedup skipped — the
/// binary's stdin dedup arm @246492139, right after the replay ack:
///
/// ```js
/// if(bi&&!Ma&&!Br) e.onCommandLifecycle?.(dt.uuid,"completed"),Qwt.delete(dt.uuid)
/// ```
///
/// `bi` is `mUo` (the message is already in the session file). Without this the
/// uuid would get `queued` and nothing else: `on_dequeued` already retired it
/// from the shadow registry, so teardown's `discarded` sweep can no longer see
/// it and a host awaiting the terminal hangs. The `!Br` guard (`hUo` = turn
/// UNANSWERED ⇒ re-execute) has no port surface — this branch skips the turn
/// unconditionally, so the command is terminal either way.
pub(super) fn emit_dedup_skip_terminal(
    lifecycle: &crate::headless::queued_commands::QueueLifecycle,
    uuid: &str,
) -> Result<(), crate::headless::stream_json_input::InputError> {
    lifecycle.emit(uuid, crate::headless::queued_commands::LIFECYCLE_COMPLETED)
}

/// msg_lifecycle_v1 terminal for a finished turn's own uuid — the stream-json
/// call site @240906553:
///
/// ```js
/// if(gt.uuid!==void 0)_r=Fe(gt.uuid, rn!==null?"cancelled":Njo(In,Nn))
/// ```
///
/// `rn` is a THROWN turn error, `In` the turn's terminal reason, `Nn` the
/// abort-signal state. A turn that threw is `cancelled` outright; otherwise
/// the reason runs through `Njo`/`mCo`. `Err(_)` is the thrown arm here —
/// `MaxTurnsReached` never reaches it (conversation.rs folds it into
/// `Ok(TurnOutcome::MaxTurns)`), which is why `max_turns` keeps `completed`.
fn emit_turn_terminal_lifecycle(
    lifecycle: &crate::headless::queued_commands::QueueLifecycle,
    uuid: Option<&str>,
    turn_result: &Result<orchestrator::TurnOutcome, orchestrator::OrchestratorError>,
    aborted: bool,
) -> Result<(), crate::headless::stream_json_input::InputError> {
    let Some(uuid) = uuid else { return Ok(()) };
    let state = match turn_result {
        Err(_) => crate::headless::queued_commands::LIFECYCLE_CANCELLED,
        Ok(outcome) => crate::headless::queued_commands::terminal_lifecycle_state(
            Some(turn_outcome_terminal_reason(outcome)),
            aborted,
        ),
    };
    lifecycle.emit(uuid, state)
}

/// Render the validated capture from the existing query driver in text mode.
async fn run_structured_output(
    runtime: &Runtime,
    prompt: &str,
    slot: &orchestrator::structured_output::StructuredOutputSlot,
    images: Vec<lingxi_core::types::ImageSource>,
    max_budget_usd: Option<f64>,
    sink: &dyn OutputSink,
) -> i32 {
    match execute_structured_output(
        runtime,
        prompt,
        images,
        None,
        None,
        slot,
        max_budget_usd,
        runtime.turn_cancel(),
        Some(sink),
    )
    .await
    {
        Ok(Some(value)) => match value.to_json_string() {
            Ok(json) => {
                let _ = runtime.stdout.write_line(&json).await;
                exit_codes::SUCCESS
            }
            Err(error) => {
                runtime.record_execution_error(&error);
                if runtime.policy_helper_refusal.current().is_none() {
                    sink.error("structured_output", &error.to_string()).await;
                }
                exit_codes::RUNTIME_ERROR
            }
        },
        Ok(None) => exit_codes::SUCCESS,
        Err(error) => {
            runtime.record_execution_error(&error);
            if runtime.policy_helper_refusal.current().is_none() {
                sink.error(
                    if stream_json_error_subtype(&error) == "error_max_structured_output_retries" {
                        "structured_output"
                    } else {
                        "runtime"
                    },
                    &error.to_string(),
                )
                .await;
            }
            exit_codes::RUNTIME_ERROR
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_structured_output(
    runtime: &Runtime,
    prompt: &str,
    images: Vec<lingxi_core::types::ImageSource>,
    projected_content: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
    message_id: Option<lingxi_core::types::MessageId>,
    slot: &orchestrator::structured_output::StructuredOutputSlot,
    max_budget_usd: Option<f64>,
    cancel: tokio_util::sync::CancellationToken,
    sink: Option<&dyn OutputSink>,
) -> Result<
    Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    orchestrator::OrchestratorError,
> {
    if let Ok(mut value) = slot.lock() {
        *value = None;
    }
    if let Some(sink) = sink {
        sink.turn_start().await;
    }
    // Schema validation and retry admission belong to the existing query
    // driver. Re-entering run_turn here would create another user message,
    // rerun UserPromptSubmit, and reset the query's limits and identity.
    let outcome = if let Some(projected) = projected_content {
        orchestrator::native_user_message_source::with_sdk_source(
            orchestrator::mod_prompt_origin::with_origin(
                json!({"kind":"sdk"}),
                runtime
                    .orchestrator
                    .run_turn_streaming_with_cancel_projected_content(
                        projected,
                        cancel.clone(),
                        message_id,
                    ),
            ),
        )
        .await
    } else {
        orchestrator::native_user_message_source::with_sdk_source(
            orchestrator::mod_prompt_origin::with_origin(
                json!({"kind":"sdk"}),
                runtime
                    .orchestrator
                    .run_turn_streaming_with_cancel_image_sources_and_message_id(
                        prompt,
                        images,
                        cancel.clone(),
                        message_id,
                    ),
            ),
        )
        .await
    };
    stop_background_agents_at_budget(
        max_budget_usd,
        runtime.orchestrator.as_ref(),
        runtime.task_registry.as_ref(),
        &runtime.output,
    )
    .await;
    let interrupted =
        matches!(outcome, Ok(orchestrator::TurnOutcome::Cancelled)) || cancel.is_cancelled();
    runtime
        .execution_interrupted
        .store(interrupted, std::sync::atomic::Ordering::Release);
    if interrupted {
        return Err(orchestrator::OrchestratorError::Internal(
            "Structured output execution interrupted".into(),
        ));
    }
    outcome?;
    let captured = slot.lock().ok().and_then(|mut value| value.take());
    Ok(captured)
}

#[inline(never)]
fn run_slash_dispatch_future<'a>(
    input: &'a Utf16JsonProjection,
    runtime: &'a Runtime,
) -> futures::future::BoxFuture<'a, SlashDispatchResult> {
    let context = command_api::ModCommandRunContext {
        origin: serde_json::json!({"kind":"sdk"}),
        is_fullscreen: false,
        columns: 80,
    };
    let cancellation = runtime.turn_cancel();
    Box::pin(command_api::with_mod_command_context(
        context,
        runtime
            .dispatcher
            .dispatch_headless(input.clone(), cancellation),
    ))
}

#[inline(never)]
fn run_fallback_prompt_turn_future<'a>(
    runtime: &'a Runtime,
    prompt: Utf16JsonProjection,
    request_context: Vec<Utf16JsonProjection>,
    transcript_attachment: Option<orchestrator::RequestContextTranscriptAttachment>,
    max_budget_usd: Option<f64>,
    no_stream: bool,
    sink: &'a dyn OutputSink,
) -> futures::future::BoxFuture<'a, i32> {
    Box::pin(run_fallback_prompt_turn(
        runtime,
        prompt,
        request_context,
        transcript_attachment,
        max_budget_usd,
        no_stream,
        sink,
    ))
}

#[inline(never)]
fn run_slash_as_turn_future<'a>(
    runtime: &'a Runtime,
    prompt: Utf16JsonProjection,
    max_budget_usd: Option<f64>,
    no_stream: bool,
    sink: &'a dyn OutputSink,
) -> futures::future::BoxFuture<'a, i32> {
    Box::pin(async move {
        sink.turn_start().await;
        let cancel = runtime.turn_cancel();
        let turn_result = if no_stream {
            Box::pin(orchestrator::native_user_message_source::with_sdk_source(
                orchestrator::mod_prompt_origin::with_origin(
                    serde_json::json!({"kind":"sdk"}),
                    runtime.orchestrator.run_turn_with_cancel(prompt, cancel),
                ),
            ))
            .await
        } else {
            Box::pin(orchestrator::native_user_message_source::with_sdk_source(
                orchestrator::mod_prompt_origin::with_origin(
                    serde_json::json!({"kind":"sdk"}),
                    runtime
                        .orchestrator
                        .run_turn_streaming_with_cancel_image_sources(&prompt, Vec::new(), cancel),
                ),
            ))
            .await
        };
        stop_background_agents_at_budget(
            max_budget_usd,
            runtime.orchestrator.as_ref(),
            runtime.task_registry.as_ref(),
            &runtime.output,
        )
        .await;
        runtime.execution_interrupted.store(
            matches!(&turn_result, Ok(orchestrator::TurnOutcome::Cancelled)),
            std::sync::atomic::Ordering::Release,
        );
        match turn_result {
            Ok(_outcome) => exit_codes::SUCCESS,
            Err(error) => {
                runtime.record_execution_error(&error);
                if runtime.policy_helper_refusal.current().is_none() {
                    sink.error("runtime", &error.to_string()).await;
                }
                exit_codes::RUNTIME_ERROR
            }
        }
    })
}

#[inline(never)]
fn run_slash_skill_as_turn_future<'a>(
    runtime: &'a Runtime,
    command: Utf16JsonProjection,
    body: Utf16JsonProjection,
    hook_messages: Vec<Utf16JsonProjection>,
    max_budget_usd: Option<f64>,
    no_stream: bool,
    sink: &'a dyn OutputSink,
) -> futures::future::BoxFuture<'a, i32> {
    Box::pin(async move {
        sink.turn_start().await;
        let cancel = runtime.turn_cancel();
        let inputs = orchestrator::skill_prompt_inputs(command, body, hook_messages);
        let turn_result = if no_stream {
            Box::pin(orchestrator::native_user_message_source::with_sdk_source(
                orchestrator::mod_prompt_origin::with_origin(
                    serde_json::json!({"kind":"sdk"}),
                    runtime
                        .orchestrator
                        .run_queued_prompt_batch_batched(inputs, cancel),
                ),
            ))
            .await
        } else {
            Box::pin(orchestrator::native_user_message_source::with_sdk_source(
                orchestrator::mod_prompt_origin::with_origin(
                    serde_json::json!({"kind":"sdk"}),
                    runtime.orchestrator.run_queued_prompt_batch(inputs, cancel),
                ),
            ))
            .await
        };
        stop_background_agents_at_budget(
            max_budget_usd,
            runtime.orchestrator.as_ref(),
            runtime.task_registry.as_ref(),
            &runtime.output,
        )
        .await;
        runtime.execution_interrupted.store(
            matches!(&turn_result, Ok(orchestrator::TurnOutcome::Cancelled)),
            std::sync::atomic::Ordering::Release,
        );
        match turn_result {
            Ok(_outcome) => exit_codes::SUCCESS,
            Err(error) => {
                runtime.record_execution_error(&error);
                if runtime.policy_helper_refusal.current().is_none() {
                    sink.error("runtime", &error.to_string()).await;
                }
                exit_codes::RUNTIME_ERROR
            }
        }
    })
}

#[inline(never)]
fn run_unknown_slash_fallback_future<'a>(
    runtime: &'a Runtime,
    input: Utf16JsonProjection,
    name: Utf16JsonProjection,
    suggestion: Option<Utf16JsonProjection>,
    max_budget_usd: Option<f64>,
    no_stream: bool,
    sink: &'a dyn OutputSink,
) -> futures::future::BoxFuture<'a, i32> {
    Box::pin(async move {
        let catalog = runtime.headless_command_catalog().await;
        let reminder =
            command_api::render_unknown_command_reminder(&name, suggestion.as_ref(), &catalog);
        let attachment = unknown_command_fallback_attachment(
            &name,
            suggestion.as_ref(),
            &catalog,
            reminder.clone(),
        );
        run_fallback_prompt_turn_future(
            runtime,
            input,
            vec![reminder],
            Some(attachment),
            max_budget_usd,
            no_stream,
            sink,
        )
        .await
    })
}

/// Dispatch a `/command [args]` line through the registry with the print-mode budget.
async fn run_slash_command_with_budget(
    input: &Utf16JsonProjection,
    runtime: &Runtime,
    max_budget_usd: Option<f64>,
    no_stream: bool,
    sink: &dyn OutputSink,
) -> i32 {
    match run_slash_dispatch_future(input, runtime).await {
        SlashDispatchResult::Failed { diagnostic } => {
            let message = diagnostic
                .string_at("/message")
                .or_else(|| diagnostic.string_at(""))
                .unwrap_or("command settings failed");
            runtime.record_execution_error(message);
            sink.error("command", message).await;
            exit_codes::RUNTIME_ERROR
        }
        SlashDispatchResult::Handled { display } => {
            let display_text = display.string_at("").unwrap_or_default();
            sink.command_output_source("", &display).await;
            // G002: `/fusion` spawns a background `local_fusion` task and
            // returns `Handled` immediately (design: no auto-turn). In print
            // mode nothing else keeps the process alive, so without this the
            // worker (and every dispatched panel) died with the process and
            // the user got only a 9-char task id for a run that may never
            // have produced output. `SlashDispatchResult` carries no
            // structured task id (§0: not this package's type to widen), so
            // recover it from the `/fusion` `Done` display's own format
            // (`fusion_command.rs`: `"{task_id}  {preset}  {scope}"`) and
            // await that one task to a terminal status before returning.
            match local_fusion_task_id_to_await(input, display_text) {
                Some(task_id) => {
                    let outcome = Box::pin(await_local_fusion_result(
                        task_id,
                        runtime.task_registry.as_ref(),
                        sink,
                    ))
                    .await;
                    fusion_result_exit_code(outcome.as_ref())
                }
                None => fusion_spawn_failure_exit_code(input, display_text),
            }
        }
        // A prompt-expanding command (`/loop`, Markdown/Plugin): run the expanded
        // prompt AS a turn through the orchestrator (claude-code `type: "prompt"`)
        // instead of just printing it, so a `/loop` invocation actually schedules
        // + executes.
        SlashDispatchResult::RunAsTurn { prompt } => {
            run_slash_as_turn_future(runtime, prompt, max_budget_usd, no_stream, sink).await
        }
        SlashDispatchResult::RunSkillAsTurn {
            command,
            body,
            hook_messages,
        } => {
            run_slash_skill_as_turn_future(
                runtime,
                command,
                body,
                hook_messages,
                max_budget_usd,
                no_stream,
                sink,
            )
            .await
        }
        SlashDispatchResult::Unknown {
            name, suggestion, ..
        } => {
            run_unknown_slash_fallback_future(
                runtime,
                input.clone(),
                name,
                suggestion,
                max_budget_usd,
                no_stream,
                sink,
            )
            .await
        }
        SlashDispatchResult::ParseFailure { input } => {
            run_fallback_prompt_turn_future(
                runtime,
                input,
                Vec::new(),
                None,
                max_budget_usd,
                no_stream,
                sink,
            )
            .await
        }
        SlashDispatchResult::NotASlashCommand => {
            // Defensive: only reached when caller violated the slash-prefix
            // contract.
            runtime.record_execution_error("not a slash command (internal error)");
            if runtime.policy_helper_refusal.current().is_none() {
                sink.error("runtime", "not a slash command (internal error)")
                    .await;
            }
            exit_codes::RUNTIME_ERROR
        }
    }
}

fn unknown_command_fallback_attachment(
    command_name: &Utf16JsonProjection,
    suggestion: Option<&Utf16JsonProjection>,
    catalog: &command_api::HeadlessCommandCatalog,
    rendered: Utf16JsonProjection,
) -> orchestrator::RequestContextTranscriptAttachment {
    let available_names = Utf16JsonProjection::array(
        command_api::native_unknown_command_fallback_available_names(catalog),
    )
    .expect("command catalog names form a JSON array");
    let available_count = Utf16JsonProjection::parse(&catalog.available_command_count.to_string())
        .expect("command count is a JSON number");
    let mut fields = vec![
        ("type".to_owned(), "unknown_command_fallback".into()),
        ("commandName".to_owned(), command_name.clone()),
    ];
    if let Some(suggestion) = command_api::native_unknown_command_fallback_suggestion(suggestion) {
        fields.push(("suggestion".to_owned(), suggestion));
    }
    fields.extend([
        ("availableCommandNames".to_owned(), available_names),
        ("availableCommandCount".to_owned(), available_count),
    ]);
    let payload =
        Utf16JsonProjection::object(fields).expect("unknown command fallback is a JSON object");
    let rendered = Utf16JsonProjection::object(vec![("content".to_owned(), rendered)])
        .and_then(|line| Utf16JsonProjection::array(vec![line]))
        .expect("unknown command rendered view is a content array");
    orchestrator::RequestContextTranscriptAttachment {
        message_id: lingxi_core::types::MessageId::new(),
        payload,
        rendered,
        // Native keeps renderedRole=user; isMeta is mapped independently from the payload type.
        rendered_role: "user",
    }
}

async fn run_fallback_prompt_turn(
    runtime: &Runtime,
    prompt: Utf16JsonProjection,
    request_context: Vec<Utf16JsonProjection>,
    transcript_attachment: Option<orchestrator::RequestContextTranscriptAttachment>,
    max_budget_usd: Option<f64>,
    no_stream: bool,
    sink: &dyn OutputSink,
) -> i32 {
    sink.turn_start().await;
    let cancel = runtime.turn_cancel();
    let result = if no_stream {
        Box::pin(orchestrator::native_user_message_source::with_sdk_source(
            orchestrator::mod_prompt_origin::with_origin(
                json!({"kind":"sdk"}),
                runtime.orchestrator.run_turn_with_request_context(
                    prompt,
                    request_context,
                    transcript_attachment,
                    cancel,
                ),
            ),
        ))
        .await
    } else {
        Box::pin(orchestrator::native_user_message_source::with_sdk_source(
            orchestrator::mod_prompt_origin::with_origin(
                json!({"kind":"sdk"}),
                runtime
                    .orchestrator
                    .run_turn_streaming_with_request_context(
                        prompt,
                        request_context,
                        transcript_attachment,
                        cancel,
                    ),
            ),
        ))
        .await
    };
    stop_background_agents_at_budget(
        max_budget_usd,
        runtime.orchestrator.as_ref(),
        runtime.task_registry.as_ref(),
        &runtime.output,
    )
    .await;
    runtime.execution_interrupted.store(
        matches!(&result, Ok(orchestrator::TurnOutcome::Cancelled)),
        std::sync::atomic::Ordering::Release,
    );
    match result {
        Ok(_) => exit_codes::SUCCESS,
        Err(error) => {
            runtime.record_execution_error(&error);
            if runtime.policy_helper_refusal.current().is_none() {
                sink.error("runtime", &error.to_string()).await;
            }
            exit_codes::RUNTIME_ERROR
        }
    }
}

/// Request normal cancellation of retained print tasks without dropping their
/// execution futures. The owning service continues polling settlement.
pub(super) async fn shutdown_tasks(runtime: &Runtime) {
    stop_print_tasks(runtime).await;
}

#[cfg(test)]
#[path = "run/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "run/init_command_projection_test.rs"]
mod init_command_projection_test;

#[cfg(test)]
#[path = "run/orphan_lifecycle_tests.rs"]
mod orphan_lifecycle_tests;

#[cfg(test)]
#[path = "run/stdio_lifecycle_tests.rs"]
mod stdio_lifecycle_tests;

#[cfg(test)]
#[path = "run/ui_lifecycle_tests.rs"]
mod ui_lifecycle_tests;

#[cfg(test)]
#[path = "run/native_proactivity_control_tests.rs"]
mod native_proactivity_control_tests;

#[cfg(test)]
#[path = "run/native_commands_295_replay_test.rs"]
mod native_commands_295_replay_tests;

#[cfg(test)]
#[path = "run/prompt_suggestion_tests.rs"]
mod prompt_suggestion_tests;

async fn set_result_position(
    runtime: &Runtime,
    stream: &StreamJsonStream,
    queued: usize,
    index: u64,
) {
    let metrics = runtime.orchestrator.completed_turn_metrics();
    let subagent_stats = runtime.agent_session_statistics().await;
    let api_error_stop_reason = metrics
        .as_ref()
        .and_then(|metrics| metrics.api_error_stop_reason.clone());
    let api_error = api_error_stop_reason.is_some();
    let terminal_reason = api_error.then(|| {
        metrics
            .as_ref()
            .and_then(|metrics| metrics.api_error_terminal_reason.clone())
            .unwrap_or_else(|| "api_error".into())
    });
    stream
        .set_result_metadata(crate::headless::stream_json::StreamJsonResultMetadata {
            is_error: api_error,
            terminal_reason,
            queued_turn_count: u64::try_from(queued).unwrap_or(u64::MAX),
            result_index: index,
            num_turns: metrics.as_ref().map(|value| u64::from(value.num_turns)),
            api_error_status: metrics.as_ref().and_then(|value| value.api_error_status),
            stop_reason: api_error_stop_reason
                .or_else(|| metrics.and_then(|value| value.model_stop_reason)),
            subagent_stats: subagent_stats.map(|stats| {
                serde_json::to_value(stats).expect("agent statistics are JSON scalars")
            }),
            ..Default::default()
        })
        .await;
}

#[cfg(test)]
mod result_publication_tests {
    use super::*;
    use crate::headless::io::Output;
    use lingxi_core::host::task_registry::{TaskCreateInput, TaskRegistryHandle};
    use orchestrator::test_support::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn graceful_model_error_keeps_success_envelope_but_fails_execution() {
        let root = tempfile::tempdir().unwrap();
        let (output, receive) = tokio::io::duplex(128 * 1024);
        let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(output)));
        let mut runtime = super::tests::fixture_runtime(stream.clone(), root.path()).await;
        let api = Arc::new(MockStreamingApiClient::with_open_error(
            llm_runtime::LlmError::InvalidRequest {
                message: "400 HEADLESS_LOCAL_PROVIDER_ERROR".into(),
            },
            vec![],
        ));
        runtime.inner.orchestrator = orchestrator::ConversationOrchestrator::into_shared(
            orchestrator::ConversationOrchestrator::new_with_streaming(
                Default::default(),
                Arc::new(MockApiClient::new(vec![])),
                api,
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                stream.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().join("project"),
            ),
        );
        let code = run_stream_json_print(
            &Argv {
                prompt: Some("go".into()),
                ..Default::default()
            },
            &runtime,
            stream.clone(),
            permission::PermissionMode::Default,
        )
        .await;
        assert_eq!(code, exit_codes::RUNTIME_ERROR);
        stream.finish().await.unwrap();
        let mut lines = tokio::io::BufReader::new(receive).lines();
        let result = next_result(&mut lines, &mut Vec::new()).await;
        assert_eq!(result["subtype"], "success");
        assert_eq!(result["is_error"], true);
        assert_eq!(result["terminal_reason"], "api_error");
        assert_eq!(result["stop_reason"], "stop_sequence");
        assert_eq!(result["result"], "400 HEADLESS_LOCAL_PROVIDER_ERROR");
        assert_eq!(result["api_error_status"], 400);
        let errors = runtime.execution_errors.lock().unwrap();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("HEADLESS_LOCAL_PROVIDER_ERROR"));
    }

    #[tokio::test]
    async fn request_too_large_keeps_terminal_reason_separate_from_inner_stop_reason() {
        let root = tempfile::tempdir().unwrap();
        let (output, receive) = tokio::io::duplex(128 * 1024);
        let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(output)));
        let mut runtime = super::tests::fixture_runtime(stream.clone(), root.path()).await;
        let api = Arc::new(MockStreamingApiClient::with_open_error(
            llm_runtime::LlmError::RequestTooLarge,
            vec![],
        ));
        runtime.inner.orchestrator = orchestrator::ConversationOrchestrator::into_shared(
            orchestrator::ConversationOrchestrator::new_with_streaming(
                Default::default(),
                Arc::new(MockApiClient::new(vec![])),
                api,
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                stream.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().join("project"),
            ),
        );

        let code = run_stream_json_print(
            &Argv {
                prompt: Some("go".into()),
                ..Default::default()
            },
            &runtime,
            stream.clone(),
            permission::PermissionMode::Default,
        )
        .await;

        assert_eq!(code, exit_codes::RUNTIME_ERROR);
        stream.finish().await.unwrap();
        let mut lines = tokio::io::BufReader::new(receive).lines();
        let result = next_result(&mut lines, &mut Vec::new()).await;
        assert_eq!(result["subtype"], "success");
        assert_eq!(result["is_error"], true);
        assert_eq!(result["terminal_reason"], "image_error");
        assert_eq!(result["stop_reason"], "stop_sequence");
        let metrics = runtime
            .inner
            .orchestrator
            .completed_turn_metrics()
            .expect("completed request-too-large turn metrics");
        assert_eq!(metrics.stop_reason.as_deref(), Some("model_error"));
        assert_eq!(
            metrics.api_error_terminal_reason.as_deref(),
            Some("image_error")
        );
        assert_eq!(
            metrics.api_error_stop_reason.as_deref(),
            Some("stop_sequence")
        );
    }

    #[tokio::test]
    async fn execution_errors_are_recorded_separately_from_shutdown_failures() {
        let root = tempfile::tempdir().unwrap();
        let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(
            tokio::io::sink(),
        )));
        let runtime = super::tests::fixture_runtime(stream.clone(), root.path()).await;
        assert_eq!(runtime.agent_session_statistics().await.unwrap().spawned, 0);
        let mut publisher = NativeQueryResultPublisher {
            runtime: &runtime,
            stream: &stream,
            prompt_suggestions: SdkPromptSuggestions::new(false),
            suggestion_turn_origin: None,
            input_status: None,
            result_hold: Default::default(),
            max_budget_usd: None,
            fast_mode_state: "off",
            fast_mode_disabled_reason: None,
            betas: &[],
            next_index: 0,
            last_error_published: false,
            last_result_failed: false,
        };
        publisher
            .publish(
                &Err(orchestrator::OrchestratorError::Internal(
                    "controlled failure".into(),
                )),
                0,
            )
            .await;
        assert_eq!(
            *runtime.execution_errors.lock().unwrap(),
            vec!["orchestrator internal error: controlled failure"]
        );
        assert!(runtime.failures.lock().unwrap().is_empty());
        assert!(publisher.last_result_failed);
        publisher
            .publish(&Ok(orchestrator::TurnOutcome::Cancelled), 0)
            .await;
        assert!(runtime
            .execution_interrupted
            .load(std::sync::atomic::Ordering::Acquire));
        publisher
            .publish(&Ok(orchestrator::TurnOutcome::EndTurn), 0)
            .await;
        assert!(
            !runtime
                .execution_interrupted
                .load(std::sync::atomic::Ordering::Acquire),
            "a later completed query has its own execution outcome"
        );
        stream.finish().await.unwrap();
    }

    #[tokio::test]
    async fn max_turns_public_outcome_keeps_its_native_error_result() {
        let root = tempfile::tempdir().unwrap();
        let (output, receive) = tokio::io::duplex(16 * 1024);
        let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(output)));
        let mut runtime = super::tests::fixture_runtime(stream.clone(), root.path()).await;
        runtime.max_turns = Some(3);
        let mut publisher = NativeQueryResultPublisher {
            runtime: &runtime,
            stream: &stream,
            prompt_suggestions: SdkPromptSuggestions::new(false),
            suggestion_turn_origin: None,
            input_status: None,
            result_hold: Default::default(),
            max_budget_usd: None,
            fast_mode_state: "off",
            fast_mode_disabled_reason: None,
            betas: &[],
            next_index: 0,
            last_error_published: false,
            last_result_failed: false,
        };
        publisher
            .publish(&Ok(orchestrator::TurnOutcome::MaxTurns), 0)
            .await;
        let mut lines = tokio::io::BufReader::new(receive).lines();
        let result = next_result(&mut lines, &mut Vec::new()).await;
        assert_eq!(result["subtype"], "error_max_turns");
        assert_eq!(result["is_error"], true);
        assert!(publisher.last_result_failed);
        assert!(publisher.last_error_published);
        assert_eq!(runtime.execution_errors.lock().unwrap().len(), 1);
        stream.finish().await.unwrap();
    }

    #[tokio::test]
    async fn one_shot_uses_streaming_by_default_and_batched_api_for_no_stream() {
        for no_stream in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(
                tokio::io::sink(),
            )));
            let mut runtime = super::tests::fixture_runtime(stream.clone(), root.path()).await;
            let batched = Arc::new(MockApiClient::new(vec![mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: "batched answer".into(),
                    cache_control: None,
                    citations: None,
                }],
                Some("end_turn"),
            )]));
            let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
                message_start("streamed", "claude-opus-4-7"),
                content_block_start_text(0),
                text_delta(0, "streamed answer"),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ]]));
            runtime.inner.orchestrator = orchestrator::ConversationOrchestrator::into_shared(
                orchestrator::ConversationOrchestrator::new_with_streaming(
                    Default::default(),
                    batched.clone(),
                    streaming.clone(),
                    Arc::new(tool_api::ToolRegistry::new()),
                    noop_hook_executor(),
                    Arc::new(NoOpPermissionGate),
                    stream.clone(),
                    Arc::new(StaticMemoryProvider::empty()),
                    root.path().join("project"),
                )
                .with_task_registry(runtime.task_registry.clone()),
            );
            let sink = crate::headless::output::PlainSink::new(
                Output::new(tokio::io::sink()),
                Output::new(tokio::io::sink()),
            );
            let code = run_oneshot_inner(
                &Argv {
                    prompt: Some("say hello".into()),
                    no_stream,
                    ..Default::default()
                },
                &runtime,
                &sink,
            )
            .await;
            assert_eq!(code, exit_codes::SUCCESS);
            if no_stream {
                assert_eq!(batched.captured_msgs().await.len(), 1);
                assert!(streaming.captured_calls().await.is_empty());
            } else {
                assert!(batched.captured_msgs().await.is_empty());
                assert_eq!(streaming.captured_calls().await.len(), 1);
            }
            stream.finish().await.unwrap();
        }
    }

    #[tokio::test]
    async fn no_stream_json_uses_batched_api_and_publishes_result() {
        let root = tempfile::tempdir().unwrap();
        let (output, receive) = tokio::io::duplex(128 * 1024);
        let stream = Arc::new(StreamJsonStream::new_json_mode_placeholder(Output::new(
            output,
        )));
        let mut runtime = super::tests::fixture_runtime(stream.clone(), root.path()).await;
        let batched = Arc::new(MockApiClient::new(vec![mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "batched answer".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        )]));
        let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
            message_start("streamed", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "streamed answer"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]]));
        runtime.inner.orchestrator = orchestrator::ConversationOrchestrator::into_shared(
            orchestrator::ConversationOrchestrator::new_with_streaming(
                Default::default(),
                batched.clone(),
                streaming.clone(),
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                stream.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().join("project"),
            )
            .with_task_registry(runtime.task_registry.clone()),
        );

        let code = run_stream_json_print(
            &Argv {
                prompt: Some("say hello".into()),
                no_stream: true,
                ..Default::default()
            },
            &runtime,
            stream.clone(),
            permission::PermissionMode::Default,
        )
        .await;

        assert_eq!(code, exit_codes::SUCCESS);
        assert_eq!(batched.captured_msgs().await.len(), 1);
        assert!(streaming.captured_calls().await.is_empty());
        stream.finish().await.unwrap();
        let mut lines = tokio::io::BufReader::new(receive).lines();
        let result = next_result(&mut lines, &mut Vec::new()).await;
        assert_eq!(result["type"], "result");
        assert_eq!(result["result"], "batched answer");
    }

    #[tokio::test]
    async fn unknown_slash_persists_typed_user_fallback_and_request_context() {
        use lingxi_core::host::FileSystem;
        use platform_posix::fs::PosixFileSystem;
        use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};

        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript = root.path().join("session.jsonl");
        let filesystem: Arc<dyn FileSystem> =
            Arc::new(PosixFileSystem::new(root.path().to_owned()));
        let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(
            tokio::io::sink(),
        )));
        let mut runtime = super::tests::fixture_runtime(stream.clone(), root.path()).await;
        let api = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
            message_start("unknown-slash", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "I can help with that request."),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]]));
        runtime.inner.orchestrator = orchestrator::ConversationOrchestrator::into_shared(
            orchestrator::ConversationOrchestrator::new_with_streaming(
                Default::default(),
                Arc::new(MockApiClient::new(Vec::new())),
                api.clone(),
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                stream.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                cwd,
            )
            .with_task_registry(runtime.task_registry.clone())
            .with_jsonl_writer(Arc::new(JsonlWriter::new(
                transcript.clone(),
                filesystem.clone(),
            ))),
        );
        let input = Utf16JsonProjection::from("/__missing_command__ keep this prompt");
        let sink = crate::headless::output::PlainSink::new(
            Output::new(tokio::io::sink()),
            Output::new(tokio::io::sink()),
        );

        let code = run_slash_command_with_budget(&input, &runtime, None, false, &sink).await;

        assert_eq!(code, exit_codes::SUCCESS);
        let calls = api.captured_calls().await;
        assert_eq!(calls.len(), 1);
        assert!(calls[0].messages.iter().any(|message| {
            matches!(
                message,
                lingxi_core::types::ConversationMessage::User { is_meta: false, .. }
            ) && message.text_content() == "/__missing_command__ keep this prompt"
        }));
        // Native 2.1.295 `unknown_command_fallback` at src_194447854.js byte 198866158 sets
        // isMeta:true while the JSONL sidecar keeps renderedRole:user.
        assert!(calls[0].messages.iter().any(|message| {
            matches!(
                message,
                lingxi_core::types::ConversationMessage::User { is_meta: true, .. }
            ) && message
                .text_content()
                .contains("The user's message starts with the slash command")
        }));
        let outbound_fallback = calls[0]
            .messages
            .iter()
            .find(|message| {
                matches!(
                    message,
                    lingxi_core::types::ConversationMessage::User { is_meta: true, .. }
                ) && message
                    .text_content()
                    .contains("The user's message starts with the slash command")
            })
            .expect("the request includes the typed fallback as a user row");
        let fallback_request_index = calls[0]
            .messages
            .iter()
            .position(|message| message.id() == outbound_fallback.id())
            .unwrap();
        let prompt_request_index = calls[0]
            .messages
            .iter()
            .position(|message| message.text_content() == "/__missing_command__ keep this prompt")
            .expect("the original slash prompt remains in the request");
        assert!(fallback_request_index < prompt_request_index);
        let history = runtime.orchestrator.session().lock().await.history.clone();
        assert!(history.iter().any(|message| {
            matches!(
                message,
                lingxi_core::types::ConversationMessage::User { is_meta: false, .. }
            ) && message.text_content() == "/__missing_command__ keep this prompt"
        }));
        let history_fallback = history
            .iter()
            .find(|message| {
                matches!(
                    message,
                    lingxi_core::types::ConversationMessage::User { is_meta: true, .. }
                ) && message
                    .text_content()
                    .contains("The user's message starts with the slash command")
            })
            .expect("the fallback remains in live model history");
        assert_eq!(history_fallback.id(), outbound_fallback.id());
        assert_eq!(
            history
                .iter()
                .filter(|message| {
                    message.id() == outbound_fallback.id()
                        && message
                            .text_content()
                            .contains("The user's message starts with the slash command")
                })
                .count(),
            1
        );

        let rows = JsonlReader::new(transcript, filesystem)
            .read_all()
            .await
            .unwrap();
        let persisted_fallback = rows
            .iter()
            .find(|row| {
                row.message_type == "attachment"
                    && row
                        .attachment_projection
                        .as_ref()
                        .and_then(|payload| payload.string_at("/type"))
                        == Some("unknown_command_fallback")
            })
            .expect("the typed fallback is persisted in the transcript");
        assert_eq!(
            persisted_fallback.uuid,
            outbound_fallback.id().as_uuid().to_string()
        );
        assert_eq!(
            persisted_fallback.extra.string_at("/renderedRole"),
            Some("user")
        );
        assert!(persisted_fallback
            .extra
            .string_at("/rendered/0/content")
            .is_some_and(|content| content == outbound_fallback.text_content()));
        stream.finish().await.unwrap();
    }

    #[tokio::test]
    async fn auxiliary_join_retains_children_registered_by_an_admitted_task() {
        let group = Arc::new(PrintAuxTaskGroup::default());
        let child_group = group.clone();
        let (started, admitted) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        group.push(tokio::spawn(async move {
            child_group.push(tokio::spawn(async move {
                let _ = wait.await;
            }));
            let _ = started.send(());
        }));
        let drain_group = group.clone();
        let drain = tokio::spawn(async move {
            drain_group.join().await;
        });
        admitted.await.unwrap();
        tokio::task::yield_now().await;
        assert!(
            !drain.is_finished(),
            "the child remains owned after the parent batch joins"
        );
        release.send(()).unwrap();
        drain.await.unwrap();
        assert!(group.tasks.lock().unwrap().is_empty());
    }

    async fn next_result(
        lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::io::DuplexStream>>,
        observed: &mut Vec<Value>,
    ) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("output remains open");
                let frame: Value = serde_json::from_str(&line).unwrap();
                observed.push(frame.clone());
                if frame["type"] == "result" {
                    return frame;
                }
            }
        })
        .await
        .expect("result must not wait for the still-running delegated task")
    }

    async fn background_result_case(sdk_eof: bool) {
        let root = tempfile::tempdir().unwrap();
        let (output, receive) = tokio::io::duplex(128 * 1024);
        let stream = Arc::new(StreamJsonStream::new_placeholder(Output::new(output)));
        let mut runtime = super::tests::fixture_runtime(stream.clone(), root.path()).await;
        let responses = ["parent", "after child"]
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                vec![
                    message_start(&format!("result-{index}"), "claude-opus-4-7"),
                    content_block_start_text(0),
                    text_delta(0, text),
                    content_block_stop(0),
                    message_delta_stop("end_turn"),
                    message_stop(),
                ]
            })
            .collect();
        let api = Arc::new(MockStreamingApiClient::with_turns(responses));
        runtime.inner.orchestrator = orchestrator::ConversationOrchestrator::into_shared(
            orchestrator::ConversationOrchestrator::new_with_streaming(
                Default::default(),
                Arc::new(MockApiClient::new(vec![])),
                api.clone(),
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                stream.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().join("project"),
            )
            .with_task_registry(runtime.task_registry.clone())
            .with_task_notifications(Arc::new(
                orchestrator::task_notifications_provider::RegistryTaskNotifications::new(
                    runtime.task_registry.clone(),
                    orchestrator::task_notifications_provider::TaskDeliveryHostFacts {
                        windows_or_wsl: cfg!(windows)
                            || matches!(
                                platform_posix::wsl_detect::detect(),
                                platform_posix::wsl_detect::WslKind::WslOne
                                    | platform_posix::wsl_detect::WslKind::WslTwo
                            ),
                        strict_tool_result_pairing: false,
                        bare_mode: false,
                    },
                ),
            )),
        );
        let task = TaskRegistryHandle::create(
            runtime.task_registry.as_ref(),
            TaskCreateInput {
                task_type: "local_agent".into(),
                description: "controlled late completion".into(),
            },
        )
        .await
        .unwrap();
        let runtime = Arc::new(runtime);
        let owned_runtime = runtime.clone();
        let owned_stream = stream.clone();
        let worker = tokio::spawn(async move {
            let argv = Argv {
                prompt: Some("go".into()),
                ..Default::default()
            };
            if sdk_eof {
                let plane = StdioControlPlane::new(owned_stream.outbound_tx());
                let (mut sender, input) = tokio::io::duplex(4096);
                sender
                    .write_all(
                        b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n",
                    )
                    .await
                    .unwrap();
                sender.shutdown().await.unwrap();
                let lifecycle = Arc::new(crate::headless::queued_commands::QueueLifecycle::new(
                    owned_stream.outbound_tx(),
                    "fixture".into(),
                ));
                let channels = crate::headless::stream_json_input::spawn_stdin_router_from_reader(
                    input,
                    Output::new(tokio::io::sink()),
                    false,
                    "fixture".into(),
                    owned_stream.outbound_tx(),
                    lifecycle.clone(),
                );
                let prepared = crate::headless::stdio::PreparedStdio::from_channels(
                    channels,
                    lifecycle,
                    plane.clone(),
                    owned_runtime.shutdown.child_token(),
                )
                .await;
                run_stream_json_input_loop(
                    &Argv::default(),
                    &owned_runtime,
                    owned_stream,
                    permission::PermissionMode::Default,
                    plane,
                    prepared,
                )
                .await
            } else {
                run_stream_json_print(
                    &argv,
                    &owned_runtime,
                    owned_stream,
                    permission::PermissionMode::Default,
                )
                .await
            }
        });
        let mut lines = tokio::io::BufReader::new(receive).lines();
        let mut observed = Vec::new();
        let first = next_result(&mut lines, &mut observed).await;
        assert_eq!(first["result"], "parent");
        assert_eq!(first["result_index"], 0);
        assert!(
            !worker.is_finished(),
            "first result precedes delegated completion"
        );
        assert_eq!(api.captured_calls().await.len(), 1);
        runtime
            .task_registry
            .set_status(&task.task_id, tasks::state::TaskStatus::Completed)
            .await
            .unwrap();
        let second = next_result(&mut lines, &mut observed).await;
        assert_eq!(second["result"], "after child");
        assert_eq!(second["result_index"], 1);
        assert_eq!(second["num_turns"], 1);
        assert_eq!(
            observed
                .iter()
                .filter(|frame| frame["type"] == "system" && frame["subtype"] == "init")
                .count(),
            2
        );
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), worker)
                .await
                .unwrap()
                .unwrap(),
            exit_codes::SUCCESS
        );
        assert_eq!(api.captured_calls().await.len(), 2);
        let history = runtime.orchestrator.session().lock().await.history.clone();
        assert_eq!(
            history
                .iter()
                .filter(|row| matches!(
                    row,
                    lingxi_core::types::ConversationMessage::User { is_meta: false, .. }
                ))
                .count(),
            1,
            "a notification query does not fabricate another human prompt"
        );
        stream.finish().await.unwrap();
    }

    #[tokio::test]
    async fn print_publishes_parent_before_wait_and_notification_query_after_completion() {
        background_result_case(false).await;
    }

    #[tokio::test]
    async fn sdk_eof_publishes_notification_query_result_before_teardown() {
        background_result_case(true).await;
    }
}
