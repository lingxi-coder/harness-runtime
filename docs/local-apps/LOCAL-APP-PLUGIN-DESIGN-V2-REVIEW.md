# LOCAL-APP-PLUGIN-DESIGN-V2 复审报告

评审对象：`docs/local-apps/LOCAL-APP-PLUGIN-DESIGN-V2.md`（1713 行）
评审日期：2026-08-29
核对基线：`5448e93e3`（main）
上一轮评审：`docs/local-apps/LOCAL-APP-PLUGIN-PLAN-REVIEW.md`

## 方法与覆盖度（先读这一节，它界定了本文的可信范围）

6 个独立维度（代码事实、findings 关闭情况、文档内部一致性、门的诚实性、新机制风险、整块遗漏）
并行审查 V2，共产出 **45 条候选 finding**。受每维度取 4 条上限约束，
**只有 24 条进入对抗验证**，每条由三个互不相同的镜头独立反驳：

- 文档镜头：穷尽搜索 V2 全文，判断是否别处已处理（默认倾向判为「已处理」）
- 代码镜头：到仓库核实 finding 的事实前提（前提站不住即判否）
- 重要性镜头：假设 finding 完全正确，改与不改实施结果是否不同（仅措辞问题判否）

三票中两票以上反驳则淘汰。结果：**14 条存活、10 条被驳回**。

**⚠️ 覆盖度限制：21 条候选 finding 未经验证，不计入本文结论。** 未验证的集中在
gates（10 条验了 4）、closure 与 new-risk（各 8 条验了 4）。若需完整覆盖需提高上限重跑。

**本次复审未做**：V2 §13 的 MCP DTO 是否与 MCP 2025-11-25 schema 逐字段对齐；
任何运行时实测；iOS/Android UI 工作量评估。

本文中标注「**已亲验**」的条目由评审者本人直接跑命令核实，其余为 agent 核实并经三镜头验证。

---

## 0. 结论

V2 是一次实质性改进。上一轮评审的 13 条中 **11 条被机制性关闭**——给了具体机制，
不是提一句：§8 独立解耦章节、§16.2 派生状态（绕开状态漂移与设备迁移）、§11.2 Canvas 硬门、
§14 permission ceiling、§15.3 惰性暴露预算、§9.3 Babylon availability gate、§9.1 模板搬迁。
三个待定决策（builtin-only / Agent 选模板 / 品牌 LINGXI）全部拍板并写入「已确定决策」。

**但存在 2 条会改变产品行为的缺陷、2 条会让实施当场失败的规格空档、3 块整块缺失。**

建议处理顺序：

| 优先级 | 内容 | 性质 |
| --- | --- | --- |
| 先定 | §1.1 纯 Canvas App 能否创建/发布 | 产品决策，不定无法动工 |
| 先定 | §1.2 `permission_policy` 的授予语义 | 契约决策，影响 Phase 5 架构 |
| 动工前 | §2.1 模板搬迁的文件集定义 | 不定则 Phase 2 必然失败 |
| 动工前 | §2.2 builtin bundle 物化的幂等与失败态 | 不定则移动端引入新的整体不可用故障模式 |
| 补章节 | §3.1 桌面端定位 / §3.2 i18n / §3.3 排期 | 整块缺失 |
| 补门 | §4 全部四条 | 门不诚实等于没门 |

---

## 1. 两条会改变产品行为的缺陷（三镜头 0/3 驳回，即无人能反驳）

### 1.1 按 V2 白纸黑字，纯 Canvas App 连**创建**都做不到

两个维度（closure、consistency）独立发现同一问题，是本次复审信号最强的一条。

V2 §12.3/L1036 驳回了上一轮评审 §5.4「放开为 0–16」的建议，坚持
「无法生成至少一个有意义 tool 时…不制造 filler tool，也不发布 App」。但：

- §10.1 把 `initial MCP authoring generates 1–16 app-specific tools`（L772）排在
  `Native create confirmation`（L775）和 `receipt`（L776）**之前**；
- 因此 authoring 返回 typed failure 不是「不发布」，而是 **整个 create 流程中止**；
- §12.2/L990-994 的外部输入只有 `app_id` 和 `user_goal`，**没有「本 App 不暴露 tool」的开关**，
  用户无法绕过。

**后果**：用户说「做个画板」，走完 template-selector、designer、builder 全部生成之后，
在 receipt 之前被 mcp-designer 的 typed failure 打回，全部工作作废。
上一轮评审 §5.4 举的三个例子（画板玩具、Phaser 小游戏、Three.js 场景）
在 V2 下不仅不能发布，是**不能创建**。

