//! Trailing tests extracted from hook_payload.rs.

use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pre_payload_serializes_byte_lock() {
        let p = PreToolUsePayload {
            hook_event_name: HookEventNamePre,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            tool_name: "Bash".into(),
            tool_input: json!({"command": "ls"}),
            tool_use_id: "tu-1".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.starts_with(r#"{"hook_event_name":"PreToolUse","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","tool_name":"Bash","tool_input":{"command":"ls"},"tool_use_id":"tu-1"}"#));
    }

    /// `prompt_id` sits on the SHARED hook-input base, between `cwd` and
    /// `permission_mode` — oracle `createBaseHookInput` (2.1.238 minified `c_`,
    /// BIN off 296935693):
    /// `{session_id, transcript_path, cwd, prompt_id:Vut()??void 0,
    ///   permission_mode, agent_id, agent_type, effort}`.
    /// `Vut()` is `undefined` until the first user input of the process
    /// lifetime, so `None` must OMIT the key entirely (never emit `null`).
    #[test]
    fn prompt_id_rides_the_shared_base_between_cwd_and_permission_mode() {
        let p = PreToolUsePayload {
            hook_event_name: HookEventNamePre,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: Some("7f1f0e2a-0000-4000-8000-000000000001".into()),
            permission_mode: Some("default".into()),
            agent_id: None,
            agent_type: None,
            effort: None,
            tool_name: "Bash".into(),
            tool_input: json!({"command": "ls"}),
            tool_use_id: "tu-1".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(
            s.contains(
                r#""cwd":"/work","prompt_id":"7f1f0e2a-0000-4000-8000-000000000001","permission_mode":"default""#
            ),
            "prompt_id must serialize between cwd and permission_mode: {s}"
        );
        // Absent until the first user input — the key is omitted, not null.
        let absent = PreToolUsePayload {
            prompt_id: None,
            ..p
        };
        let s2 = serde_json::to_string(&absent).unwrap();
        assert!(!s2.contains("prompt_id"), "{s2}");
    }

    /// The same base field must ride EVERY event, not just the tool events —
    /// the oracle shares one `XO` base across all 31 hook inputs.
    #[test]
    fn prompt_id_rides_the_lifecycle_events_too() {
        let p = SessionStartPayload {
            hook_event_name: HookEventNameSessionStart,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: Some("pid-1".into()),
            permission_mode: None,
            agent_id: None,
            source: "startup".into(),
            agent_type: None,
            effort: None,
            model: None,
            session_title: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""cwd":"/w","prompt_id":"pid-1""#), "{s}");
    }

    #[test]
    fn post_payload_includes_tool_response() {
        let p = PostToolUsePayload {
            hook_event_name: HookEventNamePost,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            tool_name: "Read".into(),
            tool_input: json!({"path": "/x"}),
            tool_response: json!({"content": "data"}),
            tool_use_id: "tu-2".into(),
            duration_ms: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""hook_event_name":"PostToolUse""#));
        assert!(s.contains(r#""tool_response":{"content":"data"}"#));
    }

    #[test]
    fn round_trip_pre_payload() {
        let p = PreToolUsePayload {
            hook_event_name: HookEventNamePre,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: Some("plan".into()),
            agent_id: Some("a-1".into()),
            agent_type: Some("general-purpose".into()),
            effort: None,
            tool_name: "Edit".into(),
            tool_input: json!({"file_path": "/f"}),
            tool_use_id: "tu".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: PreToolUsePayload = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tool_name, "Edit");
        assert_eq!(back.agent_type.as_deref(), Some("general-purpose"));
    }

    /// `effort` is the base-shape `effort: { level }` object (finding #44):
    /// present (as a nested object) only when populated, and OMITTED entirely
    /// when `None` — matching claude-code's conditional `effort:a` spread in
    /// `createBaseHookInput` (`effort` is `void 0` → key absent for
    /// session-lifecycle hooks and effort-incapable models).
    #[test]
    fn effort_present_serializes_as_level_object_and_omitted_when_none() {
        // PRESENT: `effort: { level: "high" }`, placed after the base
        // `agent_type` field and before the event-specific fields.
        let with_effort = PreToolUsePayload {
            hook_event_name: HookEventNamePre,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: Some(EffortLevel::new("high")),
            tool_name: "Bash".into(),
            tool_input: json!({"command": "ls"}),
            tool_use_id: "tu".into(),
        };
        let s = serde_json::to_string(&with_effort).unwrap();
        // Nested `{ "level": "..." }` shape, not a bare string.
        assert!(
            s.contains(r#""effort":{"level":"high"}"#),
            "effort serializes as a {{ level }} object: {s}"
        );
        // Wire position: base block, after `cwd` (the last always-present base
        // field here) and immediately before `tool_name`.
        assert!(
            s.contains(r#""cwd":"/w","effort":{"level":"high"},"tool_name":"Bash""#),
            "effort sits in the base block before event fields: {s}"
        );
        let back: PreToolUsePayload = serde_json::from_str(&s).unwrap();
        assert_eq!(back.effort, Some(EffortLevel::new("high")));

        // ABSENT: `None` → key omitted entirely (skip_serializing_if).
        let no_effort = PreToolUsePayload {
            effort: None,
            ..with_effort.clone()
        };
        let s2 = serde_json::to_string(&no_effort).unwrap();
        assert!(
            !s2.contains("effort"),
            "effort key is omitted when None: {s2}"
        );
        let back2: PreToolUsePayload = serde_json::from_str(&s2).unwrap();
        assert_eq!(back2.effort, None);
    }

    #[test]
    fn parse_response_allow_via_permission_decision() {
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}"#,
            "PreToolUse",
        )
        .unwrap();
        assert_eq!(r.decision, Some(HookDecision::Approve));
    }

    #[test]
    fn parse_response_block_via_legacy_decision() {
        let r = parse_response(
            r#"{"decision":"block","stopReason":"because","continue":false}"#,
            "PreToolUse",
        )
        .unwrap();
        assert_eq!(r.decision, Some(HookDecision::Block));
        assert_eq!(r.reason.as_deref(), Some("because"));
    }

    #[test]
    fn parse_response_event_mismatch_errors() {
        let err = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse"}}"#,
            "PreToolUse",
        )
        .unwrap_err();
        assert!(matches!(
            err,
            HookResponseParseError::EventNameMismatch { .. }
        ));
    }

    #[test]
    fn parse_response_rejects_known_fields_with_wrong_types() {
        let cases = [
            (
                r#"{"continue":"no"}"#,
                "Hook JSON output validation failed — continue: expected boolean, received string",
            ),
            (
                r#"{"decision":123}"#,
                "Hook JSON output validation failed — decision: expected string, received number",
            ),
            (
                r#"{"hookSpecificOutput":"bad"}"#,
                "Hook JSON output validation failed — hookSpecificOutput: expected object, received string",
            ),
            (
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":false}}"#,
                "Hook JSON output validation failed — hookSpecificOutput.permissionDecision: expected string, received boolean",
            ),
        ];
        for (raw, expected) in cases {
            let err = parse_response(raw, "PreToolUse").unwrap_err();
            assert_eq!(err.to_string(), expected, "raw output: {raw}");
        }
    }

    #[test]
    fn parse_response_keeps_unknown_fields_forward_compatible() {
        let response = parse_response(
            r#"{"futureField":123,"hookSpecificOutput":{"hookEventName":"PreToolUse"}}"#,
            "PreToolUse",
        )
        .unwrap();
        assert_eq!(response.decision, None);
    }

    #[test]
    fn parse_response_rejects_invalid_permission_request_answer() {
        let cases = [
            (
                r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":"allow"}}"#,
                "Hook JSON output validation failed — hookSpecificOutput.decision: expected object, received string",
            ),
            (
                r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":123}}}"#,
                "Hook JSON output validation failed — hookSpecificOutput.decision.behavior: expected string, received number",
            ),
            (
                r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"ask"}}}"#,
                "Hook JSON output validation failed — hookSpecificOutput.decision.behavior: expected \"allow\" | \"deny\", received invalid value",
            ),
            (
                r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedPermissions":[{"type":"setMode","mode":"unknown","destination":"session"}]}}}"#,
                "Hook JSON output validation failed — hookSpecificOutput.decision.updatedPermissions.0.mode: expected \"default\" | \"acceptEdits\" | \"bypassPermissions\" | \"plan\" | \"dontAsk\" | \"auto\", received invalid value",
            ),
            (
                r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest"}}"#,
                "Hook JSON output validation failed — hookSpecificOutput.decision: expected object, received missing",
            ),
        ];
        for (raw, expected) in cases {
            let err = parse_response(raw, "PermissionRequest").unwrap_err();
            assert_eq!(err.to_string(), expected, "raw output: {raw}");
        }
    }

    #[test]
    fn parse_response_permission_request_allow_maps_nested_fields() {
        let raw = r#"{
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {
                    "behavior": "allow",
                    "updatedInput": {"command": "printf safe"},
                    "updatedPermissions": [{
                        "type": "addRules",
                        "rules": [{"toolName": "Bash", "ruleContent": "printf *"}],
                        "behavior": "allow",
                        "destination": "session"
                    }]
                }
            }
        }"#;
        let response = parse_response(raw, "PermissionRequest").unwrap();
        assert_eq!(response.decision, Some(HookDecision::Allow));
        assert_eq!(
            response.updated_input,
            Some(json!({"command": "printf safe"}))
        );
        assert_eq!(
            response.updated_permissions,
            Some(vec![json!({
                "type": "addRules",
                "rules": [{"toolName": "Bash", "ruleContent": "printf *"}],
                "behavior": "allow",
                "destination": "session"
            })])
        );
        assert!(matches!(
            response.permission_request_result,
            Some(crate::response::PermissionRequestResult::Allow {
                updated_input: Some(_),
                updated_permissions: Some(_),
            })
        ));
    }

    #[test]
    fn parse_response_permission_request_accepts_set_mode_auto_update() {
        let response = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedPermissions":[{"type":"setMode","mode":"auto","destination":"session"}]}}}"#,
            "PermissionRequest",
        )
        .unwrap();

        assert_eq!(response.decision, Some(HookDecision::Allow));
        assert_eq!(
            response.updated_permissions,
            Some(vec![json!({
                "type": "setMode",
                "mode": "auto",
                "destination": "session"
            })])
        );
    }

    #[test]
    fn parse_response_permission_request_deny_maps_message_and_interrupt() {
        let response = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"not safe","interrupt":true}}}"#,
            "PermissionRequest",
        )
        .unwrap();
        assert_eq!(response.decision, Some(HookDecision::Block));
        assert_eq!(response.reason.as_deref(), Some("not safe"));
        assert_eq!(response.interrupt, Some(true));
        assert!(matches!(
            response.permission_request_result,
            Some(crate::response::PermissionRequestResult::Deny {
                message: Some(_),
                interrupt: Some(true),
            })
        ));
    }

    #[test]
    fn parse_response_permission_request_does_not_use_pre_tool_use_shape() {
        let response = parse_response(
            r#"{"reason":"legacy","hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"},"permissionDecision":"deny","permissionDecisionReason":"legacy reason","updatedInput":{"wrong":true}}}"#,
            "PermissionRequest",
        )
        .unwrap();
        assert_eq!(response.decision, Some(HookDecision::Allow));
        assert_eq!(response.updated_input, None);
        assert_eq!(response.reason, None);
    }

    #[test]
    fn parse_response_permission_request_rejects_malformed_permission_update() {
        let err = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedPermissions":[{"type":"addRules","rules":[{"toolName":4}],"behavior":"allow","destination":"session"}]}}}"#,
            "PermissionRequest",
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("updatedPermissions.0.rules.0.toolName"));
    }

    #[test]
    fn parse_response_keeps_system_message_and_additional_context_separate() {
        // Parity with claude-code: `systemMessage` and
        // `hookSpecificOutput.additionalContext` are DISTINCT fields routed to
        // SEPARATE attachments — `hook_system_message` (NOT model-facing,
        // `messages.ts:4258` → `[]`) vs `hook_additional_context` (model-facing,
        // `messages.ts:4117`). They must NEVER be merged into one field.
        let r = parse_response(
            r#"{"systemMessage":"hello","hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"world"}}"#,
            "PreToolUse",
        )
        .unwrap();
        assert_eq!(
            r.system_message.as_deref(),
            Some("hello"),
            "systemMessage stays on its own field (transcript-facing, not the model)"
        );
        assert_eq!(
            r.additional_context.as_deref(),
            Some("world"),
            "additionalContext stays on its own field (model-facing)"
        );
    }

    #[test]
    fn parse_response_elicitation_accept_with_content() {
        // hookSpecificOutput.{action,content} -> elicitation_response, no block.
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"Elicitation","action":"accept","content":{"token":"xyz"}}}"#,
            "Elicitation",
        )
        .unwrap();
        let er = r.elicitation_response.expect("elicitation response");
        assert_eq!(er.action, "accept");
        assert_eq!(er.content, Some(json!({"token": "xyz"})));
        assert_eq!(r.decision, None, "accept must NOT block");
    }

    #[test]
    fn parse_response_elicitation_decline_blocks() {
        // action:'decline' -> response set AND decision becomes Block (claude-code
        // `parseElicitationHookOutput` sets a blockingError on decline).
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"Elicitation","action":"decline"}}"#,
            "Elicitation",
        )
        .unwrap();
        let er = r.elicitation_response.expect("elicitation response");
        assert_eq!(er.action, "decline");
        assert_eq!(er.content, None);
        assert_eq!(r.decision, Some(HookDecision::Block));
    }

    #[test]
    fn parse_response_no_action_leaves_elicitation_none() {
        // `if (!specific.action) return {}` — no action => no elicitation response.
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"Elicitation","additionalContext":"x"}}"#,
            "Elicitation",
        )
        .unwrap();
        assert!(r.elicitation_response.is_none());
    }

    // ---- PostToolUse `updatedMCPToolOutput` parse (claude-code
    //      `parseHookJSONOutput`, `utils/hooks.ts:646-649`) ------------------

    #[test]
    fn parse_response_post_extracts_updated_mcp_tool_output() {
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":{"content":"new"}}}"#,
            "PostToolUse",
        )
        .unwrap();
        assert_eq!(r.updated_mcp_tool_output, Some(json!({ "content": "new" })));
    }

    #[test]
    fn parse_response_pre_ignores_updated_mcp_tool_output() {
        // The TS switch only reads `updatedMCPToolOutput` for the `PostToolUse`
        // case — a PreToolUse hook returning it has it dropped.
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedMCPToolOutput":{"content":"new"}}}"#,
            "PreToolUse",
        )
        .unwrap();
        assert!(r.updated_mcp_tool_output.is_none());
    }

    #[test]
    fn parse_response_post_null_updated_mcp_tool_output_is_noop() {
        // TS guards on truthiness — a JSON `null` is NOT a replacement.
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":null}}"#,
            "PostToolUse",
        )
        .unwrap();
        assert!(r.updated_mcp_tool_output.is_none());
    }

    #[test]
    fn parse_response_post_without_updated_output_leaves_none() {
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"hi"}}"#,
            "PostToolUse",
        )
        .unwrap();
        assert!(r.updated_mcp_tool_output.is_none());
        assert!(r.updated_tool_output.is_none());
    }

    // ---- #38 hookSpecificOutput.updatedToolOutput (all-tools) -------------

    #[test]
    fn parse_response_post_extracts_updated_tool_output_object() {
        // `!== void 0` semantics: a present non-null value IS a replacement,
        // applied for ALL tools (no isMcp gate). `Some(Some(value))`.
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":{"result":"replaced"}}}"#,
            "PostToolUse",
        )
        .unwrap();
        assert_eq!(
            r.updated_tool_output,
            Some(Some(json!({ "result": "replaced" })))
        );
    }

    #[test]
    fn parse_response_post_explicit_null_updated_tool_output_is_replacement() {
        // KEY DISTINCTION from the MCP field: `updatedToolOutput` uses
        // `!== void 0` — an explicit JSON `null` IS a replacement (the key being
        // present matters), so the outer `Some` is set with `Some(Value::Null)`.
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":null}}"#,
            "PostToolUse",
        )
        .unwrap();
        assert_eq!(r.updated_tool_output, Some(Some(serde_json::Value::Null)));
        // ... whereas the legacy MCP field treats null as a no-op:
        assert!(r.updated_mcp_tool_output.is_none());
    }

    #[test]
    fn parse_response_post_absent_updated_tool_output_is_none() {
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"hi"}}"#,
            "PostToolUse",
        )
        .unwrap();
        assert!(r.updated_tool_output.is_none());
    }

    #[test]
    fn parse_response_pre_ignores_updated_tool_output() {
        // The TS switch only reads `updatedToolOutput` for the `PostToolUse`
        // case — a PreToolUse hook returning it has it dropped.
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedToolOutput":{"x":1}}}"#,
            "PreToolUse",
        )
        .unwrap();
        assert!(r.updated_tool_output.is_none());
    }

    #[test]
    fn updated_tool_output_wire_round_trips_camel_case() {
        // The struct field round-trips through serde with the camelCase wire key
        // and `Option<Option<Value>>` shape preserved.
        let resp = crate::response::HookResponse {
            updated_tool_output: Some(Some(json!({ "a": 1 }))),
            ..Default::default()
        };
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""updated_tool_output":{"a":1}"#), "{s}");
        let back: crate::response::HookResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(back.updated_tool_output, Some(Some(json!({ "a": 1 }))));
        // Default (absent) is skip_serializing_if-omitted.
        let none = crate::response::HookResponse::default();
        let s2 = serde_json::to_string(&none).unwrap();
        assert!(!s2.contains("updated_tool_output"), "{s2}");
    }

    // ---- #37 permissionDecision "defer" (4th value) ----------------------

    #[test]
    fn parse_response_bare_top_level_permission_decision_is_ignored() {
        // R-O2a: the binary's `azn` NEVER reads a BARE top-level
        // `e.permissionDecision` — it only consults
        // `e.hookSpecificOutput.permissionDecision` (gated to PreToolUse). A
        // top-level `permissionDecision` (any value) is therefore IGNORED.
        let r = parse_response(r#"{"permissionDecision":"defer"}"#, "PreToolUse").unwrap();
        assert_eq!(r.decision, None);
        let r = parse_response(r#"{"permissionDecision":"allow"}"#, "PreToolUse").unwrap();
        assert_eq!(r.decision, None);
        // An unknown BARE top-level value must NOT throw either (the binary
        // never reaches a switch for it).
        let r = parse_response(r#"{"permissionDecision":"bogus"}"#, "PreToolUse").unwrap();
        assert_eq!(r.decision, None);
    }

    #[test]
    fn parse_response_hookspecific_defer_maps_to_defer_decision() {
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"defer"}}"#,
            "PreToolUse",
        )
        .unwrap();
        assert_eq!(r.decision, Some(HookDecision::Defer));
    }

    #[test]
    fn parse_response_session_start_initial_message_and_reload_skills() {
        // SessionStart hookSpecificOutput `initialUserMessage` + `reloadSkills`
        // must be captured (previously silently dropped at parse).
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"SessionStart","initialUserMessage":"hi there","reloadSkills":true}}"#,
            "SessionStart",
        )
        .unwrap();
        assert_eq!(r.initial_user_message.as_deref(), Some("hi there"));
        assert_eq!(r.reload_skills, Some(true));
        // Scoped to SessionStart: the same keys on another event are ignored.
        let r2 = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","initialUserMessage":"x","reloadSkills":true}}"#,
            "UserPromptSubmit",
        )
        .unwrap();
        assert_eq!(r2.initial_user_message, None);
        assert_eq!(r2.reload_skills, None);
    }

    #[test]
    fn parse_response_reads_top_level_reason() {
        // claude-code resolves a block message as `hookSpecificOutput
        // .permissionDecisionReason || e.reason || "Blocked by hook"`. The
        // top-level `reason` of a `{decision:"block", reason:"…"}` hook MUST be
        // captured (previously the port read a phantom top-level
        // `permissionDecisionReason` and dropped the real `reason`).
        let r = parse_response(
            r#"{"decision":"block","reason":"policy violation"}"#,
            "PreToolUse",
        )
        .unwrap();
        assert_eq!(r.decision, Some(HookDecision::Block));
        assert_eq!(r.reason.as_deref(), Some("policy violation"));
    }

    #[test]
    fn parse_response_hsout_permission_decision_reason_overrides_top_level_reason() {
        // Precedence: `hookSpecificOutput.permissionDecisionReason || e.reason`.
        // The hookSpecificOutput reason wins over a top-level `reason`.
        let r = parse_response(
            r#"{"reason":"top","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"specific"}}"#,
            "PreToolUse",
        )
        .unwrap();
        assert_eq!(r.decision, Some(HookDecision::Block));
        assert_eq!(r.reason.as_deref(), Some("specific"));
    }

    #[test]
    fn parse_response_ignores_phantom_top_level_permission_decision_reason() {
        // There is NO top-level `permissionDecisionReason` in CC's schema (it is
        // a hookSpecificOutput-only field). A top-level one must NOT populate the
        // reason.
        let r = parse_response(
            r#"{"decision":"block","permissionDecisionReason":"phantom"}"#,
            "PreToolUse",
        )
        .unwrap();
        assert_eq!(r.reason, None);
    }

    #[test]
    fn parse_response_hsout_permission_decision_overrides_legacy_block() {
        // R-D2: `azn`'s SECOND switch reassigns `permissionBehavior`
        // UNCONDITIONALLY — a `hookSpecificOutput.permissionDecision` ALWAYS
        // overrides a prior legacy `decision:"block"` (there is no
        // `if (decision != Block)` guard in the binary). So `block` + hsOut
        // `defer` resolves to `Defer` (the binary's `permissionBehavior="defer"`
        // wins over the earlier `"deny"`), NOT `Block`.
        let r = parse_response(
            r#"{"decision":"block","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"defer"}}"#,
            "PreToolUse",
        )
        .unwrap();
        assert_eq!(r.decision, Some(HookDecision::Defer));
    }

    #[test]
    fn parse_response_hsout_allow_overrides_legacy_block() {
        // R-D2 target case: `{"decision":"block","hookSpecificOutput":
        // {"permissionDecision":"allow"}}` → Approve (the binary's
        // `permissionBehavior="allow"`), NOT Block. The legacy block is
        // unconditionally overridden by the hsOut allow.
        let r = parse_response(
            r#"{"decision":"block","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}"#,
            "PreToolUse",
        )
        .unwrap();
        assert_eq!(r.decision, Some(HookDecision::Approve));
    }

    #[test]
    fn parse_response_hsout_ask_maps_to_ask_decision() {
        // R-D3: `permissionDecision:"ask"` → `HookDecision::Ask` (previously
        // silently dropped via `_ => {}`). It also unconditionally overrides a
        // legacy block.
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask"}}"#,
            "PreToolUse",
        )
        .unwrap();
        assert_eq!(r.decision, Some(HookDecision::Ask));
        let r = parse_response(
            r#"{"decision":"block","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask"}}"#,
            "PreToolUse",
        )
        .unwrap();
        assert_eq!(r.decision, Some(HookDecision::Ask));
    }

    #[test]
    fn parse_response_hsout_permission_decision_gated_to_pre_tool_use() {
        // R-O2b: the hsOut.permissionDecision switch is gated to
        // `hookEventName === "PreToolUse"`. For a non-PreToolUse event the
        // permissionDecision is ignored (the binary never enters the switch).
        // PostToolUse hsOut `allow` must NOT set a decision.
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","permissionDecision":"allow"}}"#,
            "PostToolUse",
        )
        .unwrap();
        assert_eq!(r.decision, None);
    }

    #[test]
    fn parse_response_model_switch_permission_decisions_match_schema() {
        let allowed = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PreModelSwitch","permissionDecision":"allow","permissionDecisionReason":"approved"}}"#,
            "PreModelSwitch",
        )
        .unwrap();
        assert_eq!(allowed.decision, Some(HookDecision::Approve));
        assert_eq!(allowed.reason.as_deref(), Some("approved"));

        let asked = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PreModelSwitch","permissionDecision":"ask"}}"#,
            "PreModelSwitch",
        )
        .unwrap();
        assert_eq!(asked.decision, Some(HookDecision::Ask));

        let blocked = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PreModelSwitch","permissionDecision":"deny"}}"#,
            "PreModelSwitch",
        )
        .unwrap();
        assert_eq!(blocked.decision, Some(HookDecision::Block));

        let deferred = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PreModelSwitch","permissionDecision":"defer"}}"#,
            "PreModelSwitch",
        )
        .unwrap_err();
        assert!(matches!(
            deferred,
            HookResponseParseError::UnknownPermissionDecision { value } if value == "defer"
        ));

        let post = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PostModelSwitch","additionalContext":"use the new model"}}"#,
            "PostModelSwitch",
        )
        .unwrap();
        assert_eq!(post.decision, None);
        assert_eq!(
            post.additional_context.as_deref(),
            Some("use the new model")
        );
    }

    #[test]
    fn parse_response_unknown_decision_throws() {
        // R-O2c: an unrecognised legacy `decision` rejects the whole output
        // (`azn` `default: throw Error("Unknown hook decision type: …")`).
        let err = parse_response(r#"{"decision":"maybe"}"#, "PreToolUse").unwrap_err();
        assert!(
            matches!(&err, HookResponseParseError::UnknownDecision { value } if value == "maybe"),
            "{err:?}"
        );
        // Exact message shape mirrors the binary.
        assert_eq!(
            err.to_string(),
            "Unknown hook decision type: maybe. Valid types are: approve, block"
        );
        // An empty `decision` is falsy in the binary's `if(e.decision)` — skipped, not thrown.
        let r = parse_response(r#"{"decision":""}"#, "PreToolUse").unwrap();
        assert_eq!(r.decision, None);
    }

    #[test]
    fn parse_response_unknown_permission_decision_throws() {
        // R-O2c: an unrecognised hsOut `permissionDecision` rejects the whole
        // output (`azn` `default: throw Error("Unknown hook permissionDecision
        // type: …")`).
        let err = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"perhaps"}}"#,
            "PreToolUse",
        )
        .unwrap_err();
        assert!(
            matches!(&err, HookResponseParseError::UnknownPermissionDecision { value } if value == "perhaps"),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "Unknown hook permissionDecision type: perhaps. Valid types are: allow, deny, ask, defer"
        );
        // An empty hsOut `permissionDecision` is falsy in the binary's
        // `&& e.hookSpecificOutput.permissionDecision` guard — skipped, not thrown.
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":""}}"#,
            "PreToolUse",
        )
        .unwrap();
        assert_eq!(r.decision, None);
    }

    #[test]
    fn defer_decision_round_trips() {
        let s = serde_json::to_string(&HookDecision::Defer).unwrap();
        assert_eq!(s, r#""Defer""#);
        let back: HookDecision = serde_json::from_str(&s).unwrap();
        assert_eq!(back, HookDecision::Defer);
        // existing variants unchanged
        assert_eq!(
            serde_json::to_string(&HookDecision::Block).unwrap(),
            r#""Block""#
        );
    }

    #[test]
    fn ask_decision_round_trips() {
        // R-D3: the new `Ask` variant serialises/deserialises like the others.
        let s = serde_json::to_string(&HookDecision::Ask).unwrap();
        assert_eq!(s, r#""Ask""#);
        let back: HookDecision = serde_json::from_str(&s).unwrap();
        assert_eq!(back, HookDecision::Ask);
    }

    // ---- #40 top-level terminalSequence ----------------------------------

    #[test]
    fn parse_response_reads_top_level_terminal_sequence() {
        // JSON-escaped ESC `]9;hi` BEL. Parse captures the RAW string; the
        // allowlist validation happens at apply time (the consumer).
        let raw = "{\"terminalSequence\":\"\\u001b]9;hi\\u0007\"}";
        let r = parse_response(raw, "PreToolUse").unwrap();
        assert_eq!(
            r.terminal_sequence.as_deref(),
            Some("\u{001b}]9;hi\u{0007}")
        );
    }

    #[test]
    fn parse_response_terminal_sequence_absent_is_none() {
        let r = parse_response(r#"{"systemMessage":"hi"}"#, "PreToolUse").unwrap();
        assert!(r.terminal_sequence.is_none());
    }

    #[test]
    fn terminal_sequence_round_trips_camel_case() {
        let resp = crate::response::HookResponse {
            terminal_sequence: Some("\u{0007}".into()),
            ..Default::default()
        };
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("\"terminal_sequence\":\"\\u0007\""), "{s}");
        let back: crate::response::HookResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(back.terminal_sequence.as_deref(), Some("\u{0007}"));
        // default (None) is omitted
        let none = crate::response::HookResponse::default();
        assert!(!serde_json::to_string(&none)
            .unwrap()
            .contains("terminal_sequence"));
    }

    // ---- PermissionDenied wire payload (`coreSchemas.ts:461-471`) ---------

    #[test]
    fn permission_denied_payload_serializes_byte_lock() {
        let p = PermissionDeniedPayload {
            hook_event_name: HookEventNamePermissionDenied,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: Some("default".into()),
            agent_id: None,
            agent_type: None,
            effort: None,
            tool_name: "Bash".into(),
            tool_input: json!({ "command": "git push" }),
            tool_use_id: "tu-9".into(),
            reason: "policy".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""hook_event_name":"PermissionDenied""#));
        assert!(s.contains(r#""tool_name":"Bash""#));
        assert!(s.contains(r#""tool_input":{"command":"git push"}"#));
        assert!(s.contains(r#""tool_use_id":"tu-9""#));
        assert!(s.contains(r#""reason":"policy""#));
    }

    // ---- B1: lifecycle-event payload byte-lock tests --------------------

    #[test]
    fn stop_payload_serializes_byte_lock() {
        let p = StopPayload {
            hook_event_name: HookEventNameStop,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            stop_hook_active: true,
            last_assistant_message: None,
            background_tasks: None,
            session_crons: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"Stop","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","stop_hook_active":true}"#
        );
    }

    #[test]
    fn stop_payload_serializes_with_last_message() {
        let p = StopPayload {
            hook_event_name: HookEventNameStop,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: Some("default".into()),
            agent_id: None,
            agent_type: None,
            effort: None,
            stop_hook_active: false,
            last_assistant_message: Some("done".into()),
            background_tasks: None,
            session_crons: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"Stop","session_id":"s","transcript_path":"/t","cwd":"/w","permission_mode":"default","stop_hook_active":false,"last_assistant_message":"done"}"#
        );
    }

    #[test]
    fn subagent_stop_payload_serializes_byte_lock() {
        let p = SubagentStopPayload {
            hook_event_name: HookEventNameSubagentStop,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            stop_hook_active: true,
            agent_id: "agent-7".into(),
            agent_transcript_path: "/tmp/agent-7.jsonl".into(),
            agent_type: "general-purpose".into(),
            effort: None,
            last_assistant_message: None,
            background_tasks: None,
            session_crons: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"SubagentStop","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","stop_hook_active":true,"agent_id":"agent-7","agent_transcript_path":"/tmp/agent-7.jsonl","agent_type":"general-purpose"}"#
        );
    }

    #[test]
    fn stop_payload_background_tasks_and_crons_byte_lock() {
        // Locks the LAST-two-keys order (background_tasks then session_crons)
        // and each element's claude key order (Lic / Mic). A `local_bash`
        // task emits {id,type,status,description,command}; a cron emits
        // {id,schedule,recurring,prompt}.
        let p = StopPayload {
            hook_event_name: HookEventNameStop,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            stop_hook_active: false,
            last_assistant_message: None,
            background_tasks: Some(vec![HookBackgroundTask {
                is_idle: false,
                id: "bt1".into(),
                r#type: "shell".into(),
                status: "running".into(),
                description: "d".into(),
                command: Some("ls".into()),
                agent_type: None,
                server: None,
                tool: None,
                name: None,
            }]),
            session_crons: Some(vec![HookSessionCron {
                id: "c1".into(),
                schedule: "* * * * *".into(),
                recurring: false,
                prompt: "p".into(),
            }]),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"Stop","session_id":"s","transcript_path":"/t","cwd":"/w","stop_hook_active":false,"background_tasks":[{"id":"bt1","type":"shell","status":"running","description":"d","command":"ls"}],"session_crons":[{"id":"c1","schedule":"* * * * *","recurring":false,"prompt":"p"}]}"#
        );
    }

    #[test]
    fn stop_payload_empty_background_arrays_emit_brackets() {
        // claude: when tool-use context IS present but registries are empty,
        // both keys are emitted as `[]` (Some(vec![])), never omitted.
        let p = StopPayload {
            hook_event_name: HookEventNameStop,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            stop_hook_active: false,
            last_assistant_message: None,
            background_tasks: Some(vec![]),
            session_crons: Some(vec![]),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"Stop","session_id":"s","transcript_path":"/t","cwd":"/w","stop_hook_active":false,"background_tasks":[],"session_crons":[]}"#
        );
    }

    #[test]
    fn task_completed_payload_serializes_byte_lock() {
        let p = TaskCompletedPayload {
            hook_event_name: HookEventNameTaskCompleted,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            task_id: "task-42".into(),
            task_subject: "Build the thing".into(),
            task_description: None,
            teammate_name: None,
            team_name: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"TaskCompleted","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","task_id":"task-42","task_subject":"Build the thing"}"#
        );
    }

    #[test]
    fn task_completed_payload_serializes_with_optionals() {
        let p = TaskCompletedPayload {
            hook_event_name: HookEventNameTaskCompleted,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            task_id: "t1".into(),
            task_subject: "subj".into(),
            task_description: Some("desc".into()),
            teammate_name: Some("alice".into()),
            team_name: Some("core".into()),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"TaskCompleted","session_id":"s","transcript_path":"/t","cwd":"/w","task_id":"t1","task_subject":"subj","task_description":"desc","teammate_name":"alice","team_name":"core"}"#
        );
    }

    #[test]
    fn task_created_payload_serializes_byte_lock() {
        let p = TaskCreatedPayload {
            hook_event_name: HookEventNameTaskCreated,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            task_id: "task-42".into(),
            task_subject: "LocalBash".into(),
            task_description: Some("do the work".into()),
            teammate_name: None,
            team_name: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"TaskCreated","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","task_id":"task-42","task_subject":"LocalBash","task_description":"do the work"}"#
        );
    }

    /// T25: when the creating teammate's identity is bound, the `TaskCreated`
    /// wire payload carries `teammate_name` / `team_name` (claude-code
    /// `getAgentName()` / `getTeamName()`, `utils/hooks.ts:3756-3764`).
    #[test]
    fn task_created_payload_serializes_teammate_and_team() {
        let p = TaskCreatedPayload {
            hook_event_name: HookEventNameTaskCreated,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            task_id: "t1".into(),
            task_subject: "subj".into(),
            task_description: Some("desc".into()),
            teammate_name: Some("researcher".into()),
            team_name: Some("alpha".into()),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"TaskCreated","session_id":"s","transcript_path":"/t","cwd":"/w","task_id":"t1","task_subject":"subj","task_description":"desc","teammate_name":"researcher","team_name":"alpha"}"#
        );
    }

    #[test]
    fn user_prompt_submit_payload_serializes_byte_lock() {
        let p = UserPromptSubmitPayload {
            hook_event_name: HookEventNameUserPromptSubmit,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            prompt: "fix the bug".into(),
            session_title: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","prompt":"fix the bug"}"#
        );
    }

    #[test]
    fn session_start_payload_serializes_byte_lock() {
        let p = SessionStartPayload {
            hook_event_name: HookEventNameSessionStart,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            source: "startup".into(),
            agent_type: None,
            effort: None,
            model: None,
            session_title: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"SessionStart","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","source":"startup"}"#
        );
    }

    #[test]
    fn session_start_payload_serializes_with_model() {
        let p = SessionStartPayload {
            hook_event_name: HookEventNameSessionStart,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            source: "resume".into(),
            agent_type: Some("code-reviewer".into()),
            effort: None,
            model: Some("claude-opus".into()),
            session_title: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"SessionStart","session_id":"s","transcript_path":"/t","cwd":"/w","source":"resume","agent_type":"code-reviewer","model":"claude-opus"}"#
        );
    }

    #[test]
    fn stop_failure_payload_serializes_byte_lock() {
        let p = StopFailurePayload {
            hook_event_name: HookEventNameStopFailure,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            error: "rate_limit".into(),
            error_details: None,
            last_assistant_message: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"StopFailure","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","error":"rate_limit"}"#
        );
    }

    #[test]
    fn stop_failure_payload_serializes_with_details() {
        let p = StopFailurePayload {
            hook_event_name: HookEventNameStopFailure,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            error: "server_error".into(),
            error_details: Some("upstream 500".into()),
            last_assistant_message: Some("partial".into()),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"StopFailure","session_id":"s","transcript_path":"/t","cwd":"/w","error":"server_error","error_details":"upstream 500","last_assistant_message":"partial"}"#
        );
    }

    // ---- B1: expected_event marker round-trips --------------------------

    #[test]
    fn lifecycle_event_name_markers_round_trip() {
        // Each marker serializes to exactly its literal and deserializes back.
        assert_eq!(
            serde_json::to_string(&HookEventNameStop).unwrap(),
            r#""Stop""#
        );
        assert_eq!(
            serde_json::to_string(&HookEventNameSubagentStop).unwrap(),
            r#""SubagentStop""#
        );
        assert_eq!(
            serde_json::to_string(&HookEventNameTaskCompleted).unwrap(),
            r#""TaskCompleted""#
        );
        assert_eq!(
            serde_json::to_string(&HookEventNameUserPromptSubmit).unwrap(),
            r#""UserPromptSubmit""#
        );
        assert_eq!(
            serde_json::to_string(&HookEventNameSessionStart).unwrap(),
            r#""SessionStart""#
        );
        assert_eq!(
            serde_json::to_string(&HookEventNameStopFailure).unwrap(),
            r#""StopFailure""#
        );

        let _: HookEventNameStop = serde_json::from_str(r#""Stop""#).unwrap();
        let _: HookEventNameSubagentStop = serde_json::from_str(r#""SubagentStop""#).unwrap();
        let _: HookEventNameTaskCompleted = serde_json::from_str(r#""TaskCompleted""#).unwrap();
        let _: HookEventNameUserPromptSubmit =
            serde_json::from_str(r#""UserPromptSubmit""#).unwrap();
        let _: HookEventNameSessionStart = serde_json::from_str(r#""SessionStart""#).unwrap();
        let _: HookEventNameStopFailure = serde_json::from_str(r#""StopFailure""#).unwrap();
    }

    #[test]
    fn lifecycle_event_name_marker_rejects_wrong_literal() {
        assert!(serde_json::from_str::<HookEventNameStop>(r#""SubagentStop""#).is_err());
        assert!(serde_json::from_str::<HookEventNameStopFailure>(r#""Stop""#).is_err());
    }

    #[test]
    fn lifecycle_payloads_round_trip() {
        let p = SubagentStopPayload {
            hook_event_name: HookEventNameSubagentStop,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: Some("plan".into()),
            stop_hook_active: true,
            agent_id: "a-1".into(),
            agent_transcript_path: "/t/a-1.jsonl".into(),
            agent_type: "general-purpose".into(),
            effort: None,
            last_assistant_message: Some("hi".into()),
            background_tasks: None,
            session_crons: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: SubagentStopPayload = serde_json::from_str(&s).unwrap();
        assert_eq!(back.agent_id, "a-1");
        assert!(back.stop_hook_active);
        assert_eq!(back.last_assistant_message.as_deref(), Some("hi"));
    }

    // ---- B1: parse_response validates the new hookEventNames ------------

    #[test]
    fn parse_response_validates_new_event_names() {
        for name in [
            "Stop",
            "SubagentStop",
            "TaskCompleted",
            "UserPromptSubmit",
            "SessionStart",
            "StopFailure",
        ] {
            // Matching event name parses fine.
            let raw = format!(
                r#"{{"hookSpecificOutput":{{"hookEventName":"{name}","additionalContext":"x"}}}}"#
            );
            let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
            let r = parse_response(&raw, leaked).unwrap();
            // `additionalContext` now lands on its own field (NOT system_message).
            assert_eq!(r.additional_context.as_deref(), Some("x"));

            // A mismatched name is rejected.
            let err = parse_response(
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse"}}"#,
                leaked,
            )
            .unwrap_err();
            assert!(matches!(
                err,
                HookResponseParseError::EventNameMismatch { .. }
            ));
        }
    }

    // ---- B6: additional-event payload byte-lock tests -------------------

    #[test]
    fn post_tool_use_failure_payload_serializes_byte_lock() {
        let p = PostToolUseFailurePayload {
            hook_event_name: HookEventNamePostToolUseFailure,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            tool_name: "Bash".into(),
            tool_input: Value::Null,
            tool_use_id: "tu-1".into(),
            error: "boom".into(),
            is_interrupt: None,
            duration_ms: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"PostToolUseFailure","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","tool_name":"Bash","tool_input":null,"tool_use_id":"tu-1","error":"boom"}"#
        );
    }

    #[test]
    fn post_tool_use_failure_payload_serializes_with_interrupt() {
        let p = PostToolUseFailurePayload {
            hook_event_name: HookEventNamePostToolUseFailure,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            tool_name: "Edit".into(),
            tool_input: json!({"file_path": "/f"}),
            tool_use_id: "tu".into(),
            error: "cancelled".into(),
            is_interrupt: Some(true),
            duration_ms: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""tool_input":{"file_path":"/f"}"#));
        assert!(s.contains(r#""error":"cancelled","is_interrupt":true"#));
    }

    #[test]
    fn session_end_payload_serializes_byte_lock() {
        let p = SessionEndPayload {
            hook_event_name: HookEventNameSessionEnd,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            reason: "logout".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"SessionEnd","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","reason":"logout"}"#
        );
    }

    #[test]
    fn pre_compact_payload_serializes_null_custom_instructions() {
        // `custom_instructions` is `.nullable()` (not optional) — must always
        // be present, serialized as `null` when absent.
        let p = PreCompactPayload {
            hook_event_name: HookEventNamePreCompact,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            trigger: "manual".into(),
            custom_instructions: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"PreCompact","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","trigger":"manual","custom_instructions":null}"#
        );
    }

    #[test]
    fn pre_compact_payload_serializes_with_custom_instructions() {
        let p = PreCompactPayload {
            hook_event_name: HookEventNamePreCompact,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            trigger: "auto".into(),
            custom_instructions: Some("keep the API surface".into()),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""trigger":"auto","custom_instructions":"keep the API surface""#));
    }

    #[test]
    fn post_compact_payload_serializes_byte_lock() {
        let p = PostCompactPayload {
            hook_event_name: HookEventNamePostCompact,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            trigger: String::new(),
            compact_summary: "did the thing".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"PostCompact","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","trigger":"","compact_summary":"did the thing"}"#
        );
    }

    #[test]
    fn model_switch_payloads_keep_required_cache_fields_and_nullable_request() {
        let pre = PreModelSwitchPayload {
            hook_event_name: HookEventNamePreModelSwitch,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            from_model: "claude-sonnet-4-6".into(),
            to_model: "claude-opus-4-6".into(),
            requested_model: None,
            source: "command".into(),
            context_tokens: 2048,
            prompt_cache_warm: false,
            cache_ttl: "5m".into(),
            estimated_cache_write_usd: 0.0123,
            pricing: "catalog".into(),
        };
        assert_eq!(
            serde_json::to_string(&pre).unwrap(),
            r#"{"hook_event_name":"PreModelSwitch","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","from_model":"claude-sonnet-4-6","to_model":"claude-opus-4-6","requested_model":null,"source":"command","context_tokens":2048,"prompt_cache_warm":false,"cache_ttl":"5m","estimated_cache_write_usd":0.0123,"pricing":"catalog"}"#
        );

        let post = PostModelSwitchPayload {
            hook_event_name: HookEventNamePostModelSwitch,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            from_model: "claude-sonnet-4-6".into(),
            to_model: "claude-opus-4-6".into(),
            requested_model: Some("opus".into()),
            source: "resume".into(),
            context_tokens: 2048,
            prompt_cache_warm: true,
            cache_ttl: "1h".into(),
            estimated_cache_write_usd: 0.0,
            pricing: "default".into(),
        };
        let post_json = serde_json::to_string(&post).unwrap();
        assert!(post_json.contains(r#""hook_event_name":"PostModelSwitch""#));
        assert!(post_json.contains(r#""requested_model":"opus""#));
        assert!(post_json
            .contains(r#""cache_ttl":"1h","estimated_cache_write_usd":0.0,"pricing":"default""#));
    }

    #[test]
    fn notification_payload_serializes_byte_lock() {
        let p = NotificationPayload {
            hook_event_name: HookEventNameNotification,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            message: "build done".into(),
            title: None,
            notification_type: "info".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"Notification","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","message":"build done","notification_type":"info"}"#
        );
    }

    #[test]
    fn notification_payload_serializes_with_title() {
        let p = NotificationPayload {
            hook_event_name: HookEventNameNotification,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            message: "msg".into(),
            title: Some("Heads up".into()),
            notification_type: "warn".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""message":"msg","title":"Heads up","notification_type":"warn""#));
    }

    #[test]
    fn permission_request_payload_serializes_byte_lock() {
        let p = PermissionRequestPayload {
            hook_event_name: HookEventNamePermissionRequest,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            tool_name: "Bash".into(),
            tool_input: json!({"command": "rm -rf /"}),
            permission_suggestions: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"PermissionRequest","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","tool_name":"Bash","tool_input":{"command":"rm -rf /"}}"#
        );
    }

    #[test]
    fn setup_payload_serializes_byte_lock() {
        let p = SetupPayload {
            hook_event_name: HookEventNameSetup,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            trigger: String::new(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"Setup","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","trigger":""}"#
        );
    }

    #[test]
    fn subagent_start_payload_serializes_byte_lock() {
        let p = SubagentStartPayload {
            hook_event_name: HookEventNameSubagentStart,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: "agent-7".into(),
            agent_type: "general-purpose".into(),
            effort: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"SubagentStart","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","agent_id":"agent-7","agent_type":"general-purpose"}"#
        );
    }

    /// `DirectoryAdded` (2.1.219) wire shape — claude-code `a$t`:
    /// `{...,hook_event_name:"DirectoryAdded",directory:e,source:t}`.
    #[test]
    fn directory_added_payload_matches_the_oracle_wire_shape() {
        let p = DirectoryAddedPayload {
            hook_event_name: HookEventNameDirectoryAdded,
            session_id: "s1".into(),
            transcript_path: "/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            directory: "/work/extra".into(),
            source: "add_dir".into(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(v["hook_event_name"], "DirectoryAdded");
        assert_eq!(v["directory"], "/work/extra");
        assert_eq!(v["source"], "add_dir");
        // Absent optionals must not serialise as nulls.
        for k in ["permission_mode", "agent_id", "agent_type", "effort"] {
            assert!(v.get(k).is_none(), "{k} must be omitted when None");
        }
    }

    #[test]
    fn cwd_changed_payload_serializes_byte_lock() {
        let p = CwdChangedPayload {
            hook_event_name: HookEventNameCwdChanged,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            old_cwd: "/old".into(),
            new_cwd: "/new".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"CwdChanged","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","old_cwd":"/old","new_cwd":"/new"}"#
        );
    }

    #[test]
    fn file_changed_payload_serializes_byte_lock() {
        let p = FileChangedPayload {
            hook_event_name: HookEventNameFileChanged,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            file_path: "/work/src/main.rs".into(),
            event: "change".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"FileChanged","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","file_path":"/work/src/main.rs","event":"change"}"#
        );
    }

    #[test]
    fn worktree_remove_payload_serializes_byte_lock() {
        let p = WorktreeRemovePayload {
            hook_event_name: HookEventNameWorktreeRemove,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            worktree_path: "/work/.worktrees/feat".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"WorktreeRemove","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","worktree_path":"/work/.worktrees/feat"}"#
        );
    }

    #[test]
    fn b6_event_name_markers_round_trip() {
        for (got, want) in [
            (
                serde_json::to_string(&HookEventNamePostToolUseFailure).unwrap(),
                r#""PostToolUseFailure""#,
            ),
            (
                serde_json::to_string(&HookEventNameSessionEnd).unwrap(),
                r#""SessionEnd""#,
            ),
            (
                serde_json::to_string(&HookEventNamePreCompact).unwrap(),
                r#""PreCompact""#,
            ),
            (
                serde_json::to_string(&HookEventNamePostCompact).unwrap(),
                r#""PostCompact""#,
            ),
            (
                serde_json::to_string(&HookEventNamePreModelSwitch).unwrap(),
                r#""PreModelSwitch""#,
            ),
            (
                serde_json::to_string(&HookEventNamePostModelSwitch).unwrap(),
                r#""PostModelSwitch""#,
            ),
            (
                serde_json::to_string(&HookEventNameNotification).unwrap(),
                r#""Notification""#,
            ),
            (
                serde_json::to_string(&HookEventNamePermissionRequest).unwrap(),
                r#""PermissionRequest""#,
            ),
            (
                serde_json::to_string(&HookEventNameSetup).unwrap(),
                r#""Setup""#,
            ),
            (
                serde_json::to_string(&HookEventNameSubagentStart).unwrap(),
                r#""SubagentStart""#,
            ),
            (
                serde_json::to_string(&HookEventNameCwdChanged).unwrap(),
                r#""CwdChanged""#,
            ),
            (
                serde_json::to_string(&HookEventNameFileChanged).unwrap(),
                r#""FileChanged""#,
            ),
            (
                serde_json::to_string(&HookEventNameWorktreeRemove).unwrap(),
                r#""WorktreeRemove""#,
            ),
        ] {
            assert_eq!(got, want);
        }
        let _: HookEventNamePostToolUseFailure =
            serde_json::from_str(r#""PostToolUseFailure""#).unwrap();
        let _: HookEventNameSessionEnd = serde_json::from_str(r#""SessionEnd""#).unwrap();
        let _: HookEventNameWorktreeRemove = serde_json::from_str(r#""WorktreeRemove""#).unwrap();
        assert!(serde_json::from_str::<HookEventNameSetup>(r#""Notification""#).is_err());
    }

    // ---- deferred-completion batch: final-four payload byte-lock tests ------

    #[test]
    fn config_change_payload_serializes_byte_lock() {
        let p = ConfigChangePayload {
            hook_event_name: HookEventNameConfigChange,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            source: crate::events::ConfigChangeSource::LocalSettings,
            file_path: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"ConfigChange","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","source":"local_settings"}"#
        );
    }

    #[test]
    fn config_change_payload_serializes_with_file_path() {
        let p = ConfigChangePayload {
            hook_event_name: HookEventNameConfigChange,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            source: crate::events::ConfigChangeSource::PolicySettings,
            file_path: Some("/etc/claude/policy.json".into()),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"ConfigChange","session_id":"s","transcript_path":"/t","cwd":"/w","source":"policy_settings","file_path":"/etc/claude/policy.json"}"#
        );
    }

    #[test]
    fn instructions_loaded_payload_serializes_byte_lock() {
        let p = InstructionsLoadedPayload {
            hook_event_name: HookEventNameInstructionsLoaded,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            file_path: "/work/LINGXI.md".into(),
            memory_type: crate::events::InstructionsMemoryType::Project,
            load_reason: crate::events::InstructionsLoadReason::SessionStart,
            globs: None,
            trigger_file_path: None,
            parent_file_path: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"InstructionsLoaded","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","file_path":"/work/LINGXI.md","memory_type":"Project","load_reason":"session_start"}"#
        );
    }

    #[test]
    fn instructions_loaded_payload_serializes_with_optionals() {
        let p = InstructionsLoadedPayload {
            hook_event_name: HookEventNameInstructionsLoaded,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            file_path: "/w/rules/api.md".into(),
            memory_type: crate::events::InstructionsMemoryType::Managed,
            load_reason: crate::events::InstructionsLoadReason::Compact,
            globs: Some(vec!["src/**/*.rs".into()]),
            trigger_file_path: Some("/w/src/main.rs".into()),
            parent_file_path: Some("/w/LINGXI.md".into()),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"InstructionsLoaded","session_id":"s","transcript_path":"/t","cwd":"/w","file_path":"/w/rules/api.md","memory_type":"Managed","load_reason":"compact","globs":["src/**/*.rs"],"trigger_file_path":"/w/src/main.rs","parent_file_path":"/w/LINGXI.md"}"#
        );
    }

    #[test]
    fn elicitation_payload_serializes_byte_lock() {
        let p = ElicitationPayload {
            hook_event_name: HookEventNameElicitation,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            mcp_server_name: "github".into(),
            message: "Authorize?".into(),
            mode: None,
            url: None,
            elicitation_id: None,
            requested_schema: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"Elicitation","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","mcp_server_name":"github","message":"Authorize?"}"#
        );
    }

    #[test]
    fn elicitation_payload_serializes_with_optionals_and_permission_mode() {
        // Elicitation alone threads permission_mode (createBaseHookInput(permissionMode)).
        let p = ElicitationPayload {
            hook_event_name: HookEventNameElicitation,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: Some("default".into()),
            agent_id: None,
            agent_type: None,
            effort: None,
            mcp_server_name: "linear".into(),
            message: "Pick".into(),
            mode: Some(crate::events::ElicitationMode::Url),
            url: Some("https://example.test".into()),
            elicitation_id: Some("e-1".into()),
            requested_schema: Some(json!({"type": "object"})),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"Elicitation","session_id":"s","transcript_path":"/t","cwd":"/w","permission_mode":"default","mcp_server_name":"linear","message":"Pick","mode":"url","url":"https://example.test","elicitation_id":"e-1","requested_schema":{"type":"object"}}"#
        );
    }

    #[test]
    fn worktree_create_payload_serializes_byte_lock() {
        let p = WorktreeCreatePayload {
            hook_event_name: HookEventNameWorktreeCreate,
            session_id: "sess-1".into(),
            transcript_path: "/tmp/t.jsonl".into(),
            cwd: "/work".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            name: "feature-x".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(
            s,
            r#"{"hook_event_name":"WorktreeCreate","session_id":"sess-1","transcript_path":"/tmp/t.jsonl","cwd":"/work","name":"feature-x"}"#
        );
    }

    #[test]
    fn deferred_batch_event_name_markers_round_trip() {
        for (got, want) in [
            (
                serde_json::to_string(&HookEventNameConfigChange).unwrap(),
                r#""ConfigChange""#,
            ),
            (
                serde_json::to_string(&HookEventNameInstructionsLoaded).unwrap(),
                r#""InstructionsLoaded""#,
            ),
            (
                serde_json::to_string(&HookEventNameElicitation).unwrap(),
                r#""Elicitation""#,
            ),
            (
                serde_json::to_string(&HookEventNameWorktreeCreate).unwrap(),
                r#""WorktreeCreate""#,
            ),
        ] {
            assert_eq!(got, want);
        }
        let _: HookEventNameConfigChange = serde_json::from_str(r#""ConfigChange""#).unwrap();
        let _: HookEventNameInstructionsLoaded =
            serde_json::from_str(r#""InstructionsLoaded""#).unwrap();
        let _: HookEventNameElicitation = serde_json::from_str(r#""Elicitation""#).unwrap();
        let _: HookEventNameWorktreeCreate = serde_json::from_str(r#""WorktreeCreate""#).unwrap();
        assert!(serde_json::from_str::<HookEventNameConfigChange>(r#""Elicitation""#).is_err());
    }

    // ---- [P0] ElicitationResult payload (parity fix) ---------------------

    #[test]
    fn elicitation_result_payload_serializes_required_fields() {
        // Binary-confirmed schema: {hook_event_name:"ElicitationResult",
        // mcp_server_name:string, …, action:enum(["accept","decline","cancel"]),
        // content?:record}. Required wire shape with minimal fields.
        let p = ElicitationResultPayload {
            hook_event_name: HookEventNameElicitationResult,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            mcp_server_name: "my-server".into(),
            elicitation_id: None,
            mode: None,
            action: "accept".into(),
            content: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(
            s.contains(r#""hook_event_name":"ElicitationResult""#),
            "{s}"
        );
        assert!(s.contains(r#""mcp_server_name":"my-server""#), "{s}");
        assert!(s.contains(r#""action":"accept""#), "{s}");
        // Optional fields absent when None.
        assert!(!s.contains("elicitation_id"), "{s}");
        assert!(!s.contains("content"), "{s}");
        assert!(!s.contains("mode"), "{s}");
    }

    #[test]
    fn elicitation_result_payload_serializes_with_content() {
        // `content` is a record(string, unknown) — passes through as-is.
        let p = ElicitationResultPayload {
            hook_event_name: HookEventNameElicitationResult,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            mcp_server_name: "srv".into(),
            elicitation_id: Some("eid-42".into()),
            mode: Some(crate::events::ElicitationMode::Form),
            action: "decline".into(),
            content: Some(json!({"reason": "no thanks"})),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""action":"decline""#), "{s}");
        assert!(s.contains(r#""elicitation_id":"eid-42""#), "{s}");
        assert!(s.contains(r#""mode":"form""#), "{s}");
        assert!(s.contains(r#""content":{"reason":"no thanks"}"#), "{s}");
    }

    #[test]
    fn elicitation_result_marker_serializes_correct_literal() {
        assert_eq!(
            serde_json::to_string(&HookEventNameElicitationResult).unwrap(),
            r#""ElicitationResult""#
        );
        let _: HookEventNameElicitationResult =
            serde_json::from_str(r#""ElicitationResult""#).unwrap();
        assert!(
            serde_json::from_str::<HookEventNameElicitationResult>(r#""Elicitation""#).is_err()
        );
    }

    // ---- [P1] session_title in input payloads ----------------------------

    #[test]
    fn user_prompt_submit_payload_with_session_title() {
        // Binary-confirmed: `session_title` is optional on `UserPromptSubmit`
        // (BIN off 201745825). When `Some`, it appears AFTER `prompt`.
        let p = UserPromptSubmitPayload {
            hook_event_name: HookEventNameUserPromptSubmit,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            agent_type: None,
            effort: None,
            prompt: "hello".into(),
            session_title: Some("My Project".into()),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""session_title":"My Project""#), "{s}");
        // When None, key is omitted.
        let p2 = UserPromptSubmitPayload {
            session_title: None,
            ..p
        };
        let s2 = serde_json::to_string(&p2).unwrap();
        assert!(!s2.contains("session_title"), "{s2}");
    }

    #[test]
    fn session_start_payload_with_session_title() {
        // Binary-confirmed: `session_title` is optional on `SessionStart`
        // (BIN off ~201746000).
        let p = SessionStartPayload {
            hook_event_name: HookEventNameSessionStart,
            session_id: "s".into(),
            transcript_path: "/t".into(),
            cwd: "/w".into(),
            prompt_id: None,
            permission_mode: None,
            agent_id: None,
            source: "startup".into(),
            agent_type: None,
            effort: None,
            model: None,
            session_title: Some("New Chat".into()),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""session_title":"New Chat""#), "{s}");
        // When None, key is omitted.
        let p2 = SessionStartPayload {
            session_title: None,
            ..p
        };
        let s2 = serde_json::to_string(&p2).unwrap();
        assert!(!s2.contains("session_title"), "{s2}");
    }

    // ---- [P1] UserPromptSubmit hookSpecificOutput: sessionTitle + suppressOriginalPrompt

    #[test]
    fn parse_response_user_prompt_submit_session_title() {
        // Binary-confirmed: `sessionTitle` in hookSpecificOutput for `UserPromptSubmit`
        // (BIN off 201754804). Scoped to `UserPromptSubmit` only.
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","sessionTitle":"Renamed"}}"#,
            "UserPromptSubmit",
        )
        .unwrap();
        assert_eq!(r.session_title.as_deref(), Some("Renamed"));
        // Not parsed for other events.
        let r2 = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"Stop","sessionTitle":"x"}}"#,
            "Stop",
        )
        .unwrap();
        assert!(
            r2.session_title.is_none(),
            "sessionTitle ignored for non-UserPromptSubmit"
        );
    }

    #[test]
    fn parse_response_user_prompt_submit_suppress_original_prompt() {
        // Binary-confirmed: `suppressOriginalPrompt` in hookSpecificOutput for
        // `UserPromptSubmit` (BIN off 201754804; description: "When decision is
        // 'block', omit the original prompt from the block message").
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","suppressOriginalPrompt":true}}"#,
            "UserPromptSubmit",
        )
        .unwrap();
        assert!(r.suppress_original_prompt);
        // `false` is passed through correctly.
        let r2 = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","suppressOriginalPrompt":false}}"#,
            "UserPromptSubmit",
        )
        .unwrap();
        assert!(!r2.suppress_original_prompt);
        // Not parsed for other events.
        let r3 = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"Stop","suppressOriginalPrompt":true}}"#,
            "Stop",
        )
        .unwrap();
        assert!(
            !r3.suppress_original_prompt,
            "suppressOriginalPrompt ignored for non-UserPromptSubmit"
        );
        // Default is false.
        let r4 = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit"}}"#,
            "UserPromptSubmit",
        )
        .unwrap();
        assert!(!r4.suppress_original_prompt);
    }

    // ---- [P1] MessageDisplay hookSpecificOutput: displayContent -----------

    #[test]
    fn parse_response_message_display_display_content() {
        // Binary-confirmed: `displayContent` in hookSpecificOutput for
        // `MessageDisplay` (BIN off 201757586; description: "Text displayed in
        // place of the delta.").
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"MessageDisplay","displayContent":"overridden text"}}"#,
            "MessageDisplay",
        )
        .unwrap();
        assert_eq!(r.display_content.as_deref(), Some("overridden text"));
        // Absent when not set.
        let r2 = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"MessageDisplay"}}"#,
            "MessageDisplay",
        )
        .unwrap();
        assert!(r2.display_content.is_none());
        // Not parsed for other events.
        let r3 = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"Stop","displayContent":"x"}}"#,
            "Stop",
        )
        .unwrap();
        assert!(
            r3.display_content.is_none(),
            "displayContent ignored for non-MessageDisplay"
        );
    }

    // ---- P2-10 hookSpecificOutput.watchPaths (FileChanged / CwdChanged) ----

    #[test]
    fn parse_response_file_changed_extracts_watch_paths() {
        // A `FileChanged` hook adds paths to the watch set; the array of strings
        // is captured verbatim (claude-code `"watchPaths" in hsOut && …`).
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"FileChanged","watchPaths":[".env",".envrc","/etc/abs.conf"]}}"#,
            "FileChanged",
        )
        .unwrap();
        assert_eq!(
            r.watch_paths,
            Some(vec![
                ".env".to_string(),
                ".envrc".to_string(),
                "/etc/abs.conf".to_string(),
            ])
        );
    }

    #[test]
    fn parse_response_cwd_changed_extracts_watch_paths() {
        // The CwdChanged re-resolution flow (`E3r`) also carries `watchPaths`.
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"CwdChanged","watchPaths":["a/b"]}}"#,
            "CwdChanged",
        )
        .unwrap();
        assert_eq!(r.watch_paths, Some(vec!["a/b".to_string()]));
    }

    #[test]
    fn parse_response_present_empty_watch_paths_is_some_empty() {
        // Present-key capture: JS arrays are always truthy, so a present-but-empty
        // `[]` is kept as `Some(vec![])` (distinct from an absent key → `None`).
        let present = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"FileChanged","watchPaths":[]}}"#,
            "FileChanged",
        )
        .unwrap();
        assert_eq!(present.watch_paths, Some(vec![]));

        let absent = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"FileChanged"}}"#,
            "FileChanged",
        )
        .unwrap();
        assert!(absent.watch_paths.is_none(), "absent key → None");
    }

    // ---- SH-01 hookSpecificOutput.classifierContext (NEW in 2.1.238) ------

    /// A `PostToolUse` hook's `classifierContext` is parsed onto its own
    /// channel — NOT onto `additional_context` (model-facing) and NOT onto
    /// `system_message` (transcript-facing). It is classifier-facing.
    #[test]
    fn parse_response_reads_post_tool_use_classifier_context() {
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","classifierContext":"the user approved this in the desktop app"}}"#,
            "PostToolUse",
        )
        .unwrap();
        assert_eq!(
            r.classifier_context.as_deref(),
            Some("the user approved this in the desktop app")
        );
        assert!(r.additional_context.is_none());
        assert!(r.system_message.is_none());
    }

    /// The consumption site guards on truthiness (`if(z.classifierContext)`),
    /// so an empty string contributes nothing.
    #[test]
    fn parse_response_ignores_an_empty_classifier_context() {
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","classifierContext":""}}"#,
            "PostToolUse",
        )
        .unwrap();
        assert!(r.classifier_context.is_none());
    }

    /// The field lives in the `PostToolUse` arm of the `hookSpecificOutput`
    /// union — a `PreToolUse` hook returning it has it ignored, exactly like
    /// `updatedToolOutput`.
    #[test]
    fn parse_response_ignores_classifier_context_outside_post_tool_use() {
        let r = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","classifierContext":"nope"}}"#,
            "PreToolUse",
        )
        .unwrap();
        assert!(r.classifier_context.is_none());
    }

    #[test]
    fn parse_response_non_array_watch_paths_is_schema_error() {
        let err = parse_response(
            r#"{"hookSpecificOutput":{"hookEventName":"FileChanged","watchPaths":".env"}}"#,
            "FileChanged",
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Hook JSON output validation failed — hookSpecificOutput.watchPaths: expected array, received string"
        );
    }

    /// PARITY 2.1.263 `imr` — the three hook-authoring hints, byte-locked. Each
    /// is verified present in the 2.1.263 binary.
    #[test]
    fn validation_hints_are_byte_locked() {
        use crate::hook_payload::validation_hint;
        let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();

        // hookSpecificOutput without hookEventName — the binary REPLACES the
        // message with this one rather than appending.
        assert_eq!(
            validation_hint(&j(
                r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#
            ))
            .as_deref(),
            Some("hookSpecificOutput is missing required field \"hookEventName\"")
        );

        // PermissionRequest with a non-object `decision`.
        assert_eq!(
            validation_hint(&j(
                r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":"allow"}}"#
            ))
            .as_deref(),
            Some(" (PermissionRequest decision must be {\"behavior\": \"allow\"} or {\"behavior\": \"deny\", \"message\": \"...\"})")
        );
        // …and none when the decision IS an object.
        assert_eq!(
            validation_hint(&j(
                r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
            )),
            None
        );

        // The legacy top-level `decision`, with the ask variant differing.
        assert_eq!(
            validation_hint(&j(r#"{"decision":"ask"}"#)).as_deref(),
            Some(" (top-level decision is the legacy approve|block field; for \"ask\" use hookSpecificOutput.permissionDecision in a PreToolUse hook)")
        );
        for behavior in ["allow", "deny"] {
            assert_eq!(
                validation_hint(&j(&format!(r#"{{"decision":"{behavior}"}}"#))).as_deref(),
                Some(format!(
                    " (top-level decision is the legacy approve|block field; for \"{behavior}\" use hookSpecificOutput.permissionDecision in a PreToolUse hook, or hookSpecificOutput.decision: {{\"behavior\": \"{behavior}\"}} in a PermissionRequest hook)"
                ).as_str())
            );
        }
        // The legacy approve/block spellings are the CORRECT use of that field,
        // so they get no hint.
        assert_eq!(validation_hint(&j(r#"{"decision":"approve"}"#)), None);
        assert_eq!(validation_hint(&j(r#"{"decision":"block"}"#)), None);
        assert_eq!(validation_hint(&j(r#"{}"#)), None);
        assert_eq!(validation_hint(&j(r#"[]"#)), None);
    }
}
