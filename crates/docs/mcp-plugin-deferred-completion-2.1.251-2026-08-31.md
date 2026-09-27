# MCP plugin/cache 六项补齐交付报告（Claude Code 2.1.251）

日期：2026-08-31
实现基线：`8469d341cddf3c72e68c5add6c0869a7025d6052`
基线父提交：`c5c46f9707fe28840205e6d7144b735404e9d2ce`
实现提交：`Close deferred MCP parity without provider-auth coupling`
集成收口：`f11dbce16`（all-target DTO compatibility 与 scoped Clippy）
最终 main 合并基线：`bef351cd9e19ac8be0109058766ce62d285ddfd6`
审查收口：Sol xhigh 六轮 13 项 P1/P2 finding 已全部修复并补回归；最终结论 `APPROVE — zero unresolved P0–P3`
配套 alignment：[mcp-discovery-cache-production-alignment-2.1.251-2026-08-31.md](./mcp-discovery-cache-production-alignment-2.1.251-2026-08-31.md)

## 1. 摘要

本报告说明 `8469d341c` 对 MCP discovery/cache 与 plugin integration 历史六项缺口的真实关闭范围。实现现在覆盖：

- `McpServerMetadata` 的 schema、来源、cache identity 与 coordinator routing 传递；
- `cli-owned`、未解析环境变量占位符和 MCP-only ambient credential 的 provenance gate；
- cache hit 之后的 live-connection、skills、channel capability safety gate；
- expected protocol era 与 actual negotiated era 的分离、modern probe、兼容回退和 stale revalidation；
- desktop 与 mobile production composition；
- mobile 的 Local Apps + shared HTTP/SSE remote MCP composite；
- MCP OAuth 的加密存储前置条件、deep-link/copy URL fallback 和 cache secret 边界。

“完成”指代码路径已经接通且有对应测试，不表示所有 Claude Code MCP 产品面已经移植。尤其不声明完整 notifications、完整 Claude channel、完整 Claude/marketplace 产品，也不涉及任何 LLM provider auth、credential、profile 或 account UUID。

## 2. 基线、提交约束与 oracle

### 2.1 代码基线

本报告以 `8469d341c` 为主实现提交，并纳入 `f11dbce16` 的全 workspace 构造点兼容、后续审查修复，以及最终 `main@bef351cd9` 的 Local App plugin integration。主实现提交明确记录了两条约束：

1. `MCP_DISCOVERY_CACHE` 与 automatic protocol negotiation 仍然默认关闭；
2. MCP/plugin 代码不得读取 LLM provider credentials 或 account UUIDs。

实现使用固定的 Claude Code 2.1.251 本地 oracle：

```text
/Users/luolingfeng/.local/share/claude/versions/2.1.251
SHA256 625869b01e0050f260b2980fac248fd9cef9e462612bded4ec9d3d49ff8969a5
```

binary 只作为可验证的 schema、literal、telemetry bucket 和行为 oracle；不能用模块注释、产品名称或“看起来相似”的 provider 逻辑替代 binary 证据。

### 2.2 六项完成矩阵

| # | 原历史缺口 | 关闭结论 | 代码证据 | 测试证据 |
|---:|---|---|---|---|
| 1 | Agent 缺少稳定 `agentSource` | ✅ 完成 | `McpServerMetadata.agent_source`，inline Agent source 映射，Agent 无 source 对 cache fail closed，logical key 纳入 `agentSource` | 7 个 source 映射/identity 与 7-way logical-key separation；registry missing-source `NoFingerprint` |
| 2 | protocol era 只有 legacy 语义 | ✅ 完成 | `McpProtocolEra`、`McpNegotiatedProtocol`、`server/discover`、compatibility fallback、modern envelope、era-aware partition | `platforms/posix/tests/mcp_protocol_negotiation_e2e_test.rs` 7/7；mcp stale era test |
| 3 | `role` 只有 JSON validation | ✅ 完成 | `role:"comms"` → config metadata → `MCPTool::mcp_role` → coordinator worker filter；generic `MCP`/`McpAuth` 绕过也被移除；Connected/Cached 都保留 | tool-mcp role rebuild；agent coordinator routing/dispatcher tests；plugin load→cache→refresh integration |
| 4 | `cli-owned` / `env-placeholder` / `ambient-credential` 无 gate | ✅ 完成 | `cache_gate_with_metadata` 固定顺序；三者均只产生 live miss，不 purge | discovery-cache gate test；registry provenance test |
| 5 | skills/channel/live post-hit 未实现 | ✅ 完成（cache safety 范围） | registry 先短路 live state；`decide_with_metadata` 执行 skills→channel→Fresh/Stale | 独立 skills flag/precedence test；live/cached registry tests |
| 6 | mobile 无 remote MCP、无 production cache wiring | ✅ 完成（有明确 transport 支持集） | shared `RemoteMcpTransport` + mobile composite；mobile cache root；encrypted-storage preflight；后台 OAuth connect、导出 copy-URL getter、generation/CAS reload | shared remote HTTP/SSE E2E 3/3；mobile composite 7/7；cache rebuild/reload/deep-link/secure-storage tests |

## 3. `McpServerMetadata` schema 与来源

### 3.1 Schema

`mcp/src/connection.rs` 中的 `McpServerMetadata` 是 MCP-local metadata，不是 provider identity：

