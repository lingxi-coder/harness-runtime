//! Per-turn reminders and memory/skill prefetch pipelines.

use super::*;
use hooks::attachment::HookPublicationGuard;
use hooks::ExactHookText;
use std::sync::Arc;

const NATIVE_MCP_POLICY_BLOCKED_ERRORS: [&str; 2] = [
    "Blocked by enterprise managed policy",
    "Disabled by disableClaudeAiConnectors setting",
];

fn native_mcp_tool_identity(name: &str) -> Option<(&str, &str)> {
    let remainder = name.strip_prefix("mcp__")?;
    let (server, tool) = remainder.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

/// Project Native `wgn`'s `xCt` allowlisted causes for one tool absent from the
/// current full identity pool. A disconnected/connecting/needs-auth server is
/// deliberately not a retraction cause.
fn native_mcp_revocation_cause(
    name: &str,
    deny_rules: &[String],
    state: Option<lingxi_core::host::McpActionState>,
    config: Option<&mcp::McpServerConfig>,
    failure: Option<&mcp::McpFailedServerSnapshot>,
) -> Option<&'static str> {
    if super::model_reminders_impl::native_mcp_dropped_tool_denied(deny_rules, name) {
        return Some("denied");
    }
    let (_, tool_name) = native_mcp_tool_identity(name)?;
    let normalized_tool_name = mcp::normalization::normalize_name_for_mcp(tool_name);
    if config.is_some_and(|config| {
        config.tool_permissions.get(&normalized_tool_name)
            == Some(&lingxi_core::host::McpPermissionCeiling::Deny)
    }) {
        return Some("org_blocked");
    }
    match state {
        Some(lingxi_core::host::McpActionState::Disabled) => Some(
            if config.is_some_and(|config| config.spec.kind() == "sdk-control") {
                "switched_off"
            } else {
                "disabled"
            },
        ),
        Some(lingxi_core::host::McpActionState::Failed) => {
            if failure
                .and_then(|failure| failure.error_code.as_ref())
                .and_then(|code| code.string_at(""))
                == Some("UNCONFIGURED")
                || config.is_some_and(mcp::McpServerConfig::is_unconfigured)
            {
                return Some("not_configured");
            }
            let error = failure
                .and_then(|failure| failure.native_raw_error.as_ref())
                .and_then(|error| error.string_at(""));
            NATIVE_MCP_POLICY_BLOCKED_ERRORS
                .contains(&error?)
                .then_some("policy_blocked")
        }
        // `pye` returns `not_configured` when there is no matching MCP client
        // row. Other live states (`pending`, `needs-auth`, connected/cached)
        // map to causes that `xCt` explicitly does not retain.
        None => Some("not_configured"),
        Some(
            lingxi_core::host::McpActionState::Connected
            | lingxi_core::host::McpActionState::Pending
            | lingxi_core::host::McpActionState::NeedsAuth
            | lingxi_core::host::McpActionState::NeedsApproval,
        ) => None,
    }
}

/// Native `pfr(..., {aheadOfRequest:true})` admission for tool-search-free
/// tool-change announcements. `None` model support is unavailable evidence and
/// fails closed; `tool_pool_refreshes` mirrors Native's
/// `options.refreshTools !== undefined` fact. It must come from a caller that
/// actually exposes that callback, never from ordinary request assembly or a
/// registry-generation guess.
fn native_tool_search_absent_admitted(
    tool_search_filtered_out: bool,
    native_mctc_supported: Option<bool>,
    mctc_sent: bool,
    mctc_rejected: bool,
    mcs_rejected: bool,
    tool_change_header_refused: bool,
    keep_deferred_tools: bool,
    declared_pool_available: bool,
    tool_pool_refreshes: Option<bool>,
    foamy_spring_enabled: bool,
) -> bool {
    let pool_refresh_callback_available = tool_pool_refreshes == Some(true);
    let pool_gate =
        declared_pool_available || (pool_refresh_callback_available && foamy_spring_enabled);
    let mctc_gate = (mctc_sent || foamy_spring_enabled) && !mctc_rejected;
    tool_search_filtered_out
        && native_mctc_supported == Some(true)
        && mctc_gate
        && !mcs_rejected
        && !tool_change_header_refused
        && keep_deferred_tools
        && pool_gate
}

fn wrap_system_reminder_source(
    source: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
    let mut units = "<system-reminder>\n".encode_utf16().collect::<Vec<_>>();
    units.extend(
        source
            .string_units("")
            .expect("reminder source must be a JSON string"),
    );
    units.extend("\n</system-reminder>".encode_utf16());
    lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
        String::from_utf16_lossy(&units),
        units,
    )
    .expect("wrapped reminder is a root string")
}

fn source_user_meta(
    id: MessageId,
    source: lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> ConversationMessage {
    let units = source
        .string_units("")
        .expect("model-visible reminder must be a JSON string");
    ConversationMessage::user_meta_js_utf16(id, String::from_utf16_lossy(&units), units)
}

fn system_reminder_inner_source(
    message: &ConversationMessage,
) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
    use lingxi_core::types::ContentBlock;
    let ConversationMessage::User { content, .. } = message else {
        return None;
    };
    let source = match content.first()? {
        ContentBlock::Text { text, .. } => {
            lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                text.clone(),
                text.encode_utf16().collect(),
            )
            .ok()?
        }
        ContentBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            ..
        } => lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
            text.clone(),
            utf16_code_units.clone(),
        )
        .ok()?,
        _ => return None,
    };
    let units = source.string_units("")?;
    let prefix = "<system-reminder>\n".encode_utf16().collect::<Vec<_>>();
    let suffix = "\n</system-reminder>".encode_utf16().collect::<Vec<_>>();
    let inner = units
        .strip_prefix(prefix.as_slice())?
        .strip_suffix(suffix.as_slice())?
        .to_vec();
    lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
        String::from_utf16_lossy(&inner),
        inner,
    )
    .ok()
}

fn prompt_ingress_payload(
    message: ConversationMessage,
    payload: lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> Option<crate::request_context::PromptIngressAttachment> {
    let rendered = crate::request_context::rendered_system_message(&message)?;
    Some(crate::request_context::PromptIngressAttachment {
        message,
        payload,
        rendered,
    })
}

/// Build Native `mls` tool-change blocks from one typed deferred-tools delta.
/// The pre-deferral wire snapshot is captured by the same request build, so
/// this never serializes a schema by rerunning a tool prompt producer.
pub(crate) fn native_tool_change_api_system_message(
    attachment: &crate::request_context::PromptIngressAttachment,
    complete_wire: &[lingxi_core::host::SourceToolWireDefinition],
) -> Option<ConversationMessage> {
    use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
    use lingxi_core::types::{ApiSystemMessage, ContentBlock, MessageId};

    if attachment.payload.string_at("/type") != Some("deferred_tools_delta") {
        return None;
    }
    let string_array = |path: &str| {
        (0..attachment.payload.array_len(path).unwrap_or_default())
            .filter_map(|index| attachment.payload.string_at(&format!("{path}/{index}")))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let mut removed = (0..attachment
        .payload
        .array_len("/retractedTools")
        .unwrap_or_default())
        .filter_map(|index| {
            attachment
                .payload
                .string_at(&format!("/retractedTools/{index}/name"))
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    removed.sort();
    removed.dedup();

    // `surfacedNames` becomes a by-value tool_addition only on Native's
    // toolSearchAbsent/pfr path. In the normal ToolSearch path Native records
    // the UUID as name-only instead; it is not a schema announcement.
    let mut added = if attachment
        .payload
        .at_pointer("/toolSearchAbsent")
        .and_then(|value| value.as_bool())
        == Some(true)
    {
        string_array("/surfacedNames")
    } else {
        Vec::new()
    };
    added.extend(string_array("/restoredNames"));
    added.sort();
    added.dedup();

    let definition_by_name = complete_wire
        .iter()
        .map(|definition| (definition.name.as_str(), definition))
        .collect::<std::collections::HashMap<_, _>>();
    let mut content = Vec::with_capacity(removed.len() + added.len());
    for name in removed {
        let reference = Source::object(vec![
            ("type".into(), Source::from("tool_reference")),
            ("name".into(), Source::from(name)),
        ])
        .ok()?;
        let block = Source::object(vec![
            ("type".into(), Source::from("tool_removal")),
            ("tool".into(), reference),
        ])
        .ok()?;
        content.push(ContentBlock::ProviderContent {
            protocol: "anthropic_messages".into(),
            value: block,
        });
    }
    for name in added {
        let tool = if let Some(definition) = definition_by_name.get(name.as_str()) {
            let mut fields = vec![
                ("name".into(), Source::from(definition.name.clone())),
                ("description".into(), definition.description.clone()),
                ("input_schema".into(), definition.input_schema.clone()),
            ];
            if definition.strict {
                fields.push(("strict".into(), Source::parse("true").ok()?));
            }
            let definition = Source::object(fields).ok()?;
            Source::object(vec![
                ("type".into(), Source::from("tool_definition")),
                ("definition".into(), definition),
            ])
            .ok()?
        } else {
            Source::object(vec![
                ("type".into(), Source::from("tool_reference")),
                ("name".into(), Source::from(name)),
            ])
            .ok()?
        };
        let block = Source::object(vec![
            ("type".into(), Source::from("tool_addition")),
            ("tool".into(), tool),
        ])
        .ok()?;
        content.push(ContentBlock::ProviderContent {
            protocol: "anthropic_messages".into(),
            value: block,
        });
    }
    (!content.is_empty()).then(|| {
        ConversationMessage::api_system(
            MessageId::new(),
            ApiSystemMessage {
                content,
                output_config: None,
            },
        )
    })
}

/// Persist the UUID latch when Native `uls` would classify at least one
/// surfaced name as only announced in ToolSearch text. Native stores the UUID
/// separately in a `deferred_tools_record` attachment.
pub(crate) fn deferred_tools_name_only_record_attachment(
    deferred_delta: &crate::request_context::PromptIngressAttachment,
    deferral: &tool_api::DeferralState,
    use_tool_search: bool,
    mid_conversation_tool_changes_active: bool,
    inline_tool_names: &std::collections::HashSet<String>,
    api_system_tool_addition_names: &std::collections::HashSet<String>,
) -> Option<crate::request_context::PromptIngressAttachment> {
    use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;

    let surfaced_count = deferred_delta.payload.array_len("/surfacedNames")?;
    if !use_tool_search
        || deferred_delta
            .payload
            .at_pointer("/toolSearchAbsent")
            .and_then(|value| value.as_bool())
            == Some(true)
        || surfaced_count == 0
    {
        return None;
    }
    let name_only = !mid_conversation_tool_changes_active
        || (0..surfaced_count).any(|index| {
            deferred_delta
                .payload
                .string_at(&format!("/surfacedNames/{index}"))
                .is_some_and(|name| {
                    !inline_tool_names.contains(name)
                        && !api_system_tool_addition_names.contains(name)
                })
        });
    if !name_only {
        return None;
    }
    let delta_uuid = deferred_delta.message.id().to_string();
    deferral.mark_name_only_announcement_uuid(delta_uuid.clone());
    let payload = Source::object(vec![
        ("type".into(), Source::from("deferred_tools_record")),
        (
            "nameOnlyAnnouncements".into(),
            Source::array(vec![Source::from(delta_uuid)]).ok()?,
        ),
    ])
    .ok()?;
    let id = MessageId::new();
    let message = ConversationMessage::user_meta(id, String::new());
    let rendered = Source::parse("[]").ok()?;
    Some(crate::request_context::PromptIngressAttachment {
        message,
        payload,
        rendered,
    })
}

/// Names already surfaced through API-system tool additions in the current
/// request, including references without a by-value schema. Native `uls` does
/// not classify those names as ToolSearch-only announcements.
pub(crate) fn native_tool_change_addition_names(
    messages: &[ConversationMessage],
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for message in messages {
        let ConversationMessage::System {
            api_system: Some(api_system),
            ..
        } = message
        else {
            continue;
        };
        for block in &api_system.content {
            let lingxi_core::types::ContentBlock::ProviderContent { protocol, value } = block
            else {
                continue;
            };
            if protocol != "anthropic_messages" || value.string_at("/type") != Some("tool_addition")
            {
                continue;
            }
            match value.string_at("/tool/type") {
                Some("tool_reference") => {
                    if let Some(name) = value.string_at("/tool/name") {
                        names.insert(name.to_owned());
                    }
                }
                Some("tool_definition") => {
                    if let Some(name) = value.string_at("/tool/definition/name") {
                        names.insert(name.to_owned());
                    }
                }
                _ => {}
            }
        }
    }
    names
}

fn trim_source_root_string(
    source: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
    let units = source.string_units("")?;
    let is_space = |unit: u16| char::from_u32(u32::from(unit)).is_some_and(char::is_whitespace);
    let start = units
        .iter()
        .position(|unit| !is_space(*unit))
        .unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|unit| !is_space(*unit))
        .map_or(start, |index| index + 1);
    if start == end {
        return None;
    }
    let trimmed = units[start..end].to_vec();
    Some(
        lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&trimmed),
            trimmed,
        )
        .expect("trimmed summary remains a root string"),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NativeRouteFailure {
    Unexamined,
    PastLink,
}

struct NativeRouteForms {
    requested: std::path::PathBuf,
    spellings: Vec<std::path::PathBuf>,
    landing: Option<std::path::PathBuf>,
    stopped_at: std::path::PathBuf,
    failure: Option<NativeRouteFailure>,
}

async fn cancellable_read_io<T>(
    cancel: Option<&lingxi_core::host::CancellationToken>,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    match cancel {
        Some(cancel) => tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = future => Some(result),
        },
        None => Some(future.await),
    }
}

fn absolute_normalized_read_path(path: &std::path::Path) -> Option<std::path::PathBuf> {
    // Every supported in-tree source producer records an absolute route
    // (file tools resolve against the session cwd; memory, SDK, and restored
    // attachment paths are absolute). Do not guess the process cwd for a
    // route-less relative value: it can differ from the active session cwd.
    if !path.is_absolute() {
        return None;
    }
    let absolute = path.to_path_buf();
    let mut normalized = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Native path.resolve clamps `..` at the volume root. Only
                // pop a real path component, never the root/prefix itself.
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            std::path::Component::Normal(part) => normalized.push(part),
        }
    }
    Some(normalized)
}

fn native_read_root_and_components(
    path: &std::path::Path,
) -> (
    std::path::PathBuf,
    std::collections::VecDeque<std::ffi::OsString>,
) {
    let mut root = std::path::PathBuf::new();
    let mut pending = std::collections::VecDeque::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => root.push(prefix.as_os_str()),
            std::path::Component::RootDir => root.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !root.pop() {
                    pending.push_back(component.as_os_str().to_os_string());
                }
            }
            std::path::Component::Normal(part) => pending.push_back(part.to_os_string()),
        }
    }
    (root, pending)
}

fn push_native_route_path(
    paths: &mut Vec<std::path::PathBuf>,
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
    path: std::path::PathBuf,
) {
    if seen.insert(path.clone()) {
        paths.push(path);
    }
}

async fn native_read_route_forms(
    path: &std::path::Path,
    cancel: Option<&lingxi_core::host::CancellationToken>,
) -> Option<NativeRouteForms> {
    const MAX_HOPS: usize = 64;
    let requested = absolute_normalized_read_path(path)?;
    let mut spellings = vec![requested.clone()];
    let mut seen_spellings = std::collections::HashSet::from([requested.clone()]);
    let (mut current, mut pending) = native_read_root_and_components(&requested);
    let mut seen_links =
        std::collections::HashSet::<(std::path::PathBuf, Vec<std::ffi::OsString>)>::new();
    let mut hops = 0;
    let mut saw_link = false;
    let mut stopped_at = requested.clone();

    while let Some(component) = pending.pop_front() {
        if cancel.is_some_and(lingxi_core::host::CancellationToken::is_cancelled) {
            return None;
        }
        if hops >= MAX_HOPS {
            return Some(NativeRouteForms {
                requested,
                spellings,
                landing: None,
                stopped_at,
                failure: Some(NativeRouteFailure::PastLink),
            });
        }
        let candidate = current.join(&component);
        stopped_at.clone_from(&candidate);
        let metadata = cancellable_read_io(cancel, tokio::fs::symlink_metadata(&candidate)).await?;
        match metadata {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                saw_link = true;
                let tail = pending.iter().cloned().collect::<Vec<_>>();
                if !seen_links.insert((candidate.clone(), tail.clone())) {
                    return Some(NativeRouteForms {
                        requested,
                        spellings,
                        landing: None,
                        stopped_at,
                        failure: Some(NativeRouteFailure::PastLink),
                    });
                }
                hops += 1;
                let target =
                    match cancellable_read_io(cancel, tokio::fs::read_link(&candidate)).await? {
                        Ok(target) => target,
                        Err(_) => {
                            return Some(NativeRouteForms {
                                requested,
                                spellings,
                                landing: None,
                                stopped_at,
                                failure: Some(NativeRouteFailure::PastLink),
                            });
                        }
                    };
                let leaf = tail.is_empty();
                let mut next = if target.is_absolute() {
                    target
                } else {
                    current.join(target)
                };
                for part in tail {
                    next.push(part);
                }
                let next = absolute_normalized_read_path(&next)?;
                if leaf {
                    push_native_route_path(&mut spellings, &mut seen_spellings, next.clone());
                }
                (current, pending) = native_read_root_and_components(&next);
            }
            Ok(_) => current.push(component),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                let mut landing = candidate;
                for part in pending.drain(..) {
                    landing.push(part);
                }
                let landing = absolute_normalized_read_path(&landing)?;
                push_native_route_path(&mut spellings, &mut seen_spellings, landing.clone());
                return Some(NativeRouteForms {
                    requested,
                    spellings,
                    landing: Some(landing),
                    stopped_at,
                    failure: None,
                });
            }
            Err(error) => {
                let loop_error = cfg!(unix) && error.raw_os_error() == Some(40);
                return Some(NativeRouteForms {
                    requested,
                    spellings,
                    landing: None,
                    stopped_at,
                    failure: Some(if saw_link || loop_error {
                        NativeRouteFailure::PastLink
                    } else {
                        NativeRouteFailure::Unexamined
                    }),
                });
            }
        }
    }

    let landing = current;
    push_native_route_path(&mut spellings, &mut seen_spellings, landing.clone());
    Some(NativeRouteForms {
        requested,
        spellings,
        landing: Some(landing),
        stopped_at,
        failure: None,
    })
}

