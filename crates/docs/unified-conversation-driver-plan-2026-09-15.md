# Unified Conversation Driver

**状态：** Review-corrected draft — 已修正第二轮源码审阅发现的契约错误；实施仍受 §9 准入条件约束  
**日期：** 2026-09-15  
**文档路径：** `lingxi-code/docs/unified-conversation-driver-plan-2026-09-15.md`  
**范围：** `lingxi-code/orchestrator` 主对话循环  
**公共 API：** 保持 `ConversationOrchestrator` / `OrchestratorHandle` 方法名、签名及返回语义

---

## 0. 阅读说明

本文是行为等价重构规格。结束行为按分支、调用条件和副作用顺序描述；路径之间现有差异需保留，后续语义修复另开 PR。

本次修订已明确：

- Stop hook 的 LoopAgain、block cap、Prevent、max-turns 是不同结果。
- Recovery 的可重试状态与耗尽终态分开处理。
- Wakeup 消费必须保留短路谓词，以及与其他结束信号的优先级。
- Streaming 取消分入口预取消、准备错误、loop-top、stream/tool abort 和晚到取消，不统一添加结束事件或收尾。

矩阵定义迁移必须保护的行为，不代表所列测试已经执行或通过。

---

## 1. 决策摘要

采用 **一个外层 `ConversationDriver`，两种模型步策略 `BatchedRound` / `StreamingRound`。**

共享每轮准备、结束判定及循环脚手架；保留真实的传输、取消、事件和入口差异。共享函数不意味着必须由外层调用：准备逻辑只有一份，但其执行边界由策略控制，确保 batched 取消仍覆盖准备阶段。

本系列是行为等价重构。现有路径之间的差异先记录、测试并保留；修复历史差异另开行为变更 PR。不能以“统一循环”为由改变取消优先级、输入消费、Stop hooks 或结束事件顺序。

### 目标

1. 将重复的 reminder、compaction 前置及 prompt snapshot 接线收敛到共享实现。
2. 共享循环状态和结束处理，降低新功能只接入 batched 的风险。
3. 保留 print / structured output / test-harness 的非流式调用，以及桌面、移动、TUI 的流式调用。
4. 每个 PR 小范围迁移、可独立验证及回滚。
5. 迁移前建立关键双路径回归测试，而不是迁移结束后补门禁。

### 非目标

- 删除 batched，或统一 HTTP/SSE 传输。
- 合并子代理的 `run_subagent_loop`。
- 迁移 stdio REPL 的输出体验。
- 一次性重写或拆分整个 `turn_loop.rs`。
- 修改对外 outcome 形状、权限、hook payload 或 JSONL 格式。
- 本系列顺带修正原有路径之间的行为差异。

---

## 2. 当前结构

| 公共入口 | 外层 | 内层 |
|---|---|---|
| `run_turn` | `try_run_turn` | `execute_one_turn_with_recovery_tracked` |
| `run_turn_with_cancel` | `try_run_turn_cancelable` | 同一 batched 模型步，整步取消赛跑 |
| `run_turn_streaming*`、queued batch、rewake | `StreamingTurnDriver::run` | prepare / open / pump / finalize |

已共享：`handle_stop_at_end`、`take_lone_wakeup_turn_end`、工具 dispatch、`prepare_model_call_snapshot`。剩余风险来自重复调用点和外层决策。

历史上 `ScheduleWakeup` 曾只接到 batched。收敛共享入口有价值，但三条循环并非等价，必须先锁定差异。

stdio REPL（`apps/cli/src/repl.rs`）走 `run_turn_with_cancel`。CLI TUI / bridge / mobile 主回合走 streaming。

---

## 3. 不可破坏的行为契约

### 3.1 取消覆盖范围

Batched 的取消赛跑必须覆盖：

```text
prefetch / compaction / snapshot / reminder
→ provider 请求及恢复
→ 本轮工具处理
```

`BatchedRound.run` 在有 token 时为 `select(run_step(), cancel)`，其中 `run_step = prepare_turn_step + execute_batched_prepared`。禁止外层先 await 准备再进入策略。

Streaming 保持 token 传入准备、provider 和 `StreamingToolExecutor`。禁止外层 `select!` 丢弃整个 streaming future（`run_turn_streaming_inputs_locked` 注释 DEFERRED-3 已说明旧行为会丢掉 in-flight 工具结果）。

`parentAborted` = `token.as_ref().is_some_and(is_cancelled)` 的**实时**值，不是“是否存在 token”。无 token 的 `run_turn` 为 false；存在但未取消也为 false。Cancelable batched 在 Stop 处用 `cancel.is_cancelled()`（`3739`）。

`CancelReason::QueueNowCommand` 继续抑制普通 interrupt message。

