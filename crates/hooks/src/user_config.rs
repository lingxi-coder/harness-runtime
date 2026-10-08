//! `${user_config.KEY}` substitution + plugin-option env helpers.
//!
//! A plugin's resolved `userConfig` values (non-sensitive from
//! `pluginConfigs[plugin].options`, sensitive from secure storage — see
//! `plugin::loader::resolve_user_config`) are substituted into the plugin's
//! MCP / LSP server config, hook commands, and (non-sensitive) skill/agent
//! content via `${user_config.KEY}` references.
//!
//! This module lives in the `hooks` crate (below `plugin` in the dependency
//! graph) so the plugin-hook executor can reuse the same substitution + gate
//! logic at spawn time; `plugin` re-exports it as `plugin::user_config`.
//!
//! Byte-faithful port of claude-code 2.1.207's two substitution forms:
//! * **whole-string, type-preserving** — a string that IS exactly
//!   `${user_config.KEY}` (`^\$\{user_config\.[^}]+\}$`) is replaced by the raw
//!   value, so a number stays a number, a boolean a boolean, and an array is
//!   spread into its parent array (`if(Array.isArray(i))r.push(...i)`);
//! * **inline** — every `${user_config.KEY}` occurrence
//!   (`\$\{user_config\.([^}]+)\}`) inside a larger string is replaced by
//!   `String(value)`.
//!
//! An unresolved key (not present in the resolved map) is left unchanged, in
//! both forms (`else r.push(n)` / no-op replace), so a `${user_config.MISSING}`
//! never turns into the literal `undefined`.

use lingxi_core::types::utf16_json::{
    JsonValueView, Utf16JsonBuilder, Utf16JsonProjection as Source,
};

const PREFIX: &str = "${user_config.";

fn source_text(units: Vec<u16>) -> Source {
    Source::root_string(String::from_utf16_lossy(&units), units).expect("source text")
}

fn number_string(number: f64) -> String {
    if number.is_nan() {
        return "NaN".into();
    }
    if number == f64::INFINITY {
        return "Infinity".into();
    }
    if number == f64::NEG_INFINITY {
        return "-Infinity".into();
    }
    let mut builder = Utf16JsonBuilder::new();
    let root = builder.push_number(number);
    builder
        .finish(root)
        .expect("number source")
        .to_json_string()
        .expect("JS number encoding")
}

fn coercion_units(value: JsonValueView<'_>) -> Vec<u16> {
    enum Work<'a> {
        Value(JsonValueView<'a>, bool),
        Comma,
    }
    let mut work = vec![Work::Value(value, false)];
    let mut out = Vec::new();
    while let Some(task) = work.pop() {
        let (value, array_element) = match task {
            Work::Comma => {
                out.push(44);
                continue;
            }
            Work::Value(value, array_element) => (value, array_element),
        };
        if let Some(units) = value.code_units() {
            out.extend_from_slice(units);
        } else if value.is_null() {
            if !array_element {
                out.extend("null".encode_utf16());
            }
        } else if let Some(value) = value.as_bool() {
            out.extend(if value { "true" } else { "false" }.encode_utf16());
        } else if let Some(value) = value.as_number() {
            out.extend(number_string(value).encode_utf16());
        } else if let Some(length) = value.array_len() {
            for index in (0..length).rev() {
                work.push(Work::Value(
                    value.get_index(index).expect("array child"),
                    true,
                ));
                if index > 0 {
                    work.push(Work::Comma);
                }
            }
        } else {
            out.extend("[object Object]".encode_utf16());
        }
    }
    out
}

/// JavaScript String(value), retaining source units, including nested array joins.
#[must_use]
pub fn value_to_string_source(value: &Source) -> Source {
    source_text(coercion_units(value.at_pointer("").expect("source root")))
}

/// Convert a substituted value at the actual process/environment boundary.
#[must_use]
pub fn value_to_env_string(value: &Source) -> String {
    String::from_utf16_lossy(&coercion_units(value.at_pointer("").expect("source root")))
}

fn whole_string_key(units: &[u16]) -> Option<&[u16]> {
    let prefix = PREFIX.encode_utf16().collect::<Vec<_>>();
    let inner = units
        .strip_prefix(prefix.as_slice())?
        .strip_suffix(&[125])?;
    (!inner.is_empty() && !inner.contains(&125)).then_some(inner)
}

