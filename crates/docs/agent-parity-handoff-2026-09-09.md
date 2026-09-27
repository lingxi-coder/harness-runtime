# agent 子系统 parity 交接 — 2026-09-09

这份文档写给**接着做 agent 子系统对齐的下一个人**。它回答四件事：这轮做了什么、
怎么做的（可复现）、还剩什么（以及为什么没做）、以及接手前哪些坑必须先知道。

审核结论的完整版在
[`agent-byte-alignment-2.1.266-2026-09-08.md`](./agent-byte-alignment-2.1.266-2026-09-08.md)
（16 条发现，逐条含 oracle 偏移量与端口 file:line）。本文不重复那些细节，只做
索引、状态和「下一步具体怎么动手」。

---

## 1. 现状

对齐基线从 **2.1.238/2.1.245 提到 2.1.266**（本机 `~/.local/share/claude/versions/2.1.266`，
`VERSION:"2.1.266"`，`GIT_SHA:eb01d60909645dfca0bf35a844b946aabaa3a75e`）。

**16 条确认发现，15 条已修**，全部在 `main` 上，8 个提交：

| 提交 | 内容 |
|---|---|
| `cc4e31307` | AG-01…AG-07：`model` 参数描述、coordinator 两臂、`SUBAGENT_MODEL_FORCE`、`workflow-subagent` 泄漏进模型目录、turn 用尽的 harness NOTE、roster 两个注册门、web-fetch 不自动后台化 |
| `c54583992` | AG-08/AG-09：`whenToUseLean`、Explore inherit-cap kill-switch、后台判定抽成纯函数 |
| `421bda9ea` | AG-17：projectSettings 从 cwd **向上走**到项目根 |
| `e47403bc8` | AG-21：agent 目录**递归**扫描（跟随软链 + 防环 + 1MiB 上限） |
| `0e700080c` | AG-18：`--add-dir` 根的 agent 目录 |
| `71044d602` | AG-19/AG-20：managed policy 层、inode 跨层去重 |
| `dde0333d2` | 记录 stop-pending 这条 blocker 为什么成立（仅文档） |
| `599f51c88` | AG-22：`outputSchema`；并把 `cacheTtl` 从「待办」改判为「明确不做」 |

### ⚠️ 有两个 hunk 不在上面 8 个提交里

`whenToUseLean` 那一轮，我把过滤后的 patch 暂存进了**共享 index**，随后并发会话的
`git commit` 把它们一起提交走了 —— 落在 **`5bca0ae94`**（提交信息与内容无关）：

- `orchestrator/src/conversation/reminders.rs` 的 `format_agent_line(entry, lean)` 调用点
- `tools/agent/src/agent_test_support.rs` 的三个 `when_to_use_lean: None` fixture 字段

顺着 AG-08 找代码的人，在我的提交里找不到这两处，去 `5bca0ae94` 找。
（当时 `platform-api` 那一半还没提交，**main 一度编不过**，`c54583992` 补上的。）

### 测试基线

私有 `CARGO_TARGET_DIR` 下（见 §5 坑 2）：

| suite | 结果 |
|---|---|
| `agent --lib` | **425 passed**（本轮开始时 405） |
| `tool-agent --lib` | **166 passed** |
| `platform-api --lib` | **319 passed** |
| `cargo check -p engine-desktop` | clean |

⛔ 当时有 3 个红**不是本轮引入的**，是并发会话未提交的在制品：
`permission_mode::tests::confined_arm_shadows_the_bypass_arm`、
`…::confined_run_refuses_a_definition_declared_escalation`（`agent/src/permission_mode.rs`，
+302 行未提交），以及 `plan_slug::tests::slugify_seed_follows_ynt`（`platform-api/src/plan_slug.rs`，
**untracked**）。接手时先重跑一遍确认它们的归属，别默认还是别人的。

---

## 2. 审核方法（可复现）

### 2.1 oracle 侧

```sh
python3 ~/.claude/oracle-chunks/extract.py 2.1.266     # 拆成 1659 个 chunk
python3 ~/.claude/oracle-chunks/find.py 2.1.266 "<needle>"   # 定位 chunk + offset
python3 ~/.claude/oracle-chunks/ctx.py  2.1.266 "<needle>" 800 800 5   # 读上下文
```

agent 表面集中在两个 chunk：

| 区域 | 位置 |
|---|---|
| 常量（`mt`/`Zar`/`elr`/`tlr`/`SW`/`dy`/`IKt`/`PKt`/`pRe`/`OKt`） | `src_160528463.js` |
| 内置 agent 定义 + catalog 加载器（`cre`/`Z$`/`qto`/`wQr`/`O5`/`vG`） | `src_162329786.js` @1489000–1536500 |
| prompt 生成器（`H2n`/`U2n`/`H4o`） | 同上 @3554000–3572000 |
| 工具对象与 `call` | 同上 @3572000–3607000 |
| 结果 finalize（`bft`） | 同上 @3530400–3533400 |
| `agent_listing_delta` 生产/渲染 | 同上 `B3t` @5174317 / 渲染 @5459577 |

### 2.2 🚨 跨版本 delta：别做全量字符串 diff

