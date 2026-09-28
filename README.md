# Harness Runtime

从 LingXi 提取的多平台 Rust 运行时。仓库包含 Agent 执行、模型宿主层、会话、权限、工具、插件、平台适配和桌面/移动装配。保留 LingXi 默认命名空间与现有数据格式。

## 使用

主入口是 `harness-runtime`（源码位于 `crates/runtime`）；现有 `api`、`models`、`desktop`、`mobile` 模块保持可用。默认启用 `core`；桌面使用 `desktop`，移动使用 `mobile`，原生绑定额外启用 `uniffi`。宿主通过 Rust 配置注入产品构建信息，`runtime_build_info()` 独立返回运行时身份。

下游通过本仓库 Git URL 和完整 commit SHA 引用所需 package。所有共享协议、平台 trait 与运行时组件必须使用同一 URL 和提交，避免同名异类型。`llm-client` 保持独立仓库和固定 Git 依赖；本仓库开发时通过 `deps/llm-client` 子模块与根 workspace 的 Cargo patch 使用本地源码。

模型层已接入 llm-client 0.3 的托管 Web Search/Web Fetch、远程 Skills、音频、文件与实时会话能力。宿主接口、流式上传和升级迁移见 [llm-client 能力接入](docs/llm-client-upgrade.md)。

## 联合开发

首次获取仓库时初始化子模块：

```sh
git clone --recurse-submodules https://github.com/lingxi-coder/harness-runtime.git
cd harness-runtime
```

已有 checkout 执行 `git submodule update --init --recursive`。在 `deps/llm-client`
直接修改 SDK 后，根目录的 Cargo 构建和测试立即使用这些改动；SDK 自身的测试单独运行：

```sh
cargo test --manifest-path deps/llm-client/Cargo.toml --locked
```

修改 SDK 后，先在子模块仓库提交并推送，再更新本仓库记录的子模块提交及固定依赖版本。
canonical Git URL 与完整 revision 统一声明在根 `[workspace.dependencies]`。
根 workspace 的 Cargo patch 不会传播给下游项目。具体流程和 SDK 修改发布顺序见
[llm-client 联合开发](docs/llm-client-upgrade.md#子模块联合开发)。

## 验证

使用仓库固定的 Rust 1.94：

```sh
cargo check --locked -p harness-runtime --no-default-features
cargo check --locked -p harness-runtime --features desktop
cargo check --locked -p harness-runtime --no-default-features --features mobile,uniffi
cargo test --locked --workspace --all-features --no-fail-fast
./scripts/check-all.sh
```

`crates/` 保存源码与内部资源，仓库根是 Cargo workspace。第三方源码保留本地补丁和各自许可证。脚本不依赖 LingXi checkout；下游只读消费固定提交中的资源，并指定自己的输出/缓存目录。

客户端 UI、CLI/TUI 产品入口、原生宿主包装和应用签名仍在 LingXi。移动运行时供应链工具与宿主验证的接口见 `docs/mobile-linux/RUNTIME-SOURCE-CONTRACT.md`。迁移来源清单见 `docs/migration/source-manifest.json`。

迁移验证结果与已知限制见 [验证记录](docs/migration/validation.md)。

[English](README.en.md)
