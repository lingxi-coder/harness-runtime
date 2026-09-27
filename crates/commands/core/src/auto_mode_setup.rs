//! `/auto-mode-setup` — the WIZARD-06 permission-hardening command.
//!
//! Ported from the binary's NON-INTERACTIVE command object (description
//! re-verified against 2.1.238 @294963678 — SLASH-03 re-worded it):
//!
//! ```text
//! mSl={type:"local",name:"auto-mode-setup",supportsNonInteractive:!0,
//!   description:"Teach auto mode about your environment, plus optional rule
//!     tweaks",
//!   argumentHint:"[--request-id <uuid>] (--wizard posture=… scope=… depth=…
//!     --propose | --expect-sha256 <64-hex> --apply-file <path>)",
//!   isEnabled:()=>hPo()&&_n(), get isHidden(){return!_n()}, load:...}
//! ```
//!
//! There is a second object with the same name (`Kay`, `type:"local-jsx"`) —
//! the interactive review dialog. `LingXi` ports the `local` half, matching how
//! [`crate::goal`] and the rest of this crate handle a split command.
//!
//! The dispatcher is the oracle's `jay`/`Way`/`zay` chain: parse the raw
//! argument string, run the matching branch, and return
//! `JSON.stringify(result, null, 2)` with `requestId` merged in last. Both this
//! surface and `lingxi-cli auto-mode-setup` call the SAME grammar
//! (`permission::auto_mode_argv`) so they cannot drift — a lenient parse on
//! either side would be a real hazard, since this command writes permission
//! settings.
//!
//! ## What this handler does NOT do
//!
//! `--propose` needs a model, and a `BuiltinCommandHandler` has no route to
//! one. Rather than silently degrade — emitting a `recon_failed` would report a
//! scan that never ran — the propose branch is driven through an injected
//! [`ProposeRunner`]. A host that has an LLM stack supplies one; without it the
//! branch reports plainly that this surface cannot run it, and names the
//! surface that can.

use std::path::PathBuf;

use async_trait::async_trait;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use permission::auto_mode_argv::{
    parse_apply_file_args, propose_result_json, ApplyFileInvocation, ApplyResult,
    AutoModeSetupInvocation, GrammarError, ProposeInvocation, USAGE,
};
use serde_json::json;

/// Runs the `--propose` branch. Injected because it needs a live model, which
/// this crate has no route to.
#[async_trait]
pub trait ProposeRunner: Send + Sync {
    /// Gather the recon, ask the model, and return the result BODY (the
    /// `{ok:…}` object) — the envelope and `requestId` are added by the caller.
    async fn run(&self, inv: &ProposeInvocation) -> serde_json::Value;
}

/// Applies a reviewed proposal file. Injected for the same reason the grammar
/// is shared: the write path is the security-relevant half, and it must be the
/// one the CLI already uses rather than a second implementation.
#[async_trait]
pub trait ApplyRunner: Send + Sync {
    /// Run the `--apply-file` pipeline.
    async fn run(&self, inv: &ApplyFileInvocation) -> ApplyResult;
}

/// `/auto-mode-setup`.
pub struct AutoModeSetupHandler {
    propose: Option<std::sync::Arc<dyn ProposeRunner>>,
    apply: Option<std::sync::Arc<dyn ApplyRunner>>,
}

impl AutoModeSetupHandler {
    /// A handler with no runners: grammar, `--help` and error reporting work;
    /// the two branches that touch the model or the settings file report that
    /// this surface cannot run them.
    #[must_use]
    pub fn new() -> Self {
        Self {
            propose: None,
            apply: None,
        }
    }

    /// Attach the `--propose` runner.
    #[must_use]
    pub fn with_propose(mut self, runner: std::sync::Arc<dyn ProposeRunner>) -> Self {
        self.propose = Some(runner);
        self
    }

    /// Attach the `--apply-file` runner.
    #[must_use]
    pub fn with_apply(mut self, runner: std::sync::Arc<dyn ApplyRunner>) -> Self {
        self.apply = Some(runner);
        self
    }
}

impl Default for AutoModeSetupHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// The oracle's `Way` usage arm: `{ok:false, code:"usage", reason, usage}`.
fn usage_body(e: &GrammarError) -> serde_json::Value {
    json!({
        "ok": false,
        "code": "usage",
        "reason": if e.message.is_empty() { USAGE } else { e.message },
        "usage": USAGE,
    })
}

/// Reported when a branch cannot run on this surface. Distinct from every
/// oracle failure code on purpose: it says the request was never attempted,
/// which is not the same as a scan or a write that failed.
const UNAVAILABLE_HERE: &str = "unavailable_here";

fn unavailable(reason: &str) -> serde_json::Value {
    json!({ "ok": false, "code": UNAVAILABLE_HERE, "reason": reason })
}

