# Claude Code 2.1.245 主流程字节级对齐审计 — 2026-08-25

## Oracle

- 可执行文件：`/Users/luolingfeng/.local/share/claude/versions/2.1.245`
- `claude upgrade`：`Claude Code is up to date (2.1.245)`
- 本项目版本源：`platform_api::CLAUDE_CODE_VERSION = "2.1.245"`

本文件是当前目标的增量收口记录。旧的 2.1.220 fixture 和 2.1.238 审计保留为历史证据，不改写其捕获版本。

## 覆盖结论

| 子系统 | 2.1.245 对照结果 |
| --- | --- |
| 主循环 | `PostToolBatch`、rapid-refill breaker 和 memory prompt 文案已存在；`gitStatus` 固定为会话第一次探测的快照，后续 worktree/cwd 切换只更新动态 cwd，不再重探并改写“start of conversation”附件。 |
| Agent / Task | background agent 的终止、worktree 清理、rest 通知和 workflow task 终态按真实 task state 收口；创建者 name/team/id 已进入公共 `TaskStateBase`，workflow/monitor/MCP 等非 agent 子任务也会阻塞父 agent 的 rest 通知；LocalAgent 在 spawn 返回但路由表尚未登记的窗口会先 stop 内层 runner，再清理路由表。 |
| File | Grep 保持 grep-regex/ripgrep 默认限制（nest 250、size 100 MiB、DFA 1000 MiB）；撤回会拒绝 Claude 合法输入的 50/10 MiB 私有上限，以及只存在于 Edit、却误加到 Write 的 1 GiB 上限。 |
| Web | URL safety 注释与实际调用链一致；WebSearch 的 LingXi 实现仍属于产品级明确差异，不伪装成 Claude 后端。 |
| Shell / Sandbox / Permission | `/bin/sh -c` hook 执行、permission layer folding、frozen command deny 和 Bash sandbox override 均有生产消费者；移除了 POSIX runner 私有的 8 MiB stdout/stderr 截断，完整输出继续交给上层 Claude-compatible 展示/持久化逻辑。 |
| Compact / Session | 正常路径仍是 snip → microcompact → autocompact。JSONL cold loader 已恢复 session/agent `content-replacement`、`marble-origami-commit/snapshot/reset` 路由；branch 按 2.1.245 `createFork` 顺序继承 history-suppression、messages、聚合 replacement、同 cwd relocated、ATIS 和 title；prompt history 不再用 256 KiB 尾窗丢弃超大最新记录。 |
| Memory | 当前本地 memory prompt 的组织、引用前验证、保存前去重文案与 2.1.245 对照；Anthropic storageV5/远端共享 memory 后端继续作为明确产品差异。 |
| Workflow | 语法错误不再注册伪 task；queued 只在预检通过后发布；并行上限外的 agent 保持 queued 并在 slot 释放后启动；终态/usage/progress 通知分离；Large workflow warning 已补齐 `tengu_ochre_gantry` 禁用门、remote agents/tokens 阈值优先级，且只对 live row 发一次 telemetry。 |
| CLI / Fixture | 当前版本标识测试已从历史 2.1.220 fixture 拆到独立 2.1.245 测试；cost `querySource` 同时锁定 `repl_main_thread` 与 SDK 的 `sdk`，并覆盖 OTEL transport 映射。 |

## Plugin eval 增量对齐

- eval manifest 的搜索顺序已锁定为 `.claude-plugin/plugin.json` 优先、根目录 `plugin.json` 回退；只有 `.lingxi-plugin/plugin.json` 时按 oracle 拒绝。`experimental.evals` 支持嵌套相对路径和路径归一化，绝对路径、类型错误和顶层 `evals` 均按 2.1.245 的 warning 文案回退到 `evals/`。
- `plugin eval init --bare` 已对根 manifest、`.claude-plugin` manifest 和品牌目录拒绝路径做 oracle 字节比较。`plugin eval __mock-server --spec ...` 也保持 oracle 的 unknown-option 行为；内部 mock runtime 不暴露隐藏 CLI surface，而是通过仅供子进程使用的私有环境变量启动。
- `--mocks record` 现在会把待测插件复制到隔离 stage，剥离 stage 中的真实 MCP 声明，并通过严格 MCP config 注入 mock server；`--mocks off` 保留真实 MCP。进程级探针证明 record 模式没有启动会触碰 marker 的真实 MCP，off 模式会启动它。
- mock 发现支持 suite/case 分层、`_tools.json` schema、shadow/standalone server、固定 markdown responder、受限输入断言、fixture 模板、MCP `isError`、unmocked 统计、逐 server 私有 JSONL call log 和中止原因。oracle quickref 标记为后续能力的 agent responder / `_server.md` 未提前实现。
- `--max-cost-usd 0` 的退出码、`partial`、`partialReason=cost_ceiling`、空 cases 和全零 aggregate 已与 oracle 对照；非法预算文案、普通 run failure 与 top-level partial 的区别、`aggregates.meanDelta` 也已补齐。

