# Codex Channels Bridge — 设计书

> Status: Draft v1 — 2026-05-15
> Owner: fork (mouriya-s-lab/codex)
> Upstream-merge intent: 不合回 openai/codex；fork-local 长期演进

---

## 1. 目标与非目标

### 目标

在不影响 codex TUI 现有交互体验的前提下，提供一条"旁路通道"：

1. **入向（external → agent）**：任意外部对端（HTTP webhook、IM bot、自有服务、其他 agent）可以把消息投递给当前正在运行的 codex 会话；消息以合成 user 消息形式参与下一轮 turn，**不抢人类正在打字的输入**。
2. **出向（agent → external）**：agent 在 turn 内可以主动向上述外部对端"回复"；回复内容**不进入 TUI 的对话流**，由桥进程转出到外部 transport。
3. **TUI 保留对人类的归属**：人类用户继续把 TUI 作为主交互界面，channel 消息在 TUI 上以可识别的方式出现（颜色/前缀），但不会被误当作人类输入。

### 非目标

- 不实现 Claude Code 的 channel 审批旁路（`claude/channel/permission`）。codex 自身审批流程结构不同，强行对齐成本高、收益低。**P2，留作未来扩展**。
- 不重写 codex 的输入队列或 turn 调度。
- 不解决多 TUI thread 切换时的 channel 跟随问题（先绑定到单一活动 thread）。
- 不内置生产级 transport（HTTP/Telegram/Discord/iMessage 都是 demo 级；用户可以替换）。

---

## 2. 关键约束与决策

### 核心约束（用户指定）

- **codex 源码侵入最小**——理想是 **0 行修改**（只在 workspace members 加一行），让 fork 长期 rebase 上游容易。
- 不合回上游，但要保持上游 rebase 路径干净。

### 决策

**采用"纯外置桥进程"方案。** 把整个 channel 功能放进一个独立的 `codex-channel-bridge` crate，它同时扮演两个角色：

1. **作为 codex 的 MCP server**（出向通道）：由用户在 `~/.codex/config.toml` 的 `[mcp_servers.codex-channel]` 里声明，由 codex 启动并通过 stdio 通信。agent 在 turn 中调用桥进程注册的 MCP 工具（如 `reply`），桥进程把内容转给外部 transport。**零 codex 源码改动**。
2. **作为 codex app-server 的 JSON-RPC 客户端**（入向通道）：桥进程同时连到 codex app-server 的 Unix 控制套接字（`app_server_control_socket_path()`），订阅 turn 通知，在合适时机通过 `turn/start` 把外部消息当合成 user input 投递进活动 thread。

#### 为什么这个方案对 codex 比对 CC 更自然

CC 必须改源码（`useManageMCPConnections.ts` 注册自定义 notification handler、`messageQueueManager.ts` 共享队列、`cli/print.ts` SleepTool 唤醒点），因为它**没有 JSON-RPC 入口让外部进程投递 user input**——唯一通道是修改 MCP 客户端把通知翻译成内部输入。

codex 的 app-server 已经把 `turn/start` 暴露成完整 JSON-RPC 接口（`codex-rs/app-server/src/request_processors/turn_processor.rs:315`），任何能连到 control socket 的进程都可以投递 turn，且服务端不绑定 client identity。也就是说 codex 已经具备 CC 自己造出来的那条"旁路注入路径"，桥进程直接复用即可。

---

## 3. 与 Claude Code channels 的功能对照

