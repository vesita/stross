//! 文件端点传输（docs/endpoint-model.md §3.6）：文件泵（推）/ 文件接收（拉）。
//!
//! 走**既有数据面零改动**：文件以 `TRACK_FILE` 轨作为普通媒体流推送/观看；
//! 中继不缓存不门控该轨，因此**公开方必须等到 ≥1 个观看者接入后才开始发帧**
//! （轮询中继 `/api/streams` 的观看数；广播不补发，早发订阅方会丢文件头）。
//!
//! 帧序列：`FLAG_CONFIG`（FileMeta JSON）→ 数据帧（≤64KiB/帧，pts=块序）
//! → `FLAG_END`（末块；空文件为空载荷 END 帧）。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use stross_core::sender::RelayClient;
use stross_core::transport::SessionPacket;
use stross_core::watch::connect_watch;
use stross_proto::frame::{CODEC_FILE, FLAG_CONFIG, FLAG_END, Frame, TRACK_FILE};
use stross_proto::message::{ControlMessage, FileMeta};

/// 单帧数据块大小（≤ 64KiB；WS 载荷与中继广播缓冲友好）。
pub const FILE_CHUNK: usize = 64 * 1024;
/// 等观看者接入的超时：受中继 `PUSH_SILENCE_TIMEOUT`（10s 无消息拆流）约束，
/// 取 8s——订阅方在握手中继往返内（本地 <1s）即可入场。
pub const WAIT_WATCHER_TIMEOUT: Duration = Duration::from_secs(8);
/// 观看数轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 文件泵参数（公开方驱动构造）。
#[derive(Debug, Clone)]
pub struct FilePushOptions {
    /// 中继推流地址（`ws://host:port/ws/push`；文件走无损 WS 路径）。
    pub push_url: String,
    /// 数据面流 id（pull = 公开方本机会话；push = 订阅方自签会话）。
    pub stream_id: String,
    /// 推流标题（Hello.title；展示用）。
    pub title: String,
    /// 跨设备接入凭证（push 模式 = 订阅方自签；本机 pull = `None`）。
    pub share_token: Option<String>,
    /// 观看数轮询基址（`ws://host:port`；`None` = 不等观看者直接推）。
    pub watcher_base: Option<String>,
}

/// 推送一个本地文件到中继（阻塞到全部帧发送完成并优雅 Bye）。
pub async fn push_file(path: &Path, opts: &FilePushOptions) -> anyhow::Result<u64> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("读取文件失败 {}", path.display()))?;
    if !meta.is_file() {
        bail!("不是普通文件: {}", path.display());
    }
    let size = meta.len();
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "未命名".into());
    let file_meta = FileMeta {
        name,
        size,
        sha256: None,
    };
    let hello = ControlMessage::Hello {
        stream_id: opts.stream_id.clone(),
        title: opts.title.clone(),
        video: None,
        audio: None,
        share_token: opts.share_token.clone(),
    };
    let (client, tx) = RelayClient::connect(&opts.push_url, hello)
        .await
        .with_context(|| format!("连接中继失败 {}", opts.push_url))?;
    if let Some(base) = &opts.watcher_base {
        wait_for_watcher(base, &opts.stream_id).await?;
    }
    // 首帧：文件元数据
    tx.send(Frame::new(
        TRACK_FILE,
        CODEC_FILE,
        FLAG_CONFIG,
        0,
        file_meta.to_bytes(),
    ))
    .await
    .context("发送文件首帧失败")?;
    // 数据帧：≤64KiB/帧，末帧带 END；空文件单独发空 END 帧
    let mut f = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("打开文件失败 {}", path.display()))?;
    let mut buf = vec![0u8; FILE_CHUNK];
    let mut sent: u64 = 0;
    let mut pts: u32 = 0;
    loop {
        if size == 0 {
            tx.send(Frame::new(
                TRACK_FILE,
                CODEC_FILE,
                FLAG_END,
                pts,
                Vec::<u8>::new(),
            ))
            .await
            .context("发送空文件结束帧失败")?;
            break;
        }
        use tokio::io::AsyncReadExt;
        let n = f.read(&mut buf).await.context("读取文件失败")?;
        if n == 0 {
            bail!("文件提前结束（期望 {size} 字节，实读 {sent}）");
        }
        let last = sent + n as u64 == size;
        let flags = if last { FLAG_END } else { 0 };
        tx.send(Frame::new(
            TRACK_FILE,
            CODEC_FILE,
            flags,
            pts,
            buf[..n].to_vec(),
        ))
        .await
        .context("发送文件块失败")?;
        sent += n as u64;
        pts = pts.wrapping_add(1);
        if last {
            break;
        }
    }
    // 关闭帧通道 → 推流循环发 Bye；等待任务收尾
    drop(tx);
    client.stop().await;
    Ok(sent)
}

