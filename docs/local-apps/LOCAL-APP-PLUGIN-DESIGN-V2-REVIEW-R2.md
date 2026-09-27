# LOCAL-APP-PLUGIN-DESIGN-V2 第二轮复审（R2）

日期：2026-08-29
审查对象：`docs/local-apps/LOCAL-APP-PLUGIN-DESIGN-V2.md`（codex 第四稿，2429 行，文件时间 2026-08-29 03:40）
代码基线：`5448e93e3`

验证状态：
- 第一批 11 条（我通读后提出）已完成三镜头对抗验证（code / doc / fix，各自独立尝试反驳；≥2/3 反驳即撤回）。
  结果：**5 条成立（全部收窄）、4 条被驳回但留下残留、2 条完全撤回**。
- 第二批 36 条（4 个 completeness critic 在我未覆盖的章节扫出）已完成同样的对抗验证（93 agent）：
  **30 条成立（多数收窄了严重度或范围）、6 条撤回**，见 §6。

## 0. 方法与覆盖面

- 通读全文 2429 行；对文中每个带 `file:line` 或数字的断言到源码核对。
- 每条 finding 交给三个独立 agent 反驳：code 镜头核对每个 file:line 在 HEAD 是否字面成立并搜索遗漏的代码；
  doc 镜头核对文档是否已在别处解决、是否与 §1 已定决策冲突；fix 镜头假设事实成立，攻击建议本身。
- 被驳回的条目**保留在 §4/§5**，写明驳回理由与 file:line——它们是我读错的地方，codex 复审时不必重走。

## 1. 复核通过的断言（新版改回的数字全部正确）

| 断言 | 实测 |
| --- | --- |
| 模板 tracked 124 / `profile_file!` 112 / 孤儿 12 | `git ls-files local-apps/templates/runtime-profiles` = 124；宏调用 112；孤儿正是 canvas-2d/three-3d/phaser-2d/babylon-3d 各自的 `app/screens/detail-screen.jsx`、`app/screens/home-screen.jsx`、`src/stores/app-store.js` |
| 112 个 scaffold 源文件约 376 KiB | 376,131 bytes |
| `CLIENT_PROTOCOL_VERSION` 当前 `9.0.0` | `client-protocol/src/version.rs:77` |
| `APPS_SCHEMA_VERSION` 当前 2 | `local-apps/src/types.rs:13` |
| 5 locale 各 1894 key | en/ja/ko/zh-Hans/zh-Hant 均 1894（上一轮报告写的 1899 是错的） |
| `minSdk=26` | `clients/android/app/build.gradle.kts:19` |
| `agent_validation.rs` 对 `permissionMode`/`hooks` 整 plugin 拒绝 | `plugin/src/agent_validation.rs:40-43` 返回 `Err` |
| `host.rs:10680,10751` 的 `"create-local-app"` | 是测试断言（`expected_names`），不是生产耦合 |
| 10 个 Local App skill 只在移动端 | `skill-api/src/builtin/bundled.rs:39` `BUILTIN_MOBILE`；桌面 `BUILTIN_DESKTOP` 只有 `claude-api`，§1.8 成立 |
| receipt 10 分钟 TTL、recovery journal 已存在 | `local_apps_host.rs:45 RUNTIME_PROFILE_RECEIPT_TTL`、`:139/:149 Pending*Receipt`、`storage.rs:140 ScaffoldRecoveryJournal` |
| `LATEST_PROTOCOL_VERSION = "2025-11-25"` | `mcp/src/initialize_params.rs:9` |
| 上一轮 §1.7 翻转的残留 | `0–16`、`ready_without_mcp`、`no_exposable_capability` 全文零命中，翻转干净 |
| Monitor substrate 存在（我第一稿说不存在，错） | `tools/task/src/monitor.rs:198 MonitorTool`、`tasks/src/handlers/monitor.rs:21,317-326`（`bypass_with_audit("monitor_task")` 即 unsandboxed runner）、`platform-api/src/task_registry.rs:42 MonitorRegistration`、`permission/src/policy.rs:559-565` Monitor→Bash 规则门 |
| libssh2 已在移动端（我第一稿说是新引入，错） | `tools/git-mobile/Cargo.toml:26-30` git2 features 含 `"ssh"`（`adcce399e`，2026-06-13）；iOS xcframework 与 Android `.so` 里已有 `libssh2_*` 符号 |

## 2. 成立的 finding（对抗验证后收窄）

### B. §13.2 的单 tool 上限可以单独吃掉整个 §19.0 exposure 预算，且没有章节定义超限时的运行时行为 — 3/3 成立

**证据（文档内部）**：§13.2 允许 description ≤ 1000 字符（≈250 token）、input/output schema 各 ≤ 32 KiB、单 App 16 tools；§19.0 `expanded_tool_definitions_max_tokens = 16384` 对应 §19.9「128 expanded tools」= 8 servers × 16 tools ⇒ **128 token/tool**。一条顶格 description 已超过每 tool 份额；一个顶格 schema（≈8k token）两个就把预算吃完。

**收窄（对抗验证指出）**：
- 撤回「100 server × 1 MiB vs 16 MiB heap」的推算——§15.2/§16.1 已定 connection 只持 app ID + digest、catalog 落盘在 `<app-data>/mcp/catalogs/<sha>.json`；不需要「决定是否惰性加载」。
- 撤回「抬高预算」的选项——§21:2399 已把 16,384 定为第一版硬限。

**真正缺的**：
1. §12.3/§13.2 没有 **per-App tool-definition token 预算**，也没写 exposed set 超过 16,384 时运行时做什么（拒绝 promote？拒绝 expose？淘汰？）。判据落在 §12.3 质量门：authoring 时按 `wire.rs` 的 2.5 chars/token 规则估算，超预算的 proposal 不过门，并给出每 tool 贡献排序（§19.9 已要求这个输出，但没有对应的生产门）。
2. §19.0「retained heap」没有测量对象：registry 在 connect 时 eager 保存 `Connected{tools}`（`mcp/src/registry.rs:957-959`，`connection.rs:151`），与 §15.2「connection 只持 app ID、digest、connection ID」矛盾。要么写明 Local App logical server 不走 eager `Connected.tools` 保留，要么把 heap 门定义为「100 个 registered server 的 registry + hub 状态」并给出 fixture 的每 tool 规模。
3. 16,384 的计数方法（tokenizer / 估算规则）未写，门没有确定性红条件（sweep 也独立扫出此点）。

