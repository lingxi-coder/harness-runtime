# LingXi-Next — 对 Claude Code 2.1.220 的 backlog 与关闭记录

**代码基线** `370946160`（`main`）  
**原始清单提交** `f0af1f7b3`  
**复核日期** 2026-07-27  
**Oracle** Claude Code `2.1.220`（本机二进制 SHA-256 `8addc857f3fe64d5a0368af9ee50321b50afb4a6918ba3ef018ab84f5dbbe081`）  
**原始复核状态** 26 个唯一未决事项 —— 20 OPEN、3 PARTIAL、1 LATENT、2 SCOPE-DECISION；另保留 1 条 DUPLICATE 交叉引用

**当前实现状态** 25 CLOSED、1 DIVERGENCE；`N-protocol-8` 仅作为该 Divergence 的历史重复 id

## 2026-07-27 实现关闭说明

本分支已按复核后的行为边界实现 25 个可移植事项。下文保留 `370946160`
上的原始缺口描述与 oracle 证据，便于审计；各条状态栏和汇总表则表示实现后的
当前状态。

- `N-env-3`（以及重复 id `N-protocol-8`）依赖 Anthropic 私有账号
  remote-memory 端点，明确记录为 **DIVERGENCE**，没有伪造本地兼容实现。
- Chrome 与 Anthropic remote-control 私有协议继续保持明确 fail-fast，不在本册
  25 项关闭范围内，也不宣称 1:1 parity。
- `deep-research` 作为不可被项目文件覆盖的 manual-only 内置 workflow 实现；
  observer、Ultracode、后台切换、clipboard/fullscreen、MCP policy、compact/live
  context、hook/telemetry 和 accessibility 项均有对应回归测试。

### 验证记录

- `cargo check --workspace --all-targets`：通过。
- `cargo test -p test-harness --test parity_claude_2_1_220`：7/7 通过。
- `cargo test -p tui -p tui-core --all-features`：TUI 948/948、tui-core
  288/288 通过。
- `cargo test --workspace --all-features`：parity 相关 crate 均通过；随后在
  本分支范围外、未提交的 mobile-linux 工作
  `platform-common::mobile_linux::manifest_rejects_executables_in_writable_paths`
  处失败。该失败没有通过修改无关工作来掩盖。
- `cargo build --release -p cli --bin lingxi-cli`、`./scripts/checks/check-deps.sh`：
  通过。
- `cargo fmt --all -- --check` 与
  `cargo clippy --workspace --all-targets -- -D warnings` 仍受仓库既有、可在
  未修改文件中复现的全局基线债务阻塞；本修复没有用跨仓库格式化或无关
  lint 清理扩大 diff。

## 二次独立真实性复核结论

原文的“22 项全部仍然成立”不能按字面保留。22 行并不等于 22 个独立、当前可观察、同等确定的 gap：

- `N-protocol-8` 与 `N-env-3` 是同一条 remote-memory push/mass-delete 管线缺失，属于**重复计数**。
- `N-protocol-7` 只能裁为 **PARTIAL**：Opus 5 条件下新增的 Bash 描述句确实未移植；但“旧的 dedicated-tool 警告已从 Opus 5 删除”被本机 2.1.220 二进制直接反证，「Agent lead-in 已删除」同样被**直接反证**——本会话即运行于 opus-5 且持有 Agent 工具，其 lead-in 仍在。故本项为**纯新增一句**，无任何删除。
- `M12-managed-row` 的 gate `tengu_maple_sundial` 默认关闭，因此是 **LATENT** implementation debt，不是默认配置下的当前行为差异。
- `N-env-3` 是 Anthropic 第一方账号 remote-memory 后端的范围决策；`N-changelog-3` 是 bundled workflow content 的范围决策。两者都应先裁定产品范围，不应伪装成无条件工程 blocker。
- 其余原条目在 `370946160` 的行为点仍可复现；其中 `FU-disabled-agent-server` 的 High 判定成立。

对 Anthropic 官方 2.1.218–2.1.220 release feed 和本机 2.1.220 binary 做反向覆盖检查后，又确认原清单漏了 5 项：

1. `/code-review` 仍走 inline prompt，没有按 2.1.218 改成 background subagent。
2. `/context` 读取 session 累计 token，compact 后不会反映新的 live context。
3. 某些终端把粘贴换行编码成 Ctrl+J 时，composer 没有把它归一化为 newline。
4. plugin/settings 列表只高亮 selected row，不把真实 terminal cursor 移到焦点行。
5. microcompact 在启用时没有真实的 last-assistant timestamp，只能退化成 count gate。

因此，原 22 行经合并重复项后是 21 个唯一事项；补入 5 个遗漏后，共 **26 个唯一未决事项**。这里的 OPEN 只表示“证据足够且行为尚未实现”，不把默认关闭的 feature flag 或未获范围批准的私有服务当作 active parity defect。

## 这份清单是怎么来的

条目来自三处已有记录（`docs/claude-code-2.1.220-sessionB-wave-2026-07-26.md` 的 §3 / §6 / §8，以及 §4 的一条部分裁定）。**但内容不是照抄。**

原记录里的 `verify` 证据大多是**二进制侧**采的，而且 §8 的 follow-up 写于 reconciliation 合并（`3905f9f9b`）**之前**。上一轮审计有过教训：32 项标记为 open 的条目重新在 port 侧核查后，**22 项其实已经关闭**。所以每一条都在 `370946160` 上由独立 agent 在**行为点**重新判定过一遍，而不是靠 grep 命中数——一次跨模块的负向 grep 什么都证明不了。

原始整理曾裁定“全部 22 项仍然成立”。本次二次复核保留其行为点证据，但修正了重复计数、确定性和范围分类，并补入了 5 个遗漏项。原条目的长证据保留在下文，修正结论以本节和汇总表为准。

### 对原记录的修正

- **`FU-disabled-agent-server` 由 Medium 升为 High。** 核查在 port 侧和 2.1.220 二进制侧双向确认：用户在 `disabledMcpServers` 里明确禁用的 server，可以被一份 agent 定义重新拉起并连上。agent 定义来自 `<cwd>/.lingxi/agents/*.md`（仓库自带、不可信内容），而一个 MCP stdio server 等于任意本地代码执行。**这是一条越权项，不是格式问题。**
- **三项量级被上调。** `N-changelog-4` M→XL（port 里根本没有 OSC 52 写入器，`/copy` 走的是子进程，不是「加一个 DCS 包裹」那么小）、`N-protocol-8` M→XL、`N-changelog-5` M→L。原估值假定了底座存在。
- **`N-env-2` 比记录的更严重。** 缺的不只是提醒链：`workflow_description.txt` 是**实际下发**给模型的工具描述，其中三处告诉模型「ultracode 会有 system-reminder 确认」——而 port 永远发不出那条 reminder。功能缺失之外，还留下了一段触发条件永不可能满足的指令。
- **`N-protocol-7` 降为 PARTIAL。** 本机 2.1.220 binary 同时含有 `Command output is displayed to you, not reliably to the user.` 与 `IMPORTANT: Avoid using this tool ... dedicated tool` 两类句子；只能确认前者缺失，不能继续声称后者已在 Opus 5 路径删除。**两条「删除」子断言现均已反证**（avoid bullet 与 Agent lead-in 在实时 opus-5 会话中都仍然存在），因此本项范围收敛为「只加一句」。
- **`N-protocol-8` 合并进 `N-env-3`。** 它只作为历史 id 的交叉引用保留，不再进入唯一 gap 计数。
- **`M12-managed-row` 改为 LATENT。** 缺少 gate-on 行为是真实的，但 gate 默认 OFF，不能写成默认配置下已发生的用户差异。
- **`N-env-3`、`N-changelog-3` 改为 SCOPE-DECISION。** 前者依赖 Anthropic 第一方 remote-memory 后端，后者属于项目此前明确按 bundled content 处理的 deep-research workflow。

---

## 汇总

按「先安全、再小工作量」排序。

