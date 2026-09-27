# plan mode + goal 对齐 2.1.266 — 交接文档（2026-09-09）

写给下一个接手 **plan 模式 / goal 子系统** 的人。回答四件事：这次审核了什么、
落地了什么、还剩什么、以及接手前必须先知道哪些坑。

细节不重复 `plan-goal-byte-alignment-2.1.266-2026-09-08.md`（本次的字节对齐报告，
含全部 oracle 偏移量与证据），本文只做索引、状态和待办。

**当前状态**：三个阶段全部实现并测试通过，已合入 `main`（`3ad34710e`）。
3989 个测试通过；余下 5 条红测试全部与本次改动无关（见下）。

---

## 一、这次审核了什么

**范围**：plan 模式（EnterPlanMode / ExitPlanMode / plan 文件 / plan-mode
reminder / 权限门）+ goal 子系统（`/goal`、Stop 评估器、check-in、goal_status
附件、ProposeGoal），对齐目标 **Claude Code 2.1.266**（当日 npm latest）。

**oracle 出处**（复核用）：
- 可执行文件 `~/.local/share/claude/versions/2.1.266`，
  sha256 `553d1b9e9e7068b275c0a783c7e139ff6503096f286e674c8c919379fb0eca62`
- 抽取出的生产 chunk：`~/.claude/oracle-chunks/2.1.266/`（1659 个，36MB），
  由 `~/.claude/oracle-chunks/extract.py 2.1.266` 生成
- ⛔ 全程读的是可执行文件里的**明文 JS 源码**，不是 `claude-code/src` 那份陈旧
  TypeScript 镜像，也不是 `strings` 猜的

**方法与证据脚本**（都在 `~/.claude/oracle-chunks/notes/plan-goal-2.1.266/`）：
- `diff_literals.py` —— 从 15 个 oracle 区段抽 350 条 prose 字面量，逐条比对整棵
  LingXi 树；归一化能穿透 Rust 的 `\"`、`\n`、`\u{…}` 和续行。开工时 114/350 命中，
  收工 138/350（其余是两条 divergence 与 oracle 内部日志）
- `resolve_enter_prompt.py` —— 从可执行文件生成 EnterPlanMode 的 3997 字符 prompt
- `gen_plan_slug.py` —— 从可执行文件生成三张 slug 词表（219/109/409）
- ⛔ **这三样产出物都不要手改**，改了就和 oracle 脱钩了；要改先改脚本再重生成

## 二、已落地（`3ad34710e`）

头号发现是 **plan 模式当时是"半接线"的**，三处各差一环，合起来让整条工作流走不通：
权限层没有 plan 文件豁免（模型被要求写的那个文件每次写都弹框）、ExitPlanMode 从不
读 plan 文件（而已移植的 prompt 恰恰告诉模型不要传参，于是 `## Approved Plan:`
是死代码）、EnterPlanMode 的 prompt 是一行自造 stub。

其余落地项（P0-1/2/3、`/goal` 状态行、idle check-in 上限层、`goal_status` 五形态、
三条 plan 模式边界 reminder、plan slug、`tengu_goal_*` 遥测、子代理门）见对齐报告的
「Implementation status」表。

🚨 **一条结构性收敛，别再拆开**：plan 文件路径原本有三处各自推导（reminder / 权限
豁免 / ExitPlanMode）。现在 `PlanFileMatcher` 落在 `platform-api`，三处统一走
`OrchestratorConfig.plan_files`，并由测试
`the_plan_path_comes_from_the_shared_identity` 钉住。**一条路径三处推导，正是权限
豁免会悄悄不再覆盖那个文件的原因**，参见 [[two-derivations-of-one-root]]。

---

## 三、🚨 已知缺陷（我引入的，优先修）

**`tengu_goal_cleared` 在自动清除路径上 reason 标错。**

`clear_goal_after_unrecoverable_error`（`orchestrator/src/turn_loop.rs:2147`）走
`clear_active_goal_state_and_hook()` → `finish_active_goal_state_and_hook(Cleared)`
→ 新加的 `fire_goal_terminal_event` **恒发 `reason:"user_clear"`**。而 oracle 的
`Ntr` 发的是 `eB(e, o==="context_limit" ? "context_limit" : "api_error")`。

即：上下文超限 / API 错误导致的 goal 拆除，会被记成"用户手动清除"。只影响分析字段，
不影响行为，但会污染这条指标。修法：给 `finish_active_goal_state_and_hook` 加一个
显式 reason 入参（`GoalClearReason` 已经在 turn_loop 里了），别在 Cleared 分支里
硬编码字符串。

---

## 四、未完成任务

### P1 —— goal 遥测还差三块