朴素扫描器在 5MB minified 文件里遇到嵌套模板字面量会错位一次，之后整个文件的引号
配对全错。实测两版各抽 ~51000 条，归一化占位符后**仍有 78% 被误报为「新增」**。
判据：拿一条**确定存在**的串去 grep 两边的抽取结果，都 0 命中 ⇒ 抽取坏了。

可靠流程（脚本都在 `~/.claude/oracle-chunks/`）：

1. `strings.py <ver> <chunk> <start> <end>` —— 只对**已知边界的区间**抽字面量，
   从安全起点开始扫，错位不会累积。
2. `inprev.py <strings-file> <旧版本>` —— 判断「是否新增」用**原始子串搜索**，
   把每条字面量里最长的无占位符片段直接在旧版全部 chunk 的**原始字节**里找。
   解析器只用一次（新版、小区间），旧版那边完全不解析。
3. `checkport.py <strings-file> <rsfiles.txt> <root>` —— 对端口同样只做子串匹配，
   且**必须同时试 Rust 转义写法**（`"` → `\"`，`—` → `\u{2014}`），否则端口里
   明明有的串会假报 MISS。实测第一版没做转义处理，84 条 MISS 里有 40 条是假的。

旧版本二进制用 `npm pack @anthropic-ai/claude-code-darwin-arm64@<ver>` 取回。

### 2.3 🚨 字面量扫描看不见的那一类

本轮**最大的两条发现都不是「字符串对不上」**，而是「整条 source 压根没人填」：

- **AG-21**：`vG(dir)` 是 `rg --files --hidden --follow --no-ignore --glob "*.md"`
  （**递归**），端口是单次 `read_dir` ⇒ `<DOT_DIR>/agents/reviewers/api.md` 完全不可见。
- **AG-17**：`O5` 从 cwd **向上走**到项目根，端口只取 cwd 一层 ⇒ monorepo 里从仓库根
  跑看不见 package 的 agent，从 package 里跑看不见根的。

我第一版报告写了「source precedence 已对齐」，依据是端口的 tier map 和 `Z$` 一致 ——
**tier 顺序确实对，但喂进这些 tier 的 source 有四条没人填**（AG-17/18/19/21）。
读 merge 函数看不出来，扫字面量更看不出来。

> **接手时照做**：审「优先级 / 合并 / 目录发现」这类逻辑，**必须同时读产出它输入的
> 那个 loader**，不能只读消费者。

---

## 3. 还剩什么

4 条未做。每条都附了**确切阻塞**和**下一步**。

### 3.1 AG-13 — stop-pending 生成门（**2026-09-10 已落地 `aa49fb523`**）

oracle 2.1.267 的谓词是 `a0`（`src_163219561.js` @699180，.266 里叫 `jH`）：
`function a0(e){return ek().stopPendingAgentIds.has(e)}`。

**读取点是 7 个不是 3 个**（原条目按 .266 记的）：

| 文案尾巴 | 端口位置 |
|---|---|
| `launch new agents.` | ✅ `tools/agent/src/agent.rs`（紧跟 depth cap，与上游同序） |
| `launch skills.` | ✅ `tools/skill/src/skill.rs` |
| `launch workflows or act on existing runs.` | ✅ `tools/workflow/src/lib.rs` |
| `send messages.` | ✅ `tools/ui/src/send_message.rs` |
| `start monitors.` | ✅ `tools/task/src/monitor.rs` |
| `resume other agents.`（调用方一侧） | ✅ 端口的 resume 走 SendMessage，已由上一行覆盖 |
| `Shell exec refused: agent ${id} has a kill pending loop settlement` | ✅ `tools/shell/src/bash.rs`（`f888a1761`） |

#### 🚨 原来的阻塞判断是错的，我中途的替代判断也是错的

原条目：`kill()` 里 status sink 同步把行翻成 `Killed` ⇒ 集合会「填了又清」。
**这条把「任务行终态」当成了「loop 已 settle」**。上游清空走的是
`E8e(taskId, cb)` —— **loop 的 settle 回调**（`AWt` 返回的那个函数），跟行状态无关。

我随后以为 `runtime.cancel` 是 tokio `abort()` 硬杀 ⇒ 门永不触发。**也错**：
`abort()` 杀掉的只是 `local_agent.rs` 里那个 **drainer future**；真正的 runner 由
`pool.allocate_with_startup` 起在自己的 task 里。drainer 被 drop ⇒
`SpawnDeallocGuard::drop` 起一个清理 task，先发**协作式** `UserInterrupt`，
再以 50ms 轮询等最多 `SPAWN_CANCEL_GRACE = 2s`，然后才 `deallocate`。

而 runner 只在**模型往返**处 `select!` 上观察 UserExit/UserInterrupt；
工具分发循环里 `event_rx` / `UserExit` / `select!` **零出现**（实测计数）。
⇒ kill 落在一次 assistant turn 的多个 `tool_use` 块中间时，剩下的块照常执行，
其中的 Agent / Skill / Workflow / SendMessage / Monitor 调用会起出**比它活得久**的活儿。
**门是可达的。**

#### 端口的填 / 清点（不搬定时器）

`platform_api::agent_processes`（就是上游 `PDt` 里放 `liveProcessesByAgentId` 的那个
registry）新增 refcount 的 stop-pending 集合，RAII guard：

* 填：`SpawnDeallocGuard::drop` 的清理 task（发 `UserInterrupt` 之前）、
  `PoolSubagentSpawner::stop`（发 `UserExit` 之前）。
