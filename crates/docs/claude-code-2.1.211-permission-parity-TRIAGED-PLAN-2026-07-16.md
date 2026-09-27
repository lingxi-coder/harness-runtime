# Permission-crate 2.1.211 parity — TRIAGED fix plan (2026-07-16)

Independent adversarial re-triage of the 73-finding ultra-audit (workflow wf_4b142d3b-18e).
**Result: 69 survivors (47 confirmed + 22 partial), 0 refuted, 4 need re-triage (connection error).**
Severities are RE-RATED by live runtime impact: findings gated behind the unwired `Auto` classifier stub are marked STUB/inert and dropped in severity.

## Progress
- ✅ Part 0: `--dangerously-skip-permissions` prompting bug fixed (`4da021c17`).
- ✅ Batch 1 (`7280dd7d5`): GIT-REMOTE-SHOW-01, PERM-AUTO-02, PERM-AST-PROMPT4-01, PERM-PS-NULL-02, +MODE-TITLE deferred. Tested + live-verified.
- ⏳ Remaining survivors below, grouped into fix lanes.

## Live-P1 (3) — do first
- **PERM-SEM-AWK-01** (bash_ast_security.rs, risk=MEDIUM): Entire awk semantic cluster (tzn set + YVc program battery) missing from check_semantics and cat-heredoc scan
- **PERM-AST-VARSCOPE-01** (bash_ast_security.rs, risk=HIGH): for/if/while scope machinery diverges: no pre-scan/merge (E3i/dUr), loop-var guard far narrower, select accepted
- **PERM-PATH-01** (command_path_containment.rs, risk=HIGH): Bash path/redirect targets are never checked against Edit/Read deny rules or the path safety check (under-DENY)

## Lane: bash-ast (7)
- [P1/CONF/risk=HIGH] **PERM-AST-VARSCOPE-01** — for/if/while scope machinery diverges: no pre-scan/merge (E3i/dUr), loop-var guard far narrower, select accepted
- [P1/CONF/risk=MEDIUM] **PERM-SEM-AWK-01** — Entire awk semantic cluster (tzn set + YVc program battery) missing from check_semantics and cat-heredoc scan
- [P2/CONF/risk=MEDIUM] **PERM-SEM-XARGS-01** — checkSemantics wrapper loop misses xargs strip + through-xargs denials (find/jq/awk)
- [P2/CONF/risk=MEDIUM] **PERM-AST-VARNAME-01** — Dangerous-variable-name battery (Itt/O3i: PATH/BASH_ENV/LD_*/DYLD_*/BASH_FUNC_*…) and unset/builtin-write denials missing
- [P2/CONF/risk=MEDIUM] **PERM-AST-STRING-01** — walk_string guards diverge: literal-residue<=1 rejection missing (under-ask), zsh $name[expr] differential missing, delimiters-only branch over-asks
- [P3/CONF/risk=LOW] **PERM-MSG-TOOCOMPLEX-01** — too_complex fallback reason bytes wrong: 'Unhandled node type: X' vs CC 'Contains shell syntax (X) that cannot be statically analyzed'
- [P3/CONF/risk=MEDIUM] **PERM-AST-EXPANSION-01** — resolve_simple_expansion diverges from ozn: no GVc blocked-name check, no $HOME → homedir() resolution, placeholder-composite substituted

## Lane: paths (8)
- [P1/CONF/risk=HIGH] **PERM-PATH-01** — Bash path/redirect targets are never checked against Edit/Read deny rules or the path safety check (under-DENY)
- [P2/CONF/risk=LOW] **PERM-PATH-06** — Working-dir containment case-folds; CC 2.1.211's EV uses caseFold:false (case-sensitive)
- [P2/CONF/risk=MEDIUM] **PERM-PATH-02** — Missing /dev/tcp,/dev/udp network-device redirect guard (incl. input redirects, which the port ignores entirely)
- [P2/CONF/risk=MEDIUM] **PERM-PATH-04** — Missing '..'-after-directory-segment traversal pre-guard (SUr) on command paths and redirect targets
- [P2/CONF/risk=MEDIUM] **PERM-PATH-05** — PATH_EXTRACTORS drift vs 2.1.211: grep/rg -f file validated, bespoke awk extractor, cut/paste/column flag-arg consumption, jq file flags, git no 2-path cap
- [P3/CONF/risk=LOW] **PERM-PATH-08** — cd-compound + redirection ask must exempt /dev/null-only redirects
- [P3/PART/risk=MEDIUM] **PERM-PATH-03** — cd COMMAND_VALIDATOR (multi-positional zsh guard) and first-positional-only cd extraction missing
- [P3/CONF/risk=MEDIUM] **PERM-PATH-07** — Redirect-target danger classification drift: '{'/'%' no longer dangerous; braces get the brace-guard message; >& charset check missing