| 功能 | Claude Code | Codex Channels Bridge |
|------|-------------|------------------------|
| 入向"用户消息"投递 | MCP 通知 `notifications/claude/channel` → 入共享队列 | 桥进程注入 prompt-injection 文本 → `turn/start` → 走 user input 同路径 |
| 让模型识别 channel 来源 / 行动 | XML markup `<channel source=...>` 包裹（CC 内部 trained-on） | **不依赖 markup**——用自然语言字段 `Source: ...` + `Required action: ...` 显式说明（GPT-5 不会对自造 markup 做特殊解释） |
| 出向回复 | MCP 工具调用（plugin 注册 `reply` 工具） | **完全一致**：MCP 工具调用（注入 prompt 强制 `Required action: MUST call channel_reply`） |
| 排队（turn 进行中累积、turn 完后投递） | SleepTool 轮询 `hasCommandsInQueue()` | 桥进程订阅 `turn/started` / `turn/completed` 通知，自己排队 |
| 优先级（人类输入永不被压住） | 三档 `now/next/later` | 不抢——人类输入走 TUI → app-server 的现有路径，桥进程的 `turn/start` 只在 thread idle 时 fire |
| 审批旁路 | `claude/channel/permission` 子协议 | **不做**（P2） |
| TUI 视觉区分 | `UserChannelMessage.tsx` 组件 | 不区分——注入文本是普通 user message，TUI 默认渲染就显示注入内容（包括 `[External channel message]` 字段段 + 原文），用户自然看懂来源；不改 TUI 源码 |
| 多种 transport（Telegram / Discord / iMessage / HTTP） | 多个独立 plugin | 桥进程内置 transport trait，先实现 HTTP demo（对标 fakechat），其它后加 |
| 安全门控（allowlist / OAuth / org policy） | GrowthBook 多层 gate | 简化为"用户配置即信任"。fork 自用，不做组织级合规层 |

---

## 4. 总体架构

```
   ┌───────────────────────────────────────────────────────────┐
   │                  codex (上游源码不动)                       │
   │                                                            │
   │  ┌───────┐   AppEvent (mpsc)   ┌──────────────────────┐   │
   │  │ TUI   │ ◄─────────────────► │   app-server         │   │
   │  └───────┘                     │   - control UDS      │   │
   │     ▲                          │   - thread 调度       │   │
   │     │ 人类打字                  │   - turn/start RPC   │   │
   │   人类                         │   - rmcp-client      │   │
   │                                └──┬─────────────┬─────┘   │
   │                                   │             │         │
   │                  Unix Domain Socket            stdio      │
   │                  (control socket)         (MCP transport) │
   └───────────────────┬─────────────────────────────┬─────────┘
                       │                             │
                       ▼                             ▼
   ┌───────────────────────────────────────────────────────────┐
   │                codex-channel-bridge (fork-features)        │
   │                                                            │
   │  ┌───────────────────────┐   ┌──────────────────────┐    │
   │  │ AppServerClient       │   │ McpServer            │    │
   │  │ - 订阅 turn 通知       │   │ - reply tool         │    │
   │  │ - fire turn/start     │   │ - send tool          │    │
   │  └────────┬──────────────┘   └──────────┬───────────┘    │
   │           │                              │                │
   │           ▼                              ▼                │
   │  ┌──────────────────────────────────────────────────┐    │
   │  │              Bridge Core                          │    │
   │  │  - inbound buffer (turn busy 时累积)               │    │
   │  │  - prompt-injection 文本构造                       │    │
   │  │    (字段化 meta + Required action + 原文三引号)    │    │
   │  │  - transport router                               │    │
   │  └──────────────────────────────────────────────────┘    │
   │                          │                                │
   │            ┌─────────────┼─────────────┐                  │
   │            ▼             ▼             ▼                  │
   │       ┌────────┐    ┌────────┐    ┌────────┐             │
   │       │  HTTP  │    │ (其它  │    │ (其它  │             │
   │       │  demo  │    │ trans  │    │ trans  │             │
   │       └───┬────┘    └────────┘    └────────┘             │
   └───────────┼───────────────────────────────────────────────┘
               │
               ▼
        外部对端 (webhook / Bot / 服务)
```

桥进程有**两条独立 I/O 通道**接到 codex：

1. **stdio**（被 codex 当 MCP server 启动时建立）：双向 MCP 协议。负责出向（工具调用）和能力声明。
2. **Unix Domain Socket**（桥进程主动连 codex app-server）：双向 JSON-RPC。负责入向（`turn/start`）和状态订阅（`turn/started` / `turn/completed` / `thread_status_changed`）。

---

## 5. 协议

### 5.1 入向（external → agent）

#### 5.1.1 transport 层（外部对端 → 桥进程）

由 transport 实现决定。HTTP demo 形式：
```
POST /channel/inbox
{
  "content": "build failed on main: https://ci.example.com/run/1234",
  "meta": {
    "source": "github-actions",
    "run_id": "1234"
  }
}
→ 200 OK { "queued": true, "queue_depth": 1 }
```

#### 5.1.2 桥 → codex 注入：prompt-injection 形式

