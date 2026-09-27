# LingXi Local App Plugin 完整设计与实施方案

状态：已整合第四轮（R2）复审并完成源码复核
日期：2026-08-29
代码基线：`5448e93e3`
复审来源：`docs/local-apps/LOCAL-APP-PLUGIN-DESIGN-V2-REVIEW.md`、
`docs/local-apps/LOCAL-APP-PLUGIN-DESIGN-V2-REVIEW-R2.md` 及 2026-08-29 源码复核
目标版本：Local App schema v3、LingXi client protocol `10.0.0`

## 0. 文档定位

本文是 Local App Plugin 的单一实施设计，吸收以下文档中的有效内容：

- `docs/local-apps/LOCAL-APP-PLUGIN-IMPLEMENTATION-PLAN.md`
- `docs/local-apps/LOCAL-APP-PLUGIN-PLAN-REVIEW.md`
- `docs/local-apps/RUNTIME-OS-V2.md`
- `docs/local-apps/HANDOFF.md`
- `lingxi-code/docs/mcp-plugin-byte-alignment-2.1.251-2026-08-28.md`

本文可独立复审。旧文档只作为设计演化记录，不作为实施契约。

技术行为以以下上游定义为 oracle：

- [Claude Code Plugins Reference](https://code.claude.com/docs/en/plugins-reference)
- [Claude Code Dynamic Workflows](https://code.claude.com/docs/en/workflows)
- [Claude Code Subagents](https://code.claude.com/docs/en/sub-agents)
- [Claude Code MCP](https://code.claude.com/docs/en/mcp)
- [MCP 2025-11-25 Schema](https://modelcontextprotocol.io/specification/2025-11-25/schema)
- [MCP Tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)

“以 Claude Code 为 oracle”只约束字段、解析、加载、命名规则、权限语义和运行行为，不改变 LingXi 自己的品牌命名空间。

本轮复核对 Claude 合入版本作出以下取舍：

- 保留：桌面端不安装 Local App Plugin、两条权限管线、Host verified template catalog、
  i18n 与桌面回归责任；Plugin 与 named saved workflow discovery 按 oracle 只扫描 `.js`，
  显式 `scriptPath` 仍可读取 workflow engine 支持的脚本。
- 修正：published App 的 tool 下界恢复为 1；模板迁移清单为 112 个实际 scaffold 文件，
  不是 124 个 tracked 文件；Catalog 使用 Host-issued digest echo 防陈旧；builtin root 使用
  非易失 Application Support/data 存储并校验 marker；scanner 从 verified component inventory
  派生且不豁免产品 Plugin；协议目标从当前 `9.0.0` 升为 `10.0.0`；approval contract、
  tool surface 与 execution catalog 使用三个独立 digest；create 在 run-scoped Host selection
  handle 建立后才进入 profile-specific stages。
- 删除：没有基准依据的人周估算、没有数值的性能占位门、按前缀批量删除 30 个仍被客户端使用的
  runtime-profile 文案、App ID 的 `-`→`_` 改写、按 workflow basename 判定 workspace lease/model
  default，以及 build 阶段提前写入的持久 `workflow_state=Ready`。

## 1. 已确定决策

以下内容不再作为实施中的开放问题。

### 1.1 品牌与技术对齐

产品品牌始终是 LingXi。

| Claude Code oracle 名称 | LingXi 产品名称 |
| --- | --- |
| `.claude-plugin/plugin.json` | `.lingxi-plugin/plugin.json` |
| `.claude-plugin/marketplace.json` | `.lingxi-plugin/marketplace.json` |
| `${CLAUDE_PLUGIN_ROOT}` | `${LINGXI_PLUGIN_ROOT}` |
| `${CLAUDE_PLUGIN_DATA}` | `${LINGXI_PLUGIN_DATA}` |
| `${CLAUDE_PROJECT_DIR}` | `${LINGXI_PROJECT_DIR}` |
| `~/.claude` | `~/.lingxi` |
| `CLAUDE.md` | `LINGXI.md` |

除上述品牌映射外，字段类型、默认值、解析失败行为、component discovery、path resolution、namespacing、workflow runtime、agent frontmatter 和 MCP 行为与 oracle 对齐。

LingXi 不读取 `.claude-plugin`，也不导出 `CLAUDE_PLUGIN_*` alias。Oracle fixture 通过品牌 token normalization 后与 LingXi 输出比较。

### 1.2 一个统一 Plugin

只创建一个 builtin Plugin：

```text
manifest.name = lingxi-local-app
source = PluginSource::BuiltIn
```

不发明 `name@builtin` 标识：现有 non-marketplace plugin identity 使用 bare manifest name，
source 由独立字段表达。`PluginId` 是进程内 opaque `plg:<uuid>`，加载后可用于 live registry，
但不得写进 Manifest、template snapshot 或长期 receipt。持久 provenance 绑定
`manifest.name="lingxi-local-app" + PluginSource::BuiltIn + bundle_sha256`。

它统一包含：

- Local App create/update/verify 能力；
- run/inspect/interact/test/debug 能力；
- DOM、Canvas2D、Three、Phaser、Babylon 平台与运行时 skills；
- template catalog 和五套 versioned template assets；
- build、use-test、per-App MCP authoring workflows；
- operator、tester、template-selector、designer、builder、verifier、mcp-designer agents。

不使用多个互相依赖的 creator/runtime Plugin，也不增加 activation group、plugin group、lease 或 required-by 私有模型。

### 1.3 Builtin-only

Local App Plugin 随 LingXi app bundle 发布，默认启用，可整体禁用。

本方案不实现：

- Local App Plugin marketplace 分发；
- Local App Plugin 网络安装或独立更新；
- Local App Plugin git/npm/archive source；
- Local App Plugin install/uninstall UI；
- 第三方替换 Local App creator；
- 未签名 Local App Plugin。

通用 Plugin install/marketplace parity 和供应链安全属于独立工作流，不阻塞本方案。

### 1.4 Agent 选择模板和 Runtime Profile

创建时由 `template-selector` 根据用户需求和 Host 返回的 catalog 选择模板。

用户不需要理解 React DOM、Canvas、Three、Phaser 或 Babylon，也不增加独立技术选型页。Native create confirmation 统一展示：

- App 名称和 brief；
- Agent 选择的 template；
- 派生 Runtime Profile；
- 选择理由和被排除候选；
- 初始依赖；
- 初始 per-App MCP tools；
- 权限和副作用。

用户可以确认、拒绝或回到会话要求修改。Host receipt 才是 scaffold 权威，Agent 输出本身不授权写入。

### 1.5 每 App 独立 MCP

每个成功发布的 Local App 必须拥有：

- 一个稳定、独立的逻辑 MCP server identity；
- 一个由该 App 需求实时生成的 active MCP catalog；
- 1–16 个有实际业务意义的 tools（数量下界见 §1.7）。

所有逻辑 server 复用一个 Host-owned physical in-process MCP hub，不启动 N 个进程，不接受 App 提供的 command、URL、headers 或 transport。

### 1.6 不保留预发布数据

当前 Local App 尚未正式发布。实施 schema v3 时清理现有开发数据、workflow task 和 builtin registry，从空数据状态发布新契约。

### 1.7 Published App 的 Tool 数量下界为 1

“每个 Local App 以自己的 MCP 暴露给 Conversation LLM”是产品定义，不是可选增强。
因此每个 **published** App 的 active catalog 必须有 **1–16 个 meaningful tools**：

- MCP authoring 对每个 App 强制执行；
- 无法生成至少一个通过质量门的 tool 时，返回 recoverable
  `mcp_authoring_required`，保留 create/update candidate 与既有 active 状态，**不得 promote**；
- Agent 应先尝试把真实用户任务绑定成有意义 Flow，例如查看摘要、读取当前状态、开始一次挑战、
  保存/导出作品或执行领域动作，而不是机械包装内部 API；
- Canvas、游戏或可视化 App 不因技术 family 被拒绝。只有在确实没有可安全、可验证、可由 Flow
  表达的业务动作时才进入 `mcp_authoring_required`；
- 不允许为凑下界生成 filler、重复、无目标或只返回静态文本的 tool。

`mcp_authoring_required` 是 staging/journal 的可恢复结果，不是 published 状态。用户可以继续
澄清希望 LLM 完成的任务，或修改 App 需求；authoring 与 QA 通过后再生成一次 Native receipt。

> **实现现状**：`create` 入口今天恒定以 `create_without_mcp=true` 调用 `approve_mcp_proposal`
> （`local_apps_mcp.rs`/`local_apps_host.rs`）——create 事务本身完全不跑 MCP authoring，
> `AppRecord.mcp_intent` 只是留给后续流程的备忘，不授予任何东西。本节的 1–16 tool 下界因此
> 不是在 create 内强制的；它在 App 之后经由独立的 MCP authoring 流程（对应 §10.2 Update 里
> 的 `active MCP catalog` 步骤）完成并 promote 时才生效。§10.1 的时序图已按这条路径更新。



### 1.8 桌面端不安装本 Plugin

`lingxi-local-app`（source=`BuiltIn`）只随移动端 app bundle 发布。桌面端今天没有 Local Apps
（`engine-desktop/src/` 中 `LocalApp` 零命中），本方案不为桌面端引入 Local Apps Host、
runtime 或 WebView QA runner。

因此：

- 桌面端 MCP inventory **不包含** managed Local App source（§17.4 相应限定为 iOS/Android）；
- 但 Phase 0a/0b 修改的是 `plugin/` 通用契约，**桌面端是它唯一的真实使用者**，
  必须承担回归验证责任（§19.11）。

若将来要在桌面端提供 Local Apps，那是独立方案，不在本文范围。

这不删除 `frontend-design` 的 Desktop presentation profile：它描述生成页面在 desktop-class
viewport/input 下的布局与交互，不等于本版本把 Local App Host 安装进 `engine-desktop`。

## 2. 目标与非目标

### 2.1 目标

1. 将 Local App 提示层、skills、agents、workflows 和 scaffold assets 从 Core 的硬编码清单中提取为一个 builtin Plugin。
2. 保留 Host 对数据、Runtime Profile、依赖、bridge、构建、权限、receipt、checkpoint 和 smoke 的控制。
3. 让 Agent 根据需求选择最合适模板，同时由 Host 验证选择并持久化不可变 Runtime Profile。
4. 为每个 Local App 生成独属 MCP tools，使 Conversation LLM 能调用该 App 的真实业务能力。
5. 复用现有 Plugin、Workflow、MCP、Permission 和 Local Apps 框架，不创建第二套扩展协议。
6. 保证 DOM、Canvas2D、Three、Phaser、Babylon 的生成和 QA 不因统一 workflow 而降级。
7. 让移动端只承担 builtin Plugin 的发现、启停和 inventory，不搬入 marketplace/install 全套功能。

### 2.2 非目标

- 不把 Local App runtime 本身做成 Plugin。
- 不把 `window.lingxi.v2` 改成 MCP。
- 不把 App Persistent Agent 改成 Plugin Agent。
- 不让 generated App 执行任意 JavaScript MCP handler。
- 不允许 App 自行声明外部 MCP server。
- 不增加 `templates`、`activationGroups` 或 `AppTaskAccess` 私有 Plugin 字段。
- 不允许 Plugin enabled 状态替代 permission/capability authorization。
- 不支持跨 Runtime Profile family 修改现有 App。

## 3. 架构边界

```text
LingXi mobile composition root
│
├── PluginManager
│   └── lingxi-local-app (BuiltIn)
│       ├── skills
│       ├── agents
│       ├── workflows
│       └── read-only assets/templates
│
├── LocalAppPluginBinding
│   ├── resolves three plugin workflows
│   ├── installs the Local App launch-context enricher
│   └── exposes verified template catalog to Local Apps Host
│
├── Local Apps Host/Core
│   ├── records + manifest + data
│   ├── Runtime Profile catalog and binding validation
│   ├── dependency snapshot
│   ├── template snapshot
│   ├── build/runtime/smoke
│   ├── receipts/checkpoints/journals
│   └── bridge/capability authorization
│
└── one physical in-process MCP hub
    ├── logical server local_app_<app-a-id>
    ├── logical server local_app_<app-b-id>
    └── connection-bound App scope + active catalog digest
```

职责原则：

- Plugin 决定“Agent 如何工作”。
- Host 决定“什么状态可信、什么操作允许、什么结果可发布”。
- App Manifest 和 Host-owned snapshot 决定“这个 App 是什么”。
- Conversation MCP catalog 决定“LLM 可以看到该 App 的哪些业务动作”。

## 4. 现状与接入点

| 模块 | 当前状态 | 本方案修改 |
| --- | --- | --- |
| `plugin/src/manifest.rs` | `PluginComponents` 无 workflows/themes/monitors，`PluginManifest` 不保留 metadata | 增加当前 oracle 字段与 typed component slots |
| `plugin/src/discovery.rs` | 安装目录必须有 manifest；扫描 commands/agents/skills/hooks/MCP/LSP | 增加 manifest-less/skills-directory plugin 与 default/custom workflows、experimental themes/monitors discovery |
| `plugin/src/manager.rs` | 物化现有 component registries；已有 `PluginSource::BuiltIn`，但 install arm明确拒绝 | 增加 workflow/theme/monitor materialization 和 verified builtin registration seam |
| `apps/engine-mobile` | 没有 `plugin` dependency/PluginManager | 接入同一个 PluginManager 的 builtin-only 组合 |
| `apps/engine-mobile/src/lib.rs` | 硬编码 10 个 Local App skills | 删除名单，由 Plugin inventory 注册 |
| `apps/engine-mobile/src/workflow_support.rs` | 按 Profile 硬编码两个 workflow 名 | 合并 workflow，由 resolved handle 注入可信 context |
| `apps/engine-mobile/src/local_apps_host.rs` | `LINGXI.md` 写死 workflow 名 | 改成 Host 中立的能力描述 |
| `apps/engine-mobile/src/local_app_runtime_profiles.rs` | 五套模板由 `include_bytes!` 编入 Core；Babylon unavailable | 移入 Plugin assets，由 builtin bundle provider 提供 |
| `platform-api/src/mcp.rs` | `McpToolDto` 缺标准 Tool 字段 | 补完整 MCP 2025-11-25 DTO |
| MCP protocol constants | `mcp` client latest 为 2025-11-25；`apps/cli mcp serve` 有独立 2025-06-18 常量 | Local App in-process hub 只复用 `mcp::initialize_params::LATEST_PROTOCOL_VERSION`，不复制 CLI 常量 |
| `mcp/src/registry.rs` | 支持远端 `notifications/tools/list_changed` | 增加 managed logical server 注册/注销 |
| `apps/engine-mobile/src/local_apps_mcp.rs` | 一个 physical hub；Conversation/App 两种 scope；notifications 为空 | 增加 ConversationExport scope、active catalog 和 broadcast |
| `permission` | 支持 exact FQN/server wildcard；`permission/src/rule.rs:188-190` 已有具名 latent 规则源 `PermissionRuleSource::McpServerPolicy`（注释：`No producer yet — latent`），已接进 `policy.rs:53`/`filesystem.rs:193`/`shadow.rs`/`tui` | 为该 latent 槽位补 producer（rules），并另加 `effective_max_permission`（ceiling）。**两者是不同机制，见 §14.1** |
| `local-apps/src/manifest.rs` | 无 Conversation-facing MCP ref | 增加 active catalog ref 和 verification identity |
| `local-apps/src/runtime_v2.rs` | Flow step 使用固定 `input_json` | 增加 ToolInput/StepOutput typed binding |

## 5. LingXi Plugin 契约

### 5.1 Manifest 路径

Plugin manifest 如果存在，唯一合法路径是：

```text
<plugin-root>/.lingxi-plugin/plugin.json
```

与 oracle 一致，通用 Plugin manifest **可选**：缺失时从 Plugin root 的默认 component
locations 自动发现，并从安装目录名派生 name。一个只有 root `SKILL.md` 的 skills-directory
plugin 也能加载。`lingxi-local-app` 是产品 builtin，必须提交 manifest；manifest-less 只是
Phase 0a/0b 通用 contract parity，不是它的发布形态。

Plugin root 的 `LINGXI.md` 不作为 project context 自动加载；Plugin 指令只能通过 skills、agents、
workflows/hooks 等正式 components 提供。§8.4 的 `LINGXI.md` 位于每个 Local App workspace，
两者不能混淆。

Marketplace 文件仍遵循 LingXi 品牌路径，但 Local App builtin 不使用它：

```text
<marketplace-root>/.lingxi-plugin/marketplace.json
```

所有生产代码通过 `branding::PLUGIN_MANIFEST_DIR` 访问目录名，不复制 `.lingxi-plugin` 字面量。

### 5.2 Manifest schema

解析器支持与当前 oracle 等价的字段和语义：

```text
name
displayName
version
description
author
homepage
repository
license
keywords
metadata
defaultEnabled
skills
commands
agents
workflows
hooks
mcpServers
outputStyles
lspServers
experimental.themes
experimental.monitors
userConfig
channels
dependencies
```

Local App Plugin 实际声明的最小子集：

```text
name
displayName
version
description
author
license
defaultEnabled
skills
agents
workflows
```

规则：

- `name` 是 kebab-case namespace。
- `displayName` 只用于 UI。
- unknown field 在 runtime 忽略、validator 警告；strict validation 将 warning 转为失败。
- 不增加 `templates` 字段，template 是普通只读 asset。
- 不在 manifest 中声明 per-App MCP server；它们是 Host managed runtime state。

### 5.3 Component paths

- 路径相对 Plugin root。
- 普通 component path 必须以 `./` 开始。
- `skills` 额外接受 `.` 和 `./`。
- `skills` custom paths 追加到 default scan。
- 当不存在 default `skills/` 且 manifest 没有 `skills` 声明时，Plugin root 的单个
  `SKILL.md` 作为 fallback skill；本 Local App Plugin 使用多 skill 目录，不依赖该 fallback。
- `commands`、`agents`、`workflows`、`outputStyles` custom paths 替换 default scan。
- `experimental.themes` custom paths 替换 default `themes/` scan；接受 path/path array。
- `experimental.monitors` 接受 relative JSON path 或 inline unique-name array，替换 default
  `monitors/monitors.json`。
- hooks、MCP、LSP 使用各自 merge 规则。
- materialize 后不允许 component path 逃逸 Plugin root。
- symlink 必须解析到 Plugin root 内部。

### 5.4 Plugin Agents

支持字段：

```text
name
description
model
effort
maxTurns
tools
disallowedTools
skills
memory
background
isolation
```

Plugin-shipped agent 中以下字段读取后忽略：

```text
permissionMode
mcpServers
hooks
color
initialPrompt
```

Runtime 对这五个字段全部 strip，不把它们传播到 `AgentDefinition` 的可执行权限/上下文；
validator 报 unsupported-field warning，strict validation 将 warning 变成失败。
其中 `permissionMode`/`mcpServers`/`hooks` 是安全边界；`color`/`initialPrompt` 虽被 LingXi
通用 Agent parser 认识，但不在当前 Claude Plugin Agent 官方支持列表中，因此 Plugin 入口不得
借通用 parser 偷偷放宽 surface。

解析语义：

- 缺 name 时使用文件路径派生名。
- YAML 损坏时仍按文件名加载，忽略 frontmatter。
- 缺 description 时使用 Plugin 默认说明。
- nested path 进入 scoped name。
- agent 自身 name 禁止 `:`；冒号由 Plugin namespace 注入。

### 5.5 Plugin Workflows

`PluginComponents` 增加：

```rust
pub workflows: Vec<ComponentPath>
pub themes: Vec<ComponentPath>
pub monitors: Vec<PluginMonitorDefinition>
```

默认目录 discovery 与 Claude oracle 对齐，只扫描 `.js`：

```text
<plugin-root>/workflows/*.js
<project>/.lingxi/workflows/*.js
<user-config>/workflows/*.js
```

`.mjs`、`.cjs`、`.ts`、extensionless 文件是 near-miss，不进入 named workflow inventory。
显式 `scriptPath` 不属于目录 discovery，可继续读取 workflow engine 支持的脚本。实施时删除
`tools/workflow/src/lib.rs` 中把 `WORKFLOW_EXTENSIONS=[".js", ".mjs", ".ts", ""]`
误当成 discovery contract 的分支；该数组今天只是 name→filename 探测表，不能拿去 walk 目录。

Workflow scoped name：

```text
<plugin-name>:<meta.name>
```

Named resolver 的终态顺序与 oracle 一致：**saved > plugin > builtin**。Saved workflow 以解析后的
`meta.name` 建索引，不以 filename 当 identity；Plugin workflow 使用 namespaced name；重复 name
按上述顺序取第一项。Local App 产品调用始终使用 §8.1 的 verified resolved handle，不经过这个
可被 saved workflow shadow 的通用 name resolver。

运行时继续使用现有 workflow engine：

- `export const meta` 必须是第一条语句和 literal object。
- `args` 是 Host 提供的 global。
- 支持 `agent()`、`pipeline()`、`parallel()`、`phase()`、`log()`。
- 不允许 import/module loading。
- workflow script 自身不能直接访问 shell/filesystem/Host tools。
- agents 的 tool call 继续走普通 permission 和 sandbox。
- task script copy、digest、pause/resume 使用现有实现。

### 5.6 Metadata、Themes 与 Monitors

内部 schema 增加：

```rust
PluginManifest::metadata: Option<serde_json::Value>
PluginComponents::themes: Vec<ComponentPath>
PluginComponents::monitors: Vec<PluginMonitorDefinition>
```

- `metadata` 是 inert free-form object：runtime 保留但不读取；非 object 时 runtime 忽略、
  validator warning、strict validation 失败。
- Themes 从 default `themes/` 或 `experimental.themes` 发现；扩展现有
  `tui-core/src/theme.rs` 的封闭六值选择层，使其可以引用 Plugin 提供的只读 theme，不能获得执行权限。
- Monitors 从 default `monitors/monitors.json` 或 `experimental.monitors` 发现；Plugin 层只负责
  discovery、`armedMonitorKeys` 去重、`always`/`on-skill-invoke:<skill>` activation 和 trust gate，
  执行复用现有 `MonitorRegistration → spawn_monitor → monitor_ws` substrate，不新建第二套 runner。
- Monitor command 是 unsandboxed hook-trust surface，必须走 Plugin trust/permission、env/path
  substitution 和 session 生命周期清理；Host 无 Monitor capability 时跳过并产出诊断。
- 与 oracle 一致，mid-session disable 只阻止新的 monitor activation，不强停已经运行的 monitor；
  已运行 monitor 在 session end 清理。本 Local App Plugin 不声明 monitor，因此不影响 §6.4 的
  Local App enable/disable 行为。
- `lingxi-local-app` 自身不声明 themes/monitors；移动端 corresponding registries 为空。
  这些字段进入本方案是因为 Phase 0b 修改通用 Plugin contract，不能继续声称 oracle parity
  却不实现同一 schema 中的 component slots。

### 5.7 LingXi Plugin 环境变量

新增并集中到 `branding`：

```rust
pub const PLUGIN_ROOT_ENV: &str = "LINGXI_PLUGIN_ROOT";
pub const PLUGIN_DATA_ENV: &str = "LINGXI_PLUGIN_DATA";
pub const PROJECT_DIR_ENV: &str = "LINGXI_PROJECT_DIR";
```

支持 `${LINGXI_PLUGIN_ROOT}`、`${LINGXI_PLUGIN_DATA}`、`${LINGXI_PROJECT_DIR}` 的位置与 oracle 一致：

- skill/agent prompt；
- hook command；
- MCP command/args/env/url/headers；
- LSP command/args/env/workspaceFolder；
- monitor command。

PowerShell 使用 `${env:LINGXI_*}` 转换。所有替换必须先做 path/root validation。
`userConfig` 继续使用现有 `${user_config.KEY}` substitution 与动态
`LINGXI_PLUGIN_OPTION_<SANITIZED_KEY>` env；Monitor command 不允许直接插入
`${user_config.*}`，应由 monitor-owned config/data file 读取，避免 unsandboxed shell injection。

## 6. Builtin Plugin 打包与移动端加载

### 6.1 Build-time bundle

源码目录：

```text
lingxi-code/plugins/lingxi-local-app/
```

构建时生成一个确定性 builtin archive 和 inventory：

```text
BuiltinPluginBundle
├── plugin_name = lingxi-local-app
├── source = PluginSource::BuiltIn
├── version
├── manifest_sha256
├── archive_sha256
├── sorted component inventory
├── sorted asset inventory
└── archive bytes
```

规则：

- inventory 按 `/` 规范化后的相对路径排序。
- digest 使用文件原始字节。
- 不包含 mtime、绝对路径或构建目录。
- build 拒绝 path traversal、重复 canonical path、缺失 component 和 digest mismatch。
- 不增加新压缩依赖，复用仓库现有 Plugin archive/unpack 能力。
- **asset inventory 的唯一真源是显式清单，不是目录遍历。** 清单为
  `assets/templates/<family>/r1/inventory.json`。首次迁移时由今天
  `apps/engine-mobile/src/local_app_runtime_profiles.rs` 的 `profile_file!` 调用
  一对一生成（五套合计 **112 个实际 scaffold 文件**）；Core 宏删除后，Plugin 中的
  versioned inventory 成为唯一真源。
- 当前目录还有 12 个 tracked 但未被任何 `profile_file!` 引用的文件：Canvas/Three/Phaser/
  Babylon 各自的 `app/screens/detail-screen.jsx`、`app/screens/home-screen.jsx`、
  `src/stores/app-store.js`。它们不属于生产 scaffold，迁移提交中应删除，不能为了把计数凑成
  124 而写入 inventory。
- 目录遍历只用于**反向对账**：先 prune 下列明确的本地构建产物目录，再要求剩余文件与
  inventory 完全相等；存在其他清单外文件即 build 失败。
- prune `node_modules/`、`dist/`、`.vite/`、`.lingxi-build-state/`。
  必要性：`phaser-2d/r1` 与 `babylon-3d/r1` 在跑过 `pnpm install` 的工作树上分别有
  13,014 / 20,093 个文件、合计 523MB，而 `plugin/src/mcpb.rs:16` 的
  `MAX_FILES = 10_000` 会直接拒绝——按目录遍历实施必然在 Phase 2 当场失败。

### 6.2 Mobile materialization

移动端启动时，builtin root 位于 app-private、非易失存储：iOS 使用 Application Support
并设置 excluded-from-backup，Android 使用 `Context.noBackupFilesDir`。它不是 OS 可主动清理的
cache。物化流程：

1. 读取编译内嵌 archive 和预期 digest。
2. 在该数据根目录创建 digest-versioned sibling staging。
3. 安全解包并逐文件验证 inventory/digest。
4. 原子 promote 为只读 builtin root。
5. 调用 `PluginManager` 新增的窄入口
   `register_verified_builtin(root, expected_name, bundle_sha256)`；该入口复用现有 manifest
   discovery、validation、`enable/load_plugin` 和 registries，只把已验证来源 stamp 为现有
   `PluginSource::BuiltIn`。
6. PluginManager 物化全部 declared components；本 Local App Plugin 实际非空的是
   skills、agents、workflows。

materialized read-only root 与 writable `${LINGXI_PLUGIN_DATA}` 分离；Plugin data 不能覆盖
manifest/components/templates，也不进入 bundle/profile/template snapshot digest。

**幂等短路（必须实现）**：若 `<plugin-data>/<archive_sha256>/` 已存在，先验证：

- root 是 canonical real directory，路径链没有 symlink；
- promote marker schema、plugin ID/version、archive SHA-256、inventory SHA-256、file count、
  total bytes 全部匹配编译内嵌 descriptor；
- manifest、inventory 与当前注册所需的 component entry files 均存在。

全部通过时跳过解包和全量逐文件 SHA-256；任一失败都从内嵌 archive 重建新的 sibling
staging，完成全量验证后原子 promote。Template catalog 被读取或某 template 被 scaffold 时，
Host 仍须对该 template 的选中文件按 inventory 重算 SHA-256，marker 不能替代使用点校验。

**失败态（必须实现）**：存储满、解包中断、marker/文件损坏或原子 promote 失败时：

- 返回 typed error `builtin_bundle_unavailable`（已加入 §17.1 清单）；
- **已 scaffold 的 App 仍可依 §9.6 的 per-App snapshot 正常 build/run/restore**；
- 只有 create/update/MCP revise 返回该 error，客户端必须有可渲染的失败 UI，
  不得表现为「按钮点了没反应」。
- 不回退到旧 Plugin digest 创建或修改 App；旧 root 只能在清理策略下等待回收。

注意这是本方案**新引入**的故障模式：今天模板是 `include_bytes!` 编进二进制的常量
（`local_app_runtime_profiles.rs:18-32`），不存在物化失败。

缓存路径是实现细节，不能写入 Manifest、Runtime Profile contract 或 App snapshot。

选择磁盘物化而不是只提供内存 byte service 有三个现有路径契约：Skill tool 把真实
`Base directory for this skill: <path>` 放入 prompt 并替换 `${LINGXI_SKILL_DIR}`；模型随后用
Read tool 打开 `references/*.md`；workflow engine 通过 `scriptPath` 读取、复制并持久化 resume
provenance。改成 VFS 会同时要求修改 Skill、Read、workflow、resume 和 PluginManager，不是窄接入。

移动端必须把 verified builtin root 加入 `SessionCwd.trusted_dirs` 的**只读授权面**，并由 Host
permission policy 对该 root 的 Edit/Write/NotebookEdit 明确 Deny。iOS mobile-linux 对 Host
绝对路径会 passthrough，因此不把“必须新增 guest mount”写成实现前提；若实现选择 guest path，
该 mount 必须 read-only。无论采用哪条路径，Phase 1 都必须在 release 真机从 Skill invocation
取得 base directory，再由模型侧 Read 成功打开 `references/router.md`，并证明写入被拒绝。

### 6.3 Mobile PluginManager scope

`engine-mobile` 接入现有 `plugin` crate 和 `PluginManager`，但只开放 builtin 路径：

- 不扫描 marketplace。
- 不下载、不 clone、不安装。
- 不提供 install/update/uninstall command。
- 不解析第三方 Plugin source。
- 不需要 Local App Plugin LSP、output-style 或 external Plugin MCP；对应 registry 可为空。
- 不需要 Local App Plugin theme/monitor；对应 registry 为空。
- 使用与 desktop 相同的 skill/agent/workflow materialization 代码。

移动端不实现第二套 `BuiltinPluginLoader`；builtin bundle 只是 `PluginManager` 的可信 source
provider。现有 `PluginManager::install(PluginSource::BuiltIn)` 继续拒绝，因为 builtin 不是安装
来源；composition 只能走上述 verified registration seam。

### 6.4 Enabled state

- 默认 enabled。
- 复用现有 `settings.enabledPlugins` 数据形状与 `PluginState`，key 使用 bare manifest name
  `lingxi-local-app`；不新增 `localAppPluginEnabled` 第二份布尔状态。**bare-key 的读写都是移动端
  新路径**：composition root 先读 `enabledPlugins["lingxi-local-app"]`，再按 explicit value >
  manifest `defaultEnabled` 决定 `Loaded`/`Disabled`；Native toggle 直接写同一 bare key。
  `register_verified_builtin` 只注册已验证 root，不解析 settings，也不伪装 marketplace record。
- iOS/Android 只展示一个 `LingXi Local App` Plugin toggle。
- disable 卸载 skills、agents、workflows 和 template catalog discovery。
- disable 不停止已生成 App、不删除 snapshot、不注销 active per-App MCP server。
- create/update/verify/MCP revise 在 disabled 时返回 typed `plugin_disabled`。
- UI 可以启用 Plugin 并只重试原动作一次。
- disable/re-enable 与 app restart 后从同一 settings snapshot 恢复；live opaque `PluginId`
  变化不影响该设置或 per-App provenance。

## 7. Unified Plugin Package

### 7.1 Manifest

```json
{
  "name": "lingxi-local-app",
  "displayName": "LingXi Local App",
  "version": "1.0.0",
  "description": "Create, update, run, verify and expose LingXi Local Apps through app-specific MCP tools.",
  "author": { "name": "LingXi" },
  "license": "UNLICENSED",
  "defaultEnabled": true,
  "skills": "./skills/",
  "agents": "./agents/",
  "workflows": "./workflows/"
}
```

### 7.2 Directory

```text
plugins/lingxi-local-app/
├── .lingxi-plugin/plugin.json
├── skills/
│   ├── create-local-app/
│   ├── local-app-use/
│   ├── local-app-run/
│   ├── local-app-inspect-view/
│   ├── local-app-capture-view/
│   ├── local-app-interact/
│   ├── local-app-test/
│   ├── local-app-debug/
│   ├── local-app-data/
│   ├── local-app-background/
│   ├── template-selection/
│   ├── frontend-design/
│   ├── frontend-qa/
│   ├── accessibility/
│   ├── react-best-practices/
│   ├── ionic-react-local-app/
│   ├── canvas-2d-local-app/
│   ├── threejs-local-app/
│   ├── phaser-2d-local-app/
│   ├── babylon-3d-local-app/
│   ├── llm-sidequery/
│   ├── llm-agent/
│   ├── device/
│   ├── expose-as-mcp/
│   ├── mcp-tool-design/
│   ├── mcp-flow-binding/
│   └── mcp-qa/
├── agents/
│   ├── operator.md
│   ├── tester.md
│   ├── template-selector.md
│   ├── designer.md
│   ├── builder.md
│   ├── verifier.md
│   └── mcp-designer.md
├── workflows/
│   ├── local-app-build.js
│   ├── local-app-use-test.js
│   └── local-app-mcp-authoring.js
├── assets/templates/
│   ├── catalog.json
│   ├── react-dom/r1/
│   ├── canvas-2d/r1/
│   ├── three-3d/r1/
│   ├── phaser-2d/r1/
│   └── babylon-3d/r1/
├── schemas/
│   ├── design-spec.schema.json
│   ├── use-test-report.schema.json
│   ├── qa-report.schema.json
│   └── mcp-proposal.schema.json
└── references/
```

Templates、schemas、references 是普通 Plugin assets，不是 manifest components。

### 7.2.1 Skill packaging rules

- 这是 multi-skill Plugin，不使用 root `SKILL.md` fallback；每个 skill 位于
  `skills/<name>/SKILL.md`，name/description frontmatter 与目录/registry identity 一致。
- `references/*.md`、schemas 和示例通过 skill-root-relative path 访问；materialization 后是普通
  file-backed resource，Host canonicalize 后必须仍位于该 skill root，拒绝 traversal/symlink escape。
- 不再生成另一份“bundled inline body”作为移动端真源；Plugin inventory/file bytes 是 listing、
  invocation 与 reference load 的共同来源，避免 SKILL.md 与 Rust 手写 reference list 漂移。
- 现有 skill 包中的 `agents/openai.yaml` 可以随目录保留为 LingXi/OpenAI 展示 metadata，但它
  不是 Claude Plugin Skill 契约，也不会被发现为 Plugin root agent 或拼进模型 body；功能不得
  依赖它。
- Plugin skill 保持 `CommandSource::Plugin`，即使来源为 `PluginSource::BuiltIn` 也**不伪装成
  bundled skill**；listing 按现有 oracle budgeter 正常截断。Packer 对 frontmatter description
  设 180 display-column 上限，并在 200k context（8,000-character listing budget）下验证 27 个
  namespaced skill 的完整描述仍全部可见且至少保留 15% 余量。
- `frontend-design` 的 iOS/iPadOS、Android、Desktop presentation profiles 与 Runtime Profile
  family 分离；Canvas HUD/overlay 与 DOM page 走不同 profile。
- `babylon-3d-local-app` skill 可以随 Plugin 存在，但 template selector 在 availability gate
  通过前不得返回 Babylon candidate。

新增 17 个 skill 的职责边界：

| Skill | 只负责 | 不负责 |
| --- | --- | --- |
| `local-app-use` | 识别用户意图并路由 run/inspect/interact/test/debug/data/background | 直接操作 Host、改源码 |
| `local-app-run` | start/stop/restart、等待 ready、报告 runtime identity | build/repair |
| `local-app-inspect-view` | DOM/accessibility/native inspect evidence | 用截图猜不可见语义 |
| `local-app-capture-view` | screenshot/canvas frame/dual-frame evidence | 宣称交互成功 |
| `local-app-interact` | Host typed pointer/touch/key/back actions与结果 | 注入任意 JS/坐标盲点 |
| `local-app-test` | 从 acceptance checks 生成有界 scenario 并调用 use-test workflow | 修改源码或自行判定修复 |
| `local-app-debug` | 汇总 console/log/bridge/runtime/build identity，定位证据链 | 在 debug 路径偷偷 repair |
| `local-app-data` | collections/fields/records 的 scoped 查询与经确认 mutation | 直接访问 SQLite/跨 App 数据 |
| `local-app-background` | background job/list/status/cancel 与平台限制 | 创建未声明 capability/schedule |
| `template-selection` | 从 Host semantic catalog 选最简单可用 template，输出 digest echo + ID/reasons | 返回 path/family/revision/hash 权威值 |
| `llm-sidequery` | 指导现有 `requestLlmChat/streamLlmChat/onLlmStreamFrame` 的 bounded side query | 新建 MCP server、选择 provider/model |
| `llm-agent` | 指导现有 App Persistent Agent session/profile API | 把 Persistent Agent 改成 Plugin Agent |
| `device` | 指导 `window.lingxi.v2` device context/capability/permission UX | 直接调用 Swift/Compose/Capacitor |
| `expose-as-mcp` | 区分 initial/update/standalone revise 并启动 authoring workflow | 自行注册/promote tool |
| `mcp-tool-design` | 用户 workflow → meaningful Tool definition/schema | 机械复制 CRUD/bridge API |
| `mcp-flow-binding` | ToolInput/StepOutput 与同 App typed Flow 绑定，发现缺失 Flow | JavaScript handler/跨 App binding |
| `mcp-qa` | discovery/schema/permission/side-effect/result/isolation evaluation | 用 prompt 声明“已通过”而无 Host evidence |

`llm-sidequery` 和 `llm-agent` 描述的是 Local App 已有 Host capability，不新增 Plugin MCP server，
也不改变 `AppAgentProfile`/`AgentSessionRecord`/app-scoped transport 的现有架构。

### 7.3 Agents

| Agent | 职责 | 可修改源码 | 可发布状态 |
| --- | --- | --- | --- |
| operator | 运行、查看、交互和调试已有 App | 否 | 否 |
| tester | 执行 use-test 并采集证据 | 否 | 否 |
| template-selector | 从 Host catalog 选择 template | 否 | 否 |
| designer | 生成 platform-aware design spec | 否 | 否 |
| builder | 在 staging/workspace 写 App-managed source | 是 | 否 |
| verifier | 解释 Host/test evidence 并输出 findings | 否 | 否 |
| mcp-designer | 生成单 App MCP proposal | 否 | 否 |

所有 agent：

- 不声明 `permissionMode`、`mcpServers`、`hooks`。
- 只获得职责所需 tools。
- 不能消费确认 receipt 或直接 promote active state。
- 不能修改 Runtime Profile family、template snapshot 或核心依赖。

工具边界：

| Agent | 允许的 Host 能力 | 明确禁止 |
| --- | --- | --- |
| operator | App list/get、validated selection read、runtime start/stop、logs/events、inspect/capture/act、data、background | source write、build、profile/dependency/catalog 修改 |
| tester | App get、validated selection read、runtime、logs/events、inspect/capture/act；测试计划明确要求时使用有界 data mutation | source write、build、repair、checkpoint restore |
| template-selector | verified template catalog、结构化需求、Host validate-selection handle issuance | workspace、scaffold、build、receipt |
| designer | Host-owned App evidence、validated selection read、design/runtime/platform skills | Write/Edit、build、Manifest mutation |
| builder | validated selection read、isolated staging 或本 App workspace 的 Read/Write/Edit、build、dependency proposal、checkpoint | package manager、core dependency/profile/template snapshot 修改 |
| verifier | validated selection read、App/build identity、Smoke/UseTest/QA/MCP QA reports、logs/evidence | Write/Edit、build、repair、restore |
| mcp-designer | Host-owned App evidence、validated selection read、data/Flow/capability graph、active catalog | Write/Edit、permission、receipt、catalog promote |

Builder 对正式 workspace 的写权限只在 update transaction 内存在；create 阶段只能写 Host 生成的 isolated staging。所有 agents 都不能从 prompt 接受绝对 workspace、Plugin 或 snapshot path。

`operator` 与 `tester` 不是等待移动端 Agent tool 的孤立定义。`local-app-use-test.js` 必须通过现有
workflow `agent(prompt, { agentType })` 路径调用它们：operator 按 scenario 执行 runtime/
inspect/capture/interact 并产生原始 Host evidence；tester 在不改源码的前提下复核 acceptance
checks 并生成 `UseTestReport`；verifier 再把 build、operator、tester 与 MCP QA evidence 归并为
findings。用户主动操作已有 App 时走 `local-app-use` router skills；它们不替代这两个 workflow
内部 agent。

## 8. Core 与提示层解耦

### 8.1 LocalAppPluginBinding

移动端 composition root 增加一个窄适配器：

```rust
enum LocalAppCapabilityOrigin {
    CurrentBuiltin,
    Plugin {
        live_plugin_id: PluginId,
        bundle_sha256: String,
    },
}

enum LocalAppBuildWorkflowBinding {
    Split {
        dom: WorkflowHandle,
        canvas: WorkflowHandle,
    },
    Unified(WorkflowHandle),
}

struct LocalAppPluginBinding {
    origin: LocalAppCapabilityOrigin,
    build_workflow: LocalAppBuildWorkflowBinding,
    use_test_workflow: Option<WorkflowHandle>,
    mcp_authoring_workflow: Option<WorkflowHandle>,
    template_catalog: Option<Arc<VerifiedTemplateCatalog>>,
}
```

它不是 Plugin manifest 新字段，也不是公共 Plugin 规则。它只负责把产品内置 Local App
能力绑定到已经由 `PluginManager` 验证、namespaced、注册的 component handles。

这个内部形态从 Phase -1 开始保持稳定，避免已知返工：

- Phase -1：`origin=CurrentBuiltin`，`build_workflow=Split { dom, canvas }`，handle 来自现有
  `BuiltinWorkflowRegistry`，三个 Phase 2 capability slots 为 `None`；
- Phase 1/2：切换为 `origin=Plugin { ... }` 并填充 verified capability slots，但 build 可暂时
  保持 `Split`；
- Phase 4：两个脚本的行为和测试迁完后原子切换为 `Unified`；
- Phase 9：删除 `CurrentBuiltin` 与 `Split` 分支，终态只允许 Plugin + Unified。

launch-context enricher 和 Host 调用端始终只依赖外层 binding，不因来源切换或 workflow 合并
再次改接口。binding 提供会返回 typed unavailable error 的访问方法，不允许调用端直接
`unwrap` optional slot。Phase 9 completion gate 要求 origin=Plugin、build=Unified 且三个 slot
全部为 `Some`，随后可删除实施期 optional/transition 分支。`CurrentBuiltin`/`Split` 不进入公共协议
或持久化状态。

Resolved handle 启动时还生成一个 caller 不可设置的 typed task scope：

```rust
struct LocalAppWorkflowTaskScope {
    app_id: String,
    purpose: LocalAppWorkflowPurpose, // Build | UseTest | McpAuthoring
}
```

它随 `TaskSpawnInput::LocalWorkflow`/task state 持久化。`tasks` 对所有三种 purpose 都让 App
delete guard 按 app ID 阻塞；只有 `Build` 取得 exclusive workspace permission lease。Mobile
composition 只对 `Build` 从同一 scope 读取该 App 的 `workflowModel` 并作为 default。删除
`tasks/src/lib.rs` 和 `tools/workflow/src/lib.rs` 两份
`LOCAL_APP_BUILD_WORKFLOWS` 及 generic `apply_local_app_build_default_model`。任意自定义 workflow
即使伪造 `meta.name="local-app-build"` 或 `args.app_id` 也拿不到这个 Host marker。

只有 Plugin package、composition binding 和测试可以出现三个 Local App workflow basename。以下
模块不能再出现 skill/agent/workflow 名字或按 basename 推导 Local App authority：

- `apps/engine-mobile/src/workflow_support.rs`
- `apps/engine-mobile/src/local_apps_host.rs`
- `apps/engine-mobile/src/lib.rs` 的 mobile skill registration
- 通用 Plugin/Workflow/MCP core，包括 `tasks/` 与 `tools/workflow/`

### 8.2 声明式 inventory

- 删除 `register_mobile_skill_commands` 的 Local App 名单。
- PluginManager 注册 inventory 中的全部 skills。
- listing、Skill tool 和 slash dispatch 读取同一个 live registry。
- 测试断言能力、source、namespace 和 body identity，不钉死第二份名字数组。

### 8.3 Unified workflow binding

只保留一个 build workflow，因此删除 Profile→workflow 名映射。

`LocalAppPluginBinding` 为 resolved build workflow 安装 Host-owned launch-context enricher：

1. 外部 args 只允许 operation、app_id、spec、revision_prompt、quality_level。
2. Host 用 app_id 读取 record mirror、Manifest、Runtime Profile、dependency snapshot 和 collection IDs。
3. Host 验证 scaffold/profile/snapshot 完整性。
4. Host 覆盖写入不可由 caller 提供的 `host_context`。
5. workflow 只能读取该 context，不能用 args 覆盖它。

Create shell 的初始 `host_context` 只包含 verified template catalog identity；update/verify 包含
持久化 Profile 和 template snapshot identity。Create launcher **不得**调用当前只接受 persisted
Profile 的 `detect_build_target` 路径。Workflow script 本身不能调用 Host tool，也不能在启动后
改写 `host_context`：template-selector agent 先调用 Host `validate_template_selection` tool，Host
把权威选择写入 run-scoped candidate journal，并把不可预测的 `validated_selection_handle` 返回给
该 agent；workflow 只接收并传递 handle。后续 designer/builder/tester/verifier 各自调用只读 Host
`get_validated_selection(handle)` 重新取得 family/revision/contract/policy。所有 staging/build/
scaffold Host call 也必须提交 handle，Host 每次从 journal 重解并校验 app/run/catalog identity，
忽略 agent result/prompt 中复制的 family/revision。这样不增加非官方 workflow primitive，也不把
Agent 的 structured output 升格为权威。
Update/verify 继续只接受 persisted binding。Resume 从 candidate journal 恢复 handle，不从
template ID、brief、imports 或 package.json 重推 Profile。

### 8.4 Workspace contract

`LINGXI.md` 只描述：

- App 当前 Runtime Profile；
- Host-managed 与 App-managed files；
- 构建、测试和恢复能力；
- 禁止 package manager、核心依赖和 profile 修改。

它不再写 `local-app-build`、`local-canvas-build` 或任何 skill/agent 名称。

### 8.5 防回归门

增加生产代码扫描：

- Local App component names 只能存在于 Plugin package、composition binding 和测试 fixtures。
- `engine-mobile` 普通业务模块以及 `tasks/src`、`tools/workflow/src` 出现 component literal 时 CI 失败。
- Scanner 自带 planted-positive test，不能只断言当前仓库 exit 0。
- **needle 集合从 Plugin discovery/build 产出的 verified component inventory 派生，
  不从原始 manifest 字段猜，也不得手写静态数组。** 默认目录会被自动发现，manifest 只声明
  component roots，单靠 manifest 不能得到所有 basename。否则 Plugin 新增或重命名一个
  component 后，host 重新硬编码它时 scanner 会静默放行。
- Phase -1 尚无 Plugin inventory，scanner 暂时读取现有 live builtin skill/workflow registries；
  Phase 2 在同一个 scanner 输入 seam 切换为 PluginManager 的 verified inventory。两阶段都禁止
  在 scanner 内维护第二份名称数组。
- `register_verified_builtin` 在 `engine-mobile` 只允许 composition module 一个调用点；scanner
  对第二个调用点给出 file:line。Bundle descriptor 模块之外不得 `include_bytes!`/`include_str!`
  引用 `local-apps/templates/**` 或 `plugins/lingxi-local-app/**`；迁移前的已知调用点用精确
  file:line/count baseline 管理，并在 Phase 9 归零，不使用泛目录 allowlist。
- **扫描面是 deny-by-default 的目录枚举，不是文件白名单**；新增 engine-mobile 模块默认被扫。
- **needle 必须同时覆盖 basename 与 namespaced FQN**（`lingxi-local-app:local-app-build`），
  否则 host 模块拼 FQN 或拆串即可绕过按 basename 的匹配。
- scanner allowlist 条目数入 CI 基线，条目增加需显式改基线。
- **新增**品牌 scanner 并接入 CI（今天它只存在于
  `.claude/worktrees/agent-namespace-plan-a/lingxi-code/scripts/`，main 上没有），
  禁止产品 surface 回流 `.claude-plugin`/`CLAUDE_PLUGIN_*`/`CLAUDE_PROJECT_DIR`，
  只豁免 oracle docs、品牌归一化 fixtures 和明确标注的上游协议字段。生产目录
  `plugins/lingxi-local-app/` **不得整体豁免**，否则真正的品牌回流会被静默放过。

## 9. Template Assets 与 Runtime Profile

### 9.1 迁移现有五套 scaffold bundle

以下目录已经同时包含 profile-managed 和 editable scaffold files，是生产模板，不重新设计另一套：

```text
local-apps/templates/runtime-profiles/react-dom/r1
local-apps/templates/runtime-profiles/canvas-2d/r1
local-apps/templates/runtime-profiles/three-3d/r1
local-apps/templates/runtime-profiles/phaser-2d/r1
local-apps/templates/runtime-profiles/babylon-3d/r1
```

**搬迁的权威文件集是 `profile_file!` 宏清单（五套合计 112 个实际 scaffold 文件），
不是目录中的 124 个 tracked 文件**——见 §6.1。其差值是四个 Canvas family 各 3 个
从未进入 production scaffold 的孤儿文件，迁移时删除。`phaser-2d/r1` 与
`babylon-3d/r1` 在本地工作树上
分别有 13,014 / 20,093 个文件（523MB 构建产物），按目录搬迁会当场超出
`plugin/src/mcpb.rs:16` 的 `MAX_FILES = 10_000`。

实施时将清单列出的文件逐字节搬入：

```text
plugins/lingxi-local-app/assets/templates/<family>/r1
```

搬迁门：

1. 从 `(family, path)` 宏调用生成一次性 migration manifest，条目数必须是 112；
2. 搬迁前后该 manifest 逐条 raw-byte digest 一致；
3. Plugin versioned inventory 与 migration manifest 的 canonical path 集合完全相等；
4. 12 个已知孤儿文件在旧模板树与新 Plugin asset tree 中都为零命中；
5. Runtime Profile contract SHA-256 保持一致。

Core 宏和旧模板目录删除后，测试只读取 Plugin inventory，不保留第二份 112 路径列表。

旧 `vite-react-static-v1`、`vite-react-canvas-v1` 只在所有引用清理后删除。
`local-apps/src/permissions.rs` 当前 include 的 `.lingxi/settings.local.json` **不是测试 fixture**，
而是 `AppService::create` 为每个 workspace 写入的生产 build-agent 授权。将字节移到
`local-apps/assets/default-workspace-settings.local.json` 这类 Host-owned production asset，
保留 `save_workspace_permission_settings_initialized` 的 create-transaction 写入点；Plugin template
不能覆盖或提供这份安全策略。

### 9.1.1 Runtime Profile catalog authority

Template asset 放进 Plugin 不改变 Runtime Profile 的 Host 权威模型：

- 全局 verified catalog 提供五个 `family + revision + contract_sha256`；
- 每 App Manifest 保存自己的不可变 binding；
- Plugin `catalog.json` 提供 Agent 选择语义，Host 从 verified inventory/core package descriptor
  重算 contract，不能信任 Agent 或 raw asset 中自报的 hash；
- App 业务源码变化只改变 build input hash，不改变 Profile；Profile hash 使用 versioned seed
  inventory 的原始字节，不读取当前 workspace；
- dependency snapshot 独立于 Profile，普通 registry package 变化不改 family/revision；核心依赖
  只能由显式同-family migration 改变；不自动迁移、不按 imports/package.json 猜测。

Revision 1 以当前源码为 byte-preserving baseline：

| Family | Surface | 额外核心包 | Availability |
| --- | --- | --- | --- |
| `react_dom` | DOM | 无 | available |
| `canvas_2d` | Canvas | 无 | available |
| `three_3d` | Canvas | `three@0.185.1` | available |
| `phaser_2d` | Canvas | `phaser@4.2.1` | available |
| `babylon_3d` | Canvas | `@babylonjs/core@9.22.1`、`@babylonjs/loaders@9.22.1`、`@babylonjs/havok@1.3.14` | unavailable pending device gate |

五个 family 共用：

```text
toolchain = pnpm@11.22.0/node@24.18.1
@ionic/react = 9.0.0
@ionic/react-router = 9.0.0
@vitejs/plugin-react = 6.0.4
react = 19.2.8
react-dom = 19.2.8
react-router = 6.30.6
react-router-dom = 6.30.6
vite = 8.2.1
zod = 4.4.3
zustand = 5.0.15
```

Revision 1 是 byte-preserving baseline；contract digest 是迁移保真度的度量对象，而不是为了兼容
任何会被保留的预发布 binding。迁移后的 canonical descriptor 必须继续覆盖：

```text
family + revision + surface + toolchainKey
+ sorted corePackages
+ sorted managed-file raw-byte SHA-256
+ sorted editable-file raw-byte SHA-256
+ sourceSeedSha256 aggregate + sourceSeedId
+ baseLockfileSha256
```

mtime、绝对路径、materialized Plugin root、App 当前业务源码与 dependency install directory
不进入 contract。未来 revision 只新增，不重写已经发布的 descriptor/digest；本次 schema v3
发布不提供任何虚构 migration edge。

### 9.2 Template catalog

`assets/templates/catalog.json` 每项包含：

```json
{
  "templateId": "phaser-2d-r1",
  "family": "phaser_2d",
  "revision": 1,
  "surface": "canvas",
  "summary": "Scene-based 2D game runtime with sprite, input and collision support.",
  "recommendedFor": ["multi-scene game", "sprites", "tilemap", "collision"],
  "notFor": ["ordinary forms", "static data pages"],
  "contractSha256": "...",
  "inventorySha256": "...",
  "available": true
}
```

规则：

- Catalog semantic fields 供 Agent 选择。
- Host 从 verified bundle 重新计算 family/revision/surface/digests。
- Agent 只能返回 template ID 和理由，不能返回 path、family、revision 或 digest 权威值。
- Catalog 不把 unavailable template 当作可选项返回给 Agent。

### 9.3 初始 availability

| Template | Profile | 初始状态 |
| --- | --- | --- |
| react-dom-r1 | react_dom r1 | available |
| canvas-2d-r1 | canvas_2d r1 | available |
| three-3d-r1 | three_3d r1 | available |
| phaser-2d-r1 | phaser_2d r1 | available |
| babylon-3d-r1 | babylon_3d r1 | unavailable |

Babylon 只有在 iOS/Android Babylon + glTF + Havok IIFE 真机 spike 全部通过并提交 evidence 后才进入 available catalog。存在模板文件不等于已通过 availability gate。

### 9.4 Template Selector

输入：

- 用户需求；
- 目标平台和 form factor；
- verified catalog semantic view；
- quality level。

输出：

```json
{
  "catalog_digest": "...",
  "template_id": "canvas-2d-r1",
  "reason": "The app needs a lightweight drawn simulation without scene management.",
  "rejected": [
    {
      "template_id": "phaser-2d-r1",
      "reason": "A full game engine is unnecessary."
    }
  ],
  "validated_selection_handle": "vsel_..."
}
```

前四项是 selector proposal；Agent 必须把它们提交给 Host validation tool，并把 tool 返回的
`validated_selection_handle` 原样放进最终 structured result。自行编造 handle 在任何下游调用都失败。

选择规则：

- 普通数据、表单、列表和多页面 App → react-dom。
- 自定义轻量 2D 绘制/仿真 → canvas-2d。
- scene/sprite/tilemap/collision 为核心 → phaser-2d。
- 自定义轻量 3D scene/visualization → three-3d。
- 需要 Babylon asset pipeline、glTF、Havok 或大型场景管理，且 catalog available → babylon-3d。
- 选择满足需求的最简单模板。
- 用户明确的功能约束优先，但不能选择 unavailable template。
- 需求足够时不提问；真正影响行为、平台或数据契约的缺口才返回聚焦问题，问题数量不固定。

### 9.4.1 Host verified template catalog API

本文多处提到「Host verified template catalog」（§3、§7.3、§10.1、Phase 3），
此处给出它的契约，否则实施者无从下手——§11.1 的 `host_context.template_catalog`
只带 `digest` 与 `available_template_ids`，**语义字段不经 host_context 下发**。

```text
LocalAppTemplateCatalog({}) -> {
  catalog_digest: String,
  templates: [{
    templateId, surface, summary, recommendedFor, notFor
  }]
}
```

规则：

- 只返回 `available == true` 的项（§9.3）。
- **不返回** path、family、revision、digest——Agent 不得持有这些权威值（§9.2）。
- Catalog 是本次 Host/workflow session 的只读全局能力，不接受模型提供的 `app_id`；目标 App
  和调用权限由 connection/session scope 决定，避免额外的跨 App spoof surface。
- 只有 `template-selector` 可调用（§7.3 工具边界）。
- Agent 先以结构化参数把 `catalog_digest` echo + `template_id` + `reason` + `rejected` 交给
  Host validation tool；最终输出再附上 Host-issued handle。`catalog_digest` 只是
  optimistic-concurrency token，不是选择权威。
- **anti-stale 由 Host 侧完成**：Host 重新读取当前 verified catalog，要求 echo 与当前 digest
  完全相等，再独立解析 template ID 对应的 family/revision/surface/contract/inventory digests；
  不一致则返回 `catalog_stale` 并最多重跑 selector 一次。
- Agent 不能返回或发明 path、family、revision、contract digest 或 inventory digest。
  允许 echo Host-issued catalog digest 不违反该原则，因为 Host 不信任其内容，只用它检测竞态。

### 9.5 Validated selection 与 receipt

Host validation tool 将 selector proposal 转换为：

```rust
struct ValidatedTemplateSelection {
    template_id: String,
    plugin_name: String,
    plugin_bundle_sha256: String,
    catalog_sha256: String,
    runtime_profile: AppRuntimeProfileBinding,
    template_inventory_sha256: String,
    reason: String,
    rejected: Vec<RejectedCandidate>,
}

struct RejectedCandidate {
    template_id: String,
    reason: String,
}
```

`rejected` 仅用于确认 UI，不进入 approval digest。Host 丢弃不在当前 catalog 中的 rejected ID，
防止 Agent 把任意字符串伪装成候选。权威 selection 连同 app ID、workflow run ID 和 catalog digest
存进 candidate journal；tool 返回 `{ validated_selection_handle, display_selection }`，workflow 只保存
handle，不能自行构造 binding。

Native create receipt 绑定：

```text
app_id
name + brief digest
plugin bundle digest
template ID + inventory digest
Runtime Profile binding + contract digest
dependency input digest（requested declaration + effective package + base lock/toolchain input）
design contract digest
initial MCP approval-contract digest（tool surface + semantic Flow refs + permission ceiling）
expiry + nonce
```

Receipt 时安装后的 `AppDependencySnapshot` 尚不存在，因此不得绑定 tree/SBOM snapshot digest。
Host 使用与 `scaffold_artifacts_for_binding` 同源的 canonical 安装前输入计算
`dependency_input_sha256`；安装后的 lock/tree/SBOM 写入 journal `built` 与最终 Manifest snapshot。
Receipt 消费后若 requested/effective/base-lock 输入变化，approval 立即失效并点名 dependency input
drift。

Receipt 为 per-App 单槽、10 分钟 TTL、不可预测 ID、一次性消费；新 receipt 原子作废旧 receipt。

### 9.6 Per-App template snapshot

Scaffold commit 将精确模板写入 Host-owned per-App snapshot：

```text
<app-data>/templates/<snapshot-digest>/
├── descriptor.json
├── inventory.json
└── files/
```

`descriptor.json` 记录 `pluginName=lingxi-local-app`、source=`BuiltIn`、plugin bundle SHA、
template ID、family/revision/contract SHA、inventory SHA 和创建时间；不记录 live `PluginId`
或 materialized root 绝对路径。

此后：

- Runtime Profile 仍是 build/runtime 路由权威。
- Template snapshot 是 managed-file 恢复来源。
- App-managed source 永不由 Plugin update 或模板恢复覆盖。
- Plugin disabled 或 bundle 不再加载时，已 scaffold App 仍能 build/run/restore。
- Build 不读取”最新 Plugin template”替代 App snapshot。

**实现现状（复核 2026-09-04）**：以上 per-App snapshot 机制尚未在任何语言中落地——
`grep -rn 'templates/' lingxi-code/local-apps/src/*.rs` 只命中 `packer.rs:21` 的一行注释，
没有代码写入 `<app-data>/templates/<snapshot-digest>/{descriptor.json,inventory.json,files/}`。
在此之前，`lingxi-code/apps/engine-mobile/src/local_app_runtime_profiles.rs` 的
`published_available_contracts()`（同文件 `r1-critic-05` 注释）已标注了对应风险：已
scaffold App 的 Manifest 若 pin 住某个 revision，而该 revision 之后从
`published_available_contracts()` 下线，App 目前没有任何恢复来源。退役一个仍被在用 App
pin 住的 revision，只能配合 App 侧迁移，或等 §9.6 真正落地后再做。

## 10. Create、Update 与 Verify

### 10.1 Create

```text
create empty shell
→ normalize user requirements
→ template-selector reads verified catalog
→ template-selector calls Host validation; Host persists run-scoped candidate and returns validated_selection_handle
→ Host prepares isolated staging from that handle
→ designer produces design_spec
→ builder writes App-managed source in staging
→ create runs with create_without_mcp=true: no MCP authoring here, MCP stays unconfigured (§1.7 实现现状注)
→ missing Flow changes return to builder（repair budget 见 §11.4）
→ Host validates source/profile/dependencies/MCP proposal
→ Native create confirmation
→ receipt
→ atomic scaffold + profile + dependency + template snapshot commit
→ build staging
→ Host smoke
→ profile-specific use-test and QA
→ MCP QA
→ atomic build + catalog promote (§16.4)
```

在 receipt 消费前，正式 workspace、Manifest、record 和 active catalog 不发生部分写入；selection、
design、source 与 proposal 只存在于 run-scoped candidate journal/staging。Receipt 消费后，
scaffold/Profile/dependency input/template snapshot 以 `draft` 原子提交；candidate build/catalog 在最终
promote 前仍留在 staging。

### 10.2 Update

```text
load persisted Profile + template snapshot + active MCP catalog
→ checkpoint
→ designer impact analysis
→ builder minimal App-managed changes
→ MCP authoring impact-check
→ unchanged or revised proposal
→ 若 approval contract（tool surface/semantic Flow/ceiling）变化，展示 Native proposal diff 并消费 receipt；完全不变则不创建 MCP receipt
→ build/smoke/profile QA/MCP QA
→ atomic promote
→ failure restores previous source/build/catalog
```

Update 不重新选择 template 或 Runtime Profile family。

### 10.3 Verify

```text
validate active build identity
→ profile-specific use-test
→ MCP QA
→ verifier produces structured findings/report
```

Verify 无 Write/Edit/Build/repair 权限。需要修复时显式进入 Update。

### 10.4 Design contract

`design_spec` 必须包含：

- 每个平台的 `{os, form_factor, presentation, navigation}`；
- information architecture 和 loading/empty/error/success/permission states；
- visual tokens；
- pointer/touch、keyboard/mouse、back、reduced motion；
- shared logic 与 platform-specific shell；
- accessibility requirements；
- acceptance checks；
- Runtime Profile family mirror。

平台 presentation profile 与 Runtime Profile 独立。多平台不得只写“responsive”。

## 11. Workflows 与 QA 强门

### 11.1 Build workflow

```text
lingxi-local-app:local-app-build
```

外部输入：

```ts
{
  operation: "create" | "update" | "verify",
  app_id: string,
  spec?: string,
  revision_prompt?: string,
  quality_level?: "fast" | "balanced" | "thorough"
}
```

禁止外部输入：

```text
runtime_profile
renderer
surface
template_id
template path/revision/digest
workspace path
expected collections
host_context
```

Host launch enricher 写入：

```ts
host_context: {
  source: "verified_host",
  app_id: string,
  scaffolded: boolean,
  runtime_profile?: { family, revision, contract_sha256 },
  template_snapshot?: { id, digest },
  dependency_snapshot?: { digest },
  expected_writable_collections: string[],
  template_catalog?: {
    digest,
    available_template_ids: string[]
  }
}
```

Update/verify 根据 launch context 选择 runtime specialist。Create 的 initial context 没有 Profile；
selector agent 返回 opaque handle 后，每个下游 agent 先用 Host read tool 解引用并选择 specialist。
Workflow 外层只处理统一的 report/finding schema，profile-specific hard gate 由 Host tool/policy 执行，
不从 selector agent 回显的 family 做安全决策，也不从 brief、imports 或 package.json 推断。

### 11.2 Profile verification policy

```rust
enum LocalAppVerificationPolicy {
    Dom,
    Canvas2d,
    Three3d,
    Phaser2d,
    Babylon3d,
}
```

Policy 由 Host 从 persisted/validated Runtime Profile 派生，并作为 schema/prompt selector，不由模型返回。

#### DOM

- DOM/accessibility tree 非空。
- 关键路由和表单可以操作。
- WebView checked。
- 无 canvas 时 `render_check.status=not_applicable` 合法。
- 嵌入 canvas/WebGL 时至少 capture 一帧。

#### Canvas2D

- `canvas_surfaces >= 1`。
- `frames_captured >= 1`。
- 必须描述实际画面证据。
- 动态 surface 必须比较两个不同时间帧。
- `motion_check.frames_compared >= 2`。
- pointer/touch 和 keyboard 均覆盖设计动作。
- phase/pause/resume/resize/reduced-motion 检查。

#### Three/Phaser/Babylon

继承 Canvas 强门，并增加：

- engine/scene lifecycle；
- context loss/recovery；
- resize/DPR；
- resource dispose；
- engine-specific adapter；
- frame-time/FPS evidence（第一版仅记录，不在没有设备基线时伪装 hard gate）；
- Phaser phase/scene transitions；
- Babylon glTF/Havok asset/runtime checks。

任何 Canvas 家族不得使用仅 DOM inspect 的成功作为 render evidence。

### 11.3 Use-test workflow

```text
lingxi-local-app:local-app-use-test
```

输入只包含 app_id、scope、scenario policy。Host 注入 build/profile identity。
Workflow 先调用 `operator` 取得 runtime/interaction/render evidence，再调用 `tester` 对 acceptance
checks 和 profile policy 生成结构化报告；两者都通过 workflow 内现有 `agent()` primitive 启动，
不依赖移动端不存在的独立 Agent tool。

输出 `UseTestReport` 绑定：

```text
app_id
build_id
runtime profile contract
verification policy
scenario results
render_check
motion_check when required
logs/console/errors
bridge/data roundtrip
evidence handles
```

Tester 不修改源码、不 build、不 repair。

### 11.4 Repair rounds

- Finding 使用结构化 object，不接受自由字符串作为唯一契约。
- 未识别 finding shape 必须 fail closed 为 synthetic blocker。
- 每轮非空 blocking finding 集合消耗一次 repair round；同轮 findings 一起进入 repair prompt，
  不按 finding 数重复扣轮次。
- `webview_checked`、render/motion/data gates 失败作为普通 blocking finding 进入 repair，不在 finding 计算前抛出不可恢复错误。
- repair budget 固定为 fast=1、balanced=1、thorough=2；Canvas family 拒绝 fast。每一轮最多修复
  当前整组 blocking findings，但轮数只在一次 repair agent round 完成后增加，budget+1 时返回
  `repair_budget_exhausted` 并点名 quality level、used/limit 与剩余 finding IDs。
- repair prompt 必须携带 design、Runtime Profile、verification evidence 和 previous changes。

## 12. Per-App MCP Authoring

### 12.1 定位

Plugin 不为所有 App 提供一组通用 tools。`mcp-designer` 基于单个 App 的：

- 用户当前目标；
- 名称、brief 和 design spec；
- Runtime Profile；
- data collections/fields；
- Flow definitions；
- Host capability graph；
- active build identity（首次 create 时缺席，使用 staging source identity）；
- smoke/use-test/QA evidence（首次 authoring 时可缺席，MCP QA 前补齐）；
- 当前 active MCP catalog（首次 create 时缺席）；

实时生成该 App 独属 proposal。

### 12.2 Authoring workflow

```text
lingxi-local-app:local-app-mcp-authoring
```

外部输入：

```ts
{
  app_id: string,
  user_goal: string
}
```

不接受：

- raw tool definitions；
- server name；
- annotations；
- permission rules；
- Flow ID；
- workspace path；
- catalog/proposal digest。

流程：

```text
Host-owned AppEvidence
→ mcp-designer
→ needs_input? return focused questions and resume
→ required_flow_changes? builder + refresh evidence + rerun
→ Host schema/binding/capability validation
→ Host derives annotations/searchHint/permission ceiling
→ canonical approval-contract digest（不含 source bytes/final build ID）
→ Native approval (create: unified create receipt; update/revise: MCP proposal receipt)
→ candidate catalog + private logical server
→ MCP discovery/call/error/isolation QA
→ atomic active catalog promote
→ tool surface 变化时才发送 notifications/tools/list_changed
```

Create 不弹两张确认 sheet：initial MCP proposal 并入 §17.3 的 unified create confirmation，
同一 receipt 同时绑定 template/Profile/dependencies/design/MCP。已有 App 的 update 或 standalone
revise 才使用独立 MCP proposal diff/approval receipt；两类 receipt 都遵守 per-App 单槽、TTL、
replay/cross-App/supersede 规则。

`approval_contract_sha256` 覆盖用户可审阅且影响授权的 design contract、template/Profile、
dependency input、完整 tool surface（含 title/description/input/output schema/annotations/execution/
icons/可见 `_meta`）、业务 Flow semantic references、required Flow changes、excluded capabilities 与
Host-derived permission ceiling。它**不覆盖** App source bytes、final Flow/build IDs 或 QA/build
provenance。Source-only repair 在这些审阅面都不变时复用已消费 receipt；任一审阅面变化都必须生成
新 confirmation/receipt。Update 若 approval contract 完全不变，不创建 MCP approval；若变化，
必须先展示 §17.4a diff，不能从 revised proposal 直接 promote。

### 12.3 Tool quality gate

每个 published App 有 1–16 个 tools（§1.7）。下界只表达产品能力，不能代替质量门；
每个 tool 必须：

- 对应用户或 App 的明确业务目标；
- 与同 App 的一个 typed Flow 绑定；
- 有具体 title/description；
- 有封闭、有界 inputSchema；
- 修改类 tool 明确副作用和失败语义；
- 能被 MCP QA 实际调用或验证；
- 不与同 App 其他 tool 重复；
- 不只是内部 CRUD/bridge API 的机械重命名；
- 优先覆盖用户可以完成的完整 workflow，而不是把每个底层 Flow step 暴露成 tool；
- 列表/搜索结果有界并带 cursor/has_more，错误给出可执行的恢复建议；
- 返回高信号 structured content，避免把大段日志、画布帧或内部状态直接塞入上下文。

Authoring 还执行 deterministic definition budget：按 `tool-api/src/wire.rs` 的 fallback 口径，
对每个 provider-visible tool 累加 UTF-16 units 的 `name + description +
JSON.stringify(input_schema)`，再按 2.5 chars/token 估算。单 App 上限 **2,048 tokens / 5,120
UTF-16 units**，conversation exposed set 聚合上限 **16,384 tokens / 40,960 units**。超限 proposal
返回 `proposal_invalid`，按 server/tool 给出贡献排序；不得截断 description 或 JSON Schema。
2,048 上限使 8 个满额 App 可以同时满足 §15.3 的聚合预算；单字段的 1,000/32 KiB 上限仍只是
拒绝异常值的安全 ceiling，不保证所有字段同时取满也能过 aggregate gate。

无法生成任何有意义 tool 时，authoring 返回 `mcp_authoring_required`，把原因写入
`excluded_capabilities`，保留 candidate journal 与用户已经完成的 staging 工作，但不生成
create receipt、不切换 active build/catalog。绝不制造 filler tool 凑数。

用户可继续对话补足想让 LLM 完成的业务任务；已有 published App 也可通过
`expose-as-mcp` 的 standalone authoring 修改或补充 tools。

### 12.4 AppMcpProposal

```rust
struct AppMcpProposal {
    app_id: String,
    manifest_revision: u64,
    user_goal_sha256: String,
    summary: String,
    tools: Vec<AppMcpToolProposal>,
    required_flow_changes: Vec<RequiredFlowChange>,
    excluded_capabilities: Vec<ExcludedCapability>,
}
```

Agent 只提议：

- tool name/title/description；
- inputSchema/outputSchema；
- 业务 Flow semantic reference；
- required flow changes；
- excluded capability 和理由。

Host 派生且 Agent 不可设置：

- server identity；
- connection scope；
- final Flow ID/build binding；
- annotations；
- icons；
- execution；
- `_meta`；
- permission ceiling；
- rate limit/timeout；
- catalog digest。

### 12.5 Typed Flow binding

Conversation tool 不继续使用 `FlowDefinition.steps[].input_json` 的固定字面量。目标模型：

```rust
enum FlowValueBinding {
    Literal(Value),
    ToolInput {
        json_pointer: String,
    },
    StepOutput {
        step_id: String,
        json_pointer: String,
    },
}

struct AppMcpFlowBinding {
    flow_id: String,
    inputs: BTreeMap<String, FlowValueBinding>,
    result: FlowValueBinding,
}
```

Host validation：

- `flow_id` 对 create 必须属于同一 App 的 staging source，对 update/revise 必须属于 candidate 或
  active build；final build binding 在 MCP QA 时由 Host 重新派生，receipt 前不假设 candidate build 已存在。
- ToolInput pointer 必须落在 tool inputSchema 声明范围。
- StepOutput 只能引用 DAG 中已经完成的前序 step。
- StepOutput pointer 必须落在该 step 的 output schema。
- 最终 result 必须满足 tool outputSchema。
- 每个 AppMcpFlowBinding 最多 32 steps；JSON Pointer 最多 512 UTF-8 bytes/32 segments；binding
  tree 最大深度 8；单 step 中间 structured value 最大 64 KiB、一次调用累计 256 KiB。
  这些限制只作用于 Conversation MCP binding，不把现有 generic `FlowDefinition` 的 128-step/
  15-minute 上限改小；Local App MCP call 仍受 §14.4 的 5-minute timeout。
- 不接受模板表达式、JavaScript、字符串插值、动态 Flow ID 或跨 App reference。
- V1 拒绝递归 FlowExecute、LLM、Agent、BackgroundSchedule 和依赖交互式 WebView/device UI 的 steps。

## 13. 标准 MCP 契约

### 13.1 McpToolDto

```rust
pub struct McpToolDefinitionDto {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub annotations: Option<McpToolAnnotationsDto>,
    pub execution: Option<McpToolExecutionDto>,
    pub icons: Vec<McpIconDto>,
    pub meta: Option<Value>,
}

pub struct McpToolDto {
    pub server_name: String,
    pub full_name: String,
    pub definition: McpToolDefinitionDto,
    pub search_hint: Option<String>,
    pub always_load: Option<bool>,
    pub effective_max_permission: Option<McpPermissionCeiling>,
}
```

`McpToolDefinitionDto` 是 MCP 2025-11-25 Tool wire shape；`tools/list` 只序列化它。
`server_name/full_name/search_hint/always_load/effective_max_permission` 是 LingXi Host enriched
projection：前两个描述连接与 FQN，`search_hint/always_load` 从标准 `_meta` 中投影，permission
ceiling 按连接/组织策略派生。它们用于 registry、tool search、Native UI 和授权，**不得作为
非标准顶层字段混入 MCP wire**。

Local App logical hub 的 initialize/version negotiation 复用
`mcp::initialize_params::LATEST_PROTOCOL_VERSION`（当前 `2025-11-25`）。不得从
`apps/cli/src/commands/mcp.rs` 复制其独立 server 常量；若后续统一 CLI server 版本，应作为
共享 MCP 变更单独验证，不改变本方案的 Local App contract。

规则：

- `_meta` 完整透传；known fields 只是投影。
- outputSchema 存在时必须校验 structuredContent。
- annotations 是提示，不是授权。
- Local App V1 `execution.taskSupport=forbidden`。
- MCP CallToolResult 保留 content、structuredContent、isError、`_meta`。
- initialize 使用结构化 serverInfo/instructions/capabilities，不再只有布尔投影。

### 13.2 Local App Tool schema limits

以下是 Host 对 **generated Local App catalog** 的产品预算，不是对所有第三方 MCP server 的
协议重定义；generic MCP client 继续按标准接受上游 Tool，并只受既有 transport/resource limits。

- name：canonical lower snake case，1–64，匹配 `^[a-z][a-z0-9]*(?:_[a-z0-9]+)*$`；禁止连续或首尾 `_`。
- title：最多 100 字符。
- description：最多 1000 字符。
- input/output schema：各最多 32 KiB、最大深度 8。
- object root 必须封闭；禁止 remote `$ref` 和无界组合。
- tool result structuredContent 最大 64 KiB。
- 单 App 1–16 tools（§1.7）。

### 13.3 Host-derived annotations

| Annotation | 派生规则 |
| --- | --- |
| readOnlyHint | 所有 steps 均只读 |
| destructiveHint | 任一 step 删除、覆盖、发送或有不可逆外部效果 |
| idempotentHint | Host 能证明所有 mutation 幂等 |
| openWorldHint | 任一 step 访问网络、设备或外部实体 |

不能证明时使用保守默认：

```text
readOnlyHint=false
destructiveHint=true
idempotentHint=false
openWorldHint=true
```

### 13.4 Host-derived `_meta`

- 默认 `alwaysLoad=false`。
- Host 可从 App 名称/brief/tool semantic 生成有界 searchHint。
- 需要每次明确人类同意时生成 `anthropic/requiresUserInteraction=true`。
- Vendor `_meta` 是技术协议字段，不改变产品品牌显示。
- Local App Tool icons 默认为空；若使用 App icon，只能引用 Host 验证、同 App scope 的资源，
  不接受 Agent 提供的 remote URL、绝对路径或任意 data URI，且单 icon decoded bytes ≤ 64 KiB。

### 13.5 `tools/call` 执行与错误分层

```text
tools/call
→ resolve app_id + last-listed tool_surface_sha256 from connection scope
→ load current active catalog_sha256/execution binding by app_id
→ reject stale surface, unknown catalog or unknown tool
→ validate inputSchema
→ materialize typed Flow bindings
→ permission ceiling + user/org permission + App capability authorization
→ execute Host Flow engine
→ bind and validate outputSchema
→ return MCP CallToolResult
```

错误规则：

- malformed JSON-RPC、unsupported MCP method、连接不存在或根本不存在的 tool 使用 protocol-level error。
- input schema、permission、capability、Flow business failure、timeout/cancel 和 output schema failure 返回 `CallToolResult { isError: true }`。
- 错误 content 只返回有界、可读说明和稳定错误码。
- 不返回绝对路径、receipt、secret、内部 stack、其他 App identity 或完整敏感 payload。
- 有 outputSchema 的成功结果必须包含匹配的 structuredContent；text content 只给用户简短摘要，不重复完整 JSON。

“stale”只表示 connection 最后成功安装的 `tool_surface_sha256` 与 active surface 不同。Build-only
update 即使改变 `catalog_sha256`，只要 surface digest 不变，旧 connection 继续调用并自动解析新的
active execution binding；不得把 execution catalog digest 当作 connection freshness token。
Surface 变化时 notification 触发 registry 重跑 `tools/list` 并更新 last-listed digest；refresh 前
call 返回 `tool_surface_stale`，removed tool 返回 protocol-level unknown-tool，schema/business 错误仍
按上面的 CallToolResult 分层。

## 14. Permission 与安全边界

### 14.1 Per-tool ceiling

Claude oracle 的两类输入必须经过**两条独立管线**，不能先合并成一个枚举：

| oracle 来源 | 中间产物 | `allow` 语义 | 接入点 |
| --- | --- | --- | --- |
| `tools[].permission_policy` | allow/ask/deny **规则** | `always_allow` 是真实 allow rule | 对同一 FQN 的重复声明先按 Deny > Ask > Allow 收敛，再写入现有 `PermissionRuleSource::McpServerPolicy` bucket |
| `toolPermissions` / organization policy | `effective_max_permission` **天花板** | `allow` 只表示不额外收紧，不能单独授予 | Host 在 tool DTO/调用上下文中派生 ceiling，并在 rule walk 结果之后封顶 |

第一条管线复用 `permission/src/rule.rs` 已存在的 `McpServerPolicy` source。它在
`policy.rs` 中按 deny bucket → ask bucket → allow bucket 的行为顺序参与现有 rule walk；
source priority 0 只表示**最低 citation precedence**，不改变 deny-wins 的决策顺序。

第二条管线使用：

```rust
enum McpPermissionCeiling {
    Allow,
    Ask,
    Deny,
}
```

两条管线最后在 Host 调用边界汇合：

```text
existing permission rule walk
→ clamp by Host-derived Local App ceiling
→ clamp by server/plugin and organization effective ceiling
→ force Ask when requiresUserInteraction=true
→ require independent App capability authorization
```

汇合后的安全顺序仍是 Deny > Ask > Allow：ceiling、`requiresUserInteraction`、App capability
都只能维持或收紧 rule-walk 结果，不能把 deny/ask 放宽为 allow。`always_allow` 可以直接授予
permission rule，但不能绕过更严格 ceiling、Host capability、connection scope、schema validation
或 Flow authorization。该拆分既保持 oracle 行为，也避免把 rule source 和 policy ceiling
实现成两个互不相认的权限系统。

### 14.2 Local App ceiling derivation

- 非法/越界 Flow → Deny。
- V1 禁止的 step（递归 Flow、LLM、Agent、BackgroundSchedule、交互式 UI）→ Deny。
- destructive/open-world/sensitive device capability → 至少 Ask。
- 需要 capability receipt 的 step → Ask + capability confirmation。
- 纯 App-private read-only Flow → 可为 Allow，但用户/org deny/ask 仍优先。

Agent、App Manifest 和 generated source 不能声明 ceiling。

### 14.3 Invocation scope

调用时 App identity 只来自 connection scope：

```rust
ConversationExport {
    app_id: String,
    listed_tool_surface_sha256: String,
}
```

- tool input 不接受 app_id。
- App A connection 不能查找 App B tool/Flow。
- listed surface 与 active surface 不同时拒绝调用并等待 `tools/list` refresh；execution catalog
  每次调用按 app ID 从 active Manifest 原子读取。
- App delete/rollback 时先停止新调用，再取消或等待 in-flight 调用。

### 14.4 执行限制

- Tool 只执行 Host Flow engine。
- 不执行 App-provided JS、shell、native module、MCP command/URL 或 WebView callback。
- Flow 最多 32 steps。
- 单次调用最长 5 分钟。
- 每 conversation + App 最多 4 个 in-flight calls，每 conversation 合计最多 8 个；
  read-only tools 每 App 每分钟 60 次，mutation tools 每 tool 每分钟 10 次。限流返回稳定
  `rate_limited` tool error + `retry_after_ms`，不排入无界队列。
- audit 只记录 app ID、catalog digest、tool name、input digest、result status、latency、cancellation；不记录 secret 或默认保存完整 payload。

32-step 是 `AppMcpFlowBinding` 的产品上限；5 分钟复用现有 remote MCP idle timeout 量级。现有
agent `flow_execute`/background 的 generic Flow 仍为 128 steps 与 `FLOW_EXECUTION_TIMEOUT` 15 分钟，
不得因为接入 Local App MCP 而全局收窄。

## 15. Logical per-App Servers 与伸缩

### 15.1 Identity

```text
configured server name: local_app_<app_id>
registry key:           local_apps:conversation-export:<app_id>
serverInfo.name:        lingxi-local-app
serverInfo.version:     <LingXi runtime version>
tool FQN:               mcp__local_app_<app_id>__<tool_name>
```

App display name 改变不改变 identity。

Schema v3 将 App ID grammar 收紧为 `^[a-z0-9][a-z0-9-]{0,53}$`（最大 54；Host mint 仍为
8 位 hex），使 `local_app_` + App ID 始终不超过 MCP server token 的 64 字符上限。当前数据未
发布，Phase 9 清理后不保留 55–64 字符 ID 的兼容分支。

App ID 经新的 `validate_app_id` 后**原样**进入 server segment；不做 `-`→`_`、截断、
hash prefix 或 display-name 映射。新的 grammar 不允许 `_`，所以 `local_app_` 前缀不会产生 `__`，
而合法的 `abc--1` 也继续只含连字符，不会碰 MCP 的 `__` 分隔符。Generated local tool name
使用 canonical lower snake case 且禁止连续/首尾 `_`，因此 server/tool split 唯一。Registry、
storage、permission 与 audit 使用同一个原始 app ID。

### 15.2 One physical hub

所有 server 使用同一个 `LocalAppsMcpTransport` physical hub：

- 不启动进程。
- 不创建 socket/HTTP server。
- Host logical-server record 只持 app ID、active surface/catalog refs 和 lightweight identity；
  exposed connection 持 app ID、last-listed `tool_surface_sha256` 与 connection ID。现有 MCP registry
  的 `Connected.tools` 只缓存至多 8 个已 exposure server 的 tool DTO，不为全部 published Apps
  eager 展开。
- tool list 从 Host-owned immutable catalog 读取。
- notification 使用一个 broadcast bus，按 logical server 过滤。

### 15.3 Lazy session exposure

每个 published App 都有持久 server identity 和 active catalog，但不要求每个 conversation 一次性展开全部 tools。

Exposure 触发：

- 用户明确提到或打开该 App；
- Conversation 调用 LocalAppList/Get 后选择该 App；
- App 被用户固定为当前会话可用；
- 当前会话已经调用过该 App tool（更新 LRU recency，不自动变成永久 pin）。

规则：

- 默认最多 8 个 logical Local App servers 在一个 conversation 中展开完整 tool definitions。
- 超限时使用 LRU 淘汰未 in-flight、未 pinned 的 server。
- 每次成功的 `tools/call` 都更新该 server 的 LRU recency；只有用户显式固定才是 hard pin。
- 8 个槽位全部被用户 pin 时，expose 第 9 个返回 `exposure_capacity_reached`，点名 pinned Apps
  并提示先 unpin；不得静默越过 token/count budget。
- 淘汰只影响当前会话 exposure，不删除 active catalog。
- 激活/淘汰发精确 `notifications/tools/list_changed`。
- 已连接 server 的 tool surface 未变化时复用 connection；若它之后被 LRU 淘汰，模型的旧 FQN
  会收到 orchestrator 的 no-such-tool 结果，恢复路径是 `LocalAppList/Get` 重新 expose。
- Tool Search 只索引 exposed server 的完整 schemas；Host 的轻量 App listing 负责发现其他 App。

此限制是 LingXi 产品资源预算，不冒充 MCP 标准限制。

### 15.4 Catalog refresh

Logical server initialize 声明 `capabilities.tools.listChanged=true`。

- active catalog promote/rollback **只有 tool surface 变化时**、tool removal、session exposure change
  才在 transaction commit 后发送 notification；execution/build-only promote 不发送。
- `unchanged`、失败 candidate 和尚未 commit 的 staging 不发送。
- 只有 `tool_surface_sha256` 变化的 commit 才对受影响 logical server 精确增加一次 surface
  generation；build/execution-only promote 不改变 generation。
- Session activation/eviction 另增该 conversation 的 `exposure_generation`，不改变 App 的
  `authoring_revision`/surface generation；两类 generation 不复用一个计数器。
- Registry 重新执行该 server 的 `tools/list` 并原子替换 tool set。
- App delete 时先停止新调用，处理 in-flight calls，再 unregister logical server。
- notification stream 断线后，reconnect 必须以 active catalog 为权威重建，而不是重放未完成 candidate。

## 16. Persistence 与原子发布

### 16.1 Manifest schema v3

```rust
struct AppManifest {
    // existing fields
    runtime_profile: Option<AppRuntimeProfileBinding>,
    dependency_snapshot: Option<AppDependencySnapshot>,
    template_origin: Option<AppTemplateOrigin>,
    active_mcp_catalog: Option<AppMcpCatalogRef>,
}
```

```rust
struct AppMcpCatalogRef {
    build_id: String,
    manifest_revision: u64,
    authoring_revision: u64,
    user_goal_sha256: String,
    proposal_sha256: String,
    approval_contract_sha256: String,
    tool_surface_sha256: String,
    catalog_sha256: String,
    mcp_verification_sha256: String,
}
```

Active catalog 内容存放在 Host-owned immutable store：

```text
<app-data>/mcp/catalogs/<catalog-sha256>.json
```

Manifest 只保存 active ref。Candidate 不写入正式 Manifest。

Schema v3 约束：

- unscaffolded shell：`surface/runtime_profile/dependency_snapshot/template_origin/
  active_mcp_catalog` 全部为 `None`；
- scaffold 已提交但首次 build/MCP QA 尚未 promote：前四项为 `Some`、active ref 为 `None`，
  查询状态仍是 `draft`；
- published：前四项、active build receipt 和 `active_mcp_catalog` 全部存在且互相绑定；
- 当前数据未发布，Phase 9 直接清理 schema v2 开发数据，不实现 v2→v3 读取或迁移分支。

`proposal_sha256` 覆盖 Host 验证后的 Agent proposal（含 semantic references，但不含 Host-derived
ceiling/final binding）；`approval_contract_sha256` 使用 §12.2 的用户审阅面；
`tool_surface_sha256` 覆盖 Conversation 可见的 tool name/title/description/inputSchema/outputSchema/
annotations/execution/icons 和可见 `_meta`；
`catalog_sha256` 还覆盖 Host execution bindings 与 build identity。Update 在 tool surface 不变时可以
产生新的完整 catalog/build binding，但保留 `authoring_revision` 与 `tool_surface_sha256`，也不发送
`notifications/tools/list_changed`；surface 变化才递增 authoring revision/generation 并广播通知。

### 16.2 Derived publication state

删除 `AppRecord.workflow_state` 与 `AppService::mark_ready`，不在 `AppRecord` 保存可漂移状态机。
Client DTO 状态完全由可信事实派生：

| Derived state | 条件 |
| --- | --- |
| draft | active build 与 active catalog 均为 None |
| published_unverified | active build + active catalog；UI runner evidence 不完整 |
| published_verified | active build + active catalog + required UI/MCP evidence 全部通过 |

`published_unverified` 和 `published_verified` 是查询投影，不是独立可写枚举。

MCP verification 与 UI verification 分开记录。任一平台无法取得所要求的 UI evidence 时：

- UI verification 可以是 unavailable/unverified。
- MCP schema/Flow/call/isolation QA 必须通过才能激活 catalog。
- UI 不显示 verified badge。

对已 scaffold App，`active build` 与 `active_mcp_catalog` 必须成对出现：同为 None 是 draft，
同为 Some 是 published，恰有一个是 `active_state_corrupt` 并 fail closed。
`mcp_authoring_required` 只存在于 candidate journal，
不新增一个可漂移的 AppRecord 状态。

### 16.3 Candidate journal

Create/update/MCP revise 使用 durable journal：

```text
prepared
→ approved
→ built
→ smoke_passed
→ mcp_verified
→ promoted
```

Journal 记录 previous build/catalog refs 和 candidate digests。进程崩溃后：

- promote 前 previous active 始终不变；journal/staging identity 与 digests 完整时从最后完成阶段
  resume，损坏或无法验证时才删除 candidate。
- Create 已消费 receipt 并提交 draft Manifest 后崩溃：保留已提交的 scaffold/Profile/dependency/
  template snapshot；有效 candidate build/catalog 与 journal 可在所有 approval-contract digests
  未变时继续 retry，不要求新 receipt；无效 candidate 才删除。
- promote 原子完成但清理未完成时，以 Manifest active ref 为权威完成清理。
- 不依赖 Git 完成 build/catalog rollback。
- receipt 首次消费后，journal 的 `approved` 状态保存其绑定 digests；同一 candidate 的
  deterministic build/QA retry 不要求用户重复确认。Source bytes 与 final build/Flow IDs只更新
  build/catalog identity，不作废 approval；design contract、template/Profile、dependency input、
  MCP approval contract 或 permission ceiling 任一 digest 变化都会使 approval 失效并要求新
  receipt，不能“修改审阅面后复用已消费 receipt”。

### 16.4 Atomic promote

一次 promote 原子切换：

- App Manifest revision；
- active build receipt；
- 非空 active MCP catalog ref；
- surface generation（仅在 `tool_surface_sha256` 变化时递增）。

Derived publication projection 不写入事务；commit 后由新的 active pair 重新计算。

失败不能产生短暂空 catalog 或一半更新的 build/catalog 配对。Create receipt 消费后允许把
scaffold/Profile/dependency/template snapshot 作为 draft 提交；候选 build/catalog 在至少一个 tool、
MCP QA 和 Native approval 全部满足且 promote 前始终停留在 candidate staging。Update/MCP revise
失败时继续使用 previous active build/catalog pair。

## 17. Client Protocol 与 Native UI

### 17.1 Protocol additions

- builtin Plugin inventory/status；
- enable/disable Local App Plugin；
- template catalog display DTO；
- unified create-plan confirmation；
- MCP proposal diff/approval；
- derived publication/verification summary；
- managed Local App MCP server inventory；
- typed operation errors/results 按下表分层，不把 verification state 或 MCP tool error 混进
  client-protocol error enum。

| Code | Wire layer / producer | 触发条件 | 恢复动作 |
| --- | --- | --- | --- |
| `plugin_disabled` | client protocol / create-update-verify-revise | builtin Plugin disabled | enable 后仅重试原动作一次 |
| `builtin_bundle_unavailable` | client protocol / materializer | 首次或当前 verified root 不可用 | 修复存储/重启；existing snapshot App 仍可运行 |
| `template_unavailable` | client protocol / template catalog | 没有满足 availability/profile policy 的候选 | 修改需求或完成 availability gate |
| `proposal_invalid` | client protocol / MCP authoring validator | schema、binding、meaningfulness 或 definition budget 失败 | 按逐 tool finding 修订 proposal |
| `catalog_stale` | client protocol / selector-authoring validation | optimistic catalog digest 已变化 | 刷新 catalog，最多自动重跑一次 |
| `active_state_corrupt` | client protocol / App read-launch-build gate | active build/catalog 恰有一个或互相不绑定 | fail closed，执行 Host repair/reset |
| `mcp_authoring_required` | recoverable client result / authoring | 0 个 meaningful tool | 继续澄清目标并 resume journal |
| `repair_budget_exhausted` | workflow terminal result / unified build | repair rounds 已用尽且仍有 blockers | 显示 finding IDs，用户修改需求或提高允许的 quality level |
| `verification_unavailable` | verification status，不是 error | 所需 UI evidence runner 不可用 | 显示 unverified，不阻断已通过的 MCP gate |
| `permission_ceiling` | MCP `CallToolResult.isError` / authorization | 调用超过 Host ceiling | 修改请求或走独立 capability approval |
| `tool_surface_stale` | MCP `CallToolResult.isError` / logical connection | last-listed surface 落后 active surface | 等待 listChanged refresh 后重试 |
| `exposure_capacity_reached` | Host LocalAppGet tool result | 8 个 exposure slots 全被用户 pin | unpin 一个 App 后重试 |

当前 `CLIENT_PROTOCOL_VERSION` 是 `9.0.0`；本方案一次性 bump 到 `10.0.0`。Rust
`client-protocol` DTO/snapshot 是 wire 真源；iOS/Android 重新生成 UniFFI bindings；Electron
同步更新 `@lingxi/bridge-client` TypeScript types、validators 和 version handshake tests，
不把 Electron 描述为 Rust generated binding。按 §1.8，Electron 不渲染 Local App UI。

### 17.2 Plugin settings

iOS/Android 显示：

- LingXi Local App；
- builtin、version、bundle digest；
- enabled state；
- skills/agents/workflows/template count；
- validation error。

不显示 marketplace source、install/update/uninstall controls。

### 17.3 Create confirmation

一个 Native sheet 展示：

- App name/brief；
- selected template + Runtime Profile；
- Agent reason/rejected candidates；
- download/dependency status；
- initial MCP tools；
- tool side effects/permission ceilings；
- 即将执行的 required gates 与 runner availability（确认时不得把尚未运行的 gate 显示为 passed）。

确认后返回 single receipt。拒绝不写正式 App 状态。

### 17.4 MCP inventory

iOS/Android 的现有 MCP inventory 增加 managed Local App source（桌面端不含，§1.8）：

- stable server name；
- App display name + ID；
- active build/catalog digest 摘要；
- tool count；
- authoring revision；
- UI/MCP verification status；
- tool schema/annotations/ceiling。

不显示伪 `.mcp.json` 路径，不允许编辑 transport。

### 17.4a MCP proposal diff

Update/revise 的 Native approval sheet 逐 tool 显示 `added`、`removed`、`changed`。Changed 至少展开
name、title/description、inputSchema、outputSchema、annotations/execution/可见 `_meta`、semantic
Flow reference 与 permission ceiling 的 before/after；同时显示 required Flow changes、excluded
capabilities、尚未通过的 gates、receipt expiry 与 supersede 状态。Final Flow/build ID、内部路径和
source digest 不展示。Surface/ceiling/semantic contract 完全 unchanged 时不显示 sheet、不创建 receipt。

### 17.5 文案与本地化

§17.1–§17.4a 新增的每一个 Native surface 都是用户可见字符串，必须走既有 i18n 管线。

**真源与硬门**：`clients/translations/*.json` 是唯一真源；
`clients/ios/Resources/Localizable.xcstrings` 与
`clients/android/app/src/main/res/values*/strings.xml` 是 `generate.py` 的生成产物，
**不可手改**。5 个 locale（zh-Hans / zh-Hant / en / ja / ko）在本次复核基线各 **1894** 个 key，
`generate.py` 以 `missing`/`extra` 集合差强制**数量必须相等**。
注意该校验只保证 key 齐，不保证已翻译；当前它**不在 CI 上**，Phase 8 必须把
`generate.py --check` 和 generated-output no-diff 接入 CI。

新增 key 前缀：

```text
local_apps_plugin_*            Plugin settings 页（§17.2）
local_apps_create_confirm_*    Create confirmation sheet（§17.3）
local_apps_mcp_proposal_*      MCP proposal diff/approval（§17.4a）
local_apps_verification_*      verification badges 与状态（§17.4）
local_apps_error_*             §17.1 中实际进入 Native UI 的 code/status 各一条用户可读文案
```

占位符注意：iOS 占位符在 KEY 里、Android 在值里，同一条带参数的文案常需两个 key。

ja / ko / zh-Hant 需要真人翻译，必须计入 Phase 8 排期（§18）。

## 18. 实施顺序

### Phase -1：切断散落命名依赖

**范围限定**：Phase -1 只在现有 builtin 注册上解耦，**不搬文件、不引入 plugin 依赖、
不改 workflow 脚本内容**。两个 workflow 脚本实质不同（`local_app_canvas_workflow.js`
390 行、captured/frame/motion 关键词 40 处 vs `local_app_build_workflow.js` 340 行、15 处），
统一分支与 per-profile verification policies 属于 Phase 4。

1. 保留两个 workflow 的既有注册，只把 workflow 名的**选择逻辑**从
   `workflow_support.rs` 移入 composition binding；统一为单一 workflow 在 Phase 4 完成。
2. 增加 `LocalAppPluginBinding` composition adapter。
3. 将 Host launch-context 注入绑定到 resolved handle。
4. 删除 workflow_support 的 Profile→workflow name map。
5. 将 `LINGXI.md` 改为提示层中立文本。
6. 将 mobile Local App skill 注册改为迭代现有 `mobile_skill_registry()`，先消除第二份名单；
   Phase 2 再把 provider 切到 PluginManager inventory。
7. 增加 component literal 防回归门（Phase -1 从现有 live registries 派生 needle，覆盖 basename
   与 FQN；Phase 2 切 verified Plugin inventory，§8.5）。
8. 为 resolved Local App workflow 写入 typed `LocalAppWorkflowTaskScope`；Build workspace lease、
   三种 purpose 的 delete guard 与 Build workflowModel default 改读该 scope。删除
   `tasks/src/lib.rs`、`tools/workflow/src/lib.rs`
   两份 workflow 名单和按 `meta.name` 授权的分支。

完成条件：除 Plugin package、composition binding 和 tests 外，engine-mobile、`tasks/`、
`tools/workflow/` 生产模块不知道任何 Local App skill/agent/workflow 名称；自定义同名 workflow
不能获得 workspace lease、delete guard 或 App model default。

规模：**L**。除 mobile binding 外还触及 `tasks` task state/handler/registry 与 generic workflow
default-model seam；不按未经团队校准的人周承诺。
依赖：无前置，可立即开始；与 Phase 0a/0b 可并行。

### Phase 0a：Local App 所需 Plugin contract 与品牌集中化

0. **把品牌防漏门摘到 main 并接入 CI**（`check_brand_leaks.py` / `check-brand-leaks.sh` /
   `brand_leak_baseline.txt`，今天只在 `.claude/worktrees/agent-namespace-plan-a/`）。
   needle 加入 `.claude-plugin` / `CLAUDE_PLUGIN_*` / `CLAUDE_PROJECT_DIR`；
   只豁免 oracle docs、品牌归一化 fixtures 和明确上游字段，**不得豁免
   `plugins/lingxi-local-app/` 生产目录**。
   **必须排在本 Phase 其余步骤之前**——Phase 0a/0b 要照 oracle 逐字段对齐，是全方案品牌回流风险最高的一步。
1. `branding` 增加三个 Plugin env constants。
2. `PluginManifest`/`PluginComponents` 增加 workflows，并完成 manifest-backed skills/agents/workflows
   default/custom path discovery。
3. Plugin workflow `.js`-only discovery、namespace、saved > plugin > builtin resolver、saved
   `meta.name` identity、task script digest/pause/resume。
4. Plugin Agent 的正常 frontmatter 与 namespace 进入共享 registry；安全字段不传播。
5. 复用现有 `PluginSource::BuiltIn`，增加 `register_verified_builtin` composition 入口；
   不修改 `PluginManager::install(BuiltIn)` 的拒绝语义。
6. 增加上述最小 contract 的品牌归一化 oracle fixtures。

完成条件：Local App 所需 manifest-backed fixture 经过品牌 token normalization 后，skills、agents、
`.js` workflows 的 inventory/name/path/resolver 行为与 oracle 相同；桌面回归的 0a 条目全绿。

规模：**L**。这是 Phase 1 的最小前置，不等待 theme/monitor/manifest-less parity。
依赖：可与 Phase -1 并行；**必须先于 Phase 1**。

### Phase 0b：完整 Plugin parity

1. `PluginManifest`/`PluginComponents` 增加 metadata、experimental themes/monitors。
2. discovery/validation/path semantics 补齐 manifest-less/skills-directory plugin 与
   default/custom/inline component forms。
3. Plugin Agent parsing 对齐 oracle：替换当前 `agent_validation.rs` 对 malformed YAML、
   `permissionMode`/`hooks` 的 whole-plugin reject；runtime 按文件名/默认描述 fallback 或 strip
   unsupported fields，validator/strict-validator 分别 warning/fail，且任何权限字段都不传播。
4. 扩展既有 theme 选择层；monitor discovery/activation 复用 `MonitorRegistration →
   spawn_monitor → monitor_ws` substrate，补 trust、mid-session disable retention、session cleanup。
5. 补齐 metadata 与 LSP/MCP/hook/monitor 的 env/path substitution parity。
6. 增加完整品牌归一化 oracle fixtures（不是 0a 第 0 步的防漏门，两者都要有）。

完成条件：同一 fixture 经过品牌 token normalization 后，LingXi 与 Claude oracle 得到相同 component inventory 和解析行为。

规模：**L**（与 0a 合计仍为 XL）。`plugin/` crate 约 4,900 行，两阶段合计触及 manifest/discovery/loader/manager、
workflow/theme/monitor registries 与桌面回归面；日历工期必须由实际 owner/测试设备校准。
依赖：Phase 0a；可与 Phase 1/2 主线并行，但 §20 完成定义要求 0b 关闭。
⚠️ 爆炸半径在桌面端（唯一真实跑第三方 plugin 的产品），回归验证见 §19.11。

### Phase 1：Builtin bundle 与移动端 PluginManager

1. 建立 deterministic builtin archive/inventory build。
2. 实现 mobile atomic materialization 和 digest verification。
3. `engine-mobile` 接入现有 PluginManager。
4. 连接 command/skill/agent/workflow registries。
5. 实现单 Plugin enable/disable/status protocol。
6. 将 materialized root 作为模型可读/不可写 trusted root 接入，并在 iOS/Android release 真机
   通过 Skill→Read reference、Write denied 的端到端门。

完成条件：移动端只发现一个 name=`lingxi-local-app`、source=`BuiltIn` 的 Plugin，同一 live
registry 驱动 listing 和 invocation。

规模：**L**。包含 build-time packer、移动端存储事务、PluginManager composition 和 protocol。
依赖：Phase 0a。packer/materializer/PluginManager composition 可与 Phase 0b/Phase 2 内容编写并行；
本 Phase 的最终 bundle 集成门在 Phase 2 package 可用后关闭。

### Phase 2：Unified Plugin package 与 template assets

1. 搬迁**现有 10 个 skill**（`skills/` 下 create-local-app、frontend-design、frontend-qa、
   accessibility、react-best-practices、ionic-react-local-app、canvas-2d-local-app、
   threejs-local-app、**phaser-2d-local-app、babylon-3d-local-app**——后两个已存在，属搬迁不属新增）。
2. 将上述 10 个 skill 的 router/reference 统一为 Plugin file-backed relative load，删除移动端
   bundled-body 特例和 Rust 手写 reference list；保留 `agents/openai.yaml` 仅作 inert metadata。
3. **新增 §7.2 目录树中其余 17 个 skill**（local-app-use/run/inspect-view/capture-view/
   interact/test/debug/data/background、template-selection、llm-sidequery、llm-agent、device、
   expose-as-mcp、mcp-tool-design、mcp-flow-binding、mcp-qa）。
4. 创建七个 agents 和三个 workflows。
5. 按 §6.1 的 migration manifest 搬迁五套 runtime-profile scaffold bundle 的 **112 个实际
   scaffold 文件**，并删除 12 个 tracked orphan（**不是搬整个目录**，见 §9.1）。
6. 生成 template catalog/inventory/digests。
7. 将 `local-apps/src/permissions.rs` 引用的生产 `settings.local.json` 字节迁到 Host-owned
   `local-apps/assets/`，保留 create transaction writer；不得放进 Plugin template。
8. Babylon 保持 unavailable。

完成条件：builtin bundle validation 能证明 manifest、components、assets 和 catalog 交叉一致。

规模：**XL，本方案最大的单块**。17 个新 skill + 7 个 agent + 3 个 workflow，另有 10 个
skill 搬迁、112 个 template asset 与 schema/catalog；不把内容工程伪装成“搬文件”。
依赖：Phase 0a 的 manifest/discovery 契约；内容编写不依赖 Phase 1，最终 archive/inventory
验证与 Phase 1 汇合。**阻塞 Phase 3 / 4 / 6**（它们消费 template-selector、mcp-designer 和三个 workflow）。
Phase 1 与 Phase 2 是并行 tracks、共同关闭一个 bundle milestone，不构成“彼此等待才能开始”的循环依赖。
这是排期上的关键路径，不要按「搬文件」估算。

### Phase 3：Agent template selection 与 candidate staging

1. Host verified template catalog API。
2. template-selector + structured output。
3. template-selector-only Host validation tool、ValidatedTemplateSelection + display-only rejected candidates。
4. run-scoped durable candidate journal + `validated_selection_handle`。
5. create launcher 分支：初始 context 只带 catalog；selector agent 返回 opaque handle，下游 agents
   分别经 Host read tool 解引用权威 selection；update/verify 继续读取 persisted Profile。
6. isolated create staging 与安装前 `dependency_input_sha256`。

完成条件：Agent 不能通过任何 args/path/digest 覆盖 Host 选择权威；create designer/builder 能从
Host-returned handle 获得 profile specialist，而不要求 Manifest 已持久化。

规模：**L**。Host API、selector、journal、handle 与 staging 是同一事务链；本 Phase 不创建最终
receipt、不写正式 Manifest，因而不消费 Phase 4/5/6 尚未产生的 digest。
依赖：Phase 1 + Phase 2；阻塞 Phase 4 create branch 和 Phase 6。

### Phase 4：Unified workflow 与 profile QA

1. build workflow create/update/verify branches。
2. Host launch-context enricher。
3. profile-specific specialist selection。
4. use-test workflow。
5. DOM/Canvas2D/Three/Phaser/Babylon verification policies。
6. render/motion/data/webview gates 和 repair rounds。
7. operator/tester workflow 调用链与 fast=1/balanced=1/thorough=2 budget。

完成条件：Canvas 家族必须以 captured frame 和必要的 dual-frame motion evidence 才能通过。

规模：**XL**。合并两个行为不同的 workflow，同时迁移所有 profile policy 与既有反向测试。
依赖：Phase -1 + Phase 2；create candidate branch 依赖 Phase 3。该阶段的端到端 create 只到
staging/build/use-test evidence，不宣称已有 Native receipt 或 published state。

### Phase 5：MCP DTO 与 permission ceiling

1. 完整 MCP Tool/Initialize/CallToolResult DTO。
2. Local App hub 复用 `LATEST_PROTOCOL_VERSION`，补 2025-11-25 initialize negotiation。
3. outputSchema/structuredContent validation。
4. `permission_policy` producer。
5. `toolPermissions`/org ceiling producer。
6. `effective_max_permission` 传播与 strictest-wins authorization。
7. Local App Host-derived ceiling。

完成条件：always_deny/blocked 不可能被 wildcard allow、auto mode 或 App proposal 放宽。

规模：**L**。DTO、标准 MCP 结果和两条权限管线跨 `traits`/`mcp`/`permission`。
依赖：可与 Phase 1–4 并行；必须先于 Phase 6/7。

### Phase 6：Per-App MCP authoring 与 persistence

1. Schema v3 App ID 最大长度收紧为 54，并重生成协议/validation goldens。
2. typed Flow input/output binding。
3. AppMcpProposal schema。
4. mcp-designer 和 authoring workflow。
5. candidate catalog/journal。
6. canonical `approval_contract_sha256`、`tool_surface_sha256`、`catalog_sha256` 三层 digest。
7. create unified confirmation/receipt 与 update/revise conditional MCP proposal approval。
8. receipt 消费后的 atomic scaffold/Profile/dependency/template snapshot draft commit。
9. immutable catalog store、Manifest active ref 与 build/catalog atomic promote。
10. 删除持久 `workflow_state`/`mark_ready`，derived publication status 成为唯一 client projection。

完成条件：每个 active tool 都能追溯到 user goal、proposal、Flow、build、QA 和 catalog digest。

规模：**XL**。包含 typed Flow、authoring/revise、Native approval、journal 和 immutable store。
依赖：Phase 2 + Phase 3 + Phase 4 + Phase 5。Phase 3 不再反向依赖本 Phase，排期无环。

### Phase 7：Logical server、registry 与 listChanged

1. ConversationExport scope。
2. stable per-App logical server identity。
3. shared physical hub connection routing。
4. managed server registration/unregistration。
5. broadcast notifications。
6. 8-server session exposure budget/LRU/pinning。
7. timeout/rate limit/cancellation/audit。
8. connection 绑定 last-listed surface、call 动态解析 active execution catalog；surface-only
   generation 与 build-only update 复用语义。

完成条件：ConversationExport scope 下 App A 无法列出或调用 App B；注册 100 logical server 后
physical transport 仍为 1，只有至多 8 个 exposed registry tool snapshots。

规模：**L**。重点是 registry generation、connection scope、LRU/pinning 与并发测试。
依赖：Phase 5 + Phase 6。

### Phase 8：客户端 UI

1. Plugin status/toggle。
2. create-plan confirmation。
3. MCP proposal diff/approval。
4. managed MCP inventory。
5. verification badges/errors。
6. **按 §17.5 在 `clients/translations/zh-Hans.json` 新增全部 key，补齐
   zh-Hant/en/ja/ko，跑 `clients/translations/generate.py` 重新生成 xcstrings 与
   strings.xml 并提交**。ja/ko/zh-Hant 需真人翻译，须单独计入排期。
7. protocol parity 和 iOS/Android tests。
8. 更新 `@lingxi/bridge-client` TypeScript types/validators 与 Electron protocol compile tests；
   不增加 Electron Local App UI。

规模：**XL**。iOS 与 Android 两套原生 surface、i18n、无障碍与真机证据分别交付。
依赖：Phase 6 + Phase 7 的 protocol/DTO 稳定后开始集成；Phase 3 只提供 selector/candidate DTO，
纯文案可提前。

### Phase 9：一次性切换与清理

1. 清理 pre-release Local App/workflow/task 数据。
2. 删除 standalone bundled Local App skill/workflow registration。
3. 删除旧 Core scaffold templates 和已迁移引用；确认 production workspace permission asset/写入
   仍在 Host-owned 路径且新建 App 的 allow/deny 内容不变。
4. 删除旧 runtime profile selector UI/receipt path；对
   `local_apps_runtime_profile_*` 做 source-reachability 对账，**只删除五个 locale 中已经无引用的
   selector-only keys**。`health_*`、`family_*`、`surface_*` 当前仍被 App detail/models 使用，
   不得按前缀批量删 30 个；create confirmation 继续复用的 key 也保留。跑 `generate.py`
   重生成并提交 generated outputs。
5. 发布 schema v3 + 新 client protocol。
6. 删除 `CurrentBuiltin`/`Split` transition、两份 `LOCAL_APP_BUILD_WORKFLOWS`、
   `apply_local_app_build_default_model` 和所有持久 `workflow_state` dead code。

规模：**M**。删除与一次性数据清理必须在全量验证后单独提交，方便回滚审查。
依赖：Phase -1–8 全部完成；是唯一切换点。

## 19. 验证计划

**判据原则（对本节每一条生效）**：一个门只有在**输出点名了具体的东西**时才算证据。
退出码为 0、测试数量、无阈值的 benchmark 都不是证据。每个 hard gate 必须有一个
「故意违反 ⇒ 必须失败」的反向用例，且失败输出要点名违反的那一项。
在基线 `5448e93e3` 上已经成立的断言不是门。

### 19.0 性能门配置

Phase 1 必须新增 checked-in `docs/local-apps/performance-baselines/local-app-plugin-v1.json`，
包含以下非零 hard limits；缺字段、值为 0 或使用非数值占位符时验证直接失败：

```json
{
  "builtin_archive_max_bytes": 4194304,
  "builtin_extracted_max_bytes": 12582912,
  "first_materialization_p95_ms": 2000,
  "cached_startup_added_p95_ms": 150,
  "published_apps_30_added_p95_ms": 75,
  "published_apps_100_added_p95_ms": 200,
  "logical_servers_100_registration_p95_ms": 250,
  "logical_servers_100_retained_heap_max_bytes": 16777216,
  "per_app_tool_definitions_max_tokens": 2048,
  "expanded_tool_definitions_max_tokens": 16384
}
```

当前 112 个 scaffold 源文件合计约 376 KiB，因此 4 MiB/12 MiB 不是对现状的追认式门。
每份 benchmark evidence 另存设备 model/OS/WebView、build type、thermal/power 状态、5 次
warmup 与至少 30 次采样的 p50/p95。iOS 使用当前已配对的 iPhone 11 / iOS 18.6.2 作为
最低基线；Android 在 Phase 1 完成前必须登记一台覆盖当前 `minSdk=26` 支持面的物理参考设备，
没有该设备 evidence 不能关闭 Phase 1。阈值可以在独立、带 benchmark evidence 的设计变更中
调整，不能为让失败构建变绿而在同一个功能提交里放宽。

`logical_servers_100_retained_heap` 测量 Host logical-server registry、shared hub、broadcast/filter
state 与最多 8 个 exposed `Connected.tools` snapshots 的总增量；fixture 使用 §13.2 合法上限内、
可通过 2,048-token per-App gate 的真实 schemas。它不把磁盘 catalog 文件大小算作 retained heap，
也不假设现有 MCP registry 的 `Connected.tools` 不存在。Canvas/Three/Phaser/Babylon 的 FPS/
frame-time 在本版本记录 evidence，但没有设备基线前不设伪 hard threshold。

### 19.1 Plugin contract

- `.lingxi-plugin/plugin.json` discovery。
- manifest-less default-directory discovery、directory-derived name、root `SKILL.md` skills plugin。
- Plugin root `LINGXI.md` 不进入 session project context。
- manifest field/default/unknown-field/strict validation。
- component path replace/add/merge 语义。
- root `SKILL.md` fallback 只在无 `skills/` 且无 manifest skills declaration 时生效。
- Local App Plugin 恰好发现 27 个 skill；每个 SKILL frontmatter、directory、registry FQN 一致，
  relative reference load 留在 skill root；nested `agents/openai.yaml` 不成为 Plugin agent/body。
- nested agent/workflow namespace。
- malformed/missing-name Plugin agents仍加载：使用 scoped file-derived name/default description，
  malformed 时忽略全部 frontmatter fields。
- `permissionMode`/`mcpServers`/`hooks` 不传播到执行态；普通 validation 警告、strict validation 失败。
- Plugin Agent `color`/`initialPrompt` 被 runtime 忽略、validator 警告；strict validation 失败。
- `${LINGXI_PLUGIN_*}`/`${LINGXI_PROJECT_DIR}` substitution。
- brand-normalized Claude oracle fixtures。
- workflows inventory、meta、task copy、digest、pause/resume。
- Plugin 与 named saved workflow directory discovery 只接受 `.js`；`.mjs/.cjs/.ts` near-miss
  不进入 inventory，explicit scriptPath 不受此门影响。
- named resolver 以 saved parsed `meta.name` > namespaced plugin > builtin 排序；Local App verified
  handle 不被同名 saved workflow shadow。
- metadata object roundtrip/inertness；non-object runtime ignore + validator warning/strict fail。
- default/custom themes 与 default/path/inline monitors discovery、dedupe、activation、
  mid-session disable 不强停、session-end cleanup。
- 现有 `PluginManager::install(PluginSource::BuiltIn)` 继续返回拒绝；只有
  `register_verified_builtin` 能注册已验证 root，且 live manifest source 被 stamp 为 `BuiltIn`。
- 27 个 Plugin skill 保持非 bundled；frontmatter description ≤180 display columns，在 200k
  context 的 `format_within_budget` 中全部完整保留且剩余预算 ≥15%。故意加长一个 description
  必须由 packer 点名 skill/FQN 拒绝。

### 19.2 Builtin bundle

- deterministic archive/inventory golden。
- path traversal、duplicate path、symlink escape 拒绝。
- single-byte tamper 导致 digest failure。
- interrupted materialization 不替换 previous verified root。
- mobile only loads builtin source。
- enable/disable whole-plugin lifecycle。
- bare `enabledPlugins["lingxi-local-app"]` 三向行为：explicit true + default false ⇒ Loaded；
  explicit false ⇒ registry 中为 `PluginState::Disabled` 而非缺席；key missing ⇒ 使用 manifest default。
  Native toggle 写回同一 bare key，不存在第二个 enable flag，restart 后状态一致。
- 重启后 live `PluginId` 可变化，但 name/source/bundle provenance、per-App snapshot 与 build/restore
  不依赖旧 `PluginId`。

- **无 previous verified root 时物化失败 ⇒ 返回 `builtin_bundle_unavailable`，不 panic、不半注册**
  （原「interrupted materialization 不替换 previous verified root」在首次安装场景恒真，
  必须拆成有/无 previous root 两个用例）。
- 幂等短路：digest 未变的第二次启动不解包、不做全量逐文件校验；marker 字段、缺失
  manifest/inventory/component entry、symlink root 各有反向用例。
- 非 prune 目录中放入 inventory 清单外的文件 ⇒ build 失败；`node_modules`/`dist` 存在时
  不进入 archive（§6.1 反向对账）。
- 在跑过 `pnpm install` 的 checkout 上，archive digest 与干净 checkout 相同。
- builtin archive ≤ 4 MiB、解包后 ≤ 12 MiB；失败输出 actual/limit bytes。
- first materialization p95 ≤ 2000 ms；verified-root cached startup added p95 ≤ 150 ms；失败输出
  platform/device/build/actual/limit。
- iOS/Android release 真机从 Skill prompt 的 base directory 经模型 Read 打开
  `references/router.md` 成功；同根 Write/Edit 被 Host policy 拒绝。该门验证真实
  `SessionCwd.trusted_dirs`/path translation，不只直接调用 filesystem helper。

### 19.3 Core decoupling

- production literal scanner planted-positive test。
- skill listing/Skill invocation/slash invocation 共用 registry。
- Host contract 无 workflow/skill/agent names。
- workflow context 只由 Host 注入。
- caller 提供 host_context/profile/template/renderer/surface 被拒绝或覆盖。
- **component scanner 三个必须红的反向用例**：
  (a) 往 Plugin fixture 新增一个可被默认目录自动发现的 component、确认它进入 verified
      inventory，再把名字种进 `local_apps_host.rs` ⇒ 必须失败（证明 needle 来自 discovery
      结果而非 raw manifest/static array）；
  (b) 种 namespaced FQN `lingxi-local-app:local-app-build` 而非 basename ⇒ 必须失败；
  (c) 新建一个未被任何 allowlist 提到的 engine-mobile 模块并种字面量 ⇒ 必须失败。
- **品牌 scanner planted-positive**：往 `apps/engine-mobile/src/` 埋一个 `.claude-plugin`
  字面量，以及往 `plugins/lingxi-local-app/` 埋一个 `CLAUDE_PLUGIN_ROOT`，两者都必须红且
  **输出点名文件与行号**。
- scanner allowlist 条目数与品牌泄漏计数入 CI 基线，基线上升即失败。
- `engine-mobile` 中 `register_verified_builtin` 恰好只在 composition module 调用一次；种第二个
  调用点必须红并输出 file:line。
- bundle descriptor 模块之外引用 `local-apps/templates/**` 或
  `plugins/lingxi-local-app/**` 的 `include_bytes!`/`include_str!` 必须红；迁移 allowlist 使用精确
  调用点/count，Phase 9 后为零。
- `tasks/src` 与 `tools/workflow/src` 不存在 Local App workflow basename/list；typed task scope
  仍能让 build workflow 获得 workspace lease/读取 workflowModel，所有 Local App workflows 阻塞
  App delete；自定义同名 workflow 不能获得 workspace lease/model default，也不能阻塞 delete。

### 19.4 Templates/Profile

- migration manifest 恰好覆盖 112 个 `(family,path)` 宏调用；搬迁前后 file/contract digests 一致。
- 12 个已知 tracked orphan 不进入 Plugin inventory，迁移清理后旧/新模板树均零命中。
- 五个 r1 的 toolchain/core package golden 与 §9.1.1 完全一致；任一 package version、managed
  byte、editable seed byte、lock byte 或 sourceSeedId 变化都改变 contract hash。
- 修改 scaffold 后的 App 业务源码或普通 dependency snapshot 不改变 persisted Profile binding。
- 四个 available template 可 scaffold。
- Babylon 不出现在 Agent candidate catalog。
- selector fixtures 命中预期最简单模板。
- `LocalAppTemplateCatalog({})` 不接受 caller-provided app ID；只返回 available semantic view。
- selector 必须 echo Host-issued catalog digest；catalog 在调用和验证之间变化时返回
  `catalog_stale`，不得把 Agent echo 当作 family/revision/digest 权威。
- selector 返回正确 ID 但伪造 family/path/contract 字段时 schema 拒绝；Host 独立解析全部权威值。
- create shell 可在没有 persisted Profile 时启动 selection stage；Host-issued selection handle 之后
  designer/builder 取得正确 specialist。伪造、跨 App、跨 run、stale catalog、resume 丢 journal 的
  handle 均被拒；update/verify 缺 persisted Profile 仍 fail closed。
- selector agent 回显错误 family/revision 时，下游 `get_validated_selection(handle)` 仍返回 Host 值；
  workflow script 没有新增 direct Host-call primitive，移除任一 agent 的 handle re-resolution 必须红。
- rejected candidates 只允许当前 catalog IDs、仅用于 UI、不改变 receipt digest。
- plugin disabled 后 existing App 使用 snapshot build/restore。
- Plugin bundle 更新不改变 existing App Profile/snapshot。
- 新建 workspace 的 Host-owned `.lingxi/settings.local.json` 仍含 `permissions.rs` 点名的
  build-agent allow/deny 条目；删掉 Plugin template 不能删掉这份生产授权。

### 19.5 Workflow/QA

- create/update/verify branches。
- DOM inspect/navigation/form/accessibility。
- **Canvas policy 下，以下 well-formed 报告必须各自产生点名对应文本的 blocking finding**
  （断言 finding 文本，不断言退出码）：`render_check.status=not_applicable`、
  `status=passed` 且 `canvas_surfaces=0`、`frames_captured=0`、
  `motion_check` 缺失、`motion_check.frames_compared<2`。
- **迁移备注**：上述反向覆盖今天**已部分存在**于
  `tools/workflow/src/builtins.rs:1944`（canvas_zero_surfaces_blocks…）与 `:2509`
  （missing/failed motion_check…），但它们经
  `drive_local_workflow("local-canvas-build", …)` **按名进入**，而 §8.3 会删掉该名字。
  合并 workflow 时必须显式改为「policy=canvas_* 的统一 build workflow」入口，
  否则这些门会静默消失。
- 现存缺口：没有任何测试把 `render_check.status="not_applicable"` 喂给 canvas workflow
  并断言必须红——`builtins.rs:2263` 只钉 schema 描述**字符串**，是文案 pin 不是行为门，必须补。
- mutation 用例：删掉 policy 分支后，上述用例必须由绿转红。
- Three context loss/dispose/resize。
- Phaser scene/phase transition。
- Babylon report-policy 的 hermetic tests 从 Phase 4 起始终启用（glTF/Havok/context-loss 字段缺失
  必须产生点名 finding）；只有需要真实 Babylon runtime 的 build/WebView/性能 spike 在 availability
  gate 通过后启用。不可达 family 不能成为“policy 没有测试”的理由。
- pointer/touch + keyboard。
- resize/background/resume/reduced motion。
- console/log/bridge/data roundtrip。
- malformed finding fail closed。
- operator → tester → verifier 调用链分别绑定同一 app/build/profile/evidence handles；任一 agent
  未被 workflow 调用或伪造 evidence identity 时失败。
- fast=1/balanced=1/thorough=2，Canvas 拒绝 fast；在 limit 后仍有 finding 时返回
  `repair_budget_exhausted` 并点名 used/limit/finding IDs。
- Engine frame-time/FPS 记录在 report，第一版不作为 blocking finding；若未来升级为 hard gate，
  必须先提交 reference device 的数值 baseline 与 planted-failure test。

### 19.6 MCP standard contract

- MCP 2025-11-25 Tool JSON golden。
- Local App initialize 接受/返回 `LATEST_PROTOCOL_VERSION=2025-11-25`；测试故意把 CLI
  `2025-06-18` 常量接入时必须失败。
- raw `tools/list` 不出现 `server_name/full_name/search_hint/always_load/effective_max_permission`；
  Host-enriched DTO 能从 definition + `_meta` + permission context 无损重建这些投影。
- title/outputSchema/annotations/execution/icons/`_meta` roundtrip。
- structured initialize result。
- outputSchema mismatch 返回 tool error。
- protocol error 与 CallToolResult isError 分层。
- `capabilities.tools.listChanged` 与 `notifications/tools/list_changed` generation/原子 refresh。

### 19.7 Permission

- strictest-wins duplicate policy。
- exact FQN、server wildcard、tool glob。
- Host Allow + org Ask → Ask。
- user Allow + Host Deny → Deny。
- auto/bypass 不绕过 admin/Host ceiling。
- requiresUserInteraction 每次提示。
- capability receipt 与 MCP permission 独立生效。
- **`permission_policy` 与 `toolPermissions` 是两条独立管线（§14.1）**：
  `always_allow` 生成的规则在默认 permission mode 下**直接授予**（不弹窗）；
  而 `toolPermissions=allow` 只解除封顶，**不授予**。两者各自有用例。
- 端到端构造 manifest `permission_policy: deny` 的精确 FQN，authorization 必须为 Deny 且 citation
  source=`McpServerPolicy`；删除 producer 对 bucket 的插入必须使该测试红。不要只断言已存在的
  `SOURCES_BY_PRIORITY` 包含枚举值。

### 19.8 Per-App MCP

- 每个 published App 有 1–16 meaningful tools（§1.7）。
- zero-tool proposal 返回 `mcp_authoring_required`，保留 candidate/staging，不生成 receipt、
  不 promote active build，不注册 logical server。
- Update/revise 的 zero-tool candidate 不改变 previous active catalog/listChanged generation。
- 补足用户目标后从同一 journal resume，生成至少一个通过质量门的 tool 才可 publish。
- filler/duplicate/unbound tool 拒绝。
- tool 以用户 workflow 为粒度；机械 CRUD 拆分、无界 list result、超过 response limit 各有反向用例。
- user goal/proposal/catalog/build digest 交叉验证。
- App A/B isolation。
- app ID 原样进入 server segment；`abc-123`、`abc--1`、`abc-` 都能经所有 FQN parser roundtrip，
  54-char ID 被接受而 55-char 被 schema/Host 同时拒绝。
  `abc` 与 `abc--1` 共存时 exact/server-wildcard permission 互不匹配；种回 `-`→`_` 必须红。
- last-listed surface stale 时拒绝；surface refresh 后成功。Build-only update 改变 catalog/build digest
  但 surface 不变时，既有 connection 无 notification、无 stale error，并调用新的 execution binding。
- create/update/standalone revise。
- standalone authoring 完全 unchanged 时不生成 revision/receipt/listChanged。
- Update 只改变 build/execution binding 而 tool surface 不变时：完整 catalog/build binding 原子更新，
  `authoring_revision` 与 `tool_surface_sha256` 不变，不发送 listChanged；name/title/description/
  inputSchema/outputSchema/annotations/execution/icons/可见 `_meta` 任一变化时才递增 surface
  generation 并发送。
- candidate failure 保留 active catalog。
- crash recovery/atomic promote/rollback。
- source-only repair 且 design/dependency/approval contract 不变时不重新确认；description/schema/
  semantic Flow/ceiling 任一变化时旧 receipt 失效。Update approval contract 不变时不生成 MCP receipt，
  变化时必须显示 §17.4a diff。
- create receipt 绑定安装前 dependency input；receipt 后改 requested/effective/base-lock 任一输入必须
  返回 drift，不得用安装后的 snapshot digest 假装 receipt-time 值。
- 33-step AppMcpFlowBinding、513-byte/33-segment pointer、depth 9、单值 >64 KiB、累计 >256 KiB
  各自被拒并点名 actual/limit；generic 128-step Flow 路径保持可用。
- Plugin disabled 后 active server 继续可用。
- 4 in-flight/App、8 in-flight/conversation、60 read calls/App/min、10 mutations/tool/min 与
  5-minute timeout 的边界/超界用例；超界返回 `rate_limited`/`retry_after_ms`，cancel 释放 slot。

### 19.9 Scaling（每条必须带数字阈值，否则不是门）

- cold-start 相对 0 App 的 added p95：30 published Apps ≤ 75 ms，100 Apps ≤ 200 ms；
  超限输出 tier/device/actual/limit。
- **注册 100 个 logical server 后 physical transport 实例数仍为 1**，
  并配反向用例：故意每 App 建一个 transport ⇒ 必须失败。
  （原「physical hub 数量恒为 1」「无 N 个进程/socket」在基线 `5448e93e3` 上**已经成立**，
  写第一行代码前就是绿的，不构成门；必须绑定到 100-server 场景才有意义。）
- 100 logical server 注册 p95 ≤ 250 ms；Host logical registry + hub + broadcast/filter + 最多 8 个
  exposed `Connected.tools` snapshots 的 retained heap 增量 ≤ 16 MiB，失败输出每类对象贡献。
- exposure set 最大 8，超出即淘汰。
- LRU 不淘汰 pinned/in-flight server。
- successful call 更新 recency 但不永久 pin；eviction 后 `LocalAppList/Get` 可重新 expose。8 个用户
  pin 占满时第 9 个返回 `exposure_capacity_reached`，不越过上限。
- activation/eviction listChanged 正确。
- 单 App definition ≤2,048 estimated tokens；128 expanded tools 总量 ≤16,384。Hermetic counter
  与 `wire.rs` 一致：UTF-16 `name+description+JSON.stringify(input_schema)` / 2.5（分别等价
  5,120/40,960 units）；`count_tokens_exact` 只记录 evidence，不决定门。超限输出 server/tool
  贡献排序，以便缩短 schema/description，而不是静默截断成无效 JSON Schema。

### 19.10 Client

- `APPS_SCHEMA_VERSION=3`、`CLIENT_PROTOCOL_VERSION=10.0.0` golden；schema v2 fixture 被拒绝，
  不存在 v2→v3 migration/defaulting branch。
- Rust protocol snapshot/breaking-major gate。
- iOS/Android generated bindings。
- `@lingxi/bridge-client` TypeScript DTO/validator 与 Rust snapshot parity，Electron typecheck/tests 通过。
- Plugin toggle/retry-once。
- create receipt expiry/replay/cross-App/superseded。
- MCP proposal approval diff：删除 tool X 必须逐行显示 `removed: X`；approval unchanged 不弹 sheet。
- managed server inventory。
- UI/MCP verification 独立显示。
- §17.1 每个 code/status 都有 production trigger 与 UI recovery test；特别区分 client error、
  verification state、MCP CallToolResult error，不能只 snapshot enum 字符串。
- **i18n key-set/staleness 门**：CI 执行 `python3 clients/translations/generate.py --check`，成功时
  打印 `OK: <n> keys, 5 locales`；故意删 generated file、改旧产物、留下 orphan 时必须分别以
  `missing generated file`/`is out of date`/orphan 诊断失败。开发者先无 `--check` 重生成，再验证
  工作树无额外 diff；不能用“生成两次第二次没 diff”代替 check mode。
- Phase 9 删除旧 selector 后，obsolete selector-only key allowlist 在五个 locale 中计数为 0；
  `health_*`/`family_*`/`surface_*` 仍能通过源代码引用测试，禁止按公共前缀清空。

### 19.11 Desktop 回归（Phase 0a/0b 的爆炸半径）

桌面端不安装本 Plugin（§1.8），但它是 `plugin/` 通用契约唯一的真实使用者，
Phase 0a/0b 的改动必须在这里验证，否则移动端全绿也可能静默弄坏桌面：

- [0a] 桌面 manifest-backed plugin discovery / 安装态 / 启停快照回归基线；`.js` workflow
  namespacing/resolver 与现有 scriptPath 路径不回归。
- [0b] manifest-less/skills-directory plugin 在桌面可发现，且不改变有 manifest plugin 的优先级/identity。
- [0a/0b] Plugin agent frontmatter 解析；0a 覆盖正常/nested，0b 覆盖缺 name/YAML 损坏 fallback。
- `${LINGXI_PLUGIN_ROOT}` / `${LINGXI_PLUGIN_DATA}` / `${LINGXI_PROJECT_DIR}`
  在 hook command、MCP command/args/env/url/headers、LSP、monitor 四个位点的替换。
- 第三方 plugin 的 commands/skills/agents/outputStyles/hooks/MCP/LSP 装载不回归。
- [0b] Plugin themes/monitors 的 default/custom discovery、monitor trust/activation、mid-session disable
  retention 与 session-end cleanup 不回归。
- 桌面端 MCP inventory **不出现** managed Local App source。
- [Phase 9] `tasks`/`tools-workflow` 的两份 Local App workflow 名单与
  `apply_local_app_build_default_model` 已删除且无 dead-code allow；桌面 builtin workflow 行为仍绿。

## 20. 完成定义

本方案只有在以下条件同时成立时完成：

1. Local App skills、agents、workflows 和 templates 只由 name=`lingxi-local-app`、
   source=`PluginSource::BuiltIn` 的 Plugin 提供。
2. Host 普通业务模块、`tasks` 与 `tools/workflow` 不硬编码提示层 component names；workspace
   lease/delete guard/workflowModel 只读 typed task scope。
3. 移动端使用现有 PluginManager；`register_verified_builtin` 在 composition module 恰好一个
   调用点，bundle descriptor 外没有 template/plugin `include_*` 入口（由 §19.3 可红 scanner 断言）。
4. LingXi 品牌路径和环境变量贯穿产品，同时 Phase 0a/0b 行为 fixtures 与 Claude oracle 对齐。
5. Agent 选择 template/Profile，Host 先写 run-scoped validated selection，再在 design/MCP/dependency
   approval contract 完整后通过 Native receipt 原子提交 draft。
6. 已 scaffold App 只从自己的 Profile binding 和 template snapshot build/restore。
7. DOM 与所有可达 Canvas/engine QA 强门有独立 executable tests；Babylon runtime 不可达期间仍有
   hermetic policy tests。
8. 每个 published App 有独立 logical MCP server 和 **1–16** 个验证过的业务 tools；
   zero-tool candidate 停留在 `mcp_authoring_required`，不能 publish（§1.7）。
9. 所有 per-App servers 复用一个 physical in-process hub；surface 不变的 build update 复用连接并
   调用新 execution catalog，surface 变化才增加 generation/listChanged。
10. per-tool permission ceiling、App capability 和 connection scope 全部 fail closed。
11. Candidate build/catalog 失败或崩溃不会改变 previous active state；create draft 与 candidate
    staging 的崩溃恢复边界明确。
12. iOS/Android protocol/UI、Electron shared protocol typecheck、desktop generic Plugin 回归、
    workflow、MCP、permission 与 Rust workspace 测试全部通过。
13. 本节每一条都能指到 §19 中一个**会红的**验证项；不能指到的条目要么补门，要么删除。

## 21. 明确保留的风险

- Babylon 仍被真机 availability gate 阻塞；这不是开放设计决策。
- Mobile PluginManager 接入会增加 binary size 和 startup work，由 §19.2 的
  bundle size 与 cold-start 两条**带阈值**的门约束（不是泛指的 benchmark）。
- **builtin bundle 物化失败会使 Local App 的 create/update/MCP revise 整体不可用**——
  这是本方案新引入的故障模式（今天模板是二进制常量，不存在该故障）。
  已 scaffold App 不受影响（§6.2、§9.6）。
- 提示层随客户端版本发布：改一句 skill 提示词需要发一次客户端版本。
  若将来需要独立更新，路径是给 `BuiltinPluginBundle` 增加签名的 OTA source provider，
  复用 §6.2 的 materialization 与 digest verification，**不是**改走 marketplace/install。
- 通用 Plugin 供应链校验（archive fetch 的 scheme/host/digest 守卫等）不在本方案范围，
  因为本 Plugin 是 builtin-only；一旦将来开放可安装 plugin，那组守卫是前置阻断项。
- 8-server/16,384-token exposure budget 是第一版 hard limit；只有真实 Tool Search/会话基准
  证明需要调整时才走独立设计变更，改变数值不改变 logical server 架构。
- 任一平台缺少所需 UI evidence runner 时只能报告 UI verification unavailable，但 MCP catalog
  自身仍必须完成 schema/Flow/call/isolation QA；Android 与 iOS 现有前台 automation/capture 能力
  都必须纳入复核，不能笼统写成“Android 没有 runner”。
- Claude oracle 会继续变化；只在 LingXi 实际使用的 component surface 上按版本更新 fixtures，不把 unrelated marketplace/install work 塞回本方案。

## 22. 源码复核账本

以下事实均在 `5448e93e3` 工作树于 2026-08-29 复核；实施时若基线变化，先更新事实和受影响
决策，不能只改数字：

| 事实 | 复核结果 | 设计影响 |
| --- | --- | --- |
| Runtime template macro | `profile_file!` 调用 112 个 | migration manifest 的初始真源 |
| Runtime template tracked files | 124 个 | 多出的 12 个是四个 Canvas family 各 3 个未引用 orphan |
| 112 个实际 scaffold 文件 raw bytes | 376,131 bytes | §19.0 bundle size 预算不是现状追认 |
| Runtime Profile toolchain/core packages | `pnpm@11.22.0/node@24.18.1`；React 19.2.8/Ionic 9/Vite 8.2.1；engine versions 见 §9.1.1 | Plugin 搬迁必须保持 contract digest |
| Builtin Plugin source | `PluginSource::BuiltIn` 已存在，`install(BuiltIn)` 明确拒绝 | 新增 verified registration seam，不新增 source/第二 loader |
| Plugin identity | `PluginId` 是 `plg:<uuid>` opaque live ID | 持久 provenance 使用 name + source + bundle digest |
| Workflow name resolver | 当前 `WORKFLOW_EXTENSIONS` 探测 `.js/.mjs/.ts/extensionless` 且 builtin-first；它不是 oracle directory scan | Phase 0a 改为 named saved/plugin `.js`-only、saved meta.name > plugin > builtin；explicit scriptPath 独立 |
| Local App workflow guards | `tasks/src/lib.rs` 与 `tools/workflow/src/lib.rs` 各有两名数组 | Phase -1 以 typed task scope 替换 lease/delete/model-default 名称判断 |
| App publication state | `AppRecord.workflow_state` 在 build 后由 `mark_ready` 写入 | schema v3 删除持久字段，active build/catalog pair 派生 client state |
| App ID / MCP server length | 当前 App ID 允许 64；`local_app_` 前缀后超过 server token 64 上限 | schema v3 收紧 App ID 至 54，Host mint 仍为 8 位 hex，不做字符映射 |
| Workspace permission asset | `permissions.rs` 的旧 template JSON 由 create 事务写入每个 App | 搬到 Host-owned production asset，不作为 fixture/Plugin template |
| Plugin Agent oracle | 官方支持列表不含 `color`/`initialPrompt` | 通用 parser 可识别不等于 Plugin surface 可接受 |
| Permission source | `PermissionRuleSource::McpServerPolicy` 已存在但无 producer | §14.1 直接复用现有 rule walk |
| Client protocol | `CLIENT_PROTOCOL_VERSION = 9.0.0` | 本方案 breaking target 是 `10.0.0` |
| Translation source | 五个 locale 各 1894 keys，`generate.py --check` 输出一致 | §17.5/§19.10 使用动态 `<n>`，不钉旧数字 |
| Runtime-profile 文案使用 | selector/status/health/family/surface 仍分别被 Swift/Kotlin 引用 | Phase 9 只删 obsolete selector-only keys |
| Local App skills | 10 个目录、44 个 Markdown、1773 行 | Phase 2 是搬迁 10 + 新增 17，不是新增 Phaser/Babylon |
| Desktop Local Apps | `engine-desktop/src` 无 Local App Host/runtime | 桌面端不安装本 Plugin，但承担通用 plugin 回归 |

上游 oracle 于同日重新读取：Claude Code Plugins Reference/Workflows 与 MCP 2025-11-25
Tools。MCP Tool 的 `icons`、`outputSchema`、`annotations`、`execution.taskSupport`、
CallToolResult `structuredContent` 和 `notifications/tools/list_changed` 均保留在 §13/§19.6；LingXi 只做品牌
命名映射，不改变这些 wire fields。