* 清：guard 落地即 `deallocate` 之后 —— 端口的 settle 点。

⚠️ **必须 refcount**：杀一个 persistent agent 会**同时**开两个窗口（drainer drop 一个、
`stop()` 一个），且 settle 顺序不定。单 owner token 会让先结束的那个把门给另一个提前打开。

上游那对 10s 升级 / 30s 逾期定时器**故意没搬**：它们存在是因为上游没有别的东西强制 loop
settle；端口的「轮询 + `deallocate`」本身就给窗口封了顶，定时器在这儿无事可做，
可观测行为一致。

#### shell exec 的形状决定（`f888a1761`）

上游 `if(Fe!==void 0&&a0(Fe)) return t('Shell exec refused: …'), rSe();`
—— `rSe()` 返回的是 **它自己 abort 路径返回的同一个值**（`xDt`：`status:"killed"`、
`code:145`、`stderr:"Command aborted before execution"`、`interrupted:!0`），
那句话只进**日志**，不给模型看。

所以不变量不是那几个 wire 值，而是「**停止中的 exec，按 abort 的方式拒**」。
端口的 abort 值是 `ToolError::Aborted`（`turn_loop` 把它映射成 `"interrupted"`
的 `toolDenialKind`）。照抄 `code:145` 那个 body 会**凭空造一个别处都不存在的结果形状**，
而且把「拒绝执行」报成「跑过了并且失败了」—— 两边代码都不是这么用它的。
那句话按上游的位置留在 debug 日志里。

⚠️ 种雷验证里 **S2（把 `Aborted` 换成 `InvalidInput`）会红** —— 形状本身被钉住了，
不是只钉了「有没有拒」。

#### ⬜ 仍然没做：resume 的**目标**一侧

上游那个站点有**两个**检查，第二个不是同一族：

```js
if(a0(e)||!xh(e)&&Ps(d.get(e)?.status??"running"))
  throw new Pne(`Agent ${e} is still stopping — its previous run was stopped but has not exited. …`)
```

`e` 是**被 resume 的那个** agent。调用方那一半端口已经覆盖（resume 走 SendMessage，
门已经在），但目标这一半在工具层**没有落点**：端口没有上游那套 resume 状态机
（`resuming` 标志、`already running or being resumed`），resume 发生在更深的
handler / streaming spawner 里。要做得先找到那个分支，别硬塞进 SendMessage ——
它同时也给**运行中**的 agent 投递消息，一刀切会拒掉正常投递。

### 3.2 AG-14 — harness-note 分层（**2026-09-10 复查：不是一个整层的移植任务**）

原条目写着「端口整层不存在（`harness_note` / `harness_tail` / `harness_head` 零命中）」，
并把它记成一个 P2 的整层移植。对 2.1.267 逐个调用点复查后，这个前提在**三处**是错的，
拆开之后真正开放的只剩一条，而且它被另一个子系统挡着。

#### 生产者 `iht`（原 `bft`，`src_163219561.js` @3542572）

```js
let {live:ue, notice:me} = ICe(e);            // ← notice = model_refusal_fallback
let {content:Xe, findings:Qe} = V4n(Ce);      // ← 输出护栏
let Je=[], ut=t5n(e);                          // ut = max_turns_reached
if(ut) Je.push({type:"text", text:`${Exe}${ut}-turn limit before finishing. …`});
if(me) Je.push({type:"text", text:`⚠ ${me.content}\n`});
if(Pe!==void 0) Je.push({type:"text", text:Pe});   // Pe = handback
let {report:dt, harnessTail:en} = r.persistedToolResultFiles ? res(…) : {report:Xe, harnessTail:[]};
let qt = Qe.some(Mn=>Mn.reportable) && dt.length>0 && dt[0]===Xe[0],
    yn = Je.length + (qt?1:0), bn = [...Je, ...dt, ...en];
return {…, harnessNoteCount:yn, harnessTailCount:en.length, harnessSectionHash:oj(bn), content:bn, …}
```

#### 逐条落点

| 片段 | 状态 |
|---|---|
| turn-limit note (`ut`) | ✅ 已移植（AG-05，`cc4e31307`） |
| `V4n` findings + `Y4n` | ✅ 已移植 —— `subagent_output_guard::sanitize_blocks` + `tengu_subagent_output_flagged`（`agent.rs:4577`） |
| `TaskOutput` 的 notes/body 切分 | ✅ 已移植 —— `harness_head`（`tasks/src/handle.rs:875`），原条目说的「零命中」已过期 |
| `⚠ ${me.content}` note | ⛔ 见下 |
| handback note (`Pe`) | 🔒 上游休眠：`vln`/`Eln`/`Cln` 全部记在 `lively_waffle` 上，`tengu_lively_waffle` 默认 **false** |
| `harnessTail` (`res`) | 🔒 门在 `r.persistedToolResultFiles`，端口**零命中**（无 tool-result 落盘） |
| `harnessNoteCount` / `TailCount` / `SectionHash` | 见下 |

#### 🚨 `Ofe()` 只挡住一个调用点，不是整层

`function Ofe(){let e=Rn.CLAUDE_CODE_HANDBACK_PROVENANCE; if(e!==void 0)return e;
return H("tengu_melodic_wolf",!1)}` —— 默认 **false**。