async fn native_unix_sge_landing(
    path: &std::path::Path,
    cancel: Option<&lingxi_core::host::CancellationToken>,
) -> Option<std::path::PathBuf> {
    const MAX_HOPS: usize = 40;
    let mut remaining = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_os_string()),
            std::path::Component::ParentDir => Some("..".into()),
            std::path::Component::Prefix(prefix) => Some(prefix.as_os_str().to_os_string()),
            std::path::Component::RootDir | std::path::Component::CurDir => None,
        })
        .collect::<std::collections::VecDeque<_>>();
    let mut current = std::path::PathBuf::from(std::path::MAIN_SEPARATOR.to_string());
    let mut hops = 0;
    while !remaining.is_empty() && hops <= MAX_HOPS {
        if cancel.is_some_and(lingxi_core::host::CancellationToken::is_cancelled) {
            return None;
        }
        let component = remaining.pop_front()?;
        let candidate = if component == ".." {
            current.parent().unwrap_or(&current).to_path_buf()
        } else {
            current.join(&component)
        };
        let metadata =
            match cancellable_read_io(cancel, tokio::fs::symlink_metadata(&candidate)).await? {
                Ok(metadata) => metadata,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    if hops == 0 || remaining.len() == 0 {
                        let mut result = current;
                        result.push(component);
                        for part in remaining {
                            result.push(part);
                        }
                        return Some(result);
                    }
                    return None;
                }
                Err(_) => return None,
            };
        let target = if metadata.file_type().is_symlink() {
            let target = match cancellable_read_io(cancel, tokio::fs::read_link(&candidate)).await?
            {
                Ok(target) => target,
                Err(_) => return None,
            };
            hops += 1;
            target
        } else {
            std::path::PathBuf::new()
        };
        let is_link = metadata.file_type().is_symlink();
        let next = if is_link {
            if target.is_absolute() {
                std::path::PathBuf::from(std::path::MAIN_SEPARATOR.to_string())
            } else {
                current.clone()
            }
        } else {
            candidate
        };
        if is_link {
            let mut target_parts = target
                .components()
                .filter_map(|part| match part {
                    std::path::Component::Normal(value) => Some(value.to_os_string()),
                    std::path::Component::ParentDir => Some("..".into()),
                    _ => None,
                })
                .collect::<std::collections::VecDeque<_>>();
            target_parts.append(&mut remaining);
            remaining = target_parts;
        }
        current = next;
    }
    if hops > MAX_HOPS {
        None
    } else {
        Some(current)
    }
}

async fn native_changed_file_read_path_set(
    path: &std::path::Path,
    cancel: Option<&lingxi_core::host::CancellationToken>,
) -> Option<Vec<std::path::PathBuf>> {
    const MAX_SET: usize = 32;
    let requested = absolute_normalized_read_path(path)?;
    let mut paths = vec![requested.clone()];
    let mut seen = std::collections::HashSet::from([requested.clone()]);
    let mut index = 0;
    while index < paths.len() {
        if cancel.is_some_and(lingxi_core::host::CancellationToken::is_cancelled) {
            return None;
        }
        let candidate = paths[index].clone();
        let forms = native_read_route_forms(&candidate, cancel).await?;
        if native_changed_file_route_is_unresolved(&forms, cancel).await? {
            return None;
        }
        for spelling in forms.spellings {
            push_native_route_path(&mut paths, &mut seen, spelling);
        }
        if forms.failure.is_some() {
            push_native_route_path(&mut paths, &mut seen, candidate.clone());
        } else {
            push_native_route_path(&mut paths, &mut seen, forms.landing?);
        }
        #[cfg(unix)]
        let extra = if candidate == requested && paths.len() > 1 {
            native_unix_sge_landing(&requested, cancel).await
        } else {
            Some(candidate)
        };
        #[cfg(not(unix))]
        let extra = Some(candidate);
        let extra = extra?;
        if paths.len() > MAX_SET {
            return None;
        }
        push_native_route_path(&mut paths, &mut seen, extra);
        index += 1;
    }
    Some(paths)
}

async fn native_changed_file_route_is_unresolved(
    forms: &NativeRouteForms,
    cancel: Option<&lingxi_core::host::CancellationToken>,
) -> Option<bool> {
    if forms.failure.is_none() {
        return Some(false);
    }
    let root = forms.stopped_at.ancestors().last()?;
    if forms.failure == Some(NativeRouteFailure::Unexamined)
        && forms.requested.is_absolute()
        && forms.stopped_at.parent() == Some(root)
    {
        let root_accessible =
            cancellable_read_io(cancel, tokio::fs::symlink_metadata(root)).await?;
        // Native Cne suppresses an unexamined launch-root route only when the
        // root itself is readable; failure to inspect that root is not treated
        // as a blanket rejection.
        return Some(root_accessible.is_ok());
    }
    Some(true)
}

async fn attach_async_hook_response(
    orch: &ConversationOrchestrator,
    text: ExactHookText,
    hook_event: Option<String>,
    publication_guard: Option<Arc<dyn HookPublicationGuard>>,
) -> Option<crate::prompt::async_hook_response::GuardedAsyncHookReminder> {
    let content = crate::prompt::async_hook_response::render_reminder(&[text])?;
    let message = crate::prompt::async_hook_response::user_meta_message(MessageId::new(), content);
    let origin = hook_event.map(|event| serde_json::json!({"kind":"hook","event":event}));
    let attached = if let Some(origin) = origin {
        let work = orch.mod_prompt_attachment("async_hook_response", message, origin);
        if let Some(guard) = publication_guard.clone() {
            crate::prompt::async_hook_response::run_hook_prompt_work(guard, work).await??
        } else {
            work.await?
        }
    } else {
        message
    };
    Some(
        crate::prompt::async_hook_response::GuardedAsyncHookReminder {
            message: attached,
            publication_guard,
        },
    )
}

impl ConversationOrchestrator {
    pub(crate) async fn register_mod_persisted_attachment(
        &self,
        message: &ConversationMessage,
        kind: &str,
        origin: serde_json::Value,
    ) {
        self.prompt_runtime
            .mod_persisted_attachments
            .lock()
            .await
            .insert(message.id(), (kind.to_owned(), origin));
    }

    /// Screen original attachment renderings only in a model request copy.
    /// Session history and its JSONL attachment record remain unchanged.
    pub(crate) async fn screen_mod_persisted_attachments(
        &self,
        messages: &mut Vec<ConversationMessage>,
    ) {
        let pending = {
            let catalog = self.prompt_runtime.mod_persisted_attachments.lock().await;
            if catalog.is_empty() {
                return;
            }
            std::mem::take(messages)
                .into_iter()
                .map(|message| {
                    let descriptor = catalog.get(&message.id()).cloned();
                    (message, descriptor)
                })
                .collect::<Vec<_>>()
        };
        for (message, descriptor) in pending {
            if let Some((kind, origin)) = descriptor {
                if let Some(rendered) = self.mod_prompt_attachment(&kind, message, origin).await {
                    messages.push(rendered);
                }
            } else {
                messages.push(message);
            }
        }
    }

    /// Let Mods rewrite one outgoing attachment before its engine framing is
    /// sent to the model. Persistent callers retain the original message in
    /// history and use [`Self::screen_mod_persisted_attachments`] at render.
    pub(crate) async fn mod_prompt_attachment(
        &self,
        kind: &str,
        message: ConversationMessage,
        origin: serde_json::Value,
    ) -> Option<ConversationMessage> {
        self.mod_prompt_attachment_with_detail(kind, message, origin, None)
            .await
    }

    /// `/brief` toggle reminder, consumed once by the next model call.
    ///
    /// The visible command status is rendered by the host immediately, while
    /// Claude Code also sends this transient meta message to the model so the
    /// next response uses the newly selected output channel. Startup
    /// `--brief` does not queue a reminder; only the interactive toggle does.
    pub(crate) fn brief_mode_reminder_message(&self) -> Option<ConversationMessage> {
        lingxi_core::host::session_flags::take_brief_mode_reminder()
            .map(|content| ConversationMessage::user_meta(MessageId::new(), content.to_string()))
    }

