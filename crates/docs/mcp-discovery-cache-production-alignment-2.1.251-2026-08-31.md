# MCP discovery cache production alignment（Claude Code 2.1.251）

日期：2026-08-31
范围：MCP discovery cache Stage 3、resources/prompts 惰性拨号、MCP grant 分区、protocol-era negotiation、agent/comms routing、desktop/mobile production wiring
实现基线：`8469d341c`（主实现）+ `f11dbce16`（全 workspace 兼容收口）
详细交付报告：[mcp-plugin-deferred-completion-2.1.251-2026-08-31.md](./mcp-plugin-deferred-completion-2.1.251-2026-08-31.md)

## 1. 结论

本轮将此前列出的六项实现缺口收敛为可运行、可测试、可审计的完成矩阵：

- desktop 与 mobile composition root 都能把 `DiscoveryCacheStore` 接到 `McpRegistry`；mobile 现在同时支持内置 Local Apps 与共享 HTTP/SSE remote MCP。
- cache identity 只由 MCP 配置、稳定 MCP metadata、协议期望值和 MCP server 自己的 OAuth refresh grant 构成；不读取、保存或推导任何 LLM provider credential 或 account UUID。
- provenance gate、post-hit capability gate、fresh/stale/miss、single-flight lazy upgrade、精确 strike 与 lifecycle purge 已贯通。
- protocol era 不再固定为单一路径：默认仍保持安全的 legacy 行为，但 opt-in auto 会做 modern probe、兼容回退和 envelope 校验；expected era 与实际 negotiated era 分开保存。
- `role:"comms"` 与 `agentSource` 已从配置解析传到 cache identity、工具元数据和 coordinator worker 路由。

这份 alignment 只声明当前实现和测试覆盖的 MCP/cache surface，不声称所有 Claude Code MCP 产品能力均已移植。明确非目标是：不实现完整 notifications 产品、不实现完整 Claude channel 产品、不实现完整 Claude/marketplace 产品；也不涉及任何 LLM provider auth、profile、credential 或 account UUID。

## 2. 审查 oracle 与基线

本轮继续以固定的 Claude Code 2.1.251 本地二进制作为 alignment oracle：

```text
/Users/luolingfeng/.local/share/claude/versions/2.1.251
SHA256 625869b01e0050f260b2980fac248fd9cef9e462612bded4ec9d3d49ff8969a5
```

本轮代码基线链为：

```text
c5c46f9707fe28840205e6d7144b735404e9d2ce  parent baseline
8469d341cddf3c72e68c5add6c0869a7025d6052  primary implementation
f11dbce16                                      all-target literal/clippy integration
```

oracle 取证仍限于固定 binary literals、schema、telemetry 与行为；没有把源码注释自述当作 oracle。实现提交的约束也保留：`MCP_DISCOVERY_CACHE` 与 automatic protocol negotiation 默认关闭，MCP/plugin 代码不得读取 LLM provider credential 或 account UUID。

## 3. 认证边界：MCP OAuth，不是 LLM provider auth

| 输入 | 是否进入 MCP cache partition | 说明 |
|---|---:|---|
| Anthropic/OpenAI/Gemini/其他 LLM provider credential | 否 | 属于模型 provider 请求，MCP cache 不读取 |
| Anthropic OAuth/profile/account UUID | 否 | MCP 路径不读取、不持久化、不推导 |
| MCP server access token | 否 | 短期 bearer，不是稳定 grant identity |
| MCP server refresh token | 是，先哈希 | 仅表示远程 MCP grant，原文不进入文件名或日志 |
| 没有 MCP token row | 是 | 映射为 `grant:none` |
| token row 存在但没有 refresh token | 禁止缓存 | 返回 `no-fingerprint`，避免错误共享 |
| MCP secure storage 无法读取/损坏 | 禁止缓存 | fail closed，不读、不写 |

固定兼容域 `acct:logged-out` 只是复现 oracle hash material 的 provider-neutral domain；它不是 LingXi 登录/登出状态，禁止替换成 provider profile、credential 或 account UUID。

## 4. Identity、metadata 与分区

### 4.1 `McpServerMetadata`

`mcp/src/connection.rs` 的 `McpServerMetadata` 是 MCP-local metadata，字段为：

