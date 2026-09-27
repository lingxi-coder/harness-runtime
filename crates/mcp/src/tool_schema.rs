//! §20a — normalize or drop an MCP tool's `inputSchema` before it reaches the
//! model.
//!
//! Two independent oracle transforms run over every advertised tool, in this
//! order, before the tool is added to the list handed to the provider:
//!
//! 1. **Root-combinator flattening** — oracle `Wrt(e)` (`cc_all.txt`
//!    @182171131). A schema whose ROOT uses a top-level `anyOf`/`oneOf`/
//!    `allOf` (the Anthropic API only accepts an `object` root) is either
//!    flattened into a plain `object` schema by merging every branch's
//!    `properties` (first-writer-wins, keys filtered through the same
//!    `^[a-zA-Z0-9_.-]{1,64}$` regex used for property-key validation) with a
//!    human-readable "Input constraint: …" note prepended to the
//!    description, or — if it can't be represented that way (a non-array
//!    combinator value) — dropped.
//! 2. **Schema-validity check** — oracle `qrt`/`Lr` (`cc_all.txt`
//!    @182173176). Checks the (possibly-flattened) schema's top-level
//!    `properties` keys against the same regex, then validates the schema
//!    document itself (as DATA) against the JSON-Schema 2020-12 meta-schema
//!    via a bundled validator (oracle: a `new Ajv2020(...).getSchema(H)`
//!    compiled meta-schema function; port: [`boon`], which ships the
//!    identical 2020-12 meta-schema set built in — see
//!    `orchestrator/src/schema_validation.rs` for the sibling tool-INPUT
//!    validator already using it). A schema that declares a `$schema` other
//!    than exactly the 2020-12 URI (with or without a trailing `#`) skips
//!    this check entirely and is treated as valid — the oracle's
//!    cross-draft support (`Ir()`) is permanently disabled
//!    (`function Ir(){return!1}`), so only 2020-12-or-absent `$schema`
//!    documents are ever actually checked.
//!
//! Both transforms are gated per-server by an ARRAY feature flag (oracle
//! `Ot(flag, serverConfig)`: `[]` disables the transform for everyone,
//! `["*"]` enables it for every server, and a list of hostnames enables it
//! only for servers whose URL hostname matches exactly or as a dot-suffix —
//! see [`gate_enabled`], the same idiom as
//! `mcp/src/protocol_negotiation.rs`'s `tengu_mcp_negotiation_server_denylist`
//! denylist, which is `telemetry::flag_string_list`'s worked example). With
//! no flag fetcher wired (this port's default, matching the shipped
//! binary's GrowthBook-absent state) both gates read empty and are always
//! OFF, so:
//!
//! * a root-combinator schema is unconditionally **dropped** (normalization
//!   never applies without the gate); and
//! * an otherwise-invalid schema is **kept, with a warning** — the
//!   drop-invalid transform never actually drops anything without its gate.
//!
//! Per-tool telemetry classification (`tengu_mcp_degraded` with reasons
//! `tool_schema_normalized` / `tool_schema_normalize_gated` /
//! `tool_schema_unsupported` / `tool_schema_invalid` /
//! `tool_property_key_invalid` / `tool_schema_invalid_gated` /
//! `tool_property_key_invalid_gated`, plus `tengu_mcp_tools_listed`'s
//! `normalizedCount`/`keptCount`) is DEFERRED — see the batch report; the
//! event registry is count-locked and these seven ids are not wired here.
//! What this module gives the caller is the pure KEEP / WARN / DROP decision
//! and the (possibly rewritten) schema + description.

use serde_json::{Map, Value};

/// `tengu_mcp_normalize_root_combinators` — array flag, default `[]`.
const FLAG_NORMALIZE_ROOT_COMBINATORS: &str = "tengu_mcp_normalize_root_combinators";
/// `tengu_mcp_drop_invalid_tool_schemas` — array flag, default `[]`.
const FLAG_DROP_INVALID_TOOL_SCHEMAS: &str = "tengu_mcp_drop_invalid_tool_schemas";

/// oracle `tt` — root combinators, in the exact order they're checked (also
/// the order they appear in a "top-level X/Y" message).
const ROOT_COMBINATORS: [&str; 3] = ["anyOf", "oneOf", "allOf"];
/// oracle `rt` — extra root keys copied verbatim onto a flattened schema.
const CARRIED_ROOT_KEYS: [&str; 6] = [
    "$defs",
    "definitions",
    "$schema",
    "additionalProperties",
    "description",
    "title",
];
/// oracle `H` — the one meta-schema this port (like the oracle) actually
/// validates against.
const META_SCHEMA_URL: &str = "https://json-schema.org/draft/2020-12/schema";

/// oracle `O` — property-key validity regex, SOURCE form (what `regex::Regex`
/// compiles).
const PROPERTY_KEY_PATTERN: &str = r"^[a-zA-Z0-9_.-]{1,64}$";
/// The same regex as the oracle RENDERS it into the rejection detail. `O` is a
/// `RegExp` object and the detail is a template literal
/// (`` `... does not match ${O}` ``), so JS stringifies it WITH its `/`
/// delimiters. Interpolating [`PROPERTY_KEY_PATTERN`] instead silently drops
/// them from a user-visible message.
const PROPERTY_KEY_PATTERN_RENDERED: &str = r"/^[a-zA-Z0-9_.-]{1,64}$/";

fn property_key_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(PROPERTY_KEY_PATTERN).expect("static regex"))
}

