//! `stross devices`：扫描局域网设备（PC + 手机），展示设备能力与在线共享状态。
//!
//! 数据来源（无头场景，不需要本机 `serve` 常驻）：
//! 1. mDNS 能力广播（F1.2）：`Discovery::browse` 拿设备名 / 角色 / 媒体 / 传输；
//! 2. 每设备 HTTP 探测：`/api/info`（SRT/QUIC 端口）与 `/api/streams`（在线共享，
//!    含观看者数）——与 GUI 设备卡片同源，命令行同样「打开即见 PC 与手机状态」。

use std::time::Duration;

use anyhow::Context;
use clap::Args;
use serde::{Deserialize, Serialize};
use stross_core::discovery::{BROWSE_TIMEOUT, Discovery};
use stross_core::net::local_ips;
use stross_proto::message::{DeviceSummary, DiscoveryInfo, MediaKind, RoleId};

#[derive(Args, Debug)]
pub struct DevicesArgs {
    /// 浏览窗口（秒），覆盖 mDNS resolve 重试预算
    #[arg(long, default_value_t = BROWSE_TIMEOUT.as_secs())]
    pub timeout: u64,
    /// 每设备 HTTP 探测超时（毫秒；不可达设备快速跳过）
    #[arg(long, default_value_t = 1500)]
    pub probe_ms: u64,
    /// JSON 输出（脚本化 / 管道）
    #[arg(long)]
    pub json: bool,
}

/// 每个设备 `/api/streams` 单条流的视图（`stross adb status` 复用）。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamView {
    pub(crate) stream_id: String,
    pub(crate) title: String,
    pub(crate) video: bool,
    pub(crate) audio: bool,
    pub(crate) watchers: u32,
}

/// 一个局域网设备的聚合状态（发现 + 探测）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceStatus {
    name: String,
    ip: String,
    port: u16,
    /// 是否本机（按本机局域网 IP 匹配）。
    is_self: bool,
    /// 角色（共享 / 接收 / 中继）。
    roles: Vec<String>,
    /// 可共享媒体（屏幕 / 麦克风 …）。
    media: Vec<String>,
    /// 支持的传输（WS / SRT / QUIC …）。
    transports: Vec<String>,
    /// 端点框架 L1：该节点公开的设备清单摘要（id/kind/name/是否已公开）。
    devices: Vec<DeviceSummary>,
    /// `/api/info` 可达（HTTP 探测成功）才为 true。
    online: bool,
    srt_port: Option<u16>,
    quic_port: Option<u16>,
    /// 该设备当前在线共享（点流可在 GUI 接收）。
    streams: Vec<StreamView>,
}

/// `/api/info` 响应（camelCase，与 stross-core relay/http.rs 一致；`stross adb status` 复用）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InfoResp {
    pub(crate) srt_port: Option<u16>,
    pub(crate) quic_port: Option<u16>,
}

