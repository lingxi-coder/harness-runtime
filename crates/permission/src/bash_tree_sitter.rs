//! tree-sitter-bash AST helpers for the bash safety layer. Ports the parts of
//! claude-code `src/utils/bash/treeSitterAnalysis.ts` that `bashSecurity.ts`
//! actually consumes. Compiled only under the `bash-ast` feature.

use tree_sitter::{Node, Parser, Tree};

/// Max command length the AST path will parse, mirroring Native 2.1.291's
/// `utils/bash/parser.ts` `command.length > MAX_COMMAND_LENGTH` check. JavaScript
/// string length counts UTF-16 code units, so use `encode_utf16().count()` below
/// rather than Rust UTF-8 bytes or Unicode scalar values. Over-length (and empty)
/// commands return `None`, matching Native's parser wrapper.
const MAX_COMMAND_LENGTH: usize = 10_000;

/// Parse `command` as bash. `None` when the command is empty / over the length
/// cap (legacy fallback, claude-code `parser.ts:59`), the parser can't be built,
/// or the parse fails outright. A successful-but-error tree still returns `Some`.
///
/// Acknowledged divergence: claude-code also fail-CLOSES on a parse abort
/// (`PARSE_TIMEOUT_MS=50` / `MAX_NODES=50_000` → `PARSE_ABORTED`). tree-sitter-bash
/// here has no node/time budget; a pathological parse runs to completion and
/// yields a normal tree. Low risk (tree-sitter-bash is robust); a budget could be
/// added later via the pinned parser's timeout API.
fn parse(command: &str) -> Option<Tree> {
    parse_raw(command)
}

/// Crate-internal `parseCommandRaw` (claude-code `parser.ts`): build a
/// tree-sitter-bash parser and parse `command`. `None` when the command is empty
/// / over the `MAX_COMMAND_LENGTH` cap (legacy fallback, `parser.ts:59`), the
/// parser can't be built, or the parse fails outright; a successful-but-ERROR
/// tree still returns `Some`. Shared by both this module and
/// [`crate::bash_ast_security`] so there is a single `Parser::new()` /
/// `set_language` setup. The returned [`Tree`] OWNS the parse; callers must keep
/// it alive for as long as they borrow [`Node`]s from `tree.root_node()`.
///
/// Acknowledged divergence (see [`parse`]): no node/time budget, so the
/// `PARSE_ABORTED` distinction is unreachable here.
#[must_use]
pub(crate) fn parse_raw(command: &str) -> Option<Tree> {
    if command.is_empty() || command.encode_utf16().count() > MAX_COMMAND_LENGTH {
        return None;
    }
    parse_source(command)
}

/// Parse an internal syntax completion while applying the input cap to the
/// caller's original command only. The suffix is produced from AST terminals,
/// never accepted from a tool caller, and does not increase the Native raw-input
/// length. The original command must still be non-empty and within the existing
/// UTF-16 cap.
#[must_use]
pub(crate) fn parse_raw_with_internal_suffix(
    original_command: &str,
    suffix: &str,
) -> Option<(Tree, String)> {
    if original_command.is_empty() || original_command.encode_utf16().count() > MAX_COMMAND_LENGTH {
        return None;
    }
    let completed_source = format!("{original_command}{suffix}");
    let tree = parse_source(&completed_source)?;
    Some((tree, completed_source))
}

fn parse_source(command: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_bash::LANGUAGE.into();
    parser.set_language(&language).ok()?;
    parser.parse(command, None)
}

/// Iterate ALL children of `node` (named + anonymous operator tokens). One
/// cursor per node (tree-sitter requirement).
fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

/// Does the AST contain a real `;`/`&&`/`||`/`list` node? Ports
/// `treeSitterAnalysis.ts` `hasActualOperatorNodes`. `\;` parses as a `word`
/// (not a `;` node), so `find -exec \;` returns `Some(false)`.
/// `None` => no tree (empty / over the `MAX_COMMAND_LENGTH` cap / parser
/// unavailable) => caller keeps the legacy regex behavior.
#[must_use]
pub fn has_actual_operator_nodes(command: &str) -> Option<bool> {
    let tree = parse(command)?;
    Some(walk_has_operator(tree.root_node()))
}

