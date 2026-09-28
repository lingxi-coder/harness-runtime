# Fusion 父模型综合设计

状态：草案（2026-09-28）

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

## 现状（代码事实）

- 入口：`FusionOrigin::{Agent, Slash}`；Agent 工具以 `subagent_type: "fusion"` 触发。workflow 脚本的
  `fusion()` 已移除，待 Fusion 稳定后再按父模型综合的形态重新加入（脚本即父级，拿 `responses`
  后自行调用 `agent()` 综合）。
- panel：经 `SubagentSpawner` 派生，类型 `fusion-panel`，工具显式只读
  （`Read/Grep/Glob/WebFetch`，`agent/src/builtins.rs:819`，有测试锁定），
  `StructuredOutputMode::WhenDone` 产出 `PanelReport`，按 `model_profile` 路由 provider。
- analyst：side query，严格 JSON，**无工具**（`fusion/src/analyst.rs:1`）。
- 裁决：`decision::interpret` 把 analyst 的 `recommendation` 转成
  `Pick` / `Merge`（独立 `synthesizerModel`）/ `NeedsParent`；`Merge` 在存在 Critical 分歧或
  `confidence < 60` 时降级为 `NeedsParent`。
- 返回：`FusionResult` 只有 `final_text`、`analysis` 和紧凑的 `PanelOutcome`
  （状态、耗时、用量），**不含 panel 原始回答**。仅 `NeedsParent` 时 `needs_parent_text`
  会把截断后的 `candidate_answer` 渲染进 `final_text`。

## 字段差距

### FusionAnalysis 对照 OpenRouter

| OpenRouter | 本仓库 | 差距 |
|---|---|---|
| `consensus` | `consensus: Vec<String>` | 缺支持的 panel 列表，父模型无法判断共识强度 |
| `contradictions{topic, stances}` | `contradictions{severity, topic, positions[{panel_id, position}]}` | 已覆盖，且多出 `severity` |
| `partial_coverage` | 无 | **缺失**。`unique_insights` 只能挂一个 `panel_id`，表达不了"部分（≥2 但非全部）覆盖" |
| `unique_insights` | `unique_insights{panel_id, insight}` | 已覆盖 |
| `blind_spots` | `coverage_gaps: Vec<String>`（文档注释"none of the panels covered"） | 语义相同，仅命名不同 |
| — | `scores`、`confidence`、`recommendation` | 本仓库特有；`recommendation` 在新设计中不再决定终点 |
| analyst 可用工具核实 | analyst 无工具 | **缺失** |

### 交给父模型的材料

| OpenRouter | 本仓库 | 差距 |
|---|---|---|
| 分析 + `responses[{model, content}]` | `final_text` + `analysis` + 紧凑 `PanelOutcome` | **缺原始回答**（除 NeedsParent 的截断渲染） |
| 最终答案永远由外层模型写 | `Pick` 直接返回某 panel 答案；`Merge` 由单独配置的 synthesizer 写 | 终点与综合者都不同 |

### 编码场景特有（OpenRouter 也没有）

| 需要 | 本仓库 | 差距 |
|---|---|---|
| panel 在隔离 worktree 中写代码 | Agent 已支持 `isolation: "worktree"`（`agent/src/handle.rs:2422`）；fusion panel 请求未传 | 需新 panel 类型与请求字段 |
| panel 的 diff | `PanelReport.candidate_answer` 是文本 | 缺结构化补丁引用 |
| 客观验证（编译/测试/lint） | 无 | 缺，且必须由宿主执行，不能采信 panel 自述 |
| 沙箱写入范围限定在 panel worktree | 沙箱 `allow_write` 以 `"."` 起始，而 `"."` 按**宿主进程** `current_dir()` 解析（`sandbox-runtime/src/path_utils.rs:375`，`fs_args.rs:477`） | **隔离缺口**：panel 的 Bash 可写主工作区；需按 agent cwd 解析 |

## 设计

### 1. Fusion 模式

Fusion 模式 = 用户选定的主模型 + 开启 fusion 入口 + 一段何时发起的系统提示。不改模型调用层，
不触碰 llm-boundary 门禁。触发方式：

- 用户显式：`/fusion`，或模式开关"下一条任务并行处理"。
- 主模型发起：Agent 工具 `subagent_type: "fusion"`。第一版建议主模型**只建议**，由用户确认后执行，
  避免成本失控；配合现有预算预留设置单次上限。

### 2. panel 模式

| 模式 | panel 类型 | 工具 | 隔离 | 成本 | 适用 |
|---|---|---|---|---|---|
| `analysis`（默认） | 现有 `fusion-panel` | 只读 | 不需要 | 低 | 方案设计、代码审查、定位问题 |
| `implement` | 新增 `fusion-implementer` | 读写 + Bash | worktree + 按 worktree 限定的沙箱 | 高（N 次完整 agent 运行 + N 份构建） | 高风险改动、有测试可验证的任务 |

`fusion-implementer` 是新类型，不放宽 `fusion-panel` 的只读约束。其权限：worktree 内编辑自动批准，
Bash 走沙箱，禁用 `Agent`、`SendMessage`、team 工具与 fusion 自身（沿用 `max_subagent_spawn_depth = 1`）。

### 3. 流程

