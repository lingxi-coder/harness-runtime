//! `/list-agents` command API.
//!
//! The current command lists agents and teammates owned by this runtime
//! session. A runtime provider supplies that live roster; command-api does not
//! infer an empty list when the task registry or team snapshot is unavailable.

use crate::model::{
    source_concat, BuiltinCommandHandler, CommandResult, NativeCommandType, PromptPart,
};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::Arc;
use unicode_normalization::UnicodeNormalization;

/// Native `list-agents` command alias. Registry wiring installs it with the
/// canonical command so both names resolve to the same live handler.
pub const LIST_AGENTS_ALIAS: &str = "peers";

/// Current host gate for Native's `Ri` / `tengu_harbor_kite` predicate.
/// Unknown is unavailable; local runtime assembly supplies its source-backed
/// default and any explicit override.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListAgentsCommandAvailability {
    /// Whether the local agent-listing feature is enabled for this host.
    pub harbor_kite_enabled: Option<bool>,
}

impl ListAgentsCommandAvailability {
    /// The handler is available only for an explicit enabled verdict.
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self.harbor_kite_enabled, Some(true))
    }
}

/// A local subagent row from the current task registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSubagentRow {
    /// Current addressable name, if the name registry has one.
    pub name: Option<Utf16JsonProjection>,
    /// Resolved agent type, required by Native's local-agent renderer.
    pub agent_type: Utf16JsonProjection,
    /// Current task status.
    pub status: Utf16JsonProjection,
    /// Native start timestamp. A provider must fail the snapshot if this
    /// source fact is missing instead of synthesizing epoch zero.
    pub started_at_ms: u64,
}

/// How the current teammate name collides with local addressable subagents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TeammateNameShadowing {
    /// No collision is known.
    #[default]
    None,
    /// The exact teammate address is held by a local subagent.
    Unreachable,
    /// Only a normalized/variant form is held by a local subagent.
    BareOnly,
}

/// Whether a teammate's relative time describes joining or starting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeammateTimeKind {
    /// A pane/roster teammate joined the team.
    Joined,
    /// An in-process teammate started.
    Started,
}

/// Optional teammate timing text. The source omits this when its timestamp is
/// absent, invalid, in the future, or older than its display window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTeammateSince {
    /// Native's `joined` or `started` label.
    pub kind: TeammateTimeKind,
    /// Source timestamp in Unix milliseconds.
    pub timestamp_ms: u64,
}

/// A current-session team member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTeammateRow {
    /// Display name, if supplied by the current team source.
    pub name: Option<Utf16JsonProjection>,
    /// Agent type when supplied by the source.
    pub agent_type: Option<Utf16JsonProjection>,
    /// Task status when an in-process task owns this teammate.
    pub status: Option<Utf16JsonProjection>,
    /// Native backend label used when task status is absent (for example,
    /// `pane` or `roster`).
    pub backend: Utf16JsonProjection,
    /// Optional source-derived teammate timing.
    pub since: Option<LocalTeammateSince>,
    /// Current addressability warning from the same agent-name registry.
    pub name_shadowing: TeammateNameShadowing,
}

/// A successful, source-ordered current-session roster snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalAgentRoster {
    /// Current local subagent task rows, in registry order.
    pub subagents: Vec<LocalSubagentRow>,
    /// Current team members belonging to this runtime session.
    pub teammates: Vec<LocalTeammateRow>,
    /// Native `localListFailed` fact for the separate local-session query. If
    /// peer discovery is not wired, callers set this true so the response
    /// warns that other local sessions may be missing.
    pub local_list_failed: bool,
}

/// Runtime-owned producer for the current session's local agent roster.
///
/// Implementations should use the ordered task registry and current team/name
/// stores. Missing stores, failed reads, or missing required subagent facts
/// must return an error instead of masquerading as an empty result. Cross-
/// session UDS/cloud/bridge/DID discovery is intentionally outside this
/// provider; set `LocalAgentRoster::local_list_failed` when the local peer-list
/// query itself is not wired.
#[async_trait]
pub trait ListAgentsProvider: Send + Sync {
    /// Current value of Native's local feature gate.
    fn command_availability(&self) -> ListAgentsCommandAvailability;

