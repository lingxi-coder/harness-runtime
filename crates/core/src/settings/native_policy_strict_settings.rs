//! Source-ordered strict validation for Native helper static settings payloads.
//!
//! This is a deliberately bounded validator for the second stage after Native
//! `I5e` and `At`. It does not deserialize through `SettingsJson` or rebuild
//! the source projection. A field without a source-backed rule remains in the
//! projection and is reported as unresolved; arbitrary unknown fields remain
//! valid because `Due(...).passthrough()` accepts them.
//!
//! Source: fixed Claude Code 2.1.295 `src_187673145.js`, binary base
//! 187673145, 434959 bytes, SHA-256
//! `6fbf3e921382e88e71bd834f2034ee24a15cada553c00e95181e44fa033d22aa`.
//! `Aa` starts at local byte 207690, `Hdt` at 180737, `un.forDocument` at
//! 181641, `Due` at 277657, and `fTr` at 371765. The ordered index below is
//! the 183 direct properties in `Aa`'s returned object followed by the seven
//! properties spread from `Vi(Hdt())`. The conditional `xaaIdp` is inside a
//! nested spread and is absent from `Due`'s `Aa(Hdt())` registry because
//! `Hdt()` returns feature names, not `CLAUDE_CODE_ENABLE_XAA`.

use super::native_managed_mcp_servers::normalize_native_managed_mcp_servers;
use super::native_permission_rule_validation::NATIVE_PERMISSION_MODES;
use super::native_policy_js_semantics::NativePolicyJsSemanticsRegistry;
use super::native_policy_marketplaces::normalize_policy_marketplace_field;
use super::native_policy_parser::normalize_mcp_server_entry;
use super::native_policy_regex::{
    NativePolicyRegexValidationOutcome, NativePolicyRegexValidationRow,
};
use crate::types::utf16_json::{JsonValueView, Utf16JsonProjection};

/// Full current `Due` property order from the pinned build: 183 static keys
/// and seven active dynamic keys. `un.forDocument` filters this list by key
/// presence, so validation order is schema order rather than JSON input order.
const NATIVE_THEME_NAMES: &[&str] = &[
    "auto",
    "dark",
    "light",
    "light-daltonized",
    "dark-daltonized",
    "light-ansi",
    "dark-ansi",
];

const NATIVE_OBJECT_PROTOTYPE_KEYS: &[&str] = &[
    "constructor",
    "__defineGetter__",
    "__defineSetter__",
    "hasOwnProperty",
    "__lookupGetter__",
    "__lookupSetter__",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toString",
    "valueOf",
    "__proto__",
    "toLocaleString",
];