```
主模型发起 fusion(task, mode)
  → 解析 panel 集合、预留预算、原子组准入（现有）
  → 并行运行 panel
      analysis: 只读调研 → PanelReport
      implement: 在各自 worktree 实现 → PanelReport + 补丁引用
  → [implement] 宿主在每个 worktree 执行验证命令 → VerificationRun（宿主记录，非 panel 自述）
  → analyst（temperature 0，只读工具）对比 → FusionAnalysis v2
  → 返回 FusionResult：analysis + 每个 panel 的原始材料 + 验证结果
  → 主模型在用户工作区综合实现最终版本
  → 落选 worktree 保留 N 天后清理
```

宿主不再挑选或合并。仍保留的宿主职责：schema 校验、匿名化、预算结算、Critical 分歧与低置信度
**标注**（写进结果供主模型参考，而不是改变终点）。

### 4. Schema 变更（`schema_version` 升级）

```rust
pub struct FusionAnalysis {
    pub schema_version: u16,
    pub consensus: Vec<SupportedPoint>,            // 由 Vec<String> 改为带支持者
    pub contradictions: Vec<FusionContradiction>,  // 不变
    pub partial_coverage: Vec<SupportedPoint>,     // 新增
    pub unique_insights: Vec<FusionUniqueInsight>, // 不变
    #[serde(alias = "coverage_gaps")]
    pub blind_spots: Vec<String>,                  // 改名，保留旧名反序列化
    pub verified_claims: Vec<VerifiedClaim>,       // 新增：analyst 用工具核实的结论
    pub scores: BTreeMap<String, BTreeMap<String, u8>>, // 保留，仅供参考
    pub confidence: u8,                            // 保留，仅供参考
    pub recommendation: Option<FusionRecommendation>,   // 改为可选，Fusion 模式下不决定终点
}

pub struct SupportedPoint { pub point: String, pub panel_ids: Vec<String> }

pub struct VerifiedClaim {
    pub panel_id: String,
    pub claim: String,
    pub verdict: ClaimVerdict,          // Supported | Refuted | Unverified
    pub evidence: Option<String>,
}

pub struct PanelMaterial {              // 新增，放入 FusionResult.responses
    pub panel_id: String,
    pub summary: String,
    pub candidate_answer: String,       // 按字节上限截断
    pub patch: Option<PanelPatch>,      // implement 模式
    pub verification: Vec<VerificationRun>,
}

pub struct PanelPatch {
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

`FusionResult` 新增 `responses: Vec<PanelMaterial>`；`FusionDecision` 新增
`ParentSynthesis`，作为 Fusion 模式的唯一终点。`consensus` 由字符串改为对象会破坏旧
反序列化，需在 `schema_version` 升级时同时接受两种形态。

### 5. 隔离与安全

- 沙箱：`allow_write` 中的 `"."` 必须按 agent 的 cwd（panel worktree）解析，而不是宿主进程的
  `current_dir()`。worktree 需要的主仓库 `.git` 写权限（`index.lock`）已由
  `worktree_main_repo_path` 处理（`sandbox/src/policy_convert.rs:193`）。
- 验证命令来自用户或项目配置，由宿主执行；panel 无法指定。
- panel 之间不可见，结果匿名化（现有 `panel::anonymize`）；analyst 看到匿名 ID，主模型看到匿名
  ID 与 provider 映射由配置决定是否披露。
- 递归：panel、analyst 不能调用 fusion。

### 6. 资源与成本

- implement 模式下每个 worktree 独立构建；共享 `CARGO_TARGET_DIR` 会锁竞争，建议提供构建缓存配置
  （例如 sccache）。
- 并发池：panel 占用 `max_concurrent_subagents`（默认 20）；原子组准入沿用
  `reserve_fusion_panel_group`。
- 预算：沿用现有预留与结算；analyst 获得工具后，其工具轮次计入同一预留。

## 兼容性

- `/fusion` 与 Agent 两个入口统一改为父模型综合，`Pick`/`Merge` 路径整体移除，不保留旧版本兼容。
- 单独的 `synthesizerModel` 配置在 Fusion 模式下不再使用；保留一个版本后移除。
- `FusionAnalysis` 旧字段通过 serde alias 与双形态反序列化兼容。

## 分阶段实施

1. **父模型综合（analysis 模式）**：`FusionResult.responses`、`ParentSynthesis` 终点、
   `FusionAnalysis` v2 字段、analyst 提示词更新。收益最大、风险最小，不涉及写权限。
2. **analyst 只读工具**：side query 改为可用只读工具的受限调用，temperature 0。
3. **沙箱按 agent cwd 解析**：独立修复，implement 模式的前置条件。
4. **implement 模式**：`fusion-implementer`、panel 请求传 `isolation: "worktree"`、宿主验证、
   `PanelPatch`、worktree 保留与清理。
5. **Fusion 模式入口**：模式开关、主模型发起提示词、确认交互。

## 待决问题

1. 主模型是否能看到 panel 的 provider/模型名（利于判断，但削弱匿名性）。
2. 验证命令的配置位置与默认值（项目设置 vs. 每次指定）。
3. 落选 worktree 的保留时长与清理触发方式。
4. analysis 模式是否需要 `max_tool_calls` 这类每 panel 工具轮次上限（OpenRouter 默认 4）。
