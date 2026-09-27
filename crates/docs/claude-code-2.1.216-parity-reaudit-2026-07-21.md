# LingXi Code 对 Claude Code 2.1.216 的真实性复核报告

> 复核日期：2026-07-21
>
> LingXi Code 基线：`c73437369`
>
> Claude Code 本机版本：`2.1.216`
>
> npm 最新版本：`@anthropic-ai/claude-code@2.1.216`

## 0. 实施后关闭状态（2026-07-21）

> 本节是对下文“修复前快照”的实时更新，优先级高于第 1–8 节的历史结论。下文保留原始证据链，用于说明当时的 gap 为什么成立；不应再将其解读为当前工作树状态。

| 项目 | 实施后状态 | 关闭依据 |
|---|---|---|
| H1–H4、M1–M2 | **CLOSED** | 保留单一真实 PTY/ConPTY background 架构；attach v2 原始字节、resize、detach/reattach、resume/fork 和完整 launch spec/runtime metadata 均有回归覆盖。 |
| H5 | **CLOSED** | managed worktree 使用版本化 `0600` owner marker/token；删除前校验 roster、canonical path、branch、worker/liveness、clean state 和目录 identity，legacy 缺少归属信息时 fail closed。 |
| H6 | **CLOSED** | `FileSystem` 增加 rooted `atomic_write_confined` 与受控锁；cron、scheduler 和 workflow 写入已迁移，Unix 拒绝 symlink，Windows 拒绝 reparse-point/path substitution。 |
| H7 | **CLOSED** | `mcp serve/login/logout/add-from-claude-desktop` 不再是 stub；stdio server 覆盖 initialize/ping/tools/list/tools/call/cancel/progress，OAuth/SecureStorage 与 desktop import 有真实测试。 |
| H8 | **CLOSED** | `/goal` 通过 session-scoped named Stop Prompt hook 强制续转，支持 replace/clear/auto-clear、resume/compact metadata，且不绕过显式 cancel 和成本/turn 硬上限。 |
| M3 | **CLOSED** | `sandbox.filesystem.disabled` 已进入配置、merge 和平台 runtime bridge；只关闭 filesystem restriction，不放开 network/process sandbox。 |
| M4 | **CLOSED** | AskUserQuestion 由 UI 合成 `Other`，支持 Unicode/paste/free-text、Esc 返回、空文本拒绝与多选并存，输出保留用户原文。 |
| M5 | **CLOSED（Chrome 除外）** | `--betas`、`--plugin-url`、`--file` 已有有效/错误/安全路径；`--no-chrome` 是真实禁用，`--chrome` 因缺少 Anthropic 私有扩展合约而明确非零 fail-fast。 |
| M6 | **HONEST BOUNDARY** | `remote-control` 兼容 2.1.216 命令面/help/flags，执行时明确非零退出；没有把 LingXi bridge 冒充为 Anthropic 私有 relay/auth parity。 |
| M7 | **CLOSED** | 已接入锁版的 OpenTelemetry SDK、OTLP HTTP/gRPC、console 和 Prometheus exporter；默认关闭、内容 opt-in、红线信息脱敏、fail-open 与限时 flush 均有回归覆盖。 |
| M8 | **CLOSED** | plugin/skill reload 共用 registry reconciliation，原子替换 TUI completion snapshot，保留 composer 并修正失效 selection。 |
| L1 | **CLOSED** | fallback model 按有序列表解析、trim/空项拒绝/稳定去重；仅 print mode 生效，每个新 user turn 从 primary 重开并记录实际 model。 |
| I1 | **CLOSED** | plugin config 写入只允许 runtime 真正消费的 user scope；project/local 写入前拒绝，`doctor` 只告警历史遗留而不自动搬运可能含密钥的值。 |
| P1、P2 | **CLOSED** | main-thread agent 版本化快照进入 JSONL/runtime metadata，resume 遵守 explicit override > snapshot > legacy name；ToolHeartbeat 由 TUI/plain/stream-json 消费并在 result/cancel/turn end 停止。 |
| 版本对外标识 | **CLOSED** | `platform_api::CLAUDE_CODE_VERSION` 已从 2.1.208 升到 2.1.216，WebFetch `User-Agent` 与子进程 `AI_AGENT` 由同一常量派生；2.1.208 测试已降为历史 fixture，不再锁死 live 标识。 |

