//! The parameterized tool-call header — `Update(src/host.rs)`.
//!
//! This table is NEW code, not a relocation. `tool-api`'s
//! `user_facing_name_for_input` / `get_activity_description` hooks look like
//! they should own this, but only a handful of tools override them and no
//! renderer has ever called them; more importantly the derivation has to run
//! in `client-adapter`, which has no tool registry to ask. See the divergence
//! note on [`tool_header`].

use serde_json::Value;

use crate::classify::classify;

/// A stable, non-localized identifier for a header verb.
///
/// Clients that ship localized UI look the verb up by this key; the terminal
/// and the Electron desktop (neither of which localizes) use
/// [`ToolVerb::english`]. Shipping only rendered English would be a hard
/// localization regression on mobile, which ships five languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolVerb {
    /// An existing file is being modified.
    Update,
    /// A new file is being written.
    Create,
    /// A file is being read.
    Read,
    /// A search over files or content.
    Search,
    /// A shell command.
    Shell,
    /// Reading the output of a running shell task.
    Output,
    /// Stopping a running shell task.
    Kill,
    /// Fetching or searching the web.
    Fetch,
    /// Delegating to a subagent.
    Task,
    /// Rewriting the todo checklist.
    Todo,
    /// Invoking a skill.
    Skill,
    /// Anything without a table entry — the raw tool name is the label.
    Generic,
}

/// A stable semantic icon identity for a tool-call header. Clients map these
/// keys to their platform icon set; Rust derives the meaning once so Bash and
/// MCP calls cannot drift between surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolIcon {
    /// Read a file.
    Read,
    /// Search content.
    Search,
    /// List files or entries.
    List,
    /// Edit content.
    Edit,
    /// Execute a command.
    Terminal,
    /// Access the web.
    Globe,
    /// Run a workflow.
    Workflow,
    /// Update a checklist.
    ListChecks,
    /// Invoke a skill.
    Sparkles,
    /// Call an external integration.
    Plug,
    /// Read task output.
    Output,
    /// Stop an operation.
    Stop,
    /// Generic tool.
    Wrench,
}

impl ToolVerb {
    /// The stable lookup key (`"update"`, `"shell"`, …).
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Create => "create",
            Self::Read => "read",
            Self::Search => "search",
            Self::Shell => "shell",
            Self::Output => "output",
            Self::Kill => "kill",
            Self::Fetch => "fetch",
            Self::Task => "task",
            Self::Todo => "todo",
            Self::Skill => "skill",
            Self::Generic => "generic",
        }
    }

    /// The English label. `None` for [`Generic`], whose label is the tool name.
    ///
    /// [`Generic`]: ToolVerb::Generic
    #[must_use]
    pub const fn english(self) -> Option<&'static str> {
        match self {
            Self::Update => Some("Update"),
            Self::Create => Some("Write"),
            Self::Read => Some("Read"),
            Self::Search => Some("Search"),
            Self::Shell => Some("Running shell command"),
            Self::Output => Some("Output"),
            Self::Kill => Some("Kill"),
            Self::Fetch => Some("Fetch"),
            Self::Task => Some("Task"),
            Self::Todo => Some("Update Todos"),
            Self::Skill => Some("Skill"),
            Self::Generic => None,
        }
    }
}

/// A header sub-line rendered under the title with its own glyph, e.g.
/// `("$", "cargo test --all")`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSubLine {
    /// Leading glyph (`"$"` for a shell command).
    pub prefix: String,
    /// Single-line body. Embedded newlines are collapsed to spaces here, so
    /// no renderer has to.
    pub text: String,
}

/// The derived header for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolHeader {
    /// Stable verb identity.
    pub verb: ToolVerb,
    /// Stable semantic icon identity.
    pub icon: ToolIcon,
    /// English label — [`ToolVerb::english`], or the raw tool name for
    /// [`ToolVerb::Generic`]. Never empty.
    pub label: String,
    /// The parenthesized primary argument, already shortened of nothing —
    /// clients elide it to taste. `None` when the tool has no single argument.
    pub primary: Option<String>,
    /// Suffix rendered after the parentheses: `" (3 edits)"`, `" (github MCP)"`.
    pub qualifier: Option<String>,
    /// Plural slot for verbs that count (shell commands). `None` when the verb
    /// does not count.
    pub count: Option<u32>,
    /// Optional second line.
    pub sub_line: Option<ToolSubLine>,
}

