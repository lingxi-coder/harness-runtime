# Computer Use：接入、动作与恢复

本文描述 2026-10-07 工作树中的实现及验收边界。实现范围是 macOS、OpenAI Responses、Claude Messages client toolset 与 Gemini Desktop Interactions。目前没有本轮真实 Provider API 或可见 macOS 桌面验收证据；协议 fixture、fake backend 和恢复测试不能代替这些验收，也不能据此宣称 NativeFirst 已开启。

## 源码与配置身份

| 项目 | 当前身份 |
|---|---|
| Harness HEAD | `4dfd9abef1dc293a9617d35387165cb2256b5d98`，另有工作树修改 |
| 实际 SDK HEAD | `18ab9d51e6591f9619be8cf23aa2f62d10deb1c4`，另有工作树修改 |
| Harness manifest 中的 Git rev | `0c6a907d897a54e656700c00cf335d91b10dca0b` |
| 当前生效的 SDK 来源 | Cargo `[patch."https://github.com/lingxi-coder/llm-client"]` 指向 `../llm-client` |
| SDK 基线合并报告 | 工作区 `.omx/context/computer-use-implementation-20261006/sdk-baseline-alignment-20261006/alignment-report.md` |

manifest 仍保留原 Git 提交；当前验证使用本地 patch 指向的新 SDK 工作树。新实现尚无已发布的不可变提交。发布前需提交并发布 SDK，再将 Harness 的 Git rev 对齐该提交；当前配对工作树构建已使用新源码。

原生启用由 host 提供 `VerifiedComputerProfile { model, profile, provider, evidence }`，通过 `ConversationOrchestrator::with_verified_computer_profiles` 注入。空 allowlist 保持 function 投影。构造器过滤空 `evidence`，但无法鉴定一段字符串是否来自真实验收；集成方必须提供能够定位实际 model/profile、endpoint、协议、操作与桌面结果的证据。当前 desktop assembly 没有注入已验收 profile；本轮不能填写虚假的 evidence 来启用原生。

## 既有工具生命周期

注册名称仍为 `computer`。当前 schema 包含原来的 25 个 action，并新增 `mouse_click`、`key_down`、`key_up`；原 action 的字段含义继续有效。`computer_batch` 仍是用户显式调用的 wrapper，保留 batch 返回结构与 wrapper hooks。管理动作包括授权、应用打开、显示器切换、剪贴板与 grant 查询，继续通过 function 入口可达。

`computer_batch` 的 `results[].result` 中 screenshot/zoom 使用现有共享媒体映射，按步骤提供文字标签与图像；原返回结构和完整图片仍交给普通 hooks 与审计。模型文字保留步骤、错误和几何元数据，移除重复的 base64。主轮、Mod 结果映射和子 Agent 使用同一规则；最终可见图像数量、最后一张完整截图与几何必须一致，早期同像素截图不能替代被移除的最终图像。后续输入、zoom 或失败仍撤销完整观察资格。

Skills、ToolSearch、工具注册/发现、动态 deny、审批、PreToolUse、PostToolUse、Mods 与历史发布仍属于 Harness 的原 Agent 生命周期。MCP 的 `computer-use` 与 `mcp__computer-use__*` 保持原有名称、发现及 instructions 条件。本期原生绑定只使用已注册的内置 `ComputerTool`，不得用 MCP 句柄替换它；恢复时也从实际 registry 取内置 Arc。

Computer 原生声明、continuation 与回执提交 authority 只附到主模型请求，包括定时任务的主轮次。辅助请求、compaction fork、分类器、估算和预热在历史转换前隔离该请求 scope；ApiService 与直接 SideQuery 路径都如此。辅助请求保留自己的历史与普通 function 目录，不能消费主回执。独立子 Agent 请求也使用自身请求上下文。

职责对应源码：

| 所有者 | 责任与源码入口 |
|---|---|
| SDK `llm-client` | 声明、Provider wire、协议归一化、continuation 与回执；`src/protocol/computer.rs`、`src/providers/{openai,anthropic,google}/*computer*.rs` |
| `ComputerTool` | schema、纯转换、授权、输入验证、持键、实际操作与观察状态；`crates/tools/computer-use/src/{lib,native,extensions,validate,lock}.rs` |
| macOS backend | 系统事件、屏幕捕获、显示器与 Retina 几何；`crates/platforms/macos-computer-control/src/macos/mod.rs` |
| Orchestrator | 本轮声明、完整响应后拆分普通工作项、有序 dispatcher、hooks 与可靠发布；`crates/orchestrator/src/native_computer.rs`、`native_computer/recovery.rs` |
| Host/Session | 泛化执行/回执契约及唯一 durable ledger；`crates/core/src/host/tool_execution.rs`、`crates/runtime/src/desktop/session_state{.rs,/tool_execution.rs}` |
| llm-runtime | 正常请求链路中的原生投影与历史 companion；`crates/llm-runtime/src/{computer,history_input,history_projection}.rs` |