### E. 收据消费后每一轮 repair 都会作废 approval，文档没说这是不是有意的 — 2/3 成立

**证据（文档内部）**：§16.3「source/proposal/template/dependency/permission 任一 digest 变化都会使 approval 失效并要求新 receipt」；§10.1 receipt 在 build/smoke/use-test/MCP QA **之前**消费；§11.4 每个 blocking finding 消耗一次 repair round，而 repair 就是 source 编辑。

**收窄（对抗验证指出）**：
- 我原来的例子「修 tool description 不该重新确认」不成立：description 在 `McpToolDefinitionDto` 里（§13.1:1485），`tools/list` 直接序列化，用户在 §17.3 确认单上看到的就是它——description 变了**应该**重新确认。
- 「output binding 修复」也不改 `proposal_sha256`：execution binding 归 `catalog_sha256`（§16.1:1770），不在 Agent 提议面。
- 因此把 receipt 绑到 `tool_surface_sha256` 的建议**撤回**。

**真正缺的**：§16.3 把 **source digest** 绑进 approval，等于「receipt → build → QA 发现 blocking finding → builder 改一行源码 → approval 失效 → 用户再确认一次」。这与 §16.3 自己「同一 candidate 的 deterministic build/QA retry 不要求重复确认」的意图相反。文档必须明确二选一：
- (a) repair round 内的 source 变化**不**作废 approval（approval 绑 design spec digest + template + dependency + tool surface + ceiling，不绑 source bytes；source 只进 build identity）；或
- (b) 每轮 repair 都重新确认，并把这个 UX 写进 §17.3/§21。
另外 §16.1 要写明 `tool_surface_sha256` **包含** description（否则 description 改了既不通知 LLM 也不重新确认）。

### F. Skill listing 的截断规则在迁移后变化，且没有门 — 3/3 成立（量化收窄）

**证据**：`orchestrator/src/prompt/skill_listing.rs:32`「bundled skills are never truncated」，`:182-219` 超预算时非 bundled skill 平分剩余字符、`max_desc_len < 20` 时退化为只剩名字；今天 10 个 Local App skill 是 `CommandSource::Bundled`（`apps/engine-mobile/src/lib.rs:432`）；迁移后是 plugin 来源、27 个、带 `lingxi-local-app:` 前缀。

**收窄（对抗验证量化）**：
- 移动端默认模型 `claude-sonnet-5`（`lib.rs:146-147`，1M 窗口 ⇒ 40,000 字符预算）：**默认路径不截断**。
- 128k 模型（预算 5120/5242 字符）：27 条描述被截到 ~135-140 字符，`create-local-app` 的 185 字符描述丢尾巴——**中段截断，不是只剩名字**。
- 只剩名字的退化要 ≤~40k 窗口或与用户/其它 plugin skill 共享预算才出现。
- 让 builtin plugin skill 算 `is_bundled` 会偏离 oracle（`isBundled: source === "bundled"`）——**该建议撤回**。

**真正缺的**：
1. §7.2.1 或 §8.2 加一句：plugin 来源 skill 按 oracle 可截断，不为 `PluginSource::BuiltIn` 开特例。
2. §19.1/§19.2 加 bundle-validation 门：对 27 个**发现的**条目（带命名空间名）在 200k 默认预算（8000 字符）下跑 `format_within_budget`，断言每条完整描述且留出显式余量（当前最坏 239 字符）；packer 对 SKILL.md frontmatter description 加宽度上限，防止新增 skill 把 turn-0 顶爆。
3. 可选：port oracle 2.1.251 的 over-budget warning + `skillListingBudgetFraction` 设置（port 里没有）。

### G. §6.4「复用现有 `settings.enabledPlugins`，bare name 做 key」——读和写两侧都是新路径 — 3/3 成立

**证据**：`plugin/src/discovery.rs:274-278`「Marketplace-qualified entries only … `continue`」；`discover_effective_plugins`（`:415-435`）按 `installed_plugins.json` 的 identifier 查，builtin 不在记录里；写侧 `apps/cli/src/commands/plugin_settings.rs:236-251` `resolve_id` 拒绝没有 `name@*` 兄弟的 bare name（"Use plugin@marketplace format"）。

**收窄（对抗验证指出）**：解析不应塞进 `register_verified_builtin`——§6.2:533 的签名没有 enabled 输入，且现有先例把 enabled-set 解析放在 composition root（engine-desktop `discover_plugin_set`，`lib.rs:4825-4848`：`load_enabled_plugins` → `discover_effective_plugins` → 才 `PluginManager::enable`；`enable` 本身不读 settings，`manager.rs:475-505`）。

**建议**：§6.4 改为「复用 settings map 与 `PluginState::{Loaded,Disabled}`；bare-key 的读（engine-mobile composition root，或 `plugin::discovery` 里一个接受 bare identifier 的纯 helper，复用 `discovery.rs:424-427` 的三行表达式）与写（移动端 toggle）都是新路径」。§19.2 的测试改成**双向**：显式 `true` + manifest `defaultEnabled:false` ⇒ Loaded；显式 `false` ⇒ 以 `PluginState::Disabled` 存在（不是缺席）；key 缺失 ⇒ 用 default。只测 false 会因为走了跳过 bare-key 的旧 resolver「什么都没加载」而假绿。

### I. `operator`（和 `tester`）没有调用方，且移动端没有 Agent tool — 3/3 成立

**证据**：§11.1/§11.3/§12.2 三个 workflow 只点名 template-selector/designer/builder/verifier/mcp-designer；§11.3 其实也没点名 tester，只有 L1308 一句角色描述。`engine-mobile` 不链接 `tool-agent`（`apps/engine-mobile/Cargo.toml` 无此依赖；`host.rs:3770-3772` 注明 AgentTool 在移动端不构建 invoker），所以「用户经 Agent tool 调用」这条路在本 Plugin 的唯一目标平台上不存在。

**收窄**：不建议删（§1.2 L86 已把 operator 列为既定组件）；§7.3:741 已有工具边界，不必再补权限说明。

**建议**：§7.3 或 §11 加一句：operator/tester 由哪个 workflow 以 `agent(prompt, {agentType: "lingxi-local-app:operator"})` 生成（`tasks/src/handlers/local_workflow.rs:1259` 是现有入口）——例如 use-test workflow 的 runtime-operation step；并写明与 `local-app-use` router skill 的分工（§7.2.1:695 vs §7.3:722）。若最终删除，同步改 §18:1998/2007 的计数。