```text
McpServerMetadata {
    transport: Option<String>,
    role: Option<McpServerRole>,
    agent_source: Option<McpAgentSource>,
    cli_owned: bool,
    ambient_credential: bool,
}
```

| 字段 | serde/语义 | 来源和消费 |
|---|---|---|
| `transport` | 可选原始 transport label | `json_config` 在 enum 投影丢失 label 时保留；进入 logical key |
| `role` | `McpServerRole::Comms`，wire 值为 `comms` | `role_flag` 只接受精确字符串；进入 logical key，并传到每个 MCP tool |
| `agent_source` | `McpAgentSource::{BuiltIn,Plugin,UserSettings,ProjectSettings,PolicySettings,FlagSettings,AdditionalDirectory}` | `agent_mcp_specs_to_scoped_configs` 从 `AgentSource` 映射；进入 Agent logical key |
| `cli_owned` | 显式 CLI 注入标记 | `apps/cli/src/init.rs::parse_cli_mcp_servers` 对 `--mcp-config` 设置；只用于 gate |
| `ambient_credential` | 显式 MCP-only 临时 credential 标记 | host 可注入；只用于 gate，不进入 partition |

默认空值使用 `skip_serializing_if`，避免普通旧 config 改变形状。Rust serialized state 可能使用 `agent_source` 字段名，而 cache canonical JSON 为了 Claude-compatible identity 使用 `agentSource`；这是同一个概念的两层表示。

### 3.2 来源闭环

```text
JSON / CLI / Agent definition
              │
              ▼
      McpServerConfig.metadata
              │
      ┌───────┼────────┐
      ▼       ▼        ▼
 cache key  MCPTool  coordinator worker resolver
```

配置 parser 的规则是：

- `role` 只有 schema 允许的 transport 才检查，只有 `"comms"` 形成 enum；无效值按上游 `.catch(undefined)` 归一为无 role；
- `discoveryCache` 仅 HTTP/SSE schema 接受 boolean，`false` 是显式 opt-out；
- `--mcp-config` 解析后标 `cli_owned=true`，不会依靠 scope 字段推测 provenance；
- Agent inline record 按 definition 的 `AgentSource` 标 source；按名称复用既有 config 不伪造 source，避免改变共享 config identity。

## 4. Cache identity 与 MCP grant 分区

### 4.1 Identity 输入

cache logical key 由 server name 和 canonical config 组成。canonicalization 递归排序 JSON object，排除 discovery 结果、state、scope、plugin path/source、config error 与 `discoveryCache`；显式 timeout/alwaysLoad、保留下来的 transport label、role、agent source 等 identity-bearing 字段按实现规则处理。

```text
logical_key = server_name + "-" + SHA256(canonical_config_json)[0..16]
```

Agent scope 若没有 `agent_source`，`discovery_cache_partition_for` 直接返回 `NoFingerprint`；它不会错误地使用普通 server partition。

### 4.2 MCP grant token 与 fingerprint

只有 remote MCP 的 stored refresh token 参与稳定授权 identity：

```text
未声明 OAuth 的 remote MCP：
    grant_token = "grant:none"

声明 OAuth 且有 stored refresh token：
    grant_token = "grant:" + SHA256(refresh_token)[0..16]

声明 OAuth 但 token row 缺少/含空 refresh token：
    NoFingerprint（连接可继续，cache read/write fail closed）

fingerprint = SHA256("acct:logged-out" + NUL + grant_token)
```

`acct:logged-out` 是 oracle compatibility domain，不代表 LingXi 登录态。access token 改变不改变 grant；refresh token 改变改变 grant。refresh token 原文、access token、client secret 都不出现在 partition 文件名或日志。

live discovery 使用的 grant provenance 与产生 bearer 的那次 OAuth resolve/refresh/step-up 绑定，而不是在握手后才读取“当前 storage”。write-through 前会重读 secure storage 并与这份不可变 fingerprint 比较；若 shared 与 agent-scoped 生命周期之间发生 refresh-token rotation，本次 catalog 不会写入新 grant partition。stale revalidation 也比较 hit partition 与 live grant partition；rotation 只淘汰旧 partition，不发布旧 bearer 取得的 catalog。

### 4.3 Era-aware partition

```text
partition_key = SHA256(
    logical_key + NUL + fingerprint + NUL +
    "era:" + expected_era + NUL + "2.1.251"
)[0..32]
```

默认 legacy 的旧 fixed vector 仍稳定：

```text
refresh-token -> grant:0eb17643d4e92611
grant:none -> 856f0d2375be22a510e79662f22d30c51c14dc3394b9d610af33a7116d81cda6
legacy logical-cache-key -> a6fad12e13235da65ecc9b068d2c62b6.json
```

`DiscoveryCacheEntry` 增加可选 `negotiated_era`。缺字段的旧 entry 按 legacy 解释；expected era 已经在 partition key 中，actual era 则随 entry/live connection 另存。

## 5. Cache gate 与 post-hit 顺序

这是当前可审计的顺序。不要把 gate-level live miss 与真正读盘后 miss 混写：