/// `/api/streams` 响应：兼容裸数组（stross-core 中继实际形态）与
/// `{ "streams": [...] }` 包裹形态（前端同样双形态兼容）。
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum StreamsResp {
    Array(Vec<StreamInfoResp>),
    Object {
        #[serde(default)]
        streams: Vec<StreamInfoResp>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamInfoResp {
    pub(crate) stream_id: String,
    pub(crate) title: String,
    pub(crate) video: Option<serde_json::Value>,
    pub(crate) audio: Option<serde_json::Value>,
    pub(crate) watchers: u32,
}

pub async fn run(args: DevicesArgs) -> anyhow::Result<()> {
    let found = Discovery::browse(Duration::from_secs(args.timeout)).await?;
    let self_ips: Vec<String> = local_ips().into_iter().map(|ip| ip.to_string()).collect();
    let probe = Duration::from_millis(args.probe_ms);

    let mut devices: Vec<DeviceStatus> = Vec::new();
    // 同一实例可能按 A/AAAA 记录各触发一次 ServiceResolved——按实例名去重，
    // 地址优先取 IPv4（发现层已剔除 link-local；IPv6 前缀跨设备常不通）。
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for d in found {
        if let Some(&idx) = seen.get(&d.instance) {
            if devices[idx].ip.contains(':') && d.ip.is_ipv4() {
                devices[idx].ip = d.ip.to_string();
            }
            continue;
        }
        seen.insert(d.instance.clone(), devices.len());
        let info = DiscoveryInfo::from_txt(&d.txt);
        let ip = d.ip.to_string();
        let mut dev = DeviceStatus {
            name: info
                .as_ref()
                .map(|i| i.name.clone())
                .unwrap_or_else(|| d.instance.clone()),
            port: d.port,
            ip: ip.clone(),
            is_self: self_ips.contains(&ip),
            roles: info
                .as_ref()
                .map(|i| i.roles.iter().map(role_label).collect())
                .unwrap_or_default(),
            media: info
                .as_ref()
                .map(|i| i.media.iter().map(media_label).collect())
                .unwrap_or_default(),
            transports: info
                .as_ref()
                .map(|i| {
                    i.transports
                        .iter()
                        .map(|t| format!("{t:?}").to_uppercase())
                        .collect()
                })
                .unwrap_or_default(),
            devices: info.as_ref().map(|i| i.devices.clone()).unwrap_or_default(),
            online: false,
            srt_port: None,
            quic_port: None,
            streams: Vec::new(),
        };
        // 探测地址：本机走回环（局域网 IP 在部分网络栈上不可自连），
        // 对端走其广播 IP；两个请求独立超时，互不拖累
        let probe_ip = if dev.is_self {
            "127.0.0.1".to_string()
        } else {
            ip
        };
        if let Ok(resp) = http_get::<InfoResp>(&probe_ip, d.port, "/api/info", probe).await {
            dev.online = true;
            dev.srt_port = resp.srt_port;
            dev.quic_port = resp.quic_port;
        }
        if let Ok(resp) = http_get::<StreamsResp>(&probe_ip, d.port, "/api/streams", probe).await {
            let list = match resp {
                StreamsResp::Array(list) => list,
                StreamsResp::Object { streams } => streams,
            };
            dev.streams = list
                .into_iter()
                .map(|s| StreamView {
                    stream_id: s.stream_id,
                    title: s.title,
                    video: s.video.is_some(),
                    audio: s.audio.is_some(),
                    watchers: s.watchers,
                })
                .collect();
        }
        devices.push(dev);
    }
    // 本机优先，其余按名字排序——输出稳定，脚本可比对
    devices.sort_by(|a, b| b.is_self.cmp(&a.is_self).then(a.name.cmp(&b.name)));

    if args.json {
        println!("{}", serde_json::to_string_pretty(&devices)?);
        return Ok(());
    }
    println!(
        "局域网设备（{0} 秒扫描窗口，发现 {1} 台）",
        args.timeout,
        devices.len()
    );
    if devices.is_empty() {
        println!("  未发现设备（mDNS 广播未达？检查网络 / 对端是否已打开 Stross）");
        println!("  提示：手机经 USB 连接时，可运行 `stross adb status` 直接查手机状态");
        return Ok(());
    }
    for dev in &devices {
        print_device(dev);
    }
    Ok(())
}

fn print_device(dev: &DeviceStatus) {
    let tag = if dev.is_self { "本机" } else { "设备" };
    println!(
        "  {tag} {name}（{ip}:{port}）",
        name = dev.name,
        ip = dev.ip,
        port = dev.port
    );
    let caps: Vec<String> = [
        if dev.roles.is_empty() {
            None
        } else {
            Some(format!("角色={}", dev.roles.join("/")))
        },
        if dev.media.is_empty() {
            None
        } else {
            Some(format!("可共享={}", dev.media.join("/")))
        },
        if dev.transports.is_empty() {
            None
        } else {
            Some(format!("传输={}", dev.transports.join("/")))
        },
    ]
    .into_iter()
    .flatten()
    .collect();
    if !caps.is_empty() {
        println!("      {}", caps.join(" · "));
    }
    if !dev.devices.is_empty() {
        let list: Vec<String> = dev
            .devices
            .iter()
            .map(|d| format!("{}{}", d.name, if d.published { "（已公开）" } else { "" }))
            .collect();
        println!("      设备: {}", list.join(" / "));
    }
    if let Some(srt) = dev.srt_port {
        println!("      SRT {srt}");
    } else {
        println!("      SRT -");
    }
    if let Some(quic) = dev.quic_port {
        println!("      QUIC {quic}");
    } else {
        println!("      QUIC -");
    }
    println!(
        "      在线共享 {} 条{}",
        dev.streams.len(),
        if dev.online {
            ""
        } else {
            "（HTTP 探测不可达）"
        }
    );
    for s in &dev.streams {
        let kinds = match (s.video, s.audio) {
            (true, true) => "视频+音频",
            (true, false) => "视频",
            (false, true) => "音频",
            (false, false) => "?",
        };
        println!(
            "        [{kinds}] {}「{}」watchers={}",
            s.stream_id, s.title, s.watchers
        );
    }
}

/// 最小 HTTP GET：raw TCP 一发一收（避免为 CLI 引入 HTTP 客户端依赖），
/// 响应体按 JSON 解析。超时由调用方控制（不可达设备快速失败）。
/// `stross adb status` 经 USB 通道复用同一探测器。
pub(crate) async fn http_get<T: for<'de> Deserialize<'de>>(
    ip: &str,
    port: u16,
    path: &str,
    timeout: Duration,
) -> anyhow::Result<T> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let stream = tokio::time::timeout(timeout, tokio::net::TcpStream::connect((ip, port)))
        .await
        .context("连接失败")??;
    stream.set_nodelay(true).ok();
    let mut stream = stream;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {ip}:{port}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );
    tokio::time::timeout(timeout, stream.write_all(req.as_bytes()))
        .await
        .context("发送请求失败")??;
    let mut buf = Vec::new();
    tokio::time::timeout(timeout, stream.read_to_end(&mut buf))
        .await
        .context("读取响应失败")??;
    let body = String::from_utf8_lossy(&buf);
    let body = body.split("\r\n\r\n").nth(1).unwrap_or("");
    serde_json::from_str(body).context("响应解析失败")
}