## 3. 被驳回但留下残留的条目

### A. 「theme/monitor substrate 不存在」被驳回；Phase 0 拆分作为排期判断保留 — 2/3 驳回

**我错在哪**：grep 扫了 `plugin/ orchestrator/ hooks/ tui/ settings/`（`settings/src` 根本不存在）而 substrate 在 `tasks/`、`tools/task/`、`traits/`、`permission/`。见 §1 表末两行。

**残留**：
- Monitor：runner/权限门/生命周期/通知都在；**新**的只是 plugin 侧薄层——`monitors.json` 发现、`armedMonitorKeys` 去重、`always`/`on-skill-invoke` 门、workspace-trust 跳过（oracle 里约 10 行）。§5.6:440-441 措辞应点名 substrate（`MonitorRegistration` → `spawn_monitor` → `monitor_ws`），不要改成「新建」——那会引导实现者造第二套 runner。
- Theme：`tui-core/src/theme.rs:47-60` 是封闭 6 值枚举，没有 custom/plugin theme registry（oracle 有 `customThemeBases/pluginThemes/parseCustomThemeRef`）。§5.6:438 的准确措辞是「扩展现有封闭枚举以接受 plugin 只读 themes」。
- **排期拆分仍成立**：Phase 1「依赖：Phase 0」（:1985）卡在整个 Phase 0，而 §5.6:448、§6.2:556-557、§6.3:575 都说本 Plugin 不需要 themes/monitors。建议：Phase 1 改为「依赖：Phase 0a」；0b（manifest-less/skills-directory plugin、themes、monitors、LSP env、**以及 metadata 透传**——§5.2 最小子集和 §7.1 都没声明它）仍是 §20 完成的必要条件；Phase 0 完成条件（:1966）按 0a（skills/agents/workflows fixtures）/0b（theme/monitor/manifest-less fixtures）拆开。
- **不要缩 §19.11**：§1.8:162-164 与 §20.13 要求每个 `plugin/` 改动都有可红的桌面门；改为按 0a/0b 给 §19.11 的条目打标签。

### C. 「deferral 已经解决上下文成本，8-server LRU 多余」被驳回；「已调用的 server 会被淘汰」保留 — 2/3 驳回

**我错在哪**：deferral 只在 Anthropic 系 profile 上生效——`orchestrator/src/conversation.rs:771-790` 对含 `haiku` 的模型和非 anthropic/bedrock/vertex-claude/foundry 的 profile 返回 false，`tool-api/src/wire.rs:258-260` 随即直接把全部 schema 内联。移动端是多 provider（用户确认的 divergence：`host.rs:1004-1021` OpenAI、`workflow_support.rs:1900` DeepSeek），在这些路由上 §15.3 的 8-server 上限与 §19.0 的 token 门是**唯一**的内联 schema 上界。另外 ToolSearch 命中的 tool 下一请求会以完整 schema 回到 wire（`wire.rs:270-289`），deferred 名单本身也经 `deferred_tools_delta` reminder 进上下文（`reminders.rs:1888-1915`）。结论：保留 8-server cap、LRU、pinning 与 16,384 门。

**残留（成立）**：§15.3:1701 的 pinned 定义是「用户固定」，:1702「当前会话已经调用过该 App tool」只是 exposure **触发条件**不是 pin；:1707 淘汰只豁免 in-flight/pinned。所以模型调用过的 server 在 8 个更新的 exposure 之后会被淘汰，下一次调用得到的是 orchestrator 合成的 `<tool_use_error>No such tool available</tool_use_error>` tool_result（`orchestrator/src/streaming_executor.rs:768-784`，**不是** §13.5 的 protocol error——我原文写错层）。文档应写明：模型的一次 tool call 是否算 LRU「使用」/是否自动 pin 到会话结束；以及调用已淘汰 server 时模型看到的结果与恢复路径（§15.3:1700 的 LocalAppList/Get 重新 expose）。

### H. 「解析顺序应为 plugin → builtin → saved」被驳回；oracle 顺序未写明的缺口保留 — 2/3 驳回

**我错在哪**：oracle 2.1.251 的 plugin workflow 存为 `name = ${pluginName}:${meta.name}`，resolver 是对**一个合并列表**做 exact-name `find`，没有按 `:` 拆到 plugin root 的步骤；合并顺序是 **saved > plugin > builtin**（saved 按 `meta.name` 键，不按文件名），且只要覆盖脚本能解析就允许覆盖。我建议的顺序是反的，写进去会违反 §1.1:61 的 oracle 对齐。`sanitize_workflow_name` 只在 save 路径（`lib.rs:516`、`workflows_view.rs:950`）被调用，不在解析路径上。

**残留（成立）**：LingXi 现有 resolver 是 builtin-first（`tools/workflow/src/lib.rs:250-251,267-276`）且没有 plugin 层。Phase 0 第 5 步「Plugin workflow namespacing、resolver」隐含两件没写出来的事：(1) 把优先级翻成 oracle 的 saved > plugin > builtin；(2) saved 查找从文件名改为 `meta.name`。文档应把这两点写进 §5.5 或 Phase 0 第 5 步，否则实现者会在现有 builtin-first 上加一层，与 :1966 的 oracle 完成条件冲突。Local App 产品路径本身不受影响（§8.1 用 resolved handle）。

### ALT1. 「embedded root 取代磁盘物化」被驳回；「为什么必须是磁盘」未写明 + iOS 可达性未确认保留 — 3/3 驳回

**我错在哪**：磁盘路径不只是 PluginManager 的需要。Skill tool 给模型的 prompt 前缀是 `Base directory for this skill: <path>`（`tools/skill/src/skill.rs:1040-1047`）并替换 `${LINGXI_SKILL_DIR}`（`:976-984`），模型随后用 Read tool 读 `references/*.md`（真实 fs，`tools/file/src/read.rs:2191`）；9/10 个现有 skill 第一句就是「先读 references/router.md」。今天「从字节服务」= 把 references 内联进 skill body（`skill-api/src/builtin/mod.rs:47-58`）——正是 §7.2.1:681-684 与 Phase 2 第 2 步要**删掉**的漂移源。workflow 引擎也按路径 `std::fs::read`（`tools/workflow/src/lib.rs:971`），resume 以 `script_path` 为 provenance（`workflow_support.rs:180-265`）。§21 的 OTA 也是复用 §6.2。所以 embedded root 需要在 discovery、manager、command-api、workflow 引擎、Skill tool、Read tool 六处开 VFS 臂——比 §6.2 差。