pub(super) const NATIVE_DUE_FIELD_ORDER: &[&str] = &[
    "$schema",
    "apiKeyHelper",
    "proxyAuthHelper",
    "awsCredentialExport",
    "awsAuthRefresh",
    "gcpAuthRefresh",
    "processWrapper",
    "policyHelper",
    "policyHelpers",
    "fileSuggestion",
    "respectGitignore",
    "breakReminder",
    "quietHours",
    "cleanupPeriodDays",
    "desktopSessionCleanupPeriodDays",
    "syncClaudeAiSkills",
    "syncClaudeAiPlugins",
    "skillListingMaxDescChars",
    "skillListingBudgetFraction",
    "wslInheritsWindowsSettings",
    "env",
    "attribution",
    "includeCoAuthoredBy",
    "includeGitInstructions",
    "permissions",
    "model",
    "fallbackModel",
    "availableModels",
    "enforceAvailableModels",
    "availableModelsMatch",
    "deniedModels",
    "modelOverrides",
    "modelPicker",
    "modelPricing",
    "enableAllProjectMcpServers",
    "enabledMcpjsonServers",
    "disabledMcpjsonServers",
    "disableClaudeAiConnectors",
    "skillOverrides",
    "disableBundledSkills",
    "managedMcpServers",
    "allowedMcpServers",
    "deniedMcpServers",
    "hooks",
    "worktree",
    "disableAllHooks",
    "disableAgentView",
    "disableRemoteControl",
    "disableWorkflows",
    "disableArtifact",
    "enableArtifact",
    "enableWorkflows",
    "workflowSizeGuideline",
    "workflowKeywordTriggerEnabled",
    "disableSkillShellExecution",
    "defaultShell",
    "bashEditDiffEnabled",
    "bashOutputMaxChars",
    "taskOutputMaxChars",
    "respondToBashCommands",
    "allowManagedHooksOnly",
    "allowedHttpHookUrls",
    "httpHookAllowedEnvVars",
    "allowManagedPermissionRulesOnly",
    "allowManagedMcpServersOnly",
    "allowAllClaudeAiMcps",
    "allowClaudeInChromeWithManagedMcp",
    "strictPluginOnlyCustomization",
    "statusLine",
    "prUrlTemplate",
    "footerLinksRegexes",
    "subagentStatusLine",
    "enabledPlugins",
    "prependPlugins",
    "appendPlugins",
    "extraKnownMarketplaces",
    "additionalMarketplaces",
    "strictKnownMarketplaces",
    "allowedMarketplaces",
    "blockedMarketplaces",
    "disableCommandPluginSources",
    "disableSideloadFlags",
    "pluginSuggestionMarketplaces",
    "forceLoginMethod",
    "forceLoginGatewayUrl",
    "gatewayInternalNetworks",
    "parentSettingsBehavior",
    "managedSourcesBehavior",
    "forceLoginOrgUUID",
    "allowedProviders",
    "forceRemoteSettingsRefresh",
    "otelHeadersHelper",
    "outputStyle",
    "viewMode",
    "language",
    "skipWebFetchPreflight",
    "sandbox",
    "feedbackSurveyRate",
    "feedbackDrafts",
    "spinnerTipsEnabled",
    "spinnerVerbs",
    "spinnerTipsOverride",
    "syntaxHighlightingDisabled",
    "maxProseWidth",
    "spellcheck",
    "terminalTitleFromRename",
    "promptCacheTtl",
    "subagentPromptCacheTtl",
    "alwaysThinkingEnabled",
    "effortLevel",
    "maxEffortLevel",
    "modelSettings",
    "ultracode",
    "autoCompactWindow",
    "advisorModel",
    "fastMode",
    "fastModePerSessionOptIn",
    "promptSuggestionEnabled",
    "emojiCompletionEnabled",
    "awaySummaryEnabled",
    "showClearContextOnPlanAccept",
    "askUserQuestionTimeout",
    "dialogExpiry",
    "agent",
    "modelProposedGoals",
    "companyAnnouncements",
    "pluginConfigs",
    "remote",
    "autoUpdatesChannel",
    "minimumVersion",
    "requiredMinimumVersion",
    "requiredMaximumVersion",
    "plansDirectory",
    "tui",
    "voice",
    "channelsEnabled",
    "allowedChannelPlugins",
    "prefersReducedMotion",
    "timeFormat",
    "timeZone",
    "doneMeansMerged",
    "totalTokensReminder",
    "totalTokensReminderBudget",
    "totalTokensReminderAfterUserTurn",
    "autoMemoryEnabled",
    "autoMemoryDirectory",
    "autoDreamEnabled",
    "showThinkingSummaries",
    "skipDangerousModePermissionPrompt",
    "skipWorkflowUsageWarning",
    "disableAutoMode",
    "remoteTools",
    "sshConfigs",
    "claudeMd",
    "claudeMdExcludes",
    "pluginTrustMessage",
    "theme",
    "editorMode",
    "keybindingFlavor",
    "vimInsertModeRemaps",
    "verbose",
    "preferredNotifChannel",
    "autoCompactEnabled",
    "precomputeCompactionEnabled",
    "idleCompaction",
    "switchModelsOnFlag",
    "autoContinueAtUsageLimit",
    "autoScrollEnabled",
    "wheelScrollAccelerationEnabled",
    "fileCheckpointingEnabled",
    "showTurnDuration",
    "showMessageTimestamps",
    "terminalProgressBarEnabled",
    "todoFeatureEnabled",
    "teammateMode",
    "remoteControlAtStartup",
    "remoteControl",
    "isolatePeerMachines",
    "daemonColdStart",
    "crossSessionInbound",
    "autoUploadSessions",
    "inputNeededNotifEnabled",
    "agentPushNotifEnabled",
    // `...ro(Vi(Hdt()), ...)`, in Hdt/jg order.
    "skipAutoPermissionPrompt",
    "useAutoModeDuringPlan",
    "autoMode",
    "disableDeepLinkRegistration",
    "voiceEnabled",
    "defaultView",
    "axScreenReader",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeStrictSettingsIssue {
    /// Native's dotted path after `path.map(String).join('.')`, retaining
    /// every UTF-16 code unit from dynamic record keys.
    pub path: Utf16JsonProjection,
    /// Source-shaped Zod message text as a JavaScript string projection.
    pub message: Utf16JsonProjection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeStrictSettingsStatus {
    /// Every present known field had a represented validator and passed.
    Valid,
    /// At least one represented non-catch field definitely rejects the value.
    Invalid,
    /// No represented rejection was found, but a present known field remains
    /// unvalidated. The caller must not treat this as a valid Native payload.
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeStrictSettingsResult {
    /// Catch-unset fields are removed here; all other source fields and their
    /// UTF-16 values remain intact.
    pub projection: Utf16JsonProjection,
    /// First three represented issues in Native schema order.
    pub issues: Vec<NativeStrictSettingsIssue>,
    /// Present current `Due` keys for which this module has no complete rule.
    pub unresolved_fields: Vec<String>,
    pub status: NativeStrictSettingsStatus,
}

#[derive(Clone, Copy)]
enum ValueRule {
    String,
    NonEmptyString,
    Boolean,
    Literal(&'static str),
    TimeOfDay(&'static str),
    Attribution,
    Enum(&'static [&'static str]),
    Number(NumberRule),
    StringArray,
    StringOrStringArray,
    StringMap,
    BooleanOrStringArrayMap,
    UnknownObjectMap,
    CoercedStringMap,
    ModelSettingsMap,
    PluginConfigsMap,
    ManagedMcpServersMap,
    BooleanOrEnumStringArray(&'static [&'static str]),
    ModelPricingOverrides,
    McpServerPolicyArray { allowlist: bool },
    FooterLinksRegexArray,
    SpinnerTipsArray,
    MarketplacePolicyArray { field: &'static str },
    AutoCompactWindow,
    Theme,
    EnumMap(&'static [&'static str]),
    Object(&'static ObjectRule),
    Array(&'static ValueRule),
    PermissionRulesArray,
    EmptyObjectOrUnresolved,
    SandboxMachLookupArray,
    SandboxIgnoreViolationsMap,
    SandboxCredentialFileArray,
    SandboxCredentialEnvArray,
    SandboxAwsPairsArray,
    AbsolutePathCatch,
}

#[derive(Clone, Copy)]
struct ObjectRule {
    fields: &'static [ObjectFieldRule],
    unknown_fields: UnknownFields,
    unknown_fields_unresolved: bool,
}

#[derive(Clone, Copy)]
struct ObjectFieldRule {
    name: &'static str,
    rule: ValueRule,
    required: bool,
    invalid_behavior: InvalidBehavior,
}

#[derive(Clone, Copy)]
enum UnknownFields {
    Strip,
    Preserve,
}

#[derive(Clone, Copy)]
enum NumberRule {
    Finite,
    InclusiveRange { min: f64, max: f64 },
    GreaterThanZeroAtMostOne,
    GreaterThanZeroAtMostTen,
    PositiveInteger,
    NonnegativeInteger,
    IntegerAtLeast(u32),
    Integer,
    Minimum(f64),
}

#[derive(Clone, Copy)]
enum InvalidBehavior {
    Reject,
    CatchUnset,
    CatchValue(&'static str),
}

struct RelativeIssue {
    /// Native path segments before its final `path.map(String).join('.')`.
    /// Keeping segments separately preserves empty keys and keys containing
    /// dots while retaining every UTF-16 code unit.
    path_segments: Vec<Vec<u16>>,
    message_units: Vec<u16>,
}

struct ValueValidation {
    replacement: Option<Utf16JsonProjection>,
    issues: Vec<RelativeIssue>,
    unresolved: Vec<String>,
}

const fn required_field(name: &'static str, rule: ValueRule) -> ObjectFieldRule {
    ObjectFieldRule {
        name,
        rule,
        required: true,
        invalid_behavior: InvalidBehavior::Reject,
    }
}

const fn optional_field(name: &'static str, rule: ValueRule) -> ObjectFieldRule {
    ObjectFieldRule {
        name,
        rule,
        required: false,
        invalid_behavior: InvalidBehavior::Reject,
    }
}

const fn optional_catch_field(name: &'static str, rule: ValueRule) -> ObjectFieldRule {
    ObjectFieldRule {
        name,
        rule,
        required: false,
        invalid_behavior: InvalidBehavior::CatchUnset,
    }
}

const fn object_rule(
    fields: &'static [ObjectFieldRule],
    unknown_fields: UnknownFields,
    unknown_fields_unresolved: bool,
) -> ObjectRule {
    ObjectRule {
        fields,
        unknown_fields,
        unknown_fields_unresolved,
    }
}

const STRING_RULE: ValueRule = ValueRule::String;
const COMMAND_FIELDS: &[ObjectFieldRule] = &[
    required_field("type", ValueRule::Literal("command")),
    required_field("command", ValueRule::String),
];
static FILE_SUGGESTION_RULE: ObjectRule = object_rule(COMMAND_FIELDS, UnknownFields::Strip, false);
static SUBAGENT_STATUS_LINE_RULE: ObjectRule =
    object_rule(COMMAND_FIELDS, UnknownFields::Strip, false);
static BREAK_REMINDER_FIELDS: &[ObjectFieldRule] = &[
    optional_field("enabled", ValueRule::Boolean),
    optional_field(
        "intervalMinutes",
        ValueRule::Number(NumberRule::PositiveInteger),
    ),
    optional_field(
        "breakThresholdMinutes",
        ValueRule::Number(NumberRule::PositiveInteger),
    ),
    optional_field("message", ValueRule::String),
];
static BREAK_REMINDER_RULE: ObjectRule =
    object_rule(BREAK_REMINDER_FIELDS, UnknownFields::Strip, false);
static QUIET_HOURS_FIELDS: &[ObjectFieldRule] = &[
    optional_field("enabled", ValueRule::Boolean),
    optional_field(
        "start",
        ValueRule::TimeOfDay("Expected 24-hour local time \"HH:MM\" (e.g. \"22:00\")"),
    ),
    optional_field(
        "end",
        ValueRule::TimeOfDay("Expected 24-hour local time \"HH:MM\" (e.g. \"07:00\")"),
    ),
];
static QUIET_HOURS_RULE: ObjectRule = object_rule(QUIET_HOURS_FIELDS, UnknownFields::Strip, false);
static WORKTREE_FIELDS: &[ObjectFieldRule] = &[
    optional_field("symlinkDirectories", ValueRule::StringArray),
    optional_field("sparsePaths", ValueRule::StringArray),
    optional_field("baseRef", ValueRule::Enum(&["fresh", "head"])),
    optional_catch_field("bgIsolation", ValueRule::Enum(&["worktree", "none"])),
    optional_catch_field("location", ValueRule::String),
];
static WORKTREE_RULE: ObjectRule = object_rule(WORKTREE_FIELDS, UnknownFields::Strip, false);
static SPINNER_VERBS_FIELDS: &[ObjectFieldRule] = &[
    required_field("mode", ValueRule::Enum(&["append", "replace"])),
    required_field("verbs", ValueRule::StringArray),
];
static SPINNER_VERBS_RULE: ObjectRule =
    object_rule(SPINNER_VERBS_FIELDS, UnknownFields::Strip, false);
static VOICE_FIELDS: &[ObjectFieldRule] = &[
    optional_field("enabled", ValueRule::Boolean),
    optional_field("mode", ValueRule::Enum(&["hold", "tap"])),
    optional_field("autoSubmit", ValueRule::Boolean),
];
static VOICE_RULE: ObjectRule = object_rule(VOICE_FIELDS, UnknownFields::Strip, false);
static REMOTE_FIELDS: &[ObjectFieldRule] =
    &[optional_field("defaultEnvironmentId", ValueRule::String)];
static REMOTE_RULE: ObjectRule = object_rule(REMOTE_FIELDS, UnknownFields::Strip, false);
static REMOTE_TOOLS_FIELDS: &[ObjectFieldRule] =
    &[optional_field("allowUnattendedServing", ValueRule::Boolean)];
static REMOTE_TOOLS_RULE: ObjectRule =
    object_rule(REMOTE_TOOLS_FIELDS, UnknownFields::Strip, false);
static STATUS_LINE_FIELDS: &[ObjectFieldRule] = &[
    required_field("type", ValueRule::Literal("command")),
    required_field("command", ValueRule::String),
    optional_field("padding", ValueRule::Number(NumberRule::Finite)),
    optional_catch_field(
        "refreshInterval",
        ValueRule::Number(NumberRule::Minimum(1.0)),
    ),
    optional_field("hideVimModeIndicator", ValueRule::Boolean),
];
static STATUS_LINE_RULE: ObjectRule = object_rule(STATUS_LINE_FIELDS, UnknownFields::Strip, false);
static SSH_CONFIG_FIELDS: &[ObjectFieldRule] = &[
    required_field("id", ValueRule::String),
    required_field("name", ValueRule::String),
    required_field("sshHost", ValueRule::String),
    optional_field("sshPort", ValueRule::Number(NumberRule::Integer)),
    optional_field("sshIdentityFile", ValueRule::String),
    optional_field("startDirectory", ValueRule::String),
];
static SSH_CONFIG_OBJECT_RULE: ObjectRule =
    object_rule(SSH_CONFIG_FIELDS, UnknownFields::Strip, false);
static SSH_CONFIG_RULE: ValueRule = ValueRule::Object(&SSH_CONFIG_OBJECT_RULE);
static REMOTE_CONTROL_FIELDS: &[ObjectFieldRule] = &[optional_catch_field(
    "shareHostProfile",
    ValueRule::Enum(&["off", "basic", "full"]),
)];
static REMOTE_CONTROL_RULE: ObjectRule =
    object_rule(REMOTE_CONTROL_FIELDS, UnknownFields::Strip, false);
static MODEL_PICKER_ROW_FIELDS: &[ObjectFieldRule] = &[
    required_field("model", ValueRule::String),
    optional_field("label", ValueRule::String),
    optional_field("description", ValueRule::String),
    optional_field("behavesAs", ValueRule::String),
];
static MODEL_PICKER_ROW_RULE: ObjectRule =
    object_rule(MODEL_PICKER_ROW_FIELDS, UnknownFields::Strip, false);
static MODEL_PICKER_ROW_VALUE_RULE: ValueRule = ValueRule::Object(&MODEL_PICKER_ROW_RULE);
static MODEL_PICKER_OPTIONS_RULE: ValueRule = ValueRule::Array(&MODEL_PICKER_ROW_VALUE_RULE);
static MODEL_PICKER_FIELDS: &[ObjectFieldRule] = &[
    required_field("options", MODEL_PICKER_OPTIONS_RULE),
    optional_field("replaceBuiltInOptions", ValueRule::Boolean),
];
static MODEL_PICKER_RULE: ObjectRule =
    object_rule(MODEL_PICKER_FIELDS, UnknownFields::Strip, false);
static SPELLCHECK_FIELDS: &[ObjectFieldRule] = &[
    optional_catch_field("enabled", ValueRule::Boolean),
    optional_catch_field(
        "checker",
        ValueRule::Enum(&["aspell", "hunspell", "ispell", "auto"]),
    ),
    optional_catch_field("language", ValueRule::String),
];
static SPELLCHECK_RULE: ObjectRule = object_rule(SPELLCHECK_FIELDS, UnknownFields::Strip, false);
static SPINNER_TIPS_FIELDS: &[ObjectFieldRule] = &[
    optional_catch_field("excludeDefault", ValueRule::Boolean),
    optional_field("tips", ValueRule::SpinnerTipsArray),
    optional_catch_field("tipsFile", ValueRule::String),
    optional_catch_field("label", ValueRule::String),
];
static SPINNER_TIPS_RULE: ObjectRule =
    object_rule(SPINNER_TIPS_FIELDS, UnknownFields::Preserve, false);
static MODEL_PRICING_FIELDS: &[ObjectFieldRule] = &[
    optional_field(
        "multiplier",
        ValueRule::Number(NumberRule::GreaterThanZeroAtMostTen),
    ),
    optional_field("overrides", ValueRule::ModelPricingOverrides),
];
static MODEL_PRICING_RULE: ObjectRule =
    object_rule(MODEL_PRICING_FIELDS, UnknownFields::Strip, false);
static MODEL_PRICING_OVERRIDE_FIELDS: &[ObjectFieldRule] = &[
    required_field(
        "input",
        ValueRule::Number(NumberRule::InclusiveRange {
            min: 0.0,
            max: 10_000.0,
        }),
    ),
    required_field(
        "output",
        ValueRule::Number(NumberRule::InclusiveRange {
            min: 0.0,
            max: 10_000.0,
        }),
    ),
    required_field(
        "cacheRead",
        ValueRule::Number(NumberRule::InclusiveRange {
            min: 0.0,
            max: 10_000.0,
        }),
    ),
    required_field(
        "cacheWrite",
        ValueRule::Number(NumberRule::InclusiveRange {
            min: 0.0,
            max: 10_000.0,
        }),
    ),
];
static MODEL_PRICING_OVERRIDE_RULE: ObjectRule =
    object_rule(MODEL_PRICING_OVERRIDE_FIELDS, UnknownFields::Strip, false);
static MODEL_SETTINGS_ROW_FIELDS: &[ObjectFieldRule] = &[
    optional_catch_field(
        "effortLevel",
        ValueRule::Enum(&["low", "medium", "high", "xhigh"]),
    ),
    optional_catch_field(
        "maxEffortLevel",
        ValueRule::Enum(&["low", "medium", "high", "xhigh", "max"]),
    ),
    required_field("autoCompactWindow", ValueRule::AutoCompactWindow),
];
static MODEL_SETTINGS_ROW_RULE: ObjectRule =
    object_rule(MODEL_SETTINGS_ROW_FIELDS, UnknownFields::Preserve, false);
static CHANNEL_PLUGIN_FIELDS: &[ObjectFieldRule] = &[
    required_field("marketplace", ValueRule::String),
    required_field("plugin", ValueRule::String),
];
static CHANNEL_PLUGIN_RULE: ObjectRule =
    object_rule(CHANNEL_PLUGIN_FIELDS, UnknownFields::Strip, false);
static CHANNEL_PLUGIN_VALUE_RULE: ValueRule = ValueRule::Object(&CHANNEL_PLUGIN_RULE);
static CHANNEL_PLUGIN_ARRAY_RULE: ValueRule = ValueRule::Array(&CHANNEL_PLUGIN_VALUE_RULE);
static ATTRIBUTION_FIELDS: &[ObjectFieldRule] = &[
    optional_field("commit", ValueRule::String),
    optional_field("pr", ValueRule::String),
    optional_field("sessionUrl", ValueRule::Boolean),
];
static ATTRIBUTION_RULE: ObjectRule =
    object_rule(ATTRIBUTION_FIELDS, UnknownFields::Preserve, false);
static PERMISSION_FIELDS: &[ObjectFieldRule] = &[
    optional_field("allow", ValueRule::PermissionRulesArray),
    optional_field("deny", ValueRule::PermissionRulesArray),
    optional_field("ask", ValueRule::PermissionRulesArray),
    optional_field("defaultMode", ValueRule::Enum(&NATIVE_PERMISSION_MODES)),
    optional_field(
        "disableBypassPermissionsMode",
        ValueRule::Enum(&["disable"]),
    ),
    optional_field("disableAutoMode", ValueRule::Enum(&["disable"])),
    optional_field("blockReadsOutsideWorkingDirectories", ValueRule::Boolean),
    optional_field("additionalDirectories", ValueRule::StringArray),
];
static PERMISSION_OBJECT_RULE: ObjectRule =
    object_rule(PERMISSION_FIELDS, UnknownFields::Preserve, true);
static AUTO_MODE_FIELDS: &[ObjectFieldRule] = &[
    optional_field("allow", ValueRule::StringArray),
    optional_field("soft_deny", ValueRule::StringArray),
    optional_field("hard_deny", ValueRule::StringArray),
    optional_field("environment", ValueRule::StringArray),
    optional_field("classifyAllShell", ValueRule::Boolean),
];
static AUTO_MODE_RULE: ObjectRule = object_rule(AUTO_MODE_FIELDS, UnknownFields::Strip, false);

// Fixed 2.1.295 Qjn schema used by `Due.sandbox`. The top-level Qjn object is
// passthrough; its nested network/filesystem/credentials objects use Zod's
// default unknown-key stripping. This is the static fTr path: `At` does not
// call the managed-document `Bo("sandbox", ...)` floor/salvage transform.
static SANDBOX_TLS_TERMINATE_FIELDS: &[ObjectFieldRule] = &[
    optional_field("caCertPath", ValueRule::NonEmptyString),
    optional_field("caKeyPath", ValueRule::NonEmptyString),
];
static SANDBOX_TLS_TERMINATE_RULE: ObjectRule =
    object_rule(SANDBOX_TLS_TERMINATE_FIELDS, UnknownFields::Strip, false);
static SANDBOX_NETWORK_FIELDS: &[ObjectFieldRule] = &[
    optional_field("allowedDomains", ValueRule::StringArray),
    optional_field("deniedDomains", ValueRule::StringArray),
    optional_field("strictAllowlist", ValueRule::Boolean),
    optional_field("allowManagedDomainsOnly", ValueRule::Boolean),
    optional_field("allowUnixSockets", ValueRule::StringArray),
    optional_field("allowAllUnixSockets", ValueRule::Boolean),
    optional_field("allowLocalBinding", ValueRule::Boolean),
    optional_field("allowMachLookup", ValueRule::SandboxMachLookupArray),
    optional_field("httpProxyPort", ValueRule::Number(NumberRule::Finite)),
    optional_field("socksProxyPort", ValueRule::Number(NumberRule::Finite)),
    optional_field(
        "tlsTerminate",
        ValueRule::Object(&SANDBOX_TLS_TERMINATE_RULE),
    ),
];
static SANDBOX_NETWORK_RULE: ObjectRule =
    object_rule(SANDBOX_NETWORK_FIELDS, UnknownFields::Strip, false);
static SANDBOX_FILESYSTEM_FIELDS: &[ObjectFieldRule] = &[
    optional_field("allowWrite", ValueRule::StringArray),
    optional_field("denyWrite", ValueRule::StringArray),
    optional_field("denyRead", ValueRule::StringArray),
    optional_field("allowRead", ValueRule::StringArray),
    optional_field("allowManagedReadPathsOnly", ValueRule::Boolean),
    optional_field("disabled", ValueRule::Boolean),
];
static SANDBOX_FILESYSTEM_RULE: ObjectRule =
    object_rule(SANDBOX_FILESYSTEM_FIELDS, UnknownFields::Strip, false);
static SANDBOX_SIGV4_FIELDS: &[ObjectFieldRule] = &[
    optional_field("streaming", ValueRule::Enum(&["deny", "passthrough"])),
    optional_field("presigned", ValueRule::Enum(&["deny", "passthrough"])),
    optional_field("sigv4a", ValueRule::Enum(&["deny", "passthrough"])),
];
static SANDBOX_SIGV4_RULE: ObjectRule =
    object_rule(SANDBOX_SIGV4_FIELDS, UnknownFields::Strip, false);
static SANDBOX_CREDENTIAL_FILE_FIELDS: &[ObjectFieldRule] = &[
    required_field("path", ValueRule::NonEmptyString),
    required_field("mode", ValueRule::Enum(&["deny", "mask"])),
    optional_field("extract", ValueRule::String),
    optional_field(
        "onExtractNoMatch",
        ValueRule::Enum(&["warn", "deny", "error"]),
    ),
    optional_field("decode", ValueRule::Literal("jwt")),
    optional_field("maskClaims", ValueRule::StringArray),
    optional_field("maskDuplicates", ValueRule::Boolean),
    optional_field("injectHosts", ValueRule::StringArray),
];
static SANDBOX_CREDENTIAL_FILE_RULE: ObjectRule =
    object_rule(SANDBOX_CREDENTIAL_FILE_FIELDS, UnknownFields::Strip, false);
static SANDBOX_CREDENTIAL_ENV_FIELDS: &[ObjectFieldRule] = &[
    required_field("name", ValueRule::String),
    required_field("mode", ValueRule::Enum(&["deny", "mask"])),
    optional_field("extract", ValueRule::String),
    optional_field(
        "onExtractNoMatch",
        ValueRule::Enum(&["warn", "deny", "error"]),
    ),
    optional_field("decode", ValueRule::Literal("jwt")),
    optional_field("maskClaims", ValueRule::StringArray),
    optional_field("maskDuplicates", ValueRule::Boolean),
    optional_field("injectHosts", ValueRule::StringArray),
];
static SANDBOX_CREDENTIAL_ENV_RULE: ObjectRule =
    object_rule(SANDBOX_CREDENTIAL_ENV_FIELDS, UnknownFields::Strip, false);
static SANDBOX_AWS_PAIR_FIELDS: &[ObjectFieldRule] = &[
    required_field("accessKeyIdVar", ValueRule::String),
    required_field("secretAccessKeyVar", ValueRule::String),
    optional_field("sessionTokenVar", ValueRule::String),
];
static SANDBOX_AWS_PAIR_RULE: ObjectRule =
    object_rule(SANDBOX_AWS_PAIR_FIELDS, UnknownFields::Strip, false);
static SANDBOX_CREDENTIALS_FIELDS: &[ObjectFieldRule] = &[
    optional_field("files", ValueRule::SandboxCredentialFileArray),
    optional_field("envVars", ValueRule::SandboxCredentialEnvArray),
    optional_field("allowPlaintextInject", ValueRule::Boolean),
    optional_field("awsPairs", ValueRule::SandboxAwsPairsArray),
    optional_field("sigv4", ValueRule::Object(&SANDBOX_SIGV4_RULE)),
];
static SANDBOX_CREDENTIALS_RULE: ObjectRule =
    object_rule(SANDBOX_CREDENTIALS_FIELDS, UnknownFields::Strip, false);
static SANDBOX_RIPGREP_FIELDS: &[ObjectFieldRule] = &[
    required_field("command", ValueRule::String),
    optional_field("args", ValueRule::StringArray),
];
static SANDBOX_RIPGREP_RULE: ObjectRule =
    object_rule(SANDBOX_RIPGREP_FIELDS, UnknownFields::Strip, false);
static SANDBOX_FIELDS: &[ObjectFieldRule] = &[
    optional_field("enabled", ValueRule::Boolean),
    optional_field("failIfUnavailable", ValueRule::Boolean),
    optional_field("autoAllowBashIfSandboxed", ValueRule::Boolean),
    optional_field("allowUnsandboxedCommands", ValueRule::Boolean),
    optional_field("network", ValueRule::Object(&SANDBOX_NETWORK_RULE)),
    optional_field("filesystem", ValueRule::Object(&SANDBOX_FILESYSTEM_RULE)),
    optional_field("credentials", ValueRule::Object(&SANDBOX_CREDENTIALS_RULE)),
    optional_field("ignoreViolations", ValueRule::SandboxIgnoreViolationsMap),
    optional_field("enableWeakerNestedSandbox", ValueRule::Boolean),
    optional_field("enableWeakerNetworkIsolation", ValueRule::Boolean),
    optional_field("allowAppleEvents", ValueRule::Boolean),
    optional_field("excludedCommands", ValueRule::StringArray),
    optional_field("ripgrep", ValueRule::Object(&SANDBOX_RIPGREP_RULE)),
    optional_catch_field("bwrapPath", ValueRule::AbsolutePathCatch),
    optional_catch_field("socatPath", ValueRule::AbsolutePathCatch),
];
static SANDBOX_RULE: ObjectRule = object_rule(SANDBOX_FIELDS, UnknownFields::Preserve, false);

/// Validate a static payload against the directly source-represented subset of
/// `Due`. The caller must pass the projection after Native `At` transforms.
/// Unknown keys pass through, known unrepresented keys remain unresolved, and
/// Zod `.catch(void 0)` fields are removed rather than rejecting the payload.
pub(crate) fn validate_native_strict_settings(
    source: &Utf16JsonProjection,
    regex_rows: &[NativePolicyRegexValidationRow],
    js_semantics: &NativePolicyJsSemanticsRegistry,
) -> NativeStrictSettingsResult {
    let projection = source.clone();
    let mut issues = Vec::new();
    let mut unresolved_fields = Vec::new();

    let Some(root) = source.at_pointer("") else {
        return NativeStrictSettingsResult {
            projection,
            issues: vec![NativeStrictSettingsIssue {
                path: string_projection(""),
                message: string_projection("Invalid input: expected object, received undefined"),
            }],
            unresolved_fields,
            status: NativeStrictSettingsStatus::Invalid,
        };
    };
    if root.object_entries().is_none() {
        return NativeStrictSettingsResult {
            projection,
            issues: vec![NativeStrictSettingsIssue {
                path: string_projection(""),
                message: string_projection(&invalid_type("object", root)),
            }],
            unresolved_fields,
            status: NativeStrictSettingsStatus::Invalid,
        };
    }

    let mut output_fields = Vec::new();
    for key in NATIVE_DUE_FIELD_ORDER {
        let Ok(value) = source.subprojection(&format!("/{key}")) else {
            continue;
        };
        let key_units = key.encode_utf16().collect::<Vec<_>>();
        let Some((rule, behavior)) = value_rule(key) else {
            // `Due` is passthrough. Only a present key in its own schema
            // registry is unresolved; arbitrary keys are accepted unchanged.
            push_unique(&mut unresolved_fields, key);
            output_fields.push((key_units, value));
            continue;
        };

        let validation = validate_value_rule_with_regex(&value, rule, regex_rows, js_semantics);
        if !validation.unresolved.is_empty() {
            for path in validation.unresolved {
                let full_path = if path.is_empty() {
                    (*key).to_owned()
                } else {
                    format!("{key}.{path}")
                };
                push_unique(&mut unresolved_fields, &full_path);
            }
        }
        if !validation.issues.is_empty() {
            match behavior {
                InvalidBehavior::Reject => {
                    issues.extend(validation.issues.into_iter().map(|issue| {
                        NativeStrictSettingsIssue {
                            path: string_projection_units(dotted_path_units(
                                &prepend_path_segment(
                                    key.encode_utf16().collect(),
                                    issue.path_segments,
                                ),
                            )),
                            message: string_projection_units(issue.message_units),
                        }
                    }));
                }
                InvalidBehavior::CatchUnset => {
                    continue;
                }
                InvalidBehavior::CatchValue(fallback_json) => {
                    let fallback = Utf16JsonProjection::parse(fallback_json)
                        .expect("static catch fallback JSON is valid");
                    output_fields.push((key_units, fallback));
                    continue;
                }
            }
        }
        let normalized = validation.replacement.unwrap_or(value);
        output_fields.push((key_units, normalized));
    }

    // Zod's constructed Due shape is iterated in schema order, followed by
    // unknown passthrough properties in the source object's order.
    if let Some(source_entries) = source.object_entries() {
        for entry in source_entries {
            let known = NATIVE_DUE_FIELD_ORDER.iter().any(|field| {
                entry.code_units == field.encode_utf16().collect::<Vec<_>>().as_slice()
            });
            if known {
                continue;
            }
            let Ok(value) = source.subprojection_property_units(entry.code_units) else {
                push_unique(&mut unresolved_fields, "<passthrough-projection>");
                continue;
            };
            output_fields.push((entry.code_units.to_vec(), value));
        }
    }

    let projection = match Utf16JsonProjection::object_units(output_fields) {
        Ok(projection) => projection,
        Err(_) => {
            push_unique(&mut unresolved_fields, "<due-output-projection>");
            source.clone()
        }
    };

    let status = if !issues.is_empty() {
        NativeStrictSettingsStatus::Invalid
    } else if !unresolved_fields.is_empty() {
        NativeStrictSettingsStatus::Unresolved
    } else {
        NativeStrictSettingsStatus::Valid
    };
    issues.truncate(3);
    NativeStrictSettingsResult {
        projection,
        issues,
        unresolved_fields,
        status,
    }
}

/// Collect the RegExp inputs that the sandbox credential super-refinements
/// will actually reach after their ordinary object schemas succeed. The
/// static `At` pass does not alter `sandbox`, so callers can pass its result;
/// runtime helper callers pass their source-selected `managedSettings` after
/// the same At pass. This is only a preflight list, not validation.
pub(crate) fn collect_native_regex_patterns(source: &Utf16JsonProjection) -> Vec<Vec<u16>> {
    let Some(sandbox) = source.subprojection("/sandbox").ok() else {
        return Vec::new();
    };
    let Some(credentials) = sandbox.subprojection("/credentials").ok() else {
        return Vec::new();
    };
    let mut patterns = Vec::new();
    for (field, kind, rule) in [
        (
            "files",
            SandboxCredentialKind::File,
            &SANDBOX_CREDENTIAL_FILE_RULE,
        ),
        (
            "envVars",
            SandboxCredentialKind::Environment,
            &SANDBOX_CREDENTIAL_ENV_RULE,
        ),
    ] {
        let Some(array) = credentials.subprojection(&format!("/{field}")).ok() else {
            continue;
        };
        let Some(length) = array.at_pointer("").and_then(JsonValueView::array_len) else {
            continue;
        };
        for index in 0..length {
            let Some(row) = array.subprojection(&format!("/{index}")).ok() else {
                continue;
            };
            let Ok(preprocessed) = preprocess_deny_credential_entry(&row) else {
                continue;
            };
            let base = validate_object(&preprocessed, rule);
            if !base.issues.is_empty() || !base.unresolved.is_empty() {
                continue;
            }
            let Some(root) = preprocessed.at_pointer("") else {
                continue;
            };
            if root.get_property("mode").and_then(JsonValueView::as_str) != Some("mask") {
                continue;
            }
            if matches!(kind, SandboxCredentialKind::Environment)
                && !native_credential_name_is_valid(root, "name")
            {
                continue;
            }
            let Some(pattern) = root
                .get_property("extract")
                .and_then(JsonValueView::code_units)
            else {
                continue;
            };
            if !patterns
                .iter()
                .any(|existing: &Vec<u16>| existing.as_slice() == pattern)
            {
                patterns.push(pattern.to_vec());
            }
        }
    }
    patterns
}

fn native_credential_name_is_valid(root: JsonValueView<'_>, field: &str) -> bool {
    let Some(units) = root.get_property(field).and_then(JsonValueView::code_units) else {
        return false;
    };
    units.first().is_some_and(|first| {
        *first == u16::from(b'_')
            || (*first >= u16::from(b'A') && *first <= u16::from(b'Z'))
            || (*first >= u16::from(b'a') && *first <= u16::from(b'z'))
    }) && units.iter().skip(1).all(|unit| {
        *unit == u16::from(b'_')
            || (*unit >= u16::from(b'A') && *unit <= u16::from(b'Z'))
            || (*unit >= u16::from(b'a') && *unit <= u16::from(b'z'))
            || (*unit >= u16::from(b'0') && *unit <= u16::from(b'9'))
    })
}

fn value_rule(key: &str) -> Option<(ValueRule, InvalidBehavior)> {
    use InvalidBehavior::{CatchUnset, Reject};
    use ValueRule::{
        Array, Attribution, AutoCompactWindow, Boolean, BooleanOrEnumStringArray,
        BooleanOrStringArrayMap, CoercedStringMap, EmptyObjectOrUnresolved, Enum, EnumMap,
        FooterLinksRegexArray, ManagedMcpServersMap, MarketplacePolicyArray, McpServerPolicyArray,
        ModelSettingsMap, NonEmptyString, Number, Object, PluginConfigsMap, String, StringArray,
        StringMap, StringOrStringArray, Theme, UnknownObjectMap,
    };

    let rule = match key {
        "$schema"
        | "model"
        | "outputStyle"
        | "language"
        | "apiKeyHelper"
        | "proxyAuthHelper"
        | "awsCredentialExport"
        | "awsAuthRefresh"
        | "gcpAuthRefresh"
        | "processWrapper"
        | "prUrlTemplate"
        | "otelHeadersHelper"
        | "advisorModel"
        | "agent"
        | "minimumVersion"
        | "requiredMinimumVersion"
        | "requiredMaximumVersion"
        | "plansDirectory"
        | "timeFormat"
        | "timeZone"
        | "autoMemoryDirectory"
        | "claudeMd"
        | "pluginTrustMessage" => (String, Reject),
        "forceLoginGatewayUrl" => (NonEmptyString, CatchUnset),
        "enforceAvailableModels"
        | "disableBundledSkills"
        | "disableAllHooks"
        | "fastModePerSessionOptIn"
        | "terminalTitleFromRename"
        | "forceRemoteSettingsRefresh"
        | "respectGitignore"
        | "syncClaudeAiSkills"
        | "syncClaudeAiPlugins"
        | "wslInheritsWindowsSettings"
        | "includeCoAuthoredBy"
        | "includeGitInstructions"
        | "enableAllProjectMcpServers"
        | "disableClaudeAiConnectors"
        | "disableAgentView"
        | "disableRemoteControl"
        | "disableWorkflows"
        | "disableArtifact"
        | "enableArtifact"
        | "enableWorkflows"
        | "workflowKeywordTriggerEnabled"
        | "disableSkillShellExecution"
        | "bashEditDiffEnabled"
        | "respondToBashCommands"
        | "allowManagedHooksOnly"
        | "allowManagedPermissionRulesOnly"
        | "allowManagedMcpServersOnly"
        | "allowAllClaudeAiMcps"
        | "allowClaudeInChromeWithManagedMcp"
        | "disableCommandPluginSources"
        | "disableSideloadFlags"
        | "skipWebFetchPreflight"
        | "spinnerTipsEnabled"
        | "syntaxHighlightingDisabled"
        | "alwaysThinkingEnabled"
        | "fastMode"
        | "promptSuggestionEnabled"
        | "emojiCompletionEnabled"
        | "awaySummaryEnabled"
        | "showClearContextOnPlanAccept"
        | "channelsEnabled"
        | "prefersReducedMotion"
        | "doneMeansMerged"
        | "totalTokensReminderAfterUserTurn"
        | "autoMemoryEnabled"
        | "autoDreamEnabled"
        | "showThinkingSummaries"
        | "skipDangerousModePermissionPrompt"
        | "skipWorkflowUsageWarning"
        | "verbose"
        | "autoCompactEnabled"
        | "precomputeCompactionEnabled"
        | "idleCompaction"
        | "switchModelsOnFlag"
        | "autoContinueAtUsageLimit"
        | "autoScrollEnabled"
        | "wheelScrollAccelerationEnabled"
        | "fileCheckpointingEnabled"
        | "showTurnDuration"
        | "showMessageTimestamps"
        | "terminalProgressBarEnabled"
        | "todoFeatureEnabled"
        | "remoteControlAtStartup"
        | "isolatePeerMachines"
        | "autoUploadSessions"
        | "inputNeededNotifEnabled"
        | "agentPushNotifEnabled"
        | "skipAutoPermissionPrompt"
        | "useAutoModeDuringPlan"
        | "voiceEnabled"
        | "axScreenReader" => (Boolean, Reject),
        "availableModelsMatch" => (Enum(&["prefix", "exact"]), Reject),
        "viewMode" => (Enum(&["default", "verbose", "focus"]), CatchUnset),
        "askUserQuestionTimeout" | "dialogExpiry" => {
            (Enum(&["60s", "5m", "10m", "never"]), CatchUnset)
        }
        "workflowSizeGuideline" => (Enum(&["unrestricted", "small", "medium", "large"]), Reject),
        "strictPluginOnlyCustomization" => (
            BooleanOrEnumStringArray(&["skills", "agents", "hooks", "mcp"]),
            CatchUnset,
        ),
        "defaultShell" => (Enum(&["bash", "powershell"]), Reject),
        "feedbackDrafts" => (Enum(&["notify", "quiet", "off"]), Reject),
        "parentSettingsBehavior" | "managedSourcesBehavior" => {
            (Enum(&["first-wins", "merge"]), Reject)
        }
        "autoUpdatesChannel" => (Enum(&["latest", "stable", "rc"]), Reject),
        "maxEffortLevel" => (Enum(&["low", "medium", "high", "xhigh", "max"]), CatchUnset),
        "modelProposedGoals" => (Enum(&["auto", "alwaysAsk", "disabled"]), CatchUnset),
        "teammateMode" => (Enum(&["tmux", "iterm2", "in-process", "auto"]), CatchUnset),
        "keybindingFlavor" => (Enum(&["classic", "readline"]), CatchUnset),
        "theme" => (Theme, CatchUnset),
        "autoCompactWindow" => (AutoCompactWindow, CatchUnset),
        "gatewayInternalNetworks" => (
            StringArray,
            InvalidBehavior::CatchValue(r#"["(unreadable)"]"#),
        ),
        "tui" => (Enum(&["default", "fullscreen"]), Reject),
        "totalTokensReminder" => (
            Enum(&["off", "infinite", "fixed", "countdown", "padded-countdown"]),
            Reject,
        ),
        "daemonColdStart" => (Enum(&["transient", "ask"]), Reject),
        "disableAutoMode" => (Enum(&["disable"]), Reject),
        "disableDeepLinkRegistration" => (Enum(&["disable"]), Reject),
        "defaultView" => (Enum(&["chat", "transcript"]), Reject),
        "forceLoginMethod" => (Enum(&["claudeai", "console", "gateway"]), CatchUnset),
        "effortLevel" => (Enum(&["low", "medium", "high", "xhigh"]), CatchUnset),
        "promptCacheTtl" | "subagentPromptCacheTtl" => (Enum(&["5m", "1h"]), CatchUnset),
        "crossSessionInbound" => (Enum(&["accept", "hold", "refuse"]), CatchUnset),
        "feedbackSurveyRate" => (
            Number(NumberRule::InclusiveRange { min: 0.0, max: 1.0 }),
            Reject,
        ),
        "skillListingBudgetFraction" => (Number(NumberRule::GreaterThanZeroAtMostOne), Reject),
        "skillListingMaxDescChars" => (Number(NumberRule::PositiveInteger), Reject),
        "cleanupPeriodDays" => (Number(NumberRule::PositiveInteger), Reject),
        "desktopSessionCleanupPeriodDays" => (Number(NumberRule::NonnegativeInteger), Reject),
        "totalTokensReminderBudget" => (Number(NumberRule::PositiveInteger), Reject),
        "bashOutputMaxChars" | "taskOutputMaxChars" => {
            (Number(NumberRule::PositiveInteger), CatchUnset)
        }
        "maxProseWidth" => (Number(NumberRule::IntegerAtLeast(40)), CatchUnset),
        "fallbackModel"
        | "availableModels"
        | "deniedModels"
        | "enabledMcpjsonServers"
        | "disabledMcpjsonServers"
        | "allowedHttpHookUrls"
        | "httpHookAllowedEnvVars"
        | "pluginSuggestionMarketplaces"
        | "companyAnnouncements"
        | "claudeMdExcludes" => (StringArray, Reject),
        "allowedChannelPlugins" => (CHANNEL_PLUGIN_ARRAY_RULE, Reject),
        // Native filters the known provider names in Due, then O_ restores
        // source-approved `anthropic[A-Z]...` names after parsing.
        "allowedProviders" => (StringArray, CatchUnset),
        "extraKnownMarketplaces" => (EmptyObjectOrUnresolved, Reject),
        "managedMcpServers" => (ManagedMcpServersMap, Reject),
        "modelSettings" => (ModelSettingsMap, CatchUnset),
        "pluginConfigs" => (PluginConfigsMap, Reject),
        "vimInsertModeRemaps" => (UnknownObjectMap, CatchUnset),
        "editorMode" => (Enum(&["normal", "vim"]), CatchUnset),
        "preferredNotifChannel" => (
            Enum(&[
                "auto",
                "iterm2",
                "terminal_bell",
                "iterm2_with_bell",
                "kitty",
                "ghostty",
                "notifications_disabled",
            ]),
            CatchUnset,
        ),
        "enabledPlugins" => (BooleanOrStringArrayMap, Reject),
        "prependPlugins" | "appendPlugins" => (StringArray, CatchUnset),
        "modelOverrides" => (StringMap, Reject),
        "forceLoginOrgUUID" => (StringOrStringArray, Reject),
        "skillOverrides" => (
            EnumMap(&["on", "name-only", "user-invocable-only", "off"]),
            Reject,
        ),
        "fileSuggestion" => (Object(&FILE_SUGGESTION_RULE), Reject),
        "breakReminder" => (Object(&BREAK_REMINDER_RULE), Reject),
        "quietHours" => (Object(&QUIET_HOURS_RULE), Reject),
        "attribution" => (Attribution, Reject),
        "worktree" => (Object(&WORKTREE_RULE), Reject),
        "spinnerVerbs" => (Object(&SPINNER_VERBS_RULE), Reject),
        "voice" => (Object(&VOICE_RULE), Reject),
        "remote" => (Object(&REMOTE_RULE), Reject),
        "remoteTools" => (Object(&REMOTE_TOOLS_RULE), Reject),
        "statusLine" => (Object(&STATUS_LINE_RULE), Reject),
        "subagentStatusLine" => (Object(&SUBAGENT_STATUS_LINE_RULE), Reject),
        "sshConfigs" => (Array(&SSH_CONFIG_RULE), Reject),
        "remoteControl" => (Object(&REMOTE_CONTROL_RULE), Reject),
        "modelPicker" => (Object(&MODEL_PICKER_RULE), Reject),
        "modelPricing" => (Object(&MODEL_PRICING_RULE), Reject),
        "spellcheck" => (Object(&SPELLCHECK_RULE), Reject),
        "spinnerTipsOverride" => (Object(&SPINNER_TIPS_RULE), CatchUnset),
        "footerLinksRegexes" => (FooterLinksRegexArray, CatchUnset),
        "allowedMcpServers" => (McpServerPolicyArray { allowlist: true }, Reject),
        "deniedMcpServers" => (McpServerPolicyArray { allowlist: false }, Reject),
        "strictKnownMarketplaces" => (
            MarketplacePolicyArray {
                field: "strictKnownMarketplaces",
            },
            Reject,
        ),
        "blockedMarketplaces" => (
            MarketplacePolicyArray {
                field: "blockedMarketplaces",
            },
            Reject,
        ),
        "permissions" => (Object(&PERMISSION_OBJECT_RULE), Reject),
        "autoMode" => (Object(&AUTO_MODE_RULE), Reject),
        "ultracode" => (Boolean, CatchUnset),
        // `Qjn` is the source `Due.sandbox` schema. It is passthrough at the
        // root and strips unknown fields from its nested objects.
        "sandbox" => (Object(&SANDBOX_RULE), Reject),
        // Empty hook roots are handled by At + nested QQ; nonempty hook rules
        // use the source-bound hook normalizer earlier in the pipeline.
        "hooks" => (EmptyObjectOrUnresolved, Reject),
        "env" => (CoercedStringMap, Reject),
        _ => return None,
    };
    Some(rule)
}

fn valid() -> ValueValidation {
    ValueValidation {
        replacement: None,
        issues: Vec::new(),
        unresolved: Vec::new(),
    }
}

fn issue(path: impl Into<String>, message: String) -> RelativeIssue {
    issue_units(path, message.encode_utf16().collect())
}

fn issue_units(path: impl Into<String>, message_units: Vec<u16>) -> RelativeIssue {
    let path = path.into();
    RelativeIssue {
        path_segments: if path.is_empty() {
            Vec::new()
        } else {
            path.split('.')
                .map(|segment| segment.encode_utf16().collect())
                .collect()
        },
        message_units,
    }
}

fn issue_segments(path_segments: Vec<Vec<u16>>, message: String) -> RelativeIssue {
    RelativeIssue {
        path_segments,
        message_units: message.encode_utf16().collect(),
    }
}

fn prepend_path_segment(segment: Vec<u16>, mut path: Vec<Vec<u16>>) -> Vec<Vec<u16>> {
    path.insert(0, segment);
    path
}

fn dotted_path_units(path_segments: &[Vec<u16>]) -> Vec<u16> {
    let total_units =
        path_segments.iter().map(Vec::len).sum::<usize>() + path_segments.len().saturating_sub(1);
    let mut units = Vec::with_capacity(total_units);
    for (index, segment) in path_segments.iter().enumerate() {
        if index > 0 {
            units.push(u16::from(b'.'));
        }
        units.extend_from_slice(segment);
    }
    units
}

fn join_path_segments(left: &[Vec<u16>], right: &[Vec<u16>]) -> Vec<Vec<u16>> {
    let mut segments = Vec::with_capacity(left.len() + right.len());
    segments.extend_from_slice(left);
    segments.extend_from_slice(right);
    segments
}

fn unresolved(path: impl Into<String>) -> ValueValidation {
    ValueValidation {
        replacement: None,
        issues: Vec::new(),
        unresolved: vec![path.into()],
    }
}

fn empty_js_semantics() -> NativePolicyJsSemanticsRegistry {
    NativePolicyJsSemanticsRegistry::from_rows(Vec::new(), Vec::new())
        .expect("empty Native policy semantic registry is valid")
}

fn validate_value_rule(value: &Utf16JsonProjection, rule: ValueRule) -> ValueValidation {
    validate_value_rule_with_regex(value, rule, &[], &empty_js_semantics())
}

fn validate_value_rule_with_regex(
    value: &Utf16JsonProjection,
    rule: ValueRule,
    regex_rows: &[NativePolicyRegexValidationRow],
    js_semantics: &NativePolicyJsSemanticsRegistry,
) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return ValueValidation {
            replacement: None,
            issues: vec![issue(
                String::new(),
                "Invalid input: expected value, received undefined".to_owned(),
            )],
            unresolved: Vec::new(),
        };
    };
    match rule {
        ValueRule::String | ValueRule::NonEmptyString => {
            let Some(units) = root.code_units() else {
                return invalid_value("string", root);
            };
            if matches!(rule, ValueRule::NonEmptyString) && units.is_empty() {
                return ValueValidation {
                    replacement: None,
                    issues: vec![issue(
                        String::new(),
                        "Too small: expected string to have >=1 characters".to_owned(),
                    )],
                    unresolved: Vec::new(),
                };
            }
            valid()
        }
        ValueRule::Boolean => {
            if root.as_bool().is_some() {
                valid()
            } else {
                invalid_value("boolean", root)
            }
        }
        ValueRule::Literal(expected) => {
            let expected_units = expected.encode_utf16().collect::<Vec<_>>();
            if root.code_units() == Some(expected_units.as_slice()) {
                valid()
            } else {
                ValueValidation {
                    replacement: None,
                    issues: vec![issue(
                        String::new(),
                        format!("Invalid input: expected {expected:?}"),
                    )],
                    unresolved: Vec::new(),
                }
            }
        }
        ValueRule::TimeOfDay(message) => {
            if root.as_str().is_some_and(is_native_time_of_day) {
                valid()
            } else {
                ValueValidation {
                    replacement: None,
                    issues: vec![issue(String::new(), message.to_owned())],
                    unresolved: Vec::new(),
                }
            }
        }
        ValueRule::Attribution => validate_attribution(value, root),
        ValueRule::Enum(options) => {
            if root.as_str().is_some_and(|value| options.contains(&value)) {
                valid()
            } else {
                ValueValidation {
                    replacement: None,
                    issues: vec![issue(String::new(), enum_error(options))],
                    unresolved: Vec::new(),
                }
            }
        }
        ValueRule::Number(rule) => match validate_number(root, rule) {
            Ok(()) => valid(),
            Err(message) => ValueValidation {
                replacement: None,
                issues: vec![issue(String::new(), message)],
                unresolved: Vec::new(),
            },
        },
        ValueRule::StringArray => validate_value_rule_with_regex(
            value,
            ValueRule::Array(&STRING_RULE),
            regex_rows,
            js_semantics,
        ),
        ValueRule::StringOrStringArray => {
            if root.code_units().is_some() {
                valid()
            } else if root.array_len().is_some() {
                validate_array_with_regex(value, ValueRule::String, regex_rows, js_semantics)
            } else {
                invalid_value("string", root)
            }
        }
        ValueRule::BooleanOrEnumStringArray(options) => {
            if root.as_bool().is_some() {
                valid()
            } else if let Some(length) = root.array_len() {
                let mut items = Vec::new();
                let mut changed = false;
                for index in 0..length {
                    let Ok(item) = value.subprojection(&format!("/{index}")) else {
                        continue;
                    };
                    if item
                        .at_pointer("")
                        .and_then(JsonValueView::as_str)
                        .is_some_and(|entry| options.contains(&entry))
                    {
                        items.push(item);
                    } else {
                        changed = true;
                    }
                }
                if !changed {
                    valid()
                } else {
                    ValueValidation {
                        replacement: Utf16JsonProjection::array(items).ok(),
                        issues: Vec::new(),
                        unresolved: Vec::new(),
                    }
                }
            } else {
                invalid_value("boolean or array", root)
            }
        }
        ValueRule::AutoCompactWindow => {
            if root.as_str() == Some("auto") {
                valid()
            } else if let Some(number) = root.as_number() {
                if number.is_finite()
                    && number.fract() == 0.0
                    && (100_000.0..=1_000_000.0).contains(&number)
                {
                    valid()
                } else {
                    invalid_value("auto or an integer from 100000 to 1000000", root)
                }
            } else {
                invalid_value("auto or an integer from 100000 to 1000000", root)
            }
        }
        ValueRule::Theme => {
            if root.code_units().is_some_and(|units| {
                units.starts_with(&"custom:".encode_utf16().collect::<Vec<_>>())
            }) {
                valid()
            } else if root
                .as_str()
                .is_some_and(|theme| NATIVE_THEME_NAMES.contains(&theme))
            {
                valid()
            } else {
                invalid_value("string", root)
            }
        }
        ValueRule::BooleanOrStringArrayMap => {
            validate_map(value, MapValueRule::BooleanOrStringArray)
        }
        ValueRule::UnknownObjectMap => validate_unknown_object_map(value),
        ValueRule::CoercedStringMap => validate_coerced_string_map(value),
        ValueRule::ModelSettingsMap => validate_model_settings_map(value),
        ValueRule::PluginConfigsMap => validate_plugin_configs_map(value),
        ValueRule::ManagedMcpServersMap => validate_managed_mcp_servers_map(value, js_semantics),
        ValueRule::ModelPricingOverrides => validate_model_pricing_overrides(value),
        ValueRule::McpServerPolicyArray { allowlist } => {
            validate_mcp_server_policy_array(value, allowlist)
        }
        ValueRule::FooterLinksRegexArray => validate_footer_links_array(value),
        ValueRule::SpinnerTipsArray => validate_spinner_tips_array(value),
        ValueRule::MarketplacePolicyArray { field } => {
            if root.array_len().is_none() {
                return invalid_value("array", root);
            }
            let normalized =
                normalize_policy_marketplace_field(field, value, regex_rows, Some(js_semantics));
            if normalized.unresolved {
                unresolved(String::new())
            } else if let Some(replacement) = normalized.normalized {
                let mut report = valid();
                if !replacement.semantically_eq(value) {
                    report.replacement = Some(replacement);
                }
                report
            } else {
                // At has already removed this caught field in production. A
                // missing projection here is therefore an exact catch-unset.
                ValueValidation {
                    replacement: None,
                    issues: vec![issue(
                        String::new(),
                        "Invalid input: expected array, received undefined".to_owned(),
                    )],
                    unresolved: Vec::new(),
                }
            }
        }
        ValueRule::StringMap => validate_map(value, MapValueRule::String),
        ValueRule::SandboxIgnoreViolationsMap => validate_map(value, MapValueRule::StringArray),
        ValueRule::EnumMap(options) => validate_map(value, MapValueRule::Enum(options)),
        ValueRule::Array(item_rule) => {
            validate_array_with_regex(value, *item_rule, regex_rows, js_semantics)
        }
        ValueRule::Object(rule) => {
            validate_object_with_regex(value, rule, regex_rows, js_semantics)
        }
        ValueRule::SandboxMachLookupArray => validate_sandbox_mach_lookup(value),
        ValueRule::SandboxCredentialFileArray => validate_sandbox_credential_array(
            value,
            SandboxCredentialKind::File,
            regex_rows,
            js_semantics,
        ),
        ValueRule::SandboxCredentialEnvArray => validate_sandbox_credential_array(
            value,
            SandboxCredentialKind::Environment,
            regex_rows,
            js_semantics,
        ),
        ValueRule::SandboxAwsPairsArray => validate_sandbox_aws_pairs(value),
        ValueRule::AbsolutePathCatch => {
            let Some(units) = root.code_units() else {
                return invalid_value("string", root);
            };
            if native_node_path_is_absolute(units) {
                valid()
            } else {
                invalid_value("string", root)
            }
        }
        ValueRule::PermissionRulesArray => {
            let Some(len) = root.array_len() else {
                return invalid_value("array", root);
            };
            let mut report = valid();
            for index in 0..len {
                let Some(item) = root.get_index(index) else {
                    continue;
                };
                let Some(units) = item.code_units() else {
                    report
                        .issues
                        .push(issue(index.to_string(), invalid_type("string", item)));
                    continue;
                };
                if units.is_empty() {
                    report.issues.push(issue(
                        index.to_string(),
                        "Permission rule cannot be empty".to_owned(),
                    ));
                } else {
                    report.unresolved.push(String::new());
                }
            }
            report
        }
        ValueRule::EmptyObjectOrUnresolved => match root.object_entries() {
            Some(entries) if entries.is_empty() => valid(),
            Some(_) => unresolved(String::new()),
            None => invalid_value("object", root),
        },
    }
}

#[cfg(unix)]
fn native_node_path_is_absolute(units: &[u16]) -> bool {
    units.first() == Some(&u16::from(b'/'))
}

#[cfg(windows)]
fn native_node_path_is_absolute(units: &[u16]) -> bool {
    let slash = u16::from(b'/');
    let backslash = u16::from(b'\\');
    if units
        .first()
        .is_some_and(|first| *first == slash || *first == backslash)
    {
        return true;
    }
    units.len() >= 3
        && units[0].is_ascii_alphabetic()
        && units[1] == u16::from(b':')
        && (units[2] == slash || units[2] == backslash)
}

#[cfg(not(any(unix, windows)))]
fn native_node_path_is_absolute(units: &[u16]) -> bool {
    units.first() == Some(&u16::from(b'/'))
}

fn invalid_value(expected: &str, value: JsonValueView<'_>) -> ValueValidation {
    ValueValidation {
        replacement: None,
        issues: vec![issue(String::new(), invalid_type(expected, value))],
        unresolved: Vec::new(),
    }
}

fn validate_model_pricing_overrides(value: &Utf16JsonProjection) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(entries) = root.object_entries() else {
        return invalid_value("object", root);
    };
    let mut report = valid();
    let mut output = Vec::with_capacity(entries.len());
    for entry in entries {
        let Ok(row) = value.subprojection_property_units(entry.code_units) else {
            report.unresolved.push("<record-entry>".to_owned());
            continue;
        };
        let row_report = validate_object(&row, &MODEL_PRICING_OVERRIDE_RULE);
        // This rule is invoked for the overrides child; the root validator
        // supplies the `modelPricing` segment once.
        let prefix = vec![entry.code_units.to_vec()];
        report
            .issues
            .extend(row_report.issues.into_iter().map(|issue| RelativeIssue {
                path_segments: join_path_segments(&prefix, &issue.path_segments),
                message_units: issue.message_units,
            }));
        if let Ok(key) = String::from_utf16(entry.code_units) {
            report.unresolved.extend(
                row_report
                    .unresolved
                    .into_iter()
                    .map(|path| join_path(&key, &path)),
            );
        } else if !row_report.unresolved.is_empty() {
            report
                .unresolved
                .push("overrides.<unresolved-source-key>".to_owned());
        }
        output.push((
            entry.code_units.to_vec(),
            row_report.replacement.unwrap_or(row),
        ));
    }
    match Utf16JsonProjection::object_units(output) {
        Ok(normalized) if !normalized.semantically_eq(value) => {
            report.replacement = Some(normalized);
        }
        Err(_) => report.unresolved.push("<overrides>".to_owned()),
        _ => {}
    }
    report
}

fn validate_unknown_object_map(value: &Utf16JsonProjection) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    if root.object_entries().is_some() {
        valid()
    } else {
        invalid_value("object", root)
    }
}

fn validate_managed_mcp_servers_map(
    value: &Utf16JsonProjection,
    js_semantics: &NativePolicyJsSemanticsRegistry,
) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let result = normalize_native_managed_mcp_servers(value, js_semantics);
    let Some(normalized) = result.normalized else {
        return invalid_value("object", root);
    };
    if result.unresolved {
        return unresolved(String::new());
    }
    if normalized.semantically_eq(value) {
        valid()
    } else {
        ValueValidation {
            replacement: Some(normalized),
            issues: Vec::new(),
            unresolved: Vec::new(),
        }
    }
}

fn validate_coerced_string_map(value: &Utf16JsonProjection) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(entries) = root.object_entries() else {
        return invalid_value("object", root);
    };
    let mut report = valid();
    let mut output = Vec::with_capacity(entries.len());
    for entry in entries {
        // ZodRecord omits this property before invoking the value schema.
        if entry.code_units == "__proto__".encode_utf16().collect::<Vec<_>>() {
            continue;
        }
        let Ok(item) = value.subprojection_property_units(entry.code_units) else {
            report.unresolved.push("<env>".to_owned());
            continue;
        };
        let Some(item_root) = item.at_pointer("") else {
            report.unresolved.push("<env-value>".to_owned());
            continue;
        };
        let replacement = if item_root.code_units().is_some() {
            Some(item.clone())
        } else if let Some(boolean) = item_root.as_bool() {
            Some(string_projection(if boolean { "true" } else { "false" }))
        } else if item_root.is_null() {
            Some(string_projection("null"))
        } else if let Some(number) = item_root.as_number() {
            Some(string_projection(&js_number_string(number)))
        } else {
            match native_js_string_units(&item) {
                Ok(units) => Some(string_projection_units(units)),
                Err(NativeStringCoercionError::CannotConvertToPrimitive) => {
                    // Native $ZodString catches String(value)'s TypeError and
                    // validates the unchanged input as a non-string value.
                    report.issues.push(issue_segments(
                        vec![entry.code_units.to_vec()],
                        invalid_type("string", item_root),
                    ));
                    None
                }
                Err(NativeStringCoercionError::ProjectionUnavailable) => {
                    report
                        .unresolved
                        .push("<env-value-source-projection>".to_owned());
                    None
                }
            }
        };
        if let Some(replacement) = replacement {
            output.push((entry.code_units.to_vec(), replacement));
        } else {
            // Native's failed coercion keeps the original value in its issue.
            // An issue or an unavailable projection prevents admission.
            output.push((entry.code_units.to_vec(), item));
        }
    }
    match Utf16JsonProjection::object_units(output) {
        Ok(normalized) if !normalized.semantically_eq(value) => {
            report.replacement = Some(normalized);
        }
        Err(_) => report.unresolved.push("<env>".to_owned()),
        _ => {}
    }
    report
}

pub(super) fn string_projection(value: &str) -> Utf16JsonProjection {
    let units = value.encode_utf16().collect::<Vec<_>>();
    string_projection_units(units)
}

pub(super) fn string_projection_units(units: Vec<u16>) -> Utf16JsonProjection {
    let display = String::from_utf16_lossy(&units);
    Utf16JsonProjection::root_string(display, units)
        .expect("display is the source carrier's lossy UTF-16 rendering")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeStringCoercionError {
    CannotConvertToPrimitive,
    ProjectionUnavailable,
}

/// Native `String(value)` over JSON values. A JSON property cannot contain a
/// callable method; a shadowed `toString` falls through to the ordinary
/// object-valued `valueOf`, so ToPrimitive fails. Arrays join recursively.
fn native_js_string_units(
    value: &Utf16JsonProjection,
) -> Result<Vec<u16>, NativeStringCoercionError> {
    use NativeStringCoercionError::{CannotConvertToPrimitive, ProjectionUnavailable};
    let root = value.at_pointer("").ok_or(ProjectionUnavailable)?;
    if let Some(units) = root.code_units() {
        return Ok(units.to_vec());
    }
    if root.is_null() {
        return Ok("null".encode_utf16().collect());
    }
    if let Some(boolean) = root.as_bool() {
        return Ok(if boolean { "true" } else { "false" }
            .encode_utf16()
            .collect());
    }
    if let Some(number) = root.as_number() {
        return Ok(js_number_string(number).encode_utf16().collect());
    }
    if root.get_property("toString").is_some() {
        return Err(CannotConvertToPrimitive);
    }
    if let Some(length) = root.array_len() {
        let mut units = Vec::new();
        for index in 0..length {
            if index > 0 {
                units.push(u16::from(b','));
            }
            let item = value
                .subprojection(&format!("/{index}"))
                .map_err(|_| ProjectionUnavailable)?;
            let item_root = item.at_pointer("").ok_or(ProjectionUnavailable)?;
            if item_root.is_null() {
                continue;
            }
            units.extend(native_js_string_units(&item)?);
        }
        return Ok(units);
    }
    root.object_entries().ok_or(ProjectionUnavailable)?;
    Ok("[object Object]".encode_utf16().collect())
}

fn validate_model_settings_map(value: &Utf16JsonProjection) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(entries) = root.object_entries() else {
        return invalid_value("object", root);
    };
    let mut report = valid();
    let mut output = Vec::with_capacity(entries.len());
    let mut changed = false;
    for entry in entries {
        if String::from_utf16(entry.code_units)
            .ok()
            .is_some_and(|key| NATIVE_OBJECT_PROTOTYPE_KEYS.contains(&key.as_str()))
        {
            changed = true;
            continue;
        }
        let Ok(row) = value.subprojection_property_units(entry.code_units) else {
            report.unresolved.push("<modelSettings>".to_owned());
            continue;
        };
        let row_report = validate_object(&row, &MODEL_SETTINGS_ROW_RULE);
        if !row_report.issues.is_empty() {
            // Each value uses .catch(undefined); one invalid model row is
            // omitted without invalidating sibling model rows.
            changed = true;
            continue;
        }
        for path in row_report.unresolved {
            let key = String::from_utf16(entry.code_units)
                .unwrap_or_else(|_| "<unrepresentable-key>".to_owned());
            report.unresolved.push(join_path(&key, &path));
        }
        let normalized = row_report.replacement.unwrap_or(row.clone());
        changed |= !normalized.semantically_eq(&row);
        output.push((entry.code_units.to_vec(), normalized));
    }
    if changed {
        match Utf16JsonProjection::object_units(output) {
            Ok(normalized) => report.replacement = Some(normalized),
            Err(_) => report.unresolved.push("<modelSettings>".to_owned()),
        }
    }
    report
}

fn validate_plugin_configs_map(value: &Utf16JsonProjection) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(entries) = root.object_entries() else {
        return invalid_value("object", root);
    };
    let mut report = valid();
    let mut output = Vec::with_capacity(entries.len());
    let mut changed = false;
    for entry in entries {
        if entry.code_units == "__proto__".encode_utf16().collect::<Vec<_>>() {
            changed = true;
            continue;
        }
        let Ok(plugin) = value.subprojection_property_units(entry.code_units) else {
            report.unresolved.push("<record-entry>".to_owned());
            continue;
        };
        let Some(plugin_root) = plugin.at_pointer("") else {
            report.issues.push(issue_segments(
                vec![entry.code_units.to_vec()],
                "Invalid input: expected object, received undefined".to_owned(),
            ));
            continue;
        };
        let Some(plugin_entries) = plugin_root.object_entries() else {
            report.issues.push(issue_segments(
                vec![entry.code_units.to_vec()],
                invalid_type("object", plugin_root),
            ));
            continue;
        };
        let mut plugin_output = Vec::new();
        for field in ["mcpServers", "options"] {
            let Some(field_value) = plugin.subprojection(&format!("/{field}")).ok() else {
                continue;
            };
            let validation = if field == "mcpServers" {
                validate_plugin_config_mcp_servers_map(&field_value)
            } else {
                validate_plugin_config_values_map(&field_value)
            };
            let prefix = vec![entry.code_units.to_vec(), field.encode_utf16().collect()];
            report
                .issues
                .extend(validation.issues.into_iter().map(|child| RelativeIssue {
                    path_segments: join_path_segments(&prefix, &child.path_segments),
                    message_units: child.message_units,
                }));
            if String::from_utf16(entry.code_units).is_ok() {
                report
                    .unresolved
                    .extend(validation.unresolved.into_iter().map(|path| {
                        let plugin_name = String::from_utf16(entry.code_units)
                            .expect("checked UTF-16 plugin key");
                        join_path(&join_path(&plugin_name, field), &path)
                    }));
            } else if !validation.unresolved.is_empty() {
                report.unresolved.push("<unresolved-source-key>".to_owned());
            }
            plugin_output.push((
                field.encode_utf16().collect::<Vec<_>>(),
                validation.replacement.unwrap_or(field_value),
            ));
        }
        for field in plugin_entries {
            if ["mcpServers", "options"].iter().any(|known| {
                field.code_units == known.encode_utf16().collect::<Vec<_>>().as_slice()
            }) {
                continue;
            }
            changed = true;
        }
        let Ok(normalized_plugin) = Utf16JsonProjection::object_units(plugin_output) else {
            report.unresolved.push("<pluginConfigs-row>".to_owned());
            continue;
        };
        changed |= !normalized_plugin.semantically_eq(&plugin);
        output.push((entry.code_units.to_vec(), normalized_plugin));
    }
    if changed {
        match Utf16JsonProjection::object_units(output) {
            Ok(normalized) => report.replacement = Some(normalized),
            Err(_) => report.unresolved.push("<record-output>".to_owned()),
        }
    }
    report
}

