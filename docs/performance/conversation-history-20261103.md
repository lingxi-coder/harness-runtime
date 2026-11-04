# 会话历史缓存与 provider 续接接入

## 边界

- Host 保留完整、可重放的历史。UI/Remote 的显示缓存、transcript 与 provider 存储分别管理；本次没有增加任何端间历史回传，也没有改动公开消息协议。已有 Remote attach 使用 epoch/after_seq 补增量，失配时重新取快照。
- `ApiService` 为主会话保留 `ConversationSession`。准备最终正文后、签名前计算增量，仍经既有准入、物理尝试预算、请求观察、用量和 settlement 路径。
- 主会话 session 以 lease 取出，无锁跨越异步准入/网络/流读取。并行请求不能借到同一会话时使用独立状态；lease 只在对应流释放后归还。
- side chat 继续使用 `fork_independent()` 创建独立 ApiService，独立维护多轮缓存及续接。
- btw、compact 摘要、分类器和其他 sidequery 保持无状态单次调用契约。辅助构造边界设置 `ExecutionContext.isolated_query`，即使复制了主会话 query-source 或采用 Main wire policy，也不会获取父会话状态。最多缓存 4 个空闲传输；归还/取出时清除所有会话历史，只复用经过身份校验的连接。
- 成功进入 compact 重写的提交阶段时，推进主会话缓存 generation，清空历史投影与续接。旧流即使随后完成或归还 lease，也不能写回新 generation。其他历史编辑由完整前缀比较保守重建。

## 缓存与复制

SDK JsonPayload 的不可变原始 JSON 使用 Arc 共享，clone 不再复制正文；修改生成新 JSON，保持精确整数、孤立 UTF-16 和原始 JSON 的现有语义。完整请求字节没有变化时复用原 Bytes，而非额外复制整个正文。

Host 对最后一次规范化消息列表的 provider replay 投影做缓存：最多 1024 行、8 MiB 序列化 source+projection 估算，截断后释放多余行。复用比较完整消息而非仅 ID/长度；修改后清除对应后缀，协议变化时清空。source sidecar、UTF-16 override 坐标、system、tools 和跨消息 toolset 关系仍按本次请求重算。依赖全局 Computer receipt/binding 的消息不缓存，继续走原有投影与权限边界。

这不是端到端零拷贝：原始 transcript 规范化、字符串/Vec 复制、完整请求编码和安全前缀检查仍有与历史大小相关的开销。本次缓存减少重复 replay 转换和原始 JSON 深拷贝，没有引入仅按长度信任历史的捷径。

## 恢复与存储

明确的 continuation/thread 拒绝最多恢复一次，限定在同一逻辑调用的预算内；先结算被拒绝的物理请求，再重新准备、签名和准入完整历史。HTTP 和首个 SSE/WS 事件拒绝共用 SDK 识别，探测字节恢复给原观察路径。首事件探测位于同一个 first-byte watchdog 中，不重置该阶段预算。未知结果、已生成后的断流不增加自动重放，也不重新执行工具。

能力取自精确 provider/endpoint 的 SDK 配置；兼容协议不再被 Host 一概声明支持 previous_response_id。单账户使用同一 captured credential 建立作用域，多账户继续使用选择时捕获的账户 generation。请求相关 ID/trace headers 不改变认证身份。

尊重显式 store=false 及原 Host 默认。OpenAI/Qwen 的持久 HTTP 续接、GLM Responses 必须由调用方明确允许存储；连接内 store=false 续接仍可使用 Responses WS。不会通过接入自动开启远端存储。

## 本机探针（debug 构建，非产品延迟承诺）

| 探针 | 之前/未缓存 | 之后/命中 |
| --- | --- | --- |
| 约 4 MiB JsonPayload clone 64 次 | 24,334 us；复制 268,438,592 源字节 | 1 us；复制 0 源字节 |
| 128 条消息、2 MiB 工具结果，投影 8 次 | 8,103,432 us | 1,135,519 us；1024 次命中；保留估算 4,221,220 字节 |

复制探针：在 llm-client 运行 `cargo run --offline --example history_copy_cost`。

历史投影探针：运行 `cargo test -p llm-runtime --lib history_projection_cost_probe -- --ignored --nocapture --test-threads=1`。

诊断仅记录消息数量、累计命中/失配、保留字节估算、投影耗时，以及续接模式和 logical/wire 字节数；不记录正文、账户凭证或远端引用。

真实账户的首字节延迟、网络流量、峰值内存、多端断线重连和移动设备表现仍需产品验收，不能由离线探针推断。

## 验证记录

- SDK 固定提交 `25d898973df7e4bbad9889d6805097bcb425b285`：9 组共 766 项通过，包含共享正文、连接池去历史、无变化请求复用、stateless 无额外握手签名，以及原 Responses/Anthropic/Gemini 回归。既有 5 项代理认证测试需要 localhost bind，提升权限后通过。
- 无路径覆盖、固定 SDK 提交的 Host 集成回归：`conversation_continuation_test` 11、`transport_stream_test` 19、`connection_failover_test` 30，全部通过。
- llm-runtime 完整单元回归串行运行：1451 通过，2 失败，1 默认忽略的性能探针已另外运行通过。两项失败为 `fusion_hints::tests::{table_ids_exist_in_catalog_or_anthropic_profiles,judge_eligible_rows_mostly_have_a_structured_output_capable_model}`，涉及现有 openai-chatgpt 的 gpt-5.6-sol/terra 目录断言。用未修改 Harness `e2051f1a` 和旧 SDK `a30a3a9` 独立基线复现相同两项失败，本次不改动模型目录或放宽断言。并行完整测试中另一次 global-fallback 失败，单独及完整串行重跑均通过。
- sidequery 完整单元回归：76 通过，覆盖实际 Session backend、side_question/btw 分隔、prompt-cache、Computer 边界、准入与批次计费。
- orchestrator 已编译的隔离工作树测试二进制：过滤 `compact` 得到 95 项通过，过滤 `persistent_chat` 得到 29 项通过；初次使用错误模块过滤 `conversation::compaction::` 得到 0 项，已改为实际名称过滤并执行上述测试。
- 固定 SDK 提交的 orchestrator `cargo check --offline --locked -p orchestrator` 通过。产品 bridge-server 使用隔离工作树依赖组合 `cargo check -p bridge-server --lib` 通过；提交前继续核对产品固定 Git 依赖组合。
- 修改 Rust 文件 rustfmt 检查和 `git diff --check` 通过。仓库已有 vendor/文档警告保留，不将局部检查描述为全仓零警告。

构建前设置 `CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`；测试源码、探针与失败断言均可在隔离分支复核。本地尚未推送的 SDK/Host 提交已加入本机 Cargo Git 缓存，以验证固定版本依赖，而不是把绝对路径覆盖作为交付依赖。