1. registry 先看当前 state；已有 `Connected` 或 `Cached` 直接复用，`live-connection` 不会再触发一次 cache consult。
2. `cache_gate_with_metadata` 先检查 feature：`MCP_DISCOVERY_CACHE` 未开启 → `FeatureDisabled`。
3. 再检查 transport：只有 HTTP/SSE → 继续，其余 → `Transport`。
4. 再按顺序检查 provenance：`cli_owned` → remote URL/header 中仍含 `${...}` → `ambient_credential`。
5. 再检查用户可翻转的 purge gate：`discoveryCache:false` → `OptOut`；`headersHelper` → `HeadersHelper`。
6. 只有 `OptOut`/`HeadersHelper` 会 best-effort purge server family；feature/transport/provenance gate 只阻止读写并继续 live，不删除旧文件。
7. gate 通过后计算 Agent source、MCP grant fingerprint 和 expected-era partition。Agent source 缺失、secure storage 出错或 token row 没有 refresh token → `NoFingerprint`。
8. 读取 entry 后检查 absent/corrupt、strike threshold、future-clock/max-stale、degenerate zero-tools。
9. post-hit capability gate 先检查 skills，再检查 channel：skills 命中 → `SkillsCapable`；否则 channel marker 命中 → `ChannelCapable`。
10. 最后才按 age 形成 `Fresh` 或 `Stale`；两者都先提供 cached catalog，随后按 surface 需要 lazy dial。

`MissReason::LiveConnection` 仍保留在 telemetry vocabulary，但 registry 的实际 live-state short circuit 在 `decide_with_metadata` 之前完成；它不是一个会被错误写进磁盘读取结果的伪 miss。

## 6. Fresh/stale/miss 与 post-hit 行为

### Fresh

`Fresh` 安装 `McpConnectionState::Cached`，记录 cache `negotiatedEra`、catalog 与 `entryAgeMs`，发 `cache_fresh`，不调用 transport。`tools/list`、resources/prompts catalog 可以先被工具注册层看到；首次实际操作再触发 foreground lazy upgrade。

### Stale

`Stale` 与 Fresh 一样先返回 catalog 并发 `cache_stale`，但创建 `LazyUpgradeSlot` 运行 detached background revalidation。foreground tool/resource/prompt waiter 会加入同一个 slot，而不是产生第二个 transport。

owner 只有在 cached connection id、config snapshot、slot identity 都仍匹配时才发布 live generation。失败或 panic 保留仍有效的 `Cached`；strike 只写入 slot 保存的“提供 stale hit 的 exact partition”。ordinary initial connect failure 不 strike。

### Miss

miss 会 live discovery。只有真正尝试读盘但不可用的 `Absent`、`Expired`、`Corrupt`、`Strike`、`NoFingerprint` 发 cache source telemetry；feature、transport、provenance 等 gate-level miss 走 live 语义，不伪造一次磁盘 health failure。

### post-hit capability gate

实现读取 entry 的真实 capability evidence：

- skills：独立 flag `tengu_mcp_skills` 开启、`capabilities.resources=true` 且 `extensions` 含 `io.modelcontextprotocol/skills`；
- channel：`capabilities.experimental["claude/channel"]` 必须是 JSON boolean `true`；
- skills 优先于 channel；任一命中都在 Fresh/Stale 前转 live miss。

这只保证 cache 不错误复用特定能力的 catalog；并不提供完整 skills discovery、channel 会话或 notifications 产品。

## 7. Expected/actual protocol-era 状态机

### 7.1 选择 expected era

`mcp/src/protocol_negotiation.rs::resolve_for_spec` 读取 `MCP_PROTOCOL_NEGOTIATION`：

- `legacy`：直接 Legacy；
- `auto`：对 eligible HTTP/stdio 尝试 Modern，其他 transport 仍 Legacy；
- 未设置或非法值：回落 transport feature gate；相关 flag 默认关闭；
- gated auto 可受 server denylist 降级为 Legacy；
- 默认配置因此仍使用 legacy，但代码不再假定“永远只有 legacy”。

registry 将这个 expected era 传入 `McpConnectOptions`，并用它选择 partition。expected 不是 server 的最终事实。

### 7.2 Probe 与 actual era

```text
resolve_for_spec
      │
      ├─ expected=legacy ─────── initialize(2025-11-25) ── actual=legacy
      │
      └─ expected=modern
           │
           ├─ server/discover 成功 ─ initialize(2026-07-28) ─ actual=modern
           └─ compatibility evidence ─ close probe + redial
                                      initialize(2025-11-25) ─ actual=legacy
```

每条 live connection 返回 `McpNegotiatedProtocol { era, version }`，`Connected`、`Cached` 和 `LiveDiscovery` 都携带它。modern probe 是 disposable connection；fallback 不复用 probe 的 socket/process。

### 7.3 Stale revalidation 的 era 判定

`LazyUpgradeSlot` 同时保存：

```text
refresh_partition.expected_era   = partition key 选择的期望期
refresh_entry_era                = stale entry 实际期
discovery.negotiated.era         = 本轮 live 实际协商期
```

判断为：

- config/generation 不匹配 → reject live discovery，做 cleanup，不碰 replacement partition；
- actual era 与 stale entry actual era 相同 → 原子 publish `Connected` 并按同一 grant/era 规则 write-through；
- actual era 改变 → 只 purge 提供 stale hit 的 partition，保留 `Cached`，不 strike、不 publish replacement、不把旧 catalog 写到新 era。

这保证了“期待 modern partition”与“本次 probe 实际 fallback legacy”两个事实彼此独立，并覆盖 grant rotation 之外的 era rotation 风险。

## 8. `server/discover` 错误分类、deadline 与 cleanup

### 8.1 错误分类