    /// Read a successful snapshot of local agents and teammates in source
    /// order. A task-registry/team-store failure is an error, not an empty
    /// roster.
    async fn snapshot_local_roster(&self) -> Result<LocalAgentRoster, Utf16JsonProjection>;
}

/// `/list-agents` handler backed by the runtime's live task/team state.
pub struct ListAgentsHandler {
    provider: Arc<dyn ListAgentsProvider>,
}

impl std::fmt::Debug for ListAgentsHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListAgentsHandler").finish_non_exhaustive()
    }
}

impl ListAgentsHandler {
    /// Bind the handler to a real runtime roster provider.
    #[must_use]
    pub fn new(provider: Arc<dyn ListAgentsProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl BuiltinCommandHandler for ListAgentsHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        if !self.provider.command_availability().is_available() {
            return CommandResult::Done {
                display: Some("Agent listing is unavailable in this session.".into()),
            };
        }

        match self.provider.snapshot_local_roster().await {
            Ok(roster) => CommandResult::Done {
                display: Some(render_local_roster(&roster)),
            },
            Err(error) => CommandResult::Done {
                display: Some(error),
            },
        }
    }

    fn name(&self) -> &str {
        "list-agents"
    }

    fn description(&self) -> &str {
        "List subagents, teammates, and other LingXi sessions you can message"
    }

    fn aliases(&self) -> Vec<Utf16JsonProjection> {
        vec![LIST_AGENTS_ALIAS.into()]
    }

    fn native_type(&self) -> Option<NativeCommandType> {
        Some(NativeCommandType::Local)
    }

    fn supports_non_interactive(&self) -> bool {
        true
    }

    fn is_enabled(&self) -> bool {
        self.provider.command_availability().is_available()
    }
}

/// Render the local sections this provider can authoritatively supply using
/// Native's source order, row formatting, and teammate cap. This does not claim
/// the separate UDS, cloud, bridge, or DID session sections.
fn render_local_roster(roster: &LocalAgentRoster) -> Utf16JsonProjection {
    render_local_roster_at(roster, current_time_ms())
}

fn render_local_roster_at(roster: &LocalAgentRoster, now_ms: u64) -> Utf16JsonProjection {
    if roster.subagents.is_empty() && roster.teammates.is_empty() {
        if roster.local_list_failed {
            return LOCAL_LIST_FAILED_WARNING.into();
        }
        return "No subagents, teammates or other LingXi sessions.".into();
    }

    let subagent_count = roster.subagents.len().to_string();
    let teammate_count = roster.teammates.len().to_string();
    let subagent_ages: Vec<_> = roster
        .subagents
        .iter()
        .map(|row| relative_age(now_ms, row.started_at_ms))
        .collect();
    let teammate_ages: Vec<_> = roster
        .teammates
        .iter()
        .map(|row| {
            row.since
                .as_ref()
                .and_then(|since| teammate_age(now_ms, since.timestamp_ms))
        })
        .collect();
    let rendered_teammates: Vec<_> = roster
        .teammates
        .iter()
        .map(|row| RenderedTeammateRow {
            name: native_teammate_field(row.name.as_ref()),
            agent_type: native_teammate_field(row.agent_type.as_ref()),
            status: native_teammate_field(row.status.as_ref()),
            backend: &row.backend,
            since_kind: row.since.as_ref().map(|since| since.kind),
            name_shadowing: row.name_shadowing,
        })
        .collect();
    let omitted_teammate_count = roster.teammates.len().saturating_sub(100).to_string();
    let mut parts = Vec::new();

    if !roster.subagents.is_empty() {
        parts.push(PromptPart::Text("Subagents ("));
        parts.push(PromptPart::Text(&subagent_count));
        parts.push(PromptPart::Text("):\n"));
        append_subagent_rows(&mut parts, &roster.subagents, &subagent_ages);
    }

    if !roster.teammates.is_empty() {
        if !parts.is_empty() {
            parts.push(PromptPart::Text("\n\n"));
        }
        parts.push(PromptPart::Text("Teammates ("));
        parts.push(PromptPart::Text(&teammate_count));
        parts.push(PromptPart::Text("):\n"));
        append_teammate_rows(
            &mut parts,
            &rendered_teammates,
            &teammate_ages,
            &omitted_teammate_count,
        );
    }

    if roster.local_list_failed {
        parts.push(PromptPart::Text("\n\n"));
        parts.push(PromptPart::Text(LOCAL_LIST_FAILED_WARNING));
    }

    source_concat(parts)
}