impl ToolHeader {
    /// `"{label}({primary}){qualifier}"` — the one-line English title.
    #[must_use]
    pub fn title(&self) -> String {
        let mut out = self.label.clone();
        if let Some(primary) = &self.primary {
            out.push('(');
            out.push_str(primary);
            out.push(')');
        }
        if let Some(qualifier) = &self.qualifier {
            out.push_str(qualifier);
        }
        out
    }
}

/// Collapse whitespace runs (including newlines) into single spaces and trim.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A non-empty string field of `input`.
fn str_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The first non-empty string among `keys`.
fn first_str<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| str_field(input, key))
}

/// Probe order for a tool with no table entry. Chosen so a third-party or MCP
/// tool still gets a meaningful header.
const GENERIC_PRIMARY_KEYS: &[&str] = &[
    "file_path",
    "path",
    "notebook_path",
    "pattern",
    "query",
    "url",
    "command",
    "name",
    "description",
];

/// Resolve the `mcp__{server}__{tool}` pair for a namespaced MCP call.
///
/// The generic `MCP` dispatcher carries the namespaced name in
/// `input.full_name` instead of the tool field. Shared with
/// `crate::classify` so there is one parser.
#[must_use]
pub fn mcp_parts<'a>(tool: &'a str, input: &'a Value) -> Option<(&'a str, &'a str)> {
    let full_name = if tool == "MCP" {
        input.get("full_name").and_then(Value::as_str)?
    } else {
        tool
    };
    full_name.strip_prefix("mcp__")?.split_once("__")
}

/// Derive a semantic icon, reusing the canonical collapse classifier for Bash
/// and namespaced MCP calls. The classifier already distinguishes `cat`/`rg`/
/// `ls`; this keeps the header icon and collapsed transcript behavior aligned.
#[must_use]
pub fn tool_icon(tool: &str, input: &Value) -> ToolIcon {
    if tool == "MCP" || tool.starts_with("mcp__") {
        // MCP identity is more useful than guessing a server-specific verb;
        // mutating and read-only tools alike are surfaced through the plug.
        ToolIcon::Plug
    } else if tool == "Bash" {
        let classification = classify(tool, input, false);
        if classification.is_search {
            ToolIcon::Search
        } else if classification.is_read {
            ToolIcon::Read
        } else if classification.is_list {
            ToolIcon::List
        } else {
            ToolIcon::Terminal
        }
    } else {
        match tool {
            "Read" | "NotebookRead" => ToolIcon::Read,
            "Grep" | "Glob" | "Search" => ToolIcon::Search,
            "List" | "ListFiles" | "ListDir" | "LS" => ToolIcon::List,
            "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => ToolIcon::Edit,
            "Shell" | "PowerShell" | "REPL" => ToolIcon::Terminal,
            "WebFetch" | "WebSearch" | "Fetch" => ToolIcon::Globe,
            "Task" | "Agent" | "Workflow" => ToolIcon::Workflow,
            "TodoWrite" | "ListChecks" => ToolIcon::ListChecks,
            "Skill" | "Sparkles" => ToolIcon::Sparkles,
            "TaskOutput" | "AgentOutput" | "AgentOutputTool" | "BashOutput" | "BashOutputTool"
            | "Output" => ToolIcon::Output,
            "TaskStop" | "KillShell" | "KillBash" | "Stop" => ToolIcon::Stop,
            "Plug" => ToolIcon::Plug,
            // First-party local-app host operations. They used to be spelled
            // `mcp__local_apps__*` and therefore took the `Plug` branch above;
            // as builtins they would otherwise all fall to `Wrench`. Map the
            // families whose verb is unambiguous.
            "LocalAppList" | "LocalAppCheckpointList" | "LocalAppBackgroundList" => ToolIcon::List,
            "LocalAppGet"
            | "LocalAppLogs"
            | "LocalAppQueryData"
            | "LocalAppEvents"
            | "LocalAppBackgroundStatus" => ToolIcon::Read,
            "LocalAppCreate"
            | "LocalAppManifest"
            | "LocalAppMutateData"
            | "LocalAppCheckpointCreate" => ToolIcon::Edit,
            "LocalAppRuntime" | "LocalAppBackgroundSchedule" => ToolIcon::Workflow,
            _ => ToolIcon::Wrench,
        }
    }
}