/// oracle `Ot(e,t)` (`cc_all.txt` @~182279107, shared with
/// `protocol_negotiation.rs`'s denylist): does `server_url`'s hostname fall
/// under the ARRAY flag named `flag` — listed explicitly (exact or
/// suffix-of-a-dot, case-insensitive) or wildcarded via a bare `"*"` entry
/// (which also enables a url-less/stdio server)?
fn gate_enabled(flag: &str, server_url: Option<&str>) -> bool {
    let list = telemetry::flag_string_list(flag, &[]);
    if list.is_empty() {
        return false;
    }
    if list.iter().any(|d| d == "*") {
        return true;
    }
    let Some(url) = server_url else {
        return false;
    };
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_lowercase();
    list.iter().any(|d| {
        if d.is_empty() {
            return false;
        }
        let d = d.to_lowercase();
        host == d || host.ends_with(&format!(".{d}"))
    })
}

/// Outcome of [`flatten_root_combinators`] (oracle `Wrt`'s `{outcome:...}`).
#[derive(Debug, Clone, PartialEq)]
enum RootCombinatorOutcome {
    /// No top-level combinator — nothing to do.
    Unchanged,
    /// Flattened into a plain `object` schema, with the combinators that
    /// were present (in oracle-`tt` order) and the human-readable note.
    Normalized {
        schema: Value,
        note: String,
        combinators: Vec<&'static str>,
    },
    /// Uses a top-level combinator that isn't (or can't be) an array —
    /// unconditionally unsupported, `reason` is the oracle's exact message.
    Drop(String),
}

/// `He(e)` — "is a plain JSON object" (not null, not an array).
fn is_object(v: &Value) -> bool {
    v.is_object()
}

/// oracle `x(e,t)` — resolve a same-document `#/$defs/<k>` or
/// `#/definitions/<k>` `$ref` against root schema `root`; anything else
/// (absent `$ref`, external/deep pointer, missing target) returns `node`
/// unchanged.
fn resolve_local_ref<'a>(node: &'a Value, root: &'a Value) -> &'a Value {
    let Some(Value::String(r)) = node.get("$ref") else {
        return node;
    };
    let Some(rest) = r.strip_prefix("#/") else {
        return node;
    };
    let Some((container, name)) = rest.split_once('/') else {
        return node;
    };
    if name.contains('/') || (container != "$defs" && container != "definitions") {
        return node;
    }
    let Some(bucket) = root.get(container).filter(|v| is_object(v)) else {
        return node;
    };
    match bucket.get(name) {
        Some(target) if is_object(target) => target,
        _ => node,
    }
}

/// oracle `a(f)` — merge a `properties` object's entries into `out`,
/// first-writer-wins, keys filtered through the property-key regex, values
/// required to be plain objects.
fn merge_properties(props: Option<&Value>, out: &mut Map<String, Value>) {
    let Some(Value::Object(map)) = props else {
        return;
    };
    for (k, v) in map {
        if property_key_regex().is_match(k) && !out.contains_key(k) && is_object(v) {
            out.insert(k.clone(), v.clone());
        }
    }
}

/// oracle `o(f)` inside `Wrt` — collect required-name strings from `names`
/// that are actual merged-property keys, first-occurrence order, deduped.
fn merge_required(names: Option<&Value>, merged_props: &Map<String, Value>, out: &mut Vec<String>) {
    let Some(Value::Array(arr)) = names else {
        return;
    };
    for name in arr {
        if let Some(s) = name.as_str() {
            if merged_props.contains_key(s) && !out.iter().any(|e| e == s) {
                out.push(s.to_string());
            }
        }
    }
}

/// oracle `ot(e)` — a short "required names, else property names" summary of
/// one branch, or `None` if neither is present.
fn describe_branch(schema: &Value) -> Option<String> {
    if !is_object(schema) {
        return None;
    }
    if let Some(Value::Array(req)) = schema.get("required") {
        if !req.is_empty() && req.iter().all(Value::is_string) {
            let joined = req
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            return Some(joined);
        }
    }
    if let Some(Value::Object(props)) = schema.get("properties") {
        if !props.is_empty() {
            return Some(props.keys().cloned().collect::<Vec<_>>().join(", "));
        }
    }
    None
}

