//! Integration: PowerShell path-containment wired into the permission gate.
//!
//! Verifies the end-to-end path — inject a [`PwshParser`], `authorize` a
//! `PowerShell` command, and observe the containment ask/deny — plus the
//! passthrough (byte-identical) behavior when no parser is wired.

use std::path::PathBuf;
use std::sync::Arc;

use permission::filesystem::FsRoots;
use permission::policy::PermissionPolicy;
use permission::powershell_parse::{parse_ps_ast_json, ParseResult, PwshParser};
use permission::result::PermissionResult;
use permission::PermissionMode;
use serde_json::json;

/// A parser that returns a fixed AST built from a JSON fixture (bypasses `pwsh`).
struct FakeParser(ParseResult);
impl PwshParser for FakeParser {
    fn parse(&self, _command: &str) -> ParseResult {
        self.0.clone()
    }
}

fn roots() -> FsRoots {
    FsRoots {
        cwd: PathBuf::from("/proj/work"),
        home: Some(PathBuf::from("/home/u")),
        lingxi_home: PathBuf::from("/home/u/.lingxi"),
    }
}

/// A `Get-Content <path>` statement AST as pwsh would emit it.
fn get_content_ast(path: &str) -> ParseResult {
    let json = format!(
        r#"{{"valid":true,"statements":[{{"type":"PipelineAst","text":"Get-Content {path}",
        "elements":[{{"type":"CommandAst","text":"Get-Content {path}","commandElements":[
          {{"type":"StringConstantExpressionAst","text":"Get-Content","value":"Get-Content"}},
          {{"type":"StringConstantExpressionAst","text":"{path}","value":"{path}"}}
        ],"redirections":[]}}]}}]}}"#
    );
    parse_ps_ast_json(&json)
}

fn remove_item_ast(path: &str) -> ParseResult {
    let json = format!(
        r#"{{"valid":true,"statements":[{{"type":"PipelineAst","text":"Remove-Item {path}",
        "elements":[{{"type":"CommandAst","text":"Remove-Item {path}","commandElements":[
          {{"type":"StringConstantExpressionAst","text":"Remove-Item","value":"Remove-Item"}},
          {{"type":"StringConstantExpressionAst","text":"{path}","value":"{path}"}}
        ],"redirections":[]}}]}}]}}"#
    );
    parse_ps_ast_json(&json)
}

fn policy_with(parser: ParseResult) -> PermissionPolicy {
    PermissionPolicy::new(PermissionMode::Default)
        .with_roots(roots())
        .with_pwsh_parser(Arc::new(FakeParser(parser)))
}

#[test]
fn powershell_outside_cwd_asks_with_containment_message() {
    let policy = policy_with(get_content_ast("/etc/passwd"));
    let result = policy.authorize(
        "PowerShell",
        &json!({ "command": "Get-Content /etc/passwd" }),
    );
    match result {
        PermissionResult::Ask { prompt, .. } => assert_eq!(
            prompt.message,
            "get-content targeting '/etc/passwd' was blocked. For security, LingXi may only access files in the allowed working directories for this session: '/proj/work'."
        ),
        other => panic!("expected containment Ask, got {other:?}"),
    }
}

#[test]
fn powershell_remove_item_protected_denies() {
    let policy = policy_with(remove_item_ast("/etc"));
    let result = policy.authorize("PowerShell", &json!({ "command": "Remove-Item /etc" }));
    match result {
        PermissionResult::Deny { explanation, .. } => assert_eq!(
            explanation.as_deref(),
            Some("Remove-Item on system path '/etc' is blocked. This path is protected from removal.")
        ),
        other => panic!("expected containment Deny, got {other:?}"),
    }
}

#[test]
fn powershell_inside_cwd_does_not_trigger_containment() {
    // A path under cwd → containment passes through; the result comes from the
    // normal flow (NOT the containment ask). We assert it is not the containment
    // message (whatever the mode/default decides is fine).
    let policy = policy_with(get_content_ast("/proj/work/notes.txt"));
    let result = policy.authorize("PowerShell", &json!({ "command": "Get-Content notes.txt" }));
    if let PermissionResult::Ask { prompt, .. } = &result {
        assert!(
            !prompt.message.contains("targeting"),
            "inside-cwd path must not produce the containment message: {}",
            prompt.message
        );
    }
}

#[test]
fn powershell_without_parser_passes_through() {
    // An invalid parse with an explicit signal now fails closed to Ask.
    let policy = PermissionPolicy::new(PermissionMode::Default)
        .with_roots(roots())
        .with_pwsh_parser(Arc::new(FakeParser(ParseResult {
            valid: false,
            statements: Vec::new(),
            invalid_reason: Some(
                "PowerShell parser precheck rejected unsupported `u{...}` escape".to_string(),
            ),
            ..ParseResult::default()
        })));
    let result = policy.authorize("PowerShell", &json!({ "command": "echo `u{263A}" }));
    match result {
        PermissionResult::Ask { prompt, .. } => assert_eq!(
            prompt.message,
            "PowerShell command could not be statically validated: PowerShell parser precheck rejected unsupported `u{...}` escape"
        ),
        other => panic!("expected invalid-parse Ask, got {other:?}"),
    }

    // No parser wired — or an invalid parse without an explicit signal —
    // preserves the inert passthrough behavior.
    let policy = PermissionPolicy::new(PermissionMode::Default)
        .with_roots(roots())
        .with_pwsh_parser(Arc::new(FakeParser(ParseResult::default())));
    let result = policy.authorize(
        "PowerShell",
        &json!({ "command": "Get-Content /etc/passwd" }),
    );
    if let PermissionResult::Ask { prompt, .. } = &result {
        assert!(
            !prompt.message.contains("targeting"),
            "invalid parse must pass through"
        );
    }
    // And a policy with genuinely no parser also does not panic / contain.
    let no_parser = PermissionPolicy::new(PermissionMode::Default).with_roots(roots());
    let _ = no_parser.authorize(
        "PowerShell",
        &json!({ "command": "Get-Content /etc/passwd" }),
    );
}
