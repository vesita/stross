# 通信模式 v2：控制面协商 + 数据面按 ID 复用

> 状态：**实施中（Phase A/B 已落地，Phase C 待实施）**。
> **开发策略：允许破坏性更新**——协议/架构可 breaking，开发期全端同步演进，
> 不做新旧 wire 兼容层。Phase C 的
> 帧头裁字段（codec/track 移到协商结果）无需保留 v1 字段。
> 关联：[plugin-architecture.md](plugin-architecture.md)（可插拔传输基座，本方案在其上演进）·
> [endpoint-model-v2.md](endpoint-model-v2.md)（端点框架）· [protocol.md](protocol.md)（线上协议）·
> [layering-architecture.md](layering-architecture.md)（分层铁律）。

## 1. 背景与动机

当前数据面是「**每路流 = 一条推流连接**」：屏幕、系统声音各占一条连接，帧头 24 字节
（`magic|version|track|codec|flags|pts|seq|frag*|len`），流的身份**隐含在连接里**。

已出现的问题（真机复现）：
- **并发推流可行**（内核引擎已 Map 化，`并发推流数=2`），但**接收端单流**——
  订阅第二路会停掉第一路接收，导致第一路在服务端被「无观众」自动收尾；
- 停一条流时另一条流被**级联带停**（共享资源/会话拆除耦合）；
- 每路流独立连接 → 分享/订阅的会话登记、鉴权、keepalive 都要按连接维护，模型重。

## 2. 目标心智模型

> **内核（控制面）负责沟通通信协议与处理方法，协商后约定一个 `stream_id`；
> 数据面每包只带这个 id；内核按 id 装载并路由到对应的处理模块。**

两个**正交**的优化维度（勿混为一谈）：

| 维度 | 层面 | 收益 | 手段 |
|---|---|---|---|
| **连接复用** | 逻辑层（分享/订阅模型） | 分享/订阅只维护**一条链路**，流是链路上的廉价条目；停一路不拆链路 | 多路流复用同一条传输连接，按 id demux |
| **字段简化** | 传输层（线上时间成本） | 每包头从 24 字节降到 ~12 字节，小包（音频/控制/文件块）省时明显 | 协商一次后 id 隐含 codec/track/版本，每包只带 `id+pts/seq+len` |

二者独立：可只做其一。v2 因「协商一次、每包引用」同时拿到两者。

## 3. 与现有架构的映射（基座已有，缺三块）

`plugin-architecture.md` 已落地：`Transport` trait（`DataSession::send_frame/recv_frame`）、
`ReliabilityProfile::{Lossless,Lossy,Adaptive}`（允许丢包/不允许/自适应）、四传输
（ws/webrtc/srt/quic）共用同一 `handle_push/handle_watch`、**QUIC 已做 control/media
双 stream 复用**（stream 即类型）。

### 3.0 数据管道模型（2026-08 定稿：pick 规则）

> **原始数据 → 装载逻辑 → 传输数据 → 接收数据 → 解读逻辑 → 呈现数据**
>
> **pick 规则**（用户定稿术语）= 装载/解读的语义规则，`PickRule` 标识，
> 目前两种：**顺序严格（StrictOrdered）** / **即时严格（Realtime）**。
> 装载与解读是同一对 pick 规则的两端对称实现，均由内核提供通用框架；
> 端点只实现「原始数据 ↔ 传输数据」的转化（压缩/编码/分块），经内核 trait 调用。

```
发送侧（装载逻辑，内核提供）              接收侧（解读逻辑，内核提供）
原始数据 ──► 端点转化 ──► 传输数据 ──网络──► 接收数据 ──► 解读模块 ──► 呈现数据
             （编码/压缩，    │                       （Interpreter：
              经内核 trait）  │                        RealtimePacing /
                             ▼                        StrictOrdered）
             内核装载：按档案打包帧（打 id/帧头/分片）
             + 按档案调度发送节奏：
               StrictOrdered = 按序完整发送（不丢不乱）
               Realtime      = 即时直通（容忍丢帧丢块）
```

**分层框架（用户定稿心智模型）**：