## Lane: policy-core (13)
- [NONE/PART/risk=LOW] [STUB/inert] **PERM-AUTO-06** — No read-time filtering of dangerous allow rules while auto mode is active (CC rce) and no 'Ignoring dangerous permission' log on strip
- [P2/CONF/risk=MEDIUM] **PERM-BYPASS-01** — Bypass/plan-bypass must override guard asks (except Dangerous rm/rmdir) — port returns every guard ask even in bypassPermissions
- [P2/CONF/risk=MEDIUM] **PERM-BGOP-01** — Missing `&` background-operator allow→ask downgrade (Yqr) on Bash checkPermissions
- [P2/CONF/risk=MEDIUM] **PERM-GENFIELD-01** — Missing generic `field:globpattern` content-rule matcher (Mjr) in the deny/ask content walks
- [P2/CONF/risk=MEDIUM] **PERM-PS-CD-03** — Compound-cd flag is never computed (hardcoded false) making the compound-cd ask dead code; its decisionReason bytes also diverge
- [P2/CONF/risk=MEDIUM] **PERM-DELTA-EDIT-READDENY-02** — 2.1.211 Edit tool gate: paths covered by a Read deny rule can no longer be edited (CZn, errorCode 13)
- [P2/CONF/risk=MEDIUM] **PERM-GLOB-01** — Tool-wide deny/ask rule names support glob matching + alias expansion; port is exact-match only
- [P2/PART/risk=MEDIUM] **PERM-DELTA-RM-CMDSUB-01** — 2.1.211 AST command-substitution catastrophic-rm analysis (eLg) missing from too-complex bash guard
- [P3/PART/risk=LOW] **PERM-SBXASK-01** — Tool-wide ask rule is exempted when the Bash sandbox auto-allow would apply; port asks before the sandbox layer
- [P3/CONF/risk=MEDIUM] **PERM-SBX-ASKWIDE-03** — Tool-wide Bash ask rule not bypassed when sandbox auto-allow applies
- [P3/PART/risk=MEDIUM] **PERM-ALLOWOVER-01** — Tool-wide allow rule must override 'other'-type checkPermissions asks (path/sed/process-substitution/injection) in non-bypass modes
- [P3/PART/risk=HIGH] [STUB/inert] **PERM-AUTO-04** — mcpPermissionModeOverrides map and Chrome/preview classifier floor (effective per-MCP-server permission mode) missing
- [P3/CONF/risk=LOW] [STUB/inert] **PERM-REASON-01** — decisionReason shape mismatches: dangerous-removal tagged Other (CC: safetyCheck/classifierApprovable:false); bash battery tagged SafetyCheck (CC: other+bashMissKind)

## Lane: powershell (4)
- [P2/CONF/risk=MEDIUM] **PERM-PS-VRG-01** — check_ps_path reduces CC's full per-path permission evaluation (vRg) to bare working-dir membership — writes inside the working dir are auto-allowed where CC 2.1.211 asks
- [P3/CONF/risk=MEDIUM] **PERM-PS-NEST-04** — Nested-command loop diverges: missing CC's control-flow pipeline-source ask, and anti-parity Remove-Item -Recurse cwd check applied to nested commands
- [P3/CONF/risk=MEDIUM] **PERM-PS-RM-05** — Remove-Item protected-path detection misses path normalization/resolution — traversal into a protected root asks instead of hard-denying
- [P3/CONF/risk=MEDIUM] **PERM-PS-PARSE-07** — Parse-layer hardening from 2.1.211 missing: 4500-byte command cap, `u{HEX}` escape refusal, pwsh timeout + 2-attempt retry, resolved-executable lookup