每轮互斥声明 `computer` function 和 native computer。正常从 function 管理轮开始；实际成功、权限、最终截图、有效几何与已验收 profile 均满足后可进入 native；native 结果按产生它的旧绑定回填，下一轮重新提供 function。其他工具仍按有效 catalog 投影。没有根据模型文本猜模式，也没有强制额外收尾请求。

流式 native 响应完整校验后才产生输入。每个归一化成员都是普通 `computer` ToolUse：PreToolUse → 审批与最终参数校验 → durable Started → tool.call → execution terminal → PostToolUse/Mods 最终输出 → 可靠普通历史 → receipt。首个失败、拒绝或取消停止其后原生成员，并记录 skipped。SDK 要求的补充截图也是普通成员；截图失败不能导致此前点击重放。

停止判断读取 journal 中的实际执行 outcome；Mods 改写展示结果不能将失败变成下一项输入的授权。Provider 的安全确认随 Started 保存且不可在后续阶段改写，恢复不依赖内存确认表；Gemini 失败结果保留确认审计事实，但不投影成功执行的安全回执。

调用关联使用 `(provider_response_id, call_id)`。恢复的 abandoned 标记保存精确调用身份，不能因 Provider 复用 call ID 而删除新响应。重复发布已存在的历史行只在该行仍为 transcript tip 时更新父链游标。

回执在发送前保存 Submitted，并在实际派发回调中再次检查 admission。只有回调明确拒绝、证明运输层尚未发送时，才保存 NotSubmitted；该回执可复用原内容重提，每次 admission 使用递增的 submission_attempt 区分 WAL 事件。其他提交失败仍按结果未知处理，禁止盲目重发。

执行记录的 `recovery_binding` 在 Started 前可靠保存完整调用、frame 与 route 的外部引用，并随后续阶段保持不可变。未知执行的旧历史被压缩后仍可恢复审计事实与同 Provider 的重新观察路径；已经从模型历史移除的 abandoned 调用各成员 ToolResult（包括 Unknown、Skipped 或此前成功的成员）不会被重新注入。

压缩前从同一 journal 查找 Prepared/NotSubmitted 回执，将其完整已接受原始 Assistant 行及后续普通调用、最终结果、回执和通知并入现有保留尾段。保留尾段通过既有 compact metadata 和 JSONL loader 冷恢复；内存派生通知须先由现有 durable append-once 发布。是否已发布依据 loader 可读取的完整物理 JSONL 行，不能把提前登记的身份 sidecar 当作写入 ACK；身份已登记但物理行缺失时继续补齐。身份交集与冷恢复共用精确 JSON 解析；普通 User/Assistant 文本及字符串 ToolResult 恢复原始 UTF-16 单元，包括孤立代理字符，避免把已发布行误判为缺失或把恢复文本改为替换字符。已提交、结果未知及已完成回执不重新保留；原始行缺失时拒绝删除上下文。

完整、经 session.append 接受的 successor 在普通行发布前保存为 `ResponsePrepared`。回执的 `response` 引用保存该完整行；ACK 同时保存 receipt ID、execution IDs、submission attempt 与 route binding。恢复只依据这份可靠保存的完整响应核验关联，补齐同一批回执的 ResponsePrepared，幂等发布原行，再保存 ResponseReceived；没有响应证据的 SubmissionUnknown 继续禁止重提。输出、回执与 successor artifact 显式保存精确 UTF-16 overrides，避免恢复时改变已接受的 JavaScript 字符串或触发重复行冲突。

按键后端、系统快捷键授权及持键 ledger 使用共同的 `canonical_computer_key`，包括 spacebar、方向键与 modifier 别名。所有 macOS 点击按钮都记录成功的 Press，只有成功 Release 才清除；失败时 ComputerTool 保留 dirty 状态和桌面租约，cleanup 重试后端记录的全部按钮。Agent 在 Completed、idle、失败或取消前最多重试三次输入释放；仍失败则明确发布 Failed，并保留尚未释放输入的所有者租约。任务池无法将这种退出误报为成功。

