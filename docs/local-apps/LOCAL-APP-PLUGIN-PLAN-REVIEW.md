# Local App Plugin 方案评审与修订路线

评审对象：`docs/local-apps/LOCAL-APP-PLUGIN-IMPLEMENTATION-PLAN.md`（1679 行）
评审日期：2026-08-29
核对基线：`5448e93e3`（main，2026-08-28）
参照文档：`docs/local-apps/HANDOFF.md`（2026-08-27）、`docs/local-apps/RUNTIME-OS-V2.md`、
`lingxi-code/docs/mcp-plugin-byte-alignment-2.1.251-2026-08-28.md`（v2，411 行）

本文中所有关于现状的断言都在上述基线上逐条核对过，并附 `文件:行号`。
凡未附证据的判断，一律标注为「判断」而非「事实」。

---

## 0. 结论摘要

原方案的**事实基础可靠**：§10.10「现有代码接入点」表里的 8 条断言全部属实，
这在同等规模的设计文档里少见。问题不在它说错了什么，在于三件它没说的事：

1. **方案没有交付它自己声明的目标。** 目标是把 create local app 流程从 core 提取出来做职责分离；
   但方案只搬动提示层文件，从未切断 Core 对提示层的**命名依赖**（4 处，见 §4）。
   搬完之后 Core 仍然硬编码 workflow 名、skill 名单和路由表，只是名字前面多了 namespace 前缀。
   职责分离会停留在 manifest 的声明里，没有任何地方强制执行。
2. **Phase 4 假设移动端已有 Plugin 子系统——它一行都没有**（§3.2）。而 Local Apps 只在移动端存在。
3. **方案改变了 Runtime Profile 的决策主体**（用户 → Agent），但没有把它作为产品决策提出（§3.3）。

修订建议的核心：**插入一个 Phase -1，先切耦合、不搬文件**。做完之后职责分离目标已达成 90%，
且一行插件代码都不用写；文件放在 `skills/` 还是 `plugins/lingxi-local-app/skills/`
降级成一个可独立排期的打包问题。

---

## 1. 已核实为准确的断言（复审时不必重查）

原方案 §10.10 表格逐条核对结果：

| 方案断言 | 核实结果 |
| --- | --- |
| `McpToolDto` 缺 title / outputSchema / annotations / execution / icons / 完整 `_meta` | ✅ `platform-api/src/mcp.rs:215-238`，仅 7 个字段 |
| `mcp/src/client.rs` 已解析 searchHint / alwaysLoad / structuredContent | ✅ `mcp/src/client.rs:542, 1562, 1566, 1573` |
| `mcp/src/registry.rs` 已处理 `notifications/tools/list_changed` | ✅ `mcp/src/registry.rs:422` |
| `LocalAppsMcpScope` 只有 `ConversationAgent` / `App(String)` | ✅ `apps/engine-mobile/src/local_apps_mcp.rs:254-257` |
| `LocalAppsMcpTransport::notifications()` 为空流 | ✅ `local_apps_mcp.rs:1931-1937`，`stream::empty()` |
| `FlowDefinition` step 使用固定 `input_json` | ✅ `local-apps/src/runtime_v2.rs:957` |
| `AppManifest` 无 LLM-facing MCP 声明 | ✅ `local-apps/src/manifest.rs` 无对应字段 |
| Host 已执行 Flow / capability / receipt / build 边界 | ✅ `local_apps_host.rs`（14,947 行） |

另外核实为准确的：

- `PluginComponents` 确实缺 `workflows` 字段——`plugin/src/manifest.rs:81-96`
  只有 commands / agents / skills / output_styles / hooks / mcp_servers / lsp_servers。
  方案 §2.6 成立。
- `flow_execute` 确实已存在并已按 app-id 绑定——`local_apps_mcp.rs:163, 1403`、
  `local_apps_host.rs:6144`，与 `RUNTIME-OS-V2.md` 的「one physical in-process MCP hub」
  描述一致。所以 §10 是对既有 hub 的扩展，不是全新子系统。

**结论：原方案的 MCP 部分（§10 + Phase 7）是全篇最扎实的一块，可以按其内部逻辑推进。**

---

