//! Host validation for the terminal `AbovePrompt` subset and 2.1.289 Desktop
//! parent UI trees. Terminal rendering remains intentionally narrower than
//! the Desktop surface schema.

use serde_json::Value;
use std::collections::HashSet;

const MAX_SAFE_JS_INTEGER: u64 = 9_007_199_254_740_991;

/// Validate the exact `Client` parent-node schema pinned by Native 2.1.289.
pub(super) fn validate_client_parent_element(node: &Value) -> Result<(), String> {
    let Some(props) = node.get("props").and_then(Value::as_object) else {
        return Err("Client props must be an object".into());
    };
    if props.keys().any(|key| {
        !matches!(
            key.as_str(),
            "key" | "module" | "props" | "width" | "height" | "flexGrow"
        )
    }) {
        return Err("Client props contain an unsupported field".into());
    }
    for field in ["key", "module"] {
        if props
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(format!("Client props need non-empty {field}"));
        }
    }
    if let Some(value) = props.get("props") {
        validate_client_plain_data(value, "Client props")?;
    }
    for field in ["width", "height"] {
        if let Some(value) = props.get(field) {
            let valid = value
                .as_f64()
                .is_some_and(|n| n.is_finite() && (0.0..=10_000.0).contains(&n))
                || value.as_str().is_some_and(valid_client_percentage);
            if !valid {
                return Err(format!(
                    "Client {field} must be a number from 0 to 10000 or a percentage"
                ));
            }
        }
    }
    if let Some(value) = props.get("flexGrow") {
        if !value
            .as_f64()
            .is_some_and(|n| n.is_finite() && n.abs() <= 10_000.0)
        {
            return Err("Client flexGrow must be a number with magnitude at most 10000".into());
        }
    }
    if node.get("children").is_some() {
        return Err(format!(
            "Client \"{}\" takes no children",
            props["key"].as_str().unwrap_or("")
        ));
    }
    Ok(())
}

fn valid_client_percentage(value: &str) -> bool {
    let Some(digits) = value.strip_suffix('%') else {
        return false;
    };
    (1..=3).contains(&digits.len()) && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn validate_client_plain_data(value: &Value, label: &str) -> Result<(), String> {
    let mut values = 0usize;
    count_plain_data_values(value, 0, &mut values, label)?;
    let serialized = serde_json::to_string(value).map_err(|error| error.to_string())?;
    if serialized.encode_utf16().count() > 100_000 {
        return Err(format!("{label} exceeds 100000 serialized characters"));
    }
    Ok(())
}

fn count_plain_data_values(
    value: &Value,
    depth: usize,
    values: &mut usize,
    label: &str,
) -> Result<(), String> {
    if depth > 32 {
        return Err(format!("{label} exceeds nesting depth 32"));
    }
    *values = values.saturating_add(1);
    if *values > 20_000 {
        return Err(format!("{label} exceeds 20000 values"));
    }
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
        Value::Array(items) => {
            for item in items {
                count_plain_data_values(item, depth + 1, values, label)?;
            }
            Ok(())
        }
        Value::Object(items) => {
            for item in items.values() {
                count_plain_data_values(item, depth + 1, values, label)?;
            }
            Ok(())
        }
    }
}

const DESKTOP_RENDER_COMPONENTS: &[&str] = &[
    "AskUserQuestion",
    "UserMessage",
    "AssistantMessage",
    "ToolUse",
    "ToolResult",
    "ToolGroup",
    "ToolProgress",
    "CommandOutput",
    "Spinner",
    "TurnDuration",
    "InfoNotice",
    "SessionMode",
    "PromptHint",
    "AbovePrompt",
    "Pane",
];

/// Normalize the Desktop `ui_render` hook event through Native's Zod object
/// schemas: unknown envelope and metadata keys are stripped, while arbitrary
/// user-provided `props` keys remain intact.
pub(crate) fn normalize_ui_render_event(input: &Value) -> Result<Value, String> {
    let surface = input
        .get("surface")
        .and_then(Value::as_str)
        .ok_or_else(|| "ui.render needs a surface".to_owned())?;
    if surface == "terminal" {
        validate_above_prompt_event(input)?;
        return Ok(input.clone());
    }
    let mut normalized = input.clone();
    if let Some(object) = normalized.as_object_mut() {
        object.retain(|key, _| {
            matches!(
                key.as_str(),
                "surface"
                    | "component"
                    | "requestId"
                    | "clientId"
                    | "props"
                    | "viewport"
                    | "onScreen"
                    | "contentRows"
                    | "keyed"
                    | "bench"
            )
        });
        if let Some(viewport) = object.get_mut("viewport").and_then(Value::as_object_mut) {
            viewport.retain(|key, _| matches!(key.as_str(), "columns" | "rows" | "isFullscreen"));
        }
        if let Some(on_screen) = object.get_mut("onScreen").and_then(Value::as_object_mut) {
            on_screen.retain(|key, _| matches!(key.as_str(), "first" | "last" | "of"));
        }
        if let Some(rows) = object.get_mut("keyed").and_then(Value::as_array_mut) {
            for row in rows {
                if let Some(row) = row.as_object_mut() {
                    row.retain(|key, _| {
                        matches!(key.as_str(), "plugin" | "key" | "top" | "bottom")
                    });
                }
            }
        }
        if let Some(bench) = object.get_mut("bench").and_then(Value::as_object_mut) {
            bench.retain(|key, _| key == "seq" || key == "t0");
        }
    }
    validate_ui_render_event_shape(&normalized)?;
    Ok(normalized)
}