左键已持有时，同键的 click/double/triple 及显式 `mouse_click(left)` 在修饰键和点击发布前拒绝，并由现有失败清理释放旧 hold；其他按钮、移动和显式 mouse-up 保持可用。macOS 文字输入保留 Enigo 的 20 字符块与前导换行处理，将其中实际发键的 Tab 改为已有 Press/Release 路径；具备持键清理能力的 backend 在 typing 前标记键盘 dirty，失败时保留释放责任与租约。不支持该能力的 backend 不要求执行不存在的持键清理。

Gemini 的 portable/native 图片回执共享非空数据、base64、图片 MIME 和 URI 校验；最终 Hooks/Mods 输出不能用无效图片满足截图要求。

## 协议、单位与回执

下表版本字符串是当前代码的数据契约标记；它们不构成对外部线上 rollout 的验收声明。

| Provider | 入口、版本/format | 坐标与单位 | 回执约束 |
|---|---|---|---|
| OpenAI | `OpenAiResponses`；`openai.responses.computer_tool.v1`、`computer_call.v1`、`computer_call_output.v1` | 模型最终可见截图的像素坐标；scroll 为像素；无参 wait 固定 2 秒 | 每个原生 call 末尾补 screenshot；只接受全部 Succeeded 且最终可见结果含图像，保留原 call/item ID、原 scoped continuation 与确认关联。旧回执可以和下一轮 function 声明同发，但真实 API 尚未验收 |
| Claude | `AnthropicMessages`；`computer_toolset_20260801`，命名空间 `computer` | 模型图像像素坐标；scroll_amount 是 wheel 刻度，绝不换算为像素；duration 秒 | ToolResult 回填原 tool_use_id、`toolset_name=computer`；错误使用 is_error；全部 skipped 使用固定未执行说明。成功 screenshot/zoom 必须有最终可见图像 |
| Gemini | `GeminiInteractions`；`interactions.desktop.v1`；`google.interactions.computer_call.v1` / `computer_result.v1` | 坐标整数 `0..=999`，按 `value / 1000 * frame_dimension` 转成图像像素；scroll 像素，缺省 300；wait 整数秒 `0..=300`，缺省 1 | `function_result` 回填原 call_id/name；保留旧 `_sdk_continuation`，codec 校验后从 wire 移除。OutcomeUnknown 不可续接；成功且要求截图时最终 Screenshot 结果必须含图像，早期动作附图不能替代；输入已成功执行时确认决策须精确匹配 |

Gemini 管理函数和 Desktop 原生控制共享 Interactions endpoint、统一 `PreparedCall`/codec、历史与 usage 链路。OpenAI 不支持按动作禁用，因此要求完整 click/move/drag/pixel-scroll/key/type/wait/screenshot 能力才声明；Claude 通过成员 enabled 过滤，Gemini 通过 excluded_predefined_functions 过滤。三者都拒绝同轮的同名管理声明。

### Backend 与测试定位约定

下面动作矩阵中的 `B-*` 指当前 macOS 实现入口，均尚未做本轮真实桌面验收：

| 编号 | Backend 入口 |
|---|---|
| B-C | `mouse_click` / `click_current`；侧键由 `post_side_button` 发真实 CGEvent |
| B-M | `mouse_move`、鼠标 down/up；path 由 ComputerTool 按点顺序执行并收尾释放 |
| B-S | 旧 wheel `scroll` / `scroll_current(..., false)`；pixel 为 `scroll_pixels` / `post_pixel_scroll` |
| B-K | `key_chord`、`key_down`、`key_up` 与解析后的 enigo key；ComputerTool 跟踪跨调用持键 |
| B-T | `type_text`，press_enter 由普通 Return 键事件补发 |
| B-W | ComputerTool 的协作式取消等待，hold 收尾释放；不把 wait 当文本结果 |
| B-I | `screenshot`、`zoom` 与 `frame_geometry`；最终模型图像由 ToolResult 媒体路径处理 |
| B-P | `cursor_position` |

测试列是定位源码的入口。`分支；无独立 fixture` 表示有实现分支但这里没有找到该 Provider 动作的独立断言；它不是动作验收通过的标记。