| 待办 | oracle 依据 | 落点 |
|---|---|---|
| `tengu_goal_evaluated` 完全没有（`git grep tengu_goal_evaluated` → 0） | 评估器 `finally` @4217700，`{outcome,durationMs,iterations,parentAborted,origin}`，outcome ∈ met/not_met/impossible/deferred/error/cancelled/absent | 需要把「评估起始时间戳」穿到 `goal_stop_hook_disposition` 路径；这是当初没做的唯一原因 |
| supersede 不发 `tengu_goal_cleared` | `rRe` 在覆盖旧 goal 时 `eB(d,"superseded")` | `orchestrator/src/handle_impl.rs` 的 set 路径直接覆盖 `s.active_goal`，没有任何发射 |
| 上面第三节那条 reason 误标 | `Ntr` | 见第三节 |

`origin` 字段目前恒为 `"user"` —— 这是**对的**，`queuedGoalOrigin` 只会被
ProposeGoal 写，而 ProposeGoal 是已确认的 divergence（见第五节）。

### P2 —— 有明确 oracle 依据、但缺宿主承接面

- **`/goal` 的交互面板（TUI）**：`/goal clear to stop early`、
  `/goal <condition> to set another`、`/goal <condition> to set one`、以及
  "Goal achieved" 成功面板。`tui/` 下 0 命中。面板还需要 `fRn` ——
  回读 transcript 里最后一条 `met && !sentinel` 的 `goal_status`
  （新形状已经支持这个查询，见 `platform-api/src/orchestrator.rs`）。
- **check-in 的 summary 通道**：`Goal check-in: background work still running` /
  `… no longer running` 两条 summary 和 `IDLE_PAUSED_SUMMARY_SUFFIX` 已按字节定义，
  但 **LingXi 的空闲路径只投 body**，summary 无处可去。要接就得给空闲投递加一个
  通知/摘要渠道。
- **空闲 tick 的跳过条件**：oracle `Ons` 里 `if(J_e()||y(rnr))` 和
  `if(D.length===0&&y(Ins))` 会「重装 60s 但不投递」。LingXi 的空闲循环没有
  message-queue peek，这两条跳过没有落点。
- **`SSt` 的两个门**：teammate 后缀目前**无条件**发。upstream 是
  `hasTaskTool && zx()==="default"`。LingXi 里 Agent 工具恒可用、implicit teammate
  恒在，所以前半永真；后半（output style）在工具层拿不到 —— 要修就得把 output
  style 穿进 `BuiltinToolContext`。
- **`d8()`**：Explore/Plan 子代理门只模拟了 `CLAUDE_CODE_DISABLE_EXPLORE_PLAN_AGENTS`
  这个逃生口，宿主特性对象本身没建模。
- **`ExitPlanMode.checkPermissions` 没有核对**：upstream 是
  `{behavior:"ask", message:"Exit plan mode?"}`；LingXi 返回 Allow，把审批交给
  `check_exit_plan_mode` 这个专用 seam。**这个 seam 是否真的会弹 ask，我没有验证** ——
  这是 Phase 3 列了但没做的那一项，也是最容易「看起来对、其实静默放行」的地方。

### P3 —— plan 文件子系统的其余部分

`platform-api/src/plan_files.rs` + `plan_slug.rs` 只覆盖了「本会话这一个 plan 文件」。
oracle 的 `src_160477403.js` 还有一大片没移植：

- `.workshop.md` 兄弟文件（`PlanFileIdentity::workshop_enabled` 已留好开关，恒 false）
- `copyPlanForResume` / `copyPlanForFork`（0 命中）、`saveRejectedUltraplan`
- slug 的 **seed 播种**：`getPlanSlug(sessionId, seed)` 的 seed 来自 transcript
  （`planSlugSeed`），LingXi 没有 seed 源，现在只走无 seed 的三词形态。
  `slugify_seed` 已按 `ynt` 实现好，接上 seed 源即可
- **碰撞检测的形态**：oracle 用 primed listing 缓存；LingXi 现在每次候选直接
  `exists()` 两个文件。行为等价、性能不同，目录很大时值得换
- `normalizeToolInput` 的 `ogl` 扩展 schema（把 `plan`/`planFilePath` 注入 input）
  没建模 —— LingXi 改成在 `call` 里自己读文件，效果等价，但发布出去的 input schema
  少了那两个 describe

---

## 五、⛔ 不要"修"的东西

已由用户在 2026-09-08 确认，写进了 [[lingxi-accepted-divergences]]：

- **ProposeGoal 不移植**。双重休眠：`Gft(){return H("tengu_propose_goal",!1)}`
  默认 false，且 `isEnabled()` 在 `Re()||Mn()`（非交互 / 远程）直接 false，而 LingXi
  引擎正是非交互形态 —— **两头都不通，零 wire bytes**。`modelProposedGoals` 设置
  保持「收但不读」是配套的，不是遗漏。
- **ultraplan 不移植**（云端 plan 模式，需要 Claude Code on the web 的会话面）。
- **workshop / prototype offer 段落不是 gap**：oracle 也只在对应 skill 存在时才发，
  LingXi 两个 skill 都没有，所以「不发」才是一致的。
- `[PLAN MODE]` / `[EXIT PLAN MODE]` 两个自造标记**已删**，别再加回来 —— 任何
  claude-code 版本都没有它们。