fn substitute_inline(units: &[u16], ctx: JsonValueView<'_>) -> Vec<u16> {
    let prefix = PREFIX.encode_utf16().collect::<Vec<_>>();
    let mut out = Vec::with_capacity(units.len());
    let mut cursor = 0;
    while let Some(relative) = units[cursor..]
        .windows(prefix.len())
        .position(|window| window == prefix)
    {
        let start = cursor + relative;
        out.extend_from_slice(&units[cursor..start]);
        let key_start = start + prefix.len();
        let Some(end) = units[key_start..]
            .iter()
            .position(|unit| *unit == 125)
            .map(|offset| key_start + offset)
        else {
            out.extend_from_slice(&units[start..]);
            return out;
        };
        if key_start != end {
            if let Some(value) = ctx.get_property_units(&units[key_start..end]) {
                out.extend(coercion_units(value));
            } else {
                out.extend_from_slice(&units[start..=end]);
            }
        } else {
            out.extend_from_slice(&units[start..=end]);
        }
        cursor = end + 1;
    }
    out.extend_from_slice(&units[cursor..]);
    out
}

/// Substitute source string values; exact whole-string array references spread
/// once into their containing array. Replacement values are not re-evaluated.
#[must_use]
pub fn substitute_value(value: &Source, ctx: &Source) -> Source {
    enum Work<'a> {
        Visit(JsonValueView<'a>, bool),
        Array(usize),
        Object(Vec<Vec<u16>>),
    }
    let ctx = ctx.at_pointer("").expect("source context root");
    let mut builder = Utf16JsonBuilder::new();
    let mut work = vec![Work::Visit(
        value.at_pointer("").expect("source root"),
        true,
    )];
    let mut completed = Vec::new();
    while let Some(task) = work.pop() {
        match task {
            Work::Array(length) => {
                let children = completed.split_off(completed.len() - length);
                completed.push(builder.push_array(children).expect("source array"));
            }
            Work::Object(keys) => {
                let children = completed.split_off(completed.len() - keys.len());
                completed.push(
                    builder
                        .push_object_units(keys.into_iter().zip(children).collect())
                        .expect("source object"),
                );
            }
            Work::Visit(value, substitute) => {
                if let Some(units) = value.code_units() {
                    if substitute {
                        if let Some(replacement) =
                            whole_string_key(units).and_then(|key| ctx.get_property_units(key))
                        {
                            work.push(Work::Visit(replacement, false));
                        } else {
                            completed
                                .push(builder.push_string_units(substitute_inline(units, ctx)));
                        }
                    } else {
                        completed.push(builder.push_string_units(units.to_vec()));
                    }
                } else if value.is_null() {
                    completed.push(builder.push_null());
                } else if let Some(value) = value.as_bool() {
                    completed.push(builder.push_bool(value));
                } else if let Some(value) = value.as_number() {
                    completed.push(builder.push_number(value));
                } else if let Some(length) = value.array_len() {
                    let mut children = Vec::new();
                    for index in 0..length {
                        let child = value.get_index(index).expect("array child");
                        let replacement = substitute
                            .then(|| {
                                child
                                    .code_units()
                                    .and_then(whole_string_key)
                                    .and_then(|key| ctx.get_property_units(key))
                            })
                            .flatten();
                        if let Some(replacement) = replacement {
                            if let Some(count) = replacement.array_len() {
                                children.extend((0..count).map(|index| {
                                    (
                                        replacement.get_index(index).expect("replacement child"),
                                        false,
                                    )
                                }));
                            } else {
                                children.push((replacement, false));
                            }
                        } else {
                            children.push((child, substitute));
                        }
                    }
                    work.push(Work::Array(children.len()));
                    work.extend(
                        children
                            .into_iter()
                            .rev()
                            .map(|(value, substitute)| Work::Visit(value, substitute)),
                    );
                } else {
                    let entries = value.object_entries().expect("source object");
                    work.push(Work::Object(
                        entries
                            .iter()
                            .map(|entry| entry.code_units.to_vec())
                            .collect(),
                    ));
                    work.extend(
                        entries
                            .into_iter()
                            .rev()
                            .map(|entry| Work::Visit(entry.value, substitute)),
                    );
                }
            }
        }
    }
    builder
        .finish(completed.pop().expect("substitution root"))
        .expect("source substitution")
}

/// Substitute a model-visible text field without crossing a display boundary.
#[must_use]
pub fn substitute_text_field(text: &Source, ctx: &Source) -> Source {
    value_to_string_source(&substitute_value(text, ctx))
}