### 3.2 取消 outcome 映射（保留入口及取消阶段）

| 路径 / 阶段 | 内层行为与事件 | File-history epilogue | 对外映射 |
|---|---|---|---|
| Batched cancelable：loop-top | 注入 interrupt（除非 Now），无 `emit_end_turn`，直接返回 | 无 | `TurnOutcome::Cancelled` |
| Batched cancelable：整步 `select!` 取消 | 丢弃 `_tracked`，注入 interrupt（除非 Now），无 `emit_end_turn` | 无 | `Cancelled` |
| Streaming 主入口：命中 `run_turn_streaming_inputs_locked` 的预取消检查 | 中止 startup prewarm、关闭 Responses websocket；不进入 driver，不发 `aborted_*` 结束事件 | 无 | `Cancelled` |
| Streaming：准备阶段 vision delegation 取消 | `VisionDelegationCancelled` 经 `prepare_iteration(...).await?` 直接传播；不经过 `aborted_*` 结束事件分支 | 跳过 | 主 streaming 包装在 token 已取消时映射为 `Cancelled` |
| Streaming：loop-top guard | emit `aborted_streaming`，按 Now 规则决定 interrupt，break | 执行 | 内层 EndTurn；可取消包装映射为 `Cancelled` |
| Streaming：post-drive stream/tool abort | 完成必要工具结果处理，消费对应 pending tool-result end 标记；emit `aborted_streaming` 或 `aborted_tools`，按 Now 规则注入对应 interrupt，break | 执行 | 内层 EndTurn；可取消包装映射为 `Cancelled` |
| Streaming：成功返回后晚到取消 | 不重新执行内层，不补发 abort 事件；保留包装层既有清理 | 取决于内层已经走过的分支 | 主 streaming 包装可将成功 EndTurn / StopHookPrevented 映射为 `Cancelled` |
| Batched：成功返回后晚到取消 | 不追加一次 token 检查 | 无 | 保留已完成的 EndTurn，不覆盖成 Cancelled |

主 streaming 包装仍优先将 `MaxTurnsReached` 映射为 `MaxTurns`；其他错误不因 token 已取消就一律变成 `Cancelled`。`VisionDelegationCancelled` 是当前明确的条件映射分支。

表中“入口预取消”指命中该检查时的行为：queued admission 有自己的更早返回分支，不得重排。Rewake 等独立包装的预检查、结束事件及错误映射按其原实现单独验证，不从主 streaming 包装推导。

内部结果保留 `Result` 错误通道。`Continue / FinishThroughEpilogue / ReturnDirect` 描述成功控制流，不能吞掉原有直接传播错误及其资源清理。

### 3.3 准备一次，重试复用

每个模型步只收集一次有副作用的 reminder。PTL retry、重新 snapshot、stream fallback 必须复用本步已收集结果。

- **新模型步：** prefetch、compaction 前置、consume-once reminder。
- **同一步请求重建：** 从最新 history 重建，复用 reminder；不得再消费通知或推进去重。

task completion 是持久消息：只写入 history/JSONL 一次。date-change 的 commit 时点保持“已送达”而非“已计算”。

PR-1 抽取 reminder 时：以函数调用图为准，不以注释为准。`turn_loop.rs:541` 写 todo body RAW、`collect_turn_reminders:367` 写 `<system-reminder>`，但两边都调用 `todo_reminder_message()`，该函数已经包 envelope（`reminders.rs:627`）。注释是过时的，不是行为差。

`in_human_turn`：batched 入口显式传 `true`；streaming 传真实值。

### 3.4 Token accounting

所有产生可计数输出的模型步必须在 Stop hooks / budget continuation **之前**累计 output tokens，含自然结束轮。

当前：

- Batched：`_tracked` 返回后、match 前累加（`2636` / `3709`）。
- Streaming：`finalize_iteration` 内累加（`1591`），早于 `decide_streaming_disposition`。

统一后：**只在 `strategy.run` 返回之后、`apply_step_disposition` 之前累加一次**，并删除 finalize 里的旧写入。预算累计 ≠ cost tracker ≠ output token pool。

### 3.5 顶循环顺序 — 按入口兼容规则，不合成一种

```text
enum LoopGuardOrder {
    Batched,            // drain → max_turns → budget → increment
    BatchedCancelable,  // cancel → max_turns → budget → increment（无 drain）
    Streaming,          // drain → max_turns → budget → increment → cancel
}
```

限额检查、drain、cancel 处理可以是共享函数；**调用顺序**由 `LoopGuardOrder` 选择。`max_turns` / budget / cancel 同时成立时，以该入口现有优先级和返回类型为准。

### 3.6 结束事件、late drain、收尾

必须分开表达：