| 字段 | 作用 | 是否改变 logical key |
|---|---|---:|
| `transport` | 保留 enum 投影会丢失的原始 transport label（例如 `claudeai-proxy`） | 是（仅显式存在时） |
| `role` | 当前唯一运行时值为 `comms`，用于 coordinator routing | 是 |
| `agent_source` | Agent inline MCP 的稳定来源：`built-in`、`plugin`、`userSettings`、`projectSettings`、`policySettings`、`flagSettings`、`additionalDirectory` | 是 |
| `cli_owned` | `--mcp-config` 显式注入标记 | 否；cache gate 拒绝 |
| `ambient_credential` | MCP-only 临时 credential 注入标记 | 否；cache gate 拒绝 |

普通空 metadata 被 serde 忽略，以保持旧配置的形状和 key bytes。Rust state 的字段名是 `agent_source`；logical-key canonical JSON 使用兼容名称 `agentSource`，两者不是两个独立身份。

来源已闭环：

- `mcp/src/json_config.rs` 解析 transport、`role:"comms"`、`discoveryCache` 并保留被 enum 投影丢掉的 raw transport label。
- `apps/cli/src/init.rs::parse_cli_mcp_servers` 将 `--mcp-config` 结果标成 `cli_owned`。
- `agent/src/mcp_servers.rs` 将 inline Agent `AgentSource` 映射为 `McpAgentSource`；按名称复用既有 config 时不伪造新的 source。
- host 可显式设置 `ambient_credential`；这类配置只允许 live，不会污染已有 partition。

### 4.2 Grant、fingerprint、logical key、partition

```text
no stored MCP token row:
    grant_token = "grant:none"

stored refresh token:
    grant_token = "grant:" + SHA256(refresh_token)[0..16]

fingerprint = SHA256("acct:logged-out" + NUL + grant_token)

logical_key = server_name + "-" + SHA256(canonical_config_json)[0..16]

partition_key = SHA256(
    logical_key + NUL + fingerprint + NUL +
    "era:" + expected_era + NUL + "2.1.251"
)[0..32]
```

canonical config 会递归排序 object key，并排除 discovery 结果、state、scope、plugin path/source、config error、`discoveryCache` 等非身份字段；显式 timeout/alwaysLoad 与 metadata identity 字段按当前实现规则进入。固定向量继续保持：

```text
refresh-token -> grant:0eb17643d4e92611
grant:none -> 856f0d2375be22a510e79662f22d30c51c14dc3394b9d610af33a7116d81cda6
legacy logical-cache-key -> a6fad12e13235da65ecc9b068d2c62b6.json
```

Agent scope 没有稳定 `agentSource` 时返回 `no-fingerprint`，不会降级共享普通 server partition。access token 轮换不改变 grant；refresh token 轮换一定改变 grant。

## 5. Protocol era：expected 与 actual 分离

`platform-api/src/mcp.rs` 定义 `McpProtocolEra::{Legacy, Modern}`、`McpNegotiatedProtocol { era, version }`、`McpConnectOptions { expected_era, deadline_ms }` 与 `McpConnectResult::negotiated`。`mcp/src/protocol_negotiation.rs` 负责从 `MCP_PROTOCOL_NEGOTIATION`、transport gate、feature flag 和 server denylist 得到期望值：

- `legacy` 明确选择单次 legacy initialize；
- `auto` 对 eligible HTTP/stdio 路径先做 modern `server/discover` probe；
- 未设置或 feature flag 关闭时仍选择 legacy，保证默认行为不变；
- probe 不支持 modern 时关闭 probe connection、重新拨 live connection，再以 legacy initialize；
- modern 成功时保存准确版本 `2026-07-28`，legacy 为 `2025-11-25`。

cache partition 编入 `expected_era`，entry 另外保存 `negotiatedEra`（旧 entry 缺失时按 legacy 兼容）。因此“期望使用 modern partition”与“这条真实连接最后协商到 legacy”不再混为一谈。

stale revalidation 的 `LazyUpgradeSlot` 同时保存：命中 partition 的 expected era、entry 的 actual era、cached connection id 和 config snapshot。revalidation 只有在 generation/config 仍匹配且 actual era 与 entry actual era 相同才安装 live generation；era 改变时只 purge 提供 stale hit 的 partition、保留当前 `Cached`、不记录 strike、不写 replacement partition。该状态机由 `stale_refresh_era_change_purges_hit_partition_without_replacement_or_strike` 锁定。

## 6. Cache gate、post-hit 顺序与 fresh/stale/miss