---

## 六、环境与坑（接手前必读）

### 🚨 `main` 目前编不过，且不是本次改动造成的

`agent/src/handle.rs` 引用 `crate::permission_mode::SpawnBypassGates`，但该类型在
HEAD 的 `agent/src/permission_mode.rs` 里**不存在** —— 它只活在另一个会话**未提交**
的工作副本里（工作树 18 处、HEAD 0 处）。并发会话把调用方提交了、定义没提交。

后果：干净 checkout 上 `cargo build -p agent` 直接失败，**任何人都拿不到 HEAD 基线**。
判据：`git grep -c <符号> HEAD -- <定义文件>` 对照 `grep -c` 工作树文件。
本次全部验证都是在**工作树**（含他们的定义）上做的。

### 🚨 build lock 会被别的会话占住几小时 —— 用私有 target 绕开

```
CARGO_TARGET_DIR=/private/tmp/<你的目录> cargo test -p …
```
共享的 `target/debug/.cargo-lock` 被谁占着，`lsof lingxi-code/target/debug/.cargo-lock`
**会直接点名**（按 shell-snapshot 文件名区分是哪个会话）。⛔ 别 kill 别人的 cargo。
私有 target 冷编约 7.9GB。本次留下的
`/private/tmp/lingxi-plangoal-target` 若不再用可以直接删。

### 磁盘

`lingxi-code/target` 约 187GB。本次经用户批准删掉了
`/tmp/lingxi-teammate-target`（74GB）。磁盘满会**伪装成编译错误**：本次实际见到
`rustc interrupted by SIGSEGV` 和 `failed to build archive … (os error 28)` 两种面孔，
见 [[enospc-masquerades-as-compile-error]]。报错形状对不上 diff 就先 `df -h`。

### 共享 checkout：提交必须按 hunk 挑，不能按文件

这棵树同时有好几个会话在写。本次提交时踩到并已处理的两类污染：
1. **我自己跑 rustfmt 把别人未格式化的行卷了进来** —— 25 个纯重排 hunk。判据是
   结构性的：把空白和 rustfmt 的尾逗号归一化后，删除侧和新增侧完全相同。
2. **别人的 `SpawnBypassGates` 改动正好落在我改的那两个宿主文件里**。

⚠️ 用 `-U3` 分 hunk 会把「我的改动」和「别人的改动」合并进同一个 hunk（本次
`engine-mobile/host.rs` 就把 `.with_plan_files` 和别人的行并到了一起，第一次过滤
直接把我的改动丢了）。**要用 `-U0` + `git apply --cached --unidiff-zero`**。
过滤脚本留在
`/private/tmp/claude-501/…/scratchpad/stage_mine.py`（scratchpad 会蒸发，要用就先拷走）。

还有：`Cargo.lock` 第一次提交时混进了别人的两行、且漏了我自己的一行。
**提交后一定要 `git show HEAD -- Cargo.lock` 逐行看**。

### 遗留红测试（都不是本次造成的，别浪费时间怀疑自己）

| 测试 | 原因 |
|---|---|
| `every_tool_file_declares_a_permission_result` | 读已删的 `tools/team/src/team.rs`（commit `bd0abff79` 删的），HEAD 上就没有 |
| `every_tool_with_output_calls_truncate_or_opts_out` | 同上，同一个文件清单 |
| `denied_fqn_tool_use_yields_permission_denied_result_and_skips_server` | `tool not found: mcp__mock__a`，注册表未命中，发生在权限判定**之前** |
| `production_prompt_bodies_match_normalized_2_1_238_manifests` | system prompt 体长漂移 9 字节 |
| `production_output_style_bodies_match_normalized_2_1_238_manifests` | 同一处 9 字节漂移 |

前两条的修法是更新那份硬编码的工具文件清单。

### 🚨 一个会反复上当的判据陷阱

**拿 oracle 的标识符去 grep 端口，零命中什么都不证明。** 本次实例：我据
`git grep -i idlecheckin` → 0 断言「LingXi 没有空闲 check-in」，写进了计划；实际上
它一直存在，只是叫 `sync_goal_checkin_idle_task` / `run_goal_checkin_idle_loop`
（`checkin_idle`，不是 `idle_checkin`）。**要 grep 的是被移植的行为，不是 oracle 的
符号名**。同 [[auth-copy-was-unreachable-2026-08-01]]。

---

## 七、复工第一步

1. 先确认 `main` 是否还编不过（第六节第一条的判据）。编不过就先找那位同事补提交
   `SpawnBypassGates` 的定义，否则你连基线都取不到。
2. 修第三节那条 `tengu_goal_cleared` reason 误标 —— 最小、最明确、有 oracle 依据。
3. 补 P1 的另外两条遥测（`tengu_goal_evaluated` + supersede）。
4. 动 P2 之前先回答一个问题：**`check_exit_plan_mode` 这个 seam 真的会弹 ask 吗**。
   如果不会，那是个静默放行，优先级要提到 P1。