它只出现在 agent tool result 那一处（@3619534，`Ofe()?TGt(…):d`）。切分函数 `lue`
另有**两个无门调用点**：kill 通知（`qD` @2112497）和 `TaskOutput`（@3769938）。
⚠️ 「这个字段被 flag 挡着」不能从一个调用点推广到全部——按调用点数门。

端口今天只产出**一条** note（turn-limit），`harness_head` 用 `max_turns_reached`
重建它、并在 `status == Killed` 时返回 `None`；对单条 note 而言这与
`lue` + `filter(!startsWith(Exe))` 等价。**计数与 hash 只有在第二条 note 可达时才需要**，
而第二条的两个来源现在一个休眠、一个被挡住。

#### ✅ `⚠ notice` 与 `PZo` 撤回过滤（`db4fde31d`）

落在 **runner** 而不是 agent 工具的 finalizer —— 因为 `ICe(e)` 要的是**消息列表**，
而端口的 finalizer 只拿到抽好的文本块；runner 手里才有 `history`。

* **`PZo`**：把被后续 notice 撤回的消息在**挑答案之前**滤掉，否则一个被取代的 hop
  的输出可能直接成为 report。system 消息恒留（notice 本身就是撤回的载体）。
  比较取 uuid 前 **24** 个字符（上游 `D4n = 24`）。
* **`ICe`**：上游是「最后一条有正文的非错误 assistant → 读它的 `model` →
  匹配 `fallbackModel` 相等的帧」。端口的 `Assistant` **没有 model 字段**，
  但 runner 本来就知道当前 serving model，而**那就是**产出答案的那个模型
  （每次 hop 都会重跑这一轮）。所以是拿同一个值去匹配，不是近似，
  **也不需要给 wire DTO 加字段**。
* **顺序自然对上**：agent 工具的 finalizer 之后会把 turn-limit note 插到 index 0，
  所以在 runner 这边前插，最终就是上游的「turn-limit → ⚠ → report」。

判据：4 个测试 + 4 个种雷（不前插 note / 撤回过滤变直通 / notice 不看 serving model /
system 消息一起被滤）全部点名变红。含控制组：没 hop 过的 run 不带 note。

🚨 上一版的 `a_refusing_subagent_hops_to_the_fallback_and_retries` 断言
`result["text"] == "done"`，加了 note 之后**理应**变红 —— 改的是断言本身
（答案现在合法地带 note），不是绕开它。把它钉成「等于模型原话」就是把 bug 钉成契约。

#### ✅ 那个窄竞态已核实：不是缺陷

原记「`qD` 在 `!p.notified` 时用已存的 `p.result` 发 `finalMessage`+`usage`，端口
`SubagentResult::Killed { agent_id }` 什么都不带 ⇒ 可能丢输出」。**实测不丢。**

`local_agent.rs::kill()` 先读 `already_terminal = status_sink.is_terminal(task_id)`，
并把 `publish_terminal = !already_terminal` 传下去；`already_terminal` 时既不
`finish_persistent_terminal` 发布终态、也不 `set_status(Killed)`。所以「run 已完成并存下
结果、通知还没 drain、此时 kill 到达」这一格里，行仍是 `Completed`，
`outcome.result` 原样留着，通知照常带着答案 drain 出去。

与上游的差别只剩**状态标签**：上游把它记成 `killed`（但同样带 `finalMessage`），
端口记成 `completed`。两边都把模型该看到的字节送到了；而这一格里那个 run 确实是跑完的，
端口的标签反而更准。⛔ 别为了对齐标签去动这条 —— 会把「不许晚到的 kill 覆盖真实终态」
那条不变量拆掉。

### 3.3 AG-15 — `agent.spawn` 插件 function hook（P3）

2.1.266 新增 `_Bo`（@2955987）：spawn 过一遍插件 function hook，可以 deny、改写
agent 类型 / model / cwd / background，改写后再对权限规则复核。六条错误文案
（@3585420–3587034）端口全无。

**⛔ 阻塞（2026-09-10 复核：仍然零命中）**：`functionHooks` / `hooks-worker` /
`FunctionHook` 在端口全树 `--include='*.rs'` 下 **0 命中**。这不是 agent 子系统的缺口。oracle 把 function hook 跑在打包好的 JS worker 里
（`HOOKS_WORKER_URL: "/$bunfs/root/src/plugins/functionHooks/hooks-worker/hooks-worker.js"`），
端口**整个 function-hook runtime 不存在**（`functionHooks` / `hooks-worker` 零命中），
`HookEvent` 只有命令钩子那几个（`PreToolUse`/`PostToolUse`/`SubagentStart`/`SubagentStop`…）。

**下一步**：等 function-hook 子系统本身立项，那是独立的一次移植。

#### 2026-09-10 复核：把「阻塞」换成可执行的清单

三条独立证据确认阻塞成立（不是从旧记录继承的）：

1. `HookExecutor` 只有 **3 个** variant —— `Command` / `Http` / `Agent`
   （`hooks/src/definition.rs:157`）。**没有** function/JS 变体。
2. `rquickjs` 只在 **`workflow` 一个 crate** 的 Cargo.toml 里；`hooks` 拿不到 JS 引擎。
3. `plugin/src/manifest.rs:310` 的 `hooks: Vec<HookDefinition>` 是**命令钩子**，
   复用的就是上面那个只有 3 个变体的类型。