**同时存在一个不可达的死状态**：§16.2/L1363 的派生状态表保留了
`ready_without_mcp = 有 active build、无 active catalog`。但 §10.1/L782 把
`atomic build + active catalog promote` 写成同一个原子步，§16.4/L1404 又规定
promote 原子切换 build receipt 与 catalog ref 且「失败不能产生短暂空 catalog
或一半更新的 build/catalog 配对」。**不存在任何合法路径产生「有 build 无 catalog」的 App。**

实施者会照表实现 `ready_without_mcp` 的分支、UI badge 和查询投影，测出来永远为空；
而 §19.8/L1659 只测「每 App 1–16 meaningful tools」，没有任何测试能暴露这个死分支。
这是「决定只落进正文没落进结构」的反向版本——一个被正文否定的状态却留在了结构表里。

**改法（二选一，不能都留）**

**A（推荐）：采纳 0–16。**
- §1.5/L109、§12.3/L1025、§13.2/L1147、§19.8/L1659、§20 第 8 条/L1701 五处统一改为 0–16；
- §12.3/L1036 改为「无可暴露的有意义能力时，authoring 返回 `no_exposable_capability`，
  理由写入 `excluded_capabilities`，App 以 `ready_without_mcp` 正常发布」；
- §16.4/L1404 放宽为「build 与 catalog 各自原子，catalog 缺失是合法终态」；
- §19.8 增加「零 tool App 可发布且不注册 logical server」测试。

**B：坚持 1–16。**
- 删除 §16.2/L1363 的 `ready_without_mcp` 行，draft 条件改为「无 active build 或 authoring 未完成」；
- §10.1 把 MCP authoring 提前到 template-selector 之后作为**可行性预检**，
  避免在 builder 做完全部工作之后才失败；
- §17.3 create confirmation 与 §19.10 增加「因无可暴露能力而创建失败」的 UX 与测试条目。

选 B 等于产品上放弃整个 canvas 家族的独立发布能力，请明确知情后再选。

### 1.2 §14.1 把 oracle 的两类 producer 合并成同一个 ceiling，抹掉了 `permission_policy` 的授予语义

V2 §14.1/L1200 声称「实现 Claude oracle 的两类 per-tool policy producer」，
把 `tools[].permission_policy` 与 `toolPermissions` 一起收敛成
`McpPermissionCeiling{Allow, Ask, Deny}`，并在 L1228 断言
「`Allow` 只表示该 ceiling 不额外收紧，绝不等于单独授予调用权限；
没有 user/session allow 时仍按当前 permission mode 处理」。

**这对 `toolPermissions` 成立，对 `permission_policy` 不成立。**
oracle 的 `permission_policy` 产出的是 session **rules**，
`always_allow` 是真正的 allow 规则，会直接授予。

**已亲验**——这个接入点在仓库里已经存在并且是空的：

```rust
// lingxi-code/permission/src/rule.rs:188-190
/// Dynamic MCP server-policy rules (claude-code `mcpServerPolicy`). Walked
/// last (lowest citation precedence). No producer yet — latent.
McpServerPolicy,
```

它已接进 `permission/src/policy.rs:53`、`permission/src/filesystem.rs:193`、
`permission/src/shadow.rs:106,586,600,611,618`、`tui/src/bottom_pane/permissions_editor_view.rs:208`，
优先级 `rule.rs:208` 为 0（最低 citation precedence）。
**这是一个专门为该 producer 预留的具名空槽。**

而 V2 §4/L194 把现状只描述成「支持 exact FQN/server wildcard；缺 per-tool ceiling producer」，
实施者看不到这个已存在的接入点，会另做一套并行机制。

**后果**：按 V2 实现后，`always_allow` 被降级成「不收紧」，在默认 permission mode 下
仍然弹窗或拒绝，与 oracle 行为不一致；同时那个 latent 规则源永远没有 producer。

**改法**：§14.1 拆成两条独立管线——
- (a) `tools[].permission_policy` → 生成 `PermissionRuleSource::McpServerPolicy` 的
  allow/ask/deny **规则**（strictest-wins 去重后接入 `permission/src/rule.rs` 现有 walk），
  `always_allow` 明确授予；
- (b) `toolPermissions` / `org_max_permission` → `effective_max_permission` **ceiling**，只做封顶。

把 L1228 那句限定到 (b)。§4/L194 的现状列补上
「`permission/src/rule.rs:188-190` 已有 latent `McpServerPolicy` 规则来源」。

---

## 2. 两条会让实施当场失败的规格空档（0/3 驳回）

### 2.1 §9.1「逐字节搬入五个目录」——目录里 99.8% 是构建产物

**已亲验**，实测五个目录：

