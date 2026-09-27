//! Tests for the PowerShell JSON-AST → node model transform (claude-code
//! `ufg`/`Nhu`/`cfg`/`yBr`/`_Br`) and the pwsh encoding helpers.

use super::*;
use crate::powershell_containment::PsElement;

#[test]
fn base64_std_matches_known_vectors() {
    assert_eq!(base64_std(b""), "");
    assert_eq!(base64_std(b"f"), "Zg==");
    assert_eq!(base64_std(b"fo"), "Zm8=");
    assert_eq!(base64_std(b"foo"), "Zm9v");
    assert_eq!(base64_std(b"foob"), "Zm9vYg==");
    assert_eq!(base64_std(b"Get-Content x"), "R2V0LUNvbnRlbnQgeA==");
}

#[test]
fn encode_for_pwsh_is_utf16le_base64() {
    // "AB" → UTF-16LE bytes 41 00 42 00 → base64 "QQBCAA=="
    assert_eq!(encode_for_pwsh("AB"), "QQBCAA==");
}

#[test]
fn build_pwsh_script_prepends_encoded_command() {
    let s = build_pwsh_script("Get-Content x");
    assert!(s.starts_with("$EncodedCommand = 'R2V0LUNvbnRlbnQgeA=='\n"));
    assert!(s.contains("ParseInput"));
}

#[test]
fn parse_invalid_json_or_errors_is_passthrough() {
    let malformed = parse_ps_ast_json("not json");
    assert!(!malformed.valid);
    assert_eq!(
        malformed.invalid_reason.as_deref(),
        Some("PowerShell parser returned malformed JSON")
    );
    let r = parse_ps_ast_json(r#"{"valid":false,"statements":[]}"#);
    assert!(!r.valid);
    assert!(r.statements.is_empty());
    assert_eq!(
        r.invalid_reason.as_deref(),
        Some("PowerShell parser reported an invalid parse")
    );
}

#[test]
fn parse_errors_surface_explicit_signal() {
    let r = parse_ps_ast_json(
        r#"{"valid":false,"errors":[{"errorId":"UnexpectedToken","message":"Unexpected token"}],"statements":[]}"#,
    );
    assert!(!r.valid);
    assert_eq!(
        r.invalid_reason.as_deref(),
        Some("PowerShell parser reported UnexpectedToken: Unexpected token")
    );
}

/// A single `Get-Content /etc/passwd` pipeline as pwsh would emit it.
fn get_content_json() -> &'static str {
    r#"{
      "valid": true,
      "statements": [{
        "type": "PipelineAst",
        "text": "Get-Content /etc/passwd",
        "elements": [{
          "type": "CommandAst",
          "text": "Get-Content /etc/passwd",
          "commandElements": [
            {"type":"StringConstantExpressionAst","text":"Get-Content","value":"Get-Content"},
            {"type":"StringConstantExpressionAst","text":"/etc/passwd","value":"/etc/passwd"}
          ],
          "redirections": []
        }]
      }]
    }"#
}

#[test]
fn transform_simple_command() {
    let r = parse_ps_ast_json(get_content_json());
    assert!(r.valid);
    assert_eq!(r.statements.len(), 1);
    let stmt = &r.statements[0];
    assert_eq!(stmt.commands.len(), 1);
    match &stmt.commands[0] {
        PsElement::Command(c) => {
            assert_eq!(c.name, "Get-Content");
            assert_eq!(c.args, vec!["/etc/passwd"]);
            // element_types[0] = name (StringConstant), [1] = arg (StringConstant)
            assert_eq!(c.element_types, vec!["StringConstant", "StringConstant"]);
        }
        other => panic!("expected Command, got {other:?}"),
    }
}