| 编号 | 测试入口与覆盖重点 |
|---|---|
| S-O | SDK `tests/computer_normalization.rs::openai_retains_actions_modifiers_geometry_safety_and_requires_explicit_observation`：middle click/modifier、完整 drag path、2 秒 wait、末尾观察、图像与失败回执 |
| S-O-wire | SDK `tests/openai_computer_codec.rs`：typed call/output、完整流式终态、确认、混合函数、旧回执配新 function 声明 |
| S-C | SDK `tests/computer_normalization.rs::claude_all_members_preserve_cursor_wheel_repeat_and_duration_semantics`：17 个成员、当前位置、wheel、repeat、duration |
| S-C-wire | SDK `tests/anthropic_client_toolsets.rs`、`anthropic_toolset_stream.rs`：namespace、成员/结果、声明互斥与流式 |
| S-G | SDK `tests/gemini_interactions_codec.rs::desktop_coordinates_defaults_and_ordered_observation`：click 坐标、scroll/wait 默认值、末尾截图、越界拒绝 |
| S-G-wire | 同文件其他测试：普通函数共享 endpoint、完整流式、确认/图像、scoped continuation、未知提交不 failover |
| H-X | Harness `execution_tests::path_modifiers_pixel_scroll_and_enter_preserve_sequence`：path、modifier、pixel/wheel 不混淆、press_enter |
| H-K | `cumulative_shortcut_is_denied_before_second_press`、`held_modifier_survives_mouse_modifier_scope_and_owned_cleanup`、`cancel_hold_releases_input_and_wait_really_waits` |
| H-I | `screenshot_readiness_is_owner_specific_and_geometry_is_rechecked`、`screenshot_requires_final_image_and_hook_replacement_invalidates_readiness`、`native_sequence_preserves_frozen_geometry_but_requires_terminal_observation` |

### OpenAI 逐动作映射

均经 `providers/openai/computer_adapter.rs::normalize` → `ComputerTool::lower_native`；坐标不再经 Provider 特有执行器。

| 原生 action | 归一化 → `computer` 参数 | 单位/补充语义 | Backend | 测试 |
|---|---|---|---|---|
| click | Click → `mouse_click` + button + coordinate + modifiers | left/right/wheel→middle/back/forward；单击 | B-C | S-O；其他按钮分支无独立 native fixture |
| double_click | Click(count=2) → `double_click` | 左键、像素坐标、保留 modifiers | B-C | S-O-wire；分支，无独立动作 fixture |
| move | Move → `mouse_move` | 像素坐标、保留 modifiers | B-M | S-O-wire；分支，无独立动作 fixture |
| drag | Drag → `left_click_drag.path` | 保留全部中间点和 modifiers | B-M | S-O、H-X |
| scroll | Scroll → `scroll.pixel_delta=[dx,dy]` | 像素；不使用 wheel amount | B-S | H-X；OpenAI 分支无独立动作 fixture |
| keypress | Key → `key.keys` | 无损键数组 | B-K | H-K；OpenAI 分支无独立动作 fixture |
| type | Type → `type.text` + press_enter=false | 原文本 | B-T | H-X；OpenAI 分支无独立动作 fixture |
| wait | Wait → `wait.duration=2` | 秒；协作式取消 | B-W | S-O、H-K |
| screenshot | Screenshot → `screenshot` | 必须来自最终模型可见图像；缺末尾观察时 SDK 补成员 | B-I | S-O、S-O-wire、H-I |

### Claude 逐成员映射

仅 `toolset_name=computer` 且 direct caller 的成员进入此适配。`text` 用于鼠标 modifier 时只允许 shift/ctrl/alt/super；不支持的字段在输入前拒绝。

| 成员 | 归一化 → `computer` 参数 | 单位/补充语义 | Backend | 测试 |
|---|---|---|---|---|
| screenshot | Screenshot → `screenshot` | 成功回执必须含最终图像 | B-I | S-C、H-I |
| zoom | Zoom → `zoom.region` | `[x0,y0,x1,y1]`，右下边界 exclusive；成功回执必须含图像 | B-I | S-C |
| left_click | Click(left,1) → `mouse_click.button=left` | 省略 coordinate 明确用当前位置；text modifiers | B-C | S-C |
| right_click | Click(right,1) → `mouse_click.button=right` | 坐标/当前位置；Full 权限 | B-C | S-C |
| middle_click | Click(middle,1) → `mouse_click.button=middle` | 坐标/当前位置；Full 权限 | B-C | S-C |
| double_click | Click(left,2) → `double_click` | 坐标/当前位置 | B-C | S-C |
| triple_click | Click(left,3) → `triple_click` | 坐标/当前位置 | B-C | S-C |
| left_click_drag | Drag → `left_click_drag.path` | 原 start_coordinate 与 coordinate 转成有序两点 path；保留 modifiers | B-M | S-C、H-X |
| mouse_move | Move → `mouse_move` | 原 coordinate 像素 | B-M | S-C |
| left_mouse_down | MouseDown(None) → `left_mouse_down` | 当前光标，无伪造坐标 | B-M | S-C |
| left_mouse_up | MouseUp(None) → `left_mouse_up` | 当前光标；释放本所有者输入 | B-M | S-C |
| cursor_position | CursorPosition → `cursor_position` | 返回目标显示器局部像素 | B-P | S-C |
| scroll | ScrollWheel → `scroll_direction/scroll_amount` | `1..=100` wheel 刻度；省略坐标为当前位置；保留 modifiers | B-S | S-C、H-X |
| type | Type → `type.text` + press_enter=false | 原文本 | B-T | S-C |
| key | repeat 个 Key → repeat 个普通 `key.keys` 成员 | `repeat=1..=100`；每次独立审批/记录；literal `+` 可表达 | B-K | S-C、H-K |
| hold_key | HoldKey → `hold_key.keys/duration` | 秒，最长 300；取消时收尾释放 | B-K/B-W | S-C、H-K |
| wait | Wait → `wait.duration` | 有限秒数，最长 300 | B-W | S-C、H-K |

