# Dataflow 图详细设计

状态：设计。本文不包含实现。已锁定的决定表在
[2026-11-10-dataflow-graph-design.md](2026-11-10-dataflow-graph-design.md)。
两文冲突时以决定表为准。失败与取消的一般规则仍未写。

## 目的

做一个 Rust dataflow 运行时，让一次会话成为一张图。图上有多个 agent、它们的工具执行、外部流和 render。Agent 在运行中用补丁改拓扑。外部数据从命名端口推进来。Render 只消费被显式接上的 decision 流。

第一阶段不替换 `orchestrator` 的 `run_turn`。新 crate 与 `workflow` 并列。它调用现有的 hook、permission、MCP、sandbox、`ToolRegistry` 和 `SkillRegistry`，不重写这些契约。

## 非目标

- 在 `Process` 里改邻接表。
- 用 timely、differential dataflow 或 Hydroflow。
- 把权限门、sandbox 选择和 tool-use 身份做成可热拔插件。
- 第一阶段就让每个成员变成独立进程。进程隔离以后再对现有 team worker。
- 在这份文档里规定节点失败、外部流关闭、agent 取消和 render 断开的总规则。

下面这些已经有规则，不属于上面的缺口：spawn 被 `AgentSpawn` 拒绝、mailbox 满或未知、权限 Deny、正常 revision 切换与退役、Ask 的恢复边界、MCP 断开后不重放已发出的调用。

## 现有 harness 落在哪里

今天的 agent 执行是过程，不是图。`run_turn` 拿住 turn 锁，组装 prompt，调模型，执行 `tool_calls`，把结果写回 transcript。外部帧要进下一轮，只能先折进 prompt。

旁边的 crate 不拿来改装：

| 现有模块 | 它实际是什么 |
| --- | --- |
| `crates/workflow` | 脚本化的多 agent 进度，phase 和 agent start/done |
| `crates/fusion` | 多方案并行评审 |
| `media-runtime` | 附件和音频路由，不是 render 图 |
| `crates/coordinator` | `spawn_worker`、按 id 和名字寻址的 mailbox、有界 inbox、团队成员 |
| `crates/hooks` | `HookEventType` 分类法。同步 hook 按优先级折叠，第一个 `Block` 粘住 |
| `crates/permission` | `PermissionPolicy` 规则引擎、`PolicyPermissionGate` 的完整裁决链和 `PermissionMode` |
| `crates/mcp` | 连接、鉴权、掉线的工具、elicitation。连接不属于图 |
| `crates/sandbox` | `SandboxDecision`：`NoSandbox` 或 `Sandbox { policy }` |
| `crates/tool-api` | `ToolRegistry::available_tools`，一次调用一个 `ToolUseContext` |
| `crates/skill` | `SkillRegistry`。`Skill` 工具加载正文，或 `context: fork` 生成子 agent |

图只决定这些模块的包从哪条边过。包的含义仍由原模块解释。

## 外部对照

### MediaPipe

本地 checkout 是 `google-ai-edge/mediapipe` 的 `3b37422b8`。数据面借它的运行时，不借它的冻结拓扑。

借来的部分：

- 边是命名端口。一个 `OutputStreamManager` 对下游做 mirror。Packet 不可变。前面的下游拿拷贝，最后一个把队列移走。见 `output_stream_manager.cc` 的 `PropagateUpdatesToMirrors`。
- 时间戳 `t` 可以往下走，当至少一条输入在 `t` 有包，其余输入要么也有 `t`，要么下界已经越过 `t`。默认实现是 `DefaultInputStreamHandler` 把就绪判断交给 `sync_set_`。
- 节点生命周期是 `Open` / `Process` / `Close`。`max_in_flight` 默认 1。输入队列有上限，满了节流上游。
- 关闭输入把下界推到 done。
- 图输入是虚拟源。外面 `push` 相当于 `AddPacketToInputStream`。

不借的部分：`CalculatorGraph::Initialize` 之后 mirror 不再改。要改拓扑就重建整张图，在飞的包和外部订阅会断。模型调用如果占着和帧相同的时间戳前沿、又不发下界，render 会一直等。第一张图禁止这个形状。

### LangGraph

LangGraph 是状态图，不是 packet 流。节点读写一份共享 state。边可以是静态的，也可以用 `Send` 在这一步把自定义 state 投给某个节点。人在回路用 `interrupt()`：必须有 checkpointer 和稳定的 `thread_id`，暂停时把状态写进去，调用方以后用 `Command(resume)` 继续。官方说明写明，恢复会从该节点开头重跑。中断中的线程可以改大部分拓扑，但不能删掉或重命名它正要进入的节点。

借来的是显式暂停点。第一阶段必须支持进程内的 permission Ask、模型切换 Ask 和 elicitation 唤醒，决定送回原 invocation。跨进程恢复是单独的可选能力，只有满足下文的 continuation 和持久化日志契约才能启用；记录拓扑、Bind 和一条 Ask 本身不足以恢复调用。模型切换使用自己的 operation id，不伪造 tool-use id。

不借的是恢复语义。Executor 已经记下 dispatched 并发出外部调用之后，不能因为恢复而从节点开头再跑一遍。LangGraph 的共享 state 也不采用。副作用包携带 `agent id`、revision 和 tool-use id，不写进一张谁都能改的状态表。

### DeepSeek Harness 和 Cordis

[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)（`dsh`）是模型外面的运行时，不是模型。官方架构说明：它建在 Cordis 上，everything is a plugin。模型适配器、工具注册、会话日志、agent loop、sandbox 和审批都是插件，没有一块特权核心。Profile 在启动时按层叠 bundle。`dsh-base` 是 web、headless、sdk、acp 共用的第一层。

Cordis 的五条和本设计有关：

