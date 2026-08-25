# 协议文档

版本：2（`VERSION = 2`，magic `"STR2"`），小端序。
v1 → v2 变更：帧头从 16 字节扩为 24 字节（新增 `seq` / `frag_*` 字段），
控制消息增加能力协商（`capabilities` / `offer` / `answer`）与路由控制（`route` / `sessionEvent`）。
v2 帧头在 WebSocket 上取 `seq=0, frag_cnt=0` 时与 v1 语义等价（接收端与服务端同源升级，整体替换，不做线上双版本转换）。

## 传输

- 推流端 → 中继：`ws://<中继IP>:<端口>/ws/push`
- 接收端 → 中继：`ws://<中继IP>:<端口>/ws/watch?stream=<id>`
- 中继 → 接收端/推流端：文本帧为控制消息，二进制帧为媒体帧

> 传输层已抽象为可插拔接口（`Transport` / `DataSession`，见
> [plugin-architecture.md](plugin-architecture.md) §4）；本页描述的是
> WebSocket 传输上的线格式，其它传输（WebRTC/QUIC/SRT）复刻同一帧语义。

## 媒体帧（二进制）

头部固定 24 字节：

| 偏移 | 长度 | 字段 | 说明 |
|---|---|---|---|
| 0 | 4 | magic | `"STR2"` |
| 4 | 1 | version | `2` |
| 5 | 1 | track | `0`=视频，`1`=音频 |
| 6 | 1 | codec | `1`=H.264(Annex-B)，`2`=AAC(ADTS) |
| 7 | 1 | flags | 位标志，见下 |
| 8 | 4 | pts_ms | 演示时间戳（相对会话起点，u32 LE） |
| 12 | 4 | seq | 会话内单调递增帧序号（有损传输乱序检测/丢包统计；无损传输取 0） |
| 16 | 1 | frag_idx | 分片位置（`frag_cnt=0` 时无意义） |
| 17 | 1 | frag_cnt | 分片总数（`0` = 未分片） |
| 18 | 4 | len | 载荷长度（u32 LE） |
| 22 | 2 | reserved | 保留（flags 扩展） |
| 24 | len | payload | 原始编码数据 |

flags 位：

- `0x01` KEYFRAME：视频帧为 IDR 访问单元（含 SPS/PPS）
- `0x02` CONFIG：解码器配置数据
- `0x04` START：推流会话开始
- `0x08` END：推流会话结束

分片/重组是传输实现的事务（UDP 类传输切大关键帧用 `frag_*`；WS 整帧发送）。

## 控制消息（JSON 文本帧）

`{"type": ...}` 区分类型，字段 `camelCase`。

### 推流端 → 中继

```jsonc
{ "type": "hello",
  "streamId": "stross-abc", "title": "我的串流",
  "video": { "codec": "h264", "width": 1280, "height": 720, "fps": 30 },
  "audio": { "codec": "aac", "sampleRate": 48000, "channels": 2 } }
```

```json
{ "type": "bye" }
```

### 中继 → 推流端

```json
{ "type": "welcome", "streamId": "stross-abc" }
{ "type": "error", "message": "流 xxx 已存在" }
```

### 中继 → 接收端

```json
{ "type": "ready", "streamId": "stross-abc" }
{ "type": "error", "message": "流 xxx 不存在" }
```

### 能力协商（协议 v2 新增，会话建立前）

```jsonc
// 推流端/接收端上报能力
{ "type": "capabilities", "caps": [
  { "kind": "source", "media": ["screen", "mic"], "codecs": ["h264", "aac"],
    "transports": ["ws"], "maxWidth": 1920, "maxHeight": 1080,
    "preferredProfile": "lossy" } ] }

// 协商提议与应答
{ "type": "offer", "sessionId": "s1",
  "transports": [ { "transport": "ws", "addr": "ws://…/ws/push", "profile": "lossless" } ],
  "codecs": ["h264"], "profile": "lossy" }
{ "type": "answer", "sessionId": "s1",
  "transport": { "transport": "ws", "addr": "ws://…/ws/watch", "profile": "lossless" },
  "ok": true }
```

### 路由控制（协议 v2 新增）

```json
// 控制传输方向（会话存续期间动态改道）
{ "type": "route", "sessionId": "s1", "path": { "kind": "direct", "node": "b" } }
{ "type": "route", "sessionId": "s1", "path": { "kind": "viaRelay", "node": "relay-1" } }
{ "type": "route", "sessionId": "s1", "path": { "kind": "mesh", "nodes": ["b", "c"] } }

// 会话事件广播
{ "type": "sessionEvent", "sessionId": "s1", "event": "started" }
```

## HTTP 接口

| 路径 | 方法 | 说明 |
|---|---|---|
| `/healthz` | GET | 健康检查，返回 `ok` |
| `/api/info` | GET | 中继信息（srtPort / quicPort 等，接收端据此拼 SRT/QUIC 拨号地址） |
| `/api/streams` | GET | 流列表（含观看人数） |
| `/api/peers` | GET | 当前连接的对端（推流/观看）列表 |
| `/api/proxy` | POST | 级联代理：`{ "upstream", "streamId", "info"? }` 把上游中继的流拉到本地作虚拟流广播 |
| `/api/proxies` | GET | 级联代理列表 |
| `/api/webrtc/start` | POST | WebRTC 接收信令①：`{ "streamId" }` → `{ "peerId", "sdp" }`（标准 SDP offer） |
| `/api/webrtc/answer` | POST | WebRTC 接收信令②：`{ "peerId", "sdp" }`（接收端 answer） |
| `/ws/push` | WS | 推流端点 |
| `/ws/watch?stream=ID` | WS | 接收端点 |

> ① WebRTC 接收端（设计文档阶段 1）：中继为每个接收端创建一个 peer，
> 含 `control`（可靠、有序，控制消息）与 `media`（不可靠、乱序，媒体帧）两个
> data channel；媒体帧与 WS 线格式完全一致（24 字节 v2 帧头 + 载荷）。
> 接收端优先 WebRTC（低延迟），失败/超时自动回退 WS。
> 浏览器观看端页面（`/`、`/app.js`、`/jmuxer.js`）已随 D1 移除，接收端全部原生。

## 关键帧对齐规则（接收端接入语义）

1. 接收端连接 → 中继发 `Ready`；
2. 若有缓存的关键帧（含 SPS/PPS），先转发该帧；
3. 之后视频帧只在关键帧后转发（GOP 对齐）；音频 ADTS 自描述，随时可转；
4. 推流端断开 → 流移除，接收端连接关闭。

## 示例：推流端（最小实现）

```python
# pip install websockets
import asyncio, json, websockets

async def main():
    async with websockets.connect("ws://192.168.1.5:8777/ws/push") as ws:
        await ws.send(json.dumps({
            "type": "hello", "streamId": "demo", "title": "demo",
            "video": {"codec": "h264", "width": 640, "height": 360, "fps": 24},
        }))
        # 逐帧发送：24 字节头 + H.264 Annex-B 数据
        # 头: b"STR2" + b"\x02" + b"\x00" + b"\x01" + flags + pts(4) + seq(4)
        #      + frag_idx + frag_cnt + len(4) + reserved(2)

asyncio.run(main())
```