fn append_subagent_rows<'a>(
    parts: &mut Vec<PromptPart<'a>>,
    rows: &'a [LocalSubagentRow],
    ages: &'a [Utf16JsonProjection],
) {
    for (index, (row, age)) in rows.iter().zip(ages).enumerate() {
        if index > 0 {
            parts.push(PromptPart::Text("\n"));
        }
        parts.push(PromptPart::Text("  ["));
        parts.push(PromptPart::Source(&row.status));
        parts.push(PromptPart::Text("]  ·  "));
        if let Some(name) = row.name.as_ref() {
            parts.push(PromptPart::Source(name));
            parts.push(PromptPart::Text("  ·  "));
            parts.push(PromptPart::Source(&row.agent_type));
        } else {
            parts.push(PromptPart::Source(&row.agent_type));
        }
        parts.push(PromptPart::Text("  ·  started "));
        parts.push(PromptPart::Source(age));
        parts.push(PromptPart::Text(" ago"));
    }
}

struct RenderedTeammateRow<'a> {
    name: Option<Utf16JsonProjection>,
    agent_type: Option<Utf16JsonProjection>,
    status: Option<Utf16JsonProjection>,
    backend: &'a Utf16JsonProjection,
    since_kind: Option<TeammateTimeKind>,
    name_shadowing: TeammateNameShadowing,
}

fn append_teammate_rows<'a>(
    parts: &mut Vec<PromptPart<'a>>,
    rows: &'a [RenderedTeammateRow<'a>],
    ages: &'a [Option<Utf16JsonProjection>],
    omitted_count: &'a str,
) {
    const MAX_DISPLAYED: usize = 100;
    for (index, (row, age)) in rows.iter().zip(ages).take(MAX_DISPLAYED).enumerate() {
        if index > 0 {
            parts.push(PromptPart::Text("\n"));
        }
        parts.push(PromptPart::Text("  ["));
        if let Some(status) = row.status.as_ref() {
            parts.push(PromptPart::Source(status));
        } else {
            parts.push(PromptPart::Source(row.backend));
        }
        parts.push(PromptPart::Text("]  ·  "));
        if let Some(name) = row.name.as_ref() {
            parts.push(PromptPart::Source(name));
        } else {
            parts.push(PromptPart::Text("(unnamed)"));
        }
        if let Some(agent_type) = row.agent_type.as_ref() {
            parts.push(PromptPart::Text("  ·  "));
            parts.push(PromptPart::Source(agent_type));
        }
        if let (Some(since), Some(age)) = (row.since_kind, age.as_ref()) {
            parts.push(PromptPart::Text("  ·  "));
            parts.push(PromptPart::Text(match since {
                TeammateTimeKind::Joined => "joined ",
                TeammateTimeKind::Started => "started ",
            }));
            parts.push(PromptPart::Source(age));
            parts.push(PromptPart::Text(" ago"));
        }
        match row.name_shadowing {
            TeammateNameShadowing::None => {}
            TeammateNameShadowing::Unreachable => {
                parts.push(PromptPart::Text("  ·  name held by a subagent here"));
            }
            TeammateNameShadowing::BareOnly => {
                parts.push(PromptPart::Text(
                    "  ·  exact name only (a subagent holds a variant)",
                ));
            }
        }
    }
    let omitted = rows.len().saturating_sub(MAX_DISPLAYED);
    if omitted > 0 {
        parts.push(PromptPart::Text("\n"));
        parts.push(PromptPart::Text("  (… "));
        parts.push(PromptPart::Text(omitted_count));
        parts.push(PromptPart::Text(" more not shown)"));
    }
}