/// Native `pluginConfigs.mcpServers` is a nested record: server name to
/// config object to scalar/string-array values. `options` is the sibling
/// flat record and is validated separately.
fn validate_plugin_config_mcp_servers_map(value: &Utf16JsonProjection) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(entries) = root.object_entries() else {
        return invalid_value("object", root);
    };
    let mut report = valid();
    for entry in entries {
        if entry.code_units == "__proto__".encode_utf16().collect::<Vec<_>>() {
            continue;
        }
        let Ok(config) = value.subprojection_property_units(entry.code_units) else {
            report.unresolved.push("<server-entry>".to_owned());
            continue;
        };
        let Some(config_root) = config.at_pointer("") else {
            report.issues.push(issue_segments(
                vec![entry.code_units.to_vec()],
                "Invalid input: expected object, received undefined".to_owned(),
            ));
            continue;
        };
        if config_root.object_entries().is_none() {
            report.issues.push(issue_segments(
                vec![entry.code_units.to_vec()],
                invalid_type("object", config_root),
            ));
            continue;
        }
        let fields = validate_plugin_config_values_map(&config);
        for issue in fields.issues {
            report.issues.push(RelativeIssue {
                path_segments: join_path_segments(
                    &[entry.code_units.to_vec()],
                    &issue.path_segments,
                ),
                message_units: issue.message_units,
            });
        }
        if let Ok(key) = String::from_utf16(entry.code_units) {
            report.unresolved.extend(
                fields
                    .unresolved
                    .into_iter()
                    .map(|path| join_path(&key, &path)),
            );
        } else if !fields.unresolved.is_empty() {
            report.unresolved.push("<unresolved-source-key>".to_owned());
        }
    }
    report
}

