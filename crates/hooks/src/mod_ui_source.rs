//! Source-preserving validation and normalization for worker supplied UI JSON.
//!
//! These routines read the flat Core projection directly so exact UTF-16
//! strings and property names stay intact across hook and worker boundaries.

use super::{validate_above_prompt_event, DESKTOP_RENDER_COMPONENTS};
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use std::collections::HashSet;

const MAX_SAFE_JS_INTEGER: f64 = 9_007_199_254_740_991.0;

pub(crate) fn validate_client_parent_element(
    projection: &Utf16JsonProjection,
) -> Result<(), String> {
    let node = projection
        .at_pointer("")
        .ok_or_else(|| "Client node is missing".to_owned())?;
    if projection.at_pointer("/props/props").is_some() {
        let data = projection
            .subprojection("/props/props")
            .map_err(|error| error.to_string())?;
        let encoded = data.to_json_string().map_err(|error| error.to_string())?;
        if encoded.encode_utf16().count() > 100_000 {
            return Err("Client props exceeds 100000 serialized characters".into());
        }
    }
    validate_client_parent_element_view(node)
}

fn validate_client_parent_element_view(node: JsonValueView<'_>) -> Result<(), String> {
    let Some(props) = node.get_property("props") else {
        return Err("Client props must be an object".into());
    };
    if props.object_entries().is_none() {
        return Err("Client props must be an object".into());
    }
    if !keys_are_only(
        props,
        &["key", "module", "props", "width", "height", "flexGrow"],
    ) {
        return Err("Client props contain an unsupported field".into());
    }
    for field in ["key", "module"] {
        if props
            .get_property(field)
            .and_then(JsonValueView::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(format!("Client props need non-empty {field}"));
        }
    }
    if let Some(value) = props.get_property("props") {
        validate_plain_view(value, "Client props", 32, 20_000)?;
    }
    for field in ["width", "height"] {
        if let Some(value) = props.get_property(field) {
            if !valid_percent_or_dimension(value) {
                return Err(format!(
                    "Client {field} must be a number from 0 to 10000 or a percentage"
                ));
            }
        }
    }
    if let Some(value) = props.get_property("flexGrow") {
        if !finite_number(value, -10_000.0, 10_000.0) {
            return Err("Client flexGrow must be a number with magnitude at most 10000".into());
        }
    }
    if node.get_property("children").is_some() {
        let key = props
            .get_property("key")
            .and_then(JsonValueView::as_str)
            .unwrap_or("");
        return Err(format!("Client \"{key}\" takes no children"));
    }
    Ok(())
}

