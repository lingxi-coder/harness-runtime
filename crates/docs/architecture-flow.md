# LingXi Architecture Flowcharts

## 1. Crate Dependency Layers (Inverted Pyramid)

```mermaid
graph TD
    subgraph Layer6["Layer 6: Composition Roots"]
        cli["apps/cli"]
        desktop["apps/engine-desktop"]
        mobile["apps/engine-mobile"]
        bridge["apps/bridge-server"]
        tui["tui"]
        tui_core["tui-core"]
    end

    subgraph Layer5["Layer 5: Platform Adapters"]
        posix["platforms/posix"]
        windows["platforms/windows"]
        ios["platforms/ios"]
        android["platforms/android"]
    end

    subgraph Layer4["Layer 4: Tool Implementations"]
        t_file["tools/file"]
        t_shell["tools/shell"]
        t_web["tools/web"]
        t_agent["tools/agent"]
        t_lsp["tools/lsp"]
        t_mcp["tools/mcp"]
        t_skill["tools/skill"]
        t_worktree["tools/worktree"]
        t_plan["tools/plan"]
        t_task["tools/task"]
        t_meta["tools/meta"]
        t_cron["tools/cron"]
        t_ui["tools/ui"]
        t_team["tools/team"]
        t_workflow["tools/workflow"]
        t_mobile["tools/mobile"]
        t_computer["tools/computer-use"]
        t_android["tools/android-use"]
        t_ios["tools/ios-use"]
    end

    subgraph Layer3["Layer 3: Engine Libraries"]
        orch["orchestrator"]
        llm["llm-runtime"]
        compact["compaction"]
        agent["agent"]
        session["session"]
        memory["memory"]
        hooks["hooks"]
        perm["permission"]
        cost["cost"]
        telemetry["telemetry"]
        mcp_lib["mcp"]
        sidequery["sidequery"]
        coord["coordinator"]
        wf["workflow"]
        fusion["fusion"]
    end

    subgraph Layer2["Layer 2: Abstraction"]
        platform_api["platform-api"]
        tool_api["tool-api"]
        skill_api["skill-api"]
        command_api["command-api"]
    end

    subgraph Layer1["Layer 1: Foundation"]
        proto["protocol"]
        core_crate["core"]
        branding["branding"]
    end

    Layer6 --> Layer5
    Layer5 --> Layer4
    Layer4 --> Layer3
    Layer3 --> Layer2
    Layer2 --> Layer1
```

## 2. Main Loop Flow (Streaming Path — TUI)