## 2. 已确定的决策

### D1 — 品牌命名空间：先沿用 LINGXI（2026-08-29 确认）

方案 §2.1 / §2.7 要求改用 `.claude-plugin/plugin.json` 和 `${CLAUDE_PLUGIN_ROOT}`。
现状与此相反且是既定决策：

- `branding/src/lib.rs:41` — `PLUGIN_MANIFEST_DIR = ".lingxi-plugin"`
- `branding/src/lib.rs:116` — 一条断言「本产品自有值里不得出现 claude」的测试
- `hooks/src/executor.rs:2217` — `LINGXI_PLUGIN_ROOT`（对照上游的
  `["CLAUDE_PROJECT_DIR","CLAUDE_PLUGIN_ROOT","CLAUDE_PLUGIN_DATA"]`，见同文件 2207 行注释）

**方案需要作出的具体修改：**

| 方案位置 | 原文 | 改为 |
| --- | --- | --- |
| §2.1 | 「唯一有效路径 `.claude-plugin/plugin.json`」 | **整节删除**——现状即最终答案，不是待办项 |
| §2.7 | 「官方变量必须可用，不能只提供 `LINGXI_*`」 | 反向：只提供 `LINGXI_PLUGIN_ROOT` / `LINGXI_PLUGIN_DATA` / `LINGXI_PROJECT_DIR` |
| §3.1 | `"$schema": "https://json.schemastore.org/claude-code-plugin-manifest.json"` | 去掉——会把 claude 字面量带进一个全新文件 |
| §3.2 | `plugins/lingxi-local-app/.claude-plugin/plugin.json` | `.lingxi-plugin/plugin.json` |

§2.2 / §2.4 / §2.5 / §2.6 不受影响（JSON key 与解析语义，与品牌无关），保留。

**两条配套要求：**

1. 决策措辞是「**先**用 LINGXI」，即后续仍可能中性化。因此新写的 Local App Plugin 代码
   **一律走 `branding::` 常量，不写任何品牌字面量**，以后翻常量即可零编辑传播。
2. **`branding` 今天没有 plugin 环境变量常量。** 全部常量只有 `DOT_DIR` /
   `GLOBAL_CONFIG_FILE` / `LEGACY_GLOBAL_CONFIG_FILE` / `CONFIG_DIR_ENV` /
   `MEMORY_FILE` / `MEMORY_LOCAL_FILE` / `PLUGIN_MANIFEST_DIR` / `PRODUCT_NAME` /
   `ENV_PREFIX` / `MANAGED_DIR_*`（`branding/src/lib.rs:21-57`）。
   `LINGXI_PLUGIN_ROOT` 目前是 `hooks/src/executor.rs:2217` 的裸字面量。
   **动手前先补这三个常量**，否则新代码会再复制一份字面量。

**风险提示（重要）：** 品牌防漏门**不在 main 上**。
`check_brand_leaks.py` / `check-brand-leaks.sh` / `brand_leak_baseline.txt` 只存在于
`.claude/worktrees/agent-namespace-plan-a/lingxi-code/scripts/`。
也就是说今天往 main 写 `.claude-plugin` **不会有任何东西报错**——品牌回流是静默的。
原方案本身就是一个例证：它照官方文档写，因此自然写成了 `.claude-plugin`。
建议把该门从 worktree 摘到 main（已带 12 条自测，真仓库 3.3s）。

---

## 3. 阻断性问题

### 3.1 §2.1 / §2.3 / §2.7 对本方案并非前置条件

除 D1 已述的品牌冲突外，还有一个独立理由：**builtin plugin 的 manifest 路径由宿主自己决定，
`lingxi-local-app` 根本不经过磁盘发现流程。** §2.3（manifestless plugin / `--plugin-dir`）
同理与 Local App Plugin 无关。

**建议：§2.1 / §2.3 / §2.7 三节整段从本方案移出**（若仍有价值，归入独立的 plugin parity workstream）。

### 3.2 Phase 4 假设移动端已有 Plugin 子系统——实际为零

核实结果：