1. 是否跑普通 Stop / StopFailure / advisory Stop。
2. 是否允许 budget continuation。
3. 是否 late drain，以及相对 `emit_end_turn` 的顺序。
4. 退出循环后跑 epilogue，还是直接返回。

循环动作只有三类：

```text
Continue
FinishThroughEpilogue(outcome)  // break 到循环后收尾
ReturnDirect(outcome)           // 循环内 return，跳过收尾
```

**File-history 今天只存在于 streaming `run()`：** 回合开始 `make_snapshot`（`1950`），循环后 `append_file_history_snapshot`（`2248`）。`try_run_turn` / `try_run_turn_cancelable` **没有**对应收尾。PR 5 不得给 batched print/REPL 新增 file-history，也不得让 streaming 的 `ReturnDirect` 突然获得收尾。

Streaming 已 `emit_end_turn` 之后的 `Complete` 仍可能 late drain 成功并 `continue`——这是现网行为，必须保留（自然结束与 lone wakeup 都走 `Complete`）。

---

## 4. 结束行为矩阵

PR 4 开始前这些行都要有可执行测试。差异保留，不以统一后的“更干净”反写。

| 场景 | 路径 | Stop | Budget | Late drain | 退出 | 已 emit_end_turn？ |
|---|---|---|---|---|---|---|
| 自然 `end_turn` | Batched / cancelable | 普通 Stop | 现有 gate（须 `allow_budget_continuation && stop_reason=="end_turn"`） | 无 | FallThrough 后 emit，然后 return（cancelable 无 epilogue） | 在 disposition 内 |
| 自然 `end_turn` | Streaming | 普通 Stop（`finish_natural_streaming_end`） | 现有 gate | **emit 之后**；成功则 Continue 同一 turn | `Complete` → epilogue | 进入 Complete 前已 emit |
| Lone ScheduleWakeup | Batched | **普通 Stop**（`Ended{end_turn, allow_budget:false, tool_requested_end:false}`） | 禁止 | 无 | 同自然结束 | disposition 内 |
| Lone ScheduleWakeup | Streaming | **跳过普通 Stop** | 禁止 | **emit 之后**，可 Continue | `Complete` → epilogue | 判定内已 emit |
| 工具显式要求结束 | Batched | advisory Stop | 禁止 | 无 | 然后 emit | disposition 内 |
| 工具显式要求结束 | Streaming | advisory Stop | 禁止 | **无**（`ForcedComplete` 不 drain） | `ForcedComplete` → epilogue | 判定内已 emit |
| EndConversation | Batched | 仅在无 hook prevent、无 tool-requested end 时调用 `take_lone_wakeup`；随后 swap end 槽，命中时以 `end_conversation` 优先，**走普通 Stop** | 禁止 | 无 | 同 Ended | disposition 内 |
| EndConversation | Streaming | swap 槽后 `Return(EndTurn)`，**可能未消费 wakeup flag**；**不走 Stop** | 禁止 | 无 | **ReturnDirect，跳过 epilogue** | 此分支仅 `emit_text`；不新增 `emit_end_turn`，入口既有处理保持 |
| PostToolBatch / pre-tool prevent | Batched | `Ended{hook_stopped}` → 普通 Stop（reason 不在 StopFailure 名单） | 禁止 | 无 | Ended | disposition 内 |
| PostToolBatch / pre-tool prevent | Streaming | `emit_end_turn("hook_stopped")` + `Return(EndTurn)`，**跳过 Stop 与 epilogue** | 禁止 | 无 | ReturnDirect | 判定内已 emit |
| blocking_limit / PTL 耗尽 | Batched | `handle_stop_at_end` → StopFailure + FallThrough | 禁止（cancelable 另有 `stop_reason=="end_turn"` gate） | 无 | emit 后 return | disposition 内 |
| blocking_limit | Streaming | prepare 内 `handle_stop_at_end("blocking_limit")` + emit，`Complete` 出 prepare | 禁止 | 随后顶循环不会再跑模型；break → epilogue | FinishThroughEpilogue | prepare 内已 emit |
| Partial finalize | Streaming only | 不走普通自然结束；truncated recovery 则 Continue；否则 emit `model_error` + break | 否 | 无 | epilogue | 终端臂已 emit |
| 入口预取消 / 准备阶段取消错误 | Streaming 主可取消入口 | 不走 Stop | 否 | 无 | 按 §3.2 直接返回或传播错误；跳过 epilogue | 无 `aborted_*` end 事件 |
| Loop-top / post-drive stream/tool abort | Streaming | 不走 Stop | 否 | 无 | `aborted_*` emit + epilogue | 是 |
| Loop-top / mid-step abort | Batched cancelable | 不走 Stop | 否 | 无 | **ReturnDirect `Cancelled`，无 emit_end_turn，无 epilogue** | 否 |
| Stop hook 正常要求续跑 | 两者 | `LoopAgain` | 本次跳过 | 无 | Continue；reset max_tokens recovery | 否 |
| Stop hook block count 超过启用的 cap | 两者 | warning 后 `FallThrough`，不执行 LoopAgain 重置 | 继续原结束原因适用的 gate | 沿原完成路径 | Budget 可续跑；否则走原结束尾部 | cap 分支不发 end，尾部负责 |
| Stop hook Prevent | 两者 | `Terminate(StopHookPrevented)` | 跳过 | 无 | ReturnDirect；cancelable batched 映射为 EndTurn | helper 已 emit |
| Stop hook 要求续跑但已达 max_turns | 两者 | `TerminateMaxTurns`；检查先于 block cap | 跳过 | 无 | 原有 MaxTurns 错误 / outcome 映射；不走 epilogue | 此分支不发 end |
| thinking-only / max_tokens / malformed：仍满足重试条件 | 两者 | 不跑结束 Stop | 跳过 | 无 | 策略返回 Continue；仍经过单一 token 累计和循环动作分派 | 否 |
| max_tokens 恢复耗尽 | Batched / cancelable | 普通 Stop，reason 为 `max_tokens` | 禁止 | 无 | Stop 结果决定续跑或结束；FallThrough 后 emit | 结束尾部 |
| max_tokens 恢复耗尽 | Streaming | 跳过 Stop | 禁止 | emit 后可 drain 并 Continue | Complete → epilogue（未 drain 到输入时） | 已 emit `max_tokens` |
| 第二次 malformed tool_use | Batched / cancelable | 普通 Stop，reason 为 `end_turn` | 禁止 | 无 | Stop 结果决定续跑或结束；FallThrough 后 emit | 结束尾部 |
| 第二次 malformed tool_use | Streaming | 跳过 Stop | 禁止 | emit 后可 drain 并 Continue | Complete → epilogue（未 drain 到输入时） | 已 emit `end_turn` |
| stop_sequence：有可见文本、已用 thinking nudge，或 nudge 被既有 gate 抑制 | Batched / cancelable | 转为 `end_turn`，普通 Stop | 现有 gate | 无 | 同自然结束 | 结束尾部 |
| 同上 stop_sequence 终态 | Streaming | 保留 `stop_sequence`，跳过 Stop | 禁止 | emit 后可 drain 并 Continue | Complete → epilogue（未 drain 到输入时） | 已 emit `stop_sequence` |
| MessageDisplay / mid-stream tools / stream fallback | Streaming only | 策略内 | — | — | 策略产出上述某行 | — |

