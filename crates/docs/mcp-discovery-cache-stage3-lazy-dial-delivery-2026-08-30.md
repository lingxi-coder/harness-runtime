# MCP discovery cache Stage 3 与 resources/prompts 惰性拨号交付报告

> 2026-08-31 更新：本文保留 Stage 3 历史交付记录；其 production identity/wiring defer 已由 `mcp-discovery-cache-production-alignment-2.1.251-2026-08-31.md` 关闭。第 9、10 节应结合新报告阅读。

| 项目 | 结果 |
| --- | --- |
| 日期 | 2026-08-30 |
| 实现分支 | `codex/mcp-dcache-stage3` |
| 基线 | `main@f2f9fea67` |
| 实现 owner | Luna xhigh |
| 代码复审 | Sol xhigh 第五轮 focused `CODE VERDICT: SHIP` |
| 架构复审 | Architect 最终 `APPROVE` |
| 整理状态 | mandatory deslop 已完成 |
| 最终复审 | Sol xhigh `OVERALL VERDICT: SHIP` |
| 状态（截至 2026-08-31） | 实现、验证、代码审查、架构审查、deslop 与最终全 diff / 文档复审均已完成 |
**提交状态：** 工作树内未提交，等待集成方决定提交/合并方式

## 1. 结论

Stage 3 discovery cache、cached resources/prompts lazy dial、plugin refresh single-flight、resource shared/scoped 边界、directory capability gate、panic / cleanup / auth-retry 收敛逻辑均已稳定。四包联合测试、focused desktop tests、env reproductions、下游 `cargo check`、scoped fmt 和 `git diff --check` 均已通过。

Sol 已完成最终全 diff 与交付文档复审，结论为 `OVERALL VERDICT: SHIP`，无剩余 P0–P3 finding。

本报告交付时 production store 尚未接线；该历史缺口已在 2026-08-31 的 production alignment follow-up 中关闭。

## 2. 审查—修复闭环

| 轮次 | 发现 / 结论 | 类别 | 当前状态 |
| --- | --- | --- | --- |
| 1 | `3 x P1 + 4 x P2 + 1 x P3` | cached prompt bridge、lazy dial joinability / cancellation、resource shared/scoped、directory gate、temp collision、env guard、文档真值 | 已闭合 |
| 2 | `1 x P1 + 1 x P2` | `set_disabled` no-op 不应触碰 slot；known-id panic cleanup 必须收敛 | 已闭合 |
| 3 | tracing `config_diagnostics` flake | `mcp` 20 轮压力 + 联合测试中的 tracing capture 稳定性 | 已闭合 |
| 4 | Architect blocker | `connections` / `clients` publish TOCTOU race | 已闭合，最终 `APPROVE` |
| 5 | Sol P1 | auth retry early-return 导致 cached lazy / publish-race 401 路径不一致 | 已闭合 |
| 6 | 最终 focused verdict | Sol xhigh `CODE VERDICT: SHIP`；Architect 最终 `APPROVE` | 已完成 |
| 7 | 最终交付复审 | deslop 后全 diff 与文档 truth pass | Sol xhigh `OVERALL VERDICT: SHIP`，无 finding |

首轮 `3 x P1 + 4 x P2 + 1 x P3` 可按问题类别概括为：

- cached prompt `C -> L1` bridge 与后续代际拒绝不完整
- stale background 与 foreground lazy dial 没有稳定共享 owner
- cancellation / panic / cleanup 可能留下 zombie `Connecting`
- resources shared/scoped、directory gate、temp staging、env guard 与文档真值均有缺口

## 3. 当前真实实现

### 3.1 Foreground / background 已共享同一个 exact-key owner

stale background revalidation 与 foreground cached lazy dial 现在不再走两条竞争拨号路径，而是共享同一个 exact-key lazy-upgrade slot 与 detached transport owner：

- stale background 命中后先安装 synthetic `Cached`，再创建 detached owner
- foreground 若随后访问同一 cached server，会 join 这个 owner
- foreground 若先触发 cached lazy dial，则会先把状态推进到 `Connecting`
- background 与其他 waiter 随后继续 join 这一个 owner

同一 exact key 在同一时刻只允许一个真实 transport owner，因此不会出现前后台重复拨号、重复 publish live generation。

### 3.2 失败语义按 owner origin 区分