| 检查项 | 结果 |
| --- | --- |
| `apps/engine-mobile/Cargo.toml` 的 `plugin` 依赖 | **无**，连 optional 都没有 |
| `apps/engine-mobile/src/` 中 `PluginManager` / `plugin::` | **零命中** |
| `clients/shared/src/protocol.ts` 中 "plugin" | **0 次出现** |
| `client-protocol/src/` 中 plugin DTO | 仅 `listings.rs` 提及一次 |
| iOS / Android plugin 管理 UI | **无任何文件**（对照：MCP 有 `Settings/MCPPages.swift`、`settings/MCPPages.kt`） |
| `plugin/src/` 中 "builtin" 概念 | **零命中**——`lingxi-local-app@builtin` 是全新的安装来源类别 |
| `engine-desktop/src/` 中 `LocalApp` | **零命中**——Local Apps 只在移动端存在 |

即：「把 Local Apps 做成 Plugin」的真实含义是「先把 Plugin 子系统搬上移动端」。
plugin crate 4,902 行，外加 discovery / marketplace / trust / lifecycle / 设置持久化 /
协议 DTO / 两端原生 UI。方案将其写为 Phase 4 的一个 bullet。

**但量级取决于走哪条路，而这一点方案没有区分（见 §7 待定决策 Q1）：**

- **builtin-only**：比预想的小得多。`SlashCommandKind::Plugin`
  （`command-api/src/model.rs:135-144`）、`CommandSource::Plugin`（同文件 `:250`）、
  Plugin-kind 的参数展开（`command-api/src/expand.rs:79`）**均已建模**，
  且移动端 `skill_loader.rs` 的 `to_descriptor` **已经 match `SlashCommandKind::Plugin`**。
  技能层在两端都已是插件感知的，缺的只是移动端产出这些记录的 loader。
  不需要 marketplace / git / 信任提示 / 安装卸载。
- **可安装 plugin**：需要完整子系统 + 分发 + 信任模型，且前置是供应链校验（见 §5.1）。

**建议：方案必须显式声明走哪条路，并据此重写 Phase 4 的范围。**

### 3.3 Runtime Profile 决策权从用户转给 Agent，未作为产品决策提出

现状（`local_apps_mcp.rs:57`、`local_apps_host.rs:3964`，HANDOFF §2/§4）：
用户在**原生一次性选择器**中选择 Runtime Profile，产生不可预测的 10 分钟 receipt，
由 `scaffold` 消费；HANDOFF 原文「Runtime Profile family 由用户决定一次，此后不可变」。

方案以 `template-selector` agent 替代，并明确写「不增加独立技术选型对话框」。
安全机械（receipt、digest 绑定）保留，被移除的是**人类决策点**。

这可能是正确的产品选择（普通用户不知道 Phaser 是什么），但它是一次边界反转，
不应作为插件化重构的副作用。**建议方案单列一节陈述该反转及其理由。**

---

## 4. 方案未交付其声明目标：Core→提示层的耦合未切断

**这是本次评审最重要的一条。**

声明目标是「把 create local app 流程从 core 提取出来做职责分离，
让 LLM agent 只关注自己的核心业务」。方案 §13 同时明确：
「Local Apps MCP、Runtime Profile 验证、每 App template snapshot、bridge、权限和构建
继续由 Host Core 掌控」——也就是 `apps/engine-mobile/src/local_app*.rs` 的 **35,632 行全部留在 Core**，
实际搬动的是 10 个 skill（`skills/`，44 个 md / 1,773 行）加 3 个 JS workflow。

问题不在搬动的比例，而在：**方案没有切断 Core 对提示层的命名依赖，一条都没有。**

现存耦合点（全部核实）：

| 位置 | 耦合内容 |
| --- | --- |
| `apps/engine-mobile/src/workflow_support.rs:1286-1290` | 宿主按 profile 硬编码选 workflow：`ReactDomR1 => "local-app-build"`；Canvas / Three / Phaser / Babylon `=> "local-canvas-build"` |
| `apps/engine-mobile/src/lib.rs:439-449` | 宿主硬编码 10 个 skill 名单（`mobile_skill_registry()` 编译进二进制），且有测试钉死该精确列表 |
| `apps/engine-mobile/src/host.rs:10680, 10751` | 再次硬编码 `"create-local-app"` |
| `apps/engine-mobile/src/local_apps_host.rs:11845` | 宿主把 workflow 名 `local-canvas-build` **写进 guided 契约文本** |

