# Local App plugin 实施 harness

本目录是 `docs/local-apps/LOCAL-APP-PLUGIN-DESIGN-V2.md` 的**执行契约**：任务清单、
测试基线，以及一批「今天就已经会骗人」的判据的实测记录。判据引擎在
`scripts/lap_gate.py`（驱动 `lap-gate.sh`）。

所有数字都是在 `lap/baseline` 这个 commit 上**量出来的**，不是从设计文档抄的。
设计文档里的数字凡与本文件冲突，以本文件为准，并把冲突记下来。

## 基线

`baseline-stageB.json` —— 15 个爆炸半径 crate 的一次
`cargo test --all-features --no-fail-fast`：

    164 个测试二进制启动，164 个报告了 test result 行（无截断）
    5031 passed，3 red

三条红是**继承来的**，不是本项目造成的（landing commit 只动了 docs 和 scripts）。
每一条都必须带着「为什么它红」进 baseline，否则后面任何一次「全绿」都无法区分
「我没弄坏东西」和「我把别人的红一起洗掉了」。

### 1. `local_app_runtime_profiles::tests::published_r1_contract_digests_are_immutable`（已在发布前纠正）

phaser-2d 实际算出 `7fd39e60…`，钉住的是 `38b5fed9…`。

**这个 pin 从写下来的那天起就是错的。** 证据：实际值 `7fd39e60…` 在**整个仓库的
任何一个 commit 里都不存在**（`git log -S … --all` 零命中）；而 phaser-2d 的模板内容
和 profile 模块自 `391d89fff`（写下这个 pin 的那个 commit，2026-08-28）以来都没有再动过。
连跑两次结果一致，所以是确定性的错值，不是 flake。

Phase 2 review 确认 Local App Plugin 尚未发布，因此旧值从未成为兼容承诺。这里做了
一次明确的 **pre-release golden correction**：Phaser r1 从 `38b5fed9…` 修正为
`7fd39e60…`，Babylon r1 从 `466838f2…` 修正为 `82827767…`，并把 catalog、Rust
golden 与真实生产 contract 三向锁定。兼容性从这组新 r1 值开始；发布后不得再用同样
方式追认漂移，任何变化都必须按版本化 contract 处理。Phase 2 gate 现在真实运行这组
Rust contract 测试，并逐 family 注入错误 hash，证明五个 catalog 项都能把门变红。

### 2. `every_tool_with_output_calls_truncate_or_opts_out`

`web_fetch.rs` / `web_search.rs` 既没有 truncation 调用，也不在豁免集里。
WebSearch(Tavily) 是**用户确认过的有意分歧**，所以这条红大概率是分歧的副作用而不是缺陷。
本项目不碰这两个文件。

### 3. `filesystem::apply_line_window_tests::prefix_read_trims_only_incomplete_trailing_utf8`

`platform-api/src/filesystem.rs:440` 的 `assert!(got.truncated)`。该文件最后一次改动是
`24e91f6ab`（2026-08-25），早于 plugin-LSP 那次提交，所以不是它带进来的。

## 已实测的「会骗人的判据」

写任何一条门之前先读这一节。每一条都是**跑出来的**，不是读代码推出来的。

### 品牌门今天看不见 Claude 侧的 plugin 标识符

在 `apps/engine-mobile/src/lib.rs` 里种字面量，`./scripts/check-brand-leaks.sh` 的表现：

| 种进去的东西 | 结果 |
| --- | --- |
| `.lingxi-plugin` | **exit 1**，点名文件，点名规则 G1/G3 —— 门是会咬的 |
| `.claude-plugin` | exit 0，一声不吭 |
| `CLAUDE_PLUGIN_ROOT` | exit 0，一声不吭 |
| `CLAUDE_PROJECT_DIR` | exit 0，一声不吭 |

`L1_PATTERNS` 守的是 LingXi 字面量**逃出** `branding`，没有任何一条规则管
Claude 侧标识符**流进来**——而后者正是本项目引入的方向。另外今天的输出是
`规则<TAB>路径<TAB>needle`，**没有行号**，而 §19.3 要求点名文件与行号。
这些都是 P0a.0 的内容，不是「把脚本抄过来」。

§19.3 品牌 planted-positive 的另一半（`plugins/lingxi-local-app/` 下的
`CLAUDE_PLUGIN_ROOT`）**在这里跑不了**，那个目录还不存在。它跟着 Phase 2 第一个任务走。
不要写一条 fixture 不存在的断言。

### `generate.py --check` 有一条什么都不比的绿色通道

    python3 generate.py --check                                   -> OK: 1894 keys, 5 locales
    python3 generate.py --check --ios-out X --android-out Y        -> OK: 1894 keys, 5 locales

