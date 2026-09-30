# llm-client 能力接入

开发子模块与运行时固定 Git 依赖当前使用 `lingxi-llm-client` 0.3.0，提交
`a4a9880fa02737d95665b3215f6743dfc128f4a4`。该版本统一模型 HTTP/WebSocket 传输、鉴权策略、模型目录、
托管搜索及 Anthropic/OpenAI/Copilot OAuth 协议。本次提交尚未推送；发布时须先推送
SDK，再推送父仓库，以便 canonical 远端 `https://github.com/lingxi-coder/llm-client`
能够解析固定依赖。

## 子模块联合开发

`deps/llm-client` 是独立 Git 子模块。根 `Cargo.toml` 将它排除在 runtime workspace 外，
并通过 `[patch."https://github.com/lingxi-coder/llm-client"]` 指向本地源码。
子模块使用相对父仓库的 URL；本地 SSH 别名和 CI 的 HTTPS checkout 各自沿用父仓库身份。
根 `[workspace.dependencies]` 保留 canonical Git URL 与完整提交，供 `llm-runtime`、
`http-client` 及下游消费；
Cargo 不会继承依赖仓库根的 patch，下游若也需要联调，应在自己的 workspace 根配置 patch。

```sh
# 新 checkout
git clone --recurse-submodules git@github.com-lingxi-coder:lingxi-coder/harness-runtime.git
cd harness-runtime

# 已有 checkout，或切换了父仓库提交后
git submodule update --init --recursive

# SDK 回归与 runtime 集成分别验证
cargo test --manifest-path deps/llm-client/Cargo.toml --locked
cargo test --locked -p llm-runtime --all-features
```

直接修改 `deps/llm-client` 会立即进入根 workspace 的构建，无需复制源码或导入 Cargo 缓存。
子模块默认检出父仓库记录的提交；开发前可在子模块中创建自己的分支。
协议、请求编码、HTTP/WebSocket 传输和提供方服务的缺陷应在 SDK 中修复，
runtime 负责宿主凭证、权限、工具执行和费用结算，并保留相应集成回归。
本轮直接在子模块联调；其他目录中的 llm-client checkout 没有被修改，也不会自动同步未提交改动。

共享修改时按顺序完成：

1. 在 `deps/llm-client` 验证、提交并推送 SDK 修改，确保目标提交可从 canonical 远端获取。
2. 更新 runtime 的固定 Git revision 与子模块检出的提交，刷新根 `Cargo.lock` 并验证集成。
3. 在父仓库提交 gitlink、固定依赖和必要的 runtime 修改；父仓库提交不会包含子模块内未提交的文件。

CI 的 checkout 使用 `submodules: recursive`，因此使用父仓库锁定的 SDK 提交。
新增 SDK 修复必须先发布，父仓库只记录已可获取的提交。

## 会话请求

`harness_runtime::models::llm` 仍是模型宿主入口。`LlmRequest.input` 直接使用
SDK `ChatRequest`，包括 `hosted_tools`、`prompt_cache`、`output_format` 和
`continuation`。宿主执行信息放在不参与序列化的 `execution` 中；旧 Rust 字段
接口已移除，历史与移动端展示格式由边界适配器保留。

```rust
use harness_runtime::models::llm::{LlmRequest, services::sdk::protocol::{HostedTool, WebSearchConfig}};

let mut request = LlmRequest::new("your-configured-model");
request.input.hosted_tools.push(HostedTool::WebSearch(WebSearchConfig::default()));
// 添加消息，再交给已有 ApiService 请求入口；支持范围由实际 profile/model 校验。
```

Web Fetch、远程 Skills 的模型执行使用相同的 `hosted_tools` 入口，分别携带
上游 `providers::anthropic::types::AnthropicWebFetchConfig` 和
`providers::anthropic::types::AnthropicCodeExecutionConfig`。远程 Skills
由提供方容器执行；它们不会安装进本地技能目录，也不会作为本地工具调用执行。
SDK 响应保留结构化结果与原生内容；持久化历史适配器将搜索结果、引用、文件检索结果及容器信息投影到 `provider_metadata.llm_client` 中；
原生提供方内容保留协议标签用于同协议续传。

托管工具及远程状态请求采用单次尝试，避免在结果不确定时自动重试或切换连接。
已有普通 Chat 请求保留原来的重试和结算机制。`account_scope` 与
`file_account_scope` 是调用时提供的非秘密账户标识，不从序列化的会话请求中恢复。
复用资源时必须传入与创建资源相同的账户作用域。

## 独立服务

`llm::ProviderServices` 持有可复用的上游客户端和 SDK 传输：

```rust,no_run
use std::sync::Arc;
use harness_runtime::models::llm::{ProviderServices, Transport, services::sdk};

fn services(transport: Arc<dyn Transport>) -> Result<ProviderServices, Box<dyn std::error::Error>> {
    let profiles = sdk::builtin_providers()?;
    Ok(ProviderServices::with_transport(
        &profiles, sdk::protocol::Region::International, transport,
    )?)
}
```

