//! OTEL metric/signal name schema — the `claude_code.*` instrument names CC
//! 2.1.207 emits, plus the meter identity.
//!
//! These names are the enterprise **Monitoring schema**: external OTLP
//! collectors and dashboards key on them exactly (the same reason `tengu_*`
//! Statsig names are kept verbatim). They are therefore NOT rebranded. The
//! constants pin the wire names; the actual instrument creation + recording
//! against a real meter is the documented egress remainder.

/// Meter / instrumentation-scope name (binary
/// `getMeter("com.anthropic.claude_code")`). Kept verbatim — collectors filter
/// on it.
pub const METER_NAME: &str = "com.anthropic.claude_code";

/// Counter: cumulative model token usage (`claude_code.token.usage`).
pub const TOKEN_USAGE: &str = "claude_code.token.usage";
/// Counter: cumulative model cost in USD (`claude_code.cost.usage`).
pub const COST_USAGE: &str = "claude_code.cost.usage";
/// Counter: CLI session starts (`claude_code.session.count`).
pub const SESSION_COUNT: &str = "claude_code.session.count";
/// Counter: lines of code added/removed (`claude_code.lines_of_code.count`).
pub const LINES_OF_CODE_COUNT: &str = "claude_code.lines_of_code.count";
/// Counter: pull requests created (`claude_code.pull_request.count`).
pub const PULL_REQUEST_COUNT: &str = "claude_code.pull_request.count";
/// Counter: git commits created (`claude_code.commit.count`).
pub const COMMIT_COUNT: &str = "claude_code.commit.count";
/// Counter: tool executions (`claude_code.tool.execution`).
pub const TOOL_EXECUTION: &str = "claude_code.tool.execution";
/// Counter: tool calls blocked awaiting user (`claude_code.tool.blocked_on_user`).
pub const TOOL_BLOCKED_ON_USER: &str = "claude_code.tool.blocked_on_user";
/// Counter: total active time in seconds (`claude_code.active_time.total`).
/// NOTE: the 2.1.220 binary REGISTERS this instrument (`Mt.activeTimeCounter`,
/// unit `s`) but never calls `add()` on it — its getter (`GSi`) has zero call
/// sites — so the port likewise defines the name without a record site.
pub const ACTIVE_TIME_TOTAL: &str = "claude_code.active_time.total";
/// Counter: accept/reject decisions on edit tools (`claude_code.code_edit_tool.decision`).
pub const CODE_EDIT_TOOL_DECISION: &str = "claude_code.code_edit_tool.decision";
/// Counter: subagent spawns (`claude_code.subagent.spawn`).
pub const SUBAGENT_SPAWN: &str = "claude_code.subagent.spawn";
/// Counter/histogram: MCP RPC calls (`claude_code.mcp.rpc`).
pub const MCP_RPC: &str = "claude_code.mcp.rpc";
/// Counter/histogram: LLM API requests (`claude_code.llm_request`).
pub const LLM_REQUEST: &str = "claude_code.llm_request";
/// Counter/histogram: hook invocations (`claude_code.hook`).
pub const HOOK: &str = "claude_code.hook";
/// Counter/histogram: context compactions (`claude_code.compaction`).
pub const COMPACTION: &str = "claude_code.compaction";
/// Counter/histogram: bash subprocesses (`claude_code.bash.subprocess`).
pub const BASH_SUBPROCESS: &str = "claude_code.bash.subprocess";

// -- Additional schema names present in the binary (signal type kept general to
//    avoid mis-categorising — some are metric prefixes, some log/trace roots). --

/// Generic tool metric root (`claude_code.tool`).
pub const TOOL: &str = "claude_code.tool";
/// Interaction metric (`claude_code.interaction`).
pub const INTERACTION: &str = "claude_code.interaction";

/// The complete `claude_code.*` schema-name set emitted by the 2.1.207 binary
/// (metrics + the log/trace signal roots), sorted. Pinned by the snapshot test
/// against the `strings`-derived list so a drift is caught at build time.
pub const ALL_SCHEMA_NAMES: &[&str] = &[
    ACTIVE_TIME_TOTAL,
    BASH_SUBPROCESS,
    CODE_EDIT_TOOL_DECISION,
    COMMIT_COUNT,
    COMPACTION,
    COST_USAGE,
    super::logs::EVENTS_SIGNAL, // claude_code.events
    HOOK,
    INTERACTION,
    LINES_OF_CODE_COUNT,
    LLM_REQUEST,
    MCP_RPC,
    PULL_REQUEST_COUNT,
    SESSION_COUNT,
    SUBAGENT_SPAWN,
    TOKEN_USAGE,
    TOOL,
    TOOL_BLOCKED_ON_USER,
    TOOL_EXECUTION,
    super::logs::TRACING_SIGNAL, // claude_code.tracing
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The binary-derived list (`strings -n 8 <2.1.207> | grep -oE
    /// 'claude_code\.[a-z_.]+' | sort -u`). Any drift between this and
    /// [`ALL_SCHEMA_NAMES`] fails the build.
    const BINARY_DERIVED: &[&str] = &[
        "claude_code.active_time.total",
        "claude_code.bash.subprocess",
        "claude_code.code_edit_tool.decision",
        "claude_code.commit.count",
        "claude_code.compaction",
        "claude_code.cost.usage",
        "claude_code.events",
        "claude_code.hook",
        "claude_code.interaction",
        "claude_code.lines_of_code.count",
        "claude_code.llm_request",
        "claude_code.mcp.rpc",
        "claude_code.pull_request.count",
        "claude_code.session.count",
        "claude_code.subagent.spawn",
        "claude_code.token.usage",
        "claude_code.tool",
        "claude_code.tool.blocked_on_user",
        "claude_code.tool.execution",
        "claude_code.tracing",
    ];

    #[test]
    fn schema_names_match_binary_exactly() {
        let mut got: Vec<&str> = ALL_SCHEMA_NAMES.to_vec();
        got.sort_unstable();
        assert_eq!(
            got, BINARY_DERIVED,
            "ALL_SCHEMA_NAMES drifted from the 2.1.207 binary-derived instrument list"
        );
    }

    #[test]
    fn meter_name_is_verbatim() {
        assert_eq!(METER_NAME, "com.anthropic.claude_code");
    }
}