#[test]
fn transform_parameter_and_variable_element_types() {
    // Set-Content -Path out.txt -Value $x  → -Path is a Parameter, $x a Variable.
    let json = r#"{
      "valid": true,
      "statements": [{
        "type": "PipelineAst", "text": "Set-Content -Path out.txt -Value $x",
        "elements": [{
          "type": "CommandAst", "text": "Set-Content -Path out.txt -Value $x",
          "commandElements": [
            {"type":"StringConstantExpressionAst","text":"Set-Content","value":"Set-Content"},
            {"type":"CommandParameterAst","text":"-Path"},
            {"type":"StringConstantExpressionAst","text":"out.txt","value":"out.txt"},
            {"type":"CommandParameterAst","text":"-Value"},
            {"type":"VariableExpressionAst","text":"$x"}
          ],
          "redirections": []
        }]
      }]
    }"#;
    let r = parse_ps_ast_json(json);
    match &r.statements[0].commands[0] {
        PsElement::Command(c) => {
            assert_eq!(c.name, "Set-Content");
            assert_eq!(c.args, vec!["-Path", "out.txt", "-Value", "$x"]);
            assert_eq!(
                c.element_types,
                vec![
                    "StringConstant",
                    "Parameter",
                    "StringConstant",
                    "Parameter",
                    "Variable"
                ]
            );
        }
        other => panic!("expected Command, got {other:?}"),
    }
}

#[test]
fn transform_expression_element_is_pipeline_source() {
    // $x | Set-Content out.txt  → first element is a CommandExpressionAst (source).
    let json = r#"{
      "valid": true,
      "statements": [{
        "type": "PipelineAst", "text": "$x | Set-Content out.txt",
        "elements": [
          {"type":"CommandExpressionAst","text":"$x","expressionType":"VariableExpressionAst","redirections":[]},
          {"type":"CommandAst","text":"Set-Content out.txt","commandElements":[
            {"type":"StringConstantExpressionAst","text":"Set-Content","value":"Set-Content"},
            {"type":"StringConstantExpressionAst","text":"out.txt","value":"out.txt"}
          ],"redirections":[]}
        ]
      }]
    }"#;
    let r = parse_ps_ast_json(json);
    let stmt = &r.statements[0];
    assert!(matches!(stmt.commands[0], PsElement::Expression { .. }));
    assert!(matches!(stmt.commands[1], PsElement::Command(_)));
}

#[test]
fn transform_single_object_not_array_via_cae() {
    // PowerShell ConvertTo-Json renders a one-element @(...) as a bare object;
    // `cae` must still yield one statement / one element.
    let json = r#"{
      "valid": true,
      "statements": {
        "type": "PipelineAst", "text": "Get-Item x",
        "elements": {
          "type": "CommandAst", "text": "Get-Item x",
          "commandElements": [
            {"type":"StringConstantExpressionAst","text":"Get-Item","value":"Get-Item"},
            {"type":"StringConstantExpressionAst","text":"x","value":"x"}
          ],
          "redirections": null
        }
      }
    }"#;
    let r = parse_ps_ast_json(json);
    assert_eq!(r.statements.len(), 1);
    match &r.statements[0].commands[0] {
        PsElement::Command(c) => assert_eq!(c.name, "Get-Item"),
        other => panic!("expected Command, got {other:?}"),
    }
}

#[test]
fn transform_redirection_and_merging() {
    let json = r#"{
      "valid": true,
      "statements": [{
        "type": "PipelineAst", "text": "Get-Process > out.txt 2>&1",
        "elements": [{
          "type":"CommandAst","text":"Get-Process","commandElements":[
            {"type":"StringConstantExpressionAst","text":"Get-Process","value":"Get-Process"}
          ],
          "redirections":[
            {"type":"FileRedirectionAst","append":false,"fromStream":"Output","locationText":"out.txt"},
            {"type":"MergingRedirectionAst"}
          ]
        }]
      }]
    }"#;
    let r = parse_ps_ast_json(json);
    let redirs = &r.statements[0].redirections;
    // File redirection to out.txt (not merging) + the merging one.
    assert!(redirs
        .iter()
        .any(|x| x.target == "out.txt" && !x.is_merging));
    assert!(redirs.iter().any(|x| x.is_merging && x.target.is_empty()));
}