/// Native 2.1.295 `FRe` applies `oue` to teammate name/type/status before it
/// prints them. `oue` strips Cc/Cf/Cs (except JS whitespace), collapses JS
/// whitespace, takes the first 200 Unicode code points, and trims again.
/// The current core `sanitize_display` is `w1` (160 UTF-16 units), so it is not
/// the same helper. Clean the UTF-16 source units directly to avoid replacing
/// malformed units before Native's Cs removal.
fn native_teammate_field(value: Option<&Utf16JsonProjection>) -> Option<Utf16JsonProjection> {
    let source_units = value?.string_units("")?;
    let mut scalar_text = String::new();
    for decoded in char::decode_utf16(source_units) {
        if let Ok(character) = decoded {
            scalar_text.push(character);
        }
    }

    let normalized = lingxi_core::host::display::sanitize_display_to(&scalar_text, usize::MAX);
    let truncated = normalized.chars().take(200).collect::<String>();
    let text = truncated.trim_end_matches(' ');
    let text = if text.is_empty() {
        "untitled session"
    } else {
        text
    };

    if is_native_fre_rejected_label(text) {
        return None;
    }

    let text = text.to_owned();
    Utf16JsonProjection::root_string(text.clone(), text.encode_utf16().collect()).ok()
}

/// The rest of Native's `FRe`: reject socket schemes/paths after testing both
/// the normalized input and the raw display form. `Qr` applies NFKC, removes
/// the same Unicode classes as `oue`, trims, lowercases, and turns whitespace
/// into hyphens before `Iy`/`fwo` classify it.
fn is_native_fre_rejected_label(value: &str) -> bool {
    let route_key = native_route_key(value);
    is_native_socket_or_pipe_address(value)
        || is_native_socket_or_pipe_address(&route_key)
        || is_native_unc_or_device_path(value)
}

fn native_route_key(value: &str) -> String {
    let compatibility_normalized = value.nfkc().collect::<String>();
    lingxi_core::host::display::sanitize_display_to(&compatibility_normalized, usize::MAX)
        .to_lowercase()
        .replace(' ', "-")
}

/// Fixed-source `Iy` and `fwo` checks used by `wOt`.
fn is_native_socket_or_pipe_address(value: &str) -> bool {
    value.starts_with("uds:")
        || value.starts_with("bridge:")
        || value.starts_with("did:")
        || is_unix_socket_path(value)
        || is_windows_pipe(value)
}

fn is_unix_socket_path(value: &str) -> bool {
    let Some(path) = value.strip_prefix('/') else {
        return false;
    };
    let first_segment = path.split(' ').next().unwrap_or_default();
    first_segment.contains(".sock")
}

fn is_windows_pipe(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 9
        && (bytes.starts_with(b"//") || bytes.starts_with(b"\\\\"))
        && matches!(bytes[2], b'.' | b'?')
        && matches!(bytes[3], b'/' | b'\\')
        && value[4..].get(..5).is_some_and(|prefix| {
            prefix.eq_ignore_ascii_case("pipe/") || prefix.eq_ignore_ascii_case("pipe\\")
        })
}

/// Fixed-source `_B(e)`: Native rejects UNC roots and paths whose win32
/// normalization becomes the `\??\` device namespace. Valid named pipes are
/// already rejected by `wOt`'s `q` socket-path predicate.
fn is_native_unc_or_device_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.starts_with(b"//") || bytes.starts_with(b"\\\\") {
        return true;
    }
    if starts_with_nt_device_namespace(value) {
        return true;
    }
    value.contains("??") && win32_normalizes_to_nt_device_namespace(value)
}

fn starts_with_nt_device_namespace(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 4
        && matches!(bytes[0], b'/' | b'\\')
        && bytes[1] == b'?'
        && bytes[2] == b'?'
        && matches!(bytes[3], b'/' | b'\\')
}

