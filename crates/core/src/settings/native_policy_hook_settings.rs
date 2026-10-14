//! Acceptance-only source port of Native 2.1.295 managed-settings hook
//! normalization (`At → u_ → p_`) and the nested `Due.hooks` schema.
//!
//! This draft is deliberately not registered in a production crate and was
//! not Cargo-compiled. Source identity is fixed to the extracted binary with
//! SHA-256 `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.
//! Main chunk `src_187673145.js` is binary offset 187673145, 434959 bytes,
//! SHA-256 `6fbf3e921382e88e71bd834f2034ee24a15cada553c00e95181e44fa033d22aa`.
//!
//! `fTr` is local 371765 / binary 188044910; `At` is local 387398 / binary
//! 188060543; `u_` is local 374361 / binary 188047506; `p_` is local 374665 /
//! binary 188047810. `fTr` passes the whole cloned settings object and its
//! source label to `At`, then checks only `strictPluginOnlyCustomization`,
//! `allowedProviders`, and finally `Due(s).safeParse(s)`. A `fatal` row from
//! `u_` is a warning row returned by `fTr`; severity by itself does not reject
//! the payload. The retained guard-bearing shapes may still fail nested Due.
//!
//! The nested schema is source-bound in the same chunk: `Ap()` declares the
//! command, prompt, agent, HTTP, and MCP-tool hook unions at local 100833;
//! `G$e()` builds their `type` discriminated union; `tn()` declares matcher
//! objects; `QQ()` is the event-key record consumed by the `hooks` field in
//! `Aa()` at local 228246. Event names and warning rows are source ordered.
//!
//! The imported `df` display formatter is bound to `src_186231195.js` at
//! binary offset 186231195, 7300 bytes, SHA-256
//! `125f57566b99b67ce958d77cd787163124654cbbb3bdd6f3a92dfd38955a2317`;
//! its `kV` dependency applies Unicode 16 grapheme-aware filtering. ASCII
//! controls and unconditional escaped code points are ported. A dynamic event
//! or hook-type string containing a grapheme-context-sensitive variation
//! selector, ZWJ, or emoji tag is kept but marks the result unresolved rather
//! than guessing at `Intl.Segmenter` behavior.

use super::native_policy_js_semantics::{
    NativePolicyJsSemanticsRegistry, NativePolicyZodUrlOutcome,
};
use crate::types::utf16_json::{JsonObjectEntryView, JsonValueView, Utf16JsonProjection};

const DOC_LINK: &str = "https://code.claude.com/docs/en/hooks";
const UNLOADABLE_GUARD: &str = "a PreToolUse/PermissionRequest hook that cannot be loaded may be what guards the permissions declared beside it, so nothing it sits in is applied until the entry is fixed or removed";
const VALID_TYPES_TEXT: &str = "command, prompt, agent, http, mcp_tool";
const INVALID_DISCRIMINATOR: &str =
    "Invalid discriminator value. Expected 'command' | 'prompt' | 'agent' | 'http' | 'mcp_tool'";
const HOOK_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PostToolBatch",
    "Notification",
    "UserPromptSubmit",
    "UserPromptExpansion",
    "SessionStart",
    "SessionEnd",
    "Stop",
    "StopFailure",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "PreModelSwitch",
    "PostModelSwitch",
    "PermissionRequest",
    "PermissionDenied",
    "Setup",
    "TeammateIdle",
    "TaskCreated",
    "TaskCompleted",
    "Elicitation",
    "ElicitationResult",
    "ConfigChange",
    "WorktreeCreate",
    "WorktreeRemove",
    "InstructionsLoaded",
    "CwdChanged",
    "FileChanged",
    "DirectoryAdded",
    "MessageDisplay",
];
const GUARDED_EVENTS: &[&str] = &["PreToolUse", "PermissionRequest"];
const SCANNED_TOP_LEVEL_KEYS: &[&str] = &[
    "mcpServers",
    "managedMcpServers",
    "lspServers",
    "pluginConfigs",
    "enabledPlugins",
    "extraKnownMarketplaces",
    "env",
    "skillOverrides",
    "modelSettings",
];
const HOOK_TYPES: &[&str] = &["command", "prompt", "agent", "http", "mcp_tool"];
const COMMAND_SHELLS: &[&str] = &["bash", "powershell"];
const FAILURE_ACTIONS: &[&str] = &["continue", "block"];
const CLOUD_ACTIONS: &[&str] = &["device", "skip"];

/// The `file` label carried through `At` into each warning row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeHookSourceContext {
    pub file: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeHookDiagnosticSeverity {
    Fatal,
    Warning,
}

/// Convenience fields mirror the exact `raw_warning` projection. `raw_value`
/// is the warning's `invalidValue` string when that own field was present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeHookSettingsDiagnostic {
    pub path: Utf16JsonProjection,
    pub message: Utf16JsonProjection,
    pub severity: Option<NativeHookDiagnosticSeverity>,
    pub raw_value: Option<Utf16JsonProjection>,
    pub raw_warning: Utf16JsonProjection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeHookSettingsResult {
    /// The post-At `/hooks` value. `None` means it was absent or removed.
    pub normalized: Option<Utf16JsonProjection>,
    /// Ordered as Native `u_` and `p_` append their rows.
    pub diagnostics: Vec<NativeHookSettingsDiagnostic>,
    /// A field must not be admitted when exact source display formatting or
    /// projection construction could not be represented by this port.
    pub unresolved: bool,
}