fn walk_has_operator(node: Node) -> bool {
    matches!(node.kind(), ";" | "&&" | "||" | "list")
        || children(node).into_iter().any(walk_has_operator)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_operators_detected() {
        assert_eq!(has_actual_operator_nodes("echo hi ; ls"), Some(true));
        assert_eq!(has_actual_operator_nodes("a && b"), Some(true));
        assert_eq!(has_actual_operator_nodes("a || b"), Some(true));
    }

    #[test]
    fn escaped_semicolon_is_not_an_operator() {
        assert_eq!(
            has_actual_operator_nodes(r"find . -exec rm {} \;"),
            Some(false)
        );
        assert_eq!(
            has_actual_operator_nodes("cat safe.txt \\; echo secret"),
            Some(false)
        );
    }

    #[test]
    fn plain_command_has_no_operators() {
        assert_eq!(has_actual_operator_nodes("ls -la"), Some(false));
    }

    #[test]
    fn length_limit_counts_native_utf16_code_units() {
        let bmp_at_limit = "\u{e9}".repeat(MAX_COMMAND_LENGTH);
        assert_eq!(bmp_at_limit.len(), MAX_COMMAND_LENGTH * 2);
        assert!(parse_raw(&bmp_at_limit).is_some());

        let astral_at_limit = "\u{1f600}".repeat(MAX_COMMAND_LENGTH / 2);
        assert_eq!(astral_at_limit.encode_utf16().count(), MAX_COMMAND_LENGTH);
        assert!(parse_raw(&astral_at_limit).is_some());

        let astral_over_limit = "\u{1f600}".repeat(MAX_COMMAND_LENGTH / 2 + 1);
        assert!(astral_over_limit.chars().count() < MAX_COMMAND_LENGTH);
        assert_eq!(
            astral_over_limit.encode_utf16().count(),
            MAX_COMMAND_LENGTH + 2
        );
        assert!(parse_raw(&astral_over_limit).is_none());
    }

    #[test]
    fn pipeline_without_logical_ops_has_no_list_operators() {
        assert_eq!(has_actual_operator_nodes("cat x | grep y"), Some(false));
    }

    #[test]
    fn empty_and_over_length_return_none() {
        // claude-code parser.ts:59 returns null (legacy fallback) for empty or
        // > MAX_COMMAND_LENGTH commands; we mirror that with None so the caller
        // keeps the legacy regex path (and still flags e.g. a hidden `\;`).
        assert_eq!(has_actual_operator_nodes(""), None);
        let over = format!("echo {} \\; rm -rf /tmp/x", "a".repeat(20_000));
        assert_eq!(has_actual_operator_nodes(&over), None);
        // Exactly at the cap still parses (no operators here).
        assert_eq!(has_actual_operator_nodes(&"a".repeat(10_000)), Some(false));
    }
}

// ---------------------------------------------------------------------------
// Quote-context extraction. Ports the `extractQuoteContext` half of
// claude-code `src/utils/bash/treeSitterAnalysis.ts`. tree-sitter offsets
// (start_byte/end_byte) are BYTE offsets, so every span is a byte range into
// command.as_bytes(); final strings are rebuilt via String::from_utf8_lossy.
// ---------------------------------------------------------------------------

/// Quote spans collected in one DFS (TS `QuoteSpans`). Each `(start,end)` is a
/// half-open byte range. `raw`=single-quoted, `ansi_c`=`$'…'` (span INCLUDES the
/// leading `$`), `double`=`"…"` outermost-only, `heredoc`=QUOTED heredoc only.
#[derive(Default)]
struct QuoteSpans {
    raw: Vec<(usize, usize)>,
    ansi_c: Vec<(usize, usize)>,
    double: Vec<(usize, usize)>,
    heredoc: Vec<(usize, usize)>,
}