/// `ApplyResult` -> the oracle's result body.
fn apply_body(result: &ApplyResult) -> serde_json::Value {
    match result {
        ApplyResult::Wrote { removed_count } => {
            json!({ "ok": true, "removed_count": removed_count })
        }
        ApplyResult::NoChange => json!({ "ok": true, "removed_count": 0 }),
        ApplyResult::Rejected { code, reason } => {
            json!({ "ok": false, "code": code, "reason": reason })
        }
        ApplyResult::InvalidSave { reason } => {
            json!({ "ok": false, "code": "usage", "reason": reason })
        }
    }
}

/// Split the raw argument string into the token vector the grammar walks.
///
/// The oracle regex-matches the whole trimmed string; tokenising is the same
/// thing for every well-formed invocation and is what the CLI surface already
/// feeds the grammar, so both surfaces see identical tokens.
fn tokens(raw: &str) -> Vec<String> {
    raw.split_whitespace().map(str::to_string).collect()
}

#[async_trait]
impl BuiltinCommandHandler for AutoModeSetupHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let parsed = tokens(&args.raw_args);
        let (body, request_id) = match parse_apply_file_args(&parsed) {
            // `Gay`'s bare/`--help` branch: `{mode:"usage", message:Nj}` with
            // NO logCode, which `Way` turns into the usage arm carrying the
            // block as its reason. It is `ok:false` in the oracle too — the
            // command did not do anything.
            Ok(AutoModeSetupInvocation::Help) => (
                json!({ "ok": false, "code": "usage", "reason": USAGE, "usage": USAGE }),
                None,
            ),
            Ok(AutoModeSetupInvocation::Propose(inv)) => {
                let id = inv.request_id.clone();
                let body = match &self.propose {
                    Some(runner) => runner.run(&inv).await,
                    None => unavailable(
                        "This surface can't run --propose (no model is wired to it). \
                         Run `lingxi-cli auto-mode-setup --wizard … --propose` instead.",
                    ),
                };
                (body, id)
            }
            Ok(AutoModeSetupInvocation::ApplyFile(inv)) => {
                let id = inv.request_id.clone();
                let body = match &self.apply {
                    Some(runner) => apply_body(&runner.run(&inv).await),
                    None => unavailable(
                        "This surface can't run --apply-file (no settings writer is wired \
                         to it). Run `lingxi-cli auto-mode-setup --expect-sha256 … \
                         --apply-file <path>` instead.",
                    ),
                };
                (body, id)
            }
            Err(e) => {
                // `if (e.logCode !== void 0) pe("auto_mode_setup_write", e.logCode)`
                // — only a real flag-grammar violation carries a logCode. The
                // plain usage outcomes above deliberately record nothing.
                if e.code == permission::auto_mode_argv::CODE_BAD_FLAG_GRAMMAR {
                    telemetry::emit_auto_mode_setup_write(e.code);
                }
                (usage_body(&e), None)
            }
        };
        CommandResult::Done {
            display: Some(propose_result_json(body, request_id.as_deref())),
        }
    }

    fn name(&self) -> &str {
        "auto-mode-setup"
    }

    fn description(&self) -> &str {
        command_api::builtin_support::core_description("auto-mode-setup")
    }
}