/// oracle `nt(e,t,r)` — build the "Input constraint: …" note prepended to a
/// normalized tool's description. `present` = combinators found (oracle's
/// `t`/outer `e`), `root` = the original schema (oracle's outer `t`),
/// `has_any_or_one` = whether `anyOf`/`oneOf` (not just `allOf`) was present.
fn build_note(present: &[&'static str], root: &Value, has_any_or_one: bool) -> String {
    if !has_any_or_one {
        return "Input constraint: all listed parameters apply together (flattened from a JSON Schema allOf).".to_string();
    }
    let combinator = if present.contains(&"oneOf") {
        "oneOf"
    } else {
        "anyOf"
    };
    let mut groups: Vec<String> = Vec::new();
    if let Some(Value::Array(branches)) = root.get(combinator) {
        for branch in branches {
            let resolved = if is_object(branch) {
                resolve_local_ref(branch, root)
            } else {
                branch
            };
            if let Some(summary) = describe_branch(resolved) {
                if !groups.iter().any(|g| g == &summary) {
                    groups.push(summary);
                }
            }
        }
    }
    let verb = if combinator == "oneOf" {
        "Provide parameters for exactly one of"
    } else {
        "Provide parameters for at least one of"
    };
    if groups.is_empty() {
        format!(
            "Input constraint: {verb} the documented parameter groups (flattened from a JSON Schema {combinator})."
        )
    } else {
        let joined = groups
            .iter()
            .map(|g| format!("({g})"))
            .collect::<Vec<_>>()
            .join(" or ");
        format!("Input constraint: {verb}: {joined}.")
    }
}

/// oracle `Wrt(e)` — flatten a root-level `anyOf`/`oneOf`/`allOf` into a
/// plain `object` schema, or report it can't be normalized.
fn flatten_root_combinators(schema: &Value) -> RootCombinatorOutcome {
    if !is_object(schema) {
        return RootCombinatorOutcome::Unchanged;
    }
    let present: Vec<&'static str> = ROOT_COMBINATORS
        .iter()
        .copied()
        .filter(|k| schema.get(*k).is_some())
        .collect();
    if present.is_empty() {
        return RootCombinatorOutcome::Unchanged;
    }

    let mut merged = Map::new();
    merge_properties(schema.get("properties"), &mut merged);
    for combinator in &present {
        let Some(branches) = schema.get(*combinator) else {
            unreachable!("combinator key was just confirmed present");
        };
        let Value::Array(branches) = branches else {
            return RootCombinatorOutcome::Drop(format!(
                "input schema has top-level {combinator} that is not an array"
            ));
        };
        for branch in branches {
            if is_object(branch) {
                let resolved = resolve_local_ref(branch, schema);
                merge_properties(resolved.get("properties"), &mut merged);
            }
        }
    }

    let mut required: Vec<String> = Vec::new();
    merge_required(schema.get("required"), &merged, &mut required);
    if let Some(Value::Array(all_of)) = schema.get("allOf") {
        for branch in all_of {
            if is_object(branch) {
                let resolved = resolve_local_ref(branch, schema);
                merge_required(resolved.get("required"), &merged, &mut required);
            }
        }
    }

    let has_any_or_one = present.contains(&"anyOf") || present.contains(&"oneOf");
    let mut flattened = Map::new();
    flattened.insert("type".to_string(), Value::String("object".to_string()));
    flattened.insert("properties".to_string(), Value::Object(merged));
    flattened.insert(
        "required".to_string(),
        Value::Array(required.iter().cloned().map(Value::String).collect()),
    );
    for key in CARRIED_ROOT_KEYS {
        if let Some(v) = schema.get(key) {
            flattened.insert(key.to_string(), v.clone());
        }
    }

    let note = build_note(&present, schema, has_any_or_one);
    RootCombinatorOutcome::Normalized {
        schema: Value::Object(flattened),
        note,
        combinators: present,
    }
}

/// oracle `Nr(e)` — the first top-level `properties` key (if any) that fails
/// the property-key regex.
fn first_invalid_property_key(schema: &Value) -> Option<String> {
    let Value::Object(map) = schema else {
        return None;
    };
    let Some(Value::Object(props)) = map.get("properties") else {
        return None;
    };
    for key in props.keys() {
        if !property_key_regex().is_match(key) {
            return Some(key.clone());
        }
    }
    None
}

/// oracle `qrt`/`Lr` — validate a schema document (as DATA) against the
/// JSON-Schema 2020-12 meta-schema, after the property-key check and the
/// `$schema`-mismatch skip. `Ok(())` = valid or skipped; `Err(detail)` names
/// the failure for the "would be rejected by the Anthropic API (<detail>)"
/// messages.
fn check_schema_validity(schema: &Value) -> Result<(), String> {
    if let Some(bad_key) = first_invalid_property_key(schema) {
        let truncated: String = bad_key.chars().take(80).collect();
        // oracle: `property key ${b(a.slice(0,80))} does not match ${O}` — `b`
        // is `JSON.stringify`, so the key is QUOTED AND ESCAPED (a key holding
        // a `"` or a backslash must not break out of the quotes), and `${O}`
        // renders the RegExp with its `/` delimiters.
        let quoted = Value::String(truncated).to_string();
        return Err(format!(
            "property key {quoted} does not match {PROPERTY_KEY_PATTERN_RENDERED}"
        ));
    }

    // oracle's shallow top-level null-value strip before validating.
    let instance = match schema {
        Value::Object(map) if map.values().any(Value::is_null) => Value::Object(
            map.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    };

    // oracle: a `$schema` other than exactly the 2020-12 URI (with/without a
    // trailing `#`), or present-but-not-a-string, skips validation entirely
    // (cross-draft support, `Ir()`, is permanently disabled in the oracle).
    if let Some(s) = instance.get("$schema") {
        let matches_2020_12 = matches!(
            s.as_str(),
            Some(v) if v == META_SCHEMA_URL || v == format!("{META_SCHEMA_URL}#")
        );
        if !matches_2020_12 {
            return Ok(());
        }
    }

    validate_against_meta_schema(&instance)
}

/// oracle `qr()` — the compiled 2020-12 meta-validator, built ONCE per process
/// and memoized in module state (`if(M===void 0){…}return M`). Recompiling it
/// per tool would re-run the whole draft-2020-12 vocabulary compile for every
/// entry of every `tools/list`.
///
/// `None` = validator unavailable; the oracle then warns once and fails OPEN.
fn meta_validator() -> Option<&'static (boon::Schemas, boon::SchemaIndex)> {
    use boon::{Compiler, SchemaIndex, Schemas};
    static META: std::sync::OnceLock<Option<(Schemas, SchemaIndex)>> = std::sync::OnceLock::new();
    META.get_or_init(|| {
        let mut schemas = Schemas::new();
        let mut compiler = Compiler::new();
        let Ok(sch) = compiler.compile(META_SCHEMA_URL, &mut schemas) else {
            // oracle `qr()`'s `M===null` arm, verbatim (@182172867):
            // `n("MCP: draft 2020-12 meta-validator unavailable — tool
            // schema checks fail open",{level:"warn"}),
            // s("tengu_mcp_degraded",{reason:w("schema_validator_unavailable")})`.
            tracing::warn!(
                "MCP: draft 2020-12 meta-validator unavailable \u{2014} tool schema checks fail open"
            );
            telemetry::emit_mcp_degraded(&telemetry::tengu::mcp::DegradedPayload {
                reason: telemetry::tengu::mcp::DegradedReason::SchemaValidatorUnavailable,
                transport_type: None,
                normalized_count: None,
                skipped_count: None,
                kept_count: None,
                mcp_server_name: None,
            });
            return None;
        };
        Some((schemas, sch))
    })
    .as_ref()
}

fn validate_against_meta_schema(instance: &Value) -> Result<(), String> {
    // oracle `Lr`: `if(e===null)return{valid:!0}` — an unavailable validator
    // fails OPEN. The bundled meta-schema always compiles in this port, but
    // keep the shape.
    let Some((schemas, sch)) = meta_validator() else {
        return Ok(());
    };
    match schemas.validate(instance, *sch) {
        Ok(()) => Ok(()),
        // oracle: `let o=e.errors?.[0]; …`schema${o.instancePath} ${o.message}``
        // — ONE line, anchored at the offending JSON pointer. `boon`'s
        // `Display` renders a multi-line indented CAUSE TREE instead, which
        // cannot be embedded in the single-sentence "would be rejected by the
        // Anthropic API (<detail>)" message, so take the first leaf cause (the
        // closest analogue of ajv's `allErrors:false` first error) and format
        // it the oracle's way.
        Err(e) => Err(first_leaf_detail(&e)),
    }
}

/// `schema<instancePath> <message>` from `boon`'s error tree — descend to the
/// first leaf cause, which carries the deepest `instance_location`.
fn first_leaf_detail(err: &boon::ValidationError<'_, '_>) -> String {
    let mut node = err;
    while let Some(first) = node.causes.first() {
        node = first;
    }
    let pointer = node.instance_location.to_string();
    format!("schema{pointer} {}", node.kind)
}

/// The decision for one tool's `inputSchema`, before it's added to the tool
/// list handed to the provider.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSchemaDecision {
    /// The (possibly normalized) schema to use if the tool is kept.
    pub schema: Value,
    /// Set when normalization applied — prepend to the tool description
    /// (oracle: `note\n\ndescription`, or bare `note` if the description was
    /// empty).
    pub description_note: Option<String>,
    /// `Some(reason)` — drop the tool entirely; `reason` is the oracle's
    /// "…: <reason>. Other tools from this server remain available." detail.
    pub drop_reason: Option<String>,
    /// `Some(detail)` — keep the tool, but the schema would be rejected by
    /// the Anthropic API; log a warning (oracle: "…would be rejected…;
    /// requests that include it may fail").
    pub warning: Option<String>,
    /// The tool's schema was flattened — the oracle's `x++`
    /// (`ToolSchemaNormalized`) counter.
    ///
    /// **This is INDEPENDENT of [`Self::classification`], because the oracle's
    /// two counters are independent.** `yn`'s flatMap body (@182317450) does
    ///
    /// ```text
    /// else if(pe.outcome==="normalized"&&U){ x++; …; ie={...E,inputSchema:pe.schema,…} }
    /// …
    /// let le=qrt(ie.inputSchema);
    /// if(le.valid)return[ie];
    /// if(!ee){ if(le.check==="meta")F++; else X++; …; return[ie] }
    /// if(le.check==="meta")_e++; else xe++;
    /// ```
    ///
    /// — the `x++` arm FALLS THROUGH into the validity check, so a tool that
    /// is normalized and then fails validation increments `x` *and* one of
    /// `_e`/`xe`/`F`/`X`, and the oracle fires TWO `tengu_mcp_degraded`
    /// events for it. Collapsing both into a single `classification` (as an
    /// earlier revision did) silently under-counted `normalizedCount` — to
    /// zero, on a server where every normalized tool also failed validity.
    pub normalized: bool,
    /// The TERMINAL `tengu_mcp_degraded` classification — the validity or
    /// drop outcome — or `None` for a schema-valid tool. Never
    /// `ToolSchemaNormalized`; that bucket is [`Self::normalized`], for the
    /// reason spelled out there. The caller aggregates both per server and
    /// fires one [`telemetry::tengu::mcp::DEGRADED`] event per nonzero
    /// bucket, NOT one per tool.
    pub classification: Option<telemetry::tengu::mcp::DegradedReason>,
}

