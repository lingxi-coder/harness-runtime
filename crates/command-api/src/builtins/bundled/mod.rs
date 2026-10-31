//! Bundled (programmatically-registered) skills — port of the reference
//! `registerBundledSkill` / `registerBundledSkills` family
//! (`claude-code/src/skills/bundledSkills.ts`).

use std::sync::Arc;

use crate::{CommandFrontmatter, CommandRegistry, CommandSource, SlashCommand, SlashCommandKind};

pub mod batch_skill;
pub mod builtin_skill;
pub mod checkup_skill;
pub mod code_review_skill;
pub mod commit_skill;
pub mod cron_skill;
pub mod dataviz_skill;
pub mod debug_skill;
pub mod explain_usage_skill;
pub mod fewer_permission_prompts_skill;
pub mod keybindings_help_skill;
pub mod loop_skill;
pub mod run_skill;
pub mod run_skill_generator_skill;
pub mod simplify_skill;
pub mod update_config_skill;
pub mod verify_skill;
pub mod workflow_authoring_skill;

/// Register all bundled skills onto `reg` (port of `registerBundledSkills`,
/// `bundledSkills.ts`).
///
/// `cron_enabled` is the host's `isKairosCronEnabled` equivalent
/// (`cron_scheduler_enabled(CLAUDE_CODE_DISABLE_CRON)` on desktop). When `false`
/// neither `/loop` nor `LingXi`'s `/cron` management command is registered. The
/// `/loop` half mirrors the reference `isEnabled: isKairosCronEnabled` gate
/// (loop.ts:83); `/cron` shares it so it cannot advertise a stopped scheduler.
pub fn register_bundled_skills(reg: &mut CommandRegistry, cron_enabled: bool) {
    // Native 2.1.295 Kbo puts workflow commands before v9's bundledSkills,
    // then builtin-plugin skills afterwards. SEe/Ws append order is retained
    // before the independent user/model invocation projections.
    register_dataviz_skill(reg);
    register_update_config_skill(reg);
    register_keybindings_help_skill(reg);
    register_verify_skill(reg);
    register_debug_skill(reg);
    register_code_review_skill(reg);
    register_commit_skill(reg);
    register_simplify_skill(reg);
    register_batch_skill(reg);
    register_fewer_permission_prompts_skill(reg);
    register_checkup_skill(reg);
    register_explain_usage_skill(reg);
    register_loop_skill(reg, cron_enabled);
    register_lingxi_api_skill(reg);
    register_workflow_authoring_skill(reg);
    register_run_skill(reg);
    register_run_skill_generator_skill(reg);
    register_plugin_authoring_skill(reg);
    register_document_authoring_skill(reg);

    // LingXi's explicit scheduler extension is outside Native's factory list.
    register_cron_skill(reg, cron_enabled);
}

/// Register `fo()` — the Native `/commit` bundled prompt. The GrowthBook gate
/// defaults off and is evaluated by Native `bvr()` on the command factory.
fn register_commit_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "commit".into(),
        description: commit_skill::COMMIT_DESCRIPTION.into(),
        menu_description: Some("Create a git commit".into()),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: commit_skill::frontmatter(),
            prompt_fn: Some(Arc::new(commit_skill::CommitPromptFn)),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        is_enabled: telemetry::flag_bool("tengu_cinder_relay", false),
        has_user_specified_description: true,
        argument_hint: Some("[guidance]".into()),
        ..SlashCommand::default()
    });
}

/// Register a desktop Skill API builtin as a slash command using the shared
/// name, description, and prompt body.
fn register_lingxi_api_skill(reg: &mut CommandRegistry) {
    let skill = skill::desktop_builtin_skill("lingxi-api")
        .expect("lingxi-api is registered as a builtin Skill");
    reg.register_command(SlashCommand {
        name: skill.name.into(),
        description: skill.description,
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter {
                session_modes: Some(vec!["chat".into(), "code".into()]),
                ..CommandFrontmatter::default()
            },
            prompt_fn: Some(Arc::new(builtin_skill::DesktopBuiltinSkillPromptFn::new(
                "lingxi-api",
            ))),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        argument_hint: Some("[topic] [question]".into()),
        has_user_specified_description: true,
        ..SlashCommand::default()
    });
}

/// Document authoring through the actual file/service capabilities of the host.
fn register_document_authoring_skill(reg: &mut CommandRegistry) {
    let skill = skill::desktop_builtin_skill("document-authoring")
        .expect("builtin document authoring skill");
    reg.register_command(SlashCommand {
        name: skill.name.into(),
        description: skill.description,
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter {
                session_modes: Some(vec!["chat".into(), "code".into()]),
                ..CommandFrontmatter::default()
            },
            prompt_fn: Some(Arc::new(builtin_skill::DesktopBuiltinSkillPromptFn::new(
                "document-authoring",
            ))),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        argument_hint: Some("[document request]".into()),
        has_user_specified_description: true,
        ..SlashCommand::default()
    });
}