fn validate_ui_render_event_shape(input: &Value) -> Result<(), String> {
    let surface = input
        .get("surface")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let component = input
        .get("component")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !matches!(surface, "desktop" | "mobile" | "vscode")
        || !DESKTOP_RENDER_COMPONENTS.contains(&component)
    {
        return Err("ui.render needs a supported surface and component".into());
    }
    if input
        .get("requestId")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err("ui.render needs a non-empty requestId".into());
    }
    let props = input
        .get("props")
        .and_then(Value::as_object)
        .ok_or_else(|| "ui.render props must be an object".to_owned())?;
    validate_client_plain_data(&Value::Object(props.clone()), "ui.render props")?;

    if let Some(client_id) = input.get("clientId") {
        let valid = client_id.as_str().is_some_and(|value| {
            (1..=64).contains(&value.len())
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        });
        if !valid {
            return Err("ui.render clientId must be 1 to 64 safe ASCII characters".into());
        }
    }
    if let Some(viewport) = input.get("viewport") {
        let Some(viewport) = viewport.as_object() else {
            return Err("ui.render viewport must be an object".into());
        };
        if viewport
            .get("columns")
            .is_none_or(|value| !safe_unsigned(value, 1))
            || viewport
                .get("rows")
                .is_none_or(|value| !safe_unsigned(value, 1))
            || viewport
                .get("isFullscreen")
                .is_some_and(|value| !value.is_boolean())
        {
            return Err("ui.render viewport needs positive columns and rows".into());
        }
    }
    if let Some(on_screen) = input.get("onScreen") {
        if !on_screen.is_null() {
            let Some(on_screen) = on_screen.as_object() else {
                return Err("ui.render onScreen must be null or { first, last, of }".into());
            };
            let first = on_screen.get("first").and_then(safe_unsigned_value);
            let last = on_screen.get("last").and_then(safe_unsigned_value);
            let of = on_screen.get("of").and_then(safe_unsigned_value);
            if first.is_none() || last.is_none() || of.is_none() || first > last || last >= of {
                return Err("ui.render onScreen must satisfy first <= last < of".into());
            }
        }
    }
    if input
        .get("contentRows")
        .is_some_and(|value| !safe_unsigned(value, 0))
    {
        return Err("ui.render contentRows must be a non-negative integer".into());
    }
    if let Some(keyed) = input.get("keyed") {
        let Some(rows) = keyed.as_array() else {
            return Err("ui.render keyed must be an array".into());
        };
        if rows.len() > 512 {
            return Err("ui.render keyed is limited to 512 rows".into());
        }
        for row in rows {
            let Some(row) = row.as_object() else {
                return Err("ui.render keyed rows must be objects".into());
            };
            let top = row.get("top").and_then(safe_unsigned_value);
            let bottom = row.get("bottom").and_then(safe_unsigned_value);
            if row.get("plugin").and_then(Value::as_str).is_none()
                || row.get("key").and_then(Value::as_str).is_none()
                || top.is_none()
                || bottom.is_none()
            {
                return Err("ui.render keyed rows need plugin, key, top and bottom".into());
            }
        }
    }
    if let Some(bench) = input.get("bench") {
        let Some(bench) = bench.as_object() else {
            return Err("ui.render bench must be { seq, t0 }".into());
        };
        if bench.get("seq").is_none_or(|value| {
            !safe_integer(
                value,
                -(MAX_SAFE_JS_INTEGER as i64),
                MAX_SAFE_JS_INTEGER as i64,
            )
        }) || bench
            .get("t0")
            .and_then(Value::as_f64)
            .is_none_or(|value| !value.is_finite())
        {
            return Err("ui.render bench must contain integer seq and numeric t0".into());
        }
    }
    Ok(())
}

/// Validate the bounded Desktop parent tree after UI hook rewriting. Client
/// ownership stamping remains a separate call so the resolver chain can pass
/// its previous-identity set explicitly.
pub(crate) fn validate_desktop_parent_tree(tree: &Value) -> Result<(), String> {
    let encoded = serde_json::to_string(tree).map_err(|error| error.to_string())?;
    if encoded.encode_utf16().count() > 100_000 {
        return Err("Desktop UI tree exceeds 100000 serialized characters".into());
    }
    let mut budget = DesktopTreeBudget { nodes: 0 };
    validate_desktop_node(tree, 0, &mut budget, false)
}

#[derive(Default)]
struct DesktopTreeBudget {
    nodes: usize,
}

fn validate_desktop_node(
    node: &Value,
    depth: usize,
    budget: &mut DesktopTreeBudget,
    inherited_hover_scope: bool,
) -> Result<(), String> {
    if depth > 32 {
        return Err("Desktop UI tree exceeds nesting depth 32".into());
    }
    if let Some(text) = node.as_str() {
        if text.encode_utf16().count() > 10_000 {
            return Err("Desktop UI text exceeds 10000 characters".into());
        }
        return Ok(());
    }
    budget.nodes += 1;
    if budget.nodes > 20_000 {
        return Err("Desktop UI tree exceeds 20000 elements".into());
    }
    let Some(object) = node.as_object() else {
        return Err("Desktop UI tree children must be text or element objects".into());
    };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "Desktop UI node needs a type".to_owned())?;
    if kind == "engine" {
        if object.keys().any(|key| key != "type" && key != "ref")
            || object.get("ref").is_none_or(|value| {
                !safe_integer(
                    value,
                    -(MAX_SAFE_JS_INTEGER as i64),
                    MAX_SAFE_JS_INTEGER as i64,
                )
            })
        {
            return Err("engine nodes need an integer ref".into());
        }
        return Ok(());
    }
    let empty_props = serde_json::Map::new();
    let props = match object.get("props") {
        Some(Value::Object(props)) => props,
        None if matches!(kind, "Box" | "Text" | "div" | "span" | "b") => &empty_props,
        _ => return Err(format!("{kind} props must be an object")),
    };
    let hover_scope = inherited_hover_scope
        || (kind == "Box"
            && props
                .get("key")
                .and_then(Value::as_str)
                .is_some_and(|key| !key.is_empty()));
    validate_client_plain_data(&Value::Object(props.clone()), "Desktop UI props")?;
    if let Some(hover) = object.get("hover") {
        if !matches!(kind, "Box" | "Text" | "Button") {
            return Err(format!("{kind} cannot carry hover metadata"));
        }
        validate_parent_hover(kind, hover, props, object, hover_scope)?;
    }
    if kind == "Box"
        && props.get("key").and_then(Value::as_str).is_some()
        && props.get("display").and_then(Value::as_str) == Some("none")
        && subtree_has_hover(node)
        && object
            .get("hover")
            .and_then(Value::as_object)
            .and_then(|hover| hover.get("display"))
            .and_then(Value::as_str)
            != Some("flex")
    {
        return Err("a hidden keyed Box needs hover display flex to reveal its scope".into());
    }

    let mut allowed = HashSet::from(["type", "props"]);
    match kind {
        "Box" | "Text" | "div" | "span" | "b" => {
            allowed.insert("children");
            allowed.insert("hover");
            allowed.insert("group");
            if let Some(group) = object.get("group") {
                validate_parent_group(group)?;
            }
            if let Some(children) = object.get("children") {
                let Some(children) = children.as_array() else {
                    return Err(format!("{kind} children must be an array"));
                };
                for child in children {
                    if kind == "Text" && !child.is_string() {
                        return Err("Text children must be strings".into());
                    }
                    if !child.is_string() {
                        validate_desktop_node(child, depth + 1, budget, hover_scope)?;
                    } else if child
                        .as_str()
                        .is_some_and(|text| text.encode_utf16().count() > 10_000)
                    {
                        return Err(format!("{kind} child text exceeds 10000 characters"));
                    }
                }
            }
            match kind {
                "Box" => validate_box_props(props)?,
                "Text" => validate_text_props(props)?,
                "div" | "span" | "b" => {
                    if props.values().any(|value| {
                        !(value.is_string() || value.is_boolean() || value.is_number())
                    }) {
                        return Err(format!(
                            "{kind} props must contain only strings, numbers, and booleans"
                        ));
                    }
                }
                _ => unreachable!(),
            }
        }
        "Button" => {
            allowed.insert("press");
            allowed.insert("hover");
            validate_button_props(props)?;
            validate_parent_press(object, "Button")?;
        }
        "Input" => {
            allowed.insert("press");
            validate_input_props(props)?;
            validate_parent_press(object, "Input")?;
        }
        "Select" => {
            allowed.insert("press");
            validate_select_props(props)?;
            validate_parent_press(object, "Select")?;
        }
        "Link" => {
            allowed.insert("children");
            validate_link_props(props)?;
            if let Some(children) = object.get("children") {
                let Some(children) = children.as_array() else {
                    return Err("Link children must be an array of text".into());
                };
                if children.iter().any(|child| !child.is_string()) {
                    return Err("Link children must be strings".into());
                }
                if children.iter().any(|child| {
                    child
                        .as_str()
                        .is_some_and(|text| text.encode_utf16().count() > 10_000)
                }) {
                    return Err("Link child text exceeds 10000 characters".into());
                }
            }
        }
        "Code" => validate_code_props(props)?,
        "Markdown" => {
            allowed.insert("press");
            if object.contains_key("press") {
                validate_parent_press(object, "Markdown")?;
                if props
                    .get("key")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err("pressable Markdown needs a non-empty key".into());
                }
            } else if props
                .get("pressableLinks")
                .and_then(Value::as_array)
                .is_some_and(|links| !links.is_empty())
            {
                return Err("Markdown pressableLinks need a private press identity".into());
            }
            validate_markdown_props(props)?;
        }
        "Client" => {
            allowed.insert("client");
            validate_client_parent_element(node)?;
            let client = object
                .get("client")
                .and_then(Value::as_object)
                .ok_or_else(|| "Client needs its private owner stamp".to_owned())?;
            if client.keys().any(|key| key != "plugin")
                || client
                    .get("plugin")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                return Err("Client owner stamp needs a non-empty plugin".into());
            }
        }
        "Svg" => {
            const ALLOWED: &[&str] = &["source", "alt", "width", "height", "isInteractive"];
            if !keys_are_only(props, ALLOWED)
                || props
                    .get("source")
                    .and_then(Value::as_str)
                    .is_none_or(|source| source.encode_utf16().count() > 131_072)
                || props.get("alt").and_then(Value::as_str).is_none_or(|alt| {
                    alt.encode_utf16().count() > 10_000
                        || alt.chars().any(|character| character.is_control())
                })
                || ["width", "height"].iter().any(|key| {
                    props
                        .get(*key)
                        .is_some_and(|value| !finite_number(value, f64::MIN_POSITIVE, 4096.0))
                })
                || props
                    .get("isInteractive")
                    .is_some_and(|value| !value.is_boolean())
            {
                return Err("Svg props do not match the Desktop schema".into());
            }
        }
        _ => return Err(format!("unsupported Desktop UI element {kind}")),
    }
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(format!("{kind} node contains unsupported fields"));
    }
    Ok(())
}