实施过程还修复了五个由全 workspace 门禁暴露的独立 bug：POSIX/Windows watcher 在 native watch 真正 armed 前返回导致首个事件丢失；bridge `/loop` keepalive 使用进程全局状态导致跨连接互相清除；POSIX env 测试并发修改进程环境且失败时泄露完整 env；macOS Keychain 测试 ID 可碰撞；`DesktopConfig` doctest 没有跟上新字段。这些均已有定向回归并被 workspace test 覆盖。

当前能力边界只剩两个不可通过公开行为补全的私有依赖：Anthropic remote-control relay/auth 合约和 Chrome 扩展协议。它们的验收标准是“命令面可兼容、不会假成功、错误可操作”，不宣称 1:1 功能 parity。代码实现层面已没有延期项；目标平台运行验收的状态单独列在 0.2，避免把交叉编译或宿主机模拟误写成目标平台实测。

### 0.1 最终验证证据

- 工具链：`rustc 1.82.0` / `cargo 1.82.0`，即本项目 MSRV，不是使用更新 Rust 绕过兼容性。
- `CARGO_INCREMENTAL=0 cargo test --workspace --quiet`：**exit 0**，unit、integration、trybuild 和 doctest 全部通过；测试套件报告的 ignored 项保持其原有 host 依赖标记。
- `cargo fmt --all -- --check`：**exit 0**。
- `CARGO_INCREMENTAL=0 cargo check --workspace`：**exit 0**。
- `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets`：**exit 0**；仓库仍有非 fatal 的历史 warning，本轮没有用全局 allow 掩盖它们。
- `CARGO_INCREMENTAL=0 cargo build --release -p cli`：**exit 0**，生成 `target/release/lingxi-cli` （macOS arm64，66 MiB），`--version` 输出 `lingxi-cli 0.12.0`。
- `cargo check -p platform-pty --target x86_64-unknown-linux-gnu` 与 `--target x86_64-pc-windows-msvc`：均 **exit 0**，覆盖 Unix PTY 与 Windows ConPTY/Job Object 编译面。
- release 产物 smoke：`attach --help` 暴露公开 attach 命令；`remote-control` 和 `--chrome` 在缺少私有合约时均输出明确原因并以 **64** 退出。

### 0.2 延期平台门禁收口（2026-07-21）

| 门禁 | 当前状态 | 本轮证据 |
|---|---|---|
| Linux real PTY/process group | **CLOSED** | Docker Desktop 的真实 Linux daemon 中，以 `rust:1.82-slim-bookworm`（`rustc 1.82.0-aarch64-unknown-linux-gnu`）运行 `cargo test -p platform-pty -- --nocapture --test-threads=1`，**8/8 passed**；覆盖原始字节/Unicode、resize、EOF、退出尾部 drain、SIGINT、进程组及 descendant termination。CI 新增独立 `ubuntu-24.04` hard gate。 |
| macOS attach/detach/reattach/resize | **CLOSED（自动化路径）** | release CLI 创建后台真实 TUI，首次 attach 使用 **100×30**，Ctrl-] detach 后 worker PID `26083` 与 session `1ab77618-3673-4aea-b334-275ca1a1defa` 保持；第二次 attach 使用 **140×50**，服务端分别输出对应 `CSI 1;30r` / `CSI 1;50r` scroll region，证明 resize 到达同一 PTY；再次 detach 后通过 `lingxi-cli rm` 清理，PID 消失。attach v2 Unix 协议测试 **10/10 passed**。 |
| TUI bottom viewport resize | **CLOSED（自动化路径）** | `cargo test -p tui terminal::tests -- --nocapture --test-threads=1`：**14/14 passed**；覆盖 bottom-pinned viewport 80×24→120×40、viewport shrink、前导空白列 repaint、buffer invalidation 和终端边界。 |
| Windows ConPTY/Job Object runtime | **READY / TARGET-RUN PENDING** | 新增 **7 个 Windows-only runtime tests**，覆盖 ConPTY 原始 I/O、Unicode/等待、resize、EOF、退出尾部 drain、zero size、unsupported SIGINT 和 kill-on-close descendant cleanup；`cargo check -p platform-pty --target x86_64-pc-windows-msvc --tests` 通过。CI 新增独立 `windows-2022` hard gate，但当前仓库没有 Git remote，且本机没有 Windows VM/runner，因此不能诚实宣称已在 Windows kernel 上执行。 |
| iTerm2 人工视觉 smoke | **MANUAL-RUN PENDING** | 底层 macOS PTY、真实后台 attach/reattach、两种尺寸与 TUI 重绘回归均已实测；当前自动化环境的 Computer Use 安全策略明确拒绝控制 iTerm2，因此未伪造 GUI 人工验收。需要在 iTerm2 中人工拖拽窗口，确认 footer/editor 无残影后才能关闭这一视觉门禁。 |