/// Register the desktop plugin-authoring reference as a prompt skill.
fn register_plugin_authoring_skill(reg: &mut CommandRegistry) {
    let skill = skill::desktop_builtin_skill("plugin-authoring")
        .expect("plugin-authoring is registered as a desktop builtin Skill");
    reg.register_command(SlashCommand {
        name: skill.name.into(),
        description: skill.description,
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter::default(),
            prompt_fn: Some(Arc::new(builtin_skill::DesktopBuiltinSkillPromptFn::new(
                "plugin-authoring",
            ))),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        has_user_specified_description: true,
        ..SlashCommand::default()
    });
}

/// Register the user-facing `/cron` scheduler command.
///
/// Unlike `/loop`, which intentionally accepts only interval-shaped inputs and
/// immediately runs the prompt once, `/cron` is the management surface for
/// natural-language or raw five-field schedules. Both commands share the
/// scheduler kill-switch so the palette never advertises an unusable command.
fn register_cron_skill(reg: &mut CommandRegistry, cron_enabled: bool) {
    if !cron_enabled {
        return;
    }
    reg.register_command(SlashCommand {
        name: "cron".into(),
        description:
            "Create, list, or cancel scheduled prompts using natural-language or five-field cron schedules."
                .into(),
        menu_description: Some("Create, list, or cancel scheduled prompts".into()),
        source: CommandSource::Bundled,
        // LingXi extension; 2.1.295 exposes recurring local schedules through
        // `/loop` and the remote `/schedule` skill, not a `/cron` prompt skill.
        // The direct desktop handler remains available to an explicitly typed
        // `/cron` invocation, while the Native Headless projection excludes it.
        native_source: None,
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter {
                allowed_tools: Some(
                    ["CronCreate", "CronList", "CronDelete"]
                        .map(str::to_string)
                        .to_vec(),
                ),
                ..CommandFrontmatter::default()
            },
            prompt_fn: Some(Arc::new(cron_skill::CronPromptFn)),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        disable_model_invocation: true,
        has_user_specified_description: true,
        argument_hint: Some("<schedule or action>".into()),
        when_to_use: Some(
            "When the user explicitly invokes /cron to create, inspect, or cancel a scheduled prompt."
                .into(),
        ),
        ..SlashCommand::default()
    });
}

/// Register the `/dataviz` design-guidance skill. The 2.1.252 oracle exposes
/// this file-backed bundle unconditionally to local sessions (`userInvocable:
/// true`, no feature gate). Its provider-neutral body is built dynamically so
/// an optional raw user request follows the same `## User Request` shape as the
/// reference `getPromptForCommand` implementation.
fn register_dataviz_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "dataviz".into(),
        description: dataviz_skill::DATAVIZ_DESCRIPTION.into(),
        menu_description: Some("Chart and dashboard design guidance".into()),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter {
                session_modes: Some(vec!["chat".into(), "code".into()]),
                ..CommandFrontmatter::default()
            },
            prompt_fn: Some(Arc::new(dataviz_skill::DatavizPromptFn)),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        has_user_specified_description: true,
        ..SlashCommand::default()
    });
}

/// Register the `/batch` bundled skill — parallel-work orchestration
/// (`userInvocable:!0`, `disableModelInvocation:!0`, `argumentHint:"<instruction>"`).
/// The reference's `isEnabled` is an is-git-repo runtime check; we register
/// unconditionally (the prompt itself, and the `await BE()` guard it replaces,
/// have no static equivalent — see [`batch_skill`]).
fn register_batch_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "batch".into(),
        description: batch_skill::BATCH_DESCRIPTION.into(),
        menu_description: Some("Plan a large change; background agents each open a PR".into()),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter::default(),
            prompt_fn: Some(Arc::new(batch_skill::BatchPromptFn)),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        // Reference `disableModelInvocation:!0` — user-only.
        disable_model_invocation: true,
        has_user_specified_description: true,
        when_to_use: Some(batch_skill::BATCH_WHEN_TO_USE.into()),
        argument_hint: Some(batch_skill::BATCH_ARGUMENT_HINT.into()),
        ..SlashCommand::default()
    });
}

/// Register the `/code-review` bundled skill — the reference's assembler skill
/// (registrar over name-var `Lee`, `userInvocable:!0`, no `isEnabled` gate,
/// `argumentHint:"[low|medium|high|xhigh|max] [--fix] [--comment] [<target>]"`).
/// Distinct from the builtin `/review` (GitHub PRs); see [`code_review_skill`].
fn register_code_review_skill(reg: &mut CommandRegistry) {
    let frontmatter = CommandFrontmatter {
        context: Some("fork".into()),
        background: Some(true),
        agent: Some("general-purpose".into()),
        ..CommandFrontmatter::default()
    };
    reg.register_command(SlashCommand {
        name: "code-review".into(),
        description: code_review_skill::CODE_REVIEW_DESCRIPTION.into(),
        menu_description: Some("Review the current diff for bugs and cleanups".into()),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter,
            prompt_fn: Some(Arc::new(code_review_skill::CodeReviewPromptFn)),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        has_user_specified_description: true,
        argument_hint: Some(code_review_skill::CODE_REVIEW_ARGUMENT_HINT.into()),
        ..SlashCommand::default()
    });
}