| probe 结果 | 分类 | 后续动作 |
|---|---|---|
| 返回 `protocolVersion:"2026-07-28"` | modern success | actual=Modern，进入 modern initialize |
| method not found、`-32001/-32020/-32021` | compatibility fallback | 关闭 probe，redial，actual=Legacy |
| `-32022` 且 `data.supported` 含 modern version | corrective retry | 只重试一次，成功则 actual=Modern |
| `-32022` 无 modern supported，或第二次仍失败 | compatibility fallback | 关闭 probe，redial Legacy |
| JSON-RPC remote data 是 HTTP 401/403 | auth/HTTP error | 形成结构化 `McpError::HttpResponse`，不当作 compatibility |
| remote timeout/network/writer error | transport failure | 返回错误，不偷偷 redial 成 Legacy |
| stdio wrong-shape/closed/timeout（仅 probe） | compatibility evidence | stdio 可回退 Legacy；remote timeout 不适用该宽松规则 |
| modern RPC 缺 `resultType` 或不是 `complete` | handshake/schema error | 失败，不把 partial result 当完整 catalog |

### 8.2 Deadline

- registry `MCP_TIMEOUT` 是 connect+initialize 总 deadline，默认 30,000 ms；正整数环境值可覆盖；
- `McpConnectOptions.deadline_ms` 与 `probe_timeout_ms` 把同一次 immutable negotiation decision 传给 platform transport；cache partition、handshake、write-through 与 stale refresh 共用这一快照；
- modern probe cap：stdio 3,000 ms，其他 auto-eligible transport 5,000 ms；shared remote 还会再次 clamp caller 值到 5,000 ms，且不突破总 deadline；
- remote HTTP/SSE 的具体 I/O 仍受 registry 外层 `timeout_at` 约束，避免 probe 或 initialize 无限等待。

### 8.3 Cleanup

连接或 initialize 被取消/超时后：

- `platforms/common::RemoteConnectionCleanupGuard` 关闭 remote JSON-RPC connection，并从 connection/negotiated map 移除；
- POSIX `ConnectionCleanupGuard` 关闭 socket、通知 stdio reaper kill child，并清 map；
- registry 对 cleanup disconnect 使用 5 秒 deadline；失败进入 pending cleanup；
- pending cleanup 最多 5 次，延迟从 10 ms 指数增加并在 250 ms 封顶；生命周期操作可再次 kick；
- CAS reject、panic、未发布 discovery 走 cleanup-only 路径，在释放 per-key lifecycle 后 disconnect/schedule，避免阻塞新 generation；
- 真实用户 disconnect/remove/disable 仍按生命周期语义等待 teardown，并分别处理 OAuth revoke。

## 9. Modern request envelope 与 catalog RPC

### 9.1 Probe envelope

`server/discover` 的 params 为：

```json
{
  "_meta": {
    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
    "io.modelcontextprotocol/clientInfo": {
      "name": "lingxi",
      "title": "LingXi",
      "version": "<crate version>",
      "description": "An agentic coding tool",
      "websiteUrl": "https://claude.com/claude-code"
    },
    "io.modelcontextprotocol/clientCapabilities": {
      "roots": {},
      "elicitation": {}
    }
  },
  "protocolVersion": "2026-07-28"
}
```

`_meta` 是闭合的三键 metadata envelope，不混入 provider credential、account identity 或 PKCE 数据。

### 9.2 Initialize 与 catalog envelope

initialize 使用对应 protocol version、roots/elicitation client capabilities 和 clientInfo；initialize 本身不附加 catalog `_meta`。Modern 下，`tools/list`、`tools/call`、`resources/list`、`resources/templates/list`、`resources/read`、`prompts/list`、`prompts/get` 等需要 metadata 的 RPC 会附加同一三键 `_meta`。

modern result 必须携带 `resultType:"complete"`；验证器移除该 transport marker 后，剩余 payload 才交给普通 DTO 解码。缺失/非 complete 结果失败，避免把 partial catalog 当成可持久化 cache。MCP 传输仍提供通用 notification stream，但完整 notifications/channel 产品不在本报告范围。

## 10. Comms routing

`role:"comms"` 是 coordinator routing role，不是 provider/account role。路径如下：

```text
role_flag("comms")
       │
       ▼
McpServerConfig.metadata.role = Some(Comms)
       │
       ├─ build_registered_mcp_tools / build_agent_mcp_tool_set
       │       ▼
       │  MCPTool.mcp_role() = Some("comms")
       │
       └─ AgentToolResolver(coordinator_mode=true)
               ├─ 移除 shared MCP comms tools
               ├─ 移除 inline agent MCP comms tools
               └─ 移除 generic MCP / McpAuth dispatcher
```

规则：

- coordinator worker 不拿 lead-only `comms` 工具；
- coordinator worker 也不注册可按任意 `full_name` 调用 server 的 generic `MCP` 与全局 `McpAuth`，因此不能绕过 per-tool 隐藏；resource helper tools 按既定产品语义保持不变；
- 普通 session/worker 保留 `comms`；
- `use_exact_tools` fork 仍保留父工具池，但 coordinator mode 仍做 lead-only comms 过滤；
- Connected 与 Cached 的 tool rebuild 都复制 role；
- `McpRegistry::has_comms_roled_server` 将 Cached 和 Connected 都计入，lazy dial 期间 routing 不漂移。

## 11. Shared remote/mobile composite 架构