- foreground owner failure：收敛到终态错误，避免调用方留下 zombie `Connecting`
- background owner failure：若当前 cached generation 仍有效，则保留 `Cached`，只记录 refresh strike
- owner panic 也走同样分流：foreground panic 进入终态错误恢复，background panic 保留 cached state 并记录 refresh failure
- `set_disabled` 的 no-op toggle 不再触碰 slot，foreground 与 background 两个方向都已覆盖回归

### 3.3 known-id panic cleanup、invalidate 与 cleanup lock discipline

当前已闭合以下收敛点：

- known-id `initialize` 之后、publish 之前的 panic
- known-id post-connect panic
- `disconnect` / `remove` 与 upgrade 并发时的 invalidation
- CAS reject 后的 cleanup

这些路径的共同约束是：

- waiter 必须收到终态结果
- slot 必须被清理
- lazy owner CAS-reject、known-id panic cleanup、discarded / unpublished transport 等 cleanup-only retirement，会在释放 per-key lifecycle 后做 disconnect 或 schedule cleanup，避免阻塞新 generation
- 用户生命周期操作 `disconnect`、agent-scoped disconnect、actual disable teardown 则有意持 lifecycle 锁等待 transport teardown，以串行化同 key 状态；这是不同契约

### 3.4 Publish 原子性、`ensure_*` / `call_*` 一致性与 prompt bridge

Architect blocker 的根因是 `connections` / `clients` publish TOCTOU。当前相关发布点已经收敛到同一临界区：

- `Connected(L1)` 状态
- live `McpClient`
- prompt predecessor `C -> L1`

因此：

- `ensure_connected_client`
- tool call
- `get_prompt`

看到的 generation、client 与 predecessor 不再彼此错位。后续真实调用还会做 live re-read，因此 `ensure_*` 与 `call_*` 使用同一条 authoritative live view。

cached prompt generation bridge 当前只允许 `C -> L1`，并满足：

- predecessor `C -> L1` 与 `Connected(L1)`、live client 同临界区原子发布
- 第二次调用同一个 cached prompt 仍然继续命中 `L1`
- `L2`、`remove`、`set_disabled`、`disconnect`、`C2` 全部拒绝旧 `C`

旧 cached prompt command 因此只能桥接到它自己的第一条 live generation，不能越代重路由。

### 3.5 Resource resolution、collision 与错误语义

resources surface 现在遵循以下规则：

- exact raw key 优先
- raw key 未命中时，normalized fallback 仅允许唯一候选
- normalized collision 直接 ambiguous fail closed，且零 RPC
- shared 与 agent-scoped state 继续隔离

这条规则同时覆盖：

- `ListMcpResourcesTool`
- `ReadMcpResourceTool`
- `ReadMcpResourceDirTool`

named `ListMcpResourcesTool` 的真实错误语义需要分开描述：

- cached lazy dial connection failure：hard error
- 已拿到 live client 后，`resources/list` RPC 自身失败：沿用 per-client isolation/catch，返回该 server 空结果并发失败事件

因此，不能把 named resources list 的任何失败都写成 hard error。

### 3.6 Auth retry、directory capability 与 `config_diagnostics`

当前三条 `McpClient` acquisition 入口已统一到 ordinary tool dispatch 的认证管线，而不是各自携带独立 early-return：

- initial live `get_client`
- cached lazy tool path
- publish-race live re-read

约束是：

- 一次 structured `401` / `403` reconnect/retry 只属于 ordinary tool dispatch `call_tool_with_auth_retry`
- 上述三条 client acquisition 都会统一进入这条 pipeline
- cached lazy `401` 与 publish-race `401` 已由回归闭合

resources / prompts 与普通 tool dispatch 共享的是 authenticated lazy connection acquisition，不是“自动 auth retry 到 RPC 完成”为止。它们在 post-RPC 阶段仍保持各自语义：

- named `ListMcpResourcesTool`：live `resources/list` RPC 失败时隔离为空结果并发失败事件
- `ReadMcpResourceTool`：read 失败仍是 hard error
- `get_prompt`：直接 map error

`directoryRead` 的真实来源也已校正为：

```text
capabilities.extensions["io.modelcontextprotocol/skills"]["directoryRead"]
```

判定保持 fail closed：

- extension key 必须是 `io.modelcontextprotocol/skills`
- `directoryRead` 必须显式为 `true`
- 缺失、旧 cache、`false`、truthy 非布尔值都不放行

本轮 `mcp/src/config_diagnostics.rs` 的修复是 test-only tracing capture stability：

- 目标是压力测试与联合测试下的 tracing capture 稳定性
- 不改变 production config diagnostics 决策语义
- telemetry fix 的落点仅限 test harness / test capture 层