Lone wakeup **不能**映射成通用 `NaturalEnd`（会给 streaming 加上 Stop），也 **不能**映射成 `ReturnDirect`（会丢掉 late drain 与 epilogue）。

EndConversation 与 wakeup 必须保留**调用条件、消费顺序和最终优先级**：

```rust
// Batched 当前短路条件；不是无条件消费。
!hook_prevent_continuation
    && !tool_requested_end_turn
    && take_lone_wakeup_turn_end(...).await
```

只有实际调用 `take_lone_wakeup_turn_end` 才会 swap wakeup flag；该函数即使判定不是 lone wakeup，也会消费 flag。Batched 随后检查 EndConversation 槽，最终 EndConversation 优先于 hook prevent / tool-requested end。Streaming 先处理这些早退条件，再检查 EndConversation；命中 EndConversation 后不会执行后面的 wakeup 消费。

因此两条路径都可能跳过 wakeup 消费。共享 helper 不得为了清理状态而无条件 drain。测试需覆盖 wakeup、EndConversation、hook prevent、tool-requested end 同时存在的组合及下一轮状态。

对外返回：`run_turn` 保留 `ConversationOutcome::StopHookPrevented`；`run_turn_with_cancel` 把 `Terminate` 映射成 `TurnOutcome::EndTurn`（`3743`，丢失 Prevented 区分）。保留。

---

## 5. 目标模块结构

```text
conversation/drivers/
  mod.rs             入口包装、ConversationDriver、策略适配
  loop_state.rs      TurnLoopState、LoopGuardOrder
  prepare.rs         共享准备、PreparedTurnStep、请求重建材料
  disposition.rs     结束原因、已执行副作用、循环动作
```

`turn_loop.rs` 保留 batched 一轮 API、recovery、工具 dispatch。Streaming open/pump/finalize 暂留原模块。

### 5.1 状态与 ingress

`TurnLoopState` 以 `StreamingTurnState` 为超集：recovery、Stop counters、budget、global tokens、turn count、malformed/thinking-only、last message id、cancel token。

