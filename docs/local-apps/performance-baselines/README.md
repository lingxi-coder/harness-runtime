# Local App plugin 性能基线

`local-app-plugin-v1.json` 的十个值逐字取自设计文档 §19.0。它们是 **hard limits**：
字段缺失、值为 0、或用了非数值占位符时，验证直接失败——这三种情况各自要有反向用例。

## 为什么这个文件在 Wave 0 而不是 Phase 1

§19.0 把它列在 Phase 1，但它**必须在任何东西被测量之前写好**。先测再写阈值就是
§19.0 自己禁止的「追认式门」：那样得到的不是门，是对现状的一次描述。

写下这些数字的时候，`runtime-profiles/` 下 124 个已跟踪的脚手架源文件合计
387,943 字节（约 379 KiB）。4 MiB 的 archive 上限和 12 MiB 的解包上限因此有约
10 倍余量，不是对现状的橡皮图章。

## 阈值可以改，但不能顺手改

调整阈值必须走**独立的、带 benchmark evidence 的设计变更**。⛔ 不允许在同一个功能
提交里放宽阈值来让失败的构建变绿——那是让门去追认现实，和用 `BLESS=1` 重新基线化
`contract_index.json` 是同一个失败形状。本仓库里已经有一个活的例子：
`published_r1_contract_digests_are_immutable` 钉住的 phaser-2d digest 从写下来那天起
就没对过（见 `../harness/README.md`）。

## 每份 evidence 必须附带的东西

只有 p50/p95 数字不构成 evidence。每次测量还要记：

- 设备 model / OS 版本 / WebView 版本
- build type（debug / release —— release 之外的数字不进基线）
- thermal 与 power 状态（降频会把一次回归伪装成噪声）
- **5 次 warmup，至少 30 次采样**

iOS 的最低基线是当前已配对的 iPhone 11 / iOS 18.6.2。
⚠️ Android 必须在 Phase 1 关闭之前登记一台覆盖当前 `minSdk = 26` 支持面的物理参考设备；
**没有那台设备的 evidence，Phase 1 不能关闭**。采购周期排在两个 L 级性能任务前面，
所以这件事属于 Wave 0，不属于 Phase 1。

## 不设伪阈值的那一部分

Canvas / Three / Phaser / Babylon 的 FPS 与 frame-time 在本版本**只记录 evidence**。
没有设备基线之前不设 hard threshold——一个没人验证过的数字看起来和门一样，
但它挡不住任何东西，只会在第一次红的时候被调宽。

`logical_servers_100_retained_heap` 量的是 Host logical-server registry、shared hub、
broadcast/filter state，以及最多 8 份 exposed `Connected.tools` snapshot 的**总增量**。
它不把磁盘上的 catalog 文件大小算进 retained heap，也不假设现有 MCP registry 的
`Connected.tools` 不存在。fixture 用的是 §13.2 合法上限内、且能通过 2,048-token
per-App gate 的真实 schema。