| family | 目录内文件数 | git tracked | 体积 |
| --- | --- | --- | --- |
| react-dom | 22 | 22 | 132K |
| canvas-2d | 25 | 25 | 152K |
| three-3d | 25 | 25 | 152K |
| **phaser-2d** | **13,014** | 26 | **263M** |
| **babylon-3d** | **20,093** | 26 | **260M** |

V2 §9.1/L617 断言这五个目录「已经同时包含 profile-managed 和 editable scaffold files，
是生产模板」——就 tracked 文件而言这句是对的（124 个文件）。
但 L627 要求「逐字节搬入」、L633 要求「搬迁前后每个文件 SHA-256 一致」，
**把「目录」当成了权威文件集，而这些目录在工作树上不是自包含的**。

**后果**：
- 照字面实施会把 33,107 个文件、523MB 的本地 `node_modules` / `dist` 打进 builtin archive；
- §6.1/L396 要求「复用仓库现有 Plugin archive/unpack 能力」，
  而 `plugin/src/mcpb.rs:16` 的 `MAX_FILES = 10_000` 会当场拒绝
  （babylon 20,093、phaser 13,014）——**已亲验**；
- 反过来，若实施者「自然地」只搬 tracked 文件，L633 那道 digest 门
  **没有可核对的基准集**，等于没有门。

**同源问题（§6.1）**：archive 含「sorted asset inventory」（L386），
但 V2 **从未定义 inventory 里有哪些文件、由什么确定**——显式清单还是遍历目录。
若是遍历目录，「确定性 builtin archive」会随开发机是否跑过 `pnpm install` 而变化，
§19.2 的 golden 测试变成薛定谔的绿。

**改法**：
1. §6.1 写死 asset inventory 的唯一真源是**显式清单**——从今天
   `apps/engine-mobile/src/local_app_runtime_profiles.rs` 的 `profile_file!` 宏清单
   一对一生成（`assets/templates/<family>/r1/inventory.json`）；
   遍历目录只用于「目录里存在清单外文件 ⇒ build 失败」的反向对账。
2. §9.1 把「逐字节搬入目录」改为「逐字节搬入 inventory 列出的 124 个文件」，
   并写死排除 `node_modules/`、`dist/`、`.vite/`、`.lingxi-build-state/`。
3. L633 的门改为「搬迁前后 inventory 清单逐条 digest 一致，
   且清单条目数与 `profile_file!` 宏清单条目数相等」——让漏搬和多搬都能红。
4. §19.2 增加两个测试：目录里放一个清单外文件 ⇒ build red；
   在有 `node_modules` 的 checkout 上 archive digest 与干净 checkout 相同。

### 2.2 builtin bundle 物化没有幂等短路，也没有失败态

V2 §6.2/L398-409 的五步全是无条件的：**没有「digest 与已 promote 的 root 相同则跳过」**，
也**没有任何一步失败时的 fallback**。§17.1/L1417 列出的 6 个 typed error
（plugin_disabled / template_unavailable / proposal_invalid / permission_ceiling /
catalog_stale / verification_unavailable）里**没有一个表示「builtin bundle 物化失败」**。

存储满、解包被系统杀死、iOS 在后台清掉 app-private cache——移动端都是常态。
任一发生，PluginManager 拿不到 builtin root，skills / agents / workflows /
template catalog 全部不注册，Local App 整个功能不可用；
而客户端没有可渲染的 typed error，UI 只能表现为「创建按钮点了没反应」。
**今天这一类故障根本不存在**——模板是 `include_bytes!` 编进二进制的常量
（`local_app_runtime_profiles.rs:18-32`）。这是 V2 新引入的故障模式。

§19.2 唯一的失败测试是「interrupted materialization 不替换 previous verified root」，
但首次安装或用户清数据后根本不存在 previous root，
**这条断言恒真、什么都不证明**。

同时：每次冷启动无条件解包并 SHA-256 校验 5 套模板加全部 skill 的成本，
§21/L1710 只说「必须由 benchmark 约束」，没有给出短路机制（另见 §4.2）。

**改法**：
1. §6.2 增加幂等短路：若 `<cache>/<archive_sha256>/` 已存在且带完整 promote marker，
   跳过解包与逐文件校验，只校验 marker。
2. 定义 `builtin_bundle_unavailable` typed error 并加进 §17.1 清单；
   明确「物化失败时已 scaffold App 仍可依 §9.6 per-App snapshot build/run/restore，
   只有 create/update/MCP revise 返回该 error」。
3. §19.2 把「interrupted materialization」拆成两个测试：
   有 previous root（不替换）与无 previous root（返回 typed error，不 panic、不半注册）。
4. §21 把「materialization 失败 = Local App 功能整体不可用」显式列为保留风险。

---

## 3. 三块整块缺失

### 3.1 桌面端

V2 全文只出现两次 "Desktop"（L420、L1449），没有任何一节讨论桌面端。但三件事同时成立：

