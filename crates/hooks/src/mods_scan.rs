//! Conservative source scans for Mod environment-variable grants and the
//! pre-evaluation `plugin.register` uses report. The worker sends type-stripped
//! source from every linked module, so comments and strings are parsed as data
//! rather than searched with a regular expression.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;
use tree_sitter::Node;

#[derive(Default)]
pub(super) struct EnvScan {
    pub reads: HashSet<String>,
    pub writes: HashSet<String>,
}

fn source_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    std::str::from_utf8(&source[node.byte_range()]).unwrap_or("")
}

fn env_method(function: Node<'_>, source: &[u8]) -> Option<&'static str> {
    if function.kind() != "member_expression" {
        return None;
    }
    let method = function.child_by_field_name("property")?;
    let env = function.child_by_field_name("object")?;
    if method.kind() != "property_identifier" || env.kind() != "member_expression" {
        return None;
    }
    let env_name = env.child_by_field_name("property")?;
    let dollar = env.child_by_field_name("object")?;
    if env_name.kind() != "property_identifier"
        || identifier_name(env_name, source).as_deref() != Some("env")
        || dollar.kind() != "identifier"
        || identifier_name(dollar, source).as_deref() != Some("$")
    {
        return None;
    }
    match identifier_name(method, source).as_deref() {
        Some("get") => Some("get"),
        Some("set") => Some("set"),
        _ => None,
    }
}

fn api_method(
    function: Node<'_>,
    source: &[u8],
    dollar_names: &HashSet<String>,
) -> Option<(String, String)> {
    if function.kind() != "member_expression" {
        return None;
    }
    let method = function.child_by_field_name("property")?;
    let noun = function.child_by_field_name("object")?;
    if method.kind() != "property_identifier" || noun.kind() != "member_expression" {
        return None;
    }
    let noun_name = noun.child_by_field_name("property")?;
    let dollar = noun.child_by_field_name("object")?;
    let noun_name = identifier_name(noun_name, source)?;
    let method = identifier_name(method, source)?;
    (dollar.kind() == "identifier"
        && identifier_name(dollar, source).is_some_and(|name| dollar_names.contains(&name)))
    .then_some((noun_name, method))
}

fn api_namespace(node: Node<'_>, source: &[u8], dollar_names: &HashSet<String>) -> Option<String> {
    if node.kind() != "member_expression" {
        return None;
    }
    let namespace = node.child_by_field_name("property")?;
    let dollar = node.child_by_field_name("object")?;
    let namespace = identifier_name(namespace, source)?;
    (dollar.kind() == "identifier"
        && identifier_name(dollar, source).is_some_and(|name| dollar_names.contains(&name)))
    .then_some(namespace)
}

fn namespace_is_direct_call(node: Node<'_>) -> bool {
    let Some(method) = node.parent() else {
        return false;
    };
    if method.kind() != "member_expression"
        || method.child_by_field_name("object") != Some(node)
        || method
            .child_by_field_name("property")
            .is_none_or(|property| property.kind() != "property_identifier")
    {
        return false;
    }
    method.parent().is_some_and(|call| {
        call.kind() == "call_expression" && call.child_by_field_name("function") == Some(method)
    })
}

fn scan_refusal(source: &[u8], node: Node<'_>, message: &str) -> String {
    let line = source_line_number(source, node);
    format!("plugin.register scan refused at line {line}: {message}")
}

fn source_line_number(source: &[u8], node: Node<'_>) -> usize {
    source[..node.start_byte().min(source.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn literal_name(argument: Node<'_>, source: &[u8]) -> Option<String> {
    String::from_utf16(&literal_name_units(argument, source)?).ok()
}

fn literal_name_units(argument: Node<'_>, source: &[u8]) -> Option<Vec<u16>> {
    super::client_source::js_string_units(argument, source)
        .ok()
        .flatten()
}

fn module_export_name_units(node: Node<'_>, source: &[u8]) -> Option<Vec<u16>> {
    match node.kind() {
        "identifier" | "property_identifier" => {
            Some(identifier_name(node, source)?.encode_utf16().collect())
        }
        "string" => literal_name_units(node, source),
        _ => None,
    }
}

fn variable_declaration_kind(node: Node<'_>, source: &[u8]) -> Option<&'static str> {
    match node.kind() {
        "variable_declaration" => Some("var"),
        "lexical_declaration" => {
            let mut cursor = node.walk();
            let declaration_kind =
                node.children(&mut cursor)
                    .find_map(|child| match source_text(child, source) {
                        "const" => Some("const"),
                        "let" => Some("let"),
                        _ => None,
                    });
            declaration_kind
        }
        _ => None,
    }
}

fn identifier_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    super::client_source::js_identifier_name(node, source)
}

fn event_pattern_error(pattern: &str) -> Option<String> {
    let (negated, positive) = pattern
        .strip_prefix('!')
        .map_or((false, pattern), |positive| (true, positive));
    if negated && positive == "*" {
        return Some(format!(
            "\"{pattern}\" is not a pattern: it selects no event"
        ));
    }
    if negated && positive.starts_with('!') {
        return Some(format!(
            "\"{pattern}\" is not a pattern: \"!\" negates once"
        ));
    }
    if positive == "*" {
        return None;
    }
    let wildcard = positive.ends_with(".*");
    let body = if wildcard {
        positive.strip_suffix(".*").unwrap_or_default()
    } else {
        positive
    };
    if body.is_empty() || !body.split('.').all(is_event_identifier) {
        return Some(format!(
            "\"{pattern}\" is not an event name or a pattern (\"*\", \"<prefix>.*\", or \"!\" before either)"
        ));
    }
    if wildcard {
        return None;
    }
    if native_event_name_295(positive) {
        return None;
    }
    let mut parts = positive.split('.');
    let namespace = parts.next().unwrap_or_default();
    let custom_name = parts.next().is_some() && parts.next().is_none();
    let known_namespace = NATIVE_EVENT_NAMES_295
        .iter()
        .filter_map(|event| event.split_once('.').map(|(namespace, _)| namespace))
        .any(|known| known == namespace);
    (!custom_name || known_namespace).then(|| format!("\"{positive}\" is not an event"))
}

const NATIVE_EVENT_NAMES_295: &[&str] = &[
    "classic.PreToolUse",
    "classic.PostToolUse",
    "classic.PostToolUseFailure",
    "classic.PostToolBatch",
    "classic.Notification",
    "classic.UserPromptSubmit",
    "classic.UserPromptExpansion",
    "classic.SessionStart",
    "classic.SessionEnd",
    "classic.Stop",
    "classic.StopFailure",
    "classic.SubagentStart",
    "classic.SubagentStop",
    "classic.PreCompact",
    "classic.PostCompact",
    "classic.PreModelSwitch",
    "classic.PostModelSwitch",
    "classic.PermissionRequest",
    "classic.PermissionDenied",
    "classic.Setup",
    "classic.TeammateIdle",
    "classic.TaskCreated",
    "classic.TaskCompleted",
    "classic.Elicitation",
    "classic.ElicitationResult",
    "classic.ConfigChange",
    "classic.WorktreeCreate",
    "classic.WorktreeRemove",
    "classic.InstructionsLoaded",
    "classic.CwdChanged",
    "classic.FileChanged",
    "classic.DirectoryAdded",
    "classic.MessageDisplay",
    "tool.call",
    "tool.check",
    "ui.render",
    "ui.resolve",
    "ui.press",
    "ui.input",
    "ui.select",
    "ui.message",
    "ui.fault",
    "ui.scroll",
    "ui.focus",
    "agent.offer",
    "agent.spawn",
    "prompt.submit",
    "prompt.fill",
    "prompt.suggest",
    "prompt.edit",
    "prompt.autocomplete",
    "prompt.section",
    "prompt.context",
    "prompt.attachment",
    "prompt.mention",
    "prompt.compose",
    "tool.describe",
    "command.run",
    "command.describe",
    "config.set",
    "config.describe",
    "telemetry.log",
    "telemetry.mark",
    "skill.prompt",
    "attribution.text",
    "session.start",
    "session.receive",
    "session.append",
    "session.send",
    "session.compact",
    "session.attach",
    "session.detach",
    "session.measure",
    "session.end",
    "plugin.register",
    "turn.start",
    "turn.step",
    "turn.complete",
    "engine.create",
    "model.complete",
    "model.classify",
    "model.fork",
    "audio.play",
    "audio.speak",
    "mcp.call",
    "mcp.connect",
    "session.cwd",
    "session.root",
    "session.model",
    "session.turns",
    "session.id",
    "session.messages",
    "session.repo",
    "session.surface",
    "session.surfaces",
    "session.authorize",
    "session.usage",
    "session.version",
    "turn.abort",
    "prompt.read",
    "flag.value",
    "tool.list",
    "tool.register",
    "command.list",
    "command.register",
    "config.list",
    "agent.list",
    "agent.register",
    "ui.toast",
    "ui.status",
    "ui.log",
    "ui.notice",
    "ui.invalidate",
    "ui.open",
    "ui.close",
    "ui.panes",
    "ui.selection",
    "ui.copy",
    "ui.notify",
    "ui.blit",
    "fs.read",
    "fs.write",
    "fs.list",
    "fs.exists",
    "fs.stat",
    "fs.ancestors",
    "store.get",
    "store.set",
    "store.delete",
    "store.keys",
    "state.get",
    "state.set",
    "clock.now",
    "clock.sleep",
    "clock.after",
    "clock.every",
    "http.fetch",
    "process.run",
    "process.spawn",
    "settings.read",
    "env.get",
    "env.set",
];

fn native_event_name_295(event: &str) -> bool {
    NATIVE_EVENT_NAMES_295.contains(&event)
}

fn is_event_identifier(part: &str) -> bool {
    static IDENTIFIER: OnceLock<regex::Regex> = OnceLock::new();
    static DEFAULT_IGNORABLE: OnceLock<regex::Regex> = OnceLock::new();
    let identifier = IDENTIFIER.get_or_init(|| {
        regex::Regex::new(r"^[\p{ID_Start}$_][\p{ID_Continue}$]*$")
            .expect("regex crate supports Unicode ID_Start and ID_Continue")
    });
    let default_ignorable = DEFAULT_IGNORABLE.get_or_init(|| {
        regex::Regex::new(r"\p{Default_Ignorable_Code_Point}")
            .expect("regex crate supports Default_Ignorable_Code_Point")
    });
    identifier.is_match(part) && !default_ignorable.is_match(part)
}

pub(super) fn scan_sources(sources: &[String]) -> EnvScan {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .is_err()
    {
        return EnvScan::default();
    }
    let mut scan = EnvScan::default();
    for source in sources {
        let Some(tree) = parser.parse(source, None) else {
            continue;
        };
        if tree.root_node().has_error() {
            continue;
        }
        let bytes = source.as_bytes();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression" {
                if let Some(method) = node
                    .child_by_field_name("function")
                    .and_then(|function| env_method(function, bytes))
                {
                    if let Some(name) = node
                        .child_by_field_name("arguments")
                        .and_then(|arguments| arguments.named_child(0))
                        .and_then(|argument| literal_name(argument, bytes))
                    {
                        match method {
                            "get" => {
                                scan.reads.insert(name);
                            }
                            "set" => {
                                scan.writes.insert(name);
                            }
                            _ => {}
                        }
                    }
                }
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
    }
    scan
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UseBinding {
    On,
    Api,
}

#[derive(Default)]
struct UsesScan {
    events: Vec<String>,
    seen_events: HashSet<String>,
    unmatched_events: HashMap<String, (String, usize)>,
    calls: BTreeSet<String>,
    env_reads: BTreeSet<Vec<u16>>,
    env_writes: BTreeSet<Vec<u16>>,
    state_reads: BTreeSet<(Vec<u16>, Vec<u16>)>,
    state_writes: BTreeSet<(Vec<u16>, Vec<u16>)>,
    visited: HashSet<(String, usize, Vec<(String, u8)>)>,
}

struct SourceGraph<'a> {
    entry: &'a str,
    files: HashMap<&'a str, &'a str>,
    links: HashMap<(&'a str, &'a str), &'a str>,
}

impl<'a> SourceGraph<'a> {
    fn from_prepared(graph: &'a super::client_source::PreparedHookSourceGraph) -> Self {
        Self {
            entry: &graph.entry,
            files: graph
                .files
                .iter()
                .map(|file| (file.file.as_str(), file.source.as_str()))
                .collect(),
            links: graph
                .links
                .iter()
                .map(|link| {
                    (
                        (link.from.as_str(), link.spelled.as_str()),
                        link.file.as_str(),
                    )
                })
                .collect(),
        }
    }

    fn target(&self, importer: &str, specifier: &str) -> Option<&'a str> {
        self.links.get(&(importer, specifier)).copied()
    }
}

#[derive(Clone)]
enum CallbackRef<'tree> {
    Local(Node<'tree>),
    Imported { file: String, export: Vec<u16> },
}

struct ImportedBinding {
    specifier: String,
    export: Vec<u16>,
}

fn is_function(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "arrow_function"
            | "generator_function"
    )
}

fn function_body<'tree>(function: Node<'tree>) -> Option<Node<'tree>> {
    function.child_by_field_name("body")
}

