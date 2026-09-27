# OpenMinis Android 真机能力报告

测试日期：2026-07-28（America/Los_Angeles）

## 范围与授权边界

本报告只验证独立参考 App `com.openminis.app`，不把 OpenMinis、PRoot、Alpine
rootfs 或其他 GPL 代码/二进制复制、链接到 LingXi 产品。

- OpenMinis commit：`9cf3a855fecd27bb5735b84cacbd56852a3ab8dd`
- PRoot 子仓库 commit：`8cf13e997cdc9472997aae19df8050c073c9a86c`
- OpenMinis App：`0.20-preview`，build 20
- LingXi 正式 `mobile-linux` 后端仍保持“授权阻塞/未链接”，没有用 Stub 冒充可用后端。
- 参考构建产生的 PRoot、rootfs、测试 loader 均位于 OpenMinis 子模块的
  gitignored 目录；参考仓库测试结束后 tracked worktree 为 clean。

## 设备

| 项目 | 值 |
| --- | --- |
| 设备 | Xiaomi `24117RK2CC` |
| Android | 16 / API 36 |
| ABI | `arm64-v8a` |
| ADB serial | `6bf447bb` |
| OpenMinis 包名 | `com.openminis.app` |

## 构建复现与发现的问题

使用上游脚本构建：

```text
./deps/build_proot.sh clean
./scripts/prepare_android_sandbox.sh
./gradlew :app:assembleDebug -x stageDebugSkillAssets
```

在当前 macOS 主机上需要注意：

1. 上游 PRoot 脚本直接依赖 GNU 风格 `readelf` 和 `awk`。macOS 默认环境缺少
   `readelf`，系统 `awk` 也不支持脚本使用的 `\y`/`strtonum`。本次以 gitignored
   临时 wrapper 调用 NDK `llvm-readelf` 并兼容 loader-info 生成；最终
   `offset_to_pokedata_workaround=1120`。
2. 仓库缺少 `.claude/skills/debug-server`，所以构建必须跳过
   `stageDebugSkillAssets`。
3. 上游脚本只把带内嵌 loader 的 `libproot.so` 放入 APK。该 loader 运行时释放到
   app 可写 cache；在真实 `untrusted_app` 进程中，Android 16 对
   `execve("/bin/sh")` 返回 `EACCES`。同一二进制在 `run-as` 域下可运行，说明
   问题是 App 执行可写文件的系统限制，不是 Alpine 文件损坏。
4. 本次把脚本已生成的 ARM64 loader 作为 gitignored
   `libproot-loader.so` 一并打包到只读 `nativeLibraryDir`，OpenMinis 现有
   `PRootKernel` 会自动设置 `PROOT_LOADER`。此后
   `debug.shellExecute` 从 `[Shell not running]` 恢复为正常执行。

这项 loader 打包修正只用于参考验证，没有进入 LingXi，也没有改动 OpenMinis
tracked 源码。

## 基础 rootfs

初始 rootfs：

| 能力 | 结果 |
| --- | --- |
| Alpine | `3.21.3` |
| BusyBox | `1.37.0` |
| `wget` | 已内置 |
| HTTPS / CA | 可用 |
| Git | 未内置 |
| Python 3 | 未内置 |
| OpenSSH Client | 未内置 |
| `curl` / `openssl` CLI | 未内置 |

因此，这个固定 OpenMinis commit 的基础 rootfs 并不满足 LingXi 固定工具集要求。

为单独验证 OpenMinis 的动态安装能力，执行：

```text
apk add --no-progress git python3 openssh-client ca-certificates
```

结果：成功，耗时 `69.10s`，安装后为 `65 MiB / 48 packages`。这只说明 OpenMinis
允许动态 `apk add`，不属于、也不能成为 LingXi 双商店版设计。

## Debug JSON-RPC 能力结果

调用接口：

```text
debug.shellExecute(command, session, timeout)
```