`.github/workflows/ci.yml` 中的 Linux/Windows PTY jobs 都是不带 `continue-on-error` 的独立 hard gate，并固定 Rust 1.82。目标 OS runtime 结果必须来自对应 runner；不得用 macOS host 的 cross-check 替代 Windows ConPTY 运行证据。

## 1. 结论

本报告不是对历史 gap 清单的简单合并，而是对本轮已经提出的 findings 逐项重新判定；它不是一次从零开始穷举所有 CLI flags 的全新审计。复核后得到：

- **17 项已确认的功能或安全 gap**：8 项高优先级、8 项中优先级、1 项低优先级。
- **2 项部分成立**：实现已经存在，但还没有达到完整语义。
- **1 项项目内部一致性 bug**：真实存在，但不能算作 Claude Code parity gap。
- **8 项旧结论被撤回、降级或排除**，其中包括 streaming 413 recovery、`/compact` 未执行、main-thread agent 完全不恢复等说法。

最需要优先处理的仍是 background session/PTY、恢复时权限与运行配置丢失、worktree 删除安全、symlink 写入安全、MCP CLI stubs，以及 `/goal` 宣称存在但实际没有的 enforcement。

“与 Claude Code 1:1、byte-level 相同”无法通过公开资料严格证明：Claude Code 是闭源产品。本报告所说的 parity，限定为可以通过 **官方 changelog、官方 CLI 黑盒行为、当前 LingXi 源码与测试** 交叉验证的外部行为和安全语义，不把实现语言、内部结构或不可观察细节假装成已知事实。

## 2. 范围、排除项与判定标准

### 2.1 纳入范围

- orchestration、turn loop、conversation、agent/subagent、workflow
- sandbox、hooks、tools、MCP、memory、compact、tasks
- LSP、file、web、CLI/TUI、background session、worktree
- Claude Code 2.1.216 以及与当前发现直接相关的近期官方变更

### 2.2 明确排除

按项目约定，下列差异不计入 parity gap：

- multiple LLM providers 等 LingXi 项目特有能力
- 自定义目录布局、crate 切分、内部模块命名
- bridge/mobile 等 Claude Code 没有对应公共表面的项目专属能力
- 纯视觉、品牌、文案差异，除非影响交互或安全语义

### 2.3 真实性等级

- **TRUE**：源码证据和 Claude Code 官方/黑盒证据相互支持。
- **PARTIAL**：相关能力已实现，但关键语义仍缺失；不能表述为“完全没有”。
- **INTERNAL BUG**：LingXi 自身读写或接口不一致，但官方行为反而说明不能按旧 parity 方向修复。
- **RETRACTED**：旧结论被测试或源码反证，或者证据不足以支撑原强度。

优先级定义：

- **High**：权限边界、数据破坏、公开命令不可用，或核心 background/session 语义失效。
- **Medium**：用户可观察行为明显不同，或恢复/配置/诊断信息不完整。
- **Low**：兼容性边缘差异，不影响主流程。