⚠️ 「端口有 rquickjs 所以离得不远」是**错的判断**，我自己先这么想过：
`workflow` 里的 JS 跑的是**用户自己写、且明确授权运行**的 workflow 脚本；
function hook 跑的是**插件提供、每次 spawn 自动执行**的代码。信任语境不同，
上游正因此把它隔离在打包好的 worker 里（`HOOKS_WORKER_URL`）。
⛔ 别为了省事把插件 JS 塞进 workflow 的引擎里跑。

**真要做，最小清单（按依赖顺序）：**

| # | 要做的 | 为什么不能跳 |
|---|---|---|
| 1 | **安全模型先定**：插件 JS 以什么权限跑、能不能碰 fs/net/env、超时与取消 | 这是唯一不能事后补的一条；其余都是接线 |
| 2 | 一个 `hooks` 能依赖的 JS 执行 seam（新 crate，或把引擎从 `workflow` 抽出来） | §8.1 分层：`hooks` 是 engine 层，不能依赖 tool 层的 `tools/workflow` |
| 3 | `HookExecutor::Function{...}` + plugin manifest 里声明 function hook 的 schema | 没有声明面就没人能注册 |
| 4 | `agent.spawn` 事件 + payload/response 契约（deny / 改写 type·model·cwd·background） | 上游 `_Bo` @2955987 |
| 5 | 改写后**重新**过一遍权限规则 | 上游明确有这一步；漏掉 = 插件可绕过权限 |
| 6 | 六条错误文案 @3585420–3587034 | 有了产出点才有意义 |

**判据**：第 1 条没有书面结论之前，2-6 都不要动。

#### 🚨 规模复核：`agent.spawn` 是 **17 个** function-hook 事件里的一个

`src_163219561.js @2127769` 的 `Akn` 表列全了 function-hook 的事件面：

```
tool.call        tool.describe     command.run     command.describe
prompt.submit    prompt.section    prompt.context
agent.offer      agent.spawn
skill.prompt     attribution.text
session.start    session.receive   session.compact
turn.start       turn.step         turn.complete
```

而且每个 site 都带一个 `core` 伪钩子（`{name:"core",isCore:!0,budgetMs:0,run}`），
每个钩子有 **`budgetMs` 预算**，`agent.spawn` 还有自己的结算规则
（`Vvn="agent.spawn: a hook answered with neither model nor deny"`，
`Yvn` 逐字段 diff 出「哪些字段被钩子改写了」并记日志）。

**结论：AG-15 不是「加一个钩子」，是移植一个带调度/预算/17 个接入点的子系统。**
把它当成 agent 子系统的一条待办去估工期一定会低估——我自己先按「一个钩子」估过一次。
真要立项，范围是 function-hook 子系统本身，`agent.spawn` 只是其中一个 site，
且它依赖前面那张表里的第 1 条（安全模型）和第 2 条（JS 执行 seam）。

⚠️ 别被 `HOOKS_WORKER_URL` / `functionHooks` 在 60 个 chunk 里的高命中数误导：
那是 Bun 打包的常量表在每个 chunk 里重复，不是 60 处逻辑。真正的逻辑集中在
`src_163219561.js` 和 `src_180269027.js`。

#### 🚨 第 1 条（安全模型）**无法照抄上游** —— 2026-09-10 实测

想「照抄上游的隔离模型」这条路已经走到头了，结论是走不通，记下来免得再走一遍：

- `HOOKS_WORKER_URL` 指向 `/$bunfs/root/src/plugins/functionHooks/hooks-worker/hooks-worker.js`，
  是一个**独立的内嵌资源**，不在我抽出来的 1657 个 chunk 里
  （chunk 是主 bundle；worker 另打一份）。
- 直接在 200MB 二进制里搜：`self.postMessage` 只有 **2** 处，**两处都是打包进去的
  node-forge 加密库**，不是 worker。
- 那条路径字符串出现在 @70371141，紧跟其后的是**字符串表**
  （`darwin`、`transport`、`timeout`… 带哈希的 interned 串），**不是资源正文**——
  worker 是编译成 bytecode 的，拿不到源码。

**能拿到的**：调度面。`budgetMs`(39)、`hookTimeout`(7)、`isCore`(20)、`agent.spawn`(55)
在二进制里都有，所以**预算/超时/core 伪钩子的契约**是可以还原的。
**拿不到的**：沙箱语义本身——worker 里暴露了哪些 global、插件 JS 能不能碰
fs/net/env/进程、取消是怎么实现的。

⇒ **第 1 条只能自己设计，不能移植。** 而「自动执行第三方插件代码」的沙箱是
本仓库最不该临时拍脑袋的一类决定。⛔ 别因为「上游有 worker」就假设照着 worker 写就安全——
上游 worker 的权限面我们**从未看到过**。

### 3.4 AG-16 — `cacheTtl`（P3，**明确不做，2026-09-10 复核仍成立**）

`sKt(e)` 读 `frontmatter.experimental.cacheTtl`（key 归一化成 `cachettl`，值域 `k_e`）
挂到定义上。