- 插件通过 `inject` 声明要等哪些服务出现，而不是手写启动顺序。
- 服务挂在 context 的稳定 key 上，调用方不 import 具体实现。
- 注册是可逆 effect，插件卸下时撤销。
- 事件有五种分发。`emit` 和 `waterfall` 按注册顺序观察，不等待。`parallel` 并行等待。`serial` 按注册顺序等待。`bail` 按顺序走到第一个截停值。
- 文档站：[deepseek-harness.github.io/deepseek-harness](https://deepseek-harness.github.io/deepseek-harness/)。Cordis 简介在仓库 `docs/cordis-primer.md`。

这五种分发正好给已经定下的投递规则起名字，不引入第二套调度：

| 我们的规则 | 对应的 Cordis 分发 |
| --- | --- |
| Mirror 把包按订阅顺序放进各队列，不调用 `Process` | `emit`：按顺序观察，不等待执行 |
| 阻塞 hook 按优先级串行等待 | `serial` |
| `blocking == false` 的 hook 后台跑，不进入裁决 | `parallel`，且排除在聚合之外 |
| 第一个 `Block` 成为裁决，后面的阻塞 hook 仍跑副作用 | 裁决取 `bail` 的第一个截停；副作用仍按 `serial` 跑完 |
| Mailbox 只投给一个地址，满了向发送方报错 | 不是扇出 |

不借的是「agent loop 本身可在运行中卸下」。权限门、sandbox 策略和一次 tool use 的身份必须活过整个 revision。插件可以换模型适配器或加一个观察者。它不能在一次调用中途把权限门卸掉。

### dsh-superagent

[dsh-superagent-prototype](https://github.com/deepseek-ai/dsh-superagent-prototype) 是 DeepSeek Harness 团队的研究原型，README 写明不是正式产品，也和文章里的未来设计不完全一致。它是一个仓库上的 bundle：router 是长期会话，负责对人、授权和派发；每张 ticket 一个 worker 会话，有自己的目录，通常是 git worktree。Worker 不直接对人说话，状态回到 router。人同时能跟上的活动会话被他们写成大约 3 到 5 个。

借来的是职责切分。对人的出口只有接上 render join 的那条 decision 流。其他成员的决策和 mailbox 不进 render，所以一个闲着的 teammate 不会把帧卡住，也不会每个人各说各话。

不借的是第一阶段的进程模型。第一阶段一张进程内的集群图。每个成员已经带 `agent id`。以后若要目录隔离，再映射到现有 team worker，而不是在图运行时里再做一个进程管理器。

### 取舍

| 问题 | MediaPipe | LangGraph | DeepSeek Harness | 本设计 |
| --- | --- | --- | --- | --- |
| 数据 | 带时间戳的不可变 packet | 共享 state | 插件 context 里的服务 | Packet 加这一版的 Bind |
| 改图 | 重建 | 中断中限制删节点 | 卸插件并撤销 effect | 调用之间应用 patch，生成下一版 revision |
| 多接收者 | Mirror，任一满队列节流上游 | 由 reducer 和 `Send` 决定 | `emit` / `parallel` / `serial` / `bail` | 按接收者策略。只有 wait 节流发送方 |
| 控制流 | Calculator 子图，拓扑静态 | 条件边和 `Send` | 插件里的 orchestrate 工具 | 基础执行、串行、并行、条件、map、有界循环。嵌套不改 revision |
| 人在回路 | 没有一等公民 | `interrupt` 后从节点开头重跑 | 审批是插件 | Ask 暂停这一次调用，不重跑已发出的副作用 |
| 多 agent | 子图，仍是静态 | 动态 `Send` | superagent 的 router / worker 会话 | 一张图里的成员，mailbox 点对点 |
| 工具和 skill | 不是这个领域 | 工具是节点或中间件 | 工具是插件注册的服务 | 工具池是 Bind。一个成员一个 executor |

## 运行时

两面。

数据面的每份 revision 都不可变。当前版接纳新入口，尚未退役的旧版只完成已准入工作。调度、mirror 和背压按上面的 MediaPipe 规则。节点拿不到 `&mut Graph`。

控制面是一条保留的 patch 事件。Agent 发出带 `patch_id` 和 `base_revision` 的 `GraphPatch`。调度器串行提交，在调用准入边界原子发布下一版；不等待全图模型调用结束。已经准入的调用保留旧版。生成 agent 的 patch 在 `AgentSpawn` 返回、拓扑与端口校验通过、新节点 `Open` 成功之前不发布。过期 `base_revision` 返回冲突，不自动重放 hook 或重基 patch；同一 `patch_id` 重复提交只返回原提交结果。

### Revision 的切换与退役

拓扑和 Bind 的更新都走同一个提交序列。仅更新 Bind 也会生成新 revision，不需要伪造拓扑 patch。准备失败保留当前 revision，并关闭本次尚未发布的节点；`SubagentStart` 只在发布后触发。保持不变的节点实例不重复 `Open`，同一 `AgentId` 的模型并发计数和会话状态跨 revision 共用。

每个 invocation 携带 `invocation_id`、`parent_invocation_id`、`agent_id`、`revision`；工具另带 `tool_use_id`。控制节点的分支、map 元素和循环圈次属于 invocation 的 continuation。它们的结果按父调用和分支索引归并，不能只靠时间戳或节点名寻址。

提交 `r → r+1` 时遵守以下规则：

1. 普通外部输入和新的根调用转到 `r+1`。旧版停止接收新的普通入口；提交前已准入的队列项、在飞调用及其 continuation 仍由旧版调度。旧工具结果、Ask 答复按原调用身份定向投递到 `r`，不按当前拓扑重新寻址。
2. 旧版只完成已准入工作及其 continuation，不再启动新的自主模型轮次。工具结果的落盘、hook 和父调用收齐仍能运行。保留的 agent 将完成结果送入自身稳定 inbox，下一次独立模型请求使用当前 revision；移除的 agent 只完成结果收集和终态记录。
3. 每个 render join 的帧时钟有切换点 `c`：它是提交前最后一个已准入帧时间戳的后继。旧版不再接纳 `t >= c` 的帧，新版只接纳 `t >= c` 的帧。包携带 render epoch，join 和 egress 都校验 epoch；发布新版后，旧 epoch 未发出的 render 包和晚到 decision 按 Drop 结束，不能修改新版的 decision 缓存。工具结果和 mailbox 不套用这条丢帧规则。
4. 只有 decision 来源节点、端口身份和 join 策略都没变，才把提交时已经发布的上一份 decision 迁移到新 join。换来源或同步策略时清空缓存，等新来源的第一份 decision；不隐式沿用 A 的决定给 B。旧调用在切换后完成的 decision 只保留其调用结果，不补进新 join。
5. 移除节点的 `Close` 等到旧版在飞调用、已准入队列、父调用结果收集、Ask 和绑定资源引用全部释放后执行一次，随后触发 `SubagentStop`。有未完成 Ask 时保留旧版；强制取消与 drain 超时留给失败与取消协议。删除节点不等于丢弃已发出的 tool use。

这些规则使“旧调用跑完”包括必要的结果投递与收集，而不只是让它的 Rust future 返回。新 revision 不共享可变的邻接表，也不把旧包改成新的 tool-use id。

### Ask 与恢复

进程内暂停保留原 future 和 continuation。询问键至少包含 `(session_id, agent_id, revision, invocation_id, ask_id, generation)`，工具询问还带 `tool_use_id`。答复只消费一次；过期、重复或身份不匹配的答复不触发执行。permission Ask、模型切换 Ask、MCP elicitation 分别记录暂停阶段，不能互相当作另一阶段的批准。

可选的跨进程恢复必须保存以下数据，否则宿主应拒绝启用恢复能力：

- 当前及尚未退役各 revision 的可重建拓扑和版本化 Bind 描述、入口归属、会话历史游标及节点身份；宿主重新解析服务句柄，不能序列化 socket、trait object 或活 sandbox 进程冒充恢复。
- 每个 invocation 的输入、执行阶段、父调用、串行下一步索引、条件选择、map/并行已完成结果、循环圈次，以及未完成 Ask。已经完成的子步骤只能读取结果，不能重跑。
- 持久化调用日志和结果投递状态，状态至少区分 `waiting`、`ready`、`dispatched`、`completed`、`outcome_unknown`。`dispatched` 必须在外部发送前持久化；终态结果先持久化再以原 id 幂等投递。Ask 答复消费与调用阶段更新一起提交。

检查点包含日志游标；恢复时先折叠后续日志，不能只读取旧检查点中的 Ask。只有日志证明尚未 dispatched 的暂停步骤可以继续，且要重新绑定宿主服务并通过权限与能力校验。恢复时发现 dispatched 而无持久化结果，一律记为 `outcome_unknown`，不自动重发；这包括“已记 dispatched 但实际发送前退出”的不确定窗口。不确定结果结束该步骤并交给父 continuation，不得作为成功启动依赖它的后续副作用；其他并行分支的取消仍属后续设计。本设计保证不自动重放，不承诺副作用 exactly-once。

例如串行 `S(A, B)` 中 A 已完成、B 等权限，恢复只继续 B，并复用 A 的已记录结果。MCP elicitation 则依赖已发出请求和活连接：第一阶段仅支持进程内唤醒；重启或连接丢失后没有服务端恢复协议时，该调用转为 `outcome_unknown`，关闭原 Ask，不再接受其迟到批准。不能用重发 MCP 请求来实现“恢复”。

### 端口

| 种类 | 对齐 | 用途 |
| --- | --- | --- |
| Stream | 时间戳合流 | 外部帧、接到 render 的 decision、render 输入 |
| Event | 只排队 | 工具请求和结果、mailbox、patch、hook 和权限询问 |
| Bind | 这一版的常量 | 模型、权限策略、sandbox 策略、工具池、skill 名单 |

### 集群

一个会话是一张集群图。成员是带 `AgentId` 的 agent 节点。这和现有 coordinator 对齐：`spawn_worker`、按 id 和名字索引的 mailbox、有界 inbox、团队成员。

Mailbox 是点名的 Event，不是 mirror。发送方给出 id 或队友名字。关闭、已满或未知 id 作为错误事件回到发送方。广播是一次显式发送，不是沿着所有边扇出。

```text
cluster
  ingress frames -----------------------------+
  agent A -- decision (Stream) --------------+-- join -- render
  agent A <-- mailbox Event --> agent B
  agent B -- decision (Stream)
       只有这条流被接到 join 时才进入上面的合流
  每个 agent
    prompt / tool Event
      -> hooks -> permission gate -> executor -> hooks -> agent
    executor 跑在这个 agent 的 sandbox scope 里
    MCP 工具和本地工具走同一条边
```

只有显式接到 render join 的流影响帧。Mailbox 和其他 agent 的 decision 留在 Event 上。安静的队友不卡住 render。第一段库内代码可以只有一个成员，但每个副作用包已经带 `agent id`、revision 和 tool-use id。第二个成员是同一张图上的另一个节点。

Spawn 是 patch。提交前 `AgentSpawn` 可以拒绝，或改写 agent 类型、模型、cwd、是否后台。`SubagentStart` 在新节点 `Open` 成功并发布 revision 之后。`SubagentStop` 在退役后的 `Close` 时。`TeammateIdle` 在该 agent 的队列空闲时。

继承只包括 spawn 上写明的 prompt、model、cwd 和 plan mode。子成员不共享父成员正在飞的工具队列、权限询问或活着的 sandbox 进程。子成员的 tool use 是新 id。

### 一个成员

四节点草图是一个成员的内部，不是整个系统。

```text
external --frames (Stream)--+
                            +-- join -- frame+decision -- render
agent ----- decision (Stream)
  | tool request (Event)
  v
hooks -> permission gate -> executor -> hooks
  |                              |
  |                              +-- 这个 agent 的 sandbox scope
  |                              +-- 本地工具或 MCP 调用
  v
tool result (Event) --> agent
agent -- GraphPatch (Event) --> controller
```

- **Ingress。** 调用方 `push(port, packet)`。每个端口自己的溢出策略。Render 满了丢帧。工具结果满了就等。慢的 render 不挡工具，慢的工具也不改 render 的丢帧策略。Mailbox 满了是给发送方的错误，不是丢掉的帧。
- **Agent。** `max_in_flight` 为 1，限制模型 `Process`，不限制运行时处理帧准入、完成事件和下界。输入是工具结果、mailbox 和它选择要读的帧。调用完成后提交 decision 候选；运行时按“合流”一节赋予生效时间戳。工具请求、mailbox 发送和 patch 仍是 Event。
- **Executor。** 图中工具副作用的唯一执行节点。发出外部调用之前先记下 dispatched；开启跨进程恢复时必须持久化。结果不明不自动重发。调用保持准入 revision 的权限、sandbox 和工具绑定；这些绑定需要宿主的 revision 适配器，不能直接复用可变的全局对象。新 revision 不把同一个 tool use 再跑一遍。
- **Render。** Egress 订阅者。图不认识像素。它把合流后的包交给调用方。

Controller 是调度器应用 patch 的动作，不是数据节点。

### 控制节点

控制节点把一次调用编成若干子步骤。它们不是第二条副作用路径。子步骤若要碰外部世界，仍走 hook、权限门和 executor，并使用新的 tool-use id。已经记下 dispatched 的调用不会因为串行的下一步、并行的另一支或循环的下一圈而重放。

叶子是基础执行节点。Agent 和工具 executor 是这个叶子的两种特化。并行、串行、条件和 map 是父节点，子节点在当前 revision 里已经存在。父节点不在 `Process` 里加节点。要增减成员仍走 patch。

| 节点 | 何时开始子步骤 | 挡住谁 | 输出 |
| --- | --- | --- | --- |
| 基础执行 | 自己的输入就绪 | 只挡住自己的下一次 `Process`（`max_in_flight` 默认 1） | 这一次 `Process` 的输出包 |
| 串行 | 前一个子步骤已经返回并投递 | 挡住列表里后面的兄弟。不挡住图上无关的节点 | 最后一个子步骤的输出 |
| 并行 | 父输入就绪后同时调度全部分支 | 父节点的输出等到每一支都给出这一次调用的结果。分支之间不排序 | 按分支声明顺序收好的一组结果 |
| 条件 | 谓词先算完，再调度恰好一条臂 | 未选中的臂不运行。父节点给那些边写下界，下游不等待 | 被选中那一臂的输出 |
| Map | 每个元素一支，受并发上限约束 | 与并行相同，但是元素个数由输入包决定 | 按输入顺序收好的一组结果 |
| 有界循环 | 上一圈返回之后才开始下一圈 | 挡住下一圈。每一圈是新的调用 id | 最后一圈的输出，或达到上界时的输出 |

基础执行节点的契约是 `Open` / `Process` / `Close`。一次就绪的输入对应一次 `Process`。它没有子节点。它若输出接到 render 的 decision，就提交 decision 候选，由运行时的输出排序器推进下界；不在下界已经越过输入时间戳后补发同时间戳的输出。

串行节点持有一份有序列表。第 k+1 个子步骤拿到的输入是第 k 个的输出包。第 k 个还没返回时，后面的不调度。这条等待不改 render 的丢帧策略。串行、并行和 map 父节点的 decision 输出也经过同一个输出排序器；父节点等待期间由运行时推进下界，不能提前发布尚未收齐的分支结果。

并行节点不是 mirror 扇出。扇出把一份包交给若干互不相关的接收者，然后它们各跑各的。并行节点是一次调用启动一组分支，再收成一个包。分支可以重叠，不规定谁先 `Process` 完。父节点在收齐之前不向自己的输出投递。某一支的队列若是等策略，只按多接收者的规则堵住它自己的发送方，不给其他分支排队。子步骤全是工具调用时，这一次收齐之后走 `PostToolBatch`。某一支失败时其余支是否取消，仍属于未写的失败规则。成功路径是等齐。

条件节点先读输入包，选出恰好一条臂。谓词不做副作用。需要工具结果时，先跑一个基础执行或工具调用，条件只看那个结果包。未选中的臂不 `Process`。父节点在那些边上把下界推过这次时间戳，下游不会因为缺包而干等。多路选择是同一个条件节点的多条臂。

Map 是并行的动态形式。输入包里有一组元素，每个元素一次叶子调用。图不为每个元素添加节点。并发度是这个 map 节点的 `max_in_flight`。结果按输入顺序收齐，不按完成顺序。

循环必须带上界。没有上界的循环不在第一阶段。每一圈使用新的调用 id。上一圈已经 dispatched 的 tool use 不在下一圈重放。下一圈的输入只是上一圈的结果包。

这些父节点可以嵌套：并行的一支可以是串行，串行的一步可以是条件。嵌套仍在同一版 revision 里。它不生成 patch。

### 多接收者

把包放上输出，和运行接收者，是两步。投递填输入队列。`Process` 在那之后按接收者独立调度。

投递按这一版的接收者列表从前到后。除最后一个以外都拿拷贝，最后一个拿走队列。顺序是订阅者加入的顺序，patch 追加到末尾。投递不调用 `Process`，也不等接收者跑完它已经排队的工作。

投递之后，接收者互不相等。调度器可以按任何顺序跑它们，也可以在不同执行队列上重叠。扇出不规定 `Process` 顺序。下游 join 等的是时间戳。并行节点和 map 等的是自己这次调用的分支结果。两者都不等接收者的身份，也不互相代替。

背压按接收者：

| 策略 | 队列满 | 对发送方 | 对其他接收者 |
| --- | --- | --- | --- |
| 等 | 这条队列有空位之前，发送方不再被调度 | 这个接收者挡住发送方 | 其他接收者继续跑已经拿到的包。发送方恢复之前，它们拿不到新包 |
| 丢 | 按这个接收者自己的规则丢（render 丢帧） | 不因为这个接收者而停 | 不受影响 |
| 报错 | Mailbox 已满、已关闭或 id 未知 | 发送方收到错误事件 | 只有一个点名接收者 |

等策略的接收者可以堵住发送方，并因此让别人拿不到后续的新包。它不能打断已经拿着包的兄弟。丢策略不参加这个堵塞。Render 保持丢。工具结果和权限询问保持等。Mailbox 保持报错。

同一条边上的多个 hook 不是扇出，是一条链。匹配的 hook 按优先级从高到低跑；优先级相同则保持 registry 里 `sort_by` 稳定排序之前的顺序。每个阻塞 hook 都要等它返回。第一个 `Block` 成为裁决，后面的阻塞 hook 仍跑，以便留下副作用。链返回之后，包才进入唯一的下游。`blocking == false` 的 hook 放到后台，不进入聚合，也不能挡住这条边。

一个 ingress 上的两个订阅者是上面的两个接收者，不是两个 hook。同一条 decision 既接 render 又接另一个 agent 时，render 丢，agent 等。堵住生产者的是 agent 的满队列。

### 合流

每个 join 声明唯一的帧时钟域。第一阶段使用可比较、可取后继的逻辑时间戳；同一 Stream 的时间戳严格递增，低于已发布下界的包被拒绝。下界 `b` 表示以后不会再出现 `timestamp < b` 的包。物理时间由 ingress 适配为这个时钟域，溢出返回错误，不能回绕。不同设备时钟不会直接合流。

Decision 分开保存 `source_timestamp`（输入依据，可为空）和 `effective_at`（在 render 上开始生效的帧时间戳）。它是可沿用的状态，不是必须逐条播放的事件。完成时间晚于来源帧时，不回写来源帧。

运行时为每条接到 join 的 decision 输出维护轻量排序器。它串行处理帧准入、decision 完成和 revision 切换，不占 agent 的模型执行槽。每次接纳帧 `t`：

1. 读取已经进入排序器、属于当前 revision 的 decision 候选。同一帧前完成多次时，只取接收序号最新的一份，其余仅保留调用结果；工具结果和 mailbox 不合并。
2. 有候选就以 `effective_at=t` 入队一个 decision 包。随后把下界推进到 `successor(t)`，再向 join 暴露这一帧；无候选也推进下界。包入队和下界更新对 join 原子可见，不能先见下界、后见同时间戳的包。
3. Join 在 `t` 有 decision 时更新缓存；否则沿用同一来源的上一份。没有任何 decision 时直接消费并丢弃这一帧，不积攒等首个模型结果。新 decision 只从下一次帧准入开始生效。

例如模型基于帧 10 开始，帧 11–19 在模型完成前到达，它们继续使用旧 decision。完成候选在帧 20 准入前被排序器接收，则 `source_timestamp=10`、`effective_at=20`。不能再发 `effective_at=10`。即使 agent 不读取后续帧、在等工具或 Ask，排序器仍处理这些帧的进度。

一般 Stream join 仍遵守“在 `t` 有包或下界越过 `t`”的就绪规则；缺包且没有下界就等待。上述排序器是 render decision 的专用适配器，不替普通工具结果流虚构完成。多条 decision 显式接到一个 join 时各自维护下界和缓存，并由 join 声明如何组合；第一阶段默认只接一条。

### 上下文、模式和模型

上下文注入、权限模式、会话剖面和模型是四条轴。它们都不扇出，也不加节点。

上下文只有一个接收者：下一次模型调用所属的那个 agent。图不重拼 prompt。宿主现有的组装器仍构建那次请求。图只决定每一段走哪条通道，并且不把这条通道 mirror 到 render、mailbox 或其他 agent。第二个 agent 只有在发送方点名再送一份时才看得到。

| 通道 | 上面走什么 | 何时生效 |
| --- | --- | --- |
| Bind | 系统提示和指令块、`ConversationMode`、工具池、skill 名单、权限模式、模型 | 下一版 revision。已经在飞的调用保持出发时的 Bind |
| Meta Event | hook 的 `additionalContext`、inline skill 正文、技能发现提醒、记忆提醒、`PostModelSwitch` 提醒、环境和 token 提醒 | 作为 meta 消息排队给这个 agent。策略是等。不是帧 |
| Transcript 改写 | 压缩 | 两次 revision 之间，`PreCompact` 之后、`PostCompact` 之前。只改这一个 agent 的历史 |

Meta 队列满时对投递方背压；agent 仍可排空队列，不能因为队列满而禁止消费。下一次模型读取吸收已入队上下文。这不占输出排序器的执行槽，agent 吸收上下文或等待切换 hook 时，join 继续沿用上一份 decision。

`ConversationMode`（`chat` 或 `code`）是会话创建时固定的剖面。它选择指令块。它不改工具门，也不改模型。若宿主以后允许中途改剖面，那是下一版 Bind 里的指令块，不是 patch。

权限模式包括 `Default`、`Plan`、`AcceptEdits`、`BypassPermissions`、`DontAsk`、`Auto`，以及内部的 `Bubble`。`EnterPlanMode` 和 `ExitPlanMode` 走现有工具与 `PermissionGate` 模式切换接口，宿主把结果发布为下一版 Bind，不直接修改所有调用共享的 `PermissionPolicy`。Plan 的读取默认放行、写入询问、显式规则和已有 bypass 能力仍由现有 gate 判断，不能简化为绝对只读。模式不拆 executor，也不换模型。已准入旧 revision 的调用（含排队、待 Ask 和已放行调用）保持旧绑定；新准入使用新绑定，强制撤销另属取消协议。`Bubble` 仍向父成员问，答复带原 Ask 身份。

模型切换按 agent。`command`、`picker`、`sdk` 在模型 Bind 改变之前跑阻塞的 `PreModelSwitch`。Hook 链的规则适用：优先级顺序，第一个 `Block` 粘住，询问只用这个 agent 的决定口，并且只暂停这次切换。`auto` 和 `resume` 不跑 `PreModelSwitch`。Bind 改变之后跑 `PostModelSwitch`，不能把模型改回去。它的 `additionalContext` 变成下一次模型请求的一条 meta 消息。父成员切换不改变子成员。子成员从 spawn patch 或 skill frontmatter 取得模型，直到它自己再切。

### 工具

一个成员一个 executor。工具是池里的名字，不是节点。`ToolRegistry::available_tools` 仍组装这个池：先是启用的内置工具，然后是 MCP、LSP 和插件工具。名字冲突时内置工具赢。会话 allow-list 照旧适用。这一版的池是 Bind。名字不在 Bind 里的调用，在 hook 和权限门之前失败。

一轮里的多个 tool use 是同一个 executor 的多次调用，各自有 tool-use id。Agent 保持 `max_in_flight` 1。Executor 可以并行跑一批。每个工具的 `PostToolUse` 之后、下一次模型请求之前，`PostToolBatch` 触发一次。新 revision 不把这些 id 变成新节点。

工具的 `is_enabled`、MCP 或插件描述符的来去、会话工具 allow-list 更新下一版工具池 Bind。Skill 的 `allowed-tools` / `disallowed-tools` 是权限规则，交给现有权限层处理，不能当成同一个名字白名单。`allowed-tools` 可增加 `command` 来源的 Allow 规则，`disallowed-tools` 增加 Deny 规则；`Bash(git commit:*)` 这样的内容匹配必须保留。工具是否可广告、某次具体调用是否获准是分别计算的，所有存活调用仍经过完整 gate。

现有规则折叠见 [permission layers](../../crates/permission/src/layers.rs)，内嵌 shell 的独立有效策略见 [prompt_shell](../../crates/tools/skill/src/prompt_shell.rs)。

工具结果里的媒体留在 tool-result Event 上，不 mirror 到 render。Render 只有在接上的 decision 自己带上它，或 patch 把另一条流接到 join 时，才看得到。

延迟的工具搜索仍是对 `ToolRegistry` 的一次工具调用。它改变以后某一版 Bind 里模型看得到的集合。它不加 executor。

### Skills

Skill 是 `SkillRegistry` 里的 `Skill`：markdown 的 frontmatter 和正文，或 `skill_from_mcp_tool` 从 MCP 工具造出来的 skill。Registry 是宿主状态，和 MCP 连接一样。名字和描述作为名单进入 prompt。名单是 Bind。加载 skill 不加节点。

`Skill` 工具是工具池里的一项。模型调用和用户斜杠命令保留来源标记，并复用宿主各自的入口校验。两个 frontmatter 标志作用不同：

- `disable-model-invocation: true` 禁止模型通过 `Skill` 调用，不禁止宿主允许的用户手动调用。
- `user-invocable: false` 从用户斜杠菜单隐藏，不禁止模型调用。用户精确输入名称时是否可执行，仍按现有 dispatcher 校验，图不另设拒绝。
- 通过入口校验的模型调用继续走 hook、权限和 executor；用户入口不绕开宿主既有的权限与 shell 展开检查。

入口投影以 [CommandRegistry](../../crates/command-api/src/registry.rs) 的 `model_invocable_commands` 与 `palette_commands` 为准，执行和 fork 作用域以 [SkillTool](../../crates/tools/skill/src/skill.rs) 为准。

Executor 按 `context` 处理：

- `inline`，默认。结果是注入这个 agent 上下文的正文。宿主按现有作用域折叠权限层，并将变化发布到后续调用的 Bind：内嵌 shell 展开为本次展开建立带 `allowed-tools` 的有效策略；inline `disallowed-tools` 与当前用户输入的 deny 规则取并集，下一条用户输入替换这个输入级规则。不能把前者扩成永久授权，也不能把后者丢掉内容匹配后变成名字过滤。这个 skill 自己的 `hooks` 仍按宿主调用作用域注册，不替换会话的 hook 分类法。
- `fork`。结果是 spawn patch，带上 skill 的 `agent`、`model` 和 `background`。提交前仍走 `AgentSpawn`。子成员的工具池复用现有 agent 定义和父成员可广告工具的限制；skill 权限规则、冻结的父 command deny 和 `additional_disallowed_tools` 按现有 fork 路径传递，不能用 skill 的 `allowed-tools` 名字求交来代替。子成员不共享父成员正在飞的那批工具调用。

带 `paths` 的 skill 在路径命中之前不进名单。激活对这次会话是单向的，更新下一版 Bind。后来的不匹配不撤回模型已经看见的 skill。`FileChanged` 可以喂这个检查。它本身不创建节点。

Hook 结果上的 `reload_skills` 把 `SkillRegistry` 刷进下一版名单 Bind。`SkillDiscoveryPrefetch` 可以在 Event 面上放一条 system reminder。除非打开 `EXPERIMENTAL_SKILL_SEARCH`，它是关的，并且它不占住 render join。

MCP 断开时，`loaded_from` 属于那个连接的 skill 和那个服务器的工具描述符一起在下一版 Bind 去掉。

### Hook

Hook 保持 `HookEventType`。它们是边上和生命周期上的拦截器。Agent 运行中不加 hook 节点。Hook 拿不到 `&mut Graph`。

能改包的同步 hook 在下游节点被调度之前、在同一版 revision 上跑：

| 位置 | Hook | 对包的作用 |
| --- | --- | --- |
| Prompt 进入 agent | `UserPromptSubmit`、`UserPromptExpansion` | 改写或拦截。展开包括斜杠命令和 MCP prompt |
| 工具请求离开 agent | `PreToolUse` | 拦截、改输入或注释 |
| 工具结果返回 | `PostToolUse`、`PostToolUseFailure`、`PostToolBatch` | 在 agent 看见之前增补或后处理 |
| Spawn patch | `AgentSpawn`，然后 `SubagentStart` / `SubagentStop` | 提交前拒绝或改写；start 和 stop 观察节点 |
| 权限门 | `PermissionRequest`、`PermissionDenied` | 观察或参与询问 |
| 模型变化 | `PreModelSwitch`、`PostModelSwitch` | 允许、询问或拦截切换 |
| 成员空闲 | `TeammateIdle` | 观察 |

Hook 若要另一个 agent，它发出 spawn 请求。该请求仍过 `AgentSpawn`。Hook 可以通过 `HookMcpInvoker` 调 MCP。那次调用是带自己 tool-use id 的 executor 副作用，不绕过权限门。

异步 hook 走 Event 面。它们不占帧的时间戳前沿。同步 hook 还没返回时，那条边等待。Join 不会编造一份 decision 来掩盖这个等待。

`SessionStart`、`SessionEnd`、`Setup`、`Stop`、`PreCompact`、`PostCompact`、`ConfigChange`、`CwdChanged`、`FileChanged`、`WorktreeCreate`、`WorktreeRemove`、`InstructionsLoaded`、`Notification`、`TaskCreated`、`TaskCompleted`，以及分类法里的其余事件，仍在原有观察点触发。图发出它们，不替换它们的 payload。

### Permission

权限是工具边上的门。Bind 保存这一版的策略描述；宿主提供 revision 绑定的 `PermissionGate` 适配器，保留 `PermissionCheckContext`、权限层、分类器、决策来源和询问 transport。`PermissionPolicy::authorize` 只是其中的规则引擎，不能用裸三分支替换 `resolve_detailed_or_abort` 等现有完整裁决路径。直接共享会变化的 live gate 不满足旧 revision 绑定，需要先完成这个适配。

- Allow：包继续到 executor，保留 gate 返回的有效输入和授权元数据。
- Deny：产生本次 tool use 的拒绝结果。只有 `PermissionDecisionSource::Classifier` 的拒绝触发 `PermissionDenied` hook；规则拒绝、模式拒绝、用户拒绝不扩大到这个 hook。拒绝通知与 `PermissionDenied` hook 分开处理。Hook 的 `retry` 仍按现有特性开关转成后续 meta 提醒，不在 executor 里自动重试。
- Ask：在调用询问 transport 前触发 `PermissionRequest`，先应用 hook 的合法决定；仍需外部决定时才登记本次 Ask 并开放决定口。不能先弹出询问、再让 hook 作出本可免除询问的决定。

完整门的实现见 [PolicyPermissionGate](../../crates/permission/src/policy_gate.rs)；hook 来源条件和 retry 提醒见 [tool_dispatch](../../crates/orchestrator/src/turn_loop/tool_dispatch.rs)。

决定可以在宿主里更新会话允许名单。更新出现在下一版 Bind。它不改写已经发出调用的那一版。`dangerously_disable_sandbox` 是下面 sandbox 选择的权限输入，不是图编辑。

### MCP

MCP 连接留在宿主：套接字、鉴权、掉线的工具、服务器角色。图不拥有连接。已连接的服务器把工具描述符放进 agent 的 Bind。调用和一个本地工具同一条边：hook、权限门、然后 executor。Executor 执行 MCP 调用。

`Elicitation` 和 `ElicitationResult` 是这次调用上的第二个询问口，发生在 MCP 请求已经 dispatched 之后。进程内答复回到原请求与连接，不暂停其他 agent，也不暂停 render join。跨进程、连接丢失后的处理遵守“Ask 与恢复”一节，不能靠重发工具调用恢复。

断开在下一版 revision 去掉该服务器的描述符。在飞的调用在发出它的那一版上结束或失败。不重放到另一台服务器。

### Sandbox

Sandbox 是命令的执行环境，范围是这个 agent。现有的 `SandboxDecision` 仍根据 sandbox 是否可用、权限决定、排除的命令和 agent 的工作区，选择 `NoSandbox` 或 `Sandbox { policy }`。文件系统和网络限制、凭据处理、违规存储留在 sandbox crate。

违规是 agent 和 hook 可以观察的 Event。它不是丢掉的帧。每个 agent revision 有自己的 sandbox 范围：cwd、根和网络。Spawn 可以复制策略定义。它不把子成员接到父成员活着的 sandbox 进程上。

### 第一阶段的 patch

作为 patch 传播的编辑：

- 生成或移除一个 agent。生成在 `AgentSpawn` 之前不提交。`context: fork` 的 skill 就是这种 patch。
- 在两个已有 agent 之间加一条 mailbox 边。
- 拆掉一条边。
- 改 join 的同步策略，或改哪条 decision 流接到 join。
- 给一个 ingress 再加一个订阅者。

拓扑编辑走 patch。帧的载荷、权限决定、工具池和 skill 名单不走 patch。MCP 连接状态、会话允许名单、`available_tools`、skill 激活和 `reload_skills` 只通过变成下一版 Bind 来跨 revision。

## 第一阶段要跑通的图

库内一张集群形状的图，上面先放一个成员：

1. 外部帧按时间戳进入 join。Agent 思考时由输出排序器推进下界，完成的 decision 从下一帧生效。没有新 decision 时沿用上一份；没有任何 decision 时消费并丢弃帧。
2. Render 端口满了丢帧。工具结果端口满了就等。两条策略互不改写。
3. 一次工具调用走过 hook 和权限门。Executor 在发出前记下 dispatched，并选出 `SandboxDecision`。
4. 一次 patch 在准入边界原子生效。旧版按原路完成结果收集并退役；新 join 和 egress 拒绝旧 epoch 输出。
5. 第二个成员用一次 spawn patch 和一条 mailbox 边加进来。Mailbox 满了向发送方报错。第二个成员的 decision 默认不接 join。
6. 同一张图上能跑控制节点。串行按列表顺序，前一步的输出是后一步的输入。并行同时启动分支，按声明顺序收齐后再输出。条件只跑被选中的一臂，其余臂写下界。Map 按元素并行，循环必须带上界。这些节点的子步骤若是工具调用，仍走原来的工具边。

外部包的载荷对图是 opaque，图只解释端口和包头。以后的适配器可以从 `media-api` 推帧。第一阶段不链接 `media-runtime`。

### 验收用例

这些是实现时必须通过的验收要求，当前文档不代表已完成实现或测试。

| 场景 | 必须观察到的结果 |
| --- | --- |
| 模型读帧 10，帧 11–19 到达，结果在帧 20 前完成 | 11–19 用旧 decision；新结果只以 `effective_at=20` 发布，保留 `source_timestamp=10` |
| 完成事件与帧准入交错，同一帧前有多个候选 | 每条 decision Stream 每个时间戳至多一个包；包与下界原子可见；选中的候选由排序器顺序确定 |
| Agent 等待工具、Meta 或 Ask，且没有首份 decision | 帧继续被消费，下界继续推进，不因等待模型而无界积帧 |
| r 中 A 调用未返回，r+1 把 render 来源改为 B，A 晚于 B 返回 | 新 join 缓存不被 A 覆盖；新 epoch 输出后不出现旧 epoch render；切源前缓存清空 |
| 移除 agent 时有在飞工具和未决 Ask | 旧答复和工具结果仍到原 invocation；完成收集前不 `Close`；完成后只 `Close` / `SubagentStop` 一次 |
| stale patch、重复 patch、新节点 Open 失败 | 分别返回冲突、原提交结果、准备失败；当前图不被部分修改，不重复触发 spawn 副作用 |
| `user-invocable:false` 与 `disable-model-invocation:true` 分别配置 | 前者隐藏斜杠菜单但模型仍可调用；后者禁止模型，手动入口沿用宿主校验 |
| Skill 声明 `Bash(git commit:*)` 与 `Bash(git push:*)` deny | 保留完整内容规则；仅匹配调用获得 Allow，Deny 优先级由现有 gate 决定；不把规则字符串当工具名 |
| rule / mode / 用户拒绝与 classifier 拒绝 | 前三者不触发 `PermissionDenied` hook；classifier 按既有规则触发；Ask 的 `PermissionRequest` 先于 transport |
| 进程内 Ask 收到重复、过期或错误 revision 答复 | 只有匹配当前 generation 的原 Ask 答复消费一次，不额外 dispatch |
| 启用持久化后，`S(A完成, B等Ask)` 重启 | 复用 A 的结果，仅恢复 B 和父 continuation；旧检查点之后的日志生效 |
| 持久化后在 dispatched 前后、结果持久化前后退出；MCP elicitation 丢连接 | 不自动重发已标 dispatched 的调用；无已存结果转 `outcome_unknown`；已存结果按原 id 只归并一次 |

## 仍未写

节点 `Process` 失败、外部流关闭、取消一个 agent、render 订阅者断开，以及一次在飞的工具调用遇见失败节点时的总规则。并行的一支失败时，其余支是否取消，也在这条未写的规则里。成功路径是等齐。Spawn 拒绝、mailbox 满、权限 Deny 和 MCP 断开已经在上面写了。

## 来源

- MediaPipe calculator graph，本地 `mediapipe` `3b37422b8`。`mediapipe/framework/calculator_graph.cc`、`calculator_node.cc`、`output_stream_manager.cc`、`stream_handler/default_input_stream_handler.cc`。
- MediaPipe 时间戳、下界与默认输入策略：[Synchronization](https://developers.google.com/edge/mediapipe/framework/framework_concepts/synchronization)。Render 的下一帧生效排序器是本设计新增适配，不声称是 MediaPipe 默认语义。
- LangGraph interrupts：[docs.langchain.com/oss/python/langgraph/interrupts](https://docs.langchain.com/oss/python/langgraph/interrupts)。Graph API 里的 `Send` 和中断期间的拓扑迁移：[docs.langchain.com/oss/python/langgraph/graph-api](https://docs.langchain.com/oss/python/langgraph/graph-api)。
- DeepSeek Harness：[github.com/deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)。架构说明 `docs/architecture.md`，Cordis 简介 `docs/cordis-primer.md`。文档站 [deepseek-harness.github.io/deepseek-harness](https://deepseek-harness.github.io/deepseek-harness/)。
- dsh-superagent 研究原型：[github.com/deepseek-ai/dsh-superagent-prototype](https://github.com/deepseek-ai/dsh-superagent-prototype)。README 标明不是正式产品。