| 层 | 职责 | 现有实现 |
|---|---|---|
| 发现层 | 设备/端点发现（已封盘） | `discovery`（mDNS + 子网扫描） |
| 传输层 | 允许丢包 / 不允许丢包 / 自适应几类协议 | `stross-transport`：`ReliabilityProfile` + WS/SRT/QUIC/WebRTC |
| **pick 规则层** | 装载/解读语义（严格顺序 / 严格即时） | `stross-kernel::pick`：`PickRule` + [`Interpreter`]（解读，已落地）+ [`Loader`]（装载，直通） |
| 数据算法层 | 数据转化（编码/压缩/分块）——端点自决 | 端点内实现（ffmpeg、文件泵） |
| 端点接口层 | 模块化端点接口 | `stross-endpoint`：`Endpoint` trait + 各端点 |

**pick 规则层模块结构（`crates/stross-kernel/src/pick/`）**：

```
pick/
  mod.rs        层说明 + re-export（PickRule / Interpreter / Loader / JitterBuffer）
  load.rs       装载逻辑（发送侧）：Loader trait + PassthroughLoader（直通）
  interpret.rs  解读逻辑（接收侧）：Interpreter trait + RealtimePacing + StrictOrdered
  manager.rs    流式通道（StreamChannel）+ 解读注册表（InterpretRegistry，按流 id 装载）
  buffer.rs     JitterBuffer（解读内部机制：抖动缓冲，只服务有损路径）
```

**关键约定**：
- 内核只接收**标准化的传输数据**（`Frame` 统一信封，可随内核开发逐步扩展）；
  载荷语义端点自决（文件端点：`FileMeta` JSON 首帧 + 裸字节已验证）；
- 端点自决载荷打包（编码/压缩/元数据/分块粒度），内核提供统一信封
  （id+len+顺序字段）保证中继可转发；codec/track 等语义字段随帧头 v2
  移到协商结果（= 端点档案的一部分，端点声明、内核执行）；

本方案在其上补三块：

1. **数据衔接层（解读模块，per-stream）**：现有 jitter/pacing/排序逻辑在 kernel 接收路径
   （`sender/watch/jitter` + PTS 调度）是隐式的，未按流独立。新增 `Interpreter` trait：
   - `RealtimePacing`：视频/音频——低延迟、按 PTS 调度、容忍丢帧丢块；
   - `StrictOrdered`：文件/剪贴板——严格有序、重传、逐字节；
   - 每条流装载一个实例（per-stream state 在适配器内），停止一条流只拆该流适配器。
2. **流级 ID 复用**：把「连接内 N 路媒体流」按 `stream_id` demux（QUIC 原生 stream id
   最自然；WS/SRT 保持每流独立连接，不为它们加 framing 层）。
3. **接收端多流化**：接收端能同时 demux 多路流（屏幕+声音同屏），这是最终 UX 闭环。

## 4. 通信流程

```
控制面（kernel，仅建链/变更时）：
  订阅/共享端点 → 协商{传输档案, 解读档案, 编解码, 轨道} → 签发 stream_id
  → 装载模块：传输模块（共享连接，节点对一条）+ 解读模块（per-stream）
  → 中继登记 [连接][stream_id] → 会话映射

数据面（建链后自治，不经过控制面）：
  发送方  [stream_id | pts | seq | payload] ──共享连接──► 中继按 id 转发
  ──► 接收方 kernel 按 id demux ──► 已装载的解读模块（RealtimePacing / StrictOrdered）
```

- 每包只保留**解读模块需要的字段**：实时流要 `pts/seq`；严格有序流要 `seq`；文件块只要 `len+payload`。
- 控制消息（订阅/变更/teardown）仍走无损通道（复用现有 WS/QUIC 控制 stream）。

## 5. 分阶段落地

### Phase A：端点档案 + 协商字段 ✅ 已落地
- `EndpointManifest` 增 `transport_profile`（Lossless/Lossy/Adaptive）与
  `pick_rule`（Realtime/StrictOrdered/None）；wire 类型补枚举
  （`PickRule` 新增；`ReliabilityProfile` 已有），serde default 兼容旧 wire。
- `Endpoint` 契约默认方法（按 `TargetKind` 推断档案：Live→Lossy+Realtime、
  Determined→Lossless+StrictOrdered）；`SubscribeCtx` / `ShareGrant` 携带两档，
  协商链路（`compose_grant` → `notify_subscribed` → ctx）全透传。
- 验证：proto/kernel/endpoint 单测全绿；`check.sh --quick` 通过；旧 wire
  无档案字段可反序列化（缺省回退 Lossy/Realtime）。

