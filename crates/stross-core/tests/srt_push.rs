//! 集成测试：SRT 推流端（阶段 2，rsrt 纯 Rust；设计文档 §4.4 Adaptive）。
//!
//! 推流端走 SRT（新路径），观看端走 WebSocket（现有路径）——中继的
//! `handle_push` / `handle_watch` 对第三条传输原样复用（传输抽象第三次验证）。
//! 大关键帧（> SRT 单消息上限 1400B）经 SRT 分片 → relay 重组 → 广播，
//! 观看端收到的应是完整帧（逐字节一致）。

use std::time::Duration;

use futures_util::StreamExt;
use stross_core::relay::RelayServer;
use stross_core::transport::srt::SrtTransport;
use stross_core::transport::{PeerAddr, SessionPacket, SessionParams, Transport};
use stross_proto::frame::{CODEC_AAC, CODEC_H264, FLAG_KEYFRAME, Frame, TRACK_AUDIO, TRACK_VIDEO};
use stross_proto::message::{ControlMessage, TrackInfo};
use tokio_tungstenite::tungstenite::Message;

/// 大关键帧：5000 字节载荷（模拟 1080p IDR），SRT 单消息上限 1400B → 必分片。
fn big_keyframe() -> Frame {
    let payload: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
    Frame::new(TRACK_VIDEO, CODEC_H264, FLAG_KEYFRAME, 0, payload)
}

fn hello() -> ControlMessage {
    ControlMessage::Hello {
        stream_id: "srt-stream".into(),
        title: "SRT 测试流".into(),
        video: Some(TrackInfo {
            codec: "h264".into(),
            width: Some(1920),
            height: Some(1080),
            fps: Some(30),
            sample_rate: None,
            channels: None,
        }),
        audio: Some(TrackInfo {
            codec: "aac".into(),
            width: None,
            height: None,
            fps: None,
            sample_rate: Some(48000),
            channels: Some(2),
        }),
    }
}

#[tokio::test]
async fn srt_push_reaches_ws_watcher() {
    let handle = RelayServer::start(0).await.unwrap();
    let srt_port = handle.srt_port.expect("relay 应启用 SRT 监听");

    // ---- SRT 推流端 ----
    let transport = SrtTransport::new();
    let peer = PeerAddr {
        transport: "srt".into(),
        addr: format!("srt://127.0.0.1:{srt_port}"),
    };
    let params = SessionParams {
        session_id: "srt-stream".into(),
        profile: stross_proto::message::ReliabilityProfile::Adaptive,
    };
    let push = transport
        .connect(&peer, &params)
        .await
        .expect("SRT 连接 relay");

    // Hello → relay 建流 → Welcome 回执
    push.send(SessionPacket::Control(hello())).await.unwrap();
    let welcome = tokio::time::timeout(Duration::from_secs(5), push.recv())
        .await
        .expect("等 Welcome 超时")
        .expect("Welcome recv 出错")
        .expect("Welcome 不应为关闭");
    assert!(
        matches!(
            welcome,
            SessionPacket::Control(ControlMessage::Welcome { .. })
        ),
        "应收到 Welcome，得到 {welcome:?}"
    );

    // 大关键帧（分片）+ 音频帧
    let big = big_keyframe();
    push.send(SessionPacket::Media(big.clone())).await.unwrap();
    push.send(SessionPacket::Media(Frame::new(
        TRACK_AUDIO,
        CODEC_AAC,
        0,
        0,
        vec![0xFF, 0xF1, 0x50, 0x00, 0x01, 0x1F, 0xFC, 0x00],
    )))
    .await
    .unwrap();

    // ---- WS 观看端（现有路径）----
    let (mut watch, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{}/ws/watch?stream=srt-stream",
        handle.port
    ))
    .await
    .expect("连接观看端点");

    // 首个消息是 Ready
    let first = watch.next().await.unwrap().unwrap();
    let ready = ControlMessage::from_text(&first.into_text().unwrap()).unwrap();
    assert_eq!(
        ready,
        ControlMessage::Ready {
            stream_id: "srt-stream".into()
        }
    );

    // 新观众先收最近关键帧（last_keyframe 缓存）→ 断言重组后的完整帧
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut got_keyframe: Option<Vec<u8>> = None;
    while tokio::time::Instant::now() < deadline && got_keyframe.is_none() {
        let msg = tokio::time::timeout(Duration::from_secs(5), watch.next())
            .await
            .expect("收帧超时")
            .unwrap()
            .unwrap();
        if let Message::Binary(b) = msg {
            let frame = Frame::from_bytes(&b).expect("观看端帧解析失败");
            if frame.header.track == TRACK_VIDEO && frame.header.is_keyframe() {
                got_keyframe = Some(frame.payload.to_vec());
            }
        }
    }
    let kf = got_keyframe.expect("应收到关键帧");
    assert_eq!(
        kf,
        big.payload.to_vec(),
        "SRT 分片推流 → relay 重组 → 观看端应收到完整关键帧（逐字节一致）"
    );

    handle.stop().await;
}