## 3. 已确认的 gap

### H1. Background attach 仍不是真正的 live interactive PTY

**结论：TRUE / High**

LingXi 已经有 background attach transport，因此“完全不能 attach”是过度描述；但当前 worker 不是 PTY-backed session，resize 被忽略，非 active turn 的 control/resize 也会被忽略，且顶层 CLI 没有与 `claude attach <id>` 对等的公开命令。

源码证据：

- `apps/cli/src/commands/bg_worker.rs:411-419`：明确因为不是真 PTY 而忽略 resize。
- `apps/cli/src/commands/bg_worker.rs:513-534`：非 active turn 时忽略 control/resize。
- `apps/cli/src/commands/daemon.rs:26-28`：模块注释仍把 real PTY/control/watchdog 列为剩余工作。
- `apps/cli/src/bg_attach.rs:749-825`：非 Unix 平台为 unsupported 路径。
- `apps/cli/src/commands/mod.rs:48-92`：顶层命令没有 `attach`。

Claude Code 黑盒基线：`claude attach --help` 提供 `claude attach <id>`，连接的是持续运行的 session，而不是只消费事件流的近似层。

影响：无法做到 shell/REPL 的完整 byte stream 输入、窗口大小同步、信号/control 语义和可靠重连。

### H2. Resume-to-background 可能立即结束，而不是等待 live 输入

**结论：TRUE / High**

background forker 使用空 prompt 启动 resume；worker 看到空 prompt 会跳过 turn。当此时没有 client，等待输入路径返回 `None`，worker 随即把 session 标记为 done。

源码证据：

- `apps/cli/src/bg_session_forker.rs:115-128`
- `apps/cli/src/commands/bg_worker.rs:318-335`
- `apps/cli/src/commands/bg_worker.rs:500-528`
- `apps/cli/src/commands/bg_worker.rs:159-164`

影响：用户想把已有会话转为后台持续运行时，session 可能在 attach 前已经退出。

### H3. Background worker 丢失原始启动配置与 permission mode

**结论：TRUE / High**

dispatch 只传递受限环境变量和少量 spec；worker 构造的新 argv 没有恢复完整 flags，并把 permission mode 落到默认值。已持久化的 `flag_args` 没有在 worker 重建流程中消费。

源码证据：

- `apps/cli/src/background_dispatch.rs:98-106,387-405`
- `apps/cli/src/commands/bg_worker.rs:171-223`
- `apps/cli/src/commands/bg_worker.rs:288-292`

影响：sandbox、权限、模型/runtime 选项与 foreground session 不一致，属于权限边界和行为连续性问题。

### H4. Background fork/resume 没有保存 agent prompt 与 tool policy

**结论：TRUE / High**

background snapshot 保存 conversation、model 与少量 metadata，但没有保存 active agent 定义、system prompt、allowed/disallowed tools 等权限上下文；forker 收到 `system_prompt` 后仍明确忽略。worker 的 cold seed 只恢复 history/model/runtime 的子集。

源码证据：

- `apps/cli/src/bg_session_forker.rs:55-66`
- `orchestrator/src/bg_snapshot.rs:10-17,62-123`
- `apps/cli/src/run.rs:1968-2010`

这与 Claude Code 2.1.216 官方 changelog 中“恢复 background agent prompt/tool restrictions”的修复方向直接对应。

影响：进入后台后可能扩大或改变 agent 权限，也可能丢失原本的行为约束。

### H5. Managed worktree 删除仍依赖路径启发式，未验证 ownership token

**结论：TRUE / High**

daemon roster 已经保存 worktree ownership token，但 `rm` 路径没有用它验证归属；删除主要依赖路径判断并直接递归移除。dirty branch、live lock 等安全原因在当前控制流中没有形成完整的拒绝门槛。

源码证据：

- `apps/cli/src/daemon_roster.rs:177-179,269-277`
- `apps/cli/src/commands/rm.rs:93-107,159-200`

影响：存在删除错误 worktree 或用户数据的风险，尤其是在并发 background worker 与跨项目路径复用时。