后者**一个字节都没有比对**：`stale_problems()` 在 `if not args.ios_out and not
args.android_out:` 里面，而 `OK:` 那行 print 在外面。两条路径的输出逐字节相同。
§19.10 把这个字符串当通过判据，所以门必须**禁掉这个参数组合**，并且另外断言 key 数。

### i18n 的三个基线计数（1893 个 key，不含 `__info_plist__`）

| 量 | 值 |
| --- | --- |
| Android 非法 key 名（`[a-zA-Z_][a-zA-Z0-9_]*` 不匹配） | 113 |
| 其中有显式 `_fmt` 兄弟 key 的 | 7 |
| **其中在 Android 侧完全没有对应 key 的** | **106** |
| 空值：zh-Hant / en / ja / ko | 26 / 20 / 26 / 26 |
| 与 zh-Hans 逐字相同（未翻译）：zh-Hant / en / ja / ko | 227 / 20 / 41 / 15 |

对着真实产物核过，不是推的：`1893 - 113 = 1780`，而
`clients/android/app/src/main/res/values/strings.xml` 里恰好有 1780 个 `<string name=`。
iOS 把占位符放在 **key** 里、Android 放在**值**里，所以每个带参字符串需要两个 key，
而**没有任何东西在强制这个配对**。§17.5 会新增带参 key，门必须断言这几个计数不上涨。

### `ClientCommand` 的 UniFFI 元数据只剩 2,247 字节

    UNIFFI_META_CONST_CLIENT_PROTOCOL_ENUM_CLIENTCOMMAND   14,137 / 16,384 字节（86.3%）
    uniffi_core-0.28.3/src/metadata.rs:87                  const BUF_SIZE: usize = 16384

§17.1 要加 ~7 个 operation。**Rust doc comment 是逐字拷进这个 buffer 的**，所以成本主要
是散文而不是结构。溢出是 const-eval 的 `error[E0080]`：它同时打断两个移动端构建，
而 `cargo build --workspace` 和整个测试套件**全绿**。

`create_metadata_items` 是**每个类型一个** 16 KiB buffer，所以把新 operation 收进一个
`LocalApp(LocalAppCommand)` 变体是真的搬走了字节（≈50 B vs ≈1,980 B 平铺）。
这是 Phase 8 **设计期**的决定，不是编码期的。

### `bridge-server` 今天是稳的,但判据仍然是三次运行

    run 1: 12 binaries, 88 passed, 0 failed
    run 2: 12 binaries, 88 passed, 0 failed
    run 3: 12 binaries, 88 passed, 0 failed        -> 三次导出的计数逐字节相同

`apps/bridge-server/src/driver.rs:553` 的 `LOOP_KA_TEST_SERIAL` 是
`std::sync::Mutex<()>`,**不是环境变量**。它在一个二进制内部串行化线程,
跨二进制什么都不做。所以「今天绿」不能推出「明天绿」,任何拥有这个 crate 的任务
仍然必须 `repeatRuns: 3`——`lap-gate.sh tasks` 会强制这一点。

`cargo-nextest` **未安装**。面对一个 15,430 个测试的套件,装 nextest 是最自然的
反应,而它会把每个二进制拆进独立进程,于是上面那把进程内的锁保护的每一个 race
都会静默回来。⛔ 不要在本项目里引入 nextest。

### ⚠️ 线索（未确认）：`confirm_runtime_profile` 找不到生产调用方

由 batch 4 的内容 grounding 发现，我自己顺着证据链复核过一遍。**这不是已确认的缺陷，
是一条值得上机验证的线索**——我一度差点把它报成「scaffold 在生产上不可能成功」，
那个结论是**错的**，记在这里以免下次再走一遍。

已经**核实**的部分：

- `LocalAppScaffold` 硬要求 `runtime_profile_receipt`，缺了就拒绝，错误文案是
  `"native runtime-profile confirmation must happen before scaffold"`
  （`local_apps_host.rs:5254-5262`）；同时**拒绝**调用方自带 `runtime_profile`/`surface`。
- 生产上唯一签发 receipt 的地方是 `issue_runtime_profile_receipt` 的
  `local_apps_host.rs:4054` 调用点，它在 `confirm_runtime_profile_value`（`:3964`）里面。
  其余 9 个调用点全部 ≥ 10029，而 `#[cfg(test)]` 从 **8874** 开始 —— 都是测试。