共享构造保留 `query_started_at` 与 `turn_start_output_baseline` 时点。入口追加、UserPromptSubmit 阻止、`begin_output_turn`、state 初始化的相对顺序不得因抽取改变。

`TurnIngress`：prompt、images、message id、`in_human_turn`、transient rewake、queued inputs、user cancel。Queued 逐项 origin/meta；transient rewake 不写普通 user history。

入口包装继续负责 gate、scope、既有 telemetry span 名（`lingxi.orchestrator.turn` / `.cancelable` / `.streaming` / `.streaming.cancelable` 不合并）。

### 5.2 共享准备（由策略调用）

```text
BatchedRound.run(ctx, state):
    无取消：await run_step()
    有取消：select(run_step(), cancel)

run_step():
    prepared = prepare_turn_step(...)
    execute_batched_prepared(prepared, recovery)

StreamingRound.run(ctx, state):
    prepared = prepare_turn_step(..., cancel)   // token 传入 snapshot/prepare
    open / pump / finalize
    返回模型步结果（含 output_tokens，finalize 不再自己累加）
```

`prepare_turn_step` 内容：prefetch → compact → peer inbox → `prepare_model_call_snapshot(path)` → leading context → `collect_turn_reminders` + 持久化 task notifications → wire tools + **唯一** `record_prompt_snapshot_if_needed` → deferred/date-change。

Streaming 在准备后追加 assistant id、MessageDisplay、blocking-limit preempt。

测试 shim `execute_one_turn` / `_with_recovery` 调用同一 `run_step`。生产路径禁止“外层准备一次 + `_tracked` 再准备一次”。

### 5.3 共享 post-tool 边界

`_tracked` 已消费 wakeup / end flags 并跑 PostToolBatch。只转译最终 `TurnStepOutcome` 无法做到“一处消费”。

先定义共享 post-tool 输入：

- assistant message id；工具 ID/名称及顺序
- 已完成的 tool-result end request
- pre-tool / tool prevent
- `post_tool_batch_calls` 与 pre-batch MCP count
- 路径兼容信息（wakeup 调用的完整短路谓词、EndConversation 检查及结束信号优先级）

共享 helper 执行 post-tool hooks 与 consume-once，返回带原因的结果：continue / tool-requested end / lone wakeup / hook stopped / end conversation。策略不再提前消费同一状态。

保留 telemetry → PostToolBatch → forced Stop 的顺序，以及 tool-requested end 优先于 batch prevent。

工具运行和历史持久化仍由现有执行器负责。共享 post-tool helper 在策略整步执行边界内调用：batched 的 PostToolBatch 及标记消费原本属于 `_tracked`，抽取后也必须留在 `select!` 覆盖范围。外层普通/advisory Stop 继续保持原有取消边界。

### 5.4 模型步结果

不要用只有 `Continue / NaturalEnd / Terminal` 的枚举。结果需提供：

- 本步 output tokens（供单一累计边界）
- 有类型的结束/继续原因（含 wakeup、tool end、abort、error）
- message id / 原有 outcome
- 已执行的副作用（是否已 emit_end_turn、是否已跑 Stop）
- 退出方式（Continue / FinishThroughEpilogue / ReturnDirect）

取消是显式内部结果，再按 §3.2 映射到公共类型。

### 5.5 共享 driver

```text
既有 ingress / UserPromptSubmit / begin-output
TurnLoopState + LoopGuardOrder

loop:
    按 LoopGuardOrder 执行 drain / cancel / limits / increment
    result = strategy.run(ctx, state)；错误保留原有清理并传播，不走成功 epilogue
    累加 result.output_tokens 一次
    action = apply_step_disposition(result, state, order)

    Continue                  → 下一轮
    FinishThroughEpilogue     → 保存 outcome，退出循环
    ReturnDirect              → 按原路径直接返回

若该入口有 epilogue：file-history snapshot（仅 streaming）
返回保存的 outcome；包装层再做 TurnOutcome 映射
```

---

## 6. 验证

共享函数单测验证逻辑；**真实入口**测试验证接线。只测共享函数不能证明两边都调用到它。

| 组 | 必须覆盖 |
|---|---|
| Reminder / snapshot | 顺序、consume-once、持久通知、双入口 prompt snapshot |
| Retry / fallback | PTL 重建、stream fallback；通知不重复、reminder 不丢失 |
| Cancel | §3.2 各阶段；准备阶段 vision 取消错误、PostToolBatch 等待取消、QueueNowCommand；assert 事件、websocket 清理、epilogue、返回映射；主入口与 rewake 分开 |
| Guards | 三入口 `LoopGuardOrder`；cancel 与 max_turns/budget 同时成立 |
| Budget | 自然结束轮跨阈值；不漏计、不双计 |
| End semantics | §4 各行；恢复仍可继续与耗尽分开；stop_sequence；有无 pending input；结束信号碰撞与下一轮 wakeup 状态 |
| Hooks | parentAborted；LoopAgain / cap FallThrough / Prevent / TerminateMaxTurns 分开；cap 后 budget；StopFailure、advisory Stop、outcome 映射 |
| Epilogue | streaming 普通完成的 file-history；ReturnDirect / batched **没有** |
| Streaming-only | partial、display、abort 分型、mid-stream tools、blocking preempt |
| Ingress | images、queued batch、meta/non-meta、transient rewake |

