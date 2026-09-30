# Harness Runtime

从 LingXi 提取的多平台 Rust 运行时，负责 Agent 执行、模型宿主服务、会话与持久化、权限、工具、插件，以及桌面和移动端装配。本仓库提供可嵌入的库；产品 UI、CLI/TUI 入口、原生宿主包装和签名仍由 LingXi 负责。

主 Cargo package 是 `harness-runtime`，源码位于 [`crates/runtime`](crates/runtime)，Rust 导入名为 `harness_runtime`。现有 LingXi 命名空间默认值和持久化数据格式继续保留。

[English](README.en.md)

## 快速开始

仓库固定使用 Rust 1.94。首次检出时获取 `llm-client` 子模块：

```sh
git clone --recurse-submodules https://github.com/lingxi-coder/harness-runtime.git
cd harness-runtime
cargo check --locked -p harness-runtime
```

已有 checkout 或切换提交后，执行 `git submodule update --init --recursive`。从仓库根运行 Cargo 命令；根 workspace 的 `[patch]` 会将固定的 `lingxi-llm-client` Git 依赖指向 `deps/llm-client` 本地源码。

## 在宿主项目中接入

在宿主的 `Cargo.toml` 中固定**完整的 40 位提交 SHA**。例如桌面宿主：

```toml
[dependencies]
harness-runtime = { git = "https://github.com/lingxi-coder/harness-runtime.git", rev = "<40 位提交 SHA>", default-features = false, features = ["desktop"] }
```

将占位符替换为已发布的实际提交。移动 Rust 宿主使用 `features = ["mobile"]`；需要跨语言绑定时使用 `features = ["uniffi"]`，它已包含 `mobile`。如果宿主还直接依赖本仓库的其他 package，所有共享 package 必须使用**相同的 Git URL 和提交**，否则 Cargo 可能把同名类型解析为不同身份。此仓库根目录的 `[patch]` 不会传播到下游 workspace。

| Feature | 作用 |
| --- | --- |
| `core`（默认） | Agent、会话、编排、权限等共享执行组件；公开 `Harness`、`HarnessBuilder` 和 `SessionHandle`。 |
| `desktop` | 桌面装配，包含 `core`；入口在 `harness_runtime::desktop`，包括 `build` 和 `build_harness`。 |
| `mobile` | iOS/Android 共用的 Rust 装配，包含 `core`；入口在 `harness_runtime::mobile`，包括 `build_mobile`。 |
| `uniffi` | 移动端原生绑定，自动启用 `mobile`。 |
| `android-computer-use` | Android 设备交互能力，自动启用 `mobile`。 |
| `realtime-websocket` | 启用模型运行时的 WebSocket 实时能力。 |

`HarnessBuilder` 接受宿主已经装配好的 `SessionService` 和 `LifecycleService`，不会自行启动进程或网络请求。桌面宿主可使用 `desktop::build_harness` 注入输出流和权限门控；移动宿主应通过 `mobile::build_mobile` 注入对应平台能力。宿主负责停止接收新任务、等待进行中的回合结束，再调用 `Harness::shutdown()`；若返回的 `ShutdownReport.complete` 为 `false`，需处理错误并串行重试。产品构建信息由宿主注入，`desktop::runtime_build_info()` / `mobile::runtime_build_info()` 分别可读取运行时自身身份。

宿主装配好 `Harness` 后，可通过统一会话接口运行回合：

```rust
use harness_runtime::{CancellationToken, HandleError, Harness, RunInput, TurnOutcome};

async fn run_turn(harness: &Harness, prompt: String) -> Result<TurnOutcome, HandleError> {
    harness.session().run(
        RunInput { prompt, images: Vec::new() },
        CancellationToken::new(),
    ).await
}
```

启用 `desktop` feature 时可用 `harness_runtime::build_harness` 装配生产桌面运行时；
移动宿主通过 `harness_runtime::mobile::build_mobile_engine` 接入对应平台能力。

模型请求通过 `harness_runtime::models::llm` 接入独立的 `llm-client`。托管 Web Search/Web Fetch、远程 Skills、音频、文件及实时会话的类型、职责边界和示例见 [llm-client 能力接入](docs/llm-client-upgrade.md)。

## 仓库结构

| 路径 | 内容 |
| --- | --- |
| [`crates/runtime`](crates/runtime) | 对外组合入口及桌面、移动端装配。 |
| [`crates/core`](crates/core) | 项目内部共享类型、宿主契约与基础状态规则。 |
| [`crates/agent`](crates/agent)、[`crates/orchestrator`](crates/orchestrator)、[`crates/session`](crates/session) | 各领域拥有自己的执行逻辑和状态。 |
| [`crates/client`](crates/client) | 客户端协议、展示与运行时适配。 |
| [`crates/tools`](crates/tools)、[`crates/platforms`](crates/platforms) | 工具实现与平台能力。 |
| [`deps/llm-client`](deps/llm-client)、[`third_party`](third_party) | 独立 SDK 子模块及带各自补丁和许可证的第三方源码。 |
| [`scripts`](scripts)、[`docs`](docs) | 构建/边界检查及设计、迁移文档。 |

宿主通过 `harness-runtime` 的公开 API 接入；仓库内部组件依赖 `core` 的共享类型和契约。依赖方向由 `scripts/check-deps.sh` 检查；模型提供方通信由 `llm-client` 负责，宿主保留凭证、会话、权限、工具执行和持久化职责。移动 Linux 资源供应链及宿主接口见 [运行时来源契约](docs/mobile-linux/RUNTIME-SOURCE-CONTRACT.md)。

## 开发与验证

```sh
cargo check --locked -p harness-runtime --no-default-features
cargo check --locked -p harness-runtime --features desktop
cargo check --locked -p harness-runtime --no-default-features --features mobile,uniffi
cargo test --locked --workspace --all-features --no-fail-fast
./scripts/check-all.sh
```

这些命令分别检查最小、桌面、移动绑定组合、workspace 测试和仓库门禁；原生设备、完整产品打包与签名需在宿主项目中另行验证。迁移时已完成的验证及已知限制见 [验证记录](docs/migration/validation.md)。

修改 `deps/llm-client` 时，根 workspace 构建会立即使用子模块改动；SDK 自身的测试需要单独运行：

```sh
cargo test --manifest-path deps/llm-client/Cargo.toml --locked
```

发布联合修改时，先在 SDK 仓库验证、提交并推送，再更新本仓库的子模块提交、根 `[workspace.dependencies]` 固定 revision 和 `Cargo.lock`。具体流程见 [子模块联合开发](docs/llm-client-upgrade.md#子模块联合开发)。迁移来源可查阅 [来源清单](docs/migration/source-manifest.json)。
