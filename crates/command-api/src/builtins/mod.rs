//! Builtin commands (M8-P9) — the cross-platform builtin slash-command handlers
//! (the 19 "core" commands) plus the registration entry points the composition
//! roots call. Built on the `command-api` runtime.
//!
//! After [`register_all_builtin_commands`] runs, the registry holds the locked
//! 86-name surface; [`register_core_batch_1`] .. [`register_core_batch_8`]
//! then overwrite the implemented entries with their real handle/auth-bound
//! handlers (batch 8 = `autocompact`/`fork`/`goal`/`recap`/`reload-skills`/
//! `skill-doctor`/`stop`). See spec §19.

#![forbid(unsafe_code)]
// Dead code kept visible, not swept: this crate had 1 item(s) rustc could
// reach from nothing when the workspace was measured (2026-09-16). The lint
// stays `warn` at the workspace level so a NEW crate still inherits it; this
// allow is scoped here so the count is per crate and repayable by deleting this
// line. This is the category where "named, computed, never wired" hides — some
// of these read like features that were built and never connected. Each wants a
// decision (delete, or wire), not a blanket deletion.
// ⚠️ The count above is ONE macOS, lib-target measurement. It is not a list of
// deletable items — see docs/HANDOFF-dead-code-adjudication-2026-09-17.md,
// which records two near-misses where it said "dead" about live code.
#![allow(dead_code)]

pub mod agents;
pub mod auto_mode_setup;
pub mod autocompact;
pub mod brief;
pub mod bundled;
pub mod cd;
pub mod clear;
pub mod commit;
pub mod commit_push_pr;
pub mod compact;
pub mod config;
pub mod connect;
pub mod context;
pub mod custom_commands;
pub mod doctor;
pub mod effort;
pub mod exit;
pub mod export;
pub mod files;
pub mod fork;
pub mod fusion;
pub mod goal;
pub mod help;
pub mod hooks;
pub mod ide;
pub mod init;
pub mod init_verifiers;
pub mod insights;
pub mod interactive_only;
pub mod keybindings;
pub mod login;
pub mod logout;
pub mod mcp;
pub mod memory;
pub mod model;
pub mod output_style;
pub mod permissions;
pub mod powerup;
pub mod recap;
pub mod register;
pub mod release_notes;
pub mod reload_skills;
pub mod resume;
pub mod review;
pub mod security_review;
pub mod side_question;
pub mod skill_doctor;
pub mod skills;
pub mod status;
pub mod statusline;
pub mod stickers;
pub mod stop;
pub mod subtask;
pub mod templates;
pub mod usage;
pub mod version;
pub mod workflows;

mod core_placeholders;

pub use agents::AgentsHandler;
pub use auto_mode_setup::{ApplyRunner, AutoModeSetupHandler, ProposeRunner};
pub use autocompact::AutocompactHandler;
pub use brief::BriefHandler;
pub use bundled::register_bundled_skills;
pub use clear::ClearHandler;
pub use commit::CommitHandler;
pub use commit_push_pr::CommitPushPrHandler;
pub use compact::CompactHandler;
pub use config::ConfigHandler;
pub use connect::{
    ChatGptConnectDriver, ConnectCredentialWriter, ConnectError, ConnectHandler,
    CopilotConnectDriver, CopilotConnectStep, OAuthConnectDriver,
};
pub use context::ContextHandler;
pub use doctor::DoctorHandler;
pub use effort::EffortHandler;
pub use exit::ExitHandler;
pub use export::ExportHandler;
pub use files::FilesHandler;
pub use fork::{ForkBackgroundHandler, ForkHandler};
pub use fusion::{
    fusion_request_from_slash, parse_fusion_slash, FusionSlashArgs, FUSION_SLASH_USAGE,
};
pub use goal::GoalHandler;
pub use help::HelpHandler;
pub use hooks::HooksHandler;
pub use ide::IdeHandler;
pub use init::InitHandler;
pub use init_verifiers::InitVerifiersHandler;
pub use insights::InsightsHandler;
pub use interactive_only::InteractiveOnlyHandler;
pub use keybindings::KeybindingsHandler;
pub use login::{LoginHandler, LoginOrgPolicy};
pub use logout::LogoutHandler;
pub use mcp::McpHandler;
pub use memory::MemoryHandler;
pub use model::ModelHandler;
pub use output_style::OutputStyleHandler;
pub use permissions::PermissionsHandler;
pub use powerup::{PowerupHandler, PowerupLesson, POWERUPS_UNLOCKED_KEY, POWERUP_LESSONS};
pub use recap::RecapHandler;
pub use release_notes::ReleaseNotesHandler;
pub use reload_skills::ReloadSkillsHandler;
pub use resume::ResumeHandler;
pub use review::ReviewHandler;
pub use security_review::SecurityReviewHandler;
pub use side_question::SideQuestionHandler;
pub use skill_doctor::SkillDoctorHandler;
pub use skills::SkillsHandler;
pub use status::StatusHandler;
pub use statusline::StatuslineHandler;
pub use stickers::StickersHandler;
pub use stop::StopHandler;
pub use subtask::SubtaskHandler;
pub use templates::INIT_PROMPT;
pub use usage::UsageHandler;
pub use version::{runtime_build_info, BuildInfo, VersionHandler};
pub use workflows::WorkflowsHandler;

pub use custom_commands::{
    load_and_register_custom_commands, load_and_register_managed_custom_commands,
    load_and_register_managed_skill_commands, load_and_register_skill_commands,
    load_and_register_skill_commands_with_roots,
};
pub use register::{
    register_all_builtin_commands, register_core_batch_1, register_core_batch_2,
    register_core_batch_3, register_core_batch_4, register_core_batch_5, register_core_batch_6,
    register_core_batch_7, register_core_batch_8, register_core_connect,
};