fn role_label(r: &RoleId) -> String {
    match r {
        RoleId::Sender => "共享".into(),
        RoleId::Viewer => "接收".into(),
        RoleId::Relay => "中继".into(),
        RoleId::Controller => "控制".into(),
    }
}

fn media_label(m: &MediaKind) -> String {
    match m {
        MediaKind::Screen => "屏幕".into(),
        MediaKind::Window => "窗口".into(),
        MediaKind::Camera => "摄像头".into(),
        MediaKind::Mic => "麦克风".into(),
        MediaKind::SystemAudio => "系统声".into(),
        MediaKind::Input => "输入".into(),
        MediaKind::Clipboard => "剪贴板".into(),
        MediaKind::File => "文件".into(),
        MediaKind::Service => "服务".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_and_media_labels() {
        assert_eq!(role_label(&RoleId::Relay), "中继");
        assert_eq!(media_label(&MediaKind::Mic), "麦克风");
    }

    #[test]
    fn parses_minimal_http_json_body() {
        // http_get 需要真实 socket；此处仅验证响应切分逻辑的输入形态
        // （body 切分在函数内，无法单测——保持集成脚本覆盖）。
        let raw = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"port\":18777}";
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["port"], 18777);
    }

    #[test]
    fn streams_resp_accepts_plain_array_and_object() {
        // 对齐前端兼容逻辑：/api/streams 可能是裸数组或 { "streams": [...] }
        let plain: StreamsResp = serde_json::from_str(
            r#"[{"streamId":"s1","title":"t","video":null,"audio":null,"watchers":2}]"#,
        )
        .unwrap();
        match plain {
            StreamsResp::Array(list) => assert_eq!(list[0].watchers, 2),
            _ => panic!("裸数组应解析为 Array 形态"),
        }
        let obj: StreamsResp = serde_json::from_str(
            r#"{"streams":[{"streamId":"s1","title":"t","video":null,"audio":null,"watchers":2}]}"#,
        )
        .unwrap();
        match obj {
            StreamsResp::Object { streams } => {
                assert_eq!(streams.len(), 1);
                assert_eq!(streams[0].stream_id, "s1");
            }
            _ => panic!("包裹形态应解析为 Object 形态"),
        }
    }
}
