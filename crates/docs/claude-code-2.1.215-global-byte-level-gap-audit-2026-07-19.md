# Claude Code 2.1.215 全局 byte-level parity 复审

## 复审状态更新 2026-07-26

本文档写于 2026-07-19，其后多轮 wave 已关闭其中大量条目；**下文各节保留为修复前证据，不应再当作当前工作树状态**。已在行为点逐条核实并关闭的：

- **H-06**（PowerShell 5.1 fail-open）— **CLOSED**。`policy.rs:217-218` 现在对无法静态验证的命令返回 `ask_powershell_invalid_parse`（"PowerShell command could not be statically validated: …"），不再 passthrough。
- **H-07**（Docker/Podman daemon redirect flags）— **CLOSED**。`read_only_command.rs` 的测试断言 `docker images --context=prod` / `--url` / `--connection` / `docker ps --identity` 均**不**是只读。
- **H-08**（`/goal` 只有文案）— **CLOSED**，见 2.1.216 复审 §0（session-scoped Stop hook 强制续转）。

**全部 30 条已于 2026-07-26 逐条 behaviour-site 复核。** 结果如下（与前两次 re-triage 一致：绝大多数早已关闭）：

**CLOSED（27）** — H-01…H-12 全部（H-01/02 见 2.1.216 §0 的 PTY/冷恢复关闭；H-03/04 settings 层级与 2 MiB；H-05 长命令强制 prompt；H-06/H-07 见上；H-08 `/goal` Stop hook；H-09 OTel exporter；H-10/11/12 marketplace policy 与 `name@marketplace` 身份），以及
M-01（`apps/cli/src/lib.rs:404` `--brief`）、M-03（`load_merged_disable_agent_view`）、M-04（`startup_resources.rs` `--plugin-url`）、M-06（`events.rs:77` tool heartbeat）、M-07（single-writer 归 `run_stream_json_input_loop`）、M-08/M-09（`resume.rs:52` effort + compaction 冷恢复连续性）、M-10（`definition.rs:79` "Parsed, stored, and EXECUTED"）、M-11、M-12、M-13（`emit_api_retry` → `TurnEvent::ApiRetry`，`lib.rs:3912`）、M-14（`events.rs:340` `CommandsChanged`）、M-15（`load_merged_ask_user_question_timeout`，注释直接标注 M-15）、L-01、L-03。

**仍开（1）** —— 下面两条在 2026-07-26 的后续核实中也已关闭：

- **M-02 — CLOSED（本轮修复）**。`SendMessage::is_enabled` 现在返回
  `self.ctx.mailbox_router.is_some()`。旧理由（"swarm surface 在 Rust host 始终存在"）
  描述的是**类型存在**，不是 **seam 已接线** —— 两个不同的问题。没有 router 时
  `call` 对每次调用都返回 `router_not_wired`，所以可用性现在与广告一致。
- **M-05 — CLOSED（早已实现）**。`PluginRuntime::refresh`
  （`engine-desktop/src/lib.rs:3664`）第一件事就是
  `replace_plugin_configs(load_plugin_configs(&self.home).await)`。我上一轮的探针
  用了 `reload.*plugin_configs` 这个模式，而调用写在 `self.manager` 上，两词不在同一行 ——
  **又一次是 grep 模式而非代码路径给出的假阴性**。
- **L-02 — CLOSED**。原述"fixtures 锁在 `cc_2_1_198`、无 2.1.214/215 delta
  suite"已过时：`test-harness/tests/` 下已有 198/207/208/215/216/217 六套
  suite，`cc_2_1_215_*_help.txt` fixture 也在。**但复核时发现了一个真问题**：
  `platform_api::CLAUDE_CODE_VERSION` 停在 `2.1.217`，而本 session 全部行为都是从
  **2.1.220** 二进制读出来移植的 —— 端口在实现 2.1.220 的同时，向服务器和子进程
  宣称自己是 2.1.217。已 bump 至 2.1.220 并新增 `parity_claude_2_1_220.rs`；
  217 suite 降级为历史 fixture（不再钉 live 常量），
  `platforms/posix` 里一处硬编码 `2-1-217` 的断言改为派生 —— 它正是"因为**正确**
  而失败"的那类测试。