桥进程**不**自造 XML markup（GPT-5 不会对自定义 markup 做特殊解释）。桥进程把每条 inbox 消息**包装成一段对模型显式说明的自然语言 prompt**，注入 channel 元字段 + 行动指令 + 原始消息，然后作为普通 `V2UserInput` 投递。

注入文本模板（用 `text` 字段单条投递，envelope 仍是普通 `role=user / type=input_text`，跟 TUI 输入字节级同等）：

```
[External channel message]
Source: <transport-name>
<meta_key_1>: <value_1>
<meta_key_2>: <value_2>
...

Required action: You MUST reply to this message by calling the MCP tool
`channel_reply` with target="<target-id>". Do not put your reply in your
normal assistant text — only the `channel_reply` tool delivers to the
external recipient. Your assistant text in this turn is invisible to
the external party.

Original message:
"""
<对端发来的原始内容，verbatim>
"""
```

字段含义：
- **`Source:` 行 + meta key/value 行**：把 transport 名字、对端标识（chat_id、user、thread_ts 等）以**字段形式**直接列出，让模型解析时不依赖任何自定义 markup
- **`Required action:`**：强制说明唯一回复路径是 MCP tool `channel_reply`，把 target 参数应该填什么写明（target-id 由桥进程根据当前 transport+对端 ID 拼好）
- **`Original message:` 三引号块**：对端原话 verbatim，跟"用户在 TUI 打字的内容"等价

注入后的完整 `TurnStart` 调用：

```jsonc
{
  "jsonrpc": "2.0",
  "method": "turn/start",
  "id": "<uuid>",
  "params": {
    "threadId": "<active-thread-id>",
    "input": [{
      "type": "text",
      "text": "<上面的注入文本>"
    }]
  }
}
```

从 codex 视角：这就是一条普通 text user input。`turn_start_inner` 不区分调用源；`V2UserInput::into_core` → `CoreInputItem` → thread runtime → 模型 inference → server notifications → TUI 渲染走 user message 块 → rollout 保存为 user role message。**envelope、流程、渲染、历史全部跟 TUI 用户打字字节级同等**。

#### 5.1.3 排队语义

桥进程内部维护一个 inbound FIFO buffer。状态机：

- thread `idle` → 立即 fire `turn/start`
- thread `in-progress` → 入 buffer
- 收到 `turn/completed` 通知 → 把 buffer 里所有积压消息**合并成单条注入文本**（多个 `[External channel message]` 段拼接，模型可以按段处理），fire

合并语义跟 CC 在 turn 进行中累积、turn 完后一次性投递一致；区别是合并形式是**多段 prompt-injection 文本拼接**，不是多个 XML 块。

### 5.2 出向（agent → external）

agent 回复外部对端**唯一**通过 MCP 工具调用。inbound 注入 prompt 中 `Required action:` 段已经显式告诉模型"必须用 `channel_reply` 工具回复，普通 assistant 文本对端看不到"——模型读到这段就会调工具。

桥进程作为 MCP server（在 `~/.codex/config.toml` 的 `[mcp_servers.codex-channel]` 配置启动）注册工具：

#### `channel_reply`

```jsonc
{
  "name": "channel_reply",
  "description": "Send a reply to an external channel recipient. The inbound channel message you are responding to tells you what `target` to use. Calling this tool is the ONLY way to deliver text to the external party — your normal assistant text is invisible to them.",
  "inputSchema": {
    "type": "object",
    "required": ["target", "text"],
    "properties": {
      "target": { "type": "string", "description": "Channel target id from the inbound message (format: <transport>:<id>, e.g. telegram:12345)" },
      "text": { "type": "string", "description": "Reply text to deliver to the external recipient" }
    }
  }
}
```

返回简短确认（`{ "ok": true, "delivered_to": "telegram:12345" }`）。TUI 显示工具调用气泡 + ok 结果，但**回复正文不进 TUI**（agent 的 assistant 文本默认不进对端；只有 `channel_reply` 调用的 `text` 参数进对端）。

#### `channel_send`（可选 P1）

主动向特定外部对端推送（不需要前置有 inbound 消息）。形式与 `channel_reply` 相同，只是 description 改为"主动 push"。

### 5.3 注入文本设计原则