registry 在已有 `Connected` 或 `Cached` state 上先短路复用；这对应 oracle 的 live-connection 语义，`decide_with_metadata` 不会把它重新伪造成一次磁盘 miss。对需要新一次 discovery 的 config，门控与判定顺序为：

1. feature gate：`MCP_DISCOVERY_CACHE` 未开启即 `FeatureDisabled`。
2. transport gate：只有 HTTP/SSE 具备 cache eligibility；其他 transport 为 `Transport`。
3. provenance gate：按 `cli-owned` → unresolved `${VAR}`（remote URL/header）→ `ambient-credential` 检查。
4. 用户可改变的 purge gate：`discoveryCache:false` 为 `OptOut`，再检查 `headersHelper` 为 `HeadersHelper`。
5. `OptOut` 与 `HeadersHelper` 会 best-effort purge server family；其余 gate（feature、transport、provenance）只 live miss，不 purge。
6. gate 通过后计算 Agent source、MCP grant fingerprint 与 expected-era partition；secure storage/refresh grant 不可用时为 `NoFingerprint`。
7. 读取 entry 后依次检查：absent/corrupt、strike threshold、future-clock/max-stale、degenerate zero-tools、skills capability、channel capability。
8. 通过 post-hit 检查后才按 age 分为 `Fresh` 或 `Stale`；fresh/stale 都先安装 cached catalog，再决定是否 live dial。

具体结果：

- `Fresh`：立即安装 `McpConnectionState::Cached`，发出 `cache_fresh`，不拨 transport；第一次真实 tool/resource/prompt 调用再单飞 lazy dial。
- `Stale`：立即安装 cached catalog，发出 `cache_stale`，后台 single-flight live revalidation；foreground waiter 可以加入同一个 owner。
- `Miss`：按原因发 telemetry（磁盘不可用的 absent/expired/corrupt/strike/no-fingerprint）后 live discovery；provenance/transport/feature 等门控 miss 不假装磁盘读过。
- background failure/panic 仅在 cached generation 仍匹配时对命中 partition 记录 strike；普通 initial connect failure 不 strike。
- generation/config 替换、disconnect/remove 或 CAS reject 会拒绝旧 owner，并对未发布 transport 做有界 cleanup。

post-hit capability 具体为：`tengu_mcp_skills` 开启且 entry 同时有 resources 与 `extensions["io.modelcontextprotocol/skills"]` 时 `SkillsCapable`；`experimental["claude/channel"] == true` 时 `ChannelCapable`。skills 优先于 channel。这里实现的是 cache safety gate，不是完整 skills/channel 产品。

## 7. Write-through、grant rotation 与持久化安全

OAuth resolve/refresh/step-up 在生成实际 `connect_spec` 时一并返回不可变 grant provenance；live discovery 用它计算 partition，而不是在认证 transport 建立后才把“当前 storage”误当成这条连接的 grant。catalog RPC 完成后重读 MCP secure storage只做 equality check；若 shared/agent-scoped 并发导致 refresh-token rotation，本次 write-through 直接跳过，stale revalidation 也只淘汰旧 partition，不会让旧 bearer 的 catalog 落入新 grant partition。OAuth token row 存在但没有稳定 refresh grant 时 cache read/write fail closed。写入 entry 同时记录实际 `negotiatedEra`。

持久化边界：

- desktop root：`<lingxi_home>/mcp-discovery-cache/`；mobile root：`<cfg.lingxi_home>/mcp-discovery-cache/`；store 已注入但 feature 默认关闭。
- root mode `0700`、entry mode `0600`、单 entry 上限 8 MiB。
- no-follow + regular-file 校验；symlink、非普通文件、超大、schema/key mismatch 都是 corrupt miss。
- exclusive staging file + atomic rename；staging 名只含 partition digest 与内部 nonce。
- serialize catalog 前反射检查 config header/env/URL credential 与 stored MCP token/client secret，包括复合 Cookie/header 和 URI percent-encoded token（hex 大小写）。secure storage 读取失败则拒绝写入。
- purge 不跟随 symlink；lifecycle purge 只删除 cache family，不隐式撤销 plugin 的 OAuth grant。

## 8. Resources、prompts、plugin 生命周期

Cached catalog 对 tools、resources、resource templates、prompts 一视同仁：