fn validate_plugin_config_values_map(value: &Utf16JsonProjection) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(entries) = root.object_entries() else {
        return invalid_value("object", root);
    };
    let mut report = valid();
    for entry in entries {
        if entry.code_units == "__proto__".encode_utf16().collect::<Vec<_>>() {
            continue;
        }
        let Ok(item) = value.subprojection_property_units(entry.code_units) else {
            report.unresolved.push("<plugin-config-values>".to_owned());
            continue;
        };
        let Some(item_root) = item.at_pointer("") else {
            report.unresolved.push("<plugin-config-value>".to_owned());
            continue;
        };
        let valid_item = item_root.code_units().is_some()
            || item_root.as_bool().is_some()
            || item_root.as_number().is_some_and(f64::is_finite)
            || item_root.array_len().is_some_and(|length| {
                (0..length).all(|index| {
                    item_root
                        .get_index(index)
                        .and_then(JsonValueView::code_units)
                        .is_some()
                })
            });
        if !valid_item {
            report.issues.push(issue_segments(
                vec![entry.code_units.to_vec()],
                invalid_type("string, number, boolean, or string array", item_root),
            ));
        }
    }
    report
}

fn validate_mcp_server_policy_array(
    value: &Utf16JsonProjection,
    allowlist: bool,
) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(length) = root.array_len() else {
        return invalid_value("array", root);
    };
    let mut report = valid();
    let mut output = Vec::with_capacity(length);
    for index in 0..length {
        let Ok(item) = value.subprojection(&format!("/{index}")) else {
            report.unresolved.push(index.to_string());
            continue;
        };
        if let Some(normalized) = normalize_mcp_server_entry(&item, allowlist) {
            output.push(normalized);
        } else {
            // In the static At call this row is removed before Due. Retaining
            // an unresolved marker makes direct use on an untransformed row
            // fail closed instead of inventing a Zod diagnostic.
            report.unresolved.push(index.to_string());
            output.push(item);
        }
    }
    match Utf16JsonProjection::array(output) {
        Ok(normalized) if !normalized.semantically_eq(value) => {
            report.replacement = Some(normalized);
        }
        Err(_) => report.unresolved.push("<mcp-server-array>".to_owned()),
        _ => {}
    }
    report
}