fn function_parameters<'tree>(function: Node<'tree>) -> Vec<Node<'tree>> {
    if let Some(parameter) = function.child_by_field_name("parameter") {
        return vec![parameter];
    }
    function
        .child_by_field_name("parameters")
        .map(|parameters| {
            let mut cursor = parameters.walk();
            parameters.named_children(&mut cursor).collect()
        })
        .unwrap_or_default()
}

fn function_expression<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    if is_function(node) {
        return Some(node);
    }
    None
}

fn add_function<'tree>(
    functions: &mut HashMap<String, Option<Node<'tree>>>,
    name: String,
    function: Node<'tree>,
) {
    match functions.entry(name) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(Some(function));
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            entry.insert(None);
        }
    }
}

fn collect_functions<'tree>(
    root: Node<'tree>,
    source: &[u8],
    functions: &mut HashMap<String, Option<Node<'tree>>>,
) {
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        let declaration = if statement.kind() == "export_statement" {
            statement.child_by_field_name("declaration")
        } else {
            Some(statement)
        };
        let Some(declaration) = declaration else {
            continue;
        };
        if matches!(
            declaration.kind(),
            "function_declaration" | "generator_function_declaration"
        ) {
            if let Some(name) = declaration
                .child_by_field_name("name")
                .and_then(|node| identifier_name(node, source))
            {
                add_function(functions, name, declaration);
            }
        } else if declaration.kind() == "lexical_declaration"
            && variable_declaration_kind(declaration, source) == Some("const")
        {
            let mut declarations = declaration.walk();
            for variable in declaration.named_children(&mut declarations) {
                if variable.kind() != "variable_declarator" {
                    continue;
                }
                let (Some(name), Some(value)) = (
                    variable.child_by_field_name("name"),
                    variable.child_by_field_name("value"),
                ) else {
                    continue;
                };
                if name.kind() == "identifier" {
                    if let (Some(name), Some(function)) =
                        (identifier_name(name, source), function_expression(value))
                    {
                        add_function(functions, name, function);
                    }
                }
            }
        }
    }
}

fn exported_register<'tree>(
    root: Node<'tree>,
    source: &[u8],
) -> Result<Option<Node<'tree>>, String> {
    let mut cursor = root.walk();
    for export in root.named_children(&mut cursor) {
        if export.kind() != "export_statement" {
            continue;
        }
        let declaration = export
            .child_by_field_name("declaration")
            .or_else(|| export.named_child(0));
        let Some(declaration) = declaration else {
            continue;
        };
        if declaration.kind() == "function_declaration" {
            if let Some(name) = declaration.child_by_field_name("name") {
                if identifier_name(name, source).as_deref() == Some("register") {
                    if top_level_binding_is_reassigned(root, "register", source) {
                        return Err(scan_refusal(
                            source,
                            name,
                            "the exported register function is assigned elsewhere",
                        ));
                    }
                    return Ok(Some(declaration));
                }
            }
        }
        if matches!(
            declaration.kind(),
            "lexical_declaration" | "variable_declaration"
        ) {
            let mut vars = declaration.walk();
            for variable in declaration.named_children(&mut vars) {
                if variable.kind() != "variable_declarator" {
                    continue;
                }
                let (Some(name), Some(value)) = (
                    variable.child_by_field_name("name"),
                    variable.child_by_field_name("value"),
                ) else {
                    continue;
                };
                if name.kind() == "identifier"
                    && identifier_name(name, source).as_deref() == Some("register")
                {
                    if top_level_binding_is_reassigned(root, "register", source) {
                        return Err(scan_refusal(
                            source,
                            name,
                            "the exported register function is assigned elsewhere",
                        ));
                    }
                    return Ok(function_expression(value));
                }
            }
        }
    }
    Ok(None)
}

fn state_reference(argument: Node<'_>, source: &[u8]) -> Option<(Vec<u16>, Vec<u16>)> {
    let mut seen = HashSet::new();
    let (plugin, key) = static_state_object(argument, source, &mut seen)?;
    Some((plugin?, key?))
}

fn static_state_object(
    node: Node<'_>,
    source: &[u8],
    seen: &mut HashSet<String>,
) -> Option<(Option<Vec<u16>>, Option<Vec<u16>>)> {
    if node.kind() == "identifier" {
        let name = identifier_name(node, source)?;
        if !seen.insert(name.clone()) {
            return None;
        }
        let initializer = visible_static_initializer(node, &name, source)?;
        let value = static_state_object(initializer, source, seen);
        seen.remove(&name);
        return value;
    }
    if node.kind() != "object" {
        return None;
    }

    let mut plugin = None;
    let mut key = None;
    let mut cursor = node.walk();
    for property in node.named_children(&mut cursor) {
        if property.kind() == "spread_element" {
            let argument = property.named_child(0)?;
            let (spread_plugin, spread_key) = static_state_object(argument, source, seen)?;
            if spread_plugin.is_some() {
                plugin = spread_plugin;
            }
            if spread_key.is_some() {
                key = spread_key;
            }
            continue;
        }
        if property.kind() != "pair" {
            return None;
        }
        let property_key = property.child_by_field_name("key")?;
        let property_value = property.child_by_field_name("value")?;
        let (plugin_field, key_field) = match property_key.kind() {
            "property_identifier" => match identifier_name(property_key, source)?.as_str() {
                "plugin" => (true, false),
                "key" => (false, true),
                _ => (false, false),
            },
            "string" => {
                let name = literal_name_units(property_key, source)?;
                (
                    name.as_slice() == "plugin".encode_utf16().collect::<Vec<_>>(),
                    name.as_slice() == "key".encode_utf16().collect::<Vec<_>>(),
                )
            }
            // Native's property-name reader also accepts numeric Literal
            // keys, but they cannot name either state field.
            "number" => continue,
            _ => return None,
        };
        if plugin_field {
            plugin = Some(literal_name_units(property_value, source)?);
        } else if key_field {
            key = Some(literal_name_units(property_value, source)?);
        }
    }
    Some((plugin, key))
}

/// Resolve only declarations Native's `po` treats as static: `const` in the
/// visible lexical scope, or initialized top-level `var`. A nearer mutable or
/// otherwise dynamic declaration shadows an outer constant and stops lookup.
fn visible_static_initializer<'tree>(
    reference: Node<'tree>,
    name: &str,
    source: &[u8],
) -> Option<Node<'tree>> {
    let mut scope = Some(reference);
    while let Some(node) = scope {
        if is_lexical_scope(node) {
            if let Some(binding) = static_binding_in_scope(node, name, source) {
                return binding;
            }
        }
        scope = node.parent();
    }
    None
}

fn is_lexical_scope(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "program"
            | "statement_block"
            | "for_statement"
            | "for_in_statement"
            | "for_of_statement"
            | "switch_statement"
            | "class_static_block"
            | "function_declaration"
            | "function_expression"
            | "arrow_function"
            | "generator_function"
            | "generator_function_declaration"
            | "catch_clause"
    )
}