**残留（成立）**：
1. 文档从未在一处写出「为什么必须是磁盘根」——§6.2 或 §6.3 加一段，点名上面的 Skill tool / Read tool / workflow scriptPath 三个路径契约。
2. **iOS 可达性未确认**：模型的 Read tool 经 `platforms/common/src/guest_fs.rs:246 translate_model_path` 按 guest mount table 翻译路径。§6.2 选的 Application Support 目录若不在 guest mount table 里，模型在 iOS 上读不到 `references/*.md`，file-backed reference 的整个理由在 iOS 上落空。这必须在 Phase 1 用真机验证，并作为 §19.2 的一条门（模型侧 `Read <skill_root>/references/router.md` 成功）。

## 4. 完全撤回的条目（我的错误，记录以免重踩）

### D. 「plugin crate 把 libssh2 第一次拉进移动端」— 3/3 驳回

`tools/git-mobile/Cargo.toml:26-30` 的 git2 features 是 `["vendored-libgit2", "https", "ssh"]`——我看的是被 `head` 截断的 grep 输出，只见前两行。`tool-git-mobile` 是 `engine-mobile` 的无条件依赖（`Cargo.toml:239`），`cargo tree -p engine-mobile --features uniffi -i libssh2-sys` 在 iOS/Android target 上都已命中；`clients/ios/Frameworks/LingxiCodeFFI.xcframework` 两个 slice 各含 ~600 个 `_libssh2_*` 符号。`tempfile` 也早经 `sandbox` 进来。唯一新增的是纯 Rust 的 `zip`（`default-features=false, deflate`，无 `*-sys`），归 §19.2 已有的体积门管。我建议的 `grep libssh2 必须为空` 门在基线上就是红的。

### J. 「§1.7 下界=1 的后果没写」— 2/3 驳回

§16.1:1764-1765 已定义「scaffold 已提交但首次 build/MCP QA 尚未 promote ⇒ draft」；§1.7:149-150 写了恢复路径；§16.3:1815-1817 的 journal 保住 `smoke_passed` 状态、同一 candidate 的 retry 不重复确认；§21:2401 已记录「UI QA 不是发布门、MCP QA 是」。`published_no_mcp` 直接撞 §16.2:1792-1794 的 `active_state_corrupt` 定义。撤回。

## 5. 可选优化（非缺陷，不进验证矩阵）

- **operator skill 收敛**：`local-app-run/inspect-view/capture-view/interact/debug/data/background` 各自「只负责」调用一个既有 Host 能力（§7.2.1 表），其描述与 Host tool description 重复。收成 `local-app-use`（router）+ `local-app-test`、逐动词指引下沉到 `references/`，可砍 Phase 2 的内容工程量与每轮 listing 字符。若采纳，需同步改 §19.1:2160「恰好 27 个」、Phase 2:1995-1998「其余 17 个」、§22:2425。这是设计取舍，不是文档缺陷。

## 6. 第二批：completeness sweep（36 条候选 → 30 条成立、6 条撤回）

4 个 critic 分别扫 §12.5–§16、§17–§18/§20、§19、§9–§11，产出 36 条候选（6 P1 / 15 P2 / 15 P3）。
每条经 code / doc / materiality 三镜头（P3 为 facts / materiality 两镜头）独立反驳；P1/P2 ≥2/3 反驳、P3 2/2 反驳即撤回。
下面的严重度是**反驳后收窄的**，不是 critic 的原始标签。编号 S01–S36 对应 workflow journal。

### 6.1 P1

#### S03 · 连接持有的 `catalog_digest` 没定义；surface 不变的 Update 会把所有已连接会话打成 stale — 0/3 反驳

§14.3:1642-1650 `ConversationExport { app_id, catalog_digest }`、「stale catalog digest 拒绝调用」；§15.2:1688 connection 只持 app ID + catalog digest；§15.3:1709 config 未变时**复用** connection；§16.1:1769-1772 `catalog_sha256` 覆盖 build identity，且 surface 不变的 Update **不发** listChanged。四条合起来：build-only 的 Update（最常见的 Update）之后，复用的 connection 仍带旧 digest，§13.5:1568-1569 拒绝每一次调用，而 §15.4 唯一的刷新路径（收到 notification 后重跑 `tools/list`）被明确不触发。文档从未说 connection 持有的是 `tool_surface_sha256` 还是 `catalog_sha256`。

**修法（局部，不是架构）**：(a) §14.3/§15.2 写明 connection scope 绑 `app_id` + `tool_surface_sha256`（或 `authoring_revision`），§13.5 在调用时按 `app_id` 解析**当前** active `catalog_sha256`/execution binding，stale 定义为 surface digest ≠ active surface digest；(b) §19.8 加门「surface 不变的 Update 之后既有 connection 继续成功调用」+ 反向「绑旧 surface 的 connection 在 surface 变化的 Update 后被拒，输出点名 app/digest 对」；(c) 顺手解决 S02。

#### S02 · generation 递增规则前后矛盾 — 0/3 反驳（收窄为 P2）

§15.4:1720「每次 commit 精确增加一次 generation」、§16.4:1826 每次 promote 切换「list_changed generation」 vs §16.1:1772 / §19.8:2310「surface 变化才递增」。通知是否发送在四处说法一致（§15.4:1719 的 `unchanged` 例外），矛盾只在内部计数器。一行限定词即可：§16.4:1826 改「list_changed generation（仅当 `tool_surface_sha256` 变化时递增，§16.1）」，§15.4:1720 同样限定。§19.6:2275 不是「每次 promote」的依据，别引。

#### S28 · create 分支里，验证后的 Runtime Profile 没有回到正在运行的 workflow 的通道 — 0/3 反驳

§11.1:1236 要 workflow「根据这个 context 选择 runtime specialist」，§11.2:1250 policy 从 persisted/validated Profile 派生；但 create 时 Profile 由同一 workflow 的第 3-4 步产出（§10.1:1123-1124），而 §8.3:827 明说 create shell 的 `host_context` 只带 catalog identity。HEAD 的 launcher 硬要求 persisted profile 才启动（`workflow_support.rs:1275-1279`）。§10.1 第 5-10 步（designer、builder、MCP authoring、Host 验证）在 scaffold commit 之前运行并消费 specialist/family（L1181、L1236、L1308），却没有可信输入。存在一个文档禁止的逃生门：脚本能拿到 selector 的结构化输出 `template_id`（§9.4:1004-1015），template ID 编码了 family（§9.3），实现者可能从它推 specialist——违反 L1236 与 L1057（family 是 Host 权威）。