### 11.1 Shared remote transport

`platforms/common/src/mcp_remote.rs` 是 desktop/mobile 共用的 HTTP/SSE 实现，负责：

- HTTP JSON-RPC 与 SSE connection map；
- modern probe、protocol negotiation、initialize；
- tools/resources/prompts list/call/read；
- modern request/result envelope；
- deadline-aware I/O；
- inbound notification stream；
- remote connection close 和 cancellation cleanup。

POSIX transport 继续负责 stdio process/reaper，并把 HTTP/SSE 委托 shared remote；不会再复制一份 mobile-only HTTP/SSE 协议实现。

### 11.2 Mobile composite

mobile composition 的实际拓扑为：

```text
MobileRuntime
  └─ McpRegistry
      ├─ DiscoveryCacheStore(<cfg.lingxi_home>/mcp-discovery-cache)
      ├─ optional OAuthDeps (仅 encrypted SecureStorage)
      └─ MobileMcpTransport
          ├─ LocalAppsMcpTransport  (InProcess)
          └─ RemoteMcpTransport     (Sse / Http)
```

`MobileMcpTransport` 保存 `McpConnectionId -> Route`：

- `InProcess` 只去 Local Apps；没有 raw JSON-RPC connection；
- `Sse`/`Http` 只去 shared remote，并通过 `RawConnectionProvider` 暴露 live connection 给 `McpClient`；
- `disconnect`、未知 id、route map failure 都显式返回/清理；
- WebSocket、stdio、IDE、SDK control 不在 mobile composite supported set 中。

`apps/engine-mobile/src/host.rs` 使用同一个 MCP parser 读取 app-private settings 和 project `.mcp.json`，先 bootstrap Local Apps，再为每个 startup config 记录初始 desired intent，并通过与 reload 相同的 guarded background job 连接 HTTP/SSE；交互式 OAuth 或网络等待不会阻塞 engine handle 返回，且 boot-time A 不会在 reload 已改为 B 后继续发布。disabled startup/reload config 只 guarded-seed `Disconnected`，不拨 transport。catalog change receiver 在 cache/live replacement 后重建 mobile ToolRegistry。带 store 的 registry 连接 Local Apps 不会创建 cache directory，因为 InProcess 仍然 transport-ineligible。

配置 reload 使用完整的 config snapshot 比较：未变化 settled server 不 dial、不 purge、不 revoke；修改/删除变成 per-server 后台 generation/CAS job。每个 server 另存最新磁盘 desired snapshot；intent 从 `A→B→A` 或 `A→deleted→A` 时，即使 registry 可见 state 仍是 A，也会递增 generation 使旧 B/remove job 失效。若这个同 config state 仍处于 `Connecting`/`AwaitingOAuth`，只有磁盘 intent 确实发生变化时才为 latest generation 重新安排 A owner，避免旧 owner 被拒绝后留下空状态；重复读取完全相同的 intent 会保留当前 transitional owner，不排队同代 teardown/redial。reconciliation 名称集合包含 registry、tracked intent 与本轮磁盘配置三者并集，因此 remove→connect 的短暂无 state 窗口仍可被删除/恢复操作取消。guard 在 registry lifecycle lock 内再次检查，并在 cache hit、live discovery 发布和失败状态写入前复查。过期 live discovery 会断开而不发布；过期失败不会写入旧 `Disconnected`；exact `Connecting`/`AwaitingOAuth` cleanup 与 conditional remove 均复用完整 client/catalog/lazy-slot/cache-retire 路径，同时不撤销 OAuth grant。因此只读 listing 不等待 teardown、network 或交互式 OAuth，也不会由 startup/stale reload generation 覆盖最新配置意图。

## 12. OAuth encrypted-storage、deep-link 与 cache 安全边界

### 12.1 Secure storage preflight

mobile 从 `Platform::secure_storage()` 取 secure store，并以 `SecureStorage::is_encrypted()` 形成 `oauth_supported`：

- 没有 native store 时使用 non-persisting development stub；
- `mobile_mcp_preflight` 在 `connect_all` 前扫描 remote OAuth config；
- 若 `oauth_supported=false`，写入 config error `MCP OAuth requires an encrypted secure credential store`；
- 因为错误发生在 connect 前，不会先做 OAuth discovery、remote dial 或 plaintext token persist；
- 注入 iOS Keychain/Android Keystore/EncryptedFile 等 encrypted store 后，OAuthDeps 才接入 registry，允许 load/refresh/PKCE persist。

该边界只约束 MCP OAuth；mobile provider credential manager 是另一条 host/provider 路径，不被 MCP cache identity 复用。

### 12.2 Deep-link callback

OAuth authorization URL callback 的顺序是：

1. 先写 host-owned copyable URL slot；
2. 有 `Platform::deep_link()` 时 best-effort `open(url)`；
3. opener 不存在或失败，记录 warning，并让 host 读取 slot 展示复制 fallback；
4. slot 只保存 authorization URL，不保存 access token、refresh token 或 PKCE verifier。

copyable slot 通过 UniFFI 可达的 `mcp_oauth_authorization_url` 暴露；URL 在尝试 deep-link 前写入，所以 opener 缺失或失败时宿主仍能轮询展示。真实 OAuth connect 始终在后台执行，不会把构建过程卡在 loopback callback 的等待期。

### 12.3 Cache security