/// The outer option says whether this scope declares the name. The inner
/// option carries the initializer only when the declaration is source-static.
fn static_binding_in_scope<'tree>(
    scope: Node<'tree>,
    name: &str,
    source: &[u8],
) -> Option<Option<Node<'tree>>> {
    let mut found: Option<Option<Node<'tree>>> = None;
    {
        let mut consider =
            |binding_name: Option<String>, initializer: Option<Node<'tree>>, is_static: bool| {
                if binding_name.as_deref() == Some(name) {
                    if found.is_some() {
                        found = Some(None);
                    } else {
                        found = Some(if is_static { initializer } else { None });
                    }
                }
            };

        if matches!(
            scope.kind(),
            "function_declaration"
                | "function_expression"
                | "arrow_function"
                | "generator_function"
                | "generator_function_declaration"
        ) {
            if let Some(function_name) = scope.child_by_field_name("name") {
                consider(identifier_name(function_name, source), None, false);
            }
            for parameter in function_parameters(scope) {
                if pattern_mentions_identifier(parameter, name, source) {
                    consider(Some(name.to_owned()), None, false);
                }
            }
            if function_var_declares_name(scope, name, source) {
                // `var` is hoisted to the function scope even when its declaration
                // is nested under an if/block. Native does not treat its
                // initializer as a statically readable function-scope const.
                consider(Some(name.to_owned()), None, false);
            }
        }
        if scope.kind() == "catch_clause" {
            if let Some(parameter) = scope.child_by_field_name("parameter") {
                if pattern_mentions_identifier(parameter, name, source) {
                    consider(Some(name.to_owned()), None, false);
                }
            }
        }
        if scope.kind() == "class_static_block"
            && class_static_block_var_declares_name(scope, name, source)
        {
            // Tree-sitter wraps StaticBlock statements in a body statement_block,
            // while Native hoists `var` to the StaticBlock's own var scope.
            consider(Some(name.to_owned()), None, false);
        }

        let mut cursor = scope.walk();
        let mut statements = scope.named_children(&mut cursor).collect::<Vec<_>>();
        if scope.kind() == "switch_statement" {
            statements.clear();
            let mut switch_cursor = scope.walk();
            for child in scope.named_children(&mut switch_cursor) {
                if child.kind() != "switch_body" {
                    continue;
                }
                let mut body_cursor = child.walk();
                for case in child.named_children(&mut body_cursor) {
                    if !matches!(case.kind(), "switch_case" | "switch_default") {
                        continue;
                    }
                    let mut case_cursor = case.walk();
                    statements.extend(case.named_children(&mut case_cursor).filter(|statement| {
                        matches!(
                            statement.kind(),
                            "lexical_declaration"
                                | "variable_declaration"
                                | "function_declaration"
                                | "generator_function_declaration"
                                | "class_declaration"
                        )
                    }));
                }
            }
        }
        for statement in statements {
            if statement.kind() == "import_statement" {
                for local in import_local_names(statement, source) {
                    consider(Some(local), None, false);
                }
                continue;
            }
            let is_exported_declaration = statement.kind() == "export_statement";
            let declaration = if is_exported_declaration {
                statement.child_by_field_name("declaration")
            } else {
                Some(statement)
            };
            let Some(declaration) = declaration else {
                continue;
            };
            if matches!(
                declaration.kind(),
                "function_declaration" | "generator_function_declaration" | "class_declaration"
            ) {
                if let Some(binding_name) = declaration
                    .child_by_field_name("name")
                    .and_then(|identifier| identifier_name(identifier, source))
                {
                    consider(Some(binding_name), None, false);
                }
                continue;
            }
            if !matches!(
                declaration.kind(),
                "lexical_declaration" | "variable_declaration"
            ) {
                continue;
            }
            let declaration_kind =
                variable_declaration_kind(declaration, source).unwrap_or_default();
            let const_scope = matches!(
                scope.kind(),
                "program" | "statement_block" | "class_static_block"
            );
            if declaration_kind == "var"
                && !matches!(scope.kind(), "program" | "class_static_block")
            {
                // `var` belongs to its enclosing var scope, not this block/loop.
                // Function-scoped instances are collected from the function body
                // above; top-level non-direct declarations have no Native static
                // initializer entry.
                continue;
            }
            let is_program_var =
                declaration_kind == "var" && scope.kind() == "program" && !is_exported_declaration;
            let mut declarations = declaration.walk();
            for variable in declaration.named_children(&mut declarations) {
                if variable.kind() != "variable_declarator" {
                    continue;
                }
                let Some(variable_name) = variable.child_by_field_name("name") else {
                    continue;
                };
                if !pattern_mentions_identifier(variable_name, name, source) {
                    continue;
                }
                let initializer = if variable_name.kind() == "identifier" {
                    variable.child_by_field_name("value")
                } else {
                    None
                };
                let is_static_kind = (declaration_kind == "const" && const_scope)
                    || (is_program_var
                        && !top_level_binding_is_reassigned(scope, name, source)
                        && !top_level_var_has_early_read(scope, name, source, variable_name));
                consider(
                    Some(name.to_owned()),
                    initializer,
                    is_static_kind && initializer.is_some(),
                );
            }
        }
    }
    found
}