## Lane: rules-matching (8)
- [P2/PART/risk=LOW] **PERM-WILD-01** — Wildcard matcher lacks whitespace-normalization mode (Ale 4th param) used by every Bash rule match
- [P2/CONF/risk=LOW] **PERM-BASH-02** — Prefix (:*) rule comparison not whitespace-collapsed — CC normalizes [ \t]+ in both prefix and candidate
- [P2/CONF/risk=MEDIUM] **PERM-BASH-01** — Wildcard Bash rules never try the xargs-wrapped form (deny/ask always; allow when pattern ends in unescaped *)
- [P3/CONF/risk=LOW] **PERM-CAND-01** — Prefix-mode candidate set includes the redirection-preserving original; CC 2.1.211 only does that in exact mode
- [P3/PART/risk=MEDIUM] **PERM-WILD-02** — Wildcard matcher missing GLOBSTAR transform: /**/ should compile to /(?:.*/)? (zero-or-more segments)
- [P3/PART/risk=LOW] [STUB/inert] **PERM-SRC-01** — PermissionRuleSource missing 2.1.211 sources: toolsNarrowing and mcpServerPolicy
- [P3/CONF/risk=LOW] [STUB/inert] **PERM-SRC-01** — PermissionRuleSource missing 2.1.211 sources: toolsNarrowing and mcpServerPolicy
- [P3/CONF/risk=MEDIUM] [STUB/inert] **PERM-DELTA-RULE-LINT-05** — 2.1.211 permission-rule lint: file-permission-check warning for Write/NotebookEdit/MultiEdit/Glob path rules + startup warning surfacing

## Lane: mode-resolution (7)
- [P2/CONF/risk=HIGH] **MODE-FRONTMATTER-04** — agent frontmatter permissionMode missing from orderedModes (between CLI flag and settings defaultMode)
- [P2/CONF/risk=MEDIUM] **MODE-ENV-SCRUB-03** — CLAUDE_CODE_SUBPROCESS_ENV_SCRUB force-to-default short-circuit missing from mode resolution
- [P3/CONF/risk=LOW] **MODE-TITLE-BYTES-05** — Mode titles "Plan Mode"/"Auto mode" diverge from 2.1.211 "Plan"/"Auto" in the user-facing ask message
- [P3/CONF/risk=LOW] **BYPASS-ANT-DOCKER-07** — Ant-only Docker/no-internet bypass refusal retained though removed from 2.1.211
- [P3/PART/risk=LOW] [STUB/inert] **MODE-SAFE-LIST-06** — Plan-safe / SAFE_YOLO allowlist stale vs 2.1.211 (TeamCreate/TeamDelete/Sleep removed; six names + plugin-skill set added; check now input-aware)
- [P3/PART/risk=MEDIUM] [STUB/inert] **MODE-SETTINGS-AUTO-TRUST-01** — settings defaultMode:"auto" honored from repo-controllable project settings (2.1.211 restricts to policy/user/flag tiers)
- [P3/PART/risk=MEDIUM] [STUB/inert] **MODE-BG-DISCLAIMER-02** — bg-session bypass downgrade (xlc/Rlc disclaimer gate) missing from initialPermissionModeFromCLI

## Lane: readonly-removal (3)
- [P3/CONF/risk=LOW] **GIT-REFLOG-01** — git reflog read-only callback is stale: missing drop/write and wrong show/list gate
- [P3/CONF/risk=LOW] **RO-BASE-EXACT-01** — pwd/whoami/alias/arch treated as read-only with arbitrary args (CC allows only bare/exact forms)
- [P3/PART/risk=MEDIUM] **SED-XWU-RESTRUCTURE-01** — checkSedConstraints not updated to 2.1.211 Xwu: missing redirect-prefix normalization and the over-length / redirect-borne ask reasons