- root `0700`，entry `0600`，entry 上限 8 MiB；
- no-follow + regular-file 检查；symlink/非普通/oversize/schema/key mismatch → corrupt miss；
- exclusive staging + atomic rename，staging 名只使用 partition digest + nonce；
- `discovery_cache_secret_candidates_for` 读取 config URL/header/env 与 MCP stored token/client secret；secure storage 出错即拒绝 write-through；
- reflection 检查覆盖 composite Cookie/header 值（空白、`,`、`;`、`=` 分隔）和 URI percent-encoded token（大小写 hex）；
- cache 文件只含 catalog、capability、actual era 和短 hash identity，不含任何 token 原文。

## 13. 文件职责

| 文件 | 责任 |
|---|---|
| `mcp/src/connection.rs` | `McpServerMetadata`、Agent source、role、Cached/Connected state 与 negotiated metadata |
| `mcp/src/discovery_cache.rs` | cache policy、gate、logical/partition key、fingerprint、post-hit、store hardening |
| `mcp/src/protocol_negotiation.rs` | `MCP_PROTOCOL_NEGOTIATION`、feature/denylist 到 expected era 的纯决策 |
| `mcp/src/registry.rs` | cache consult/read/write、immutable grant provenance、lazy owner、exact strike、era compare、guarded lifecycle CAS、cleanup |
| `mcp/src/json_config.rs` | MCP transport schema、`discoveryCache`、`role`、raw transport metadata |
| `mcp/src/oauth.rs` | MCP OAuth 2.1/PKCE、secure storage、refresh-grant fingerprint 输入 |
| `mcp/src/client.rs` | live MCP client、catalog/tool/resource/prompt dispatch、negotiated protocol metadata |
| `platform-api/src/mcp.rs` | transport spec/kind、era/options/result、capabilities/extensions DTO |
| `platforms/common/src/mcp_remote.rs` | shared HTTP/SSE wire、server/discover、envelope、deadline、remote cleanup |
| `platforms/posix/src/mcp.rs` | stdio child/reaper、POSIX raw bridge、POSIX probe/cleanup delegation |
| `agent/src/mcp_servers.rs` | inline Agent source → MCP metadata、scoped config conversion |
| `agent/src/tool_resolver.rs` | coordinator-mode `comms` filtering |
| `tool-api/src/tool_trait.rs` | `Tool::mcp_role()` contract |
| `tools/mcp/src/mcp_tool.rs` | MCP per-tool construction、role propagation、Cached/Connected resources/prompts surfaces |
| `apps/cli/src/init.rs` | `--mcp-config` → `cli_owned` provenance |
| `apps/engine-desktop/src/lib.rs` | desktop registry/OAuth/cache composition、coordinator mode、catalog refresh |
| `apps/engine-mobile/src/host.rs` | mobile registry composition、OAuth preflight、deep-link callback、catalog refresh |
| `apps/engine-mobile/src/mcp_transport.rs` | Local Apps/remote route composite 与 raw connection bridge |
| `platforms/common/tests/mcp_remote_e2e_test.rs` | shared HTTP/SSE end-to-end round trips |
| `platforms/posix/tests/mcp_protocol_negotiation_e2e_test.rs` | modern success/fallback/corrective retry/deadline/auth/result envelope |
| `plugin/src/manager.rs` | plugin MCP unload/remove lifecycle integration and test fixtures |

## 14. 测试证据

### 14.1 本次直接复核

以下命令在主实现加全 targets 集成收口后的分支上直接运行；输出包含既有 warnings，但非测试失败：

```text
cargo test -p jsonrpc -p mcp -p agent --lib --quiet
jsonrpc 63、mcp 592、agent 358 全部通过

cargo test -p tool-mcp --quiet
148 passed；0 failed

cargo test -p plugin --quiet
148 + 9 + 3 + 15 + 3 passed；0 failed

cargo test -p tasks --lib --quiet
291 passed；1 个 latest-main Local App grammar baseline failure，见 14.2

cargo test -p platform-common -p platform-posix --quiet
全部 unit/integration suites 通过；modern negotiation E2E 7/7

cargo test -p engine-desktop --quiet
181/235 lib passed；54 个 latest-main Local App workflow baseline failure，见 14.2

cargo check --workspace --all-targets
passed

cargo check -p ios-framework -p android-aar --quiet
passed

cargo clippy -p jsonrpc -p mcp -p tool-mcp -p agent -p plugin \
  -p platform-common -p platform-posix -p engine-desktop -p engine-mobile \
  --all-targets --no-deps
passed（保留 workspace 既有 warnings）

cargo test -p engine-mobile --features uniffi --lib mobile_mcp_ --quiet
13 passed；0 failed
```

覆盖重点：