- `confirm_runtime_profile_value` 的调用方只有两个：`:6395`（trait impl）和 `:10418`（测试）。
- `:6395` 只能从 `local_apps_mcp.rs:1606` 的 dispatch 分支到达，而那个分支只能由
  `call_host_operation(self.operation, …)` 进入；`call_host_operation` 全仓库只有一个
  调用方（`local_apps_tools.rs:409`），`self.operation` 取自 `LOCAL_APP_TOOLS` 那张
  **24 个操作**的表 —— 表里**没有** `confirm_runtime_profile`。
- Swift / Kotlin / 模板 JS / 其余 Rust 里对 `confirm_runtime_profile` 零引用。

**我差点报错的地方。** 上面这串证据看起来像「receipt 在生产上铸不出来 ⇒ scaffold 永远失败」。
不对。原生回路**是通的**，只是走另一条路：
`ClientCommand::ResolveAppRuntimeProfileSelection` → `host.rs:7083` →
`resolve_runtime_profile_selection(request_id, selected_family)`，它**唤醒
`confirm_runtime_profile_value` 内部挂起的那个 await**，然后 `:4054` 才签发 receipt。
iOS 客户端确实实现了 `RuntimeProfileSelection`（`clients/ios/Sources/LocalApps/` 三个文件）。

所以缺的不是「客户端没接」，而是**发起那一侧**：谁在生产上第一次调用
`confirm_runtime_profile_value` 把选择请求推给客户端。我没找到。

⚠️ **判据只能是真机**：跑一次「创建空 workspace → scaffold」。这条链上每一层的单测都是绿的
（`:10418` 直接调用那个函数，绕过了整张工具表），所以**任何测试都不会发现这个问题**——
如果它真的是问题的话。参考 [[local-app-phase-1a-ui-contract-2026-08-24]]：那批 UI 契约
15 个提交全绿且过评审，一行都没上过真机。

### ⚠️ 四个操作有 dispatch 分支和 schema，但没有调用方（**上一版记错了，这是更正**）

⛔ **先记我记错的地方。** 我一度写下「`list_tools` 把这些当工具广告出去、`call_tool`
又拒绝执行」，并说这是「一整类缺陷的第二例」。**两处都不对**，多查一层就翻转了：

- `list_tools`（`local_apps_mcp.rs:1850`）**根本不服务** `tool_catalog()`，它只服务
  `dynamic_tool_catalog(&manifest)`，即 per-app 动态命名空间。它自己的注释写明了原因：
  静态 host 操作现在是 builtin 工具（`LocalApp*`），同时从 MCP 端再暴露一次会让
  「同一个操作在两个名字下可达，而两套权限语义不同」。
- 所以 `call_tool` 拒绝静态名字**是和 `list_tools` 一致的**，不是矛盾。
  **没有任何「列出来却调不动」的工具。**

**实测到的、确实成立的事实**（比我原来的说法窄）：

| 操作 | builtin 表 | dispatch 分支 | `tool_catalog()` schema |
| --- | --- | --- | --- |
| `confirm_runtime_profile` | ❌ | ✅ | ✅ |
| `confirm_dependency_change` | ❌ | ✅ | ✅ |
| `update_dependencies` | ❌ | ✅ | ✅ |
| `migrate_runtime_profile` | ❌ | ✅ | ✅ |

`tool_catalog()` 声明 28 个，`LOCAL_APP_TOOLS` 只有 24 个，差就是这四个。
`call_host_operation` 的唯一调用方（`local_apps_tools.rs:409`）拿的是表里的
`self.operation`，所以这四条 dispatch 分支**我没找到调用方**。

⚠️ 它们四个都是**确认 / 迁移**类操作 —— 即铸 receipt 的那一类。这和上面
`confirm_runtime_profile` 那条线索是同一件事，只是范围更清楚了：不是「一整类缺陷」，
是**同一条未接通的确认通路上的四个操作**。

判据仍然只能是真机：跑一次「创建 → 确认 runtime profile → scaffold」。
每一层的单测都是绿的，因为测试直接调函数、绕过了那张表。

**方法论教训**（比结论本身值钱）：我在两个不同的地方，凭「A 引用了 B、B 拒绝了 A」
的形状就宣布了缺陷，两次都是再查一层就翻转。证据链看起来闭合的时候，恰恰是最该
多查一层的时候——尤其是当结论意味着「这个功能在生产上根本不可能工作」，而这个功能
明显在工作。

### ⚠️ builder agent 的写边界是**构建期**的，不是写入期的

七个 agent 里只有 `builder` 拿到 `Read`/`Write`/`Edit`。它的边界**不在**工具授权里——
`tools/file/src/{read,write,edit}.rs` 没有路径作用域，agent frontmatter 也没有这种字段。

真正的强制点是 `restore_host_managed_files`（`local_apps_build.rs:505`），它在**构建时**
把 `managed_files` 重新钉回去。那个函数自己的注释写得很清楚：
「`.lingxi/source-policy.json` **声明** `host_managed_paths`；在这里恢复，
才让构建成为真正的强制点，而不是把契约只留在 prompt 文本里。」