执行完方案 Phase 2 / 3 之后，这四条**一条都不会消失**，只是字符串变成
`lingxi-local-app:local-app-build` / `lingxi-local-app:create`。
Core 仍然知道提示层叫什么、有几个、怎么路由。

### 建议：插入 Phase -1「先切耦合，不搬文件」

1. **合并两个 workflow**（方案 §7.1 本已计划）→ 直接删除 `workflow_support.rs:1286-1290`
   的 profile→workflow 映射。方案把这条写成整理工作，实际上它是在拆 Core→提示层耦合，
   应作为独立验收点标出。
2. **技能清单改为声明式**：`lib.rs:439-449` 的硬编码名单改为「注册发现到的全部」；
   钉死 10 个名字的测试改为断言**能力**而非**名单**——否则新增一个 skill 就要改 Rust 测试，
   这本身即是耦合未断的证据。
3. **`local_apps_host.rs:11845` 的契约文本**改为宿主中立措辞（描述「运行构建流程」，不点名 workflow）。
4. **加一道门**：断言 `apps/engine-mobile/src/` 中不出现任何 skill / workflow / agent 名字字面量，
   计数基线入 CI。

第 4 条不是可选项。本仓库的历史记录显示：`.claude`→`.lingxi` 之后仍留下 60 个活的 `CLAUDE_*`，
原因不是没做完，而是**每一轮 parity 对齐都会重新带进来**。没有门，下一个改动会把 workflow 名字
重新写回 host，且不会有任何东西报错。**门自身也需要埋雷测试 + 计数基线，只断言 exit 0 不够。**

**Phase -1 完成后，职责分离目标已达成 90%，且未写一行插件代码。**

---

## 5. 实质缺口

### 5.1 供应链校验缺失——如果走「可安装 plugin」，这是第一笔账

`mcp-plugin-byte-alignment-2.1.251` v2（411 行，2026-08-28 20:30 修订）把
**§15 排在施工顺序第 1 位**：

> Plugin archive 下载没有 scheme / host / digest 校验。
> `apps/cli/src/startup_resources.rs:135-154` 对原始 URL 直接 `bounded_get` 然后解包：
> 无 https 检查、无私网 / link-local / 云元数据主机检查、无 sha256 校验。
> 一个 marketplace catalog 条目可以让 CLI 抓取 `http://169.254.169.254/…`
> 或任意 loopback 服务并解包其返回内容。

同族且同样缺失：§16（install-consent 对 command 的长度 / 可打印 ASCII 反欺骗）、
§8（2.1.247 名称校验）、reserved-name gate、每个 plugin-entry source 上的
`sha`（40-hex git）/ `sha256`（64-hex）pinning。

**结论：「可安装 plugin」路线的前置不是移动端 UI，而是这组供应链校验。**
另需注意 §6：**object 形式的 `commands` map 会静默丢掉整个插件**——
`lingxi-local-app` 的 manifest 若写成 object 形式，插件消失且不报错。

### 5.2 per-tool 权限天花板缺失，而方案的安全性正建立在其上

同一审计 P1 §1 / §2：`tools[].permission_policy`（always_allow / always_ask / always_deny，
最严者胜）与 `toolPermissions`（allow / ask / blocked + `org_max_permission` 组织天花板）
**在端口里零实现**——「一个静默不生效的管理性权限上限」。

方案 §10.5 把安全性寄托于「实际调用仍走现有 MCP permission，可授权精确 FQN 或 server wildcard」，
而现有 permission 缺的恰是这一层。Local App tool 由**不可信内容作者**编写，
方案自己也声明 annotations 只是提示不是授权——这正是最需要 per-tool ask / deny 上限的场景。

**建议：并入 Phase 7 step 1（MCP DTO parity）一起做。** 审计自身把它排在第 2 位，
仅次于 §15。

### 5.3 `published` / `published_unverified` 是全新生命周期状态，被当作既有状态使用