/// Register the `/fewer-permission-prompts` bundled skill — an inline-body skill
/// in the reference (`userInvocable:!0`, `requires:{workspace:!0}`, no
/// `isEnabled` gate). Its prompt appends `## Additional instructions from the
/// user` on an argument (see [`fewer_permission_prompts_skill`]).
fn register_fewer_permission_prompts_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "fewer-permission-prompts".into(),
        description: fewer_permission_prompts_skill::FEWER_PERMISSION_PROMPTS_DESCRIPTION.into(),
        menu_description: Some("Pre-approve safe read-only commands based on your usage".into()),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter::default(),
            prompt_fn: Some(Arc::new(
                fewer_permission_prompts_skill::FewerPermissionPromptsPromptFn,
            )),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        has_user_specified_description: true,
        ..SlashCommand::default()
    });
}

/// Register the `/run-skill-generator` bundled skill — a file-based skill in the
/// reference (registrar `hab`), USER-only (`disableModelInvocation:!0`, no
/// `isEnabled` gate). Prompt shape matches [`verify_skill`].
fn register_run_skill_generator_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "run-skill-generator".into(),
        description: run_skill_generator_skill::RUN_SKILL_GENERATOR_DESCRIPTION.into(),
        menu_description: Some("Create a skill that knows how to run this project’s app".into()),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter::default(),
            prompt_fn: Some(Arc::new(
                run_skill_generator_skill::RunSkillGeneratorPromptFn,
            )),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        // Reference `disableModelInvocation:!0` — a user-only command.
        disable_model_invocation: true,
        has_user_specified_description: true,
        ..SlashCommand::default()
    });
}

/// Register the `/simplify` bundled skill — an inline-body skill in the
/// reference (registrar `eVp`, `userInvocable:!0`, `argumentHint:"[<target>]"`,
/// no `isEnabled` gate). Its `getPromptForCommand` PREPENDS a `Review target:`
/// line (see [`simplify_skill`]).
/// `Ao()` — the `/debug` skill. See `debug_skill`'s module doc for the two
/// deliberate adaptations (no mid-session enable; the removed guide agent).
fn register_debug_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "debug".into(),
        description: debug_skill::DEBUG_DESCRIPTION.into(),
        menu_description: Some(debug_skill::DEBUG_MENU_DESCRIPTION.into()),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter {
                allowed_tools: Some(["Read", "Grep", "Glob"].map(str::to_string).to_vec()),
                ..CommandFrontmatter::default()
            },
            prompt_fn: Some(Arc::new(debug_skill::DebugPromptFn)),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        disable_model_invocation: true,
        has_user_specified_description: true,
        argument_hint: Some(debug_skill::DEBUG_ARGUMENT_HINT.into()),
        ..SlashCommand::default()
    });
}

/// `Mo()` — the Native `/doctor` bundled prompt, retaining `/checkup` as its
/// declared alias. The structured GUI DoctorReport remains a separate host
/// event and is not a slash command.
fn register_checkup_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "doctor".into(),
        terminal_oriented: true,
        description: checkup_skill::CHECKUP_DESCRIPTION.into(),
        menu_description: Some(checkup_skill::CHECKUP_MENU_DESCRIPTION.into()),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        aliases: vec!["checkup".into()],
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter::default(),
            prompt_fn: Some(Arc::new(checkup_skill::CheckupPromptFn)),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        // Native `disableModelInvocation:!0` — users may invoke the setup
        // prompt, but Headless model-fallback names omit it.
        disable_model_invocation: true,
        has_user_specified_description: true,
        // ⚠️ Upstream also sets `progressMessage:"running checkup"`.
        // `SlashCommand` has no such field in this port, so the spinner keeps
        // its generic text. Recorded rather than dropped silently: the constant
        // stays in `checkup_skill` so the copy is not lost if the field lands.
        ..SlashCommand::default()
    });
}

/// `dCr()` — the `workflow-authoring` skill.
///
/// Registered unconditionally; see the module doc for why this port does not
/// mirror upstream's `isEnabled: () => qc()`.
fn register_workflow_authoring_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "workflow-authoring".into(),
        description: workflow_authoring_skill::WORKFLOW_AUTHORING_DESCRIPTION.into(),
        menu_description: Some(
            workflow_authoring_skill::WORKFLOW_AUTHORING_MENU_DESCRIPTION.into(),
        ),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter::default(),
            prompt_fn: Some(Arc::new(
                workflow_authoring_skill::WorkflowAuthoringPromptFn,
            )),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        has_user_specified_description: true,
        ..SlashCommand::default()
    });
}