fn validate_parent_group(group: &Value) -> Result<(), String> {
    let Some(group) = group.as_object() else {
        return Err("Desktop UI group must contain a plugin".into());
    };
    if group.keys().any(|key| key != "plugin")
        || group
            .get("plugin")
            .and_then(Value::as_str)
            .is_none_or(|plugin| plugin.is_empty() || plugin.encode_utf16().count() > 256)
    {
        return Err("Desktop UI group must contain a 1 to 256 character plugin".into());
    }
    Ok(())
}

fn validate_parent_hover(
    kind: &str,
    hover: &Value,
    base_props: &serde_json::Map<String, Value>,
    node: &serde_json::Map<String, Value>,
    has_keyed_box_scope: bool,
) -> Result<(), String> {
    let Some(hover) = hover.as_object() else {
        return Err("Desktop UI hover metadata must be an object".into());
    };
    let explicit_scope = hover.get("scope");
    if explicit_scope.is_some_and(|scope| {
        scope.as_str().is_none_or(|scope| {
            scope.is_empty()
                || scope.encode_utf16().count() > 64
                || scope.chars().any(char::is_control)
        })
    }) {
        return Err("Desktop UI hover scope must be 1 to 64 safe characters".into());
    }
    if explicit_scope.is_none() && !has_keyed_box_scope {
        return Err("Desktop UI hover needs a keyed Box scope".into());
    }
    if hover.values().any(|value| {
        value.is_null()
            || value.is_array()
            || value.is_object()
            || value.as_f64().is_some_and(|number| !number.is_finite())
    }) {
        return Err("Desktop UI hover values must be finite primitive values".into());
    }
    let allowed: &[&str] = match kind {
        "Box" => &[
            "scope",
            "borderStyle",
            "borderColor",
            "borderDimColor",
            "backgroundColor",
            "display",
            "top",
            "left",
            "right",
            "bottom",
        ],
        "Text" | "Button" => &[
            "scope",
            "color",
            "backgroundColor",
            "dimColor",
            "bold",
            "italic",
            "underline",
            "strikethrough",
            "inverse",
        ],
        _ => &[],
    };
    if !keys_are_only(hover, allowed) {
        return Err(format!("{kind} hover contains an unsupported style"));
    }
    if explicit_scope.is_some() {
        match kind {
            "Box" | "Text" | "div" | "span" | "b" => validate_parent_group(
                node.get("group")
                    .ok_or_else(|| format!("{kind} scoped hover needs a group owner"))?,
            )?,
            "Button" => validate_parent_press(node, "Button")?,
            _ => {}
        }
    }
    for key in ["borderColor", "backgroundColor", "color"] {
        if hover.get(key).is_some_and(|value| !valid_color(value)) {
            return Err(format!("{kind} hover {key} must be a color string"));
        }
    }
    for key in [
        "borderDimColor",
        "dimColor",
        "bold",
        "italic",
        "underline",
        "strikethrough",
        "inverse",
    ] {
        if hover.get(key).is_some_and(|value| !value.is_boolean()) {
            return Err(format!("{kind} hover {key} must be a boolean"));
        }
    }
    if let Some(border_style) = hover.get("borderStyle") {
        const BORDER_STYLES: &[&str] = &[
            "single",
            "double",
            "round",
            "bold",
            "singleDouble",
            "doubleSingle",
            "classic",
            "arrow",
            "dashed",
            "quote",
        ];
        if !base_props.contains_key("borderStyle")
            || border_style
                .as_str()
                .is_none_or(|style| !BORDER_STYLES.contains(&style))
        {
            return Err("Box hover borderStyle needs a base border and valid style".into());
        }
    }
    if hover.get("display").is_some_and(|value| {
        base_props.get("display").and_then(Value::as_str) != Some("none")
            || value.as_str() != Some("flex")
    }) {
        return Err("Box hover display can only restore flex from a hidden base Box".into());
    }
    for key in ["top", "left", "right", "bottom"] {
        if let Some(value) = hover.get(key) {
            if base_props.get("position").and_then(Value::as_str) != Some("absolute")
                || !safe_integer(value, -10_000, 10_000)
            {
                return Err(format!(
                    "Box hover {key} needs an absolute base position and bounded integer"
                ));
            }
        }
    }
    Ok(())
}

fn subtree_has_hover(node: &Value) -> bool {
    let Some(object) = node.as_object() else {
        return false;
    };
    object.contains_key("hover")
        || object
            .get("children")
            .and_then(Value::as_array)
            .is_some_and(|children| children.iter().any(subtree_has_hover))
}