双策略 helper 可复用 fixture，但路径特有行为必须保留差异断言。**必须单列 cancelable batched**；仅 Batched/Streaming 两种参数盖不住三条旧外层。

不重写 fixture 去适配意外行为变化。发现应对齐的历史差，拆独立行为 PR。

---

## 7. PR 计划

每 PR：先补该步会破坏的基线测试，再搬代码。仓库适用的 fmt/lint/`cargo test -p orchestrator`（相关包）必须绿。

### PR 1 — Reminder 基线与共享 collector

- 先补双入口顺序、task-notification、consume-once 测试。
- 新增 `prepare.rs`，抽取 `collect_turn_reminders`；batched 显式传 `true`。
- 以函数为准核对 wrapping/顺序；过时注释一并改掉，避免下一轮当行为差。
- 验证 output-style、plan-mode、skill、todo、memory、silent-turn、task-notification。

### PR 2 — 共享准备及 retry 材料

**依赖：** PR 1。

- 先补：准备阶段取消、PTL retry、stream fallback、双入口 snapshot。
- `PreparedTurnStep` 与同一步重建边界写进类型/注释。
- 策略内部执行准备；batched `select!` 包住 prepare+execute。
- 验收：无双准备、无重复通知、无 reminder 丢失，date-change 提交时点不变。

### PR 3 — 共享 `TurnLoopState`

**依赖：** PR 2。

- 三条外层换同一 state 构造，初始化顺序不变。
- 仍保留三条外层循环，便于定位 state 抽取回归。
- 锁定 baseline、计数器重置、token 字段。

### PR 4 — 共享 post-tool 与结束处理

**依赖：** PR 3；§4 矩阵对应测试已存在。

- 先抽 post-tool 输入/结果，再迁消费点，最后接 `apply_step_disposition`。
- 结束原因、已执行副作用、退出动作及错误传播类型落地；Stop cap、恢复耗尽、stop_sequence、信号碰撞均有基线。
- disposition 前统一累计 token，删除 finalize 旧累计。
- 范围过大则按终态族拆子 PR（wakeup+tool-end → 自然结束 → abort/error）。

### PR 5 — 统一外层 driver

**依赖：** PR 4。

- 迁移前锁定三入口 guards、取消/限额竞态、§3.2 各取消阶段、直接传播错误及 epilogue；rewake 等包装单独验证。
- `LoopGuardOrder` 保存三条顺序。
- 入口变成 gate / telemetry / ingress / 策略选择 / outcome 映射。
- 仅 streaming 走 file-history epilogue。
- 跑 orchestrator 包测试、bridge driver、test-harness 抽样。

### PR 6 — 文档和门禁收网

**依赖：** PR 5。

- 更新 `lib.rs`、`conversation.rs` 模块头、`HANDOFF-2026-09-10.md`（“两条 turn loop”改为“一个外层 + 两种策略 + 兼容规则”）。
- 新的每回合行为必须打真实入口（含 cancelable batched，若逻辑在外层）。
- 静态搜索确认 snapshot / wakeup / reminder 消费无残留生产分支。

### 后续可选 — stdio REPL 迁 Streaming

独立 UX，不属于本系列。

---

## 8. 风险与回滚

| 风险 | 控制 |
|---|---|
| 准备阶段退出取消范围 | 策略执行准备；准备阻塞取消测试 |
| Retry 重复消费 | prepare-once / rebuild；恢复请求内容断言 |
| Wakeup 被加上 Stop 或丢掉 late drain | §4 专行；禁止 NaturalEnd/ReturnDirect 偷懒映射 |
| Token 漏计或双计 | 单一累计边界；删 finalize 写入；阈值测试 |
| `Done` 直接返回跳过 file-history | `FinishThroughEpilogue` vs `ReturnDirect` |
| 给 batched 误加 file-history | §3.6 写明今天没有 |
| Guard 顺序改变输入消费 | `LoopGuardOrder` + 三入口竞态测试 |
| EndConversation 改变 wakeup 消费 | 保留完整短路谓词、顺序及结束信号优先级；碰撞测试 |
| cap 或恢复耗尽被误当 Continue | 分开列举 cap FallThrough、TerminateMaxTurns 和 recovery 终态 |
| 准备取消错误被统一成 abort + epilogue | 保留错误传播及入口条件映射；不新增结束事件或收尾 |
| 新抽象仍两处消费 | 先定义 post-tool 输入；静态+入口测试 |
| Streaming 包装把晚到取消映射为 Cancelled，batched 没有 | §3.2；禁止在 batched 最终 return 探测 token |