1. 今天唯一真正跑 Plugin 的产品是**桌面端**；
2. 桌面端**完全没有 Local Apps**（`engine-desktop/src/` 中 `LocalApp` 零命中）；
3. V2 §17.4/L1449 却要求「Desktop/iOS/Android 的现有 MCP inventory 增加 managed Local App source」。

**后果**：
- **回归风险无人接**：Phase 0 要重写 discovery / agent parsing / path 语义，
  而唯一在跑第三方 plugin + hooks + LSP + MCP 的是桌面端，
  §19 没有一条桌面断言——桌面 plugin 加载可以在移动端全绿的情况下静默坏掉。
- **Phase 8 的「managed MCP inventory」在桌面端不可实现**：桌面既无
  `LocalAppsMcpTransport` 也无 App 记录，工程师照 §17.4 施工时才会发现无数据可显示，
  届时才回头做「桌面要不要装这个 builtin plugin」的产品决策——
  而那个决策会反过来推翻 §6.2 的移动端专用 materialization 设计。

**改法**：新增「§X 桌面端定位」并把结论写进 §1 已确定决策，二选一：
- **A：桌面端不装** → §17.4 删掉 Desktop，明确桌面端 MCP inventory 无 Local App source；
- **B：桌面端装** → 必须补 engine-desktop 的 Local Apps Host、runtime、WebView QA runner，
  这是另一个数量级的工作，应作为独立方案。

无论选哪个：§18 Phase 0 增加一步「桌面 plugin 加载 / hook / LSP / MCP 回归基线与快照测试」；
§19 增加 19.11 Desktop 小节，至少覆盖 discovery、agent frontmatter、
`${LINGXI_PLUGIN_ROOT}` 在 hook·MCP·LSP·monitor 四个位点的桌面端行为。
另：§17.1/L1419「iOS/Android 与 Rust bindings 同步生成」应改为
「iOS/Android/Electron 与 Rust bindings 同步生成」。

### 3.2 i18n

**已亲验**：V2 中「i18n / 文案 / 翻译 / 本地化 / localization / 多语言 / locale」
出现次数**全为 0**。

而 §17 要新增 plugin settings 页（§17.2）、create confirmation sheet（§17.3）、
MCP proposal diff/approval sheet、verification badges、managed MCP inventory（§17.4），
以及 6 个 typed error 码（§17.1/L1417）——**全部是要给用户看的字符串**。
§18 Phase 8 的 6 个条目没有一条是字符串工作，§19.10 也没有一条 locale 校验。

**已亲验的现状**：
- 真源是 `clients/translations/*.json`，iOS `Localizable.xcstrings` 与
  Android `strings.xml` 是 `generate.py` 的**生成产物**，不可手改；
- 5 个 locale（zh-Hans / zh-Hant / en / ja / ko）**各 1899 个 key，数量必须相等**——
  `generate.py:87-94` 以 `missing` / `extra` 集合差强制；
- Phase 9/L1581 要删的旧 runtime profile selector UI 有实打实的 key：
  `clients/translations/en.json:252-281` 共 **30 个** `local_apps_runtime_profile_*`
  （`..._title` 到 `..._surface_canvas`），×5 语种 = 150 条。

**后果**：
- Phase 8 交付会卡在最后一米：ja / ko / zh-Hant 需要真人翻译，这段人工排期完全没进计划；
- Phase 9 删 UI 时，只删 Swift/Kotlin 不删 json ⇒ 150 条死文案永久滞留；
  只删 zh-Hans 不删其余 4 个 locale ⇒ `generate.py` 报 extra keys 直接失败。
  V2 没给任何一侧指令。

⚠️ 修正一处：`generate.py` 校验的是 **key 集合相等**，不是译文质量。
把中文原文粘进 en/ja/ko 的值里同样能通过——所以它抓不住「没翻译」，
只抓得住「key 不齐」。而且**该门不在 CI 上**（`.github/` 中 `translations` 零命中），
所以更危险的形态是没人跑生成器、新 UI 直接硬编码中文字面量。

**改法**：
1. §17 增加「§17.5 文案与本地化」：列出每个新 Native surface 的 key 前缀
   （`local_apps_plugin_*`、`local_apps_create_confirm_*`、`local_apps_mcp_proposal_*`、
   `local_apps_verification_*`），并为 6 个 typed error 各定一条用户可读文案，
   标明是否带占位符（注意 iOS 占位符在 KEY 里、Android 在值里，常需两个 key）。
2. §18 Phase 8 增加一步：「在 `clients/translations/zh-Hans.json` 新增 key 并补齐
   其余 4 个 locale，跑 `generate.py` 重新生成 xcstrings 与 strings.xml 并提交」。