/// Substitute an OS command, URL, or environment field at its scalar boundary.
#[must_use]
pub fn substitute_string_field(text: &str, ctx: &Source) -> String {
    value_to_env_string(&substitute_value(&text.into(), ctx))
}

/// Expand argv just before process dispatch, then apply the OS string conversion.
#[must_use]
pub fn substitute_args(args: &[String], ctx: &Source) -> Vec<String> {
    let array = Source::array(args.iter().map(Source::from).collect()).expect("source argv");
    let substituted = substitute_value(&array, ctx);
    (0..substituted.array_len("").expect("argv array"))
        .map(|index| {
            value_to_env_string(
                &substituted
                    .subprojection(&format!("/{index}"))
                    .expect("argv child"),
            )
        })
        .collect()
}

/// `true` if `s` contains at least one `${user_config.KEY}` reference (with a
/// non-empty `KEY`). Mirrors `\$\{user_config\.([^}]+)\}` presence — used by the
/// hook/monitor safety gates.
#[must_use]
pub fn references_user_config(s: &str) -> bool {
    let mut rest = s;
    while let Some(pos) = rest.find(PREFIX) {
        let after = &rest[pos + PREFIX.len()..];
        match after.find('}') {
            Some(0) => rest = &after[1..], // empty key, keep scanning
            Some(_) => return true,
            None => return false,
        }
    }
    false
}

/// Sanitize a userConfig field name into the env-var suffix: every character
/// outside `[A-Za-z0-9_]` becomes `_`, then the whole thing is upper-cased
/// (`He.replace(/[^A-Za-z0-9_]/g,"_").toUpperCase()`).
#[must_use]
pub fn option_env_key(name: &str) -> String {
    name.encode_utf16()
        .map(|unit| match unit {
            48..=57 | 65..=90 | 97..=122 | 95 => {
                char::from_u32(u32::from(unit)).expect("ASCII option key")
            }
            _ => '_',
        })
        .collect::<String>()
        .to_ascii_uppercase()
}

/// The hook child-process env var a userConfig field is exposed under:
/// `LINGXI_PLUGIN_OPTION_<KEY>` (claude-code `CLAUDE_PLUGIN_OPTION_<KEY>`, with
/// the established LingXi `LINGXI_` prefix — cf. `LINGXI_PLUGIN_ROOT`).
#[must_use]
pub fn option_env_var(name: &str) -> String {
    format!("LINGXI_PLUGIN_OPTION_{}", option_env_key(name))
}

/// Build the byte-faithful rejection message for a plugin **monitor** command
/// that references `${user_config.*}` (monitor commands can never safely carry a
/// substituted value). `name` is the monitor/hook display name.
#[must_use]
pub fn monitor_reference_rejection(name: &str) -> String {
    format!(
        "{name} references ${{user_config.*}} in its command. The substituted \
         value would be passed to a shell. Monitor commands cannot safely \
         reference ${{user_config.*}}; have the monitor script read the value \
         from a config file or prompt instead."
    )
}

/// Build the rejection message for a **shell-form** plugin hook command (a bare
/// command string with no discrete exec-form `args`) that references
/// `${user_config.*}`: the substituted value would be re-parsed by the shell, so
/// the hook fails rather than running with an injection hazard.
///
/// Byte-faithful port of claude-code 2.1.207's hook advisory (`GSo` /
/// `throw new Bn(…)`), reassembled from its template quasis:
/// `` `Hook from ${c?`plugin ${c}`:"a plugin"} references ${user_config.*} in a
/// shell-form command. The substituted value would be re-parsed by the shell.
/// Use exec `+'form instead — {"command": "<executable>", "args": '+'["${user_config.KEY}",
/// ...]} — or read '+`$CLAUDE_PLUGIN_OPTION_<KEY> from the hook's environment.
/// Command: ${Vve(e)}` ``. The em-dashes (`—`, U+2014) and the exact wording are
/// preserved; the ONLY branding change is the env-var name
/// `CLAUDE_PLUGIN_OPTION_<KEY>` → `LINGXI_PLUGIN_OPTION_<KEY>` (cf.
/// [`option_env_var`]). `plugin_display` is the owning plugin's name (CC's `c`);
/// `command` is the offending command string (CC's `Vve(e)`, the hook display
/// text — for a shell-form hook that is the bare command).
#[must_use]
pub fn shell_form_reference_rejection(plugin_display: &str, command: &str) -> String {
    format!(
        "Hook from plugin {plugin_display} references ${{user_config.*}} in a \
         shell-form command. The substituted value would be re-parsed by the \
         shell. Use exec form instead \u{2014} {{\"command\": \"<executable>\", \
         \"args\": [\"${{user_config.KEY}}\", ...]}} \u{2014} or read \
         $LINGXI_PLUGIN_OPTION_<KEY> from the hook's environment. Command: \
         {command}"
    )
}

