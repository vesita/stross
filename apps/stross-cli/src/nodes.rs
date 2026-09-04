//! `stross nodes`：扫描局域网节点（PC + 手机），展示节点能力与在线共享状态。
//!
//! 分层（docs/layering-architecture.md）：**聚合与探测收敛在
//! `stross_kernel::discovery::scan_lan`**（内核层，CLI 与 GUI 共用）；本文件只做
//! **参数解析 + 中文标签 + 展示**。

use std::time::Duration;

use clap::Args;
use stross_kernel::discovery::BROWSE_TIMEOUT;
use stross_kernel::discovery::ScannedNode;
use stross_proto::message::{MediaKind, RoleId};

#[derive(Args, Debug)]
pub struct NodesArgs {
    /// 浏览窗口（秒），覆盖 mDNS resolve 重试预算
    #[arg(long, default_value_t = BROWSE_TIMEOUT.as_secs())]
    pub timeout: u64,
    /// 每节点 HTTP 探测超时（毫秒；不可达节点快速跳过）
    #[arg(long, default_value_t = 1500)]
    pub probe_ms: u64,
    /// JSON 输出（脚本化 / 管道）
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: NodesArgs) -> anyhow::Result<()> {
    let browse = Duration::from_secs(args.timeout);
    let probe = Duration::from_millis(args.probe_ms);
    let nodes = stross_kernel::discovery::scan_lan(browse, probe, Vec::new()).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
        return Ok(());
    }
    println!(
        "局域网节点（{0} 秒扫描窗口，发现 {1} 个）",
        args.timeout,
        nodes.len()
    );
    if nodes.is_empty() {
        println!("  未发现节点（mDNS 广播未达？检查网络 / 对端是否已打开 Stross）");
        println!("  提示：手机经 USB 连接时，可运行 `stross adb status` 直接查手机状态");
        return Ok(());
    }
    for node in &nodes {
        print_node(node);
    }
    Ok(())
}

fn print_node(node: &ScannedNode) {
    let tag = if node.is_self { "本机" } else { "节点" };
    println!(
        "  {tag} {name}（{ip}:{port}）",
        name = node.name,
        ip = node.ip,
        port = node.port
    );
    let caps: Vec<String> = [
        if node.roles.is_empty() {
            None
        } else {
            Some(format!(
                "角色={}",
                node.roles
                    .iter()
                    .map(role_label)
                    .collect::<Vec<_>>()
                    .join("/")
            ))
        },
        if node.media.is_empty() {
            None
        } else {
            Some(format!(
                "可共享={}",
                node.media
                    .iter()
                    .map(media_label)
                    .collect::<Vec<_>>()
                    .join("/")
            ))
        },
        if node.transports.is_empty() {
            None
        } else {
            Some(format!(
                "传输={}",
                node.transports
                    .iter()
                    .map(|t| format!("{t:?}").to_uppercase())
                    .collect::<Vec<_>>()
                    .join("/")
            ))
        },
    ]
    .into_iter()
    .flatten()
    .collect();
    if !caps.is_empty() {
        println!("      {}", caps.join(" · "));
    }
    if !node.endpoints.is_empty() {
        let list: Vec<String> = node
            .endpoints
            .iter()
            .map(|e| {
                let avail = if e.available { "" } else { "（不可用）" };
                let pub_ = if e.published { "（已通告）" } else { "" };
                format!("{}{}{}", e.name, avail, pub_)
            })
            .collect();
        println!("      端点: {}", list.join(" / "));
    }
    if let Some(srt) = node.srt_port {
        println!("      SRT {srt}");
    } else {
        println!("      SRT -");
    }
    if let Some(quic) = node.quic_port {
        println!("      QUIC {quic}");
    } else {
        println!("      QUIC -");
    }
    println!(
        "      在线共享 {} 条{}",
        node.streams.len(),
        if node.online {
            ""
        } else {
            "（HTTP 探测不可达）"
        }
    );
    for s in &node.streams {
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
        MediaKind::SystemAudio => "系统声音".into(),
        MediaKind::File => "文件".into(),
        MediaKind::Clipboard => "剪贴板".into(),
        MediaKind::Input => "输入".into(),
        MediaKind::Service => "服务".into(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use stross_kernel::discovery::StreamView;
    use stross_proto::StreamId;

    #[test]
    fn role_and_media_labels() {
        assert_eq!(role_label(&RoleId::Relay), "中继");
        assert_eq!(media_label(&MediaKind::Mic), "麦克风");
    }

    #[test]
    fn caps_fmt() {
        let node = ScannedNode {
            name: "x".into(),
            ip: "192.168.1.5".into(),
            port: 18777,
            is_self: false,
            roles: vec![RoleId::Sender],
            media: vec![MediaKind::Screen],
            transports: vec![],
            endpoints: vec![],
            online: true,
            srt_port: None,
            quic_port: None,
            streams: vec![StreamView {
                stream_id: StreamId::from("s"),
                title: "t".into(),
                video: true,
                audio: false,
                watchers: 1,
            }],
        };
        let _ = serde_json::to_string(&node).unwrap(); // JSON 输出可序列化
    }
}
