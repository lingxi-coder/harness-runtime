//! PowerShell command parsing via `pwsh` — byte-faithful port of claude-code's
//! parse layer for [`crate::powershell_containment`].
//!
//! claude-code does NOT hand-parse PowerShell. It base64-encodes the command,
//! prepends it to an embedded PowerShell script that calls
//! `[System.Management.Automation.Language.Parser]::ParseInput`, runs that
//! through `pwsh -NoProfile -NonInteractive -NoLogo -EncodedCommand <utf16le-b64>`,
//! and consumes the emitted JSON AST — then transforms it into the node model the
//! containment validator (`xgg`/`Z_u`) walks. When `pwsh` is unavailable or the
//! command does not parse, `valid` is false and the caller passes through (no
//! containment) — so this is inert on hosts without PowerShell.
//!
//! This module owns: the embedded parse script ([`PS_PARSE_SCRIPT`]), the
//! `pwsh` argument vector ([`PWSH_ARGS`]), the script/command wrapping
//! ([`build_pwsh_script`]), and the pure JSON-AST → [`PsStatement`] transform
//! (claude-code `ufg`/`Nhu`/`cfg`/`yBr`/`_Br`). The actual subprocess spawn is
//! injected by the caller (the permission gate wiring), keeping the transform
//! fully testable.

use crate::powershell_containment::{
    PsCommand, PsElement, PsRedirection, PsSecurityPatterns, PsStatement, PsVariable,
};
use serde_json::Value;

/// The embedded PowerShell parse script (claude-code `Fhu`) — verbatim. It reads
/// `$EncodedCommand` (base64 of the command), parses via `ParseInput`, and emits
/// a compact JSON AST.
pub const PS_PARSE_SCRIPT: &str = include_str!("powershell_parse.ps1");

/// The `pwsh` argument vector preceding the encoded command (claude-code
/// `["-NoProfile","-NonInteractive","-NoLogo","-EncodedCommand", …]`).
pub const PWSH_ARGS: [&str; 4] = [
    "-NoProfile",
    "-NonInteractive",
    "-NoLogo",
    "-EncodedCommand",
];

/// Build the PowerShell script to run for `command` (claude-code `afg`):
/// `$EncodedCommand = '<base64-of-command>'` followed by [`PS_PARSE_SCRIPT`].
/// The result is what the caller passes (UTF-16LE-base64-encoded) to
/// `pwsh -EncodedCommand`.
#[must_use]
pub fn build_pwsh_script(command: &str) -> String {
    format!(
        "$EncodedCommand = '{}'\n{PS_PARSE_SCRIPT}",
        base64_std(command.as_bytes())
    )
}

/// Standard base64 (claude-code `Buffer.from(e,"utf8").toString("base64")`), for
/// the `$EncodedCommand` literal inside the script.
#[must_use]
pub fn base64_std(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// UTF-16LE base64 for `pwsh -EncodedCommand` (claude-code `sfg`): encode the
/// script as little-endian UTF-16 bytes, then standard base64.
#[must_use]
pub fn encode_for_pwsh(script: &str) -> String {
    let mut utf16le = Vec::with_capacity(script.len() * 2);
    for u in script.encode_utf16() {
        utf16le.push((u & 0xFF) as u8);
        utf16le.push((u >> 8) as u8);
    }
    base64_std(&utf16le)
}

/// A parsed PowerShell command (claude-code parse result). `valid` is false when
/// `pwsh` was unavailable or the command did not parse — the caller then passes
/// through with NO containment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseResult {
    /// Whether the command parsed with no errors.
    pub valid: bool,
    /// The transformed statements to validate.
    pub statements: Vec<PsStatement>,
    /// Variable references discovered in the parse (claude-code
    /// `parseResult.variables[]`) — the `acceptEdits` `Voe` aggregate reads
    /// `isSplatted` to detect splatting.
    pub variables: Vec<PsVariable>,
    /// Whether the command contains a `--%` stop-parsing token (claude-code
    /// `parseResult.hasStopParsing`) — an unvalidatable construct that blocks the
    /// `acceptEdits` auto-allow.
    pub has_stop_parsing: bool,
    /// Optional explicit invalid/error signal from the parser/precheck. When
    /// present, callers can fail closed to an Ask instead of silently passing
    /// through on an otherwise-invalid parse.
    pub invalid_reason: Option<String>,
}