#[test]
fn transform_module_qualifier_and_dash_normalization() {
    // A `Microsoft.PowerShell.Management\Get-Content` name strips the module
    // prefix; a unicode dash in the name normalizes to ASCII.
    let json = r#"{
      "valid": true,
      "statements": [{
        "type":"PipelineAst","text":"x",
        "elements":[{"type":"CommandAst","text":"x","commandElements":[
          {"type":"StringConstantExpressionAst","text":"Mod\\Get–Content","value":"Mod\\Get–Content"}
        ],"redirections":[]}]
      }]
    }"#;
    let r = parse_ps_ast_json(json);
    match &r.statements[0].commands[0] {
        PsElement::Command(c) => assert_eq!(c.name, "Get-Content"),
        other => panic!("expected Command, got {other:?}"),
    }
}

// ── PERM-PS-PARSE-07: parse-layer hardening (byte cap, u{} refusal) ──

#[test]
fn precheck_rejects_oversized_command() {
    // 4500 bytes is allowed; 4501 passes through without spawning pwsh.
    let ok = "a".repeat(PWSH_MAX_COMMAND_BYTES);
    assert!(parse_precheck(&ok).is_none());
    let too_long = "a".repeat(PWSH_MAX_COMMAND_BYTES + 1);
    let too_long = parse_precheck(&too_long).expect("oversized command should fail precheck");
    assert_eq!(
        too_long.invalid_reason.as_deref(),
        Some("PowerShell parser precheck rejected command longer than 4500 bytes")
    );
    // Byte-length, not char count: a multibyte char pushes the boundary in bytes.
    let multibyte = "é".repeat(PWSH_MAX_COMMAND_BYTES); // 2 bytes each → >4500 bytes
    let multibyte = parse_precheck(&multibyte).expect("multibyte overrun should fail precheck");
    assert_eq!(
        multibyte.invalid_reason.as_deref(),
        Some("PowerShell parser precheck rejected command longer than 4500 bytes")
    );
}

#[test]
fn precheck_refuses_unicode_codepoint_escape() {
    assert!(has_unicode_codepoint_escape("Write-Output `u{1F600}"));
    assert!(has_unicode_codepoint_escape("`u{41}"));
    assert!(has_unicode_codepoint_escape("prefix `u{aB}cd"));
    // Not an escape: no hex digit after `{`, or missing pieces.
    assert!(!has_unicode_codepoint_escape("`u{ }"));
    assert!(!has_unicode_codepoint_escape("`u{"));
    assert!(!has_unicode_codepoint_escape("u{41}"));
    assert!(!has_unicode_codepoint_escape("Get-Content notes.txt"));
    let rejected =
        parse_precheck("Remove-Item /etc `u{263A}").expect("unicode escape should fail precheck");
    assert_eq!(
        rejected.invalid_reason.as_deref(),
        Some("PowerShell parser precheck rejected unsupported `u{...}` escape")
    );
}

#[test]
fn system_parser_passthrough_on_oversized_and_escape() {
    // These return before any spawn, so they are testable without pwsh installed.
    let parser = SystemPwshParser;
    let too_long = parser.parse(&"x".repeat(PWSH_MAX_COMMAND_BYTES + 1));
    assert_eq!(
        too_long.invalid_reason.as_deref(),
        Some("PowerShell parser precheck rejected command longer than 4500 bytes")
    );
    let unicode = parser.parse("echo `u{263A}");
    assert_eq!(
        unicode.invalid_reason.as_deref(),
        Some("PowerShell parser precheck rejected unsupported `u{...}` escape")
    );
}

#[test]
fn transform_nested_commands() {
    // A statement with nestedCommands (e.g. inside a script block).
    let json = r#"{
      "valid": true,
      "statements": [{
        "type":"IfStatementAst","text":"if ($true) { Remove-Item /etc }",
        "nestedCommands":[{
          "type":"CommandAst","text":"Remove-Item /etc","commandElements":[
            {"type":"StringConstantExpressionAst","text":"Remove-Item","value":"Remove-Item"},
            {"type":"StringConstantExpressionAst","text":"/etc","value":"/etc"}
          ],"redirections":[]
        }]
      }]
    }"#;
    let r = parse_ps_ast_json(json);
    let stmt = &r.statements[0];
    assert_eq!(stmt.nested_commands.len(), 1);
    assert_eq!(stmt.nested_commands[0].name, "Remove-Item");
    assert_eq!(stmt.nested_commands[0].args, vec!["/etc"]);
}