fn validate_parent_press(node: &serde_json::Map<String, Value>, kind: &str) -> Result<(), String> {
    let Some(press) = node.get("press").and_then(Value::as_object) else {
        return Err(format!("{kind} needs a private press identity"));
    };
    if press.keys().any(|key| key != "plugin" && key != "handle")
        || press
            .get("plugin")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || press
            .get("handle")
            .and_then(Value::as_u64)
            .is_none_or(|handle| handle == 0 || handle > MAX_SAFE_JS_INTEGER)
    {
        return Err(format!(
            "{kind} press identity must be {{ plugin, handle }}"
        ));
    }
    Ok(())
}

fn keys_are_only(props: &serde_json::Map<String, Value>, allowed: &[&str]) -> bool {
    props.keys().all(|key| allowed.contains(&key.as_str()))
}

fn finite_number(value: &Value, minimum: f64, maximum: f64) -> bool {
    value
        .as_f64()
        .is_some_and(|number| number.is_finite() && minimum <= number && number <= maximum)
}

fn safe_integer(value: &Value, minimum: i64, maximum: i64) -> bool {
    value
        .as_i64()
        .is_some_and(|number| minimum <= number && number <= maximum)
        || value.as_u64().is_some_and(|number| {
            minimum >= 0 && number >= minimum as u64 && number <= maximum.max(0) as u64
        })
}

fn safe_unsigned_value(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .filter(|number| *number <= MAX_SAFE_JS_INTEGER)
}

fn safe_unsigned(value: &Value, minimum: u64) -> bool {
    safe_unsigned_value(value).is_some_and(|number| number >= minimum)
}

fn valid_percent_or_dimension(value: &Value) -> bool {
    finite_number(value, 0.0, 10_000.0) || value.as_str().is_some_and(valid_client_percentage)
}

fn valid_color(value: &Value) -> bool {
    value.as_str().is_some_and(|value| {
        (1..=40).contains(&value.len())
            && value.bytes().all(|byte| {
                b"#abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_().,% -"
                    .contains(&byte)
            })
    })
}

fn validate_box_props(props: &serde_json::Map<String, Value>) -> Result<(), String> {
    const ALLOWED: &[&str] = &[
        "key",
        "flexDirection",
        "flexGrow",
        "flexShrink",
        "flexWrap",
        "alignItems",
        "alignSelf",
        "justifyContent",
        "gap",
        "columnGap",
        "rowGap",
        "width",
        "height",
        "minWidth",
        "minHeight",
        "margin",
        "marginX",
        "marginY",
        "marginTop",
        "marginBottom",
        "marginLeft",
        "marginRight",
        "padding",
        "paddingX",
        "paddingY",
        "paddingTop",
        "paddingBottom",
        "paddingLeft",
        "paddingRight",
        "borderStyle",
        "borderColor",
        "borderDimColor",
        "backgroundColor",
        "overflow",
        "display",
        "position",
        "top",
        "left",
        "right",
        "bottom",
    ];
    if !keys_are_only(props, ALLOWED) {
        return Err("Box props contain an unsupported field".into());
    }
    if props
        .get("key")
        .is_some_and(|value| value.as_str().is_none_or(str::is_empty))
    {
        return Err("Box key must be a non-empty string".into());
    }
    let enums: &[(&str, &[&str])] = &[
        (
            "flexDirection",
            &["row", "column", "row-reverse", "column-reverse"],
        ),
        ("flexWrap", &["nowrap", "wrap", "wrap-reverse"]),
        (
            "alignItems",
            &["flex-start", "center", "flex-end", "stretch"],
        ),
        ("alignSelf", &["flex-start", "center", "flex-end", "auto"]),
        (
            "justifyContent",
            &[
                "flex-start",
                "center",
                "flex-end",
                "space-between",
                "space-around",
                "space-evenly",
            ],
        ),
        ("overflow", &["visible", "hidden"]),
        ("display", &["flex", "none"]),
        ("position", &["relative", "absolute"]),
        (
            "borderStyle",
            &[
                "single",
                "double",
                "round",
                "bold",
                "singleDouble",
                "doubleSingle",
                "classic",
                "arrow",
                "dashed",
                "quote",
            ],
        ),
    ];
    for (key, values) in enums {
        if props.get(*key).is_some_and(|value| {
            value
                .as_str()
                .is_none_or(|candidate| !values.contains(&candidate))
        }) {
            return Err(format!("Box {key} has an unsupported value"));
        }
    }
    for key in ["width", "height", "minWidth", "minHeight"] {
        if props
            .get(key)
            .is_some_and(|value| !valid_percent_or_dimension(value))
        {
            return Err(format!(
                "Box {key} must be a dimension from 0 to 10000 or a percentage"
            ));
        }
    }
    for key in [
        "flexGrow",
        "flexShrink",
        "gap",
        "columnGap",
        "rowGap",
        "margin",
        "marginX",
        "marginY",
        "marginTop",
        "marginBottom",
        "marginLeft",
        "marginRight",
        "padding",
        "paddingX",
        "paddingY",
        "paddingTop",
        "paddingBottom",
        "paddingLeft",
        "paddingRight",
    ] {
        if props
            .get(key)
            .is_some_and(|value| !finite_number(value, -10_000.0, 10_000.0))
        {
            return Err(format!(
                "Box {key} must be a finite number with magnitude at most 10000"
            ));
        }
    }
    for key in ["top", "left", "right", "bottom"] {
        if props
            .get(key)
            .is_some_and(|value| !safe_integer(value, -10_000, 10_000))
        {
            return Err(format!("Box {key} must be an integer from -10000 to 10000"));
        }
    }
    for key in ["borderColor", "borderDimColor", "backgroundColor"] {
        if props.get(key).is_some_and(|value| !valid_color(value)) {
            return Err(format!("Box {key} must be a color string"));
        }
    }
    Ok(())
}

fn validate_text_props(props: &serde_json::Map<String, Value>) -> Result<(), String> {
    const ALLOWED: &[&str] = &[
        "key",
        "color",
        "backgroundColor",
        "dimColor",
        "bold",
        "italic",
        "underline",
        "strikethrough",
        "inverse",
        "wrap",
    ];
    if !keys_are_only(props, ALLOWED) {
        return Err("Text props contain an unsupported field".into());
    }
    if props
        .get("key")
        .is_some_and(|value| value.as_str().is_none_or(str::is_empty))
    {
        return Err("Text key must be a non-empty string".into());
    }
    for key in ["color", "backgroundColor"] {
        if props.get(key).is_some_and(|value| !valid_color(value)) {
            return Err(format!("Text {key} must be a color string"));
        }
    }
    for key in [
        "dimColor",
        "bold",
        "italic",
        "underline",
        "strikethrough",
        "inverse",
    ] {
        if props.get(key).is_some_and(|value| !value.is_boolean()) {
            return Err(format!("Text {key} must be a boolean"));
        }
    }
    const WRAP: &[&str] = &[
        "wrap",
        "end",
        "middle",
        "truncate-end",
        "truncate",
        "truncate-middle",
        "truncate-start",
    ];
    if props
        .get("wrap")
        .is_some_and(|value| value.as_str().is_none_or(|value| !WRAP.contains(&value)))
    {
        return Err("Text wrap has an unsupported value".into());
    }
    Ok(())
}