无 feature flag。每步验证后等价替换。回滚以 PR 为单位；已有后续依赖时需反向撤回或兼容补丁，**不能假设任意中间 PR 都可孤立 revert**。

不新增 crate 依赖。不合并 telemetry 事件名。wakeup telemetry 只在共享消费成功处发射一次。

---

## 9. 实施准入

### 已决定

- 一个外层 driver，两种模型步策略。
- 公共 API 不变；batched 保留；子代理不合并。
- 共享准备放在策略执行边界内。
- retry/fallback 不重新消费本步 reminder。
- 差异先保留；语义修正另行评审。
- 取消 outcome、顶循环顺序、file-history、lone wakeup Stop/late-drain 按 §3–§4 保留。

### 对应 PR 开始前必须完成

- PR 2：准备/重建边界；取消注入点（准备阻塞、请求中、工具中）。
- PR 4：§4 各行及结束信号碰撞有测试；post-tool 的取消边界、输入和退出动作类型已定。
- PR 5：三入口 `LoopGuardOrder`、§3.2 分阶段映射、准备取消错误和 epilogue 基线已定；独立 rewake 包装已核对。

这些是实施准入，不是“无阻塞问题”。

### 最终验收

- 三条公共入口进入同一外层实现，但 `LoopGuardOrder` 不同。
- Reminder、prompt snapshot、post-tool 消费、共享结束判定达到约定的单一实现。
- §4 差异由显式规则与测试保护。
- 准备取消、恢复、budget、wakeup、输入排空、file-history、cancel 映射测试通过。
- 调用方无迁移；权限、输出协议、JSONL 无意外变化。

---

## 9.5 实施状态（2026-09-15 收尾）

标签 `ucd-p1`…`ucd-p4`。全程判据：`cargo test -p orchestrator --tests --no-fail-fast`
90 binaries / 1558 passed / 0 failed，`cargo build --workspace --tests` 干净。

### 已落地

| | 位置 | 生产调用点 |
|---|---|---|
| 每回合 reminder | `drivers/prepare.rs` `collect_turn_reminders` | 1 |
| 回合准备（15 步） | `drivers/prepare.rs` `prepare_turn_step` | 2（每驱动一个） |
| prompt snapshot | `prepare_turn_step` 内 | 1（§5.2 要求的"唯一"） |
| `TurnLoopState` + output-token baseline | `drivers/loop_state.rs` | 1（原 3 处） |
| 顶循环 guards | `drivers/loop_state.rs` `run_turn_loop_guards` | 3（每入口一个） |
| EndConversation 消费 | `drivers/disposition.rs` | 2（每驱动一个） |
| token 累计 | 各入口循环体，同一边界（原 streaming 深一层） | 3 |

§4 四个终态族、§3.2 五个取消阶段、§3.5 三种 guard 顺序、§3.6 epilogue 归属，
全部有测试，且**每条都种雷验证过会红**。

### 统一外层 driver（PR 5 核心）

三条外层现在是同一个形状，就是 §5.5 画的那个：

```text
loop:
    run_turn_loop_guards(order, state)      // §3.5，三种顺序
    （streaming 在这里插自己的 cancel 守卫）
    verdict = 本轮的 round
    match verdict { Continue / 终态 } → 各入口自己命名
```

| | 位置 | 生产调用点 |
|---|---|---|
| streaming 循环体 | `drivers/mod.rs` `run_round` → `StepExit` | 1 |
| batched 循环体 | `drivers/mod.rs` `run_batched_round` | 2（两个 batched 入口） |
| 结束序列（Stop hooks → budget → emit） | `drivers/disposition.rs` `end_of_turn_sequence` | 3（原来三份拷贝） |

§5.4 的三种退出方式（`Continue` / `FinishThroughEpilogue` / `ReturnDirect`）落成
`StepExit`；`TurnEndVerdict` 是结束序列的裁决，跟 `GuardVerdict` 同一套做法——
**报告事件、不报告 outcome**，命名留在各入口，§3.2 的分歧因此仍然看得见。

PR 4 尾账的 `apply_step_disposition` 就落在这两个类型上：它不是一个函数，而是
`end_of_turn_sequence` 裁决 + 各入口一段四臂 match 命名。写成一个"按入口分派"的
函数会把命名收进去，正是 §3.2 不允许的那件事。