/// Where the `--apply-file` containment roots come from, shared with the CLI
/// surface: the system temp directory and the config directory, each in both
/// its literal and its realpath spelling.
#[must_use]
pub fn apply_file_roots(config_dir: &std::path::Path) -> Vec<PathBuf> {
    let mut roots = vec![std::env::temp_dir(), config_dir.to_path_buf()];
    for root in roots.clone() {
        if let Ok(canonical) = std::fs::canonicalize(&root) {
            if !roots.contains(&canonical) {
                roots.push(canonical);
            }
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "auto-mode-setup".to_string(),
            raw_args: raw.to_string(),
            positional_args: tokens(raw),
        }
    }

    async fn run(raw: &str) -> serde_json::Value {
        let h = AutoModeSetupHandler::new();
        match h.handle(&cmd(raw)).await {
            CommandResult::Done { display: Some(s) } => serde_json::from_str(&s).unwrap(),
            other => panic!("{other:?}"),
        }
    }

    const UUID: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3301";

    #[tokio::test]
    async fn bare_invocation_prints_usage() {
        // `ok:false` matches the oracle: nothing ran. What distinguishes it
        // from a grammar error is that the reason IS the usage block, with no
        // "Couldn't parse arguments." prefix.
        let v = run("").await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "usage");
        assert!(v["reason"].as_str().unwrap().starts_with("Usage:"));
        assert!(!v["reason"].as_str().unwrap().contains("Couldn"));
    }

    #[tokio::test]
    async fn help_takes_the_same_branch_as_bare() {
        for raw in ["--help", "-h"] {
            let v = run(raw).await;
            assert_eq!(v["ok"], false, "{raw}");
            assert!(v["reason"].as_str().unwrap().starts_with("Usage:"), "{raw}");
        }
    }

    #[tokio::test]
    async fn a_grammar_error_reports_usage_with_the_oracles_message() {
        let v = run("--apply-file").await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "usage");
        assert_eq!(
            v["reason"],
            "--apply-file needs a path to the reviewed proposal JSON."
        );
        // The usage block rides along, matching `Way`'s usage arm.
        assert!(v["usage"].as_str().unwrap().contains("--propose"));
    }

    #[tokio::test]
    async fn the_slash_surface_shares_the_cli_grammar() {
        // Same rejection the CLI surface produces for the same tokens: the
        // point of hosting the grammar below both.
        let v = run(
            "--request-id not-a-uuid --wizard posture=personal scope=project depth=here --propose",
        )
        .await;
        assert_eq!(v["code"], "usage");
        assert!(v["reason"]
            .as_str()
            .unwrap()
            .contains("canonical 8-4-4-4-12"));
        // The rejected token must not be echoed back into the output.
        assert!(!v["reason"].as_str().unwrap().contains("not-a-uuid"));
    }

    #[tokio::test]
    async fn propose_without_a_runner_says_so_instead_of_faking_a_scan() {
        let v = run("--wizard posture=personal scope=project depth=here --propose").await;
        assert_eq!(v["ok"], false);
        // NOT `recon_failed`: nothing was scanned, so reporting a failed scan
        // would describe work that never happened.
        assert_eq!(v["code"], "unavailable_here");
        assert!(v["reason"].as_str().unwrap().contains("lingxi-cli"));
    }

    #[tokio::test]
    async fn the_request_id_is_echoed_on_the_propose_branch() {
        let v = run(&format!(
            "--request-id {UUID} --wizard posture=personal scope=project depth=here --propose"
        ))
        .await;
        assert_eq!(v["requestId"], UUID);
    }

    struct StubPropose;
    #[async_trait]
    impl ProposeRunner for StubPropose {
        async fn run(&self, inv: &ProposeInvocation) -> serde_json::Value {
            json!({ "ok": true, "proposal": { "scope": inv.scope } })
        }
    }

    #[tokio::test]
    async fn a_wired_runner_drives_the_propose_branch() {
        let h = AutoModeSetupHandler::new().with_propose(std::sync::Arc::new(StubPropose));
        let raw = format!(
            "--request-id {UUID} --wizard posture=enterprise scope=all depth=both --propose"
        );
        let CommandResult::Done { display: Some(s) } = h.handle(&cmd(&raw)).await else {
            panic!("expected Done");
        };
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["proposal"]["scope"], "all");
        assert_eq!(v["requestId"], UUID);
        // `requestId` is merged LAST, as `zay` does.
        assert!(s
            .trim_end()
            .ends_with(&format!("\"requestId\": \"{UUID}\"\n}}")));
    }

    struct StubApply;
    #[async_trait]
    impl ApplyRunner for StubApply {
        async fn run(&self, _inv: &ApplyFileInvocation) -> ApplyResult {
            ApplyResult::Wrote { removed_count: 2 }
        }
    }

    #[tokio::test]
    async fn a_wired_apply_runner_reports_what_was_written() {
        let h = AutoModeSetupHandler::new().with_apply(std::sync::Arc::new(StubApply));
        let raw = format!(
            "--expect-sha256 {} --apply-file /tmp/p.json",
            "a".repeat(64)
        );
        let CommandResult::Done { display: Some(s) } = h.handle(&cmd(&raw)).await else {
            panic!("expected Done");
        };
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["removed_count"], 2);
    }

    #[tokio::test]
    async fn apply_without_a_runner_never_claims_a_write() {
        let v = run(&format!(
            "--expect-sha256 {} --apply-file /tmp/p.json",
            "a".repeat(64)
        ))
        .await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "unavailable_here");
    }

    #[test]
    fn the_description_is_the_oracles() {
        assert_eq!(
            AutoModeSetupHandler::new().description(),
            "Teach auto mode about your environment, plus optional rule tweaks"
        );
    }

    #[test]
    fn roots_cover_both_spellings_of_each_root() {
        let config = std::env::temp_dir().join("lingxi-w53-roots-probe");
        let roots = apply_file_roots(&config);
        assert!(roots.contains(&config));
        assert!(roots.contains(&std::env::temp_dir()));
        // On macOS `/var` is a symlink to `/private/var`, so the realpath
        // spelling must be present too or a host that canonicalised the
        // proposal path would be refused for a file genuinely in temp.
        if let Ok(canonical) = std::fs::canonicalize(std::env::temp_dir()) {
            assert!(roots.contains(&canonical));
        }
    }
}
