//! 集成测试：QUIC 推流端（阶段 2 后续，quinn + rustls-ring）。
//!
//! 推流端走 QUIC（新路径：control/media 双 stream 多路复用，Lossless），
//! 观看端走 WebSocket（现有路径）——中继的 `handle_push` / `handle_watch`
//! 对第四条传输原样复用（传输抽象第四次验证）。QUIC 无单消息大小限制，
//! 100KB 关键帧整体发送，观看端收到逐字节一致的完整帧。

use std::time::Duration;

use futures_util::StreamExt;
use stross_core::relay::RelayServer;
use stross_core::transport::quic::QuicTransport;
use stross_core::transport::{PeerAddr, SessionPacket, SessionParams, Transport};
use stross_proto::frame::{CODEC_AAC, CODEC_H264, FLAG_KEYFRAME, Frame, TRACK_AUDIO, TRACK_VIDEO};
use stross_proto::message::{CodecId, ControlMessage, TrackInfo, TransportId};
use tokio_tungstenite::tungstenite::Message;

/// 大关键帧：100KB 载荷（模拟高码率 1080p IDR）；QUIC 整体发送不分片。
fn big_keyframe() -> Frame {
    let payload: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
    Frame::new(TRACK_VIDEO, CODEC_H264, FLAG_KEYFRAME, 0, payload)
}

fn hello() -> ControlMessage {
    ControlMessage::Hello {
        stream_id: "quic-stream".into(),
        title: "QUIC 测试流".into(),
        video: Some(TrackInfo {
            codec: CodecId::H264,
            width: Some(1920),
            height: Some(1080),
            fps: Some(30),
            sample_rate: None,
            channels: None,
        }),
        audio: Some(TrackInfo {
            codec: CodecId::Aac,
            width: None,
            height: None,
            fps: None,
            sample_rate: Some(48000),
            channels: Some(2),
        }),
    }
}

#[tokio::test]
async fn quic_push_reaches_ws_watcher() {
    let handle = RelayServer::start(0).await.unwrap();
    let quic_port = handle.quic_port.expect("relay 应启用 QUIC 监听");

    // ---- QUIC 推流端 ----
    let transport = QuicTransport::new();
    let peer = PeerAddr {
        transport: TransportId::Quic,
        addr: format!("quic://127.0.0.1:{quic_port}"),
    };
    let params = SessionParams {
        session_id: "quic-stream".into(),
        profile: stross_proto::message::ReliabilityProfile::Lossless,
    };
    let push = transport
        .connect(&peer, &params)
        .await
        .expect("QUIC 连接 relay");

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

    // 大关键帧 + 音频帧（control/media 双 stream 多路复用）
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
        "ws://127.0.0.1:{}/ws/watch?stream=quic-stream",
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
            stream_id: "quic-stream".into()
        }
    );

    // 新观众先收最近关键帧（last_keyframe 缓存）→ 断言完整帧逐字节一致
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
        "QUIC 推流 → relay → 观看端应收到完整关键帧（逐字节一致）"
    );

    handle.stop().await;
}
