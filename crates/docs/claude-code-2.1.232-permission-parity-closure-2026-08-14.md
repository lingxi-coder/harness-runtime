# Claude Code 2.1.232 permission parity 关闭记录

**日期：** 2026-08-14

**LingXi 基线：** `main@c5627025102a`（工作树内实现，未提交）

**Oracle：** 本机 Claude Code `2.1.232`

**Oracle SHA-256：**
`7b39c1588df919d001dea3ffd5651adb682f2451b5a0e18d42d4233296b53cc7`

**公开来源：**

- <https://raw.githubusercontent.com/anthropics/claude-code/refs/heads/main/CHANGELOG.md>
- <https://code.claude.com/docs/en/permissions>
- <https://code.claude.com/docs/en/permission-modes>

## 结论

Claude Code 2.1.232 changelog 中可由本项目 permission/sandbox 边界复现的公开、
可观察安全变更已经对齐。对齐范围包括判定结果、规则来源优先级、用户可见原因文本和
关键解析边界；报告不声称复制 Claude Code 的私有内部实现。

项目全局 `CLAUDE_CODE_VERSION` 仍保持 `2.1.220`。本轮只验证 permission/security
范围，未验证 2.1.221—2.1.232 的全部非权限产品能力，因此不做误导性的全局版本升级。

## 已关闭差异

| 范围 | 对齐结果 |
|---|---|
| PowerShell variable-writing | `acceptEdits` 识别主命令与嵌套命令中的变量写入参数，保护 `$PSDefaultParameterValues` 和 ActionPreference 变量；修正反引号混淆及 slash-prefix 过度拦截。 |
| Windows Git Bash/Cygwin symlink | 识别 `!<symlink>` cookie，按 oracle 上限有界读取；支持 UTF-8、Latin-1 与带 BOM 的 UTF-16LE，Windows 路径转换保留 lone surrogate，并保留 cookie 目标后的路径尾部。 |
| Bash input redirect | 把文件型 `< file`、`<> file`、`<& file` 送入 `Read(...)` deny 与工作目录检查；`/dev/null` 跳过；`<&0`/`<&-` 等 fd duplication 不误识别为路径；deny 原因文本逐字节锁定。 |
| Bash/zsh parser hardening | 覆盖全部根节点、注释、允许的语句间隔、wrapper 递归、同一行相邻命令、zsh `[[ ]]` regex/extglob 和 heredoc/未知变量回退。 |
| Filesystem containment | 同时检查词法路径和所有可发现的原生/cookie symlink 解析形态；目标路径解析失败保持 fail-closed，单个额外 working dir 解析失败则跳过并继续检查其他有效目录。 |
| `sandbox.ripgrep` provenance | 只接受 managed、`--settings` 和 user 来源，优先级为 managed > flag > user；project/local 合并值不再覆盖 sandbox 二进制。 |
| Auto-mode eligibility | session 切换和 MCP pin 共用同一不可用原因，并锁定用户可见错误文本。 |
| Auto-mode denial breaker | 连续 3 次或总计 20 次 classifier deny 后，headless 返回精确 abort；interactive 保持 Ask，并通过 typed context 转发 `classifier` decision reason 与 `Latest blocked action`；普通 classifier deny 也保留 oracle 的原始 reason 文本。 |

## 验证证据

- `CARGO_TARGET_DIR=/tmp/lingxi-permission-verify cargo test -p permission --lib --quiet`：1300/1300 通过。
- 同一 target 加 `--features bash-ast`：1460/1460 通过。
- auto-mode classifier deny reason、headless breaker 和 interactive typed-context 回归：3/3 通过。
- orchestrator headless abort 传播回归：1/1 通过；`cargo check -p orchestrator --tests --no-default-features` 通过。
- tool-api terminal abort 与 agent 无 tool-result abort 回归：各 1/1 通过。
- UTF-16LE cookie target + remaining tail filesystem 回归：1/1 通过；此前 filesystem fail-closed/多 working-dir focused tests 全部通过。
- scoped `git diff --check`：通过。
- 工作树包含其他功能分支的未提交格式变更；因此没有把 workspace-wide `cargo fmt --check` 的失败误报为本轮 permission 代码失败。

## 明确边界与剩余风险

### 既有 accepted carve-outs

- Enterprise managed-policy MCP governance 已由项目状态记录确认为 out of scope。
- Claude 私有 classifier 的 safety-filter refusal 与 attribution-header 请求行为在本地确定性
  classifier 中没有对应的远端调用，因此属于结构性不适用，而不是伪造等价结果。
- Windows 目标已交叉编译并对原始 UTF-16 code unit 做回归；本轮没有 Windows 实机运行。

因此可宣称的是“Claude Code 2.1.232 公开可观察 permission/security delta 已对齐”，
不能宣称“整个 Claude Code 私有实现全链路 byte-identical”。