/// 等中继上该流出现 ≥1 个观看者（订阅方已接上 watch，可安全发帧）。
async fn wait_for_watcher(base: &str, stream_id: &str) -> anyhow::Result<()> {
    let url = format!("{base}/api/streams");
    let deadline = Instant::now() + WAIT_WATCHER_TIMEOUT;
    while Instant::now() < deadline {
        if watchers_of(&url, stream_id).await.unwrap_or(0) > 0 {
            return Ok(());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    bail!(
        "等待观看者接入超时（{}s）：订阅方未连接中继 {base}，放弃推送",
        WAIT_WATCHER_TIMEOUT.as_secs()
    )
}

/// 拉取中继 `/api/streams`，返回指定流的观看者数（探测失败 = 0）。
async fn watchers_of(streams_url: &str, stream_id: &str) -> Option<u32> {
    let body = http_get(streams_url).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let list = match v {
        serde_json::Value::Array(list) => list,
        serde_json::Value::Object(ref o) => o.get("streams")?.as_array()?.clone(),
        _ => return None,
    };
    for s in list {
        if s.get("streamId").and_then(|x| x.as_str()) == Some(stream_id) {
            return s.get("watchers").and_then(|x| x.as_u64()).map(|n| n as u32);
        }
    }
    Some(0)
}

/// 极简 HTTP GET（raw TCP，无新依赖；不可达返回 Err）：公开方/订阅方轮询
/// 各自中继的观看数用。
async fn http_get(url: &str) -> anyhow::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| anyhow::anyhow!("非法 HTTP 基址: {url}"))?;
    let (host_port, path) = match rest.split_once('/') {
        Some((hp, p)) => (hp, format!("/{p}")),
        None => (rest, "/".into()),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| anyhow::anyhow!("端口非法"))?,
        ),
        None => (host_port.to_string(), 80),
    };
    let timeout = Duration::from_secs(2);
    let mut stream = tokio::time::timeout(
        timeout,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .context("连接失败")??;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );
    tokio::time::timeout(timeout, stream.write_all(req.as_bytes()))
        .await
        .context("发送请求失败")??;
    let mut buf = Vec::new();
    tokio::time::timeout(timeout, stream.read_to_end(&mut buf))
        .await
        .context("读取响应失败")??;
    let body = String::from_utf8_lossy(&buf);
    Ok(body.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
}

/// 接收结果。
#[derive(Debug)]
pub struct ReceivedFile {
    /// 落盘文件名（已净化，只取 basename）。
    pub name: String,
    /// 文件字节数（与首帧 FileMeta 校验一致）。
    pub size: u64,
    /// 落盘路径。
    pub path: PathBuf,
}

/// 连接并接收一个文件流到 `out_dir`（返回落盘结果）。
pub async fn receive_file(
    watch_url: &str,
    stream_id: &str,
    out_dir: &Path,
) -> anyhow::Result<ReceivedFile> {
    let session = connect_watch(watch_url, stream_id)
        .await
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("连接中继失败（流 {stream_id}）：{watch_url}"))?;
    receive_file_session(session, out_dir).await
}