0.3 将提供方专用 API 收敛到 `sdk::providers`。通过配置中的 profile 名称绑定
提供方客户端，再直接调用 SDK 的资源方法：

```rust,no_run
use harness_runtime::models::llm::{ProviderServices, services::{providers, sdk}};

async fn list_files(
    services: &ProviderServices,
    profile_name: &str,
    options: &sdk::RequestOptions,
) -> Result<sdk::files::ProviderFilePage, Box<dyn std::error::Error>> {
    let snapshot = services.snapshot();
    let anthropic = snapshot.provider::<providers::AnthropicClient>(profile_name)?;
    Ok(anthropic.files().list(None, options).await?)
}
```

- OpenAI：`services.client().provider::<providers::OpenAiClient>(profile)` 后使用
  `audio()`、`files()`、`images()`、`embeddings()`、`batches()` 等资源。
- Anthropic：绑定 `providers::AnthropicClient` 后使用 `files()` 或 `skills(scope)`；
  scope 类型是 `providers::anthropic::types::AnthropicSkillScope`。
- Google 等其他提供方：使用 `providers` 中对应客户端的专用资源方法；支持范围由 SDK 校验。
- `services.snapshot()`：直接返回 SDK 配置快照，再绑定提供方，适合需要保持同一配置版本的相关操作。
- `services::sdk`：完整的同版本 SDK 重导出，下游无需再引入另一份可能不一致的依赖。

旧的 runtime `services.audio()`、`services.anthropic_skills()` 和
`snapshot.files()` / `files_with_authenticator()` 包装已移除。调用方迁移到上述 SDK
provider 入口；`services` 仅重导出 `providers`、`files` 和 `realtime` 模块。

凭据、稳定的非秘密 `account_scope` / `file_account_scope` 和 `total_timeout`
放在每次操作的 `sdk::RequestOptions`，不嵌入历史或全局客户端。
自定义认证在 `ProviderServices::with_configured_transport` 注册到共享 SDK builder，
文件操作复用该认证注册。独立服务不经过会话的权限、Fusion 预算和持久化费用账本，
宿主须在调用处完成授权、取消和记账；音频录制、播放与文件落盘也由宿主负责。

## 传输与实时连接

流式文件、音频和 Skills 上传直接沿 SDK `Transport::send_stream_raw` →
SDK `HttpTransport` 传递，保持一次性流及原始二进制响应。宿主上传桥接接口已删除。
宿主注入 mTLS 和自定义 CA 配置；上传、请求头校验和网络错误脱敏直接复用 SDK。
自定义传输未实现流式上传时明确报不支持，不隐式收集整个输入。

Responses WebSocket 和 Realtime 使用同一个 SDK `HttpTransport` 的网络配置。
HTTP、文件上传、Responses 和 Realtime 共用 CA、mTLS 与代理配置；
runtime 将 SDK 的 HTTP/Responses 空闲读取期限设为 `None`，由宿主的首响应与
流 watchdog 控制等待，避免 SDK 默认 60 秒抢先截断长推理。SDK 独立使用时仍默认
60 秒；`with_read_timeout_and_client_configurator` 可同时配置 HTTP/Responses 期限
和 TLS/代理。Pong 写入仍有独立期限，并监听连接取消。

`responses-websocket` 启用 Responses，`realtime-websocket` 另外启用双向接口。
`HttpTransport` 同时实现 `Transport` 和 `realtime::RealtimeTransport`，旧
`RustlsWebSocketTransport` 已删除。Realtime 仍使用独立会话和双向流，设备录放音由宿主负责。

## 验证边界

回归覆盖宿主协议投影、账户隔离、流式上传、原生内容重放和防重复发送。
本次不使用真实提供方凭据；编译和模拟传输测试不等于真实账户或设备验收。

在 macOS / Rust 1.94 使用以下命令验证：

```sh
CARGO_INCREMENTAL=0 cargo check --offline --locked -p harness-runtime --all-features
CARGO_INCREMENTAL=0 cargo test --offline --locked -p llm-runtime -p http-client -p core --all-features --no-fail-fast
cargo metadata --offline --locked --all-features --format-version 1 | python3 scripts/check_deps.py
git diff --check
```

HTTP/TLS/OAuth 测试需要允许绑定本机端口。为避免生成大量增量缓存，本次设置
`CARGO_INCREMENTAL=0`。升级后的运行结果以本轮验证日志为准。
子模块开发使用本地源码，本地追加修复仍须遵守
上述发布顺序，才能在干净的团队或 CI 环境重现。

## 统一调用边界

生产模型请求直接注入 `Arc<dyn sdk::Transport>`。已删除 `LlmTransportBridge`、
宿主 Responses WebSocket 连接及 `AnthropicRequestBuilder`，调用方不得恢复这些实现。
`ModelRuntime` 按有效路由和网络配置复用 SDK client；请求鉴权通过
`RequestOptions.authenticator` 注入，在途 draft 保留自己的账号与凭据提供者。
缓存 scope 和显式 continuation 由 SDK 类型校验并编码，不在宿主补写协议 JSON。