**⛔ 决定不做，不是阻塞**：端口**根本没有 prompt-cache TTL 这个概念** —— 请求侧没有
`cache_control` 断点选择、没有 1h/5m ephemeral 设置（`llm-runtime/src/stream_accumulator.rs`
里那些 `cache_control` 是**响应解析**）。加它要动全部 **74 处** `AgentDefinition`
字面量，产出一个没人能读的值 —— 正是本审计要找的「named, computed, never wired」形状，
不该自己造一个。

**重新评估的时机**：请求路径上出现 cache-TTL seam 的那一刻。

#### 🚨 2026-09-10 推翻：那个 seam **早就在了**，这条不再是「已关闭的决定」

上面写的「端口根本没有 prompt-cache TTL 这个概念」**是错的**。实测，请求侧有一条完整的活链路：

```
apps/engine-desktop/src/lib.rs:4442  prompt_cache_write_ttl_1h_enabled()   env 门
  → llm-runtime/src/service.rs:1095   should_1h_cache_ttl()                 ENABLE_PROMPT_CACHING_1H
  → llm-runtime/src/service.rs:1219   SplitOptions{ ttl_1h }
  → llm-runtime/src/prompt_format.rs:111/121  CacheControl::EphemeralScoped{ ttl_1h }
  → llm-runtime/src/providers/anthropic.rs:359  上线 `ttl:"1h"`
```

⚠️ 原记录取证只看了 `stream_accumulator.rs`（**响应**解析）和 `convert.rs`，
**漏了 `prompt_format.rs` 这个真正的请求侧格式化器**——正是
[[divergence-reason-enumerated-one-setter]] 那个形状：找到第一个点就当成全部。

**上游契约**（`src_163219561.js` @1339284 / @3483567）：
`experimental.cacheTtl: "5m" | "1h"`，**仅当** `subagentPromptCacheTtl` 设置/env 都没设时生效；
订阅处于 overage 时 `"1h"` 被忽略。消费点是 `agentCacheTtlOverride: e.cacheTtl`。
端口的 `ttl_1h: bool` 正好能表达（`"5m"` = 普通 Ephemeral，`"1h"` = ttl_1h）。

**所以这条现在是「未做」，不是「不该做」。真实成本（实测，不是估的）：**

| | 数量 | 说明 |
|---|---|---|
| `AgentDefinition {` **构造点** | **~17** | 没有 `Default` derive，没有一处用 `..Default::default()` ⇒ 加字段必须全改 |
| `build_request(` 调用点 | **58** | 分布在 llm-runtime(18) / service_test(31) / tool-api(6) / web / sidequery / test-harness |

⛔ **不能用「在 service 上加个字段」偷懒**：`ApiService` 是
`Arc<llm_runtime::ApiService>` 共享的（`orchestrator/src/provider_adapter.rs:27`），
并发子代理会互相踩。必须按请求传参 —— 干净的做法是把 `build_request` 的位置参数
收进一个 options struct（顺带解决它已经 8 个参数的问题）。

**两条可选实现路径（先选一条再动手）：**

- **A. options struct**：把 `build_request` 的 8 个位置参数收进一个结构体，
  加 `agent_cache_ttl` 字段。干净、显式，但要动 **58** 个调用点。
- **B. `tokio::task_local!`**：runner 在自己那一轮外面套一层 scope，
  `should_1h_cache_ttl()` 在 env 没设时读它。调用点 **0** 处改动。
  本仓库已有先例：`tasks/src/handlers/local_workflow.rs:2736` 就是这么用的。

🚨 **选 B 之前必须先验一件事**：LLM 请求是不是**在 runner 自己那个 tokio task 里**发出的。
如果请求是丢给另一个 task（channel / spawn）去发，task-local **传不过去**，
结果是一个恒为 None 的读点 —— 测试照绿、线上无效，正是本审计要抓的
「named, computed, never wired」换了个马甲。判据：在 `should_1h_cache_ttl()` 里
读到的值必须能在一个**真的走完 runner→请求**的测试里断言到，而不是单测里手动 set 一遍。

⚠️ **「75」是我数错的，实测是 ~17。** `grep -c 'AgentDefinition {'` 把**模式匹配和类型标注**
也算进去了；真正需要补字段的**构造点**，靠编译器 `E0063` 迭代收敛，两轮就到 0
（7 + 10）。⇒ 引用「N 处调用点」当成本理由之前，**用编译器数，别用 grep 数**。
同一个错误形状：[[divergence-reason-enumerated-one-setter]]。

**结论**：范围是「选一条实现路径 + ~17 处字段修改 + 优先级/overage 语义 + 端到端断言」，
不是「加一个没人读的字段」。字段本身**已实测可加、两轮收敛**；真正的工作量在
task-local 传播验证 + 优先级/overage 语义 + 那条端到端断言。
⛔ 别再引用「没有消费者」当理由——那个理由已经被推翻。

**2026-09-10 复核**（因为这一整轮里「N 个调用点」这类估算我错过两次，所以实测）：
`llm-runtime/src/convert.rs` 里出往 wire 的 `cache_control` **恒为 `None`**（589/597 行，
外加文档表格里两行），没有断点选择、没有 TTL 字段；`AgentDefinition {` 字面量现在是
**75** 个（原记 74）。结论不变，而且现在有据：加上去就是改 75 处去产出一个
**没有任何消费者**的值。这是一个**已关闭的决定**，不是待办。