**范围**：update/verify 分支与独立 use-test workflow 不受影响（带 persisted identity 启动）。**修法**：定义一个 Host-owned 通道——Host 验证步写入 `host_context.validated_selection` 供脚本读取；或把 create 拆成 selection+staging workflow，scaffold commit 后再经现有 enricher（`workflow_support.rs:1298-1305`）启动 build。只靠 pause/resume 修不了 commit 前那一段。

#### S11 + S05 · Phase 3 的 receipt 消费 Phase 4/5/6 的产物，却声明只依赖 Phase 1+2 — 0/3 反驳（收窄为 P2，合并）

§9.5:1086-1087 receipt 绑「design proposal digest」（designer 在 Phase 4 第 1 步的 create branch，L2032）与「initial MCP proposal/catalog digest」（`AppMcpProposal` schema 与 canonical digest是 Phase 6 第 2-3 步，L2061-2062；ceiling 是 Phase 5 第 7 步，L2051）；§17.3:1873-1874 sheet 要显示 initial MCP tools 与 ceilings。而 Phase 3:2027「依赖：Phase 1 + Phase 2；阻塞 Phase 6」，Phase 4:2041「端到端 create 验证依赖 Phase 3」——Phase 3↔4、3↔6 互相依赖。mcp-designer agent 与四个 mcp-* skill 已在 Phase 2 交付，缺的只是 Host 侧 schema/validation/digest/ceiling。

**修法（二选一，明写）**：(a) 声明 Phase 3 的 receipt 是过渡形态，design/MCP digest 槽位未填（沿用 §8.1:786-797 的 optional-slot 先例），Phase 6 填槽并加 Phase 9 完成门删除过渡形态；在此之前 create 停在 §16.1:1763-1765 的 scaffold-committed draft，不 promote；或 (b) 把 Phase 3 第 5/7 步移入一个依赖 Phase 4 第 1 步 + Phase 5 第 7 步 + Phase 6 第 2-3 步的子阶段，:2027 改为「阻塞 Phase 6（仅 staging/snapshot）」，§19.4:2228 / §19.10:2340 的 receipt 门随之改挂。Phase 4 的「端到端 create」须明确为 staging-only。

#### S20 · 两个通用 crate 里的硬编码 workflow 名单没人管，改名后 lease/delete 守卫静默失效 — 1/3 反驳（收窄）

`tasks/src/lib.rs:37` 与 `tools/workflow/src/lib.rs:310` 各有一份 `LOCAL_APP_BUILD_WORKFLOWS = ["local-app-build", "local-canvas-build"]`；前者决定哪个运行中的 workflow 必须持有 workspace lease（`tasks/src/handlers/local_workflow.rs:522-523`）和哪些任务阻塞 App delete（`tasks/src/registry.rs:918` → `host.rs:6172`），后者决定 per-app workflowModel 默认（`lib.rs:319`）。§8.1 说「通用 Plugin/Workflow/MCP core」不能出现名字；§8.5 scanner 只扫 `engine-mobile`；§18 没有一步删除或重键这两份名单。

**收窄**：`workflow_id` 是脚本的 `meta.name`（`workflow_support.rs:1465/1608`），若统一后的 `local-app-build.js` 保留 `meta.name: "local-app-build"`，第一项仍匹配，只有 `local-canvas-build` 项与 `local_apps_build.rs:4335` 的 pin 变成死条目。**确定的缺陷**：(1) §8.1 对通用 core 的规则没有门（scanner 看不见 `tasks/`、`tools/workflow/`）；(2) §18 没有重键步骤；(3) §19 没有门点名 lease guard / delete guard / workflowModel default 在改名后仍生效。桌面端一腿很弱：桌面只经 `BUILTIN_WORKFLOWS`（`builtins.rs:38,61`）到达名单，Phase 9 第 2 步删它——§19.11 只需一条「两份名单与 `apply_local_app_build_default_model` 不作为死代码活过 Phase 9.2」（P3）。

#### S01 · `-`→`_` 映射 + `__` 分隔符：`abc--1` 会被解析成别的 server — 0/3 反驳（收窄为 P2：潜伏，非现役）

§15.1:1678-1679「App ID grammar 不允许 `_`，所以该映射无碰撞」只证明了单射，没看分隔符。grammar `^[a-z0-9][a-z0-9-]{0,63}$`（`local-apps/src/ids.rs:44-56`）允许 `abc--1`、`abc-`；映射后 server 名 `local_app_abc__1`，FQN `mcp__local_app_abc__1__t`，而所有解析器都在 `mcp__` 后的**第一个** `__` 切（`tools/mcp/src/mcp_tool.rs:72-92`，测试 `:1982-1990` 钉死 `mcp__a__b__c` → (`a`, `b__c`)；`permission/src/policy.rs:2377`；`policy_gate.rs:2069`；`tool-api/src/defer.rs:550`；`mcp/src/registry.rs:739,2087`）⇒ server=`local_app_abc`，tool=`1__t`。更糟：`policy.rs:2427-2431` 的 server 级规则 `mcp__local_app_abc` 会匹配 App `abc--1` 的全部 tool——违反 §19.8:2302 App A/B isolation 与 §1.5。§19.8:2303 的门只测 `abc-123`，对此恒绿。

**为何是潜伏**：今天唯一的铸 ID 路径是 8 位 hex（`service.rs:1374-1376` → `ids.rs:28-30`），create 不接受 caller app_id；连字符 ID 只能经手改 `apps/index.json` 进入。**修法**：映射本身多余——连字符在 FQN server 段合法（`mcp/src/normalization.rs:37`，`protocol/src/mcp_name.rs:10` 保留 `-`），现有 `dynamic_tool_name` 就保留连字符（`local_apps_mcp.rs:1100-1102`）。要么删掉映射，要么 `validate_app_id` 禁 `--` 与尾随 `-`；§19.8 加 `abc--1`、`abc-` 往返 `parse_full_name` 的反向用例，以及 `abc` 与 `abc--1` 共存的跨 App 用例。

### 6.2 P2

#### S06 · `workflow_state: Ready` 在 build 时就被盖章，与派生 `draft` 打架；没有 Phase 搬这个章 — 0/3 反驳