fn validate_button_props(props: &serde_json::Map<String, Value>) -> Result<(), String> {
    const ALLOWED: &[&str] = &[
        "key",
        "label",
        "hotkey",
        "action",
        "plain",
        "dimColor",
        "variant",
        "role",
        "autoFocus",
    ];
    if !keys_are_only(props, ALLOWED)
        || props
            .get("key")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || props.get("label").and_then(Value::as_str).is_none()
        || props.get("hotkey").is_some_and(|value| {
            value.as_str().is_none_or(|value| {
                value.len() != 1
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            })
        })
        || props
            .get("action")
            .is_some_and(|value| value.as_str().is_none_or(str::is_empty))
        || props.get("plain").is_some_and(|value| value != true)
        || props
            .get("dimColor")
            .is_some_and(|value| !value.is_boolean())
        || props
            .get("variant")
            .is_some_and(|value| !matches!(value.as_str(), Some("primary" | "secondary")))
        || props
            .get("role")
            .is_some_and(|value| value.as_str() != Some("dismiss"))
        || props.get("autoFocus").is_some_and(|value| value != true)
    {
        return Err("Button props do not match the Desktop schema".into());
    }
    Ok(())
}

fn validate_input_props(props: &serde_json::Map<String, Value>) -> Result<(), String> {
    const ALLOWED: &[&str] = &[
        "key",
        "label",
        "placeholder",
        "value",
        "submitLabel",
        "autoFocus",
    ];
    if !keys_are_only(props, ALLOWED)
        || props
            .get("key")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || ["label", "placeholder", "value", "submitLabel"]
            .iter()
            .any(|key| {
                props
                    .get(*key)
                    .is_some_and(|value| value.as_str().is_none())
            })
        || props.get("autoFocus").is_some_and(|value| value != true)
    {
        return Err("Input props do not match the Desktop schema".into());
    }
    Ok(())
}

fn validate_select_props(props: &serde_json::Map<String, Value>) -> Result<(), String> {
    const ALLOWED: &[&str] = &["key", "label", "options", "value", "autoFocus"];
    let Some(options) = props.get("options").and_then(Value::as_array) else {
        return Err("Select needs an options array".into());
    };
    let mut seen = HashSet::new();
    if !keys_are_only(props, ALLOWED)
        || props
            .get("key")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || !(1..=64).contains(&options.len())
        || options.iter().any(|option| {
            let Some(option) = option.as_object() else {
                return true;
            };
            let Some(value) = option.get("value").and_then(Value::as_str) else {
                return true;
            };
            option.keys().any(|key| key != "value" && key != "label")
                || option
                    .get("label")
                    .is_some_and(|label| label.as_str().is_none())
                || !seen.insert(value.to_owned())
        })
        || ["label", "value"].iter().any(|key| {
            props
                .get(*key)
                .is_some_and(|value| value.as_str().is_none())
        })
        || props.get("autoFocus").is_some_and(|value| value != true)
    {
        return Err("Select props do not match the Desktop schema".into());
    }
    Ok(())
}

fn validate_link_props(props: &serde_json::Map<String, Value>) -> Result<(), String> {
    const ALLOWED: &[&str] = &["href", "label"];
    if !keys_are_only(props, ALLOWED)
        || props
            .get("href")
            .and_then(Value::as_str)
            .is_none_or(|href| href.trim().is_empty() || href.encode_utf16().count() > 2_048)
        || props.get("label").is_some_and(|value| {
            value.as_str().is_none_or(|label| {
                label.trim().is_empty() || label.encode_utf16().count() > 10_000
            })
        })
    {
        return Err("Link props do not match the Desktop schema".into());
    }
    Ok(())
}

fn validate_code_props(props: &serde_json::Map<String, Value>) -> Result<(), String> {
    const ALLOWED: &[&str] = &["source", "language", "path", "startLine", "format", "wrap"];
    if !keys_are_only(props, ALLOWED)
        || props.get("source").and_then(Value::as_str).is_none()
        || ["language", "path"].iter().any(|key| {
            props
                .get(*key)
                .is_some_and(|value| value.as_str().is_none())
        })
        || props
            .get("startLine")
            .is_some_and(|value| !safe_integer(value, 1, 1_000_000_000))
        || props
            .get("format")
            .is_some_and(|value| !matches!(value.as_str(), Some("source" | "diff")))
        || props
            .get("wrap")
            .is_some_and(|value| !matches!(value.as_str(), Some("wrap" | "truncate-end")))
    {
        return Err("Code props do not match the Desktop schema".into());
    }
    Ok(())
}

fn validate_markdown_props(props: &serde_json::Map<String, Value>) -> Result<(), String> {
    const ALLOWED: &[&str] = &["key", "text", "dimColor", "pressableLinks"];
    if !keys_are_only(props, ALLOWED)
        || props.get("text").and_then(Value::as_str).is_none()
        || props
            .get("key")
            .is_some_and(|value| value.as_str().is_none_or(str::is_empty))
        || props
            .get("dimColor")
            .is_some_and(|value| !value.is_boolean())
    {
        return Err("Markdown props do not match the Desktop schema".into());
    }
    if let Some(links) = props.get("pressableLinks") {
        let Some(links) = links.as_array() else {
            return Err("Markdown pressableLinks must be an array".into());
        };
        if links.len() > 256
            || links.iter().any(|link| {
                link.as_str().is_none_or(|href| {
                    href.trim().is_empty() || href.encode_utf16().count() > 2_048
                })
            })
        {
            return Err("Markdown pressableLinks contain an invalid link".into());
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct AbovePromptButton {
    pub(crate) plugin: String,
    pub(crate) element: String,
    pub(crate) handle: u64,
    pub(crate) worker_epoch: String,
    pub(crate) render_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AbovePromptPressRequest {
    pub(crate) surface: String,
    pub(crate) component: String,
    pub(crate) request_id: String,
    pub(crate) button: AbovePromptButton,
}

pub(crate) fn parse_above_prompt_press_request(
    input: &Value,
) -> Result<AbovePromptPressRequest, String> {
    let Some(object) = input.as_object() else {
        return Err("ui.press request must be an object".into());
    };
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "surface" | "component" | "requestId" | "plugin" | "element" | "press"
        )
    }) {
        return Err("ui.press request has unknown fields".into());
    }
    let string = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("ui.press request needs {key}"))
    };
    let surface = string("surface")?.to_owned();
    let component = string("component")?.to_owned();
    let request_id = string("requestId")?.to_owned();
    let plugin = string("plugin")?.to_owned();
    let element = string("element")?.to_owned();
    if surface != "terminal" || component != "AbovePrompt" {
        return Err("ui.press currently supports terminal AbovePrompt only".into());
    }
    let press = input
        .get("press")
        .and_then(Value::as_object)
        .ok_or_else(|| "ui.press request needs a private press token".to_owned())?;
    if press
        .keys()
        .any(|key| !matches!(key.as_str(), "handle" | "workerEpoch" | "renderRevision"))
    {
        return Err("ui.press token has unknown fields".into());
    }
    let handle = press
        .get("handle")
        .and_then(Value::as_u64)
        .filter(|handle| *handle > 0)
        .ok_or_else(|| "ui.press token needs a positive handle".to_owned())?;
    let worker_epoch = press
        .get("workerEpoch")
        .and_then(Value::as_str)
        .filter(|epoch| !epoch.is_empty())
        .ok_or_else(|| "ui.press token needs a worker epoch".to_owned())?;
    let render_revision = press
        .get("renderRevision")
        .and_then(Value::as_u64)
        .filter(|revision| *revision > 0)
        .ok_or_else(|| "ui.press token needs a positive render revision".to_owned())?;
    Ok(AbovePromptPressRequest {
        surface,
        component,
        request_id,
        button: AbovePromptButton {
            plugin,
            element,
            handle,
            worker_epoch: worker_epoch.to_owned(),
            render_revision,
        },
    })
}