今天只有 `scaffolded: bool`。HANDOFF 明确拒绝状态机：
「shell 阶段不是 draft 状态机，`scaffolded` 是一个 bool、两个位置、没有中间持久化步骤」。

方案需要 `draft → approved candidate → active`、`published` vs `published_unverified`、
`AppMcpCatalogRef` 原子 promote / rollback。这是独立一期的量，
**且需要设备端数据迁移**——`local-apps/src/data.rs` 那类 schema 变更在仓库里没有任何东西能抓到失败
（单测用新建临时库，一致的改名让设备上每个已存在的本地应用静默丢数据）。

**建议：单列一个 Phase，并要求给出迁移方案 + 一个从「旧代码状态」播种的测试。**

### 5.4 「每个 published App 必须有 1–16 个 tool」是个坏门

方案 §10.2 的 publish gate。一个画板玩具、Phaser 小游戏或 Three.js 场景可能真的没有
值得暴露给 LLM 的能力；强迫 designer 编一个出来，产出的是污染全局工具命名空间的垃圾 tool。

**建议：放开为 0–16，把「本 App 无需暴露 tool」变成合法且被记录的结论**——
`excluded_capabilities` 已能表达理由。保留 authoring 步骤强制，放开 tool 数量。

### 5.5 per-App server × N 个 App 没有伸缩方案

方案 §10.2「已发布 App 始终注册自己的 server」。30 个 App = 30 个 in-process server、
30 条连接、启动时 30 次 `tools/list`、最多 480 个 tool。方案只谈 deferred loading 与 searchHint，
从未给**已注册 server 数量**设界。

**建议：惰性注册**（首次打开该 App，或每 App 一个显式「暴露给对话」开关）+ 硬上限 +
明确的淘汰规则。同时缩小 Phase 7 step 6 的启动恢复面。

### 5.6 合并 workflow 时 canvas 的 verify 门会静默降级

今天是两个 builtin：`local-app-build`（DOM / Ionic 路由）与 `local-canvas-build`
（canvas 家族，带 captured-frame 与 motion 证据的 prompt + schema），共享
`local_app_workflow_core.js`。合并方向正确，但**verify 门必须继续按 profile 选择**：
DOM inspect 对一个只有 `<canvas>` 的页面是恒真的。合并时若让它成为统一门，
canvas 家族的渲染验证会变成一个永远绿、什么都不证明的门。

### 5.7 模板叙述需要修正

- §5.1 写「五套模板全部放入统一 Plugin」，读起来像要新建。实际上**五个家族已完整存在**：
  `local-apps/templates/runtime-profiles/{react-dom,canvas-2d,three-3d,phaser-2d,babylon-3d}/r1/`。
  应改写为「搬迁 runtime-profiles，并退役 `vite-react-static-v1` / `vite-react-canvas-v1`
  两套遗留脚手架」——后者仍被 `local-apps/src/permissions.rs:37` 引用。
- §5.2 「Babylon spike 未通过时不进 catalog」与现状矛盾：`babylon-3d/r1` 已是完整 profile
  且已接线（`local_app_runtime_profiles.rs:147-158, 838`）。
  **要么它已经通过，要么 catalog 里现有一个未验证的 profile——需先查清。**
- **模板是编译进二进制的**：`local_app_runtime_profiles.rs:18-32` 用
  `include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../local-apps/templates/runtime-profiles/", …))`。
  方案假定 PluginManager 按 digest 解析磁盘资产——**这一点在移动端不成立**
  （移动端没有磁盘 plugin root，资产必须在 app bundle 内或编译内嵌）。
  无论新位置选在哪，都必须在 crate 的 include 路径内且被 git 跟踪
  （曾有 `include_str!` 拉取 gitignore 文件导致新 worktree 编不过的先例）。

---

## 6. 修订后的实施路线

原方案 Phase 0→8 的顺序把最贵、最有争议的排在最前（品牌反转的插件契约对齐），
把唯一有产品价值的排在最后（per-App MCP）。建议按下述两条互不阻塞的 track 重排。

### Track A — 立即可做，不依赖任何待定决策