### Gemini Desktop 逐函数映射

源为 `providers/google/computer.rs::operation`。带坐标的动作从 `0..=999` 转到最终截图像素。除 take_screenshot 外，每个 call 追加普通 screenshot 成员。

| 函数 | 归一化 → `computer` 参数 | 单位/补充语义 | Backend | 测试 |
|---|---|---|---|---|
| click | Click(left,1) → `mouse_click` | x/y 归一化坐标 | B-C | S-G |
| double_click | Click(left,2) → `double_click` | x/y 归一化坐标 | B-C | 分支；无独立动作 fixture |
| triple_click | Click(left,3) → `triple_click` | x/y 归一化坐标 | B-C | 分支；无独立动作 fixture |
| right_click | Click(right,1) → `mouse_click.button=right` | x/y；Full 权限 | B-C | 分支；无独立动作 fixture |
| middle_click | Click(middle,1) → `mouse_click.button=middle` | x/y；Full 权限 | B-C | 分支；无独立动作 fixture |
| move | Move → `mouse_move` | x/y 归一化坐标 | B-M | 分支；无独立动作 fixture |
| mouse_down | MouseDown(Some(point)) → `left_mouse_down.coordinate` | 先定位再按下 | B-M | 分支；无独立动作 fixture |
| mouse_up | MouseUp(Some(point)) → `left_mouse_up.coordinate` | 先定位再释放 | B-M | 分支；无独立动作 fixture |
| type | Type → `type.text/press_enter` | press_enter 缺省 false | B-T | H-X；Gemini 分支无独立动作 fixture |
| drag_and_drop | Drag → `left_click_drag.path` | start_x/start_y、end_x/end_y 两点保序 | B-M | H-X；Gemini 分支无独立动作 fixture |
| scroll | Scroll → `scroll.pixel_delta` | direction；magnitude_in_pixels `0..=999`，缺省 300；正轴右/下 | B-S | S-G、H-X |
| press_key | Key → `key.keys=[key]` | 单键 | B-K | H-K；Gemini 分支无独立动作 fixture |
| hotkey | Key → `key.keys` | 无损键数组 | B-K | H-K；Gemini 分支无独立动作 fixture |
| key_down | KeyDown → `key_down.text` | 单键、跨调用持有；累计组合键授权 | B-K | H-K；Gemini 分支无独立动作 fixture |
| key_up | KeyUp → `key_up.text` | 单键、释放本所有者持有输入 | B-K | H-K；Gemini 分支无独立动作 fixture |
| wait | Wait → `wait.duration` | seconds 整数 `0..=300`，缺省 1 | B-W | S-G、H-K |
| take_screenshot | Screenshot → `screenshot` | 最终可见图像；不再补重复截图 | B-I | S-G-wire、H-I |

## 授权、持键、取消与几何

原函数扩展支持五类鼠标按钮；key_down/key_up 用 text 表示一个键；key/hold_key 支持 keys 数组且与 text 互斥；drag.path 与旧起终点字段互斥；scroll.pixel_delta 与旧 wheel 字段互斥；type.press_enter 默认 false；点击与滚动的 use_current_cursor 与 coordinate 互斥。wait/hold 的秒单位保持，范围上限 300。最终 schema/参数校验由工具自身负责，审批或 PreToolUse 改过的输入须再次验证。

键盘、侧键、组合鼠标输入覆盖对应 Full 权限。系统快捷键检查包含跨调用累计持键，未知键、非法按钮或超限参数在输入前失败。鼠标 modifier 作用域只释放本动作新增的键；已有持键继续归本 Agent 所有。desktop lock 使用原子排他文件锁，并区分同进程的 Agent 所有者；竞争返回 busy。原生序列、终态观察与持有输入期间保持所有权。

