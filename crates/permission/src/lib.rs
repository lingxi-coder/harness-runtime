//! Permission engine: rule-driven authorization with mode fallback.
//!
//! M1.3 ships minimal `authorize`. Plan 03 (Tools) wires classifiers and
//! shadow detection.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 20 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

pub mod allow_suggestion;
pub mod auto_edit_safety;
pub mod auto_gate;
pub mod auto_mode_argv;
pub mod auto_mode_defaults;
pub mod auto_mode_destructive;
pub mod auto_mode_facts;
pub mod auto_mode_gates;
pub mod auto_mode_io;
pub mod auto_mode_pregather;
pub mod auto_mode_producers;
pub mod auto_mode_propose;
pub mod auto_mode_recon;
pub mod auto_mode_sections;
pub mod auto_mode_setup;
pub mod auto_mode_wizard;
#[cfg(feature = "bash-ast")]
pub mod bash_ast_security;
pub mod bash_command_clamp;
pub mod bash_security;
#[cfg(feature = "bash-ast")]
pub mod bash_tree_sitter;
pub mod bypass_guard;
pub mod classifier;
pub mod cli_mode;
pub mod command_path_containment;
pub mod computer_access;
pub mod dangerous_patterns;
pub mod dangerous_perms;
pub mod dangerous_removal;
pub mod defaults_per_tool;
pub mod denial_tracking;
pub mod filesystem;
pub mod gate;
pub mod git_bare_repo;
pub mod handback_review;
pub mod headless_gate;
pub mod host_context;
pub mod internal_writes;
pub mod layers;
pub mod loader;
pub mod loop_llm;
pub mod mcp_policy;
pub mod mode;
pub mod mode_policy;
mod model_path;
pub mod path_constraints;
pub mod plan_files {
    //! The session's plan files — see [`lingxi_core::host::plan_files`].
    //!
    //! The type lives in `platform-api` because BOTH the permission policy
    //! (for the carve-out) and the orchestrator (for the plan-mode reminder and
    //! `ExitPlanMode`'s read-back) must name the SAME plan file; two
    //! derivations of one path is exactly how the carve-out silently stops
    //! covering the file the model was told to write.
    pub use lingxi_core::host::plan_files::{
        PlanFileIdentity, PlanFileMatcher, PLAN_FILE_READ_ALLOW_REASON,
        PLAN_FILE_WRITE_ALLOW_REASON,
    };
}
pub mod persist;
pub mod policy;
pub mod policy_gate;
pub mod powershell_containment;
pub mod powershell_parse;
pub mod prompting_gate;
pub mod read_auto_allow;
pub mod read_block;
pub mod read_deny_globs;
pub mod read_only_command;
pub mod result;
pub mod rule;
pub mod sandbox_auto_allow;
#[cfg(feature = "bash-ast")]
pub mod sed_redirect_borne;
pub mod sed_validation;
pub mod set_cwd;
pub mod shadow;
pub mod shell_command;
pub mod shell_rule_matching;
pub mod update;
pub mod working_dirs;
pub mod workspace_lease;