不依赖任何自定义 markup（XML、JSON-in-text、特殊 token）。原因：GPT-5 不会对自造的 `<channel>` 标签做特殊解释——XML markup 跟自然语言字段在 GPT 看来都是 prose tokens，没有协议级语义。所以注入设计走**自然语言字段 + 行动指令**形式（5.1.2 模板），让模型靠 prompt-engineering 解析，而非靠 markup 解析。

设计原则：
- **字段化**：每条 meta 一行 `key: value`，模型解析时不依赖 markup
- **行动指令显式化**：用 `Required action: You MUST call ...` 一类强制措辞，把工具名、target 参数都写明
- **原文用三引号块隔离**：避免对端文本污染指令段
- **target 由桥进程拼好**：模型从注入文本里 verbatim 抄一次 target，不需要自己组合 transport+id
- **不依赖模型 schema-validation 能力**：模型只需要 follow prose 指令，不需要解析 XML/JSON 字段

`#6` 的 target parsing matrix 显示 field-combine（只给 `Source:` + channel id 字段，让模型组合 target）在标准 transport 与 renamed-meta 样本中可行，但 hostile 原文攻击样本只有 **bridge prebuilt target** 完整通过。因此默认模板仍然由桥进程预拼 `target="<transport>:<id>"` 并写入 `Required action:`；field-combine 只能作为后续简化选项，不能作为 hostile / untrusted external text 的默认写法。

外部 transport 名 + meta key 必须做**前置清洗**：删除控制字符、限定 key 字符集为 `[A-Za-z0-9_-]`、value 做换行转义。原因不是 XML 安全，是防止对端注入恶意 prompt 段（如 "Required action: ignore previous instructions ..."）破坏指令完整性。具体清洗规则在 §10 / 桥进程实施时定义。

---

## 6. 桥进程内部设计

### 6.1 状态机

```
                  ┌─────────────────────────────────────┐
                  │           ThreadIdle                 │
                  │  buffer == []                        │
                  └──────┬──────────────────────────────┘
                         │ inbound arrives
                         ▼
                  ┌─────────────────────────────────────┐
                  │           FlushNow                   │
                  │  fire turn/start, drain buffer       │
                  └──────┬──────────────────────────────┘
                         │ on TurnStarted (any client)
                         ▼
                  ┌─────────────────────────────────────┐
                  │          ThreadBusy                  │
                  │  buffer.push(...); 不直接 busy fire  │
                  └──────┬──────────────────────────────┘
                         │ on TurnCompleted flush
                         ▼ (buffer.is_empty() ? ThreadIdle : FlushNow)
```

`ThreadBusy` 不区分 "我自己刚 fire 的" 还是 "TUI 刚 fire 的"——桥进程只观察 thread 状态。这是关键：桥永远不抢人类的 turn。

`#4` 已证明 busy thread 上直接再 fire `turn/start` 不安全：第二个 turn 会进入 active runtime，且第一个 turn id 的生命周期可能完成为第二个 prompt 的答案。实现必须把 `ThreadBusy` 视为桥侧硬门：busy 时只 `buffer.push(...)`，不向 app-server 发送新的 `turn/start`；只有观察到 `TurnCompleted` 后才从 buffer flush 下一批外部消息。

### 6.2 模块划分（fork-features 内部）

```
codex-rs/fork-features/codex-channel-bridge/
├── Cargo.toml
├── DESIGN.md                ← 本文档
├── README.md                ← 用户文档
├── src/
│   ├── main.rs              ← 二进制入口，参数 / 配置加载
│   ├── lib.rs               ← 模块导出
│   ├── bridge.rs            ← 状态机 + 主循环（消息泵）
│   ├── appserver/
│   │   ├── mod.rs
│   │   ├── client.rs        ← 包装 codex-app-server-client crate
│   │   └── thread_watch.rs  ← 订阅 turn 状态通知
│   ├── mcp/
│   │   ├── mod.rs
│   │   └── server.rs        ← 用 rmcp crate (server side) 暴露 reply/send 工具
│   ├── transport/
│   │   ├── mod.rs           ← Transport trait
│   │   ├── http.rs          ← P0 demo (类似 fakechat HTTP 127.0.0.1:8787)
│   │   └── webhook.rs       ← P1 outbound webhook（agent → 第三方）
│   ├── inject.rs            ← prompt-injection 文本构造（字段化 meta + Required action + 原文三引号，独立单元测试覆盖 meta 清洗 / 行动指令完整性）
│   └── config.rs            ← 桥的配置（端口、threadId 绑定策略等）
└── tests/
    ├── inject_format.rs     ← prompt-injection 文本格式 + meta 清洗（防注入恶意指令）
    ├── state_machine.rs
    └── e2e_http_roundtrip.rs ← 拉起内嵌 mock app-server + 桥 + HTTP curl
```

