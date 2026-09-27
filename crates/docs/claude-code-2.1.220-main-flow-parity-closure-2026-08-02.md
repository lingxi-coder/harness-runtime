# Claude Code 2.1.220 主流程 parity 关闭记录

**日期：** 2026-08-02

**LingXi 基线：** `main@d665b0a5d`

**实现分支：** `codex/claude-2.1.220-main-flow-parity`

**Oracle：** 本机 Claude Code `2.1.220`，SHA-256
`8addc857f3fe64d5a0368af9ee50321b50afb4a6918ba3ef018ab84f5dbbe081`

## 结论

本轮确认的 settings/policy、MCP、plugin、AskUserQuestion、hooks、status-line、
goal、memory 和 tool schema/classifier 缺口均已在 production composition path
关闭。`terminalSequence`、POSIX hook 首行 `{"async":true}`、compact、sandbox、
tasks/workflow 原本已有实现，本轮只加强 production-path 回归，没有引入第二套实现。

这里的 parity 指可观察的 CLI、协议、持久化、安全、恢复和 TUI 状态语义。
报告不声称知道或复制 Claude Code 的私有内部实现；涉及 Anthropic 私有服务的能力继续
作为明确 divergence 处理。

## 实现提交

| 提交 | 关闭范围 |
|---|---|
| `227a7493d` | 固定 oracle、production-path regression 和授权边界 |
| `9ed4d5598` | strict-plugin、marketplace gate、AskUserQuestion、process wrapper、status-line trust |
| `282d2a84e` | MCP effective policy、OAuth scopes/重试、HTTP/SSE/WS `headersHelper` |
| `46e9f84bc` | plugin source、typed config、依赖图、事务安装、uninstall/prune |
| `2f8a11c62` | hooks、goal、memory、status-line、classifier、Agent schema 和恢复语义 |
| `10eab0c22` | POSIX/Windows/parity MCP transport fixture 的跨平台兼容 |

## CLOSED 项

### Settings、policy 与交互授权

- `strictPluginOnly` 只锁定 Skills、Agents、Hooks、MCP；plugin、builtin 和 managed
  来源继续保留，未知数组项忽略。
- marketplace policy 使用 typed source identity；空 allowlist、blocked precedence、
  optional ref/path、host/path pattern 都在网络、clone、解压和缓存写入前检查。
- headless/print/无 broker 的 AskUserQuestion 不再生成 first-option 默认答案；防御性调用
  返回 `InteractionRequired`，timeout 只提交用户确认的答案。
- `askUserQuestionTimeout` 与 `processWrapper` 按受限 source precedence 解析；wrapper
  使用 quoted argv 解析并在非法 quoting 时 fail closed，后台和 self-spawn 复用同一快照。
- status-line command 在 spawn 前经过 workspace trust、`disableAllHooks`、
  managed-hooks-only 和 source provenance gate。

### MCP

- HTTP、SSE、WebSocket 共用动态 `headersHelper`，每次 connect/reconnect 重新执行，
  10 秒 timeout，动态值覆盖静态 header，并严格校验 string-to-string JSON object。
- project/local helper 在 trust 之前不会启动；plugin helper 使用 plugin root，并注入受控
  server name、URL 和 plugin root 环境。
- OAuth 配置保留原始 scopes 字符串；pinned scopes 优先。401/403 和
  `insufficient_scope` 只允许一次刷新/重认证重试，第二次进入 NeedsAuthentication。
- CLI 和 engine boot 共享 effective MCP policy；deny 合并所有本地层并优先，allow
  仍只由 managed policy 决定。
- transport 错误保留 HTTP status 与 `WWW-Authenticate`；旧 MCP 配置缺少新增字段时
  继续可读。

### Plugin

- marketplace catalog 和 plugin source 已 typed 化，覆盖 relative path、GitHub、git
  subdir、URL、npm、file/directory，并复用 URL safety、archive traversal 和原子 materialize。
- `--config` 按 schema 转换 string/boolean/number；敏感项写 SecureStorage，普通项写
  `pluginConfigs`，失败时回滚 settings、secret 和 installed record。
- manifest 与 marketplace dependency 合并后递归解析，检测 cycle/range conflict，
  支持受策略约束的跨 marketplace 依赖；自动依赖记录 `auto_installed`/`required_by`。
- enable/disable、reload/update、uninstall、`--keep-data`、`--prune`、非 TTY 与 `-y`
  的依赖和清理语义均已接入；手工安装项不会被 prune。

### Hooks、goal、memory、status-line 与 tools

- hook permission gate 使用 live mode；attachment 保存真实 source；
  PostToolUseFailure 的 event name 在 attachment/context/prose 中一致。
- `asyncTimeout`、`asyncRewake`、`rewakeMessage` 已接入；runtime `{"async":true}`
  的 live progress 会持续到 detached child 真正完成，不再提前结束状态行。