| 调用 | 生产路径 |
| --- | --- |
| 主对话、侧查询、压缩、精确 token counting | ApiService / ModelRuntime → SDK prepared call → SDK Transport |
| Hosted WebSearch | HostedWebSearchClient → 会话 ApiService → SDK hosted tool；工具只展示结果和管理预算 |
| Files、Audio、Images、Skills、Embeddings、Batches | ProviderServices → SDK provider resource → 同一 SDK Transport |
| 移动连接测试与模型目录 | SDK directory::probe，包含鉴权、分页与总期限；宿主仅投影 UI DTO |
| Responses / Realtime | SDK 共用 HTTP upgrade connector；取消或异常不自动重放 |

模型 provider 的 OAuth 协议、token exchange/refresh、设备授权与账户查询也使用 SDK
认证接口和共享 Transport；宿主保留登录交互、凭据存储与刷新调度。
普通网页下载、Brave/Tavily/SearXNG/DDG、MCP（包括 MCP OAuth）、遥测及 Monitor
继续使用宿主通用网络栈。

`scripts/check-llm-boundary.sh` 自动进入 `check-all.sh` 和 CI：检查生产模型端点拼装及
旧适配器回流，同时禁止 provider 原始流事件解析和未声明的 WebSocket 实现。
明确例外仅包括 MCP、Monitor、宿主 CLI 事件输出及测试夹具。
网络 mock、回环 TLS/WS 测试和交叉编译不能替代真实 provider 或设备验证。


## 本轮交付验证（2026-09-28）

- SDK HTTP/SSE、上传期限、Responses/Realtime、鉴权、prepared calls、缓存与 failover：296 项通过；新增 connector 流聚合单测通过。
- runtime、WebSearch、侧查询、HTTP/TLS 和工具接口集成：1750 项通过；后续清理分别重跑网络/工具 441 项、provider adapter 20 项、移动 DTO 3 项、平台装配 2 项、单轮 e2e 2 项及 Anthropic codec 43 项，全部通过。部分测试有重叠，不相加为总数。
- runtime 全特性编译、runtime 与 llm-runtime 最小特性检查通过。
- SDK 启用 Realtime 的 iOS arm64、Android arm64、Windows GNU x64 交叉编译通过；未执行跨平台设备测试。
- 移除本地 SDK patch 的独立源码副本从 canonical Git 获取固定提交，runtime 全特性编译通过。
- 架构检查自测和全仓扫描通过。未使用真实 provider 凭据，未验证真实账户、录放音或移动设备行为。


审查后补充回归：HTTP/Responses 在虚拟时间超过 5 分钟后仍可读取，显式 SDK
短期限仍生效；阻塞 Pong 写入可取消或超时；OpenAI hosted search 的原生调用
产生一次进度与计数，引用及最终输出重复不重复计数。Responses 在 SDK 首次
提供完整搜索调用时发出该次进度，不伪造 provider 尚未给出的搜索次数。

审查修复的 SDK 相关测试 201 项、HTTP/TLS/上传回归 38 项、Anthropic/OpenAI
搜索集成 2 项通过；移除本地
patch 的独立源码副本使用远端固定 SDK 提交完成 runtime 全特性编译。


框架审查修复：精确 token counting 的准备、鉴权、发送和响应读取统一受宿主
120 秒总期限约束；流式请求的错误响应体也受宿主 watchdog 期限约束。
已经发送但没有完整 usage 的请求保留 Unknown 状态及输出预算占用，不能以
“未收到响应”推断模型没有执行。文件服务优先采用请求级 authenticator，且不把
覆盖设置保留到后续调用；Responses 会话缓存 HTTP fallback 后仍遵守
`allow_http=false`，重复预热不得绑定正式 HTTP 生成请求。


普通非流式模型调用也有宿主期限：每次尝试默认 600 秒，可用正数
`API_TIMEOUT_MS` 调整；准备与鉴权消耗同一预算，宿主准入等待不计入。
超时仍经过未知执行结果结算。Responses 连接身份排除逐请求 `x-request-id`，
继续保留账号、认证和连接配置校验。自动续传必须匹配上轮完整输入及最终输出；
无法确认完整前缀时保留完整历史。搜索流中断时，独立收到的有效引用也可作为
部分结果返回，并明确标记不完整。


Responses 会话还保留所选 Transport 的所有权：更换 Transport 时关闭旧连接并清除
续传及降级状态，共用同一个 Transport 的不同 client 可继续复用。
`ResponsesSession::prepare_using` 的显式传输参数改为 `Option<Arc<dyn Transport>>`；
准备之后再以其他 Transport 派发会在宿主准入之前被拒绝。

## Runtime contract consolidation

See [Model execution and host policy](llm-runtime-boundary.md) for the canonical SDK request/event boundary, the breaking Rust API migration, durable-history adapters, and execution invariants.