/// Exact JavaScript inputs reached by the fixed `u_` hook normalization.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NativeHookSemanticInputs {
    pub url_inputs: Vec<Vec<u16>>,
    pub display_inputs: Vec<Vec<u16>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeHookDueIssue {
    /// Full `fTr` path, including the outer `hooks` field.
    pub path: Utf16JsonProjection,
    pub message: Utf16JsonProjection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeHookDueResult {
    /// The `QQ()` parsed output, with unknown matcher/hook keys stripped.
    /// `None` means Due rejected the value or an unresolved parser boundary.
    pub normalized: Option<Utf16JsonProjection>,
    /// First three nested hooks issues in the order returned by Due.
    pub issues: Vec<NativeHookDueIssue>,
    pub unresolved: bool,
}

/// Port Native `u_(root, context.file)` plus `p_(root, context.file)`.
/// Pass the full settings root because `G8` searches for guard-bearing hooks
/// outside the actual `hooks` field. Call at the same position in `At` after
/// `d_`; this function only owns and returns the resulting `/hooks` field.
#[must_use]
pub(crate) fn normalize_native_hook_settings(
    root: &Utf16JsonProjection,
    context: &NativeHookSourceContext,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> NativeHookSettingsResult {
    let Some(root_view) = root.at_pointer("") else {
        return hook_unresolved(None);
    };
    if root_view.object_entries().is_none() {
        return NativeHookSettingsResult {
            normalized: None,
            diagnostics: Vec::new(),
            unresolved: false,
        };
    }

    let mut diagnostics = Vec::new();
    let mut unresolved = false;
    if misplaced_guard_hooks(root_view) {
        emit_warning(
            &mut diagnostics,
            &mut unresolved,
            context,
            ascii_units("hooks"),
            ascii_units(&format!(
                "PreToolUse/PermissionRequest hooks are declared outside \"hooks\" (at the top level or under another key) — {UNLOADABLE_GUARD}."
            )),
            Some(NativeHookDiagnosticSeverity::Fatal),
            None,
            false,
        );
    }

    let Ok(hooks) = root.subprojection("/hooks") else {
        return NativeHookSettingsResult {
            normalized: None,
            diagnostics,
            unresolved,
        };
    };
    let Some(hooks_view) = hooks.at_pointer("") else {
        return hook_unresolved_with(diagnostics);
    };

    if hooks_view.is_null() || hooks_view.object_entries().is_none() {
        let received = js_type(hooks_view);
        let invalid_array_has_object = hooks_view.array_len().is_some()
            && (array_has_object(&hooks, hooks_view) || array_contains_guard(&hooks, hooks_view));
        if invalid_array_has_object {
            let message = format!(
                "\"hooks\" must be an object mapping event names to matcher arrays; received {received} — {UNLOADABLE_GUARD}."
            );
            emit_warning(
                &mut diagnostics,
                &mut unresolved,
                context,
                ascii_units("hooks"),
                ascii_units(&message),
                None,
                Some(string_projection(&received)),
                false,
            );
            return NativeHookSettingsResult {
                normalized: Some(hooks),
                diagnostics,
                unresolved,
            };
        }

        let message = format!(
            "\"hooks\" must be an object mapping event names to matcher arrays; received {received}. This field was ignored."
        );
        emit_warning(
            &mut diagnostics,
            &mut unresolved,
            context,
            ascii_units("hooks"),
            ascii_units(&message),
            Some(NativeHookDiagnosticSeverity::Warning),
            Some(string_projection(&received)),
            false,
        );
        return NativeHookSettingsResult {
            normalized: None,
            diagnostics,
            unresolved,
        };
    }

    if is_single_matcher(hooks_view) {
        let message = format!(
            "\"hooks\" must be an object mapping event names to matcher arrays; received a single matcher — {UNLOADABLE_GUARD}."
        );
        emit_warning(
            &mut diagnostics,
            &mut unresolved,
            context,
            ascii_units("hooks"),
            ascii_units(&message),
            None,
            None,
            false,
        );
        return NativeHookSettingsResult {
            normalized: Some(hooks),
            diagnostics,
            unresolved,
        };
    }

    let result = normalize_event_map_at(&hooks, context, &mut diagnostics, unresolved, semantics);
    result
}

/// Parse the nested `Due` field built as `QQ() = record(enum(Dp), array(tn()))`.
/// This does not rerun `At`; its input is exactly the field projection returned
/// by `normalize_native_hook_settings`.
#[must_use]
pub(crate) fn validate_native_hooks_due(
    hooks: &Utf16JsonProjection,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> NativeHookDueResult {
    let Some(root) = hooks.at_pointer("") else {
        return due_unresolved();
    };
    let mut parser = DueParser {
        issues: Vec::new(),
        unresolved: false,
        js_semantics: semantics,
    };
    let normalized = parser.parse_event_map(hooks, root);
    if parser.unresolved {
        return NativeHookDueResult {
            normalized: None,
            issues: take_due_issues(parser.issues),
            unresolved: true,
        };
    }
    if parser.issues.is_empty() {
        NativeHookDueResult {
            normalized,
            issues: Vec::new(),
            unresolved: false,
        }
    } else {
        NativeHookDueResult {
            normalized: None,
            issues: take_due_issues(parser.issues),
            unresolved: false,
        }
    }
}

fn normalize_event_map_at(
    hooks: &Utf16JsonProjection,
    context: &NativeHookSourceContext,
    diagnostics: &mut Vec<NativeHookSettingsDiagnostic>,
    mut unresolved: bool,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> NativeHookSettingsResult {
    let Some(root) = hooks.at_pointer("") else {
        return hook_unresolved_with(std::mem::take(diagnostics));
    };
    let Some(entries) = js_ordered_entries(root) else {
        return hook_unresolved_with(std::mem::take(diagnostics));
    };

    let mut retained = std::collections::HashMap::<Vec<u16>, Utf16JsonProjection>::new();
    let mut event_issues = 0usize;
    let mut pending_arrays = Vec::<(Vec<u16>, Utf16JsonProjection)>::new();
    for entry in &entries {
        let event_units = entry.code_units;
        let known = is_hook_event(event_units);
        let Ok(event_value) = hooks.subprojection_property_units(event_units) else {
            unresolved = true;
            continue;
        };
        let event_view = match event_value.at_pointer("") {
            Some(view) => view,
            None => {
                unresolved = true;
                continue;
            }
        };

        if !known {
            if contains_guard_hooks(event_view, 3, !event_view.array_len().is_some(), &[], false) {
                event_issues += 1;
                if let Some(display_event) = native_display_units(event_units, semantics) {
                    let mut path = ascii_units("hooks.");
                    path.extend_from_slice(&display_event);
                    let mut message = ascii_units("\"");
                    message.extend_from_slice(&display_event);
                    message.extend(ascii_units("\" is not a hook event, but it holds PreToolUse/PermissionRequest hooks — "));
                    message.extend(ascii_units(UNLOADABLE_GUARD));
                    message.push(u16::from(b'.'));
                    emit_warning(
                        diagnostics,
                        &mut unresolved,
                        context,
                        path,
                        message,
                        None,
                        None,
                        false,
                    );
                } else {
                    unresolved = true;
                }
                if event_view.array_len().is_some() {
                    pending_arrays.push((event_units.to_vec(), event_value.clone()));
                }
                retained.insert(event_units.to_vec(), event_value);
            } else {
                event_issues += 1;
                if let Some(display_event) = native_display_units(event_units, semantics) {
                    let mut path = ascii_units("hooks.");
                    path.extend_from_slice(&display_event);
                    let mut message = ascii_units("Unknown hook event \"");
                    message.extend_from_slice(&display_event);
                    message.extend(ascii_units("\" was ignored. Valid events: "));
                    message.extend(ascii_units(&HOOK_EVENTS.join(", ")));
                    emit_warning(
                        diagnostics,
                        &mut unresolved,
                        context,
                        path,
                        message,
                        Some(NativeHookDiagnosticSeverity::Warning),
                        Some(string_projection_units(&display_event)),
                        true,
                    );
                } else {
                    unresolved = true;
                }
            }
            continue;
        }

        if event_view.array_len().is_none() {
            let invalid_guard = is_guarded_event(event_units) && !event_view.is_null()
                || contains_guard_hooks(event_view, 3, false, &[], false);
            if invalid_guard {
                event_issues += 1;
                let message = format!(
                    "Hook event \"{}\" must be an array of matchers; received {} — {UNLOADABLE_GUARD}.",
                    String::from_utf16_lossy(event_units),
                    js_type(event_view)
                );
                emit_warning(
                    diagnostics,
                    &mut unresolved,
                    context,
                    hook_event_path(event_units),
                    ascii_units(&message),
                    None,
                    Some(string_projection(&js_type(event_view))),
                    false,
                );
                retained.insert(event_units.to_vec(), event_value);
            } else {
                event_issues += 1;
                let received = js_type(event_view);
                let message = format!(
                    "Hook event \"{}\" must be an array of matchers; received {received}. This entry was ignored.",
                    String::from_utf16_lossy(event_units)
                );
                emit_warning(
                    diagnostics,
                    &mut unresolved,
                    context,
                    hook_event_path(event_units),
                    ascii_units(&message),
                    Some(NativeHookDiagnosticSeverity::Warning),
                    Some(string_projection(&received)),
                    !event_view.is_null(),
                );
            }
            continue;
        }

        retained.insert(event_units.to_vec(), event_value.clone());
        if event_view.array_len().is_some() {
            pending_arrays.push((event_units.to_vec(), event_value));
        }
    }

    // p_ first handles every unknown/non-array event root, then iterates the
    // remaining Object.entries(s) to inspect matcher arrays. Keep this second
    // pass separate so an early matcher warning never precedes a later root
    // event warning.
    for (event_units, event_value) in pending_arrays {
        if let Some(normalized_event) = normalize_matcher_list_at(
            &event_value,
            &event_units,
            context,
            diagnostics,
            &mut unresolved,
            &mut event_issues,
            semantics,
        ) {
            retained.insert(event_units, normalized_event);
        }
    }

    if event_issues > 0 && retained.is_empty() {
        // Native `p_` removes invalid event keys from `hooks`, but keeps the
        // object itself. When every event is removed, the source result is `{}`.
        return NativeHookSettingsResult {
            normalized: Some(
                Utf16JsonProjection::object_units(Vec::new())
                    .expect("empty Native hook event map is representable"),
            ),
            diagnostics: std::mem::take(diagnostics),
            unresolved,
        };
    }

    let mut output = Vec::with_capacity(retained.len());
    for entry in root.object_entries().unwrap_or_default() {
        if let Some(value) = retained.remove(entry.code_units) {
            output.push((entry.code_units.to_vec(), value));
        }
    }
    match Utf16JsonProjection::object_units(output) {
        Ok(normalized) => NativeHookSettingsResult {
            normalized: Some(normalized),
            diagnostics: std::mem::take(diagnostics),
            unresolved,
        },
        Err(_) => NativeHookSettingsResult {
            normalized: Some(hooks.clone()),
            diagnostics: std::mem::take(diagnostics),
            unresolved: true,
        },
    }
}

fn normalize_matcher_list_at(
    list: &Utf16JsonProjection,
    event_units: &[u16],
    context: &NativeHookSourceContext,
    diagnostics: &mut Vec<NativeHookSettingsDiagnostic>,
    unresolved: &mut bool,
    event_issues: &mut usize,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> Option<Utf16JsonProjection> {
    let Some(root) = list.at_pointer("") else {
        *unresolved = true;
        return Some(list.clone());
    };
    let Some(length) = root.array_len() else {
        *unresolved = true;
        return Some(list.clone());
    };
    let protected = is_guarded_event(event_units);
    let mut keep = vec![true; length];
    let mut retained = vec![None; length];
    let mut stripped = Vec::new();
    let mut unloadable = Vec::new();

    for matcher_index in (0..length).rev() {
        let Ok(matcher) = list.subprojection(&format!("/{matcher_index}")) else {
            *unresolved = true;
            continue;
        };
        let Some(value) = matcher.at_pointer("") else {
            *unresolved = true;
            continue;
        };

        if contains_guard_hooks(value, 3, false, &[], false) {
            let issue = OnIssue {
                path: vec![PathPart::Index(matcher_index)],
                problem: ascii_units(
                    "holds PreToolUse/PermissionRequest hooks where a matcher was expected",
                ),
                received: ascii_units(&ze_type(value)),
                about_type: false,
                hook_index: None,
            };
            unloadable.push(issue);
            retained[matcher_index] = Some(matcher);
            continue;
        }

        if value.object_entries().is_none() {
            let received = ze_type(value);
            let problem = format!("Hook matcher must be an object; received {received}");
            if !protected {
                keep[matcher_index] = false;
            }
            stripped.push(OnIssue {
                path: vec![PathPart::Index(matcher_index)],
                problem: ascii_units(&problem),
                received: ascii_units(&received),
                about_type: false,
                hook_index: None,
            });
            retained[matcher_index] = Some(matcher);
            continue;
        }

        let matcher_hooks = value.get_property("hooks");
        let Some(matcher_hooks) = matcher_hooks else {
            let received = "undefined";
            let problem = format!(
                "Hook matcher \"hooks\" must be an array of hook entries; received {received}"
            );
            if !protected {
                keep[matcher_index] = false;
            }
            stripped.push(OnIssue {
                path: vec![PathPart::Index(matcher_index)],
                problem: ascii_units(&problem),
                received: ascii_units(received),
                about_type: false,
                hook_index: None,
            });
            retained[matcher_index] = Some(matcher);
            continue;
        };
        let Some(matcher_hooks_length) = matcher_hooks.array_len() else {
            let received = ze_type(matcher_hooks);
            let problem = format!(
                "Hook matcher \"hooks\" must be an array of hook entries; received {received}"
            );
            if !protected {
                keep[matcher_index] = false;
            }
            stripped.push(OnIssue {
                path: vec![PathPart::Index(matcher_index)],
                problem: ascii_units(&problem),
                received: ascii_units(&received),
                about_type: false,
                hook_index: None,
            });
            retained[matcher_index] = Some(matcher);
            continue;
        };

        let mut kept_hook_rows = vec![true; matcher_hooks_length];
        let mut hook_issues = Vec::new();
        for hook_index in (0..matcher_hooks_length).rev() {
            let Ok(hook) = matcher.subprojection(&format!("/hooks/{hook_index}")) else {
                *unresolved = true;
                continue;
            };
            match validate_hook_entry_due(&hook, semantics) {
                HookEntryCheck::Valid => {}
                HookEntryCheck::Invalid(problem) => {
                    if !protected {
                        kept_hook_rows[hook_index] = false;
                    }
                    hook_issues.push(OnIssue {
                        path: vec![
                            PathPart::Index(matcher_index),
                            PathPart::Key(ascii_units("hooks")),
                            PathPart::Index(hook_index),
                        ],
                        problem: problem.problem,
                        received: problem.received,
                        about_type: problem.about_type,
                        hook_index: Some(hook_index),
                    });
                }
                HookEntryCheck::Unresolved => {
                    *unresolved = true;
                    if !protected {
                        kept_hook_rows[hook_index] = false;
                    }
                }
            }
        }

        let filtered_hooks = match rebuild_array(&matcher, "hooks", &kept_hook_rows) {
            Ok(value) => value,
            Err(_) => {
                *unresolved = true;
                matcher.clone()
            }
        };
        let schema_input = if protected {
            match replace_property(&filtered_hooks, "hooks", Some(empty_array())) {
                Ok(value) => value,
                Err(_) => {
                    *unresolved = true;
                    filtered_hooks.clone()
                }
            }
        } else {
            filtered_hooks.clone()
        };
        let (schema_issues, schema_unresolved) =
            validate_matcher_due_for_at(&schema_input, semantics);
        *unresolved |= schema_unresolved;
        if !schema_issues.is_empty() {
            // Native `on()` uses `vo(error)`, which maps and joins every Zod
            // issue. The outer `fTr` Due error path alone truncates to three.
            let problem = format_schema_issues(&schema_issues);
            if !protected {
                keep[matcher_index] = false;
            }
            let issue = OnIssue {
                path: vec![PathPart::Index(matcher_index)],
                problem: concat_units(&[
                    &ascii_units("Invalid hook matcher ("),
                    &problem,
                    &ascii_units(")"),
                ]),
                received: ascii_units("matcher"),
                about_type: false,
                hook_index: None,
            };
            if protected {
                unloadable.push(issue);
            } else {
                stripped.push(issue);
            }
            retained[matcher_index] = Some(matcher);
            continue;
        }

        if protected {
            unloadable.extend(hook_issues);
        } else {
            stripped.extend(hook_issues);
        }
        retained[matcher_index] = Some(filtered_hooks);
    }

    stripped.reverse();
    unloadable.reverse();
    for issue in &unloadable {
        *event_issues += 1;
        let Some(path) = on_issue_path(event_units, issue, issue.about_type) else {
            *unresolved = true;
            continue;
        };
        let mut message = issue.problem.clone();
        message.extend(ascii_units(" — "));
        message.extend(ascii_units(UNLOADABLE_GUARD));
        if issue.about_type {
            message.extend(ascii_units(". Valid types: "));
            message.extend(ascii_units(VALID_TYPES_TEXT));
        }
        emit_warning(
            diagnostics,
            unresolved,
            context,
            path,
            message,
            Some(NativeHookDiagnosticSeverity::Fatal),
            Some(string_projection_units(&issue.received)),
            false,
        );
    }
    for issue in &stripped {
        *event_issues += 1;
        let Some(path) = on_issue_path(event_units, issue, issue.about_type) else {
            *unresolved = true;
            continue;
        };
        let mut message = issue.problem.clone();
        if issue.about_type {
            message.extend(ascii_units("; entry ignored. Valid types: "));
            message.extend(ascii_units(VALID_TYPES_TEXT));
        } else {
            message.extend(ascii_units(if issue.hook_index.is_none() {
                "; matcher ignored."
            } else {
                "; entry ignored."
            }));
        }
        emit_warning(
            diagnostics,
            unresolved,
            context,
            path,
            message,
            Some(NativeHookDiagnosticSeverity::Warning),
            Some(string_projection_units(&issue.received)),
            true,
        );
    }

    let values = (0..length)
        .filter_map(|index| keep[index].then(|| retained[index].clone()).flatten())
        .collect::<Vec<_>>();
    match Utf16JsonProjection::array(values) {
        Ok(value) => Some(value),
        Err(_) => {
            *unresolved = true;
            Some(list.clone())
        }
    }
}

#[derive(Debug, Clone)]
enum PathPart {
    Key(Vec<u16>),
    Index(usize),
}

#[derive(Debug, Clone)]
struct SchemaIssue {
    path: Vec<PathPart>,
    message: Vec<u16>,
}

struct DueParser<'a> {
    issues: Vec<SchemaIssue>,
    unresolved: bool,
    js_semantics: &'a NativePolicyJsSemanticsRegistry,
}

impl DueParser<'_> {
    fn parse_event_map(
        &mut self,
        source: &Utf16JsonProjection,
        root: JsonValueView<'_>,
    ) -> Option<Utf16JsonProjection> {
        let mut root_path = vec![PathPart::Key(ascii_units("hooks"))];
        let Some(entries) = js_ordered_entries(root) else {
            self.issue(&root_path, invalid_type_message("record", js_type(root)));
            return None;
        };

        let mut output = Vec::new();
        let mut unknown_keys = Vec::new();
        for entry in entries {
            if !is_hook_event(entry.code_units) {
                unknown_keys.push(entry.code_units.to_vec());
                continue;
            }
            let Ok(event_value) = source.subprojection_property_units(entry.code_units) else {
                self.unresolved = true;
                continue;
            };
            let mut event_path = root_path.clone();
            event_path.push(PathPart::Key(entry.code_units.to_vec()));
            if let Some(event) = self.parse_matcher_list(&event_value, &event_path) {
                output.push((entry.code_units.to_vec(), event));
            }
        }
        if !unknown_keys.is_empty() {
            let mut message = Vec::new();
            let count = unknown_keys.len();
            message.extend(ascii_units(if count == 1 {
                "Unrecognized key: "
            } else {
                "Unrecognized keys: "
            }));
            for (index, key) in unknown_keys.iter().enumerate() {
                if index > 0 {
                    message.extend(ascii_units(", "));
                }
                message.push(u16::from(b'"'));
                message.extend_from_slice(key);
                message.push(u16::from(b'"'));
            }
            self.issue(&root_path, message);
        }
        root_path.clear();
        if self.issues.is_empty() && !self.unresolved {
            match Utf16JsonProjection::object_units(output) {
                Ok(value) => Some(value),
                Err(_) => {
                    self.unresolved = true;
                    None
                }
            }
        } else {
            None
        }
    }

    fn parse_matcher_list(
        &mut self,
        source: &Utf16JsonProjection,
        path: &[PathPart],
    ) -> Option<Utf16JsonProjection> {
        let Some(root) = source.at_pointer("") else {
            self.unresolved = true;
            return None;
        };
        let Some(length) = root.array_len() else {
            self.issue(path, invalid_type_message("array", js_type(root)));
            return None;
        };
        let mut matchers = Vec::with_capacity(length);
        let mut valid = true;
        for index in 0..length {
            let Ok(matcher) = source.subprojection(&format!("/{index}")) else {
                self.unresolved = true;
                valid = false;
                continue;
            };
            let mut matcher_path = path.to_vec();
            matcher_path.push(PathPart::Index(index));
            match self.parse_matcher(&matcher, &matcher_path) {
                Some(value) => matchers.push(value),
                None => valid = false,
            }
        }
        if valid {
            Utf16JsonProjection::array(matchers).ok().or_else(|| {
                self.unresolved = true;
                None
            })
        } else {
            None
        }
    }

    fn parse_matcher(
        &mut self,
        source: &Utf16JsonProjection,
        path: &[PathPart],
    ) -> Option<Utf16JsonProjection> {
        let Some(root) = source.at_pointer("") else {
            self.unresolved = true;
            return None;
        };
        let Some(entries) = root.object_entries() else {
            self.issue(path, invalid_type_message("object", js_type(root)));
            return None;
        };
        let mut output = Vec::with_capacity(2);
        let mut valid = true;

        if let Some(matcher) = root.get_property("matcher") {
            if matcher.code_units().is_some() {
                if let Ok(value) = source.subprojection_property_units(&ascii_units("matcher")) {
                    output.push((ascii_units("matcher"), value));
                } else {
                    self.unresolved = true;
                    valid = false;
                }
            } else {
                let mut field_path = path.to_vec();
                field_path.push(PathPart::Key(ascii_units("matcher")));
                self.issue(
                    &field_path,
                    invalid_type_message("string", js_type(matcher)),
                );
                valid = false;
            }
        }

        match root.get_property("hooks") {
            Some(hooks_view) => {
                let Ok(hooks) = source.subprojection_property_units(&ascii_units("hooks")) else {
                    self.unresolved = true;
                    return None;
                };
                let mut hooks_path = path.to_vec();
                hooks_path.push(PathPart::Key(ascii_units("hooks")));
                if let Some(parsed) = self.parse_hook_array(&hooks, &hooks_path) {
                    output.push((ascii_units("hooks"), parsed));
                } else {
                    let _ = hooks_view;
                    valid = false;
                }
            }
            None => {
                let mut hooks_path = path.to_vec();
                hooks_path.push(PathPart::Key(ascii_units("hooks")));
                self.issue(&hooks_path, invalid_type_message("array", "undefined"));
                valid = false;
            }
        }
        let _ = entries;
        if valid {
            Utf16JsonProjection::object_units(output).ok().or_else(|| {
                self.unresolved = true;
                None
            })
        } else {
            None
        }
    }

    fn parse_hook_array(
        &mut self,
        source: &Utf16JsonProjection,
        path: &[PathPart],
    ) -> Option<Utf16JsonProjection> {
        let Some(root) = source.at_pointer("") else {
            self.unresolved = true;
            return None;
        };
        let Some(length) = root.array_len() else {
            self.issue(path, invalid_type_message("array", js_type(root)));
            return None;
        };
        let mut output = Vec::with_capacity(length);
        let mut valid = true;
        for index in 0..length {
            let Ok(hook) = source.subprojection(&format!("/{index}")) else {
                self.unresolved = true;
                valid = false;
                continue;
            };
            let mut hook_path = path.to_vec();
            hook_path.push(PathPart::Index(index));
            match self.parse_hook_entry(&hook, &hook_path) {
                Some(value) => output.push(value),
                None => valid = false,
            }
        }
        if valid {
            Utf16JsonProjection::array(output).ok().or_else(|| {
                self.unresolved = true;
                None
            })
        } else {
            None
        }
    }

    fn parse_hook_entry(
        &mut self,
        source: &Utf16JsonProjection,
        path: &[PathPart],
    ) -> Option<Utf16JsonProjection> {
        let Some(root) = source.at_pointer("") else {
            self.unresolved = true;
            return None;
        };
        if root.object_entries().is_none() {
            self.issue(path, invalid_type_message("object", js_type(root)));
            return None;
        }
        let Some(type_units) = root
            .get_property("type")
            .and_then(JsonValueView::code_units)
        else {
            let mut type_path = path.to_vec();
            type_path.push(PathPart::Key(ascii_units("type")));
            self.issue(&type_path, ascii_units(INVALID_DISCRIMINATOR));
            return None;
        };
        let Some(type_name) = HOOK_TYPES
            .iter()
            .copied()
            .find(|name| ascii_eq(type_units, name))
        else {
            let mut type_path = path.to_vec();
            type_path.push(PathPart::Key(ascii_units("type")));
            self.issue(&type_path, ascii_units(INVALID_DISCRIMINATOR));
            return None;
        };
        let Some(type_value) = source
            .subprojection_property_units(&ascii_units("type"))
            .ok()
        else {
            self.unresolved = true;
            return None;
        };
        let mut output = vec![(ascii_units("type"), type_value)];
        let mut valid = true;
        match type_name {
            "command" => {
                self.string_field(source, root, path, "command", true, &mut output, &mut valid);
                self.string_array_field(source, root, path, "args", &mut output, &mut valid);
                self.string_field(source, root, path, "if", false, &mut output, &mut valid);
                self.enum_field(
                    source,
                    root,
                    path,
                    "shell",
                    COMMAND_SHELLS,
                    &mut output,
                    &mut valid,
                );
                self.positive_number_field(source, root, path, "timeout", &mut output, &mut valid);
                self.enum_field(
                    source,
                    root,
                    path,
                    "onFailure",
                    FAILURE_ACTIONS,
                    &mut output,
                    &mut valid,
                );
                self.string_field(
                    source,
                    root,
                    path,
                    "statusMessage",
                    false,
                    &mut output,
                    &mut valid,
                );
                self.boolean_field(source, root, path, "once", &mut output, &mut valid);
                self.boolean_field(source, root, path, "async", &mut output, &mut valid);
                self.boolean_field(source, root, path, "asyncRewake", &mut output, &mut valid);
                self.string_field(
                    source,
                    root,
                    path,
                    "rewakeMessage",
                    false,
                    &mut output,
                    &mut valid,
                );
                self.string_field(
                    source,
                    root,
                    path,
                    "rewakeSummary",
                    false,
                    &mut output,
                    &mut valid,
                );
                self.catch_enum_field(source, root, "cloud", CLOUD_ACTIONS, &mut output);
            }
            "prompt" => {
                self.string_field(source, root, path, "prompt", true, &mut output, &mut valid);
                self.string_field(source, root, path, "if", false, &mut output, &mut valid);
                self.positive_number_field(source, root, path, "timeout", &mut output, &mut valid);
                self.string_field(source, root, path, "model", false, &mut output, &mut valid);
                self.boolean_field(
                    source,
                    root,
                    path,
                    "continueOnBlock",
                    &mut output,
                    &mut valid,
                );
                self.string_field(
                    source,
                    root,
                    path,
                    "statusMessage",
                    false,
                    &mut output,
                    &mut valid,
                );
                self.boolean_field(source, root, path, "once", &mut output, &mut valid);
            }
            "agent" => {
                self.string_field(source, root, path, "prompt", true, &mut output, &mut valid);
                self.string_field(source, root, path, "if", false, &mut output, &mut valid);
                self.positive_number_field(source, root, path, "timeout", &mut output, &mut valid);
                self.string_field(source, root, path, "model", false, &mut output, &mut valid);
                self.string_field(
                    source,
                    root,
                    path,
                    "statusMessage",
                    false,
                    &mut output,
                    &mut valid,
                );
                self.boolean_field(source, root, path, "once", &mut output, &mut valid);
            }
            "http" => {
                self.url_field(source, root, path, "url", true, &mut output, &mut valid);
                self.string_field(source, root, path, "if", false, &mut output, &mut valid);
                self.positive_number_field(source, root, path, "timeout", &mut output, &mut valid);
                self.enum_field(
                    source,
                    root,
                    path,
                    "onFailure",
                    FAILURE_ACTIONS,
                    &mut output,
                    &mut valid,
                );
                self.record_field(source, root, path, "headers", true, &mut output, &mut valid);
                self.string_array_field(
                    source,
                    root,
                    path,
                    "allowedEnvVars",
                    &mut output,
                    &mut valid,
                );
                self.string_field(
                    source,
                    root,
                    path,
                    "statusMessage",
                    false,
                    &mut output,
                    &mut valid,
                );
                self.boolean_field(source, root, path, "once", &mut output, &mut valid);
                self.catch_enum_field(source, root, "cloud", CLOUD_ACTIONS, &mut output);
            }
            "mcp_tool" => {
                self.string_field(source, root, path, "server", true, &mut output, &mut valid);
                self.string_field(source, root, path, "tool", true, &mut output, &mut valid);
                self.record_field(source, root, path, "input", false, &mut output, &mut valid);
                self.string_field(source, root, path, "if", false, &mut output, &mut valid);
                self.positive_number_field(source, root, path, "timeout", &mut output, &mut valid);
                self.string_field(
                    source,
                    root,
                    path,
                    "statusMessage",
                    false,
                    &mut output,
                    &mut valid,
                );
                self.boolean_field(source, root, path, "once", &mut output, &mut valid);
            }
            _ => unreachable!("type name came from HOOK_TYPES"),
        }
        if valid {
            Utf16JsonProjection::object_units(output).ok().or_else(|| {
                self.unresolved = true;
                None
            })
        } else {
            None
        }
    }

    fn string_field(
        &mut self,
        source: &Utf16JsonProjection,
        root: JsonValueView<'_>,
        path: &[PathPart],
        key: &str,
        required: bool,
        output: &mut Vec<(Vec<u16>, Utf16JsonProjection)>,
        valid: &mut bool,
    ) {
        match root.get_property(key) {
            Some(value) if value.code_units().is_some() => {
                if let Ok(projection) = source.subprojection_property_units(&ascii_units(key)) {
                    output.push((ascii_units(key), projection));
                } else {
                    self.unresolved = true;
                    *valid = false;
                }
            }
            Some(value) => {
                let mut field_path = path.to_vec();
                field_path.push(PathPart::Key(ascii_units(key)));
                self.issue(&field_path, invalid_type_message("string", js_type(value)));
                *valid = false;
            }
            None if required => {
                let mut field_path = path.to_vec();
                field_path.push(PathPart::Key(ascii_units(key)));
                self.issue(&field_path, invalid_type_message("string", "undefined"));
                *valid = false;
            }
            None => {}
        }
    }

    fn string_array_field(
        &mut self,
        source: &Utf16JsonProjection,
        root: JsonValueView<'_>,
        path: &[PathPart],
        key: &str,
        output: &mut Vec<(Vec<u16>, Utf16JsonProjection)>,
        valid: &mut bool,
    ) {
        let Some(value) = root.get_property(key) else {
            return;
        };
        let field_path = path_with_key(path, key);
        let Some(length) = value.array_len() else {
            self.issue(&field_path, invalid_type_message("array", js_type(value)));
            *valid = false;
            return;
        };
        let Ok(projection) = source.subprojection_property_units(&ascii_units(key)) else {
            self.unresolved = true;
            *valid = false;
            return;
        };
        let mut field_valid = true;
        for index in 0..length {
            let Some(item) = value.get_index(index) else {
                self.unresolved = true;
                field_valid = false;
                continue;
            };
            if item.code_units().is_none() {
                self.issue(
                    &path_with_index(&field_path, index),
                    invalid_type_message("string", js_type(item)),
                );
                field_valid = false;
            }
        }
        if field_valid {
            output.push((ascii_units(key), projection));
        } else {
            *valid = false;
        }
    }

    fn enum_field(
        &mut self,
        source: &Utf16JsonProjection,
        root: JsonValueView<'_>,
        path: &[PathPart],
        key: &str,
        options: &[&str],
        output: &mut Vec<(Vec<u16>, Utf16JsonProjection)>,
        valid: &mut bool,
    ) {
        let Some(value) = root.get_property(key) else {
            return;
        };
        let field_path = path_with_key(path, key);
        let Some(units) = value.code_units() else {
            self.issue(&field_path, invalid_type_message("string", js_type(value)));
            *valid = false;
            return;
        };
        if !options.iter().any(|option| ascii_eq(units, option)) {
            self.issue(&field_path, invalid_enum_message(options));
            *valid = false;
            return;
        }
        match source.subprojection_property_units(&ascii_units(key)) {
            Ok(projection) => output.push((ascii_units(key), projection)),
            Err(_) => {
                self.unresolved = true;
                *valid = false;
            }
        }
    }

    fn catch_enum_field(
        &mut self,
        source: &Utf16JsonProjection,
        root: JsonValueView<'_>,
        key: &str,
        options: &[&str],
        output: &mut Vec<(Vec<u16>, Utf16JsonProjection)>,
    ) {
        let Some(value) = root.get_property(key) else {
            return;
        };
        if value
            .code_units()
            .is_some_and(|units| options.iter().any(|option| ascii_eq(units, option)))
        {
            match source.subprojection_property_units(&ascii_units(key)) {
                Ok(projection) => output.push((ascii_units(key), projection)),
                Err(_) => self.unresolved = true,
            }
        } else {
            output.push((ascii_units(key), string_projection("skip")));
        }
    }

    fn positive_number_field(
        &mut self,
        source: &Utf16JsonProjection,
        root: JsonValueView<'_>,
        path: &[PathPart],
        key: &str,
        output: &mut Vec<(Vec<u16>, Utf16JsonProjection)>,
        valid: &mut bool,
    ) {
        let Some(value) = root.get_property(key) else {
            return;
        };
        let field_path = path_with_key(path, key);
        match value.as_number() {
            Some(number) if number.is_finite() && number > 0.0 => {
                match source.subprojection_property_units(&ascii_units(key)) {
                    Ok(projection) => output.push((ascii_units(key), projection)),
                    Err(_) => {
                        self.unresolved = true;
                        *valid = false;
                    }
                }
            }
            Some(_) => {
                self.issue(
                    &field_path,
                    ascii_units("Too small: expected number to be >0"),
                );
                *valid = false;
            }
            None => {
                self.issue(&field_path, invalid_type_message("number", js_type(value)));
                *valid = false;
            }
        }
    }

    fn boolean_field(
        &mut self,
        source: &Utf16JsonProjection,
        root: JsonValueView<'_>,
        path: &[PathPart],
        key: &str,
        output: &mut Vec<(Vec<u16>, Utf16JsonProjection)>,
        valid: &mut bool,
    ) {
        let Some(value) = root.get_property(key) else {
            return;
        };
        if value.as_bool().is_some() {
            match source.subprojection_property_units(&ascii_units(key)) {
                Ok(projection) => output.push((ascii_units(key), projection)),
                Err(_) => {
                    self.unresolved = true;
                    *valid = false;
                }
            }
        } else {
            self.issue(
                &path_with_key(path, key),
                invalid_type_message("boolean", js_type(value)),
            );
            *valid = false;
        }
    }

    fn url_field(
        &mut self,
        source: &Utf16JsonProjection,
        root: JsonValueView<'_>,
        path: &[PathPart],
        key: &str,
        required: bool,
        output: &mut Vec<(Vec<u16>, Utf16JsonProjection)>,
        valid: &mut bool,
    ) {
        let Some(value) = root.get_property(key) else {
            if required {
                self.issue(
                    &path_with_key(path, key),
                    invalid_type_message("string", "undefined"),
                );
                *valid = false;
            }
            return;
        };
        let Some(units) = value.code_units() else {
            self.issue(
                &path_with_key(path, key),
                invalid_type_message("string", js_type(value)),
            );
            *valid = false;
            return;
        };
        let Some(row) = self.js_semantics.url(units) else {
            self.unresolved = true;
            *valid = false;
            return;
        };
        let NativePolicyZodUrlOutcome::Valid {
            zod_value_units, ..
        } = &row.zod_url
        else {
            self.issue(&path_with_key(path, key), ascii_units("Invalid URL"));
            *valid = false;
            return;
        };
        let projection = if zod_value_units.as_slice() == units {
            source.subprojection_property_units(&ascii_units(key))
        } else {
            Ok(string_projection_units(zod_value_units))
        };
        match projection {
            Ok(projection) => output.push((ascii_units(key), projection)),
            Err(_) => {
                self.unresolved = true;
                *valid = false;
            }
        }
    }

    fn record_field(
        &mut self,
        source: &Utf16JsonProjection,
        root: JsonValueView<'_>,
        path: &[PathPart],
        key: &str,
        string_values: bool,
        output: &mut Vec<(Vec<u16>, Utf16JsonProjection)>,
        valid: &mut bool,
    ) {
        let Some(value) = root.get_property(key) else {
            return;
        };
        let field_path = path_with_key(path, key);
        let Some(entries) = js_ordered_entries(value) else {
            self.issue(&field_path, invalid_type_message("record", js_type(value)));
            *valid = false;
            return;
        };
        let Ok(record) = source.subprojection_property_units(&ascii_units(key)) else {
            self.unresolved = true;
            *valid = false;
            return;
        };
        let mut normalized = Vec::with_capacity(entries.len());
        let mut field_valid = true;
        for entry in entries {
            // The source Zod record parser skips this exact key before parsing
            // its value, for both string headers and arbitrary MCP input.
            if ascii_eq(entry.code_units, "__proto__") {
                continue;
            }
            if string_values && entry.value.code_units().is_none() {
                let mut row_path = field_path.clone();
                row_path.push(PathPart::Key(entry.code_units.to_vec()));
                self.issue(
                    &row_path,
                    invalid_type_message("string", js_type(entry.value)),
                );
                field_valid = false;
                continue;
            }
            match record.subprojection_property_units(entry.code_units) {
                Ok(child) => normalized.push((entry.code_units.to_vec(), child)),
                Err(_) => {
                    self.unresolved = true;
                    field_valid = false;
                }
            }
        }
        if !field_valid {
            *valid = false;
            return;
        }
        match Utf16JsonProjection::object_units(normalized) {
            Ok(value) => output.push((ascii_units(key), value)),
            Err(_) => {
                self.unresolved = true;
                *valid = false;
            }
        }
    }

    fn issue(&mut self, path: &[PathPart], message: Vec<u16>) {
        self.issues.push(SchemaIssue {
            path: path.to_vec(),
            message,
        });
    }
}

enum HookEntryCheck {
    Valid,
    Invalid(HookEntryProblem),
    Unresolved,
}

struct HookEntryProblem {
    problem: Vec<u16>,
    received: Vec<u16>,
    about_type: bool,
}

fn validate_hook_entry_due(
    source: &Utf16JsonProjection,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> HookEntryCheck {
    let Some(root) = source.at_pointer("") else {
        return HookEntryCheck::Unresolved;
    };
    if root.object_entries().is_none() {
        let received = ze_type(root);
        return HookEntryCheck::Invalid(HookEntryProblem {
            problem: ascii_units(&format!(
                "Hook entry must be an object; received {received}"
            )),
            received: ascii_units(&received),
            about_type: false,
        });
    }
    let Some(type_value) = root.get_property("type") else {
        return HookEntryCheck::Invalid(HookEntryProblem {
            problem: ascii_units("Hook entry has no \"type\""),
            received: ascii_units("undefined"),
            about_type: true,
        });
    };
    let Some(type_units) = type_value.code_units() else {
        let received = js_type(type_value);
        return HookEntryCheck::Invalid(HookEntryProblem {
            problem: ascii_units(&format!(
                "Hook entry \"type\" must be a string; received {received}"
            )),
            received: ascii_units(&received),
            about_type: true,
        });
    };
    let Some(type_name) = HOOK_TYPES
        .iter()
        .copied()
        .find(|name| ascii_eq(type_units, name))
    else {
        let Some(display_type) = native_display_units(type_units, semantics) else {
            return HookEntryCheck::Unresolved;
        };
        let mut problem = ascii_units("Unknown hook type \"");
        problem.extend_from_slice(&display_type);
        problem.push(u16::from(b'"'));
        return HookEntryCheck::Invalid(HookEntryProblem {
            problem,
            received: display_type,
            about_type: true,
        });
    };

    let mut parser = DueParser {
        issues: Vec::new(),
        unresolved: false,
        js_semantics: semantics,
    };
    let parsed = parser.parse_hook_entry(source, &[]);
    if parser.unresolved {
        return HookEntryCheck::Unresolved;
    }
    if parser.issues.is_empty() {
        if parsed.is_some() {
            return HookEntryCheck::Valid;
        }
        return HookEntryCheck::Unresolved;
    }
    let formatted = format_schema_issues(&parser.issues);
    let mut problem = ascii_units(&format!("Invalid {type_name} hook ("));
    problem.extend(formatted);
    problem.extend(ascii_units(")"));
    HookEntryCheck::Invalid(HookEntryProblem {
        problem,
        received: ascii_units(type_name),
        about_type: false,
    })
}

fn validate_matcher_due_for_at(
    source: &Utf16JsonProjection,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> (Vec<SchemaIssue>, bool) {
    let mut parser = DueParser {
        issues: Vec::new(),
        unresolved: false,
        js_semantics: semantics,
    };
    match source.at_pointer("") {
        Some(_) => parser.parse_matcher(source, &[]),
        None => {
            parser.unresolved = true;
            None
        }
    };
    (parser.issues, parser.unresolved)
}

#[derive(Debug, Clone)]
struct OnIssue {
    path: Vec<PathPart>,
    problem: Vec<u16>,
    received: Vec<u16>,
    about_type: bool,
    hook_index: Option<usize>,
}

fn emit_warning(
    diagnostics: &mut Vec<NativeHookSettingsDiagnostic>,
    unresolved: &mut bool,
    context: &NativeHookSourceContext,
    path_units: Vec<u16>,
    message_units: Vec<u16>,
    severity: Option<NativeHookDiagnosticSeverity>,
    invalid_value: Option<Utf16JsonProjection>,
    preserve_on_write: bool,
) {
    let path = string_projection_units(&path_units);
    let message = string_projection_units(&message_units);
    let raw_value = invalid_value.clone();
    let mut fields = vec![
        (ascii_units("file"), string_projection(&context.file)),
        (ascii_units("path"), path.clone()),
        (ascii_units("message"), message.clone()),
    ];
    if let Some(severity) = severity {
        fields.push((
            ascii_units("severity"),
            string_projection(match severity {
                NativeHookDiagnosticSeverity::Fatal => "fatal",
                NativeHookDiagnosticSeverity::Warning => "warning",
            }),
        ));
    }
    if let Some(value) = invalid_value {
        fields.push((ascii_units("invalidValue"), value));
    }
    fields.push((ascii_units("docLink"), string_projection(DOC_LINK)));
    if preserve_on_write {
        fields.push((ascii_units("preserveOnWrite"), bool_projection(true)));
    }
    match Utf16JsonProjection::object_units(fields) {
        Ok(raw_warning) => diagnostics.push(NativeHookSettingsDiagnostic {
            path,
            message,
            severity,
            raw_value,
            raw_warning,
        }),
        Err(_) => *unresolved = true,
    }
}

fn on_issue_path(event_units: &[u16], issue: &OnIssue, about_type: bool) -> Option<Vec<u16>> {
    let mut output = ascii_units("hooks.");
    // `p_` interpolates the original event key in its matcher diagnostics;
    // unlike the preceding root-event diagnostics, it does not call `df`.
    output.extend_from_slice(event_units);
    for part in &issue.path {
        output.push(u16::from(b'.'));
        match part {
            PathPart::Key(key) => output.extend_from_slice(key),
            PathPart::Index(index) => output.extend(ascii_units(&index.to_string())),
        }
    }
    if about_type {
        output.extend(ascii_units(".type"));
    }
    Some(output)
}

fn format_schema_issues(issues: &[SchemaIssue]) -> Vec<u16> {
    let mut output = Vec::new();
    for (index, issue) in issues.iter().enumerate() {
        if index > 0 {
            output.extend(ascii_units("; "));
        }
        let path = path_units(&issue.path);
        if !path.is_empty() {
            output.extend(path);
            output.extend(ascii_units(": "));
        }
        output.extend_from_slice(&issue.message);
    }
    output
}

fn take_due_issues(issues: Vec<SchemaIssue>) -> Vec<NativeHookDueIssue> {
    issues
        .into_iter()
        .take(3)
        .map(|issue| NativeHookDueIssue {
            path: string_projection_units(&path_units(&issue.path)),
            message: string_projection_units(&issue.message),
        })
        .collect()
}

fn path_units(path: &[PathPart]) -> Vec<u16> {
    let mut output = Vec::new();
    for (index, part) in path.iter().enumerate() {
        if index > 0 {
            output.push(u16::from(b'.'));
        }
        match part {
            PathPart::Key(key) => output.extend_from_slice(key),
            PathPart::Index(index) => output.extend(ascii_units(&index.to_string())),
        }
    }
    output
}

fn path_with_key(path: &[PathPart], key: &str) -> Vec<PathPart> {
    let mut output = path.to_vec();
    output.push(PathPart::Key(ascii_units(key)));
    output
}

fn path_with_index(path: &[PathPart], index: usize) -> Vec<PathPart> {
    let mut output = path.to_vec();
    output.push(PathPart::Index(index));
    output
}

fn invalid_type_message(expected: &str, received: &str) -> Vec<u16> {
    ascii_units(&format!(
        "Invalid input: expected {expected}, received {received}"
    ))
}

fn invalid_enum_message(options: &[&str]) -> Vec<u16> {
    let mut output = ascii_units("Invalid option: expected one of ");
    for (index, option) in options.iter().enumerate() {
        if index > 0 {
            output.push(u16::from(b'|'));
        }
        output.push(u16::from(b'"'));
        output.extend(ascii_units(option));
        output.push(u16::from(b'"'));
    }
    output
}

fn js_type(value: JsonValueView<'_>) -> &'static str {
    if value.is_null() {
        "null"
    } else if value.code_units().is_some() {
        "string"
    } else if value.as_bool().is_some() {
        "boolean"
    } else if value.as_number().is_some() {
        "number"
    } else if value.array_len().is_some() {
        "array"
    } else if value.object_entries().is_some() {
        "object"
    } else {
        "undefined"
    }
}

fn ze_type(value: JsonValueView<'_>) -> String {
    match js_type(value) {
        "null" => "null".to_owned(),
        "undefined" => "undefined".to_owned(),
        "array" => "an array".to_owned(),
        "object" => "an object".to_owned(),
        "string" => "a string".to_owned(),
        "number" => "a number".to_owned(),
        "boolean" => "a boolean".to_owned(),
        _ => "an object".to_owned(),
    }
}

fn js_ordered_entries<'a>(value: JsonValueView<'a>) -> Option<Vec<JsonObjectEntryView<'a>>> {
    let mut entries = value.object_entries()?;
    entries.sort_by(|left, right| {
        match (array_index(left.code_units), array_index(right.code_units)) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
    Some(entries)
}

fn array_index(units: &[u16]) -> Option<u32> {
    if units.is_empty() || (units.len() > 1 && units[0] == u16::from(b'0')) {
        return None;
    }
    if units.iter().any(|unit| !matches!(*unit, 48..=57)) {
        return None;
    }
    let text = String::from_utf16(units).ok()?;
    let index = text.parse::<u32>().ok()?;
    (index < u32::MAX).then_some(index)
}

fn is_hook_event(units: &[u16]) -> bool {
    HOOK_EVENTS.iter().any(|event| ascii_eq(units, event))
}

fn is_guarded_event(units: &[u16]) -> bool {
    GUARDED_EVENTS.iter().any(|event| ascii_eq(units, event))
}

fn is_single_matcher(value: JsonValueView<'_>) -> bool {
    let Some(entries) = value.object_entries() else {
        return false;
    };
    let has_matcher = value.get_property("matcher").is_some();
    if !has_matcher && entries.iter().any(|entry| is_hook_event(entry.code_units)) {
        return false;
    }
    let Some(hooks) = value.get_property("hooks") else {
        return false;
    };
    if let Some(length) = hooks.array_len() {
        return length > 0;
    }
    hooks.object_entries().is_some()
        && (has_matcher
            || hooks
                .get_property("type")
                .and_then(JsonValueView::code_units)
                .is_some())
}

fn has_guard_property(value: JsonValueView<'_>) -> bool {
    let Some(entries) = value.object_entries() else {
        return false;
    };
    entries.iter().any(|entry| {
        GUARDED_EVENTS
            .iter()
            .any(|event| ascii_eq(entry.code_units, event))
            && !entry.value.is_null()
            && !entry.value.array_len().is_some_and(|length| length == 0)
    })
}

fn contains_guard_hooks(
    value: JsonValueView<'_>,
    depth: usize,
    matchers_count: bool,
    unscanned_keys: &[&str],
    in_hooks_list: bool,
) -> bool {
    if has_guard_property(value) {
        return true;
    }
    if matchers_count && is_single_matcher(value) {
        return true;
    }
    if depth == 0 {
        return false;
    }
    if let Some(length) = value.array_len() {
        return (0..length).any(|index| {
            value.get_index(index).is_some_and(|child| {
                contains_guard_hooks(
                    child,
                    depth - 1,
                    matchers_count,
                    unscanned_keys,
                    in_hooks_list,
                )
            })
        });
    }
    if in_hooks_list
        && value
            .get_property("type")
            .and_then(JsonValueView::code_units)
            .is_some()
    {
        return false;
    }
    let Some(entries) = js_ordered_entries(value) else {
        return false;
    };
    entries.iter().any(|entry| {
        !unscanned_keys
            .iter()
            .any(|key| ascii_eq(entry.code_units, key))
            && contains_guard_hooks(
                entry.value,
                depth - 1,
                matchers_count && !is_hook_event(entry.code_units),
                unscanned_keys,
                ascii_eq(entry.code_units, "hooks") && entry.value.array_len().is_some(),
            )
    })
}

fn misplaced_guard_hooks(root: JsonValueView<'_>) -> bool {
    if has_guard_property(root) {
        return true;
    }
    let Some(entries) = js_ordered_entries(root) else {
        return false;
    };
    entries.iter().any(|entry| {
        !ascii_eq(entry.code_units, "hooks")
            && !SCANNED_TOP_LEVEL_KEYS
                .iter()
                .any(|key| ascii_eq(entry.code_units, key))
            && contains_guard_hooks(
                entry.value,
                3,
                !is_hook_event(entry.code_units),
                SCANNED_TOP_LEVEL_KEYS,
                false,
            )
    })
}

fn array_has_object(source: &Utf16JsonProjection, value: JsonValueView<'_>) -> bool {
    let Some(length) = value.array_len() else {
        return false;
    };
    for index in 0..length {
        if let Ok(child) = source.subprojection(&format!("/{index}")) {
            if child
                .at_pointer("")
                .is_some_and(|child| child.object_entries().is_some())
            {
                return true;
            }
        }
    }
    false
}

fn array_contains_guard(source: &Utf16JsonProjection, value: JsonValueView<'_>) -> bool {
    let Some(length) = value.array_len() else {
        return false;
    };
    for index in 0..length {
        if let Ok(child) = source.subprojection(&format!("/{index}")) {
            if child.at_pointer("").is_some_and(contains_guard_array) {
                return true;
            }
        }
    }
    false
}

fn contains_guard_array(value: JsonValueView<'_>) -> bool {
    if is_single_matcher(value) || has_guard_property(value) {
        return true;
    }
    if let Some(length) = value.array_len() {
        return (0..length).any(|index| value.get_index(index).is_some_and(contains_guard_array));
    }
    let Some(entries) = js_ordered_entries(value) else {
        return false;
    };
    entries.iter().any(|entry| {
        GUARDED_EVENTS
            .iter()
            .any(|event| ascii_eq(entry.code_units, event))
            && !entry.value.is_null()
            && !entry.value.array_len().is_some_and(|length| length == 0)
            || contains_guard_array(entry.value)
    })
}

fn rebuild_array(
    parent: &Utf16JsonProjection,
    key: &str,
    keep: &[bool],
) -> Result<Utf16JsonProjection, ()> {
    let source = parent
        .subprojection_property_units(&ascii_units(key))
        .map_err(|_| ())?;
    let Some(root) = source.at_pointer("") else {
        return Err(());
    };
    if root.array_len() != Some(keep.len()) {
        return Err(());
    }
    let mut values = Vec::new();
    for (index, keep) in keep.iter().enumerate() {
        if *keep {
            values.push(source.subprojection(&format!("/{index}")).map_err(|_| ())?);
        }
    }
    let array = Utf16JsonProjection::array(values).map_err(|_| ())?;
    replace_property(parent, key, Some(array))
}

fn replace_property(
    source: &Utf16JsonProjection,
    key: &str,
    replacement: Option<Utf16JsonProjection>,
) -> Result<Utf16JsonProjection, ()> {
    let Some(root) = source.at_pointer("") else {
        return Err(());
    };
    let Some(entries) = root.object_entries() else {
        return Err(());
    };
    let key_units = ascii_units(key);
    let mut output = Vec::with_capacity(entries.len());
    let mut replaced = false;
    for entry in entries {
        if entry.code_units == key_units {
            replaced = true;
            if let Some(value) = &replacement {
                output.push((entry.code_units.to_vec(), value.clone()));
            }
        } else {
            output.push((
                entry.code_units.to_vec(),
                source
                    .subprojection_property_units(entry.code_units)
                    .map_err(|_| ())?,
            ));
        }
    }
    if !replaced {
        if let Some(value) = replacement {
            output.push((key_units, value));
        }
    }
    Utf16JsonProjection::object_units(output).map_err(|_| ())
}

fn hook_event_path(event_units: &[u16]) -> Vec<u16> {
    let mut path = ascii_units("hooks.");
    path.extend_from_slice(event_units);
    path
}

/// Discover only JS inputs passed to `native_display_units` or `url_field`
/// by the fixed `u_` hook walk. Unknown event keys are always formatted;
/// nested hook types/URLs are visited only for event arrays which the same
/// pass actually walks (including unknown arrays protected by guard hooks).
pub(crate) fn collect_native_hook_semantic_inputs(
    hooks: &Utf16JsonProjection,
) -> NativeHookSemanticInputs {
    let mut result = NativeHookSemanticInputs::default();
    let Some(root) = hooks.at_pointer("") else {
        return result;
    };
    let Some(events) = js_ordered_entries(root) else {
        return result;
    };
    // `normalize_native_hook_settings` returns before visiting event rows when
    // this is the legacy single-matcher shape; Due then rejects the keys as
    // event names without invoking the display helper.
    if is_single_matcher(root) {
        return result;
    }
    let mut pending_arrays = Vec::new();

    for event in events {
        let known_event = is_hook_event(event.code_units);
        if !known_event {
            super::native_policy::NativePolicySemanticInputs::push_unique(
                &mut result.display_inputs,
                event.code_units.to_vec(),
            );
        }
        if event.value.array_len().is_none() {
            continue;
        }
        if known_event || contains_guard_hooks(event.value, 3, false, &[], false) {
            pending_arrays.push(event.value);
        }
    }

    for matchers in pending_arrays {
        let Some(length) = matchers.array_len() else {
            continue;
        };
        for matcher_index in (0..length).rev() {
            let Some(matcher) = matchers.get_index(matcher_index) else {
                continue;
            };
            if contains_guard_hooks(matcher, 3, false, &[], false) {
                continue;
            }
            let Some(hook_rows) = matcher
                .object_entries()
                .and_then(|_| matcher.get_property("hooks"))
            else {
                continue;
            };
            let Some(hook_count) = hook_rows.array_len() else {
                continue;
            };
            for hook_index in (0..hook_count).rev() {
                let Some(hook) = hook_rows.get_index(hook_index) else {
                    continue;
                };
                if hook.object_entries().is_none() {
                    continue;
                }
                let Some(type_units) = hook
                    .get_property("type")
                    .and_then(JsonValueView::code_units)
                else {
                    continue;
                };
                let known_type = HOOK_TYPES.iter().any(|kind| ascii_eq(type_units, kind));
                if !known_type {
                    super::native_policy::NativePolicySemanticInputs::push_unique(
                        &mut result.display_inputs,
                        type_units.to_vec(),
                    );
                } else if ascii_eq(type_units, "http") {
                    if let Some(url) = hook.get_property("url").and_then(JsonValueView::code_units)
                    {
                        super::native_policy::NativePolicySemanticInputs::push_unique(
                            &mut result.url_inputs,
                            url.to_vec(),
                        );
                    }
                }
            }
        }
    }
    result
}

fn native_display_units(
    input: &[u16],
    semantics: &NativePolicyJsSemanticsRegistry,
) -> Option<Vec<u16>> {
    semantics.display(input).map(|row| row.output_units.clone())
}

fn empty_array() -> Utf16JsonProjection {
    Utf16JsonProjection::array(Vec::new()).expect("an empty projection array is valid")
}

fn bool_projection(value: bool) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(if value { "true" } else { "false" })
        .expect("a fixed boolean is valid JSON")
}

fn string_projection(value: &str) -> Utf16JsonProjection {
    string_projection_units(&ascii_units(value))
}

fn string_projection_units(units: &[u16]) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec())
        .expect("display spelling is derived from the exact UTF-16 units")
}

fn ascii_units(value: &str) -> Vec<u16> {
    value.encode_utf16().collect()
}

fn ascii_eq(units: &[u16], expected: &str) -> bool {
    units.iter().copied().eq(expected.encode_utf16())
}

fn concat_units(parts: &[&[u16]]) -> Vec<u16> {
    parts.iter().flat_map(|part| part.iter().copied()).collect()
}

fn hook_unresolved(normalized: Option<Utf16JsonProjection>) -> NativeHookSettingsResult {
    NativeHookSettingsResult {
        normalized,
        diagnostics: Vec::new(),
        unresolved: true,
    }
}

fn hook_unresolved_with(
    diagnostics: Vec<NativeHookSettingsDiagnostic>,
) -> NativeHookSettingsResult {
    NativeHookSettingsResult {
        normalized: None,
        diagnostics,
        unresolved: true,
    }
}

fn due_unresolved() -> NativeHookDueResult {
    NativeHookDueResult {
        normalized: None,
        issues: Vec::new(),
        unresolved: true,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_native_hook_settings as normalize_native_hook_settings_source,
        validate_native_hooks_due as validate_native_hooks_due_source, NativeHookSourceContext,
    };
    use crate::settings::native_policy_js_semantics::{
        NativePolicyDisplayRow, NativePolicyJsSemanticsRegistry, NativePolicyUrlSemanticRow,
        NativePolicyZodUrlOutcome,
    };
    use crate::types::utf16_json::{JsonValueView, Utf16JsonProjection};

    fn parse(source: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(source).expect("fixture is valid JSON")
    }

    fn context() -> NativeHookSourceContext {
        NativeHookSourceContext {
            file: "policyHelpers.defaultSettings".to_owned(),
        }
    }

    fn test_semantics(source: &Utf16JsonProjection) -> NativePolicyJsSemanticsRegistry {
        let mut url_inputs = Vec::<Vec<u16>>::new();
        if let Some(root) = source.at_pointer("") {
            let mut stack = vec![root];
            while let Some(value) = stack.pop() {
                if let Some(entries) = value.object_entries() {
                    for entry in entries {
                        if entry.code_units.iter().copied().eq("url".encode_utf16()) {
                            if let Some(units) = entry.value.code_units() {
                                if !url_inputs
                                    .iter()
                                    .any(|existing| existing.as_slice() == units)
                                {
                                    url_inputs.push(units.to_vec());
                                }
                            }
                        }
                        stack.push(entry.value);
                    }
                } else if let Some(length) = value.array_len() {
                    for index in 0..length {
                        if let Some(item) = value.get_index(index) {
                            stack.push(item);
                        }
                    }
                }
            }
        }

        let mut display_inputs = Vec::<Vec<u16>>::new();
        let hooks = source
            .subprojection("/hooks")
            .ok()
            .or_else(|| Some(source.clone()));
        if let Some(hooks) = hooks {
            if let Some(events) = hooks.at_pointer("").and_then(JsonValueView::object_entries) {
                for event in events {
                    push_test_display(&mut display_inputs, event.code_units);
                    let Some(length) = event.value.array_len() else {
                        continue;
                    };
                    for index in 0..length {
                        let Some(matcher) = event.value.get_index(index) else {
                            continue;
                        };
                        let Some(hooks_array) = matcher.get_property("hooks") else {
                            continue;
                        };
                        let Some(hook_count) = hooks_array.array_len() else {
                            continue;
                        };
                        for hook_index in 0..hook_count {
                            if let Some(units) = hooks_array
                                .get_index(hook_index)
                                .and_then(|hook| hook.get_property("type"))
                                .and_then(JsonValueView::code_units)
                            {
                                push_test_display(&mut display_inputs, units);
                            }
                        }
                    }
                }
            }
        }
        let urls = url_inputs.into_iter().map(test_url_row).collect();
        let displays = display_inputs
            .into_iter()
            .map(|input_units| {
                let mut output_units = Vec::new();
                for unit in &input_units {
                    match *unit {
                        0x000a => output_units.push(0x2424),
                        0x0009 => output_units.push(0x2409),
                        0x200d => output_units.extend("\\u{200D}".encode_utf16()),
                        _ => output_units.push(*unit),
                    }
                }
                NativePolicyDisplayRow {
                    input_units,
                    output_units,
                }
            })
            .collect();
        NativePolicyJsSemanticsRegistry::from_rows(urls, displays)
            .expect("test semantic inputs are unique")
    }

    fn push_test_display(values: &mut Vec<Vec<u16>>, units: &[u16]) {
        if !values.iter().any(|existing| existing.as_slice() == units) {
            values.push(units.to_vec());
        }
    }

    fn test_url_row(input_units: Vec<u16>) -> NativePolicyUrlSemanticRow {
        let text = String::from_utf16_lossy(&input_units);
        let trimmed = text.trim().to_owned();
        let normalized_units = trimmed.encode_utf16().collect::<Vec<_>>();
        let parsed = url::Url::parse(&trimmed).ok();
        let zod_url = parsed
            .as_ref()
            .map_or(NativePolicyZodUrlOutcome::Invalid, |url| {
                let host = url.host_str().unwrap_or_default().to_owned();
                let protocol = format!("{}:", url.scheme());
                NativePolicyZodUrlOutcome::Valid {
                    zod_value_units: normalized_units,
                    href_units: url.as_str().encode_utf16().collect(),
                    protocol_units: protocol.encode_utf16().collect(),
                    username_units: url.username().encode_utf16().collect(),
                    password_units: url.password().unwrap_or_default().encode_utf16().collect(),
                    host_units: host.encode_utf16().collect(),
                    hostname_units: host.encode_utf16().collect(),
                    port_units: url
                        .port()
                        .map_or_else(Vec::new, |port| port.to_string().encode_utf16().collect()),
                    pathname_units: url.path().encode_utf16().collect(),
                    search_units: url.query().map_or_else(Vec::new, |value| {
                        format!("?{value}").encode_utf16().collect()
                    }),
                    hash_units: url.fragment().map_or_else(Vec::new, |value| {
                        format!("#{value}").encode_utf16().collect()
                    }),
                    origin_units: url.origin().ascii_serialization().encode_utf16().collect(),
                    rdt_zod_value_valid: false,
                    archive_zod_refine_valid: false,
                    archive_zod_hostname_blocked: None,
                }
            });
        NativePolicyUrlSemanticRow {
            input_units,
            raw_rdt_valid: false,
            raw_archive_refine_valid: false,
            raw_archive_hostname_blocked: None,
            zod_url,
        }
    }

    fn normalize_native_hook_settings(
        root: &Utf16JsonProjection,
        context: &NativeHookSourceContext,
    ) -> super::NativeHookSettingsResult {
        let semantics = test_semantics(root);
        normalize_native_hook_settings_source(root, context, &semantics)
    }

    fn validate_native_hooks_due(hooks: &Utf16JsonProjection) -> super::NativeHookDueResult {
        let semantics = test_semantics(hooks);
        validate_native_hooks_due_source(hooks, &semantics)
    }

    #[test]
    fn at_orders_root_event_warnings_before_matcher_warnings_and_keeps_raw_rows() {
        let root = parse(
            r#"{"hooks":{"zzz":[],"PostToolUse":[{"matcher":3,"hooks":[{"type":"command","command":"echo"}]},{"hooks":[{"type":"command","command":2}] }],"PreToolUse":[]}}"#,
        );
        let result = normalize_native_hook_settings(&root, &context());
        assert!(!result.unresolved);
        assert_eq!(result.diagnostics.len(), 3);
        assert_eq!(
            result.normalized.unwrap().to_json_string().unwrap(),
            r#"{"PostToolUse":[{"hooks":[]}],"PreToolUse":[]}"#
        );
        let rows = result
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.raw_warning.to_json_string().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            rows[0],
            r#"{"file":"policyHelpers.defaultSettings","path":"hooks.zzz","message":"Unknown hook event \"zzz\" was ignored. Valid events: PreToolUse, PostToolUse, PostToolUseFailure, PostToolBatch, Notification, UserPromptSubmit, UserPromptExpansion, SessionStart, SessionEnd, Stop, StopFailure, SubagentStart, SubagentStop, PreCompact, PostCompact, PreModelSwitch, PostModelSwitch, PermissionRequest, PermissionDenied, Setup, TeammateIdle, TaskCreated, TaskCompleted, Elicitation, ElicitationResult, ConfigChange, WorktreeCreate, WorktreeRemove, InstructionsLoaded, CwdChanged, FileChanged, DirectoryAdded, MessageDisplay","severity":"warning","invalidValue":"zzz","docLink":"https://code.claude.com/docs/en/hooks","preserveOnWrite":true}"#
        );
        assert!(rows[1].contains("hooks.PostToolUse.0\""));
        assert!(rows[1].contains("matcher ignored."));
        assert!(rows[2].contains("hooks.PostToolUse.1.hooks.0\""));
        assert!(rows[2].contains("entry ignored."));
        assert!(rows.iter().all(|row| row.starts_with("{\"file\":")));
    }

    #[test]
    fn misplaced_guard_and_admin_source_context_are_exact_warning_rows() {
        let root =
            parse(r#"{"PreToolUse":[{"hooks":[{"type":"command","command":"echo"}]}],"hooks":{}}"#);
        let result = normalize_native_hook_settings(&root, &context());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0].raw_warning.to_json_string().unwrap(),
            format!(
                "{{\"file\":\"policyHelpers.defaultSettings\",\"path\":\"hooks\",\"message\":\"PreToolUse/PermissionRequest hooks are declared outside \\\"hooks\\\" (at the top level or under another key) — {}.\",\"severity\":\"fatal\",\"docLink\":\"{}\"}}",
                super::UNLOADABLE_GUARD,
                super::DOC_LINK
            )
        );

        let exempt = parse(
            r#"{"mcpServers":{"PreToolUse":[{"hooks":[{"type":"command","command":"echo"}]}]}}"#,
        );
        let result = normalize_native_hook_settings(&exempt, &context());
        assert!(result.diagnostics.is_empty());
        assert!(result.normalized.is_none());
    }

    #[test]
    fn invalid_root_shapes_and_single_matchers_follow_at_keep_or_remove_rules() {
        let dropped = normalize_native_hook_settings(&parse(r#"{"hooks":null}"#), &context());
        assert!(dropped.normalized.is_none());
        assert_eq!(dropped.diagnostics.len(), 1);
        assert_eq!(
            dropped.diagnostics[0].raw_warning.to_json_string().unwrap(),
            format!(
                "{{\"file\":\"policyHelpers.defaultSettings\",\"path\":\"hooks\",\"message\":\"\\\"hooks\\\" must be an object mapping event names to matcher arrays; received null. This field was ignored.\",\"severity\":\"warning\",\"invalidValue\":\"null\",\"docLink\":\"{}\"}}",
                super::DOC_LINK
            )
        );

        let kept = normalize_native_hook_settings(
            &parse(r#"{"hooks":[{"matcher":"Write","hooks":[]}] }"#),
            &context(),
        );
        assert_eq!(
            kept.normalized.unwrap().to_json_string().unwrap(),
            r#"[{"matcher":"Write","hooks":[]}]"#
        );
        assert_eq!(kept.diagnostics.len(), 1);
        assert!(kept.diagnostics[0].severity.is_none());

        let single = normalize_native_hook_settings(
            &parse(r#"{"hooks":{"matcher":"Write","hooks":[]}}"#),
            &context(),
        );
        assert_eq!(single.normalized.unwrap().to_json_string().unwrap(), "{}",);
        assert_eq!(single.diagnostics.len(), 2);
        assert_eq!(
            single.diagnostics[0].path.string_at(""),
            Some("hooks.matcher"),
        );
        assert_eq!(
            single.diagnostics[1].path.string_at(""),
            Some("hooks.hooks"),
        );
        assert!(single
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity
                == Some(super::NativeHookDiagnosticSeverity::Warning)));
    }

    #[test]
    fn event_name_lf_lone_surrogate_and_zwj_escape_use_worker_display_rows() {
        let lf = parse(r#"{"hooks":{"bad\nevent":[]}}"#);
        let result = normalize_native_hook_settings(&lf, &context());
        assert!(!result.unresolved);
        assert_eq!(
            result
                .normalized
                .as_ref()
                .unwrap()
                .to_json_string()
                .unwrap(),
            "{}",
        );
        assert_eq!(
            result.diagnostics[0].path.string_at(""),
            Some("hooks.bad␤event")
        );

        let lone = parse(r#"{"hooks":{"\ud800":[]}}"#);
        let result = normalize_native_hook_settings(&lone, &context());
        assert!(!result.unresolved);
        assert_eq!(
            result.diagnostics[0]
                .path
                .at_pointer("")
                .and_then(|value| value.code_units()),
            Some(&[104, 111, 111, 107, 115, 46, 0xd800][..])
        );
        assert!(result.diagnostics[0]
            .raw_warning
            .to_json_string()
            .unwrap()
            .contains(r#""invalidValue":"\ud800""#));

        let emoji_joiner = parse(r#"{"hooks":{"bad\u200dkey":[]}}"#);
        let result = normalize_native_hook_settings(&emoji_joiner, &context());
        assert!(!result.unresolved);
        assert_eq!(
            result.diagnostics[0].path.string_at(""),
            Some("hooks.bad\\u{200D}key"),
        );
        assert_eq!(result.normalized.unwrap().to_json_string().unwrap(), "{}",);
    }

    #[test]
    fn nested_due_normalizes_all_five_hook_types_and_strips_unknown_record_keys() {
        let hooks = parse(
            r#"{"PreToolUse":[{"matcher":"Bash(git *)","extraMatcher":true,"hooks":[{"type":"command","command":"echo ok","args":["-lc"],"shell":"bash","timeout":1.5,"onFailure":"block","once":true,"async":true,"asyncRewake":true,"rewakeMessage":"x","rewakeSummary":"y","cloud":"bad","extra":true},{"type":"prompt","prompt":"check","continueOnBlock":false,"model":"sonnet"},{"type":"agent","prompt":"check"},{"type":"http","url":"ftp://example.com/path","headers":{"A":"1","__proto__":"skip"},"allowedEnvVars":["TOKEN"]},{"type":"mcp_tool","server":"svc","tool":"inspect","input":{"nested":{"x":1},"__proto__":"skip"}}]}]}"#,
        );
        let result = validate_native_hooks_due(&hooks);
        assert!(!result.unresolved);
        assert!(result.issues.is_empty());
        assert_eq!(
            result.normalized.unwrap().to_json_string().unwrap(),
            r#"{"PreToolUse":[{"matcher":"Bash(git *)","hooks":[{"type":"command","command":"echo ok","args":["-lc"],"shell":"bash","timeout":1.5,"onFailure":"block","once":true,"async":true,"asyncRewake":true,"rewakeMessage":"x","rewakeSummary":"y","cloud":"skip"},{"type":"prompt","prompt":"check","model":"sonnet","continueOnBlock":false},{"type":"agent","prompt":"check"},{"type":"http","url":"ftp://example.com/path","headers":{"A":"1"},"allowedEnvVars":["TOKEN"]},{"type":"mcp_tool","server":"svc","tool":"inspect","input":{"nested":{"x":1}}}]}]}"#
        );
    }

    #[test]
    fn protected_due_errors_keep_issue_order_path_and_native_message() {
        let root = parse(
            r#"{"hooks":{"PreToolUse":[{"matcher":3,"hooks":[{"type":"command","command":4}]}]}}"#,
        );
        let at = normalize_native_hook_settings(&root, &context());
        assert_eq!(at.diagnostics.len(), 1);
        assert_eq!(
            at.diagnostics[0].severity,
            Some(super::NativeHookDiagnosticSeverity::Fatal)
        );
        let due = validate_native_hooks_due(at.normalized.as_ref().unwrap());
        assert!(!due.unresolved);
        assert_eq!(due.issues.len(), 2);
        assert_eq!(
            due.issues[0].path.string_at(""),
            Some("hooks.PreToolUse.0.matcher")
        );
        assert_eq!(
            due.issues[0].message.string_at(""),
            Some("Invalid input: expected string, received number")
        );
        assert_eq!(
            due.issues[1].path.string_at(""),
            Some("hooks.PreToolUse.0.hooks.0.command")
        );
    }

    #[test]
    fn at_keeps_all_matcher_hook_issues_while_final_due_projects_only_three() {
        let root = parse(
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":3,"args":["-lc",4],"if":2,"shell":"fish","timeout":0,"onFailure":"ask","statusMessage":7,"once":"yes"}]}]}}"#,
        );
        let at = normalize_native_hook_settings(&root, &context());
        assert!(!at.unresolved);
        assert_eq!(at.diagnostics.len(), 1);
        let diagnostic = &at.diagnostics[0];
        assert_eq!(
            diagnostic.path.string_at(""),
            Some("hooks.PreToolUse.0.hooks.0")
        );
        assert_eq!(
            diagnostic.severity,
            Some(super::NativeHookDiagnosticSeverity::Fatal)
        );
        let message = diagnostic.message.string_at("").unwrap();
        let summary = message
            .strip_prefix("Invalid command hook (")
            .and_then(|message| message.split_once(") — ").map(|(summary, _)| summary))
            .expect("At warning contains one fully formatted command-hook issue list");
        assert_eq!(summary.matches("; ").count(), 7, "all eight issues remain");
        for issue in [
            "command: Invalid input: expected string, received number",
            "args.1: Invalid input: expected string, received number",
            "if: Invalid input: expected string, received number",
            "shell: Invalid option: expected one of \"bash\"|\"powershell\"",
            "timeout: Too small: expected number to be >0",
            "onFailure: Invalid option: expected one of \"continue\"|\"block\"",
            "statusMessage: Invalid input: expected string, received number",
            "once: Invalid input: expected boolean, received string",
        ] {
            assert!(
                summary.contains(issue),
                "At warning omitted {issue:?}: {summary}"
            );
        }

        let due = validate_native_hooks_due(at.normalized.as_ref().unwrap());
        assert!(!due.unresolved);
        assert_eq!(due.issues.len(), 3);
        assert_eq!(
            due.issues
                .iter()
                .map(|issue| issue.path.string_at("").unwrap())
                .collect::<Vec<_>>(),
            vec![
                "hooks.PreToolUse.0.hooks.0.command",
                "hooks.PreToolUse.0.hooks.0.args.1",
                "hooks.PreToolUse.0.hooks.0.if",
            ]
        );
    }
}
