# Harness Runtime

从 LingXi 提取的多平台 Rust 运行时。仓库包含 Agent 执行、模型宿主层、会话、权限、工具、插件、平台适配和桌面/移动装配。保留 LingXi 默认命名空间与现有数据格式。

## 使用

主入口是 `harness-runtime`；现有 `api`、`models`、`desktop`、`mobile` 模块保持可用。默认启用 `core`；桌面使用 `desktop`，移动使用 `mobile`，原生绑定额外启用 `uniffi`。宿主通过 Rust 配置注入产品构建信息，`runtime_build_info()` 独立返回运行时身份。

下游通过本仓库 Git URL 和完整 commit SHA 引用所需 package。所有共享协议、平台 trait 与运行时组件必须使用同一 URL 和提交，避免同名异类型。`llm-client` 保持独立 Git 依赖。

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

[English](README.en.md)