```mermaid
flowchart TD
    START(["User presses Enter in TUI composer"])
    START --> ENTRY["conversation.rs:5545<br/>try_run_turn_streaming()"]

    subgraph PREFLIGHT["Preflight (once per turn)"]
        SYS_PROMPT["Build system prompt<br/>effective_system_prompt()"]
        APPEND_USER["Append user message to<br/>session.history + persist JSONL"]
        HOOK["Fire UserPromptSubmit hook<br/>(Block → abort turn)"]
        WIRE_TOOLS["Build wire tool definitions<br/>build_wire_tools()"]
    end

    ENTRY --> PREFLIGHT
    PREFLIGHT --> LOOP_TOP

    subgraph PER_ITERATION["Per-iteration Loop Top"]
        DRAIN["Drain mid-turn input<br/>(ESC / Now commands)"]
        GUARDS["Guard checks:<br/>max_turns / over_budget / cancel"]
        COMPACT["Proactive compaction<br/>maybe_compact_before_call()"]
        SNAPSHOT["Snapshot session.history"]
        INJECT["Inject per-turn reminders:<br/>date, LINGXI.md, output-style,<br/>plan-mode, skill-listing,<br/>diagnostics, agent-listing,<br/>todo-reminder, memories"]
        ESTIMATE["Token estimate +<br/>blocking-limit preempt"]
    end

    LOOP_TOP --> PER_ITERATION
    PER_ITERATION --> MODEL_CALL

    subgraph MODEL["Model Call Chain"]
        ADAPTER["ProviderApiAdapter::stream()<br/>provider_adapter.rs:542"]
        SERVICE["ApiService::stream()<br/>service.rs:2610"]
        BUILD_REQ["build_request()<br/>strip_media → normalize → to_llm_messages"]
        DRIVE["drive_stream()<br/>service.rs:2284"]
        PREPARE["DefaultLlmClient::prepare()<br/>resolve route + auth + codec<br/>soft-degrade reasoning/vision"]
        RETRY{"Connect OK?"}
        SUCCESS["Open SSE/WebSocket stream<br/>record rate-limit headers"]
        RETRY_LOOP["Retry loop:<br/>exponential backoff + jitter<br/>AWS auth refresh (up to 2x)<br/>RateLimited → retry-after<br/>ContextOverflow → PTL recovery"]
    end

    ADAPTER --> SERVICE --> BUILD_REQ --> DRIVE --> PREPARE --> RETRY
    RETRY -->|No| RETRY_LOOP --> RETRY
    RETRY -->|Yes| SUCCESS

    SUCCESS --> SSE_PUMP

    subgraph PUMP["SSE Event Pump"]
        PUMP_INNER["pump_stream_inner()<br/>streaming_loop.rs:381"]
        ACCUM["BlockAccumulator tracks<br/>in-progress content blocks"]
        EVENT_LOOP{"tokio::select! loop"}
        FRAME["Read next SSE frame<br/>decoder.decode_frame(frame)"]
        DISPATCH["dispatch_event()<br/>sse/event_router.rs:77"]

        subgraph EVENTS["Event Types"]
            MSG_START["MessageStart → emit to output"]
            CBT_START["ContentBlockStart → begin block"]
            DELTA["TextDelta / ThinkingDelta<br/>→ append + emit live token"]
            CBT_STOP_TU["ContentBlockStop(tool_use)<br/>→ StreamingToolExecutor::add_tool()<br/>TOOL EXECUTES MID-STREAM"]
            CBT_STOP_TXT["ContentBlockStop(text/think)<br/>→ AppendAssistantBlock"]
            MSG_DELTA["MessageDelta<br/>→ RecordStopReason"]
            MSG_STOP["MessageStop → EndOfStream"]
        end

        TOOL_DRAIN["Poll in-flight tool futures<br/>executor.drain_one() concurrent"]

        ERROR_HANDLE{{"Stream error?"}}
        TRANSIENT["Transient + no real content<br/>→ reopen with backoff (cap 2)"]
        PARTIAL["Real content completed<br/>→ PartialFinalize + notice"]
        OVERLOADED["Overloaded + no content<br/>→ 529 non-streaming fallback"]
        PTL_413["ContextOverflow<br/>→ PTL recovery (truncate + retry)"]
    end

    PUMP_INNER --> ACCUM --> EVENT_LOOP
    EVENT_LOOP -->|event| FRAME --> DISPATCH --> EVENTS
    EVENT_LOOP -->|tool| TOOL_DRAIN
    EVENT_LOOP -->|error| ERROR_HANDLE
    ERROR_HANDLE -->|transient| TRANSIENT
    ERROR_HANDLE -->|partial| PARTIAL
    ERROR_HANDLE -->|overloaded| OVERLOADED
    ERROR_HANDLE -->|context overflow| PTL_413
    EVENTS -->|not EndOfStream| EVENT_LOOP
    EVENTS -->|EndOfStream| TOOL_DRAIN_COMPLETE

    SSE_PUMP --> TOOL_DRAIN_COMPLETE

    subgraph DRAIN["Post-Stream Tool Drain"]
        DRAIN_LOOP{"exec.inflight empty?"}
        APPLY_ABORT["apply_abort_to_pending()"]
        PROCESS_QUEUE["process_queue()"]
        COMPLETED["For each completed tool_result:<br/>persist to JSONL with UUID parentage<br/>replay injected messages (Skill)<br/>collect ContextModifiers"]
        WAIT["exec.drain_one().await"]
    end

    TOOL_DRAIN_COMPLETE --> DRAIN
    DRAIN --> POST_PROCESS

    subgraph POST["Post-Process + Disposition"]
        COST_TRACK["Record cost via CostTracker<br/>(wall-clock, retries, cache tokens)"]
        PERSIST["Assemble + persist assistant<br/>ConversationMessage to history<br/>(per-block JSONL lines)"]
        DISPOSITION{"stop_reason?"}
        TOOL_USES["tool_uses → continue loop"]
        END_TURN["end_turn → Stop hooks<br/>→ emit_end_turn → break"]
        MAX_TOKENS["max_tokens → recovery nudge<br/>→ continue (up to 3x)"]
        MALFORMED["malformed tool_use<br/>→ nudge once → continue or break"]
        REFUSAL["refusal + fallback<br/>→ swap model → continue"]
    end

    POST_PROCESS --> DISPOSITION
    DISPOSITION -->|tool_uses| TOOL_USES --> LOOP_TOP
    DISPOSITION -->|end_turn| END_TURN --> DONE(["Turn Complete"])
    DISPOSITION -->|max_tokens| MAX_TOKENS --> LOOP_TOP
    DISPOSITION -->|malformed| MALFORMED
    DISPOSITION -->|refusal| REFUSAL --> MODEL_CALL
```