### H6. Workflow 与 scheduled-task 写入仍可能跟随 symlink

**结论：TRUE / High**

scheduled task 与 workflow 保存路径使用普通目录创建和文件打开流程，没有对路径组件/最终文件执行拒绝 symlink 的安全检查，也没有 `O_NOFOLLOW` 等等价保护。

源码证据：

- `tools/cron/src/schedule_cron.rs:806-840`
- `tools/cron/src/cron_delete.rs:197-220`
- `cron/src/scheduler.rs:474-484`
- `tools/workflow/src/lib.rs:333-386`

Claude Code 2.1.216 changelog 明确修复了 workflow saves 和 scheduled-task writes 跟随 `.claude` symlink 的问题。

影响：恶意或意外 symlink 可使写入/覆盖发生在预期目录之外，属于文件系统安全漏洞。

### H7. 公开 MCP CLI 子命令仍包含显式 stub

**结论：TRUE / High**

`serve`、`login`、`logout`、`add-from-claude-desktop` 等公开命令仍走 placeholder/stub 分支，而 Claude Code 对应命令已经可用，例如 `claude mcp login --help` 提供完整 OAuth 参数与 `--no-browser`。

源码证据：

- `apps/cli/src/commands/mcp.rs:301-331`

影响：公开 surface 存在但不工作，脚本兼容性和实际 MCP 管理能力都不对等。

### H8. `/goal` 宣称有 stop-hook enforcement，但实际只存状态

**结论：TRUE / High**

命令和 directive 文案承诺在 goal 完成前阻止退出，但实现注释明确说明 enforcement 尚未接线；orchestrator handle 只更新 goal 状态，没有形成停止门槛。

源码证据：

- `commands/core/src/goal.rs:48-61,133-141`
- `orchestrator/src/handle_impl.rs:171-195`
- `commands/core/src/register.rs:299-379`

影响：这是可观察的行为承诺失真。用户可能以为长任务有完成保障，但进程实际上可以正常结束。

### M1. Background resume 可能从错误 worktree/cwd 查找 transcript

**结论：TRUE / Medium**

forker 使用调用者当前 cwd 创建 background spec；worker 恢复时忽略 `Launch::Resume.transcript_path`，转而用 `spec.cwd + session_id` 重新查找 transcript。

源码证据：

- `apps/cli/src/bg_session_forker.rs:120-128`
- `apps/cli/src/commands/bg_worker.rs:243-271`

影响：从其他 worktree 或目录 resume 时可能加载失败，或命中错误的同名 session 上下文。

### M2. Background cold seed 没有完整恢复 compact/deferred-tool metadata — **CLOSED**（见 §0 表；本节保留为修复前证据）

**结论：TRUE / Medium**

标准 resume 会恢复 `preCompactDiscoveredTools` 等 compact 前状态；background/TUI cold seed 手工重建时只恢复子集，并且 background snapshot 也不完整保存 effort、compact boundary 与相关策略状态。

源码证据：

- `orchestrator/src/resume.rs:227-250,378-387`
- `orchestrator/src/bg_snapshot.rs:10-17,62-123`
- `apps/cli/src/run.rs:1968-2010`

影响：compact 后的延迟 tool discovery、effort 和恢复语义可能与原会话不一致。

### M3. 缺少 `sandbox.filesystem.disabled` 配置语义

**结论：TRUE / Medium**

当前 sandbox runtime config 和 policy merge 只建模 allow/deny/read/write 等规则，没有 Claude Code 2.1.216 新增的 `sandbox.filesystem.disabled` 开关。

源码证据：

- `sandbox/src/runtime_config.rs:137-159,201-230,361-378`
- `sandbox/src/policy_convert.rs:260-287`

影响：无法用官方同名配置明确关闭 filesystem sandbox，迁移配置时会产生静默语义差异。

### M4. AskUserQuestion 缺少自动 “Other”/free-text 交互

**结论：TRUE / Medium**

tool schema 文案承诺自动提供 Other，但模块注释和 TUI widget 都只支持固定选项；非交互 resolver 还可能直接落到首选项。

