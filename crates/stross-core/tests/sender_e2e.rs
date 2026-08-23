//! 端到端测试：ffmpeg 真实编码 → 管线 → 中继 → 观看端。
//!
//! 需要本机安装 ffmpeg（含 libx264）。没有 ffmpeg 时自动跳过。

use std::time::Duration;

use futures_util::StreamExt;
use stross_core::pipeline::{Quality, StreamConfig, VideoSource};
use stross_core::sender::SenderEngine;
use stross_proto::frame::{Frame, TRACK_VIDEO};
use stross_proto::message::ControlMessage;
use tokio_tungstenite::tungstenite::Message;

fn has_ffmpeg() -> bool {
    stross_core::pipeline::ffmpeg_available()
}

#[tokio::test]
async fn synthetic_video_flows_end_to_end() {
    if !has_ffmpeg() {
        eprintln!("跳过：未找到 ffmpeg");
        return;
    }

    let cfg = StreamConfig {
        stream_id: "e2e".into(),
        title: "端到端测试".into(),
        video: Some(VideoSource::Synthetic {
            pattern: "testsrc2".into(),
        }),
        quality: Quality::LOW,
        audio: None,
        duration_secs: Some(3),
    };

    let engine = SenderEngine::start(cfg, None, 0).await.expect("启动推流引擎");
    let port = engine.relay_port().expect("内嵌中继端口");

    // 观看端连接
    let (mut watch, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/ws/watch?stream=e2e"
    ))
    .await
    .expect("连接观看端点");

    // 首个消息是 Ready
    let first = watch.next().await.unwrap().unwrap();
    let ready = ControlMessage::from_text(&first.into_text().unwrap()).unwrap();
    assert!(matches!(ready, ControlMessage::Ready { .. }));

    // 收集真实视频帧，直到会话结束
    let mut saw_keyframe = false;
    let mut saw_non_keyframe = false;
    let mut keyframe_has_sps = false;
    let mut total = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), watch.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                let frame = Frame::from_bytes(&bytes).unwrap();
                if frame.header.track != TRACK_VIDEO {
                    continue;
                }
                total += 1;
                if frame.header.is_keyframe() {
                    saw_keyframe = true;
                    // 关键帧访问单元应包含 SPS（起始码 00 00 01 后跟 NAL type 7 = 0x67）
                    keyframe_has_sps |= frame
                        .payload
                        .windows(4)
                        .any(|w| w == [0x00, 0x00, 0x01, 0x67]);
                } else {
                    saw_non_keyframe = true;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => break, // 推流结束（3 秒后 Bye）
            Err(_) => break,
        }
        if saw_keyframe && saw_non_keyframe && total > 10 {
            break;
        }
    }

    engine.stop().await;

    assert!(saw_keyframe, "应收到 H.264 关键帧（共 {total} 帧）");
    assert!(saw_non_keyframe, "应收到非关键帧");
    assert!(keyframe_has_sps, "关键帧应包含 SPS（repeat-headers=1）");
    assert!(total > 10, "帧数过少: {total}");
    eprintln!("端到端 OK：收到 {total} 帧真实 H.264 视频帧");
}