---

## 子代理拒答级联（subagent refusal cascade）

**状态：已落地（`cc363fe49` 拆架构阻塞 + `d072cc4dd` 接线）。**

### 为什么本端口的子代理原先没有

上游子代理和主线程走**同一个 query 生成器**（`Nfr`），级联是天然共享的。本端口有两条循环，
级联只在 `orchestrator/`，而 `agent` **不能**依赖 `orchestrator`（方向相反）——
所以唯一的短路做法是把一个带 latch / provisional / collapse 的状态机抄两份。

### 做法

`refusal_cascade.rs` / `refusal_notice.rs`（**零 import**）移到 `platform-api`，
`orchestrator` 侧 re-export 保持 `crate::refusal_*` 不变；新增
`platform_api::refusal_driver::RefusalCascadeState` 收拢两条循环必须一致的部分
（chain 走位、latch、tried 集合、notice 累积/折叠），**不含**换模型 / post-switch hooks /
写 transcript 帧这些宿主形状的东西。orchestrator 也改走它，`ModelRuntime` 四个字段并成一个；
判据是那 15 个 refusal 测试的**断言一个字没改**。

配置下发：`OrchestratorConfig::refusal_chain()` 现在是「空 chain ⇒ 退回单个
`refusal_fallback_model`」这条规则的**唯一**所在，主线程和 spawner 都读它，
两边不会漂。两个生产组合根（desktop `lib.rs` / mobile `host.rs`）都已接。

runner：`resolve_model` 的绑定改成 `mut`，hop 改写它，下一轮 round-trip 就打到 fallback；
拒答臂对齐两条主循环（`turn_loop.rs:1333` / `drivers/mod.rs:3192`），同样只是 continue，
**都不**给被拒的那一轮立碑。

### 🚨 `scope` 是 `"local"` 不是 `"session"`

子代理的换模型只在这一次 run 内有效，不碰会话模型 —— 对应上游 `scope:"local"`。
主线程那条是持久换会话模型（`"session"`）。**`ICe` 恰好只认 `local`**，
所以照抄主线程的 `"session"` 会让 notice 永远找不到，而这里没有任何编译期或类型判据
能拦住 —— 已用测试钉死。

### 判据

4 个测试 + 3 个种雷（拒答臂删掉 / 不换模型 / scope 改回 session）全部点名变红。
含两个控制组：无 chain 时拒答仍然终止；chain 用尽后停下（每跳只消费一次，按序）。

### 这条通了之后

AG-14 的 `⚠ notice` 与 `PZo` 撤回过滤才谈得上（见本文件 §3.2）。

## 4. 保留的 LingXi divergence（⛔ 别「修」）

按既定决定不对齐：缺席的 `claude-code-guide` 和 `claude` catch-all 内置 agent
（多 provider）、`fusion` agent 表面及其 listing 条目、`.lingxi/agents/*.md` 代替
`.claude/agents/*.md`、`LINGXI_*` 环境变量命名、以及延后的 remote/CCR isolation 路径。

`vBo` 的 `forceAsync`（fork 特性开启即全后台）也是**故意没做**，原因记在
`should_run_in_background` 的 rustdoc 上：端口的 fork 路径是全同步的，父提示词和 fork
上下文消息只挂在同步 dispatch 建的 request 上，`dispatch_async` 没有对应物；加了会让
fork 走上**丢失继承上下文**的路（3 个 fork 测试当场变红）。

---

## 5. 🚨 接手前必须知道的坑

### 坑 1：共享 checkout 里 `git add` 会把你的活儿送给别人

`.git/index` 整个 checkout 只有一份。`git add` / `git apply --cached` / `git reset <path>`
全是对它的**写操作**。并发会话的 `git commit`（不带 pathspec）提交的是**整个 index**，
不是它自己的文件 —— 我因此把两个 hunk 送进了 `5bca0ae94`，另一半没提交，**main 当场编不过**。

共享 checkout 里提交的唯一安全姿势：

```sh
export GIT_INDEX_FILE=$SCRATCH/tmpindex; rm -f "$GIT_INDEX_FILE"
git read-tree HEAD                       # 私有 index
git add -- <只属于我的文件>
python3 ~/.claude/oracle-chunks/stage_hunks.py <共享文件> <标记>   # 半个文件的用过滤 patch
TREE=$(git write-tree); OLD=$(git rev-parse HEAD)
COMMIT=$(git commit-tree "$TREE" -p "$OLD" -F msg.txt)
unset GIT_INDEX_FILE
git update-ref refs/heads/main "$COMMIT" "$OLD"   # ⚠️ 带 old-value，HEAD 动过就失败
git reset -q -- <我的路径>                        # 🚨 收尾必做，见下
```

**🚨 收尾那一步不能省**：`update-ref` 之后共享 index 里你那些文件的条目还停在**旧 blob**，
`git status` 显示 `MM`。这时别人一 commit 就会把你刚提交的改动**反向提交回去**。

另外两条：`HEAD` 会在你眼皮底下动 —— 本轮我的 8 个提交之间夹着并发会话的 **12 个提交**，
提交前必须重新 `git rev-parse`；判断「这个 hunk 是不是我的」不能靠文件名，要
`git diff -U0 <file> | grep '^@@'` 数 hunk —— 我在 `reminders.rs` 只改了 1 处，
那个文件当时有 5 个 hunk。过滤 patch 的**标记要足够窄**：我用 `add_dir` 当标记时，
上下文里含 `cfg.add_dir` 的另一个会话的 hunk 也被匹配进来了。

