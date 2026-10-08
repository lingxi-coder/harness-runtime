//! Conservative source scans for Mod environment-variable grants and the
//! pre-evaluation `plugin.register` uses report. The worker sends type-stripped
//! source from every linked module, so comments and strings are parsed as data
//! rather than searched with a regular expression.

use std::collections::{BTreeSet, HashMap, HashSet};
use tree_sitter::Node;

#[derive(Default)]
pub(super) struct EnvScan {
    pub reads: HashSet<String>,
    pub writes: HashSet<String>,
}

fn source_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    std::str::from_utf8(&source[node.byte_range()]).unwrap_or("")
}

fn env_method<'a>(function: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
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
        || source_text(env_name, source) != "env"
        || dollar.kind() != "identifier"
        || source_text(dollar, source) != "$"
    {
        return None;
    }
    match source_text(method, source) {
        "get" => Some("get"),
        "set" => Some("set"),
        _ => None,
    }
}

fn api_method<'a>(
    function: Node<'_>,
    source: &'a [u8],
    dollar_names: &HashSet<String>,
) -> Option<(&'a str, &'a str)> {
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
    (noun_name.kind() == "property_identifier"
        && dollar.kind() == "identifier"
        && dollar_names.contains(source_text(dollar, source)))
    .then(|| (source_text(noun_name, source), source_text(method, source)))
}

fn api_namespace<'a>(
    node: Node<'_>,
    source: &'a [u8],
    dollar_names: &HashSet<String>,
) -> Option<&'a str> {
    if node.kind() != "member_expression" {
        return None;
    }
    let namespace = node.child_by_field_name("property")?;
    let dollar = node.child_by_field_name("object")?;
    (namespace.kind() == "property_identifier"
        && dollar.kind() == "identifier"
        && dollar_names.contains(source_text(dollar, source)))
    .then(|| source_text(namespace, source))
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
    let line = source[..node.start_byte().min(source.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1;
    format!("plugin.register scan refused at line {line}: {message}")
}

fn literal_name(argument: Node<'_>, source: &[u8]) -> Option<String> {
    if argument.kind() != "string" {
        return None;
    }
    let raw = source_text(argument, source);
    if raw.starts_with('"') {
        return serde_json::from_str(raw).ok();
    }
    let inner = raw.strip_prefix('\'')?.strip_suffix('\'')?;
    if inner.contains('\\') {
        return None;
    }
    Some(inner.to_owned())
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
    calls: BTreeSet<String>,
    env_reads: BTreeSet<String>,
    env_writes: BTreeSet<String>,
    state_reads: BTreeSet<(String, String)>,
    state_writes: BTreeSet<(String, String)>,
    visited: HashSet<(usize, Vec<(String, u8)>)>,
}

fn is_function(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_declaration" | "function_expression" | "arrow_function" | "generator_function"
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
    node: Node<'tree>,
    source: &[u8],
    functions: &mut HashMap<String, Option<Node<'tree>>>,
) {
    if node.kind() == "function_declaration" {
        if let Some(name) = node.child_by_field_name("name") {
            add_function(functions, source_text(name, source).to_owned(), node);
        }
    } else if node.kind() == "variable_declarator" {
        if let (Some(name), Some(value)) = (
            node.child_by_field_name("name"),
            node.child_by_field_name("value"),
        ) {
            if name.kind() == "identifier" {
                if let Some(function) = function_expression(value) {
                    add_function(functions, source_text(name, source).to_owned(), function);
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_functions(child, source, functions);
    }
}

fn exported_register<'tree>(root: Node<'tree>, source: &[u8]) -> Option<Node<'tree>> {
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
            let name = declaration.child_by_field_name("name");
            if name.is_some_and(|name| source_text(name, source) == "register") {
                return Some(declaration);
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
                if name.kind() == "identifier" && source_text(name, source) == "register" {
                    return function_expression(value);
                }
            }
        }
    }
    None
}

fn state_reference(argument: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    if argument.kind() != "object" {
        return None;
    }
    let mut plugin = None;
    let mut key = None;
    let mut cursor = argument.walk();
    for property in argument.named_children(&mut cursor) {
        if property.kind() != "pair" {
            return None;
        }
        let property_key = property.child_by_field_name("key")?;
        let property_value = property.child_by_field_name("value")?;
        let name = match property_key.kind() {
            "property_identifier" => source_text(property_key, source).to_owned(),
            "string" => literal_name(property_key, source)?,
            _ => return None,
        };
        let value = literal_name(property_value, source)?;
        match name.as_str() {
            "plugin" if plugin.is_none() => plugin = Some(value),
            "key" if key.is_none() => key = Some(value),
            "plugin" | "key" => return None,
            _ => {}
        }
    }
    Some((plugin?, key?))
}