所以：**写入的那一刻没有任何东西挡着**，agent 可以写工作区里的任何文件；下一次构建
会把 host-managed 的那些覆盖回来。对「防止 agent 意外改坏脚手架」来说够用；对
「防止 agent 故意写别处」来说不够。写 builder 的 prompt 时不要暗示写入本身受限。

### 门的覆盖面：三条实测边界

**组件字面量扫描器看不到 `plugins/`。** `SCAN_ROOTS` 是
`["apps/engine-mobile/src", "tasks/src", "tools/workflow/src"]` —— plugin 包不在里面。
这是**对的**：plugin 包本来就该包含组件名（那是它的内容）。但要记住这句话的另一半：
「§19.3 的属性成立」说的是**这三个目录**，不是整个仓库。

**品牌门看得到 `plugins/`。** 实测过，不是推的：往 `skills/device/SKILL.md` 种一个
`.lingxi-plugin` 立刻报红并点名。它按 `git ls-files` 枚举全仓库。

⚠️ **我犯的流程错误**：内容批次落地后我跑了 frontmatter 门，**没跑品牌门**。
于是 8 条合法的 plugin 内容引用（`workspace/LINGXI.md`、`.lingxi/source-policy.json`）
一直红着，直到我为了别的事去跑它才发现。**新增一批文件之后要跑的门不止一道**，
而「我记得跑哪几道」不是机制。

**§19.3 品牌 planted-positive 的第二半现在可构造了。** 它要求往
`plugins/lingxi-local-app/` 里种一个 `CLAUDE_PLUGIN_ROOT` 并要求变红。之前那个目录
不存在所以只能推迟；现在它存在且有 31 个已跟踪文件。P0a.0 可以把这一半补上了。

### 其他量出来的事实

| 事实 | 值 |
| --- | --- |
| `engine-mobile` 测试数：无 feature / `--all-features` | 2 / 452 |
| workspace 成员 / default-members | 88 / 74（`engine-mobile` 被排除） |
| `plugin` crate 的 fixture 目录 / 品牌归一化 | 都不存在 |
| `git remote -v` | 空 —— **没有 CI**，每一条门必须是签入的脚本 |
| 旧 `drive_local_workflow("local-canvas-build")` / builtin 调用点 | `0`；Phase 9 后不再有 standalone builtin Local App workflow，统一由 `lingxi-local-app:local-app-build` 启动 |
| Canvas 两个具名门 | Plugin `workflows/local-app-build.js` 的 `render_check` / `motion_check`，都在 |
| `profile_file!` 调用点 | 112 |
| `runtime-profiles/` 已跟踪文件 / 字节 | 124 / 387,943 |
| `templates/` 全目录已跟踪 / 磁盘占用 | 156 文件 / **524 MiB** |
| `babylon-3d` / `phaser-2d` 磁盘文件数 | 20,093 / 13,014（`mcpb.rs:16` `MAX_FILES = 10_000`） |
| `skills/` | 10 目录 / 44 md；`bundled.rs` 里 44 个 `include_str!` |
| `CLIENT_PROTOCOL_VERSION` / `APPS_SCHEMA_VERSION` | `10.0.0` / `3` |
| `blessed_major.txt` / `contract_index.json` | `10` / 1,068 条 |
| MCP 版本常量 | `mcp/src/initialize_params.rs:9` = `2025-11-25`；`apps/cli/src/commands/mcp.rs:358` = `2025-06-18`（分歧） |

## 用法

    cd lingxi-code
    ./scripts/lap-gate.sh list          # 每个子命令守哪一条判据
    ./scripts/lap-gate.sh selftest      # 引擎自己的 10 个种雷用例
    ./scripts/lap-gate.sh tasks         # 加载时校验任务清单
    ./scripts/lap-gate.sh precheck --baseline docs/local-apps/harness/baseline-stageB.json

重跑基线：

    cargo test -p engine-mobile -p plugin -p client-protocol -p branding -p tasks \
      -p tool-workflow -p local-apps -p skill-api -p engine-desktop -p permission \
      -p platform-api -p mcp -p bridge-server -p test-harness -p cli \
      --all-features --no-fail-fast 2>&1 | tee /tmp/stageB.txt
    ./scripts/lap-gate.sh parse --run /tmp/stageB.txt --out /tmp/stageB.json

⛔ `--all-features` 不能省：`engine-mobile` 的 Local App 模块全在
`#[cfg(feature = "uniffi")]` 后面，452 个测试里有 450 个在没有这个 flag 时**不存在**。