## 3. Provider / Model Resolution Chain

```mermaid
flowchart TD
    REQ["LlmRequest { model, profile, messages, tools, ... }"]
    REQ --> RESOLVE["ModelRegistry::resolve_in()<br/>find route matching model alias"]

    RESOLVE --> CAP_CHECK{{"Capabilities check"}}
    CAP_CHECK -->|streaming false<br/>but request.stream| ERR["UnsupportedCapability"]
    CAP_CHECK -->|tools false<br/>but request.tools| ERR

    subgraph SOFT_DEGRADE["Soft Degrade (prepare_at)"]
        REASONING{{"reasoning false +<br/>request.reasoning or<br/>history has Reasoning blocks?"}}
        VISION{{"vision false +<br/>history has Image/ImageUrl blocks?"}}
        STRIP_R["Strip reasoning from<br/>request + message history"]
        STRIP_V["Strip Image/ImageUrl from<br/>message history (silently)"]
    end

    CAP_CHECK --> REASONING
    REASONING -->|yes| STRIP_R --> VALIDATE
    REASONING -->|no| VISION
    VISION -->|yes| STRIP_V --> VALIDATE
    VISION -->|no| VALIDATE

    VALIDATE["validate_capabilities()<br/>streaming / tools / reasoning<br/>structured_output / vision / documents"]

    VALIDATE -->|fail| ERR
    VALIDATE -->|pass| CODEC

    CODEC["codec.encode_request()<br/>AnthropicMessages / OpenAiChat /<br/>Gemini / OpenAiResponses /<br/>AzureOpenAi / VertexClaude /<br/>BedrockClaude"]

    CODEC --> AUTH["authenticate()<br/>None / ApiKey / Bearer /<br/>OAuthBearer / AwsSigV4 /<br/>AzureToken / GcpToken /<br/>ChatGptOAuth / CopilotBearer"]

    AUTH --> INJECT["inject_stream_headers()<br/>request-id, betas, User-Agent,<br/>CLAUDE_CODE_EXTRA_BODY"]

    INJECT --> TRANSPORT(["ProviderRequest → HTTP/WebSocket"])
```

## 4. Compaction Pipeline

```mermaid
flowchart TD
    TRIGGER{"Trigger?"}
    TRIGGER -->|Manual /compact| MANUAL["force_compact_with_cancel()<br/>conversation.rs:2334"]
    TRIGGER -->|Auto threshold| AUTO["maybe_compact_before_call()<br/>conversation.rs:2943"]
    TRIGGER -->|Reactive PTL| REACTIVE["call_api_with_ptl_recovery()<br/>turn_loop.rs:1105"]

    MANUAL --> EMIT_START["emit_compaction_started()"]
    AUTO --> SHOW["emit_compaction_started()<br/>→ TUI shows spinner"]

    EMIT_START --> PRE_HOOK["Fire PreCompact hook<br/>(Block → abort)"]
    SHOW --> PRE_HOOK
    REACTIVE --> PRE_HOOK

    subgraph LAYERS["Three-Layer Pipeline"]
        SNIP{"LINGXI_HISTORY_SNIP?"}
        SNIP_ON["Snip: drop oldest messages<br/>target autocompact threshold<br/>(cheap, no LLM)"]
        SNIP -->|off| MICRO
        SNIP -->|on| SNIP_ON --> MICRO

        MICRO{"microcompact enabled?"}
        MICRO_ON["Microcompact: clear stale<br/>large tool results<br/>(Read/Grep/Glob output → placeholder)<br/>must save ≥ 20,000 tokens"]
        MICRO -->|off| AUTO_CHECK
        MICRO -->|on| MICRO_ON --> AUTO_CHECK

        AUTO_CHECK{{"over threshold +<br/>breaker not tripped?"}}
        AUTO_COMPACT["Autocompact: LLM summarization<br/>via ForkedAgentRunner<br/>(shares parent prompt cache)<br/><br/>1. strip_images_from_messages()<br/>2. select_preserved_tail()<br/>3. build compact prompt + fork agent<br/>4. PTL retry loop (up to 3x)<br/>5. format_compact_summary()<br/>6. get_compact_user_summary_message()"]
        RAPID_REFILL["Rapid-refill breaker tripped<br/>(3+ refills within 3 turns)<br/>→ SKIP, surface thrashing msg"]
        CIRCUIT["Circuit breaker tripped<br/>(3 consecutive failures)<br/>→ SKIP"]
    end

    PRE_HOOK --> LAYERS

    AUTO_CHECK -->|breaker tripped| CIRCUIT --> APPLY
    AUTO_CHECK -->|rapid refill| RAPID_REFILL --> APPLY
    AUTO_CHECK -->|yes| AUTO_COMPACT --> APPLY
    AUTO_CHECK -->|no| APPLY

    subgraph APPLY_POST["apply_post_compact()"]
        BOUNDARY["Create boundary marker<br/>'Conversation compacted'<br/>+ compactMetadata"]
        ASSEMBLE["Assemble post-compact history:<br/>[boundary, summary, preserved_tail,<br/>restored_files, restored_skills]"]
        SWAP["Swap session.history"]
        PERSIST_BC["Persist boundary + summary to JSONL<br/>(chain reset: parentUuid=null)"]
        EMIT_COMPLETE["emit_compaction_completed()<br/>→ TUI hides spinner"]
    end

    APPLY --> POST_HOOK["Fire PostCompact hook<br/>(best-effort, never fails)"]
    APPLY_POST --> POST_HOOK
```