/// Derive the header for one tool call. PURE — the ONE table.
///
/// KNOWN DIVERGENCE: `tools/agent`'s `user_facing_name_for_input`
/// (`agent.rs:1312`) and `tools/worktree`'s two overrides express the same
/// idea in the tool crates. They cannot be called from here — `tui-core` must
/// not depend on tool implementations, and the derivation runs where no tool
/// registry exists — so the `Task`/`Agent` row below reimplements
/// `agent.rs`'s `subagent_type` read. If either side changes, change both.
#[must_use]
pub fn tool_header(tool: &str, input: &Value) -> ToolHeader {
    // MCP calls are namespaced, never table entries.
    if let Some((server, mcp_tool)) = mcp_parts(tool, input) {
        return ToolHeader {
            verb: ToolVerb::Generic,
            icon: tool_icon(tool, input),
            label: mcp_tool.to_string(),
            primary: first_str(input, GENERIC_PRIMARY_KEYS).map(one_line),
            qualifier: Some(format!(" ({server} MCP)")),
            count: None,
            sub_line: None,
        };
    }

    let mut header = ToolHeader {
        verb: ToolVerb::Generic,
        icon: tool_icon(tool, input),
        // Resolved below: an arm may set `label_override`, otherwise the verb's
        // English label wins, otherwise the raw tool name.
        label: String::new(),
        primary: None,
        qualifier: None,
        count: None,
        sub_line: None,
    };
    let mut label_override: Option<String> = None;

    match tool {
        "Edit" => {
            header.verb = ToolVerb::Update;
            header.primary = str_field(input, "file_path").map(str::to_string);
        }
        "MultiEdit" => {
            header.verb = ToolVerb::Update;
            header.primary = str_field(input, "file_path").map(str::to_string);
            let edits = input
                .get("edits")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if edits > 1 {
                header.qualifier = Some(format!(" ({edits} edits)"));
            }
        }
        "Write" => {
            header.verb = ToolVerb::Create;
            header.primary = str_field(input, "file_path").map(str::to_string);
        }
        "NotebookEdit" => {
            header.verb = ToolVerb::Update;
            header.primary = first_str(input, &["notebook_path", "file_path"]).map(str::to_string);
            if let Some(cell) = str_field(input, "cell_id") {
                header.qualifier = Some(format!(" (cell {cell})"));
            }
        }
        "Read" => {
            header.verb = ToolVerb::Read;
            header.primary = str_field(input, "file_path").map(str::to_string);
            let offset = input.get("offset").and_then(Value::as_u64);
            let limit = input.get("limit").and_then(Value::as_u64);
            if offset.is_some() || limit.is_some() {
                let start = offset.unwrap_or(1).max(1);
                header.qualifier = Some(match limit {
                    // Saturating: `offset`/`limit` come straight off the
                    // MODEL's tool input, and the header is derived BEFORE the
                    // tool validates its schema — `offset: u64::MAX` used to
                    // panic here with "attempt to add with overflow".
                    Some(limit) if limit > 0 => {
                        let end = start.saturating_add(limit).saturating_sub(1);
                        format!(" (lines {start}-{end})")
                    }
                    _ => format!(" (from line {start})"),
                });
            }
        }
        "Bash" | "Shell" | "PowerShell" => {
            header.verb = ToolVerb::Shell;
            label_override = Some("Running 1 shell command…".to_string());
            header.count = Some(1);
            if let Some(command) = str_field(input, "command") {
                header.sub_line = Some(ToolSubLine {
                    prefix: "$".to_string(),
                    text: one_line(command),
                });
            }
        }
        "REPL" => {
            header.verb = ToolVerb::Shell;
            label_override = Some("REPL".to_string());
            if let Some(code) = str_field(input, "code") {
                header.sub_line = Some(ToolSubLine {
                    prefix: "›".to_string(),
                    text: one_line(code),
                });
            }
        }
        // `TaskOutput` and `TaskStop` are registered under claude-code's
        // legacy names too (`tools/task/src/task.rs` `aliases()`), and the
        // wire carries whichever name the model used.
        "TaskOutput" | "AgentOutput" | "AgentOutputTool" | "BashOutput" | "BashOutputTool" => {
            header.verb = ToolVerb::Output;
            header.primary =
                first_str(input, &["bash_id", "shell_id", "task_id"]).map(str::to_string);
            if input.get("block").is_some_and(|value| {
                value == &Value::Bool(false) || value.as_str() == Some("false")
            }) {
                header.qualifier = Some(" (non-blocking)".into());
            }
        }
        "TaskStop" | "KillShell" | "KillBash" => {
            header.verb = ToolVerb::Kill;
            header.primary = first_str(input, &["shell_id", "task_id"]).map(str::to_string);
        }
        "Grep" => {
            header.verb = ToolVerb::Search;
            header.primary = str_field(input, "pattern").map(one_line);
            header.qualifier = str_field(input, "path")
                .map(|path| format!(" in {path}"))
                .or_else(|| str_field(input, "glob").map(|glob| format!(" ({glob})")));
        }
        "Glob" => {
            header.verb = ToolVerb::Search;
            header.primary = str_field(input, "pattern").map(one_line);
            header.qualifier = str_field(input, "path").map(|path| format!(" in {path}"));
        }
        "TodoWrite" => header.verb = ToolVerb::Todo,
        "Task" | "Agent" => {
            header.verb = ToolVerb::Task;
            // Mirrors `tools/agent/src/agent.rs:1312`: the subagent type is the
            // label, except the two generic types which read as plain "Task".
            label_override = str_field(input, "subagent_type")
                .filter(|kind| !matches!(*kind, "general-purpose" | "worker"))
                .map(str::to_string);
            header.primary = str_field(input, "description").map(one_line);
        }
        "WebFetch" => {
            header.verb = ToolVerb::Fetch;
            header.primary = str_field(input, "url").map(str::to_string);
        }
        "WebSearch" => {
            header.verb = ToolVerb::Fetch;
            label_override = Some("Web Search".to_string());
            header.primary = str_field(input, "query").map(one_line);
        }
        "Skill" => {
            header.verb = ToolVerb::Skill;
            header.primary = first_str(input, &["command", "name", "skill"]).map(one_line);
        }
        _ => header.primary = first_str(input, GENERIC_PRIMARY_KEYS).map(one_line),
    }

    // Precedence, highest first: an arm's explicit override, the verb's
    // English label, the raw tool name.
    header.label = label_override
        .or_else(|| header.verb.english().map(str::to_string))
        .unwrap_or_else(|| tool.to_string());
    header
}