### 6.3 thread 绑定策略

桥进程启动时可以选三种模式：

1. **`--thread-id <id>`**：绑定指定 thread（脚本/自动化场景）。
2. **`--latest`**（默认）：通过 `thread/list` 取最近活跃 thread。
3. **`--follow-tui`**：订阅 `thread_status_changed`，跟随当前 TUI 焦点 thread。最贴近 CC 体验但实现复杂度高，放 P1。

---

## 7. 零源码侵入的依据

整个方案对上游 codex 源码的改动**仅有一处**：

### 唯一改动：`codex-rs/Cargo.toml` workspace members

```toml
[workspace]
members = [
    # ... 上游已有 100+ crate
    "fork-features/codex-channel-bridge",  # ← 新增一行
]
```

这一行在 rebase 上游时几乎不会冲突（即使冲突也是机械合并）。

**所有其它代码都在 `fork-features/codex-channel-bridge/` 目录下**：

- ❌ 不动 `codex-rs/core/`、`codex-rs/tui/`、`codex-rs/app-server/`、`codex-rs/rmcp-client/`
- ❌ 不动 codex 启动逻辑、不加新 CLI flag、不修改 `~/.codex/config.toml` schema
- ✅ 桥进程通过用户自己写的 `[mcp_servers.codex-channel] command = "codex-channel-bridge"` 配置启动
- ✅ 桥进程通过 `codex-rs/app-server-client/`（已经是上游公共 crate）连 app-server
- ✅ 桥进程通过 `rmcp` crate（外部依赖）实现 MCP server 端

### 依赖的上游 stability 假设

桥进程依赖这些上游公共 API，rebase 时要回归验证：

1. `codex-app-server-protocol::ClientRequest::TurnStart` 仍然存在且字段兼容
2. `codex-app-server-transport::app_server_control_socket_path()` 路径规则不变
3. `codex-app-server-client` crate 仍然存在（这是 codex 自己的客户端 lib）
4. `[mcp_servers]` 配置语义不变

这些都是上游对外的稳定接口（已经有 `experimental` 注解控制实验性），变化概率低。

---

## 8. 用户接入方式

### 8.1 启用

#### 一次性安装
```bash
cd /Users/mouriya/Ext/code/codex/codex-rs
cargo install --path fork-features/codex-channel-bridge
```

#### 在 `~/.codex/config.toml` 加配置
```toml
[mcp_servers.codex-channel]
command = "codex-channel-bridge"
args    = ["--transport", "http", "--http-port", "8787", "--latest"]
# 环境变量（可选）
env     = { CODEX_CHANNEL_LOG = "info" }
```

下次启动 codex，桥进程就跟着启动。模型自动看到 `channel_reply` 工具。外部对端可以 `POST http://127.0.0.1:8787/channel/inbox` 投递消息。

### 8.2 transport 扩展

用户写自己的 transport：

```rust
// fork-features/codex-channel-bridge/src/transport/my_telegram.rs
use crate::transport::Transport;

pub struct TelegramTransport { /* ... */ }

#[async_trait::async_trait]
impl Transport for TelegramTransport {
    fn name(&self) -> &'static str { "telegram" }
    async fn run_inbound(&self, sink: InboundSink) -> Result<()> { /* ... */ }
    async fn send_outbound(&self, target: &str, text: &str) -> Result<()> { /* ... */ }
}
```

在 `transport/mod.rs` 的 registry 里注册一行即可。

---

## 9. Spike 结果日志（Open Questions）

> 2026-05-20 状态：Q1 / Q2 / Q3 / Q4 / Q6 已由 #3-#7 spike 关闭。后续 #2 implementation 必须按这些 verdict 写实现，而不是继续沿用旧假设。