§16.2:1776「不在 AppRecord 新增可漂移状态机」，但 record 已有可写的 `workflow_state`（`local-apps/src/types.rs:216-217`），`mark_ready` 在 build 成功后立即盖 `Ready`（`service.rs:853-862`，调用点 `local_apps_host.rs:6437`），早于 MCP QA/promote；§16.4:1825「record ready state」又要在 promote 时写。built-but-unpromoted 的 App：record 说 Ready、派生说 draft。同类问题：build provenance receipt 也在 build 时写（`local_apps_build.rs:1133`）。**修法**：Phase 6 加一步——把 `mark_ready` 从 `build_app` 搬进 §16.4 promote 事务并重钉相关测试（`service.rs:1772-1794,2211,2246,3159-3194`、`storage.rs:1822,2450`、`host.rs:13703`），或按 §1.6 无迁移的前提删掉持久化 `workflow_state`、让 DTO 成为 active build 的投影；§19.8 加「build 成功但 MCP/use-test QA 未过的 candidate 在 wire 上报 `draft`」。

#### S13 · §19.10 的 i18n 门没带 `--check`，按字面读法恒绿 — 0/3 反驳

§19.10:2344-2346 写的是 `python3 clients/translations/generate.py` 退出 0 且打印 `OK: <n> keys, 5 locales`，重跑无 diff。`generate.py:283-290`：不带 `--check` 就直接写 xcstrings/strings.xml，第二次运行必然无 diff；`stale_problems` 只在 `--check` 下执行；`OK:` 行两种模式都打（`:296`）；孤儿生成文件只有 `--check` 抓（`:244-249`）。key-set 一致性两种模式都跑（`:267-270`，`INCONSISTENT:` 退出 1），所以「key-set 门」本身没坏，坏的是 staleness/no-diff 子句。**修法**：§19.10 改为 `generate.py --check`，点名红路径字符串（`is out of date` / `missing generated file` / `orphaned?`），并写明 no-diff 指「重生成后工作树无变化」。与 §17.5:1903、§22:2421 对齐。

#### S14 · §17.1 的 9 个 typed error 里 4 个全文只出现一次，没有生产方也没有门 — 0/3 反驳

`template_unavailable`、`proposal_invalid`、`permission_ceiling`、`verification_unavailable` 只在 :1843-1844。收窄：`verification_unavailable` 在 §16.2:1788/§21:2401 是**状态**不是 error，归错了前缀（应属 `local_apps_verification_*`）；`proposal_invalid` 有行为（§12.3、§19.8:2300）但名字没绑；`permission_ceiling` 可能是 §13.5:1580 的 CallToolResult 内 error code，不该出现在 client-protocol 列表或需明写双面；`template_unavailable` 完全没有生产方。**修法**：一张小表（operation、wire 层：client-protocol DTO vs MCP CallToolResult、触发条件、恢复动作）+ 每个 code 一条 §19 反向用例，输出点名 code。

#### S15 · §20.3 交给 §19.3 的 scanner 断言，可 §19.3 那条没有 needle 也没有反向用例 — 0/3 反驳

§20.3:2372「由 §19.3 的 scanner 断言，不靠人工判断」；§19.3:2219 只是同一句话的复述，与其上 (a)(b)(c) 三条有反向用例的门不同。「materialization 入口」是概念，不是可 grep 的 token。**修法**：needle = (i) `engine-mobile` 内 `register_verified_builtin` 恰好一个调用点（composition module），反向：加第二个调用点 ⇒ 红且点名 file:line；(ii) bundle-descriptor 模块之外不得 `include_bytes!`/`include_str!` `local-apps/templates/**` 或 `plugins/lingxi-local-app/**`——今天的 `local_app_runtime_profiles.rs:22/:800/:836`、`local_apps_build.rs:4153-4214` 作为基线条目入 allowlist 计数。

#### S21 · §5.5「与 `WORKFLOW_EXTENSIONS` 同一集合」是范畴错误；oracle 的 plugin 扫描只认 `.js` — 0/3 反驳

`WORKFLOW_EXTENSIONS`（`tools/workflow/src/lib.rs:52`）是 name→filename 的探测列表（`:198-208,221-236` `dir.join(format!("{name}{ext}"))`），不是目录扫描 glob；带 `""` 成员的目录扫描会把 README/LICENSE 都吃进去（`:806` 今天就这样）。oracle 2.1.251 的 plugin `workflows/` 默认扫描只认 `.js`，`.mjs/.cjs/.ts` 记为 near-miss 跳过。**修法**：§0:34、§5.5:400-407、§22:2417 统一改为 `.js`-only（或显式 Divergence 条目），删掉「foo.mjs 放 ~/.lingxi/workflows/ 可见」的理由（oracle 的用户目录扫描同样 `.js`-only；`WORKFLOW_EXTENSIONS` 本身是 port 自创的 resolver，`ccfa607bc`）；§19.1 加一条钉住扫描集合的用例。

#### S22 · Babylon 测试挂在真机门后，§20.7「所有 Canvas/engine 强门有可执行测试」对 Babylon 是空的 — 0/3 反驳

§19.5「Babylon tests 只在 availability gate 通过后启用」+ §21「Babylon 仍被阻塞」⇒ 完成时 Babylon3d policy 没有任何会红的门。收窄：Babylon3d 分支在完成时**不可达**（§9.2:981、§9.3:991、§19.4:2234；`local_apps_host.rs:10426` 断言 selected family 永不是 babylon_3d），所以不是运行时缺陷，是完成定义写空了。**修法（二选一）**：(a) Babylon policy 的 report-driven 测试（无需真机，harness 已支持 `builtins.rs:2640-2660,2233-2238`）在 Phase 4 就启用并写明反向用例，只把真机 spike 留在 availability gate 后；或 (b) 把 Babylon policy 从 Phase 4 第 5 步移到 availability 翻转那份工作里，并在 §20.7 显式排除。

#### S30 · §9.1 把 `permissions.rs` 的 settings 叫「fixture」，它是写进每个 App workspace 的生产授权 — 0/3 反驳