fn call_arguments(call: Node<'_>) -> Vec<Node<'_>> {
    call.child_by_field_name("arguments")
        .map(|arguments| {
            let mut cursor = arguments.walk();
            arguments.named_children(&mut cursor).collect()
        })
        .unwrap_or_default()
}

fn scan_function<'tree>(
    function: Node<'tree>,
    bindings: HashMap<String, UseBinding>,
    functions: &HashMap<String, Option<Node<'tree>>>,
    source: &[u8],
    scan: &mut UsesScan,
    depth: usize,
) -> Result<(), String> {
    if depth > 32 {
        return Ok(());
    }
    let mut binding_key = bindings
        .iter()
        .map(|(name, binding)| (name.clone(), u8::from(*binding == UseBinding::Api)))
        .collect::<Vec<_>>();
    binding_key.sort();
    if !scan.visited.insert((function.start_byte(), binding_key)) {
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
                    .then(|| source_text(function_node, source));
                if function_name.is_some_and(|name| on_names.contains(name)) {
                    let event = arguments
                        .first()
                        .and_then(|argument| literal_name(*argument, source))
                        .ok_or_else(|| {
                            scan_refusal(
                                source,
                                node,
                                "the event name passed to on() is not a string literal",
                            )
                        })?;
                    if scan.seen_events.insert(event.clone()) {
                        scan.events.push(event);
                    }
                    if let Some(handler) = arguments.last().copied() {
                        let handler = function_expression(handler).or_else(|| {
                            (handler.kind() == "identifier")
                                .then(|| source_text(handler, source))
                                .and_then(|name| functions.get(name).copied().flatten())
                        });
                        if let Some(handler) = handler {
                            let mut handler_bindings = HashMap::new();
                            if let Some(parameter) = function_parameters(handler).first() {
                                if parameter.kind() == "identifier" {
                                    handler_bindings.insert(
                                        source_text(*parameter, source).to_owned(),
                                        UseBinding::Api,
                                    );
                                }
                            }
                            scan_function(
                                handler,
                                handler_bindings,
                                functions,
                                source,
                                scan,
                                depth + 1,
                            )?;
                        }
                    }
                } else if let Some((noun, method)) = api_method(function_node, source, &api_names) {
                    scan.calls.insert(format!("{noun}.{method}"));
                    let first_argument = arguments.first().copied();
                    match (noun, method) {
                        ("env", "get" | "set") => {
                            if let Some(name) =
                                first_argument.and_then(|argument| literal_name(argument, source))
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
                    if let Some(Some(helper)) = functions.get(name) {
                        if helper.start_byte() != function.start_byte() {
                            let parameters = function_parameters(*helper);
                            let mut helper_bindings = HashMap::new();
                            for (parameter, argument) in parameters.iter().zip(arguments.iter()) {
                                if parameter.kind() != "identifier"
                                    || argument.kind() != "identifier"
                                {
                                    continue;
                                }
                                if let Some(binding) = bindings.get(source_text(*argument, source))
                                {
                                    helper_bindings.insert(
                                        source_text(*parameter, source).to_owned(),
                                        *binding,
                                    );
                                }
                            }
                            if !helper_bindings.is_empty() {
                                scan_function(
                                    *helper,
                                    helper_bindings,
                                    functions,
                                    source,
                                    scan,
                                    depth + 1,
                                )?;
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
/// Code 2.1.288 derives `plugin.register` uses from the exported `register(on)`
/// entry and the handler functions it reaches. API calls in dead helpers or
/// outside those handlers are not reported. Events keep registration order;
/// calls and literal env/state references are sorted.
pub(super) fn scan_uses(sources: &[String]) -> Result<serde_json::Value, String> {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .is_err()
    {
        return Ok(serde_json::json!({"events":[],"calls":[]}));
    }
    let mut scan = UsesScan::default();
    for source in sources {
        let Some(tree) = parser.parse(source, None) else {
            continue;
        };
        if tree.root_node().has_error() {
            continue;
        }
        let bytes = source.as_bytes();
        let root = tree.root_node();
        let mut functions = HashMap::new();
        collect_functions(root, bytes, &mut functions);
        let Some(register) = exported_register(root, bytes) else {
            continue;
        };
        // Byte offsets are only unique inside one parsed source tree.
        scan.visited.clear();
        let mut bindings = HashMap::new();
        if let Some(parameter) = function_parameters(register).first() {
            if parameter.kind() == "identifier" {
                bindings.insert(source_text(*parameter, bytes).to_owned(), UseBinding::On);
            }
        }
        scan_function(register, bindings, &functions, bytes, &mut scan, 0)?;
    }

    let mut result = serde_json::json!({
        "events":scan.events,
        "calls":scan.calls.into_iter().collect::<Vec<_>>()
    });
    if !scan.env_reads.is_empty() || !scan.env_writes.is_empty() {
        result["env"] = serde_json::json!({
            "reads":scan.env_reads.into_iter().collect::<Vec<_>>(),
            "writes":scan.env_writes.into_iter().collect::<Vec<_>>()
        });
    }
    if !scan.state_reads.is_empty() || !scan.state_writes.is_empty() {
        let state_refs = |items: BTreeSet<(String, String)>| {
            items
                .into_iter()
                .map(|(plugin, key)| serde_json::json!({"plugin":plugin,"key":key}))
                .collect::<Vec<_>>()
        };
        result["state"] = serde_json::json!({
            "reads":state_refs(scan.state_reads),
            "writes":state_refs(scan.state_writes)
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

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
              on('tool.call', () => {});
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
    fn plugin_register_refuses_dynamic_event_names() {
        let error = scan_uses(&[r#"
            export function register(on) {
              on(EVENT, ($, e, next) => next(e));
            }
        "#
        .into()])
        .unwrap_err();
        assert!(error.contains("event name passed to on() is not a string literal"));
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
