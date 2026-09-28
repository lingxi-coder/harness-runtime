# llm-client 能力接入

开发子模块与运行时固定 Git 依赖当前使用 `lingxi-llm-client` 0.3.0，提交
`34c6be3e409000ffb31d6827797fe5b97a7cbfdb`，已从 canonical 远端
`https://github.com/lingxi-coder/llm-client` 获取。该版本包含 provider 命名空间重组。
该提交包含会话隔离、文件操作期限及流式上传修复，并已推送。后续子模块修改仍须先独立提交并推送，再更新父仓库记录。

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
本地尚未提交的 SDK 修复不属于上述远端提交；共享前需完成上述发布步骤。

## 会话请求

`harness_runtime::models::llm` 仍是模型宿主入口。`LlmRequest` 新增
`hosted_tools`、`prompt_cache`、`output_format` 和 `continuation`，直接使用
`llm::services::sdk::protocol` 中的类型。原有 `response_format`、system 缓存标记
及 Responses 控制保留；新旧输出契约或缓存 TTL 冲突时返回错误。

```rust
use harness_runtime::models::llm::{LlmRequest, services::sdk::protocol::{HostedTool, WebSearchConfig}};

let mut request = LlmRequest::new("your-configured-model");
request.hosted_tools.push(HostedTool::WebSearch(WebSearchConfig::default()));
// 添加消息，再交给已有 ApiService 请求入口；支持范围由实际 profile/model 校验。
```

Web Fetch、远程 Skills 的模型执行使用相同的 `hosted_tools` 入口，分别携带
上游 `providers::anthropic::types::AnthropicWebFetchConfig` 和
`providers::anthropic::types::AnthropicCodeExecutionConfig`。远程 Skills
由提供方容器执行；它们不会安装进本地技能目录，也不会作为本地工具调用执行。
搜索结果、引用、文件检索结果及容器信息保留在响应的 `provider_metadata.llm_client` 中；
原生提供方内容保留协议标签用于同协议续传。

托管工具及远程状态请求采用单次尝试，避免在结果不确定时自动重试或切换连接。
已有普通 Chat 请求保留原来的重试和结算机制。`account_scope` 与
`file_account_scope` 是调用时提供的非秘密账户标识，不从序列化的会话请求中恢复。
复用资源时必须传入与创建资源相同的账户作用域。

## 独立服务

`llm::ProviderServices` 持有可复用的上游客户端和宿主传输：

```rust,no_run
use std::sync::Arc;
use harness_runtime::models::llm::{ProviderServices, Transport, services::sdk};

fn services(transport: Arc<dyn Transport>) -> Result<ProviderServices, Box<dyn std::error::Error>> {
    let profiles = sdk::builtin_providers()?;
    Ok(ProviderServices::with_host_transport(
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

流式文件、音频和 Skills 上传沿 `Transport::send_stream_raw` → 平台
`HttpTransport::send_stream` → SDK `HttpTransport` 传递，保持一次性流及原始二进制响应。
宿主注入 mTLS 和自定义 CA 配置；上传、请求头校验和网络错误脱敏直接复用 SDK。
自定义传输未实现流式上传时明确报不支持，不隐式收集整个输入。

Responses WebSocket 继续用于聊天续传。双向音频和 Live 会话使用独立的
`services::realtime` API，可注入上游 `RealtimeTransport`。启用
`harness-runtime/realtime-websocket` 后，可用同模块的 `RustlsWebSocketTransport`；
它使用独立的连接配置，不继承宿主 HTTP 代理、监控和认证策略。

上游传输要求可被异步任务长期持有，因此 `DefaultLlmClient` 的执行、流式执行、
预热、文件上传和精确 token 计数入口接收 `Arc<dyn Transport>`。
迁移调用方时将传输保存在 `Arc` 中并传入 `transport.clone()`。

## 验证边界

回归覆盖宿主协议投影、账户隔离、流式上传、原生内容重放和防重复发送。
本次不使用真实提供方凭据；编译和模拟传输测试不等于真实账户或设备验收。

在 macOS / Rust 1.94 使用以下命令验证：

```sh
CARGO_INCREMENTAL=0 cargo check --offline --locked -p harness-runtime --all-features
CARGO_INCREMENTAL=0 cargo test --offline --locked -p llm-runtime -p http-client -p platform-api --all-features --no-fail-fast
cargo metadata --offline --locked --all-features --format-version 1 | python3 scripts/check_deps.py
git diff --check
```

HTTP/TLS/OAuth 测试需要允许绑定本机端口。为避免生成大量增量缓存，本次设置
`CARGO_INCREMENTAL=0`。升级后的运行结果以本轮验证日志为准。
子模块开发使用本地源码，本地追加修复仍须遵守
上述发布顺序，才能在干净的团队或 CI 环境重现。
