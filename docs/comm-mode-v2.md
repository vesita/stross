# 通信模式 v2：控制面协商 + 数据面按 ID 复用

> 状态：**设计提案（待实施）**。
> **开发策略：允许破坏性更新**——协议/架构可 breaking，开发期全端同步演进，
> 不做新旧 wire 兼容层。Phase C 的
> 帧头裁字段（codec/track 移到协商结果）无需保留 v1 字段。
> 关联：[plugin-architecture.md](plugin-architecture.md)（可插拔传输基座，本方案在其上演进）·
> [endpoint-model.md](endpoint-model.md)（端点框架）· [protocol.md](protocol.md)（线上协议）·
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

本方案在其上补三块：

1. **数据衔接层（解读模块，per-stream）**：现有 jitter/pacing/排序逻辑在 kernel 接收路径
   （`sender/watch/jitter` + PTS 调度）是隐式的，未按流独立。新增 `DataPlaneAdapter` trait：
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

### Phase A：端点档案 + 协商字段（小，不破坏现有链路）
- `EndpointManifest` 增 `transport_profile`（Lossless/Lossy/Adaptive）与
  `interpret_profile`（Realtime/StrictOrdered/None）；wire 类型补枚举（`ReliabilityProfile` 已有）。
- 订阅握手（`SubscribeCtx`）携带/确认这两档；端点在 `share()` 前完成装载选择。
- 验证：`check.sh --quick` + jsdom + kernel 测试不变；目录/订阅行为等价。

### Phase B：数据衔接层模块化（中，行为等价重构）
- 新增 `DataPlaneAdapter` trait + `RealtimePacing`/`StrictOrdered` 两个实现；
- 把 kernel 接收路径的 jitter/PTS 调度/重传逻辑迁入适配器；发送侧按流 id 打标；
- 传输选择逻辑收进「传输模块」（共享 per-node-pair）。
- 验证：现有接收/播放行为逐项等价（jsdom + kernel roundtrip 全绿）；可同时跑两路不同
  解读档的流。

### Phase C：流级 ID 复用 + 接收端多流（大，真正解锁屏幕+声音同屏）
- QUIC：一条连接内按 stream_id 开多路 media stream（现 control/media 两流 → N 媒体流）；
- 中继 `[连接][stream_id]` demux 表（原「每流一会话」→「连接内多流」）；
- 接收端多流 demux + 混音/混画（视频画面 + 音频同播）；
- 帧头 v2 裁字段（codec/track 移到协商结果，每包只带 id+pts/seq）。
- 验证：真机「屏幕+系统声音」同屏播放；停一路不级联；单流回退路径（WS/SRT）不受影响。

## 6. 决策记录（待定项）

| 项 | 倾向 | 理由 |
|---|---|---|
| id 作用域 | **连接内局部 id**（2 字节）+ 控制面映射表 | 小、无需全局注册表（QUIC stream id 同思路） |
| 共享传输载体 | **QUIC**（原生多路复用） | 零额外成本；WS/SRT 保持独立连接 |
| 解读模块粒度 | per-stream 实例 | 每条流时序/丢包语义独立，停止互不影响 |
| 帧头演进 | v2 裁字段；v1 兼容保留（WS 上取默认值等价） | 同源升级，不做双版本转换（沿用 plugin-architecture §5） |

## 7. 收尾（做完后更新）

- 本文档状态改「已落地（第 N 轮）」；
- `iteration-plan.md` 记录各阶段轮次；
- `dev-playbook.md` 增补「数据衔接层」「流级 ID 复用」「接收端多流」坑位。