3. §18 Phase 9 step 4 补一句：「同步删除 5 个 locale 中的 30 个
   `local_apps_runtime_profile_*` key（en.json:252-281 及对应位置），跑 `generate.py` 重生成」。
4. §19.10 增加一条：「`python3 clients/translations/generate.py` 退出 0
   且打印 `OK: <n> keys, 5 locales`，重跑后生成产物无 diff」——
   判据落在输出点名的内容上，不落在退出码。

### 3.3 工作量与排期

「工作量 / 人日 / 人周 / 排期 / 估算 / 并行 / 里程碑 / 周期」在 V2 中出现次数**全为 0**。
10 个 Phase，读者无法判断这是两周还是两个季度，也无法判断哪些能并行。

**文档自身还低估了最大的一块**：§7.2 列了 27 个 skill，仓库里只有 10 个
（`skills/` 下 10 个目录 / 44 个 md / 1,773 行）；
而 Phase 2 step 2 只写「增加 Phaser/Babylon/MCP authoring skills」——
**Phaser 和 Babylon 恰恰是已经存在的两个**（`skills/phaser-2d-local-app/`、
`skills/babylon-3d-local-app/`），它们属于 step 1 的搬迁项。

**后果**：Phase 2 在文档里读起来是「搬文件 + 补几个 skill」的一天量级工作，
实际是 17 个新 skill + 7 个 agent + 3 个 workflow 的提示工程——
本方案最大的单块工作量，而 Phase 3/4/6 全部依赖它的产物
（template-selector、mcp-designer、三个 workflow），排期必被这块串行阻塞。

**改法**：§18 每个 Phase 后加两行——「规模：新增/改动文件数量级 + 提示层字数量级 + 人周区间」
和「依赖：可与 Phase N 并行 / 必须串行于 Phase M」。至少标清三条真实关键路径：
Phase 2 是最大单块且阻塞 3/4/6；Phase 5（MCP DTO + ceiling）与 Phase 2 无依赖可并行；
Phase 8 依赖 Phase 3/6 的 protocol 定型。同时把 Phase 2 step 2 改为
「新增 17 个 skill（明列名字）」并删掉「Phaser/Babylon」。

---

## 4. 门的诚实性

判据原则：**一个门只有在输出点名了具体的东西时才算证据。**
退出码为 0、测试数量、无阈值的 benchmark 都不是证据。绿门必须先证明自己能红。

### 4.1 品牌防漏门全文只出现一次，无 Phase 步骤、无验证条目

上一轮评审 §2 D1「风险提示」明确要求把 brand-leak 门从 worktree 摘到 main，
理由是「今天往 main 写 `.claude-plugin` 不会有任何东西报错——品牌回流是静默的」。

V2 用 §8.5/L611 一行正文回应（「品牌 scanner 禁止产品 surface 回流
`.claude-plugin`/`CLAUDE_PLUGIN_*`」），但：

- Phase -1 step 7（L1471）建的是 **component literal 门**，不是品牌门；
- Phase 0 step 7（L1483）是「增加品牌归一化 oracle fixtures」——那是给 oracle
  对比用的 normalization，**不是防漏门**；
- §19.3（L1608）只有「production literal scanner planted-positive test」，
  同样是 component literal scanner。

**整份 §18 Phase 列表和 §19 验证计划里，「品牌 scanner」零条目。**
而 V2 的措辞「品牌 scanner 禁止…」预设该门已存在——它不存在，
只在 `.claude/worktrees/agent-namespace-plan-a/lingxi-code/scripts/`。

**后果**：Phase 0 要按 oracle 逐字段对齐 manifest schema、component discovery、
path semantics、agent frontmatter 解析，是整个方案里品牌回流风险最高的一步。
被 V2 取代的初版方案本身就是这样写成 `.claude-plugin/plugin.json` 和
`${CLAUDE_PLUGIN_ROOT}` 的。没有落进 Phase 的门不会被实施；
没有落进 §19 的门即使实施了也可能是恒绿的空门。
§20 第 4 条要求「LingXi 品牌路径和环境变量贯穿产品」，但没有任何可执行的东西检查它。

**改法**：
1. Phase 0 增加一步（建议排在 step 1 之前）：把 `check_brand_leaks.py` /
   `check-brand-leaks.sh` / `brand_leak_baseline.txt` 从 worktree 摘到
   `lingxi-code/scripts/` 并接入 CI；`.claude-plugin` / `CLAUDE_PLUGIN_*` /
   `CLAUDE_PROJECT_DIR` 写入 needle 集合；`plugins/lingxi-local-app/` 与
   oracle fixtures 目录写入豁免清单。