/// The source calls `path.win32.normalize` on all platforms. Only the prefix
/// result is observable here, so reproduce its separator/dot-segment behavior
/// without needing a host-platform path parser.
fn win32_normalizes_to_nt_device_namespace(value: &str) -> bool {
    let normalized_separators = value.replace('/', "\\");
    let bytes = normalized_separators.as_bytes();
    let rooted = bytes.first() == Some(&b'\\');
    let trailing_separator = bytes.last() == Some(&b'\\');
    let mut segments: Vec<&str> = Vec::new();
    for segment in normalized_separators.split('\\') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|last| *last != "..") {
                    segments.pop();
                } else if !rooted {
                    segments.push(segment);
                }
            }
            _ => segments.push(segment),
        }
    }
    rooted && segments.first() == Some(&"??") && (segments.len() > 1 || trailing_separator)
}

const LOCAL_LIST_FAILED_WARNING: &str = "(the session list on this machine could not be read just now — other local sessions may be missing from this listing; try /list-agents again shortly)";

/// Native 2.1.295 `rn(ms,{mostSignificantOnly:true})`, bound from
/// `chunk-8ymxsjn1.js` at fixed VFS byte 186,909,762 (SHA-256
/// `3a733f757a838c84e4e351efef09ea29818b806212c17078830dd3866b309b11`).
/// `chunk-hv2gtdpr.js` binds the helper and calls it from `T` at source-local
/// byte 15,008. The input here is the same non-negative
/// `Date.now() - sourceTimestamp` delta.
#[must_use]
pub fn relative_age(now_ms: u64, source_timestamp_ms: u64) -> Utf16JsonProjection {
    let elapsed_ms = now_ms.saturating_sub(source_timestamp_ms);
    let output = if elapsed_ms < 60_000 {
        if elapsed_ms == 0 {
            "0s".to_owned()
        } else {
            format!("{}s", elapsed_ms / 1_000)
        }
    } else {
        let mut days = elapsed_ms / 86_400_000;
        let mut hours = (elapsed_ms % 86_400_000) / 3_600_000;
        let mut minutes = (elapsed_ms % 3_600_000) / 60_000;
        let mut seconds = (elapsed_ms % 60_000 + 500) / 1_000;
        if seconds == 60 {
            seconds = 0;
            minutes += 1;
        }
        if minutes == 60 {
            minutes = 0;
            hours += 1;
        }
        if hours == 24 {
            hours = 0;
            days += 1;
        }

        if days > 0 {
            format!("{days}d")
        } else if hours > 0 {
            format!("{hours}h")
        } else if minutes > 0 {
            format!("{minutes}m")
        } else {
            format!("{seconds}s")
        }
    };
    output.into()
}