### 坑 2：cargo 锁被别的会话卡死 ⇒ 用私有 target dir

实测同时有 **8 个 cargo/rustc 进程全部 0.0% CPU**，最老的 35 分钟，我的 `cargo build`
10 分钟一个字节都没输出 —— 那不是「在编译」，是锁队列加一个卡死的 rustc。

- 判据：`ps -eo pid,%cpu,etime,comm | grep -E 'rustc|cargo'`，全 0.0% CPU 就是卡住；
  `lsof target/debug/.cargo-lock` 点名持有者。
- 逃生口：`CARGO_TARGET_DIR=<scratchpad>/tgt cargo check -p <crate> --all-targets`。
  **实测 35 秒**完成全量 check（依赖走共享缓存，不是从零重建）。用完 `rm -rf`（会长到 3G）。
- ⛔ 别 kill 别的会话的 rustc，也别删它的 target。

### 坑 3：后台任务通知的 exit code 是 wrapper 的

本轮实测：通知报 `exit code 0`，实际 cargo 以 3 个编译错误失败。**永远读 output 文件**，
别信通知里的退出码。（那次的错误还是并发会话的中间态，已自愈 —— 归因也别想当然。）

### 坑 4：绿测试可能正钉着 bug 本身

AG-04 的 6 个红测试**就是证据本身**：它们拿 `builtin_agent_definitions().len()` 当
「目录长度」的期望值，等于把 bug 钉成了契约。AG-05 反过来 ——
`completed_no_output_uses_marker` 一直是绿的，因为它的 fixture 少了 `max_turns` 字段，
恰好躲开了新逻辑。

> **接手时照做**：目录/清单类断言要断言**名字在不在里面**，不要断言计数。

### 坑 5：🚨「做不了」这个结论要和「发现」同等取证 —— 我四次都错了

| 我写的 blocker | 实际 |
|---|---|
| `--add-dir`：EngineConfig 没有 add-dir 字段 | `cfg.add_dir` 一直都在（我 grep 的是 settings 派生的 `additional_directories` 就停了） |
| policy 目录：端口没有 managed 目录 helper | `settings_watch::managed_settings_dir()` 一直都在 |
| `outputSchema`：声明了可能让线上调用失败 | 它只在 PostToolUse hook 返回 `updatedToolOutput` 时校验替换值、不匹配就保留原值，**永远不会让工具调用失败** |
| stop-pending：没有底座 | ✅ 这条**成立**，但成立的理由是结构性的（§3.1），不是「grep 不到」 |

**凡是从 grep 得出的 blocker，都要再读一遍定义本身或消费者**（`grep -n "pub " <struct>` /
直接打开那个模块）。三条本以为要「plumbing」的活，读完之后都只是「接线」。

---

## 6. 文件索引

| 位置 | 内容 |
|---|---|
| [`agent-byte-alignment-2.1.266-2026-09-08.md`](./agent-byte-alignment-2.1.266-2026-09-08.md) | 完整审核报告：16 条发现，逐条含 oracle 偏移量、端口 file:line、修法或阻塞 |
| `~/.claude/oracle-chunks/` | `extract.py` / `find.py` / `ctx.py` / `strings.py` / `inprev.py` / `checkport.py` / `stage_hunks.py`，以及 2.1.245 / 2.1.263 / 2.1.266 的 chunk。⚠️ 都在 `~/.claude` 下而不是 scratchpad —— scratchpad 会随会话整个蒸发 |
| `~/.claude/oracle-chunks/notes/agent-audit/` | 本轮抽出的区间字面量清单 |
| `agent/src/catalog.rs` | 目录发现与 frontmatter 解析（AG-17/18/19/20/21 都在这里） |
| `agent/src/builtins.rs` | 内置 roster 与注册门（AG-06）、Explore 两份描述（AG-08） |
| `agent/src/handle.rs` | `agent_listing_entries`（AG-04/AG-08 的汇合点） |
| `tools/agent/src/agent.rs` | Agent 工具：schema（AG-01/02/03/22）、prompt、`call`、后台判定（AG-07）、turn-limit NOTE（AG-05） |
| `platform-api/src/subagent_spawn.rs` | `SubagentListingEntry` + `format_agent_line(entry, lean)`（AG-08） |
| `apps/engine-desktop/src/lib.rs` @~11409 | catalog 组合根：目录优先级、flag 合并、policy 层 |

---

## 7. 建议的下一步

1. **先重跑测试确认红的归属**（§1）—— 并发会话当时有三个在制品红，现在可能已经绿了，
   也可能变成了别的。
2. **AG-14 优先于 AG-13**：它的价值（`TaskOutput` 里模型看得见的字节）比 AG-13 高，
   而且不需要先造一套定时器机制。等 `tasks/src/handlers/local_agent.rs` 空出来就能动。
3. **AG-13 要连 `Xne` 一起搬**，否则就是永不触发的门。
4. AG-15 / AG-16 别单独动 —— 前者等 function-hook 子系统，后者等 cache-TTL seam。