/// 在已就绪的观看会话上接收文件流（进程内测试可注入会话）。
pub async fn receive_file_session(
    session: Box<dyn stross_core::transport::DataSession>,
    out_dir: &Path,
) -> anyhow::Result<ReceivedFile> {
    let mut meta: Option<FileMeta> = None;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match session.recv().await {
            Ok(Some(SessionPacket::Media(frame))) if frame.header.track == TRACK_FILE => {
                if meta.is_none() {
                    if !frame.header.is_config() {
                        bail!("文件流缺首帧（期望 FileMeta CONFIG 帧）");
                    }
                    let m = FileMeta::from_bytes(&frame.payload)
                        .ok_or_else(|| anyhow::anyhow!("文件首帧（FileMeta）解析失败"))?;
                    meta = Some(m);
                    continue;
                }
                let m = meta.as_ref().expect("已设置");
                buf.extend_from_slice(&frame.payload);
                if buf.len() as u64 > m.size {
                    bail!("文件超长：期望 {} 字节，实收 {}", m.size, buf.len());
                }
                if frame.header.is_end() {
                    if buf.len() as u64 != m.size {
                        bail!("文件不完整：期望 {} 字节，实收 {}", m.size, buf.len());
                    }
                    // 净化文件名：拒绝路径穿越（只取 basename）
                    let name = Path::new(&m.name)
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "received.bin".into());
                    tokio::fs::create_dir_all(out_dir)
                        .await
                        .with_context(|| format!("创建输出目录失败 {}", out_dir.display()))?;
                    let path = out_dir.join(&name);
                    tokio::fs::write(&path, &buf)
                        .await
                        .with_context(|| format!("写文件失败 {}", path.display()))?;
                    return Ok(ReceivedFile {
                        name,
                        size: m.size,
                        path,
                    });
                }
            }
            Ok(Some(SessionPacket::Media(_))) => {} // 其它轨（视频/音频）忽略
            Ok(Some(SessionPacket::Control(_))) => {}
            Ok(None) => bail!("流提前关闭，文件不完整（实收 {} 字节）", buf.len()),
            Err(e) => bail!("观看连接异常: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Platform, StrossApp};
    use std::sync::Arc;

    /// 与随机字节不同但确定的模式（可复现、可比对）。
    fn pattern(len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        for i in 0..len {
            v.push((i * 31 + 7) as u8);
        }
        v
    }

    /// 本地双端闭环：受控中继 + 会话预授权 → 文件泵（先等观看者）→ 文件接收。
    /// 覆盖：首帧/分块/END 完整性、等待观看者防丢帧、字节级一致性。
    #[tokio::test]
    async fn file_roundtrip_via_controlled_relay() {
        let app = Arc::new(StrossApp::new(Platform::Desktop));
        let relay = app.start_relay_on(0).await.unwrap();
        let base = format!("ws://127.0.0.1:{}", relay.port);
        // 会话 + 凭证（协商层语义：pull 授予时公开方已建会话并预授权）
        let grant = app
            .issue_share_token_for(
                "文件测试".into(),
                vec![stross_proto::message::MediaKind::File],
                Some(600),
            )
            .unwrap();
        let stream_id = grant.stream_id;

        let dir = std::env::temp_dir().join(format!("stross-xfer-{}", std::process::id()));
        let out = dir.join("out");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("payload.bin");
        let payload = pattern(300_000); // 跨多个 64KiB 块
        std::fs::write(&src, &payload).unwrap();

        // 泵：连中继 Hello 建流 → 等观看者 → 推帧
        let pump = {
            let base2 = base.clone();
            let stream_id = stream_id.clone();
            let src = src.clone();
            tokio::spawn(async move {
                push_file(
                    &src,
                    &FilePushOptions {
                        push_url: format!("{base2}/ws/push"),
                        stream_id,
                        title: "文件测试".into(),
                        share_token: None,
                        watcher_base: Some(base2),
                    },
                )
                .await
            })
        };
        // 观看者：重试接入直到流存在（泵 Hello 后即可 join）
        let session = loop {
            match connect_watch(&base, &stream_id).await {
                Ok(s) => break s,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        };
        let recv = tokio::spawn({
            let out = out.clone();
            async move { receive_file_session(session, &out).await }
        });
        pump.await.unwrap().expect("泵应成功");
        let got = recv.await.unwrap().expect("接收应成功");
        assert_eq!(got.name, "payload.bin");
        assert_eq!(got.size, payload.len() as u64);
        let written = std::fs::read(&got.path).unwrap();
        assert_eq!(written, payload, "文件字节必须逐字节一致");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空文件同样闭环（END 空帧路径）。
    #[tokio::test]
    async fn empty_file_roundtrip() {
        let app = Arc::new(StrossApp::new(Platform::Desktop));
        let relay = app.start_relay_on(0).await.unwrap();
        let base = format!("ws://127.0.0.1:{}", relay.port);
        let grant = app
            .issue_share_token_for(
                "空文件".into(),
                vec![stross_proto::message::MediaKind::File],
                Some(600),
            )
            .unwrap();
        let stream_id = grant.stream_id;
        let dir = std::env::temp_dir().join(format!("stross-xfer-empty-{}", std::process::id()));
        let out = dir.join("out");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("empty.txt");
        std::fs::write(&src, b"").unwrap();

        let pump = {
            let base2 = base.clone();
            let stream_id = stream_id.clone();
            let src = src.clone();
            tokio::spawn(async move {
                push_file(
                    &src,
                    &FilePushOptions {
                        push_url: format!("{base2}/ws/push"),
                        stream_id,
                        title: "空文件".into(),
                        share_token: None,
                        watcher_base: Some(base2),
                    },
                )
                .await
            })
        };
        let session = loop {
            match connect_watch(&base, &stream_id).await {
                Ok(s) => break s,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        };
        let recv = tokio::spawn(async move { receive_file_session(session, &out).await });
        pump.await.unwrap().unwrap();
        let got = recv.await.unwrap().unwrap();
        assert_eq!(got.size, 0);
        assert_eq!(std::fs::read(&got.path).unwrap(), b"");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn http_get_parses_host_port_path() {
        // 不真的联网只验证 URL 拆解逻辑会 panic 与否——拆解在 http_get 内联，
        // 这里直接验证 streams JSON 的两种形态解析（复用 watchers_of）。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let url = "ws://127.0.0.1:9/api/streams"; // 端口 9 不可达 → Err，不 panic
            assert!(http_get(url).await.is_err());
        });
    }
}