2. §19.3 增加两条：「品牌 scanner planted-positive test（往 `apps/engine-mobile/src/`
   埋一个 `.claude-plugin` 字面量，门必须红且**输出点名该文件行**）」；
   「品牌泄漏计数基线，基线上升即失败」。
3. §8.5/L611 措辞从「品牌 scanner 禁止…」改为「**新增**品牌 scanner 并接入 CI，禁止…」。

### 4.2 §19.9 的 scaling 条目一个阈值都没有，且 §21 委托给了不存在的门

- L1672「30、100 个 published App 启动成本 benchmark」**没有任何数字上限**——
  无阈值的 benchmark 只会 print 数字，永远不会红；
- L1678「maximum 128 expanded tools 的 prompt/tool-search budget」**没有 token 上限**；
- 反过来 L1673「physical hub 数量恒为 1」和 L1674「无 N 个进程/socket」
  **在基线 `5448e93e3` 上已经成立**，写第一行代码前就是绿的；
- §21/L1710 声称 binary size / startup 风险「必须由 bundle size/startup benchmark 约束」，
  但 §19.2 Builtin bundle 一节（L1597-1604）**没有任何 bundle size 或 startup 条目**——
  被委托的那个门不存在，形成闭环空引用。

同理 §20 第 3 条「移动端使用现有 PluginManager，不存在第二套 Plugin loader」
在 §19 里也没有对应验证——§19.2 的「mobile only loads builtin source」
验的是 source 类型，不是「只有一个 loader 实现」。

**改法**：
1. 给出数字并记基线：cold-start 在 0 / 30 / 100 published App 三档的 p50/p95 增量上限（ms），
   超限非 0 退出并**输出点名超限的那一档**；
2. §19.2 补两条：「builtin archive 解包后体积 ≤ N MB、编入二进制的 archive 字节 ≤ M MB，
   超阈值 CI 失败」「冷启动 materialization + PluginManager 注册耗时基线，
   与接入前基线对比，回归超过 X% 失败」，并挂到 Phase 1 完成条件上；
3. 128 expanded tools 的 tool-definition token 上限给出具体数字，超限即失败；
4. L1673 改成在基线上**不成立**的断言，例如「注册 100 个 logical server 后
   physical transport 实例数仍为 1」，并配一个「故意每 App 建一个 transport ⇒ 必须失败」的反向用例；
5. §20 第 3 条改为可验证表述：「除 `PluginManager` 外，engine-mobile 不存在
   第二个 plugin materialization 入口」，并在 §19.3 scanner 上加对应 planted-positive 断言；
6. §21/L1710 从「保留风险」改写为指向上述门的说明。

### 4.3 防回归 scanner 的 needle 集合来源未定义，FQN 可绕过，计数基线消失

§8.5/L610「Scanner 自带 planted-positive test，不能只断言当前仓库 exit 0」方向正确，
但**没说 needle 集合从哪来**。planted-positive 只能证明 scanner 命中**已经在它列表里的**名字；
若列表是静态数组，Plugin 新增或重命名一个 component 后 host 重新硬编码它，scanner 静默放行。

§8.5/L582 的豁免写成「只有 composition module 可以出现三个 Local App workflow **basename**」——
只说 basename，没覆盖 §11.1/L835 的 namespaced FQN 形式
`lingxi-local-app:local-app-build`。host 模块拼 FQN 或拆串即可躲过按 basename 的 grep。

上一轮评审明确要求的「计数基线入 CI」在 V2 里完全消失（全文零次「基线」）。

**改法**：§8.5 写死两件事——
(1) needle 集合**在扫描时从 Plugin manifest inventory 派生**（唯一真源），不得手写数组；
(2) 扫描面是 **deny-by-default 的目录枚举**，不是文件白名单。
§19.3 把 planted-positive 拆成三个必须红的反向用例：
(a) 往 manifest fixture 新增一个 component，把名字种进 `local_apps_host.rs` ⇒ 必须失败（证明 needle 不静态）；
(b) 种 namespaced FQN 而非 basename ⇒ 必须失败；
(c) 新建一个未被任何 allowlist 提到的 engine-mobile 模块并种字面量 ⇒ 必须失败。
再补：scanner allowlist 条目数入 CI 基线，条目增加需显式改基线。

### 4.4 §5.5 把 plugin workflow 默认扫描收窄到 `*.js`，与仓库自身不一致

V2 §5.5/L328 把 plugin workflow 默认扫描定为 `<plugin-root>/workflows/*.js`，
比初版方案的 `*.js` / `*.mjs` / `*.ts` 收窄。而 §5 自称是通用「LingXi Plugin 契约」。

