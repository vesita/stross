//! 接收播放集成测试（1e）：推流 → 中继 → 原生接收解码（真实 ffmpeg）。
//!
//! 覆盖 GUI「📥 接收」页使用的同一套 API：
//! `StrossApp::start_receive` → WS watch → SessionDataManager →
//! `FfmpegPlaybackSink` 解码 → `receive_status` 统计。

use std::sync::Arc;
use std::time::{Duration, Instant};

use stross_app::{Platform, Receiver, StrossApp};
use stross_core::relay::RelayServer;
use stross_core::transport::srt::SrtTransport;
use stross_core::transport::{PeerAddr, SessionPacket, SessionParams, Transport};
use stross_media::capture::FfmpegBackend;
use stross_media::pipeline::ffmpeg_available;
use stross_media::pipeline::{Quality, StreamConfig, StreamSession, VideoSource};
use stross_media::playback::AudioOut;
use stross_proto::frame::Frame;
use stross_proto::message::{ControlMessage, ReliabilityProfile};
use tokio::sync::mpsc;

fn cfg(stream_id: &str, secs: u32) -> StreamConfig {
    StreamConfig {
        stream_id: stream_id.into(),
        title: "接收测试".into(),
        video: Some(VideoSource::Synthetic {
            pattern: "testsrc2".into(),
        }),
        quality: Quality::LOW,
        audio: None,
        duration_secs: Some(secs),
        share_token: None,
    }
}

#[tokio::test]
async fn receive_decodes_live_stream() {
    if !ffmpeg_available() {
        eprintln!("跳过：未找到 ffmpeg");
        return;
    }
    let app = Arc::new(StrossApp::new(Platform::Desktop));
    app.set_backend(Arc::new(FfmpegBackend::new()));
    let relay = app.start_relay_on(0).await.expect("启动中继");
    let relay_ws = format!("ws://127.0.0.1:{}", relay.port);

    // 推流 3 秒（内核签发 session/stream id，D4）
    let started = app
        .start_stream(cfg("recv-test", 3), None)
        .await
        .expect("推流启动");
    assert!(!started.stream_id.is_empty(), "应返回内核签发的 stream id");

    // 开始接收：解码帧通道可取（测试环境音频丢弃，无声卡依赖）
    let recv = app
        .start_receive(relay_ws, started.stream_id.clone(), AudioOut::Discard)
        .await
        .expect("接收启动");
    let mut frames = recv.take_frames().expect("应有帧通道");
    let frame_task = tokio::spawn(async move {
        let mut n = 0u32;
        while let Some(f) = frames.recv().await {
            assert_eq!(f.rgba.len() as u32, f.width * f.height * 4, "RGBA 帧尺寸");
            n += 1;
        }
        n
    });

    // 等待解码产出（真实 ffmpeg 编解码，给足时间）
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let s = recv.stats();
        if s.decoded_video > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    app.stop_receive();

    let s = recv.stats();
    assert!(s.received > 0, "应收到协议帧: {s:?}");
    assert!(s.decoded_video > 0, "应解码出视频帧: {s:?}");
    let drawn = tokio::time::timeout(Duration::from_secs(3), frame_task)
        .await
        .map(|r| r.unwrap_or(0))
        .unwrap_or(0);
    assert!(drawn > 0, "解码帧通道应有帧流出");
    app.stop_stream().await.ok();
}

/// 接收走 SRT 观看端（阶段 0b）：真实 ffmpeg 合成源经 SRT 推流 → 中继 →
/// `Receiver`（SRT watch 参数化路径）解码，验证 UDP 观看端端到端可用。
#[tokio::test]
async fn receive_over_srt_decodes_live_stream() {
    if !ffmpeg_available() {
        eprintln!("跳过：未找到 ffmpeg");
        return;
    }
    let relay = RelayServer::start(0).await.expect("启动中继");
    let srt_url = format!("srt://127.0.0.1:{}", relay.srt_port.expect("SRT 监听"));

    // SRT 推流：真实 ffmpeg 合成源（testsrc2 LOW，3 秒）
    let (tx, mut rx) = mpsc::channel::<Frame>(256);
    let cap = StreamSession::spawn(&cfg("recv-srt", 3), tx).expect("启动采集");
    let transport = SrtTransport::new();
    let peer = PeerAddr {
        transport: stross_proto::message::TransportId::Srt,
        addr: srt_url.clone(),
    };
    let params = SessionParams {
        session_id: "recv-srt".into(),
        profile: ReliabilityProfile::Adaptive,
    };
    let push = transport
        .connect(&peer, &params)
        .await
        .expect("SRT 连接中继");
    push.send(SessionPacket::Control(ControlMessage::Hello {
        stream_id: "recv-srt".into(),
        title: "SRT 接收测试".into(),
        video: None,
        audio: None,
        share_token: None,
    }))
    .await
    .unwrap();
    // 等 Welcome，确保流已建立（避免观看端先到报「流不存在」）
    loop {
        match tokio::time::timeout(Duration::from_secs(5), push.recv()).await {
            Ok(Ok(Some(SessionPacket::Control(ControlMessage::Welcome { .. })))) => break,
            Ok(Ok(Some(_))) => continue,
            Ok(Ok(None)) => panic!("推流连接提前关闭"),
            Ok(Err(e)) => panic!("推流 recv 错误: {e}"),
            Err(_) => panic!("等 Welcome 超时"),
        }
    }
    let push_task = tokio::spawn(async move {
        // 会话内递增 seq（与真实推流端 RelayClient 一致，B5：有损路径的
        // 接收端抖动缓冲按 seq 排序；裸推流必须自行填充）
        let mut next_seq = 0u32;
        while let Some(mut f) = rx.recv().await {
            f.header.seq = next_seq;
            next_seq = next_seq.wrapping_add(1);
            if push.send(SessionPacket::Media(f)).await.is_err() {
                break;
            }
        }
    });

    // 观看端：SRT watch → 解码
    let recv = Receiver::start(srt_url, "recv-srt".into(), AudioOut::Discard, None)
        .await
        .expect("SRT 接收启动");
    let mut frames = recv.take_frames().expect("应有帧通道");
    let frame_task = tokio::spawn(async move {
        let mut n = 0u32;
        while let Some(f) = frames.recv().await {
            assert_eq!(f.rgba.len() as u32, f.width * f.height * 4, "RGBA 帧尺寸");
            n += 1;
        }
        n
    });

    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let s = recv.stats();
        if s.decoded_video > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    recv.stop();

    let s = recv.stats();
    assert!(s.received > 0, "应收到协议帧: {s:?}");
    assert!(s.decoded_video > 0, "应解码出视频帧: {s:?}");
    let drawn = tokio::time::timeout(Duration::from_secs(3), frame_task)
        .await
        .map(|r| r.unwrap_or(0))
        .unwrap_or(0);
    assert!(drawn > 0, "解码帧通道应有帧流出");

    drop(cap);
    push_task.abort();
    relay.stop().await;
}