| # | 项 | 级别 | 量级 | 状态 | 一句话 | 阻塞 |
|---|---|---|---|---|---|---|
| 1 | [`FU-disabled-agent-server`](#fu-disabled-agent-server) | 🔴 High | S | CLOSED | A server named in `disabledMcpServers` is revived and connected by an agent-frontmatter … | — |
| 2 | [`FU-orch-7-currentdate`](#fu-orch-7-currentdate) | 🟡 Medium | S | CLOSED | `# currentDate` additional-context entry is recomputed live every turn instead of using … | — |
| 3 | [`P1-user_prompt-log`](#p1-user_prompt-log) | 🟡 Medium | S | CLOSED | `claude_code.user_prompt` OTEL log record has no emit site — `user_prompts_enabled()` is… | — |
| 4 | [`M6-flagSettings-env-tier`](#m6-flagsettings-env-tier) | 🟡 Medium | S | CLOSED | The `--settings` (flagSettings) `env` block is never folded into the enterprise MCP poli… | — |
| 5 | [`FU-mcp-get-health`](#fu-mcp-get-health) | 🟡 Medium | M | CLOSED | `mcp get` never health-checks a connectable server and prints no `Status:` line for it | — |
| 6 | [`FU-afe-shadow`](#fu-afe-shadow) | 🟡 Medium | M | CLOSED | A pending/rejected project `.mcp.json` server that shadows a USER server deletes the use… | — |
| 7 | [`M7-plugin-warn-variant`](#m7-plugin-warn-variant) | 🟡 Medium | M | CLOSED | No warn variant naming WHY agent-frontmatter MCP servers were skipped; the `strictPlugin… | — |
| 8 | [`M12-attach-detach`](#m12-attach-detach) | 🟡 Medium | M | CLOSED | ←-on-empty in an attached background session never detaches back to the agents view (the… | — |
| 9 | [`N-env-2`](#n-env-2) | 🟡 Medium | L | CLOSED | Ultracode ultra-effort enter/sparse/exit reminder chain, the EK(model,effort,workflowsOn… | — |
| 10 | [`N-changelog-3`](#n-changelog-3) | 🟡 Medium | L | CLOSED | No built-in workflow library: `deep-research` workflow, its `/deep-research` entry point… | — |
| 11 | [`H6-remainder`](#h6-remainder) | 🟡 Medium | L | CLOSED | H6 remainder = the `register_repo_root` SDK control request (absent entirely), the Direc… | — |
| 12 | [`N-protocol-5`](#n-protocol-5) | 🟡 Medium | XL | CLOSED | Agent observer pairing (observer / observerMessage / observeSubagents) is entirely absen… | — |
| 13 | [`N-env-3`](#n-env-3) | 🟡 Medium | XL | DIVERGENCE | Memory push/pull sync engine (mass-delete hold + CLAUDE_CODE_DISABLE_MEMORY_MASS_DELETE_… | Anthropic 私有后端 |
| 14 | [`M12-midturn-backgrounding`](#m12-midturn-backgrounding) | 🟡 Medium | XL | CLOSED | Mid-turn backgrounding state machine absent — ← never backgrounds the live conversation … | — |
| 15 | [`N-protocol-7`](#n-protocol-7) | ⚪ Low | S | CLOSED | Opus 5 Bash description 缺一句；两条「删除」子断言已反证，纯新增 | — |
| 16 | [`FU-mcpcli-5`](#fu-mcpcli-5) | ⚪ Low | S | CLOSED | JSON agent parser rejects an empty `{}` record inside `mcpServers`, dropping the agent (… | — |
| 17 | [`M12-managed-row`](#m12-managed-row) | ⚪ Low | S | CLOSED | `/config` has no `tengu_maple_sundial` collapsed read-only "Agents view" (`managedEnum`)… | — |
| 18 | [`FU-orch-decision-class`](#fu-orch-decision-class) | ⚪ Low | M | CLOSED | tool_decision OTEL label ignores the host's explicit decisionClassification, and tool_pa… | — |
| 19 | [`M5-issue-formatting`](#m5-issue-formatting) | ⚪ Low | M | CLOSED | `Skipped — invalid MCP server config for "X": <issues>` renders three canned reasons ins… | — |
| 20 | [`N-changelog-5`](#n-changelog-5) | ⚪ Low | L | CLOSED | Screen-reader input announcements (2.1.218 deleted-text + typed-space echo, 2.1.219 per-… | — |
| 21 | [`N-changelog-4`](#n-changelog-4) | ⚪ Low | XL | CLOSED | No OSC 52 clipboard writer and no mouse-selection copy surface — `/copy` is subprocess-o… | — |
| 22 | [`N-protocol-8`](#n-protocol-8) | ⚪ Low | — | DUPLICATE | 与 `N-env-3` 相同的 remote-memory/mass-delete 管线；由 `N-env-3` 的 DIVERGENCE 覆盖 | 合并 |
| 23 | [`O1-code-review-background`](#o1-code-review-background) | 🟡 Medium | M | CLOSED | `/code-review` 仍把完整 review prompt 注入主对话，没有后台 subagent 隔离 | — |
| 24 | [`O2-context-post-compact`](#o2-context-post-compact) | 🟡 Medium | S | CLOSED | `/context` 使用累计 token；compact 后仍显示 pre-compact 量级 | — |
| 25 | [`O3-ctrl-j-paste-newline`](#o3-ctrl-j-paste-newline) | ⚪ Low | S | CLOSED | Ctrl+J 编码的粘贴换行被当作 modified key 丢弃，而不是 newline | — |
| 26 | [`O4-panel-focus-cursor`](#o4-panel-focus-cursor) | ⚪ Low | M | CLOSED | plugin/settings 选中行不拥有 terminal cursor，屏幕阅读器/放大器无法跟随焦点 | — |
| 27 | [`O5-microcompact-idle-gap`](#o5-microcompact-idle-gap) | 🟡 Medium | M | CLOSED | microcompact 有清理逻辑，但缺真实 message timestamp，无法执行 exact idle-gap gate | — |

量级：S = 单点改动；M = 一个模块；L = 跨模块；XL = 子系统级（可能需要先做范围决策）。

表内保留 27 个历史/新增 id；合并第 22 项 duplicate 后是 26 个唯一事项。当前状态合计：25 CLOSED、1 DIVERGENCE。

## 建议顺序

1. **`FU-disabled-agent-server`** —— 唯一一条有越权后果的，且量级 S。注意它当前的行为被 `apps/engine-desktop/src/lib.rs:9563` 的测试钉住了，那个测试分不出两种 disable 原因，所以修复必然要同时改测试——**改之前先确认新断言表达的是「按名字拒绝」而不是「放行」**。
2. **新增的 S/M 可观察缺口** —— `O2-context-post-compact`、`O3-ctrl-j-paste-newline`、`O4-panel-focus-cursor` 都有官方 release contract 和明确行为点，适合先补回归测试再修；`O1-code-review-background` 需要复用现有 subagent/runtime，不要再造第二条执行链。
3. **`N-protocol-7`** —— 只加已证实的那一句 Bash bullet。**不要删除任何现有 Agent/Bash 文本**：两条「删除」子断言均已被实时 opus-5 会话反证，照原记录动手会毁掉正确的 prompt 文本。
4. **B 组其余 + C 组碎片** —— 都是 S/M，互相独立；`O5-microcompact-idle-gap` 需要先解决 timestamp schema。
5. **A 组的 L/XL 项** —— `N-env-2`、`N-protocol-5` 是真正的功能移植；`N-changelog-3` 先做 bundled-content 范围裁定。
6. **`N-env-3` / `N-protocol-8` 先不要写代码** —— 见下。

### 需要产品判断，而不是工程排期

`N-env-3` 与 `N-protocol-8` 是同一个洞的两个角度：port 里**根本没有后端 memory 同步管线**，所以 mass-delete hold 和它的 escape hatch 都没有消费者。它紧邻已冻结的 team/swarm 功能。

**先决定后端 memory 同步（第一方账号端点）是否在范围内。** 如果不在，正确做法是记一条显式的 Divergence(reason)，把这两项从 backlog 移除——而不是让两个 XL 项无限期挂着。同样地，`N-changelog-3` 要先确认项目是否改变“bundled workflow content 不属于 core parity”的既有裁定。

---

## A. 2.1.220 猎取但未实施的 gap

来自 Session B wave 报告 §3。这些是对 2.1.220 做 gap-hunt 时找到、但该轮未动手的项。

### `N-env-2`

**Ultracode ultra-effort enter/sparse/exit reminder chain, the EK(model,effort,workflowsOn) gate, the bop()/CLAUDE_CODE_JUNIPER_SUNDIAL cadence override, and the workflow_keyword_request attachment are all absent — while the live Workflow tool description tells the model those reminders exist**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | L | CLOSED |

The oracle treats ultra-effort ("Ultracode") as a transcript state machine (`j2y`). On each turn it evaluates `EK(model, effort, workflowsOn)` — true when workflows are available and enabled AND the resolved effort is `xhigh`. On the false→true edge it emits an `ultra_effort_enter` attachment with `reminderType:"full"` ("Ultracode is on: ... Use the Workflow tool on every substantive task..."); while it stays true it re-emits a sparse variant ("Ultracode is still on...") every `bop()` non-meta user turns; on the true→false edge it emits `ultra_effort_exit` ("Ultracode is off..."). `bop()` resolves the cadence by precedence: env `CLAUDE_CODE_JUNIPER_SUNDIAL` > statsig `tengu_juniper_sundial` > GrowthBook gate > `ULTRA_EFFORT_CONFIG.TURNS_BETWEEN_MAINTENANCE = 10`. A sibling `workflow_keyword_request` attachment (gated by the `workflowKeywordTriggerEnabled` setting) fires when the user types the literal keyword "ultracode" in a prompt.

The port implements none of this. `dynamic_workflows_enabled()` in `commands/core/src/effort.rs` is a hardcoded `false`, so `/effort ultracode` can only ever render the gated-off guidance string and the ultracode→xhigh mapping at `effort.rs:537` is dead code. There is no `EK`-equivalent composition anywhere: the two inputs exist independently (the orchestrator holds live effort in `current_effort`, and the Workflow tool owns a private `workflows_disabled()` gate) but nothing joins them, and `commands/core` does not even depend on `tools/workflow`. The per-turn reminder chain — which already has the exact full/sparse precedent it would need, in `plan_mode_reminder_message` — has no effort-gated member in either the streaming or the batched twin. No code reads `CLAUDE_CODE_JUNIPER_SUNDIAL` (or a `LINGXI_`-prefixed equivalent), no `TURNS_BETWEEN_MAINTENANCE` constant exists, and there is no non-meta user-turn counter to drive a cadence off.

The user-visible consequence is larger than a missing reminder, because the model-facing half of the feature DID land. The live Workflow tool description shipped on every request names two of these reminders as the authoritative opt-in signals: it tells the model that typing "ultracode" produces a confirming system-reminder, and that a standing session-level Ultracode mode is confirmed by a system-reminder. Neither can ever appear. So a user who types "ultracode" gets nothing — the oracle's keyword opt-in path is entirely inert, and the model, following its own tool description, has no reason to treat the keyword as authorization. In practice the Workflow tool is reachable only through the natural-language opt-in bullet ("use a workflow", "fan out agents") or an explicit skill/slash-command instruction, and the entire session-level Ultracode mode — standing authorization to orchestrate every substantive task, plus its enter/still-on/exit lifecycle — is unreachable. The description text also leaves the model holding an instruction whose trigger condition can never be satisfied, which is a latent prompt-hygiene defect independent of the missing feature.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

Re-verified port-side at 370946160; the recorded evidence HOLDS and is worse than recorded. (1) `commands/core/src/effort.rs:106-108` is still literally `fn dynamic_workflows_enabled() -> bool { false }` — unchanged by the reconciliation merge. (2) Repo-wide case-insensitive grep over ALL text files (not just .rs) for `ultra_effort|Ultracode is on|Ultracode is still|Ultracode is off|JUNIPER_SUNDIAL|TURNS_BETWEEN_MAINTENANCE|workflow_keyword|keywordTrigger|sundial` returns only: `commands/core/src/effort.rs` (the `/effort ultracode` command surface), `tools/workflow/src/workflow_description.txt:6,7,25` (model-facing prose), `tools/file/src/edit.rs:180` (unrelated `tengu_cedar_sundial`), and `docs/` (the backlog record itself). Zero emission code, zero env read, zero cadence constant. (3) Verified at the BEHAVIOUR SITE rather than by grep absence: the port's complete per-turn reminder chain is enumerated at `orchestrator/src/conversation.rs:7051-7176` (streaming twin) and mirrored at `orchestrator/src/turn_loop.rs:391-508` (batched twin) — 12 builders: output_style, plan_mode, skill_listing, conditional_rules, new_diagnostics, agent_listing, todo, async_hook_response, task_notification, relevant_memory, skill_discovery, deferred_tools/date_change. NONE is effort-gated and there is no 13th slot. (4) Both `EK()` inputs exist but are never composed: live effort state at `orchestrator/src/conversation.rs:886` (`current_effort: RwLock<Option<String>>`) + `orchestrator/src/handle_impl.rs:595`, and the Workflow tool is live and permissive-by-default at `tools/workflow/src/lib.rs:560` (`!self.workflows_disabled()`). (5) NEW, not in the original record — a dangling model-facing reference: `tools/workflow/src/workflow_description.txt` IS the live tool description (`lib.rs:51` `static DESCRIPTION`, returned by `prompt()` at `lib.rs:594`), and line 6 says "The user included the keyword \"ultracode\" in their prompt (you'll see a system-reminder confirming it)", line 7 says "Ultracode is on for the session (a system-reminder confirms it)", line 25 says "When a system-reminder confirms ultracode is on ... When a reminder says ultracode is off, revert". The port ships all three sentences to the model and can never emit any of the reminders they name. (6) `telemetry/src/tengu/workflow.rs:37` independently confirms the keyword half is unbuilt: `tengu_workflow_keyword`/`_dismissed`/`_restored` are listed as unreachable with the note "no keyword UI". (7) Session turn counters exist (`core/src/session.rs:118,124` `turns_since_last_todo_write` / `turns_since_last_reminder`) but are todo-reminder-specific; no ultra-effort maintenance counter.

</details>

**阻塞：** Three reminder bodies (ultra_effort_enter full, the sparse still-on variant, ultra_effort_exit) and the workflow_keyword_request attachment body must be byte-extracted from the 2.1.220 binary — they are not present anywhere in-tree, so the work cannot start from the repo alone. Also needs a product decision on flipping dynamic_workflows_enabled() to true: the Workflow tool is already live and permissive, so the honest EK() gate would be on, but flipping it changes the /effort usage block and invalid-arg hint and breaks two locked fixtures (commands/core/src/effort.rs:691 asserts !dynamic_workflows_enabled(), and usage_is_byte_exact_with_gate_off). Secondary: the workflows-enabled gate currently lives private on WorkflowTool (tools/workflow/src/lib.rs:484) and must move to a crate both commands/core and orchestrator can reach before EK() can be composed.

**涉及文件：** `commands/core/src/effort.rs`, `orchestrator/src/conversation.rs`, `orchestrator/src/turn_loop.rs`, `orchestrator/src/prompt/plan_reminder.rs`, `tools/workflow/src/lib.rs`, `tools/workflow/src/workflow_description.txt`, `telemetry/src/tengu/workflow.rs`, `core/src/session.rs`

### `N-changelog-3`

**No built-in workflow library: `deep-research` workflow, its `/deep-research` entry point, and the 2.1.220 prompt guard sentence are all absent**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | L | CLOSED |

Oracle 2.1.220 ships a built-in workflow library and bundles a complete `deep-research` workflow — phases Scope/Search/Fetch/Verify/Synthesize driving a generated script with `VOTES_PER_CLAIM=3` and `MAX_FETCH=15` — reachable both as `Workflow({name:"deep-research", args:"<question>"})` and via the `/deep-research` entry point, gated only by a default-enabled kill-switch (`tengu_sorrel_avocet`); 2.1.218 narrowed it to start only when invoked manually, and 2.1.220 added the system-prompt guard sentence "Do not use workflows or deep-research unless the user requested it". The port has no built-in workflow library of any kind: `Workflow`'s `name` argument resolves exclusively against user-saved script files under `.lingxi/workflows/` and `~/.lingxi/workflows/`, so `Workflow({name:"deep-research"})` fails with "no saved workflow named 'deep-research' under …", `/workflows` never lists it, and no `/deep-research` slash command or bundled skill is registered. User-visible consequence: a user who asks for deep research, or who follows Claude Code documentation and invokes the named workflow, gets a hard error and no research pipeline — the multi-source search/fetch/vote/verify behaviour simply does not exist; the model can only hand-author an equivalent script inline each time, with no vote threshold, fetch cap, or phase structure. The guard half is a partial non-issue: the port's Workflow tool description already forbids calling the tool without explicit user opt-in (a superset of the oracle's guard), so behaviourally the port is at least as conservative; only the literal sentence is missing, and its `deep-research` clause is moot until the workflow exists. Note the port's own parity ledger has previously dispositioned a related 2.1.196 `/deep-research` item as a Divergence on the grounds that it is "bundled skill content, not core behavior" — so the real gate here is a product decision on whether to adopt bundled workflow content at all, not a technical blocker.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

Re-verified port-side at commit 370946160 in /Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code. (1) Name resolution: `tools/workflow/src/lib.rs:176` `resolve_script` handles `scriptPath` > `script` > `name`; the `name` branch loops only over `saved_workflow_candidates(&name)` (project `.lingxi/workflows/<name>` then `$LINGXI_CONFIG_DIR|~/.lingxi/workflows/<name>`) and, on miss, returns `WorkflowLaunchError("no saved workflow named '{name}' under {dirs}")` at line 195. There is no builtin table consulted before or after the filesystem lookup — the doc comment at line 174-175 ("LingXi ships no built-in workflow library, so a `name` that isn't a saved file is an error") is an accurate description of the code, not a stale comment. So `Workflow({name:"deep-research"})` errors. (2) Listing: `list_available_workflow_names` (`tools/workflow/src/lib.rs:503-527`) enumerates only `saved_workflow_dirs()` via `read_dir` and returns `None` when no dir exists — builtins are not injected into the errorCode-1b message or into the `/workflows` browser (`tui/src/command.rs:226-239` -> `ChatWidget::cmd_workflows`, whose backing view reads the same `workflows` dir, `tui/src/bottom_pane/workflows_view.rs:1265`). (3) Registration: `register_all` at `tools/workflow/src/lib.rs:858-861` registers only `WorkflowTool::new(None)`; there is no workflow-content registry. (4) Command surface: `commands/core/src/bundled/mod.rs:28` `register_bundled_skills` registers exactly batch, code-review, fewer-permission-prompts, run-skill-generator, simplify, run, verify, loop — no `deep-research`. (5) Repo-wide grep for `deep-research` / `deep research` / `deep_research` outside `docs/` and the `llm-runtime/data/models-dev/*.json` model catalogs yields exactly two hits, both non-production: a test fixture in `apps/cli/src/stream_json.rs:1726` (a `vec!["deep-research".to_string()]` workflows list fed to `build_init_params` in `init_frame_has_correct_keys`), and a parity-ledger row `test-harness/tests/parity_claude_2_1_198.rs:513` recording the 2.1.196 `/deep-research` verifier-reporting item as `Divergence("bundled skill content, not core behavior")`. No kill-switch equivalent of `tengu_sorrel_avocet` exists (0 hits). PROMPT-GUARD HALF (checked separately, per instructions): the literal sentence "Do not use workflows or deep-research unless the user requested it" is ABSENT — 0 hits repo-wide outside `docs/claude-code-2.1.220-sessionB-wave-2026-07-26.md:64`; `orchestrator/src/prompt/*` (the system-prompt composition site) mentions "workflow" only in `task_notification.rs` (`local_workflow` task type) and `plan_reminder.rs` (plan-mode phrasing), never as a usage guard. However the port carries a STRICTER functional equivalent in the Workflow tool description, `tools/workflow/src/workflow_description.txt` para. 3: "ONLY call this tool when the user has explicitly opted into multi-agent orchestration... For any other task — even one that would clearly benefit from parallelism — do NOT call this tool." That already enforces the 2.1.218 manual-invocation-only semantics for workflows generally, so only the literal sentence and its deep-research clause are missing.

</details>

**阻塞：** Product decision: whether LingXi adopts bundled workflow CONTENT at all. The port's parity ledger already dispositions a sibling /deep-research item as Divergence("bundled skill content, not core behavior") (test-harness/tests/parity_claude_2_1_198.rs:513); if that policy stands, this item should be reclassified as a Divergence rather than implemented. If it is to be implemented, the mechanical prerequisite is a builtin-workflow registry consulted by resolve_script (tools/workflow/src/lib.rs:176) and by list_available_workflow_names (line 503) before the filesystem lookup — that seam does not exist yet.

**涉及文件：** `tools/workflow/src/lib.rs`, `commands/core/src/bundled/mod.rs`, `tools/workflow/src/workflow_description.txt`, `tui/src/command.rs`, `test-harness/tests/parity_claude_2_1_198.rs`

### `N-protocol-5`

**Agent observer pairing (observer / observerMessage / observeSubagents) is entirely absent — fields silently dropped by both agent parsers**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | XL | CLOSED |

The oracle's agent-definition zod schema accepts three observer fields: `observer` (a string naming another agent), `observerMessage`, and `observeSubagents` (a boolean, new in 2.1.218). At load time the runtime logs "[agentObserver] Agent X declares observer Y…" and arms a pairing whose `fanoutToSubagents` is computed as `observeSubagents !== false` together with a `fanoutDepth` bounded by a depth cap; subagents spawned beneath an observed agent inherit the same observer unless the agent sets `observeSubagents: false`, in which case the runtime logs "not fanning out to observer agent (no chaining)" and stops the chain. The port implements none of this. Neither agent parser knows the three keys — the markdown frontmatter struct (agent/src/catalog.rs) lists 16 fields and carries no `deny_unknown_fields`, and the JSON `parse_agent_from_json` path reads a fixed key set — so `observer:`/`observerMessage:`/`observeSubagents:` in an agent definition are dropped during deserialization without warning, error, or log line. `AgentDefinition` has no place to store them, `SubagentSpawnRequest` has no field to thread them to a child, and no spawn-time code path arms or inherits an observer. User-visible consequence: an agent definition (hand-written, plugin-shipped, or copied from Claude Code docs) that declares an observer runs with no observer whatsoever, and the user gets no diagnostic that the declaration was ignored — the agent appears to load cleanly. Any workflow relying on an observer agent to watch a subagent tree silently produces no observation output at all. The one prerequisite that used to block this is now in place: the spawn-depth default was raised to 3 (platform-api/src/subagent_spawn.rs:527) and is enforced in the tool resolver, so a fan-out depth cap can be layered on the existing `depth` field rather than inventing new plumbing.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

Re-verified port-side at commit 370946160 in /Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code. (1) agent/src/definition.rs:22-97 — `pub struct AgentDefinition` carries 38 fields (agent_type, when_to_use, tools, max_turns, model, permission_mode, source, base_dir, system_prompt, mcp_servers, frontmatter_hooks, icon, allowed_tools, worktree_requirement, disallowed_tools, skills, required_mcp_servers, background, isolation, memory, effort, initial_prompt, color); there is no observer, observer_message, or observe_subagents field. (2) agent/src/catalog.rs:98-131 — the markdown `struct Frontmatter` enumerates exactly 16 recognised keys (name, description, tools, model, disallowedTools, skills, mcpServers, hooks, maxTurns, background, memory, isolation, permissionMode, effort, color, initialPrompt); `grep -rn deny_unknown_fields agent/src/` returns nothing, so serde silently discards an `observer:` key with no warning and no telemetry. (3) agent/src/catalog.rs:818-1044 — the JSON path `parse_agent_from_json` reads only description/prompt/tools/disallowedTools/skills/model/effort/permissionMode/mcpServers/hooks/maxTurns/initialPrompt/memory/background/isolation; all three observer keys are absent there too. (4) `grep -rni 'observ' agent/ tools/agent/ platform-api/src/subagent_spawn.rs` yields only unrelated prose (builtin agent prompt text at agent/src/builtins.rs:233/272/282/302 and comments at agent/src/catalog.rs:1357, agent/src/runner.rs:543) — no observer concept. (5) Repo-wide `grep -rn 'agentObserver|agent_observer|observeSubagents|observerMessage|fanoutToSubagents|no chaining'` matches exactly one line, docs/claude-code-2.1.220-sessionB-wave-2026-07-26.md:82, i.e. the backlog entry recording this very gap — no implementation and no test. (6) platform-api/src/subagent_spawn.rs:25-230 — `SubagentSpawnRequest` has no observer/fanout field, so nothing could be threaded to a child even if a definition carried it. Depth note: the spawn-depth constant HAS been raised — platform-api/src/subagent_spawn.rs:527 `DEFAULT_MAX_SUBAGENT_SPAWN_DEPTH: u32 = 3`, resolved by `max_subagent_spawn_depth()` at :552 and enforced at agent/src/tool_resolver.rs:240 (`if depth >= platform_api::subagent_spawn::max_subagent_spawn_depth()`), so the depth-3 prerequisite is already landed.

</details>

**阻塞：** Not blocked by the spawn-depth work any more — DEFAULT_MAX_SUBAGENT_SPAWN_DEPTH is already 3 (platform-api/src/subagent_spawn.rs:527) and enforced at agent/src/tool_resolver.rs:240. What is still missing is an oracle spec extraction of the observer RUNTIME, not just the schema: when the observer agent is actually invoked, what payload/transcript it receives, how `observerMessage` is delivered, and whether `fanoutDepth` is a counter independent of `spawnDepth` or is clamped by the same CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH cap. Implementing from the schema alone would guess the semantics.

**涉及文件：** `agent/src/definition.rs`, `agent/src/catalog.rs`, `platform-api/src/subagent_spawn.rs`, `tools/agent/src/agent.rs`, `agent/src/tool_resolver.rs`

### `N-env-3`

**Memory push/pull sync engine (mass-delete hold + CLAUDE_CODE_DISABLE_MEMORY_MASS_DELETE_HOLD escape hatch) entirely unported**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | XL | DIVERGENCE |

The oracle runs a user+team multistore memory sync engine: a push/pull cycle that reconciles local memory entries against a remote store, logging push_written / push_deleted / conflicts, with a delete policy selected by CLAUDE_CODE_MEMORY_PUSH_DELETE_MODE / the tengu_mem_push_delete_mode gate (corroborate | immediate | never). Guarding that engine is a data-loss hold: Ity(e) returns +Infinity when CLAUDE_CODE_DISABLE_MEMORY_MASS_DELETE_HOLD is set, otherwise max(50, floor(e * 0.1)); a push that would delete at least that many entries is HELD rather than applied, so a corrupted or truncated local store can never wipe a user's or a team's memories remotely. The hold is on by default and the env var is the deliberate escape hatch for the rare legitimate bulk purge. The port has none of this. Its memory subsystem is entirely local-filesystem: a LINGXI.md hierarchy loader, a memdir scanner/ranker, a side-query relevance selector with prefetch, session-memory extraction, surfacing into the prompt, a secret scanner, and a retention sweep. The only team-memory code is TeamMemoryWatcher, a poll-based mtime differ over ~/.lingxi/team-mem that reports changed .md files for hot-reload — and it is not constructed anywhere outside its own unit tests, so even that local half never runs in a shipped binary. There is no remote store, no push, no pull, no conflict record, no delete mode, no hold threshold, and no telemetry event that could report any of it. User-visible consequence today is nil in the sense that no memory data can be lost — nothing is ever pushed anywhere — but the flip side is the whole feature: LingXi users get no cross-machine memory sync and no team memory sharing beyond a directory somebody has to populate by hand, and the CLAUDE_CODE_DISABLE_MEMORY_MASS_DELETE_HOLD env var is silently ignored. The mass-delete hold itself is a roughly 20-line pure function; it is only meaningful once the push side exists, so this item cannot be closed piecemeal — porting the guard without the engine would be dead code of exactly the shape this backlog flags as a defect.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

Confirmed port-side, repo root /Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code. (1) memory/src/lib.rs:10-22 enumerates the WHOLE memory crate: file, index_cap, lingxi_md, memdir, prefetch, retention, secret_scan, selector, session_memory, snapshot, surfacing, team_memory, tier — there is no sync/push/pull/store module. (2) Repo-wide `grep -rn --include='*.rs' -E "MASS_DELETE_HOLD|push_deleted|push_written|MEMORY_PUSH_DELETE|mem_push_delete|mass_delete"` → 0 hits (the only `corroborate` hits are forked-skill resume in session/src/forked_skill.rs etc., unrelated). (3) Consumer-side search for the feature under any other name — `grep -irE "mem(ory)?[_-]?sync|sync[_-]?mem|multistore|multi_store|memory_push|memory_pull|remote_memory|memory_backend"` → 0 hits repo-wide; no HTTP/API surface for memories exists either. (4) telemetry/src/tengu/memory.rs:1-50 is the complete `tengu_memory_*` schema — 12 events, all local (loaded, load_failed, case_mismatch, file_too_large, secret_redacted, age_penalty_applied, dropped_for_age, rank_computed, team_scan_started/completed/failed, claude_md_hierarchy_walked). No push/pull/conflict event exists, so a sync could not even be logged. (5) memory/src/team_memory.rs:50-69 `TeamMemoryWatcher::poll_changes` is exactly what was recorded: an mtime diff over `<home>/.lingxi/team-mem` returning changed `.md` paths, with an event-time secret scan. No network, no deletes, no remote store. (6) The env escape hatch is absent: the port's honoured `CLAUDE_CODE_DISABLE_*` set is 1M_CONTEXT (migrations/src/migrate_opus_to_opus1m.rs:34), ARTIFACT (tool-api/src/artifact_gate.rs:85), BACKGROUND_TASKS (tools/mcp/src/auto_background.rs:42), EXPERIMENTAL_BETAS (tool-api/src/defer.rs:139), AGENT_VIEW (platform-api/src/agent_view.rs:33), NONESSENTIAL_TRAFFIC (platform-api/src/traffic_mode.rs:9) — no MEMORY_MASS_DELETE_HOLD. (7) EXTRA finding beyond the record: even the local team-memory half is dead code. `grep -rn 'TeamMemoryWatcher|poll_changes'` returns hits only inside memory/src/team_memory.rs itself (impl + its own #[cfg(test)] tests) — zero production constructors — and the team memdir root `memory::memdir::paths::memdir_roots_at(.., team_enabled)` (memory/src/memdir/paths.rs:43-47) is only ever called with a literal in memory/src/memdir/paths.rs tests and orchestrator/src/turn_loop_test.rs:5170. Nothing reads a real `settings.team_memory.enabled` value: `grep -rn 'team_memory|teamMemory'` outside memory/src/team_memory.rs finds only doc comments (tools/team/src/team.rs:7, memory/src/memdir/paths.rs:25-32) and test fixtures. The adjacent tools/team/src/team.rs TeamCreate/TeamDelete only mkdir/rmdir `~/.lingxi/team-mem/<team>/`.

</details>

**阻塞：** Needs a product scope decision before any engineering: backend memory sync sits adjacent to the frozen team/swarm feature set, and porting it means standing up a remote memory store plus its auth, conflict model and telemetry schema (12 new tengu_mem_* events) — none of which exist. Decide first whether LingXi ships remote/team memory sync at all. If yes, the mass-delete hold and its env escape hatch are a trivial rider on that work (S) and should be built into the push path from the start rather than bolted on. If no, this item is not XL but UNNECESSARY and should be closed as out-of-scope — and the dead TeamMemoryWatcher / team memdir root should be either wired to a real settings.team_memory.enabled read or deleted, since today they are unreachable code.

**涉及文件：** `memory/src/team_memory.rs`, `memory/src/lib.rs`, `memory/src/memdir/team_paths.rs`, `memory/src/memdir/paths.rs`, `telemetry/src/tengu/memory.rs`

### `N-protocol-7`

**Opus 5 Bash 新增 description bullet 未移植（纯新增，两处「删除」子主张均已反证）；tool-prompt layer 本身仍 model-blind**

| 级别 | 量级 | 状态 |
|---|---|---|
| ⚪ Low | S | CLOSED |

The oracle serves tool DESCRIPTIONS that vary by model capability, not just by the coarse `Dh(model)` short/long gate. Running 2.1.220 against claude-opus-5 yields a Bash description carrying an extra bullet, "- Command output is displayed to you, not reliably to the user.", placed with the other usage bullets; the same binary driven with claude-opus-4-8 does not emit it, even though both models take the SHORT (`Dh`-true) branch. The recorded finding also claimed opus-5 loses the `IMPORTANT: Avoid using this tool to run cat/head/tail/sed/awk/echo` bullet and that the Agent tool drops its "Reach for this when the task matches…" lead-in; both of those DELETION subclaims are now refuted first-hand (see evidence): a live opus-5 session shows the avoid bullet still present alongside the new one, and shows the Agent lead-in still present. So this item is a pure ADDITION of one bullet. **Delete nothing.**

The port's tool-prompt layer is model-blind below the `Dh` gate. `simple_prompt_concise` in tools/shell/src/prompt.rs takes only a sandbox config and returns one fixed string, so every lean model — opus-4-8, opus-5, fable-5 — receives byte-identical Bash text; the new bullet appears nowhere in the repository. The Agent tool also discards `PromptOptions::model` and cannot currently express model-conditional wording, but that structural limitation alone never proved the recorded Agent lead-in delta — and that subclaim is now refuted, so there is no Agent-side text change to make.

User-visible consequence is confined to prompt drift, not function: an Opus 5 session in LingXi gets a Bash tool description that differs from what real Claude Code sends the same model. The missing bullet is the one telling the model that Bash output is not reliably shown to the user — without it the model is marginally more likely to run a command and assume the user saw the output rather than relaying it, which reads as terser, less helpful answers after shell work. No API contract, schema, or execution path is affected.

The fix is small and now unblocked: thread `PromptOptions::model` into `simple_prompt_concise` (and into the Agent `build_prompt`), then branch on `platform_api::model_capabilities::has_capability(model, ModelCapability::Opus5PromptBundle)` — the same discriminator orchestrator/src/prompt/body_sections.rs already uses to separate opus-5 from the other lean models. `LeanPrompt` is the wrong gate here: opus-4-8 carries it too and shows none of these changes.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

PORT-SIDE, at the behaviour site:
(1) tools/shell/src/prompt.rs:656 — `pub fn simple_prompt_concise(sandbox: &SandboxRuntimeConfig) -> String` takes NO `model` parameter, so the SHORT Bash description is structurally incapable of varying per model. Line 667 still emits `format!("- IMPORTANT: Avoid using this tool to run {avoid_commands} commands, …")` (line 660 sets `avoid_commands = "`cat`, `head`, `tail`, `sed`, `awk`, or `echo`"` and the comment explicitly says "NOT config-gated"), and lines 668-672 jump straight from that bullet to the `- `timeout` is in milliseconds` line — there is no `Command output is displayed to you` bullet anywhere between them.
(2) `grep -rn "displayed to you" --include="*.rs"` over the whole worktree = 0 hits. The ONLY hit repo-wide is docs/claude-code-2.1.220-sessionB-wave-2026-07-26.md:88, i.e. this backlog entry itself. Also 0 hits for "not reliably to the user".
(3) tools/shell/src/bash.rs:1366 is the sole call site: `if tool_api::dh_simple_system_prompt(opts.model.as_deref()) { simple_prompt_concise(...) } else { simple_prompt(...) }`. `dh_simple_system_prompt` is a BINARY gate, and tool-api/src/model_prompt_gate.rs:163 asserts `claude-opus-4-8`, `claude-opus-5` and `claude-fable-5` all land on the SAME arm ⇒ opus-5 and opus-4-8 receive byte-identical Bash descriptions today. The oracle differentiates them; the port cannot.
(4) Agent tool: tools/agent/src/agent.rs:1311 `async fn prompt(&self, _: &PromptOptions) -> String` DISCARDS the model (bound to `_`), and `fn build_prompt(agents, _mcp_server_names, is_coordinator)` at :592 has no model parameter. The `## When to use` / "Reach for this when the task matches an available agent type, …" lead-in at :688 is suppressed only by a non-empty `pro_block` (plan tier), never by model.
INFRASTRUCTURE (the recorded blocker) — NOW PRESENT AND WIRED: platform-api/src/model_capabilities.rs:33-47 defines `ModelCapability::{LeanPrompt, Opus5PromptBundle, …}`; :72 `capabilities_for` carries the verbatim 2.1.220 table; :155 `has_capability` with :136 `normalize_model_id` (handles `[1m]`, `-eap`, provider prefixes). tool-api/src/model_prompt_gate.rs:84 consults the registry inside `uwu_standard_model`, and :119 `dh_simple_system_prompt` is consumed live by tools/shell/src/bash.rs:1366, tools/file/src/{read,edit,write,glob,grep}.rs, tools/web/src/{web_fetch,web_search}.rs and tools/task/src/todo_write.rs:246. orchestrator/src/prompt/body_sections.rs:333 `has_opus_5_prompt_bundle` + :557 already gate SYSTEM-prompt sections on `Opus5PromptBundle`, proving the exact discriminator opus-5-vs-opus-4-8 is available and in production use — it just was never applied to the TOOL description layer.
CAVEAT ON THE RECORDED ORACLE CLAIM (verify before implementing): this agent is itself running as claude-opus-5, and its live Bash tool description contains BOTH the `- IMPORTANT: Avoid using this tool to run `cat`, `head`, `tail`, `sed`, `awk`, or `echo` commands…` bullet AND `- Command output is displayed to you, not reliably to the user.` (the latter immediately after the former). So the "avoid-list bullet is DROPPED for opus-5" half of the recorded finding is stale/mis-read; the ADDED bullet is real.
RESOLVED BY THE ORCHESTRATOR SESSION (which the subagent above could not check, having no Agent tool): that session also runs claude-opus-5 (`claude-opus-5[1m]`) AND holds the Agent tool, and its live Agent description still opens `## When to use` with "Reach for this when the task matches an available agent type, when you have independent work to run in parallel, or when answering would mean reading across several files — delegate it and you keep the conclusion, not the file dumps." The recorded "Agent drops its lead-in for opus-5" claim is therefore REFUTED by direct observation, on the same model the claim is about. Net: N-protocol-7 is one bullet to ADD to the Bash SHORT builder; nothing anywhere is to be removed. This matters because both deletion subclaims, if acted on, would have destroyed correct prompt text.

</details>

**阻塞：** DEPENDENCY NOW SATISFIED — this is no longer blocked on the sibling H1/M1/H2 capability-registry work. All three pieces exist on 370946160 and are live: platform-api/src/model_capabilities.rs (registry + `has_capability` + `normalize_model_id`), the lean predicate (`ModelCapability::LeanPrompt`, plus `is_lean_prompt_model` at orchestrator/src/prompt/body_sections.rs:393), and tool-api/src/model_prompt_gate.rs (`dh_simple_system_prompt`, consumed by eight tool crates). The opus-5-only discriminator `Opus5PromptBundle` is already used in production by body_sections.rs:333/557. Remaining prerequisite is a data question, not an infrastructure one: re-read 2.1.220 to confirm (a) the exact bullet text and its position in the SHORT Bash builder, (b) which capability gates it — `opus_5_prompt_bundle` is the only one that separates opus-5 from opus-4-8, so it is almost certainly that and NOT `lean_prompt`, (c) is CLOSED: both deletion subclaims are refuted by direct observation of live opus-5 sessions — the avoid-list bullet and the Agent lead-in are BOTH still served to opus-5. Scope is therefore add-one-bullet only. Do not delete existing text on the strength of the recorded claim.

**涉及文件：** `tools/shell/src/prompt.rs`, `tools/shell/src/bash.rs`, `tools/agent/src/agent.rs`, `tool-api/src/model_prompt_gate.rs`, `platform-api/src/model_capabilities.rs`

### `N-changelog-5`

**Screen-reader input announcements (2.1.218 deleted-text + typed-space echo, 2.1.219 per-character echo) are absent — the whole --ax-screen-reader mode is inert past its startup banner**

| 级别 | 量级 | 状态 |
|---|---|---|
| ⚪ Low | L | CLOSED |

In --ax-screen-reader mode the oracle drives an accessible input surface: 2.1.218 added spoken announcements of the DELETED TEXT for word deletions (Ctrl-W) and line deletions (Ctrl-U / dd), and fixed VoiceOver announcing "new line" instead of echoing the space the user actually typed; 2.1.219 then fixed the mode rewriting the ENTIRE input line on every keystroke so it echoes only the newly typed character. The port does less than the pre-2.1.218 baseline the item assumed. `apps/cli/src/ax_screen_reader.rs` resolves the flag/env/config gate correctly and prints `[Accessible screen reader mode: on]` at startup (apps/cli/src/lib.rs:935-942), but nothing downstream ever reads the gate: `is_enabled()` has no caller outside its own module, `subprocess_env()` has no caller at all, and `tui/src/screen_reader.rs` — the flat-text serializer plus the `word_wrap`/`diff_lines` helpers that were meant to be the announcement engine — is declared in tui/src/lib.rs:41 and consumed by nobody. The composer key handlers (tui/src/bottom_pane/mod.rs:1459, :1533, :1542; tui/src/vim.rs:148) have no accessibility branch, and the deletion primitives in tui/src/composer.rs discard the removed characters rather than returning them, so there is no value available to announce even if a sink existed. User-visible consequence: a blind user who passes --ax-screen-reader gets one banner line and then an ordinary ratatui frame — no flat-text transcript, no per-keystroke echo, no confirmation of what a word/line deletion removed, and the mode does not propagate into spawned child sessions. This is the classic computed-but-never-wired shape: the serializer and diff helpers exist and are unit-tested, but no publisher feeds them and no consumer reads them.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

Port-side, verified at the behaviour site (worktree /Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code @ 370946160):
(1) CONSUMER SEARCH, not definition search: `rg -n 'is_enabled\(\)' apps/cli/src tui/src` returns hits ONLY inside apps/cli/src/ax_screen_reader.rs itself (lines 106, 113, 124, 136). No TUI code branches on the gate. Repo-wide `rg 'LINGXI_AX_SCREEN_READER|CLAUDE_AX_SCREEN_READER'` hits only ax_screen_reader.rs plus two doc comments (core/src/settings/schema.rs:214, apps/cli/src/argv.rs:616).
(2) The whole gate surface reachable today is apps/cli/src/lib.rs:935-942 — `ax_screen_reader::init(...)` then `maybe_announce(...)`, which prints the one line `[Accessible screen reader mode: on]` and nothing else. `subprocess_env()` (ax_screen_reader.rs:123) has ZERO call sites (`rg -n subprocess_env` → only its own def/doc/test), so the mode does not even propagate to child processes.
(3) tui/src/screen_reader.rs is fully unwired dead code: `rg -n 'to_flat_text|diff_lines|word_wrap|AxNode|LineChange|SymbolKind' -g '!tui/src/screen_reader.rs'` returns no hit that refers to this module (only unrelated `edit_write_diff_lines` in tui/src/history_cell/tool.rs:80 and an LSP `SymbolKind`). tui/src/lib.rs:41 declares `pub mod screen_reader;` and nothing consumes it.
(4) Deletion announcement: the deleted text is never even captured. Composer::backspace (tui/src/composer.rs:303), delete_word (:467), kill_to_line_start (:475), kill_to_line_end (:483), delete_line (:490) all `remove`/`drain` and return `()`. Their key-handler call sites — tui/src/bottom_pane/mod.rs:1459-1461 (Ctrl-W → delete_word), :1533-1536 (Backspace), :1538-1541 (Delete), tui/src/vim.rs:148 (dd) — contain no accessibility branch.
(5) Typed-character echo: tui/src/bottom_pane/mod.rs:1542-1545 `KeyCode::Char(c) if !ctrl => self.composer.insert(c)` — no echo emission of any kind, so neither the pre-219 whole-line rewrite nor the 219 per-character echo exists.
(6) Not fixed by the reconciliation merge: `git log -- tui/src/screen_reader.rs apps/cli/src/ax_screen_reader.rs` shows the last touch is e2ab92008 ("2.1.218 parity wave"), whose entire diff to these files is a 1-line dead-store removal in `wrap_line_into` (`current_width = 0;`). No announcement code landed.
Both modules still self-document as 2.1.201 ports (apps/cli/src/ax_screen_reader.rs:3, tui/src/screen_reader.rs:9).

</details>

**阻塞：** Design decision on where the gate lives: `apps/cli` depends on `tui` (apps/cli/Cargo.toml:65), so the `tui` crate cannot call `apps/cli::ax_screen_reader::is_enabled()`. The gate must be relocated to `tui-core`/`traits` or re-read from LINGXI_AX_SCREEN_READER inside `tui` first. The prerequisite alternate flat-text render path (the consumer for tui/src/screen_reader.rs) is itself unwired and has to land before any per-keystroke or deletion announcement has somewhere to go.

**涉及文件：** `tui/src/screen_reader.rs`, `tui/src/composer.rs`, `tui/src/bottom_pane/mod.rs`, `tui/src/vim.rs`, `apps/cli/src/ax_screen_reader.rs`, `apps/cli/src/lib.rs`

### `N-changelog-4`

**No OSC 52 clipboard writer and no mouse-selection copy surface — `/copy` is subprocess-only, so the 2.1.219 GNU-screen DCS-passthrough fix has no behaviour site**

| 级别 | 量级 | 状态 |
|---|---|---|
| ⚪ Low | XL | CLOSED |

The oracle writes the clipboard through a multiplexer-aware `setClipboard` that base64-encodes the text, does a native copy (pbcopy / wl-copy / xclip / xsel / powershell, each also writing the X11 PRIMARY selection) when not over SSH, pushes the text into a tmux buffer via `tmux load-buffer -w -`, and then emits an OSC 52 clipboard write whose framing depends on the detected multiplexer: bare OSC 52 outside a mux, OSC 52 plus a `ESC P tmux; … ESC \` DCS passthrough under tmux, and — the 2.1.219 change — a chunked `ESC P … ESC \` DCS passthrough under GNU screen (`$STY`). That emitter is consumed both by `/copy` and by copy-on-select: in the fullscreen renderer the TUI captures the mouse, maintains a selection store, and on mouse-up (gated by the `copyOnSelect` setting, default on, surfaced as the "Copy on select" row in `/config`) copies the selection and shows a toast that names the transport used ("copied … to clipboard" / "copied … to tmux buffer · paste with prefix + ]" / "sent … via OSC 52 · if paste fails, hold Shift/Option/Fn while selecting for native copy").

The port has none of this. `tui/src/copy.rs::copy_to_clipboard_native` is a plain subprocess copy — first utility that spawns wins, no OSC 52, no PRIMARY selection, no tmux buffer, no SSH detection, no multiplexer detection — and it is reached from exactly one place, the `/copy` slash command. The TUI never enables mouse capture (only raw mode and bracketed paste), so there is no selection state and therefore no site at which a copy-on-select could fire; the fullscreen renderer the feature lives in is hard-disabled (`FULLSCREEN_ENABLED = false`) and `/tui`/`/focus` exist only as headless-fallback stubs. There is no `copyOnSelect` (or `copyFullResponse`) setting in the port's config at all.

User-visible consequence, in increasing order of how often it bites: (1) dragging the mouse over output never copies anything — users must fall back to the host terminal's own selection, which is at least still available since the port does not capture the mouse, so this is a missing convenience rather than a broken one; (2) `/copy` run inside tmux or GNU screen writes only to the pane process's own clipboard utility and never reaches the outer terminal's clipboard, so under `ssh` + `screen` the copied text lands on the remote host (or nowhere, if the remote has no pbcopy/xclip/wl-copy/xsel) and the user gets a "Copied to clipboard (N characters, M lines)" confirmation that is a lie; (3) on Linux there is no PRIMARY-selection write, so middle-click paste never sees `/copy` output. Because the port emits no OSC 52 at all, the 2.1.219 GNU-screen DCS-passthrough fix has literally nothing to apply to.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

Port (worktree /Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code @ 370946160): repo-wide `grep -rn ']52;' --include='*.rs'` → 0 hits; `grep -rni 'osc52|osc_52|copy_on_select|copyOnSelect'` → 0 hits; `grep -rni 'tmux|STY|DCS|passthrough'` finds only the swarm tmux backend (platforms/posix/src/swarm/) and platforms/pty PSEUDOCONSOLE_PASSTHROUGH_MODE — nothing clipboard-related. The only clipboard write path is tui/src/copy.rs:99 `copy_to_clipboard_native`, which shells out to pbcopy (macOS) / clip (Windows) / wl-copy→xclip→xsel (Linux) and returns after the first spawn succeeds — no OSC 52 fallback, no `--primary`/`-selection primary` second write, no `tmux load-buffer`, no SSH check (`grep -rn SSH_CONNECTION --include='*.rs'` → 0 hits). It is reached only from tui/src/app.rs:424 on `ChatOutcome::CopyToClipboard`, i.e. only from `/copy` (tui/src/chat_widget.rs:2444). There is no mouse surface at all: tui/src/terminal.rs:72-73 `TerminalSession::new` enables ONLY raw mode + `EnableBracketedPaste`; `grep -rni 'MouseCapture|EnableMouse|100[0236]h'` → 0 hits anywhere in tui/tui-core, so there is no selection store to hang copy-on-select off. (The comment at tui/src/connect/screen.rs:265-266 claiming "the TUI captures the mouse" is stale/false.) The fullscreen renderer that owns this feature in the oracle is explicitly absent: tui-core/src/collapse/classify.rs:15 `const FULLSCREEN_ENABLED: bool = false;` and commands/core/src/register.rs:418-421 registers `/tui` and `/focus` as headless-fallback stubs because "the fullscreen renderer" does not exist. Oracle (~/.local/share/claude/versions/2.1.220, byte 230284700-230286600) confirms `FC(e)` = setClipboard: base64s the text, calls native `iRu(e)` when not SSH (pbcopy / wl-copy + wl-copy --primary / xclip clipboard + primary / xsel / powershell), awaits `o7g(e)` = `tmux load-buffer -w -` with a `tmux load-buffer -` retry, then picks an emit mode from `Las()` (TMUX→"tmux", STY→"screen", else null): tmux ⇒ raw `ESC ]52;c;<b64> ST` plus `OB()` DCS wrap `ESC P tmux; <esc-doubled> ESC \\`; screen ⇒ chunked `ESC P ESC ]52;c;<chunk> ESC \\` segments; else plain `Pw(US.CLIPBOARD,"c",b64)`. The copy-on-select behaviour site is `BJo` (byte 241359000): subscribes to the selection store, skips while `isDragging`, gates on `Rt().copyOnSelect ?? true`, calls `copySelectionNoClear()`, fires `be("clipboard_write")` telemetry and the `FJo` toast ("copied N chars to clipboard" / "copied N chars to tmux buffer · paste with prefix + ]" / "sent N chars via OSC 52 · if paste fails, hold {mod} while selecting for native copy"); the `/config` row is gated by `ds()` (fullscreen) at byte 236110724.

</details>

**阻塞：** Copy-on-select itself is blocked on a mouse-capture + selection-store surface (the oracle's fullscreen renderer), which the port hard-disables (tui-core/src/collapse/classify.rs:15) and has never built — that is the XL part. The transport half is NOT blocked: an OSC 52 writer with SSH detection ($SSH_CONNECTION), multiplexer detection ($TMUX/$STY), tmux `load-buffer -w -`, the tmux/screen DCS passthrough wrappers, and the X11 PRIMARY second write can be dropped into tui/src/copy.rs::copy_to_clipboard_native behind the existing ChatOutcome::CopyToClipboard wiring as a self-contained M, and would fix the real `/copy`-over-ssh+tmux/screen bug on its own. Recommend splitting the item.

**涉及文件：** `tui/src/copy.rs`, `tui/src/app.rs`, `tui/src/terminal.rs`, `tui-core/src/terminal_setup.rs`, `tui-core/src/collapse/classify.rs`, `commands/core/src/register.rs`, `tui/src/connect/screen.rs`

### `N-protocol-8`

**Memory-backend push mass-delete hold (and its CLAUDE_CODE_DISABLE_MEMORY_MASS_DELETE_HOLD escape hatch) absent — duplicate of N-env-3, not a distinct hole**

| 级别 | 量级 | 状态 |
|---|---|---|
| ⚪ Low | — | DUPLICATE |

Oracle 2.1.220 runs a user+team multistore memory sync engine that pushes local memory entries to a first-party backend. Before a push it computes a mass-delete threshold (`Ity(e)`): `max(50, floor(entries * 0.1))`, and a push whose delete set reaches that threshold is HELD rather than applied — a data-loss safety net that is ON by default. `CLAUDE_CODE_DISABLE_MEMORY_MASS_DELETE_HOLD` (new in 2.1.218) makes the threshold POSITIVE_INFINITY, disabling the hold. The hold sits alongside the pre-existing push-delete modes corroborate|immediate|never (CLAUDE_CODE_MEMORY_PUSH_DELETE_MODE / tengu_mem_push_delete_mode) and the push_written/push_deleted/conflicts accounting. The port has no backend memory sync at all: the memory crate is purely local-filesystem (LINGXI.md hierarchy loader, memdir scanner/ranker, retention, prefetch, surfacing), and its only team-shaped component, TeamMemoryWatcher, is a read-only mtime poll over ~/.lingxi/team-mem that never writes, never deletes, and is not even constructed outside its own tests. Consequently neither the hold, the threshold constants (50 / 10%), the delete modes, nor the env escape hatch has any consumer to attach to. User-visible consequence today is nil — with no push path there is no mass-delete to hold, so no data can be lost — and it only becomes real if/when backend memory sync is ported; setting CLAUDE_CODE_DISABLE_MEMORY_MASS_DELETE_HOLD in LingXi is silently inert. TRIAGE — this is NOT distinct from N-env-3: both entries describe the identical oracle function (Ity, max(50, 10% of entries)), the identical env var introduced in 2.1.218, the identical corroborate|immediate|never delete modes, and the identical missing prerequisite (the whole push/pull sync engine). N-env-3 found it via an env-var sweep and scoped it as "whole sync engine unported" (Medium/XL); N-protocol-8 found it via a protocol/backend-module sweep and scoped only the hold (Low/M). They are one hole seen from two angles. Recommendation: keep N-env-3 as the canonical item, fold N-protocol-8 into it as a cross-reference, and resolve them with a single product call — either port the backend memory sync subsystem (which drags in first-party account endpoints and is adjacent to the frozen team/swarm surface) or record one explicit Divergence(reason) covering both. Do not implement the hold or the env var on their own: a threshold constant with no push pipeline would be exactly the defined-but-never-wired shape this backlog exists to eliminate.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

Re-verified port-side in /Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code @370946160. (1) /Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/memory/src/lib.rs:10-22 declares the crate's entire module set — file, index_cap, lingxi_md, memdir, prefetch, retention, secret_scan, selector, session_memory, snapshot, surfacing, team_memory, tier. There is no sync/push/pull/backend module of any name, so the search is exhaustive at the module boundary, not a bare negative grep. (2) The only cross-machine-shaped code is memory/src/team_memory.rs — its own header (lines 1-3) says "poll-based hot-reload + event-time secret scan over <home>/.lingxi/team-mem/"; poll_changes (line 48) does read_dir + mtime compare + secret scan and returns changed PathBufs. No HTTP, no writer, no delete path, no conflict/counter bookkeeping. memory/Cargo.toml pulls in no http-client/llm-runtime. (3) `grep -rn MASS_DELETE` over the whole tree (excluding target/.git) returns exactly three hits, all in the backlog document itself: docs/claude-code-2.1.220-sessionB-wave-2026-07-26.md:56, :58 (N-env-3) and :94 (this item). Zero Rust hits. (4) `grep -rn -iE 'mass_delete|push_delete|memory_stream|push_written|tengu_mem_|MEMORY_PUSH'` over *.rs → 0 hits; the ~25 `corroborate` hits are all fork/resume identity checks (tasks/src/state.rs:169, session/src/forked_skill.rs:348, permission/src/auto_mode_propose.rs:157) and have nothing to do with memory push-delete modes. (5) Consumer-side check for a publisher: `grep -rn TeamMemoryWatcher --include=*.rs` matches only its own definition (memory/src/team_memory.rs:15,21) and its own unit tests (lines 120-159) — zero production consumers anywhere in engine/orchestrator/coordinator/tui, so even the LOCAL half of team memory is currently unwired. Nothing computes a delete count, so there is nothing for a hold to guard and nothing for the env var to switch off.

</details>

**阻塞：** Duplicate of N-env-3 — merge the two and triage once. Blocked on a product-scope decision: whether backend memory sync (first-party account push/pull endpoints, user+team multistore, push-delete modes) is in scope for LingXi at all. If yes, the hold is a small rider on an XL subsystem port; if no, close both with one explicit Divergence(reason). Zero standalone work either way.

**涉及文件：** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/memory/src/lib.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/memory/src/team_memory.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/memory/src/memdir/team_paths.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/docs/claude-code-2.1.220-sessionB-wave-2026-07-26.md`

---

## B. Ultra-review 相邻 follow-up

来自 §8。39 个确认发现已全部修复并在合并树中验证；这 6 项是各 lane 记录而未修的相邻问题，每项都需要动其 lane 所有权之外的文件。其中 2 项（`FU-mcpcli-5`、`FU-orch-7-currentdate`）在 review 中被判 refuted 的理由是**范围**，不是判断错了 oracle，因此同样入册。

### `FU-disabled-agent-server`

**A server named in `disabledMcpServers` is revived and connected by an agent-frontmatter `mcpServers` entry — the denylist is bypassed**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🔴 High | S | CLOSED |

The oracle evaluates its MCP disable predicate (`Fw`, the minified `eI`) by NAME at connect time, downstream of every merge — including the agent-frontmatter merge `FWt` — so a name listed in the project's `disabledMcpServers` denylist is rendered as `type:"disabled"` and never spawned, no matter which config source supplied it. The port precomputes the decision into `McpServerConfig::disabled` inside `apply_project_server_gate`, which runs BEFORE the agent merge. The MCPCLI-2 fix made an agent's replacement config carry `disabled = false` — correct for the rejected-`.mcp.json` case it targeted (the oracle's `afe` really does drop a rejected project server from the discovered map entirely, so the agent's entry is the only one left) — but because the port cannot tell WHY a config was marked disabled, the same code path also clears a denylist decision. Consequence: any server the user disabled via `disabledMcpServers` is re-enabled the moment an agent definition declares a server of that name, whether it replaces the user's discovered entry (inheriting the agent's transport spec and command line) or is pushed as a brand-new entry. Since agent definitions are loaded from `<cwd>/.lingxi/agents/*.md` — untrusted, repo-supplied content — and an MCP stdio server is arbitrary local code execution, a checked-in agent file plus `--agent <name>` (or a resumed `agentSetting`) silently defeats an explicit user denial. The `computer-use` allowlist half is NOT exposed: `agent_mcp_specs_to_scoped_configs` skips reserved names, so no agent can re-enable the builtin.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

CONFIRMED, and the security consequence is REAL. Port ordering in `apps/engine-desktop/src/lib.rs`: `:5767` load → `:5772-5778` `--mcp-config` merge → `:5786` `mcp::apply_project_server_gate` (the ONLY consumer of the denylist; `grep disabledMcpServers` across the tree matches only `mcp/src/server_gate.rs` plus two comments) → `:5793` enterprise policy → `:5899` `merge_agent_frontmatter_mcp_servers`. The merge (`:3649-3690`) does `Some(slot) => *slot = cfg` (`:3685`) or `None => existing.push(cfg)` (`:3686`), where `cfg` comes from `agent/src/mcp_servers.rs:88` → `mcp/src/json_config.rs:388` `disabled: entry.disabled`, i.e. `false` unless the agent's own frontmatter says otherwise. `mcp/src/registry.rs:1279` (`if config.disabled`) is the only connect-time check and reads the precomputed flag, and the reconnect guard `state_is_disabled` (`registry.rs:2277-2286`) likewise reads `config.disabled` — nothing re-consults the list by name. Agent markdown from a cloned repo can carry inline `mcpServers` (no source privilege gate: `agent/src/catalog.rs:703-726` parses records from any source; `agent/src/mcp_servers.rs:39` strict lock is passed `false` at `lib.rs:3671`). The current behaviour is PINNED by a test at `apps/engine-desktop/src/lib.rs:9563-9566` ("a rejected discovered server must not suppress the agent's") which cannot distinguish the two disable reasons. Oracle re-verified in the 2.1.220 binary: `Fw` (the `eI` gate) at offset 231830466 reads `disabledMcpServers` by NAME, and the connect loop `gKu` at ~232096400 applies it to the ALREADY-MERGED map — `for(let _ of n)if(Fw(_[0]))e({client:{name:_[0],type:"disabled",…}})` and again per server in `g=async([_,y])=>{if(Fw(_)){…type:"disabled";return}}`. The merged map is `po={...an,...Uo}` (offset 246009450) where `Uo` is the `FWt(Ot,_l,…)` agent-frontmatter merge (offset 246008900), fed to `sEm`/`prefetchAllMcpResources`. So in the oracle the agent's server is gated by name; in the port it connects.

</details>

**涉及文件：** `mcp/src/server_gate.rs`, `apps/engine-desktop/src/lib.rs`, `mcp/src/registry.rs`, `agent/src/mcp_servers.rs`, `mcp/src/json_config.rs`

### `FU-orch-7-currentdate`

**`# currentDate` additional-context entry is recomputed live every turn instead of using the memoized session-start date**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | S | CLOSED |

The oracle's leading additionalContext meta message renders `# currentDate` from `LGe()`, a session-memoized snapshot of the local date (`LGe = Vr(wcs)`, cleared only by `clearSessionCaches`, i.e. session start / `/clear` / resume). The whole userContext builder `jA` is itself memoized on the cwd, so within one session at one cwd the meta message body is byte-stable; when a session crosses local midnight the oracle keeps showing the SESSION-START date in `# currentDate` and communicates the rollover exclusively through the one-shot `date_change` system-reminder. The port instead calls `current_date_string()` fresh inside `additional_context_message()`, which is re-run and re-prepended at index 0 of the outgoing snapshot on every single model call. Consequences: (1) prompt-cache breakage — the first request after local midnight mutates message[0], the very front of the cacheable message prefix, so the entire conversation must be re-processed as uncached input tokens on that request (a real, repeatable cost for overnight, cron- and `--bg`-driven sessions, which LingXi ships); (2) content divergence — the port announces the new date twice, once via a now-updated `# currentDate` and once via the `date_change` reminder the port already emits correctly, whereas the oracle sends only the reminder. Nothing else in the port depends on the live read, and the exact memoized value the oracle uses is already materialised in `DateChangeState.session_date`, so the fix is a small one: have `additional_context_message` read (and, if unseeded, seed) that per-session date under the same `session_id` key the turn loop already passes to `date_change_reminder_message` at `turn_loop.rs:534` / `conversation.rs:7203`. Watch the seeding order — `additional_context_message` runs before the `date_change` producer in a turn, so whichever runs first must seed the shared memo, exactly as `Vr(wcs)` does.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

PORT — `orchestrator/src/conversation.rs:9539-9543` pushes `format!("# currentDate\nToday's date is {}.", crate::prompt::env_meta::current_date_string())` on every call. `orchestrator/src/prompt/env_meta.rs:176-181`: `current_date_string()` is a bare `chrono::Local::now()` format with no memo. `additional_context_message` is invoked on every model call from five sites — `orchestrator/src/turn_loop.rs:378` (`history_snapshot.insert(0, ctx_msg)`) and `orchestrator/src/conversation.rs:7038`, `:7363`, `:7601`, `:7751` — and its own doc comment at `:9480-9482` states "this is recomputed and prepended to the OUTGOING snapshot each turn". The memoized session-start date DOES already exist in the port but is not consumed here: `conversation.rs:859` `DateChangeState { session_id, session_date, delivered_date }`, seeded on first producer run at `:9981-9987` and read only by `date_change_reminder_message` (`:9967-9999`). ORACLE (2.1.220, verified in the binary) — the userContext builder is `jA=Vr(async()=>{ … , currentDate:`Today's date is ${LGe()}.`},Dds)` at @230827185 (the `Vr(...)` memoize wrapper with cache key `Dds=()=>kt()` @230824214, i.e. keyed on cwd), and `LGe=Vr(wcs)` @230540156 where `wcs()` @230539891 is the local `YYYY-MM-DD` builder — so the date itself is memoized session-wide, independent of the cwd key. `clearSessionCaches` @236058855 clears `jA.cache`, `pk.cache`, `LGe.cache` together, i.e. the memo lives exactly one session. Recorded as refuted-for-scope, not refuted-on-substance, at `docs/claude-code-2.1.220-sessionB-wave-2026-07-26.md` §8.2 (ORCH-7 row) and §8.1.

</details>

**涉及文件：** `orchestrator/src/conversation.rs`, `orchestrator/src/prompt/env_meta.rs`, `orchestrator/src/turn_loop.rs`

### `FU-mcp-get-health`

**`mcp get` never health-checks a connectable server and prints no `Status:` line for it**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | M | CLOSED |

The oracle's `mcp get` handler (`hJy` @238844777) always resolves a status and prints a `Status:` line for every server: pending and rejected short-circuit to their fixed strings, and EVERY other server goes through `yEp(t,i)`, which performs the same spawn/connect probe `mcp list` uses and reports `✔ Connected` or `✘ Failed to connect` with an `Issue:` line. The port's `run_get` prints `Status:` only on the pending, rejected and config-error/unconfigured branches; a healthy or merely broken-at-runtime server takes none of them, so the command prints the server's scope and transport and stops. User-visible consequence: `mcp get <name>` is silent about whether the server actually works. A user debugging a server that fails to start sees a perfectly normal-looking record with no error, and must run `mcp list` (which probes every server) to discover the failure — the opposite of the per-server drill-down the command exists for. The output also diverges from the oracle's line set, so any script or doc that greps `mcp get` output for `Status:` finds nothing for exactly the servers that are working.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

`apps/cli/src/commands/mcp.rs:2000` — `fn run_get(a: &GetArgs) -> i32` is SYNCHRONOUS and holds no transport/registry. The only `Status:` prints are `:2025` (pending), `:2027` (rejected) and `:2031` (the `unconnectable_status` config-only branch — `- Not configured` / `✘ Failed to connect` + `Issue:`). Control then falls straight into the transport dump at `:2036-2059` and the removal hint at `:2062-2065`, so an approved, well-formed server yields Name / Scope / Type / Command|URL with NO Status line at all. Contrast `run_list` (`:1911`, async) which does dial: `registry.connect(cfg.clone())` under `HEALTH_CHECK_TIMEOUT` at `:1962-1976` producing `✔ Connected` / `✘ Failed to connect — <err>`. MCPCLI-1 only retargeted the config-only branch (the `unconnectable_status` split at `:1900-1909`); it added no probe. The dispatcher already awaits (`:328-329` `Sub::List => run_list().await, Sub::Get(a) => run_get(a)`), so nothing structural blocks making `run_get` async.

</details>

**涉及文件：** `apps/cli/src/commands/mcp.rs`

### `FU-afe-shadow`

**A pending/rejected project `.mcp.json` server that shadows a USER server deletes the user's server instead of standing down**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | M | CLOSED |

The oracle's `afe` loader treats a project `.mcp.json` entry that is not yet approved as a no-op when a server of the same name already exists in user, local or plugin scope: the project loop `continue`s before the pending/rejected arms, so the user's own server survives untouched and connects normally. (An APPROVED project entry still overrides, so the documented local > project > user precedence is unchanged for the approved case.) The port collapses all scopes into a single by-name map in which the project entry unconditionally overwrites the user entry, so the one surviving config carries `scope: Project`. Every downstream consumer keys off that scope: `mcp list` filters the row out as rejected, or labels it "Pending approval" and refuses to health-check it; `mcp get` prints the project entry's transport and the Rejected/Pending status; and at runtime `apply_project_server_gate` marks it disabled so it is never connected. User-visible consequence: dropping a `.mcp.json` into a repo whose server name collides with one the user added via `mcp add -s user` — or declining/disabling that project server — silently disables the user's own working server. The user sees their server disappear from `mcp list` with no message, and `mcp get <name>` shows the repo's command line instead of their own. It is fail-closed (nothing untrusted is executed), but a cloned repository can suppress an arbitrary user-scope MCP server just by naming a project server after it.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

Port: `mcp/src/json_config.rs:496-551` `load_mcp_servers` collapses every scope into one `HashMap<String, McpServerConfig>` — USER inserted first (500-513), PROJECT second (516-529, so it OVERWRITES the user entry), LOCAL last (532-546). The surviving entry for a shadowed name therefore carries `scope: Project`. `apps/cli/src/commands/mcp.rs:1919` then runs `listed_servers(load_all_servers(), &approval.rejected)` → `:1988-1996` filters on `is_rejected_project_server` (`:2321-2323`, `scope == Project && rejected.contains(name)`), so the row vanishes entirely; the same happens for pending via `is_pending_project_server` (`:2312-2314`). The doc comment at `:2305-2311` asserts the opposite ("a same-named USER or LOCAL server … take precedence in `load_mcp_servers`") — true for LOCAL, FALSE for USER, so the scope guard it calls SECURITY-RELEVANT does not actually fire for the user case. Runtime path is equally affected: `apps/engine-desktop/src/lib.rs:5767` loads the same collapsed list and `:5786` `apply_project_server_gate` marks the Project-scoped survivor disabled via the `disabledMcpjsonServers` arm (`mcp/src/server_gate.rs:177-183`). Oracle re-verified in the 2.1.220 binary at offset ~231822700 (`afe`): `for(let[D,x]of Object.entries(s)){let O=m(D); if(O==="approved"){_[D]=x;continue} if(D in a||D in i||D in l)continue; …}` — a NON-approved (pending OR rejected) project entry whose name exists in local (`a`), user (`i`) or plugin (`l`) is skipped before the pending/rejected arms, and the final map `Object.assign({},b,i,_,a)` keeps the user row.

</details>

**阻塞：** Needs a decision on where the approved/pending/rejected classifier lives: `load_mcp_servers` currently has no access to it (it is computed in `apps/cli/src/commands/mcp.rs::project_server_approval` and, for the reject arm only, in `mcp/src/server_gate.rs`). Either plumb the classifier into the loader so the project insert can be skipped when the name already exists from USER/LOCAL and the entry is not approved, or keep per-scope maps and resolve late.

**涉及文件：** `mcp/src/json_config.rs`, `apps/cli/src/commands/mcp.rs`, `apps/engine-desktop/src/lib.rs`, `mcp/src/server_gate.rs`

### `FU-mcpcli-5`

**JSON agent parser rejects an empty `{}` record inside `mcpServers`, dropping the agent (and every `--agents` agent) instead of warning per entry**

| 级别 | 量级 | 状态 |
|---|---|---|
| ⚪ Low | S | CLOSED |

The oracle's JSON agent schema declares `mcpServers: z.array(z.union([z.string(), z.record(z.string(), McpServerConfigSchema)])).optional()`. Because zod's `z.record` happily parses an empty object, an array item of `{}` is a VALID spec: the agent definition is built and kept, and the later conversion `agentMcpSpecsToScopedConfigs` warns `[Agent: <name>] Invalid MCP server spec: expected exactly one key` and skips only that entry. The port's `parse_mcp_servers_json_strict` adds an unstated `!map.is_empty()` guard, so an empty record is classified as a schema violation and the ENTIRE agent definition is thrown away with only a `debug!` line. On the `--agents <json>` CLI flag — the only production caller — that single rejected entry escalates further: `parse_agents_from_flag_json` treats any per-agent parse failure as an all-or-nothing record-schema throw and returns an empty vector, so every agent supplied on the command line silently vanishes and the session boots with only the built-in/dir agents. Note that the literal reading of the recorded claim (a top-level `"mcpServers": {}` non-array object) is NOT a divergence: the oracle's `z.array(...)` rejects it and throws, and the port drops the agent too — they agree. The real, still-open defect is an empty object as an ARRAY ITEM, i.e. `"mcpServers": [{}]`. The fix is to drop the `!map.is_empty()` guard so an empty record becomes `AgentMcpServerSpec::Record({})` and is handled by the already-correct `record.len() != 1` warn+skip in `agent/src/mcp_servers.rs`, plus a regression test that the agent survives.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

PORT: `agent/src/catalog.rs:1201` guards the record arm with `serde_json::Value::Object(map) if !map.is_empty() =>`; an empty object therefore falls through to `_ => return Err(())` at `catalog.rs:1214`. The caller at `catalog.rs:943-949` maps that `Err` to `tracing::debug!("Error parsing agent '{name}' from JSON: invalid mcpServers"); return None` — the whole agent definition is discarded. On the production `--agents` path, `parse_agents_from_flag_json` (`catalog.rs:1135-1142`) converts that `None` into `return Vec::new()`, so ALL flag agents are lost, and that is the only production consumer (`apps/engine-desktop/src/lib.rs:3599`, `merge_cli_flag_agents`). ORACLE 2.1.220 @231505497: `eju=Se(()=>E.union([E.string(),E.record(E.string(),r1e())]))` and `tju=...mcpServers:E.array(eju()).optional()...` — `z.record` accepts `{}` (no keys to validate), so `[{}]` passes the schema and the agent is built. ORACLE `obs` (agentMcpSpecsToScopedConfigs) @231497090: `let n=Object.entries(r);if(n.length!==1){w(`[Agent: ${e.agentType}] Invalid MCP server spec: expected exactly one key`,{level:"warn"});continue}` — per-entry warn + skip, agent survives. The port ALREADY has that exact warn+skip at `agent/src/mcp_servers.rs:55-61` (`if record.len() != 1`), and its YAML frontmatter path at `agent/src/catalog.rs:726-737` correctly keeps an empty mapping as `AgentMcpServerSpec::Record({})` — so the divergence is JSON-path-only and the downstream handler is already in place. No test asserts the current behaviour (grep of `agent/src/catalog.rs` tests: only `[123]` and `"slack"` invalid-mcpServers cases exist), so no anti-parity test blocks the fix.

</details>

**涉及文件：** `agent/src/catalog.rs`, `agent/src/mcp_servers.rs`

### `FU-orch-decision-class`

**tool_decision OTEL label ignores the host's explicit decisionClassification, and tool_parameters (HWr) has no port twin at all**

| 级别 | 量级 | 状态 |
|---|---|---|
| ⚪ Low | M | CLOSED |

The oracle derives the `tool_decision` / `code_edit_tool.decision` `source` label with `eQ_`, whose `permissionPromptTool` arm first reads the HOST's own `decisionClassification` off the `can_use_tool` tool result and returns it verbatim when it is one of `user_temporary` / `user_permanent` / `user_reject`, only falling back to "temporary for allow, reject for deny" when the host omits it or sends an invalid value. The oracle also attaches a `tool_parameters` attribute built by `HWr` (bash_command, full_command, timeout, description, dangerouslyDisableSandbox, mcp_server_name, mcp_tool_name, skill_name, subagent_type) to both `tool_decision` and `tool_result` records whenever `OTEL_LOG_TOOL_DETAILS` is truthy. The port does neither: the stdio control-plane parser in `map_payload` discards `decisionClassification` (the field never crosses the `PermissionOutcome` seam, which has no place to carry it), so the turn loop unconditionally hardcodes `user_temporary` for every host-approved tool, and the port has no `HWr` twin at all, so `tool_parameters` is never emitted on any record. User-visible consequence: an SDK/stdio host that classifies its grant as a PERMANENT one is reported to the collector as `user_temporary`, so OTEL dashboards under-count persisted permission grants; and operators who opt in with `OTEL_LOG_TOOL_DETAILS=1` get no tool-parameter detail on any `claude_code.events` record, silently losing the whole opt-in payload the flag exists to produce. Both are telemetry-only — no behaviour on the tool-execution path changes — which is why the wave documented rather than fixed them. Fixing needs a new field on `PermissionOutcome::Allow` plus the `control_plane.rs` parse for half (a), and a port of `HWr` plus its wiring into `record_tool_permission_decision` (and ideally the `tool_result` bridge) for half (b); note `logs::tool_details_enabled()` already exists and is currently dead, so it is the natural gate to consume.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

PORT — (a) decisionClassification: `orchestrator/src/turn_loop.rs:3059-3082` is the permissionPromptTool arm; it hardcodes `decision_otel_source = "user_temporary"` at :3078 and carries an explicit `// DEFERRED: the port does not yet parse `decisionClassification` (it would have to come up through `PermissionOutcome::Allow`)` comment at :3074-3077 (added by `6a8d96377`). The only stdio parse site, `apps/cli/src/control_plane.rs:631-699` (`map_payload`), reads exactly `behavior`, `updatedInput`, `updatedPermissions`, `message`, `interrupt` — `decisionClassification` is never looked up. `platform-api/src/permission_gate.rs:173-199` `PermissionOutcome::Allow` has only `updated_input` + `permission_updates`; there is no classification field. Repo-wide grep for `decisionClassification|decision_classification` over `**/*.rs` returns 3 hits, all comments in turn_loop.rs. (b) tool_parameters: `telemetry/src/otel/runtime.rs:603-615` emits the `tool_decision` log record with exactly 5 attrs (decision, source, tool_name, tool_use_id, tool_source) — no `tool_parameters`. There is no `HWr` twin anywhere: grep for `HWr|bash_command|full_command` across `telemetry/` and `orchestrator/` returns nothing. `telemetry/src/otel/logs.rs:36-40` defines `tool_details_enabled()` but it has ZERO consumers repo-wide (defined-never-wired); `telemetry/src/otel/config.rs:437,451` parses `OTEL_LOG_TOOL_DETAILS` into `LogIncludeFlags.tool_details`, whose only consumer is `runtime.rs:1091-1105` `gated_proto_attr`, which gates a different attribute family (`command`/`url`/`slug`/`server_name`/`tool_name`/`branch_name` on tengu-bridged records), not a `tool_parameters` body. ORACLE (2.1.220, verified in the binary) — `eQ_` @235395945: `case"permissionPromptTool":{let n=e.toolResult?.decisionClassification;if(n==="user_temporary"||n==="user_permanent"||n==="user_reject")return n;return t==="allow"?"user_temporary":"user_reject"}`. Dispatch-site emit @235411416: `vc("tool_decision",{decision:ce,source:se,tool_name:ua(e.name),tool_use_id:t,...Jro(e.mcpInfo),...Object.keys(ne).length>0&&{tool_parameters:Ie(ne)}},…)` with `ne=HWr(e.name,b,e.userFacingName?.(void 0),e.mcpInfo)`. `HWr` @228877052 early-returns `{}` when `!yg()` unless `Yro(mcpInfo)` (`serverType==="sdk"&&$Me()`, unreachable in the port), and `yg()` @~228878000 is literally `return Z.OTEL_LOG_TOOL_DETAILS`. Documented on the backlog at `docs/claude-code-2.1.220-sessionB-wave-2026-07-26.md:210-214`.

</details>

**涉及文件：** `orchestrator/src/turn_loop.rs`, `apps/cli/src/control_plane.rs`, `platform-api/src/permission_gate.rs`, `telemetry/src/otel/runtime.rs`, `telemetry/src/otel/logs.rs`, `telemetry/src/otel/config.rs`

---

## C. 携出的 deferred 碎片

来自 §6。合并不阻塞它们，但它们此前只存在于各 lane 的 summary 里，没有统一去处。

### `P1-user_prompt-log`

**`claude_code.user_prompt` OTEL log record has no emit site — `user_prompts_enabled()` is defined but never called**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | S | CLOSED |

Claude Code's enterprise OpenTelemetry monitoring emits a `claude_code.user_prompt` log record on the `claude_code.events` logger every time a prompt is submitted, carrying `prompt_length`, `prompt.id`, `message.uuid` and — only when `OTEL_LOG_USER_PROMPTS` is enabled — the prompt text itself (otherwise the literal `<REDACTED>`); the slash-command path swaps `message.uuid` for `command_name`/`command_source`. LingXi ported the gate predicate (`logs::user_prompts_enabled`) and the whole emit substrate, and wired every sibling record (`assistant_response`, `tool_result`, `tool_decision`, `api_request`, `api_error`), but never added the `user_prompt` call site at the prompt-submit seam. The consequence for anyone running LingXi with `LINGXI_ENABLE_TELEMETRY` against an OTLP collector is that one of the five documented `claude_code.*` events is simply never produced: prompt counts, prompt-length distributions and prompt-to-response correlation (via `prompt.id`) are all missing from enterprise dashboards, and `OTEL_LOG_USER_PROMPTS=1` has no effect at all. The natural seam is `ConversationOrchestrator::fire_user_prompt_submit`'s three call sites in `orchestrator/src/conversation.rs` (lines 6658, 6885, 8682), the port's equivalent of the oracle's `p2_`/`RPp` input-prompt handlers.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

`telemetry/src/otel/logs.rs:30-34` defines `pub fn user_prompts_enabled()` (the `OTEL_LOG_USER_PROMPTS` gate); a repo-wide grep for `user_prompts_enabled` returns exactly ONE hit — its own definition. No call site anywhere: computed, never wired. The port documents this itself at `telemetry/src/otel/mod.rs:52-54` ("Still open: the `user_prompt` named log record (its CC emit site is the prompt-assembly seam, unported)") and `telemetry/src/otel/record.rs:51` ("`user_prompt` still lacks a port emit site (the prompt-assembly seam) — the one documented remainder"). `user_prompt` appears in `record.rs` only as a test fixture string (lines 593/605/636/670). The emit machinery is ready: `telemetry/src/otel/runtime.rs:545 emit_named_log_event()` and `emit_log_event()` (runtime.rs:310-328, `claude_code.events` logger, sets event name + body) work — the sibling `assistant_response` record is wired from `orchestrator/src/turn_loop.rs:818`. Oracle 2.1.220 emits it from two adjacent prompt-assembly sites: `vc("user_prompt",{prompt_length:String(e.length),prompt:PVr(e),"prompt.id":ae,"message.uuid":Ee})` (plain prompt path, next to `M("tengu_input_prompt",…)`), and `vc("user_prompt",{prompt_length,prompt:PVr(P),"prompt.id",command_name,command_source})` on the slash-command path, where `PVr(e)= eNg()?e:"<REDACTED>"` and `vc` stamps `event.name`/`event.timestamp`/`event.sequence` and a `claude_code.user_prompt` body.

</details>

**涉及文件：** `telemetry/src/otel/logs.rs`, `telemetry/src/otel/runtime.rs`, `telemetry/src/otel/record.rs`, `telemetry/src/otel/mod.rs`, `orchestrator/src/conversation.rs`

### `M6-flagSettings-env-tier`

**The `--settings` (flagSettings) `env` block is never folded into the enterprise MCP policy's deny-side fallback expansion env — its slot in `U__` is documented and left empty**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | S | CLOSED |

The oracle expands enterprise MCP policy predicates against a frozen startup env snapshot overlaid with the managed sources' `env` blocks, and additionally gives the DENY side a fallback env assembled from four tiers in order: the global config, `userSettings`, `flagSettings` (the `env` block of whatever file `--settings` pointed at), then `policySettings` — later wins. The port implements three of those four and leaves the `flagSettings` slot empty, with no parameter on `policy_expansion_env_with` through which the composition root could supply it. Consequence: an enterprise denylist entry whose `serverCommand`/`serverUrl` predicate references a `${VAR}` that is defined only in a `--settings` file's `env` block never expands — the `${VAR}` stays literal, the pattern fails to match the real server, and a server the managed policy intended to block is allowed through. It is a narrow under-deny (it needs a policy predicate that depends on a variable only the CLI settings file defines), but it is an enterprise-policy bypass, and the allow side is unaffected since the oracle deliberately gives it no fallback env.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

Referent located at the behaviour site, and the port documents the hole itself: /Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/mcp/src/enterprise_policy.rs:403-405 — "(The `--settings` flagSettings tier is not plumbed into this crate; its slot in the fallback fold is documented here and intentionally empty.)" and :478-479 — "claude `U__().fallbackEnv` — globalConfig → userSettings → (flagSettings, unplumbed) → policySettings `env` blocks, later wins." The fold itself, mcp/src/enterprise_policy.rs:510-520, iterates exactly `[global_config, user_settings]` then `managed` — three tiers where the oracle's `Object.assign({}, globalConfig, userSettings, flagSettings, policySettings)` has four. `policy_expansion_env()` (:486-493) takes no flagSettings input, and neither does the parameterized core `policy_expansion_env_with(dir, startup_snapshot, global_config, user_settings)` (:498-503) — so there is no seam a caller could fill even if it wanted to. Production reach confirmed: `is_denied_with_env`/`is_allowed_with_env` call `policy_expansion_env()` at :925 and :981, and those are reached from apps/cli/src/commands/mcp.rs:1555/1558 (`mcp add` gate) and apps/engine-desktop/src/lib.rs:5793 (`apply_enterprise_mcp_policy`) / :3675 (`is_server_allowed` in the agent-frontmatter merge). Cross-crate check: the parsed `--settings` layer exists but only in `engine` (core/src/settings/mod.rs:118-123, core/src/settings/tracer.rs:24), and mcp/Cargo.toml does not depend on `engine`, so nothing bridges the two. Repo-wide `flagSettings|flag_settings|FlagSettings` returns hits in permission/, agent/, sandbox/, hooks/ — and zero in the mcp policy-expansion path other than the two comments above.

</details>

**阻塞：** Needs an injection seam: `mcp` does not depend on `engine`, so the parsed `--settings` env block has to be primed from the composition root the same way `mcp::enterprise_policy::prime_startup_env()` is primed at apps/cli/src/lib.rs:435 (or `policy_expansion_env_with` gains a fourth tier parameter threaded through `is_denied_with_env`/`is_allowed_with_env`). No decision blocks the work.

**涉及文件：** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/mcp/src/enterprise_policy.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/apps/cli/src/lib.rs`

### `M7-plugin-warn-variant`

**No warn variant naming WHY agent-frontmatter MCP servers were skipped; the `strictPluginOnlyCustomization` (plugin-lock) branch is hardcoded unreachable, and plugin agents are hard-rejected instead of warn+ignored**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | M | CLOSED |

The oracle has a single warn variant covering every reason an agent's frontmatter `mcpServers` gets dropped: it computes one reason token (`strictPluginOnlyCustomization`, `--strict-mcp-config`, `--safe-mode`/`--bare`, `remote mode`, `enterprise MCP config`), logs `[Agent: X] Skipping frontmatter MCP servers: blocked by <reason> (agent source: Y)` at warn level, and hands the server names plus the reason to the caller's `onBlocked` callback. The port's merge returns silently at each of those gates — a user whose agent declares MCP servers that never appear gets no reason, anywhere. Compounding that, the plugin-lock reason can never fire here: the `strictPluginOnlyCustomization` branch is ported verbatim but its caller passes a hardcoded `false`, and `StrictPluginOnlyPolicy` is never populated from managed settings by any loader, so a `strictPluginOnlyCustomization: ["mcp"]` managed policy is silently ignored and user/project agents' MCP servers connect anyway. In the opposite direction the port is harsher than the oracle in the one place the oracle is permissive: any plugin whose agent markdown merely CONTAINS the substring `mcpServers` — including inside a description or a comment — fails validation and takes the entire plugin down with it, where the oracle loads the plugin and honours those servers.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

Three port-side facts, each at the behaviour site. (1) The plugin lock is ported but DEAD: agent/src/mcp_servers.rs:39-46 holds the byte-exact `[Agent: X] Skipping frontmatter MCP servers: strictPluginOnlyCustomization locks MCP to plugin-only (agent source: Y)` branch behind a `strict_plugin_only_mcp: bool` parameter, and the ONLY production caller passes a literal `false` — apps/engine-desktop/src/lib.rs:3671, with the comment at :3667-3670 admitting "the composition root never populates the strict policy today (`StrictPluginOnlyPolicy::empty()` above), so the lock is always open". `StrictPluginOnlyPolicy` has no loader at all: plugin/src/strict_policy.rs offers only `empty()` (:39-44), and every construction site repo-wide is `StrictPluginOnlyPolicy::empty()` (apps/engine-desktop/src/lib.rs:8371 plus 8 test sites). (2) NO warn variant exists: apps/engine-desktop/src/lib.rs:3660-3667 returns `Vec::new()` silently at the safe-mode, `--strict-mcp-config` and managed-MCP gates; the only user-facing string in the whole merge is the enterprise-policy one at :5916. Oracle @233268016 (`_2_`) computes a single reason token — `o="strictPluginOnlyCustomization"` / `"--strict-mcp-config"` / `"--safe-mode"` / `"--bare"` / `"remote mode"` / `"enterprise MCP config"` — then emits ``w(`[Agent: ${e.agentType}] Skipping frontmatter MCP servers: blocked by ${o} (agent source: ${e.source})`,{level:"warn"})`` AND calls `r?.(names, o)`; zero hits for `blocked by` in the port's agent/MCP path. (3) Plugin agents are hard-rejected: plugin/src/agent_validation.rs:36-38 returns `McpServersForbidden` on a raw SUBSTRING scan of the frontmatter YAML for `mcpServers`, and plugin/src/manager.rs:600-604 turns that into `PluginManagerError::Validation`, failing the WHOLE plugin (comment at :584-586: "a privilege-escalating agent rejects the whole plugin, not just the agent"). In the oracle, `plugin` is one of `wke`'s trusted sources (agent/src/mcp_servers.rs:100-105 ports the same set), so a plugin agent's `mcpServers` is exactly the case the oracle honours — and the only one it honours once the lock is on. The two halves of `obs` and `FWt` the port DID land were re-verified against 2.1.220 @231497050 and @245974400 this session and are 1:1.

</details>

**阻塞：** The plugin-lock arm is blocked on `StrictPluginOnlyPolicy` gaining a loader that reads managed `strictPluginOnlyCustomization` (bool or string array) — no code reads that setting today. The warn-variant string + `onBlocked` reason, and relaxing plugin/src/agent_validation.rs from hard-reject to warn+ignore, are unblocked and can land first.

**涉及文件：** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/apps/engine-desktop/src/lib.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/agent/src/mcp_servers.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/plugin/src/agent_validation.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/plugin/src/strict_policy.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/plugin/src/manager.rs`

### `M12-attach-detach`

**←-on-empty in an attached background session never detaches back to the agents view (the `via:"detach"` arm of `kGt` is unwired)**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | M | CLOSED |

In the oracle, a session you attached to from the agents view knows it is a background session (`rs()`/`isBg`). Its left-arrow gesture therefore resolves to the DETACH arm rather than the open-agents arm: the armed hint reads "Press ← again to go back to agents", and confirming runs `Wet` → `E4e()`, which detaches the controller and drops you back in the fleet view while the session keeps running. A separate `GO_BACK_CONFIRM_HINT` ("Press ← again to go back") covers a host-supplied back handler, and `Wet` carries a 1s repeat guard keyed on the attach stamp so the second half of a fast double-tap cannot detach a session you just attached to. The port ported the three hint strings as byte-locked constants but wired only the open-agents one: the pane always reports `in_attach_quiet_window: false`/`attach_stamp_ms: 0`, always shows "Press ← again to open agents", and its only fire outcome is `OpenAgentsView`, which pushes the `/tasks` picker. A user attached to a LingXi background session (via `lingxi-cli agents` → Enter, or `lingxi-cli attach`) who presses ← therefore gets the wrong hint and the wrong action — a task picker stacked on top of the attached session instead of a clean detach back to the fleet — and must know to press Ctrl-Z, which nothing in the TUI advertises. The `tengu_left_arrow_blocked` counter (reasons `attach-quiet-hint`/`attach-quiet`/`editing-quiet`/`not-solo`) is also computed by `blocked_reason()` and never emitted.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

`tui/src/bottom_pane/mod.rs:1103-1150` — `left_arrow_hint()` unconditionally returns `OPEN_AGENTS_CONFIRM_HINT`, and `on_left_arrow_on_empty()` hardcodes `in_attach_quiet_window: false, attach_stamp_ms: 0`; the only outcome for `Fire` is `BottomPaneOutcome::OpenAgentsView` (mod.rs:1511). `tui-core/src/left_arrow_gesture.rs:52-58` defines `BACK_TO_AGENTS_CONFIRM_HINT` and `GO_BACK_CONFIRM_HINT`; a repo-wide grep for both names outside their own file and its byte-lock test returns ZERO consumers — defined, never wired. `BottomPaneOutcome` (mod.rs:124-170) has no Detach variant. `LeftArrowAction::blocked_reason()` (left_arrow_gesture.rs:82) likewise has no publisher anywhere — `tengu_left_arrow_blocked` is never recorded. The substrate exists but is not connected: `commands/core/src/stop.rs:69 is_bg_session()` (`LINGXI_SESSION_KIND=bg`) is consulted only by `/stop`, and `apps/cli/src/bg_attach.rs:62,1015-1052` implements an explicit Ctrl-Z controller detach that the TUI never triggers. Oracle: `kGt` = `if(yt.ok&&yt.via==="detach") return {handler:Wet, confirmHint:"Press ← again to go back to agents"}`, where `lY_` returns `{ok:!0,via:"detach"}` the moment `isBg` is true, and `Wet` = `if(now-RCt.current<1000 && Kke()<=RCt.current) return; RCt.current=now, E4e()` (E4e is the same detach primitive `/exit` uses when `rs()`).

</details>

**涉及文件：** `tui/src/bottom_pane/mod.rs`, `tui-core/src/left_arrow_gesture.rs`, `tui/src/chat_widget.rs`, `apps/cli/src/bg_attach.rs`, `commands/core/src/stop.rs`

### `M12-midturn-backgrounding`

**Mid-turn backgrounding state machine absent — ← never backgrounds the live conversation (`idle-fork`/`defer-then-fork`/`abort-then-fork`, defer cap, interstitials)**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | XL | CLOSED |

In the oracle the left-arrow gesture does not merely open a view: it BACKGROUNDS the current conversation and then shows the fleet. `lY_`/`aY_` classify the transition — `idle-fork` when nothing is in flight, `defer-then-fork` when a turn is running between tool calls (wait for the current tool to finish, showing "Backgrounding after the current tool finishes…", capped by `tengu_defer_cap_ms`, default 10s), and `abort-then-fork` when it must abort the in-flight request after flushing, carrying the partial assistant text, the boundary UUID and the restartable-subagent count into the forked job so work is not lost. It refuses outright when persistence is disabled, when the model ended the conversation, when queued commands or unsent draft text would be lost, and it emits `tengu_left_arrow_blocked` with the reason and in-flight kinds in each case. LingXi implements none of this: `open_agents_view()` pushes the `/tasks` picker over a conversation that keeps running in the FOREGROUND, so ← is a view toggle rather than a backgrounding gesture. The user-visible consequence is that there is no way to hand a running turn off to the background from the composer at all — the oracle's core "press ← to park this and go look at my other agents" workflow is missing, and mid-turn ← silently does something different (opens a modal) rather than deferring/aborting-then-forking. The substrate for the fork half exists (`platform-api/src/bg_session_forker.rs` + `apps/cli/src/bg_session_forker.rs`, reachable today only via `/fork`); what is missing is the state machine, the interstitials, and the turn-loop signals (`isLoading`, `betweenCalls`, in-flight count/kinds, restartable count) that would drive it.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

Repo-wide grep across all `.rs` for `defer-then-fork`, `abort-then-fork`, `idle-fork`, `tengu_defer_cap`, `Backgrounding after`, `Still backgrounding`, `Cannot open agents`, `Cannot background`, `tengu_background_already_bg` returns ZERO hits. The port's whole fire path is `tui/src/bottom_pane/mod.rs:1511` → `tui/src/chat_widget.rs:3584` → `open_agents_view()` (chat_widget.rs:2612-2617), whose body is exactly `self.bottom_pane.show_tasks(self.task_snapshot().unwrap_or_default()); ChatOutcome::Continue` — no persistence check, no queued-command check, no in-flight check (`turn_running()` exists at chat_widget.rs but is never consulted on this path), no fork. Oracle `GLe` (the open-agents handler) does all of: `w1()` persistence block ("Cannot open agents — session persistence is disabled, so this conversation cannot be backgrounded."), `endedByModel` block, `dNs(qie())` queued-command block ("Cannot open agents — N queued command(s) would be lost…"), draft block, then `C2t({isBg:!1,isLoading:xo.isActive,betweenCalls:EIo(...),inFlight:dr})` → `aY_` → `via` ∈ {`idle-fork`,`defer-then-fork`,`abort-then-fork`} → `cue({via,replyOnResume,inflightCount,inflightKinds,restartableCount,partialChars,partialText,boundaryUuid,deferWaitMs,abortAfterFlush})`, with a `Ke("tengu_defer_cap_ms",1e4)` timer, the interstitials `"Backgrounding after the current tool finishes…"` / `ykd(n)` ("Still backgrounding after the current tool — waiting for N running subagent(s) so the work carries over. Press ← again to skip ahead and restart them from the beginning.") and the counters `tengu_defer_cap_refused_queued` / `tengu_defer_cap_refused_restartable`.

</details>

**阻塞：** The orchestrator must expose in-flight state to the TUI (is-loading, between-calls, in-flight task count/kinds, restartable-subagent count, partial assistant text + boundary uuid) before the defer/abort classification can be computed; today `ChatWidget` only has the coarse `turn_running()`.

**涉及文件：** `tui/src/bottom_pane/mod.rs`, `tui/src/chat_widget.rs`, `platform-api/src/bg_session_forker.rs`, `apps/cli/src/bg_session_forker.rs`, `orchestrator/src/turn_loop.rs`

### `M12-managed-row`

**`/config` has no `tengu_maple_sundial` collapsed read-only "Agents view" (`managedEnum`) row — the two agents-view rows are always editable**

| 级别 | 量级 | 状态 |
|---|---|---|
| ⚪ Low | S | CLOSED |

With the `tengu_maple_sundial` gate on, the oracle collapses the two per-setting agents-view rows in `/config` into a single row `{id:"agentsView", label:"Agents view"}` of type `managedEnum` whose value is the OR of the two underlying settings rendered as "on"/"off", and whose `onChange` is a no-op — i.e. the row becomes read-only and neither `leftArrowOpensAgents` nor `defaultToAgentsView` can be toggled from `/config`. Because the `/config` shorthand resolves keys against that same row list, `defaultToAgentsView` and `leftArrowOpensAgents` also stop being addressable by shorthand and fall through to the unknown-key answer. LingXi has no `tengu_maple_sundial` gate and no `managedEnum` row concept: `settings_lines()` always renders the two editable boolean rows and the shorthand always accepts both keys, so if Anthropic flips the gate server-side the port shows two toggles where the oracle shows one locked summary row, and lets the user change settings the oracle has made read-only. Gate default is OFF, so today the divergence is latent.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

Oracle 2.1.220 settings row list: `...X7e() ? ($H()||H7e() ? [{id:"agentsView",label:"Agents view",value:H7e()&&(t.leftArrowOpensAgents??!0)||$H()&&(t.defaultToAgentsView??!1)?"on":"off",type:"managedEnum",onChange(){}}] : []) : [...$H()?[{id:"defaultToAgentsView",label:"Open agents view by default",type:"boolean",…}]:[], ...H7e()?[{id:"leftArrowOpensAgents",label:`${DW} opens agents`,type:"boolean",…}]:[]]`, with `function X7e(){return Ke("tengu_maple_sundial",!1)}` (the same gate also drives `egr(e,t)`, e.g. the "Auto-scroll" → "Auto-scroll output" relabel). Port: `tui/src/bottom_pane/screen_view.rs:630-639` unconditionally pushes both editable rows whenever `agent_view_enabled`; `tui/src/chat_widget.rs:2352-2362` accepts both `/config` shorthand keys whenever `platform_api::agent_view::is_enabled()`. Repo-wide grep for `maple_sundial`, `managedEnum`, `managed_enum` and the label `"Agents view"` across all `.rs`/`.ts`/`.tsx` returns ZERO hits (the only `Agents view` strings are stale parity-ledger entries in `test-harness/tests/parity_claude_2_1_198.rs`). The port does port other default-off `tengu_*_sundial` gates behaviourally (`tools/file/src/edit.rs:180 telemetry::flag_bool("tengu_cedar_sundial", false)`), so the absence is a gap by the project's own convention, not an accepted "flag-gated ⇒ inert" case.

</details>

**涉及文件：** `tui/src/bottom_pane/screen_view.rs`, `tui/src/chat_widget.rs`, `tui-core/src/theme_persist.rs`

### `M5-issue-formatting`

**`Skipped — invalid MCP server config for "X": <issues>` renders three canned reasons instead of the oracle's zod issue list (and the diagnostics panel drops the ⚠ glyph and tree guides)**

| 级别 | 量级 | 状态 |
|---|---|---|
| ⚪ Low | M | CLOSED |

When an MCP server entry fails schema validation, the oracle reports every zod issue for that entry, formatted `<path.join('.') || "(root)">: <message with a leading "Invalid input: " stripped>` and joined with `"; "` — e.g. `Skipped — invalid MCP server config for "bad": url: expected string, received undefined`. The port substitutes a three-way canned guess (`command: Required` / `url: Required` / `invalid entry`), which is both incomplete (it can only ever name one field, never a second issue, and never a wrong-type issue) and factually wrong: `Required` is zod-3 phrasing, and 2.1.220 ships zod 4. Separately, the `mcp list` diagnostics panel is missing the oracle's ` ⚠` on the title line, the blank line after it, and the ` ├ ` / ` └ ` tree guides on each row. User-visible consequence: someone debugging a broken `.mcp.json` sees a plausible-looking but different reason than the documentation and the oracle produce, with any second problem in the same entry silently hidden, and the panel does not match the product it is imitating byte-for-byte.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

The id appears exactly once in the tree — /Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/docs/claude-code-2.1.220-sessionB-wave-2026-07-26.md:15 and :116 — as a bare name; no lane summary defining it survives (the scratchpad `gap220-evidence.json` was ephemeral). The referent is unambiguous from the owning module, which names it itself: /Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/mcp/src/config_diagnostics.rs:12-15 — "One string is not byte-reproducible: the `<issues>` detail … comes from Zod's validator; the port emits the exact message *shell* with a best-effort reason." The best-effort reason is `invalid_reason()` at mcp/src/config_diagnostics.rs:224-239, which returns exactly one of three literals: `"command: Required"` (:234), `"url: Required"` (:236), `"invalid entry"` (:238); consumed at :153. The port's own test locks the wrong string in (mcp/src/config_diagnostics.rs:491 asserts `…for "bad": url: Required`). LIVE oracle run (2.1.220, sandboxed HOME, project `.mcp.json` = `{"bad":{"type":"http"},"bad2":{"type":"stdio"}}`) prints `url: expected string, received undefined` and `command: expected string, received undefined` — never the word `Required`. Oracle source @231827517: ``let b=m.error.issues.map((C)=>{let A=C.message.replace(/^Invalid input: /,"");return `${C.path.join(".")||"(root)"}: ${A}`}).join("; ")`` — ALL issues, `"; "`-joined, `(root)` for an empty path. Second half: `od -c` of the same oracle run shows the panel title is `MCP config diagnostics ⚠\n\n` and rows are prefixed ` ├ ` / ` └ ` (last row `└`); the port prints `MCP config diagnostics` with no glyph and no blank line (apps/cli/src/commands/mcp.rs:2264-2267) and prefixes rows with two spaces (apps/cli/src/commands/mcp.rs:2300). The port's comment at apps/cli/src/commands/mcp.rs:2226 asserts "The ink panel's status glyph and tree guides have no plain-text equivalent" — the live oracle CLI, piped to a file, emits both.

</details>

**涉及文件：** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/mcp/src/config_diagnostics.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/apps/cli/src/commands/mcp.rs`

---

## D. 判定为部分完成的余项

来自 §4 的裁定：事件、载荷与触发点已闭合，余下部分留册。

### `H6-remainder`

**H6 remainder = the `register_repo_root` SDK control request (absent entirely), the DirectoryAdded matcher query (never wired), the hook results (discarded), and a wrong `source` token (`add_dir` vs the oracle's `slash_command`)**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | L | CLOSED |

The H6 event, payload and `/add-dir` firing site are genuinely closed. What remains is everything downstream of the fire plus the second entry point. First, the `register_repo_root` SDK control request is absent from the port entirely — an SDK or stream-json host cannot register a working directory mid-session at all; the request is answered `Unsupported control request subtype`. In the oracle it validates the target, adds it to the session's permission context, refreshes the sandbox configuration so sandboxed tools and permission state already see the directory before hooks run, fires DirectoryAdded with `source: "register_repo_root"`, and optionally reloads CLAUDE.md, skills and plugins. Second, `source` is documented in the port as the hook matcher query but `match_query_for` has no DirectoryAdded arm, so the query is `None` and every subscribed hook fires regardless of its declared matcher — a hook a user scoped to `register_repo_root` runs on every `/add-dir`. Third, the port discards the hook results: the oracle logs each failed hook's output at error level, renders each hook's `systemMessage` into the conversation as bounded context the model actually sees, and summarises the failure count; the port's `let _ = …execute(…)` drops all of it, so a DirectoryAdded hook's output is invisible to both the user and the model. Fourth, the port fires `source: "add_dir"` where 2.1.220 fires `"slash_command"` — the wire payload diverges, and once the matcher query is wired the documented matcher value would stop matching, so (b) and (d) must be fixed together.

<details>
<summary>核查证据（370946160 上重新确认）</summary>

Closed halves re-verified first: the event exists (hooks/src/events.rs:85-86, :507-514), the wire payload is byte-shaped and tested (hooks/src/hook_payload.rs:754-775, hooks/src/hook_payload_test.rs:1310-1327), the settings-name parser accepts it (hooks/src/loader.rs:513, :651), and it fires (apps/cli/src/mode.rs:1760). The REMAINDER, four distinct gaps: (a) **`register_repo_root` does not exist** — repo-wide grep for `register_repo_root|registerRepoRoot` yields ONLY doc comments (platform-api/src/orchestrator.rs:1146,1149; hooks/src/events.rs:85,507; hooks/src/hook_payload.rs:759); the stream-json control dispatch at apps/cli/src/run.rs:470-910 handles 14 subtypes (`initialize`,`interrupt`,`set_model`,…,`set_cwd`,`set_permission_mode`,`end_session`) and has no arm, so the request falls through to `Unsupported control request subtype` (apps/cli/src/run.rs:950). (b) **The matcher query is documented but NOT wired** — `HookRegistry::match_query_for` (hooks/src/registry.rs:563-610) enumerates every event that yields a query and has NO `DirectoryAdded` arm, so it hits `_ => None` at :607-609; with `None`, hooks/src/registry.rs:361-366 takes `_ => true`, i.e. EVERY declared matcher passes. (c) **Hook results are thrown away** — orchestrator/src/conversation.rs:6591-6603 is `let _ = self.hooks.execute(HookEvent::DirectoryAdded{..}, ctx).await;`. (d) **Wrong `source` token** — apps/cli/src/mode.rs:1760 passes `"add_dir"`, and hooks/src/hook_payload.rs:759 asserts as fact that the oracle's values are `add_dir, register_repo_root`. The 2.1.220 binary says otherwise: the `/add-dir` site @240940662 calls `a$t(a,"slash_command")`, and the hook-docs metadata @222640688 declares `matcherMetadata:{fieldToMatch:"source",values:["slash_command","register_repo_root"]}`. Oracle behaviour for (a) and (c) read from @246447308: validate (`register_repo_root: target is not a directory` / `… is outside the allowed registration scope` / `… is already a registered working directory`) → `addDirectories` into `toolPermissionContext` at `destination:"session"` → `Mo.refreshConfig()` → `a$t(dir,"register_repo_root")` (debug-log only) → honour `reload_claude_md` / `reload_skills` / `reload_plugins` (the last also reconnects the MCP clients the reload dropped); and @240940662 for `/add-dir`: per-failure `DirectoryAdded hook failed: <output>` at error level, each `systemMessage` rendered as `DirectoryAdded hook: <msg>`, a trailing `N DirectoryAdded hook(s) failed; output is in the debug log, not shown here`, all pushed to the conversation as a `task-notification` meta message, with a `.catch` variant `DirectoryAdded hook execution failed: <err>`.

</details>

**阻塞：** The `register_repo_root` handler needs a sandbox-config-refresh seam (the oracle's `Mo.refreshConfig()` ordering is load-bearing — hooks must observe the new directory) and a plugin-reload path that reconnects the MCP clients a reload drops; scope-check that against the existing `set_cwd` handler in apps/cli/src/run.rs:632 first. The matcher-query wiring, the `add_dir`→`slash_command` rename and the hook-result handling are unblocked and independent.

**涉及文件：** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/apps/cli/src/run.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/hooks/src/registry.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/orchestrator/src/conversation.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/apps/cli/src/mode.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/hooks/src/hook_payload.rs`, `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/backlog/lingxi-code/platform-api/src/orchestrator.rs`

---

## E. 二次复核补出的遗漏

以下 5 项不在原 22 行中。前 4 项由 Anthropic 官方 2.1.218 release note 明确描述，并在 2.1.220 本机 oracle/当前 port 行为点上复核；第 5 项由 port 自己的实现注释和调用链直接确认。

### `O1-code-review-background`

**`/code-review` 仍在主线程展开 inline prompt，没有按 Claude Code 2.1.218 改成 background subagent**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | M | CLOSED |

Claude Code 2.1.218 把 `/code-review` 改成后台 subagent，目标是避免 review 工作填满主对话，并保留 stacked slash commands 作为 review target。LingXi 的 `commands/core/src/bundled/mod.rs` 仍把它注册成 `SlashCommandKind::Bundled { prompt_fn: CodeReviewPromptFn }`；`commands/core/src/bundled/code_review_skill.rs` 还明确标注这是 `inline plain-text path`。调用后生成的整段 review prompt 继续进入主 conversation，没有 background/fork metadata、subagent launch 或隔离 transcript。

这不是提示词字节差异，而是 conversation ownership、compact 压力、取消和恢复语义的可观察差异。

**涉及文件：** `commands/core/src/bundled/mod.rs`, `commands/core/src/bundled/code_review_skill.rs`, `agent/src/runner.rs`, `orchestrator/src/conversation.rs`

### `O2-context-post-compact`

**`/context` 使用 session 累计 token 计数，compact 后仍显示 compact 前的量级**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | S | CLOSED |

Claude Code 2.1.218 明确修复了“从 message picker compact 后 `/context` 仍报告 pre-compact token usage”。LingXi 的 `OrchestratorHandle::context_window_usage()` 读取 `SessionState::usage` 的累计 input + output tokens；该值记录历次请求用量，不是当前 compact 后 history 的 live token estimate。`CompactionCompleted` 只替换 history/写入 boundary 并更新 TUI spinner，没有重算或重置这份累计 usage。`/context` 每次虽然重新调用 handle，但拿到的仍是错误指标，因此不是 UI cache 问题。

验收必须区分两种量：cost/telemetry 的累计 token 不能重置；`/context` 应使用当前送模上下文的估算/准确计数。

**涉及文件：** `orchestrator/src/handle_impl.rs`, `orchestrator/src/conversation.rs`, `commands/core/src/context.rs`, `tui/src/chat_widget.rs`

### `O3-ctrl-j-paste-newline`

**终端把粘贴换行编码成 Ctrl+J 时，composer 没有归一化为 newline**

| 级别 | 量级 | 状态 |
|---|---|---|
| ⚪ Low | S | CLOSED |

Claude Code 2.1.218 修复了某些终端中 multi-line paste 的换行变成 `j`/被折叠的问题。LingXi 的 paste-burst 层只把“无 Ctrl/Alt modifier 的 `KeyCode::Enter`”认作粘贴换行；`Ctrl+J` 进入 modified-key 分支，先 flush/close burst，后续 composer match 也没有 `Char('j') + CONTROL` 的 newline 分支。全 TUI 没有更早的 key normalization 层。

该项应在 paste classification 边界归一化，并用真实 `KeyEvent(Char('j'), CONTROL)` 回归测试锁定；不能把所有交互式 Ctrl+J 无条件改写而破坏 modal/快捷键语义。

**涉及文件：** `tui/src/app.rs`, `tui/src/chat_widget.rs`, `tui/src/bottom_pane/mod.rs`, `tui/src/bottom_pane/paste_burst.rs`

### `O4-panel-focus-cursor`

**plugin/settings 列表只改变高亮，不把 terminal cursor 移到 focused row**

| 级别 | 量级 | 状态 |
|---|---|---|
| ⚪ Low | M | CLOSED |

Claude Code 2.1.218 的 accessibility 修复要求 plugin/settings panel 在方向键导航时把 terminal cursor 移到焦点行，使 screen reader 和 magnifier 能跟随。LingXi 的 `PluginsView`/settings `ScreenView` 保存并渲染 `selected`，但没有实现 `Renderable::cursor_pos`；`BottomPane::cursor_pos` 对 list modal 会退回 composer cursor。视觉高亮看似正常，辅助技术收到的实际焦点却仍在输入框。

这与 `N-changelog-5` 的 typed/deleted-text announcements 是不同的 accessibility contract，不能只靠启用 `--ax-screen-reader` 关闭。

**涉及文件：** `tui/src/bottom_pane/plugins_view.rs`, `tui/src/bottom_pane/screen_view.rs`, `tui/src/bottom_pane/view.rs`, `tui/src/bottom_pane/mod.rs`

### `O5-microcompact-idle-gap`

**microcompact 清理逻辑存在，但没有真实 last-assistant timestamp，无法执行 Claude Code 的 idle-gap gate**

| 级别 | 量级 | 状态 |
|---|---|---|
| 🟡 Medium | M | CLOSED |

`compaction/src/microcompact.rs` 已实现 compactable tool 选择、20k 最小节省阈值和 keep-recent 保护，也暴露了可接收 out-of-band timestamp 的 trigger helper；但生产调用点 `compaction/src/orchestrator.rs` 明确承认 `ConversationMessage` 没有 per-message timestamp。启用 microcompact 后，orchestrator 直接按 count gate 执行，而不是先判断 `now - lastAssistant.timestamp >= gapThresholdMinutes`。

默认 `enabled = false` 降低了默认路径风险，但一旦配置启用，清理时机就与 Claude Code 不同，所以应记为 PARTIAL，而不能继续当作 compact parity 已闭合。

**涉及文件：** `protocol/src`, `compaction/src/microcompact.rs`, `compaction/src/orchestrator.rs`

---

## 出处

- 条目来源：`docs/claude-code-2.1.220-sessionB-wave-2026-07-26.md` §3（猎取 backlog）、§6（deferred 碎片）、§8（ultra-review 相邻 follow-up）、§4（H6 部分裁定）。
- 原始状态核查：2026-07-27，13 个只读 agent 在 `370946160` 的独立 checkout 上并行执行；其行为点证据继续保留，但“22 个独立 active gap”的汇总结论已被本次复核修正。
- 二次复核 oracle：本机 Claude Code `2.1.220` binary（SHA-256 见文首）和 Anthropic 官方 [Claude Code release feed](https://github.com/anthropics/claude-code/blob/main/feed.xml)。2.1.220 自身的公开说明只有 “Bug fixes and reliability improvements”，所以具体行为以仍适用于 2.1.220 的 [v2.1.218 release entry](https://github.com/anthropics/claude-code/releases/tag/v2.1.218) 加本机 binary/行为探针为准。
- port 侧核查：读取 Git object `370946160`，避免当前 checkout 分支差异污染结论；新增遗漏都落到了实际 consumer/dispatch/state 行为点，不以单次负向 grep 作为唯一证据。
- **不在本册**：已接受的有意分歧（多 provider、WebSearch/Tavily、crossreview、mobile、冻结的 team/swarm、`+500k` 预算、`.lingxi`/`LINGXI_` 命名、`--help` 排版、remote-session client）。这些是 USER-CONFIRMED 的分歧，不是 gap，不要「修」。