**后果**：同一个 `foo.mjs`，放在 `~/.lingxi/workflows/` 能被解析
（`tools/workflow/src/lib.rs:204-207`），放进 plugin 的 `workflows/` 目录却**静默不可见**——
没有报错、没有诊断，只是名字不出现在 registry 里。且 §5.3/L275 规定 `workflows` 的
custom path「替换 default scan」，作者也无法靠声明补回来。

**改法**：改回与 `tools/workflow/src/lib.rs:52` 的 `WORKFLOW_EXTENSIONS` 一致；
若确实要只收 `.js`，需说明它是有意 divergence，并加一条
「plugin workflows 目录里出现 `.mjs`/`.ts` 时发诊断」的规则，避免静默丢失。

---

## 5. 被驳回、但留下了更准确版本的三条

以下三条的**原始表述**被三镜头判为过强并驳回，但残留问题为真，且比原版更值得注意。

### 5.1 合并 workflow 会孤儿化现有的 Canvas 硬门测试

Canvas 硬门的反向覆盖**今天已经存在**于 `lingxi-code/tools/workflow/src/builtins.rs`：
`canvas_zero_surfaces_blocks_even_when_render_and_motion_pass`（:1944，断言点名 :1978/:1993 的 finding 文本）、
`a_missing_or_failed_motion_check_blocks_and_buys_a_repair_round`（:2509）、
`local_canvas_build_is_model_invocable_and_pins_its_contract`（:2243）。

但它们**全部通过 `drive_local_workflow("local-canvas-build", …)` 按名进入**
（builtins.rs:1950、:2243），而 §8.3/L581 要删掉这个 workflow 名。
**合并时必须显式改为「policy=canvas_* 的统一 build workflow」入口，否则这些门会静默消失。**

另有一个真缺口：**没有任何测试把 `render_check.status="not_applicable"` 喂给 canvas workflow
并断言必须红**——`:2263` 只是钉 schema 描述**字符串**，是文案 pin 不是行为门。

**改法**：§19.5 把 L1627/L1628 从名词短语改成拒绝句式，例如
「Canvas policy 下 `render_check.status=not_applicable`、`canvas_surfaces=0`、
`frames_captured=0`、`motion_check` 缺失或 `frames_compared<2`
**各自产生点名对应文本的 blocking finding**；测试断言 finding 文本，不断言退出码」。
并在 §8.3 或 §19.5 加一句迁移备注：现有三个测试的入口必须随 workflow 合并一起改。

### 5.2 Phase -1 按 §8.1 的定义无法执行

§8.1/L556-563 把 `LocalAppPluginBinding` 定义为持有 `plugin_id: PluginId`
和三个**从 `PluginManager` 解析来的** `WorkflowHandle`，
而 engine-mobile 接入 PluginManager 排在 Phase 1 step 3（L1491），
`PluginComponents` 增加 workflows 排在 Phase 0 step 2（L1478）。
**按 §8.1 的字面定义，Phase -1 无法构造这个 adapter。**

同时 Phase -1 step 1（合并两个 workflow registration）与 step 4（删除 Profile→workflow map）
依赖的统一 workflow 内容排在 Phase 4。两个脚本实质不同
（`local_app_canvas_workflow.js` 390 行、captured/frame/motion 关键词 40 处 vs
`local_app_build_workflow.js` 340 行、15 处），Phase -1 无法在不引入 Phase 4 内容的前提下完成。

（镜头正确驳回了原 finding 的「功能中断」结论：该功能未发布、Phase 9 才做一次性切换，
中间态不对外；skill 注册也不会断——`lib.rs:439-449` 的 10 个名字与
`skill-api/src/builtin/bundled.rs` 的 `BUILTIN_MOBILE` 同集，
Phase -1 可以改成迭代 `mobile_skill_registry()`，既不出现名字也不改变注册结果。）

**改法**：
1. §18 Phase -1 开头加一句限定：「Phase -1 只在现有 builtin 注册上解耦，
   不搬文件、不引入 plugin 依赖」；
2. §8.1 补一句：该 adapter 在 Phase -1 先对现有 `BuiltinWorkflowRegistry`
   （`tools/workflow/src/builtins.rs:69-70`）解析 handle，Phase 1 之后改为对 PluginManager 解析——
   即 L563 的「已经由 PluginManager 验证、namespaced、注册」是**终态约束**而非 Phase -1 前置条件；
3. L1465 / L1468 加限定：「保留两个 workflow 的既有注册直到 Phase 4 的统一分支与
   verification policies 落地，Phase -1 只把名字字面量移出
   `workflow_support.rs` / `local_apps_host.rs` / `lib.rs`」——这与 L1579 一致。

### 5.3 Host verified template catalog API 有名无实