/// Process one tool's `inputSchema` (oracle's `M.flatMap` body inside `yn`,
/// `cc_all.txt` @182315200-182320200): flatten a root combinator (gated),
/// then check schema validity (drop gated, else keep-with-warning).
///
/// `server_url` is the connected server's URL (`None` for `stdio`/url-less
/// transports) — feeds the per-server gate the same way
/// `protocol_negotiation.rs`'s denylist does. `tool_name` is used only in
/// the returned decision's log-worthy detail text (already embedded in
/// `drop_reason`/`warning` fully-formed by the caller's message, so this
/// function does not itself need it — see call sites for the "Skipping tool
/// …"/"Tool … input schema …" wrapper text).
#[must_use]
pub fn decide_tool_schema(server_url: Option<&str>, schema: &Value) -> ToolSchemaDecision {
    use telemetry::tengu::mcp::DegradedReason;

    let normalize_gate = gate_enabled(FLAG_NORMALIZE_ROOT_COMBINATORS, server_url);
    let drop_gate = gate_enabled(FLAG_DROP_INVALID_TOOL_SCHEMAS, server_url);

    let (working_schema, description_note) = match flatten_root_combinators(schema) {
        RootCombinatorOutcome::Unchanged => (schema.clone(), None),
        RootCombinatorOutcome::Normalized {
            schema: s, note, ..
        } if normalize_gate => (s, Some(note)),
        RootCombinatorOutcome::Normalized { combinators, .. } => {
            return ToolSchemaDecision {
                schema: schema.clone(),
                description_note: None,
                drop_reason: Some(format!(
                    "its input schema uses top-level {}, which the Anthropic API does not accept",
                    combinators.join("/")
                )),
                warning: None,
                normalized: false,
                classification: Some(DegradedReason::ToolSchemaNormalizeGated),
            };
        }
        RootCombinatorOutcome::Drop(reason) => {
            return ToolSchemaDecision {
                schema: schema.clone(),
                description_note: None,
                drop_reason: Some(reason),
                warning: None,
                normalized: false,
                classification: Some(DegradedReason::ToolSchemaUnsupported),
            };
        }
    };
    // Reaching here with `description_note.is_some()` means the flatten DID
    // apply (the `Normalized { .. } if normalize_gate` arm above) — the
    // oracle's `x++` counter (`ToolSchemaNormalized`) fires unconditionally
    // for that arm, before the schema-validity check even runs, and the arm
    // FALLS THROUGH into that check rather than returning. So `normalized`
    // rides out on EVERY arm below, including the two `Err` ones: see
    // `ToolSchemaDecision::normalized` for the oracle disassembly.
    let normalized = description_note.is_some();

    match check_schema_validity(&working_schema) {
        Ok(()) => ToolSchemaDecision {
            schema: working_schema,
            description_note,
            drop_reason: None,
            warning: None,
            normalized,
            classification: None,
        },
        Err(detail) => {
            // oracle: `le.check==="meta"` (the meta-schema validator failed)
            // vs. everything else (the property-key regex failed) — see
            // `check_schema_validity`'s early-return shape.
            let is_property_key = detail.starts_with("property key ");
            if drop_gate {
                ToolSchemaDecision {
                    schema: working_schema,
                    description_note,
                    drop_reason: Some(format!(
                        "its input schema would be rejected by the Anthropic API ({detail})"
                    )),
                    warning: None,
                    normalized,
                    classification: Some(if is_property_key {
                        DegradedReason::ToolPropertyKeyInvalid
                    } else {
                        DegradedReason::ToolSchemaInvalid
                    }),
                }
            } else {
                ToolSchemaDecision {
                    schema: working_schema,
                    description_note,
                    normalized,
                    classification: Some(if is_property_key {
                        DegradedReason::ToolPropertyKeyInvalidGated
                    } else {
                        DegradedReason::ToolSchemaInvalidGated
                    }),
                    drop_reason: None,
                    warning: Some(format!(
                        "input schema would be rejected by the Anthropic API ({detail}); requests that include it may fail"
                    )),
                }
            }
        }
    }
}