辅助请求使用普通工具历史投影：保留现有合成 `computer` ToolUse、最终 ToolResult 和图像，忽略主轮次的原生绑定、回执、ACK 与 continuation companions。同 Provider 与跨 Provider 的 summarizer 都不能提交主轮次回执；主请求仍严格校验原生来源。Interactions 的普通与辅助请求也从本次捕获的凭证派生账户 scope，但不因此获得 Computer binding 或提交权限。

`type` 的完整文本在输入前检查 Tab 与累计持键形成的系统快捷键，包括文本中间或 backend chunk 开头的 Tab；不能先输入文本前缀再检查。`systemKeyCombos` 已授权或没有受限持键时保留正常 Tab 输入。

子 Agent 在 Completed、Failed、Killed 或 idle 对外发布前等待同一 owner 的输入清理；常驻 Agent 和 Pool 收到终态后立即回收也适用。两个 drag 输入形式都登记成功的鼠标按下；组合键以独立 Press/Release 追踪主键和修饰键。释放失败时保留输入状态与租约，后续由当前 owner 重试清理。

取消会停止未开始的成员；已开始动作完成必要释放，wait、hold 和 request_access 等待可取消；迟到授权结果不再写 grant。清理覆盖本执行器持有的键与鼠标输入；普通进程内 cleanup 不能承诺进程崩溃后的全部设备状态已释放。

清理释放键鼠也是桌面输入：在实际 release 前更新同一 lease 的共享输入版本，并撤销当前与冻结观察，包括部分失败。版本更新失败仍尝试必要的物理释放，但保留租约隔离，直到重试成功发布失效；无持有或 dirty 输入的清理不推进版本。

`computer_frame` 保存模型图像尺寸、capture 尺寸、display ID、origin、scale、crop 与 geometry_version。SDK 先将 Provider 坐标归一到模型图像；ComputerTool 按实际缩放把模型图像坐标换到 capture 像素；macOS `pixel_to_global` 再用 `origin + pixel/scale` 换到系统点。每次输入重查当前显示器几何；切屏、分辨率变化或最终 hook 替图/删图使旧观察失效。screenshot 的图像处理和 metadata 必须一致；当前 zoom 返回裁剪图与 capture_region，并使旧 native frame 失效，不把裁剪图冒充下一轮全屏坐标依据。继续原生输入前需要重新 screenshot。桌面输入版本保存在现有独占租约的锁文件中，并在输入前更新，包括部分失败。观察记录捕获时的版本；其他 Agent、同一桌面的其他 Tool 实例或进程操作后，旧图、已 lower 的输入和新租约中的旧 sequence 均拒绝。冻结版本只在当前 owner 的独占 sequence 内有效；再次截图记录新版本。macOS 的整数系统点采用向下取整，使合法 capture 像素留在对应逻辑点内，避免 Retina 最右/下像素被四舍五入到相邻显示器。真实 Retina、多显示器落点仍待桌面验收。

macOS 的显示器列表、`display_size` 与 `frame_geometry` 共用像素尺寸换算；xcap 的逻辑点尺寸需要乘以 scale。2026-10-07 本机只读检查暴露并修复了 Retina 列表返回逻辑点的问题。该检查只读取权限和显示器几何，不构成实际输入、截图、多显示器落点或 Provider 原生协议验收。

## 普通与原生工具结果发布

普通和原生调用都返回同一个 `DeferredToolDispatch`。现有轮次驱动先通过 generation fence 发布结果元数据与客户端结果事件，再保存逐项结果、读取 MCP `end_turn` 和执行后续 Hooks。原生适配层只负责有序分发与输入租约收尾，避免普通工具提前返回时遗漏 MCP 元数据，也避免原生结果绕过已取消 generation 的发布检查。

`prompt.attachment` 对普通 `Text` 和精确 `TextJsUtf16` 使用既有 Mod UTF-16 投影接口。附加上下文的改写结果和缓存身份均保存原始字符单元，两个显示文本相同的孤立代理字符不能共用缓存结果。

## 唯一 durable ledger 与恢复

`ToolExecutionJournal` 由现有 `SessionStateCoordinator`/`SessionStateManager` 实现，复用原 bounded queue、retention pin、writer lease 和 `DurableJournal::append_once` 的 fsync ACK。没有第二个执行 store。执行 ID 对 session、provider response/call ID 与 member index 作长度分帧哈希；相同成员重试获得 duplicate ACK，不能再授权输入。