### Q1: `turn/start` 在 thread 已经有 in-progress turn 时的行为
**Verdict**：#4 选择 `Interrupts turn1` / unsafe active-runtime injection。busy thread 上第二个 `turn/start` 会立即返回 `status=inProgress`，并且第二个 user message 会进入第一个 turn 的 active runtime；`task_complete` 记录在 turn1 id 下，但 final answer 来自 turn2 prompt。
**设计影响**：桥不能依赖 app-server 的 direct `turn/start` queue semantics，也不能在 `ThreadBusy` 时直接 fire 新 turn。6.1 状态机必须保留桥侧 serialization：busy 时 buffer，观察到 `TurnCompleted` 后 flush。
**证据**：#4 runtime evidence `busy-poc.jsonl`、`tui-session-busy.jsonl`、`rollout-lifecycle-excerpt.jsonl`、`busy-analysis.md`。

### Q2: 模型是否会按 prompt-injection 文本的 `Required action:` 指令调 `channel_reply`
**Verdict**：#5 narrow positive but baseline-high。带 `Required action:` 的 prompt 10/10 调用 `channel_reply`，且 10/10 正确复制 `target="telegram:12345"`；但去掉 `Required action:` 的 baseline 也 10/10 调 tool，所以该段不是已证明的唯一因果触发。
**设计影响**：当前 prose prompt + MCP tool description 足以覆盖简单样本，不需要为 #2 引入 system prompt patch 或 forced tool-choice。`Required action:` 继续保留为清晰合同，但不要把它描述成唯一可靠触发机制。
**证据**：#5 `codex exec --json` 20-trial matrix、`codex-channel-reply-calls.jsonl`、MCP handshake log。

### Q3: prompt-injection 文本中 meta 字段对模型解析的稳健性
**Verdict**：#6 field-combine 在 normal / renamed-meta 样本中可行（60/60 exact），prebuilt-target 也可行（60/60 exact），但 hostile original text 样本只有 prebuilt-target 完整完成并 10/10 抵抗 `target="evil"`。
**设计影响**：5.3 的默认写法保持 bridge-prebuilt target：桥进程预先拼好 `<transport>:<id>` 并写进 `Required action:`，模型只需 verbatim copy。field-combine 是可能的后续简化，不作为 hostile external text 默认。
**证据**：#6 140-trial matrix、`aggregate.tsv`、`aggregate-by-transport.tsv`、`failures.tsv`、MCP call log。

### Q4: codex 启动 MCP server 时的生命周期
**Verdict**：#7 positive。`/nonexistent`、`/bin/echo` 非 MCP 协议进程、启动后立即 `exit 1` 三种失败模式都没有让 codex panic / 挂死；TUI 能启动，非交互 turn 能完成，错误通过 stderr/tracing 可见。
**设计影响**：桥进程外层 wrapper 不是强制架构前提；用户文档仍应说明如何从 stderr/tracing 排查 MCP startup failure，因为 TUI banner 可见性弱。
**证据**：#7 bad-path / echo-protocol / exit-1 TUI startup probe、`codex exec --json` marker turn、stderr traces、config restore hash。

### Q5: 控制套接字 / 注入路径的访问权限
**假设**：fork-features 起的 UDS 设 0600（仅当前用户）。桥进程跟 codex 同用户，没问题。
**意义**：不需要额外鉴权层（fork 自用场景）。

### Q6（前提性，必须最先验证）：embedded TUI 模式下注入路径是否存在
**Verdict**：#3 All pass。embedded TUI 可通过 fork-features env-var hook clone `InProcessAppServerRequestHandle`，启动 repo-local UDS bridge，并把外部 line-delimited JSON-RPC `turn/start` 转发到 active TUI thread。PoC 注入文本作为普通 user message 进入 rollout/TUI，turn completed，无 panic/hang；no-env control 不创建 socket，bad-path control 只 warn 且主 TUI turn 正常。
**设计影响**：#2 可以基于 A' 注入路径继续：fork-features 内部启动 bridge task + UDS，桥同用户访问，走 `InProcessAppServerRequestHandle::request(ClientRequest::TurnStart)`。该结论不等于允许 production 直接合并 #3 spike branch；#2 仍需重新实现成最小、可维护的 bridge crate。
**证据**：#3 branch `spike/issue-3-injection-poc` at `81c7898f7826fa642fd5b8eefe62cce29c550b74`，socket permission / cleanup logs、PoC turn response、TUI session JSONL、rollout excerpt。

---