fn validate_footer_links_array(value: &Utf16JsonProjection) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(length) = root.array_len() else {
        return invalid_value("array", root);
    };
    for index in 0..length {
        let Some(item) = root.get_index(index) else {
            continue;
        };
        if item.object_entries().is_none()
            || item
                .get_property("type")
                .and_then(JsonValueView::code_units)
                .is_none()
        {
            return invalid_value("object with a string type", item);
        }
    }
    valid()
}

fn validate_spinner_tips_array(value: &Utf16JsonProjection) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(length) = root.array_len() else {
        return invalid_value("array", root);
    };
    for index in 0..length {
        let Some(item) = root.get_index(index) else {
            continue;
        };
        if item.code_units().is_none() && item.object_entries().is_none() {
            return invalid_value("string or object", item);
        }
    }
    valid()
}

#[derive(Clone, Copy)]
enum MapValueRule {
    String,
    StringArray,
    Enum(&'static [&'static str]),
    BooleanOrStringArray,
}

#[derive(Clone, Copy)]
enum SandboxCredentialKind {
    File,
    Environment,
}

fn validate_sandbox_mach_lookup(value: &Utf16JsonProjection) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(length) = root.array_len() else {
        return invalid_value("array", root);
    };
    let mut report = valid();
    for index in 0..length {
        let Ok(item) = value.subprojection(&format!("/{index}")) else {
            report.unresolved.push(format!("{index}"));
            continue;
        };
        let Some(units) = item.at_pointer("").and_then(JsonValueView::code_units) else {
            let Some(item_root) = item.at_pointer("") else {
                report.unresolved.push(format!("{index}"));
                continue;
            };
            report
                .issues
                .push(issue(index.to_string(), invalid_type("string", item_root)));
            continue;
        };
        let wildcard_start = if units.last() == Some(&u16::from(b'*')) {
            units.len().saturating_sub(1)
        } else {
            units.len()
        };
        if units[..wildcard_start].contains(&u16::from(b'*')) {
            report.issues.push(issue(
                index.to_string(),
                "Wildcards are only allowed as a single trailing \"*\" (e.g., \"com.example.*\" or \"*\" for all services).".to_owned(),
            ));
        }
    }
    report
}

fn validate_sandbox_credential_array(
    value: &Utf16JsonProjection,
    kind: SandboxCredentialKind,
    regex_rows: &[NativePolicyRegexValidationRow],
    js_semantics: &NativePolicyJsSemanticsRegistry,
) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(length) = root.array_len() else {
        return invalid_value("array", root);
    };
    let rule = match kind {
        SandboxCredentialKind::File => &SANDBOX_CREDENTIAL_FILE_RULE,
        SandboxCredentialKind::Environment => &SANDBOX_CREDENTIAL_ENV_RULE,
    };
    let mut report = valid();
    let mut normalized_items = Vec::with_capacity(length);
    let mut changed = false;
    for index in 0..length {
        let Ok(original) = value.subprojection(&format!("/{index}")) else {
            report.unresolved.push(index.to_string());
            continue;
        };
        let preprocessed = match preprocess_deny_credential_entry(&original) {
            Ok(entry) => entry,
            Err(()) => {
                report.unresolved.push(index.to_string());
                normalized_items.push(original);
                continue;
            }
        };
        let mut entry_report =
            validate_object_with_regex(&preprocessed, rule, regex_rows, js_semantics);
        if entry_report.issues.is_empty() && entry_report.unresolved.is_empty() {
            validate_credential_entry_refinements(
                &preprocessed,
                kind,
                regex_rows,
                &mut entry_report,
            );
        }
        for entry_issue in entry_report.issues {
            report.issues.push(RelativeIssue {
                path_segments: join_path_segments(
                    &[index.to_string().encode_utf16().collect()],
                    &entry_issue.path_segments,
                ),
                message_units: entry_issue.message_units,
            });
        }
        for unresolved in entry_report.unresolved {
            report
                .unresolved
                .push(join_path(&index.to_string(), &unresolved));
        }
        let normalized = entry_report.replacement.unwrap_or(preprocessed);
        changed |= !normalized.semantically_eq(&original);
        normalized_items.push(normalized);
    }
    if changed {
        match Utf16JsonProjection::array(normalized_items) {
            Ok(normalized) => report.replacement = Some(normalized),
            Err(_) => report
                .unresolved
                .push("<credential-array-projection>".to_owned()),
        }
    }
    report
}