| 表面 | Cached 行为 |
|---|---|
| `ListMcpResourcesTool` 指定 server | `ensure_connected_client` 后执行 `resources/list` |
| `ListMcpResourcesTool` 全 server | 逐 server best-effort lazy dial，单个失败不阻塞其他 server |
| `ReadMcpResourceTool` | lazy dial 后执行 `resources/read` |
| `ReadMcpResourceDirTool` | lazy dial 后重新读取 live capability，再执行 directory read |
| `connected_prompts` | 从 cached catalog 构建 prompt command |
| `get_prompt` | 以 cached generation 为 predecessor，lazy dial 后核对 live generation/config |

disconnect/remove/plugin unload 都会 best-effort purge 对应 server cache family；I/O 失败记录日志，不把本地 registry 状态卡在 live。plugin unload/remove 使用不撤销 OAuth 的路径，避免卸载插件造成远程 server 被登出；显式 remove 仍遵循现有 revoke 语义。

## 9. Production composition：desktop 与 mobile

### Desktop

`apps/engine-desktop/src/lib.rs` 生产 composition root 同时注入：

```rust
McpRegistry::with_raw_conn(...)
    .with_discovery_cache_store(DiscoveryCacheStore::new(
        cfg.lingxi_home.join("mcp-discovery-cache")
    ))
    .with_oauth(...)
```

同一 `McpRegistry` 供 orchestrator、plugin runtime、MCP tools、prompts 和 catalog refresh 使用；已有 `build_wires_mcp_discovery_cache_store` regression test 验证 store 可达。

### Mobile

mobile composition 现已接入 discovery-cache store，并同时承载 Local Apps 与共享 HTTP/SSE remote MCP。`apps/engine-mobile/src/host.rs` 现在：

1. 从 `Platform` 取得 HTTP、clock、secure storage、deep-link opener；
2. 构造 `RemoteMcpTransport` 与 `MobileMcpTransport` composite；
3. 构造带 raw connection bridge 的 `McpRegistry`，注入 mobile cache root；
4. 先连接内置 `LocalAppsMcpTransport`（`InProcess`），再用共享 parser 读取 app-private `settings.json` 与 project `.mcp.json` 并连接 HTTP/SSE；
5. 让 mobile ToolRegistry 订阅 catalog changes，cached/live replacement 后重建 MCP tools。

`MobileMcpTransport` 只路由 connection id，不按 server name 猜路由：`InProcess` 委托 Local Apps，`Sse`/`Http` 委托共享 remote；`WebSocket`、stdio、IDE 与 SDK control 不在 mobile composite 的支持集内。Local Apps 仍 cache-ineligible，带 store 的 mobile registry 连接它不会创建 discovery-cache 目录。

## 10. OAuth encrypted storage 与 deep-link 安全边界

mobile 只在 `Platform::secure_storage()` 返回 `is_encrypted() == true` 时把 OAuth deps 接入 registry。没有原生 Keychain/Keystore 时使用 non-persisting development stub，但 `mobile_mcp_preflight` 会在任何 remote OAuth dial 前把 config 标成 `MCP OAuth requires an encrypted secure credential store`；因此不会先做 OAuth discovery、remote HTTP 或 plaintext token write。注入真实 encrypted store 后，OAuth load/refresh/PKCE persist 路径才可用。

授权 URL 回调先写入 host-owned 的 copyable slot，再尝试 `DeepLinkOpener::open`；opener 不可用或失败只留下可复制 URL并记录 warning，不暴露 access token 或 PKCE verifier。该 slot 通过 UniFFI getter 暴露，configured remote connect 在后台运行，因此 loopback callback 等待不会阻塞 engine build。MCP OAuth token 仍只进入 `mcp-oauth` secure-storage service；cache 只保存 refresh-grant 的短哈希 fingerprint，并在 write-through 做 secret reflection refusal。

mobile startup 在 spawn 前把初始配置写入同一 desired-intent map，并通过 guarded job 连接；boot A 因此不能在 reload 到 B 后发布。listing reload 比较完整 config snapshot：未变 settled server 不 dial、不 purge、不 revoke；修改/删除进入 per-server 后台 generation/CAS reconciliation。每个 server 的 generation state 另存 latest desired snapshot，且 reconciliation 包含 registry、tracked intent、磁盘配置三者的名称并集，所以 `A→B→A`、`A→deleted→A` 以及 remove→connect 的无 state 窗口都会失效旧 job；matching config 若仍为 `Connecting`/`AwaitingOAuth`，latest generation 会重建 owner。guard 在 lifecycle lock 内和 cache/live/error publish 前复查，stale discovery 会断开而不发布，exact pending cleanup/conditional remove 复用完整 client/catalog/lazy/cache retire 且不撤销 OAuth。disabled config 只 seed `Disconnected`、不拨号。因此 listing 不等待 pending OAuth/connect，过期 startup/reload generation 也不能覆盖最新配置意图。