- `statusMessage` 进入 TUI live hook row；超过 10 KiB 的 output 原子保存到 session
  `tool-results` 并以路径引用，不再静默截断。
- goal 保存 `tokens_at_start` 和 `iterations`，使用 typed `goal_status` attachment；
  set/evaluate/achieve/clear、resume/fork/background/compact 同时兼容旧记录。
- surfaced memories 按 oracle 顺序生成独立 transient `user_meta` 消息，不写 JSONL，
  token estimator 保留独立消息边界。
- main status-line 包含 API duration、edit lines、input/output/current usage，并支持
  `refreshInterval`、`hideVimModeIndicator`；subagent status-line 按 task/columns 执行。
- read-only classifier 以 permission crate 为唯一真值，shell 直接复用；补齐 `gh` 和安全
  `jq`，redirect/mutation/shell expansion 负例仍拒绝。
- Agent tool 在构造时冻结 background availability；禁用后台或 pro-plan 时 schema 不发布
  `run_in_background`。
- Stop hook 只持久化 typed attachment，API normalization 时派生 meta message，避免恢复
  时重复注入。

### 已有能力的回归确认

- `terminalSequence` 仍通过 allowlist 后写入真实 TUI terminal。
- POSIX command hook 首行 `{"async":true}` 仍走 streaming detection 与 async registry。
- compact typed boundary、sandbox、tasks/workflow 和 background PTY 未被本轮重写。
- Claude 模型 prompt profile 与非 Claude `FullHarness` 路由不受本轮影响。

## 生产路径证据

关闭依据是生产模块及其 composition-root tests，不是只含 `target: CLOSED` 的 inventory
fixture。代表性覆盖包括：

- `apps/cli/src/mode.rs`、`apps/cli/src/init.rs`：settings source、headless AskUser、
  wrapper、status-line policy。
- `mcp/src/headers_helper.rs`、`mcp/src/registry.rs`、`mcp/tests/oauth_flow_test.rs`：
  dynamic headers、effective policy、OAuth retry。
- `plugin/src/manager.rs` 与 CLI plugin integration tests：transaction、dependency、prune。
- `hooks/src/executor_test.rs`、`test-harness/tests/parity_hooks_runtime.rs`：runtime marker、
  source、rewake、large output、terminal sequence。
- `orchestrator/tests/resume_test.rs`、`orchestrator/tests/stop_hooks_test.rs`：typed goal/Stop
  attachment 与恢复。
- `memory/src/surfacing.rs`、`tools/agent/src/agent_test.rs`、
  `permission/src/read_only_command.rs`：memory message boundary、dynamic Agent schema、classifier。

## 验证结果

- `cargo fmt --all -- --check`：通过。
- `cargo check --workspace --all-targets`：通过；该门禁额外发现并修复了四个跨平台 MCP
  fixture 的新增字段/类型遗漏。
- `cargo test -p hooks --lib`：403/403 通过。
- orchestrator、TUI、CLI 及相关 focused production-path suites：通过。
- `cargo test --workspace --all-features --no-fail-fast`：518 个 suite、13,695 个测试
  通过，10 个 ignored，0 个失败。
- `cargo build --release -p cli --bin lingxi-cli`：在 Rust 1.82.0 下通过；release 二进制
  `--version` 和 `--help` 启动 smoke 通过。
- MSRV 环境：`rustc 1.82.0`、`cargo 1.82.0`。
- `cargo clippy --workspace --all-targets -- -D warnings`：未通过；阻塞项位于与基线
  `d665b0a5d` 完全相同、且本分支未修改的 `telemetry-macros/src/lib.rs`、
  `workflow/src/lib.rs`、`protocol/src/iso8601.rs`、`protocol/src/transport.rs`，包括
  `map_unwrap_or`、pedantic cast/命名和文档 Markdown 旧债。本轮没有为制造“全绿”而混入
  无关重构。
- macOS iTerm2、Linux PTY 和 Windows ConPTY/process-wrapper 的真实平台 smoke 尚未执行；
  下节继续作为明确延迟项记录。

## 明确 divergence

### LingXi 产品差异（accepted）

- 多 LLM provider、LingXi 品牌和自定义目录结构。
- 非 Claude provider 使用完整 `FullHarness`，不会因 Claude prompt 精简而缩短。
- 面向多 provider 的认证入口继续使用 `/connect`。

### Anthropic 私有服务（fail-fast）

- Chrome extension/private Chrome protocol。
- Anthropic remote-control relay/auth。
- remote-memory 私有账号同步端点。
- 私有 report upload/publish endpoint。

这些命令面可提供清楚的非零错误，但不能伪装成 1:1 可用实现。

## 仍需平台环境验证

- macOS iTerm2 的人工 status-line/hook live-row smoke。
- Linux PTY CI。
- Windows ConPTY、process-wrapper、MCP transport CI/真实机器 smoke。

这些是平台验证延迟项，不是本轮已知的 Rust 实现缺口；在对应平台门禁通过前不得把它们
写成“已实机验证”。