/// Normalize the Desktop `ui_render` envelope while preserving JSON order,
/// exact string units, and arbitrary `props` subtrees.
pub(crate) fn normalize_ui_render_event(
    input: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, String> {
    let surface = input
        .string_at("/surface")
        .ok_or_else(|| "ui.render needs a surface".to_owned())?;
    if surface == "terminal" {
        validate_above_prompt_event(input)?;
        return Ok(input.clone());
    }

    let mut normalized = retain_object_fields(
        input,
        &[
            "surface",
            "component",
            "requestId",
            "clientId",
            "props",
            "viewport",
            "onScreen",
            "contentRows",
            "keyed",
            "bench",
        ],
    )?;
    for (field, allowed) in [
        ("viewport", &["columns", "rows", "isFullscreen"][..]),
        ("onScreen", &["first", "last", "of"][..]),
        ("bench", &["seq", "t0"][..]),
    ] {
        if let Some(value) = input.get_property(field) {
            if value.is_null() {
                continue;
            }
            let child = input
                .subprojection(&format!("/{field}"))
                .map_err(|error| error.to_string())?;
            let child = if child.object_entries().is_some() {
                retain_object_fields(&child, allowed)?
            } else {
                child
            };
            normalized
                .set_field(field, child)
                .map_err(|error| error.to_string())?;
        }
    }
    if let Some(keyed) = input.get_property("keyed") {
        if let Some(length) = keyed.array_len() {
            let mut rows = Vec::with_capacity(length);
            for index in 0..length {
                let row = input
                    .subprojection(&format!("/keyed/{index}"))
                    .map_err(|error| error.to_string())?;
                rows.push(if row.object_entries().is_some() {
                    retain_object_fields(&row, &["plugin", "key", "top", "bottom"])?
                } else {
                    row
                });
            }
            normalized
                .set_field(
                    "keyed",
                    Utf16JsonProjection::array(rows).map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?;
        }
    }
    validate_ui_render_event_shape(&normalized)?;
    Ok(normalized)
}

fn retain_object_fields(
    value: &Utf16JsonProjection,
    allowed: &[&str],
) -> Result<Utf16JsonProjection, String> {
    let entries = value
        .object_entries()
        .ok_or_else(|| "Expected object while normalizing UI event".to_owned())?;
    let allowed_units = allowed
        .iter()
        .map(|field| field.encode_utf16().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let mut fields = Vec::new();
    for entry in entries {
        if allowed_units.iter().any(|key| *key == entry.code_units) {
            fields.push((
                entry.code_units.to_vec(),
                value
                    .subprojection_property_units(entry.code_units)
                    .map_err(|error| error.to_string())?,
            ));
        }
    }
    Utf16JsonProjection::object_units(fields).map_err(|error| error.to_string())
}

fn validate_ui_render_event_shape(input: &Utf16JsonProjection) -> Result<(), String> {
    let surface = input.string_at("/surface").unwrap_or_default();
    let component = input.string_at("/component").unwrap_or_default();
    if !matches!(surface, "desktop" | "mobile" | "vscode")
        || !DESKTOP_RENDER_COMPONENTS.contains(&component)
    {
        return Err("ui.render needs a supported surface and component".into());
    }
    if input.string_at("/requestId").is_none_or(str::is_empty) {
        return Err("ui.render needs a non-empty requestId".into());
    }
    let props = input
        .at_pointer("/props")
        .filter(|props| props.object_entries().is_some())
        .ok_or_else(|| "ui.render props must be an object".to_owned())?;
    validate_plain_view(props, "ui.render props", 32, 20_000)?;
    let props_json = input
        .subprojection("/props")
        .and_then(|props| props.to_json_string())
        .map_err(|error| error.to_string())?;
    if props_json.encode_utf16().count() > 100_000 {
        return Err("ui.render props exceeds 100000 serialized characters".into());
    }

    if let Some(client_id) = input.at_pointer("/clientId") {
        let Some(units) = client_id.code_units() else {
            return Err("ui.render clientId must be 1 to 64 safe ASCII characters".into());
        };
        if !(1..=64).contains(&units.len())
            || !units.iter().all(|unit| {
                *unit <= 0x7f
                    && ((*unit as u8).is_ascii_alphanumeric()
                        || matches!(*unit as u8, b'.' | b'_' | b'-'))
            })
        {
            return Err("ui.render clientId must be 1 to 64 safe ASCII characters".into());
        }
    }
    if let Some(viewport) = input.at_pointer("/viewport") {
        if !keys_are_only(viewport, &["columns", "rows", "isFullscreen"])
            || !viewport
                .get_property("columns")
                .is_some_and(|value| safe_unsigned(value, 1))
            || !viewport
                .get_property("rows")
                .is_some_and(|value| safe_unsigned(value, 1))
            || viewport
                .get_property("isFullscreen")
                .is_some_and(|value| value.as_bool().is_none())
        {
            return Err("ui.render viewport needs positive columns and rows".into());
        }
    }
    if let Some(on_screen) = input.at_pointer("/onScreen") {
        if !on_screen.is_null() {
            if !keys_are_only(on_screen, &["first", "last", "of"]) {
                return Err("ui.render onScreen must be null or { first, last, of }".into());
            }
            let first = on_screen
                .get_property("first")
                .and_then(safe_unsigned_value);
            let last = on_screen.get_property("last").and_then(safe_unsigned_value);
            let of = on_screen.get_property("of").and_then(safe_unsigned_value);
            if first
                .zip(last)
                .zip(of)
                .is_none_or(|((first, last), of)| first > last || last >= of)
            {
                return Err("ui.render onScreen must satisfy first <= last < of".into());
            }
        }
    }
    if input
        .at_pointer("/contentRows")
        .is_some_and(|value| !safe_unsigned(value, 0))
    {
        return Err("ui.render contentRows must be a non-negative integer".into());
    }
    if let Some(keyed) = input.at_pointer("/keyed") {
        let Some(length) = keyed.array_len().filter(|length| *length <= 512) else {
            return Err("ui.render keyed is limited to 512 rows".into());
        };
        for index in 0..length {
            let row = keyed.get_index(index).expect("index is in range");
            if row.object_entries().is_none() {
                return Err("ui.render keyed rows must be objects".into());
            }
            if !keys_are_only(row, &["plugin", "key", "top", "bottom"])
                || row
                    .get_property("plugin")
                    .and_then(JsonValueView::as_str)
                    .is_none()
                || row
                    .get_property("key")
                    .and_then(JsonValueView::as_str)
                    .is_none()
                || !row
                    .get_property("top")
                    .is_some_and(|value| safe_unsigned(value, 0))
                || !row
                    .get_property("bottom")
                    .is_some_and(|value| safe_unsigned(value, 0))
            {
                return Err("ui.render keyed rows need plugin, key, top and bottom".into());
            }
            let top = row
                .get_property("top")
                .and_then(safe_unsigned_value)
                .unwrap();
            let bottom = row
                .get_property("bottom")
                .and_then(safe_unsigned_value)
                .unwrap();
            if bottom < top {
                return Err("ui.render keyed row range is invalid".into());
            }
        }
    }
    if let Some(bench) = input.at_pointer("/bench") {
        if !keys_are_only(bench, &["seq", "t0"])
            || !bench.get_property("seq").is_some_and(safe_integer)
            || !bench
                .get_property("t0")
                .and_then(JsonValueView::as_number)
                .is_some_and(f64::is_finite)
        {
            return Err("ui.render bench must contain integer seq and numeric t0".into());
        }
    }
    Ok(())
}

fn validate_plain_view(
    value: JsonValueView<'_>,
    label: &str,
    max_depth: usize,
    max_values: usize,
) -> Result<(), String> {
    let mut values = 0usize;
    let mut stack = vec![(value, 0usize)];
    while let Some((value, depth)) = stack.pop() {
        if depth > max_depth {
            return Err(format!("{label} exceeds nesting depth {max_depth}"));
        }
        values = values.saturating_add(1);
        if values > max_values {
            return Err(format!("{label} exceeds {max_values} values"));
        }
        if let Some(length) = value.array_len() {
            for index in (0..length).rev() {
                stack.push((
                    value.get_index(index).expect("index is in range"),
                    depth + 1,
                ));
            }
        } else if let Some(entries) = value.object_entries() {
            for entry in entries.into_iter().rev() {
                stack.push((entry.value, depth + 1));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_desktop_parent_tree(tree: &Utf16JsonProjection) -> Result<(), String> {
    let serialized = tree.to_json_string().map_err(|error| error.to_string())?;
    if serialized.encode_utf16().count() > 100_000 {
        return Err("Desktop UI tree exceeds 100000 serialized characters".into());
    }
    let mut budget = 0usize;
    validate_desktop_node(
        tree.at_pointer("")
            .ok_or_else(|| "Desktop UI tree is empty".to_owned())?,
        0,
        &mut budget,
        false,
    )
}

fn validate_desktop_node(
    node: JsonValueView<'_>,
    depth: usize,
    budget: &mut usize,
    inherited_hover_scope: bool,
) -> Result<(), String> {
    if depth > 32 {
        return Err("Desktop UI tree exceeds nesting depth 32".into());
    }
    if let Some(text) = node.code_units() {
        if text.len() > 10_000 {
            return Err("Desktop UI text exceeds 10000 characters".into());
        }
        return Ok(());
    }
    *budget += 1;
    if *budget > 20_000 {
        return Err("Desktop UI tree exceeds 20000 elements".into());
    }
    let Some(object) = node.object_entries() else {
        return Err("Desktop UI tree children must be text or element objects".into());
    };
    let kind = node
        .get_property("type")
        .and_then(JsonValueView::as_str)
        .ok_or_else(|| "Desktop UI node needs a type".to_owned())?;
    if kind == "engine" {
        if !keys_are_only(node, &["type", "ref"])
            || !node.get_property("ref").is_some_and(|value| {
                safe_integer_range(
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
    let default_props = matches!(kind, "Box" | "Text" | "div" | "span" | "b");
    let props = match node.get_property("props") {
        Some(props) if props.object_entries().is_some() => props,
        None if default_props => node,
        _ => return Err(format!("{kind} props must be an object")),
    };
    let props = if default_props && node.get_property("props").is_none() {
        None
    } else {
        Some(props)
    };
    let has_keyed_scope = inherited_hover_scope
        || (kind == "Box"
            && props
                .and_then(|props| props.get_property("key"))
                .and_then(JsonValueView::as_str)
                .is_some_and(|key| !key.is_empty()));
    if let Some(props) = props {
        validate_plain_view(props, "Desktop UI props", 32, 20_000)?;
    }
    if let Some(hover) = node.get_property("hover") {
        if !matches!(kind, "Box" | "Text" | "Button") {
            return Err(format!("{kind} cannot carry hover metadata"));
        }
        validate_parent_hover(kind, hover, props, node, has_keyed_scope)?;
    }
    if kind == "Box"
        && props
            .and_then(|props| props.get_property("key"))
            .and_then(JsonValueView::as_str)
            .is_some()
        && props
            .and_then(|props| props.get_property("display"))
            .and_then(JsonValueView::as_str)
            == Some("none")
        && subtree_has_hover(node)
        && node
            .get_property("hover")
            .and_then(|hover| hover.get_property("display"))
            .and_then(JsonValueView::as_str)
            != Some("flex")
    {
        return Err("a hidden keyed Box needs hover display flex to reveal its scope".into());
    }

    let mut allowed = vec!["type", "props"];
    match kind {
        "Box" | "Text" | "div" | "span" | "b" => {
            allowed.extend(["children", "hover", "group"]);
            if let Some(group) = node.get_property("group") {
                validate_parent_group(group)?;
            }
            if let Some(children) = node.get_property("children") {
                let Some(length) = children.array_len() else {
                    return Err(format!("{kind} children must be an array"));
                };
                for index in 0..length {
                    let child = children.get_index(index).expect("index is in range");
                    if kind == "Text" && child.code_units().is_none() {
                        return Err("Text children must be strings".into());
                    }
                    if child.code_units().is_none() {
                        validate_desktop_node(child, depth + 1, budget, has_keyed_scope)?;
                    } else if child.code_units().is_some_and(|text| text.len() > 10_000) {
                        return Err(format!("{kind} child text exceeds 10000 characters"));
                    }
                }
            }
            if let Some(props) = props {
                match kind {
                    "Box" => validate_box_props_source(props)?,
                    "Text" => validate_text_props_source(props)?,
                    "div" | "span" | "b" => {
                        if props.object_entries().is_none_or(|entries| {
                            entries.into_iter().any(|entry| {
                                entry.value.code_units().is_none()
                                    && entry.value.as_bool().is_none()
                                    && entry.value.as_number().is_none()
                            })
                        }) {
                            return Err(format!(
                                "{kind} props must contain only strings, numbers, and booleans"
                            ));
                        }
                    }
                    _ => unreachable!(),
                }
            }
        }
        "Button" => {
            allowed.extend(["press", "hover"]);
            validate_button_props_source(
                props.ok_or_else(|| "Button props must be an object".to_owned())?,
            )?;
            validate_parent_press(node, "Button")?;
        }
        "Input" => {
            allowed.push("press");
            validate_input_props_source(
                props.ok_or_else(|| "Input props must be an object".to_owned())?,
            )?;
            validate_parent_press(node, "Input")?;
        }
        "Select" => {
            allowed.push("press");
            validate_select_props_source(
                props.ok_or_else(|| "Select props must be an object".to_owned())?,
            )?;
            validate_parent_press(node, "Select")?;
        }
        "Link" => {
            allowed.push("children");
            validate_link_props_source(
                props.ok_or_else(|| "Link props must be an object".to_owned())?,
            )?;
            if let Some(children) = node.get_property("children") {
                let Some(length) = children.array_len() else {
                    return Err("Link children must be an array of text".into());
                };
                for index in 0..length {
                    let child = children.get_index(index).expect("index is in range");
                    if child.code_units().is_none() {
                        return Err("Link children must be strings".into());
                    }
                    if child.code_units().is_some_and(|text| text.len() > 10_000) {
                        return Err("Link child text exceeds 10000 characters".into());
                    }
                }
            }
        }
        "Code" => validate_code_props_source(
            props.ok_or_else(|| "Code props must be an object".to_owned())?,
        )?,
        "Markdown" => {
            allowed.push("press");
            if node.get_property("press").is_some() {
                validate_parent_press(node, "Markdown")?;
                if props
                    .and_then(|props| props.get_property("key"))
                    .and_then(JsonValueView::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err("pressable Markdown needs a non-empty key".into());
                }
            } else if props
                .and_then(|props| props.get_property("pressableLinks"))
                .is_some_and(|links| links.array_len().is_some_and(|length| length > 0))
            {
                return Err("Markdown pressableLinks need a private press identity".into());
            }
            validate_markdown_props_source(
                props.ok_or_else(|| "Markdown props must be an object".to_owned())?,
            )?;
        }
        "Client" => {
            allowed.push("client");
            validate_client_parent_element_view(node)?;
            let client = node
                .get_property("client")
                .filter(|client| client.object_entries().is_some())
                .ok_or_else(|| "Client needs its private owner stamp".to_owned())?;
            if !keys_are_only(client, &["plugin"])
                || client
                    .get_property("plugin")
                    .and_then(JsonValueView::as_str)
                    .is_none_or(str::is_empty)
            {
                return Err("Client owner stamp needs a non-empty plugin".into());
            }
        }
        "Svg" => {
            let props = props.ok_or_else(|| "Svg props must be an object".to_owned())?;
            const ALLOWED: &[&str] = &["source", "alt", "width", "height", "isInteractive"];
            if !keys_are_only(props, ALLOWED)
                || props
                    .get_property("source")
                    .and_then(JsonValueView::as_str)
                    .is_none_or(|source| utf16_len(source) > 131_072)
                || props
                    .get_property("alt")
                    .and_then(JsonValueView::as_str)
                    .is_none_or(|alt| utf16_len(alt) > 10_000 || has_control_chars(alt))
                || ["width", "height"].iter().any(|key| {
                    props
                        .get_property(key)
                        .is_some_and(|value| !finite_number(value, f64::MIN_POSITIVE, 4096.0))
                })
                || props
                    .get_property("isInteractive")
                    .is_some_and(|value| value.as_bool().is_none())
            {
                return Err("Svg props do not match the Desktop schema".into());
            }
        }
        _ => return Err(format!("unsupported Desktop UI element {kind}")),
    }
    if !keys_are_only(node, &allowed) {
        return Err(format!("{kind} node contains unsupported fields"));
    }
    // Keep the borrow live through the schema checks above; it also verifies
    // that the node really is an object even if an empty object was supplied.
    let _ = object;
    Ok(())
}

fn validate_parent_group(group: JsonValueView<'_>) -> Result<(), String> {
    if group.object_entries().is_none()
        || !keys_are_only(group, &["plugin"])
        || group
            .get_property("plugin")
            .and_then(JsonValueView::as_str)
            .is_none_or(|plugin| plugin.is_empty() || utf16_len(plugin) > 256)
    {
        return Err("Desktop UI group must contain a 1 to 256 character plugin".into());
    }
    Ok(())
}

fn validate_parent_hover(
    kind: &str,
    hover: JsonValueView<'_>,
    base_props: Option<JsonValueView<'_>>,
    node: JsonValueView<'_>,
    has_keyed_box_scope: bool,
) -> Result<(), String> {
    if hover.object_entries().is_none() {
        return Err("Desktop UI hover metadata must be an object".into());
    }
    let explicit_scope = hover.get_property("scope");
    if explicit_scope.is_some_and(|scope| {
        scope.as_str().is_none_or(|scope| {
            scope.is_empty() || utf16_len(scope) > 64 || has_control_chars(scope)
        })
    }) {
        return Err("Desktop UI hover scope must be 1 to 64 safe characters".into());
    }
    if explicit_scope.is_none() && !has_keyed_box_scope {
        return Err("Desktop UI hover needs a keyed Box scope".into());
    }
    if hover.object_entries().is_some_and(|entries| {
        entries.into_iter().any(|entry| {
            entry.value.is_null()
                || entry.value.array_len().is_some()
                || entry.value.object_entries().is_some()
                || entry
                    .value
                    .as_number()
                    .is_some_and(|number| !number.is_finite())
        })
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
                node.get_property("group")
                    .ok_or_else(|| format!("{kind} scoped hover needs a group owner"))?,
            )?,
            "Button" => validate_parent_press(node, "Button")?,
            _ => {}
        }
    }
    for key in ["borderColor", "backgroundColor", "color"] {
        if hover
            .get_property(key)
            .is_some_and(|value| !valid_color(value))
        {
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
        if hover
            .get_property(key)
            .is_some_and(|value| value.as_bool().is_none())
        {
            return Err(format!("{kind} hover {key} must be a boolean"));
        }
    }
    if let Some(border_style) = hover.get_property("borderStyle") {
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
        if !base_props
            .and_then(|props| props.get_property("borderStyle"))
            .is_some()
            || border_style
                .as_str()
                .is_none_or(|style| !BORDER_STYLES.contains(&style))
        {
            return Err("Box hover borderStyle needs a base border and valid style".into());
        }
    }
    if hover.get_property("display").is_some_and(|value| {
        base_props
            .and_then(|props| props.get_property("display"))
            .and_then(JsonValueView::as_str)
            != Some("none")
            || value.as_str() != Some("flex")
    }) {
        return Err("Box hover display can only restore flex from a hidden base Box".into());
    }
    for key in ["top", "left", "right", "bottom"] {
        if hover.get_property(key).is_some_and(|value| {
            base_props
                .and_then(|props| props.get_property("position"))
                .and_then(JsonValueView::as_str)
                != Some("absolute")
                || !safe_integer_range(value, -10_000, 10_000)
        }) {
            return Err(format!(
                "Box hover {key} needs an absolute base position and bounded integer"
            ));
        }
    }
    Ok(())
}

fn subtree_has_hover(node: JsonValueView<'_>) -> bool {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if node.get_property("hover").is_some() {
            return true;
        }
        if let Some(children) = node.get_property("children") {
            if let Some(length) = children.array_len() {
                for index in 0..length {
                    if let Some(child) = children.get_index(index) {
                        stack.push(child);
                    }
                }
            }
        }
    }
    false
}

fn validate_parent_press(node: JsonValueView<'_>, kind: &str) -> Result<(), String> {
    let Some(press) = node.get_property("press") else {
        return Err(format!("{kind} needs a private press identity"));
    };
    if !keys_are_only(press, &["plugin", "handle"])
        || press
            .get_property("plugin")
            .and_then(JsonValueView::as_str)
            .is_none_or(str::is_empty)
        || press
            .get_property("handle")
            .and_then(safe_unsigned_value)
            .is_none_or(|handle| handle == 0 || handle as f64 > MAX_SAFE_JS_INTEGER)
    {
        return Err(format!(
            "{kind} press identity must be {{ plugin, handle }}"
        ));
    }
    Ok(())
}

fn validate_box_props_source(props: JsonValueView<'_>) -> Result<(), String> {
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
        .get_property("key")
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
    for (key, variants) in enums {
        if props.get_property(key).is_some_and(|value| {
            value
                .as_str()
                .is_none_or(|candidate| !variants.contains(&candidate))
        }) {
            return Err(format!("Box {key} has an unsupported value"));
        }
    }
    for key in ["width", "height", "minWidth", "minHeight"] {
        if props
            .get_property(key)
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
            .get_property(key)
            .is_some_and(|value| !finite_number(value, -10_000.0, 10_000.0))
        {
            return Err(format!(
                "Box {key} must be a finite number with magnitude at most 10000"
            ));
        }
    }
    for key in ["top", "left", "right", "bottom"] {
        if props
            .get_property(key)
            .is_some_and(|value| !safe_integer_range(value, -10_000, 10_000))
        {
            return Err(format!("Box {key} must be an integer from -10000 to 10000"));
        }
    }
    for key in ["borderColor", "borderDimColor", "backgroundColor"] {
        if props
            .get_property(key)
            .is_some_and(|value| !valid_color(value))
        {
            return Err(format!("Box {key} must be a color string"));
        }
    }
    Ok(())
}

fn validate_text_props_source(props: JsonValueView<'_>) -> Result<(), String> {
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
        .get_property("key")
        .is_some_and(|value| value.as_str().is_none_or(str::is_empty))
    {
        return Err("Text key must be a non-empty string".into());
    }
    for key in ["color", "backgroundColor"] {
        if props
            .get_property(key)
            .is_some_and(|value| !valid_color(value))
        {
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
        if props
            .get_property(key)
            .is_some_and(|value| value.as_bool().is_none())
        {
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
        .get_property("wrap")
        .is_some_and(|value| value.as_str().is_none_or(|wrap| !WRAP.contains(&wrap)))
    {
        return Err("Text wrap has an unsupported value".into());
    }
    Ok(())
}

fn validate_button_props_source(props: JsonValueView<'_>) -> Result<(), String> {
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
            .get_property("key")
            .and_then(JsonValueView::as_str)
            .is_none_or(str::is_empty)
        || props
            .get_property("label")
            .and_then(JsonValueView::as_str)
            .is_none()
        || props.get_property("hotkey").is_some_and(|value| {
            value.as_str().is_none_or(|text| {
                text.len() != 1
                    || !text
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            })
        })
        || props
            .get_property("action")
            .is_some_and(|value| value.as_str().is_none_or(str::is_empty))
        || props
            .get_property("plain")
            .is_some_and(|value| value.as_bool() != Some(true))
        || props
            .get_property("dimColor")
            .is_some_and(|value| value.as_bool().is_none())
        || props
            .get_property("variant")
            .is_some_and(|value| !matches!(value.as_str(), Some("primary" | "secondary")))
        || props
            .get_property("role")
            .is_some_and(|value| value.as_str() != Some("dismiss"))
        || props
            .get_property("autoFocus")
            .is_some_and(|value| value.as_bool() != Some(true))
    {
        return Err("Button props do not match the Desktop schema".into());
    }
    Ok(())
}

fn validate_input_props_source(props: JsonValueView<'_>) -> Result<(), String> {
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
            .get_property("key")
            .and_then(JsonValueView::as_str)
            .is_none_or(str::is_empty)
        || ["label", "placeholder", "value", "submitLabel"]
            .iter()
            .any(|key| {
                props
                    .get_property(key)
                    .is_some_and(|value| value.as_str().is_none())
            })
        || props
            .get_property("autoFocus")
            .is_some_and(|value| value.as_bool() != Some(true))
    {
        return Err("Input props do not match the Desktop schema".into());
    }
    Ok(())
}

fn validate_select_props_source(props: JsonValueView<'_>) -> Result<(), String> {
    const ALLOWED: &[&str] = &["key", "label", "options", "value", "autoFocus"];
    let Some(options) = props
        .get_property("options")
        .and_then(|options| options.array_len().map(|len| (options, len)))
    else {
        return Err("Select needs an options array".into());
    };
    let (options, length) = options;
    let mut seen = HashSet::new();
    if !keys_are_only(props, ALLOWED)
        || props
            .get_property("key")
            .and_then(JsonValueView::as_str)
            .is_none_or(str::is_empty)
        || !(1..=64).contains(&length)
    {
        return Err("Select props do not match the Desktop schema".into());
    }
    for index in 0..length {
        let option = options.get_index(index).expect("index is in range");
        let Some(value) = option
            .get_property("value")
            .and_then(JsonValueView::code_units)
        else {
            return Err("Select props do not match the Desktop schema".into());
        };
        if !keys_are_only(option, &["value", "label"])
            || option
                .get_property("label")
                .is_some_and(|label| label.as_str().is_none())
            || !seen.insert(value.to_vec())
        {
            return Err("Select props do not match the Desktop schema".into());
        }
    }
    if ["label", "value"].iter().any(|key| {
        props
            .get_property(key)
            .is_some_and(|value| value.as_str().is_none())
    }) || props
        .get_property("autoFocus")
        .is_some_and(|value| value.as_bool() != Some(true))
    {
        return Err("Select props do not match the Desktop schema".into());
    }
    Ok(())
}

fn validate_link_props_source(props: JsonValueView<'_>) -> Result<(), String> {
    const ALLOWED: &[&str] = &["href", "label"];
    if !keys_are_only(props, ALLOWED)
        || props
            .get_property("href")
            .and_then(JsonValueView::as_str)
            .is_none_or(|href| href.trim().is_empty() || utf16_len(href) > 2_048)
        || props.get_property("label").is_some_and(|value| {
            value
                .as_str()
                .is_none_or(|label| label.trim().is_empty() || utf16_len(label) > 10_000)
        })
    {
        return Err("Link props do not match the Desktop schema".into());
    }
    Ok(())
}

fn validate_code_props_source(props: JsonValueView<'_>) -> Result<(), String> {
    const ALLOWED: &[&str] = &["source", "language", "path", "startLine", "format", "wrap"];
    if !keys_are_only(props, ALLOWED)
        || props
            .get_property("source")
            .and_then(JsonValueView::as_str)
            .is_none()
        || ["language", "path"].iter().any(|key| {
            props
                .get_property(key)
                .is_some_and(|value| value.as_str().is_none())
        })
        || props
            .get_property("startLine")
            .is_some_and(|value| !safe_integer_range(value, 1, 1_000_000_000))
        || props
            .get_property("format")
            .is_some_and(|value| !matches!(value.as_str(), Some("source" | "diff")))
        || props
            .get_property("wrap")
            .is_some_and(|value| !matches!(value.as_str(), Some("wrap" | "truncate-end")))
    {
        return Err("Code props do not match the Desktop schema".into());
    }
    Ok(())
}

fn validate_markdown_props_source(props: JsonValueView<'_>) -> Result<(), String> {
    const ALLOWED: &[&str] = &["key", "text", "dimColor", "pressableLinks"];
    if !keys_are_only(props, ALLOWED)
        || props
            .get_property("text")
            .and_then(JsonValueView::as_str)
            .is_none()
        || props
            .get_property("key")
            .is_some_and(|value| value.as_str().is_none_or(str::is_empty))
        || props
            .get_property("dimColor")
            .is_some_and(|value| value.as_bool().is_none())
    {
        return Err("Markdown props do not match the Desktop schema".into());
    }
    if let Some(links) = props.get_property("pressableLinks") {
        let Some(length) = links.array_len() else {
            return Err("Markdown pressableLinks must be an array".into());
        };
        if length > 256
            || (0..length).any(|index| {
                links
                    .get_index(index)
                    .and_then(JsonValueView::as_str)
                    .is_none_or(|href| href.trim().is_empty() || utf16_len(href) > 2_048)
            })
        {
            return Err("Markdown pressableLinks contain an invalid link".into());
        }
    }
    Ok(())
}

fn keys_are_only(value: JsonValueView<'_>, allowed: &[&str]) -> bool {
    value.object_entries().is_some_and(|entries| {
        entries.into_iter().all(|entry| {
            allowed.iter().any(|allowed| {
                entry.code_units == allowed.encode_utf16().collect::<Vec<_>>().as_slice()
            })
        })
    })
}

fn finite_number(value: JsonValueView<'_>, minimum: f64, maximum: f64) -> bool {
    value
        .as_number()
        .is_some_and(|number| number.is_finite() && minimum <= number && number <= maximum)
}

fn safe_integer(value: JsonValueView<'_>) -> bool {
    safe_integer_range(
        value,
        -(MAX_SAFE_JS_INTEGER as i64),
        MAX_SAFE_JS_INTEGER as i64,
    )
}

fn safe_integer_range(value: JsonValueView<'_>, minimum: i64, maximum: i64) -> bool {
    value.as_number().is_some_and(|number| {
        number.is_finite()
            && number.fract() == 0.0
            && number >= minimum as f64
            && number <= maximum as f64
            && number.abs() <= MAX_SAFE_JS_INTEGER
    })
}

fn safe_unsigned_value(value: JsonValueView<'_>) -> Option<u64> {
    let number = value.as_number()?;
    (number.is_finite() && number.fract() == 0.0 && number >= 0.0 && number <= MAX_SAFE_JS_INTEGER)
        .then_some(number as u64)
}

fn safe_unsigned(value: JsonValueView<'_>, minimum: u64) -> bool {
    safe_unsigned_value(value).is_some_and(|number| number >= minimum)
}

fn valid_percent_or_dimension(value: JsonValueView<'_>) -> bool {
    finite_number(value, 0.0, 10_000.0) || value.as_str().is_some_and(valid_client_percentage)
}

fn valid_client_percentage(value: &str) -> bool {
    let Some(digits) = value.strip_suffix('%') else {
        return false;
    };
    (1..=3).contains(&digits.len()) && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_color(value: JsonValueView<'_>) -> bool {
    value.as_str().is_some_and(|value| {
        (1..=40).contains(&value.len())
            && value.bytes().all(|byte| {
                b"#abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_().,% -"
                    .contains(&byte)
            })
    })
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn has_control_chars(value: &str) -> bool {
    value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_ui_render_event, validate_client_parent_element, validate_desktop_parent_tree,
    };
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    #[test]
    fn render_normalization_keeps_exact_props_and_input_member_order() {
        let input = Utf16JsonProjection::parse(
            r#"{"unknown":true,"surface":"desktop","component":"Pane","requestId":"\ud800","props":{"\ud800":"\ud800","ok":1},"viewport":{"extra":true,"columns":80,"rows":24}}"#,
        )
        .unwrap();
        let normalized = normalize_ui_render_event(&input).unwrap();
        assert_eq!(
            normalized.to_json_string().unwrap(),
            r#"{"surface":"desktop","component":"Pane","requestId":"\ud800","props":{"\ud800":"\ud800","ok":1},"viewport":{"columns":80,"rows":24}}"#
        );
    }

    #[test]
    fn client_parent_validation_reads_unpaired_strings_from_projection() {
        let client = Utf16JsonProjection::parse(
            r#"{"type":"Client","props":{"key":"card","module":"view.ts","props":{"\ud800":"\ud800"}},"client":{"plugin":"review"}}"#,
        )
        .unwrap();
        validate_client_parent_element(&client).unwrap();
        validate_desktop_parent_tree(&client).unwrap();
    }
}