impl ParseResult {
    fn invalid(reason: impl Into<String>) -> Self {
        Self {
            valid: false,
            statements: Vec::new(),
            variables: Vec::new(),
            has_stop_parsing: false,
            invalid_reason: Some(reason.into()),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// JSON-AST → node model transform (ufg / Nhu / cfg / yBr / _Br)
// ───────────────────────────────────────────────────────────────────────────

/// `Cae` — normalize a JSON value that may be absent / a single object / an
/// array (PowerShell `ConvertTo-Json` renders a one-element `@(x)` as a bare
/// object and `@()` as null) into a slice of elements.
fn cae(v: Option<&Value>) -> Vec<&Value> {
    match v {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.iter().collect(),
        Some(x) => vec![x],
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// `Yve` — normalize the en/em/horizontal-bar dashes to ASCII `-`.
fn yve(s: &str) -> String {
    s.replace(['\u{2013}', '\u{2014}', '\u{2015}'], "-")
}

/// Strip ONE leading and ONE trailing straight quote (claude-code
/// `/^['"]|['"]$/g` applied to a command name).
fn strip_one_quote(s: &str) -> &str {
    let s = s.strip_prefix(['\'', '"']).unwrap_or(s);
    s.strip_suffix(['\'', '"']).unwrap_or(s)
}

/// `xzn` — strip a `Module\Cmdlet` module-qualifier prefix (but not a drive /
/// UNC / `.\`/`..\` path).
fn strip_module_path(e: &str) -> &str {
    let Some(t) = e.rfind('\\') else {
        return e;
    };
    let is_drive = e.len() >= 2 && e.as_bytes()[0].is_ascii_alphabetic() && e.as_bytes()[1] == b':';
    if is_drive || e.starts_with("\\\\") || e.starts_with(".\\") || e.starts_with("..\\") {
        return e;
    }
    let r = &e[t + 1..];
    if r.is_empty() {
        e
    } else {
        r
    }
}

/// `_Br` — map a raw AST node type (and, for a `CommandExpressionAst`, its inner
/// expression type) to the simplified element type the extractor uses.
fn br_map(node_type: &str, expr_type: Option<&str>) -> String {
    match node_type {
        "ScriptBlockExpressionAst" => "ScriptBlock",
        "SubExpressionAst" | "ArrayExpressionAst" | "ParenExpressionAst" => "SubExpression",
        "ExpandableStringExpressionAst" => "ExpandableString",
        "InvokeMemberExpressionAst" | "MemberExpressionAst" => "MemberInvocation",
        "VariableExpressionAst" => "Variable",
        "StringConstantExpressionAst" | "ConstantExpressionAst" => "StringConstant",
        "CommandParameterAst" => "Parameter",
        "CommandExpressionAst" => {
            return expr_type.map_or("Other".to_string(), |t| br_map(t, None))
        }
        _ => "Other",
    }
    .to_string()
}

/// The internal (operator, target, is_merging) of a transformed redirection —
/// `operator` is used only for `ufg`'s dedup, then dropped.
struct Redir {
    operator: String,
    target: String,
    is_merging: bool,
}

/// `yBr` — transform a raw redirection JSON node.
fn transform_redirection(raw: &Value) -> Redir {
    if str_field(raw, "type") == Some("MergingRedirectionAst") {
        return Redir {
            operator: "2>&1".to_string(),
            target: String::new(),
            is_merging: true,
        };
    }
    let append = raw.get("append").and_then(Value::as_bool).unwrap_or(false);
    let from = str_field(raw, "fromStream").unwrap_or("Output");
    let operator = match (append, from) {
        (true, "Error") => "2>>",
        (true, "All") => "*>>",
        (true, _) => ">>",
        (false, "Error") => "2>",
        (false, "All") => "*>",
        (false, _) => ">",
    };
    Redir {
        operator: operator.to_string(),
        target: str_field(raw, "locationText").unwrap_or("").to_string(),
        is_merging: false,
    }
}

/// `gXi` — classify a command NAME (already stripped of one surrounding quote,
/// BEFORE module-path stripping / dash normalization): `"cmdlet"` for a Verb-Noun
/// form, `"application"` for a path-like name (contains `.`/`\`/`/`), else
/// `"unknown"`.
fn gxi(f: &str) -> &'static str {
    // Verb-Noun: `^[A-Za-z]+-[A-Za-z][A-Za-z0-9_]*$`.
    let bytes = f.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    let verb_ok = i > 0 && i < bytes.len() && bytes[i] == b'-';
    if verb_ok {
        let noun = &bytes[i + 1..];
        let noun_ok = !noun.is_empty()
            && noun[0].is_ascii_alphabetic()
            && noun[1..]
                .iter()
                .all(|&b| b.is_ascii_alphanumeric() || b == b'_');
        if noun_ok {
            return "cmdlet";
        }
    }
    if f.contains('.') || f.contains('\\') || f.contains('/') {
        return "application";
    }
    "unknown"
}

/// The command NAME's resolved kind (claude-code `nameType`): a name containing
/// any non-ASCII char (`/[-￿]/`, incl. astral via surrogates) is an
/// `"application"`; otherwise defer to [`gxi`].
fn compute_name_type(f: &str) -> String {
    if f.chars().any(|c| c as u32 >= 0x80) {
        "application".to_string()
    } else {
        gxi(f).to_string()
    }
}

/// `Nhu` — transform a raw `CommandAst` element into a [`PsCommand`].
fn transform_command(raw: &Value) -> PsCommand {
    let elems = cae(raw.get("commandElements"));
    let mut name = String::new();
    let mut name_type = "unknown".to_string();
    let mut args: Vec<String> = Vec::new();
    let mut element_types: Vec<String> = Vec::new();
    let mut children: Vec<Option<Vec<String>>> = Vec::new();
    let mut any_child = false;

    if let Some((first, rest)) = elems.split_first() {
        let ftype = str_field(first, "type").unwrap_or("");
        let is_string =
            ftype == "StringConstantExpressionAst" || ftype == "ExpandableStringExpressionAst";
        let raw_name = if is_string {
            first.get("value").and_then(Value::as_str)
        } else {
            None
        }
        .unwrap_or_else(|| str_field(first, "text").unwrap_or(""));
        let f = strip_one_quote(raw_name);
        name_type = compute_name_type(f);
        name = yve(strip_module_path(f));
        element_types.push(br_map(ftype, str_field(first, "expressionType")));

        for g in rest {
            let gtype = str_field(g, "type").unwrap_or("");
            let is_str =
                gtype == "StringConstantExpressionAst" || gtype == "ExpandableStringExpressionAst";
            let text = if is_str {
                g.get("value").and_then(Value::as_str)
            } else {
                None
            }
            .unwrap_or_else(|| str_field(g, "text").unwrap_or(""));
            args.push(yve(text));
            element_types.push(br_map(gtype, str_field(g, "expressionType")));

            // Per-arg inline-value children (claude-code `Cde(g.children)`): the
            // simplified element type(s) of a `-Param:value` bound argument.
            let child = cae(g.get("children"));
            if child.is_empty() {
                children.push(None);
            } else {
                any_child = true;
                children.push(Some(
                    child
                        .iter()
                        .map(|c| {
                            br_map(
                                str_field(c, "type").unwrap_or(""),
                                str_field(c, "expressionType"),
                            )
                        })
                        .collect(),
                ));
            }
        }
    }

    // claude-code attaches `children` only when SOME arg had an inline value
    // (`...s&&{children:i}`); an all-`None` vector reads as "undefined".
    let children = if any_child { children } else { Vec::new() };

    let redirections = cae(raw.get("redirections"))
        .iter()
        .map(|r| {
            let t = transform_redirection(r);
            PsRedirection {
                target: t.target,
                is_merging: t.is_merging,
            }
        })
        .collect();

    PsCommand {
        name,
        name_type,
        args,
        element_types,
        children,
        redirections,
    }
}

/// `ufg` — transform a raw statement into a [`PsStatement`].
fn transform_statement(raw: &Value) -> PsStatement {
    let mut commands: Vec<PsElement> = Vec::new();
    let mut redirs: Vec<Redir> = Vec::new();

    let elements = cae(raw.get("elements"));
    if elements.is_empty() {
        // No pipeline elements → a single CommandExpression node (claude-code's
        // `else` arm) + the statement's redirections.
        commands.push(PsElement::Expression {
            text: yve(str_field(raw, "text").unwrap_or("")),
        });
        for r in cae(raw.get("redirections")) {
            redirs.push(transform_redirection(r));
        }
    } else {
        for l in &elements {
            if str_field(l, "type") == Some("CommandAst") {
                commands.push(PsElement::Command(transform_command(l)));
            } else {
                // `cfg` — a non-CommandAst element (expression / paren): a pipeline
                // source, kept as an Expression carrying its text.
                commands.push(PsElement::Expression {
                    text: yve(str_field(l, "text").unwrap_or("")),
                });
            }
            for c in cae(l.get("redirections")) {
                redirs.push(transform_redirection(c));
            }
        }
        // Append statement-level redirections, deduped on (operator, target).
        let mut seen: std::collections::HashSet<(String, String)> = redirs
            .iter()
            .map(|r| (r.operator.clone(), r.target.clone()))
            .collect();
        for r in cae(raw.get("redirections")) {
            let c = transform_redirection(r);
            let key = (c.operator.clone(), c.target.clone());
            if seen.insert(key) {
                redirs.push(c);
            }
        }
    }

    let nested_commands = cae(raw.get("nestedCommands"))
        .iter()
        .map(|c| transform_command(c))
        .collect();

    PsStatement {
        commands,
        nested_commands,
        redirections: redirs
            .into_iter()
            .map(|r| PsRedirection {
                target: r.target,
                is_merging: r.is_merging,
            })
            .collect(),
        statement_type: str_field(raw, "type").unwrap_or("").to_string(),
        security_patterns: transform_security_patterns(raw.get("securityPatterns")),
    }
}

/// Read the raw `securityPatterns` object (absent → all-false) into a
/// [`PsSecurityPatterns`]. Only the four flags the `acceptEdits` `Voe` aggregate
/// consumes are threaded; unset/absent fields default to `false`.
fn transform_security_patterns(raw: Option<&Value>) -> PsSecurityPatterns {
    let flag = |key: &str| {
        raw.and_then(|v| v.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    PsSecurityPatterns {
        has_member_invocations: flag("hasMemberInvocations"),
        has_sub_expressions: flag("hasSubExpressions"),
        has_expandable_strings: flag("hasExpandableStrings"),
        has_script_blocks: flag("hasScriptBlocks"),
    }
}

/// Read the top-level `variables` array (claude-code `parseResult.variables[]`)
/// into [`PsVariable`]s carrying `path` + `isSplatted`.
fn transform_variables(raw: Option<&Value>) -> Vec<PsVariable> {
    cae(raw)
        .iter()
        .map(|v| PsVariable {
            path: str_field(v, "path").unwrap_or("").to_string(),
            is_splatted: v
                .get("isSplatted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
        .collect()
}

/// Parse the `pwsh` JSON output into a [`ParseResult`] (the pure half). Returns
/// `valid=false` (→ passthrough) when the JSON is malformed or reports parse
/// errors. `statements` are the transformed [`PsStatement`]s.
#[must_use]
pub fn parse_ps_ast_json(json: &str) -> ParseResult {
    let Ok(root) = serde_json::from_str::<Value>(json) else {
        return ParseResult::invalid("PowerShell parser returned malformed JSON");
    };
    let valid = root.get("valid").and_then(Value::as_bool).unwrap_or(false);
    if !valid {
        let signal = root
            .get("errors")
            .and_then(Value::as_array)
            .and_then(|errors| errors.first())
            .map(|error| {
                let error_id = error.get("errorId").and_then(Value::as_str).unwrap_or("");
                let message = error.get("message").and_then(Value::as_str).unwrap_or("");
                match (error_id.is_empty(), message.is_empty()) {
                    (false, false) => format!("PowerShell parser reported {error_id}: {message}"),
                    (false, true) => format!("PowerShell parser reported {error_id}"),
                    (true, false) => format!("PowerShell parser reported: {message}"),
                    (true, true) => "PowerShell parser reported an invalid parse".to_string(),
                }
            })
            .unwrap_or_else(|| "PowerShell parser reported an invalid parse".to_string());
        return ParseResult::invalid(signal);
    }
    let statements = cae(root.get("statements"))
        .iter()
        .map(|s| transform_statement(s))
        .collect();
    ParseResult {
        valid: true,
        statements,
        variables: transform_variables(root.get("variables")),
        has_stop_parsing: root
            .get("hasStopParsing")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        invalid_reason: None,
    }
}

/// A capability that parses a PowerShell command into a [`ParseResult`]. Injected
/// into the permission gate (like the sandbox runtime) so the gate stays pure by
/// default: with no parser, PowerShell path-containment passes through — exactly
/// claude-code's behavior on a host without `pwsh`.
pub trait PwshParser: Send + Sync {
    /// Parse `command`; `valid=false` (→ passthrough) on any failure.
    fn parse(&self, command: &str) -> ParseResult;
}

/// Maximum command byte length the pwsh parser will attempt (claude-code
/// `ZKi=NAg=4500`): a longer command passes through WITHOUT containment rather
/// than spawning `pwsh` needlessly (`Buffer.byteLength(e,"utf8")>ZKi`).
pub const PWSH_MAX_COMMAND_BYTES: usize = 4500;

/// Number of spawn attempts before giving up (claude-code `xAg=2`).
const PWSH_PARSE_ATTEMPTS: usize = 2;

/// Default per-attempt `pwsh` timeout in ms (claude-code `AAg=5000`), overridable
/// by `LINGXI_PWSH_PARSE_TIMEOUT_MS` / `CLAUDE_CODE_PWSH_PARSE_TIMEOUT_MS`.
const PWSH_PARSE_DEFAULT_TIMEOUT_MS: u64 = 5000;

/// Whether `command` contains a runtime-resolved `` `u{HEX} `` codepoint escape
/// (claude-code `` /`u\{[0-9A-Fa-f]/ ``): a backtick, `u`, `{`, then a hex digit.
/// Such a command cannot be statically validated and passes through.
#[must_use]
pub fn has_unicode_codepoint_escape(command: &str) -> bool {
    let b = command.as_bytes();
    let mut i = 0;
    while i + 4 <= b.len() {
        if b[i] == b'`' && b[i + 1] == b'u' && b[i + 2] == b'{' && b[i + 3].is_ascii_hexdigit() {
            return true;
        }
        i += 1;
    }
    false
}

/// The GAg pre-check (claude-code): return `Some(passthrough)` when the command
/// exceeds the byte cap or contains a `` `u{HEX} `` escape — in both cases
/// claude-code returns an invalid parse (passthrough) WITHOUT spawning `pwsh`.
#[must_use]
pub fn parse_precheck(command: &str) -> Option<ParseResult> {
    if command.len() > PWSH_MAX_COMMAND_BYTES {
        return Some(ParseResult::invalid(format!(
            "PowerShell parser precheck rejected command longer than {PWSH_MAX_COMMAND_BYTES} bytes"
        )));
    }
    if has_unicode_codepoint_escape(command) {
        return Some(ParseResult::invalid(
            "PowerShell parser precheck rejected unsupported `u{...}` escape",
        ));
    }
    None
}

/// The per-attempt `pwsh` timeout (claude-code `RAg`): env override
/// (`LINGXI_PWSH_PARSE_TIMEOUT_MS`, then `CLAUDE_CODE_PWSH_PARSE_TIMEOUT_MS`) when
/// it parses to a positive integer, else [`PWSH_PARSE_DEFAULT_TIMEOUT_MS`].
fn pwsh_parse_timeout() -> std::time::Duration {
    let ms = [
        "LINGXI_PWSH_PARSE_TIMEOUT_MS",
        "CLAUDE_CODE_PWSH_PARSE_TIMEOUT_MS",
    ]
    .iter()
    .find_map(|k| std::env::var(k).ok())
    .and_then(|v| v.trim().parse::<u64>().ok())
    .filter(|&t| t > 0)
    .unwrap_or(PWSH_PARSE_DEFAULT_TIMEOUT_MS);
    std::time::Duration::from_millis(ms)
}

/// Outcome of a bounded `pwsh` run for one executable name.
enum PwshRun {
    /// Exited 0; the captured stdout.
    Success(Vec<u8>),
    /// The executable was not found on PATH — the caller tries the next name.
    NotFound,
    /// Spawned but failed (non-zero exit, timeout after all attempts, or I/O).
    Failed,
}

/// Spawn `exe` with the encoded command, draining stdout on a background thread
/// (so a full pipe cannot deadlock the child) and killing the child if it does
/// not exit within `timeout`. Retries up to [`PWSH_PARSE_ATTEMPTS`] on
/// timeout/failure; a first-attempt "not found" returns immediately.
fn run_pwsh(exe: &str, encoded: &str, timeout: std::time::Duration) -> PwshRun {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    for _ in 0..PWSH_PARSE_ATTEMPTS {
        let spawned = Command::new(exe)
            .args(PWSH_ARGS)
            .arg(encoded)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            // Executable absent → let the caller try the next name (matches the
            // port's prior pwsh→powershell fallback). Never retried.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PwshRun::NotFound,
            Err(_) => return PwshRun::Failed,
        };
        // Drain stdout on a thread so the child can't block on a full pipe.
        let reader = child.stdout.take().map(|mut out| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = out.read_to_end(&mut buf);
                buf
            })
        });
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let buf = reader.and_then(|r| r.join().ok()).unwrap_or_default();
                    if status.success() {
                        return PwshRun::Success(buf);
                    }
                    // Non-zero exit → retry (next loop iteration).
                    break;
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = reader.and_then(|r| r.join().ok());
                        // Timed out → retry.
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.and_then(|r| r.join().ok());
                    return PwshRun::Failed;
                }
            }
        }
    }
    PwshRun::Failed
}

/// The production [`PwshParser`]: pre-check the byte cap and `` `u{HEX} `` escape,
/// resolve `pwsh` (then `powershell`), run the embedded parse script via
/// `-EncodedCommand` under a timeout with a 2-attempt retry, and transform the
/// JSON AST. Any failure (oversized command, `u{}` escape, executable absent,
/// timeout, non-zero exit, unparsed) → `valid=false` (passthrough). Wire this at
/// the engine boot site on hosts that have PowerShell; leaving the gate's parser
/// `None` keeps containment inert.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemPwshParser;

impl PwshParser for SystemPwshParser {
    fn parse(&self, command: &str) -> ParseResult {
        // GAg pre-checks: oversized command / `u{HEX}` escape → passthrough, no spawn.
        if let Some(passthrough) = parse_precheck(command) {
            return passthrough;
        }
        let script = build_pwsh_script(command);
        let encoded = encode_for_pwsh(&script);
        let timeout = pwsh_parse_timeout();
        for exe in ["pwsh", "powershell"] {
            match run_pwsh(exe, &encoded, timeout) {
                PwshRun::Success(stdout) => {
                    return parse_ps_ast_json(&String::from_utf8_lossy(&stdout));
                }
                // Resolved but failed/timed out after all attempts. Surface an
                // explicit invalid signal so the permission gate can fail closed.
                PwshRun::Failed => {
                    return ParseResult::invalid("PowerShell parser execution failed");
                }
                // Executable not found → try the next name.
                PwshRun::NotFound => {}
            }
        }
        ParseResult::default()
    }
}

#[cfg(test)]
#[path = "powershell_parse_test.rs"]
mod powershell_parse_test;