/// Derive a resolved call's header, preserving ordinary named subagents.
/// Claude Code 2.1.263 A$n uses the input name as an @-label only when the
/// corresponding result confirms a persistent teammate was spawned.
#[must_use]
pub fn tool_header_with_result(tool: &str, input: &Value, result: &Value) -> ToolHeader {
    let mut header = tool_header(tool, input);
    if tool == "Read" {
        if let Some(id) = result
            .get("file")
            .and_then(|f| f.get("taskId"))
            .and_then(Value::as_str)
        {
            header.label = "Read agent output".into();
            header.primary = Some(id.into());
        }
    }
    if tool == "Agent" && result.get("status").and_then(Value::as_str) == Some("teammate_spawned") {
        if let Some(name) = str_field(input, "name") {
            header.label = format!("@{name}");
            header.primary = str_field(input, "subagent_type")
                .filter(|kind| !matches!(*kind, "general-purpose" | "worker"))
                .map(str::to_owned);
        }
    }
    header
}

/// Human label shown in the spinner for an in-flight tool call, mapping the
/// tool name to a claude-code-style gerund (`Bash` → `Running Bash`).
#[must_use]
pub fn activity_label(tool: &str) -> String {
    match tool {
        "Bash" | "BashOutput" => "Running Bash".to_string(),
        "Read" => "Reading".to_string(),
        "Write" => "Writing".to_string(),
        "Edit" | "MultiEdit" => "Editing".to_string(),
        "Grep" | "Glob" => "Searching".to_string(),
        "WebFetch" | "WebSearch" => "Browsing".to_string(),
        "Task" => "Delegating".to_string(),
        other => format!("Running {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn title(tool: &str, input: &Value) -> String {
        tool_header(tool, input).title()
    }

    #[test]
    fn registered_output_read_header_uses_task_id_from_result() {
        let input = serde_json::json!({"file_path":"/tmp/session/tasks/b12345678.output"});
        let plain = tool_header_with_result("Read", &input, &serde_json::json!({"file":{}}));
        assert_eq!(plain.label, "Read");
        let task = tool_header_with_result(
            "Read",
            &input,
            &serde_json::json!({"file":{"taskId":"b12345678"}}),
        );
        assert_eq!(task.label, "Read agent output");
        assert_eq!(task.primary.as_deref(), Some("b12345678"));
    }

    #[test]
    fn file_tools_render_verb_and_path() {
        assert_eq!(
            title("Edit", &json!({"file_path": "src/host.rs"})),
            "Update(src/host.rs)"
        );
        assert_eq!(
            title("Write", &json!({"file_path": "src/new.rs"})),
            "Write(src/new.rs)"
        );
        assert_eq!(
            title("Read", &json!({"file_path": "src/host.rs"})),
            "Read(src/host.rs)"
        );
    }

    #[test]
    fn read_qualifies_a_partial_range() {
        assert_eq!(
            title(
                "Read",
                &json!({"file_path": "a.rs", "offset": 40, "limit": 41})
            ),
            "Read(a.rs) (lines 40-80)"
        );
        assert_eq!(
            title("Read", &json!({"file_path": "a.rs", "offset": 40})),
            "Read(a.rs) (from line 40)"
        );
    }

    #[test]
    fn read_range_does_not_overflow_on_unvalidated_model_input() {
        // The header is derived from the MODEL's raw tool input, BEFORE the
        // tool validates it against its schema, so any `u64` can land here.
        // `start + limit - 1` used to panic with "attempt to add with
        // overflow" on a debug build — a model typo crashed the process.
        let saturated = u64::MAX - 1;
        assert_eq!(
            title(
                "Read",
                &json!({"file_path": "a.rs", "offset": u64::MAX, "limit": u64::MAX})
            ),
            format!("Read(a.rs) (lines {}-{saturated})", u64::MAX)
        );
        assert_eq!(
            title(
                "Read",
                &json!({"file_path": "a.rs", "offset": 0, "limit": u64::MAX})
            ),
            format!("Read(a.rs) (lines 1-{saturated})")
        );
        assert_eq!(
            title("Read", &json!({"file_path": "a.rs", "limit": u64::MAX})),
            format!("Read(a.rs) (lines 1-{saturated})")
        );
    }

    #[test]
    fn multi_edit_counts_only_when_plural() {
        assert_eq!(
            title(
                "MultiEdit",
                &json!({"file_path": "a.rs", "edits": [1, 2, 3]})
            ),
            "Update(a.rs) (3 edits)"
        );
        assert_eq!(
            title("MultiEdit", &json!({"file_path": "a.rs", "edits": [1]})),
            "Update(a.rs)"
        );
    }

    #[test]
    fn bash_uses_the_counted_verb_and_a_dollar_sub_line() {
        let header = tool_header("Bash", &json!({"command": "cargo test\n  --all"}));
        assert_eq!(header.verb, ToolVerb::Shell);
        assert_eq!(header.label, "Running 1 shell command…");
        assert_eq!(header.count, Some(1));
        assert!(header.primary.is_none());
        let sub = header.sub_line.expect("a $ sub-line");
        assert_eq!(sub.prefix, "$");
        // Newlines are collapsed so no renderer has to.
        assert_eq!(sub.text, "cargo test --all");
    }

    #[test]
    fn search_tools_qualify_by_path_then_glob() {
        assert_eq!(
            title("Grep", &json!({"pattern": "TODO", "path": "src"})),
            "Search(TODO) in src"
        );
        assert_eq!(
            title("Grep", &json!({"pattern": "TODO", "glob": "*.rs"})),
            "Search(TODO) (*.rs)"
        );
        assert_eq!(
            title("Glob", &json!({"pattern": "**/*.rs"})),
            "Search(**/*.rs)"
        );
    }

    #[test]
    fn shell_task_aliases_resolve_to_the_same_verbs() {
        // `tools/task/src/task.rs` registers the claude-code legacy names as
        // aliases, and the wire carries whichever the model used.
        for tool in ["TaskOutput", "BashOutput", "BashOutputTool"] {
            let header = tool_header(tool, &json!({"bash_id": "sh_1"}));
            assert_eq!(header.verb, ToolVerb::Output, "{tool}");
            assert_eq!(header.title(), "Output(sh_1)", "{tool}");
        }
        for tool in ["TaskStop", "KillShell", "KillBash"] {
            let header = tool_header(tool, &json!({"shell_id": "sh_1"}));
            assert_eq!(header.verb, ToolVerb::Kill, "{tool}");
            assert_eq!(header.title(), "Kill(sh_1)", "{tool}");
        }
    }

    #[test]
    fn task_output_nonblocking_annotation_honors_coerced_input() {
        for tool in [
            "TaskOutput",
            "AgentOutput",
            "AgentOutputTool",
            "BashOutput",
            "BashOutputTool",
        ] {
            for block in [json!(false), json!("false")] {
                assert_eq!(
                    tool_header(tool, &json!({"task_id": "b12345678", "block": block})).title(),
                    "Output(b12345678) (non-blocking)"
                );
            }
            assert_eq!(
                tool_header(tool, &json!({"task_id": "b12345678", "block": true})).title(),
                "Output(b12345678)"
            );
        }
    }

    #[test]
    fn todo_write_has_a_verb_but_no_argument() {
        let header = tool_header("TodoWrite", &json!({"todos": []}));
        assert_eq!(header.verb, ToolVerb::Todo);
        assert_eq!(header.title(), "Update Todos");
        assert!(header.primary.is_none());
    }

    #[test]
    fn agent_labels_by_subagent_type_except_the_generic_ones() {
        assert_eq!(
            title(
                "Task",
                &json!({"subagent_type": "code-reviewer", "description": "review the diff"})
            ),
            "code-reviewer(review the diff)"
        );
        // `general-purpose` / `worker` read as plain "Task" (agent.rs:1312).
        assert_eq!(
            title(
                "Task",
                &json!({"subagent_type": "general-purpose", "description": "look around"})
            ),
            "Task(look around)"
        );
    }

    #[test]
    fn mcp_tools_are_namespaced_and_qualified_by_server() {
        assert_eq!(
            title("mcp__github__search_code", &json!({"query": "fn main"})),
            "search_code(fn main) (github MCP)"
        );
        // The generic dispatcher carries the name in `full_name` instead.
        assert_eq!(
            title(
                "MCP",
                &json!({"full_name": "mcp__linear__create_issue", "name": "Bug"})
            ),
            "create_issue(Bug) (linear MCP)"
        );
    }

    #[test]
    fn unknown_tools_fall_back_to_the_name_and_a_probed_argument() {
        let header = tool_header("SomeFutureTool", &json!({"query": "hello"}));
        assert_eq!(header.verb, ToolVerb::Generic);
        assert_eq!(header.title(), "SomeFutureTool(hello)");
        // Nothing probeable -> the bare tool name.
        assert_eq!(
            tool_header("SomeFutureTool", &json!({"weird": 1})).title(),
            "SomeFutureTool"
        );
    }

    #[test]
    fn missing_arguments_degrade_to_the_verb_alone() {
        assert_eq!(title("Edit", &json!({})), "Update");
        assert_eq!(title("Bash", &json!({})), "Running 1 shell command…");
        // An empty string is treated as absent, not rendered as `Update()`.
        assert_eq!(title("Edit", &json!({"file_path": ""})), "Update");
    }

    #[test]
    fn verb_keys_are_stable_and_distinct() {
        // Clients key localized copy off these; a collision would silently
        // merge two verbs into one translation.
        let verbs = [
            ToolVerb::Update,
            ToolVerb::Create,
            ToolVerb::Read,
            ToolVerb::Search,
            ToolVerb::Shell,
            ToolVerb::Output,
            ToolVerb::Kill,
            ToolVerb::Fetch,
            ToolVerb::Task,
            ToolVerb::Todo,
            ToolVerb::Skill,
            ToolVerb::Generic,
        ];
        let mut keys: Vec<&str> = verbs.iter().map(|v| v.key()).collect();
        keys.sort_unstable();
        let count = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), count, "verb keys must be distinct");
    }

    #[test]
    fn activity_label_maps_known_tools_to_gerunds() {
        assert_eq!(activity_label("Bash"), "Running Bash");
        assert_eq!(activity_label("Read"), "Reading");
        assert_eq!(activity_label("Edit"), "Editing");
        assert_eq!(activity_label("Task"), "Delegating");
        assert_eq!(activity_label("Whatever"), "Running Whatever");
    }

    #[test]
    fn icons_follow_bash_classifier_and_mcp_identity() {
        assert_eq!(
            tool_icon("Bash", &json!({"command": "cat file"})),
            ToolIcon::Read
        );
        assert_eq!(
            tool_icon("Bash", &json!({"command": "rg needle src"})),
            ToolIcon::Search
        );
        assert_eq!(
            tool_icon("Bash", &json!({"command": "ls -la"})),
            ToolIcon::List
        );
        assert_eq!(
            tool_icon("Bash", &json!({"command": "cargo test"})),
            ToolIcon::Terminal
        );
        assert_eq!(
            tool_icon("mcp__github__search_code", &json!({})),
            ToolIcon::Plug
        );
        assert_eq!(
            tool_icon("MCP", &json!({"full_name": "mcp__fs__read_file"})),
            ToolIcon::Plug
        );
    }

    /// The local-app host operations moved from `mcp__local_apps__*` to
    /// builtin `LocalApp*` names. That silently changed their icon: the
    /// `mcp__` prefix took the `Plug` branch, while an unmapped builtin falls
    /// to the generic `Wrench`.
    #[test]
    fn local_app_tools_keep_a_meaningful_icon_after_the_rename() {
        use serde_json::json;
        assert_eq!(tool_icon("LocalAppList", &json!({})), ToolIcon::List);
        assert_eq!(tool_icon("LocalAppLogs", &json!({})), ToolIcon::Read);
        assert_eq!(tool_icon("LocalAppQueryData", &json!({})), ToolIcon::Read);
        assert_eq!(tool_icon("LocalAppMutateData", &json!({})), ToolIcon::Edit);
        assert_eq!(tool_icon("LocalAppRuntime", &json!({})), ToolIcon::Workflow);
        // Deliberately generic: a build is a tool action with no better icon.
        assert_eq!(tool_icon("LocalAppBuild", &json!({})), ToolIcon::Wrench);
        // A real MCP server still plugs.
        assert_eq!(
            tool_icon("mcp__github__search_code", &json!({})),
            ToolIcon::Plug
        );
    }

    #[test]
    fn icons_cover_non_bash_tool_families() {
        let cases = [
            ("Read", ToolIcon::Read),
            ("Grep", ToolIcon::Search),
            ("List", ToolIcon::List),
            ("Edit", ToolIcon::Edit),
            ("Shell", ToolIcon::Terminal),
            ("WebFetch", ToolIcon::Globe),
            ("Task", ToolIcon::Workflow),
            ("TodoWrite", ToolIcon::ListChecks),
            ("Skill", ToolIcon::Sparkles),
            ("TaskOutput", ToolIcon::Output),
            ("TaskStop", ToolIcon::Stop),
            ("Other", ToolIcon::Wrench),
        ];
        for (tool, expected) in cases {
            assert_eq!(tool_icon(tool, &json!({})), expected, "{tool}");
        }
    }
}

#[cfg(test)]
mod teammate_header_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_confirmed_teammates_use_the_input_name_as_an_at_label() {
        let input =
            json!({"name":"reviewer","subagent_type":"Explore","description":"Review the module"});
        let spawned = tool_header_with_result(
            "Agent",
            &input,
            &json!({"status":"teammate_spawned","name":"reviewer-2"}),
        );
        assert_eq!(spawned.label, "@reviewer");
        assert_eq!(spawned.primary.as_deref(), Some("Explore"));
        let ordinary = tool_header_with_result("Agent", &input, &json!({"status":"completed"}));
        assert_eq!(ordinary, tool_header("Agent", &input));
        let generic = tool_header_with_result(
            "Agent",
            &json!({"name":"reviewer","subagent_type":"general-purpose"}),
            &json!({"status":"teammate_spawned"}),
        );
        assert_eq!(generic.label, "@reviewer");
        assert_eq!(generic.primary, None);
    }
}