/// Serializes every test that writes the PROCESS-GLOBAL §20a feature flags.
/// `registry.rs`'s connect-path gate tests run in the SAME test binary as this
/// module's, so a registry test setting `tengu_mcp_normalize_root_combinators`
/// would otherwise race `gate_default_empty_is_always_off` here.
#[cfg(test)]
pub(crate) fn flag_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The oracle's `tengu_mcp_normalize_root_combinators` key, re-exported for the
/// `registry.rs` connect-path gate test so BOTH sides are never routed through
/// this module's private constant.
#[cfg(test)]
pub(crate) const ORACLE_NORMALIZE_FLAG_FOR_TEST: &str = "tengu_mcp_normalize_root_combinators";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        super::flag_test_lock()
    }

    fn clear_flags() {
        telemetry::test_clear_flag_list(FLAG_NORMALIZE_ROOT_COMBINATORS);
        telemetry::test_clear_flag_list(FLAG_DROP_INVALID_TOOL_SCHEMAS);
    }

    /// Every other gate test in this module writes the flag through
    /// [`FLAG_NORMALIZE_ROOT_COMBINATORS`]/[`FLAG_DROP_INVALID_TOOL_SCHEMAS`]
    /// and reads it back through the same constants, so both sides move
    /// together and a corrupted constant stays GREEN while `GrowthBook`'s real
    /// flag can never enable the transform in production. This test is the
    /// only place either literal is asserted; it is the same discipline
    /// `protocol_negotiation.rs`'s `ORACLE_DENYLIST_FLAG` records (where the
    /// shared-constant form was verified to leave the whole module green
    /// under a deliberately corrupted constant).
    ///
    /// Oracle: `Ot("tengu_mcp_normalize_root_combinators",…)` /
    /// `Ot("tengu_mcp_drop_invalid_tool_schemas",…)` — binary @182279468 and
    /// @182279535.
    #[test]
    fn gate_flag_names_are_the_oracle_literals() {
        assert_eq!(
            FLAG_NORMALIZE_ROOT_COMBINATORS,
            "tengu_mcp_normalize_root_combinators"
        );
        assert_eq!(
            FLAG_DROP_INVALID_TOOL_SCHEMAS,
            "tengu_mcp_drop_invalid_tool_schemas"
        );
    }

    /// Oracle `yn`'s `x++` arm FALLS THROUGH into the schema-validity check,
    /// so a tool that is normalized AND then fails validity increments TWO
    /// counters and fires TWO `tengu_mcp_degraded` events. An earlier
    /// revision collapsed both into one `Option<DegradedReason>`: the `Err`
    /// arms overwrote the classification with the invalid variant and dropped
    /// the normalize tally on the floor, so `tool_schema_normalized` never
    /// fired for such a server and `normalizedCount` silently under-reported
    /// (to ZERO, on a server where every normalized tool also failed).
    ///
    /// ## Why the co-occurrence is reached through a CARRIED root key
    ///
    /// The review that raised this proposed a flattened schema failing the
    /// PROPERTY-KEY regex. That repro is impossible — at this port and at the
    /// oracle alike. Both merges filter invalid keys out while merging:
    /// `merge_properties` tests `property_key_regex().is_match(k)`, and the
    /// oracle's `Wrt` (@182171131) does the same with
    /// `a=(f)=>{…if(O.test(m)&&!(m in r)&&He(_))r[m]=_}`. A flattened schema
    /// therefore never CONTAINS a bad property key, so
    /// `tool_schema_normalized` + `tool_property_key_invalid` cannot co-occur.
    ///
    /// The reachable co-occurrence is the META branch. `CARRIED_ROOT_KEYS`
    /// (oracle `rt`) copies `$defs` / `definitions` / `$schema` /
    /// `additionalProperties` / `description` / `title` from the ORIGINAL
    /// schema into the flattened one verbatim, unvalidated. A meta-invalid
    /// value among them survives the flatten and then fails `qrt` with
    /// `check === "meta"` — the oracle's `x++` AND `_e++` (drop gate on) or
    /// `x++` AND `F++` (gate off). `additionalProperties: 5` is such a value:
    /// the 2020-12 meta-schema requires a schema (object or boolean) there.
    #[test]
    fn a_normalized_tool_that_then_fails_meta_validity_reports_both_buckets() {
        let _g = lock();
        clear_flags();
        telemetry::test_set_flag_list(FLAG_NORMALIZE_ROOT_COMBINATORS, vec!["*".to_string()]);
        telemetry::test_set_flag_list(FLAG_DROP_INVALID_TOOL_SCHEMAS, vec!["*".to_string()]);

        let schema = json!({
            "anyOf": [{"type": "object", "properties": {"ok": {"type": "string"}}}],
            "additionalProperties": 5
        });
        let decision = decide_tool_schema(None, &schema);

        assert!(
            decision.normalized,
            "the flatten DID apply (oracle `x++`), so the normalize bucket must be tallied \
             even though the flattened schema then failed meta-validation"
        );
        assert_eq!(
            decision.classification,
            Some(telemetry::tengu::mcp::DegradedReason::ToolSchemaInvalid),
            "the terminal outcome is still the meta failure (oracle `_e++`, drop gate on)"
        );
        assert!(
            decision.drop_reason.is_some(),
            "drop gate is ON, so the tool is dropped"
        );
        clear_flags();
    }

    /// The same co-occurrence with the drop gate OFF: oracle `x++` AND `F++`
    /// (`tool_schema_invalid_gated`, `keptCount`).
    #[test]
    fn a_normalized_tool_kept_with_a_meta_warning_still_reports_the_normalize_bucket() {
        let _g = lock();
        clear_flags();
        telemetry::test_set_flag_list(FLAG_NORMALIZE_ROOT_COMBINATORS, vec!["*".to_string()]);

        let schema = json!({
            "anyOf": [{"type": "object", "properties": {"ok": {"type": "string"}}}],
            "additionalProperties": 5
        });
        let decision = decide_tool_schema(None, &schema);

        assert!(decision.normalized, "oracle `x++` still fires");
        assert_eq!(
            decision.classification,
            Some(telemetry::tengu::mcp::DegradedReason::ToolSchemaInvalidGated),
            "oracle `F++` — kept with a warning"
        );
        assert!(
            decision.drop_reason.is_none(),
            "drop gate is OFF, tool kept"
        );
        clear_flags();
    }

    /// The companion guard: `classification` must never carry
    /// `ToolSchemaNormalized` any more (that bucket moved to `normalized`),
    /// or the caller would double-count a normalized-and-valid tool.
    #[test]
    fn a_normalized_but_valid_tool_reports_only_the_normalize_bucket() {
        let _g = lock();
        clear_flags();
        telemetry::test_set_flag_list(FLAG_NORMALIZE_ROOT_COMBINATORS, vec!["*".to_string()]);

        let schema = json!({
            "anyOf": [{"type": "object", "properties": {"ok": {"type": "string"}}}]
        });
        let decision = decide_tool_schema(None, &schema);

        assert!(decision.normalized, "the flatten applied");
        assert_eq!(
            decision.classification, None,
            "a schema-valid tool has no TERMINAL classification; the normalize tally lives \
             on `normalized` alone so the caller cannot count it twice"
        );
        assert!(decision.drop_reason.is_none());
        clear_flags();
    }

    /// The gate must resolve through the REAL flag reader under the oracle's
    /// literal key, not merely through whatever `FLAG_*` happens to hold: a
    /// misspelled constant here would leave every other gate test green while
    /// the shipped binary's flag never reached `gate_enabled`.
    #[test]
    fn gate_reads_the_oracle_literal_key_end_to_end() {
        const ORACLE_NORMALIZE_FLAG: &str = "tengu_mcp_normalize_root_combinators";
        let _g = lock();
        clear_flags();
        telemetry::test_clear_flag_list(ORACLE_NORMALIZE_FLAG);
        let schema =
            json!({"anyOf": [{"type": "object", "properties": {"a": {"type": "string"}}}]});
        assert!(
            decide_tool_schema(None, &schema).drop_reason.is_some(),
            "baseline: with the oracle flag unset the combinator schema is dropped"
        );
        telemetry::test_set_flag_list(ORACLE_NORMALIZE_FLAG, vec!["*".to_string()]);
        assert!(
            decide_tool_schema(None, &schema).drop_reason.is_none(),
            "setting the ORACLE's literal flag key must enable normalization"
        );
        telemetry::test_clear_flag_list(ORACLE_NORMALIZE_FLAG);
        clear_flags();
    }

    // ── gate_enabled ─────────────────────────────────────────────────────

    #[test]
    fn gate_default_empty_is_always_off() {
        let _g = lock();
        clear_flags();
        assert!(!gate_enabled(
            FLAG_NORMALIZE_ROOT_COMBINATORS,
            Some("https://x.example.com/mcp")
        ));
        assert!(!gate_enabled(FLAG_NORMALIZE_ROOT_COMBINATORS, None));
    }

    #[test]
    fn gate_wildcard_enables_url_less_server_too() {
        let _g = lock();
        clear_flags();
        telemetry::test_set_flag_list(FLAG_NORMALIZE_ROOT_COMBINATORS, vec!["*".to_string()]);
        assert!(gate_enabled(FLAG_NORMALIZE_ROOT_COMBINATORS, None));
        clear_flags();
    }

    #[test]
    fn gate_hostname_suffix_matches_case_insensitively() {
        let _g = lock();
        clear_flags();
        telemetry::test_set_flag_list(
            FLAG_NORMALIZE_ROOT_COMBINATORS,
            vec!["Example.com".to_string()],
        );
        assert!(gate_enabled(
            FLAG_NORMALIZE_ROOT_COMBINATORS,
            Some("https://sub.EXAMPLE.com/mcp")
        ));
        assert!(!gate_enabled(
            FLAG_NORMALIZE_ROOT_COMBINATORS,
            Some("https://other.org/mcp")
        ));
        clear_flags();
    }

    // ── flatten_root_combinators ─────────────────────────────────────────

    #[test]
    fn unchanged_when_no_root_combinator() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "string"}}});
        assert_eq!(
            flatten_root_combinators(&schema),
            RootCombinatorOutcome::Unchanged
        );
    }

    #[test]
    fn allof_merges_properties_and_intersects_required() {
        let schema = json!({
            "allOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]},
                {"type": "object", "properties": {"b": {"type": "number"}}}
            ]
        });
        let RootCombinatorOutcome::Normalized {
            schema: flat,
            note,
            combinators,
        } = flatten_root_combinators(&schema)
        else {
            panic!("expected Normalized");
        };
        assert_eq!(combinators, vec!["allOf"]);
        assert_eq!(flat["type"], json!("object"));
        assert_eq!(flat["properties"]["a"]["type"], json!("string"));
        assert_eq!(flat["properties"]["b"]["type"], json!("number"));
        assert_eq!(flat["required"], json!(["a"]));
        assert!(note.contains("allOf"));
    }

    #[test]
    fn anyof_note_lists_each_branchs_parameter_group() {
        let schema = json!({
            "anyOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}},
                {"type": "object", "properties": {"b": {"type": "string"}}}
            ]
        });
        let RootCombinatorOutcome::Normalized { note, .. } = flatten_root_combinators(&schema)
        else {
            panic!("expected Normalized");
        };
        assert_eq!(
            note,
            "Input constraint: Provide parameters for at least one of: (a) or (b)."
        );
    }

    #[test]
    fn oneof_uses_exactly_one_wording() {
        let schema = json!({
            "oneOf": [
                {"type": "object", "required": ["a"]},
                {"type": "object", "required": ["b"]}
            ],
            "properties": {"a": {"type": "string"}, "b": {"type": "string"}}
        });
        let RootCombinatorOutcome::Normalized { note, .. } = flatten_root_combinators(&schema)
        else {
            panic!("expected Normalized");
        };
        assert!(note.starts_with("Input constraint: Provide parameters for exactly one of:"));
    }

    #[test]
    fn non_array_combinator_value_is_reported_unsupported() {
        let schema = json!({"anyOf": "not-an-array"});
        assert_eq!(
            flatten_root_combinators(&schema),
            RootCombinatorOutcome::Drop(
                "input schema has top-level anyOf that is not an array".to_string()
            )
        );
    }

    #[test]
    fn ref_branches_are_resolved_against_defs() {
        let schema = json!({
            "$defs": {"Foo": {"type": "object", "properties": {"z": {"type": "boolean"}}}},
            "anyOf": [{"$ref": "#/$defs/Foo"}]
        });
        let RootCombinatorOutcome::Normalized { schema: flat, .. } =
            flatten_root_combinators(&schema)
        else {
            panic!("expected Normalized");
        };
        assert_eq!(flat["properties"]["z"]["type"], json!("boolean"));
    }

    // ── check_schema_validity ────────────────────────────────────────────

    #[test]
    fn valid_object_schema_passes() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}}
        });
        assert_eq!(check_schema_validity(&schema), Ok(()));
    }

    #[test]
    fn invalid_property_key_is_reported_before_meta_validation() {
        let schema = json!({
            "type": "object",
            "properties": {"bad key!": {"type": "string"}}
        });
        let err = check_schema_validity(&schema).unwrap_err();
        assert!(err.contains("bad key!"), "{err}");
    }

    #[test]
    fn meta_invalid_type_keyword_is_rejected() {
        // "type" must be a string or array of strings per the 2020-12
        // meta-schema, not a number.
        let schema = json!({"type": 5});
        assert!(check_schema_validity(&schema).is_err());
    }

    #[test]
    fn mismatched_schema_dialect_skips_validation() {
        // A draft-07 `$schema` on an otherwise meta-invalid document is
        // treated as valid — the oracle's cross-draft path is dead code.
        let schema = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": 5});
        assert_eq!(check_schema_validity(&schema), Ok(()));
    }

    #[test]
    fn matching_2020_12_schema_dialect_is_actually_checked() {
        let schema = json!({"$schema": META_SCHEMA_URL, "type": 5});
        assert!(check_schema_validity(&schema).is_err());
    }

    // ── decide_tool_schema (end-to-end) ──────────────────────────────────

    #[test]
    fn combinator_schema_is_dropped_by_default_gate_off() {
        let _g = lock();
        clear_flags();
        let schema =
            json!({"anyOf": [{"type": "object", "properties": {"a": {"type": "string"}}}]});
        let d = decide_tool_schema(None, &schema);
        assert!(d.drop_reason.is_some(), "{d:?}");
        assert!(d.drop_reason.unwrap().contains("anyOf"));
    }

    #[test]
    fn combinator_schema_is_normalized_and_kept_when_gate_on() {
        let _g = lock();
        clear_flags();
        telemetry::test_set_flag_list(FLAG_NORMALIZE_ROOT_COMBINATORS, vec!["*".to_string()]);
        let schema =
            json!({"anyOf": [{"type": "object", "properties": {"a": {"type": "string"}}}]});
        let d = decide_tool_schema(None, &schema);
        assert!(d.drop_reason.is_none(), "{d:?}");
        assert!(d.description_note.is_some());
        assert_eq!(d.schema["type"], json!("object"));
        clear_flags();
    }

    #[test]
    fn invalid_schema_is_kept_with_warning_by_default_gate_off() {
        let _g = lock();
        clear_flags();
        let schema = json!({"type": 5});
        let d = decide_tool_schema(None, &schema);
        assert!(d.drop_reason.is_none(), "{d:?}");
        assert!(d.warning.is_some());
    }

    #[test]
    fn invalid_schema_is_dropped_when_drop_gate_on() {
        let _g = lock();
        clear_flags();
        telemetry::test_set_flag_list(FLAG_DROP_INVALID_TOOL_SCHEMAS, vec!["*".to_string()]);
        let schema = json!({"type": 5});
        let d = decide_tool_schema(None, &schema);
        assert!(d.drop_reason.is_some(), "{d:?}");
        clear_flags();
    }

    /// oracle `Lr`: ``detail: `property key ${b(a.slice(0,80))} does not match ${O}` ``
    /// — `${O}` stringifies the `RegExp` WITH its `/` delimiters and `b` is
    /// `JSON.stringify`, so a key holding a `"` is escaped rather than
    /// breaking out of the quotes. This detail is embedded verbatim in the
    /// user-visible drop / warn copy.
    #[test]
    fn property_key_detail_carries_the_regex_delimiters_and_escapes_the_key() {
        let _g = lock();
        clear_flags();
        let schema = json!({"type": "object", "properties": {"bad key!": {"type": "string"}}});
        let d = decide_tool_schema(None, &schema);
        let warning = d.warning.expect("kept with a warning");
        assert!(
            warning.contains("does not match /^[a-zA-Z0-9_.-]{1,64}$/"),
            "the regex must render with its delimiters, got: {warning}"
        );
        assert!(
            warning.contains(r#"property key "bad key!""#),
            "the key must be JSON-quoted, got: {warning}"
        );

        let schema = json!({"type": "object", "properties": {"a\"b!": {"type": "string"}}});
        let warning = decide_tool_schema(None, &schema)
            .warning
            .expect("kept with a warning");
        assert!(
            warning.contains(r#"property key "a\"b!""#),
            "a quote inside the key must be JSON-escaped, got: {warning}"
        );
    }

    /// oracle `Lr`'s meta arm: ``detail: `schema${o.instancePath} ${o.message}` ``
    /// — a ONE-LINE detail anchored at the offending JSON pointer. `boon`'s
    /// `ValidationError` `Display` is a multi-line indented tree, which cannot
    /// be embedded in the single-sentence "would be rejected by the Anthropic
    /// API (<detail>)" copy.
    #[test]
    fn meta_failure_detail_is_one_line_anchored_at_the_instance_pointer() {
        let _g = lock();
        clear_flags();
        let d = decide_tool_schema(None, &json!({"type": 5}));
        let warning = d.warning.expect("kept with a warning");
        assert!(
            !warning.contains('\n'),
            "the detail must be a single line, got: {warning:?}"
        );
        assert!(
            warning.contains("schema/type "),
            "the detail must be anchored at the /type instance pointer, got: {warning:?}"
        );
    }

    #[test]
    fn valid_plain_schema_is_kept_unchanged() {
        let _g = lock();
        clear_flags();
        let schema = json!({"type": "object", "properties": {"a": {"type": "string"}}});
        let d = decide_tool_schema(None, &schema);
        assert_eq!(d.schema, schema);
        assert!(d.drop_reason.is_none());
        assert!(d.warning.is_none());
        assert!(d.description_note.is_none());
    }
}