### Phase B：pick 规则层模块化 ✅ 已落地（行为等价重构）
- 新增 `Interpreter` trait（`push`/`poll`/`rule`）+ 两个实现：
  `RealtimePacing`（复用 `StreamChannel`：无损直通/有损抖动双路径，行为等价）、
  `StrictOrdered`（直通 + seq 单调校验，防御式丢弃乱序/重复）。
- 模块重组（用户定稿心智模型）：`pick/` 目录（`load.rs` 装载 / `interpret.rs`
  解读 / `manager.rs` 注册表 / `buffer.rs` 抖动缓冲），`InterpretProfile` →
  `PickRule`，`SessionDataManager` → `InterpretRegistry`（按流 id 装载/索引
  `Box<dyn Interpreter>`，per-stream 实例，拆除互不级联）；接收链路
  `watch_consume_loop` 经解读模块消费，`Receiver::start_with_rule` 可指定
  pick 规则（默认 Realtime，行为等价）。
- 验证：kernel 105 单测全绿（含双规则独立运行测试）；`check.sh --quick` 通过。

### Phase C：流级 ID 复用 + 接收端多流（大，真正解锁屏幕+声音同屏）⏳ 待实施
- QUIC：一条连接内按 stream_id 开多路 media stream（现 control/media 两流 → N 媒体流）；
- 中继 `[连接][stream_id]` demux 表（原「每流一会话」→「连接内多流」）；
- 接收端多流 demux + 混音/混画（视频画面 + 音频同播）；
- 帧头 v2 裁字段（codec/track 移到协商结果，每包只带 id+pts/seq）；
- 语义 id 派生落地（§6：`derive(endpoint_id, transport_profile, pick_rule)`）。
- 验证：真机「屏幕+系统声音」同屏播放；停一路不级联；单流回退路径（WS/SRT）不受影响。

## 6. 决策记录（2026-08 定稿：id 机制两层化）

| 项 | 决策 | 理由 |
|---|---|---|
| **语义 id（身份层）** | **端点级确定性 id**：`derive(endpoint_id, transport_profile, pick_rule)`——`[端点 协议 解析]` 三要素唯一定义，一个端点有且仅有一个 id | 结构性订阅收敛（同端点必然同 id，无需运行期查表）；订阅方拿到目录+协商档案即可**本地推导** id，不依赖 grant 返回；多订阅者 = 同一条流（中继多 watcher 天然复用）；停一路只停该 id 数据面活动，互不级联；id 可推导 ≠ 可接入（受控中继仍校验 Hello 凭证） |
| **线上 id（传输层）** | 连接内 **2 字节短 id** ↔ 语义 id 的映射表（协商时控制面下发） | 数据包只带短 id（帧头 v2 裁字段目标之一）；QUIC stream id 同思路；无需全局注册表 |
| 共享传输载体 | **QUIC**（原生多路复用） | 零额外成本；WS/SRT 保持独立连接 |
| 解读模块粒度 | per-stream 实例 | 每条流时序/丢包语义独立，停止互不影响 |
| 帧头演进 | v2 裁字段；v1 兼容保留（WS 上取默认值等价） | 同源升级，不做双版本转换（沿用 plugin-architecture §5） |

**两层关系**：语义 id 是「你是谁、收敛到哪」（逻辑/身份，协商时双方各自可推导）；
短 id 是「线上每包带什么」（传输/效率，连接内映射）。二者正交——
语义 id 保证订阅收敛与停流隔离，短 id 保证传输字节最小化。

**配套改动（已探明影响面）**：
- push 模式两端用**同一派生函数**各自算出同一 id（端点 + 档案是双方共知的
  协商结果），取代订阅方自签 `sess-N`；
- `create_session` 对同 id 幂等（重复调用返回既有会话或建前查重）；
- grant 返回派生 id，订阅方用于一致性校验（watch 用自己能算出的 id）。
- codec 为可扩展维度：同端点同刻只产一种编码，故暂不进 id 三要素；
  未来若同端点多 codec 多路，扩为 `[端点 协议 解析 codec]` 即可。

## 7. 收尾（做完后更新）

- 本文档状态改「已落地（第 N 轮）」；
- `iteration-plan.md` 记录各阶段轮次；
- `dev-playbook.md` 增补「数据衔接层」「流级 ID 复用」「接收端多流」坑位。
