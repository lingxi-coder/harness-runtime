# LingXi Code 对 Claude Code 2.1.220 的全局真实性复核报告

> 复核日期：2026-07-26
>
> LingXi Code 基线：`80dc7a6a6`
>
> Claude Code 本机版本：`2.1.220`
>
> 本机二进制：`/Users/luolingfeng/.local/share/claude/versions/2.1.220`
>
> 本机二进制 SHA-256：`8addc857f3fe64d5a0368af9ee50321b50afb4a6918ba3ef018ab84f5dbbe081`

## 1. 执行摘要

本轮重新检查了 orchestrator、turn loop、conversation、agent/subagent、workflow、sandbox、hooks、tools、MCP、memory、compact、tasks、LSP、file、web、CLI/TUI、background session、PTY、恢复和 telemetry。

结论：

- 之前修复的 PTY/background、权限解析、文件写入安全、compact 主链、fallback model、heartbeat 等大项没有在本轮重新打开。
- 仍确认 **8 组 High gap**、**13 组 Medium gap** 和 **3 组 Partial/实现债**。
- 本轮最重要的新增结论，是之前漏审了 Claude 5 generation model 的 **lean system-prompt 路径**。LingXi 仍无主 prompt 的按模型裁剪链路，因此没有实现 Anthropic 所说的“对新模型删除超过 80% system prompt”。
- Chrome 和 Anthropic remote-control 继续作为明确的私有协议边界处理：兼容公开命令面、明确 fail-fast，不宣称 1:1 功能 parity。
- Windows ConPTY、Linux PTY 和 iTerm2 resize 的源码/自动化结论不能替代当前提交在对应真实环境上的重新 smoke；这些列为验证债，不冒充功能 gap。

本报告中的 parity 仅指可通过官方 release note、官方公开文档、本机 Claude Code 2.1.220 黑盒/二进制和 LingXi 源码交叉验证的外部行为、协议、安全与状态语义。Claude Code 是闭源产品，因此不能对未知私有实现声称内部结构上的 1:1。

## 2. Oracle、范围与排除项

### 2.1 Oracle