| 用例 | 结果 | 耗时/输出摘要 |
| --- | --- | --- |
| Alpine / BusyBox | 通过 | `3.21.3`；BusyBox `1.37.0` |
| 环境变量 | 通过 | `TEST_VAR=hello` |
| Pipeline | 通过 | `printf pipe \| tr a-z A-Z` → `PIPE` |
| 非零退出码 | 通过 | 返回 exit code `7` |
| stderr | **失败** | guest stderr 只进入 `PRootStderr` logcat，没有进入 RPC `output` |
| DNS | 通过 | `dl-cdn.alpinelinux.org` 解析为 IP |
| HTTPS / CA | 通过 | 获取 `https://www.alpinelinux.org/`，耗时 `2.22s` |
| workspace 写入 | 通过 | `/var/minis/workspace/marker.txt` |
| App 重启后持久化 | 通过 | 重启后仍读到 `persisted-data` |
| Session workspace 隔离 | 通过 | `workspace-b` 看不到 `workspace-a` 文件 |
| timeout 返回 | 部分通过 | `sleep 5` 在 `1.23s` 返回 124 |
| timeout 真正终止 | **失败** | 下一命令仍等待剩余约 `3.99s`，并泄漏上一命令 marker |
| 后台进程回收 | **失败** | `sleep 30 &` 后进程仍存在，需手动 `pkill` |
| 显式 shell cancel RPC | **不可用** | `debug.shellExecute` 没有对应 guest 命令取消接口 |

### Git（动态安装后）

`git init`、`add`、`commit`、`status` 全部通过：

```text
head=initial
status=clean
```

耗时 `0.14s`。

### Python 3（动态安装后）

标准库 JSON、`pathlib`、SHA-256、UTF-8 文件读写通过；异常 traceback 和退出码
通过显式 `2>&1` 验证：

```text
6bc0da1f42f9 True
RuntimeError: boom
python_rc=1
```

### OpenSSH Client（动态安装后）

通过：

```text
OpenSSH_9.9p2, OpenSSL 3.3.3 11 Feb 2025
```

Python 与 SSH 组合用例耗时 `0.16s`。

## 上游仪器测试

### `PRootKernelInstrumentedTest`

通过：`20/20`，耗时 `0.082s`。

### `ShellExecutorInstrumentedTest`

原测试用 `alpine-minirootfs.tar.gz` 探测资产，但 APK 实际打包名为
`alpine-minirootfs.tar`；同时 `skipIfNoBoot()` 只返回自身、不会跳过测试，导致原始
运行结果为 `20/22` 失败且都报 kernel 未 boot。

临时只修正测试夹具资产名后（跑完已恢复，未保留源码改动）：

- 通过：`12/22`
- 失败：`10/22`
- 总耗时：`61.964s`

主要失败原因：

- `ShellExecutor` 合并了 PRoot debug stderr，`native_offload` 注册日志污染 stdout，
  使 echo、pipeline、multiline、callback 等精确断言失败。
- `sleep 60` 配置 2 秒超时时实际运行约 60 秒并返回 `0`，证明阻塞式 read 没有被
  coroutine timeout 中断。
- DNS 测试硬编码要求 `/etc/resolv.conf` 包含 `8.8.8.8`，与真实设备 DNS 不符。

### `ExecutionCoordinatorInstrumentedTest`

无法编译。该测试仍引用已经从当前 `ExecutionCoordinator` 删除的
`mountedSessionId`（8 处 unresolved reference），并假设旧的全局 mount 切换模型；
当前实现已经改为每 Session 一个 persistent shell。该测试需要上游按新架构重写，
本次没有为了“跑绿”而修改产品实现。

## 安全与商店结论

当前 OpenMinis 能证明 Android 16 真机上的 PRoot/Alpine 基础机制可行，但不能直接
作为 LingXi 商店运行时：

- rootfs 可写，且允许 `apk add` 下载并执行新 ELF/动态库。
- PRoot 本身不是安全边界，当前参考链没有 LingXi 计划中的外层 Minijail、
  executable manifest、只读 base rootfs 和 W→X/动态库加载策略。
- timeout/cancel 不会可靠回收命令或完整进程树。
- Debug persistent shell 丢失 guest stderr。
- 默认 rootfs 缺少 Git、Python 3 和 OpenSSH Client。
- App 进程需要显式打包只读 loader，不能从可写 cache 执行 loader。
- PRoot/OpenMinis 的 GPL 授权条件仍与 LingXi 的 MIT/Apache 发布目标冲突。

结论：Project 的稳定 UUID、内部 workspace 和 Session 索引可以继续作为将来授权
PRoot 后端的宿主数据层；正式接入仍必须经过授权清单、固定哈希、不可变 rootfs、
Minijail 外层策略以及可靠的取消/进程树回收门禁。