- fixed logical/fingerprint/partition vectors、OAuth optional-field canonicalization、grant rotation、无 refresh grant fail-closed 与 immutable grant write-through；
- provenance gate 顺序及 non-purging；
- fresh/stale/miss、single-flight foreground/background owner、panic/CAS cleanup；
- exact stale strike 与 replacement generation 隔离；
- era change 只 purge hit partition，不 strike replacement；
- role 在 Connected/Cached tool rebuild 中保留，coordinator worker 正确过滤；
- generic `MCP`/`McpAuth` dispatcher 不可绕过 coordinator comms filter；plugin role 在 load→connect/cache→refresh 全链路保留；
- modern server/discover success、legacy fallback、`-32022` corrective retry、remote timeout/network/auth 分类、partial result rejection；
- wrong response id compatibility fallback、caller probe cap、单次 negotiation snapshot 与 stale expected-mode drift purge；
- shared HTTP/SSE tools/resources/prompts round trip；
- mobile composite route、disconnect/unsupported、remember failure cleanup、Local Apps cache-ineligible；
- mobile encrypted storage injection、plaintext OAuth preflight、nonblocking OAuth boot、deep-link/copy fallback、generation/CAS reload 与跨 registry cache hit；
- guarded reload 在 lifecycle wait 后失效、live publish 前失效、失败返回前失效，以及 newer config 不被 stale job 删除；
- blocked startup `A→B`、真实 pending owner 的 `A→B→A` / `A→deleted→A`、重复 identical reload 一次 dial/零 teardown、exact `AwaitingOAuth` cleanup 与 disabled-no-dial。

### 14.2 latest-main 基线红灯与归因

最终合并前，本分支曾在 `d053be447` 基线上得到 mobile **474/475**，唯一失败为历史 `phaser_2d` digest fixture。随后本地 `main` 前进到 `bef351cd9` 并合入完整 Local App plugin history；该基线改变了测试集合和失败面，所以最终报告以新基线实测为准，不继续把旧的单 fixture 结果冒充为当前状态。

最终组合树上的完整结果是：

```text
engine-mobile --features uniffi: 472 passed / 65 failed / 537 total
engine-desktop:                 181 passed / 54 failed / 235 lib tests
tasks:                          291 passed / 1 failed / 292 lib tests
```

mobile 的同一命令在未合入本分支的 `main@bef351cd9` 上为 **449 passed / 65 failed / 514 total**，失败名称集合相同；本分支新增的 23 项测试全部通过。代表性 baseline 原因包括 Local App template `templateOrigin/dependencySnapshot` contract、builtin bundle `/var` 与 `/private/var` canonical path、device/runtime fixtures，以及 host-injected workflow value 的随机 capability 比较。这些失败不在 MCP remote/cache/OAuth/comms/protocol delta 中。

desktop 的代表性 `build_wires_one_plugin_workflow_registry_into_every_participant` 在 `main@bef351cd9` 上独立复现同一失败；其余多数 build tests由未注册 `Workflow` tool 与随后 poisoned lock 连锁失败。tasks 唯一失败位于本分支未修改的 `scope.rs`，是 64 字符 app id 在 tasks/local-apps 两套 grammar 间的基线分歧。

为保证本次组合树至少可完整编译，集成收口只做了机械 test/API seam 修复：适配 `MCPTool::new_for_tool` 与 Agent scoped-config 新签名、恢复 main 已存在 `brand_normalize` 模块导出、把 plugin workflow 测试改为真实 script path，并删除一份完全重复的 TUI 同名测试。修复后 `cargo check --workspace --all-targets` 通过；MCP/plugin/mobile-MCP 定向回归全部通过。

本报告不会把 latest-main 的 Local App baseline 红灯隐去，也不会把本次 MCP/plugin 完成声明扩张成“全 workspace 测试全绿”。

### 14.3 Sol xhigh 审查闭环

第一轮最终审查给出 3 项 P1、2 项 P2，均已修复并由独立回归覆盖：

| finding | 修复 | 回归证据 |
|---|---|---|
| generic dispatcher 绕过 `comms` | coordinator worker 移除 `MCP`/`McpAuth`，normal 与 `use_exact_tools` 都覆盖 | agent production spawner/resolver tests |
| mobile listing reload revoke/purge | snapshot compare；未变 read-only；修改/删除不 revoke；重连后台化 | unchanged reload、cache rebuild tests |
| OAuth URL fallback 不可达且阻塞 build | connect 后台化；URL getter 导出；先写 slot 后 open | nonblocking build、deep-link failure/copy fallback tests |
| probe budget 丢失 | `probe_timeout_ms` 从 negotiation decision 传到 transport，并在 remote 再 clamp | cap unit + wrong-id/redial E2E |
| expected era 在 attempt 内漂移 | immutable `NegotiationMode` 贯穿 consult/handshake/write/stale | expected-mode mismatch purge/no-write test |

第二轮复审在合入最新 `main` 后给出 2 项 P1，也已关闭：

| finding | 修复 | 回归证据 |
|---|---|---|
| OAuth catalog 可能用旧 bearer、却写入重读到的新 grant partition | resolve/refresh/step-up 返回与成功 connect spec 同源的 immutable grant provenance；write-through 重读只用于 equality check；无 refresh grant fail closed | shared/agent-scoped same-grant + rotation、access-only OAuth no-partition/no-write |
| mobile reload 等待持有 lifecycle lock 的 OAuth/connect | listing 只入队 generation/CAS job；remove/connect 在 lifecycle lock 内校验 guard；publish/error 前再校验，stale live connection 自动清理 | guarded remove/connect 4 项确定性交错测试；mobile changed/deleted pending-OAuth 4 项 reload tests |

主集成复核还主动关闭了 direct transport cleanup 遗留 client/catalog 的风险、generation check-before-await TOCTOU，以及 stale network failure 写入旧状态的问题。