## 11. 验证结果与已知风险

本轮直接复核的结果：

```text
cargo test -p jsonrpc -p mcp -p agent --lib --quiet
jsonrpc 63、mcp 592、agent 358 全部通过

cargo test -p tool-mcp -p plugin --quiet
tool-mcp 148 及 plugin 全 suites 通过

cargo test -p tasks --lib --quiet
291 passed；1 个 latest-main Local App grammar baseline failure

cargo test -p platform-common -p platform-posix --quiet
全部 unit/integration suites 通过；modern negotiation E2E 7/7

cargo test -p engine-desktop --quiet
181/235 lib passed；54 个 latest-main Local App workflow baseline failure

cargo check --workspace --all-targets
passed
```

最终组合树的 `engine-mobile --features uniffi` 结果为 472/537；同一命令在未合入本分支的 `main@bef351cd9` 上为 449/514，二者有相同的 65 个 Local App baseline failure。本分支增加的 23 项测试全部通过，mobile MCP 定向集为 13/13。代表性 baseline failure 可稳定复现：

```text
local_apps_build::tests::a_failed_build_cleans_up_its_staging_directory
InvalidRequest("templateOrigin/dependencySnapshot requires a matching surface and runtimeProfile")
```

该失败在 `main@bef351cd9` 上独立复现，不是 MCP/cache 行为失败。desktop 的代表性 plugin-workflow registry failure 也在该 main 上复现；tasks 另有一个未修改 `scope.rs` 的 app-id grammar baseline failure。本报告不把这些 Local App 风险或既有 warnings 伪装成 workspace-wide 全绿。

补充并发与 latest-main API seam 回归后，集成复核确认：`workspace/all-targets`、core MCP/plugin、platform、iOS framework/Android AAR checks 均通过；mobile/desktop/tasks 的上述 baseline tests 仍红。新增回归验证了 blocked startup 期间重复相同 reload 只拨号一次且不 teardown 当前 owner。该结果不应被概括为 workspace 全绿。

`git diff --check` 与文档结构检查在本次文档修改后执行；最终状态见详细交付报告第 14 节。

Sol xhigh 最终只读复审结论：**APPROVE — zero unresolved P0–P3**。

## 12. 六项完成矩阵（替代历史残余列表）

| # | 历史残余项 | 当前状态 | 实现落点 | 回归/证据 |
|---:|---|---|---|---|
| 1 | Agent `agentSource` 稳定 source identity | ✅ 已完成 | `McpServerMetadata.agent_source`；inline source 全量映射；Agent 无 source 直接 `NoFingerprint`；logical key 纳入 `agentSource` | 7 个 source 映射/identity、7-way logical-key separation、registry missing-source fail-closed |
| 2 | Protocol era 与 partition | ✅ 已完成 | `McpProtocolEra`/`McpNegotiatedProtocol`；immutable negotiation decision、budgeted modern probe、fallback、corrective retry、modern envelope；partition 纳入 expected era，entry 保存 actual era | protocol E2E 7/7；wrong-id、probe clamp、era/mode-change stale tests |
| 3 | `role` runtime consumer | ✅ 已完成 | parser 识别 `role:"comms"`；MCPTool 保留 `mcp_role`；coordinator worker 过滤 shared/inline comms 以及 generic `MCP`/`McpAuth`，普通 worker 保留；Cached/Connected rebuild 一致 | tool-mcp role rebuild；agent dispatcher/routing；plugin load→cache→refresh integration |
| 4 | `cli-owned` / `env-placeholder` / `ambient-credential` gate | ✅ 已完成 | provenance gate 按固定顺序执行；三者均 non-purging live miss；opt-out/helper 仍是唯一 gate purge | `discovery_cache::provenance_gates_are_ordered_and_non_purging`；registry provenance read/purge test |
| 5 | skills/channel/live post-hit | ✅ 已完成（cache gate 范围） | live state 在 cache 前短路；entry post-hit 依次检查 skills→channel→Fresh/Stale；独立 skills flag；channel marker safety gate | `skills_flag_is_independent_and_skills_miss_precedes_channel_miss`；live/cached registry tests |
| 6 | Mobile remote MCP 与 production wiring | ✅ 已完成（支持集有边界） | shared `platform-common::RemoteMcpTransport`；mobile Local Apps + remote composite；cache root/store；后台 OAuth、exported copy URL、generation/CAS reload | shared remote E2E 3/3；mobile composite 7/7；cache rebuild/reload/deep-link/encrypted-store/guarded-interleaving tests；fixture 风险见第 11 节 |