fn import_local_names(statement: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut locals = Vec::new();
    let mut cursor = statement.walk();
    for clause in statement.named_children(&mut cursor) {
        if clause.kind() != "import_clause" {
            continue;
        }
        let mut clause_cursor = clause.walk();
        for specifier in clause.named_children(&mut clause_cursor) {
            match specifier.kind() {
                "identifier" => {
                    if let Some(name) = identifier_name(specifier, source) {
                        locals.push(name);
                    }
                }
                "named_imports" => {
                    let mut imports = specifier.walk();
                    for import in specifier.named_children(&mut imports) {
                        if import.kind() == "import_specifier" {
                            let local = import
                                .child_by_field_name("alias")
                                .or_else(|| import.child_by_field_name("name"));
                            if let Some(name) = local.and_then(|node| identifier_name(node, source))
                            {
                                locals.push(name);
                            }
                        }
                    }
                }
                "namespace_import" => {
                    let mut namespace = specifier.walk();
                    for child in specifier.named_children(&mut namespace) {
                        if let Some(name) = identifier_name(child, source) {
                            locals.push(name);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    locals
}

fn pattern_mentions_identifier(node: Node<'_>, name: &str, source: &[u8]) -> bool {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "identifier" | "shorthand_property_identifier_pattern"
        ) && identifier_name(node, source).as_deref() == Some(name)
        {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

fn function_var_declares_name(function: Node<'_>, name: &str, source: &[u8]) -> bool {
    let Some(body) = function_body(function) else {
        return false;
    };
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if node != body && (is_function(node) || node.kind() == "class_static_block") {
            continue;
        }
        if node.kind() == "variable_declarator"
            && node
                .parent()
                .is_some_and(|declaration| declaration.kind() == "variable_declaration")
            && node
                .child_by_field_name("name")
                .is_some_and(|binding| pattern_mentions_identifier(binding, name, source))
        {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

fn class_static_block_var_declares_name(static_block: Node<'_>, name: &str, source: &[u8]) -> bool {
    let Some(body) = static_block.child_by_field_name("body") else {
        return false;
    };
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if node != body && (is_function(node) || node.kind() == "class_static_block") {
            continue;
        }
        if node.kind() == "variable_declarator"
            && node
                .parent()
                .is_some_and(|declaration| declaration.kind() == "variable_declaration")
            && node
                .child_by_field_name("name")
                .is_some_and(|binding| pattern_mentions_identifier(binding, name, source))
        {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

fn pattern_binding_identifier<'tree>(
    pattern: Node<'tree>,
    name: &str,
    source: &[u8],
) -> Option<Node<'tree>> {
    match pattern.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            (identifier_name(pattern, source).as_deref() == Some(name)).then_some(pattern)
        }
        "pair_pattern" | "pair" => pattern
            .child_by_field_name("value")
            .and_then(|value| pattern_binding_identifier(value, name, source)),
        "assignment_pattern" | "object_assignment_pattern" => pattern
            .child_by_field_name("left")
            .and_then(|left| pattern_binding_identifier(left, name, source)),
        "rest_pattern" => pattern
            .child_by_field_name("argument")
            .or_else(|| pattern.named_child(0))
            .and_then(|argument| pattern_binding_identifier(argument, name, source)),
        "variable_declarator" => pattern
            .child_by_field_name("name")
            .and_then(|binding| pattern_binding_identifier(binding, name, source)),
        "variable_declaration" | "lexical_declaration" => {
            let mut cursor = pattern.walk();
            let binding = pattern
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "variable_declarator")
                .find_map(|declaration| pattern_binding_identifier(declaration, name, source));
            binding
        }
        "object_pattern" | "array_pattern" | "object" | "array" | "parenthesized_expression" => {
            let mut cursor = pattern.walk();
            let binding = pattern
                .named_children(&mut cursor)
                .find_map(|child| pattern_binding_identifier(child, name, source));
            binding
        }
        _ => None,
    }
}

fn write_target_identifier<'tree>(
    expression: Node<'tree>,
    name: &str,
    source: &[u8],
) -> Option<Node<'tree>> {
    match expression.kind() {
        "assignment_expression" | "augmented_assignment_expression" => expression
            .child_by_field_name("left")
            .and_then(|left| pattern_binding_identifier(left, name, source)),
        "update_expression" => expression
            .child_by_field_name("argument")
            .and_then(|argument| pattern_binding_identifier(argument, name, source)),
        "for_in_statement" | "for_of_statement" => expression
            .child_by_field_name("left")
            .and_then(|left| pattern_binding_identifier(left, name, source)),
        _ => None,
    }
}

fn top_level_binding_is_reassigned(program: Node<'_>, name: &str, source: &[u8]) -> bool {
    let mut stack = vec![program];
    while let Some(node) = stack.pop() {
        if let Some(reference) = write_target_identifier(node, name, source) {
            let mut ancestor = reference.parent();
            let mut shadowed = false;
            while let Some(scope) = ancestor {
                if scope == program {
                    break;
                }
                if is_lexical_scope(scope) && static_binding_in_scope(scope, name, source).is_some()
                {
                    shadowed = true;
                    break;
                }
                ancestor = scope.parent();
            }
            if !shadowed {
                return true;
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

/// Native refuses a top-level `var` initializer when module evaluation read
/// the binding before its declaration. Reads inside function bodies are
/// deferred and do not trigger that guard.
fn top_level_var_has_early_read(
    program: Node<'_>,
    name: &str,
    source: &[u8],
    binding: Node<'_>,
) -> bool {
    let declaration_start = binding.start_byte();
    let mut stack = vec![program];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "identifier" | "shorthand_property_identifier")
            && node.start_byte() < declaration_start
            && identifier_name(node, source).as_deref() == Some(name)
            && !is_deferred_from_program(node, program)
            && !is_shadowed_from_program(node, program, name, source)
            && is_identifier_read(node, name, source)
        {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

fn is_deferred_from_program(mut node: Node<'_>, program: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if parent == program {
            return false;
        }
        if is_function(parent) {
            return true;
        }
        node = parent;
    }
    false
}

fn is_shadowed_from_program(
    mut node: Node<'_>,
    program: Node<'_>,
    name: &str,
    source: &[u8],
) -> bool {
    while let Some(parent) = node.parent() {
        if parent == program {
            return false;
        }
        if is_lexical_scope(parent) && static_binding_in_scope(parent, name, source).is_some() {
            return true;
        }
        node = parent;
    }
    false
}

fn is_identifier_read(node: Node<'_>, name: &str, source: &[u8]) -> bool {
    let mut child = node;
    while let Some(parent) = child.parent() {
        if matches!(
            parent.kind(),
            "assignment_expression" | "augmented_assignment_expression"
        ) && parent
            .child_by_field_name("left")
            .and_then(|left| pattern_binding_identifier(left, name, source))
            .is_some_and(|target| target.start_byte() == node.start_byte())
        {
            return false;
        }
        if parent.kind() == "update_expression"
            && parent
                .child_by_field_name("argument")
                .and_then(|argument| pattern_binding_identifier(argument, name, source))
                .is_some_and(|target| target.start_byte() == node.start_byte())
        {
            return false;
        }
        if matches!(parent.kind(), "for_in_statement" | "for_of_statement")
            && parent.child_by_field_name("left").is_some_and(|left| {
                pattern_binding_identifier(left, name, source)
                    .is_some_and(|target| target.start_byte() == node.start_byte())
            })
        {
            return false;
        }
        if parent.kind() == "variable_declarator"
            && parent.child_by_field_name("name").is_some_and(|binding| {
                binding.start_byte() <= node.start_byte() && node.end_byte() <= binding.end_byte()
            })
        {
            return false;
        }
        if matches!(
            parent.kind(),
            "function_declaration"
                | "generator_function_declaration"
                | "function_expression"
                | "class_declaration"
                | "class_expression"
        ) && parent.child_by_field_name("name").is_some_and(|binding| {
            binding.start_byte() <= node.start_byte() && node.end_byte() <= binding.end_byte()
        }) {
            return false;
        }
        if is_function(parent) {
            break;
        }
        child = parent;
    }
    true
}

fn call_arguments(call: Node<'_>) -> Vec<Node<'_>> {
    call.child_by_field_name("arguments")
        .map(|arguments| {
            let mut cursor = arguments.walk();
            arguments.named_children(&mut cursor).collect()
        })
        .unwrap_or_default()
}

fn is_optional_call(call: Node<'_>) -> bool {
    call.child_by_field_name("optional_chain").is_some()
}

fn module_imports(root: Node<'_>, source: &[u8]) -> HashMap<String, ImportedBinding> {
    let mut imports = HashMap::new();
    let mut root_cursor = root.walk();
    for statement in root.named_children(&mut root_cursor) {
        if statement.kind() != "import_statement" {
            continue;
        }
        let Some(specifier) = statement
            .child_by_field_name("source")
            .and_then(|source_node| literal_name(source_node, source))
        else {
            continue;
        };
        let mut statement_cursor = statement.walk();
        let Some(clause) = statement
            .named_children(&mut statement_cursor)
            .find(|child| child.kind() == "import_clause")
        else {
            continue;
        };
        let mut clause_cursor = clause.walk();
        for child in clause.named_children(&mut clause_cursor) {
            match child.kind() {
                // `import defaultName from './module.js'`.
                "identifier" => {
                    let Some(local) = identifier_name(child, source) else {
                        continue;
                    };
                    imports.insert(
                        local,
                        ImportedBinding {
                            specifier: specifier.clone(),
                            export: "default".encode_utf16().collect(),
                        },
                    );
                }
                // `import { named as local } from './module.js'`.
                "named_imports" => {
                    let mut named_cursor = child.walk();
                    for imported in child.named_children(&mut named_cursor) {
                        if imported.kind() != "import_specifier" {
                            continue;
                        }
                        let Some(name) = imported.child_by_field_name("name") else {
                            continue;
                        };
                        let Some(export) = (match name.kind() {
                            "identifier" => identifier_name(name, source)
                                .map(|name| name.encode_utf16().collect()),
                            "string" => literal_name_units(name, source),
                            _ => None,
                        }) else {
                            continue;
                        };
                        let local = imported.child_by_field_name("alias").unwrap_or(name);
                        if local.kind() == "identifier" {
                            if let Some(local) = identifier_name(local, source) {
                                imports.insert(
                                    local,
                                    ImportedBinding {
                                        specifier: specifier.clone(),
                                        export,
                                    },
                                );
                            }
                        }
                    }
                }
                // Native rejects a namespace import when it is used as a hook.
                "namespace_import" => {}
                _ => {}
            }
        }
    }
    imports
}

fn top_level_function<'tree>(root: Node<'tree>, name: &str, source: &[u8]) -> Option<Node<'tree>> {
    if top_level_binding_is_reassigned(root, name, source) {
        return None;
    }
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        let declaration = if statement.kind() == "export_statement"
            && statement.child_by_field_name("source").is_none()
        {
            statement.child_by_field_name("declaration")
        } else if statement.kind() == "export_statement" {
            None
        } else {
            Some(statement)
        };
        let Some(declaration) = declaration else {
            continue;
        };
        match declaration.kind() {
            "function_declaration" | "generator_function_declaration" => {
                if declaration
                    .child_by_field_name("name")
                    .is_some_and(|identifier| {
                        identifier_name(identifier, source).as_deref() == Some(name)
                    })
                {
                    return Some(declaration);
                }
            }
            "lexical_declaration" => {
                if variable_declaration_kind(declaration, source) != Some("const") {
                    continue;
                }
                let mut declarations = declaration.walk();
                for variable in declaration.named_children(&mut declarations) {
                    if variable.kind() != "variable_declarator"
                        || variable
                            .child_by_field_name("name")
                            .is_none_or(|identifier| {
                                identifier_name(identifier, source).as_deref() != Some(name)
                            })
                    {
                        continue;
                    }
                    if let Some(function) = variable
                        .child_by_field_name("value")
                        .and_then(function_expression)
                    {
                        return Some(function);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn is_default_export_statement(statement: Node<'_>) -> bool {
    let mut cursor = statement.walk();
    let is_default = statement
        .children(&mut cursor)
        .any(|child| child.kind() == "default");
    is_default
}

fn exported_function<'tree>(
    root: Node<'tree>,
    export_name: &[u16],
    source: &[u8],
) -> Result<Node<'tree>, String> {
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        if statement.kind() != "export_statement" {
            continue;
        }
        let declaration = statement
            .child_by_field_name("declaration")
            .or_else(|| statement.named_child(0));
        let default_export = "default".encode_utf16().collect::<Vec<_>>();
        if export_name == default_export.as_slice() {
            if let Some(declaration) = declaration {
                if is_function(declaration) {
                    let assigned_name = declaration
                        .child_by_field_name("name")
                        .and_then(|identifier| identifier_name(identifier, source));
                    if !assigned_name
                        .is_some_and(|name| top_level_binding_is_reassigned(root, &name, source))
                    {
                        return Ok(declaration);
                    }
                }
                if declaration.kind() == "identifier" {
                    if let Some(name) = identifier_name(declaration, source) {
                        if let Some(function) = top_level_function(root, &name, source) {
                            return Ok(function);
                        }
                    }
                }
            }
        }
        if !is_default_export_statement(statement) {
            if let Some(declaration) = declaration {
                if matches!(
                    declaration.kind(),
                    "function_declaration" | "generator_function_declaration"
                ) && declaration
                    .child_by_field_name("name")
                    .is_some_and(|identifier| {
                        identifier_name(identifier, source)
                            .is_some_and(|name| name.encode_utf16().eq(export_name.iter().copied()))
                    })
                    && declaration
                        .child_by_field_name("name")
                        .and_then(|identifier| identifier_name(identifier, source))
                        .is_some_and(|name| !top_level_binding_is_reassigned(root, &name, source))
                {
                    return Ok(declaration);
                }
                if declaration.kind() == "lexical_declaration"
                    && variable_declaration_kind(declaration, source) == Some("const")
                {
                    let mut variable_cursor = declaration.walk();
                    for variable in declaration.named_children(&mut variable_cursor) {
                        if variable.kind() != "variable_declarator"
                            || variable.child_by_field_name("name").is_none_or(|name| {
                                identifier_name(name, source).is_none_or(|name| {
                                    !name.encode_utf16().eq(export_name.iter().copied())
                                })
                            })
                        {
                            continue;
                        }
                        if let Some(function) = variable
                            .child_by_field_name("value")
                            .and_then(function_expression)
                        {
                            let name = variable
                                .child_by_field_name("name")
                                .and_then(|name| identifier_name(name, source));
                            if name.is_some_and(|name| {
                                !top_level_binding_is_reassigned(root, &name, source)
                            }) {
                                return Ok(function);
                            }
                        }
                    }
                }
            }
        }
        if statement.child_by_field_name("source").is_some() {
            continue;
        }
        let mut export_cursor = statement.walk();
        for clause in statement.named_children(&mut export_cursor) {
            if clause.kind() != "export_clause" {
                continue;
            }
            let mut clause_cursor = clause.walk();
            for specifier in clause.named_children(&mut clause_cursor) {
                if specifier.kind() != "export_specifier" {
                    continue;
                }
                let Some(local) = specifier.child_by_field_name("name") else {
                    continue;
                };
                let exported = specifier.child_by_field_name("alias").unwrap_or(local);
                if module_export_name_units(exported, source).as_deref() == Some(export_name) {
                    if let Some(local_name) = module_export_name_units(local, source)
                        .and_then(|units| String::from_utf16(&units).ok())
                    {
                        if let Some(function) = top_level_function(root, &local_name, source) {
                            return Ok(function);
                        }
                    }
                }
            }
        }
    }
    Err(scan_refusal(
        source,
        root,
        &format!("no export named {}", String::from_utf16_lossy(export_name)),
    ))
}

fn async_generator(function: Node<'_>, source: &[u8]) -> bool {
    let text = source_text(function, source);
    let Some(keyword) = text.find("function") else {
        return false;
    };
    let prefix = &text[..keyword];
    let after_keyword = &text[keyword + "function".len()..];
    prefix.contains("async")
        && after_keyword[..after_keyword.find('(').unwrap_or(after_keyword.len())].contains('*')
}

fn direct_result_use(value: Node<'_>, holder: Node<'_>) -> bool {
    if let Some(parent) = value.parent() {
        if parent.kind() == "expression_statement" && parent.named_child(0) == Some(value) {
            return true;
        }
        if parent.kind() == "return_statement" && parent.named_child(0) == Some(value) {
            return parent
                .parent()
                .is_some_and(|function| function.start_byte() == holder.start_byte());
        }
    }
    holder.kind() == "arrow_function"
        && function_body(holder).is_some_and(|body| body.start_byte() == value.start_byte())
}

fn callback_function<'tree>(
    callback: Node<'tree>,
    what: &str,
    functions: &HashMap<String, Option<Node<'tree>>>,
    imports: &HashMap<String, ImportedBinding>,
    file: &str,
    graph: &SourceGraph<'_>,
    source: &[u8],
) -> Result<CallbackRef<'tree>, String> {
    if is_function(callback) {
        return Ok(CallbackRef::Local(callback));
    }
    if callback.kind() == "identifier" {
        if let Some(name) = identifier_name(callback, source) {
            if let Some(Some(function)) = functions.get(&name) {
                if !top_level_binding_is_reassigned(function_root(callback), &name, source) {
                    return Ok(CallbackRef::Local(*function));
                }
            }
            if let Some(import) = imports.get(&name) {
                if let Some(target) = graph.target(file, &import.specifier) {
                    return Ok(CallbackRef::Imported {
                        file: target.to_owned(),
                        export: import.export.clone(),
                    });
                }
            }
        }
    }
    Err(scan_refusal(
        source,
        callback,
        &format!("{what} is not a function literal or the name of one"),
    ))
}

fn parse_module(source: &str) -> Result<tree_sitter::Tree, String> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|error| format!("JavaScript parser setup failed: {error}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "JavaScript parser did not return a syntax tree".to_owned())?;
    if tree.root_node().has_error() {
        return Err("linked module has a syntax error".into());
    }
    Ok(tree)
}

fn callback_is_async_generator(
    callback: &CallbackRef<'_>,
    graph: &SourceGraph<'_>,
    current_source: &[u8],
    callback_node: Node<'_>,
) -> Result<bool, String> {
    match callback {
        CallbackRef::Local(function) => Ok(async_generator(*function, current_source)),
        CallbackRef::Imported { file, export } => {
            let Some(source) = graph.files.get(file.as_str()) else {
                return Err(scan_refusal(
                    current_source,
                    callback_node,
                    &format!("imported hook target {file} is not linked"),
                ));
            };
            let tree = parse_module(source)
                .map_err(|message| scan_refusal(current_source, callback_node, &message))?;
            let bytes = source.as_bytes();
            let function = exported_function(tree.root_node(), export, bytes)?;
            Ok(async_generator(function, bytes))
        }
    }
}

fn scan_callback(
    callback: CallbackRef<'_>,
    current_file: &str,
    mut bindings: HashMap<String, UseBinding>,
    add_api_parameter: bool,
    graph: &SourceGraph<'_>,
    scan: &mut UsesScan,
    depth: usize,
) -> Result<(), String> {
    match callback {
        CallbackRef::Local(function) => {
            let Some(source) = graph.files.get(current_file) else {
                return Ok(());
            };
            let root = function_root(function);
            let bytes = source.as_bytes();
            let mut functions = HashMap::new();
            collect_functions(root, bytes, &mut functions);
            if add_api_parameter {
                if let Some(parameter) = function_parameters(function).first() {
                    if parameter.kind() == "identifier" {
                        if let Some(name) = identifier_name(*parameter, bytes) {
                            bindings.insert(name, UseBinding::Api);
                        }
                    }
                }
            }
            scan_function(
                function,
                bindings,
                &functions,
                current_file,
                graph,
                bytes,
                scan,
                depth,
            )
        }
        CallbackRef::Imported { file, export } => {
            let Some(source) = graph.files.get(file.as_str()) else {
                return Ok(());
            };
            let tree = parse_module(source)?;
            let root = tree.root_node();
            let bytes = source.as_bytes();
            let mut functions = HashMap::new();
            collect_functions(root, bytes, &mut functions);
            let function = exported_function(root, &export, bytes)?;
            if add_api_parameter {
                if let Some(parameter) = function_parameters(function).first() {
                    if parameter.kind() == "identifier" {
                        if let Some(name) = identifier_name(*parameter, bytes) {
                            bindings.insert(name, UseBinding::Api);
                        }
                    }
                }
            }
            scan_function(
                function, bindings, &functions, &file, graph, bytes, scan, depth,
            )
        }
    }
}

fn function_root<'tree>(mut function: Node<'tree>) -> Node<'tree> {
    while let Some(parent) = function.parent() {
        function = parent;
    }
    function
}

struct OnRegistration<'tree> {
    event: String,
    handlers: Vec<CallbackRef<'tree>>,
    has_matcher: bool,
    is_pattern: bool,
}

fn validate_on_call<'tree>(
    call: Node<'tree>,
    on_name: &str,
    holder: Node<'tree>,
    functions: &HashMap<String, Option<Node<'tree>>>,
    imports: &HashMap<String, ImportedBinding>,
    file: &str,
    graph: &SourceGraph<'_>,
    source: &[u8],
) -> Result<OnRegistration<'tree>, String> {
    let arguments = call_arguments(call);
    if let Some(spread) = arguments
        .iter()
        .find(|argument| argument.kind() == "spread_element")
    {
        return Err(scan_refusal(
            source,
            *spread,
            &format!("{on_name}() is called with a spread argument"),
        ));
    }
    if !(2..=3).contains(&arguments.len()) {
        return Err(scan_refusal(
            source,
            call,
            &format!(
                "{on_name}() takes (event, hook) or (event, matcher, hook); got {} argument(s)",
                arguments.len()
            ),
        ));
    }
    let event_units = arguments
        .first()
        .and_then(|argument| literal_name_units(*argument, source))
        .ok_or_else(|| {
            scan_refusal(
                source,
                arguments[0],
                &format!("the event name passed to {on_name}() is not a string literal"),
            )
        })?;
    // A lone surrogate is still a cooked JS string. It cannot match Native's
    // Unicode event-name grammar; keep it in the string-validation path so it
    // is refused as an invalid pattern rather than mistaken for a dynamic arg.
    let event =
        String::from_utf16(&event_units).unwrap_or_else(|_| String::from_utf16_lossy(&event_units));
    if let Some(error) = event_pattern_error(&event) {
        return Err(scan_refusal(source, arguments[0], &error));
    }
    let has_matcher = arguments.len() == 3;
    let is_pattern = event == "*" || event.ends_with(".*") || event.starts_with('!');
    let hook_arg = *arguments.last().expect("on() has at least two arguments");
    let hook = callback_function(
        hook_arg, "the hook", functions, imports, file, graph, source,
    )?;
    let streaming = matches!(event.as_str(), "turn.step" | "process.spawn");
    let hook_is_async_generator = callback_is_async_generator(&hook, graph, source, hook_arg)?;
    if streaming != hook_is_async_generator {
        let message = if streaming {
            format!("the hook on \"{event}\" is not an async generator")
        } else {
            format!("the hook on \"{event}\" is an async generator")
        };
        return Err(scan_refusal(source, hook_arg, &message));
    }
    let mut handlers = vec![hook];
    let description = format!("{on_name}(\"{event}\")");
    let Some(parent) = call.parent() else {
        return Err(scan_refusal(
            source,
            call,
            &format!("the value of {description} is kept"),
        ));
    };
    if parent.kind() != "member_expression" || parent.child_by_field_name("object") != Some(call) {
        if direct_result_use(call, holder) {
            return Ok(OnRegistration {
                event,
                handlers,
                has_matcher,
                is_pattern,
            });
        }
        return Err(scan_refusal(
            source,
            call,
            &format!("the value of {description} is kept"),
        ));
    }
    if parent.child_by_field_name("optional_chain").is_some()
        || parent
            .child_by_field_name("property")
            .is_none_or(|property| {
                property.kind() != "property_identifier"
                    || identifier_name(property, source).as_deref() != Some("catch")
            })
    {
        return Err(scan_refusal(
            source,
            parent,
            &format!("{description} is read for something other than .catch"),
        ));
    }
    let Some(catch_call) = parent.parent().filter(|node| {
        node.kind() == "call_expression"
            && node.child_by_field_name("function") == Some(parent)
            && !is_optional_call(*node)
    }) else {
        return Err(scan_refusal(
            source,
            parent,
            &format!("{description}.catch is read without being called"),
        ));
    };
    if event == "engine.create" {
        return Err(scan_refusal(
            source,
            catch_call,
            &format!("{description}.catch(): an engine.create hook has no budget and its failure fails the load; .catch does not apply"),
        ));
    }
    let catch_args = call_arguments(catch_call);
    if catch_args
        .iter()
        .any(|argument| argument.kind() == "spread_element")
    {
        return Err(scan_refusal(
            source,
            catch_call,
            &format!("{description}.catch() is called with a spread argument"),
        ));
    }
    if catch_args.len() != 1 {
        return Err(scan_refusal(
            source,
            catch_call,
            &format!(
                "{description}.catch() takes (handler); got {} argument(s)",
                catch_args.len()
            ),
        ));
    }
    let catch_handler = callback_function(
        catch_args[0],
        "the .catch handler",
        functions,
        imports,
        file,
        graph,
        source,
    )?;
    if catch_call.parent().is_some_and(|node| {
        node.kind() == "member_expression" && node.child_by_field_name("object") == Some(catch_call)
    }) {
        return Err(scan_refusal(
            source,
            catch_call.parent().unwrap(),
            &format!("{description}.catch(...) is followed by another member; a registration takes one .catch"),
        ));
    }
    if !direct_result_use(catch_call, holder) {
        return Err(scan_refusal(
            source,
            catch_call,
            &format!("the value of {description}.catch() is kept"),
        ));
    }
    let catch_is_async_generator =
        callback_is_async_generator(&catch_handler, graph, source, catch_args[0])?;
    if streaming != catch_is_async_generator {
        let message = if streaming {
            format!("the .catch handler on \"{event}\" is not an async generator")
        } else {
            format!("the .catch handler on \"{event}\" is an async generator")
        };
        return Err(scan_refusal(source, catch_args[0], &message));
    }
    handlers.push(catch_handler);
    Ok(OnRegistration {
        event,
        handlers,
        has_matcher,
        is_pattern,
    })
}

fn validate_on_uses(
    function: Node<'_>,
    on_name: &str,
    functions: &HashMap<String, Option<Node<'_>>>,
    imports: &HashMap<String, ImportedBinding>,
    file: &str,
    graph: &SourceGraph<'_>,
    source: &[u8],
) -> Result<(), String> {
    let Some(body) = function_body(function) else {
        return Ok(());
    };
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if node.kind() == "identifier"
            && identifier_name(node, source).as_deref() == Some(on_name)
            && node
                .parent()
                .is_some_and(|parent| parent.kind() != "formal_parameters")
        {
            let Some(parent) = node.parent() else {
                continue;
            };
            if parent.kind() == "call_expression"
                && parent.child_by_field_name("function") == Some(node)
                && !is_optional_call(parent)
            {
                continue;
            }
            if parent.kind() == "arguments" {
                let Some(call) = parent
                    .parent()
                    .filter(|call| call.kind() == "call_expression")
                else {
                    continue;
                };
                let Some(callee) = call.child_by_field_name("function") else {
                    continue;
                };
                let helper = if callee.kind() == "identifier" {
                    identifier_name(callee, source).is_some_and(|name| {
                        functions.get(&name).is_some_and(Option::is_some)
                            || imports.get(&name).is_some_and(|import| {
                                graph.target(file, &import.specifier).is_some()
                            })
                    })
                } else {
                    false
                };
                if helper
                    && !call_arguments(call)
                        .iter()
                        .any(|argument| argument.kind() == "spread_element")
                {
                    continue;
                }
                return Err(scan_refusal(
                    source,
                    node,
                    &format!("\"{on_name}\" is used other than as a call or an argument to a function of the module's own"),
                ));
            }
            return Err(scan_refusal(
                source,
                node,
                &format!("\"{on_name}\" is used other than as a call or an argument to a function of the module's own"),
            ));
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    Ok(())
}

fn scan_function<'tree>(
    function: Node<'tree>,
    bindings: HashMap<String, UseBinding>,
    functions: &HashMap<String, Option<Node<'tree>>>,
    file: &str,
    graph: &SourceGraph<'_>,
    source: &[u8],
    scan: &mut UsesScan,
    depth: usize,
) -> Result<(), String> {
    let mut binding_key = bindings
        .iter()
        .map(|(name, binding)| (name.clone(), u8::from(*binding == UseBinding::Api)))
        .collect::<Vec<_>>();
    binding_key.sort();
    if !scan
        .visited
        .insert((file.to_owned(), function.start_byte(), binding_key))
    {
        return Ok(());
    }
    let Some(body) = function_body(function) else {
        return Ok(());
    };
    let api_names = bindings
        .iter()
        .filter_map(|(name, binding)| (*binding == UseBinding::Api).then(|| name.clone()))
        .collect::<HashSet<_>>();
    let on_names = bindings
        .iter()
        .filter_map(|(name, binding)| (*binding == UseBinding::On).then(|| name.clone()))
        .collect::<HashSet<_>>();
    let imports = module_imports(function_root(function), source);
    for on_name in &on_names {
        validate_on_uses(function, on_name, functions, &imports, file, graph, source)?;
    }
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if is_function(node) {
            continue;
        }
        if let Some(noun) = api_namespace(node, source, &api_names) {
            if !namespace_is_direct_call(node) {
                return Err(scan_refusal(
                    source,
                    node,
                    &format!(
                        "$.{noun} is used as a value; call a literal API method directly, such as $.{noun}.method(...)"
                    ),
                ));
            }
        }
        if node.kind() == "call_expression" {
            let function_node = node.child_by_field_name("function");
            let arguments = call_arguments(node);
            if let Some(function_node) = function_node {
                let function_name = (function_node.kind() == "identifier")
                    .then(|| identifier_name(function_node, source))
                    .flatten();
                if function_name
                    .as_ref()
                    .is_some_and(|name| on_names.contains(name))
                {
                    let on_name = function_name
                        .as_deref()
                        .expect("the function name was matched as on");
                    let registration = validate_on_call(
                        node, on_name, function, functions, &imports, file, graph, source,
                    )?;
                    let OnRegistration {
                        event,
                        handlers,
                        has_matcher,
                        is_pattern,
                    } = registration;
                    if let Some((first_file, first_line)) = scan.unmatched_events.get(&event) {
                        if !has_matcher || is_pattern {
                            let matcher_suffix = if has_matcher {
                                ""
                            } else {
                                " without a matcher"
                            };
                            return Err(scan_refusal(
                                source,
                                node,
                                &format!(
                                    "{on_name}(\"{event}\") is registered twice{matcher_suffix}; the first is at {first_file}:{first_line}"
                                ),
                            ));
                        }
                    }
                    if !has_matcher || is_pattern {
                        scan.unmatched_events.insert(
                            event.clone(),
                            (file.to_owned(), source_line_number(source, node)),
                        );
                    }
                    if scan.seen_events.insert(event.clone()) {
                        scan.events.push(event);
                    }
                    for handler in handlers {
                        scan_callback(handler, file, HashMap::new(), true, graph, scan, depth + 1)?;
                    }
                } else if let Some((noun, method)) = api_method(function_node, source, &api_names) {
                    scan.calls.insert(format!("{noun}.{method}"));
                    let first_argument = arguments.first().copied();
                    match (noun.as_str(), method.as_str()) {
                        ("env", "get" | "set") => {
                            if let Some(name) = first_argument
                                .and_then(|argument| literal_name_units(argument, source))
                            {
                                if method == "get" {
                                    scan.env_reads.insert(name);
                                } else {
                                    scan.env_writes.insert(name);
                                }
                            }
                        }
                        ("state", "get" | "set") => {
                            if let Some((plugin, key)) = first_argument
                                .and_then(|argument| state_reference(argument, source))
                            {
                                if method == "get" {
                                    scan.state_reads.insert((plugin, key));
                                } else {
                                    scan.state_writes.insert((plugin, key));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(name) = function_name {
                    if !on_names.contains(&name) {
                        let local_helper = functions.get(&name).and_then(Option::as_ref).copied();
                        let imported = imports.get(&name);
                        if let Some(helper) = local_helper {
                            let mut helper_bindings = HashMap::new();
                            for (parameter, argument) in
                                function_parameters(helper).iter().zip(arguments.iter())
                            {
                                if parameter.kind() != "identifier"
                                    || argument.kind() != "identifier"
                                {
                                    continue;
                                }
                                if let Some(binding) = identifier_name(*argument, source)
                                    .as_ref()
                                    .and_then(|name| bindings.get(name))
                                {
                                    if let Some(name) = identifier_name(*parameter, source) {
                                        helper_bindings.insert(name, *binding);
                                    }
                                }
                            }
                            if !helper_bindings.is_empty() {
                                if top_level_binding_is_reassigned(
                                    function_root(function),
                                    &name,
                                    source,
                                ) {
                                    return Err(scan_refusal(
                                        source,
                                        function_node,
                                        &format!("local helper {name} is assigned elsewhere"),
                                    ));
                                }
                                if helper.start_byte() != function.start_byte() {
                                    scan_function(
                                        helper,
                                        helper_bindings,
                                        functions,
                                        file,
                                        graph,
                                        source,
                                        scan,
                                        depth + 1,
                                    )?;
                                }
                            }
                        } else if let Some(imported) = imported.filter(|_| {
                            arguments.iter().any(|argument| {
                                argument.kind() == "identifier"
                                    && identifier_name(*argument, source).is_some_and(
                                        |argument_name| bindings.contains_key(&argument_name),
                                    )
                            })
                        }) {
                            if top_level_binding_is_reassigned(
                                function_root(function),
                                &name,
                                source,
                            ) {
                                return Err(scan_refusal(
                                    source,
                                    function_node,
                                    &format!("imported helper {name} is assigned elsewhere"),
                                ));
                            }
                            if let Some(target_file) = graph.target(file, &imported.specifier) {
                                if let Some(target_source) = graph.files.get(target_file) {
                                    let target_tree = parse_module(target_source)?;
                                    let target_root = target_tree.root_node();
                                    let target_bytes = target_source.as_bytes();
                                    let mut target_functions = HashMap::new();
                                    collect_functions(
                                        target_root,
                                        target_bytes,
                                        &mut target_functions,
                                    );
                                    let helper = exported_function(
                                        target_root,
                                        &imported.export,
                                        target_bytes,
                                    )?;
                                    let mut helper_bindings = HashMap::new();
                                    for (parameter, argument) in
                                        function_parameters(helper).iter().zip(arguments.iter())
                                    {
                                        if parameter.kind() != "identifier"
                                            || argument.kind() != "identifier"
                                        {
                                            continue;
                                        }
                                        if let Some(binding) = identifier_name(*argument, source)
                                            .as_ref()
                                            .and_then(|name| bindings.get(name))
                                        {
                                            if let Some(name) =
                                                identifier_name(*parameter, target_bytes)
                                            {
                                                helper_bindings.insert(name, *binding);
                                            }
                                        }
                                    }
                                    if !helper_bindings.is_empty() {
                                        scan_function(
                                            helper,
                                            helper_bindings,
                                            &target_functions,
                                            target_file,
                                            graph,
                                            target_bytes,
                                            scan,
                                            depth + 1,
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let mut cursor = node.walk();
        let children = node.children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    Ok(())
}

/// Scan the module's reachable hook registrations before evaluation. Claude
/// Code 2.1.295 follows callbacks imported from the linked hook graph, and API
/// calls in dead helpers or outside reachable handlers are not reported.
/// Events keep registration order; calls and literal env/state references are
/// sorted.
pub(super) fn scan_uses_graph(
    prepared: &super::client_source::PreparedHookSourceGraph,
) -> Result<Utf16JsonProjection, String> {
    let graph = SourceGraph::from_prepared(prepared);
    scan_uses_entries(&[graph.entry], &graph)
}

/// Test helper for independent source snippets without import links.
#[cfg(test)]
pub(super) fn scan_uses(sources: &[String]) -> Result<serde_json::Value, String> {
    let paths = (0..sources.len())
        .map(|index| format!("source-{index}"))
        .collect::<Vec<_>>();
    let files = paths
        .iter()
        .zip(sources.iter())
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect();
    let graph = SourceGraph {
        entry: "",
        files,
        links: HashMap::new(),
    };
    let entries = paths.iter().map(String::as_str).collect::<Vec<_>>();
    let uses = scan_uses_entries(&entries, &graph)?;
    serde_json::from_str(&uses.to_json_string().map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())
}

fn scan_uses_entries(
    entries: &[&str],
    graph: &SourceGraph<'_>,
) -> Result<Utf16JsonProjection, String> {
    let mut scan = UsesScan::default();
    for file in entries {
        let Some(source) = graph.files.get(file) else {
            continue;
        };
        let Ok(tree) = parse_module(source) else {
            continue;
        };
        let bytes = source.as_bytes();
        let root = tree.root_node();
        let mut functions = HashMap::new();
        collect_functions(root, bytes, &mut functions);
        let Some(register) = exported_register(root, bytes)? else {
            continue;
        };
        // Byte offsets are only unique inside one parsed source tree.
        scan.visited.clear();
        let mut bindings = HashMap::new();
        if let Some(parameter) = function_parameters(register).first() {
            if parameter.kind() == "identifier" {
                if let Some(name) = identifier_name(*parameter, bytes) {
                    bindings.insert(name, UseBinding::On);
                }
            }
        }
        scan_function(
            register, bindings, &functions, file, graph, bytes, &mut scan, 0,
        )?;
    }

    let events = scan
        .events
        .into_iter()
        .map(|event| projected_string(event.encode_utf16().collect()))
        .collect::<Result<Vec<_>, _>>()?;
    let calls = scan
        .calls
        .into_iter()
        .map(|call| projected_string(call.encode_utf16().collect()))
        .collect::<Result<Vec<_>, _>>()?;
    let events = Utf16JsonProjection::array(events).map_err(|error| error.to_string())?;
    let calls = Utf16JsonProjection::array(calls).map_err(|error| error.to_string())?;
    let mut fields = vec![("events".into(), events), ("calls".into(), calls)];
    if !scan.env_reads.is_empty() || !scan.env_writes.is_empty() {
        let reads = projected_string_array(scan.env_reads)?;
        let writes = projected_string_array(scan.env_writes)?;
        fields.push((
            "env".into(),
            Utf16JsonProjection::object(vec![("reads".into(), reads), ("writes".into(), writes)])
                .map_err(|error| error.to_string())?,
        ));
    }
    if !scan.state_reads.is_empty() || !scan.state_writes.is_empty() {
        let reads = projected_state_array(scan.state_reads)?;
        let writes = projected_state_array(scan.state_writes)?;
        fields.push((
            "state".into(),
            Utf16JsonProjection::object(vec![("reads".into(), reads), ("writes".into(), writes)])
                .map_err(|error| error.to_string())?,
        ));
    }
    Utf16JsonProjection::object(fields).map_err(|error| error.to_string())
}

fn projected_string(units: Vec<u16>) -> Result<Utf16JsonProjection, String> {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .map_err(|error| error.to_string())
}

fn projected_string_array(values: BTreeSet<Vec<u16>>) -> Result<Utf16JsonProjection, String> {
    let values = values
        .into_iter()
        .map(projected_string)
        .collect::<Result<Vec<_>, _>>()?;
    Utf16JsonProjection::array(values).map_err(|error| error.to_string())
}

fn projected_state_array(
    values: BTreeSet<(Vec<u16>, Vec<u16>)>,
) -> Result<Utf16JsonProjection, String> {
    let values = values
        .into_iter()
        .map(|(plugin, key)| {
            Utf16JsonProjection::object(vec![
                ("plugin".into(), projected_string(plugin)?),
                ("key".into(), projected_string(key)?),
            ])
            .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Utf16JsonProjection::array(values).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_file(file: &str, source: &str) -> super::super::client_source::HookSourceFile {
        super::super::client_source::HookSourceFile {
            file: file.into(),
            source: source.into(),
        }
    }

    fn source_link(
        from: &str,
        spelled: &str,
        file: &str,
    ) -> super::super::client_source::ClientSourceLink {
        super::super::client_source::ClientSourceLink {
            from: from.into(),
            spelled: spelled.into(),
            file: file.into(),
        }
    }

    fn projection_json(value: &Utf16JsonProjection) -> serde_json::Value {
        serde_json::from_str(&value.to_json_string().unwrap()).unwrap()
    }

    #[test]
    fn grants_only_direct_literal_calls_in_code() {
        let scan = scan_sources(&[r#"
            // $.env.get('COMMENT')
            const note = "$.env.get('STRING')";
            $.env.get('HOME');
            $.env . get ("USERPROFILE");
            $.env.set('GIT_PAGER', 'cat');
            $.env.get(dynamic);
            other.env.get('OTHER');
        "#
        .into()]);
        assert_eq!(
            scan.reads,
            HashSet::from(["HOME".into(), "USERPROFILE".into()])
        );
        assert_eq!(scan.writes, HashSet::from(["GIT_PAGER".into()]));
    }

    #[test]
    fn malformed_module_grants_nothing() {
        let scan = scan_sources(&["$.env.get('HOME'); this is not JS @@@".into()]);
        assert!(scan.reads.is_empty());
    }

    #[test]
    fn plugin_register_uses_preserve_event_order_and_sort_calls() {
        let uses = scan_uses(&[r#"
            function readInputs($) {
              $.fs.read('x');
              $.env.get('HOME');
              $.state.get({ plugin: 'other-mod', key: 'flag' });
            }
            function deadCode($, on) {
              $.tool.list();
              on('not.registered', () => {});
            }
            export function register(on) {
              on('tool.call', async ($, e, next) => {
                readInputs($);
                return next(e);
              });
              on('session.start', () => {});
              on('tool.call', { to: 'Bash' }, () => {});
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            uses,
            serde_json::json!({
                "events":["tool.call","session.start"],
                "calls":["env.get","fs.read","state.get"],
                "env":{"reads":["HOME"],"writes":[]},
                "state":{"reads":[{"plugin":"other-mod","key":"flag"}],"writes":[]}
            })
        );
    }

    #[test]
    fn plugin_register_uses_match_native_json_field_order() {
        let graph = super::super::client_source::PreparedHookSourceGraph {
            entry: "plugin.js".into(),
            files: vec![source_file(
                "plugin.js",
                r#"
                    export function register(on) {
                      on('tool.call', ($) => {
                        $.env.get('HOME');
                        $.state.get({ plugin: 'other-mod', key: 'flag' });
                      });
                    }
                "#,
            )],
            links: vec![],
        };
        let uses = scan_uses_graph(&graph).unwrap();
        assert_eq!(
            uses.to_json_string().unwrap(),
            r#"{"events":["tool.call"],"calls":["env.get","state.get"],"env":{"reads":["HOME"],"writes":[]},"state":{"reads":[{"plugin":"other-mod","key":"flag"}],"writes":[]}}"#
        );
    }

    #[test]
    fn plugin_register_ignores_calls_outside_reachable_handlers() {
        let uses = scan_uses(&[r#"
            function unused($) {
              $.tool.list();
            }
            export function register(on) {
              on('tool.call', ($, e, next) => next(e));
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(uses, serde_json::json!({"events":["tool.call"],"calls":[]}));
    }

    #[test]
    fn plugin_register_accepts_native_295_api_and_custom_event_spellings() {
        let uses = scan_uses(&[r#"
            export function register(on) {
              on('prompt.read', () => {});
              on('\x70rompt.submit', () => {});
              on("\x74ool.check", () => {});
              on(`tool.\u0063all`, () => {});
              on('!custom.event', () => {});
              on('custom.e·vent', () => {});
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            uses,
            serde_json::json!({
                "events":["prompt.read", "prompt.submit", "tool.check", "tool.call", "!custom.event", "custom.e·vent"],
                "calls":[]
            })
        );
    }

    #[test]
    fn plugin_register_matches_native_duplicate_event_rules() {
        let error = scan_uses(&[r#"
            export function register(on) {
              on('tool.call', () => {});
              on('tool.call', () => {});
            }
        "#
        .into()])
        .unwrap_err();
        assert!(error.contains("on(\"tool.call\") is registered twice without a matcher"));

        let error = scan_uses(&[r#"
            export function register(on) {
              on('tool.*', { to: 'Bash' }, () => {});
              on('tool.*', { to: 'Read' }, () => {});
            }
        "#
        .into()])
        .unwrap_err();
        assert!(error.contains("on(\"tool.*\") is registered twice"));
        assert!(!error.contains("without a matcher"));

        let uses = scan_uses(&[r#"
            export function register(on) {
              on('tool.call', { to: 'Bash' }, () => {});
              on('tool.call', { to: 'Bash' }, () => {});
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(uses, serde_json::json!({"events":["tool.call"],"calls":[]}));
    }

    #[test]
    fn plugin_register_follows_imported_callbacks_and_on_helpers_through_linked_files() {
        let graph = super::super::client_source::PreparedHookSourceGraph {
            entry: "plugin.js".into(),
            files: vec![
                source_file(
                    "plugin.js",
                    r#"
                      import { bind as setup } from './registra\u0074ion.js';
                      import handler from './hand\u006cer.js';
                      export function register(on) {
                        setup(on);
                        on('!custom.event', handler);
                      }
                    "#,
                ),
                source_file(
                    "registration.js",
                    r#"
                      export function bind(on) {
                        on('prompt.read', ($) => $.prompt.read());
                      }
                    "#,
                ),
                source_file(
                    "handler.js",
                    r#"
                      export default function handler($, event, next) {
                        $.prompt.read();
                        return next(event);
                      }
                    "#,
                ),
            ],
            links: vec![
                source_link("plugin.js", "./registration.js", "registration.js"),
                source_link("plugin.js", "./handler.js", "handler.js"),
            ],
        };

        let uses = scan_uses_graph(&graph).unwrap();
        assert_eq!(
            projection_json(&uses),
            serde_json::json!({
                "events":["prompt.read", "!custom.event"],
                "calls":["prompt.read"]
            })
        );
    }

    #[test]
    fn plugin_register_resolves_cooked_escaped_identifier_names() {
        let graph = super::super::client_source::PreparedHookSourceGraph {
            entry: "plugin.js".into(),
            files: vec![
                source_file(
                    "plugin.js",
                    r#"
                      import { \u0068andler as \u0063allback } from './handler.js';
                      export function \u0072egister(\u006fn) {
                        \u006fn('tool.call', callback);
                      }
                    "#,
                ),
                source_file(
                    "handler.js",
                    r#"
                      function \u0068andler($, event, next) {
                        $.tool.call();
                        return next(event);
                      }
                      export { \u0068andler as \u0068andler };
                    "#,
                ),
            ],
            links: vec![source_link("plugin.js", "./handler.js", "handler.js")],
        };

        let uses = scan_uses_graph(&graph).unwrap();
        assert_eq!(
            projection_json(&uses),
            serde_json::json!({
                "events": ["tool.call"],
                "calls": ["tool.call"]
            })
        );
    }

    #[test]
    fn plugin_register_resolves_static_state_identifiers_spreads_and_cooked_crlf() {
        let source = "\
            const base = { plugin: 'p', key: 'old', '\\uD800': dynamic, 1: dynamic };\n\
            export function register(on) {\n\
              const options = { ...base, key: `f\r\nl` };\n\
              on('tool.call', ($) => $.state.get({ ...options, plugin: 'q' }));\n\
            }\n\
        ";
        let uses = scan_uses(&[source.to_owned()]).unwrap();
        assert_eq!(
            uses,
            serde_json::json!({
                "events": ["tool.call"],
                "calls": ["state.get"],
                "state": {
                    "reads": [{ "plugin": "q", "key": "f\nl" }],
                    "writes": []
                }
            })
        );
    }

    #[test]
    fn plugin_register_excludes_exported_var_from_static_state_resolution() {
        let exported = scan_uses(&[r#"
            export var opts = { plugin: 'p', key: 'k' };
            export function register(on) {
              on('tool.call', ($) => $.state.get(opts));
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            exported,
            serde_json::json!({ "events": ["tool.call"], "calls": ["state.get"] })
        );

        let unexported = scan_uses(&[r#"
            var opts = { plugin: 'p', key: 'k' };
            export function register(on) {
              on('tool.call', ($) => $.state.get(opts));
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            unexported,
            serde_json::json!({
                "events": ["tool.call"],
                "calls": ["state.get"],
                "state": {
                    "reads": [{ "plugin": "p", "key": "k" }],
                    "writes": []
                }
            })
        );
    }

    #[test]
    fn plugin_register_refuses_rebound_var_and_hoisted_function_var_state_values() {
        let rebound = scan_uses(&[r#"
            var opts = { plugin: 'p', key: 'k' };
            opts = dynamicValue;
            export function register(on) {
              on('tool.call', ($) => $.state.get(opts));
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            rebound,
            serde_json::json!({ "events": ["tool.call"], "calls": ["state.get"] })
        );

        let hoisted_shadow = scan_uses(&[r#"
            const opts = { plugin: 'outer', key: 'outer-key' };
            export function register(on) {
              if (false) {
                var opts = dynamicValue;
              }
              on('tool.call', ($) => $.state.get(opts));
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            hoisted_shadow,
            serde_json::json!({ "events": ["tool.call"], "calls": ["state.get"] })
        );
    }

    #[test]
    fn plugin_register_reads_const_state_with_comment_after_keyword() {
        let uses = scan_uses(&[r#"
            const/*x*/ opts = { plugin: 'p', key: 'k' };
            export function register(on) {
              on('tool.call', ($) => $.state.get(opts));
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            uses,
            serde_json::json!({
                "events": ["tool.call"],
                "calls": ["state.get"],
                "state": {
                    "reads": [{ "plugin": "p", "key": "k" }],
                    "writes": []
                }
            })
        );
    }

    #[test]
    fn plugin_register_only_resolves_named_imports_from_named_exports() {
        let default_named_function = super::super::client_source::PreparedHookSourceGraph {
            entry: "plugin.js".into(),
            files: vec![
                source_file(
                    "plugin.js",
                    r#"
                      import { handler as callback } from './handler.js';
                      export function register(on) {
                        on('tool.call', callback);
                      }
                    "#,
                ),
                source_file(
                    "handler.js",
                    r#"
                      export default function handler($) {
                        $.env.get('HOME');
                      }
                    "#,
                ),
            ],
            links: vec![source_link("plugin.js", "./handler.js", "handler.js")],
        };
        let error = scan_uses_graph(&default_named_function).unwrap_err();
        assert!(error.contains("no export named handler"));

        let commented_const_function = super::super::client_source::PreparedHookSourceGraph {
            entry: "plugin.js".into(),
            files: vec![
                source_file(
                    "plugin.js",
                    r#"
                      import { handler } from './handler.js';
                      export function register(on) {
                        on('tool.call', handler);
                      }
                    "#,
                ),
                source_file(
                    "handler.js",
                    r#"
                      const/*x*/ handler = ($) => $.tool.call();
                      export { handler };
                    "#,
                ),
            ],
            links: vec![source_link("plugin.js", "./handler.js", "handler.js")],
        };
        let uses = scan_uses_graph(&commented_const_function).unwrap();
        assert_eq!(
            projection_json(&uses),
            serde_json::json!({ "events": ["tool.call"], "calls": ["tool.call"] })
        );

        let default_const_function = super::super::client_source::PreparedHookSourceGraph {
            entry: "plugin.js".into(),
            files: vec![
                source_file(
                    "plugin.js",
                    r#"
                      import handler from './handler.js';
                      export function register(on) {
                        on('tool.call', handler);
                      }
                    "#,
                ),
                source_file(
                    "handler.js",
                    r#"
                      const/*x*/ handler = ($) => $.tool.call();
                      export default handler;
                    "#,
                ),
            ],
            links: vec![source_link("plugin.js", "./handler.js", "handler.js")],
        };
        let uses = scan_uses_graph(&default_const_function).unwrap();
        assert_eq!(
            projection_json(&uses),
            serde_json::json!({ "events": ["tool.call"], "calls": ["tool.call"] })
        );

        let reassigned_export = super::super::client_source::PreparedHookSourceGraph {
            entry: "plugin.js".into(),
            files: vec![
                source_file(
                    "plugin.js",
                    r#"
                      import { handler } from './handler.js';
                      export function register(on) {
                        on('tool.call', handler);
                      }
                    "#,
                ),
                source_file(
                    "handler.js",
                    r#"
                      export function handler($) { $.tool.call(); }
                      handler = replacement;
                    "#,
                ),
            ],
            links: vec![source_link("plugin.js", "./handler.js", "handler.js")],
        };
        let error = scan_uses_graph(&reassigned_export).unwrap_err();
        assert!(error.contains("no export named handler"));

        let reassigned_string_alias = super::super::client_source::PreparedHookSourceGraph {
            entry: "plugin.js".into(),
            files: vec![
                source_file(
                    "plugin.js",
                    r#"
                      import { "aliased" as handler } from './handler.js';
                      export function register(on) {
                        on('tool.call', handler);
                      }
                    "#,
                ),
                source_file(
                    "handler.js",
                    r#"
                      function handler($) { $.tool.call(); }
                      handler = replacement;
                      export { handler as "aliased" };
                    "#,
                ),
            ],
            links: vec![source_link("plugin.js", "./handler.js", "handler.js")],
        };
        let error = scan_uses_graph(&reassigned_string_alias).unwrap_err();
        assert!(error.contains("no export named aliased"));
    }

    #[test]
    fn plugin_register_does_not_follow_nested_module_helpers() {
        let uses = scan_uses(&[r#"
            export function register(on) {
              const inner = ($) => $.state.get({ plugin: 'p', key: 'k' });
              on('tool.call', ($) => inner($));
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            uses,
            serde_json::json!({ "events": ["tool.call"], "calls": [] })
        );
    }

    #[test]
    fn plugin_register_matches_static_block_state_scopes_and_top_level_var_read_order() {
        let static_block_const = scan_uses(&[r#"
            export function register(on) {
              class C {
                static {
                  const opts = { plugin: 'inside', key: 'block' };
                  on('tool.call', ($) => $.state.get(opts));
                }
              }
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            static_block_const,
            serde_json::json!({
                "events": ["tool.call"],
                "calls": ["state.get"],
                "state": {
                    "reads": [{ "plugin": "inside", "key": "block" }],
                    "writes": []
                }
            })
        );

        let static_block_var_does_not_shadow_function_binding = scan_uses(&[r#"
            const opts = { plugin: 'outer', key: 'module' };
            export function register(on) {
              class C { static { var opts = dynamicValue; } }
              on('tool.call', ($) => $.state.get(opts));
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            static_block_var_does_not_shadow_function_binding,
            serde_json::json!({
                "events": ["tool.call"],
                "calls": ["state.get"],
                "state": {
                    "reads": [{ "plugin": "outer", "key": "module" }],
                    "writes": []
                }
            })
        );

        let static_block_var_shadows_inside_its_own_scope = scan_uses(&[r#"
            const opts = { plugin: 'outer', key: 'module' };
            export function register(on) {
              class C {
                static {
                  var opts = dynamicValue;
                  on('tool.call', ($) => $.state.get(opts));
                }
              }
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            static_block_var_shadows_inside_its_own_scope,
            serde_json::json!({ "events": ["tool.call"], "calls": ["state.get"] })
        );

        let early_read = scan_uses(&[r#"
            const previous = opts;
            var opts = { plugin: 'p', key: 'k' };
            export function register(on) {
              on('tool.call', ($) => $.state.get(opts));
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            early_read,
            serde_json::json!({ "events": ["tool.call"], "calls": ["state.get"] })
        );

        let early_shorthand_read = scan_uses(&[r#"
            const before = { opts };
            var opts = { plugin: 'p', key: 'k' };
            export function register(on) {
              on('tool.call', ($) => $.state.get(opts));
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            early_shorthand_read,
            serde_json::json!({ "events": ["tool.call"], "calls": ["state.get"] })
        );

        let deferred_read = scan_uses(&[r#"
            function readLater() { return opts; }
            var opts = { plugin: 'p', key: 'k' };
            export function register(on) {
              on('tool.call', ($) => $.state.get(opts));
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            deferred_read,
            serde_json::json!({
                "events": ["tool.call"],
                "calls": ["state.get"],
                "state": {
                    "reads": [{ "plugin": "p", "key": "k" }],
                    "writes": []
                }
            })
        );

        let shorthand_binding_is_not_an_early_read = scan_uses(&[r#"
            {
              const { opts } = dynamicValue;
            }
            var opts = { plugin: 'p', key: 'k' };
            export function register(on) {
              on('tool.call', ($) => $.state.get(opts));
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            shorthand_binding_is_not_an_early_read,
            serde_json::json!({
                "events": ["tool.call"],
                "calls": ["state.get"],
                "state": {
                    "reads": [{ "plugin": "p", "key": "k" }],
                    "writes": []
                }
            })
        );
    }

    #[test]
    fn plugin_register_has_no_fixed_static_alias_or_helper_depth_limit() {
        let mut aliases = String::from("const s0 = { plugin: 'p', key: 'k' };\n");
        for index in 1..=40 {
            aliases.push_str(&format!("const s{index} = s{};\n", index - 1));
        }
        aliases
            .push_str("export function register(on) { on('tool.call', ($) => $.state.get(s40)); }");
        let uses = scan_uses(&[aliases]).unwrap();
        assert_eq!(
            uses,
            serde_json::json!({
                "events": ["tool.call"],
                "calls": ["state.get"],
                "state": {
                    "reads": [{ "plugin": "p", "key": "k" }],
                    "writes": []
                }
            })
        );

        let mut helpers = String::new();
        for index in 0..40 {
            helpers.push_str(&format!("function h{index}($) {{ h{}($); }}\n", index + 1));
        }
        helpers.push_str("function h40($) { $.state.get({ plugin: 'p', key: 'k' }); }\n");
        helpers.push_str("export function register(on) { on('tool.call', ($) => h0($)); }");
        let uses = scan_uses(&[helpers]).unwrap();
        assert_eq!(
            uses,
            serde_json::json!({
                "events": ["tool.call"],
                "calls": ["state.get"],
                "state": {
                    "reads": [{ "plugin": "p", "key": "k" }],
                    "writes": []
                }
            })
        );
    }

    #[test]
    fn plugin_register_does_not_follow_reassigned_exports_or_helpers() {
        let reassigned_register = scan_uses(&[r#"
            export function register(on) {
              on('tool.call', ($) => $.state.get({ plugin: 'p', key: 'k' }));
            }
            register = replacement;
        "#
        .into()]);
        assert!(reassigned_register
            .unwrap_err()
            .contains("exported register function is assigned elsewhere"));

        let reassigned_helper = scan_uses(&[r#"
            function helper($) { $.state.get({ plugin: 'p', key: 'k' }); }
            helper = replacement;
            export function register(on) {
              on('tool.call', ($) => helper($));
            }
        "#
        .into()]);
        assert!(reassigned_helper
            .unwrap_err()
            .contains("local helper helper is assigned elsewhere"));

        let reassigned_helper_without_tracked_argument = scan_uses(&[r#"
            function helper() { }
            helper = replacement;
            export function register(on) {
              helper();
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            reassigned_helper_without_tracked_argument,
            serde_json::json!({ "events": [], "calls": [] })
        );

        let reassigned_imported_helper = super::super::client_source::PreparedHookSourceGraph {
            entry: "plugin.js".into(),
            files: vec![
                source_file(
                    "plugin.js",
                    r#"
                      import { helper } from './helper.js';
                      helper = replacement;
                      export function register(on) {
                        on('tool.call', ($) => helper($));
                      }
                    "#,
                ),
                source_file(
                    "helper.js",
                    r#"
                      export function helper($) { $.state.get({ plugin: 'p', key: 'k' }); }
                    "#,
                ),
            ],
            links: vec![source_link("plugin.js", "./helper.js", "helper.js")],
        };
        let error = scan_uses_graph(&reassigned_imported_helper).unwrap_err();
        assert!(error.contains("imported helper helper is assigned elsewhere"));
    }

    #[test]
    fn plugin_register_resolves_string_named_exports_and_preserves_surrogate_state_units() {
        let graph = super::super::client_source::PreparedHookSourceGraph {
            entry: "plugin.js".into(),
            files: vec![
                source_file(
                    "plugin.js",
                    r#"
                      import { "handler" as asciiCallback, "hand\uD800" as callback } from './handler.js';
                      export function register(on) {
                        on('tool.call', callback);
                        on('tool.describe', asciiCallback);
                        on('tool.check', ($) => $.state.get({ plugin: 'p', key: '\uD800' }));
                      }
                    "#,
                ),
                source_file(
                    "handler.js",
                    r#"
                      function handler($, event, next) {
                        $.tool.call();
                        return next(event);
                      }
                      export { handler as "handler", handler as "hand\uD800" };
                    "#,
                ),
            ],
            links: vec![source_link("plugin.js", "./handler.js", "handler.js")],
        };

        let uses = scan_uses_graph(&graph).unwrap();
        assert_eq!(uses.string_at("/events/0"), Some("tool.call"));
        assert_eq!(uses.string_at("/events/1"), Some("tool.describe"));
        assert_eq!(uses.string_at("/events/2"), Some("tool.check"));
        assert_eq!(
            uses.string_units("/state/reads/0/plugin"),
            Some(vec![b'p' as u16])
        );
        assert_eq!(uses.string_units("/state/reads/0/key"), Some(vec![0xd800]));
        assert_eq!(uses.string_at("/calls/0"), Some("state.get"));
        assert_eq!(uses.string_at("/calls/1"), Some("tool.call"));
    }

    #[test]
    fn plugin_register_cooks_escaped_literal_state_names() {
        let uses = scan_uses(&[r#"
            export function register(on) {
              on('tool.call', ($) => {
                $.state.get({ plugin: 'custom-\u006dod', key: `fl\u0061g` });
              });
            }
        "#
        .into()])
        .unwrap();
        assert_eq!(
            uses,
            serde_json::json!({
                "events":["tool.call"],
                "calls":["state.get"],
                "state":{"reads":[{"plugin":"custom-mod","key":"flag"}],"writes":[]}
            })
        );
    }

    #[test]
    fn plugin_register_refuses_dynamic_event_names() {
        let error = scan_uses(&[r#"
            export function register(on) {
              on(EVENT, ($, e, next) => next(e));
            }
        "#
        .into()])
        .unwrap_err();
        assert!(error.contains("event name passed to on() is not a string literal"));

        let surrogate_event = scan_uses(&[r#"
            export function register(on) {
              on('\uD800', () => {});
            }
        "#
        .into()])
        .unwrap_err();
        assert!(surrogate_event.contains("is not an event name or a pattern"));
    }

    #[test]
    fn plugin_register_refuses_computed_api_methods() {
        let error = scan_uses(&[r#"
            export function register(on) {
              on('tool.call', ($, e, next) => {
                return $.fs[name]('README.md');
              });
            }
        "#
        .into()])
        .unwrap_err();
        assert!(error.contains("$.fs is used as a value"));
    }

    #[test]
    fn plugin_register_refuses_api_namespace_aliases() {
        let error = scan_uses(&[r#"
            export function register(on) {
              on('tool.call', ($, e, next) => {
                const fs = $.fs;
                return next(e);
              });
            }
        "#
        .into()])
        .unwrap_err();
        assert!(error.contains("$.fs is used as a value"));
    }
}