/// Build the byte-faithful rejection message for a plugin **MCP stdio** server
/// whose `command` (the shell-executed field) references `${user_config.*}`. The
/// substituted value would be passed to a shell, so the server is rejected and
/// the operator is told to read the value inside the helper script (e.g. from an
/// env var set in the server's `"env"` block — which IS a safe substitution
/// surface). `name` is the scoped server name (CC's `${…}` server key).
///
/// Byte-faithful port of claude-code 2.1.207's advisory, quoted verbatim from
/// the binary (`'` + `${name}` + `'` +
/// `` ` references ${user_config.*}. The substituted value would be passed to a
/// shell; read the value inside the helper script instead (e.g. from an env var
/// set in the server's "env" block).` ``). No branding change — this message
/// names no env var.
#[must_use]
pub fn mcp_stdio_reference_rejection(name: &str) -> String {
    format!(
        "'{name}' references ${{user_config.*}}. The substituted value would be \
         passed to a shell; read the value inside the helper script instead \
         (e.g. from an env var set in the server's \"env\" block)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    macro_rules! json {
        ($($value:tt)*) => { Source::from_serde_value(&serde_json::json!($($value)*)).unwrap() };
    }

    fn ctx() -> Source {
        json!({"API_KEY":"sk-123", "PORT":8080, "DEBUG":true, "HOSTS":["a","b"]})
    }

    #[test]
    fn whole_string_preserves_number_type() {
        let out = substitute_value(&json!("${user_config.PORT}"), &ctx());
        assert_eq!(out, json!(8080));
    }

    #[test]
    fn whole_string_preserves_bool_type() {
        let out = substitute_value(&json!("${user_config.DEBUG}"), &ctx());
        assert_eq!(out, json!(true));
    }

    #[test]
    fn whole_string_preserves_array_type() {
        let out = substitute_value(&json!("${user_config.HOSTS}"), &ctx());
        assert_eq!(out, json!(["a", "b"]));
    }

    #[test]
    fn inline_stringifies() {
        let out = substitute_value(
            &json!("port=${user_config.PORT}&k=${user_config.API_KEY}"),
            &ctx(),
        );
        assert_eq!(out, json!("port=8080&k=sk-123"));
    }

    #[test]
    fn inline_array_joins_with_comma() {
        let out = substitute_value(&json!("hosts=${user_config.HOSTS}"), &ctx());
        assert_eq!(out, json!("hosts=a,b"));
    }

    #[test]
    fn missing_key_left_unchanged_whole_and_inline() {
        assert_eq!(
            substitute_value(&json!("${user_config.NOPE}"), &ctx()),
            json!("${user_config.NOPE}")
        );
        assert_eq!(
            substitute_value(&json!("x=${user_config.NOPE};y"), &ctx()),
            json!("x=${user_config.NOPE};y")
        );
    }

    #[test]
    fn empty_key_is_literal() {
        assert_eq!(
            substitute_value(&json!("a${user_config.}b"), &ctx()),
            json!("a${user_config.}b")
        );
    }

    #[test]
    fn array_spread_flattens_into_parent() {
        let args = vec!["--hosts".to_string(), "${user_config.HOSTS}".to_string()];
        assert_eq!(substitute_args(&args, &ctx()), vec!["--hosts", "a", "b"]);
    }

    #[test]
    fn args_scalar_and_inline() {
        let args = vec![
            "--port=${user_config.PORT}".to_string(),
            "${user_config.API_KEY}".to_string(),
        ];
        assert_eq!(
            substitute_args(&args, &ctx()),
            vec!["--port=8080", "sk-123"]
        );
    }

    #[test]
    fn object_recurses() {
        let out = substitute_value(
            &json!({"env": {"KEY": "${user_config.API_KEY}", "P": "${user_config.PORT}"}}),
            &ctx(),
        );
        assert_eq!(out, json!({"env": {"KEY": "sk-123", "P": 8080}}));
    }

    #[test]
    fn string_field_stringifies_number() {
        assert_eq!(
            substitute_string_field("${user_config.PORT}", &ctx()),
            "8080"
        );
    }

    #[test]
    fn source_substitution_preserves_exact_keys_units_and_nonrecursive_replacements() {
        let ctx = Source::parse(
            r#"{"\ud800":"\udc00","items":["\ud800",{"v":"${user_config.PORT}"}],"PORT":42}"#,
        )
        .unwrap();
        let input = Source::parse(r#"{"text":"prefix ${user_config.\ud800}","args":["${user_config.items}"],"\ud800":"${user_config.PORT}"}"#).unwrap();
        let result = substitute_value(&input, &ctx);
        assert_eq!(
            result.string_units("/text"),
            Some("prefix ".encode_utf16().chain([0xdc00]).collect())
        );
        assert_eq!(result.string_units("/args/0"), Some(vec![0xd800]));
        assert_eq!(result.string_at("/args/1/v"), Some("${user_config.PORT}"));
        assert_eq!(
            result
                .subprojection_property_units(&[0xd800])
                .unwrap()
                .number_at(""),
            Some(42.0)
        );
        assert_eq!(
            value_to_env_string(&result.subprojection("/args/0").unwrap()),
            "�"
        );
    }

    #[test]
    fn string_coercion_uses_javascript_array_null_and_number_semantics() {
        assert_eq!(
            value_to_string_source(&Source::parse(r#"[null,[null,"x"],1e-7,-0]"#).unwrap())
                .string_at(""),
            Some(",,x,1e-7,0")
        );
        assert_eq!(
            value_to_string_source(&Source::parse("1e400").unwrap()).string_at(""),
            Some("Infinity")
        );
        assert_eq!(option_env_key("a😀b"), "A__B");
    }

    #[test]
    fn references_detection() {
        assert!(references_user_config("x ${user_config.K} y"));
        assert!(!references_user_config("no refs here"));
        assert!(!references_user_config("${user_config.}")); // empty key: not a match
        assert!(!references_user_config("${user_config.K")); // unterminated
    }

    #[test]
    fn env_key_sanitizes_and_uppercases() {
        assert_eq!(option_env_key("api-key.v2"), "API_KEY_V2");
        assert_eq!(option_env_key("Already_OK"), "ALREADY_OK");
        assert_eq!(option_env_var("api-key"), "LINGXI_PLUGIN_OPTION_API_KEY");
    }

    #[test]
    fn value_to_env_string_forms() {
        assert_eq!(value_to_env_string(&json!("s")), "s");
        assert_eq!(value_to_env_string(&json!(42)), "42");
        assert_eq!(value_to_env_string(&json!(false)), "false");
        assert_eq!(value_to_env_string(&json!(["a", "b", "c"])), "a,b,c");
    }

    #[test]
    fn monitor_rejection_is_byte_faithful() {
        assert_eq!(
            monitor_reference_rejection("my-monitor"),
            "my-monitor references ${user_config.*} in its command. The \
             substituted value would be passed to a shell. Monitor commands \
             cannot safely reference ${user_config.*}; have the monitor script \
             read the value from a config file or prompt instead."
        );
    }

    #[test]
    fn shell_form_rejection_is_byte_faithful() {
        // Branding-adjusted env var only (CLAUDE_PLUGIN_OPTION → LINGXI_PLUGIN_OPTION);
        // the rest mirrors the binary's advisory quasis, em-dashes (U+2014) and
        // all: `Hook from plugin <name> …Use exec form instead — {…} — or read …`.
        assert_eq!(
            shell_form_reference_rejection("weather", "./run.sh ${user_config.KEY}"),
            "Hook from plugin weather references ${user_config.*} in a \
             shell-form command. The substituted value would be re-parsed by \
             the shell. Use exec form instead \u{2014} {\"command\": \
             \"<executable>\", \"args\": [\"${user_config.KEY}\", ...]} \u{2014} \
             or read $LINGXI_PLUGIN_OPTION_<KEY> from the hook's environment. \
             Command: ./run.sh ${user_config.KEY}"
        );
    }

    #[test]
    fn mcp_stdio_rejection_is_byte_faithful() {
        assert_eq!(
            mcp_stdio_reference_rejection("plugin:weather:api"),
            "'plugin:weather:api' references ${user_config.*}. The substituted \
             value would be passed to a shell; read the value inside the helper \
             script instead (e.g. from an env var set in the server's \"env\" \
             block)."
        );
    }
}
