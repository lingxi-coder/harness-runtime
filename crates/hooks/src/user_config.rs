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

use serde_json::{Map, Value};

/// The literal `${user_config.` prefix (14 bytes: `${` + `user_config` + `.`).
const PREFIX: &str = "${user_config.";

/// `String(value)` semantics for a resolved userConfig value: strings pass
/// through, numbers/booleans use their JS `String()` form, and an array joins
/// its (stringified) elements with `,` (`String(["a","b"]) === "a,b"`). Objects
/// collapse to `[object Object]` and null to `null`, matching JS coercion —
/// userConfig values are declared as `string | number | boolean | string[]`, so
/// these last two are defensive.
#[must_use]
pub fn value_to_env_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(a) => a
            .iter()
            .map(value_to_env_string)
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// If `s` is EXACTLY `${user_config.KEY}` (whole-string form), return `KEY`.
/// Mirrors `^\$\{user_config\.[^}]+\}$`: the inner segment must be non-empty and
/// contain no `}` (so `${user_config.A}${user_config.B}` does NOT match).
fn whole_string_key(s: &str) -> Option<&str> {
    let inner = s.strip_prefix(PREFIX)?.strip_suffix('}')?;
    if inner.is_empty() || inner.contains('}') {
        return None;
    }
    Some(inner)
}

/// Replace every inline `${user_config.KEY}` occurrence in `s` with
/// `String(value)`. Unresolved keys (and the degenerate empty-key
/// `${user_config.}`, which `[^}]+` never matches) are emitted verbatim.
fn substitute_inline(s: &str, ctx: &Map<String, Value>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(PREFIX) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + PREFIX.len()..];
        match after.find('}') {
            // Empty key — not a `[^}]+` match; emit the prefix literally and
            // continue scanning after it.
            Some(0) => {
                out.push_str(PREFIX);
                rest = after;
            }
            Some(end) => {
                let key = &after[..end];
                match ctx.get(key) {
                    Some(v) => out.push_str(&value_to_env_string(v)),
                    None => {
                        out.push_str(PREFIX);
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            // No closing brace at all — the tail can hold no further match.
            None => {
                out.push_str(PREFIX);
                out.push_str(after);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Substitute `${user_config.*}` references throughout a JSON value.
///
/// Objects recurse per-value; arrays recurse per-element with whole-string
/// spread (an element that IS `${user_config.KEY}` resolving to an array is
/// flattened into the parent); strings use whole-string type-preservation when
/// they match exactly, else inline `String()` substitution.
#[must_use]
pub fn substitute_value(v: &Value, ctx: &Map<String, Value>) -> Value {
    match v {
        Value::String(s) => match whole_string_key(s) {
            Some(key) => ctx.get(key).cloned().unwrap_or_else(|| v.clone()),
            None => Value::String(substitute_inline(s, ctx)),
        },
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                if let Value::String(s) = item {
                    if let Some(key) = whole_string_key(s) {
                        match ctx.get(key) {
                            Some(Value::Array(inner)) => out.extend(inner.iter().cloned()),
                            Some(other) => out.push(other.clone()),
                            None => out.push(item.clone()),
                        }
                        continue;
                    }
                }
                out.push(substitute_value(item, ctx));
            }
            Value::Array(out)
        }
        Value::Object(o) => {
            let mut m = Map::with_capacity(o.len());
            for (k, val) in o {
                m.insert(k.clone(), substitute_value(val, ctx));
            }
            Value::Object(m)
        }
        other => other.clone(),
    }
}

/// Substitute into a plain string field (an MCP `command`, an env value): the
/// whole-string form is stringified (env/command values are always strings), so
/// this flattens a resolved value through [`value_to_env_string`].
#[must_use]
pub fn substitute_string_field(s: &str, ctx: &Map<String, Value>) -> String {
    value_to_env_string(&substitute_value(&Value::String(s.to_string()), ctx))
}

/// Substitute into an argv vector, honouring whole-string array-spread: an arg
/// that IS `${user_config.KEY}` resolving to an array contributes one arg per
/// element (`r.push(...i)`); every other arg is substituted in place.
#[must_use]
pub fn substitute_args(args: &[String], ctx: &Map<String, Value>) -> Vec<String> {
    let arr = Value::Array(args.iter().map(|a| Value::String(a.clone())).collect());
    match substitute_value(&arr, ctx) {
        Value::Array(items) => items.iter().map(value_to_env_string).collect(),
        _ => args.to_vec(),
    }
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
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
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
    use serde_json::json;

    fn ctx() -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("API_KEY".into(), json!("sk-123"));
        m.insert("PORT".into(), json!(8080));
        m.insert("DEBUG".into(), json!(true));
        m.insert("HOSTS".into(), json!(["a", "b"]));
        m
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
