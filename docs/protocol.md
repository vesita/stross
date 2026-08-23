# 协议文档

版本：1（`VERSION = 1`），小端序。

## 传输

- 推流端 → 中继：`ws://<中继IP>:<端口>/ws/push`
- 观看端 → 中继：`ws://<中继IP>:<端口>/ws/watch?stream=<id>`
- 中继 → 观看端/推流端：文本帧为控制消息，二进制帧为媒体帧

## 媒体帧（二进制）

头部固定 16 字节：

| 偏移 | 长度 | 字段 | 说明 |
|---|---|---|---|
| 0 | 4 | magic | `"STR1"` |
| 4 | 1 | version | `1` |
| 5 | 1 | track | `0`=视频，`1`=音频 |
| 6 | 1 | codec | `1`=H.264(Annex-B)，`2`=AAC(ADTS) |
| 7 | 1 | flags | 位标志，见下 |
| 8 | 4 | pts_ms | 演示时间戳（相对会话起点，u32 LE） |
| 12 | 4 | len | 载荷长度（u32 LE） |
| 16 | len | payload | 原始编码数据 |

flags 位：

- `0x01` KEYFRAME：视频帧为 IDR 访问单元（含 SPS/PPS）
- `0x02` CONFIG：解码器配置数据
- `0x04` START：推流会话开始
- `0x08` END：推流会话结束

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

### 中继 → 观看端

```json
{ "type": "ready", "streamId": "stross-abc" }
{ "type": "error", "message": "流 xxx 不存在" }
```

## HTTP 接口

| 路径 | 方法 | 说明 |
|---|---|---|
| `/` | GET | 观看端页面 |
| `/app.js` `/style.css` `/jmuxer.js` | GET | 观看端静态资源 |
| `/healthz` | GET | 健康检查，返回 `ok` |
| `/api/streams` | GET | 流列表（含观看人数） |
| `/ws/push` | WS | 推流端点 |
| `/ws/watch?stream=ID` | WS | 观看端点 |

## 关键帧对齐规则（观看端接入语义）

1. 观看端连接 → 中继发 `Ready`；
2. 若有缓存的关键帧（含 SPS/PPS），先转发该帧；
3. 之后视频帧只在关键帧后转发（GOP 对齐）；音频 ADTS 自描述，随时可转；
4. 推流端断开 → 流移除，观看端连接关闭。

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
        # 逐帧发送：16 字节头 + H.264 Annex-B 数据
        # 头: b"STR1" + b"\x01" + b"\x00" + b"\x01" + flags + pts(4) + len(4)

asyncio.run(main())
```