fn register_simplify_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "simplify".into(),
        description: simplify_skill::SIMPLIFY_DESCRIPTION.into(),
        menu_description: Some("Clean up the changed code without changing behavior".into()),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter::default(),
            prompt_fn: Some(Arc::new(simplify_skill::SimplifyPromptFn)),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        has_user_specified_description: true,
        // Reference `argumentHint:"[<target>]"`.
        argument_hint: Some(simplify_skill::SIMPLIFY_ARGUMENT_HINT.into()),
        ..SlashCommand::default()
    });
}

/// Register the `/run` bundled skill — a file-based bundled skill in the
/// reference (static SKILL.md, `userInvocable:!0`, no `isEnabled` gate).
fn register_run_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "run".into(),
        description: run_skill::RUN_DESCRIPTION.into(),
        menu_description: Some("Launch this project’s app to see your change working".into()),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter::default(),
            prompt_fn: Some(Arc::new(run_skill::RunPromptFn)),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        has_user_specified_description: true,
        ..SlashCommand::default()
    });
}

/// Register the `/verify` bundled skill — a file-based bundled skill in the
/// reference (`Lu({name:Mee, description:Pob, userInvocable:!0, files:…,
/// getPromptForCommand})`, name-var `Mee="verify"`). Unconditionally registered
/// (no `isEnabled` gate, unlike `/loop`).
fn register_verify_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "verify".into(),
        description: verify_skill::VERIFY_DESCRIPTION.into(),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter::default(),
            prompt_fn: Some(Arc::new(verify_skill::VerifyPromptFn)),
        },
        // Sets `is_bundled` in the skill listing + satisfies the listing's
        // loadedFrom ∈ {bundled,…} filter.
        loaded_from: Some("bundled".into()),
        // Reference `userInvocable:!0`.
        user_invocable: Some(true),
        // Native's dynamic `disableModelInvocation: () => !wvr()` reads the
        // `tengu_opal_circuit` rollout, defaulting off when no reader is
        // installed. The current host's flag snapshot is evaluated at
        // registration, matching the other bundled factory gates.
        disable_model_invocation: !telemetry::flag_bool("tengu_opal_circuit", false),
        // The frontmatter carries an explicit `description` ⇒ listing-eligible.
        has_user_specified_description: true,
        ..SlashCommand::default()
    });
}

/// Register the `/loop` bundled skill (loop.ts:74-92). Metadata is byte-faithful
/// to `registerBundledSkill({ … })` (loop.ts:75-82).
fn register_loop_skill(reg: &mut CommandRegistry, cron_enabled: bool) {
    // loop.ts:83 — `isEnabled: isKairosCronEnabled`.
    if !cron_enabled {
        return;
    }
    reg.register_command(SlashCommand {
        // PARITY: binary `_Zm` (cc_all.txt:521920) `name:"loop"`.
        name: "loop".into(),
        // PARITY 2.1.263 `G()`: `description:"Run a prompt or slash command on a
        // recurring interval (e.g. /loop 5m /foo). Omit the interval to let the
        // model self-pace."` — the dynamic mode is no longer flag-gated.
        description:
            "Run a prompt or slash command on a recurring interval (e.g. /loop 5m /foo). Omit the interval to let the model self-pace."
                .into(),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter::default(),
            prompt_fn: Some(Arc::new(loop_skill::LoopPromptFn)),
        },
        // Sets `is_bundled` in the skill listing + satisfies the listing's
        // loadedFrom ∈ {bundled,…} filter.
        loaded_from: Some("bundled".into()),
        // PARITY: binary `_Zm` `aliases:["proactive"]` (cc_all.txt:521920).
        aliases: vec!["proactive".into()],
        // PARITY: binary `_Zm` `whenToUse` (cc_all.txt:521920).
        when_to_use: Some(
            "When the user wants to set up a recurring task, poll for status, or run something repeatedly on an interval (e.g. \"check the deploy every 5 minutes\", \"keep running /babysit-prs\"). Do NOT invoke for one-off tasks."
                .into(),
        ),
        // PARITY 2.1.263 `G()`: `argumentHint` is `[interval] [prompt]` — the
        // prompt is optional (no prompt = autonomous loop).
        argument_hint: Some("[interval] [prompt]".into()),
        // PARITY: binary `_Zm` `userInvocable:!0` (cc_all.txt:521920).
        user_invocable: Some(true),
        // Explicit description ⇒ listing-eligible.
        has_user_specified_description: true,
        // PARITY: binary `_Zm` `menuDescription:"Repeat a prompt or command on an
        // interval (e.g. /loop 5m /foo)"` (cc_all.txt:521920) — the compact
        // `/`-menu label, now wired via `menu_description` + the completion popup
        // (`menuDescription ?? description`).
        menu_description: Some(
            "Repeat a prompt or command on an interval (e.g. /loop 5m /foo)".into(),
        ),
        ..SlashCommand::default()
    });
}