源码证据：

- `tools/ui/src/ask_user_question.rs:45-52,267-302,605`
- `tui/src/bottom_pane/ask_user_question_view.rs:167-205`

Claude Code 2.1.216 还专门修复了 AskUserQuestion 自由文本输入时的中立 wording，说明自由文本是正式交互的一部分。

### M5. 多个公开 CLI flags 只解析、不生效

**结论：TRUE / Medium**

- `--betas` 有 TODO，runtime 构建请求时仍硬编码为空。
- `--plugin-url` 只存在于 argv/tests/comments，没有运行时消费者。
- `--chrome`、`--no-chrome`、`--file` 可解析，但没有接入启动/会话流程。

源码证据：

- `apps/cli/src/argv.rs:309-317,359-362,554-571`
- `apps/cli/src/run.rs:256-261,995-997`

这些 flags 同时出现在 Claude Code 2.1.216 的公开 help 中，因此不是纯内部差异。

### M6. 缺少 Claude Code 的 remote-control 公共 surface

**结论：TRUE / Medium**

Claude Code 当前公开 help 包含 remote-control 相关能力，LingXi 的 CLI 没有对等命令/flags；feature 表也显示该方向被移除或禁用。

源码证据：

- `apps/cli/src/argv.rs` 与 `apps/cli/src/commands/mod.rs` 中不存在对等 surface。

这是“有意不实现的官方能力”，仍属于 parity gap；它不是 LingXi 的项目特定 feature，因此不能按排除项消掉。

### M7. OpenTelemetry/Prometheus 仍是配置骨架，不是完整 exporter

**结论：TRUE / Medium**

当前 OTEL 模块能够读取环境配置并构造 record 类型，但没有看到完整 provider/exporter 初始化和稳定的运行时记录链路。Claude Code 2.1.216 changelog 继续维护 Prometheus endpoint 的 metric unit，说明这是一项实际产品能力。

源码证据：

- `telemetry/src/otel/mod.rs:43-52,84-99`
- `telemetry/src/otel/record.rs:22-52`

影响：配置看似支持但不会产生对等 telemetry 输出，运维监控行为不一致。

### M8. 运行中 reload plugin 后，TUI command completion 可能保持旧快照

**结论：TRUE / Medium**

TUI bottom pane 初始化时快照 registry commands；`/reload-skills` 会显式重新同步这份快照，但 `/reload-plugins` 只刷新 engine registry 并发送 SystemNotice，没有同步 TUI command catalog。

源码证据：

- `tui/src/bottom_pane/mod.rs:253-300`
- `tui/src/chat_widget.rs:2957-2983`
- `apps/cli/src/mode.rs:1245-1290`

Claude Code 2.1.216 changelog 明确说明会话中新增/删除的 commands 与 skills 应立即出现在 `/` autocomplete，无需重启。

### L1. `--fallback-model` 只支持单值，不支持官方列表语义

**结论：TRUE / Low**

Claude Code help 把 fallback model 定义为逗号分隔列表；LingXi 使用单一 `Option<String>` 并按单模型向下传递。

源码证据：

- `apps/cli/src/argv.rs:85-96`

影响主要是极端 provider/model failure 时的回退兼容性，不阻塞常规会话。

## 4. 部分成立，不能表述为“完全缺失”

### P1. Main-thread agent restore 已实现，但按当前 catalog 名称重解析

**结论：PARTIAL / Medium**

main thread resume 会从 JSONL 读取 agent 名称，并从当前 agent catalog 恢复 prompt/tool restrictions，所以“agent restore 完全缺失”是错误结论。

但 JSONL 只保存名称，没有保存不可变的 agent definition snapshot。如果同名 agent 在会话后被修改，resume 会采用新定义而非原定义。仅凭公开 changelog 无法证明 Claude Code 是否持久化完整不可变 snapshot，因此这里保留为风险和部分 gap，不升级成已证实的高危 parity 结论。

源码证据：

- `apps/engine-desktop/src/lib.rs:7108-7215`
- `session/src/jsonl/writer.rs:153-175`
- `session/src/jsonl/loader.rs:753-767`