`local-apps/src/permissions.rs:35-38` `include_bytes!(".../vite-react-static-v1/.lingxi/settings.local.json")`，`:349-359 save_workspace_permission_settings_initialized` 在 `#[cfg(test)]` 之外、由 create 路径调用（`service.rs:1175`）原子写入 `.lingxi/settings.local.json`。收窄：路径消失是编译错误不是静默丢失，且 `service.rs:1655-1675` 有测试断言 create 写了 allow list；伤害是**归属误导**（字节被停在测试 fixture 路径、或实现者把 writer 当测试专用）。**修法**：§9.1:901、Phase 2 第 7 步:2002、Phase 9 第 3 步:2109 改为「这是 Host 在 create 事务写入的生产授权，`include_bytes!` 源要搬到 Host-owned 生产位置」并点名目的地；§19 加门：新建 workspace 的 `settings.local.json` 含点名的 allow/deny 条目（`permissions.rs:479-511` 已有，但按 §19 原则要钉住它活过迁移）。

#### S32 · Three/Phaser/Babylon 的「performance budget」强门没有任何数字 — 0/3 反驳

L1279 是全文唯一出现；§19.5:2255-2257 没有阈值。这还与 L40「删除…没有数值的性能占位门」的自述矛盾。**修法（§20.13 二选一）**：(a) 给出参考设备（§19.0:2148 iPhone 11 / iOS 18.6.2）上的 p95 frame-time / 最低持续 fps，作为 `UseTestReport`（L1293-1305）的数值字段与 §19.5 的反向用例；或 (b) 从 §11.2 强门列表删掉，保留为现有 `frame_budget` 设计字段（`local_app_canvas_workflow.js:131`，prompt 提示，非门），并修正 L40。

#### S29 · §12.5「`flow_id` 必须属于 candidate/active build」在 receipt 前没有可指的 build — 1/3 反驳（收窄为文本修正）

§10.1 第 7 步 authoring、第 10 步 Host 验证都在 receipt（第 11 步）前，build 在第 13 步；§16.3 的 journal 在所有路径上 `approved` 都先于 `built`，所以 binding 验证时任何路径都没有「candidate build」。文档已有正确的钩子：§12.4:1426 Agent 只给「业务 Flow semantic reference」、:1434 Host 派生「final Flow ID/build binding」，§16.1:1770 `catalog_sha256` 覆盖 build identity。**修法**：§12.5:1468 改为「Flow 定义属于同 App 的 staging source（create）或 candidate/active build（update/revise），build binding 在 MCP QA 时重新派生」；§12.1:1331-1332 的两项输入标注「首次 create 时缺席」。

#### S12 · §19.7 括号里「断言该来源在 walk 中被引用」在基线上已经绿 — 1/3 反驳（收窄为 P3 措辞）

`permission/src/policy.rs:53` `SOURCES_BY_PRIORITY` 已含 `McpServerPolicy`，`authorize_inner`（:665）/`first_match`（:1458-1477）遍历全部条目。producer 那一半在基线上是红的，且 §19.7:2286-2288 的行为用例（`always_allow` 直接授予 vs `toolPermissions=allow` 只解封）没有 producer 过不了——门不是无牙，只是括号那句描述了一个恒绿断言。**修法**：把 :2289-2290 改写成端到端用例——manifest `permission_policy: deny` 精确 FQN 产生 Deny，其引用规则的 source 是 `McpServerPolicy`；删掉 producer 的插入 ⇒ 红。

### 6.3 P3（措辞与门的精度）

- **S07**（1/2）§16.4:1828「始终停留在 staging」的「staging」未限定；§10.1:1130-1139 在 receipt 后就 commit scaffold，§16.1:1762 把它列为合法 `draft`。改「候选 build/catalog 停留在 staging；scaffold/profile/dependency/template snapshot 在 receipt 消费后以 draft 提交」；§16.3 补一条 Create 崩溃规则（已提交的 draft Manifest 保留、候选 build/catalog 删除、journal 允许不换 receipt 重试）。
- **S08**（1/2）§16.2:1792「必须同时存在」与 :1793 的「只存在其中一个」是同一句里两个谓词；改「必须成对出现（同为 None 视为 draft，同为 Some 视为 published）」，:1778-1782 表加第四行 `active_state_corrupt | 恰有一个 active ref`。
- **S09**（0/2）§12.5:1473「全部有界」无数字；§14.4:1657 的 32-step 与 `FlowDefinition` 现有 1..=128（`runtime_v2.rs:965`）未对账——要写明 32 只作用于 `AppMcpFlowBinding`，agent `flow_execute`（`local_apps_host.rs:6158`）与 background（`:7135`）路径不变；5 分钟等于现有 `MCP_TOOL_IDLE_TIMEOUT_REMOTE_MS`（`mcp/src/client.rs:1323`）应引用；4/8/60/10 在代码里无来源需自证；§19.8 加「33-step binding 被拒且错误点名步数」；§22 加 128 步 / 15 分钟 `FLOW_EXECUTION_TIMEOUT`（`local_apps_host.rs:82`）一行。
- **S10**（0/2）「Android 缺 UI runner」（:1786、:2401）没点名缺什么；`clients/android/.../LocalAppWebView.kt:117-168` 已分发与 iOS 相同的 `LocalAppUiAutomationAction` 集合含 `CaptureView`（PixelCopy）。两端都是前台 preview runner，都没有离屏 runner。要么点名具体缺失符号，要么改为平台中立的「UI evidence unavailable」；§22 补 Android UI automation 一行。
- **S16**（0/2）Phase 7 完成条件:2083「30+ Apps 不产生线性增长」在基线上已成立，与 §19.9:2321-2325 自己的判断矛盾。改绑 §19.9 的场景：ConversationExport scope 下 App A 连接不能列/调 App B（新 scope，基线测试 `local_apps_mcp.rs:2454` 只覆盖 `App(String)` scope）；100 个 registered logical server ⇒ physical transport == 1 + 反向用例；删「30+」。
- **S18**（0/2）§17.5:1910 把 `local_apps_mcp_proposal_*` 指向 §17.4，而 §17.4 是 MCP inventory；update/revise 的 proposal diff/approval sheet 没有任何 §17 小节定义行级内容。加 §17.4a：逐 tool added/removed/changed（name/inputSchema/outputSchema/annotation 差异，对齐 §19.8 的 surface 维度）、ceiling 差异、required_flow_changes、excluded_capabilities、gates、receipt TTL/supersede 状态；§19.10 的反向用例（「删掉 tool X 的 proposal 必须列出 X 为 removed」）据此可写。
- **S19**（0/2）§17.3:1871 要显示「rejected candidates」，§9.5 的 `ValidatedTemplateSelection` 没有该字段（只有 `reason`）。加 `rejected: Vec<RejectedCandidate { template_id, reason }>`，标注 display-only、不进 receipt，Host 过滤 template_id 必须在当前 catalog 内。
- **S25**（1/2）§19.1「Plugin root `LINGXI.md` 不进入 project context」在基线上结构性成立（机制在 `memory/src/lingxi_md/hierarchy.rs`，与 plugin crate 无关）——只能当回归 pin；§19.1 的 BuiltIn 条目只有前半句恒绿，`register_verified_builtin` 半句是真门——改写不删；§19.7 见 S12。
- **S27**（0/2）16,384-token 门没写计数器。用 `tool-api/src/wire.rs:225-252` 的 Tool Search 回退口径（name + description + JSON.stringify(input_schema) 的 UTF-16 单位 / 2.5）作为 hermetic 度量（= 40,960 字符），`count_tokens_exact`（`tooling.rs:640-656`）只作证据不作门；§22 记录仓库并存两种 char/token 比例（wire.rs 2.5、snip.rs 4）。
- **S33**（0/2）§11.4:1316「repair budget 明确」但文档没有数值；现值 fast=1 / balanced=1 / thorough=2（`local_app_workflow_core.js:138-155`），canvas 形态拒绝 `fast`（`local_app_canvas_workflow.js:252`）；§10.1:1129「max 2 rounds」与 §12.2:1368 无界复述不一致。写出三档数值、把 L1129 绑到 thorough 预算、§19.5 加「budget+1 个 blocking finding ⇒ 失败且文本点名耗尽的预算」。
- **S34**（1/2）§9.1.1:942「为保持已写入 binding 的 contract digest 不变」——§1.6/§16.1 说没有任何 binding 会存活。规则本身要留（§9.1 门 5 与 §19.4 量它；`local_app_runtime_profiles.rs:697` 已钉五个 r1 digest golden），理由换成「r1 是 byte-preserving baseline，contract digest 是迁移保真度的度量对象」。参见 memory：对的规则配错理由会被一起删。
- **S35**（0/2）§10.2:1145-1157 的 Update 管线从「revised proposal」直通「atomic promote」，没有 §12.2:1372 要求的 Native MCP proposal approval 步；在 L1151-1152 之间插入。更要紧的是它没提的一支：**surface 不变的 Update** 消费什么 approval？§16.3:1815 说 source 变化就要新 receipt，§19.8:2307 只豁免 standalone-unchanged，§17.1:1840 只有两种确认 DTO——三者合不上。这与 §2 的 E 是同一个决定，一起定。
- **S36**（0/2）§9.5:1085 receipt 绑「dependency snapshot digest」，但 §16.1:1762 说 receipt 时 `dependency_snapshot` 为 None，`AppDependencySnapshot` 的 `dependency_tree_sha256/sbom_sha256`（`manifest.rs:306-318`）来自安装后的 node_modules。改绑**安装前**输入的 Host 计算 digest（requested 声明 + effective package.json + base lockfile，即 `scaffold_artifacts_for_binding` 的输入，`local_app_runtime_profiles.rs:607-625`，镜像现有 `PendingDependencyChangeReceipt` 的 `requested_json + effective_package_json`，`local_apps_host.rs:149-159`）；安装后 snapshot 在 journal `built` 记录；§19.10 加「receipt 后改 requested 声明 ⇒ approval 失效，输出点名 dependency digest」。