| 步骤 | 内容 | 依据 |
| --- | --- | --- |
| A1 | 补 `branding` 的三个 plugin env 常量 | §2 D1 |
| A2 | 把 brand-leak 门从 worktree 摘到 main | §2 D1 风险提示 |
| A3 | **Phase -1 切耦合四步** | §4 |
| A4 | MCP DTO parity（原 Phase 7 step 1）+ 审计 §1/§2 权限天花板 | §5.2 |
| A5 | `PluginComponents` 加 `workflows` + namespaced resolver（原 §2.6） | §1 |

A3 完成即交付职责分离目标。A4 / A5 是 §10 全部内容的地基，且不依赖插件化改造。

### Track B — 等待 Q1 决策

- **若 builtin-only**：移动端一个只读 builtin loader
  （`SlashCommandKind::Plugin` 已建模，见 §3.2）。范围小。
- **若可安装**：前置为审计 §15 / §16 / §8 + reserved-name gate + digest pinning（§5.1），
  之后才谈移动端 UI 与分发。

### Track C — per-App MCP 产品能力（原 Phase 7 主体）

**可以在今天的 bundled skills + builtin workflows 上完成，不必等插件化。**
按方案 §11 Phase 7 的 1→7 内部顺序推进，但：

- step 1 并入 A4；
- 补 §5.3 的状态机与迁移作为独立前置；
- 应用 §5.4（0–16）、§5.5（惰性注册）的修正。

### 从原方案中移出

§2.1、§2.3、§2.7（品牌冲突且非前置，见 §3.1）。

---

## 7. 待定决策

**Q1（阻断 Track B）：builtin-only 还是可安装 plugin？**

- builtin-only（编译进 app bundle）：拿到命名空间、生命周期、代码组织的收益，
  **但拿不到「不发版就能改 prompt」**——今天 10 个 skill 由 `mobile_skill_registry()`
  编译进二进制（`lib.rs:439-449`），改一句提示词需要发一次 App Store。
- 可安装：拿到独立更新与第三方扩展能力，代价是完整子系统 + 分发 + 信任模型。
  且 Local App 生成的代码本身已是不可信内容，再叠一层可安装插件，信任模型需重新设计。

> 「不发版改 prompt」是本次改造里价值最大的副产品，原方案通篇未提，
> 而 `@builtin` 按现在的写法并不提供它。此项单独决定 Phase 4 的形状。

**Q2（阻断 Phase 3）：Runtime Profile 由 Agent 选择，是否确认？**（见 §3.3）

**Q3（阻断 §5.7）：`babylon-3d` 的真机验证状态是什么？**

---

## 8. 给复审者的重点

请优先质疑以下四条判断，它们是本文结论的承重点：

1. **§4 的核心论断**——「方案搬完文件后职责分离仍未发生」。
   反驳方式：指出方案中任何一处**明确要求删除或反转** `workflow_support.rs:1286-1290`
   / `lib.rs:439-449` / `host.rs:10680,10751` / `local_apps_host.rs:11845` 这四处耦合的文字。
   若存在，本文 §4 应予撤回。
2. **§3.2 的量级判断**——builtin-only 路线「比预想小得多」，依据是
   `SlashCommandKind::Plugin` / `CommandSource::Plugin` / `expand.rs:79` /
   移动端 `skill_loader.rs` 已建模。反驳方式：找出 builtin plugin 在移动端落地
   仍必须引入的 plugin crate 模块（`plugin/src/` 中 "builtin" 为零命中，
   意味着安装来源类别需新增）。
3. **§5.6 的门降级风险**——合并 workflow 后 canvas verify 是否真的会退化为恒真门。
   反驳方式：给出合并后仍按 profile 选择证据类型的具体机制。
4. **§5.7 的模板结论**——「五套模板已存在」是否把 runtime-profile 契约
   误当成了 scaffold 模板。二者在 `local-apps/templates/` 下确为不同目录，
   若语义不等价，§5.7 与 Track C 的排期需修正。

另请注意本文**未做**的事：

- 未验证方案 §10 内部的 MCP 契约细节是否与 MCP 2025-11-25 schema 逐字段对齐
  （仅核对了它对现有代码的描述属实）。
- 未实测任何运行时行为；全部结论来自源码与文档核对。
- 未评估方案对 iOS / Android 客户端的 UI 工作量。