```text
响应与旧绑定可靠保存
→ 最终输入校验及审批
→ Started 的 durable ACK（duplicate=false 才可输入）
→ 实际输入 → Terminal
→ hooks/Mods 后最终 StoredOutput 媒体 artifact
→ OutputPrepared
→ 精确普通 JSONL 行 append-once + fsync
→ OutputPublished
→ StoredReceipt artifact → Prepared
→ 精确 receipt 历史行 append-once + fsync
→ Submitted 的 durable ACK → Provider transport
→ 完整后继 Assistant 行可靠发布 ACK
→ ResponseReceived
```

ledger 保存状态、摘要和媒体引用，拒绝内嵌截图 base64；完整最终输出/截图和精确 JSONL 行保存在已有 tool-results 媒体路径。普通 transcript append 不能作为输入前持久化保证。原生可靠发布使用已有 durable append-once API，恢复复用保存的时间戳/UUID/metadata，不能重新序列化成冲突行。

| 恢复记录 | 必须采用的处理 |
|---|---|
| Started，无确定 Terminal | fresh coordinator startup 持久化 OutcomeUnknown；绝不自动重放。live `execution()` 返回真实 Started，不能把正在执行误当崩溃 |
| Terminal，无最终输出 | 保留已成功/失败/拒绝/取消事实；不重跑动作或 hooks；放弃旧 native call 投影，由原 Provider function 路径重新观察 |
| OutputPrepared | 只可靠补发精确 StoredOutput 行并标记 Published；不重跑输入或输出 hook |
| OutputPublished，无 receipt | outcome 确定时从保存的最终输出编码 receipt；不执行动作。即使已有保存的错误 ToolResult，outcome=Unknown 仍放弃旧 native call，由同一 Provider function 重新观察，不能送入回执编码器而制造 CannotResume |
| Prepared | 从 ledger 枚举找回，即使历史 receipt marker 尚未写成功；复用原 StoredReceipt、旧 binding 与精确行 |
| NotSubmitted | 派发回调明确拒绝，确定尚未发送；复用同一回执，以新的 submission_attempt 重提，仍须通过最新 admission |
| Submitted / SubmissionUnknown | fresh startup 持久化 SubmissionUnknown；阻止正常模型请求，不能盲目重建请求、切 Provider 或 failover |
| ResponseReceived | 必须先有完整后继 Assistant 行的 durable ACK，才写入此状态。startup 只有当前历史仍保留该 call binding，且有匹配实际 successor response ID 的 Assistant continuation/ACK 边界时才修复缺失投影；从真实边界恢复旧 scope，更新 transcript ref 优先，冲突拒绝。旧不完整 Received 解释性阻断，不能用 ledger ID 伪造请求续接边界 |
| CannotResume | 编码器无法把最终 hook 可见结果组成合法回执的 durable 终态；保留错误事实与最终输出引用，阻止请求。不能从隐藏缓存取图、补拍或用 function fallback 绕过它 |
| 损坏/遗失 authoritative WAL | fail closed，保留 WAL/snapshot 证据。无法证明坏记录只含 cost；不自动 quarantine 成空 ledger 后重新授权输入 |

`recover_session` 是 owner 重建时的枚举边界，未实现恢复的 journal 明确报错。`execution`/`receipt` 是真实 live lookup，正常轮的 Submitted 保持 Submitted，直到确定响应写入或 fresh startup 恢复。`rehydrate` 从实际 registry、绑定 marker 与 ledger 重建 Work。每轮先在 `recover_before_request` 中完成 owner 恢复或 `refresh_pending`，再准备、渲染和捕获请求历史，覆盖 Prepared ACK 成功后 marker/state append 失败的窗口，保留已有 next_native。`prepare_projection` 使用已准备的有效工具目录，不再追加恢复历史；原生声明预检携带该目录中的全部普通工具，遇到 Gemini desktop 成员名称冲突时回退 function。live refresh 仅修复未完成阶段；已 ResponseReceived 的执行、输出、回执与 ACK 不因消息 ID 缺失而重新加入 history。startup 同样要求当前 call binding 与实际后继 Assistant 边界证明仍需修复；compaction 已移除的完成历史保持移除，ledger 继续保留审计。

Unknown/no-final-output 的 `lingxi_computer_abandoned` 与明确已知/未知执行事实的 UserMeta、Received ACK marker 都是权威 ledger 的确定性内存投影；owner 重建时、下一次请求前重新生成，不引入独立事实 store。只有这些需要重新观察的调用放弃旧 server continuation；保留同一 Provider 的 route 绑定。统一 abandon_calls 使已有保存输出的 Unknown 同样进入这一路径；旧 Unknown 审计记录不能在每次后续 prepare 再次清除新 function 观察和 readiness。SubmittedUnknown 与 CannotResume 必须继续阻断。