#### 之前判"收益为负"是错的

上一轮的测量没错，从测量得出的结论错了。两条 batched disposition 确实是
7 个 hunk，但剥掉注释后语义分歧只有四处，而且其中三处是**同一件事**：两个入口
给终态取了不同的名字。把它们折成一个参数表并不需要七个参数——只需要
"返回事件、让调用方命名"，而这个做法本系列早在 `GuardVerdict` 就已经用过了。
解锁它的不是新信息，是本仓库里已有的一个模式。

#### 抽取过程中补上的三个测试

重构会把测试掏空，也会把结构保证降级成可传错的参数。这次每一步都对**新结构**
重新种雷，三个雷活了下来，对应三个此前从未被驱动过的行为：

| 种的雷 | 补的测试 |
|---|---|
| cancelable 入口的 Stop-hook max-turns 改成跟孪生一样 raise `Err` | `stop_hooks_test::the_cancelable_entry_reports_a_stop_block_at_max_turns_as_an_outcome` |
| cancelable 入口不再上报自己的 cancel token（`parentAborted` 恒 false） | `goal_evaluated_analytics_tests::the_two_batched_entries_supply_different_parent_aborted_flags` |
| 工具请求的结束走完整 Stop-hook 裁决分支 | `stop_hooks_test::a_blocking_stop_hook_does_not_reopen_a_tool_requested_end` |

前两个是重构把"相隔 1000 行的两份代码"变成"相邻四行"带来的新风险；第三个是把
原本由**结构**保证的区别（那个 helper 直接调另一个函数）降级成了一个布尔参数。
三个测试都断言两条入口**不一致**，而不是单独测某一条——单独测的那一半在两条被
拉齐之后仍然是绿的。

### PR 6 的文档收网

`HANDOFF-2026-09-10.md` 的措辞**没有**按 PR 6 写的那样改成"一个外层 + 两种策略"。
落地的不是一个外层函数：三个 `loop {}` 仍然在各自的入口里，共享的是形状和里面
每一段。写成"一个外层"会制造一个更危险的误解——让人以为改一处就覆盖了全部，而
`run_round` 和 `run_batched_round` 仍是两个函数。那一条"加分支要加两遍"的陷阱条目
因此原样保留，表格改成说明**哪些**已经只有一处。

静态检查（PR 6）：`record_prompt_snapshot_if_needed` 与 `collect_turn_reminders`
各 1 个生产调用点；`take_lone_wakeup_turn_end` 与 `take_end_conversation_request`
各 2 个——那是 §4 明确要保留的**顺序**差异，不是残留分支。

### 仍未落地

`StreamingIterationDisposition` 没有并进 `StepExit`。它比 `StepExit` 多一个
`Complete` / `ForcedComplete` 的区别（自然结束还要吃一次 late drain，工具请求的
结束不吃），合并要把那次 drain 移进 `decide_streaming_disposition`，收益是少一层
4 行映射，风险是动 §3.6 的 drain 时点。没做。

### 判据已就位

上面每一条"不得改变"的差异都由一个**断言差异本身**的跨入口测试守着。统一外层
做完之后这些全部仍然是绿的，且每一条都对**新结构**重新种过雷——重构能把测试掏空
和弄红一样容易，绿不是证据。这是本系列最重要的交付物：`turn_preparation_boundary_test`、`turn_loop_state_boundary_test`、
`turn_end_wakeup_boundary_test`、`turn_end_conversation_boundary_test`、
`turn_epilogue_boundary_test`、`turn_cancel_mapping_test`、
`reminder_twin_wiring_test`，加上 `stop_hooks_test` /
`token_budget_continuation_test` / `mid_turn_input_test` 里新增的 streaming 孪生。

---

## 10. 参考

- `lingxi-code/orchestrator/src/conversation/drivers/mod.rs` — 三条循环、取消、late drain、file-history
- `lingxi-code/orchestrator/src/turn_loop.rs` — batched 准备、工具结果、wakeup 消费
- `lingxi-code/orchestrator/src/conversation.rs` — `StopHookFlow`
- `lingxi-code/orchestrator/src/conversation/hooks.rs` — Stop continuation、block cap、Prevent 与 max-turns
- `lingxi-code/orchestrator/src/vision_model_call.rs` — vision delegation 取消错误
- `lingxi-code/orchestrator/src/conversation/reminders.rs` — `todo_reminder_message` 实际 envelope
- `lingxi-code/docs/HANDOFF-2026-09-10.md` — 双路径接线风险
- `lingxi-code/docs/cron-parity-audit-2026-09-06.md` — wakeup 历史问题

**验证说明：** 本修订基于 2026-09-15 源码审阅；未跑实现迁移或运行时回归。上述测试是实施要求，不代表已经通过。