## 4. Mandatory deslop 报告

mandatory deslop 已完成，并严格遵守“先锁行为、再整理”的约束：

- 只作用于本轮已修改文件
- 行为锁定先行，先补回归，再做整理
- 不新引依赖，不借整理名义扩大架构范围

主要结果：

- 删除 live recheck exact-raw helper 中重复的 normalized 扫描
- 删除 desktop/mobile 的死绑定逻辑
- 在保持行为不变的前提下净删 `83` 行

整理完成后又重新执行：

- `tool-mcp` 与下游 `cargo check`
- 四包联合测试
- focused desktop tests
- env reproductions
- scoped fmt
- `git diff --check`

因此，这里的 deslop 完成，指的是“整理后全套重新回归仍通过”。

## 5. 文件级变更清单

| 文件 | 职责 |
| --- | --- |
| `mcp/src/registry.rs` | Stage 3 状态机、detached lazy-upgrade owner、foreground/background join、ordinary tool dispatch auth-retry pipeline、panic cleanup、publish 原子性 |
| `mcp/src/discovery_cache.rs` | stale 判定、cache schema、env guard、collision-free store、并发 store 测试 |
| `mcp/src/client.rs` | live capability 与 client acquisition 行为对齐 |
| `mcp/src/config_diagnostics.rs` | test-only tracing capture stability |
| `platform-api/src/mcp.rs` | `ServerCapabilitiesDto::directory_read` |
| `platforms/posix/src/mcp.rs` | `io.modelcontextprotocol/skills.directoryRead` 的准确解码 |
| `platforms/posix-minimal/src/mcp.rs` | DTO 默认能力补齐 |
| `platforms/windows/src/mcp.rs` | DTO 默认能力补齐 |
| `test-harness/src/mocks/mock_mcp.rs` | mock DTO 默认能力补齐与相关测试支撑 |
| `tools/mcp/src/mcp_tool.rs` | resource resolution、named/all-server 语义、lazy dial、ambiguous zero-RPC、authenticated lazy acquisition |
| `tools/mcp/src/read_mcp_resource_dir.rs` | directory lazy dial、live fail-closed gate、normalized collision fail-closed |
| `tool-api/src/registry.rs` | MCP tool partition 原子替换 |
| `apps/engine-desktop/src/lib.rs` | plugin refresh single-flight、listener rebuild、focused concurrency regression |
| `apps/engine-mobile/src/host.rs` | mobile listener 完整重建与替换 |
| `apps/engine-mobile/src/local_apps_mcp.rs` | capability DTO 适配 |
| `plugin/src/lifecycle.rs` | 八状态生命周期图与注释 |
| `plugin/src/manager.rs` | partial unload failure 保留、重试与 ownership 收敛 |
| `mcp/tests/oauth_flow_test.rs` | integration fixture 补齐 `directory_read`，恢复 integration target 真值 |

没有引入新依赖。

## 6. 回归覆盖

本轮新增或强化的回归重点包括：

- `set_disabled` foreground no-op 不触碰 slot
- `set_disabled` background no-op 不触碰 slot
- known-id initialize panic cleanup
- known-id post-connect panic cleanup
- foreground/background shared owner
- foreground owner panic recovery
- background owner panic 保留 Cached 并记录 refresh strike
- zombie `Connecting` 不可残留
- strike 仅记录在仍匹配的 cached generation 上
- prompt `C -> L1` 原子发布
- `L2`、`remove`、`disable`、`disconnect`、`C2` 拒绝旧 `C`
- publish TOCTOU 下 `ensure_*` 与 tool call 一致
- publish-race 401
- cached lazy 401
- normalization collision 时 exact raw key 优先
- ambiguous normalized alias fail closed 且零 RPC
- shared/scoped resource resolution 继续隔离
- `directory_read` 缺失字段向后兼容
- cleanup-only retirement 在释放 lifecycle 锁后再做 disconnect / schedule cleanup
- config diagnostics tracing / interest-cache stress

config diagnostics 稳定性验证还包含：

- `mcp` 20 轮压力测试
- 联合测试执行
- tracing capture 不再产生假阳性波动

## 7. 验证证据

以下范围已经完成最终回归：

- 四包联合测试：`mcp`、`tool-mcp`、`tool-api`、`plugin`
- `plugin` 各 target 明细
- 两条 desktop focused tests
- `cargo check -p engine-desktop -p engine-mobile -p plugin`
- 两条 host-env reproductions
- scoped fmt
- `git diff --check`