live refresh 与冷启动使用相同的 `needs_fresh_observation` 判断：已接受但未 Started，或已 Terminal 但没有最终输出的调用均放弃旧 native 续接，保留执行事实并要求普通 function 重新观察。移除已完成回执的 live 投影前，先保存其 execution IDs，防止内存旧 work 被误判为未开始；不会重放输入或重跑输出 hooks，新的 function 截图不会被旧审计事实反复撤销。

session event encode/decode、语义 fold、snapshot、hydration 与 retention 一并覆盖执行/回执。尚未 Published 的执行和未 Received 的回执保留审计，不能因 coordinator 缓存回收丢失待处理状态。

## 测试与验收命令

以下是针对当前实现的可重复检查入口。Cargo 任务应串行执行，避免共享构建缓存/磁盘并发竞争。本文编辑没有运行 Cargo；最新具体通过结果以本轮根任务输出为准。

基线合并报告记录了早期验证。后续根任务验证结果：SDK 513 项（library 389、integration 124）、llm-runtime 1156 项、ComputerTool 83 项、macOS backend 11 项、native 跨层 13 项、Session 56 项全部通过。测试均使用实际本地 SDK 工作树；默认线程栈下 dispatcher 4 项、普通 MCP 及独立进程取消回归也已通过。这些结果不证明真实 Provider 或可见桌面验收。

```sh
# cwd: /Users/luolingfeng/lingxi/llm-client
cargo test --test computer_normalization
cargo test --test openai_computer_codec
cargo test --test anthropic_client_toolsets
cargo test --test anthropic_toolset_stream
cargo test --test gemini_interactions_codec

# cwd: /Users/luolingfeng/lingxi/harness-runtime
cargo test -p tool-computer-use --lib
cargo test -p platform-macos-computer-control --lib
cargo test -p orchestrator --lib native_computer::tests
cargo test -p orchestrator --lib final_input_validation_tests
cargo test -p llm-runtime --lib
cargo test -p harness-runtime --features desktop --lib desktop::session_state::tool_execution::tests
cargo test -p harness-runtime --features desktop --lib desktop::session_state::tests
cargo check -p llm-runtime
```

必须关联证据的 Harness 回归入口包括：

- `ordered_native_failure_stops_input_and_publishes_ordinary_results`、`native_members_keep_their_position_among_ordinary_calls`：首错停止、普通工具交错顺序与历史。
- `dynamic_scope_denial_removes_declaration_and_rejects_inflight_native_work`：ToolSearch/有效 catalog/动态 deny。
- `native_assistant_durable_write_failure_prevents_all_input_admission`、`duplicate_started_admission_and_recovered_started_never_replay_input`：执行前持久化与重复输入拒绝。
- `post_hook_removed_image_is_absent_from_model_frame_and_receipt`：最终图像可见性与 CannotResume。
- `function_native_function_switch_uses_registered_catalog_and_final_history`：管理→控制→管理投影。
- `fresh_owner_reuses_prepared_receipt_and_published_output_without_input`、`fresh_owner_refuses_uncertain_submitted_receipt_without_input_or_resubmission`：重建与提交不确定。
- `fresh_owner_does_not_append_completed_rows_after_a_retained_successor_boundary`、`old_unknown_call_id_does_not_invalidate_a_new_successful_response_scope`：部分历史裁剪后的重启与跨响应复用 call ID。
- Session 的 live lookup、ACK waiter 取消、Started 丢 ACK/reopen、OutputPrepared 补发布、Prepared/Submitted 恢复、CannotResume、真实 WAL corruption/reopen 与严格拒绝内嵌图像测试。
- `same_process_peer_and_sibling_agent_cannot_interleave_sequence`、`cancelled_access_cannot_commit_late_grants` 与 macOS `retina_secondary_pixels_become_global_points`：所有权、授权取消与几何计算。

逐 Provider 真实验收仍需：完整授权→点击→输入→wheel/pixel 滚动→完整路径拖拽→截图→function 管理轮；保留 action、版本、单位、model/profile/endpoint 与实际可见结果。另需侧键、跨调用持键/组合键、取消释放、Retina 与多显示器落点证据。OpenAI 旧 native 回执加下一轮 function 声明、Claude namespace 与 Gemini 同一 Interactions 历史续接应分别走真实 API。只有满足对应证据的 profile 才能进入 VerifiedComputerProfile allowlist；function fallback 成功或 Unsupported fixture 通过不算对应 native 能力通过。