## 5. Tool Execution Flow

```mermaid
flowchart TD
    TOOL_USE(["ContentBlockStop(tool_use)"])
    TOOL_USE --> REGISTER["StreamingToolExecutor::add_tool()<br/>streaming_executor.rs"]
    REGISTER --> PROCESS["process_queue() — start immediately<br/>(mid-stream, while SSE still flowing)"]

    subgraph EXEC["Tool Execution Pipeline"]
        PRE_HOOK["Fire PreToolUse hook"]
        PERM{"PermissionGate::check()"}
        PERM_ALLOW["Mode default/acceptEdits:<br/>execute"]
        PERM_ASK["Mode default + ask:<br/>prompt user"]
        PERM_DENY["Mode plan / rule deny:<br/>return synthetic error result"]
        DISPATCH["ToolRegistry::dispatch()<br/>invoke concrete tool impl"]
        POST_HOOK["Fire PostToolUse hook<br/>collect additionalContext /<br/>injectedMessages / preventContinuation"]
    end

    PROCESS --> PRE_HOOK --> PERM
    PERM -->|allow| PERM_ALLOW --> DISPATCH --> POST_HOOK --> RESULT(["ToolResult + ContextModifier"])
    PERM -->|ask| PERM_ASK --> RESULT2(["User decision → Result"])
    PERM -->|deny| PERM_DENY --> RESULT3(["Synthetic error result"])

    RESULT --> PERSIST_TR["Persist as JSONL User message<br/>parentUuid = tool_use line UUID"]
    PERSIST_TR --> COLLECT["Collect ContextModifiers<br/>(model override, injected msgs)"]

    COLLECT --> DRAIN_LOOP{"More in-flight tools?"}
    DRAIN_LOOP -->|yes| DRAIN_ONE["exec.drain_one().await<br/>wait for next completion"]
    DRAIN_ONE --> PROCESS
    DRAIN_LOOP -->|no| APPLY_MODS["apply_model_context_modifiers()"]
    APPLY_MODS --> NEXT_TURN(["Continue loop — feed<br/>tool_results back to model"])
```

## 6. Data Type Relationships