/// Native Fs() removes malformed optional properties from `mode:"deny"`
/// entries before applying the ordinary Zod object schema. `mode:"mask"`
/// entries are left untouched and are validated below.
fn preprocess_deny_credential_entry(
    value: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, ()> {
    let Some(root) = value.at_pointer("") else {
        return Err(());
    };
    let Some(entries) = root.object_entries() else {
        return Ok(value.clone());
    };
    let mode_key = "mode".encode_utf16().collect::<Vec<_>>();
    let mode_projection = value.subprojection_property_units(&mode_key).ok();
    let is_deny = mode_projection
        .as_ref()
        .and_then(|mode| mode.at_pointer(""))
        .and_then(JsonValueView::as_str)
        == Some("deny");
    if !is_deny {
        return Ok(value.clone());
    }

    let mut output = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry.code_units;
        let child = value.subprojection_property_units(name).map_err(|_| ())?;
        let keep = match String::from_utf16(name).ok().as_deref() {
            Some("extract") => child
                .at_pointer("")
                .is_some_and(|child| child.code_units().is_some()),
            Some("onExtractNoMatch") => child
                .at_pointer("")
                .and_then(JsonValueView::as_str)
                .is_some_and(|choice| ["warn", "deny", "error"].contains(&choice)),
            Some("decode") => child.at_pointer("").and_then(JsonValueView::as_str) == Some("jwt"),
            Some("maskClaims" | "injectHosts") => {
                validate_value_rule(&child, ValueRule::StringArray)
                    .issues
                    .is_empty()
            }
            Some("maskDuplicates") => child
                .at_pointer("")
                .is_some_and(|child| child.as_bool().is_some()),
            _ => true,
        };
        if keep {
            output.push((name.to_vec(), child));
        }
    }
    Utf16JsonProjection::object_units(output).map_err(|_| ())
}

fn validate_credential_entry_refinements(
    value: &Utf16JsonProjection,
    kind: SandboxCredentialKind,
    regex_rows: &[NativePolicyRegexValidationRow],
    report: &mut ValueValidation,
) {
    let Some(root) = value.at_pointer("") else {
        report.unresolved.push("<credential-entry>".to_owned());
        return;
    };
    let mode = root.get_property("mode").and_then(JsonValueView::as_str);
    if mode != Some("mask") {
        if matches!(kind, SandboxCredentialKind::Environment) {
            validate_credential_name(root, "name", report);
        }
        return;
    }

    if matches!(kind, SandboxCredentialKind::Environment) {
        validate_credential_name(root, "name", report);
    }

    if matches!(kind, SandboxCredentialKind::File)
        && root
            .get_property("path")
            .and_then(JsonValueView::code_units)
            .is_some_and(|path| path.last() == Some(&u16::from(b'/')))
    {
        report.issues.push(issue(
            "path",
            "Credential mode \"mask\" applies to a single file, not a directory. List the specific credential file(s), or use \"deny\" for the directory.".to_owned(),
        ));
    }

    let extract = root.get_property("extract");
    let has_extract = extract.is_some();
    let has_decode = root.get_property("decode").is_some();
    if let Some(pattern_units) = extract.and_then(JsonValueView::code_units) {
        let mut matching_rows = regex_rows
            .iter()
            .filter(|row| row.matches_pattern(pattern_units));
        match (matching_rows.next(), matching_rows.next()) {
            (None, _) => report
                .unresolved
                .push("extract/js-regexp-result-missing".to_owned()),
            (Some(_), Some(_)) => report
                .unresolved
                .push("extract/js-regexp-result-duplicate".to_owned()),
            (Some(row), None) => match &row.outcome {
                NativePolicyRegexValidationOutcome::Invalid {
                    error_message_units,
                } => {
                    let mut message = "extract is not a valid regular expression: "
                        .encode_utf16()
                        .collect::<Vec<_>>();
                    message.extend_from_slice(error_message_units);
                    report.issues.push(issue_units("extract", message));
                }
                NativePolicyRegexValidationOutcome::Valid { capture_count, .. }
                    if *capture_count < 1 =>
                {
                    report.issues.push(issue(
                        "extract",
                        "extract must contain at least one capturing group — group 1 is the credential value to mask (e.g. \"token:\\s*(\\S+)\").".to_owned(),
                    ));
                }
                NativePolicyRegexValidationOutcome::Valid { .. } => {}
            },
        }
    }

    if let Some(claims) = root.get_property("maskClaims") {
        let empty_or_empty_name = claims.array_len().is_some_and(|length| {
            length == 0
                || (0..length).any(|index| {
                    claims
                        .get_index(index)
                        .and_then(JsonValueView::code_units)
                        .is_some_and(|units| units.is_empty())
                })
        });
        if empty_or_empty_name {
            report.issues.push(issue(
                "maskClaims",
                "maskClaims must name at least one non-empty claim — omit maskClaims for whole-token masking.".to_owned(),
            ));
        }
        if !has_decode {
            report.issues.push(issue(
                "maskClaims",
                "maskClaims requires decode — without a decode format there is no token to read claims from. Set decode, or omit maskClaims.".to_owned(),
            ));
        }
    }

    if matches!(kind, SandboxCredentialKind::Environment) && has_extract && has_decode {
        report.issues.push(issue(
            "extract",
            "extract cannot be combined with decode on an env entry — the runtime takes the decode path (whole-value JWT verification) and never consults extract, silently disabling the structured masking. Remove one of the two.".to_owned(),
        ));
    }
    if matches!(kind, SandboxCredentialKind::Environment)
        && has_decode
        && root
            .get_property("onExtractNoMatch")
            .and_then(JsonValueView::as_str)
            .is_some_and(|value| value == "deny" || value == "error")
    {
        report.issues.push(issue(
            "onExtractNoMatch",
            "onExtractNoMatch cannot be honored on an env entry with decode — the runtime takes the decode path (which is unconditionally fail-open on verify failure) and never consults extract or onExtractNoMatch. Remove onExtractNoMatch, or drop decode to use extract-based masking (whose no-match handling does honor it).".to_owned(),
        ));
    }
}

fn validate_credential_name(root: JsonValueView<'_>, field: &str, report: &mut ValueValidation) {
    let Some(units) = root.get_property(field).and_then(JsonValueView::code_units) else {
        return;
    };
    let valid = units.first().is_some_and(|first| {
        *first == u16::from(b'_')
            || (*first >= u16::from(b'A') && *first <= u16::from(b'Z'))
            || (*first >= u16::from(b'a') && *first <= u16::from(b'z'))
    }) && units.iter().skip(1).all(|unit| {
        *unit == u16::from(b'_')
            || (*unit >= u16::from(b'A') && *unit <= u16::from(b'Z'))
            || (*unit >= u16::from(b'a') && *unit <= u16::from(b'z'))
            || (*unit >= u16::from(b'0') && *unit <= u16::from(b'9'))
    });
    if !valid {
        report.issues.push(issue(
            field,
            "Environment variable name must start with a letter or underscore and contain only letters, digits, and underscores".to_owned(),
        ));
    }
}

fn validate_sandbox_aws_pairs(value: &Utf16JsonProjection) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(length) = root.array_len() else {
        return invalid_value("array", root);
    };
    let mut report = valid();
    let mut normalized_items = Vec::with_capacity(length);
    let mut any_replacement = false;
    let mut seen = std::collections::HashMap::<Vec<u16>, (usize, &'static str)>::new();

    for index in 0..length {
        let Ok(item) = value.subprojection(&format!("/{index}")) else {
            report.unresolved.push(index.to_string());
            continue;
        };
        let mut row = validate_object(&item, &SANDBOX_AWS_PAIR_RULE);
        let Some(row_root) = row.replacement.as_ref().unwrap_or(&item).at_pointer("") else {
            report.unresolved.push(index.to_string());
            normalized_items.push(item);
            continue;
        };
        if row.issues.is_empty() && row.unresolved.is_empty() {
            let fields = ["accessKeyIdVar", "secretAccessKeyVar", "sessionTokenVar"];
            let mut within = std::collections::HashMap::<Vec<u16>, &'static str>::new();
            let mut row_names = Vec::<(Vec<u16>, &'static str)>::new();
            for field in fields {
                let Some(units) = row_root
                    .get_property(field)
                    .and_then(JsonValueView::code_units)
                else {
                    continue;
                };
                if !is_native_credential_env_name(units) {
                    row.issues.push(issue(
                        field,
                        "Environment variable name must start with a letter or underscore and contain only letters, digits, and underscores".to_owned(),
                    ));
                    continue;
                }
                let key = units.to_vec();
                if let Some(previous) = within.get(&key) {
                    row.issues.push(issue(
                        field,
                        format!(
                            "{field} names the same env var ('{}') as {previous} — each pair member must be a distinct variable.",
                            String::from_utf16(units).unwrap_or_default()
                        ),
                    ));
                } else {
                    within.insert(key.clone(), field);
                    row_names.push((key, field));
                }
            }
            if row.issues.is_empty() {
                for (key, field) in &row_names {
                    if seen.contains_key(key) {
                        let units = row_root
                            .get_property(field)
                            .and_then(JsonValueView::code_units)
                            .unwrap_or_default();
                        row.issues.push(issue(
                            *field,
                            format!(
                                "\"{}\" appears in more than one awsPairs slot (within or across pairs) — each variable can fill exactly one slot.",
                                String::from_utf16(units).unwrap_or_default()
                            ),
                        ));
                    }
                }
            }
            if row.issues.is_empty() {
                for (key, field) in row_names {
                    seen.insert(key, (index, field));
                }
            }
        }
        for row_issue in row.issues {
            report.issues.push(RelativeIssue {
                path_segments: join_path_segments(
                    &[index.to_string().encode_utf16().collect()],
                    &row_issue.path_segments,
                ),
                message_units: row_issue.message_units,
            });
        }
        for unresolved in row.unresolved {
            report
                .unresolved
                .push(join_path(&index.to_string(), &unresolved));
        }
        let normalized = row.replacement.unwrap_or(item.clone());
        any_replacement |= !normalized.semantically_eq(&item);
        normalized_items.push(normalized);
    }
    if any_replacement {
        match Utf16JsonProjection::array(normalized_items) {
            Ok(normalized) => report.replacement = Some(normalized),
            Err(_) => report.unresolved.push("<aws-pairs-projection>".to_owned()),
        }
    }
    report
}

fn is_native_credential_env_name(units: &[u16]) -> bool {
    let Some(first) = units.first() else {
        return false;
    };
    let is_letter = |unit: u16| {
        (unit >= u16::from(b'A') && unit <= u16::from(b'Z'))
            || (unit >= u16::from(b'a') && unit <= u16::from(b'z'))
    };
    (*first == u16::from(b'_') || is_letter(*first))
        && units.iter().skip(1).all(|unit| {
            *unit == u16::from(b'_')
                || is_letter(*unit)
                || (u16::from(b'0')..=u16::from(b'9')).contains(unit)
        })
}

fn validate_map(value: &Utf16JsonProjection, rule: MapValueRule) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(entries) = root.object_entries() else {
        return invalid_value("object", root);
    };
    let mut report = valid();
    let mut output_fields = Vec::with_capacity(entries.len());
    let mut changed = false;
    for entry in entries {
        if entry.code_units == "__proto__".encode_utf16().collect::<Vec<_>>().as_slice() {
            // ZodRecord's object accumulator explicitly skips this key.
            changed = true;
            continue;
        }
        let Ok(child) = value.subprojection_property_units(entry.code_units) else {
            report
                .unresolved
                .push("<record-value-projection>".to_owned());
            continue;
        };
        let original_child = child.clone();
        let child_report = match rule {
            MapValueRule::String => validate_value_rule(&child, ValueRule::String),
            MapValueRule::StringArray => validate_value_rule(&child, ValueRule::StringArray),
            MapValueRule::Enum(options) => validate_value_rule(&child, ValueRule::Enum(options)),
            MapValueRule::BooleanOrStringArray => {
                let Some(child_root) = child.at_pointer("") else {
                    continue;
                };
                if child_root.as_bool().is_some() {
                    valid()
                } else if child_root.array_len().is_some() {
                    validate_array(&child, ValueRule::String)
                } else {
                    invalid_value("boolean or string array", child_root)
                }
            }
        };
        let key_prefix = vec![entry.code_units.to_vec()];
        for child_issue in child_report.issues {
            report.issues.push(RelativeIssue {
                path_segments: join_path_segments(&key_prefix, &child_issue.path_segments),
                message_units: child_issue.message_units,
            });
        }
        if let Ok(key) = String::from_utf16(entry.code_units) {
            for child_path in child_report.unresolved {
                report.unresolved.push(join_path(&key, &child_path));
            }
        } else if !child_report.unresolved.is_empty() {
            report.unresolved.push("<unresolved-map-key>".to_owned());
        }
        let normalized = child_report.replacement.unwrap_or(child);
        changed |= !normalized.semantically_eq(&original_child);
        output_fields.push((entry.code_units.to_vec(), normalized));
    }
    if changed {
        match Utf16JsonProjection::object_units(output_fields) {
            Ok(normalized) => report.replacement = Some(normalized),
            Err(_) => report
                .unresolved
                .push("<record-output-projection>".to_owned()),
        }
    }
    report
}

fn validate_array(value: &Utf16JsonProjection, item_rule: ValueRule) -> ValueValidation {
    validate_array_with_regex(value, item_rule, &[], &empty_js_semantics())
}

fn validate_array_with_regex(
    value: &Utf16JsonProjection,
    item_rule: ValueRule,
    regex_rows: &[NativePolicyRegexValidationRow],
    js_semantics: &NativePolicyJsSemanticsRegistry,
) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(len) = root.array_len() else {
        return invalid_value("array", root);
    };
    let mut report = valid();
    let mut normalized_items = Vec::with_capacity(len);
    let mut changed = false;
    for index in 0..len {
        let Ok(item) = value.subprojection(&format!("/{index}")) else {
            continue;
        };
        let item_report =
            validate_value_rule_with_regex(&item, item_rule, regex_rows, js_semantics);
        let prefix = vec![index.to_string().encode_utf16().collect()];
        for item_issue in item_report.issues {
            report.issues.push(RelativeIssue {
                path_segments: join_path_segments(&prefix, &item_issue.path_segments),
                message_units: item_issue.message_units,
            });
        }
        for item_path in item_report.unresolved {
            report
                .unresolved
                .push(join_path(&index.to_string(), &item_path));
        }
        if let Some(replacement) = item_report.replacement {
            normalized_items.push(replacement);
            changed = true;
        } else {
            normalized_items.push(item);
        }
    }
    if changed {
        match Utf16JsonProjection::array(normalized_items) {
            Ok(replacement) => report.replacement = Some(replacement),
            Err(_) => report.unresolved.push("<array-projection>".to_owned()),
        }
    }
    report
}

fn validate_object(value: &Utf16JsonProjection, rule: &ObjectRule) -> ValueValidation {
    validate_object_with_regex(value, rule, &[], &empty_js_semantics())
}

fn validate_object_with_regex(
    value: &Utf16JsonProjection,
    rule: &ObjectRule,
    regex_rows: &[NativePolicyRegexValidationRow],
    js_semantics: &NativePolicyJsSemanticsRegistry,
) -> ValueValidation {
    let Some(root) = value.at_pointer("") else {
        return invalid_root_missing();
    };
    let Some(entries) = root.object_entries() else {
        return invalid_value("object", root);
    };
    let mut report = valid();
    let mut output = Vec::with_capacity(entries.len());
    let mut recognized_units = Vec::with_capacity(rule.fields.len());

    for field in rule.fields {
        let key_units = field.name.encode_utf16().collect::<Vec<_>>();
        recognized_units.push(key_units.clone());
        let Some(child_view) = root.get_property(field.name) else {
            if field.required {
                report
                    .issues
                    .push(issue(field.name, missing_value_message(field.rule)));
            }
            continue;
        };
        let Ok(child) = value.subprojection_property_units(&key_units) else {
            report.unresolved.push(field.name.to_owned());
            continue;
        };
        let child_report =
            validate_value_rule_with_regex(&child, field.rule, regex_rows, js_semantics);
        if !child_report.issues.is_empty() {
            match field.invalid_behavior {
                InvalidBehavior::CatchUnset => continue,
                InvalidBehavior::CatchValue(_) => continue,
                InvalidBehavior::Reject => {
                    report
                        .issues
                        .extend(child_report.issues.into_iter().map(|issue| RelativeIssue {
                            path_segments: prepend_path_segment(
                                field.name.encode_utf16().collect(),
                                issue.path_segments,
                            ),
                            message_units: issue.message_units,
                        }));
                }
            }
        }
        for path in child_report.unresolved {
            report.unresolved.push(join_path(field.name, &path));
        }
        let child_projection = child_report.replacement.unwrap_or(child);
        output.push((key_units, child_projection));
        let _ = child_view;
    }

    for entry in entries {
        if recognized_units
            .iter()
            .any(|key_units| *key_units == entry.code_units)
        {
            continue;
        }
        match rule.unknown_fields {
            UnknownFields::Strip => {}
            UnknownFields::Preserve => {
                if rule.unknown_fields_unresolved {
                    let key = String::from_utf16(entry.code_units)
                        .unwrap_or_else(|_| "<unrepresentable-key>".to_owned());
                    report.unresolved.push(key);
                }
                if let Ok(child) = value.subprojection_property_units(entry.code_units) {
                    output.push((entry.code_units.to_vec(), child));
                }
            }
        }
    }

    let Ok(normalized) = Utf16JsonProjection::object_units(output) else {
        report.unresolved.push("<object-projection>".to_owned());
        return report;
    };
    if !normalized.semantically_eq(value) {
        report.replacement = Some(normalized);
    }
    report
}

