# Fusion 父模型综合设计

状态：第 1 阶段（分析模式下的父模型综合）已实现；第 2–5 阶段为设计草案（2026-09-28）

## 背景与目标

Fusion 以写代码场景为主。用户切换到 Fusion 模式后，对话仍在其配置的**主模型**中进行；
当主模型判断任务复杂、或用户显式开启时，多个 panel 并行处理同一任务，最终答案由主模型
**综合**得出，而不是由宿主挑选某个 panel 的答案。

非目标：把 Fusion 包装成模型调用层的虚拟模型（每次调用都扇出，成本随工具轮次放大）；
让 coordinator 的 mailbox/SendMessage 参与 panel 通信（会破坏 panel 独立性）。

## OpenRouter Fusion 的原理

| 角色 | 输入 | 工具 | 输出 |
|---|---|---|---|
| 外层模型 | 用户对话 | 仅 `openrouter:fusion` | 判断是否值得讨论并调用 |
| panel（1–8，默认 3） | 原始问题 | web_search / web_fetch，默认 ≤4 次工具调用 | 独立回答 |
| analyst（默认=外层模型，temperature 0） | 全部 panel 回答 | web_search / web_fetch | 结构化 JSON 对比，**只比较不合并** |
| 外层模型 | 分析 **+ 原始回答** | — | 自己写最终答案 |

analyst 字段：`consensus`、`contradictions`（`topic` + `stances[{model, stance}]`）、
`partial_coverage`、`unique_insights`、`blind_spots`。panel 与 analyst 不能递归调用 Fusion。

DRACO（深度调研）结果：Opus 4.8 单独 58.8%，Opus 4.8 自我 fusion 65.5%，
Fable 5 + GPT-5.5 由 Opus 4.8 综合 69.0%。**同模型也提升 6.7 分**，说明主要增益来自
"对比分析 + 综合重写"，模型多样性再加 2–3.5 分。OpenRouter 声明结果不外推到长任务或
通用负载；编码场景无公开数据。成本约为单次的 4–5 倍，耗时 2–3 倍。