/// Single-pass DFS collecting every quote span (TS `collectQuoteSpans`):
/// `raw_string/ansi_c_string` push + RETURN; string pushes outermost only then
/// recurses `in_double=true` then RETURN; quoted `heredoc_redirect` (first
/// `heredoc_start` byte is ' " or \\) pushes + RETURN, else falls through.
fn collect_quote_spans(node: Node<'_>, src: &[u8], out: &mut QuoteSpans, in_double: bool) {
    let span = (node.start_byte(), node.end_byte());
    match node.kind() {
        "raw_string" => {
            out.raw.push(span);
            return;
        }
        "ansi_c_string" => {
            out.ansi_c.push(span);
            return;
        }
        "string" => {
            if !in_double {
                out.double.push(span);
            }
            for child in children(node) {
                collect_quote_spans(child, src, out, true);
            }
            return;
        }
        "heredoc_redirect" => {
            let mut is_quoted = false;
            for child in children(node) {
                if child.kind() == "heredoc_start" {
                    let first = src.get(child.start_byte()).copied();
                    is_quoted = matches!(first, Some(b'\'' | b'"' | b'\\'));
                    break;
                }
            }
            if is_quoted {
                out.heredoc.push(span);
                return;
            }
        }
        _ => {}
    }
    for child in children(node) {
        collect_quote_spans(child, src, out, in_double);
    }
}

/// Set of every byte position covered by `spans` (TS `buildPositionSet`).
fn build_position_set(spans: &[(usize, usize)]) -> std::collections::HashSet<usize> {
    let mut set = std::collections::HashSet::new();
    for &(start, end) in spans {
        for i in start..end {
            set.insert(i);
        }
    }
    set
}

/// Drop spans strictly contained within another (TS `dropContainedSpans`).
/// Equal-extent duplicates both survive. Generic via a `bounds` closure.
fn drop_contained_spans<T: Clone>(spans: &[T], bounds: impl Fn(&T) -> (usize, usize)) -> Vec<T> {
    spans
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            let (ss, se) = bounds(s);
            !spans.iter().enumerate().any(|(j, other)| {
                if j == *i {
                    return false;
                }
                let (os, oe) = bounds(other);
                os <= ss && oe >= se && (os < ss || oe > se)
            })
        })
        .map(|(_, s)| s.clone())
        .collect()
}