pub(crate) fn validate_ui_press_event(input: &Value) -> Result<(), String> {
    let Some(object) = input.as_object() else {
        return Err("ui.press event must be an object".into());
    };
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "plugin" | "element" | "component" | "requestId" | "surface" | "link"
        )
    }) {
        return Err("ui.press event has unknown fields".into());
    }
    for key in ["plugin", "element", "component", "requestId"] {
        if input
            .get(key)
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(format!("ui.press event needs a non-empty {key}"));
        }
    }
    if !matches!(
        input.get("surface").and_then(Value::as_str),
        Some("terminal" | "desktop")
    ) {
        return Err("ui.press event needs a supported surface".into());
    }
    if let Some(link) = input.get("link") {
        let Some(link) = link.as_object() else {
            return Err("ui.press link must contain an href".into());
        };
        if link.get("href").and_then(Value::as_str).is_none() {
            return Err("ui.press link must contain a string href".into());
        }
    }
    Ok(())
}

pub(crate) fn validate_above_prompt_event(input: &Value) -> Result<(), String> {
    if input.get("surface").and_then(Value::as_str) != Some("terminal")
        || input.get("component").and_then(Value::as_str) != Some("AbovePrompt")
    {
        return Err("ui.render currently supports terminal AbovePrompt only".into());
    }
    if input
        .get("requestId")
        .and_then(Value::as_str)
        .is_none_or(|request_id| request_id.is_empty())
    {
        return Err("ui.render AbovePrompt needs a non-empty requestId".into());
    }
    let Some(props) = input.get("props").and_then(Value::as_object) else {
        return Err("ui.render AbovePrompt props must be an object".into());
    };
    if props.keys().any(|key| {
        !matches!(
            key.as_str(),
            "hasSurvey" | "isWorking" | "maxRows" | "bodyColumns" | "scroll" | "view"
        )
    }) || !props.get("hasSurvey").is_some_and(Value::is_boolean)
        || !props.get("isWorking").is_some_and(Value::is_boolean)
        || props
            .get("maxRows")
            .is_none_or(|value| value.as_u64().is_none())
        || props
            .get("bodyColumns")
            .is_none_or(|value| value.as_u64().is_none_or(|columns| columns == 0))
    {
        return Err(
            "ui.render AbovePrompt props need hasSurvey, isWorking, maxRows, and bodyColumns"
                .into(),
        );
    }
    let Some(scroll) = props.get("scroll").and_then(Value::as_object) else {
        return Err("ui.render AbovePrompt scroll must contain offset and bodyRows".into());
    };
    if scroll
        .keys()
        .any(|key| key != "offset" && key != "bodyRows")
        || scroll
            .get("offset")
            .is_none_or(|value| value.as_u64().is_none())
        || scroll
            .get("bodyRows")
            .is_none_or(|value| value.as_u64().is_none())
    {
        return Err(
            "ui.render AbovePrompt scroll must contain non-negative offset and bodyRows".into(),
        );
    }
    let Some(view) = props.get("view").and_then(Value::as_object) else {
        return Err("ui.render AbovePrompt view must be an object".into());
    };
    if view.keys().any(|key| key != "agentId")
        || view
            .get("agentId")
            .is_some_and(|value| value.as_str().is_none_or(|text| text.is_empty()))
    {
        return Err("ui.render AbovePrompt view may contain only a non-empty agentId".into());
    }
    if let Some(viewport) = input.get("viewport") {
        if !viewport.is_object()
            || viewport.as_object().is_some_and(|viewport| {
                viewport
                    .keys()
                    .any(|key| !matches!(key.as_str(), "columns" | "rows" | "isFullscreen"))
                    || viewport
                        .get("columns")
                        .is_none_or(|value| value.as_u64().is_none_or(|value| value == 0))
                    || viewport
                        .get("rows")
                        .is_none_or(|value| value.as_u64().is_none_or(|value| value == 0))
                    || viewport
                        .get("isFullscreen")
                        .is_none_or(|value| !value.is_boolean())
            })
        {
            return Err(
                "ui.render viewport must contain positive dimensions and a boolean fullscreen flag"
                    .into(),
            );
        }
    }
    Ok(())
}

/// Convert the supported tree to terminal rows. Box row layout joins children
/// with `columnGap`; column layout stacks them. No other layout/style props or
/// interactive elements are accepted by this first terminal slice.
pub(crate) fn above_prompt_lines(tree: &Value) -> Result<Vec<String>, String> {
    match tree.get("type").and_then(Value::as_str) {
        Some("Text") => {
            let children = tree
                .get("children")
                .and_then(Value::as_array)
                .ok_or_else(|| "Text needs a children array".to_owned())?;
            let props = tree
                .get("props")
                .and_then(Value::as_object)
                .ok_or_else(|| "Text props must be an object".to_owned())?;
            if !props.is_empty() {
                return Err("Text props are outside the terminal AbovePrompt subset".into());
            }
            let mut text = String::new();
            for child in children {
                text.push_str(
                    child
                        .as_str()
                        .ok_or_else(|| "Text children must be strings".to_owned())?,
                );
            }
            Ok(vec![text])
        }
        Some("Button") => {
            let props = tree
                .get("props")
                .and_then(Value::as_object)
                .ok_or_else(|| "Button props must be an object".to_owned())?;
            let label = props
                .get("label")
                .and_then(Value::as_str)
                .ok_or_else(|| "Button needs a string label".to_owned())?;
            if props.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "key"
                        | "label"
                        | "hotkey"
                        | "action"
                        | "plain"
                        | "dimColor"
                        | "variant"
                        | "role"
                        | "autoFocus"
                )
            }) || props
                .get("key")
                .and_then(Value::as_str)
                .is_none_or(|key| key.is_empty())
                || props.get("hotkey").is_some_and(|value| {
                    value.as_str().is_none_or(|hotkey| {
                        hotkey.len() != 1
                            || !hotkey
                                .bytes()
                                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                    })
                })
                || props
                    .get("action")
                    .is_some_and(|value| value.as_str().is_none_or(str::is_empty))
                || props.get("plain").is_some_and(|value| value != true)
                || props
                    .get("dimColor")
                    .is_some_and(|value| !value.is_boolean())
                || props
                    .get("variant")
                    .is_some_and(|value| !matches!(value.as_str(), Some("primary" | "secondary")))
                || props
                    .get("role")
                    .is_some_and(|value| value.as_str() != Some("dismiss"))
                || props.get("autoFocus").is_some_and(|value| value != true)
                || tree
                    .get("children")
                    .and_then(Value::as_array)
                    .is_none_or(|children| !children.is_empty())
            {
                return Err(
                    "Button props or children are outside the terminal AbovePrompt subset".into(),
                );
            }
            validate_above_prompt_button(tree)?;
            Ok(vec![format!("[{label}]")])
        }
        Some("Box") => {
            let children = tree
                .get("children")
                .and_then(Value::as_array)
                .ok_or_else(|| "Box needs a children array".to_owned())?;
            let props = tree
                .get("props")
                .and_then(Value::as_object)
                .ok_or_else(|| "Box props must be an object".to_owned())?;
            if props
                .keys()
                .any(|key| key != "flexDirection" && key != "columnGap")
                || props
                    .get("flexDirection")
                    .is_some_and(|value| !matches!(value.as_str(), Some("row" | "column")))
                || props
                    .get("columnGap")
                    .is_some_and(|value| value.as_u64().is_none_or(|gap| gap > 16))
            {
                return Err("Box props are outside the terminal AbovePrompt subset".into());
            }
            let row = props.get("flexDirection").and_then(Value::as_str) == Some("row");
            let gap = props
                .get("columnGap")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .min(16) as usize;
            let mut child_lines = Vec::with_capacity(children.len());
            for child in children {
                child_lines.push(if let Some(text) = child.as_str() {
                    vec![text.to_owned()]
                } else {
                    above_prompt_lines(child)?
                });
            }
            if row {
                let rows = child_lines.iter().map(Vec::len).max().unwrap_or(0);
                let separator = " ".repeat(gap);
                let mut lines = Vec::with_capacity(rows);
                for row_index in 0..rows {
                    lines.push(
                        child_lines
                            .iter()
                            .map(|lines| lines.get(row_index).map(String::as_str).unwrap_or(""))
                            .collect::<Vec<_>>()
                            .join(&separator),
                    );
                }
                Ok(lines)
            } else {
                Ok(child_lines.into_iter().flatten().collect())
            }
        }
        Some(other) => Err(format!("unsupported AbovePrompt element {other}")),
        None => Err("AbovePrompt tree node needs a type".into()),
    }
}

