//! 集成测试：WebRTC 观看端（设计文档阶段 1 的抽象价值证明）。
//!
//! 推流端走 WebSocket（现有路径），观看端走 WebRTC（新路径）——
//! 中继的 `handle_watch` 转发逻辑对两条传输原样复用：
//! 观看端应收到 `Ready` 语义（这里体现为关键帧对齐转发）+ 视频关键帧 + 音频帧。
//!
//! 信令经 HTTP（`/api/webrtc/start` + `/answer`，标准 SDP 文本），
//! 媒体经 UDP/DTLS/SCTP data channel（media 通道不可靠、乱序）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use stross_core::relay::RelayServer;
use stross_proto::frame::{Frame, FLAG_KEYFRAME, TRACK_AUDIO, TRACK_VIDEO};
use stross_proto::message::{ControlMessage, TrackInfo};
use str0m::change::SdpOffer;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

// ---------------------------------------------------------------------------
// WS 推流端（与 relay_integration.rs 相同的构造方式）
// ---------------------------------------------------------------------------

fn video_frame(keyframe: bool) -> Vec<u8> {
    let payload = if keyframe {
        vec![0x67, 0x00, 0x01, 0x02, 0x05, 0x03]
    } else {
        vec![0x41, 0x00, 0x01, 0x02]
    };
    Frame::new(
        TRACK_VIDEO,
        stross_proto::frame::CODEC_H264,
        if keyframe { FLAG_KEYFRAME } else { 0 },
        0,
        payload,
    )
    .to_bytes()
    .to_vec()
}

fn audio_frame() -> Vec<u8> {
    Frame::new(
        TRACK_AUDIO,
        stross_proto::frame::CODEC_AAC,
        0,
        0,
        vec![0xFF, 0xF1, 0x50, 0x00, 0x01, 0x1F, 0xFC, 0x00],
    )
    .to_bytes()
    .to_vec()
}

fn hello() -> String {
    ControlMessage::Hello {
        stream_id: "test-stream".into(),
        title: "测试串流".into(),
        video: Some(TrackInfo {
            codec: "h264".into(),
            width: Some(640),
            height: Some(360),
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
    .to_text()
}

// ---------------------------------------------------------------------------
// WebRTC 测试客户端 run loop（str0m 对端）
// ---------------------------------------------------------------------------

/// 把媒体通道（binary）收到的数据转发给 `tx`。
async fn client_loop(udp: Arc<UdpSocket>, mut rtc: Rtc, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut next_timeout: Option<Instant> = None;
    loop {
        let wait = next_timeout
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(1));
        tokio::select! {
            res = udp.recv_from(&mut buf) => {
                let Ok((n, from)) = res else { break };
                let local = udp.local_addr().unwrap_or(from);
                if let Ok(recv) = Receive::new(Protocol::Udp, from, local, &buf[..n]) {
                    let _ = rtc.handle_input(Input::Receive(Instant::now(), recv));
                }
            }
            _ = tokio::time::sleep(wait) => {
                let _ = rtc.handle_input(Input::Timeout(Instant::now()));
            }
        }
        loop {
            match rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    let _ = udp.send_to(&t.contents[..], t.destination).await;
                }
                Ok(Output::Timeout(t)) => {
                    next_timeout = Some(t);
                    break;
                }
                Ok(Output::Event(ev)) => match ev {
                    Event::ChannelData(d) => {
                        if d.binary {
                            let _ = tx.send(d.data.to_vec()).await;
                        }
                    }
                    Event::Closed => return,
                    _ => {}
                },
                Err(_) => break,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webrtc_watch_receives_frames() {
    let relay = RelayServer::start(0).await.unwrap();
    let port = relay.port;
    let base = format!("ws://127.0.0.1:{port}");

    // 1) WS 推流：先建流（hello），媒体帧稍后推（避免广播订阅竞争）
    let (mut push, _) = tokio_tungstenite::connect_async(format!("{base}/ws/push"))
        .await
        .expect("连接推流端点");
    push.send(Message::Text(hello().into())).await.unwrap();
    let welcome = push.next().await.unwrap().unwrap();
    assert!(welcome.is_text(), "应收到 Welcome");

    // 2) WebRTC 信令：start → SDP offer
    let http = reqwest::Client::new();
    let start: serde_json::Value = http
        .post(format!("http://127.0.0.1:{port}/api/webrtc/start"))
        .json(&serde_json::json!({ "streamId": "test-stream" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let peer_id = start["peerId"].as_str().unwrap().to_string();
    let offer_sdp = start["sdp"].as_str().unwrap().to_string();

    // 3) 客户端 Rtc：接受 offer → answer（标准 SDP 文本）
    let udp = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let mut rtc = Rtc::new(Instant::now());
    let candidate = Candidate::host(udp.local_addr().unwrap(), "udp").expect("host 候选");
    rtc.add_local_candidate(candidate).unwrap();
    let offer = SdpOffer::from_sdp_string(&offer_sdp).expect("解析 offer SDP");
    let answer = rtc.sdp_api().accept_offer(offer).expect("接受 offer");

    // 4) 提交 answer
    let resp = http
        .post(format!("http://127.0.0.1:{port}/api/webrtc/answer"))
        .json(&serde_json::json!({ "peerId": peer_id, "sdp": answer.to_sdp_string() }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "answer 提交失败: {}", resp.status());

    // 5) 启动客户端 run loop，收集媒体帧
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(client_loop(udp, rtc, tx));

    // 6) 推视频关键帧（经 last_keyframe 缓存必达），等它到达证明转发已存活
    push.send(Message::Binary(video_frame(true).into())).await.unwrap();
    let mut saw_keyframe = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline && !saw_keyframe {
        match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
            Ok(Some(bytes)) => {
                if let Ok(frame) = Frame::from_bytes(&bytes) {
                    if frame.header.track == TRACK_VIDEO {
                        assert!(frame.header.is_keyframe(), "WebRTC 观看端不应收到非关键帧");
                        saw_keyframe = true;
                    }
                }
            }
            _ => break,
        }
    }
    assert!(saw_keyframe, "WebRTC 观看端应收到视频关键帧");

    // 7) 再推音频帧（此时观看端已在实时订阅），断言到达
    push.send(Message::Binary(audio_frame().into())).await.unwrap();
    let mut saw_audio = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !saw_audio {
        match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
            Ok(Some(bytes)) => {
                if let Ok(frame) = Frame::from_bytes(&bytes) {
                    if frame.header.track == TRACK_AUDIO {
                        saw_audio = true;
                    }
                }
            }
            _ => break,
        }
    }
    assert!(saw_audio, "WebRTC 观看端应收到音频帧");

    push.close(None).await.unwrap();
    relay.stop().await;
}