pub use allow_suggestion::{allow_suggestion, call_matches_rule};
pub use auto_edit_safety::{
    check_path_safety_for_auto_edit, has_suspicious_windows_path_pattern,
    is_dangerous_file_path_to_auto_edit, normalize_case_for_comparison, AutoEditSafety,
    DANGEROUS_DIRECTORIES, DANGEROUS_FILES,
};
pub use auto_gate::{
    apply_auto_mode_gate, auto_mode_available, auto_mode_denial_reason, cannot_set_auto_message,
    is_anthropic_managed_provider, model_supports_auto_mode, AutoGateDenialReason, AutoGateInputs,
};
pub use bash_command_clamp::{
    clamp_bash_deny_message, clamp_crash_deny_message, clamp_surface_deny_message, find_clamp_miss,
    ClampMiss, ClampMissKind, CLAMP_FAIL_CLOSED_REASON, CLAMP_NO_MATCH_REASON,
    MONITOR_WEBSOCKET_SURFACE, POWERSHELL_CLAMP_DENY_MESSAGE,
};
pub use bash_security::{bash_command_is_safe, BashSafetyVerdict};
pub use bypass_guard::{enforce_bypass_safety, BypassEnv};
pub use classifier::is_classifier_permissions_enabled;
pub use cli_mode::{
    boot_agent_may_adopt_bypass, initial_permission_mode_from_cli,
    initial_permission_mode_from_cli_with_ide, permission_mode_from_cli_string, CliModeSettings,
    IdeSessionInputs, IDE_BYPASS_UNCONSENTED_MSG,
};
pub use command_path_containment::check_command_path_containment;
pub use dangerous_patterns::{
    dangerous_bash_patterns, CROSS_PLATFORM_CODE_EXEC, POWERSHELL_DANGEROUS_PATTERNS,
};
pub use dangerous_perms::{
    find_dangerous_classifier_permissions, find_dangerous_classifier_permissions_with_flag,
    is_dangerous_bash_permission, is_dangerous_classifier_permission,
    is_dangerous_classifier_permission_with_flag, is_dangerous_powershell_permission,
    is_dangerous_task_permission, DangerousPermissionInfo,
};
pub use dangerous_removal::{check_dangerous_removal, is_dangerous_removal_path, DangerousRemoval};
pub use defaults_per_tool::{tool_default, tool_default_names, tool_default_row};
pub use filesystem::FsRoots;
pub use gate::{
    ClassifierOnlyOnBlock, ClassifierOnlyOutcome, ClassifierOnlyPolicy,
    ClassifierOnlyReviewRequest, PermissionDecision, PermissionGate, PermissionRequest,
    PermissionResponse, PromptDecision, PromptDefault, PromptError, PromptingGate,
};
pub use headless_gate::DenyOnAskGate;
pub use internal_writes::{consume_internal_write, mark_internal_write};
pub use layers::{
    apply_context_layers, fold_permission_layers, layer_mode_from_wire, parse_permission_layers,
    strip_widening_permission_layers, FoldedPermissionContext, LayerFoldInputs, PermissionLayer,
};
pub use loader::{
    additional_directories_from_settings_json,
    allow_managed_permission_rules_only_from_settings_json, auto_mode_disabled_from_settings_json,
    auto_mode_grantable_by_source, block_reads_outside_working_directories_from_settings_json,
    bypass_permissions_disabled_from_settings_json, classify_all_shell_from_settings_json,
    default_mode_applies_from_source, default_mode_from_settings_json,
    fold_block_reads_outside_working_directories, permission_rule_file_warning,
    permission_rule_startup_warning, permission_rules_from_settings_json,
    UNTRUSTED_AUTO_DEFAULT_MODE_WARN, UNTRUSTED_BYPASS_DEFAULT_MODE_WARN,
};
pub use mcp_policy::{
    clamp_mcp_permission_result, mcp_server_policy_rules, permission_rules_from_mcp_tool_policies,
    upstream_name_drift_warning, McpPolicyTool, McpServerPolicyView, McpServerToolDecl,
    McpToolMaxPermission, McpToolPermissionPolicy,
};
pub use mode::{next_permission_mode, PermissionMode};
pub use mode_policy::is_plan_safe_tool;
pub use path_constraints::{check_path_constraints, PathConstraintAsk};
pub use persist::{
    persist_auto_mode_save, persist_permission_mode, persist_permission_rule_set,
    persist_permission_update, persist_workspace_directories, persist_workspace_directory,
    remove_permission_update, replace_permission_rules, AutoModeSaveOutcome, PermissionPaths,
    PersistError,
};
pub use policy::{
    job_tmp_session_allowance, tool_wide_name_matches, PermissionPolicy, SessionReadAllowance,
    JOB_TMP_READ_ALLOW_REASON, PROJECT_TEMP_READ_ALLOW_REASON, SCRATCHPAD_READ_ALLOW_REASON,
    TOOL_RESULT_READ_ALLOW_REASON,
};
pub use policy_gate::{LiveModelContext, LiveModelProvider, PolicyPermissionGate};
pub use prompting_gate::InteractivePromptingGate;
pub use read_deny_globs::read_deny_exclude_globs;
pub use read_only_command::command_is_read_only;
pub use result::{
    ClassifierKind, PermissionDecisionReason, PermissionResult, PermissionUpdateDestination,
    SafetyCircuitBreaker, SandboxOverrideReason,
};
pub use rule::{
    normalize_legacy_tool_name, PermissionBehavior, PermissionRule, PermissionRuleSource,
    PermissionRuleValue,
};
pub use sandbox_auto_allow::SandboxAutoAllowConfig;
pub use sed_validation::{
    sed_auto_allow_verdict, sed_constraint_verdict, SedVerdict, SED_ASK_MESSAGE, SED_ASK_REASON,
};
pub use shadow::{detect_unreachable_rules, is_shared_setting_source, ShadowType, UnreachableRule};
pub use update::PermissionUpdate;
pub use working_dirs::{
    is_network_working_directory, is_network_working_directory_against,
    network_working_directory_message, AdditionalWorkingDirs, WorkingDirectory,
};
pub use workspace_lease::{
    local_app_id_for_root, WorkspaceLeaseInfo, WorkspacePermissionLease,
    WorkspacePermissionLeaseRegistry,
};

pub use model_path::{FileSystemPathTranslator, ModelPathOutcome, ModelPathTranslator};

mod monitor_url;
pub use monitor_url::monitor_websocket_url_host;