### P2. Tool heartbeat 已产生，但主 CLI/TUI adapter 没有传播该事件

**结论：PARTIAL / Low-Medium**

orchestrator 会发 tool heartbeat；client adapter 会转发，但 primary TUI bridge、普通 CLI output adapter 和 stream-json adapter 没有 override 默认 no-op hook。另一方面，TUI 本身有整体 turn elapsed spinner，因此不能说“UI 完全冻结”。

源码证据：

- `orchestrator/src/turn_loop.rs:2949-2973`
- `platform-api/src/orchestrator.rs:1529-1543`
- `client-adapter/src/output_stream.rs:128-136`
- `tui-core/src/orchestrator_bridge.rs:252-330`
- `apps/cli/src/output_adapter.rs:28-64`
- `apps/cli/src/stream_json.rs:893-970`
- `tui/src/chat_widget.rs:3435-3475`

## 5. 真实存在，但不是 Claude parity gap

### I1. project/local `pluginConfigs` 的 CLI 写入与 runtime 读取规则冲突

**结论：INTERNAL BUG / Medium**

CLI 的 `plugin install --config` 可以在 project/local scope 持久化配置，但 runtime 按安全策略明确忽略这些 scope。

源码证据：

- `apps/cli/src/commands/plugin_install.rs:286-305`
- `apps/engine-desktop/src/lib.rs:3062-3070`

Claude Code 2.1.207 官方变更明确规定：`pluginConfigs` 不再从项目 settings 读取，只允许 user、`--settings` 和 managed settings。因此旧建议“让 loader 读取 project/local 配置”是错误修复方向，会重新引入被官方关闭的信任边界。

正确方向应是：CLI 在 project/local scope 拒绝、重定向或不写 `pluginConfigs`，并给出清晰错误，而不是扩大 runtime loader 的读取范围。

## 6. 已撤回、已修复或证据不足的旧结论

### R1. “Streaming 413/prompt-too-long 不会自动 compact/retry”——撤回

conversation streaming path 已有 reactive prompt-too-long recovery。定向测试 `streaming_connect_413_recovers_via_reactive_ptl` 实际通过：1 passed，0 failed。

证据：

- `orchestrator/src/conversation.rs:6468-6545`
- `orchestrator/tests/streaming_vs_batched_equivalence_test.rs:226-269`

`orchestrator/src/conversation.rs:6171-6178` 的旧注释与后续实现不一致，应视为 stale comment，而不是功能缺失证据。

### R2. “已确认缺少 Claude 2.1.216 的 O(N²) normalization 修复”——降为待性能分析

当前实现每 turn 会 clone/遍历增长的 history，因此整个长会话的累计工作量可能呈二次增长；但具体 normalization、tool pairing、media stripping 实现本身主要是线性扫描。这不能证明它复现了 Claude Code 2.1.216 changelog 所指的同一个 accidental quadratic normalization bug。

证据：

- `orchestrator/src/turn_loop.rs:336-340`
- `orchestrator/src/conversation.rs:6181-6185`
- `llm-runtime/src/convert.rs:41-42,99-166,219-403`
- `llm-runtime/src/service.rs:838-844,2786-2837`

该项可以作为性能 profiling/benchmark 候选，但在有 flamegraph 或相同 workload 的基准对比前，不再计入 confirmed parity gap。

### R3. “CLI `/compact` 一秒结束，说明根本没执行”——撤回

`/compact` 会进入 real manual compaction 流程；执行很快可能是上下文较小、provider 返回快或压缩收益有限，不能用耗时判断是否执行。

证据：

- `apps/cli/src/mode.rs:685-708`
- `commands/core/src/compact.rs:38-72`

仍存在一个较弱的诊断差异：proactive autocompact failure 当前只记录 warning，用户不一定看到失败原因（`orchestrator/src/conversation.rs:3381-3388`）。该行为值得改善，但没有足够官方黑盒证据把它单列为 confirmed parity gap。

### R4. “Main-thread resume 完全不恢复 agent”——撤回