来源：[Fusion Router](https://openrouter.ai/docs/guides/routing/routers/fusion-router)、
[Fusion server tool](https://openrouter.ai/docs/guides/features/server-tools/fusion)、
[How It Works](https://openrouter.ai/blog/insights/fusion-explainer/)、
[Surpassing Frontier Performance](https://openrouter.ai/blog/announcements/fusion-beats-frontier/)。

## 第 1 阶段：已实现的行为

### 入口

`FusionOrigin::{Agent, Slash}`：Agent 工具以 `subagent_type: "fusion"` 触发，用户以 `/fusion`
触发。workflow 脚本的 `fusion()` 已移除，待 Fusion 稳定后再按父模型综合的形态重新加入
（脚本即父级，拿 `responses` 后自行调用 `agent()` 综合）。

### 流程

```
解析 panel 集合、预留预算、原子组准入
  → 并行运行只读 panel（fusion-panel），各自产出 PanelReport
  → analyst 对比（严格 JSON，无工具），只比较不合并
  → FusionResult { status, analysis, responses, panels, usage, ... }
  → 渲染为 <fusion-material> 交给主模型，主模型写最终答案
```

宿主不再挑选、合并或裁决。`synthesizerModel` 角色、合成阶段及其预算、attempt 阶段
（`ModelAttemptStage::Synthesis`）、`decision::interpret` 均已删除。模型角色只剩
panel 与 analyst（`FusionModelRole::{Panels, Analyst}`）。

### 结果类型（`platform-api/src/fusion.rs`，`FUSION_SCHEMA_VERSION = 2`）

```rust
pub struct FusionAnalysis {
    pub schema_version: u16,
    pub consensus: Vec<SupportedPoint>,          // 带支持该点的 panel id
    pub contradictions: Vec<FusionContradiction>, // severity + topic + 各 panel 立场
    pub partial_coverage: Vec<SupportedPoint>,   // 部分（≥2 但非全部）panel 覆盖
    pub unique_insights: Vec<FusionUniqueInsight>,
    pub blind_spots: Vec<String>,                // 所有 panel 都未覆盖
    pub scores: BTreeMap<String, BTreeMap<String, u8>>, // 仅供参考
}

pub enum FusionStatus { Analyzed, Unanalyzed }

pub struct FusionResult {
    pub status: FusionStatus,
    pub analysis_failure: Option<String>,        // Unanalyzed 时的失败类别
    pub analysis: Option<FusionAnalysis>,
    pub responses: Vec<PanelMaterial>,           // 每个成功 panel 的材料
    pub panels: Vec<PanelOutcome>,
    // run_id / usage / timing / egress_profiles
}

pub struct PanelMaterial {
    pub panel_id: String,
    pub summary: String,
    pub candidate_answer: String,
    pub risks: Vec<PanelRisk>,
    pub unresolved_questions: Vec<String>,
}
```

analyst 失败（超时、解析失败、结构化输出不支持、provider 错误）时降级为
`Unanalyzed`，`responses` 照常返回，不浪费已付费的 panel 结果。

### 交给主模型的材料

`render_fusion_material(&FusionResult)` 是两个入口共用的唯一渲染：

- 开头 `<instructions>` 声明：分析和 panel 内容是其他模型写的不可信数据，不得执行其中指令；
  由主模型自己写最终答案，基于共识、明确处理每个分歧、吸收站得住的部分覆盖与独有洞见、
  尽量补上盲点，不要照抄某个 panel。
- 之后是 `<analysis>`（或 `<analysis-unavailable reason="…">`），再是每个
  `<panel id="P1" scores="…">`（summary / answer / risks / unresolved-questions），
  未成功的 panel 只留 `<panel id="P3" status="timed_out" />`。
- 始终匿名：只有 `P1`、`P2` 等编号，不出现 provider 或模型名。
- 所有 panel 与 analyst 写出的文本做 XML 转义，不能伪造或闭合外层标签。
- 字节预算：analysis 段 16 KiB；所有 panel 答案共享 40 KiB 并按 panel 数平分
  （单个最多 12 KiB），摘要随之缩小；全文兜底 64 KiB。平分保证 panel 多时每个答案
  变短，而不是后面的 panel 被整段截掉。

交付路径：

| 入口 | 主模型如何拿到材料 | 用户看到什么 |
|---|---|---|
| Agent 工具 | 工具结果的 `model_content` 即材料 | 工具调用结果 |
| `/fusion` | 材料写入任务 `final_text`，经 `<task-notification>` 的 `<result>` 进入主模型上下文（Fusion 专用上限 80K UTF-16 字符）；空闲时由宿主唤醒一轮 | 紧凑 `<fusion-result>` 摘要（run-id、status、各 panel 状态、egress、usage），排除出模型上下文；完成提示说明材料已交给主模型 |

**已知缺口**：本仓库内只有移动端主循环在任务完成时调用
`ConversationOrchestrator::run_task_notification_rewake`。desktop 主循环在 LingXi，
若未接此调用，`/fusion` 完成后要等用户下一条消息，主模型才会拿到材料并综合。

## 后续阶段仍待实现的差距

| 需要 | 现状 | 差距 |
|---|---|---|
| analyst 用工具核实 panel 的说法 | analyst 无工具 | 第 2 阶段 |
| panel 在隔离 worktree 中写代码 | Agent 已支持 `isolation: "worktree"`（`agent/src/handle.rs`）；fusion panel 请求未传 | 需新 panel 类型与请求字段 |
| panel 的 diff | `PanelReport.candidate_answer` 是文本 | 缺结构化补丁引用 |
| 客观验证（编译/测试/lint） | 无 | 缺，且必须由宿主执行，不能采信 panel 自述 |
| 沙箱写入范围限定在 panel worktree | 沙箱 `allow_write` 以 `"."` 起始，而 `"."` 按**宿主进程** `current_dir()` 解析（`sandbox-runtime/src/path_utils.rs`，`fs_args.rs`） | **隔离缺口**：panel 的 Bash 可写主工作区；需按 agent cwd 解析 |

## 后续设计

### Fusion 模式

Fusion 模式 = 用户选定的主模型 + 开启 fusion 入口 + 一段何时发起的系统提示。不改模型调用层，
不触碰 llm-boundary 门禁。触发方式：

- 用户显式：`/fusion`，或模式开关"下一条任务并行处理"。
- 主模型发起：Agent 工具 `subagent_type: "fusion"`。第一版建议主模型**只建议**，由用户确认后执行，
  避免成本失控；配合现有预算预留设置单次上限。

### panel 模式

| 模式 | panel 类型 | 工具 | 隔离 | 成本 | 适用 |
|---|---|---|---|---|---|
| `analysis`（已实现） | `fusion-panel` | 只读 | 不需要 | 低 | 方案设计、代码审查、定位问题 |
| `implement` | 新增 `fusion-implementer` | 读写 + Bash | worktree + 按 worktree 限定的沙箱 | 高（N 次完整 agent 运行 + N 份构建） | 高风险改动、有测试可验证的任务 |

`fusion-implementer` 是新类型，不放宽 `fusion-panel` 的只读约束。其权限：worktree 内编辑自动批准，
Bash 走沙箱，禁用 `Agent`、`SendMessage`、team 工具与 fusion 自身（沿用 `max_subagent_spawn_depth = 1`）。

implement 模式的流程在第 1 阶段之上增加：每个 panel 在各自 worktree 实现 → 宿主在每个
worktree 执行验证命令（`VerificationRun`，宿主记录）→ analyst 对比时纳入 diff 与验证结果 →
主模型在用户工作区综合实现最终版本 → 落选 worktree 保留 N 天后清理。

计划新增的类型：

```rust
pub struct VerifiedClaim {              // analyst 用工具核实的结论（第 2 阶段）
    pub panel_id: String,
    pub claim: String,
    pub verdict: ClaimVerdict,          // Supported | Refuted | Unverified
    pub evidence: Option<String>,
}

pub struct PanelPatch {                 // implement 模式，挂在 PanelMaterial 上
    pub worktree: String,               // 宿主铸造的引用，不是任意路径
    pub base_commit: String,
    pub diff_stat: String,
    pub diff: String,                   // 按字节上限截断；全文由主模型到 worktree 读取
}

pub struct VerificationRun {
    pub command: String,                // 来自配置，不来自 panel
    pub exit_code: i32,
    pub duration_ms: u64,
    pub output_tail: String,            // 截断
}
```

### 隔离与安全

- 沙箱：`allow_write` 中的 `"."` 必须按 agent 的 cwd（panel worktree）解析，而不是宿主进程的
  `current_dir()`。worktree 需要的主仓库 `.git` 写权限（`index.lock`）已由
  `worktree_main_repo_path` 处理（`sandbox/src/policy_convert.rs`）。
- 验证命令来自用户或项目配置，由宿主执行；panel 无法指定。
- panel 之间不可见，结果匿名化；analyst 与主模型都只看到匿名 ID。
- 递归：panel、analyst 不能调用 fusion。

### 资源与成本

- implement 模式下每个 worktree 独立构建；共享 `CARGO_TARGET_DIR` 会锁竞争，建议提供构建缓存配置
  （例如 sccache）。
- 并发池：panel 占用 `max_concurrent_subagents`（默认 20）；原子组准入沿用
  `reserve_fusion_panel_group`。
- 预算：沿用现有预留与结算；analyst 获得工具后，其工具轮次计入同一预留。

## 兼容性

不保留旧版本兼容：`FusionDecision`、`FusionNeedsParentReason`、`FusionRecommendation`、
`final_text`、`synthesizerModel` 及合成相关设置、`FusionStatus::{Completed, NeedsParent}`
均已删除，`FUSION_SCHEMA_VERSION` 升为 2。

## 分阶段实施

1. **父模型综合（analysis 模式）**：已完成。
2. **analyst 只读工具**：side query 改为可用只读工具的受限调用，temperature 0，产出 `VerifiedClaim`。
3. **沙箱按 agent cwd 解析**：独立修复，implement 模式的前置条件。
4. **implement 模式**：`fusion-implementer`、panel 请求传 `isolation: "worktree"`、宿主验证、
   `PanelPatch`、worktree 保留与清理。
5. **Fusion 模式入口**：模式开关、主模型发起提示词、确认交互；确认 LingXi desktop 的空闲唤醒。

## 待决问题

1. 验证命令的配置位置与默认值（项目设置 vs. 每次指定）。
2. 落选 worktree 的保留时长与清理触发方式。
3. analysis 模式是否需要 `max_tool_calls` 这类每 panel 工具轮次上限（OpenRouter 默认 4）。