fn teammate_age(now_ms: u64, since_ms: u64) -> Option<Utf16JsonProjection> {
    const MAX_AGE_MS: u64 = 315_360_000_000;
    (since_ms <= now_ms && now_ms - since_ms <= MAX_AGE_MS).then(|| relative_age(now_ms, since_ms))
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Install the Native factory descriptor, retaining its exact enabled fact.
/// Native reserves factory spellings before filtering disabled commands.
/// The provider is mandatory so a missing runtime service cannot become a
/// successful empty listing.
pub fn register(registry: &mut crate::CommandRegistry, provider: Arc<dyn ListAgentsProvider>) {
    registry.register_builtin_handler(Arc::new(ListAgentsHandler::new(provider)));
    registry.set_native_builtin_catalog_order(
        "list-agents",
        super::register::native_builtin_catalog_order("list-agents"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BuiltinCommandHandler;
    use std::sync::Mutex;

    struct FakeProvider {
        availability: ListAgentsCommandAvailability,
        result: Result<LocalAgentRoster, Utf16JsonProjection>,
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl ListAgentsProvider for FakeProvider {
        fn command_availability(&self) -> ListAgentsCommandAvailability {
            self.availability
        }

        async fn snapshot_local_roster(&self) -> Result<LocalAgentRoster, Utf16JsonProjection> {
            *self.calls.lock().unwrap() += 1;
            self.result.clone()
        }
    }

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "list-agents".into(),
            raw_args: String::new().into(),
            positional_args: vec![],
        }
    }

    fn enabled() -> ListAgentsCommandAvailability {
        ListAgentsCommandAvailability {
            harbor_kite_enabled: Some(true),
        }
    }

    #[tokio::test]
    async fn renders_structured_source_roster_and_preserves_utf16_fields() {
        let source_name = Utf16JsonProjection::parse(r#""Active teammate \u202e🚀""#).unwrap();
        let source_type = Utf16JsonProjection::from("general-purpose");
        let started_at_ms = current_time_ms().saturating_sub(120_000);
        let started_ago = relative_age(current_time_ms(), started_at_ms);
        let provider = Arc::new(FakeProvider {
            availability: enabled(),
            result: Ok(LocalAgentRoster {
                subagents: vec![LocalSubagentRow {
                    name: Some(source_name.clone()),
                    agent_type: source_type.clone(),
                    status: "running".into(),
                    started_at_ms,
                }],
                ..LocalAgentRoster::default()
            }),
            calls: Mutex::new(0),
        });
        let handler = ListAgentsHandler::new(provider.clone());

        let CommandResult::Done {
            display: Some(display),
        } = handler.handle(&args()).await
        else {
            panic!("expected the local roster display");
        };
        let expected = source_concat([
            PromptPart::Text("Subagents (1):\n  ["),
            PromptPart::Source(&Utf16JsonProjection::from("running")),
            PromptPart::Text("]  ·  "),
            PromptPart::Source(&source_name),
            PromptPart::Text("  ·  "),
            PromptPart::Source(&source_type),
            PromptPart::Text("  ·  started "),
            PromptPart::Source(&started_ago),
            PromptPart::Text(" ago"),
        ]);
        assert_eq!(display, expected);
        assert_eq!(*provider.calls.lock().unwrap(), 1);
        assert_eq!(handler.native_type(), Some(NativeCommandType::Local));
        assert!(handler.supports_non_interactive());
        assert!(!handler.disable_model_invocation());
        assert_eq!(LIST_AGENTS_ALIAS, "peers");
    }

    #[tokio::test]
    async fn does_not_report_a_failed_task_snapshot_as_an_empty_roster() {
        let failure = Utf16JsonProjection::from("Could not read the current local agent roster.");
        let provider = Arc::new(FakeProvider {
            availability: enabled(),
            result: Err(failure.clone()),
            calls: Mutex::new(0),
        });
        let handler = ListAgentsHandler::new(provider.clone());

        let CommandResult::Done {
            display: Some(display),
        } = handler.handle(&args()).await
        else {
            panic!("expected a visible read failure");
        };
        assert_eq!(display, failure);
        assert_eq!(*provider.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn disabled_gate_does_not_query_the_task_registry() {
        let provider = Arc::new(FakeProvider {
            availability: ListAgentsCommandAvailability {
                harbor_kite_enabled: Some(false),
            },
            result: Ok(LocalAgentRoster::default()),
            calls: Mutex::new(0),
        });
        let handler = ListAgentsHandler::new(provider.clone());

        let CommandResult::Done {
            display: Some(display),
        } = handler.handle(&args()).await
        else {
            panic!("expected the disabled state");
        };
        assert_eq!(
            display.string_at(""),
            Some("Agent listing is unavailable in this session.")
        );
        assert_eq!(*provider.calls.lock().unwrap(), 0);
    }

    #[test]
    fn local_sections_and_warning_use_native_blank_line_separators_without_final_newline() {
        let display = render_local_roster_at(
            &LocalAgentRoster {
                subagents: vec![LocalSubagentRow {
                    name: None,
                    agent_type: "builder".into(),
                    status: "running".into(),
                    started_at_ms: 0,
                }],
                teammates: vec![LocalTeammateRow {
                    name: Some("Alex".into()),
                    agent_type: None,
                    status: None,
                    backend: "pane".into(),
                    since: None,
                    name_shadowing: TeammateNameShadowing::None,
                }],
                local_list_failed: true,
            },
            0,
        );

        assert_eq!(
            display.string_at(""),
            Some(concat!(
                "Subagents (1):\n  [running]  ·  builder  ·  started 0s ago\n\n",
                "Teammates (1):\n  [pane]  ·  Alex\n\n",
                "(the session list on this machine could not be read just now — ",
                "other local sessions may be missing from this listing; ",
                "try /list-agents again shortly)"
            ))
        );
    }

    #[tokio::test]
    async fn local_peer_query_failure_keeps_the_native_warning_instead_of_claiming_empty() {
        let provider = Arc::new(FakeProvider {
            availability: enabled(),
            result: Ok(LocalAgentRoster {
                local_list_failed: true,
                ..LocalAgentRoster::default()
            }),
            calls: Mutex::new(0),
        });
        let handler = ListAgentsHandler::new(provider);

        let CommandResult::Done {
            display: Some(display),
        } = handler.handle(&args()).await
        else {
            panic!("expected the local-list warning");
        };
        assert_eq!(
            display.string_at(""),
            Some("(the session list on this machine could not be read just now — other local sessions may be missing from this listing; try /list-agents again shortly)")
        );
    }

    #[test]
    fn teammate_fields_use_native_source_sanitization_without_losing_utf16() {
        let name = Utf16JsonProjection::root_string(
            "Name\u{202e} 🚀\u{fffd}type\u{fffd}".into(),
            vec![
                b'N' as u16,
                b'a' as u16,
                b'm' as u16,
                b'e' as u16,
                0x202e,
                b' ' as u16,
                0xd83d,
                0xde80,
                0xd800,
                b't' as u16,
                b'y' as u16,
                b'p' as u16,
                b'e' as u16,
                0xdc00,
            ],
        )
        .unwrap();
        let teammate = LocalTeammateRow {
            name: Some(name),
            agent_type: Some("General\u{200b}-purpose".into()),
            status: Some("run\u{202e}ning".into()),
            backend: "pane".into(),
            since: None,
            name_shadowing: TeammateNameShadowing::None,
        };
        let display = render_local_roster_at(
            &LocalAgentRoster {
                teammates: vec![teammate],
                ..LocalAgentRoster::default()
            },
            1,
        );

        assert_eq!(
            display.string_at(""),
            Some("Teammates (1):\n  [running]  ·  Name 🚀type  ·  General-purpose")
        );
        assert_eq!(
            display.string_units("").unwrap(),
            "Teammates (1):\n  [running]  ·  Name 🚀type  ·  General-purpose"
                .encode_utf16()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn registration_adds_native_order_and_alias_only_for_an_enabled_provider() {
        let enabled_provider = Arc::new(FakeProvider {
            availability: enabled(),
            result: Ok(LocalAgentRoster::default()),
            calls: Mutex::new(0),
        });
        let mut registry = crate::CommandRegistry::new();
        register(&mut registry, enabled_provider);

        let canonical = Utf16JsonProjection::from("list-agents");
        let alias = Utf16JsonProjection::from(LIST_AGENTS_ALIAS);
        assert_eq!(
            registry
                .get_handler(&canonical)
                .as_deref()
                .map(|handler| handler.name()),
            Some("list-agents")
        );
        assert_eq!(
            registry
                .get_handler(&alias)
                .as_deref()
                .map(|handler| handler.name()),
            Some("list-agents")
        );
        let descriptor = registry.resolve(&canonical).expect("canonical command");
        assert_eq!(descriptor.aliases, vec![alias.clone()]);
        assert_eq!(
            registry
                .resolve(&alias)
                .and_then(|command| command.name.string_at("")),
            Some("list-agents")
        );
        assert!(registry
            .native_source_ordered_commands()
            .iter()
            .any(|command| command.name.string_at("") == Some("list-agents")));

        let disabled_provider = Arc::new(FakeProvider {
            availability: ListAgentsCommandAvailability {
                harbor_kite_enabled: Some(false),
            },
            result: Ok(LocalAgentRoster::default()),
            calls: Mutex::new(0),
        });
        let mut disabled_registry = crate::CommandRegistry::new();
        register(&mut disabled_registry, disabled_provider);
        assert!(disabled_registry.get_handler(&canonical).is_some());
        let disabled = disabled_registry
            .resolve(&alias)
            .expect("factory is retained");
        assert!(!disabled.is_enabled);
        assert!(disabled.native_catalog_order.is_some());
        assert!(!disabled_registry.headless_command_admitted(disabled));
        assert!(disabled_registry.headless_init_commands().is_empty());
    }

    #[tokio::test]
    async fn teammate_section_uses_native_cap_and_omitted_count() {
        let teammates = (0..101)
            .map(|_| LocalTeammateRow {
                name: Some("worker".into()),
                agent_type: Some("general-purpose".into()),
                status: None,
                backend: "pane".into(),
                since: None,
                name_shadowing: TeammateNameShadowing::None,
            })
            .collect();
        let provider = Arc::new(FakeProvider {
            availability: enabled(),
            result: Ok(LocalAgentRoster {
                teammates,
                ..LocalAgentRoster::default()
            }),
            calls: Mutex::new(0),
        });
        let handler = ListAgentsHandler::new(provider);

        let CommandResult::Done {
            display: Some(display),
        } = handler.handle(&args()).await
        else {
            panic!("expected teammate listing");
        };
        let text = display.string_at("").unwrap();
        assert!(text.starts_with("Teammates (101):\n"));
        assert!(text.ends_with("\n  (… 1 more not shown)"));
        assert!(!text.ends_with('\n'));
        assert_eq!(text.matches("[pane]").count(), 100);
    }

    #[test]
    fn relative_age_matches_native_most_significant_unit_and_rounding() {
        for (delta_ms, expected) in [
            (0, "0s"),
            (999, "0s"),
            (59_999, "59s"),
            (60_000, "1m"),
            (119_500, "2m"),
            (3_599_500, "1h"),
            (3_600_000, "1h"),
            (86_399_500, "1d"),
            (86_400_000, "1d"),
            (315_360_000_000, "3650d"),
        ] {
            assert_eq!(
                relative_age(delta_ms, 0).string_at(""),
                Some(expected),
                "elapsed {delta_ms} ms"
            );
        }
        assert_eq!(relative_age(100, 101).string_at(""), Some("0s"));
    }

    #[test]
    fn teammate_time_uses_the_native_future_and_ten_year_boundaries() {
        const TEN_YEARS_MS: u64 = 315_360_000_000;
        assert!(teammate_age(10, 11).is_none());
        assert_eq!(
            teammate_age(TEN_YEARS_MS, 0)
                .as_ref()
                .and_then(|age| age.string_at("")),
            Some("3650d")
        );
        assert!(teammate_age(TEN_YEARS_MS + 1, 0).is_none());
    }

    #[test]
    fn teammate_fields_use_native_empty_and_address_fallbacks() {
        let empty_name = native_teammate_field(Some(&Utf16JsonProjection::from(""))).unwrap();
        assert_eq!(empty_name.string_at(""), Some("untitled session"));
        assert!(native_teammate_field(Some(&Utf16JsonProjection::from("ｕｄｓ:worker"))).is_none());
        assert!(
            native_teammate_field(Some(&Utf16JsonProjection::from(r"\team\..\??\worker",)))
                .is_none()
        );
        assert!(
            native_teammate_field(Some(&Utf16JsonProjection::from("/tmp/worker.sock.extra",)))
                .is_none()
        );
        assert!(
            native_teammate_field(Some(&Utf16JsonProjection::from("/tmp/ordinary-worker",)))
                .is_some()
        );

        let ipc_teammate = LocalTeammateRow {
            name: Some("uds:/tmp/worker.sock".into()),
            agent_type: Some("did:worker".into()),
            status: Some("bridge:worker".into()),
            backend: "pane".into(),
            since: None,
            name_shadowing: TeammateNameShadowing::None,
        };
        let display = render_local_roster_at(
            &LocalAgentRoster {
                teammates: vec![ipc_teammate],
                ..LocalAgentRoster::default()
            },
            1,
        );
        assert_eq!(
            display.string_at(""),
            Some("Teammates (1):\n  [pane]  ·  (unnamed)")
        );

        let long_label = Utf16JsonProjection::from("x".repeat(201));
        let expected_long_label = "x".repeat(200);
        assert_eq!(
            native_teammate_field(Some(&long_label))
                .and_then(|value| value.string_at("").map(str::to_owned))
                .as_deref(),
            Some(expected_long_label.as_str())
        );
    }
}