恢复逻辑存在，并会按 agent 名称重新加载当前定义；剩余问题已降级为 P1，而不是“完全缺失”。

### R5. “Runtime 应读取 project/local pluginConfigs”——撤回

官方 2.1.207 明确禁止该读取路径。真实问题是 CLI 不应把相关配置写入不会被读取的 scope，见 I1。

### R6. “LingXi 仍拒绝所有 live background attach”——缩小表述

attach transport 已存在，可以连接和接收事件。真实 gap 是没有完整 PTY、resize/control、跨平台和公开 `attach` surface，见 H1。

### R7. “iTerm2 resize 后 bottom UI 仍是开放 gap”——撤回

当前 resize/reflow 路径已有针对 viewport bottom pin 和 leading blank repaint 的回归测试，定向测试通过。因此没有新的复现证据时，不应继续把旧截图当作当前 bug。

证据：

- `tui/src/terminal.rs:300-342,482-504,1282-1393`
- `tui/src/app.rs:2149-2203`

### R8. Bridge `CommandsChanged` production wiring inert——从 parity 计数排除

bridge production boot 确实没有连接 push channel，但 bridge 是项目特定 surface；按本次范围规则，不计入 Claude Code parity gap。核心 TUI 的动态 plugin command refresh 问题已经单独记录为 M8。

证据：

- `apps/bridge-server/src/boot.rs:428-438`
- `apps/bridge-server/src/router.rs:413-432`

## 7. 建议修复顺序

1. **安全与数据完整性**：H5 worktree ownership、H6 symlink-safe writes。
2. **background 权限连续性**：H3、H4，再处理 M1/M2。
3. **完整 live session**：H1、H2，以真实 PTY/socket 为核心，而不是继续扩展事件流模拟层。
4. **公开但不可用的 surface**：H7 MCP stubs、H8 `/goal` enforcement。
5. **近期官方配置/交互 parity**：M3 sandbox disabled、M4 AskUserQuestion、M8 dynamic commands。
6. **CLI 与 telemetry 完整性**：M5、M6、M7、L1。
7. **部分项与内部一致性**：P1、P2、I1。

每项修复应先加失败回归测试。涉及文件删除、symlink、permission mode、agent tool policy 的测试必须覆盖恶意输入与跨 worktree 场景，而不只是 happy path。

## 8. 复核与验证记录

### 8.1 官方与黑盒基线

- `claude --version` → `2.1.216 (Claude Code)`
- `npm view @anthropic-ai/claude-code version --json` → `"2.1.216"`
- 官方 changelog：<https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md>
- 检查过 `claude --help`、`claude attach --help`、`claude daemon --help`、`claude mcp login --help` 的公开 surface。

### 8.2 构建与定向测试

- `cargo check -p cli -p sandbox -p orchestrator -p llm-runtime -p command-core -p tool-ui -p tool-cron -p memory -p tool-web -p tool-lsp`：通过，仅有既有 warnings。
- `cargo check -p bridge-server -p engine-desktop -p cron -p mcp -p plugin`：通过，仅有既有 warnings。
- `cargo test -p orchestrator --test streaming_vs_batched_equivalence_test streaming_connect_413_recovers_via_reactive_ptl`：通过，1 passed，0 failed。
- `cargo test -p tui terminal_resize_keeps_bottom_pinned_viewport_at_new_bottom`：通过。
- `cargo test -p tui viewport_shrink_repaints_leading_blank_columns`：通过。

### 8.3 报告边界

- 本轮没有修改产品源码、配置或测试，只生成本报告。
- 没有把“存在 TODO”“代码结构不同”自动判为 parity gap；每个 TRUE 项都要求有用户可观察、安全或配置语义差异。
- 对无法从闭源 Claude Code 内部实现验证的事项，使用黑盒行为或降为 PARTIAL/待验证，而不是宣称 byte-level 已证明。
- 本报告保证的是所列结论的证据强度，不宣称未列出的功能域已经自动达到 100% parity；后续 Claude Code 版本变化也需要重新建立基线。