M-13 的成因值得记：它是被一句**过期的注释**触发的 —— `retry_ux.rs` 的模块头一直写着 "not wired"，而接线早已存在。该注释已改正。

---


日期：2026-07-19  
LingXi 基线：`50b7b88c0b01ccc1b56d53559561d634a2c4bfc9`  
Claude Code 基线：`2.1.215`（npm `latest` / `next`；native binary SHA-256 `90608b5c5ab504e96e77365cea6203d046e291d59b2bb42cf28dcb2ccdf9dd58`）

## 结论

本轮不是“旧 gap 清单复述”。它重新下载了官方 2.1.215 包，对 CLI help、隐藏命令、Mach-O 字符串、2.1.214/2.1.212 官方变更与当前 Rust 运行路径逐项交叉验证，并对近期大量修复做了去重。

当前仍有 **12 个 HIGH、15 个 MEDIUM、3 个 LOW** 的已确认问题。最需要先修的不是 compact 主流程：`/compact` 的真实摘要调用、边界写入、恢复后的已发现工具等核心链路已经存在。当前阻塞 1:1 parity 的主轴是：

1. background attach 仍不是完整 PTY；跨 worktree 冷恢复仍可能读错 transcript；
2. settings 层级、2 MiB 上限和若干 settings-to-runtime 接线仍不一致；
3. 2.1.214 新修的 Bash / PowerShell / Docker 权限边界仍有 fail-open；
4. `/goal`、OTel、插件托管策略和插件配置隔离仍是“表面已实现、关键执行面未接线”；
5. SDK/stream-json、long-tool heartbeat、冷恢复 effort/compact tracking 仍有协议或状态丢失。

本文排除了明确的产品差异：多 LLM provider、LingXi 品牌目录/环境变量、自定义 WebSearch provider、移动端专属 surface、Claude Remote Control/Claude.ai 订阅 UI 等。

## 审计方法与可信度边界

