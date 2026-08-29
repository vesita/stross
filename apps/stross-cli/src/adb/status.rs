//! 手机状态聚合 + 展示视图（经 adb forward 直通中继 HTTP；探测契约复用
//! `stross_kernel::relay::client`，展示投影复用 `stross_kernel::devices`）。

use std::time::Duration;

use serde::Serialize;
use stross_kernel::relay::client as relay_http;

use stross_kernel::devices::StreamView;

use super::device::{adb_forward, adb_forward_remove, adb_sh, free_local_port, pick_device};

/// 手机运行状态聚合（与 `stross devices` 的 ScannedDevice 同构，USB 通道来源）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneStatus {
    serial: String,
    model: String,
    android: String,
    /// wlan0 IPv4（LAN 身份；AP 隔离时不可达，仅作信息展示）。
    wifi_ip: Option<String>,
    online: bool,
    relay_port: Option<u16>,
    srt_port: Option<u16>,
    quic_port: Option<u16>,
    streams: Vec<StreamView>,
}

/// 探测已连接手机的状态（`adb forward` 直通中继 /api/info + /api/streams）。
pub(crate) async fn phone_status(ports_arg: &str) -> anyhow::Result<PhoneStatus> {
    let ports: Vec<u16> = ports_arg
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    let serial = pick_device().await?;
    let model = adb_sh(&serial, "getprop ro.product.model")
        .await
        .unwrap_or_default();
    let android = adb_sh(&serial, "getprop ro.build.version.release")
        .await
        .unwrap_or_default();
    let wifi_ip = adb_sh(&serial, "ip -4 addr show wlan0")
        .await
        .ok()
        .and_then(|out| {
            out.lines()
                .find(|l| l.trim_start().starts_with("inet "))
                .map(|l| {
                    l.split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .split('/') // 去掉 /24 前缀长度
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
        })
        .filter(|ip| !ip.is_empty());

    // 经 adb forward 探测中继 HTTP：先在 PC 侧占一个空闲监听端口
    let local_port = free_local_port();
    let mut status = PhoneStatus {
        serial: serial.clone(),
        model: model.trim().to_string(),
        android: android.trim().to_string(),
        wifi_ip: wifi_ip.clone(),
        online: false,
        relay_port: None,
        srt_port: None,
        quic_port: None,
        streams: Vec::new(),
    };
    for relay_port in &ports {
        if !adb_forward(&serial, local_port, *relay_port).await? {
            continue; // forward 失败，换下一个候选端口
        }
        let probe = Duration::from_millis(1500);
        if let Ok(info) = relay_http::info("127.0.0.1", local_port, probe).await {
            status.online = true;
            status.relay_port = Some(*relay_port);
            status.srt_port = info.srt_port;
            status.quic_port = info.quic_port;
            // /api/streams（同一 forward 会话）
            if let Ok(list) = relay_http::streams("127.0.0.1", local_port, probe).await {
                status.streams = stross_kernel::devices::to_views(list);
            }
        } else {
            // 该端口不是中继（或无 HTTP），清理后试下一个
        }
        let _ = adb_forward_remove(&serial, local_port).await;
        if status.online {
            break;
        }
    }
    Ok(status)
}

pub(crate) fn print_status(s: &PhoneStatus) {
    println!("手机状态（经 USB/adb，serial={}）", s.serial);
    println!(
        "  型号      {}",
        if s.model.is_empty() { "?" } else { &s.model }
    );
    println!(
        "  系统      Android {}",
        if s.android.is_empty() {
            "?"
        } else {
            &s.android
        }
    );
    match &s.wifi_ip {
        Some(ip) => println!("  WiFi IP   {ip}"),
        None => println!("  WiFi IP   未获取到 wlan0 IPv4"),
    }
    if !s.online {
        println!("  中继      未探测到（手机未运行 Stross？或中继端口非 8777/18777）");
        return;
    }
    let srt = s.srt_port.map_or_else(|| "-".into(), |p| p.to_string());
    let quic = s.quic_port.map_or_else(|| "-".into(), |p| p.to_string());
    println!(
        "  中继      ws://{}:{}（SRT {srt} · QUIC {quic}）",
        s.wifi_ip.as_deref().unwrap_or("<ip>"),
        s.relay_port.unwrap_or(0)
    );
    println!("  在线共享  {} 条", s.streams.len());
    for st in &s.streams {
        let kinds = match (st.video, st.audio) {
            (true, true) => "视频+音频",
            (true, false) => "视频",
            (false, true) => "音频",
            (false, false) => "?",
        };
        println!(
            "    [{kinds}] {}「{}」watchers={}",
            st.stream_id, st.title, st.watchers
        );
    }
    println!("  提示      手机与 PC 同网段时 `stross devices` 也能发现；AP 隔离时用本命令");
}