fn validate_attribution(value: &Utf16JsonProjection, root: JsonValueView<'_>) -> ValueValidation {
    if let Some(enabled) = root.as_bool() {
        let normalized = if enabled {
            Utf16JsonProjection::parse("{}").expect("static JSON object is valid")
        } else {
            Utf16JsonProjection::parse(r#"{"commit":"","pr":"","sessionUrl":false}"#)
                .expect("static JSON object is valid")
        };
        return ValueValidation {
            replacement: Some(normalized),
            issues: Vec::new(),
            unresolved: Vec::new(),
        };
    }
    if root.object_entries().is_some() {
        return validate_object(value, &ATTRIBUTION_RULE);
    }
    let received = if root.array_len().is_some() {
        "array"
    } else if root.is_null() {
        "null"
    } else if root.code_units().is_some() {
        "string"
    } else if root.as_number().is_some() {
        "number"
    } else {
        "object"
    };
    ValueValidation {
        replacement: None,
        issues: vec![issue(
            String::new(),
            format!(
                "Expected false, true, or an object such as {{ \"commit\": \"\", \"pr\": \"\" }}, but received {received}"
            ),
        )],
        unresolved: Vec::new(),
    }
}

fn invalid_root_missing() -> ValueValidation {
    ValueValidation {
        replacement: None,
        issues: vec![issue(
            String::new(),
            "Invalid input: expected value, received undefined".to_owned(),
        )],
        unresolved: Vec::new(),
    }
}

fn join_path(parent: &str, child: &str) -> String {
    match (parent.is_empty(), child.is_empty()) {
        (true, true) => String::new(),
        (true, false) => child.to_owned(),
        (false, true) => parent.to_owned(),
        (false, false) => format!("{parent}.{child}"),
    }
}

fn missing_value_message(rule: ValueRule) -> String {
    match rule {
        ValueRule::String | ValueRule::NonEmptyString => {
            "Invalid input: expected string, received undefined".to_owned()
        }
        ValueRule::Boolean => "Invalid input: expected boolean, received undefined".to_owned(),
        ValueRule::Number(_) => "Invalid input: expected number, received undefined".to_owned(),
        ValueRule::Array(_)
        | ValueRule::StringArray
        | ValueRule::SandboxMachLookupArray
        | ValueRule::SandboxCredentialFileArray
        | ValueRule::SandboxCredentialEnvArray
        | ValueRule::SandboxAwsPairsArray
        | ValueRule::PermissionRulesArray
        | ValueRule::FooterLinksRegexArray
        | ValueRule::SpinnerTipsArray
        | ValueRule::McpServerPolicyArray { .. }
        | ValueRule::MarketplacePolicyArray { .. } => {
            "Invalid input: expected array, received undefined".to_owned()
        }
        ValueRule::Object(_)
        | ValueRule::StringMap
        | ValueRule::SandboxIgnoreViolationsMap
        | ValueRule::BooleanOrStringArrayMap
        | ValueRule::UnknownObjectMap
        | ValueRule::CoercedStringMap
        | ValueRule::ModelSettingsMap
        | ValueRule::PluginConfigsMap
        | ValueRule::ManagedMcpServersMap
        | ValueRule::ModelPricingOverrides => {
            "Invalid input: expected object, received undefined".to_owned()
        }
        ValueRule::Enum(options) | ValueRule::EnumMap(options) => enum_error(options),
        ValueRule::StringOrStringArray => "Invalid input: expected string or array".to_owned(),
        ValueRule::Literal(expected) => format!("Invalid input: expected {expected:?}"),
        ValueRule::TimeOfDay(message) => message.to_owned(),
        // Both are Native Zod unions. A missing value fails every union
        // branch, whose default Zod union message is the generic text.
        ValueRule::AutoCompactWindow | ValueRule::Theme => "Invalid input".to_owned(),
        ValueRule::Attribution => "Expected false, true, or an object".to_owned(),
        ValueRule::BooleanOrEnumStringArray(_) => {
            "Invalid input: expected boolean or array".to_owned()
        }
        ValueRule::EmptyObjectOrUnresolved => {
            "Invalid input: expected object, received undefined".to_owned()
        }
        ValueRule::AbsolutePathCatch => {
            "Invalid input: expected string, received undefined".to_owned()
        }
    }
}

fn enum_error(options: &[&str]) -> String {
    format!(
        "Invalid option: expected one of {}",
        options
            .iter()
            .map(|option| format!("\"{option}\""))
            .collect::<Vec<_>>()
            .join("|")
    )
}

fn is_native_time_of_day(value: &str) -> bool {
    let Some((hour, minute)) = value.split_once(':') else {
        return false;
    };
    if hour.is_empty()
        || hour.len() > 2
        || !hour.bytes().all(|byte| byte.is_ascii_digit())
        || minute.len() != 2
        || !minute.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    let Ok(hour) = hour.parse::<u8>() else {
        return false;
    };
    let Ok(minute) = minute.parse::<u8>() else {
        return false;
    };
    hour <= 23 && minute <= 59
}

fn validate_number(value: JsonValueView<'_>, rule: NumberRule) -> Result<(), String> {
    let Some(number) = value.as_number() else {
        return Err(invalid_type("number", value));
    };
    if !number.is_finite() {
        return Err(format!(
            "Invalid input: expected number, received {}",
            js_number_string(number)
        ));
    }
    match rule {
        NumberRule::Finite => Ok(()),
        NumberRule::Minimum(min) => {
            if number < min {
                Err(format!("Too small: expected number to be >={min}"))
            } else {
                Ok(())
            }
        }
        NumberRule::InclusiveRange { min, max } => {
            if number < min {
                Err(format!("Too small: expected number to be >={min}"))
            } else if number > max {
                Err(format!("Too big: expected number to be <={max}"))
            } else {
                Ok(())
            }
        }
        NumberRule::GreaterThanZeroAtMostOne => {
            if number <= 0.0 {
                Err("Too small: expected number to be >0".to_owned())
            } else if number > 1.0 {
                Err("Too big: expected number to be <=1".to_owned())
            } else {
                Ok(())
            }
        }
        NumberRule::GreaterThanZeroAtMostTen => {
            if number <= 0.0 {
                Err("Too small: expected number to be >0".to_owned())
            } else if number > 10.0 {
                Err("Too big: expected number to be <=10".to_owned())
            } else {
                Ok(())
            }
        }
        NumberRule::PositiveInteger => validate_integer(number, true, None),
        NumberRule::NonnegativeInteger => validate_integer(number, false, Some(0.0)),
        NumberRule::IntegerAtLeast(min) => validate_integer(number, false, Some(f64::from(min))),
        NumberRule::Integer => validate_integer(number, false, None),
    }
}

fn validate_integer(number: f64, positive: bool, minimum: Option<f64>) -> Result<(), String> {
    if number.fract() != 0.0 {
        return Err("Invalid input: expected int, received number".to_owned());
    }
    if number.abs() > 9_007_199_254_740_991.0 {
        return if number > 0.0 {
            Err("Too big: expected int to be <=9007199254740991".to_owned())
        } else {
            Err("Too small: expected int to be >=-9007199254740991".to_owned())
        };
    }
    if positive && number <= 0.0 {
        return Err("Too small: expected number to be >0".to_owned());
    }
    if let Some(minimum) = minimum {
        if number < minimum {
            return Err(format!("Too small: expected number to be >={minimum}"));
        }
    }
    Ok(())
}

fn invalid_type(expected: &str, value: JsonValueView<'_>) -> String {
    format!(
        "Invalid input: expected {expected}, received {}",
        received_type(value)
    )
}

fn received_type(value: JsonValueView<'_>) -> String {
    if value.is_null() {
        "null".to_owned()
    } else if value.array_len().is_some() {
        "array".to_owned()
    } else if value.code_units().is_some() {
        "string".to_owned()
    } else if value.as_bool().is_some() {
        "boolean".to_owned()
    } else if let Some(number) = value.as_number() {
        if number.is_nan() {
            "NaN".to_owned()
        } else if number == f64::INFINITY {
            "Infinity".to_owned()
        } else if number == f64::NEG_INFINITY {
            "-Infinity".to_owned()
        } else {
            "number".to_owned()
        }
    } else {
        "object".to_owned()
    }
}

fn js_number_string(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value == f64::INFINITY {
        "Infinity".to_owned()
    } else if value == f64::NEG_INFINITY {
        "-Infinity".to_owned()
    } else if value == 0.0 {
        "0".to_owned()
    } else {
        ryu_js::Buffer::new().format_finite(value).to_owned()
    }
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NativePolicyRegexValidationOutcome, NativePolicyRegexValidationRow,
        NativeStrictSettingsStatus, NATIVE_DUE_FIELD_ORDER,
    };
    use crate::settings::native_policy_js_semantics::NativePolicyJsSemanticsRegistry;
    use crate::types::utf16_json::Utf16JsonProjection;
    use std::collections::HashSet;

    fn parse(source: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(source).expect("fixture is valid JSON")
    }

    fn validate(source: &Utf16JsonProjection) -> super::NativeStrictSettingsResult {
        let semantics = NativePolicyJsSemanticsRegistry::from_rows(Vec::new(), Vec::new()).unwrap();
        super::validate_native_strict_settings(source, &[], &semantics)
    }

    fn validate_with_regex(
        source: &Utf16JsonProjection,
        rows: &[NativePolicyRegexValidationRow],
    ) -> super::NativeStrictSettingsResult {
        let semantics = NativePolicyJsSemanticsRegistry::from_rows(Vec::new(), Vec::new()).unwrap();
        super::validate_native_strict_settings(source, rows, &semantics)
    }

    #[test]
    fn source_index_has_190_unique_due_fields_in_native_order() {
        let unique = NATIVE_DUE_FIELD_ORDER
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        assert_eq!(NATIVE_DUE_FIELD_ORDER.len(), 190);
        assert_eq!(unique.len(), 190);
        assert_eq!(NATIVE_DUE_FIELD_ORDER[0], "$schema");
        assert_eq!(NATIVE_DUE_FIELD_ORDER[183], "skipAutoPermissionPrompt");
        assert_eq!(NATIVE_DUE_FIELD_ORDER[189], "axScreenReader");
    }

    #[test]
    fn simple_static_settings_validate_and_empty_objects_are_admitted() {
        let valid = parse(
            r#"{"model":"claude-sonnet","outputStyle":"concise","language":"Japanese","disableAllHooks":true,"disableBundledSkills":false,"fastModePerSessionOptIn":true,"permissions":{},"sandbox":{},"hooks":{}}"#,
        );
        let result = validate(&valid);
        assert_eq!(result.status, NativeStrictSettingsStatus::Valid);
        assert!(result.issues.is_empty());
        assert!(result.unresolved_fields.is_empty());

        let empty = validate(&parse("{}"));
        assert_eq!(empty.status, NativeStrictSettingsStatus::Valid);
        assert!(empty.issues.is_empty());

        // Due's object schema is passthrough, so unknown keys remain valid.
        let unknown = validate(&parse(r#"{"futureSetting":{}}"#));
        assert_eq!(unknown.status, NativeStrictSettingsStatus::Valid);
    }

    #[test]
    fn native_schema_order_controls_first_three_static_payload_issues() {
        let result = validate(&parse(
            r#"{"language":17,"outputStyle":null,"model":false,"disableAllHooks":"true"}"#,
        ));
        assert_eq!(result.status, NativeStrictSettingsStatus::Invalid);
        assert_eq!(
            result
                .issues
                .iter()
                .map(|issue| {
                    (
                        issue.path.string_at("").expect("root string path"),
                        issue.message.string_at("").expect("root string message"),
                    )
                })
                .collect::<Vec<_>>(),
            [
                ("model", "Invalid input: expected string, received boolean"),
                (
                    "disableAllHooks",
                    "Invalid input: expected boolean, received string"
                ),
                (
                    "outputStyle",
                    "Invalid input: expected string, received null"
                ),
            ]
        );
        assert_eq!(result.issues.len(), 3);
    }

    #[test]
    fn native_catch_unset_fields_are_removed_and_simple_permission_fallback_is_valid() {
        let result = validate(&parse(
            r#"{"askUserQuestionTimeout":"invalid","maxProseWidth":20,"language":"en","permissions":{"allow":[]}}"#,
        ));
        assert_eq!(result.status, NativeStrictSettingsStatus::Valid);
        assert!(result.issues.is_empty());
        assert!(result
            .projection
            .at_pointer("/askUserQuestionTimeout")
            .is_none());
        assert!(result.projection.at_pointer("/maxProseWidth").is_none());
        assert_eq!(result.projection.string_at("/language"), Some("en"));
        assert!(result.unresolved_fields.is_empty());
        assert_eq!(result.projection.array_len("/permissions/allow"), Some(0));
    }

    #[test]
    fn helper_static_fallback_common_settings_validate_as_one_due_payload() {
        let result = validate(&parse(
            r#"{"model":"fallback","language":"ja","disableAllHooks":false,"permissions":{"allow":[],"deny":[],"defaultMode":"default","additionalDirectories":["/repo"],"disableBypassPermissionsMode":"disable"},"worktree":{"symlinkDirectories":["node_modules"],"baseRef":"head","bgIsolation":"unknown","unknown":"stripped"}}"#,
        ));
        assert_eq!(result.status, NativeStrictSettingsStatus::Valid);
        assert!(result.issues.is_empty());
        assert!(result.unresolved_fields.is_empty());
        assert!(result
            .projection
            .at_pointer("/worktree/bgIsolation")
            .is_none());
        assert!(result.projection.at_pointer("/worktree/unknown").is_none());
        assert_eq!(result.projection.string_at("/model"), Some("fallback"));
        assert_eq!(
            result.projection.string_at("/permissions/defaultMode"),
            Some("default")
        );
        assert_eq!(
            result
                .projection
                .string_at("/permissions/additionalDirectories/0"),
            Some("/repo")
        );
    }

    #[test]
    fn only_unported_permission_rule_syntax_stays_unresolved() {
        let result = validate(&parse(
            r#"{"model":"fallback","permissions":{"allow":["Read"],"additionalDirectories":["/repo"]}}"#,
        ));
        assert_eq!(result.status, NativeStrictSettingsStatus::Unresolved);
        assert_eq!(result.unresolved_fields, vec!["permissions.allow"]);
        assert!(result.issues.is_empty());
        assert_eq!(result.projection.string_at("/model"), Some("fallback"));
    }

    #[test]
    fn source_transform_and_nested_object_schema_are_applied() {
        let transformed = validate(&parse(
            r#"{"attribution":false,"quietHours":{"enabled":true,"start":"22:30","end":"07:15","extra":true},"autoMode":{"classifyAllShell":false,"allow":["$defaults"],"extra":true},"defaultView":"transcript","skipAutoPermissionPrompt":true}"#,
        ));
        assert_eq!(transformed.status, NativeStrictSettingsStatus::Valid);
        assert!(transformed.issues.is_empty());
        assert_eq!(
            transformed.projection.string_at("/attribution/commit"),
            Some("")
        );
        assert_eq!(
            transformed.projection.string_at("/attribution/pr"),
            Some("")
        );
        assert_eq!(
            transformed
                .projection
                .at_pointer("/attribution/sessionUrl")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
        assert!(transformed
            .projection
            .at_pointer("/quietHours/extra")
            .is_none());
        assert!(transformed
            .projection
            .at_pointer("/autoMode/extra")
            .is_none());
    }

    #[test]
    fn numeric_boundaries_and_utf16_strings_are_kept_source_faithful() {
        let valid = validate(&parse(
            r#"{"skillListingBudgetFraction":0.01,"skillListingMaxDescChars":1536,"feedbackSurveyRate":1,"outputStyle":"x\ud800y"}"#,
        ));
        assert_eq!(valid.status, NativeStrictSettingsStatus::Valid);
        assert_eq!(
            valid.projection.string_units("/outputStyle"),
            Some(vec![b'x' as u16, 0xd800, b'y' as u16])
        );

        let invalid = validate(&parse(
            r#"{"skillListingBudgetFraction":0,"skillListingMaxDescChars":0,"feedbackSurveyRate":1.1}"#,
        ));
        assert_eq!(invalid.status, NativeStrictSettingsStatus::Invalid);
        assert_eq!(invalid.issues.len(), 3);
        assert_eq!(
            invalid.issues[0].path.string_at(""),
            Some("skillListingMaxDescChars")
        );
        assert_eq!(
            invalid.issues[1].path.string_at(""),
            Some("skillListingBudgetFraction")
        );
        assert_eq!(
            invalid.issues[2].path.string_at(""),
            Some("feedbackSurveyRate")
        );
    }

    #[test]
    fn model_pricing_footer_spinner_and_strict_plugin_shapes_are_normalized() {
        let result = validate(&parse(
            r#"{"modelPricing":{"multiplier":1.2,"overrides":{"model":{"input":1,"output":2,"cacheRead":0.4,"cacheWrite":3,"extra":true}},"extra":true},"footerLinksRegexes":[{"type":"regex","pattern":"(?<id>.*)","url":"https://example.test/{id}","extra":true},{"type":"future","opaque":1}],"spinnerTipsOverride":{"tips":["tip",{"text":"other","priority":2}],"label":"Tip","vendor":{"keep":true}},"strictPluginOnlyCustomization":["skills","future",17,"mcp"]}"#,
        ));
        assert_eq!(result.status, NativeStrictSettingsStatus::Valid);
        assert!(result.issues.is_empty());
        assert!(result.unresolved_fields.is_empty());
        assert!(result
            .projection
            .at_pointer("/modelPricing/extra")
            .is_none());
        assert!(result
            .projection
            .at_pointer("/modelPricing/overrides/model/extra")
            .is_none());
        assert!(result
            .projection
            .at_pointer("/footerLinksRegexes/0/extra")
            .is_some());
        assert!(result
            .projection
            .at_pointer("/spinnerTipsOverride/vendor/keep")
            .is_some());
        assert_eq!(
            result
                .projection
                .array_len("/strictPluginOnlyCustomization"),
            Some(2)
        );
        assert_eq!(
            result
                .projection
                .string_at("/strictPluginOnlyCustomization/1"),
            Some("mcp")
        );
    }

    #[test]
    fn enabled_plugins_schema_rejects_object_values_and_model_pricing_rejects_bad_rates() {
        let plugin = validate(&parse(r#"{"enabledPlugins":{"a@local":{"enabled":true}}}"#));
        assert_eq!(plugin.status, NativeStrictSettingsStatus::Invalid);
        assert_eq!(
            plugin.issues[0].path.string_at(""),
            Some("enabledPlugins.a@local")
        );

        let pricing = validate(&parse(
            r#"{"modelPricing":{"multiplier":11,"overrides":{"model":{"input":0,"output":1,"cacheRead":2,"cacheWrite":10001}}}}"#,
        ));
        assert_eq!(pricing.status, NativeStrictSettingsStatus::Invalid);
        assert!(pricing
            .issues
            .iter()
            .any(|issue| issue.path.string_at("") == Some("modelPricing.multiplier")));
        assert!(pricing.issues.iter().any(|issue| {
            issue.path.string_at("") == Some("modelPricing.overrides.model.cacheWrite")
        }));
    }

    #[test]
    fn imported_ui_enum_model_and_plugin_config_schemas_are_source_shaped() {
        let valid = validate(&parse(
            r#"{"editorMode":"vim","preferredNotifChannel":"kitty","theme":"dark","vimInsertModeRemaps":{"jj":"<Esc>","opaque":{"keep":true}},"modelSettings":{"claude-sonnet-4-6":{"autoCompactWindow":"auto","effortLevel":"high","custom":"kept"},"bad":{"autoCompactWindow":17}},"pluginConfigs":{"review@local":{"mcpServers":{"server":{"token":"x","count":2,"flag":true,"tags":["a"]}},"options":{"mode":"strict","limit":4},"unknown":true}}}"#,
        ));
        assert_eq!(valid.status, NativeStrictSettingsStatus::Valid);
        assert!(valid.issues.is_empty());
        assert!(valid.unresolved_fields.is_empty());
        assert_eq!(valid.projection.string_at("/editorMode"), Some("vim"));
        assert_eq!(
            valid.projection.string_at("/preferredNotifChannel"),
            Some("kitty")
        );
        assert_eq!(
            valid
                .projection
                .string_at("/modelSettings/claude-sonnet-4-6/effortLevel"),
            Some("high")
        );
        assert!(valid.projection.at_pointer("/modelSettings/bad").is_none());
        assert!(valid
            .projection
            .at_pointer("/pluginConfigs/review@local/unknown")
            .is_none());
        assert_eq!(
            valid
                .projection
                .string_at("/pluginConfigs/review@local/options/mode"),
            Some("strict")
        );
    }

    #[test]
    fn sandbox_qjn_nested_schemas_validate_and_strip_only_nested_unknown_keys() {
        let result = validate(&parse(
            r#"{"sandbox":{"enabled":true,"network":{"allowedDomains":["api.example.test"],"allowMachLookup":["com.apple.*"],"httpProxyPort":8080,"nestedUnknown":true},"filesystem":{"allowWrite":["/tmp"],"disabled":false,"nestedUnknown":1},"credentials":{"files":[{"path":"/tmp/token","mode":"deny","unknown":true}],"envVars":[{"name":"API_TOKEN","mode":"deny"}],"allowPlaintextInject":false,"awsPairs":[{"accessKeyIdVar":"AWS_ACCESS_KEY_ID","secretAccessKeyVar":"AWS_SECRET_ACCESS_KEY"}],"sigv4":{"streaming":"deny","unknown":true},"nestedUnknown":true},"ignoreViolations":{"Bash":["/tmp/cache"]},"excludedCommands":["git status"],"ripgrep":{"command":"rg","args":["--hidden"],"unknown":true},"bwrapPath":"relative/path","futureSandboxOption":{"kept":true}}}"#,
        ));
        assert_eq!(result.status, NativeStrictSettingsStatus::Valid);
        assert!(result.issues.is_empty());
        assert!(result.unresolved_fields.is_empty());
        assert_eq!(
            result
                .projection
                .string_at("/sandbox/network/allowedDomains/0"),
            Some("api.example.test")
        );
        assert!(result
            .projection
            .at_pointer("/sandbox/network/nestedUnknown")
            .is_none());
        assert!(result
            .projection
            .at_pointer("/sandbox/credentials/files/0/unknown")
            .is_none());
        assert!(result.projection.at_pointer("/sandbox/bwrapPath").is_none());
        assert!(result
            .projection
            .at_pointer("/sandbox/futureSandboxOption/kept")
            .is_some());
    }

    #[test]
    fn sandbox_mach_lookup_refinement_and_credential_constraints_are_preserved() {
        let mach = validate(&parse(
            r#"{"sandbox":{"network":{"allowMachLookup":["com.example.*.bad"]}}}"#,
        ));
        assert_eq!(mach.status, NativeStrictSettingsStatus::Invalid);
        assert_eq!(
            mach.issues[0].path.string_at(""),
            Some("sandbox.network.allowMachLookup.0")
        );
        assert_eq!(
            mach.issues[0].message.string_at(""),
            Some("Wildcards are only allowed as a single trailing \"*\" (e.g., \"com.example.*\" or \"*\" for all services).")
        );

        let mask_claims = validate(&parse(
            r#"{"sandbox":{"credentials":{"files":[{"path":"/tmp/","mode":"mask","maskClaims":[]}]}}}"#,
        ));
        assert_eq!(mask_claims.status, NativeStrictSettingsStatus::Invalid);
        assert!(mask_claims
            .issues
            .iter()
            .any(|issue| issue.path.string_at("") == Some("sandbox.credentials.files.0.path")));
        assert!(mask_claims.issues.iter().any(|issue| {
            issue.path.string_at("") == Some("sandbox.credentials.files.0.maskClaims")
        }));

        let regexp = validate(&parse(
            r#"{"sandbox":{"credentials":{"envVars":[{"name":"TOKEN","mode":"mask","extract":"token=(.*)"}]}}}"#,
        ));
        assert_eq!(regexp.status, NativeStrictSettingsStatus::Unresolved);
        assert!(regexp
            .unresolved_fields
            .iter()
            .any(|path| path == "sandbox.credentials.envVars.0.extract/js-regexp-result-missing"));
    }

    #[test]
    fn sandbox_ignore_violations_is_a_record_of_string_arrays() {
        let result = validate(&parse(
            r#"{"sandbox":{"ignoreViolations":{"Bash":["/tmp/a"],"Read":["/tmp/b"]}}}"#,
        ));
        assert_eq!(result.status, NativeStrictSettingsStatus::Valid);

        let invalid = validate(&parse(
            r#"{"sandbox":{"ignoreViolations":{"Bash":"/tmp/a"}}}"#,
        ));
        assert_eq!(invalid.status, NativeStrictSettingsStatus::Invalid);
        assert_eq!(
            invalid.issues[0].path.string_at(""),
            Some("sandbox.ignoreViolations.Bash")
        );

        let proto = validate(&parse(
            r#"{"sandbox":{"ignoreViolations":{"__proto__":["/ignored"],"Bash":["/kept"]}}}"#,
        ));
        assert_eq!(proto.status, NativeStrictSettingsStatus::Valid);
        assert!(proto
            .projection
            .at_pointer("/sandbox/ignoreViolations/__proto__")
            .is_none());
        assert!(proto
            .projection
            .at_pointer("/sandbox/ignoreViolations/Bash/0")
            .is_some());
    }

    #[test]
    fn sandbox_extract_consumes_exact_utf16_bun_regexp_results() {
        let source = parse(
            r#"{"sandbox":{"credentials":{"files":[{"path":"/tmp/key","mode":"mask","extract":"secret=(.*)"}]}}}"#,
        );
        let pattern = "secret=(.*)".encode_utf16().collect::<Vec<_>>();
        let valid_row = NativePolicyRegexValidationRow {
            pattern_units: pattern.clone(),
            outcome: NativePolicyRegexValidationOutcome::Valid {
                source_units: "secret=(.*)".encode_utf16().collect(),
                capture_count: 1,
            },
        };
        let valid = validate_with_regex(&source, &[valid_row.clone()]);
        assert_eq!(valid.status, NativeStrictSettingsStatus::Valid);
        assert!(valid.issues.is_empty());
        assert!(valid.unresolved_fields.is_empty());

        let no_capture_row = NativePolicyRegexValidationRow {
            pattern_units: "secret=.*".encode_utf16().collect(),
            outcome: NativePolicyRegexValidationOutcome::Valid {
                source_units: "secret=.*".encode_utf16().collect(),
                capture_count: 0,
            },
        };
        let no_capture = validate_with_regex(
            &parse(
                r#"{"sandbox":{"credentials":{"files":[{"path":"/tmp/key","mode":"mask","extract":"secret=.*"}]}}}"#,
            ),
            &[no_capture_row],
        );
        assert_eq!(no_capture.status, NativeStrictSettingsStatus::Invalid);
        assert_eq!(
            no_capture.issues[0].path.string_at(""),
            Some("sandbox.credentials.files.0.extract")
        );
        assert_eq!(
            no_capture.issues[0].message.string_at(""),
            Some("extract must contain at least one capturing group — group 1 is the credential value to mask (e.g. \"token:\\s*(\\S+)\").")
        );

        let mut raw_error = "Bun syntax error: ".encode_utf16().collect::<Vec<_>>();
        raw_error.push(0xd800);
        let invalid_row = NativePolicyRegexValidationRow {
            pattern_units: pattern,
            outcome: NativePolicyRegexValidationOutcome::Invalid {
                error_message_units: raw_error.clone(),
            },
        };
        let invalid = validate_with_regex(&source, &[invalid_row]);
        assert_eq!(invalid.status, NativeStrictSettingsStatus::Invalid);
        let invalid_units = invalid.issues[0]
            .message
            .string_units("")
            .expect("exact UTF-16 Bun message projection");
        assert_eq!(
            invalid_units,
            [
                "extract is not a valid regular expression: "
                    .encode_utf16()
                    .collect::<Vec<_>>(),
                raw_error,
            ]
            .concat()
        );
    }

    #[test]
    fn env_record_uses_native_string_coercion_for_json_primitives_and_arrays() {
        let result = validate(&parse(
            r#"{"env":{"TEXT":"kept","BOOL":true,"NULL":null,"NEGATIVE_ZERO":-0,"LARGE":1e21,"ARRAY":[null,"x",2,[3,null]],"OBJECT":{}}}"#,
        ));
        assert_eq!(result.status, NativeStrictSettingsStatus::Valid);
        assert_eq!(result.projection.string_at("/env/TEXT"), Some("kept"));
        assert_eq!(result.projection.string_at("/env/BOOL"), Some("true"));
        assert_eq!(result.projection.string_at("/env/NULL"), Some("null"));
        assert_eq!(result.projection.string_at("/env/NEGATIVE_ZERO"), Some("0"));
        assert_eq!(result.projection.string_at("/env/LARGE"), Some("1e+21"));
        assert_eq!(result.projection.string_at("/env/ARRAY"), Some(",x,2,3,"));
        assert_eq!(
            result.projection.string_at("/env/OBJECT"),
            Some("[object Object]")
        );

        let shadowed = validate(&parse(r#"{"env":{"OBJECT":{"toString":"not-callable"}}}"#));
        assert_eq!(shadowed.status, NativeStrictSettingsStatus::Invalid);
        assert!(shadowed.unresolved_fields.is_empty());
        assert_eq!(shadowed.issues.len(), 1);
        assert_eq!(shadowed.issues[0].path.string_at(""), Some("env.OBJECT"));
        assert_eq!(
            shadowed.issues[0].message.string_at(""),
            Some("Invalid input: expected string, received object")
        );
    }

    #[test]
    fn env_coercion_failure_keeps_original_array_type_and_source_key() {
        let result = validate(&parse(
            r#"{"env":{"\ud800":["first",[{"toString":null}]]}}"#,
        ));
        assert_eq!(result.status, NativeStrictSettingsStatus::Invalid);
        assert!(result.unresolved_fields.is_empty());
        assert_eq!(result.issues.len(), 1);
        assert_eq!(
            result.issues[0].path.string_units("").unwrap(),
            &[101, 110, 118, 46, 0xd800]
        );
        assert_eq!(
            result.issues[0].message.string_at(""),
            Some("Invalid input: expected string, received array")
        );
    }

    #[test]
    fn env_record_omits_proto_before_coercion_and_keeps_ordinary_valueof() {
        let result = validate(&parse(
            r#"{"env":{"__proto__":{"toString":false},"KEPT":{"valueOf":null},"NULLS":[null,[null,null]]}}"#,
        ));
        assert_eq!(result.status, NativeStrictSettingsStatus::Valid);
        assert!(result.issues.is_empty());
        assert!(result.projection.at_pointer("/env/__proto__").is_none());
        assert_eq!(
            result.projection.string_at("/env/KEPT"),
            Some("[object Object]")
        );
        assert_eq!(result.projection.string_at("/env/NULLS"), Some(",,"));
    }

    #[test]
    fn invalid_caught_ui_fields_disappear_but_bad_plugin_config_values_reject() {
        let caught = validate(&parse(
            r#"{"editorMode":"insert","preferredNotifChannel":"system"}"#,
        ));
        assert_eq!(caught.status, NativeStrictSettingsStatus::Valid);
        assert!(caught.projection.at_pointer("/editorMode").is_none());
        assert!(caught
            .projection
            .at_pointer("/preferredNotifChannel")
            .is_none());

        let invalid = validate(&parse(
            r#"{"pluginConfigs":{"review@local":{"options":{"enabled":null}}}}"#,
        ));
        assert_eq!(invalid.status, NativeStrictSettingsStatus::Invalid);
        assert_eq!(
            invalid.issues[0].path.string_at(""),
            Some("pluginConfigs.review@local.options.enabled")
        );
    }
}