```mermaid
flowchart LR
    subgraph PROTO["protocol (shared DTOs)"]
        ConvMsg["ConversationMessage<br/>{User, Assistant, System}"]
        CB["ContentBlock<br/>{Text, ToolUse, ToolResult,<br/>Thinking, Image, Document,<br/>RedactedThinking, ServerToolUse}"]
        ImgSrc["ImageSource<br/>{Base64, Url}"]
    end

    subgraph LLM["llm-runtime (provider types)"]
        LlmReq["LlmRequest<br/>{model, messages, tools,<br/>stream, reasoning,<br/>response_format, ...}"]
        LlmMsg["Message<br/>{role, content: Vec<ContentBlock>}"]
        LlmCB["ContentBlock<br/>{Text, Image, ImageUrl,<br/>Document, ToolCall, ToolResult,<br/>Reasoning, RedactedThinking}"]
        LlmEvt["LlmEvent<br/>{MessageStart, ContentBlockStart,<br/>TextDelta, ContentBlockStop,<br/>MessageDelta, MessageStop}"]
        LlmResp["LlmResponse<br/>{id, content, stop_reason, usage}"]
    end

    subgraph BRIDGE["convert.rs"]
        TO_LLM["to_llm_messages()"]
        NORM["normalize_messages_for_api()"]
        PAIR["ensure_tool_result_pairing()"]
    end

    ConvMsg -->|"SessionState: history<br/>(append-only JSONL)"| ConvMsg
    ConvMsg -->|"snapshot + preprocess"| NORM --> PAIR --> TO_LLM --> LlmMsg
    LlmMsg -->|"encoded by<br/>provider codec"| LlmReq
    LlmReq -->|"API call"| LlmEvt
    LlmEvt -->|"non-streaming"| LlmResp
```

## 7. Session Persistence (JSONL)

```mermaid
flowchart TD
    subgraph WRITE["Write Path"]
        TURN["Each turn: user + assistant msgs"]
        TURN --> PERSIST["JsonlWriter::append()<br/>fsync + flock <br/>per-content-block lines<br/>each with UUID"]
        PERSIST --> CHAIN["Chain linking:<br/>parentUuid points to<br/>previous line's uuid"]
        CHAIN --> COMPACT_BOUNDARY{{"Compaction?"}}
        COMPACT_BOUNDARY -->|yes| BOUNDARY_LINE["Write boundary line:<br/>parentUuid=null (chain reset)<br/>logicalParentUuid=prev<br/>subtype=compact_boundary"]
        COMPACT_BOUNDARY -->|no| NEXT_LINE["Next line chains normally"]
    end

    subgraph READ["Read Path (Resume)"]
        LOAD["JsonlReader::read_all()<br/>walk parentUuid chain<br/>stop at parentUuid=null"]
        LOAD --> FIND_BC["findLastCompactBoundaryIndex()"]
        FIND_BC --> REBUILD["Rebuild in-memory history:<br/>[boundary, ...summary, preserved_tail]"]
        REBUILD --> RESTORE_FILES["Restore post-compact files<br/>from boundary metadata"]
    end

    PERSIST -->|on disk| LOAD
```

## 8. Key File Map

| File | Purpose |
|---|---|
| `orchestrator/src/conversation.rs:5545` | `try_run_turn_streaming` — main streaming turn loop |
| `orchestrator/src/conversation.rs:2334` | `force_compact_with_cancel` — manual /compact handler |
| `orchestrator/src/conversation.rs:2592` | `apply_post_compact` — post-compact history swap |
| `orchestrator/src/conversation.rs:2943` | `maybe_compact_before_call` — proactive auto-compact |
| `orchestrator/src/turn_loop.rs:1105` | `call_api_with_ptl_recovery` — reactive 413 recovery |
| `orchestrator/src/streaming_loop.rs:381` | `pump_stream_inner` — SSE event pump core |
| `orchestrator/src/sse/event_router.rs:77` | `dispatch_event` — SSE event → RouterAction |
| `orchestrator/src/streaming_executor.rs` | `StreamingToolExecutor` — mid-stream tool execution |
| `orchestrator/src/provider_adapter.rs:542` | `ProviderApiAdapter::stream` — main-loop adapter |
| `llm-runtime/src/service.rs:2284` | `ApiService::drive_stream` — retry loop + connect |
| `llm-runtime/src/service.rs:809` | `ApiService::build_request` — message preprocessing |
| `llm-runtime/src/client.rs:283` | `DefaultLlmClient::prepare_at` — resolve + validate + degrade |
| `llm-runtime/src/protocol.rs:838` | `validate_capabilities` — capability enforcement |
| `llm-runtime/src/convert.rs:41` | `to_llm_messages` — protocol → llm-runtime bridge |
| `compaction/src/orchestrator.rs:185` | `process_iteration_tracked` — snip/micro/auto pipeline |
| `compaction/src/autocompact.rs:162` | `Autocompactor::compact` — LLM summarization |
| `compaction/src/boundary.rs:157` | `create_compact_boundary` — boundary marker |
| `tui/src/files.rs:29` | `file_completions` — @file autocomplete |
| `tui/src/composer.rs:230` | `at_fragment` — detect @ token in composer |
| `session/src/jsonl/writer.rs` | `JsonlWriter` — append-only JSONL persistence |