## Lane: gates (8)
- [P2/CONF/risk=HIGH] **PERM-GATE-UPDATES-01** — updatedPermissions are never applied in-memory (applyPermissionUpdate/Xb unported): setMode guard+logs missing, session-destination rules dropped entirely, setMode never persisted
- [P2/CONF/risk=HIGH] **PERM-DELTA-HOOK-ASKFLOOR-03** — 2.1.211 hookAskFloor: a PreToolUse hook 'ask' must floor the auto-mode classifier, but the port's delegated gate re-runs the classifier and can allow
- [P2/CONF/risk=MEDIUM] **PERM-GATE-HOOKALLOW-01** — check_after_hook_allow maps a re-check Ask to Allow; CC turns an ask on hook-rewritten input into a DENY ('ask rule on hook-rewritten input')
- [P3/CONF/risk=LOW] **PERM-GATE-SETMODE-01** — parse_settable_mode lacks the 'manual'→'default' alias and diverges from CC on unknown modes
- [P3/PART/risk=LOW] **PERM-GATE-PROMPT-01** — InteractivePromptingGate's stdin '[Y/n]' prompt strings claim byte-lock to claude-code but have no counterpart in the 2.1.211 binary
- [P3/PART/risk=MEDIUM] **PERM-GATE-WIRE-01** — can_use_tool control_request omits display_name and decision_reason_type (and description/classifier_approvable/requires_user_interaction)
- [P3/PART/risk=MEDIUM] **PERM-GATE-SYSMSG-01** — stdio host never receives the system/permission_denied message CC emits on a pre-prompt policy deny
- [P3/PART/risk=MEDIUM] **PERM-GATE-HEADLESS-01** — Headless deny-on-ask message is invented; CC denies with the byte-locked 'Permission to use X has been denied. IMPORTANT: ...' guidance

## Lane: auto-mode (3)
- [P2/CONF/risk=LOW] **PERM-AUTO-01** — isDangerousBashPermission matcher outdated: missing whitespace/star-only check and python -m module exception (2.1.211 Qqr rework)
- [P2/PART/risk=MEDIUM] **PERM-AUTO-05** — hasSuspiciousWindowsPathPattern: 2.1.211 applies the trailing dot/space check PER SEGMENT, not on the whole path; auto-edit guard gained remote-mode exemptions
- [P3/PART/risk=MEDIUM] [STUB/inert] **PERM-AUTO-03** — settings autoMode.classifyAllShell escalation missing: CC treats ALL Bash/PowerShell allow rules as dangerous when set

## Lane: sandbox-suggestions (4)
- [P2/PART/risk=MEDIUM] **PERM-SBX-WOG-02** — No strict too-complex gate: Rust auto-allows unparseable commands CC only allows via WOg's restrictive static analysis
- [P3/CONF/risk=MEDIUM] **PERM-DELTA-SBX-SYMLINK-DENY-04** — 2.1.211 Linux sandbox resolves deny paths through symlinks and fails closed when resolution fails
- [P3/PART/risk=HIGH] [STUB/inert] **PERM-DELTA-AUTOMODE-WIZARD-06** — 2.1.208-211 auto-mode setup wizard (recon proposal + --apply-file with read-deny gating + permissions.allow removals) absent
- [P3/PART/risk=LOW] [STUB/inert] **PERM-SRC-05** — format_source / PermissionRuleSource missing 2.1.211 sources toolsNarrowing and mcpServerPolicy

## Needs re-triage (4) — connection error, not yet verified
- idx41 **PERM-PS-CALLER-06** (policy.rs) — 2.1.211 PowerShell compound-command security asks are entirely missing (cd+git bare-repo, git-internal writes, archive extraction, PS5.1 PATH shadowing, xcopy/robocopy+git)
- idx53 **PERM-AUTO-07** (policy_gate.rs) — Denial-breaker trip payload discarded at the live consumer: no headless abort, no telemetry, no warn line, no rewritten decision reason
- idx54 **PERM-SBX-BAU-01** (sandbox_auto_allow.rs) — Sandbox auto-allow missing checkSandboxAutoAllow's unsafe-env / net-redirect / cd+rm guards
- idx57 **PERM-SUG-04** (allow_suggestion.rs) — AllowAlways bash narrowing diverges from CC ruleSuggestions: wrong bytes (':*' vs ' *'), wrong prefix algorithm, missing exact-command fallback