## 10. 分阶段实施

### Phase 0：可行性验证（半天）

只跑 PoC，**不写桥**，确认假设：

- 写一个 `codex-rs/fork-features/codex-channel-bridge/poc/turn_inject.rs` 一次性脚本
- 启动 codex TUI，在另一个终端跑脚本，连 UDS 发 `turn/start`
- 看 TUI 是否渲染 channel 消息、是否抢断当前 turn
- 写一段 200 字内的验证报告，更新本设计书的 Open Questions

### Phase 1：MVP（1–2 天）

最小可用：

- `transport::http` HTTP demo 一个 `POST /channel/inbox` + `POST /channel/outbox-test`
- `mcp::server` 暴露 `channel_reply` 一个工具（schema 严格 `target` + `text`）
- `appserver::client` 订阅 `TurnStarted` / `TurnCompleted` 通知，绑定 `--latest` thread
- `bridge` 状态机（最简：buffer + flush-on-completed）
- `inject.rs` prompt-injection 文本构造（含 meta 清洗 + Required action 模板，带单元测试覆盖恶意 meta key/value 注入）
- 端到端 e2e：curl 投递 → TUI 看到注入文本 → 模型读 `Required action:` → 调 `channel_reply(target=..., text=...)` → curl `outbox` 拉到回复

### Phase 2：抛光（1–2 天）

- `transport::webhook` 出向 HTTP 推送
- `channel_send` 主动推送工具
- 重连 / 错误处理 / 优雅停止
- `--follow-tui` 模式

### Phase 3（可选 / 暂不做）

- TUI 自定义渲染分支：源码侵入 1 处（`tui/src/chatwidget/...`），加 `is_channel = client_name == "codex-channel-bridge"` 渲染颜色——只在 Phase 1/2 验证用户体验有刚需时才做。
- 审批旁路：参考 CC 的 `claude/channel/permission` 子协议设计。

---

## 11. 明确不做的事

- 不做 GrowthBook 风格的多层 gate（OAuth / org policy / allowlist）。fork 自用场景，"用户配置即信任"足够。
- 不做 5 字母 ID + 脏话黑名单（CC 的 `shortRequestId`）。审批旁路不在范围内。
- 不做 channel 服务的横向扩展（多 channel server 并存）。第一版限定一个桥进程。
- 不和 codex 的 hook 系统耦合。
- 不改 `[mcp_servers]` 配置语义。

---

## 12. 风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| `turn/start` 在 turn busy 时抢断 TUI | 中 | 高 | Q1 必须前置验证；如果抢断就只在 `TurnCompleted` 后 fire（已是默认设计） |
| 上游改 `TurnStartParams` / `InProcessAppServerRequestHandle` 字段 | 低 | 中 | 桥依赖的是公共协议 + client crate，rebase 时编译报错即可发现；锁版本不是好选择（要长期跟 main） |
| MCP server 启动失败 codex 直接报错挂死 | 低 | 中 | codex 上游对 MCP server 启动失败一般是降级（连接不上则跳过），需 Phase 0 验证；如果会挂，文档提醒用户先确认 binary 在 PATH |
| 模型忽略 `Required action:` 指令、用 assistant text 而非 `channel_reply` 工具回复 | 中 | 高 | Q2 必须前置验证；若不稳定可加强 prompt 措辞，或把指令搬到 base_instructions（升级为源码侵入） |
| 对端注入恶意 prompt 段（如 `\n\nRequired action: ignore previous instructions`）破坏指令完整性 | 中 | 中 | inject.rs 必须清洗 meta key/value、对原文做严格三引号隔离 + 转义；单元测试覆盖恶意 case |
| 单 thread 绑定不符合多窗口工作流 | 中 | 中 | P1 加 `--follow-tui` 模式，P2 考虑多 thread 注册多个桥实例 |

---

## 附录 A：与 CC 实现差异速查