仍未宣称 plugin eval 全面字节相同：本机无法在无外部认证/计费依赖的条件下让 oracle 和端口各完成一次真实模型驱动的 mock tool call；当前证据由单元测试、进程级 MCP 隔离探针和可离线 oracle CLI 比较组成。另外，Clap 生成的 `plugin eval --help` 在排版、usage 大小写和帮助子命令展示上仍与 Claude 自定义 help renderer 不同。

## 本轮发现并修复的主流程回归

- `llm-runtime` 的 retry 环境读取不得用进程级 `OnceLock` 固化。Claude/JS 的 `process.env` 是逐请求读取的，而且 provider fallback 测试会在请求之间更新 `USER_TYPE` 等变量；缓存导致 repeated 529 被错误判为不可重试。现已恢复每次构建 retry 环境时读取实时进程变量。
- `try_run_turn_streaming` 的 async state machine 很大。三个公开 streaming wrapper 原先把它直接嵌入外层 future，默认测试线程栈下会 stack overflow；现只在这三个入口 `Box::pin` 内层 future，保持控制流、取消和 task-local scope 不变，同时缩小外层 future frame。独立 code review 未发现新的高/中风险问题。

## contextCollapse 边界

对 2.1.245 可执行文件做精确字符串计数：`CLAUDE_CONTEXT_COLLAPSE` 和 `CLAUDE_CONTEXT_COLLAPSE=` 均为 0；只有 `CLAUDE_CONTEXT_COLLAPSE_MODEL` 命中 6 次。此前把前者当作命中，是后者包含该子串造成的误报。二进制保留 `marble-origami-*` 持久化记录，但 `recoverFromOverflow`、`tryReactiveCompact`、`isContextCollapseEnabled` 和 `CLAUDE_CODE_REACTIVE_COMPACT` 也均为零命中，当前发布版没有足够证据证明存在已启用的 collapse engine。

因此本轮只实现可从 oracle 证明的 JSONL load/reset/fork 语义，不凭空实现 collapse 算法。若未来发布版真正启用该路径，需以新二进制重新提取算法和 prompt 后单独补齐。

## 明确差异

- Anthropic storageV5 / 远端共享 memory 同步协议。
- remote agent/workflow、CCR 和云端 session 后端。
- LingXi 的多模型、移动端、local-apps 和自有 WebSearch 产品面。

这些差异不应被“空实现”或伪成功结果掩盖。

## 验证证据

- `cargo test -p cli --lib`：998 passed，0 failed；plugin eval 定向回归：35 passed，0 failed。
- `cargo build -p cli --bin lingxi-cli`：通过。
- 主流程 14 包跨域命令：`orchestrator agent tool-agent tasks task-store tool-file tool-web tool-shell sandbox-runtime permission compaction workflow memory tool-workflow` 全部通过；其中 orchestrator lib 960 项、三个 streaming 集成组各 5 项均在默认栈设置下通过。
- `parity_surface.py`：55 command paths、128 long flags，oracle-only flags 0。
- `parity_behaviour.py`：writes identical 5/diff 0；behaviour identical 12/diff 0/skipped 0。
- auto-mode fixture：oracle 与端口 SHA-256 均为 `e836600bf411eb5f52852ae2db7ee5b512edc404c54c429a7f8190ff881a7df2`。
- `cargo fmt --all --check`、`git diff --check`：通过；客户端设备端测试不在本机覆盖范围内。