- 官方包：`@anthropic-ai/claude-code@2.1.215` 及 darwin-arm64 native binary。
- 动态探针：`claude --version`、`claude --help`、隐藏的 `claude attach --help`、npm dist-tags、`--settings` >2 MiB 行为。
- 静态对照：官方二进制字符串、[官方 changelog](https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md)、[settings 文档](https://code.claude.com/docs/en/settings)、[monitoring 文档](https://code.claude.com/docs/en/monitoring-usage)、[Agent SDK 文档](https://platform.claude.com/docs/en/agent-sdk/typescript)。
- Rust 验证：权限、settings、hooks、memory、session、plugin 的定向测试全部通过；测试通过只说明当前实现自洽，不代表与 2.1.215 一致。
- Anthropic 未公开完整 Claude Code 源码，因此“byte-level”指可观察协议、字符串、序列化、命令面和行为分支的逐字节/逐字段对照，不声称能证明私有实现内部 100% 同构。

## HIGH：必须优先修复

### H-01 live background attach 不是 PTY attach

**触发**：attach 到仍运行的会话后进入权限交互、`/mcp` 菜单、alternate-screen/raw-mode 应用，或调整终端大小。

**当前行为**：`apps/cli/src/bg_attach.rs:31-39` 的协议只有 `Line`、`Interrupt`、`ClientDetached`；`409-411` 自称 “small PTY-like pump”，`451-499` 在客户端本地做行编辑后整行写 socket。没有 raw byte stream、PTY master/slave、window-size frame、SIGWINCH、screen state 或 detach/reattach terminal ownership。`apps/cli/src/daemon_lock.rs:49-53` 还明确说明正式 `control.sock` / `rvAuth` / `ptyAuth` daemon 没有接线。

**Claude 2.1.215**：隐藏命令 `claude attach <id>` 会把当前终端真正附着到后台 session；2.1.210/2.1.214 还明确修复 attach 期间 resize 与“只有无 terminal attached 的 background session 才拒绝菜单”的语义。

**影响**：不是 UI 差异，而是 live session 的输入/control 能力缺失；当前实现遇到全屏 UI、终端 resize 或逐字节控制协议会错乱。

### H-02 跨 git worktree 的 cold reopen / resume 可能恢复空会话或错误 cwd

**触发**：agent view 或 `/resume` 选择 sibling worktree 中的历史 session，再 reopen、attach fallback 或 resume-as-background。

**当前行为**：session loader 会枚举 sibling worktree，但 `apps/cli/src/commands/agents.rs:420-446` 重新启动时只传 `--resume <id>`，没有传原始 cwd/transcript path；`676-692` 在当前 cwd spawn。`apps/cli/src/background_dispatch.rs:301-333` 已保存 `Launch::Resume.transcript_path`，但 `apps/cli/src/commands/bg_worker.rs:199-213` 完全忽略它，重新用 `spec.cwd` 推导路径。

**影响**：列表里看得到 session，但打开后可能空白、读到错误路径，或在错误工作树执行后续工具。

### H-03 settings precedence 与 scope 模型错误

**当前行为**：`core/src/settings/mod.rs:137-155` 按 defaults → project → user → env 合并，实际是 **user 覆盖 project**；`171-173` 明确承认没有独立 `settings.local.json`，`--setting-sources local` 只能映射到 project 或忽略。managed/CLI/local 也没有形成 Claude 的统一 precedence source-of-truth。

**Claude 2.1.215**：正式顺序是 managed > CLI > local > project > user；local 文件位于 repo root，并通过 worktree 解析到主 checkout。

**影响**：同一配置在 LingXi 与 Claude 得到相反结果；local 个性化覆盖失效，project 配置可被 user 意外覆盖。CLI、desktop、bridge 多个 composition root 都调用这套 loader，因此不是死代码。

### H-04 settings 文件缺少 2 MiB fail-fast，上游 read 仍无界

**当前行为**：`core/src/settings/loader.rs:24-40` 直接 `std::fs::read(path)`，读取前没有 metadata/type/2 MiB 检查。

**Claude 2.1.215**：>2 MiB 立即以 `Settings file exceeds the 2MiB limit` 失败；2.1.214 专门修复 device file / multi-GB file 导致的无界内存增长。

**影响**：`--settings` 指向设备文件、FIFO 或超大文件时可能阻塞或耗尽内存。

### H-05 >10,000 字符 Bash 命令没有强制 prompt

**当前行为**：`permission/src/bash_tree_sitter.rs:38-40` 对超长命令返回 `None`；`bash_ast_security.rs:3129-3135` 将其变成 `ParseUnavailable` 并退回 legacy regex。安全的超长 `echo`/read-only shape 仍可能自动允许。

**Claude 2.1.214+**：超过 10,000 字符一律 prompt，不再 fallback 后自动执行。

**影响**：长命令可绕过 AST 安全分析和新的 fail-closed 语义。现有测试只锁定“返回 None”，实际上把旧行为保护成正确行为。

### H-06 PowerShell 5.1 绕过仍按旧版 fail-open

**当前行为**：`permission/src/powershell_parse.rs:386-392` 对超长命令和 `` `u{HEX} `` 直接返回 invalid parse；`497-517` 对 timeout、non-zero、无法解析同样返回 `valid=false`，而 `policy.rs:176-180` 将 invalid parse 当 passthrough。

**Claude 2.1.214+**：官方 changelog 明确修复 Windows PowerShell 5.1 permission-check bypass。

**影响**：无法静态验证的命令没有强制询问，可能绕过 path containment。当前注释仍声称这是 Claude 行为，说明移植基线已过期。

### H-07 Docker/Podman daemon redirect flags 仍被只读 auto-allow

**当前行为**：`permission/src/read_only_command.rs:247-256` 看到 `docker ps` 或 `docker images` 就返回 true，完全不扫描其余 argv。携带 `--url`、`--connection`、`--identity` 或 Podman remote mode 的形状仍会被标成只读。

**Claude 2.1.214+**：这些 daemon redirect flags 强制 permission prompt。

**影响**：命令可把“只读”查询重定向到本地策略之外的 daemon/identity。

### H-08 `/goal` 只有文案，没有 Stop-hook 执行语义

**当前行为**：`commands/core/src/goal.rs:48-73` 已自述：没有 app-state goal seam、没有添加/移除 Stop hook、directive 只是要求模型继续；workspace trust 永远 true，hooks-restricted 永远 false（`103-112` 及后续函数）。

**Claude 2.1.215**：二进制会向 session hook registry 注册 prompt 型 Stop hook；未达成时 block stop 并累加 iterations/lastReason，达成时自动清除；不可信 workspace 和 restricted hooks 会拒绝 `/goal`。

**影响**：用户看到“hook is now active”，实际上模型仍能直接结束；同时绕过 trust/managed-hook gate。

### H-09 OpenTelemetry 只有配置 schema，没有真实 exporter/recording

**当前行为**：`telemetry/src/otel/mod.rs:35-41` 明确说明 OTLP egress、metric readers、LoggerProvider、trace provider 和约 20 个记录点均未接线；`70-72` 仍称 future commit。

**Claude 2.1.215**：官方 monitoring contract 导出 metrics、logs/events 和可选 traces；2.1.214 又增加 `message.uuid`、`client_request_id`、`tool_source` 和 `CLAUDE_CODE_OTEL_CONTENT_MAX_LENGTH`。

**影响**：管理员设置 `LINGXI_ENABLE_TELEMETRY`/`OTEL_*` 后会得到“配置被接受但没有数据”的静默失败。品牌变量重命名是允许差异，缺少导出不是。

### H-10 managed marketplace restriction 没有 enforcement

**当前行为**：settings/schema 对 `blockedMarketplaces` / strict-known-marketplace policy 无完整实现；`apps/cli/src/commands/plugin_marketplace.rs:449-485` 的 add 和 `770-818` 的 update 在副作用前没有托管策略检查。

**Claude 2.1.215**：`blockedMarketplaces` 是 managed-only，并在 marketplace add 以及 plugin install/update/refresh/auto-update 下载前拦截。

**影响**：企业管理员以为已封禁的 marketplace 仍能被添加和刷新。

### H-11 project settings 可注入 `pluginConfigs`，违反 2.1.207+ 安全边界

**当前行为**：`apps/engine-desktop/src/lib.rs:3000-3007` 从 user 和 **project** settings 合并 pluginConfigs，且 project 后合并并覆盖 user；这些值随后进入 `${user_config.*}`、plugin hooks、MCP/LSP 配置。

**Claude 2.1.207+**：`pluginConfigs` 只允许 user、`--settings` 和 managed，明确忽略 project/local，因为 cloned repository 不应供应会被代入 hooks/MCP/LSP 的值。

**影响**：恶意仓库可控制插件非敏感配置，进而影响命令、header、endpoint 或 hook 参数。这是 trust-boundary bug，不是普通 precedence 差异。

### H-12 plugin config/secret 以 bare manifest name 命名，跨 marketplace 冲突

**当前行为**：`plugin/src/manager.rs:500-518` 用 `manifest.name` 同时查 `pluginConfigs` 和作为 secret namespace；官方 identity 是 `name@marketplace`。`engine-desktop/src/lib.rs:3011-3013` 也把它标成 follow-up。

**影响**：两个 marketplace 中同名插件会读取/覆盖彼此的 options 或 secret namespace；目标插件的 `pluginConfigs["name@marketplace"]` 反而不生效。

## MEDIUM：功能/协议 parity 缺口

| ID | Gap / bug | 证据与影响 |
|---|---|---|
| M-01 | `--brief` 缺失且 `SendUserMessage` 永远暴露 | Claude 2.1.215 help 只有传 `--brief` 才启用；LingXi argv 无此 flag，`tools/ui/src/brief.rs:195-196` 恒 true，改变默认 toolset 与 prompt。 |
| M-02 | 普通 session 永远暴露不可用的 `SendMessage` | `tools/ui/src/lib.rs:33-36,63-65` 默认注册；`send_message.rs:579-583` 恒 true，但无 mailbox router 时 `771-783` 只返回 internal error。Claude 将该工具绑定到 agent-team/teammate context。 |
| M-03 | `disableAgentView` setting 未接到命令注册 | `commands/core/src/register.rs:361-369` 只读 env gate，已有的 `is_enabled_with_setting` 没被 composition root 调用；`/fork`/`/subtask` 注册语义错误。 |
| M-04 | `--plugin-url` 缺失 | Claude 2.1.215 提供 repeatable `--plugin-url <url>` 作为 session-only zip plugin；LingXi help/argv 无此选项，URL marketplace fetch 也仍在 `plugin_marketplace.rs:463-465` 明确 decline。 |
| M-05 | `/reload-plugins` 不重载 `pluginConfigs` | config 只在 engine build 时读取并 move 进 `PluginManager`; refresh 重扫组件/enable set，但不重新读取 options/secrets scope。配置修改后必须重启。 |
| M-06 | long-running tool periodic heartbeat 缺失 | `client-protocol/src/events.rs` 只有 tool started/result；没有 `tool_heartbeat` timer/frame，长工具在 SDK/CLI 中继续静默。2.1.214 明确新增 heartbeat。 |
| M-07 | stream-json replay ack 破坏 single-writer 顺序 | drain 启动后应由一个 task 写 stdout，但 `stream_json_input.rs:286-308` 直接 lock/write；`run.rs:1037-1042` 在 live turn loop 调用，能与 queued frame 交错或越序。 |
| M-08 | cold resume 丢失 reasoning effort | assistant JSONL 已在 `orchestrator/src/conversation.rs:3983-3999` 写 `effort`；`orchestrator/src/resume.rs:132-160` 只恢复 model，不恢复 effort/config。无新 `--effort` 的 resume 会降回默认。 |
| M-09 | cold resume 丢失 compact cumulative/tracking | `conversation.rs:949-964` 说明状态依赖 prior compact boundary；constructor `1400-1402` 归零，`resume.rs:235-244` 只恢复 discovered tools。rapid-refill/circuit-breaker 与 cumulativeDroppedTokens 会漂移。 |
| M-10 | agent frontmatter `memory:` 只解析不执行 | `agent/src/definition.rs:79-83`、`catalog.rs:264-266` 明确说明 auto-memory tool injection 未接线；声明 `memory: user/project/local` 的 agent 不会得到预期持久 memory。 |
| M-11 | memory frontmatter 缺 ISO `modified` | `memory/src/file.rs:28-43` 的 `MemoryFrontmatter` 无 modified；2.1.214 已要求写入 ISO timestamp。影响选择、诊断与文件 parity。 |
| M-12 | `paths:` 用自制扫描器，不是 YAML 语义 | `memory/src/lingxi_md/loader.rs:421-441` 承认不支持完整 YAML；如 `paths: src/** # note` 会把 inline comment 计入 glob，复杂 quoting/flow scalar 也会漂移。 |
| M-13 | live retry status 没接线 | `tui-core/src/retry_ux.rs:16-21` 明确只有渲染纯函数/demo fixture，没有 `llm-runtime onRetryStatus` producer；真实 retry 时 TUI 仍像卡死。 |
| M-14 | 动态 slash-command 协议不完整 | CLI init snapshot 只取 shared registry，遗漏 TUI local `/reload-plugins`；client protocol 无 `commands_changed`，bridge 的 SlashCommands 也未路由。插件/skills reload 后 SDK 客户端目录不会更新。 |
| M-15 | `askUserQuestionTimeout` 解析了但 runtime 固定为 `never` | schema/config UI 已有 60s/5m/10m/never；`tools/ui/src/lib.rs:73-80` 明确 composition root 未接线，始终构造 `Never`。用户设置被静默忽略。 |

## LOW：序列化与审计基线问题

| ID | Gap / bug | 说明 |
|---|---|---|
| L-01 | resume 丢失 compact-summary visibility flags | JSONL 有 `isVisibleInTranscriptOnly` / `isCompactSummary`，但 `build_state_from_jsonl` 只恢复 `isMeta`，冷恢复后的 transcript-only/summary 标记不可表达。 |
| L-02 | parity harness 基线陈旧 | fixtures 主要锁在 `cc_2_1_198`，最新显式 parity test 到 2.1.208；没有 2.1.214/2.1.215 delta suite，旧的 42-tool registry snapshot 也不能覆盖新 gated tools。 |
| L-03 | gap/stub 元数据已陈旧 | `command-api/src/builtin_support/names.rs` 的 deferred gap 列表仍把已实现的 `btw`、`reload-plugins` 归为缺失；会让后续审计产生假阳性并掩盖真实 gap。 |

## 本轮已核实关闭，不应重复修复

- `/compact`：manual command 已走真实 compaction orchestrator/LLM summary；compact boundary、preCompactDiscoveredTools、attachments/tool-results 等主链路已存在。剩余是 **cold-resume bookkeeping**，不是“一秒完成代表完全没执行”的旧问题。
- Hook exit code 2 + stdout invalid JSON：`hooks/src/executor.rs:2158-2275` 已按 exit-code fallback 阻塞，符合 2.1.214。
- TUI resize ghost/bottom-pane：fullscreen resize invalidation 与 narrow diff 标记已有覆盖，本轮未复现旧问题。
- LSP 50-document LRU、超长单行 Read、Web 529 retry/API Error filtering已实现。
- MCP >2 分钟 auto-background、completed background session removal、idle worker cleanup 已实现并有测试。
- `updatedToolOutput` / `updatedMCPToolOutput`、`EndConversation`、SessionStart fork source、`file -m/-f`、single-segment `dir/**`、help/man 安全收紧均未发现当前残留。

## 推荐修复顺序

### P0：安全与数据完整性

1. H-03/H-04 重建统一 settings source graph，并在所有文件入口先做 regular-file + 2 MiB guard。
2. H-05/H-06/H-07 把最新 fail-closed checks 放在任何 allow/read-only/sandbox shortcut 之前，并补黑盒 fixture。
3. H-10/H-11/H-12 统一 managed marketplace policy 和 `name@marketplace` identity；禁止 project/local pluginConfigs 进入 substitution。
4. H-02 使用已保存 `transcript_path`/original cwd 贯穿 agent-view、resume-background、worker seed。

### P1：核心行为

5. H-01 建立 daemon rendezvous/control + PTY socket 协议：raw bytes、resize、detach、reattach、alt-screen、auth rotation，并增加隐藏 `attach` command。
6. H-08 把 activeGoal 放入 session app state，注册真实 Stop prompt hook，接入 trust/managed-hook gate 和 auto-clear/iteration telemetry。
7. H-09 建立真实 meter/logger/tracer provider 与 exporter，补全 record sites 和 2.1.214 attributes。

### P2：协议与恢复

8. 修 M-07 single-writer，再补 heartbeat 与 dynamic commands，避免 SDK 兼容层继续漂移。
9. resume loader 恢复 effort、compact metadata/tracking、summary flags；用 compact → exit → cold resume → auto-compact 的端到端测试锁定。
10. 将 `--brief`、SendMessage、agent-view、AskUserQuestion timeout 等 gate 统一由 resolved settings/session capability 决定。

## 验证记录

- `cargo test -p permission`：879 tests passed，0 failed。
- `cargo test -q -p core settings`：通过。
- `cargo test -q -p hooks`：通过。
- `cargo test -q -p memory`：通过。
- `cargo test -q -p session`：通过。
- `cargo test -q -p plugin`：通过。
- `claude --version`：`2.1.215 (Claude Code)`。
- npm dist-tags：`latest=2.1.215`、`next=2.1.215`、`stable=2.1.205`。
- `claude --help` 与 LingXi help 对比确认 `--brief`、`--plugin-url` 缺失；Remote Control flags 按项目特定差异排除。

## Release gate

当前结论：**REQUEST CHANGES / 尚不能宣称 Claude Code 2.1.215 1:1 parity。**

至少 H-03～H-07、H-10～H-12 属于 release-blocking security/config isolation；H-01、H-02、H-08 属于用户可直接触发的核心行为不一致。修复后需要以最新 native binary 建立可重复的 differential suite，而不是继续只靠字符串注释或旧 fixture。