/// Remove `spans` from `command` (TS `removeSpans`): drop contained, sort by
/// start descending, splice out.
fn remove_spans(command: &str, spans: &[(usize, usize)]) -> String {
    if spans.is_empty() {
        return command.to_string();
    }
    let mut sorted: Vec<(usize, usize)> = drop_contained_spans(spans, |&(s, e)| (s, e));
    sorted.sort_by(|a, b| b.0.cmp(&a.0));
    let mut bytes = command.as_bytes().to_vec();
    for &(start, end) in &sorted {
        bytes.drain(start..end);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Replace each span's content with `open + close` (TS `replaceSpansKeepQuotes`).
fn replace_spans_keep_quotes(command: &str, spans: &[(usize, usize, String, String)]) -> String {
    if spans.is_empty() {
        return command.to_string();
    }
    let mut sorted = drop_contained_spans(spans, |s| (s.0, s.1));
    sorted.sort_by(|a, b| b.0.cmp(&a.0));
    let mut bytes = command.as_bytes().to_vec();
    for (start, end, open, close) in &sorted {
        let mut repl = Vec::with_capacity(open.len() + close.len());
        repl.extend_from_slice(open.as_bytes());
        repl.extend_from_slice(close.as_bytes());
        bytes.splice(*start..*end, repl);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Three quote-stripped views of a command (TS `QuoteContext`).
pub struct QuoteContext {
    /// Single-quoted/ANSI-C/heredoc content removed and double-quote delimiters
    /// dropped, but content inside double quotes preserved.
    pub with_double_quotes: String,
    /// All quoted spans (single, ANSI-C, double, quoted heredoc) removed entirely.
    pub fully_unquoted: String,
    /// All quoted spans emptied but their surrounding quote characters kept.
    pub unquoted_keep_quote_chars: String,
}

/// Extract quote context from the AST (TS `extractQuoteContext`). `None` when
/// the command is empty / over the length cap / unparseable.
#[must_use]
pub fn extract_quote_context(command: &str) -> Option<QuoteContext> {
    let tree = parse(command)?;
    let src = command.as_bytes();

    let mut spans = QuoteSpans::default();
    collect_quote_spans(tree.root_node(), src, &mut spans, false);

    let mut single_quote_positions: Vec<(usize, usize)> = Vec::new();
    single_quote_positions.extend_from_slice(&spans.raw);
    single_quote_positions.extend_from_slice(&spans.ansi_c);
    single_quote_positions.extend_from_slice(&spans.heredoc);
    let single_quote_set = build_position_set(&single_quote_positions);
    let mut double_quote_delim_set = std::collections::HashSet::new();
    for &(start, end) in &spans.double {
        double_quote_delim_set.insert(start);
        double_quote_delim_set.insert(end - 1);
    }
    let mut with_double_bytes: Vec<u8> = Vec::with_capacity(src.len());
    for (i, &b) in src.iter().enumerate() {
        if single_quote_set.contains(&i) || double_quote_delim_set.contains(&i) {
            continue;
        }
        with_double_bytes.push(b);
    }
    let with_double_quotes = String::from_utf8_lossy(&with_double_bytes).into_owned();

    let mut all_quote_spans: Vec<(usize, usize)> = Vec::new();
    all_quote_spans.extend_from_slice(&spans.raw);
    all_quote_spans.extend_from_slice(&spans.ansi_c);
    all_quote_spans.extend_from_slice(&spans.double);
    all_quote_spans.extend_from_slice(&spans.heredoc);
    let fully_unquoted = remove_spans(command, &all_quote_spans);

    let mut swap: Vec<(usize, usize, String, String)> = Vec::new();
    for &(s, e) in &spans.raw {
        swap.push((s, e, "'".to_string(), "'".to_string()));
    }
    for &(s, e) in &spans.ansi_c {
        swap.push((s, e, "$'".to_string(), "'".to_string()));
    }
    for &(s, e) in &spans.double {
        swap.push((s, e, "\"".to_string(), "\"".to_string()));
    }
    for &(s, e) in &spans.heredoc {
        swap.push((s, e, String::new(), String::new()));
    }
    let unquoted_keep_quote_chars = replace_spans_keep_quotes(command, &swap);

    Some(QuoteContext {
        with_double_quotes,
        fully_unquoted,
        unquoted_keep_quote_chars,
    })
}

// ---------------------------------------------------------------------------
// Dangerous-pattern flags. Ports `extractDangerousPatterns`
// (`treeSitterAnalysis.ts:448`): a single DFS setting a flag per node kind.
// ---------------------------------------------------------------------------

/// AST dangerous-pattern flags (TS `DangerousPatterns`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DangerousPatterns {
    /// `$(…)` / backtick command substitution (`command_substitution`).
    pub has_command_substitution: bool,
    /// `<(…)` / `>(…)` process substitution (`process_substitution`).
    pub has_process_substitution: bool,
    /// `${…}` parameter expansion (`expansion`).
    pub has_parameter_expansion: bool,
    /// A heredoc (`heredoc_redirect`).
    pub has_heredoc: bool,
    /// A `# …` comment (`comment`).
    pub has_comment: bool,
}

/// Extract dangerous-pattern flags from the AST (TS `extractDangerousPatterns`).
/// `None` when the command is empty / over the length cap / unparseable (the
/// caller keeps the legacy path).
#[must_use]
pub fn extract_dangerous_patterns(command: &str) -> Option<DangerousPatterns> {
    let tree = parse(command)?;
    let mut out = DangerousPatterns::default();
    walk_dangerous(tree.root_node(), &mut out);
    Some(out)
}

fn walk_dangerous(node: Node<'_>, out: &mut DangerousPatterns) {
    match node.kind() {
        "command_substitution" => out.has_command_substitution = true,
        "process_substitution" => out.has_process_substitution = true,
        "expansion" => out.has_parameter_expansion = true,
        "heredoc_redirect" => out.has_heredoc = true,
        "comment" => out.has_comment = true,
        _ => {}
    }
    for child in children(node) {
        walk_dangerous(child, out);
    }
}

// ---------------------------------------------------------------------------
// Compound-structure extraction (the AST command splitter). Ports
// `extractCompoundStructure` (`treeSitterAnalysis.ts:296`) — the tree-sitter
// replacement for the heuristic `splitCommand`. The TS `walkTopLevel`'s
// `walkTopLevel({...node, children:[x]})` reconstruction is rendered here as a
// direct call to [`process_top_level_child`] on the single child `x` (it just
// means "process `x` as a top-level child").
// ---------------------------------------------------------------------------

/// AST compound-command structure (TS `CompoundStructure`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompoundStructure {
    /// Has top-level `&&`/`||`/`;` operators (TS `operators.length > 0`).
    pub has_compound_operators: bool,
    /// Has a pipeline (`pipeline`).
    pub has_pipeline: bool,
    /// Has a subshell (`subshell`).
    pub has_subshell: bool,
    /// Has a command group `{ … }` (`compound_statement`).
    pub has_command_group: bool,
    /// Top-level operator tokens found, in order (`&&` / `||` / `;`).
    pub operators: Vec<String>,
    /// Command segments split by the compound operators.
    pub segments: Vec<String>,
}

/// Extract compound structure from the AST (TS `extractCompoundStructure`).
/// `None` when the command is empty / over the length cap / unparseable.
#[must_use]
pub fn extract_compound_structure(command: &str) -> Option<CompoundStructure> {
    let tree = parse(command)?;
    let src = command.as_bytes();
    let mut acc = CompoundStructure::default();
    for child in children(tree.root_node()) {
        process_top_level_child(child, src, &mut acc);
    }
    // TS: "If no segments found, the whole command is one segment."
    if acc.segments.is_empty() {
        acc.segments.push(command.to_string());
    }
    acc.has_compound_operators = !acc.operators.is_empty();
    Some(acc)
}

/// The node's source text (TS `node.text`).
fn node_text(node: Node<'_>, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

/// Per-top-level-child arm of TS `walkTopLevel` (`treeSitterAnalysis.ts:308`).
fn process_top_level_child(child: Node<'_>, src: &[u8], acc: &mut CompoundStructure) {
    match child.kind() {
        "list" => {
            for lc in children(child) {
                match lc.kind() {
                    "&&" | "||" => acc.operators.push(lc.kind().to_string()),
                    // Nested list / redirected_statement wrapping a list|pipeline:
                    // recurse so inner operators/pipelines are detected.
                    "list" | "redirected_statement" => process_top_level_child(lc, src, acc),
                    "pipeline" => {
                        acc.has_pipeline = true;
                        acc.segments.push(node_text(lc, src));
                    }
                    "subshell" => {
                        acc.has_subshell = true;
                        acc.segments.push(node_text(lc, src));
                    }
                    "compound_statement" => {
                        acc.has_command_group = true;
                        acc.segments.push(node_text(lc, src));
                    }
                    _ => acc.segments.push(node_text(lc, src)),
                }
            }
        }
        ";" => acc.operators.push(";".to_string()),
        "pipeline" => {
            acc.has_pipeline = true;
            acc.segments.push(node_text(child, src));
        }
        "subshell" => {
            acc.has_subshell = true;
            acc.segments.push(node_text(child, src));
        }
        "compound_statement" => {
            acc.has_command_group = true;
            acc.segments.push(node_text(child, src));
        }
        "command" | "declaration_command" | "variable_assignment" => {
            acc.segments.push(node_text(child, src));
        }
        "redirected_statement" => {
            // Recurse into the non-redirect body (TS skips `file_redirect`).
            let mut found_inner = false;
            for inner in children(child) {
                if inner.kind() == "file_redirect" {
                    continue;
                }
                found_inner = true;
                process_top_level_child(inner, src, acc);
            }
            if !found_inner {
                acc.segments.push(node_text(child, src));
            }
        }
        "negated_command" => {
            acc.segments.push(node_text(child, src));
            for c in children(child) {
                process_top_level_child(c, src, acc);
            }
        }
        "if_statement"
        | "while_statement"
        | "for_statement"
        | "case_statement"
        | "function_definition" => {
            acc.segments.push(node_text(child, src));
            for c in children(child) {
                process_top_level_child(c, src, acc);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod analysis_tests {
    use super::*;

    #[test]
    fn dangerous_patterns_detected_per_kind() {
        let d = extract_dangerous_patterns("echo $(whoami)").unwrap();
        assert!(d.has_command_substitution && !d.has_process_substitution);
        assert!(
            extract_dangerous_patterns("cat <(ls)")
                .unwrap()
                .has_process_substitution
        );
        assert!(
            extract_dangerous_patterns("echo ${HOME}")
                .unwrap()
                .has_parameter_expansion
        );
        assert!(
            extract_dangerous_patterns("cat <<EOF\nx\nEOF")
                .unwrap()
                .has_heredoc
        );
        assert!(
            extract_dangerous_patterns("echo hi # note")
                .unwrap()
                .has_comment
        );
        // A plain command trips nothing.
        assert_eq!(
            extract_dangerous_patterns("ls -la").unwrap(),
            DangerousPatterns::default()
        );
        assert_eq!(extract_dangerous_patterns(""), None);
    }

    #[test]
    fn compound_operators_and_segments() {
        let c = extract_compound_structure("echo a && echo b").unwrap();
        assert!(c.has_compound_operators);
        assert_eq!(c.operators, vec!["&&".to_string()]);
        assert_eq!(c.segments, vec!["echo a".to_string(), "echo b".to_string()]);

        let s = extract_compound_structure("echo a ; echo b").unwrap();
        assert_eq!(s.operators, vec![";".to_string()]);
        assert_eq!(s.segments.len(), 2);

        let pipe = extract_compound_structure("cat x | grep y").unwrap();
        assert!(pipe.has_pipeline && !pipe.has_compound_operators);
        assert_eq!(pipe.segments, vec!["cat x | grep y".to_string()]);
    }

    #[test]
    fn subshell_and_command_group_and_single() {
        assert!(extract_compound_structure("(echo a)").unwrap().has_subshell);
        assert!(
            extract_compound_structure("{ echo a; }")
                .unwrap()
                .has_command_group
        );
        // A single plain command yields exactly one segment = the whole command.
        let one = extract_compound_structure("echo hello world").unwrap();
        assert!(!one.has_compound_operators && !one.has_pipeline);
        assert_eq!(one.segments, vec!["echo hello world".to_string()]);
        assert_eq!(extract_compound_structure(""), None);
    }

    #[test]
    fn redirected_compound_recurses_to_inner_operators() {
        // `cmd1 && cmd2 2>/dev/null` — tree-sitter wraps the list in a
        // redirected_statement; the inner `&&` must still be detected.
        let c = extract_compound_structure("echo a && echo b 2>/dev/null").unwrap();
        assert!(
            c.has_compound_operators,
            "inner && detected through redirect"
        );
        assert_eq!(c.operators, vec!["&&".to_string()]);
    }
}

#[cfg(test)]
mod quote_context_tests {
    use super::*;

    fn ctx(cmd: &str) -> QuoteContext {
        extract_quote_context(cmd).expect("parseable command")
    }

    #[test]
    fn plain_command_unchanged() {
        let c = ctx("echo hello world");
        assert_eq!(c.with_double_quotes, "echo hello world");
        assert_eq!(c.fully_unquoted, "echo hello world");
        assert_eq!(c.unquoted_keep_quote_chars, "echo hello world");
    }

    #[test]
    fn single_quotes_removed_double_path_and_fully() {
        let c = ctx("echo 'a;b'");
        assert_eq!(c.with_double_quotes, "echo ");
        assert_eq!(c.fully_unquoted, "echo ");
        assert_eq!(c.unquoted_keep_quote_chars, "echo ''");
    }

    #[test]
    fn double_quotes_content_kept_for_with_double_delims_dropped() {
        let c = ctx(r#"echo "a;b""#);
        assert_eq!(c.with_double_quotes, "echo a;b");
        assert_eq!(c.fully_unquoted, "echo ");
        assert_eq!(c.unquoted_keep_quote_chars, r#"echo """#);
    }

    #[test]
    fn nested_single_inside_double() {
        let c = ctx(r#"echo "$(echo 'hi')""#);
        assert_eq!(c.with_double_quotes, "echo $(echo )");
        assert_eq!(c.fully_unquoted, "echo ");
        assert_eq!(c.unquoted_keep_quote_chars, r#"echo """#);
    }

    #[test]
    fn ansi_c_string_includes_leading_dollar() {
        let c = ctx("echo $'x'");
        assert_eq!(c.with_double_quotes, "echo ");
        assert_eq!(c.fully_unquoted, "echo ");
        assert_eq!(c.unquoted_keep_quote_chars, "echo $''");
    }
}