- [Claude Code 官方 release feed](https://github.com/anthropics/claude-code/blob/main/feed.xml)
- [Claude Code 官方 changelog](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md)
- [Claude Code 官方 MCP 文档](https://code.claude.com/docs/en/mcp)
- [Claude Code 官方 settings 文档](https://code.claude.com/docs/en/settings)
- [Anthropic：The new rules of context engineering for Claude 5 generation models](https://claude.com/blog/the-new-rules-of-context-engineering-for-claude-5-generation-models)
- 本机 Claude Code `2.1.220` arm64 Mach-O
- 第三方 prompt extraction 仅用作交叉验证，不作为独立官方事实来源：
  [Piebald-AI v2.1.220 prompt tracker](https://github.com/Piebald-AI/claude-code-system-prompts/releases/tag/v2.1.220)

### 2.2 版本时间点澄清

Claude Code 2.1.220 的公开 release note 只有 “Bug fixes and reliability improvements”。Opus 5 和相关公开能力在 2.1.219 release note 中出现；Anthropic 关于“删除超过 80% system prompt”的文章发布于 2026-07-24，明确点名 Opus 5、Fable 5 等新模型。

本机 2.1.220 二进制仍包含并启用以下 runtime capability/path：

- `lean_prompt`
- `opus_5_prompt_bundle`
- `fable_5_mitigations`
- `CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT`
- model-conditional lean/full system-prompt selector

第三方逐版本 prompt extraction 显示 2.1.219 → 2.1.220 没有新的 prompt 文本 diff。因此应把该能力理解为 **2.1.220 中仍然生效、随新模型能力选择的 runtime path**，而不是 2.1.220 单独删除一批静态文件。

### 2.3 明确排除

下列差异不计入 parity gap：

- LingXi 的 multiple LLM provider
- LingXi 自定义目录、crate/module 切分和品牌文案
- bridge/mobile 等项目特定能力
- Claude Code 没有公开合约的 Chrome 扩展协议
- Claude Code 没有公开合约的 Anthropic remote-control relay/auth 协议

项目特定能力不得破坏共同语义。例如，多 provider 本身不是 gap，但不能因为支持多 provider 就把未知 GPT/DeepSeek/Gemini 模型错误归类为 Claude lean-prompt 模型。

## 3. 状态总览

| ID | 严重度 | Gap | 状态 |
|---|---|---|---|
| H1 | High | Claude 5 lean main system prompt 缺失 | Confirmed |
| H2 | High | Opus 5 模型目录、能力和默认 Opus 路径缺失 | Confirmed |
| H3 | High | `context: fork` skill 没有真实 fork/background 执行链 | Confirmed |
| H4 | High | `/code-review` 仍是 inline prompt，不是 background subagent | Confirmed |
| H5 | High | `sandbox.network.strictAllowlist` 缺失 | Confirmed |
| H6 | High | `DirectoryAdded` hook 和 `register_repo_root` control request 缺失 | Confirmed |
| H7 | High | subagent 默认嵌套深度仍为 1，不是 3 | Confirmed |
| H8 | High | stream-json init 缺 `mcp_server_errors` 和 terminal startup warning | Confirmed |
| M1 | Medium | 主 prompt 与工具 prompt 使用不同、过时的模型 gate | Confirmed |
| M2 | Medium | system-prompt static/dynamic global-cache boundary 未激活 | Confirmed |
| M3 | Medium | dynamic workflow 默认/配置/status 仍是旧语义 | Confirmed |
| M4 | Medium | managed `disableWorkflows` 未真正消费 | Confirmed |
| M5 | Medium | MCP list/get 可观察状态与诊断不完整 | Confirmed |
| M6 | Medium | managed MCP allow/deny 的环境变量来源不完整 | Confirmed |
| M7 | Medium | agent frontmatter `mcpServers` 无法进入 live tool registry | Confirmed |
| M8 | Medium | `auth login` 浏览器失败后的手动 fallback 不完整 | Confirmed |
| M9 | Medium | agent 名称仍允许 `:`，与 plugin namespace 冲突 | Confirmed |
| M10 | Medium | 对外 parity 版本仍标记为 2.1.217 | Confirmed |
| M11 | Medium | Windows `CLAUDE_CODE_GIT_BASH_PATH` 校验/忽略-with-warning 缺失 | Confirmed |
| M12 | Medium | agent-view 左箭头进入/返回/确认状态机不完整 | Confirmed |
| M13 | Medium | OAuth 来源和 enterprise 订阅态仍有 partial seam | Confirmed/Partial |
| P1 | Partial | OpenTelemetry record-site 覆盖不完整 | Partial |
| P2 | Partial | micro-compact 无真实消息时间戳 idle-gap 判定 | Partial |
| P3 | Partial | `MEMORY.md` near-cap advisory 未接入真实 write hook | Partial |

## 4. High gaps

### H1. Claude 5 lean main system prompt 缺失

**结论：Confirmed / High**

Anthropic 官方说明，对 Opus 5、Fable 5 等新模型删除了超过 80% 的 Claude Code system prompt，且编码评测没有可测损失。2.1.220 二进制的实际实现不是全局删除，而是按模型 capability 在 full prompt 和 lean prompt 之间选择。

2.1.220 lean 静态 prompt 主要保留：

- agent 身份与授权安全边界
- `# Harness`
- Markdown 输出
- permission denial 后调整而不是原样重试
- hooks 反馈
- 优先使用专用 file/search tools
- 独立 tool calls 可并行
- `file_path:line_number` 引用格式

LingXi 的主 assembler 仍按 Claude Code 2.1.183 的完整模板锁定：

- `orchestrator/src/prompt/mod.rs:84-100`
- `orchestrator/src/prompt/mod.rs:108-143`
- `orchestrator/src/prompt/body_sections.rs:1-33`
- `orchestrator/src/prompt/body_sections.rs:387-428`

`ConversationOrchestrator::build_system_prompt` 每轮无条件调用完整 assembler：

- `orchestrator/src/conversation.rs:9191-9208`

仓库中没有主 prompt 的 `# Harness` 实现，也没有 model → `lean_prompt` capability → lean assembler 的 composition path。

影响：

- 新模型继续承受 Anthropic 已移除的过度约束。
- 旧 `# Doing tasks`、`# Executing actions with care`、`# Using your tools`、`# Tone and style` 等 section 持续占用 context。
- prompt cache 即使命中，也不能消除 context-window 占用和 cache-read 成本。
- LingXi 对 Claude 5 generation models 的行为、成本和上下文工程不具备 2.1.220 parity。
- forked subagent 若继承父 prompt，也会继承这个过大的旧 prompt。

### H2. Opus 5 模型目录、能力和默认 Opus 路径缺失

**结论：Confirmed / High**

Claude Code 2.1.219 已把 `claude-opus-5` 加为默认 Opus，支持 1M context 和 fast mode。LingXi 当前：

- `orchestrator/src/config.rs:21-25`：默认仍是 `claude-opus-4-8`
- `llm-runtime/src/provider_settings.rs:718-734`：模型表没有 `claude-opus-5`
- `apps/cli/src/run.rs:1462-1479`：capability 分支没有 Opus 5
- `orchestrator/src/prompt/env_meta.rs:29-45`：marketing name 没有 Opus 5
- `orchestrator/src/prompt/env_meta.rs:118-138`：knowledge cutoff 没有 Opus 5
- `orchestrator/src/prompt/env_block.rs:49-57`：latest Opus 常量仍是 4.8
- `orchestrator/src/prompt/env_block.rs:149-174`：模型目录和 fast-mode 文案仍是 Opus 4.8/4.7

缺失不仅影响 picker，还影响 pricing、fast mode、effort、context window、fallback、beta/capability 和 prompt-bundle 选择。

### H3. `context: fork` skill 没有真实执行链

**结论：Confirmed / High**

Claude Code 2.1.218 将 `context: fork` skill 默认改为 background 执行，并允许 `background: false` opt-out。

LingXi 能解析部分 metadata，但 `Skill` tool 源码明确说明 forked-agent execution 没有 Rust substrate：

- `tools/skill/src/skill.rs:21-30`
- `tools/skill/src/skill.rs:710-723`

当前结果只是 inline resolution/metadata surfacing，不具备真实 fork、background、progress、resume 和独立 transcript 语义。

### H4. `/code-review` 仍为 inline prompt

**结论：Confirmed / High**

Claude Code 2.1.218 将 `/code-review` 改为 background subagent，避免 review 内容填满主对话，并保留 stacked slash command 作为 review target。

LingXi 仍把 `/code-review` 注册为 bundled `prompt_fn`：

- `commands/core/src/bundled/mod.rs:65-78`
- `commands/core/src/bundled/code_review_skill.rs:18-31`
- `commands/core/src/bundled/code_review_skill.rs:144-160`

因此主 conversation 污染、后台生命周期、stacked command target 和 review agent isolation 均未对齐。

### H5. `sandbox.network.strictAllowlist` 缺失

**结论：Confirmed / High**

Claude Code 2.1.219 新增 `sandbox.network.strictAllowlist`：sandboxed command 访问未列入 allowlist 的 host 时直接拒绝，而不是提示。

LingXi 当前 network restriction 只有 `allowed_domains`、`denied_domains`、`allow_managed_domains_only`、socket/local-binding 等旧字段：

- `sandbox/src/runtime_config.rs:82-120`
- `core/src/settings/schema.rs:144-150`

没有 `strictAllowlist` 的 settings schema、merge、managed precedence 和 macOS/Linux/Windows runtime enforcement。

### H6. `DirectoryAdded` hook 和 `register_repo_root` 缺失

**结论：Confirmed / High**

Claude Code 2.1.219 新增 `DirectoryAdded` hook，在 `/add-dir` 或 SDK `register_repo_root` control request 中途添加工作目录后触发。

LingXi hook enum 和 settings-name parser 均没有该事件：

- `hooks/src/events.rs:17-84`
- `hooks/src/loader.rs:470-514`

现有 `/add-dir` 只完成本地 trusted/additional-directory 更新，没有完整的 SDK control request → live root registration → MCP roots/list_changed → hook payload 链。

### H7. subagent 默认嵌套深度仍是 1

**结论：Confirmed / High**

Claude Code 2.1.219 把默认嵌套深度从 1 提升到 3；设置 `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH=1` 才恢复旧行为。

LingXi 仍写死：

- `platform-api/src/subagent_spawn.rs:470-482`
- `platform-api/src/subagent_spawn.rs:495-502`
- `agent/src/tool_resolver.rs:21-22`

这会让已经实现的 depth-2+ stream-json forwarding 在默认配置下无法真正发生。

### H8. stream-json init 缺 `mcp_server_errors`

**结论：Confirmed / High**

Claude Code 2.1.219 的 headless `system/init` 增加 `mcp_server_errors`，列出因配置校验而跳过的 `--mcp-config` 项；terminal run 还会打印 startup warning。

LingXi：

- `apps/cli/src/stream_json.rs:281-301`：init params 无该字段
- `apps/cli/src/stream_json.rs:1352-1388`：仍锁定旧 20-key frame

`mcp::config_diagnostics` 已能生成部分 diagnostics，但没有进入这个公开 frame，也没有形成与 Claude 相同的 terminal startup warning surface。

## 5. Medium gaps

### M1. 主 prompt 与工具 prompt 使用不同、过时的模型 gate

**结论：Confirmed / Medium**

`tool-api/src/model_prompt_gate.rs:107-121` 已实现一个旧版 `dh_simple_system_prompt`，并被 Read/Write/Edit/Glob/Grep/Bash/TodoWrite/WebSearch/WebFetch 使用。但：

- 主 system prompt 完全不调用它。
- 它依据模型名字猜测，而不是统一的 provider/model capability registry。
- `claude-opus-5` 没有显式 capability entry，只会因 unknown-model fallthrough 偶然得到 SHORT。
- provider class 没有进入该层，未知非 Claude 模型可能被错误归为 SHORT。
- `LINGXI_SIMPLE_SYSTEM_PROMPT` 只影响部分 tool prompt，不能控制主 prompt。

正确语义应是同一个 resolved capability snapshot 同时驱动 main system prompt、tool descriptions、dynamic prompt bundles 和缓存 key。

### M2. system-prompt global-cache boundary 未激活

**结论：Confirmed / Medium**

`llm-runtime/src/prompt_format.rs:29-36` 明确记录：`SYSTEM_PROMPT_DYNAMIC_BOUNDARY` 虽已实现，但 LingXi assembler 从未输出它。因此 global static/dynamic cache path 总是退回 org default。

影响：

- static lean/full bundle 与动态 cwd/git/env/memory 状态没有正确分桶。
- model switch、output style 和 dynamic section 变更的 cache behavior 不是 2.1.220 语义。
- 当前测试只锁旧 full body 顺序，没有 lean/full × model × cache-boundary golden matrix。

### M3. dynamic workflow 默认/配置/status 仍是旧语义

**结论：Confirmed / Medium**

Claude Code 2.1.219：

- 默认 `medium`，建议少于 15 agents
- `workflowSizeGuideline` 可来自任意 settings file
- settings file 控制时隐藏 `/config` 行
- running workflow status line 显示当前 guideline，并提示 `/config`

LingXi 当前：

- `tools/workflow/src/lib.rs:433-452` 默认 `Unrestricted`
- `tools/workflow/src/size_guideline.rs:35-64` 文档和 fallback 都仍把 `Unrestricted` 当默认
- `tui/src/bottom_pane/screen_view.rs:599-609` 缺省仍显示 unrestricted
- `tui/src/status_line.rs` 没有 workflow-size payload

### M4. managed `disableWorkflows` 未真正消费

**结论：Confirmed / Medium**

`tools/workflow/src/lib.rs:520-537` 和 `:613-630` 明确说明只实现了环境变量分支；managed settings 没有进入 `ToolStaticContext`/validation。

结果是组织策略设置 `disableWorkflows: true` 时，Workflow 工具仍可能被广告并执行。

### M5. MCP list/get 可观察状态和诊断不完整

**结论：Confirmed / Medium**

Claude Code 2.1.219 增加了 MCP list 和 `/mcp` 的 HTTP status、error text、隐藏首尾空白 warning；当前官方 MCP 行为还包括 missing-env warning、空 URL 的 `not configured` placeholder 和 rejected project server 的明确状态。

LingXi 当前：

- `apps/cli/src/commands/mcp.rs:1828-1848` 明确不做 approved-server network probe
- `mcp/src/json_config.rs:245-252` 遇到空 URL 直接跳过
- `apps/cli/src/commands/mcp.rs:1988-2054` 把 rejected 和未批准状态折叠，不能稳定显示 rejected 原因
- missing-env diagnostics 主要进入 tracing/doctor/startup 局部路径，没有进入 `mcp list` 的逐 server 输出

### M6. managed MCP allow/deny 的环境变量来源不完整

**结论：Confirmed / Medium**

Claude Code 2.1.219 将 managed MCP allowlist/denylist 中 `${VAR}` 的来源改为 startup environment 和 managed-settings env，而不是 settings-file env。

LingXi `mcp/src/enterprise_policy.rs:188-191` 仍直接使用通用 process-env expansion；`mcp/src/env_expansion.rs:32-45` 只有 process environment lookup，没有 startup snapshot / managed-env composition。

### M7. agent frontmatter `mcpServers` 无法进入 live tool registry

**结论：Confirmed / Medium**

`apps/engine-desktop/src/lib.rs:7654-7660` 明确记录 residual seam：agent catalog 完成时 ToolRegistry 已被 `Arc` 封口，frontmatter MCP 无法让工具进入模型可见集合。

解析字段存在不等于执行语义存在；当前 agent-specific MCP scope 不能宣称 closed。

### M8. `auth login` 浏览器失败后的手动 fallback 不完整

**结论：Confirmed / Medium**

OAuth 底层注释称浏览器打开失败时 CLI 可打印 URL fallback：

- `llm-runtime/src/oauth/anthropic/handle.rs:310-325`

但顶层命令只打印 “Opening browser to sign in…” 并把 OAuth error 直接变成 login failure：

- `apps/cli/src/commands/auth.rs:142-164`

缺少完整的 URL copy、手动 callback/code 输入和继续等待状态机。

### M9. agent 名称仍允许 `:`

**结论：Confirmed / Medium**

Claude Code 2.1.218 拒绝 agent markdown 中包含 `:` 的名称，因为它保留给 plugin namespace。

LingXi `agent/src/catalog.rs:163-174` 只检查非空；另一方面 `apps/engine-desktop/src/agent_skill_loader.rs:51-64` 已把 `:` 当 plugin prefix delimiter。二者冲突会导致名称解析和 skill namespace 歧义。

### M10. 对外 parity 版本仍标记为 2.1.217

**结论：Confirmed / Medium**

`platform-api/src/lib.rs:15-21` 的 `CLAUDE_CODE_VERSION` 仍为 `2.1.217`，并进入 `AI_AGENT` 和 WebFetch `User-Agent` 等外部可见标识。

在 2.1.218–2.1.220 行为尚未修完前，不应仅机械改字符串并宣称 parity；但当前状态同样不能表示为已对齐 2.1.220。

### M11. Windows `CLAUDE_CODE_GIT_BASH_PATH` 校验缺失

**结论：Confirmed / Medium**

Claude Code 2.1.219 会验证该路径确实是 bash/sh；无效时忽略并 warning。

LingXi 仓库对 `CLAUDE_CODE_GIT_BASH_PATH` 没有消费链；`tools/shell/src/bash.rs:259-281` 只处理 `LINGXI_SHELL`，且主要依据字符串包含 `bash`/`zsh`。

### M12. agent-view 左箭头状态机不完整

**结论：Confirmed / Medium**

Claude Code 2.1.218/2.1.219 修复了：

- 编辑后按左箭头不再无确认丢弃 conversation
- Esc 从 agent view 回到被 background 的 conversation
- NORMAL/INSERT mode 的空 composer 左箭头一致进入 agent view

LingXi `tui/src/bottom_pane/mod.rs:1303-1317` 的 Left 分支只移动 composer cursor，没有对应的进入/确认/恢复状态机。

### M13. OAuth 来源和 enterprise 订阅态仍有 partial seam

**结论：Confirmed/Partial / Medium**

源码仍明确记录：

- `apps/engine-desktop/src/lib.rs:2983-2984`：FD-inherited key 和 managed-context OAuth forcing 未进入 `DesktopConfig`
- `apps/engine-desktop/src/lib.rs:4913-4919`：build hot path 的 enterprise state 先固定为 false
- `llm-runtime/src/service.rs:835-849`：后续通过 shared snapshot 部分补偿

这不是“完全没实现”，但最早请求和特殊认证来源仍可能与 2.1.220 不一致。

## 6. Partial/实现债

### P1. OpenTelemetry record-site 覆盖不完整

exporter/runtime 基础层已存在，不能再把 telemetry 描述为 stub；但：

- `telemetry/src/otel/mod.rs:45-49` 明确记录 app-code record-site coverage 仍窄
- `telemetry/src/otel/record.rs:22-40` 仍列出大量未接入 sites

因此状态应为 **Partial**，而不是旧报告中的完整 closed。

### P2. micro-compact 无真实 idle-gap 判定

`compaction/src/orchestrator.rs:289-308` 明确说明 `ConversationMessage` 没有 per-message timestamp；启用 micro-compact 后只根据 `keep_recent` 近似执行，不能复刻真实 since-last-assistant idle gap。

### P3. `MEMORY.md` near-cap advisory 未接入 write hook

`memory/src/index_cap.rs:27-34` 已有 byte-faithful notice generator，但真正的 Write/Edit `PostToolUse` activation 尚未进入 engine composition root。

## 7. 已确认关闭或本轮不重新打开的旧项

以下旧 gap 在当前源码中已有真实实现或回归覆盖，本报告不重新计为 open：

- background worker 使用真实 PTY/ConPTY substrate
- attach v2 原始输入、resize、detach/reattach、单 controller
- background resume/fork launch spec 和运行时 metadata
- worktree owner marker/token 和安全删除
- confined atomic write 与 symlink/reparse-point 防护
- MCP serve/login/logout/desktop import 不再是 stub
- `/goal` enforcement
- `sandbox.filesystem.disabled`
- AskUserQuestion `Other` 自由文本
- `--betas`、`--plugin-url`、`--file`
- fallback model 有序列表
- agent snapshot resume
- ToolHeartbeat 的 TUI/plain/stream-json 消费
- mid-stream API error 后保留已经生成的 print answer
- stream-json `update_environment_variables` 双 key allowlist
- Windows `\u` 路径 repair guard

这些结论不代表整个子系统已经达到 2.1.220 parity；只表示对应旧 finding 不应原样重复报告。

## 8. 私有协议和验证边界

### 8.1 Honest boundaries

- Chrome：公开命令面可兼容；缺少 Anthropic 私有扩展协议时明确非零 fail-fast。
- Remote Control：公开命令面可兼容；缺少 Anthropic 私有 relay/auth 合约时明确非零 fail-fast。

不得将 LingXi 自己的 bridge 冒充 Claude remote-control parity。

### 8.2 目标平台验证债

- Windows ConPTY/Job Object：需要在 Windows kernel 上运行 runtime tests。
- Linux PTY/process group：需要在对应当前 commit 的 Linux runner 上重新执行。
- macOS iTerm2：需要人工拖动窗口和 attach/reattach，确认 footer/editor 无残影。

交叉编译、macOS host 模拟或旧提交的 smoke 不能替代当前提交的目标平台证据。

## 9. 修复顺序建议

### Phase 1：prompt/model 核心

1. 建立统一的 provider-aware model capability registry。
2. 接入 Opus 5 全模型栈和默认 Opus alias。
3. 实现 lean/full main prompt selector。
4. 移植 `# Harness` 和 lean dynamic overlays。
5. 让 main prompt、tool prompts、subagent fork、cache key 共用同一 capability snapshot。
6. 增加 lean/full × model × provider × model-switch golden matrix。

### Phase 2：安全、hook、agent 执行

1. `sandbox.network.strictAllowlist`
2. `DirectoryAdded` + `register_repo_root` + MCP roots changed
3. subagent default depth 3
4. `context: fork` skill execution
5. `/code-review` background subagent
6. agent `mcpServers` mutable/early registry composition

### Phase 3：workflow、MCP、auth、TUI

1. workflow 默认 medium、config precedence、row hiding、status line
2. managed `disableWorkflows`
3. `mcp_server_errors`
4. MCP list/get health、errors、missing-env、placeholder/rejected state
5. managed MCP startup/managed env
6. OAuth manual fallback
7. agent-name colon validation
8. Windows Git Bash validation
9. agent-view navigation/recovery

### Phase 4：实现债和目标平台验证

1. OTEL record-site coverage
2. timestamp-backed micro-compact
3. memory near-cap PostToolUse activation
4. Windows/Linux/macOS manual/runtime gates

只有完成前述可观察行为后，才应把 `CLAUDE_CODE_VERSION` 提升为 `2.1.220`。

## 10. 本轮验证

执行并通过：

- `cargo test -p tool-api model_prompt_gate --lib`
  - 5 passed
- `cargo test -p orchestrator prompt::body_sections::tests::full_body_order_locked --lib`
  - 1 passed

这两个测试同时揭示了测试缺口：工具层 SHORT/LONG gate 有测试，主 prompt 只有旧 full-body order test，没有 Claude 5 lean `# Harness` test。

本轮只新增本报告，没有修改产品代码，也没有运行完整 workspace test/build。