### 6.4 撤回的 6 条（记录以免 codex 重走）

- **S04**「Phase 5 的 ceiling 派生消费 Phase 6 的 typed Flow binding」— 3/3 驳回：§14.2 只读 step kind 与 capability class，这些在 HEAD 已存在（`runtime_v2.rs` CapabilityId），不依赖 Phase 6。
- **S17**「§22 说 §17.5 不钉 key 数，§17.5 钉了 1894」— 2/2 驳回：1894 是「本次复核基线」的描述事实，判据是 `generate.py` 的集合差相等，没有任何门读 1894。
- **S23**「§19.0 的延迟/heap 阈值无推导」— 2/3 驳回：§19.0:2147-2152 已把它们定义为带证据协议的第一版硬限并规定了调整流程；缺推导不等于不能红。（S27 的「没写计数器」是另一回事，成立。）
- **S24**「receipt/approval 的 surface 在 Phase 8 才交付」— 3/3 驳回：Phase 3 第 5 步就是 Host 侧 receipt 协议，Phase 8 的 UI 在其下游（:2027、:2103）。
- **S26**「§19.6 反向用例点名了一个接不进来的 CLI 常量」— 2/2 驳回：`apps/cli/src/commands/mcp.rs:358` 是私有常量、engine-mobile 不依赖 cli——用例意图是「把 2025-06-18 接进 hub 必须红」，用字面量复制即可实现，不需要引用那个符号。
- **S31**「motion_check 条件性与 §19.5 无条件 blocker 矛盾」— 3/3 驳回：§11.2:1266 下一条就是无条件的「`motion_check.frames_compared >= 2`」，:1265 只是对动态 surface 的补充说明；`builtins.rs:2509` 的现有测试也按无条件处理。

## 7. 建议处理顺序

先做需要**决定**的，再做措辞。

1. **一个决定，牵动 E / S35 / S03 / S02**：approval 到底绑什么、connection 到底持什么 digest。建议：approval 绑 design spec + template + dependency 输入 + `tool_surface_sha256`（含 description）+ ceiling 摘要，不绑 source bytes；connection 绑 `app_id` + `tool_surface_sha256`，调用时解析当前 active catalog；generation 只随 surface 变化递增。定了这一个，四条一起关。
2. **S28**：create 分支的 validated selection 回流通道——这是 Phase 4 第 3 步能不能开工的问题。
3. **S11+S05**：Phase 3 receipt 的过渡形态或子阶段——排期图现在是环。
4. **A 残留**：Phase 1 改依赖 Phase 0a；0b 保留为 §20 完成条件。
5. **B + S27**：§12.3 加 per-App tool-definition 预算门并点名计数器；§19.0 写清 heap 测量对象。
6. **S20**：两份通用 crate 名单加进 §18 与 §19（lease/delete guard 的门）。
7. **ALT1 残留 2**：Phase 1 真机验证 iOS 上模型能 Read 物化根下的 references。
8. **S01 / S06 / S13 / S14 / S15 / S21 / S22 / S30 / S32 / S29 / S12**：各一段文字或一条门。
9. **F / G / I / C 残留 / H 残留 / S07–S36 其余**：措辞。