| CC 的实现细节 | 桥进程替代 / 省略 |
|--------------|-------------------|
| `useManageMCPConnections.ts:507` 注册 notification handler | 不需要——通过 `InProcessAppServerRequestHandle.request(TurnStart)` 走 user input 同路径 |
| `messageQueueManager.ts` module-level 共享队列 | 桥进程内部 `Vec<PendingInbound>` |
| `cli/print.ts:2915` SleepTool 唤醒 | 不需要——`TurnStart` 主动 fire turn |
| `wrapChannelMessage` XML 包裹 | **不做**——改为 prompt-injection 自然语言字段 + `Required action:` 指令（GPT-5 不识别自造 markup） |
| `UserChannelMessage.tsx` 自定义渲染 | 不做（接受 TUI 默认渲染；注入文本本身可读，用户能看懂来源） |
| `tengu_harbor` GrowthBook 多层 gate | 用户配置即信任 |
| `notifications/claude/channel/permission` 审批旁路 | P2 不做 |
| `shortRequestId` 5 字母 ID + 脏话黑名单 | 同上 |

## 附录 B：相关源文件索引

### codex 上游（仅引用，不修改）
- `codex-rs/app-server-protocol/src/protocol/v2/turn.rs:49` — `TurnStartParams`
- `codex-rs/app-server/src/request_processors/turn_processor.rs:315` — `turn_start_inner`
- `codex-rs/app-server-transport/src/lib.rs:17` — 公共 transport API
- `codex-rs/app-server-client/src/lib.rs:468` — `InProcessAppServerRequestHandle`（Clone-able 跨 task 句柄）
- `codex-rs/app-server-client/src/lib.rs:609` — `InProcessAppServerClient::request_handle()`
- `codex-rs/config/src/mcp_types.rs` — `[mcp_servers]` schema

### CC 参考（`/tmp/claude-code-src/`）—— 协议形态参考但**不照搬 markup**
- `src/services/mcp/channelNotification.ts:106` — `wrapChannelMessage`（XML markup，本设计不沿用）
- `src/services/mcp/useManageMCPConnections.ts:507-531` — notification handler 注册形态
- `src/utils/messageQueueManager.ts:128` — `enqueue` 与优先级
- `src/services/mcp/channelPermissions.ts` — 审批旁路（P2 参考）

---

## 决策日志

- **2026-05-15** 初稿。选定纯外置桥方案。Open Questions 待 Phase 0 验证。
- **2026-05-17** 修订：
  - falsified 原方案 Y（"在 `run_server` 加 env-var patch 启 socket"）—— embedded TUI 走 `InProcessAppServerClient::start` → `in_process::start_uninitialized`，完全不调用 `run_server`，patch 位置不存在于 embedded 路径。
  - 改为 fork-features 在 TUI 进程内通过 `InProcessAppServerRequestHandle` 注入 `TurnStart`，不动 `in_process.rs`。
  - **删除 `<channel>` XML 包裹设计** —— GPT-5 不会对自造 markup 做特殊解释，markup 跟自然语言 prose 在 GPT 看来都是 prose tokens 没有协议级语义。
  - 改为 **prompt-injection 形式**：注入文本由字段化 meta（`Source: ... \nchat_id: ... \nuser: ...`）+ 行动指令（`Required action: You MUST call channel_reply with target=...`）+ 三引号包裹的对端原文组成。模型靠 prompt-engineering 解析指令，调 MCP 工具回复。
  - 新增 Q2（模型是否可靠 follow `Required action:`）和 Q3（字段化 meta 解析稳健性）作为前置验证项。
  - 新增"对端注入恶意 prompt 段"风险（meta 字段必须清洗）。
- **2026-05-20** spike 结果分支：
  - #3 verified A' injection path: embedded TUI 可以通过 fork-features UDS bridge + `InProcessAppServerRequestHandle` 注入 `TurnStart`，外部消息进入普通 user-message runtime path。
  - #4 falsified direct busy fire safety: busy thread 上第二个 `turn/start` 会进入 active runtime；#2 implementation 必须 bridge-side buffer，等 `TurnCompleted` 后 flush。
  - #5 verified simple-sample `channel_reply` follow behavior（10/10），但 baseline 也 10/10；`Required action:` 保留为合同文本，不视为唯一因果触发。
  - #6 selected bridge-prebuilt target as default: field-combine 可行，但 hostile external text 下只有预拼 target 完整通过。
  - #7 verified MCP startup failures degrade without panic/hang; errors visible through stderr/tracing, wrapper 不是强制前提。
  - 结论：#2 可以基于 A' 注入路径继续，但 implementation gate 必须包括 busy serialization、prebuilt target、meta 清洗、MCP failure debug path，以及重新实现而非直接合并 spike branch。