/// Register the `/update-config` bundled skill (reference registrar `cn`).
///
/// `allowedTools:["Read"]` and `userInvocable:!0`, both verbatim. The skill is
/// model-invocable — its whole purpose is to be reached when the user asks for
/// an automated behaviour that only a hook can deliver.
fn register_update_config_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "update-config".into(),
        description: update_config_skill::UPDATE_CONFIG_DESCRIPTION.into(),
        menu_description: Some("Change settings: hooks, permissions, environment variables".into()),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter {
                allowed_tools: Some(vec!["Read".to_string()]),
                ..CommandFrontmatter::default()
            },
            prompt_fn: Some(Arc::new(update_config_skill::UpdateConfigPromptFn)),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        has_user_specified_description: true,
        ..SlashCommand::default()
    });
}

/// Register the `keybindings-help` bundled skill (reference registrar `$o`).
///
/// `userInvocable:!1` — model-invocable ONLY. The user-facing route is the
/// `/keybindings` command; this exists so a request like "rebind ctrl+s"
/// reaches the model with the live tables attached.
fn register_keybindings_help_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "keybindings-help".into(),
        description: keybindings_help_skill::KEYBINDINGS_HELP_DESCRIPTION.into(),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter {
                allowed_tools: Some(vec!["Read".to_string()]),
                ..CommandFrontmatter::default()
            },
            prompt_fn: Some(Arc::new(keybindings_help_skill::KeybindingsHelpPromptFn)),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(false),
        // Native's bundled-skill constructor derives `isHidden` from
        // `!userInvocable`; this remains model-only and must not enter the
        // unknown-command name list or available-command count.
        is_hidden: true,
        has_user_specified_description: true,
        ..SlashCommand::default()
    });
}