## 13. 文件职责摘要

| 文件 | 职责 |
|---|---|
| `mcp/src/connection.rs` | `McpServerMetadata`、Agent source、role、Cached/Connected state schema |
| `mcp/src/discovery_cache.rs` | gate、logical/partition key、fingerprint、fresh/stale/miss、post-hit、store hardening |
| `mcp/src/protocol_negotiation.rs` | env/flag/denylist 到 expected era 的纯决策 |
| `mcp/src/registry.rs` | consult/read/write、immutable grant provenance、single-flight、exact strike、lazy dial、guarded lifecycle CAS、error cleanup |
| `mcp/src/json_config.rs` | transport/schema、`discoveryCache`、`role` 与 metadata 来源 |
| `mcp/src/oauth.rs` | MCP OAuth secure storage、refresh-grant token 派生、PKCE/token lifecycle |
| `mcp/src/client.rs` | live client、tool/prompt/resource dispatch 与 negotiated protocol metadata |
| `platform-api/src/mcp.rs` | transport spec、protocol era/options/result、capabilities extensions |
| `platforms/common/src/mcp_remote.rs` | 共享 HTTP/SSE wire、probe、deadline、modern envelope、connection cleanup |
| `platforms/posix/src/mcp.rs` | stdio process/reaper 与 shared remote bridge |
| `agent/src/mcp_servers.rs` | inline Agent source metadata 注入与 scoped config |
| `agent/src/tool_resolver.rs` | coordinator worker 的 `role:"comms"` 过滤 |
| `tools/mcp/src/mcp_tool.rs` | Cached/Connected MCP tool construction、role propagation、resource/prompt tools |
| `apps/engine-desktop/src/lib.rs` | desktop OAuth/cache composition、coordinator mode、catalog refresh |
| `apps/engine-mobile/src/host.rs` | mobile composition、OAuth preflight、secure store/deep link、catalog refresh |
| `apps/engine-mobile/src/mcp_transport.rs` | Local Apps/remote connection-id composite |
| `platforms/common/tests/mcp_remote_e2e_test.rs` | shared HTTP/SSE round trips |
| `platforms/posix/tests/mcp_protocol_negotiation_e2e_test.rs` | modern success、fallback、corrective retry、deadline/auth/error envelope |

## 14. 明确非目标、剩余风险与交叉引用

明确非目标：

- 不实现完整 notifications 产品；当前只提供 transport-level notification stream 与 legacy list-changed 路径的既有接线。
- 不实现完整 Claude channel 产品；`claude/channel` 只作为 cache post-hit safety marker。
- 不实现完整 Claude/marketplace/plugin 产品面；本文只覆盖 plugin-owned MCP cache lifecycle 与已验证 routing seam。
- 不读取、保存、派生或依赖任何 LLM provider auth、credential、profile 或 account UUID；MCP OAuth 是独立的 server grant。

剩余风险：

- mobile 原生 encrypted Keychain/Keystore 的具体平台实现仍由宿主注入；没有它时 OAuth remote 会明确拒绝，而不会降级到明文。
- mobile 不支持 stdio/WebSocket/IDE/SDK control；这是 composite 的明确 supported-transports 边界，不是“所有 mobile MCP”声明。
- modern feature 与 discovery cache 默认关闭；启用前仍应运行固定 vectors、protocol E2E、grant rotation、stale strike、lifecycle purge。
- `phaser_2d` fixture digest 失败需要独立 fixture owner 处理。

历史 Stage 3 报告保留当日交付记录；其中关于 production store 与 mobile remote 的旧描述只代表 2026-08-30 截面，应以本文件和[详细交付报告](./mcp-plugin-deferred-completion-2.1.251-2026-08-31.md)为准。