    /// OUTSTYLE.3: the byte-exact per-turn output-style reminder, or `None` when
    /// the default style is active.
    ///
    /// 1:1 with claude-code's `output_style` attachment. On EVERY turn where
    /// `settings.outputStyle != 'default'`, claude-code injects a meta user
    /// message into the model's input: `getOutputStyleAttachment`
    /// (`attachments.ts:1597-1612`) → `normalizeAttachmentForAPI`'s
    /// `'output_style'` case (`messages.ts:3797-3811`), which wraps
    /// `` `${outputStyle.name} output style is active. Remember to follow the
    /// specific guidelines for this style.` `` via `wrapInSystemReminder`
    /// (`messages.ts:3097-3099`, literally `` `<system-reminder>\n${content}\n</system-reminder>` ``).
    /// `outputStyle.name` is the builtin's `OUTPUT_STYLE_CONFIG[style].name`
    /// (`"Explanatory"` / `"Learning"`), here the resolved
    /// [`outputstyles::BuiltinOutputStyle::name`].
    ///
    /// Returns `None` for the `None`/`"default"`/unknown style (the same gate as
    /// the system-prompt section above), so the styleless path stays
    /// byte-identical and the locked turn-loop + streaming fixtures stay green.
    ///
    /// The reminder is a plain user-text [`ConversationMessage`] carrying the
    /// byte-exact string. The fresh [`MessageId`] is irrelevant: callers append
    /// this ONLY to the per-turn outgoing message snapshot, never to
    /// `session.history` nor JSONL, so it is TRANSIENT and never accumulates —
    /// its `isMeta` state is therefore immaterial (nothing persists it)
    /// (TS recomputes the attachment each turn — see `query.ts` mid-turn
    /// `getAttachmentMessages`). Position mirrors TS: the caller appends it as a
    /// trailing meta user message after the user prompt / tool-results
    /// (`processTextPrompt` returns `[userMessage, ...attachmentMessages]`;
    /// `query.ts:1580-1590` pushes the attachment after `toolResults`).
    pub(crate) async fn output_style_reminder_message(&self) -> Option<ConversationMessage> {
        let resolved = self.resolve_active_output_style().await?;
        // 2.1.238 renderer (`Cqm.output_style`, table @296733172):
        //
        // ```js
        // output_style:(e)=>{if(typeof e.style!=="string"||e.style==="")return[];
        //  if(e.style.length>gFn)return T(`Output style name exceeds ${gFn} characters (${e.style.length}); suppressing its per-turn reminder`,{level:"error"}),[];
        //  return Zy([kn({content:`${pze(e.style)} output style is active. ${e.turnReminder??"Remember to follow the specific guidelines for this style."}`,isMeta:!0})])},
        // ```
        //
        // The empty-name arm is already covered by `resolve_active_output_style`
        // (`None` for default/unknown). `gFn = 256` (@285128933) and the `pze`
        // escape are new in 2.1.238; `e.style.length` is UTF-16 code units.
        let name = resolved.name.as_str();
        if name.is_empty() {
            return None;
        }
        let name_len = name.encode_utf16().count();
        if name_len > crate::prompt::sanitize::MAX_OUTPUT_STYLE_NAME_LEN {
            tracing::error!(
                "Output style name exceeds {} characters ({name_len}); suppressing its per-turn reminder",
                crate::prompt::sanitize::MAX_OUTPUT_STYLE_NAME_LEN
            );
            return None;
        }
        // The renderer is
        // `` `${escape(style)} output style is active. ${e.turnReminder ??
        //    "Remember to follow the specific guidelines for this style."}` ``.
        //
        // This file used to argue at length that the fallback was the ONLY
        // reachable arm, because the two styles carrying a `turnReminder`
        // (`Proactive`, `Concise`) were not ported. They are now, so the arm is
        // live and the style's own sentence wins.
        //
        // A DISK or PLUGIN style still renders the fallback: `turnReminder` is
        // not a frontmatter key upstream, so a file cannot supply one — see
        // `ResolvedOutputStyle::turn_reminder`.
        let reminder = resolved
            .turn_reminder
            .unwrap_or("Remember to follow the specific guidelines for this style.");
        let content = format!(
            "<system-reminder>\n{} output style is active. {reminder}\n</system-reminder>",
            crate::prompt::sanitize::escape_reminder_text(name)
        );
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// SKILLLIST.1: the `skill_listing` model message, or `None` when no
    /// provider is wired, the `Skill` tool is absent this turn, or there are no
    /// model-invocable skills. Initial regular-user ingress persists its typed
    /// payload and rendered cache; later model steps use the message-only
    /// wrapper below.
    ///
    /// 1:1 with claude-code's `skill_listing` attachment: `getSkillToolCommands`
    /// (`commands.ts:565`) selects the eligible skills, `formatCommandsWithinBudget`
    /// (`SkillTool/prompt.ts`) renders them within a ~1%-of-context char budget,
    /// and `normalizeAttachmentForAPI`'s `'skill_listing'` case
    /// (`messages.ts:3728-3738`) wraps the body in a `<system-reminder>` meta
    /// user message: `"The following skills are available for use with the Skill
    /// tool:\n\n{listing}"`. The Skill-tool gate mirrors `attachments.ts:2668`.
    ///
    /// The model-facing message remains a meta user row. The initial regular-user
    /// ingress stores a typed `skill_listing` attachment with its exact rendered
    /// cache; later tool steps do not regenerate the initial row. `None` keeps
    /// the styleless/skilless path byte-identical and the locked fixtures green.
    ///
    /// DELTA (SKILLLIST.1): turn-0 emits the FULL listing; each later turn emits
    /// ONLY skills that have NOT appeared in a prior turn's reminder, tracked via
    /// [`Self::sent_skill_names`]. When no new skill appears, returns `None` (no
    /// reminder that turn). 1:1 with TS `sentSkillNames` (attachments.ts:2607,
    /// 2699): the budgeter still runs over the delta subset, so the rendered
    /// bytes match what TS would send for that turn's new-skill set.
    /// The per-turn, transient plan-mode reminder (206 `plan_mode` attachment,
    /// builder `xEg`), or `None` when plan mode is not active.
    ///
    /// 1:1 with the binary's `xEg(t)`: gated on `permissionMode === "plan"`
    /// (here `session.plan_mode`), it returns the `{type:"plan_mode",
    /// reminderType, isSubAgent, planFilePath, planExists, ...customInstructions}`
    /// attachment which `KJn` assembles (`l=await xEg(t)`) BEFORE the
    /// invoked-skills bodies (`REg`) and the tool/mcp deltas (`gYt`/`YJn`), i.e.
    /// before the skill-listing reminder in the port's per-turn sequence.
    ///
    /// CADENCE (2.1.238 `X4T` @296525982, `txl` @296558044 — see
    /// [`PlanReminderCadence`]): at most ONE plan-mode reminder per
    /// [`PLAN_TURNS_BETWEEN_ATTACHMENTS`] real user turns, and every
    /// [`PLAN_FULL_REMINDER_EVERY_N_ATTACHMENTS`]th emitted attachment is the
    /// FULL body (`c % 5 === 1` ⇒ #1, #6, #11 … full; the rest sparse). The port
    /// previously emitted on EVERY model call, full exactly once and sparse
    /// forever after — which both over-fired and never returned to the full body.
    ///
    /// `isSubAgent` is ALWAYS `false` here: subagents never run through
    /// `ConversationOrchestrator` (every orchestrator is a depth-0 main thread),
    /// so the `H5T` variant is unreachable via this path. Appended ONLY to the
    /// per-turn OUTGOING snapshot (never `session.history` / JSONL) so it never
    /// accumulates; `None` keeps the locked turn-loop fixtures byte-identical
    /// (default: plan mode OFF).
    /// Upstream returns a LIST here (`J_s`): a `plan_mode_reentry` attachment
    /// may precede the `plan_mode` one on the entry that finds an existing plan
    /// file, and both sit behind the SAME cadence gate — `lyr`'s early return
    /// runs before the reentry push, so a suppressed turn emits neither.
    pub(crate) async fn plan_mode_turn_messages(&self) -> Vec<ConversationMessage> {
        let (path, exists, real_user_turns, entered_plan_mode, reentry) = {
            let mut s = self.session.lock().await;
            if !s.plan_mode {
                return Vec::new();
            }
            let path = self.session_plan_file_path(&s.session_id);
            let exists = std::path::Path::new(&path).exists();
            // `ixl`'s turn counter: non-meta user messages carrying NO
            // `tool_result` block. Tool-result continuations within one turn are
            // NOT turns, so the cadence gate holds the reminder for the whole
            // multi-step turn rather than re-firing on every model call.
            let real_user_turns = s
                .history
                .iter()
                .filter(|m| match m {
                    ConversationMessage::User {
                        content,
                        is_meta: false,
                        ..
                    } => !content
                        .iter()
                        .any(|b| matches!(b, lingxi_core::types::ContentBlock::ToolResult { .. })),
                    _ => false,
                })
                .count();
            // `plan_reminder_shown == false` marks a fresh plan-mode ENTRY
            // (`EnterPlanMode` / `handle_impl` clear it), i.e. the
            // `plan_mode_exit` boundary `Y4T` stops counting at.
            let entered_plan_mode = !s.plan_reminder_shown;
            s.plan_reminder_shown = true;
            // `if(nPt()&&y!==null){C.push({type:"plan_mode_reentry",…}),NM(!1)}`
            // — one reentry reminder per exit→enter cycle, and only when a plan
            // file from the previous session is actually on disk. A missing file
            // leaves the flag set for the next entry, exactly as upstream does.
            let reentry = s.plan_mode_exited && exists;
            if reentry {
                s.plan_mode_exited = false;
            }
            (path, exists, real_user_turns, entered_plan_mode, reentry)
        };
        // Decide emission + full/sparse under the cadence lock so two concurrent
        // turns cannot both render attachment `c`.
        let sparse = {
            let mut c = self.prompt_runtime.plan_reminder_cadence.lock().await;
            if entered_plan_mode {
                *c = PlanReminderCadence::default();
            }
            if let Some(last) = c.real_user_turns_at_last_emission {
                // `if(_ && y < TURNS_BETWEEN_ATTACHMENTS) return []`
                if real_user_turns.saturating_sub(last) < PLAN_TURNS_BETWEEN_ATTACHMENTS {
                    return Vec::new();
                }
            }
            c.attachments_emitted += 1;
            c.real_user_turns_at_last_emission = Some(real_user_turns);
            // `c % FULL_REMINDER_EVERY_N_ATTACHMENTS === 1 ? "full" : "sparse"`
            c.attachments_emitted % PLAN_FULL_REMINDER_EVERY_N_ATTACHMENTS != 1
        };
        let params = crate::prompt::plan_reminder::PlanReminderParams {
            plan_file_path: &path,
            plan_exists: exists,
            // C5: `--plan-mode-instructions` custom workflow body (borrows from
            // `self.config`, which outlives `params`; the session guard is already
            // dropped). `None` ⇒ the default 5-phase reminder.
            custom_instructions: self.config.plan_mode_instructions.as_deref(),
            is_subagent: false,
            reminder_type_sparse: sparse,
            // `zx()==="default"` — an unset `output_style` IS the default style.
            output_style_is_default: self.config.output_style.as_ref().is_none_or(|style| {
                style
                    .string_units("")
                    .is_some_and(|units| units == "default".encode_utf16().collect::<Vec<_>>())
            }),
        };
        // All three plan-mode renderers (`M5T` full / `L5T` sparse / `H5T`
        // subagent) return through the batch wrapper `Zy` (2.1.238 @296675470),
        // which maps `NT` = `` `<system-reminder>\n${e}\n</system-reminder>` ``
        // (@296673554) over every message and marks it `isMeta:!0`. The body
        // renderer stays pure (its byte-exact unit tests pin the bare body); the
        // envelope is applied here, exactly as the other per-turn reminders do.
        let body = crate::prompt::plan_reminder::render_plan_mode_reminder(&params);
        let content = format!("<system-reminder>\n{body}\n</system-reminder>");
        let mut out = Vec::with_capacity(2);
        if reentry {
            let reentry_body = crate::prompt::plan_reminder::render_plan_mode_reentry(&path);
            out.push(ConversationMessage::user_meta(
                MessageId::new(),
                format!("<system-reminder>\n{reentry_body}\n</system-reminder>"),
            ));
        }
        out.push(ConversationMessage::user_meta(MessageId::new(), content));
        out
    }

    /// The `plan_mode_exit` reminder — 2.1.266 `Z_s`:
    ///
    /// ```js
    /// async function Z_s(e,n){if(ue(n).mode==="plan")return Vz(!1),[];
    ///   let{foundPlanModeAttachment:r}=lyr(e??[]);
    ///   if(!n$n()&&!r)return[];
    ///   Vz(!1);
    ///   let o=ay(n.agentId),d=zF(n.agentId)!==null;
    ///   return[{type:"plan_mode_exit",planFilePath:o,planExists:d}]}
    /// ```
    ///
    /// Still in plan mode ⇒ clear the pending flag and emit nothing. Otherwise
    /// emit when the flag is set. The `!r` half of upstream's guard (a
    /// `plan_mode` attachment still visible in history even with no pending
    /// flag) is not reproduced: LingXi's plan-mode reminders live only in the
    /// per-turn outgoing snapshot, never in `session.history`, so there is no
    /// history to scan — the flag is the only witness.
    pub(crate) async fn plan_mode_exit_message(&self) -> Option<ConversationMessage> {
        let (path, exists) = {
            let mut s = self.session.lock().await;
            if s.plan_mode {
                s.plan_mode_exit_pending = false;
                return None;
            }
            if !s.plan_mode_exit_pending {
                return None;
            }
            s.plan_mode_exit_pending = false;
            let path = self.session_plan_file_path(&s.session_id);
            let exists = std::path::Path::new(&path).exists();
            (path, exists)
        };
        let body = crate::prompt::plan_reminder::render_plan_mode_exit(&path, exists);
        Some(ConversationMessage::user_meta(
            MessageId::new(),
            format!("<system-reminder>\n{body}\n</system-reminder>"),
        ))
    }

    pub(crate) async fn skill_listing_prompt_ingress_attachment(
        &self,
    ) -> Option<crate::request_context::PromptIngressAttachment> {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;

        let provider = self.prompt_runtime.skill_listing.as_ref()?;
        // Gate on the Skill tool being available this turn (attachments.ts:2668).
        self.find_dispatchable_tool("Skill")?;
        let listing_snapshot = provider.skill_listing_snapshot().await;
        let entries = listing_snapshot.entries;

        // DELTA: keep only skills not yet sent this session, then record them as
        // sent. Turn 0 keeps everything (the set is empty); subsequent turns keep
        // only newly-appeared names. An empty delta ⇒ no reminder this turn.
        let (new_entries, is_initial): (
            Vec<crate::prompt::skill_listing::SkillListingEntry>,
            bool,
        ) = {
            let mut sent = self.prompt_runtime.sent_skill_names.lock().await;
            let is_initial = sent.is_empty();
            let delta: Vec<_> = entries
                .into_iter()
                .filter(|entry| {
                    let name_units = entry
                        .name
                        .string_units("")
                        .expect("skill listing names must be root strings");
                    !sent.contains(&name_units)
                })
                .collect();
            for entry in &delta {
                sent.insert(
                    entry
                        .name
                        .string_units("")
                        .expect("skill listing names must be root strings"),
                );
            }
            (delta, is_initial)
        };
        if new_entries.is_empty() {
            return None;
        }

        // ~1% of the active model's context window (TS getCharBudget). Resolved
        // with no betas — the small 200k↔1M budget delta only matters past ~30
        // skills, where the budgeter degrades gracefully. Read the LIVE model
        // (mutated by /model + resume), not the frozen boot `config.model`, so a
        // switch across a 200k↔1M window boundary re-sizes the budget correctly
        // (mirrors `build_prompt_context`).
        let model = self.session.lock().await.model.clone();
        let window =
            compaction::context_window::context_window_for_model(&model, &self.api.active_betas())
                as usize;
        let chars_per_token = memory::native_skill_listing_chars_per_token(Some(&model)) as usize;
        let settings = listing_snapshot.budget_settings;
        let rendered_source = crate::prompt::skill_listing::render_reminder(
            &new_entries,
            Some(window),
            chars_per_token,
            settings,
        )?;
        let rendered_units = rendered_source.string_units("")?;
        let prefix = "<system-reminder>\nThe following skills are available for use with the Skill tool:\n\n"
            .encode_utf16()
            .collect::<Vec<_>>();
        let suffix = "\n</system-reminder>".encode_utf16().collect::<Vec<_>>();
        let content_units = rendered_units
            .strip_prefix(prefix.as_slice())?
            .strip_suffix(suffix.as_slice())?
            .to_vec();
        let content =
            Source::root_string(String::from_utf16_lossy(&content_units), content_units).ok()?;
        let names =
            Source::array(new_entries.iter().map(|entry| entry.name.clone()).collect()).ok()?;
        let skill_count = Source::parse(&new_entries.len().to_string()).ok()?;
        let is_initial = Source::from_serde_value(&serde_json::Value::Bool(is_initial)).ok()?;
        let payload = Source::object(vec![
            ("type".into(), Source::from("skill_listing")),
            ("content".into(), content),
            ("skillCount".into(), skill_count),
            ("isInitial".into(), is_initial),
            ("names".into(), names),
        ])
        .ok()?;
        prompt_ingress_payload(source_user_meta(MessageId::new(), rendered_source), payload)
    }

    pub(crate) async fn skill_listing_reminder_message(&self) -> Option<ConversationMessage> {
        self.skill_listing_prompt_ingress_attachment()
            .await
            .map(|row| row.message)
    }

    /// Build the source-ordered, tool-gated listing rows used by the Native
    /// prompt-ingress attachment fan-out. They are persisted with their original
    /// rendering before Mods screen the request-local copies.
    pub(crate) async fn prompt_ingress_listing_attachments(
        &self,
    ) -> Vec<crate::request_context::PromptIngressAttachment> {
        let mut rows = Vec::new();
        for row in [
            self.agent_listing_prompt_ingress_attachment().await,
            self.skill_listing_prompt_ingress_attachment().await,
        ] {
            let Some(row) = row else {
                continue;
            };
            rows.push(row);
        }
        rows
    }

    /// The per-turn, transient `async_hook_response` reminder, or `None` when no
    /// source is wired or no background (`async`) hook has completed since the
    /// last turn.
    ///
    /// 1:1 with claude-code's `async_hook_response` attachment
    /// (`getAsyncHookResponseAttachments`, attachments.ts:3464 →
    /// `normalizeAttachmentForAPI`, messages.ts:4026): drains the completed
    /// background-hook responses (CONSUME-ONCE — TS `removeDeliveredAsyncHooks`)
    /// and wraps their `system_message` text (which already folds in any
    /// `additionalContext`) in one `<system-reminder>` meta user message. Like
    /// the skill-/agent-listing reminders it is appended ONLY to the per-turn
    /// OUTGOING snapshot, never `session.history` / JSONL, so it never
    /// accumulates. No delta set is needed — draining the source IS the dedup.
    /// Keep each completed settings hook's event attached to its own outgoing
    /// reminder so Mods receive `origin: {kind:'hook', event}`.
    pub(crate) async fn async_hook_response_mod_messages(
        &self,
    ) -> Vec<crate::prompt::async_hook_response::GuardedAsyncHookReminder> {
        let Some(provider) = self.prompt_runtime.async_hook_responses.as_ref() else {
            return Vec::new();
        };
        let mut output = Vec::new();
        let mut without_event = Vec::new();
        for response in provider.take_pending_with_events().await {
            let Some(publication_guard) = response.publication_guard else {
                if response.hook_event.is_none() {
                    without_event.push(response.text);
                    continue;
                }
                if let Some(reminder) =
                    attach_async_hook_response(self, response.text, response.hook_event, None).await
                {
                    output.push(reminder);
                }
                continue;
            };
            if let Some(reminder) = attach_async_hook_response(
                self,
                response.text,
                response.hook_event,
                Some(publication_guard),
            )
            .await
            {
                output.push(reminder);
            }
        }
        // Older providers cannot identify an origin event. Preserve their
        // established aggregate reminder without fabricating a hook origin.
        if let Some(content) = crate::prompt::async_hook_response::render_reminder(&without_event) {
            output.push(
                crate::prompt::async_hook_response::GuardedAsyncHookReminder {
                    message: crate::prompt::async_hook_response::user_meta_message(
                        MessageId::new(),
                        content,
                    ),
                    publication_guard: None,
                },
            );
        }
        output
    }

    /// T35: the per-turn `task-notification` reminders — one message per
    /// completion, empty when no source is wired or no background task finished
    /// since the last turn.
    ///
    /// Mirrors [`Self::async_hook_response_mod_messages`]: drains the
    /// registry's terminal-not-notified tasks (CONSUME-ONCE — the registry marks
    /// each `notified` + evicts on drain) and renders their `<task-notification>`
    /// blocks (claude-code's per-task-type `enqueue*Notification` formats) inside
    /// one `<system-reminder>` meta user message PER completion, each whose
    /// first line is the
    /// `NON_USER_INPUT_HEADER` provenance header (2.1.238 `b_a` @285068292,
    /// applied to every `task-notification`-origin user message so the model
    /// never treats a machine-generated completion as user consent; it also
    /// escapes any literal `</system-reminder>` in the task output so a task
    /// cannot close the envelope early). Each durable row is paired with a
    /// `queued_command` transcript attachment carrying the pre-envelope prompt
    /// and rendered cache, so hot requests and cold resume retain Native's typed
    /// attachment identity and ordering. Draining the registry remains the
    /// consume-once dedup boundary.
    ///
    /// REM-14 side effect: a terminal `dream` task in the drained batch is the
    /// port's only signal that the BACKGROUND MEMORY CONSOLIDATOR finished, so
    /// this is where the oracle's `setAppState({pendingMemoryUpdates:[…]})`
    /// enqueue lands. The queue is drained separately by
    /// [`Self::memory_update_reminder_messages`].
    ///
    /// Both turn drivers reach the drain through
    /// [`Self::task_notification_reminder_messages_in_turn`] (via the shared
    /// collector), which is where `in_human_turn` comes from; this
    /// `in_human_turn = false` wrapper has no production caller left and is
    /// kept for tests that drain the registry directly.
    ///
    /// `#[cfg(test)]` so that stays true: without it the method is dead in a
    /// non-test build (it has warned since the collector landed), and a future
    /// production caller would silently contradict the paragraph above instead
    /// of failing to compile.
    #[cfg(test)]
    pub(crate) async fn task_notification_reminder_messages(&self) -> Vec<ConversationMessage> {
        self.task_notification_reminder_messages_in_turn(false)
            .await
            .into_iter()
            .map(|row| row.message)
            .collect()
    }

    pub(crate) async fn task_notification_reminder_messages_in_turn(
        &self,
        in_human_turn: bool,
    ) -> Vec<crate::request_context::PromptIngressAttachment> {
        let Some(provider) = self.prompt_runtime.task_notifications.as_ref() else {
            return Vec::new();
        };
        let notifications = provider.take_pending_task_notifications().await;
        self.enqueue_memory_updates_from(&notifications);
        // ONE message per completion: claude-code's `ha(…)` enqueue runs once
        // per notification and the envelope is applied per message, so two
        // tasks finishing in the same turn are two user messages. Folding them
        // into one envelope also folded two provenance headers into one.
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;

        let rendered_rows = crate::prompt::task_notification::render_reminder_sources_in_turn(
            &notifications,
            in_human_turn,
        );
        let mut rows = Vec::with_capacity(rendered_rows.len());
        for (notification, (prompt, rendered)) in notifications.into_iter().zip(rendered_rows) {
            let id = MessageId::new();
            let outer_uuid = id.as_uuid();
            let source_uuid = loop {
                let candidate = uuid::Uuid::new_v4();
                if candidate != outer_uuid {
                    break candidate;
                }
            };
            let delivery_id = loop {
                let candidate = uuid::Uuid::new_v4();
                if candidate != outer_uuid && candidate != source_uuid {
                    break candidate;
                }
            };
            let mut payload_fields = vec![
                ("type".into(), Source::from("queued_command")),
                ("prompt".into(), prompt),
                ("source_uuid".into(), Source::from(source_uuid.to_string())),
                ("delivery_id".into(), Source::from(delivery_id.to_string())),
                ("commandMode".into(), Source::from("task-notification")),
            ];
            if let Some(task_delivery) = notification.task_delivery.as_ref() {
                payload_fields.push(("taskDelivery".into(), task_delivery.as_projection().clone()));
            }
            let payload = Source::object(payload_fields)
                .expect("task notification attachment has a valid Native shape");
            let message = source_user_meta(id, rendered);
            rows.push(
                prompt_ingress_payload(message, payload)
                    .expect("task notification rendering yields a system-role cache"),
            );
        }
        rows
    }

    /// Cap on [`Self::pending_memory_updates`]; oldest entries are dropped.
    ///
    /// Enqueue and drain are separate halves that used to sit on different
    /// drivers — the batched path enqueued without ever draining, so the queue
    /// could only grow. Both drivers now share one collector
    /// (`conversation/drivers/prepare.rs`), which drains a few lines after it
    /// enqueues, so in production the queue is emptied every model step. The
    /// bound stays as defence in depth: [`Self::enqueue_memory_updates_from`]
    /// is reached from
    /// [`Self::task_notification_reminder_messages_in_turn`], and a caller that
    /// drains notifications OUTSIDE the collector still enqueues here with
    /// nothing to empty it.
    pub(crate) const MAX_PENDING_MEMORY_UPDATES: usize = 8;

    /// REM-14 enqueue half: turn every terminal `dream` notification into a
    /// [`crate::prompt::memory_update::PendingMemoryUpdate`].
    ///
    /// `summary` is the dream agent's own final text (`result`) — the very
    /// thing `tasks/src/handlers/dream.rs`'s prompt asks it to return ("Return a
    /// brief summary of what you consolidated, updated, or pruned") — falling
    /// back to the task description when the agent returned nothing. A `failed`
    /// / `killed` dream is skipped: nothing was consolidated.
    pub(super) fn enqueue_memory_updates_from(
        &self,
        notifications: &[lingxi_core::host::task_registry::TaskNotification],
    ) {
        let fresh: Vec<crate::prompt::memory_update::PendingMemoryUpdate> = notifications
            .iter()
            .filter(|n| n.task_type == "dream" && n.status == "completed")
            .map(|n| crate::prompt::memory_update::PendingMemoryUpdate {
                source: crate::prompt::memory_update::MemoryUpdateSource::Dream,
                summary: n
                    .result
                    .as_ref()
                    .and_then(trim_source_root_string)
                    .unwrap_or_else(|| n.description.clone()),
            })
            .collect();
        if fresh.is_empty() {
            return;
        }
        let mut queue = self
            .prompt_runtime
            .pending_memory_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.extend(fresh);
        let overflow = queue.len().saturating_sub(Self::MAX_PENDING_MEMORY_UPDATES);
        if overflow > 0 {
            queue.drain(..overflow);
        }
    }

    /// REM-14 — the per-turn `memory_update` reminders: one meta message per
    /// queued background memory consolidation.
    ///
    /// 1:1 with the oracle producer `jzm(e)` @**296554545**:
    ///
    /// ```js
    /// function jzm(e){let t=e.getAppState().pendingMemoryUpdates;if(t.length===0)return[];
    ///   e.setAppState(…clear…);
    ///   let n=…memory index path…, o=XAa(e.session),
    ///       i=(s)=>s===n||o.has(s)||e.readFileState.has(s)||e.loadedNestedMemoryPaths?.[s]===!0;
    ///   return t.map((s)=>({type:"memory_update",source:s.source,summary:s.summary,
    ///                       paths:s.paths,inContextPaths:s.paths.filter(i)}))}
    /// ```
    ///
    /// * DRAIN — consume-once, exactly like the oracle's clear-on-read.
    /// * `paths` — the oracle's writer reports them; the port recomputes them by
    ///   listing the user memdir (the same directory the `# Memory` section
    ///   points the model at, `memory_prefetch.user_memdir()`) for entries whose
    ///   mtime is newer than the previous scan. `None` when no memdir is wired —
    ///   which is also when the memory feature itself is off, so nothing can have
    ///   been consolidated.
    /// * `inContextPaths` — [`crate::prompt::memory_update::select_in_context_paths`]
    ///   over `readFileState.has(s)`, the one arm of the oracle's `i` predicate
    ///   the port has (there is no separate memory-index file or session-memory
    ///   set here).
    /// * RENDER — [`crate::prompt::memory_update::render_memory_update`], wrapped
    ///   per update, matching `Zy([kn({content:o.join("\n"),isMeta:!0})])`.
    ///
    /// Silent in a stock session: nothing queues unless a `dream` task completes.
    pub(crate) async fn memory_update_reminder_messages(&self) -> Vec<ConversationMessage> {
        let pending: Vec<crate::prompt::memory_update::PendingMemoryUpdate> = {
            let mut queue = self
                .prompt_runtime
                .pending_memory_updates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *queue)
        };
        if pending.is_empty() {
            return Vec::new();
        }
        let Some(memdir) = self
            .prompt_runtime
            .memory_prefetch
            .as_ref()
            .and_then(|p| p.user_memdir())
            .map(std::path::Path::to_path_buf)
        else {
            return Vec::new();
        };

        // Which memdir files moved since the last scan.
        let since = self
            .prompt_runtime
            .last_memory_scan_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let now_ms = tool_api::read_file_state::mtime_ms_floor(std::time::SystemTime::now());
        self.prompt_runtime
            .last_memory_scan_ms
            .store(now_ms, std::sync::atomic::Ordering::Relaxed);
        let mut paths: Vec<String> = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&memdir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let Ok(modified) = meta.modified() else {
                    continue;
                };
                if tool_api::read_file_state::mtime_ms_floor(modified) > since {
                    paths.push(path.to_string_lossy().into_owned());
                }
            }
        }
        paths.sort();

        let in_context = {
            let guard = self
                .prompt_runtime
                .read_state_map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::prompt::memory_update::select_in_context_paths(&paths, |p| {
                guard.contains(std::path::Path::new(p))
            })
        };

        pending
            .into_iter()
            .map(|queued| {
                let body = crate::prompt::memory_update::render_memory_update(
                    &crate::prompt::memory_update::MemoryUpdate {
                        source: queued.source,
                        summary: queued.summary,
                        paths: paths.clone(),
                        in_context_paths: in_context.clone(),
                    },
                );
                source_user_meta(MessageId::new(), wrap_system_reminder_source(&body))
            })
            .collect()
    }

    /// Finding #73: the per-turn `todo_reminder` (V1) / `task_reminder` (V2)
    /// meta user message, or `None` when not eligible this turn.
    ///
    /// 1:1 with the binary's producer `()=>TE()?B4p(o,t):M4p(o,t)` (`ytl`,
    /// offset ~203087213). [`tool_task::reminder::select_mode`] picks V1 vs V2
    /// (`TE()`). For the selected variant this mirrors the `M4p`/`B4p` gates:
    /// 1. killswitch `wgo()!=="off"`;
    /// 2. the `Brief` tool (`rjn`/`SendUserMessage`) is ABSENT (present ⇒ skip);
    /// 3. the relevant tool is PRESENT this turn — `TodoWrite` (V1) /
    ///    `TaskUpdate` (V2);
    /// 4. the history is non-empty (`!e||e.length===0 ⇒ []`);
    /// 5. BOTH counters reach their thresholds (`turns_since_last_todo_write >=
    ///    TURNS_SINCE_WRITE && turns_since_last_reminder >= TURNS_BETWEEN_REMINDERS`).
    ///
    /// On fire it renders the body inside a `<system-reminder>` envelope as a
    /// META user message — the oracle's `Zy([kn({content:o,isMeta:!0})])`
    /// (2.1.238 @296690005 / @296690634, `Zy` @296675470 mapping `NT`
    /// @296673554) — and RESETS `turns_since_last_reminder` to
    /// `0`. V1 reads `session.todos`; V2 reads the wired
    /// [`crate::prompt::todo_reminder::TodoReminderTaskProvider`] (no provider ⇒
    /// base text only, an empty store). Appended ONLY to the per-turn OUTGOING
    /// snapshot (never `session.history` / JSONL) so it never accumulates.
    ///
    /// COUNTER NOTE: the binary recomputes the counters by scanning the message
    /// log for the last `TodoWrite`/`Task` tool_use and the last reminder
    /// ATTACHMENT. This engine never persists the reminder attachment, so the
    /// counters are tracked as explicit `SessionState` fields, incremented once
    /// per assistant turn (`bump_reminder_turn_counters`) and reset on the
    /// relevant tool call (`note_todo_reminder_tool_call`).
    pub(crate) async fn todo_reminder_message(&self) -> Option<ConversationMessage> {
        // (1) killswitch.
        if tool_task::reminder::is_killswitched() {
            return None;
        }
        // (2) Brief (`SendUserMessage`/`Brief`) present ⇒ skip (both variants).
        if self.find_dispatchable_tool("SendUserMessage").is_some() {
            return None;
        }

        // (2b) the `OO()` model gate. `select_mode` returns `None` when the
        // todo/task tools have been withdrawn, porting BOTH oracle guards
        // (`if(X_()||!OO())return[]` for V1, `if(!h3())return[]` for V2) — the
        // reminder must not describe tools the model was never offered. The
        // canonical main-loop model is the one the registry publishes, so this
        // and `available_tools` cannot disagree.
        let Some(mode) = tool_task::reminder::select_mode(self.tools.main_loop_model().as_deref())
        else {
            return None;
        };

        // (3) tool-presence gate + (4) non-empty history + (5) counters, all
        // read under one session lock so the snapshot is consistent. We reset
        // `turns_since_last_reminder` here (inside the lock) iff we fire.
        let mut s = self.session.lock().await;

        // (4) empty history ⇒ no reminder.
        if s.history.is_empty() {
            return None;
        }
        // (5) both thresholds.
        if s.turns_since_last_todo_write < tool_task::reminder::TURNS_SINCE_WRITE
            || s.turns_since_last_reminder < tool_task::reminder::TURNS_BETWEEN_REMINDERS
        {
            return None;
        }

        match mode {
            tool_task::reminder::ReminderMode::V1Todo => {
                // (3) TodoWrite must be present this turn.
                self.find_dispatchable_tool("TodoWrite")?;
                let items: Vec<(
                    lingxi_core::TodoState,
                    lingxi_core::types::utf16_json::Utf16JsonProjection,
                )> = s
                    .todos
                    .iter()
                    .map(|t| (t.status, t.content.clone()))
                    .collect();
                s.turns_since_last_reminder = 0;
                drop(s);
                // `case"todo_reminder"` returns `Zy([kn({content:o,isMeta:!0})])`
                // (2.1.238 @296690005), i.e. the body wrapped by `NT` =
                // `` `<system-reminder>\n${e}\n</system-reminder>` `` and marked
                // meta. The body renderer stays pure (byte-locked in
                // `tool_task::reminder`); the envelope is applied here.
                let body = tool_task::reminder::render_v1(&items);
                Some(source_user_meta(
                    MessageId::new(),
                    wrap_system_reminder_source(&body),
                ))
            }
            tool_task::reminder::ReminderMode::V2Task => {
                // (3) TaskUpdate must be present this turn.
                self.find_dispatchable_tool("TaskUpdate")?;
                let session_id = s.session_id;
                s.turns_since_last_reminder = 0;
                drop(s);
                // Read the V2 task store outside the session lock.
                let items: Vec<(
                    String,
                    lingxi_core::TodoState,
                    lingxi_core::types::utf16_json::Utf16JsonProjection,
                )> = match &self.prompt_runtime.todo_reminder_tasks {
                    Some(provider) => provider
                        .task_items(session_id)
                        .await
                        .into_iter()
                        .map(|t| (t.id, t.status, t.subject))
                        .collect(),
                    None => Vec::new(),
                };
                // `case"task_reminder"` — same `Zy([kn({…,isMeta:!0})])` envelope
                // as the V1 branch (2.1.238 @296690634).
                let body = tool_task::reminder::render_v2(&items);
                Some(source_user_meta(
                    MessageId::new(),
                    wrap_system_reminder_source(&body),
                ))
            }
        }
    }

    /// `date_change` (cc `Cop` + renderer `date_change:` in the attachment
    /// table): a session that crosses local midnight tells the model the new
    /// date once per changed date. Producer logic 1:1 —
    /// `wcs()` = local `YYYY-MM-DD` ([`crate::prompt::env_meta::current_date_string`]),
    /// `LGe()` = the memoized session-start date; equal ⇒ no attachment, and an
    /// already-DELIVERED reminder for the same `newDate` dedupes.
    ///
    /// PURE — the dedupe is advanced by [`Self::commit_date_change_reminder`]
    /// once the request carrying the reminder has actually been issued. The
    /// oracle can latch on produce because it materialises the attachment as a
    /// real message (`Va(c,o)`) and pushes it into the message array BEFORE the
    /// call, so its dedupe reads the same fact it delivered; the port's
    /// reminder lives only in the outgoing snapshot, so a step that ends before
    /// the call (blocking-limit preempt, stream error, abort) must not consume
    /// it.
    ///
    /// Rendered through `pm([zr({content, isMeta:!0})])` = `<system-reminder>`
    /// wrap + meta user message, appended to THIS turn's OUTGOING snapshot only
    /// (never `session.history` / JSONL).
    pub(crate) fn date_change_reminder_message(
        &self,
        session_id: lingxi_core::types::SessionId,
    ) -> Option<ConversationMessage> {
        let today = crate::prompt::env_meta::current_date_string();
        let session_date = self.session_start_date(session_id);
        let state = self
            .prompt_runtime
            .date_change
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if session_date == today || state.delivered_date.as_deref() == Some(today.as_str()) {
            return None;
        }
        drop(state);
        // Byte-exact reminder body (2.1.238 renderer @296739637, string-table
        // copy @256746832), wrapped by `NT`:
        // `<system-reminder>\n{e}\n</system-reminder>`. The tail sentence was
        // rewritten upstream between 2.1.220 ("DO NOT mention this to the user
        // explicitly because they are already aware.") and 2.1.238; the dash is
        // U+2014.
        let content = format!(
            "<system-reminder>\nThe date has changed. Today's date is now {today}. \
No need to announce the new date \u{2014} the user's own clock shows it.\n</system-reminder>"
        );
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// Does THIS model step continue a tool round rather than follow a fresh
    /// user prompt?
    ///
    /// The oracle's attachment fan-out (@296520120) distinguishes the two with
    /// `e === null` (no new prompt was handed to `getAttachments`) plus
    /// `!s?.isRegularUserPrompt`. LingXi's turn drivers re-enter the same
    /// assembly for both cases, so the discriminator is recovered from the
    /// history tail: a step that follows tool execution ends on a user line
    /// carrying `tool_result` blocks (claude's `sxl`, @296542062).
    pub(crate) fn step_follows_tool_results(history: &[ConversationMessage]) -> bool {
        matches!(
            history.last(),
            Some(ConversationMessage::User { content, .. })
                if content
                    .iter()
                    .any(|b| matches!(b, lingxi_core::types::ContentBlock::ToolResult { .. }))
        )
    }

    /// The per-turn, transient `silent_turn_reminder` (2.1.238, producer `K4T`
    /// @296525255), or `None` when the gate is off or the stretch is too short.
    ///
    /// Gate, 1:1 with the fan-out condition @296520120:
    /// `p && e===null && !s?.isRegularUserPrompt && !CDt() && u3m(model)` —
    /// main agent only (every `ConversationOrchestrator` is depth-0, so `p` is
    /// always true), only on a tool-round continuation, and only when the
    /// capability/env gate is on. `CDt()` is the focus/brief-transcript view
    /// mode, which LingXi does not have ⇒ always `false` ⇒ never suppresses.
    ///
    /// 2.1.263 `jfr` consults the explicit env override, then the current
    /// model's Fable 5.1 prompt bundle / silent-turn reminder capability.
    ///
    /// On fire the body is wrapped in the usual `<system-reminder>` envelope
    /// (renderer @296738727: `[kn({content:NT(e.text),isMeta:!0})]`) and the
    /// emission position is recorded so `Ezm`'s `remindersInStretch` can be
    /// reconstructed on later turns. Appended to THIS turn's OUTGOING snapshot
    /// only — never `session.history` / JSONL.
    pub(crate) async fn silent_turn_reminder_message(&self) -> Option<ConversationMessage> {
        let history = {
            let s = self.session.lock().await;
            if !crate::prompt::silent_turn::is_enabled(&s.model) {
                return None;
            }
            s.history.clone()
        };
        if !Self::step_follows_tool_results(&history) {
            return None;
        }
        let mut marks = self
            .prompt_runtime
            .silent_turn_reminder_marks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stretch = crate::prompt::silent_turn::scan_silent_stretch(&history, marks.as_slice());
        if !crate::prompt::silent_turn::should_emit(
            stretch,
            crate::prompt::silent_turn::turns_between_reminders(),
        ) {
            return None;
        }
        marks.push(history.len());
        drop(marks);
        let body = crate::prompt::silent_turn::reminder_text();
        let content = format!("<system-reminder>\n{body}\n</system-reminder>");
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// The per-step `total_tokens_reminder` (producer `D3T`
    /// @296556375), or `None` when the mode resolves to `off`.
    ///
    /// ```js
    /// let i=srt(); if(i==="off")return[];
    /// let s=n??"main", a=hoe(t), l=RYn.of(e);
    /// if(o) l.reanchorTaskBudget(s,a);
    /// let c = i==="countdown" ? OR(r,Ox())-a : i==="padded-countdown" ? uOi()-l.cumulativeUsed(s,a) : 0;
    /// return [{type:"total_tokens_reminder", text:dOi(i,c)}]
    /// ```
    ///
    /// The fan-out only calls `D3T` when the step continues a tool round
    /// (`e===null`) or when a regular user prompt arrived AND
    /// `totalTokensReminderAfterUserTurn` is on — the latter also being the
    /// `reanchor` flag. `hoe(messages)` (@294688350) is the LAST assistant
    /// message's `input + cache_creation + cache_read + output`, cached here as
    /// the reminder-only usage snapshot. Compaction resets that snapshot when
    /// the replacement history no longer carries the prior assistant usage.
    ///
    /// The collector persists this non-ephemeral attachment. Later requests
    /// retain its original position and bytes, matching the 2.1.286 renderer.
    pub(crate) async fn total_tokens_reminder_message(
        &self,
        is_regular_user_prompt: bool,
    ) -> Option<ConversationMessage> {
        use crate::prompt::total_tokens as tt;
        let mode = tt::resolve_mode(None);
        if mode == tt::TotalTokensMode::Off {
            return None;
        }
        let model = self.session.lock().await.model.clone();
        let reanchor = is_regular_user_prompt && tt::after_user_turn(None);
        if is_regular_user_prompt && !reanchor {
            return None;
        }
        let used = i64::try_from(
            self.compaction_runtime
                .total_tokens_reminder_usage
                .load(std::sync::atomic::Ordering::Relaxed),
        )
        .unwrap_or(i64::MAX);
        let context_window = i64::try_from(compaction::effective_context_window_size(
            &model,
            &self.api.active_betas(),
        ))
        .unwrap_or(i64::MAX);
        let budget = tt::resolve_budget(None);
        let body = {
            let mut ledger = self
                .compaction_runtime
                .total_tokens_ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if reanchor {
                ledger.reanchor_task_budget("main", used);
            }
            let remaining =
                tt::remaining_tokens(mode, &mut ledger, "main", used, context_window, budget);
            tt::format_total_tokens(mode, remaining)
        };
        // Renderer @296738663: `[kn({content:NT(e.text),isMeta:!0})]`.
        let content = format!("<system-reminder>\n{body}\n</system-reminder>");
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// Typed version used by prompt ingress. The model-facing text and JSONL
    /// cache are produced together so the row can be persisted before the
    /// model engine is admitted and later reinserted by ID.
    pub(crate) async fn total_tokens_prompt_ingress_attachment(
        &self,
        is_regular_user_prompt: bool,
    ) -> Option<crate::request_context::PromptIngressAttachment> {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
        let message = self
            .total_tokens_reminder_message(is_regular_user_prompt)
            .await?;
        let text = system_reminder_inner_source(&message)?;
        let payload = Source::object(vec![
            ("type".into(), Source::from("total_tokens_reminder")),
            ("text".into(), text),
        ])
        .ok()?;
        prompt_ingress_payload(message, payload)
    }

    /// Build Native's optional max-budget ingress attachment from the already
    /// configured session ceiling and cost tracker snapshot.
    pub(crate) async fn budget_usd_prompt_ingress_attachment(
        &self,
    ) -> Option<crate::request_context::PromptIngressAttachment> {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;

        let total_nano_usd = self.config.max_budget_nano_usd?;
        if total_nano_usd == 0 {
            return None;
        }
        let used_nano_usd = self
            .cost_state()
            .await
            .map_or(0, |state| state.total_nano_usd);
        #[allow(clippy::cast_precision_loss)]
        let total_usd = total_nano_usd as f64 / 1_000_000_000.0;
        #[allow(clippy::cast_precision_loss)]
        let used_usd = used_nano_usd as f64 / 1_000_000_000.0;
        let remaining_usd = total_usd - used_usd;
        let body = format!("USD budget: ${used_usd}/${total_usd}; ${remaining_usd} remaining");
        let message = ConversationMessage::user_meta(
            MessageId::new(),
            format!("<system-reminder>\n{body}\n</system-reminder>"),
        );
        let number = |value: f64| Source::parse(&value.to_string()).ok();
        let payload = Source::object(vec![
            ("type".into(), Source::from("budget_usd")),
            ("used".into(), number(used_usd)?),
            ("total".into(), number(total_usd)?),
            ("remaining".into(), number(remaining_usd)?),
        ])
        .ok()?;
        prompt_ingress_payload(message, payload)
    }

    /// The tool name that issued `tool_use_id`, recovered from the assistant
    /// line that carried the `tool_use` block.
    pub(super) async fn tool_name_for_use_id(
        &self,
        tool_use_id: &lingxi_core::types::ToolUseId,
    ) -> Option<String> {
        let s = self.session.lock().await;
        s.history.iter().rev().find_map(|msg| {
            let ConversationMessage::Assistant { content, .. } = msg else {
                return None;
            };
            content.iter().find_map(|b| match b {
                lingxi_core::types::ContentBlock::ToolUse { id, name, .. } if id == tool_use_id => {
                    Some(name.clone())
                }
                _ => None,
            })
        })
    }

    /// Return the local date memoized for `session_id`, seeding it exactly once.
    ///
    /// Both the leading `# currentDate` context and the midnight reminder use
    /// this producer, so call order cannot create two independent date memos.
    pub(crate) fn session_start_date(&self, session_id: lingxi_core::types::SessionId) -> String {
        let today = crate::prompt::env_meta::current_date_string();
        let mut state = self
            .prompt_runtime
            .date_change
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.session_id != Some(session_id) {
            *state = DateChangeState {
                session_id: Some(session_id),
                session_date: today,
                delivered_date: None,
            };
        }
        state.session_date.clone()
    }

    /// Mark the current local date's `date_change` reminder as DELIVERED — the
    /// commit half of [`Self::date_change_reminder_message`]. Called once the
    /// request carrying this turn's outgoing snapshot has actually been issued.
    pub(crate) fn commit_date_change_reminder(&self) {
        self.prompt_runtime
            .date_change
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .delivered_date = Some(crate::prompt::env_meta::current_date_string());
    }

    /// Finding #73: increment BOTH reminder counters by one assistant turn.
    /// Called once per assistant turn (after the API response is processed) on
    /// both the batched and streaming paths, mirroring the binary's per-
    /// assistant-message counting in `L4p`/`N4p`.
    pub(crate) async fn bump_reminder_turn_counters(&self) {
        let mut s = self.session.lock().await;
        s.turns_since_last_todo_write = s.turns_since_last_todo_write.saturating_add(1);
        s.turns_since_last_reminder = s.turns_since_last_reminder.saturating_add(1);
    }

    /// Finding #73: reset `turns_since_last_todo_write` to `0` when this turn's
    /// assistant response invoked the variant's "recent use" tool — `TodoWrite`
    /// (V1) or `TaskCreate`/`TaskUpdate` (V2). Mirrors `L4p`/`N4p` finding the
    /// last such tool_use in the message log (which zeroes their `r` counter).
    /// `tool_names` is the set of tool names invoked in the assistant turn.
    pub(crate) async fn note_todo_reminder_tool_call(&self, tool_names: &[String]) {
        // Ungated (`select_mode_raw`): the oracle's counters advance and reset
        // regardless of whether the reminder can currently render, so a session
        // that re-enables the tools mid-flight does not inherit a stale count.
        let resets = match tool_task::reminder::select_mode_raw() {
            tool_task::reminder::ReminderMode::V1Todo => {
                tool_names.iter().any(|n| n == "TodoWrite")
            }
            tool_task::reminder::ReminderMode::V2Task => tool_names
                .iter()
                .any(|n| n == "TaskCreate" || n == "TaskUpdate"),
        };
        if resets {
            let mut s = self.session.lock().await;
            s.turns_since_last_todo_write = 0;
        }
    }

    /// The per-turn, transient `agent_listing_delta` reminder, or `None` when
    /// the `Agent` tool is absent this turn, or no NEW agent
    /// type has appeared since the last reminder. A wired DISK catalog is NOT
    /// required — built-ins are always announced (binary `aLe` uses
    /// `activeAgents`, which includes built-ins).
    ///
    /// 1:1 with claude-code's `agent_listing_delta` attachment
    /// (`getAgentListingDeltaAttachment`, attachments.ts:1490-1554 →
    /// `normalizeAttachmentForAPI`'s `'agent_listing_delta'` case,
    /// messages.ts:4194-4215):
    /// - PLACEMENT: the Agent tool description carries a static pointer and
    ///   the catalog is conveyed here, so catalog changes do not invalidate
    ///   the tool-schema prompt cache.
    /// - TOOL GATE: skip when the `Agent` tool is not in the registry this turn
    ///   (attachments.ts:1497-1501) — the listing would be unactionable.
    /// - ENTRIES: the merged built-ins + catalog listing via
    ///   [`agent::agent_listing_entries`] (later-wins precedence, sorted), the
    ///   same source of truth the Agent tool uses to validate selections.
    /// - DELTA: emit lines only for types NOT yet announced
    ///   ([`Self::sent_agent_names`]); `is_initial` = the set was empty BEFORE
    ///   this turn (TS `announced.size === 0`). An empty delta ⇒ `None`.
    /// - RENDER: `<system-reminder>\n{header}\n{lines}\n</system-reminder>` with
    ///   the `is_initial`-conditional header (messages.ts:4197-4199), wrapped as
    ///   a meta user message.
    ///
    /// The initial regular-user ingress stores a typed `agent_listing_delta`
    /// attachment with its exact rendered cache; its message-only wrapper is
    /// used on model steps where no initial ingress batch applies.
    ///
    /// AGT-15 — the two branches that used to be documented as deferrals are
    /// now implemented, 1:1 with the oracle renderer @296704484:
    ///
    /// ```js
    /// if(n.length>0&&o.length>0){let a=e.isInitial?"Available agent types for the Agent tool:":"New agent types are now available for the Agent tool:";s.push(`${a}\n${n.join("\n")}`)}
    /// if(i.length>0)s.push(`The following agent types are no longer available:\n${i.map((a)=>`- ${a}`).join("\n")}`),s.push($io);
    /// if(n.length>0&&o.length>0&&e.isInitial&&e.showConcurrencyNote)s.push("When you launch multiple agents for independent work, send them in a single message with multiple tool uses so they run concurrently.");
    /// if(s.length===0)return[];
    /// return Zy([kn({content:s.join("\n\n"),isMeta:!0})])
    /// ```
    ///
    /// * REMOVAL: `removedTypes` = announced-minus-current, sorted (the oracle's
    ///   `c.sort()`, a plain lexicographic sort, NOT the `localeCompare` used for
    ///   the added list). Its section is followed by the shared ambient-context
    ///   trailer `$io` (@296730196) as a SEPARATE section, so the two are joined
    ///   by a blank line. Removed types are dropped from
    ///   [`Self::sent_agent_names`] — the oracle's `s.delete(p)` replay — so a
    ///   type that comes back is re-announced.
    ///   The Rust catalog CAN shrink mid-session: `agent_catalog` is a
    ///   `RwLock` the plugin/MCP reload path rewrites, which is exactly the
    ///   `removedTypes` case.
    /// * CONCURRENCY NOTE: gated on `isInitial && showConcurrencyNote` with
    ///   `showConcurrencyNote = Cc()!=="pro" && DZ()==="default"` (producer
    ///   @296530704) — i.e. NOT a Pro subscription and the subagent steer left at
    ///   `default`. Both signals exist in the port:
    ///   [`lingxi_core::host::subscription::is_pro_plan`] and
    ///   [`lingxi_core::host::live_sessions::subagent_steer_is_default`].
    ///
    /// A session with the Agent tool sends the concurrency note on its first
    /// listing. The removal branch stays silent until the catalog shrinks.
    pub(crate) async fn agent_listing_prompt_ingress_attachment(
        &self,
    ) -> Option<crate::request_context::PromptIngressAttachment> {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;

        // Gate on the Agent tool being available this turn (attachments.ts:1497).
        // This is the ONLY structural gate in the binary's `aLe` — it does NOT
        // gate on a wired
        // DISK catalog (see below).
        self.find_dispatchable_tool("Agent")?;

        // Merge BUILT-INS first, then the wired DISK catalog (if any) on top.
        // Built-ins are ALWAYS part of the listing — the binary's `aLe` builds
        // the delta from `activeAgents` (= built-ins + user/project agents via
        // `getAgents`), so a session with NO disk catalog still announces the
        // built-in agents. (Previously this early-returned when `agent_catalog`
        // was unset, suppressing built-ins entirely under the gate — a divergence
        // from `aLe`.) Later-wins precedence: a same-named catalog agent overrides
        // a built-in, matching the inline `AgentTool` prompt's
        // `PoolSubagentSpawner::listing_entries` (built-in < user/project).
        let mut defs = agent::builtin_agent_definitions();
        let built_in_types = defs
            .iter()
            .map(|definition| definition.agent_type.clone())
            .collect::<std::collections::HashSet<_>>();
        if let Some(catalog) = self.lifecycle_runtime.agent_catalog.as_ref() {
            defs.extend(catalog.read().await.iter().cloned());
        }
        // `agent.offer` filters only this model-facing projection. The
        // catalog itself remains untouched for explicit Agent dispatch.
        let candidates = agent::agent_listing_candidates(&defs);
        let mod_host = if let Some(registry) = &self.lifecycle_runtime.hook_registry {
            registry.read().await.mod_host()
        } else {
            None
        };
        let entries = agent::filter_agent_offer_candidates(
            candidates,
            mod_host,
            agent::AgentOfferContext::default(),
        )
        .await;

        // DELTA: keep only types not yet announced, then record them as sent;
        // and (AGT-15) compute the REMOVED set — announced types that are no
        // longer in the listing — dropping them from the announced set so a
        // type that returns is re-announced (oracle `s.delete(p)`).
        // `is_initial` is captured BEFORE inserting (TS `announced.size === 0`).
        let (is_initial, new_entries, removed_types): (
            bool,
            Vec<lingxi_core::host::subagent_spawn::SubagentListingEntry>,
            Vec<String>,
        ) = {
            let mut sent = self.prompt_runtime.sent_agent_names.lock().await;
            let is_initial = sent.is_empty();
            let current: std::collections::HashSet<String> =
                entries.iter().map(|e| e.agent_type.clone()).collect();
            let mut removed: Vec<String> = sent
                .iter()
                .filter(|t| !current.contains(t.as_str()))
                .cloned()
                .collect();
            // Oracle `c.sort()` — plain lexicographic (UTF-16 code-unit) order,
            // deliberately NOT the `localeCompare` used on the added list.
            removed.sort();
            for t in &removed {
                sent.remove(t);
            }
            let delta: Vec<_> = entries
                .into_iter()
                .filter(|e| !sent.contains(&e.agent_type))
                .collect();
            for e in &delta {
                sent.insert(e.agent_type.clone());
            }
            (is_initial, delta, removed)
        };
        if new_entries.is_empty() && removed_types.is_empty() {
            return None;
        }

        // RENDER: the oracle builds an array of SECTIONS and joins them with a
        // BLANK LINE (`s.join("\n\n")`), then wraps the whole thing in one
        // `<system-reminder>` (@296704484).
        let mut sections: Vec<String> = Vec::new();

        // 1. ADDED: header (is_initial-conditional) + one formatAgentLine per
        //    new type.
        if !new_entries.is_empty() {
            let header = if is_initial {
                "Available agent types for the Agent tool:"
            } else {
                "New agent types are now available for the Agent tool:"
            };
            // `U2n(N, D)` where `D = VU(YK(e.options.mainLoopModel))` (producer
            // @5174317): the catalog lines are rendered for the MAIN-LOOP model,
            // so a non-lean session gets a definition's full `whenToUse` even
            // when it declares a lean variant.
            let lean = tool_api::dh_simple_system_prompt(self.tools.main_loop_model().as_deref());
            let lines = new_entries
                .iter()
                .map(|entry| agent::format_agent_line(entry, lean))
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!("{header}\n{lines}"));
        }

        // 2. REMOVED + the shared ambient-context trailer `$io` (@296730196),
        //    pushed as its OWN section so a blank line separates them.
        if !removed_types.is_empty() {
            let lines = removed_types
                .iter()
                .map(|t| format!("- {t}"))
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!(
                "The following agent types are no longer available:\n{lines}"
            ));
            sections.push(crate::prompt::memory_update::AMBIENT_CONTEXT_TRAILER.to_string());
        }

        // 3. CONCURRENCY NOTE: initial listing only, and only when the plan is
        //    not Pro and the subagent steer is `default`
        //    (`showConcurrencyNote:Cc()!=="pro"&&DZ()==="default"`, @296530704).
        let show_concurrency_note = !new_entries.is_empty()
            && is_initial
            && !lingxi_core::host::subscription::is_pro_plan()
            && lingxi_core::host::live_sessions::subagent_steer_is_default();
        if show_concurrency_note {
            sections.push(
                "When you launch multiple agents for independent work, send them in a single \
message with multiple tool uses so they run concurrently."
                    .to_string(),
            );
        }

        if sections.is_empty() {
            return None;
        }
        let body = sections.join("\n\n");
        let content = format!("<system-reminder>\n{body}\n</system-reminder>");
        let added_types = Source::array(
            new_entries
                .iter()
                .map(|entry| Source::from(entry.agent_type.clone()))
                .collect(),
        )
        .ok()?;
        let added_lines = Source::array(
            new_entries
                .iter()
                .map(|entry| {
                    Source::from(agent::format_agent_line(
                        entry,
                        tool_api::dh_simple_system_prompt(self.tools.main_loop_model().as_deref()),
                    ))
                })
                .collect(),
        )
        .ok()?;
        let built_in_types = Source::array(
            new_entries
                .iter()
                .filter(|entry| built_in_types.contains(&entry.agent_type))
                .map(|entry| Source::from(entry.agent_type.clone()))
                .collect(),
        )
        .ok()?;
        let removed_types = Source::array(
            removed_types
                .iter()
                .map(|agent_type| Source::from(agent_type.clone()))
                .collect(),
        )
        .ok()?;
        let is_initial = Source::from_serde_value(&serde_json::Value::Bool(is_initial)).ok()?;
        let show_concurrency_note =
            Source::from_serde_value(&serde_json::Value::Bool(show_concurrency_note)).ok()?;
        let payload = Source::object(vec![
            ("type".into(), Source::from("agent_listing_delta")),
            ("addedTypes".into(), added_types),
            ("addedLines".into(), added_lines),
            ("builtInTypes".into(), built_in_types),
            ("removedTypes".into(), removed_types),
            ("isInitial".into(), is_initial),
            ("showConcurrencyNote".into(), show_concurrency_note),
        ])
        .ok()?;
        prompt_ingress_payload(
            ConversationMessage::user_meta(MessageId::new(), content),
            payload,
        )
    }

    pub(crate) async fn agent_listing_reminder_message(&self) -> Option<ConversationMessage> {
        self.agent_listing_prompt_ingress_attachment()
            .await
            .map(|row| row.message)
    }

    /// REM-10 — the periodic `tool_search_usage_reminder`, or `None` (the
    /// default) when its gate is off.
    ///
    /// 1:1 with the oracle producer `Uzm` @**296553134**; see
    /// [`crate::prompt::tool_search_reminder`] for the oracle listing and for why
    /// this ships INERT (the upstream GrowthBook payload `juniper_shoal.
    /// marsh_lantern` is unset in a stock install, so `Lda()` is `null` and
    /// `Uzm` returns `[]` on its first line).
    ///
    /// Gate order, matching `Uzm`:
    /// 1. `Lda()` — [`crate::prompt::tool_search_reminder::config`]; `None` ⇒ off.
    /// 2. `if(!e||e.length===0)` — empty history ⇒ nothing.
    /// 3. `R3T` — BOTH counters must have reached `everyNTurns`.
    /// 4. `if(mBr()!=="tst")` — tool search must be in the plain enabled mode;
    ///    `tst-auto` ([`tool_api::defer::ToolSearchMode::Auto`]) is excluded.
    /// 5. `if(!bjt(t.options.tools))` — the ToolSearch tool must be present.
    /// 6. `l.length===0` — there must be at least one UNDISCOVERED deferred tool.
    /// 7. `if(c)return s("task_reminder_same_turn")` — never in the same turn as
    ///    a todo/task reminder.
    ///
    /// # Divergence (reason)
    /// `Uzm` also gates on `e1e(model)` / `!QLe(Fo(model))` — a per-model
    /// capability table and a Vertex exclusion. The port has neither table, and
    /// inventing one would gate on a guess; the remaining six gates are ported
    /// exactly.
    ///
    /// MUTATES the emission marks, so it must be called at most ONCE per
    /// outgoing model step, like every other member of this family.
    pub(crate) async fn tool_search_usage_reminder_message(
        &self,
        todo_reminder_fired_this_turn: bool,
    ) -> Option<ConversationMessage> {
        // (1) gate.
        let config = crate::prompt::tool_search_reminder::config()?;
        // (4) mode. `Enabled` is the oracle's `"tst"`; `Auto` is `"tst-auto"`.
        if self.tools.deferral().mode() != tool_api::defer::ToolSearchMode::Enabled {
            return None;
        }
        // (5) the ToolSearch tool must be available this turn.
        let tool_search = self.find_dispatchable_tool("ToolSearch")?;
        let tool_search_name = tool_search.name().to_string();

        // (2)+(3) history + both turn counters.
        let history = { self.session.lock().await.history.clone() };
        if history.is_empty() {
            return None;
        }
        let marks = self
            .prompt_runtime
            .tool_search_reminder_marks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (since_tool_search, since_reminder) =
            crate::prompt::tool_search_reminder::count_turns(&history, &marks, &tool_search_name);
        if since_tool_search < config.every_n_turns || since_reminder < config.every_n_turns {
            return None;
        }
        // (7) never alongside a todo/task reminder.
        if todo_reminder_fired_this_turn {
            return None;
        }
        // (6) the undiscovered set — the searchable view minus what this session
        // has already loaded, sorted (oracle `.sort()`).
        let loaded: std::collections::HashSet<String> = self
            .tools
            .deferral()
            .loaded_tool_names()
            .into_iter()
            .collect();
        let mut undiscovered: Vec<String> = self
            .tools
            .tool_search_view()
            .entries()
            .into_iter()
            .map(|entry| entry.name)
            .filter(|name| !loaded.contains(name))
            .collect();
        undiscovered.sort();
        undiscovered.dedup();
        if undiscovered.is_empty() {
            return None;
        }
        let count = undiscovered.len();
        undiscovered.truncate(config.max_names);
        let body = crate::prompt::tool_search_reminder::render_reminder(
            &undiscovered,
            count,
            &tool_search_name,
        )?;
        self.prompt_runtime
            .tool_search_reminder_marks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(history.len());
        Some(ConversationMessage::user_meta(
            MessageId::new(),
            format!("<system-reminder>\n{body}\n</system-reminder>"),
        ))
    }

    async fn route_still_resolves_to_cached_file(
        &self,
        route_spellings: &[std::path::PathBuf],
        cached_path: &std::path::Path,
        cancel: Option<&lingxi_core::host::CancellationToken>,
    ) -> bool {
        // Cache keys can be lexical routes (`/var/...`) or canonical paths
        // (`/private/var/...`), depending on the current producer. Compare the
        // resolved cache identity to each CAPTURED route's resolved identity;
        // never turn `cached_path` into a route or fallback authorization.
        let Some(cached_result) =
            cancellable_read_io(cancel, tokio::fs::canonicalize(cached_path)).await
        else {
            return false;
        };
        let Ok(cached_identity) = cached_result else {
            return false;
        };
        for spelling in route_spellings {
            if !spelling.is_absolute() {
                continue;
            }
            let Some(result) = cancellable_read_io(cancel, tokio::fs::canonicalize(spelling)).await
            else {
                return false;
            };
            if result.is_ok_and(|resolved| resolved == cached_identity) {
                return true;
            }
        }
        false
    }

    /// REM-05 — the per-turn `edited_text_file` (changed-files) reminders: one
    /// meta user message per file that changed ON DISK since the model last saw
    /// it.
    ///
    /// 1:1 with the oracle producer `Izm(ctx)` @**296537358**:
    ///
    /// ```js
    /// async function Izm(e){let t=OWr(e.readFileState);if(t.length===0)return[];
    ///  let r=gn(e),o=(await Promise.all(t.map(async(s)=>{
    ///    let a=e.readFileState.get(s);if(!a)return null;
    ///    if(a.offset!==void 0||a.limit!==void 0)return null;
    ///    let l=Zi(s);if(qhe(l,r))return null;
    ///    try{ if(await f4e(l)<=a.timestamp)return null;
    ///         …let p=await mC.call({file_path:l},e);
    ///         if(p.data.type==="text"){ if(p.data.file.truncatedByTokenCap===!0)return null;
    ///           if(vNe(a,p.data.file.content))return null;
    ///           let f=SEf(a.content,p.data.file.content); if(f==="")return null;
    ///           return{type:"edited_text_file",filename:l,snippet:f}} …}
    ///    catch(c){if(ur(c))e.readFileState.delete(s);return null}})))
    ///   .filter((s)=>s!=null), i=0;
    ///  for(let s of o){…if(i>=m3T)s.snippet="";else i+=s.snippet.length}
    ///  return o}
    /// ```
    ///
    /// Step for step:
    ///
    /// 1. **Scan** every read-state entry with a source route (model read,
    ///    rendered memory, post-compact restore, or SDK host seed), in the
    ///    LRU's MRU→LRU order, through
    ///    [`tool_api::read_file_state::ReadFileStateLru::peek`] so the scan does
    ///    not rewrite recency (the oracle iterates the Map, which does not
    ///    either).
    ///
    ///    `OWr(e.readFileState)` yields EVERY key, including memory and host
    ///    seeds marked outside the current model-context set. LingXi records a
    ///    source route when each real producer inserts those entries; only
    ///    route-less, non-model/non-rendered seeds are excluded.
    /// 2. **Skip partial reads** — `a.offset!==void 0||a.limit!==void 0`.
    ///    After the actual Read call, Native separately suppresses a newly-read
    ///    text result only when `truncatedByTokenCap` is true. Cached
    ///    `is_partial_view` is not a pre-read filter.
    /// 3. **mtime gate** — `if(await f4e(l)<=a.timestamp)return null`. A missing
    ///    file DROPS the entry (`if(ur(c))e.readFileState.delete(s)`).
    /// 4. **Read-tool call + content compare** — `vNe(a,content)`: the registered
    ///    Read tool supplies the result and refreshes its own read-state entry;
    ///    identical text ⇒ no reminder.
    /// 5. **Diff** — [`crate::prompt::changed_files::render_snippet`] (`SEf`,
    ///    `structuredPatch` at context 8, 8192-char cap); an empty diff ⇒ no
    ///    reminder.
    /// 6. **Budget** — [`crate::prompt::changed_files::apply_snippet_budget`]
    ///    (`m3T = 16384`, cumulative across the turn's files; entries past the
    ///    threshold render the "diff is omitted here" arm).
    /// 7. **Render** — text attachments become wrapped meta messages. Native
    ///    2.1.291's generic `edited_image_file` renderer returns an empty list;
    ///    its typed image payload is retained but not sent as a model message.
    ///
    /// For a route that passes the current Read-deny and held-outside checks,
    /// the registered Read call refreshes text/notebook state itself, matching
    /// the Native nested `Read` call side effect. No reminder-specific cache
    /// setter is used.
    ///
    pub(crate) async fn changed_files_reminder_messages(
        &self,
        cancel: Option<&lingxi_core::host::CancellationToken>,
    ) -> Vec<crate::request_context::PromptIngressAttachment> {
        // (1) Snapshot the registry without touching recency. Keep each current
        // source route alongside the cache key; live deny rules may name
        // either side of a symlink and Native rechecks the route before its
        // changed-file Read.
        let candidates: Vec<(
            std::path::PathBuf,
            tool_api::read_file_state::ReadFileEntry,
            Vec<Vec<std::path::PathBuf>>,
        )> = {
            let guard = self
                .prompt_runtime
                .read_state_map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .changed_file_candidate_keys()
                .into_iter()
                .filter_map(|p| {
                    guard
                        .peek(&p)
                        .map(|entry| (p.clone(), entry, guard.requested_path_groups(&p)))
                })
                .collect()
        };
        if candidates.is_empty() {
            return Vec::new();
        }

        // Native's internal Read exists even when session tool visibility filters
        // hide it. Both current desktop and mobile compositions register this as
        // a built-in; `find_registered` selects built-ins first and bypasses the
        // model-visible allowlist.
        let Some(read_tool) = self.tools.find_registered("Read") else {
            return Vec::new();
        };
        let read_tool_name = read_tool.name().to_string();
        let messages = self.session.lock().await.model_context_history();
        let mut base_read_context =
            crate::turn_loop::streaming_tool_context_base(self, messages).await;
        base_read_context.cancel = cancel.cloned();
        // Native `sy(context)` uses these three current permission facts. A
        // real policy snapshot supplies its live facts; a transport-only gate
        // explicitly reports inactive facts.
        let policy_snapshot = self.perms.read_path_policy_snapshot();
        let native_path_recheck = policy_snapshot.facts().requires_path_recheck();

        let mut changed: Vec<crate::prompt::changed_files::ChangedFile> = Vec::new();
        for (path, entry, requested_path_groups) in candidates {
            if cancel.is_some_and(lingxi_core::host::CancellationToken::is_cancelled) {
                return Vec::new();
            }
            // (2) Native skips only entries previously read with an offset/limit.
            if entry.offset.is_some() || entry.limit.is_some() {
                continue;
            }
            // (3) mtime gate; a vanished file drops its entry.
            let Some(metadata_result) =
                cancellable_read_io(cancel, tokio::fs::metadata(&path)).await
            else {
                return Vec::new();
            };
            let mtime_ms = match metadata_result.and_then(|m| m.modified()) {
                Ok(t) => tool_api::read_file_state::mtime_ms_floor(t),
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::NotFound {
                        if let Ok(mut guard) = self.prompt_runtime.read_state_map.lock() {
                            let _ = guard.remove(&path);
                        }
                    }
                    continue;
                }
            };
            if mtime_ms <= entry.mtime_ms {
                continue;
            }
            // Re-check every spelling of each observed route. The canonical
            // cache key is checked for a direct deny as well, but it is never
            // treated as a new allowed route: it is an I/O key, not a spelling
            // the model used. Only routes captured by current model/file
            // producers authorize a reminder read; a cache key alone does not.
            if requested_path_groups.is_empty() {
                continue;
            }
            let mut reminder_route = None;
            for route_spellings in requested_path_groups {
                let Some(source_spelling) = route_spellings.first().cloned() else {
                    continue;
                };
                let mut check_spellings = if native_path_recheck {
                    // Native `XB` has an abort-aware 32-member Set and, on
                    // macOS/Linux, a separate 40-hop `xne` landing walk.
                    // Direct Read permission uses its independent 64-hop
                    // synchronous resolver.
                    let Some(spellings) =
                        native_changed_file_read_path_set(&source_spelling, cancel).await
                    else {
                        if cancel.is_some_and(lingxi_core::host::CancellationToken::is_cancelled) {
                            return Vec::new();
                        }
                        continue;
                    };
                    spellings
                } else {
                    vec![source_spelling.clone()]
                };
                for spelling in route_spellings.iter().chain(std::iter::once(&path)) {
                    if !check_spellings.contains(spelling) {
                        check_spellings.push(spelling.clone());
                    }
                }
                let mut route_denied = false;
                if native_path_recheck {
                    for spelling in &check_spellings {
                        let check = policy_snapshot.check_path(spelling);
                        if check.denied_by_read_rule
                            == lingxi_core::host::permission_gate::ReadPathPolicyMatch::Match
                            || check.held_outside
                                == lingxi_core::host::permission_gate::ReadPathPolicyMatch::Match
                        {
                            route_denied = true;
                            break;
                        }
                        // An active `PolicyPermissionGate` can be constructed
                        // without filesystem roots. Its path-specific deny and
                        // held-path answers are then explicitly unavailable;
                        // preserve the existing changed-file behavior until a
                        // real root producer is plumbed, rather than treating
                        // missing data as either a Native allow or deny.
                    }
                }
                let route_still_resolves = self
                    .route_still_resolves_to_cached_file(&route_spellings, &path, cancel)
                    .await;
                if cancel.is_some_and(lingxi_core::host::CancellationToken::is_cancelled) {
                    return Vec::new();
                }
                if route_denied || !route_still_resolves {
                    continue;
                }
                reminder_route = Some(source_spelling);
                break;
            }
            let Some(reminder_path) = reminder_route else {
                continue;
            };
            // (4) Invoke the current registered Read tool directly, matching
            // Native `ZS.call` after the path-policy checks above. The host-built
            // request contains only the selected original route.
            let input = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &serde_json::json!({
                "file_path": reminder_path.to_string_lossy().into_owned(),
                }),
            )
            .expect("host-generated Read reminder input is valid JSON");
            if crate::schema_validation::validate_tool_schema_detailed(read_tool.as_ref(), &input)
                .is_err()
            {
                continue;
            }
            let read_context = base_read_context.clone();
            if read_tool
                .validate_input(&input, &read_context)
                .await
                .is_err()
            {
                continue;
            }
            if cancel.is_some_and(lingxi_core::host::CancellationToken::is_cancelled) {
                return Vec::new();
            }
            let (progress_tx, _progress_rx) = tool_api::progress_channel();
            let read_result = match read_tool.call(input, read_context, progress_tx).await {
                Ok(result) => result,
                Err(_) => {
                    if cancel.is_some_and(lingxi_core::host::CancellationToken::is_cancelled) {
                        return Vec::new();
                    }
                    continue;
                }
            };
            if cancel.is_some_and(lingxi_core::host::CancellationToken::is_cancelled) {
                return Vec::new();
            }
            // The actual Read call owns cache/state updates. Consume only the
            // Native changed-file result kinds; `new_messages` from this inner
            // call are intentionally not forwarded.
            match read_result.data.string_at("/type") {
                Some("text") => {
                    let Ok(file) = read_result.data.subprojection("/file") else {
                        continue;
                    };
                    if file
                        .at_pointer("/truncatedByTokenCap")
                        .is_some_and(|value| value.as_bool() == Some(true))
                    {
                        continue;
                    }
                    let Some(fresh) = file.string_at("/content") else {
                        continue;
                    };
                    if fresh == entry.content {
                        continue;
                    }
                    // (5) diff.
                    let snippet =
                        crate::prompt::changed_files::render_snippet(&entry.content, fresh, false);
                    if snippet.is_empty() {
                        continue;
                    }
                    changed.push(crate::prompt::changed_files::ChangedFile::text(
                        reminder_path.to_string_lossy().into_owned(),
                        snippet,
                    ));
                }
                Some("image") => {
                    if let Some(image) = crate::prompt::changed_files::ChangedFile::from_read_result(
                        reminder_path.to_string_lossy().into_owned(),
                        &read_result.data,
                    ) {
                        changed.push(image);
                    }
                }
                // Native NTr ignores PDFs, notebooks, `parts`, and every other
                // non-text/non-image result.
                _ => continue,
            }
        }
        if changed.is_empty() {
            return Vec::new();
        }
        // (6) cross-file snippet budget, then (7) render one wrapped meta
        // message per changed file. Preserve Native's typed payload and exact
        // original rendered cache alongside the stable message identity; the
        // caller persists this row before screening its request-local copy.
        crate::prompt::changed_files::apply_snippet_budget(&mut changed);
        changed
            .iter()
            .filter_map(|file| {
                let message = crate::prompt::changed_files::render_changed_file_message(
                    file,
                    &read_tool_name,
                )?;
                let payload =
                    lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                        &serde_json::json!({
                            "type": "edited_text_file",
                            "filename": file.filename,
                            "snippet": file.snippet,
                        }),
                    )
                    .ok()?;
                let rendered = crate::request_context::rendered_system_message(&message)?;
                Some(crate::request_context::PromptIngressAttachment {
                    message,
                    payload,
                    rendered,
                })
            })
            .collect()
    }

    /// §F: the per-turn, transient `conditional_rules` reminder — path-gated
    /// LINGXI.md rules (`paths:`-globbed) that newly ACTIVATE because a file the
    /// session has touched this run matches their globs. Returns `None` when no
    /// memory provider is wired, the hierarchy has no conditional rules, or no
    /// newly-activated rule exists this turn.
    ///
    /// 1:1 with claude-code `processConditionedMdRules` (claudemd.ts:1354-1397)
    /// fed through the `nested_memory` render seam (messages.ts:3700-3707):
    ///
    /// Per-turn, transient `<new-diagnostics>` reminder — newly-reported LSP
    /// diagnostics not yet surfaced to the model (claude-code's
    /// `formatDiagnosticsBlock` flow). `None` when no LSP source is wired (no
    /// servers ⇒ the common case) or there are no new diagnostics.
    ///
    /// The block carries its own `<new-diagnostics>` tag, and the oracle wraps
    /// that in a `<system-reminder>` on top of it: 2.1.238 @296692400
    /// `case"diagnostics":{…return Zy([kn({content:Bve.formatDiagnosticsBlock(n),isMeta:!0})])}`,
    /// where `Zy` (@296675470) maps `NT` = `` `<system-reminder>\n${e}\n</system-reminder>` ``
    /// (@296673554) over every message. The `<new-diagnostics>` literal
    /// (@236015184) contains no envelope of its own, so the two tags nest.
    /// Appended ONLY to the outgoing snapshot (never `session.history` / JSONL).
    pub(crate) async fn new_diagnostics_reminder_message(&self) -> Option<ConversationMessage> {
        let block = self
            .prompt_runtime
            .new_diagnostics_source
            .as_ref()?
            .take_new_diagnostics_block()
            .await?;
        let block = lingxi_core::types::utf16_json::Utf16JsonProjection::from(block);
        Some(source_user_meta(
            MessageId::new(),
            wrap_system_reminder_source(&block),
        ))
    }

    /// Consume actual per-context Read/readFor triggers, independent of LRU
    /// contents. One per-trigger acquisition produces current durable rows.
    pub async fn nested_memory_reminder_messages(
        &self,
        context_attachments: &[lingxi_core::types::utf16_json::Utf16JsonProjection],
    ) -> Vec<ConversationMessage> {
        let queue = &self.prompt_runtime.nested_memory_triggers;
        let disabled =
            std::env::var_os("LINGXI_DISABLE_LINGXI_MDS").is_some_and(|value| !value.is_empty());
        if !queue.begin(false, disabled) {
            return Vec::new();
        }
        let roots = if memory::lingxi_md::agents::attachments_enabled()
            && (self.memory.filesystem_discovery()
                || self.prompt_runtime.nested_memory_roots.is_some())
        {
            self.prompt_runtime
                .nested_memory_roots
                .clone()
                .or_else(|| self.memory.hierarchy_roots())
        } else {
            None
        };
        let probe_cwd = self.prompt_probe_cwd(&self.session_cwd.cwd());
        let cwd = tokio::fs::canonicalize(&probe_cwd)
            .await
            .unwrap_or(probe_cwd);
        let mode = self.memory.instruction_files_mode();
        let prior = self
            .nested_memory_history_with_context_attachments(context_attachments)
            .await;
        let excluder = self.memory.excluder();
        let mut messages = Vec::new();
        let mut cursor = 0;
        while let Some(trigger) = queue.next(&mut cursor) {
            let rules = self
                .memory
                .load_conditional_rules(&cwd, &trigger, mode)
                .await
                .into_iter()
                .filter(|rule| {
                    crate::prompt::conditional_rules::rule_matches_touched_file(
                        rule, &trigger, &cwd,
                    )
                })
                .collect();
            messages.extend(
                self.persist_nested_memory_files(rules, &trigger, &cwd, &prior)
                    .await,
            );
            if let Some((home, managed)) = &roots {
                let files = crate::prompt::nested_memory::discover_with_mode(
                    &trigger,
                    &cwd,
                    home,
                    managed.as_deref(),
                    excluder.as_ref(),
                    mode,
                );
                messages.extend(
                    self.persist_nested_memory_files(files, &trigger, &cwd, &prior)
                        .await,
                );
            }
        }
        messages
    }

    /// P0.1: arm the memory-selector prefetch for THIS turn, firing it
    /// CONCURRENTLY with the main API call (claude-code's `wAo` prefetch
    /// side-channel). Called at the START of each turn in BOTH drivers, BEFORE
    /// the snapshot is assembled, so the in-flight handle is ready for
    /// [`Self::relevant_memory_reminder_messages`] to await. A strict no-op when
    /// no prefetch is wired ([`Self::memory_prefetch`] is `None`) — then the slot
    /// stays empty and the surfacing reminder list is empty, keeping the locked
    /// fixtures byte-identical.
    ///
    /// The prefetch query is the latest NON-meta user-message text in the
    /// session history (mirroring TS `e.findLast(m => m.type==="user" &&
    /// !m.isMeta)` in `wAo`). The memdir directory is derived from the cwd; the
    /// stub prefetch ignores both for now (it resolves to an empty set) so this
    /// is inert by default.
    pub(super) async fn discard_stale_prefetches(&self) {
        *self.prompt_runtime.pending_memory_prefetch.lock().await = None;
        *self.prompt_runtime.pending_skill_prefetch.lock().await = None;
    }

    fn has_visible_prefetch_text(text: &str) -> bool {
        text.chars().any(|ch| {
            !ch.is_whitespace()
                && !matches!(
                    ch,
                    '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}'
                )
        })
    }

    fn latest_prefetch_query(history: &[ConversationMessage]) -> String {
        history
            .iter()
            .rev()
            .find_map(|message| match message {
                ConversationMessage::User {
                    content,
                    is_meta: false,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                    ..
                } => {
                    let text = content
                        .iter()
                        .filter_map(|block| match block {
                            lingxi_core::types::ContentBlock::Text { text, .. } => {
                                Some(text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    Self::has_visible_prefetch_text(&text).then_some(text)
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    fn latest_prefetch_query_and_tools(history: &[ConversationMessage]) -> (String, Vec<String>) {
        let query = Self::latest_prefetch_query(history);
        let last_assistant_tools = history
            .iter()
            .rev()
            .find(|m| matches!(m.role(), lingxi_core::types::MessageRole::Assistant))
            .map(|m| {
                m.tool_calls()
                    .into_iter()
                    .filter_map(|b| match b {
                        lingxi_core::types::ContentBlock::ToolUse { name, .. } => {
                            Some(name.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        (query, last_assistant_tools)
    }

    pub(crate) async fn start_memory_prefetch(&self) {
        let Some(prefetch) = self.prompt_runtime.memory_prefetch.as_ref() else {
            return; // no prefetch wired ⇒ surfacing channel stays inert
        };
        // Keep at most one in-flight selector. A slow result continues under
        // the current model call instead of being overwritten or queueing
        // unbounded side queries.
        if self
            .prompt_runtime
            .pending_memory_prefetch
            .lock()
            .await
            .is_some()
        {
            return;
        }
        // Latest REAL user message = the turn query. Compact summaries,
        // transcript-only rows, Stop-hook feedback, and tool-result user rows
        // are synthetic context rather than user intent.
        let (query, session_id) = {
            let s = self.session.lock().await;
            // Memory selection must follow the same model-visible history as
            // request assembly. Transcript-only/background rows can remain in
            // the UI history, but they are not a valid user query and must not
            // move the selector's starting point.
            let history = s.model_context_history();
            (
                Self::latest_prefetch_query(&history),
                s.session_id.to_string(),
            )
        };
        // Task 5 (worktree 206 session-cwd plumbing): the live cwd, so a future
        // non-stub prefetch derives the memdir from the post-swap worktree, not
        // the frozen boot cwd. Currently inert (the stub prefetch ignores its
        // cwd argument), so this is a no-behavior-change correctness fix.
        let (model, profile) = self.current_prompt_route().await;
        let pending = prefetch
            .start_for_session(
                query,
                model,
                profile,
                self.session_cwd.cwd(),
                Some(session_id),
            )
            .await;
        *self.prompt_runtime.pending_memory_prefetch.lock().await = Some(pending);
    }

    /// P0.1: the per-turn, transient `relevant_memories` SURFACING reminders — the
    /// memory-selector/prefetch result rendered as one meta user message per
    /// surfaced memory. Returns an empty list when no prefetch was armed this turn
    /// ([`Self::start_memory_prefetch`] left the slot empty / no prefetch wired),
    /// the prefetch resolved to an empty set, or every surfaced memory was
    /// already injected (the SHARED dedup below).
    ///
    /// 1:1 with claude-code v2.1.181's `relevant_memories` attachment
    /// (`normalizeAttachmentForAPI` case `"relevant_memories"`, messages.ts —
    /// see [`memory::surfacing::render_surfacing_messages`] for the exact shape):
    /// the em-dash idx-0 preamble + per-memory `Memory: {path}:` header (with a
    /// `>1`-day staleness prefix), preserving each memory's message boundary.
    ///
    /// SHARED DEDUP: a memory is skipped when its path is in EITHER
    /// [`Self::surfaced_memory_paths`] (already surfaced a prior turn) OR
    /// [`Self::read_state_map`] (already loaded as a nested/conditional P3.2
    /// attachment OR read by a file tool) — so a file can never be double-injected
    /// across the surfacing + nested channels. Surfaced paths are recorded so each
    /// memory injects ONCE (TS prefetch consume-once + `loadedNestedMemoryPaths`).
    ///
    /// Like every other per-turn reminder, the message is appended ONLY to the
    /// per-turn OUTGOING snapshot (never `session.history` / JSONL), so it is
    /// recomputed each turn and never accumulates.
    pub(crate) async fn relevant_memory_reminder_messages(&self) -> Vec<ConversationMessage> {
        // Never await an unresolved side query on the model-call critical path.
        // Leave it in the slot so it can run concurrently with this iteration
        // and be collected by a later one.
        let pending = {
            let mut slot = self.prompt_runtime.pending_memory_prefetch.lock().await;
            let Some(pending) = slot.take() else {
                return Vec::new();
            };
            if !pending.is_ready() {
                *slot = Some(pending);
                return Vec::new();
            }
            pending
        };
        let surfaced = pending.take().await;
        if surfaced.is_empty() {
            return Vec::new();
        }

        // SHARED DEDUP — skip any memory already surfaced this session OR already
        // loaded as a nested/conditional attachment / tool read (`read_state_map`).
        let already_read: std::collections::HashSet<std::path::PathBuf> = self
            .prompt_runtime
            .read_state_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .model_context_keys()
            .into_iter()
            .collect();
        let fresh: Vec<memory::surfacing::SurfacedMemory> = {
            let mut surfaced_set = self.prompt_runtime.surfaced_memory_paths.lock().await;
            let mut out = Vec::new();
            for m in surfaced {
                if surfaced_set.contains(&m.path) || already_read.contains(&m.path) {
                    continue; // double-injection guard
                }
                surfaced_set.insert(m.path.clone());
                out.push(m);
            }
            out
        };
        if fresh.is_empty() {
            return Vec::new();
        }

        memory::surfacing::render_surfacing_messages(&fresh)
            .into_iter()
            .map(|content| ConversationMessage::user_meta(MessageId::new(), content))
            .collect()
    }

    /// EXPERIMENTAL_SKILL_SEARCH: arm the skill-discovery prefetch CONCURRENTLY
    /// with this turn (1:1 with claude-code `startSkillDiscoveryPrefetch`, bundle
    /// fn `C1z`: `B=at1?.startSkillDiscoveryPrefetch(null,V,T)` at iteration top).
    /// A strict no-op when no prefetch is wired ([`Self::skill_discovery_prefetch`]
    /// is `None`) — then the slot stays empty and the surfacing reminder is `None`,
    /// keeping the locked fixtures byte-identical.
    ///
    /// The prefetch query is the latest non-meta user-message text (same scan as
    /// [`Self::start_memory_prefetch`], mirroring TS `findLast(user/!meta)`). The
    /// per-iteration `findWritePivot` guard (`query.ts:323` — discovery only fires
    /// on write-pivot iterations) is computed from the most recent assistant
    /// message's requested tools (see [`skill::find_write_pivot`],
    /// [RECONSTRUCTED]); on a non-write iteration the prefetch ships empty.
    pub(crate) async fn start_skill_discovery_prefetch(&self) {
        let Some(prefetch) = self.prompt_runtime.skill_discovery_prefetch.as_ref() else {
            return; // no prefetch wired ⇒ discovery channel stays inert
        };
        // One bounded in-flight discovery. A slow result remains eligible for a
        // later iteration and never queues another side query behind it.
        if self
            .prompt_runtime
            .pending_skill_prefetch
            .lock()
            .await
            .is_some()
        {
            return;
        }
        // Latest non-meta user message = the turn query (TS findLast user/!meta),
        // and the most recent assistant message's tool names for the write-pivot
        // predicate — both read in one history lock.
        let (query, last_assistant_tools) = {
            let s = self.session.lock().await;
            // Keep the query and write-pivot scan on the exact same
            // model-visible slice. UI-only rows are deliberately excluded.
            let history = s.model_context_history();
            Self::latest_prefetch_query_and_tools(&history)
        };
        let is_write_pivot = skill::find_write_pivot(&last_assistant_tools);
        let pending = prefetch.start(query, is_write_pivot).await;
        *self.prompt_runtime.pending_skill_prefetch.lock().await = Some(pending);
    }

    /// EXPERIMENTAL_SKILL_SEARCH: the per-turn, transient `skill_discovery`
    /// SURFACING reminder — the prefetch result rendered as a single
    /// `<system-reminder>` meta user message (1:1 with claude-code's
    /// `collectSkillDiscoveryPrefetch` → `skill_discovery` attachment,
    /// `messages.ts:3506-3519`). Returns `None` when no prefetch was armed this
    /// turn, the prefetch resolved to an empty set, or every discovered skill was
    /// already surfaced.
    ///
    /// Emits the `hidden_by_main_turn` telemetry field (`query.ts:1617`): `true`
    /// when the prefetch resolved BEFORE collection (it hid under the main turn's
    /// streaming + tool execution; expected >98%). Peeked via
    /// [`skill::PendingSkillDiscoveryPrefetch::is_ready`] before the consuming
    /// `take`.
    ///
    /// DEDUP: a skill is skipped when its `name` is in
    /// [`Self::surfaced_skill_names`] (already surfaced a prior turn). Keyed on
    /// `name` (skill names are not files, so — unlike the memory channel — this
    /// does NOT consult `read_file_state`). Surfaced names are recorded so each
    /// skill injects ONCE. Like every other per-turn reminder, the message is
    /// appended ONLY to the per-turn OUTGOING snapshot (never `session.history` /
    /// JSONL).
    pub(crate) async fn skill_discovery_reminder_message(&self) -> Option<ConversationMessage> {
        // Consume the in-flight prefetch handle armed at turn start.
        let pending = {
            let mut slot = self.prompt_runtime.pending_skill_prefetch.lock().await;
            let pending = slot.take()?;
            // A side query that has not hidden under available work must not
            // delay the API call. Keep it alive and try again next iteration.
            if !pending.is_ready() {
                *slot = Some(pending);
                telemetry::emit_skill_discovery_collected(false);
                return None;
            }
            pending
        };
        telemetry::emit_skill_discovery_collected(true);

        let skills = pending.take().await;
        if skills.is_empty() {
            return None;
        }

        // DEDUP by name — skip any skill already surfaced this session.
        let fresh: Vec<skill::DiscoveredSkill> = {
            let mut surfaced = self.prompt_runtime.surfaced_skill_names.lock().await;
            let mut out = Vec::new();
            for s in skills {
                if surfaced.contains(&s.name) {
                    continue; // already surfaced a prior turn
                }
                surfaced.insert(s.name.clone());
                out.push(s);
            }
            out
        };

        // render returns None on empty (TS `return []`).
        let content = skill::render_skill_discovery_block(&fresh)?;
        let source_units = content
            .string_units("")
            .expect("skill discovery block is a source string");
        let display = String::from_utf16_lossy(&source_units);
        Some(ConversationMessage::user_meta_js_utf16(
            MessageId::new(),
            display,
            source_units,
        ))
    }

    /// `deferred_tools_delta` typed row for THIS outgoing model step.
    ///
    /// Claude Code (`A1s` + the `deferred_tools_delta` attachment renderer)
    /// announces the searchable deferred set as a `<system-reminder>` only when
    /// that set has CHANGED since the prior request — listing the newly-available
    /// names, re-appearing names (MCP reconnect), and removed names — rather than
    /// repeating the whole catalog every turn. The announced / ever-added state
    /// is tracked in the session's [`DeferralState`].
    ///
    /// MUTATES the delta tracking (via `compute_deferred_delta`), so it must be
    /// called at most ONCE per outgoing model step. Callers that rebuild the
    /// snapshot for a retry of the SAME step (streaming recovery / non-streaming
    /// fallback) reuse the value computed here instead of re-invoking it.
    pub(crate) async fn deferred_tools_prompt_ingress_attachment(
        &self,
        current_tool_names: &[String],
        tool_pool_refresh_callback_available: Option<bool>,
    ) -> Option<crate::request_context::PromptIngressAttachment> {
        // Currently-deferred (undiscovered) set = oracle `g`: the searchable
        // view minus tools already loaded this session. The view is the
        // `wants_defer` candidate set (which still includes loaded tools), so
        // subtract the loaded names to obtain the `should_defer` set.
        let loaded: std::collections::HashSet<String> = self
            .tools
            .deferral()
            .loaded_tool_names()
            .into_iter()
            .collect();
        let mut current: Vec<String> = self
            .tools
            .tool_search_view()
            .entries()
            .into_iter()
            .map(|entry| entry.name)
            .filter(|name| !loaded.contains(name))
            .collect();
        current.sort();
        current.dedup();
        let deferral = self.tools.deferral();
        if !deferral.keeps_deferred_tools() {
            return None;
        }
        let prior_declared = deferral.declared_tool_names();
        let (model, profile) = {
            let session = self.session.lock().await;
            (session.model.clone(), session.model_profile.clone())
        };
        let scope = self.native_tool_search_scope_snapshot();
        let native_latches = scope.native_tool_search_state();
        let beta_rejections = scope.beta_rejections();
        let mctc_beta =
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::MidConversationToolChanges;
        let mcs_beta =
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::MidConversationSystem;
        let mctc_rejected = beta_rejections.rejected(mctc_beta);
        let mcs_rejected = beta_rejections.rejected(mcs_beta);
        let foamy_spring_enabled = telemetry::feature_flags::flag_bool("tengu_foamy_spring", true);
        let native_mctc_supported = self
            .api
            .native_mid_conversation_tool_change_supported(&model, profile.as_deref());
        let tool_search_filtered_out = self.tools.find_by_name("ToolSearch").is_some()
            && !current_tool_names.iter().any(|name| name == "ToolSearch")
            && !current.is_empty()
            && super::tool_search_supported_for_request(
                self.api.as_ref(),
                &model,
                profile.as_deref(),
            );
        let tool_search_absent = native_tool_search_absent_admitted(
            tool_search_filtered_out,
            native_mctc_supported,
            native_latches.mid_conversation_tool_changes_sent,
            mctc_rejected,
            mcs_rejected,
            native_latches.tool_change_header_refused,
            deferral.keeps_deferred_tools(),
            prior_declared.is_some(),
            tool_pool_refresh_callback_available,
            foamy_spring_enabled,
        );
        if !tool_search_absent && !current_tool_names.iter().any(|name| name == "ToolSearch") {
            return None;
        }
        let pending = Some(match &self.mcp_registry {
            Some(registry) => registry.pending_deferred_tool_servers().await,
            None => Vec::new(),
        });
        let action_states = match &self.mcp_registry {
            Some(registry) => registry.action_states().await,
            None => Vec::new(),
        };
        let mcp_states = action_states
            .iter()
            .map(|(name, state)| (mcp::normalization::normalize_name_for_mcp(name), *state))
            .collect::<std::collections::HashMap<_, _>>();
        // Native supplies this argument only when launchOptions.isInteractive()
        // is false. Do not derive the gate from querySource or bridge transport.
        let needs_auth = if !self.prompt_is_interactive() {
            let mut names = match &self.mcp_registry {
                Some(registry) => registry
                    .needs_auth_deferred_tool_servers()
                    .await
                    .into_iter()
                    .map(|server| server.name)
                    .collect::<Vec<_>>(),
                None => Vec::new(),
            };
            names.sort();
            Some(names)
        } else {
            None
        };
        // `wgn` classifies departed names from the full live MCP state even
        // when the independent `failedMcpServers` attachment flag is off.
        let failed_snapshots = match &self.mcp_registry {
            Some(registry) => registry.failed_deferred_tool_servers().await,
            None => Vec::new(),
        };
        let failed_by_name = failed_snapshots
            .iter()
            .map(|server| {
                (
                    mcp::normalization::normalize_name_for_mcp(&server.name),
                    server,
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        let failed = telemetry::flag_bool("tengu_surface_failed_mcp_servers", true).then(|| {
            failed_snapshots
                .iter()
                .cloned()
                .map(|server| tool_api::defer::DeferredMcpFailedServer {
                    name: server.name,
                    error_code: server.error_code,
                    error: server.error,
                    will_retry: server.will_retry,
                })
                .collect()
        });
        let status = tool_api::defer::DeferredMcpServerStatus {
            pending,
            needs_auth,
            failed,
        };
        let deny_rules = self.perms.tool_wide_deny_names().await;
        let mut configs = std::collections::HashMap::new();
        if let Some(registry) = &self.mcp_registry {
            for name in prior_declared.as_deref().unwrap_or_default() {
                let Some((server_name, _)) = native_mcp_tool_identity(name) else {
                    continue;
                };
                let normalized_server = mcp::normalization::normalize_name_for_mcp(server_name);
                if !configs.contains_key(&normalized_server) {
                    configs.insert(normalized_server, registry.get_config(server_name).await);
                }
            }
        }
        let revoked = prior_declared
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|name| !current_tool_names.iter().any(|current| current == *name))
            .filter_map(|name| {
                let (server_name, _) = native_mcp_tool_identity(name)?;
                let normalized_server = mcp::normalization::normalize_name_for_mcp(server_name);
                let cause = native_mcp_revocation_cause(
                    name,
                    &deny_rules,
                    mcp_states.get(&normalized_server).copied(),
                    configs.get(&normalized_server).and_then(Option::as_ref),
                    failed_by_name.get(&normalized_server).copied(),
                )?;
                Some(tool_api::defer::DeferredToolRevocation {
                    name: name.clone(),
                    cause: cause.to_owned(),
                })
            })
            .collect::<Vec<_>>();
        let delta = deferral.compute_deferred_delta_with_native_state(
            &current,
            status,
            tool_api::defer::DeferredToolNativeContext {
                current_tool_names: current_tool_names.to_vec(),
                declared_pool_available: prior_declared.is_some(),
                revoked,
                // Source-backed Native `pfr` result. Unknown route/model, pool,
                // and refresh facts fail closed in the predicate above.
                tool_search_absent,
                name_only_announcement_uuids: Vec::new(),
            },
        );
        // Consume the previous declared-pool snapshot above, then retain the
        // current pool for the next request's AQo delta.
        deferral.record_declared_tool_names(current_tool_names.iter().cloned());
        if delta.is_empty() {
            return None;
        }
        let payload = delta.attachment_payload();
        let id = MessageId::new();
        let (message, rendered) = if let Some(body) = delta.render_reminder() {
            let message = ConversationMessage::user_meta(id, body);
            let rendered = crate::request_context::rendered_system_message(&message)?;
            (message, rendered)
        } else {
            // Native can persist a status-clearing delta whose renderer returns
            // no user row. Keep its JSONL attachment and placement candidate,
            // while the empty rendered cache stays out of model history.
            let rendered = lingxi_core::types::utf16_json::Utf16JsonProjection::parse("[]").ok()?;
            (ConversationMessage::user_meta(id, String::new()), rendered)
        };
        Some(crate::request_context::PromptIngressAttachment {
            message,
            payload,
            rendered,
        })
    }
}

#[cfg(test)]
mod native_deferred_revocation_tests {
    use super::*;

    #[test]
    fn native_pfr_tool_search_absence_requires_every_source_gate() {
        let admitted = || {
            native_tool_search_absent_admitted(
                true,
                Some(true),
                false,
                false,
                false,
                false,
                true,
                true,
                None,
                true,
            )
        };
        assert!(admitted());
        assert!(!native_tool_search_absent_admitted(
            true, None, false, false, false, false, true, true, None, true,
        ));
        assert!(!native_tool_search_absent_admitted(
            true,
            Some(true),
            false,
            false,
            true,
            false,
            true,
            true,
            None,
            true,
        ));
        assert!(!native_tool_search_absent_admitted(
            true,
            Some(true),
            false,
            true,
            false,
            false,
            true,
            true,
            None,
            true,
        ));
        assert!(!native_tool_search_absent_admitted(
            true,
            Some(true),
            false,
            false,
            false,
            false,
            true,
            false,
            None,
            true,
        ));
        assert!(!native_tool_search_absent_admitted(
            true,
            Some(true),
            false,
            false,
            false,
            false,
            true,
            false,
            Some(false),
            true,
        ));
        assert!(native_tool_search_absent_admitted(
            true,
            Some(true),
            false,
            false,
            false,
            false,
            true,
            false,
            Some(true),
            true,
        ));
        assert!(!native_tool_search_absent_admitted(
            true,
            Some(true),
            true,
            false,
            false,
            false,
            true,
            false,
            Some(true),
            false,
        ));
    }

    fn config(spec: lingxi_core::host::McpTransportSpec) -> mcp::McpServerConfig {
        mcp::McpServerConfig {
            name: "srv".into(),
            spec,
            scope: mcp::ConfigScope::Dynamic,
            disabled: false,
            timeout_ms: None,
            discovery_cache: None,
            always_load: false,
            tools: Vec::new(),
            tool_permissions: std::collections::BTreeMap::new(),
            config_error: None,
            metadata: mcp::McpServerMetadata::default(),
        }
    }

    fn stdio_config() -> mcp::McpServerConfig {
        config(lingxi_core::host::McpTransportSpec::Stdio {
            command: "tool-server".into(),
            args: Vec::new(),
            env: std::collections::HashMap::new(),
        })
    }

    fn source_string(value: &str) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        lingxi_core::types::utf16_json::Utf16JsonProjection::from(value)
    }

    fn deferred_attachment(payload: &str) -> crate::request_context::PromptIngressAttachment {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;

        let message =
            ConversationMessage::user_meta(lingxi_core::types::MessageId::new(), String::new());
        crate::request_context::PromptIngressAttachment {
            message,
            payload: Source::parse(payload).expect("fixture payload is valid JSON"),
            rendered: Source::parse("[]").expect("empty rendered list is valid JSON"),
        }
    }

    #[test]
    fn deferred_tool_change_blocks_preserve_source_schema_and_order() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
        use lingxi_core::types::ContentBlock;

        let input_schema = Source::parse(
            r#"{"type":"object","properties":{"2":{"type":"string"},"10":{"type":"number"}}}"#,
        )
        .unwrap();
        let attachment = deferred_attachment(
            r#"{"type":"deferred_tools_delta","retractedTools":[{"name":"mcp__srv__removed","cause":"disabled"}],"surfacedNames":["mcp__srv__new"],"restoredNames":["mcp__srv__restored"],"toolSearchAbsent":true}"#,
        );
        let message = native_tool_change_api_system_message(
            &attachment,
            &[lingxi_core::host::SourceToolWireDefinition {
                name: "mcp__srv__new".into(),
                description: Source::from("source description"),
                input_schema: input_schema.clone(),
                strict: true,
                defer_loading: true,
            }],
        )
        .expect("tool changes produce one API-system row");
        let ConversationMessage::System {
            api_system: Some(api_system),
            subtype: Some(subtype),
            ..
        } = &message
        else {
            panic!("tool changes use the existing API-system transcript carrier");
        };
        assert_eq!(subtype, "api_system");
        assert_eq!(api_system.content.len(), 3);
        let ContentBlock::ProviderContent {
            protocol: removal_protocol,
            value: removal,
        } = &api_system.content[0]
        else {
            panic!("first Native block is tool_removal");
        };
        assert_eq!(removal_protocol, "anthropic_messages");
        assert_eq!(removal.string_at("/type"), Some("tool_removal"));
        assert_eq!(removal.string_at("/tool/type"), Some("tool_reference"));
        assert_eq!(removal.string_at("/tool/name"), Some("mcp__srv__removed"));

        let ContentBlock::ProviderContent {
            value: addition, ..
        } = &api_system.content[1]
        else {
            panic!("second Native block is tool_addition");
        };
        assert_eq!(addition.string_at("/type"), Some("tool_addition"));
        assert_eq!(addition.string_at("/tool/type"), Some("tool_definition"));
        assert_eq!(
            addition.string_at("/tool/definition/name"),
            Some("mcp__srv__new")
        );
        assert_eq!(
            addition.string_at("/tool/definition/description"),
            Some("source description")
        );
        assert_eq!(
            addition
                .subprojection("/tool/definition/input_schema")
                .unwrap(),
            input_schema,
            "schemas stay as source projections rather than passing through serde"
        );
        assert_eq!(
            addition
                .at_pointer("/tool/definition/strict")
                .and_then(|value| value.as_bool()),
            Some(true)
        );

        let ContentBlock::ProviderContent {
            value: restored, ..
        } = &api_system.content[2]
        else {
            panic!("third Native block restores a deferred tool");
        };
        assert_eq!(restored.string_at("/tool/type"), Some("tool_reference"));
        assert_eq!(restored.string_at("/tool/name"), Some("mcp__srv__restored"));
        assert_eq!(
            native_tool_change_addition_names(std::slice::from_ref(&message)),
            std::collections::HashSet::from([
                "mcp__srv__new".to_owned(),
                "mcp__srv__restored".to_owned(),
            ])
        );
    }

    #[test]
    fn deferred_tool_name_only_record_requires_a_name_only_surfacing_path() {
        let attachment = deferred_attachment(
            r#"{"type":"deferred_tools_delta","surfacedNames":["alpha","beta"]}"#,
        );
        let delta_uuid = attachment.message.id().to_string();
        let deferral = tool_api::DeferralState::new(tool_api::ToolSearchMode::Enabled, false);
        let inline_names = std::collections::HashSet::from(["alpha".to_owned()]);
        let addition_names = std::collections::HashSet::from(["beta".to_owned()]);

        assert!(deferred_tools_name_only_record_attachment(
            &attachment,
            &deferral,
            false,
            true,
            &inline_names,
            &addition_names,
        )
        .is_none());
        assert!(deferred_tools_name_only_record_attachment(
            &attachment,
            &deferral,
            true,
            true,
            &inline_names,
            &addition_names,
        )
        .is_none());

        let name_only = deferred_tools_name_only_record_attachment(
            &attachment,
            &deferral,
            true,
            true,
            &inline_names,
            &std::collections::HashSet::new(),
        )
        .expect("a surfaced name missing from both current wire paths latches its UUID");
        assert_eq!(
            name_only.payload.string_at("/type"),
            Some("deferred_tools_record")
        );
        assert_eq!(
            name_only.payload.array_len("/nameOnlyAnnouncements"),
            Some(1)
        );
        assert_eq!(
            name_only.payload.string_at("/nameOnlyAnnouncements/0"),
            Some(delta_uuid.as_str())
        );
    }

    #[test]
    fn tool_search_absence_api_system_additions_use_the_current_inline_definitions() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
        use lingxi_core::types::ContentBlock;

        let attachment = deferred_attachment(
            r#"{"type":"deferred_tools_delta","surfacedNames":["alpha","beta"],"toolSearchAbsent":true}"#,
        );
        let message = native_tool_change_api_system_message(
            &attachment,
            &[lingxi_core::host::SourceToolWireDefinition {
                name: "alpha".into(),
                description: Source::from("alpha description"),
                input_schema: Source::parse(r#"{"type":"object"}"#).unwrap(),
                strict: false,
                defer_loading: true,
            }],
        )
        .expect("Native pfr path defines surfaced names through API-system blocks");
        let ConversationMessage::System {
            api_system: Some(api_system),
            ..
        } = &message
        else {
            panic!("tool additions use an API-system row");
        };
        assert_eq!(api_system.content.len(), 2);
        let ContentBlock::ProviderContent { value: alpha, .. } = &api_system.content[0] else {
            panic!("first addition is alpha");
        };
        assert_eq!(alpha.string_at("/tool/type"), Some("tool_definition"));
        assert_eq!(alpha.string_at("/tool/definition/name"), Some("alpha"));
        let ContentBlock::ProviderContent { value: beta, .. } = &api_system.content[1] else {
            panic!("second addition is beta");
        };
        assert_eq!(beta.string_at("/tool/type"), Some("tool_reference"));
        assert_eq!(beta.string_at("/tool/name"), Some("beta"));

        let ordinary_tool_search =
            deferred_attachment(r#"{"type":"deferred_tools_delta","surfacedNames":["alpha"]}"#);
        assert!(native_tool_change_api_system_message(&ordinary_tool_search, &[]).is_none());
    }

    #[test]
    fn wgn_retains_only_native_revocation_causes() {
        let blocked_config = {
            let mut config = stdio_config();
            config.tool_permissions.insert(
                "write".into(),
                lingxi_core::host::McpPermissionCeiling::Deny,
            );
            config
        };
        let sdk_config = config(lingxi_core::host::McpTransportSpec::SdkControl {
            control_channel_id: "host".into(),
        });
        let policy_failure = mcp::McpFailedServerSnapshot {
            name: "srv".into(),
            error_code: None,
            error: Some(source_string("Blocked by enterprise managed policy")),
            native_raw_error: Some(source_string("Blocked by enterprise managed policy")),
            will_retry: None,
        };
        let unconfigured_failure = mcp::McpFailedServerSnapshot {
            name: "srv".into(),
            error_code: Some(source_string("UNCONFIGURED")),
            error: None,
            native_raw_error: None,
            will_retry: None,
        };

        assert_eq!(
            native_mcp_revocation_cause(
                "mcp__srv__tool",
                &["mcp__srv__to*".into()],
                Some(lingxi_core::host::McpActionState::Connected),
                None,
                None,
            ),
            Some("denied"),
            "Native wildcard deny patterns are evaluated on the normalized full MCP name"
        );
        assert_eq!(
            native_mcp_revocation_cause(
                "mcp__srv__write",
                &[],
                Some(lingxi_core::host::McpActionState::Connected),
                Some(&blocked_config),
                None,
            ),
            Some("org_blocked")
        );
        assert_eq!(
            native_mcp_revocation_cause(
                "mcp__srv__tool",
                &[],
                Some(lingxi_core::host::McpActionState::Disabled),
                Some(&stdio_config()),
                None,
            ),
            Some("disabled")
        );
        assert_eq!(
            native_mcp_revocation_cause(
                "mcp__srv__tool",
                &[],
                Some(lingxi_core::host::McpActionState::Disabled),
                Some(&sdk_config),
                None,
            ),
            Some("switched_off")
        );
        assert_eq!(
            native_mcp_revocation_cause(
                "mcp__srv__tool",
                &[],
                Some(lingxi_core::host::McpActionState::Failed),
                None,
                Some(&policy_failure),
            ),
            Some("policy_blocked")
        );
        let display_only_policy_failure = mcp::McpFailedServerSnapshot {
            name: "srv".into(),
            error_code: None,
            error: Some(source_string("Blocked by enterprise managed policy")),
            native_raw_error: None,
            will_retry: None,
        };
        assert_eq!(
            native_mcp_revocation_cause(
                "mcp__srv__tool",
                &[],
                Some(lingxi_core::host::McpActionState::Failed),
                None,
                Some(&display_only_policy_failure),
            ),
            None,
            "displayDetail-only text must not become a Native policy_blocked cause"
        );
        assert_eq!(
            native_mcp_revocation_cause(
                "mcp__srv__tool",
                &[],
                Some(lingxi_core::host::McpActionState::Failed),
                None,
                Some(&unconfigured_failure),
            ),
            Some("not_configured")
        );
        let disconnected = mcp::McpFailedServerSnapshot {
            name: "srv".into(),
            error_code: None,
            error: Some(source_string("connection reset")),
            native_raw_error: Some(source_string("connection reset")),
            will_retry: Some(true),
        };
        assert_eq!(
            native_mcp_revocation_cause(
                "mcp__srv__tool",
                &[],
                Some(lingxi_core::host::McpActionState::Failed),
                None,
                Some(&disconnected),
            ),
            None,
            "Native disconnected and retrying failures are not retractedTools causes"
        );
        assert_eq!(
            native_mcp_revocation_cause("mcp__srv__tool", &[], None, None, None),
            Some("not_configured"),
            "Native maps a missing current MCP client row to not_configured"
        );
    }
}

impl ConversationOrchestrator {
    pub(crate) async fn mod_prompt_attachment_with_detail(
        &self,
        kind: &str,
        mut message: ConversationMessage,
        origin: serde_json::Value,
        detail: Option<serde_json::Value>,
    ) -> Option<ConversationMessage> {
        let Some(host) = (if let Some(registry) = &self.lifecycle_runtime.hook_registry {
            registry.read().await.mod_host()
        } else {
            None
        })
        .filter(|host| host.has_event("prompt.attachment")) else {
            return Some(message);
        };
        let message_id = message.id();
        let ConversationMessage::User { content, .. } = &mut message else {
            return Some(message);
        };
        use lingxi_core::types::utf16_json::Utf16JsonProjection;
        use lingxi_core::types::ContentBlock;
        let (units, citations, was_utf16) = match content.as_slice() {
            [ContentBlock::Text { text, citations }] => (
                text.encode_utf16().collect::<Vec<_>>(),
                citations.clone(),
                false,
            ),
            [ContentBlock::TextJsUtf16 {
                utf16_code_units,
                citations,
                ..
            }] => (utf16_code_units.clone(), citations.clone(), true),
            _ => return Some(message),
        };
        let prefix = "<system-reminder>\n".encode_utf16().collect::<Vec<_>>();
        let suffix = "\n</system-reminder>".encode_utf16().collect::<Vec<_>>();
        let inner = units
            .strip_prefix(prefix.as_slice())
            .and_then(|body| body.strip_suffix(suffix.as_slice()));
        let wrapped = inner.is_some();
        let body_units = inner.unwrap_or(&units).to_vec();
        let body = String::from_utf16_lossy(&body_units);
        // Native JHo omits attachments whose unframed rendered text is blank.
        if body.trim().is_empty() {
            return Some(message);
        }
        let mut input = Utf16JsonProjection::from_serde_value(
            &serde_json::json!({"type":kind,"text":body,"origin":origin}),
        )
        .expect("host-generated prompt attachment is valid JSON");
        if body.encode_utf16().ne(body_units.iter().copied()) {
            input
                .set_string_units("/text", &body_units)
                .expect("prompt attachment text field is a string");
        }
        if let Some(detail) = detail {
            input
                .set_field(
                    "detail",
                    Utf16JsonProjection::from_serde_value(&detail)
                        .expect("host-generated attachment detail is valid JSON"),
                )
                .expect("prompt attachment input is an object");
        }
        let identity = host.registration_identity();
        let (generation, cached) = {
            let cache = self.prompt_runtime.mod_prompt_attachments.lock().await;
            let cached = cache
                .answers
                .get(&message_id)
                .and_then(|(source, catalog, answer)| {
                    (source == &input && catalog == &identity).then_some(answer.clone())
                });
            (cache.generation, cached)
        };
        let answer = if let Some(cached) = cached {
            cached
        } else {
            let pinned_kind = kind.to_owned();
            let pinned_origin = Utf16JsonProjection::from_serde_value(&origin)
                .expect("host-generated attachment origin is valid JSON");
            let log_output = self.output.clone();
            let toast_output = self.output.clone();
            let status_output = self.output.clone();
            let result = host
                .dispatch_with_utf16_at_context_scope(
                    "prompt.attachment",
                    input.clone(),
                    &self.current_cwd(),
                    Some(self),
                    hooks::mods::ModDispatchScope::default(),
                    lingxi_core::host::task_registry::FieldPresence::Missing,
                    move |forwarded| {
                        let pinned_kind = pinned_kind.clone();
                        let pinned_origin = pinned_origin.clone();
                        async move {
                            let Some(forwarded) = forwarded else {
                                return Err(hooks::mods::ModError::Hook(
                                    "prompt.attachment needs forwarded input".into(),
                                ));
                            };
                            let pinned_kind_units = pinned_kind.encode_utf16().collect::<Vec<_>>();
                            if forwarded.string_units("/type").as_deref()
                                != Some(pinned_kind_units.as_slice())
                                || forwarded
                                    .subprojection("/origin")
                                    .map_or(true, |origin| !origin.semantically_eq(&pinned_origin))
                                || forwarded.contains_property("agentId")
                            {
                                return Err(hooks::mods::ModError::Hook(
                                    "prompt.attachment type and origin are pinned".into(),
                                ));
                            }
                            let text = forwarded.string_units("/text").ok_or_else(|| {
                                hooks::mods::ModError::Hook("prompt.attachment needs text".into())
                            })?;
                            let value = Utf16JsonProjection::root_string(
                                String::from_utf16_lossy(&text),
                                text,
                            )
                            .expect("forwarded text is valid exact UTF-16");
                            Utf16JsonProjection::object(vec![("text".into(), value)])
                                .map(Some)
                                .map_err(|error| hooks::mods::ModError::Protocol(error.to_string()))
                        }
                    },
                    move |plugin, text| {
                        let output = log_output.clone();
                        async move { output.emit_mod_log(&plugin, &text).await }
                    },
                    move |plugin, text, timeout_ms| {
                        let output = toast_output.clone();
                        async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
                    },
                    move |plugin, text| {
                        let output = status_output.clone();
                        async move { output.emit_mod_status(&plugin, text.as_ref()).await }
                    },
                )
                .await;
            let answer = match result {
                Ok(result)
                    if result
                        .result_projection
                        .at_pointer("/text")
                        .is_some_and(|value| value.is_null()) =>
                {
                    None
                }
                Ok(result) => result.result_projection.string_units("/text"),
                Err(error) => {
                    tracing::warn!(attachment = kind, %error, "prompt.attachment Mod failed");
                    Some(body_units)
                }
            };
            let mut cache = self.prompt_runtime.mod_prompt_attachments.lock().await;
            if cache.generation == generation {
                cache
                    .answers
                    .insert(message_id, (input, identity, answer.clone()));
            }
            answer
        };
        let answer = answer?;
        let answer = if wrapped {
            [prefix, answer, suffix].concat()
        } else {
            answer
        };
        content[0] = match String::from_utf16(&answer) {
            Ok(text) if !was_utf16 => ContentBlock::Text { text, citations },
            _ => ContentBlock::TextJsUtf16 {
                text: String::from_utf16_lossy(&answer),
                utf16_code_units: answer,
                citations,
            },
        };
        Some(message)
    }
}

#[cfg(test)]
mod native_read_route_normalization_tests {
    use super::absolute_normalized_read_path;
    use std::path::{Path, PathBuf};

    #[test]
    fn absolute_route_clamps_parent_components_at_the_root() {
        assert_eq!(
            absolute_normalized_read_path(Path::new("/../../tmp/read/a.rs")),
            Some(PathBuf::from("/tmp/read/a.rs"))
        );
        assert_eq!(
            absolute_normalized_read_path(Path::new("/tmp/../read/./a.rs")),
            Some(PathBuf::from("/read/a.rs"))
        );
    }

    #[test]
    fn relative_source_route_is_unavailable_without_its_session_cwd() {
        assert_eq!(
            absolute_normalized_read_path(Path::new("relative/read.rs")),
            None
        );
    }
}

#[cfg(test)]
mod async_hook_prompt_publication_tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;

    struct GenerationGuard(lingxi_core::host::CancellationToken);

    impl HookPublicationGuard for GenerationGuard {
        fn is_current(&self) -> bool {
            !self.0.is_cancelled()
        }

        fn generation_cancellation_token(&self) -> Option<lingxi_core::host::CancellationToken> {
            Some(self.0.clone())
        }

        fn publish_if_current<'a>(
            &'a self,
            publication: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            let root = self.0.clone();
            Box::pin(async move {
                tokio::select! {
                    biased;
                    () = root.cancelled() => false,
                    () = publication => true,
                }
            })
        }

        fn commit_if_current<'a>(
            &'a self,
            mutation: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            self.publish_if_current(mutation)
        }
    }

    struct FutureDrop(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for FutureDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn reset_during_async_hook_mod_attachment_drops_stale_result() {
        let root = lingxi_core::host::CancellationToken::new();
        let guard: Arc<dyn HookPublicationGuard> = Arc::new(GenerationGuard(root.clone()));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (_release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dropped_for_work = Arc::clone(&dropped);
        let work = async move {
            let _drop = FutureDrop(dropped_for_work);
            let _ = entered_tx.send(());
            let _ = release_rx.await;
            Some("stale modifier result")
        };
        let attachment = tokio::spawn(crate::prompt::async_hook_response::run_hook_prompt_work(
            guard, work,
        ));

        entered_rx.await.expect("modifier reached its gated await");
        root.cancel();

        assert_eq!(
            attachment.await.expect("cancelled modifier wrapper"),
            None,
            "reset must not return a prompt message produced by the old generation"
        );
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "reset must cancel the in-progress prompt modifier future"
        );

        let live: Arc<dyn HookPublicationGuard> =
            Arc::new(GenerationGuard(lingxi_core::host::CancellationToken::new()));
        assert_eq!(
            crate::prompt::async_hook_response::run_hook_prompt_work(live, async { Some("live") },)
                .await,
            Some(Some("live")),
            "a current generation remains able to attach its async-hook reminder"
        );
    }

    #[tokio::test]
    async fn reset_before_prompt_request_admission_prunes_obsolete_hook_reminder() {
        let root = lingxi_core::host::CancellationToken::new();
        let guard: Arc<dyn HookPublicationGuard> = Arc::new(GenerationGuard(root.clone()));
        let reminder = ConversationMessage::user_meta(MessageId::new(), "old hook result".into());
        let reminder_id = reminder.id();
        let user = ConversationMessage::user(MessageId::new(), "current request".into());
        let mut messages = vec![reminder.clone(), user.clone()];
        let mut turn_reminders = vec![reminder];
        let mut guards = vec![(reminder_id, guard)];
        root.cancel();
        crate::prompt::async_hook_response::retain_current_async_hook_reminders(
            &mut messages,
            &mut turn_reminders,
            &mut guards,
        );
        let texts: Vec<_> = messages
            .iter()
            .map(ConversationMessage::text_content)
            .collect();
        assert_eq!(texts, ["current request"]);

        let live_guard: Arc<dyn HookPublicationGuard> =
            Arc::new(GenerationGuard(lingxi_core::host::CancellationToken::new()));
        let live_reminder =
            ConversationMessage::user_meta(MessageId::new(), "live hook result".into());
        let live_reminder_id = live_reminder.id();
        let mut live_messages = vec![live_reminder.clone(), user];
        let mut live_turn_reminders = vec![live_reminder];
        let mut live_guards = vec![(live_reminder_id, live_guard)];
        crate::prompt::async_hook_response::retain_current_async_hook_reminders(
            &mut live_messages,
            &mut live_turn_reminders,
            &mut live_guards,
        );
        assert_eq!(live_messages.len(), 2, "a live hook reminder is admitted");
        assert_eq!(live_guards.len(), 1);
    }
}

#[cfg(test)]
mod prefetch_history_tests {
    use super::*;
    use lingxi_core::types::{ConversationMessage, MessageId, SessionId};
    use lingxi_core::SessionState;

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.to_string())
    }

    #[test]
    fn prefetch_starts_from_model_visible_history() {
        let excluded = user("background transcript row");
        let invisible = user("\u{200B}\u{FEFF}");
        let current = user("current request");
        let mut state = SessionState::empty(SessionId::nil(), "model".into());
        state.model_context_excluded_messages.insert(excluded.id());
        state.history.extend([excluded, invisible, current]);

        let visible = state.model_context_history();
        let (query, tools) = ConversationOrchestrator::latest_prefetch_query_and_tools(&visible);

        assert_eq!(query, "current request");
        assert!(tools.is_empty());
    }
}