V2 在四处提到它——L155（`LocalAppPluginBinding` exposes verified template catalog）、
L539（§7.3 template-selector 允许的 Host 能力）、L768（§10.1 `template-selector reads verified catalog`）、
L1511（Phase 3 step 1「Host verified template catalog API」）——
**但全文没有任何一处定义它的名字、入参或返回体**（对
`LocalAppTemplateCatalog|LocalAppValidateTemplateSelection` 零命中）。

而 §11.1/L873-876 的 `host_context.template_catalog` 只有 `digest` 和 `available_template_ids`，
§9.2/L637-651 的语义字段（summary / recommendedFor / notFor / surface）必须走那个未定义的 API。
读者容易误以为 `host_context` 就是全部通道，而 args 通道又被 §11.1/L855-864 禁止。

同时 V2 丢掉了初版方案的一条约束：初版
（`LOCAL-APP-PLUGIN-IMPLEMENTATION-PLAN.md:746`）要求
`app_id + template_id + catalog_digest + reason` 且 Host 复算 digest、校验 App 未 scaffold。
V2 §9.5 只说「Host 将 Agent 输出转换为 `ValidatedTemplateSelection`」，
**没有说 Agent 的输出经哪个 seam 回到 Host，也没有 anti-stale 约定**。

**改法**：在 §9.4 之后加一小节给出该 API 的入参与返回体
（仅 `available=true` 项，含 `templateId` / `surface` / `summary` / `recommendedFor` / `notFor`，
**排除 path / family / revision / digest**——与 L660「Agent 不能返回 digest 权威值」一致），
并明确 Agent 输出的回传 seam。§19.4 增加一条：
「`host_context` 只含 digest + available_template_ids，语义字段只能经该 API 取得；
stale catalog 被拒绝」。

⚠️ 注意：不要照搬初版让 **Agent 回传 `catalog_digest`** —— 那与 L660 和 §9.5 由 Host 复算的模型冲突。
anti-stale 应由 Host 侧比对 `host_context.template_catalog.digest` 与 API 返回值实现。

---

## 6. 已完全驳回的 finding（无需处理，列出以免重复讨论）

以下候选在三镜头下被驳回且无有效残留，不建议按它们改动 V2：

1. §15.1/§15.2 的 per-App registry key 与现有 transport 结构不兼容 —— 兼容。
2. §1.6「清理预发布数据」缺 §19 覆盖 —— 该动作是一次性切换，非持续门。
3. Q1 代价「builtin-only 拿不到不发版改 prompt」被静默丢弃 —— §1.3 的 L74+L79
   已用机制语言关闭该决策；仅缺一句后果化表述，属可选可读性改进。
   （若要补，最小改法：在 §1.3 末尾加「提示层随客户端版本发布；若后续需要独立更新，
   路径是给 `BuiltinPluginBundle` 增加签名的 OTA source provider，复用 §6.2 的
   materialization 与 digest verification，而不是改走 marketplace/install」。）
4. Phase -1→Phase 4 期间 Local App 功能中断 —— 不成立（见 §5.2）。
5. §19.5 的 Canvas 硬门完全没有反向用例 —— 部分不成立（见 §5.1）。
6. §20 第 12 条「Rust workspace 测试全部通过」是恒真门 —— 不成立。
7. §15.3 的暴露触发是无判定者的启发式 + LRU 撤掉在用 tool —— 不成立。
8. §12.5 改的 FlowStep 是 App 侧运行时 wire 契约 —— 不成立。
9. 回滚与中止策略整块缺失 —— 部分与 §5.2 重合，其余不成立。
10. template-selector 语义字段通道缺失（原始 P1 表述）—— 降级为 §5.3 的规格空档。

---

## 7. 给复审者的重点

请优先质疑以下三条，它们是本文结论的承重点：

1. **§1.1 的结论「纯 Canvas App 连创建都做不到」。**
   反驳方式：指出 V2 中任何一处允许 create 在 MCP authoring 失败后继续、
   或允许用户声明「本 App 不暴露 tool」的机制。若存在，§1.1 应撤回。
2. **§1.2 的结论「`permission_policy` 是规则不是天花板」。**
   反驳方式：给出 oracle 中 `permission_policy` 仅作封顶、不授予的证据。
   注意 `permission/src/rule.rs:188-190` 的 latent slot 注释明确写的是
   「Dynamic MCP server-policy **rules**」。
3. **§2.1 的文件集结论。** 反驳方式：指出 V2 中已定义 asset inventory 来源的段落。
   注意目录实测数据（phaser 13,014 / babylon 20,093 个文件，523MB）
   与 `mcpb.rs:16 MAX_FILES = 10_000` 均为本评审者亲验。

同时请注意本文 **§0 方法与覆盖度**声明的限制：45 条候选中 21 条未经验证，
不在本文结论内；未验证的集中在 gates / closure / new-risk 三个维度。