以下命令已经验证通过：

```bash
cargo test -p mcp -p tool-mcp -p tool-api -p plugin --no-fail-fast
cargo test -p plugin --no-fail-fast
cargo test -p engine-desktop concurrent_plugin_runtime_refresh_is_single_flight_and_leaves_one_owner -- --nocapture
cargo test -p engine-desktop plugin_runtime_refresh_aborts_enable_phase_after_disable_failure_then_recovers -- --nocapture
cargo check -p engine-desktop -p engine-mobile -p plugin
MCP_DISCOVERY_CACHE=true cargo test -p mcp --lib feature_enabled_matrix -- --nocapture
MCP_DISCOVERY_CACHE_TTL_S=1 cargo test -p mcp --lib ttl_and_max_stale_defaults -- --nocapture
```

| Package | Target | Passed | Failed | 说明 |
| --- | --- | ---: | ---: | --- |
| `mcp` | `lib` | 550 | 0 | 最终稳定结果 |
| `tool-mcp` | `lib` | 139 | 0 | 四包联合测试 |
| `tool-api` | `lib` | 184 | 0 | 四包联合测试 |
| `plugin` | `lib` | 137 | 0 | `cargo test -p plugin --no-fail-fast` target summary |
| `plugin` | `tests/discovery_bootstrap.rs` | 9 | 0 | 同上 |
| `plugin` | `tests/enabled_discovery.rs` | 3 | 0 | 同上 |
| `plugin` | `tests/materialize.rs` | 11 | 0 | 同上 |
| `plugin` | `doc-tests` | 0 | 0 | 无 doc tests 失败 |
| `engine-desktop` | `concurrent_plugin_runtime_refresh_is_single_flight_and_leaves_one_owner` | 1 | 0 | focused test |
| `engine-desktop` | `plugin_runtime_refresh_aborts_enable_phase_after_disable_failure_then_recovers` | 1 | 0 | focused test |
| `mcp` | `feature_enabled_matrix` | 1 | 0 | host env override reproduction |
| `mcp` | `ttl_and_max_stale_defaults` | 1 | 0 | host env override reproduction |

本表按 target 列示，不把 focused tests 或 env reproductions 重复并入 package 汇总，也不伪造跨命令合并总数。

以下检查已经通过：

- `cargo check -p engine-desktop -p engine-mobile -p plugin`，exit `0`
- scoped `cargo fmt` 已覆盖本轮变更涉及的 11 个 crate，exit `0`
- `git diff --check`，exit `0`

需要明确说明的是：

- 仓库仍有既有 warnings
- 本报告不声称“零 warning”
- repo-wide `cargo fmt --all -- --check` **仍不为绿色**
- 失败原因仍是未改动文件 `client-protocol/src/message.rs` 的基线多余空行
- 本次交付没有越界修改该文件

因此，本报告只声明已验证的编译、scoped fmt 与 diff 检查通过，不声明全仓格式或 warnings 全绿。

## 8. 与 oracle 的对齐表述

本轮文档刻意避免把实现描述成“byte-level 完全相同”。更准确的表述是：

- 已验证的 oracle literals、schema 与 behavior alignment 均已对齐到当前实现范围
- miss reason、capability gate、tool naming、部分 error copy 与 schema 兼容性按可验证范围对齐
- 本文当时明确 defer 的 production store、grant fingerprint 与失效边界，已由 2026-08-31 follow-up 实施并单独验证

## 9. Production store wiring 的后续边界

本文交付时 production discovery cache store 尚未接线；2026-08-31 follow-up 已完成：

- provider-neutral 固定兼容域 + MCP refresh grant fingerprint
- grant rotation partition isolation 与 exact stale strike
- plugin unload/remove/disconnect family purge
- desktop composition-root store injection
- `<lingxi_home>/mcp-discovery-cache` production root

Mobile 当前只有 cache-ineligible 的 `InProcess` MCP，因此保持 store unwired；未来引入 remote mobile MCP 时再评估平台持久化。

## 10. 交接清单

- [x] 代码修复完成
- [x] 回归测试完成
- [x] focused code review `SHIP`
- [x] architect `APPROVE`
- [x] mandatory deslop 完成
- [x] 最终全 diff / 文档复审：Sol xhigh `OVERALL VERDICT: SHIP`

本文的 production defer 已由 2026-08-31 production alignment follow-up 关闭；剩余长期边界以新报告第 12 节为准。