pub(crate) fn above_prompt_buttons(tree: &Value) -> Result<Vec<AbovePromptButton>, String> {
    let mut buttons = Vec::new();
    collect_above_prompt_buttons(tree, &mut buttons)?;
    Ok(buttons)
}

fn collect_above_prompt_buttons(
    tree: &Value,
    buttons: &mut Vec<AbovePromptButton>,
) -> Result<(), String> {
    match tree.get("type").and_then(Value::as_str) {
        Some("Button") => buttons.push(validate_above_prompt_button(tree)?),
        Some("Box") => {
            let children = tree
                .get("children")
                .and_then(Value::as_array)
                .ok_or_else(|| "Box needs a children array".to_owned())?;
            for child in children {
                if !child.is_string() {
                    collect_above_prompt_buttons(child, buttons)?;
                }
            }
        }
        Some("Text") => {}
        Some(other) => return Err(format!("unsupported AbovePrompt element {other}")),
        None => return Err("AbovePrompt tree node needs a type".into()),
    }
    Ok(())
}

fn validate_above_prompt_button(tree: &Value) -> Result<AbovePromptButton, String> {
    let press = tree
        .get("press")
        .and_then(Value::as_object)
        .ok_or_else(|| "Button needs a private press identity".to_owned())?;
    if press.keys().any(|key| {
        !matches!(
            key.as_str(),
            "plugin" | "handle" | "workerEpoch" | "renderRevision"
        )
    }) {
        return Err("Button press identity has unknown fields".into());
    }
    let plugin = press
        .get("plugin")
        .and_then(Value::as_str)
        .filter(|plugin| !plugin.is_empty())
        .ok_or_else(|| "Button press identity needs a plugin".to_owned())?;
    let handle = press
        .get("handle")
        .and_then(Value::as_u64)
        .filter(|handle| *handle > 0)
        .ok_or_else(|| "Button press identity needs a positive numeric handle".to_owned())?;
    let worker_epoch = press
        .get("workerEpoch")
        .and_then(Value::as_str)
        .filter(|epoch| !epoch.is_empty())
        .ok_or_else(|| "Button press identity needs a worker epoch".to_owned())?;
    let render_revision = press
        .get("renderRevision")
        .and_then(Value::as_u64)
        .filter(|revision| *revision > 0)
        .ok_or_else(|| "Button press identity needs a positive render revision".to_owned())?;
    let element = tree
        .get("props")
        .and_then(|props| props.get("key"))
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| "Button needs a non-empty props.key".to_owned())?;
    Ok(AbovePromptButton {
        plugin: plugin.to_owned(),
        element: element.to_owned(),
        handle,
        worker_epoch: worker_epoch.to_owned(),
        render_revision,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_optional_viewport_and_flattens_supported_box_text_layout() {
        validate_above_prompt_event(&json!({
            "surface":"terminal",
            "component":"AbovePrompt",
            "requestId":"above-prompt-main",
            "props":{"hasSurvey":false,"isWorking":false,"maxRows":12,"bodyColumns":80,
                "scroll":{"offset":0,"bodyRows":11},"view":{}}
        }))
        .unwrap();
        let lines = above_prompt_lines(&json!({
            "type":"Box",
            "props":{"flexDirection":"row","columnGap":2},
            "children":[
                {"type":"Text","props":{},"children":["Ready"]},
                {"type":"Text","props":{},"children":["2 items"]}
            ]
        }))
        .unwrap();
        assert_eq!(lines, vec!["Ready  2 items"]);
    }

    #[test]
    fn client_parent_props_match_native_dimensions_and_leaf_rules() {
        for width in [json!(0), json!(10_000), json!("80%"), json!("999%")] {
            validate_client_parent_element(&json!({
                "type":"Client",
                "props":{"key":"board","module":"./board.tsx","width":width},
                "client":{"plugin":"plugin-a"}
            }))
            .unwrap();
        }
        for width in [json!(-1), json!(10_001), json!("1000%"), json!(" 80%")] {
            assert!(validate_client_parent_element(&json!({
                "type":"Client",
                "props":{"key":"board","module":"./board.tsx","width":width},
                "client":{"plugin":"plugin-a"}
            }))
            .is_err());
        }
        assert!(validate_client_parent_element(&json!({
            "type":"Client",
            "props":{"key":"board","module":"./board.tsx"},
            "children":[],
            "client":{"plugin":"plugin-a"}
        }))
        .unwrap_err()
        .contains("takes no children"));
        assert!(validate_client_parent_element(&json!({
            "type":"Client",
            "props":{"key":"board","module":"./board.tsx","onPress":true},
            "client":{"plugin":"plugin-a"}
        }))
        .is_err());
    }

    #[test]
    fn validates_desktop_render_event_and_optional_control_metadata() {
        normalize_ui_render_event(&json!({
            "surface":"desktop",
            "component":"AssistantMessage",
            "requestId":"render-1",
            "clientId":"client_1",
            "props":{"text":"hello"},
            "viewport":{"columns":120,"rows":40,"isFullscreen":false},
            "onScreen":{"first":0,"last":2,"of":3},
            "contentRows":8,
            "keyed":[{"plugin":"plugin-a","key":"board","top":1,"bottom":2}],
            "bench":{"seq":7,"t0":1.25}
        }))
        .unwrap();
        for invalid in [
            json!({"surface":"desktop","component":"NotASite","requestId":"r","props":{}}),
            json!({"surface":"desktop","component":"Pane","requestId":"r","props":{},"clientId":"bad id"}),
            json!({"surface":"desktop","component":"Pane","requestId":"r","props":{},"viewport":{"columns":1,"rows":0}}),
            json!({"surface":"desktop","component":"Pane","requestId":"r","props":{},"onScreen":{"first":3,"last":2,"of":4}}),
        ] {
            assert!(normalize_ui_render_event(&invalid).is_err(), "{invalid}");
        }

        let normalized = normalize_ui_render_event(&json!({
            "surface":"desktop",
            "component":"Pane",
            "requestId":"render-2",
            "props":{"pluginField":"preserved"},
            "extraEnvelopeField":true,
            "viewport":{"columns":80,"rows":24,"isFullscreen":false,"extraViewportField":true},
            "onScreen":{"first":0,"last":1,"of":2,"extraOnScreenField":true},
            "keyed":[{"plugin":"plugin-a","key":"card","top":9,"bottom":1,"extraRowField":true}],
            "bench":{"seq":1,"t0":2.5,"extraBenchField":true}
        }))
        .unwrap();
        assert!(normalized.get("extraEnvelopeField").is_none());
        assert_eq!(normalized["props"]["pluginField"], "preserved");
        assert!(normalized["viewport"].get("extraViewportField").is_none());
        assert!(normalized["onScreen"].get("extraOnScreenField").is_none());
        assert!(normalized["keyed"][0].get("extraRowField").is_none());
        assert!(normalized["bench"].get("extraBenchField").is_none());
    }

    #[test]
    fn validates_desktop_tree_styles_press_stamps_and_client_leaf() {
        validate_desktop_parent_tree(&json!({
            "type":"Box",
            "props":{"flexDirection":"row","gap":2,"width":"80%","key":"row"},
            "children":[
                {"type":"Text","props":{"bold":true,"color":"#fff","wrap":"truncate-end"},"children":["Ready"]},
                {"type":"Button","props":{"key":"go","label":"Go","variant":"primary"},"press":{"plugin":"plugin-a","handle":1}},
                {"type":"Markdown","props":{"key":"links","text":"go","pressableLinks":["https://example.com"]},"press":{"plugin":"plugin-a","handle":2}},
                {"type":"Client","props":{"key":"board","module":"./board.tsx","props":{"title":"hello"}},"client":{"plugin":"plugin-a"}}
            ]
        }))
        .unwrap();

        for invalid in [
            json!({"type":"Box","props":{"flexDirection":"diagonal"},"children":[]}),
            json!({"type":"Text","props":{},"children":[{"type":"Client","props":{"key":"x","module":"./x.tsx"},"client":{"plugin":"p"}}]}),
            json!({"type":"Button","props":{"key":"go","label":"Go"},"press":{"plugin":"p","handle":0}}),
            json!({"type":"Select","props":{"key":"pick","options":[{"value":"a"},{"value":"a"}]},"press":{"plugin":"p","handle":1}}),
            json!({"type":"Markdown","props":{"text":"go","pressableLinks":["https://example.com"]},"press":{"plugin":"p","handle":1}}),
            json!({"type":"Markdown","props":{"key":"links","text":"go","pressableLinks":["https://example.com"]}}),
            json!({"type":"Client","props":{"key":"x","module":"./x.tsx"},"children":[],"client":{"plugin":"p"}}),
        ] {
            assert!(validate_desktop_parent_tree(&invalid).is_err(), "{invalid}");
        }
        validate_desktop_parent_tree(&json!({
            "type":"Svg",
            "props":{"source":"<svg/>","alt":"diagram","width":100,"isInteractive":false}
        }))
        .unwrap();
        validate_desktop_parent_tree(&json!({"type":"engine","ref":42})).unwrap();
    }

    #[test]
    fn desktop_hover_uses_keyed_box_scope_and_typed_owner_stamps() {
        validate_desktop_parent_tree(&json!({
            "type":"Box",
            "props":{"key":"group","display":"none","borderStyle":"single","position":"absolute"},
            "group":{"plugin":"owner-plugin"},
            "hover":{"scope":"hover-scope","display":"flex","borderStyle":"double","borderDimColor":false,"top":2},
            "children":[
                {"type":"Text","props":{"bold":false},"hover":{"bold":true}},
                {"type":"Text","props":{},"group":{"plugin":"owner-plugin"},"hover":{"scope":"separate","color":"green"}},
                {"type":"Button","props":{"key":"go","label":"Go"},"press":{"plugin":"owner-plugin","handle":2},"hover":{"scope":"action","color":"blue"}}
            ]
        }))
        .unwrap();
        for invalid in [
            json!({"type":"Box","props":{"key":"hidden","display":"none"},"children":[{"type":"Text","props":{},"hover":{"bold":true}}]}),
            json!({"type":"Text","props":{},"hover":{"bold":true}}),
            json!({"type":"Text","props":{},"hover":{"scope":"named","bold":true}}),
            json!({"type":"Box","props":{"key":"scope"},"hover":{"scope":"x","unknown":true}}),
            json!({"type":"Box","props":{"key":"scope"},"group":{"plugin":"p".repeat(257)},"hover":{"scope":"x"}}),
            json!({"type":"Box","props":{"key":"scope"},"hover":{"borderDimColor":"true"}}),
            json!({"type":"div","props":{},"hover":{"scope":"x"}}),
        ] {
            assert!(validate_desktop_parent_tree(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn ui_press_accepts_the_desktop_markdown_link_payload() {
        validate_ui_press_event(&json!({
            "plugin":"plugin-a",
            "element":"links",
            "component":"Pane",
            "requestId":"request-1",
            "surface":"desktop",
            "link":{"href":"https://example.com/path"}
        }))
        .unwrap();
        validate_ui_press_event(&json!({
            "plugin":"plugin-a",
            "element":"links",
            "component":"Pane",
            "requestId":"request-1",
            "surface":"desktop",
            "link":{"href":""}
        }))
        .unwrap();
        validate_ui_press_event(&json!({
            "plugin":"plugin-a",
            "element":"links",
            "component":"Pane",
            "requestId":"request-1",
            "surface":"desktop",
            "link":{"href":"h".repeat(2_049)}
        }))
        .unwrap();
        for invalid in [
            json!({"plugin":"plugin-a","element":"links","component":"Pane","requestId":"request-1","surface":"desktop","link":{"href":9}}),
            json!({"plugin":"plugin-a","element":"links","component":"Pane","requestId":"request-1","surface":"desktop","link":"https://example.com"}),
        ] {
            assert!(validate_ui_press_event(&invalid).is_err(), "{invalid}");
        }
    }
}