第三轮 cross-check 发现 latest-intent 回滚仍可能保留旧 generation：可见 state 仍为 A 时，`A→B→A` 的第二次 reload 会被 equality fast-path 短路，旧 B job 之后仍可发布。修复后 generation state 同时保存独立 desired snapshot，并跟踪 pending name；`A→B→A` 与 `A→deleted→A` 的 lifecycle-lock 交错测试均验证旧 job 不再删除或替换最新 A。

第四轮使用真实 blocked guarded connect 继续检查后关闭两条遗漏：一是 matching A 仍处于 `Connecting`/`AwaitingOAuth` 时必须为最新 generation 重新安排 owner，并在旧 guard reject 时清理 exact pending state；二是 startup config 也必须先 seed 同一个 intent map、再走 guarded job，不能保留独立的 unguarded `connect_all`。blocked startup `A→B`、`A→B→A`、`A→deleted→A`、changed-to-disabled 与 registry disabled-no-dial 回归均通过。

第五轮发现相同配置的重复 listing 会因为仅判断 transitional state 而排队同 generation replacement：首个 startup/OAuth owner 成功后，后续排队 job 可能依次拆掉刚建立的连接再重拨。修复后 production fast-path 同时使用 `intent_changed`：配置相同时仅允许“新 intent + transitional owner”触发 replacement；重复相同 intent 与 settled Connected 都保持 read-only。确定性 blocked-startup 回归验证重复 reload 期间只有一次 dial、零 disconnect，并同时锁住配置变化仍会 replacement。

第六轮在折入 `main@bef351cd9` 后发现两项 test-target P2：`MCPTool::new_for_tool` 的测试调用未适配 9 参数，以及 plugin role integration/oracle tests 未适配 scoped-config 新签名与 `brand_normalize` module export。两项均仅做必要兼容修复；tool-mcp 148 tests、plugin 全 suites、role parse→scope→connect/cache→refresh 指定测试和 workspace all-targets check 均通过。

最终 Sol xhigh 对 clean 4-commit 分支复核了 tool-mcp、plugin role、mobile lifecycle/cache、provider-neutral credential boundary、brand-gate composition、工作树状态和 diff check，结论为：**APPROVE — zero unresolved P0–P3**。

## 15. Provider-neutral grep 说明

### 15.1 Scope-sensitive 命令

对 MCP/cache 执行的 grep 是：

```bash
rg -n -i \
  'account[_-]?uuid|provider.?credential|anthropic_api_key|openai_api_key|profile.?id' \
  mcp/src/{connection,discovery_cache,oauth,registry}.rs
```

当前命中只有两个“负面边界”文档注释（`connection.rs` 说明不持久化 provider credential/profile/account identifier；`discovery_cache.rs` 说明不得把 provider credential/profile/account UUID 替换进 compatibility domain）。没有命中可执行的 provider credential/account UUID read/write 路径。

### 15.2 为什么不能 grep 全仓后宣称零命中

`apps/engine-desktop/src/lib.rs` 与 `apps/engine-mobile/src/host.rs` 必然包含 LLM provider auth/provider credential/profile 代码：它们负责模型 provider 的 API key/OAuth、picker 和 host settings。这些命中不属于 MCP cache identity，也不证明 MCP cache 读取了它们。正确边界是：

```text
LLM provider auth/profile/credential
             │  （不传入 MCP cache partition）
             └──────────────╳

MCP OAuth server grant → refresh-token hash → MCP fingerprint → partition
```

MCP scope 中保留 `MCP_CLIENT_WEBSITE_URL`、XAA/IdP 类型和 oracle compatibility literal 也不等于 provider auth coupling；它们分别属于 wire clientInfo、MCP OAuth 扩展或 binary parity。审计结论是“无 provider credential/account UUID 依赖的可执行 MCP cache path”，不是“全仓没有 provider 字样”。

## 16. 明确非目标与剩余风险

### 明确非目标

- 不实现完整 notifications 产品；当前实现 transport-level notification stream，legacy list-changed 继续按现有路径处理。
- 不实现完整 Claude channel 产品；`claude/channel` 仅作为 post-hit cache safety marker。
- 不实现完整 Claude/marketplace/plugin 产品；本文只覆盖 plugin-owned MCP lifecycle/cache purge 与已验证 routing seam。
- 不实现 mobile stdio/WebSocket/IDE/SDK control；mobile supported set 是 InProcess + HTTP/SSE。
- 不读取、保存、推导或依赖任何 LLM provider auth、credential、profile 或 account UUID。

### 剩余风险

- mobile native Keychain/Keystore 由具体宿主注入；未注入时 OAuth remote 会在 dial 前明确拒绝，而不是明文降级；真实设备 secure-storage implementation 仍需平台级验收。
- `MCP_DISCOVERY_CACHE` 与 auto negotiation 默认关闭；启用后应持续运行 fixed vectors、grant rotation、era fallback、stale strike、lifecycle purge 和 envelope tests。
- `main@bef351cd9` 的 Local App mobile/desktop/tasks baseline failures 需由对应 owner 单独修复，不能由 MCP/plugin 交付隐式吸收。
- 本报告覆盖已实现/已测试的 MCP surface，不对所有第三方 MCP server、所有 Claude Code feature flag 组合或所有产品 channel 行为做 100% parity 承诺。

## 17. 交付判定

在上述边界内，六项历史缺口已完成代码、测试和 production wiring 对齐。MCP/cache 交付可按当前支持集审计与集成；mobile `phaser_2d` fixture failure、既有 warnings 和未实现的完整产品面均已显式列为非本交付的风险/非目标。