/// Register the `/explain-usage` bundled skill (reference registrar `Mo`).
fn register_explain_usage_skill(reg: &mut CommandRegistry) {
    reg.register_command(SlashCommand {
        name: "explain-usage".into(),
        description: explain_usage_skill::EXPLAIN_USAGE_DESCRIPTION.into(),
        menu_description: Some(
            "See where this session\u{2019}s tokens went, in plain words".into(),
        ),
        source: CommandSource::Bundled,
        native_source: Some("built-in".into()),
        native_type: Some(crate::model::NativeCommandType::Prompt),
        kind: SlashCommandKind::Bundled {
            frontmatter: CommandFrontmatter::default(),
            prompt_fn: Some(Arc::new(explain_usage_skill::ExplainUsagePromptFn)),
        },
        loaded_from: Some("bundled".into()),
        user_invocable: Some(true),
        has_user_specified_description: true,
        ..SlashCommand::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceTextTestExt;

    /// Lock exact membership as well as individual behavior. Desktop Skill API
    /// entries exposed through slash invocation use the same body as the
    /// `Skill` tool.
    #[test]
    fn the_bundled_skill_name_set_is_locked() {
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, true);
        // Enumerate what actually registered, so this catches an ADDITION as
        // well as a loss. Filtering a hardcoded list against the registry would
        // only ever notice removals -- a new skill would slip in silently,
        // which is half a lock.
        let mut got: Vec<String> = reg
            .list_all()
            .into_iter()
            .filter(|cmd| cmd.source == CommandSource::Bundled)
            .map(|cmd| {
                cmd.name
                    .string_at("")
                    .expect("bundled command names are valid Unicode")
                    .to_owned()
            })
            .collect();
        got.sort();

        let want = vec![
            "batch".to_string(),
            "code-review".to_string(),
            "commit".to_string(),
            "cron".to_string(),
            "dataviz".to_string(),
            // Ported 2026-09-10, once the session debug log it reads existed.
            "debug".to_string(),
            "doctor".to_string(),
            "document-authoring".to_string(),
            // Ported 2026-09-10.
            "explain-usage".to_string(),
            "fewer-permission-prompts".to_string(),
            // Ported 2026-09-10: model-invocable only (`userInvocable:!1`), so
            // it never shows in the slash menu; the user route is /keybindings.
            "keybindings-help".to_string(),
            "lingxi-api".to_string(),
            "loop".to_string(),
            "plugin-authoring".to_string(),
            "run".to_string(),
            "run-skill-generator".to_string(),
            "simplify".to_string(),
            // Ported 2026-09-10: settings.json / hooks authoring. Its prompt
            // carries the LIVE `SettingsJson` schema, so it cannot describe a
            // key the loader would reject.
            "update-config".to_string(),
            "verify".to_string(),
            // Ported 2026-09-10 with the 2.1.267 Workflow description split:
            // this skill IS the 17 KB script-writing reference that the tool
            // description no longer inlines on every request.
            "workflow-authoring".to_string(),
        ];
        assert_eq!(
            got, want,
            "every name above must still register; losing one silently is the \
             failure this test exists to catch"
        );

        let doctor = reg.resolve(&"doctor".into()).expect("Native doctor prompt");
        assert_eq!(doctor.source, CommandSource::Bundled);
        assert_eq!(
            doctor.native_type,
            Some(crate::model::NativeCommandType::Prompt)
        );
        assert!(doctor.disable_model_invocation);
        assert!(doctor.terminal_oriented);
        assert_eq!(
            reg.resolve(&"checkup".into())
                .and_then(|command| command.name.string_at("")),
            Some("doctor"),
            "/checkup must resolve through Native doctor's declared alias"
        );

        // The other half of a name set: names that must NOT be here. These are
        // upstream bundled skills whose surfaces this port does not ship, so a
        // sudden appearance means someone wired a skill without its substrate.
        for absent in [
            "artifact-components",
            "claude-in-chrome",
            "design-sync",
            "whiteboard",
            "workshop",
        ] {
            assert!(
                reg.resolve(&absent.into()).is_none(),
                "{absent} needs a surface this port does not have -- registering \
                 it would advertise a skill that cannot run"
            );
        }
    }

    #[test]
    fn commit_bundle_uses_native_default_off_growthbook_gate() {
        let _guard = crate::builtin_support::names::ENV_LOCK.lock().unwrap();
        const FLAG: &str = "tengu_cinder_relay";
        telemetry::test_clear_flag(FLAG);

        let mut disabled = CommandRegistry::new();
        register_bundled_skills(&mut disabled, false);
        let commit = disabled
            .resolve(&"commit".into())
            .expect("commit factory row");
        assert_eq!(commit.source, CommandSource::Bundled);
        assert_eq!(
            commit.native_type,
            Some(crate::model::NativeCommandType::Prompt)
        );
        assert!(!commit.is_enabled, "Native bvr() defaults the feature off");

        telemetry::test_set_flag(FLAG, true);
        let mut enabled = CommandRegistry::new();
        register_bundled_skills(&mut enabled, false);
        assert!(enabled.resolve(&"commit".into()).unwrap().is_enabled);
        telemetry::test_clear_flag(FLAG);
    }

    #[test]
    fn registers_loop_when_cron_enabled() {
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, true);
        let cmd = reg.resolve(&"loop".into()).expect("loop registered");
        assert_eq!(cmd.source, CommandSource::Bundled);
        assert_eq!(cmd.loaded_from.as_deref(), Some("bundled"));
        assert_eq!(cmd.user_invocable, Some(true));
        assert_eq!(
            cmd.argument_hint
                .as_ref()
                .and_then(|hint| hint.string_at("")),
            Some("[interval] [prompt]")
        );
        assert!(cmd.has_user_specified_description);
        assert!(matches!(cmd.kind, SlashCommandKind::Bundled { .. }));
        assert_eq!(
            cmd.description.string_at(""),
            Some(
                "Run a prompt or slash command on a recurring interval (e.g. /loop 5m /foo). Omit the interval to let the model self-pace."
            )
        );
    }

    #[test]
    fn registers_cron_when_scheduler_is_enabled() {
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, true);

        let cmd = reg.resolve(&"cron".into()).expect("cron registered");
        assert_eq!(cmd.source, CommandSource::Bundled);
        assert_eq!(cmd.loaded_from.as_deref(), Some("bundled"));
        assert_eq!(cmd.user_invocable, Some(true));
        assert!(cmd.disable_model_invocation);
        assert_eq!(
            cmd.argument_hint
                .as_ref()
                .and_then(|hint| hint.string_at("")),
            Some("<schedule or action>")
        );
        assert_eq!(
            cmd.menu_description.as_deref(),
            Some("Create, list, or cancel scheduled prompts")
        );
        assert!(cmd.has_user_specified_description);
        match &cmd.kind {
            SlashCommandKind::Bundled {
                frontmatter,
                prompt_fn,
            } => {
                assert_eq!(
                    frontmatter.allowed_tools.as_deref(),
                    Some(
                        ["CronCreate", "CronList", "CronDelete"]
                            .map(str::to_string)
                            .as_slice()
                    )
                );
                let prompt = prompt_fn
                    .as_ref()
                    .expect("prompt_fn set")
                    .build(&"启动一个每天早上汇报武汉天气的任务".into());
                assert!(prompt.contains("CronCreate"));
                assert!(prompt.contains("启动一个每天早上汇报武汉天气的任务"));
            }
            other => panic!("expected Bundled kind, got {other:?}"),
        }
    }

    #[test]
    fn skips_cron_backed_commands_when_scheduler_is_disabled() {
        // The shared scheduler kill-switch keeps both entry points out of the catalog.
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, false);
        assert!(reg.resolve(&"loop".into()).is_none());
        assert!(reg.resolve(&"cron".into()).is_none());
    }

    #[test]
    fn verify_is_registered_unconditionally() {
        // `/verify` has no `isEnabled` gate — present even when cron is off.
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, false);
        let cmd = reg.resolve(&"verify".into()).expect("verify registered");
        assert_eq!(cmd.source, CommandSource::Bundled);
        assert_eq!(cmd.loaded_from.as_deref(), Some("bundled"));
        assert_eq!(cmd.user_invocable, Some(true));
        assert!(cmd.has_user_specified_description);
        assert!(cmd
            .description
            .starts_with("Verify that a code change actually does"));
        match &cmd.kind {
            SlashCommandKind::Bundled { prompt_fn, .. } => {
                let f = prompt_fn.as_ref().expect("prompt_fn set");
                assert!(f
                    .build(&"".into())
                    .starts_with("**Verification is runtime observation.**"));
                assert!(f
                    .build(&"check X".into())
                    .contains("\n\n## User Request\n\ncheck X"));
            }
            other => panic!("expected Bundled kind, got {other:?}"),
        }
    }

    #[test]
    fn verify_model_invocation_follows_native_opal_circuit_default_and_override() {
        const FLAG: &str = "tengu_opal_circuit";
        telemetry::test_clear_flag(FLAG);

        let mut defaulted = CommandRegistry::new();
        register_bundled_skills(&mut defaulted, false);
        assert!(
            defaulted
                .resolve(&"verify".into())
                .unwrap()
                .disable_model_invocation,
            "Native wvr() defaults to false without a flag reader"
        );

        telemetry::test_set_flag(FLAG, true);
        let mut enabled = CommandRegistry::new();
        register_bundled_skills(&mut enabled, false);
        assert!(
            !enabled
                .resolve(&"verify".into())
                .unwrap()
                .disable_model_invocation
        );
        telemetry::test_clear_flag(FLAG);
    }

    #[test]
    fn keybindings_help_maps_non_user_invocable_to_native_hidden() {
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, false);
        let command = reg
            .resolve(&"keybindings-help".into())
            .expect("model-only keybindings prompt remains registered");
        assert_eq!(command.user_invocable, Some(false));
        assert!(command.is_hidden);
    }

    #[test]
    fn run_is_registered_unconditionally() {
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, false);
        let cmd = reg.resolve(&"run".into()).expect("run registered");
        assert_eq!(cmd.source, CommandSource::Bundled);
        assert_eq!(cmd.loaded_from.as_deref(), Some("bundled"));
        assert_eq!(cmd.user_invocable, Some(true));
        assert!(cmd
            .description
            .starts_with("Launch and drive this project's app"));
        match &cmd.kind {
            SlashCommandKind::Bundled { prompt_fn, .. } => {
                let f = prompt_fn.as_ref().expect("prompt_fn set");
                assert!(f
                    .build(&"".into())
                    .starts_with("**Running means launching the actual app"));
            }
            other => panic!("expected Bundled kind, got {other:?}"),
        }
    }

    #[test]
    fn simplify_is_registered_with_argument_hint() {
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, false);
        let cmd = reg
            .resolve(&"simplify".into())
            .expect("simplify registered");
        assert_eq!(cmd.source, CommandSource::Bundled);
        assert_eq!(cmd.loaded_from.as_deref(), Some("bundled"));
        assert_eq!(cmd.user_invocable, Some(true));
        assert_eq!(
            cmd.argument_hint
                .as_ref()
                .and_then(|hint| hint.string_at("")),
            Some("[<target>]")
        );
        assert!(cmd
            .description
            .contains("reuse, simplification, efficiency, and altitude"));
        match &cmd.kind {
            SlashCommandKind::Bundled { prompt_fn, .. } => {
                let f = prompt_fn.as_ref().expect("prompt_fn set");
                // Inline skill PREPENDS a target line (contrast verify/run).
                assert!(f
                    .build(&"".into())
                    .starts_with("`/simplify → 4 cleanup agents"));
                assert!(f
                    .build(&"pull/9".into())
                    .starts_with("Review target: `pull/9`\n\n"));
            }
            other => panic!("expected Bundled kind, got {other:?}"),
        }
    }

    #[test]
    fn run_skill_generator_is_user_only() {
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, false);
        let cmd = reg
            .resolve(&"run-skill-generator".into())
            .expect("registered");
        assert_eq!(cmd.source, CommandSource::Bundled);
        assert_eq!(cmd.user_invocable, Some(true));
        // Reference `disableModelInvocation:!0` — the model may not invoke it.
        assert!(cmd.disable_model_invocation);
        assert!(cmd
            .description
            .starts_with("Author or improve the run-<unit> skill"));
        // Excluded from the model-invocable listing.
        assert!(!reg
            .model_invocable_commands()
            .iter()
            .any(|c| c.name.string_at("") == Some("run-skill-generator")));
    }

    #[test]
    fn fewer_permission_prompts_is_registered() {
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, false);
        let cmd = reg
            .resolve(&"fewer-permission-prompts".into())
            .expect("registered");
        assert_eq!(cmd.source, CommandSource::Bundled);
        assert_eq!(cmd.user_invocable, Some(true));
        assert!(cmd.description.contains(".lingxi/settings.json"));
        match &cmd.kind {
            SlashCommandKind::Bundled { prompt_fn, .. } => {
                let f = prompt_fn.as_ref().expect("prompt_fn set");
                assert!(f
                    .build(&"".into())
                    .starts_with("# Fewer Permission Prompts"));
                assert!(f
                    .build(&"x".into())
                    .contains("\n\n## Additional instructions from the user\n\nx"));
            }
            other => panic!("expected Bundled kind, got {other:?}"),
        }
    }

    #[test]
    fn code_review_is_registered_with_argument_hint() {
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, false);
        let cmd = reg.resolve(&"code-review".into()).expect("registered");
        assert_eq!(cmd.source, CommandSource::Bundled);
        assert_eq!(cmd.user_invocable, Some(true));
        assert_eq!(
            cmd.argument_hint
                .as_ref()
                .and_then(|hint| hint.string_at("")),
            Some("[low|medium|high|xhigh|max] [--fix] [--comment] [<target>]")
        );
        assert!(cmd
            .description
            .starts_with("Review the current diff for correctness bugs"));
        // Distinct command from the builtin `/review`.
        assert!(reg.resolve(&"review".into()).is_none());
        match &cmd.kind {
            SlashCommandKind::Bundled { prompt_fn, .. } => {
                let f = prompt_fn.as_ref().expect("prompt_fn set");
                // Default = the medium effort body; explicit level selects another.
                assert!(f.build(&"".into()).starts_with("`medium effort"));
                assert!(f.build(&"high".into()).starts_with("`high effort"));
            }
            other => panic!("expected Bundled kind, got {other:?}"),
        }
    }

    #[test]
    fn batch_is_user_only_with_when_to_use() {
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, false);
        let cmd = reg.resolve(&"batch".into()).expect("registered");
        assert_eq!(cmd.source, CommandSource::Bundled);
        assert_eq!(cmd.user_invocable, Some(true));
        assert!(cmd.disable_model_invocation);
        assert_eq!(
            cmd.argument_hint
                .as_ref()
                .and_then(|hint| hint.string_at("")),
            Some("<instruction>")
        );
        assert!(cmd
            .when_to_use
            .as_ref()
            .and_then(|value| value.string_at(""))
            .unwrap()
            .starts_with("Use when the user wants"));
        assert!(cmd.description.contains("5–30 isolated worktree agents"));
        match &cmd.kind {
            SlashCommandKind::Bundled { prompt_fn, .. } => {
                let f = prompt_fn.as_ref().expect("prompt_fn set");
                assert!(f.build(&"".into()).starts_with("Provide an instruction"));
                assert!(f
                    .build(&"do X".into())
                    .contains("## User Instruction\n\ndo X"));
            }
            other => panic!("expected Bundled kind, got {other:?}"),
        }
    }

    #[test]
    fn dataviz_is_unconditional_user_invocable_bundle_with_reference_assets() {
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, false);
        let cmd = reg.resolve(&"dataviz".into()).expect("dataviz registered");
        assert_eq!(cmd.source, CommandSource::Bundled);
        assert_eq!(cmd.loaded_from.as_deref(), Some("bundled"));
        assert_eq!(cmd.user_invocable, Some(true));
        assert!(!cmd.disable_model_invocation);
        assert!(cmd.has_user_specified_description);
        assert_eq!(
            cmd.menu_description.as_deref(),
            Some("Chart and dashboard design guidance")
        );
        assert!(cmd
            .description
            .starts_with("Use this skill whenever you are about to create"));
        match &cmd.kind {
            SlashCommandKind::Bundled { prompt_fn, .. } => {
                let prompt = prompt_fn.as_ref().expect("prompt_fn set");
                assert!(prompt
                    .build(&"".into())
                    .starts_with("# Data Visualization\n"));
                assert!(prompt
                    .build(&"make this dashboard accessible".into())
                    .contains("\n\n## User Request\n\nmake this dashboard accessible"));
            }
            other => panic!("expected bundled command, got {other:?}"),
        }
        assert_eq!(dataviz_skill::DATAVIZ_REFERENCE_FILES.len(), 3);
    }

    #[test]
    fn registered_loop_carries_dynamic_prompt_fn() {
        // `LoopPromptFn` touches process-global loop state (`K_n`, the loop.md
        // delivery cache). Serialize with the builder tests in `loop_skill` so
        // this assertion is deterministic under the default parallel runner.
        let _guard = loop_skill::LOOP_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut reg = CommandRegistry::new();
        register_bundled_skills(&mut reg, true);
        let cmd = reg.resolve(&"loop".into()).expect("loop registered");
        match &cmd.kind {
            SlashCommandKind::Bundled { prompt_fn, .. } => {
                let f = prompt_fn.as_ref().expect("prompt_fn set");
                // 2.1.263: empty args → the autonomous default (dynamic pacing);
                // non-empty → the dynamic prompt builder.
                assert!(f
                    .build(&"".into())
                    .starts_with("# /loop — autonomous default with dynamic pacing"));
                assert!(f
                    .build(&"5m /x".into())
                    .starts_with("# /loop — schedule a recurring or self-paced prompt"));
            }
            other => panic!("expected Bundled kind, got {other:?}"),
        }
    }
}
