//! 内核 ↔ 受控中继闭环集成测试（Phase 1a，需求 F2.2 / D4）。
//!
//! 验证：
//! * 内核创建会话后 id 被预授权，推流端用该 id 可接入受控中继；
//! * 未授权 id 推流被拒绝（「先会话后传输」语义）；
//! * 流生命周期事件（StreamStarted / StreamEnded / WatchersChanged）
//!   经数据面后端转发为 [`KernelEvent`]；
//! * 会话拆除后预授权撤销，同一 id 不再可推流。

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use stross_app::kernel::{Kernel, RelayDataPlane};
use stross_app::{KernelEvent, SessionPrefs};
use stross_core::relay::RelayServer;
use stross_proto::frame::{Frame, TRACK_VIDEO};
use stross_proto::message::{CodecId, ControlMessage, TrackInfo};
use tokio_tungstenite::tungstenite::Message;

fn hello(stream_id: &str) -> ControlMessage {
    ControlMessage::Hello {
        stream_id: stream_id.into(),
        title: "内核会话测试".into(),
        video: Some(TrackInfo {
            codec: CodecId::H264,
            width: Some(640),
            height: Some(360),
            fps: Some(30),
            sample_rate: None,
            channels: None,
        }),
        audio: None,
    }
}

fn video_frame() -> Vec<u8> {
    Frame::new(
        TRACK_VIDEO,
        stross_proto::frame::CODEC_H264,
        stross_proto::frame::FLAG_KEYFRAME,
        0,
        vec![0x67, 0x00, 0x01, 0x02, 0x05, 0x03],
    )
    .to_bytes()
    .to_vec()
}

/// 订阅内核事件并断言下一条匹配指定模式（带超时）。
macro_rules! expect_event {
    ($rx:expr, $pat:pat => $body:block) => {
        match tokio::time::timeout(Duration::from_secs(3), $rx.recv())
            .await
            .expect("等待内核事件超时")
            .expect("内核事件通道关闭")
        {
            $pat => $body,
            other => panic!("期望 {}，得到 {other:?}", stringify!($pat)),
        }
    };
}

#[tokio::test]
async fn kernel_session_drives_controlled_relay() {
    // 受控中继 + 内核接线（数据面后端）
    let relay = RelayServer::start_controlled(0).await.unwrap();
    let port = relay.port;
    let kernel = Kernel::new();
    kernel.attach_data_plane(Arc::new(RelayDataPlane::new(&relay)));
    let mut events = kernel.subscribe();

    // 1) 创建会话：id 由内核签发并预授权
    let session = kernel
        .create_session("local", &["local".into()], &SessionPrefs::default())
        .await
        .unwrap();
    expect_event!(events, KernelEvent::SessionStarted { .. } => {});
    assert!(kernel.has_session(&session.id), "会话应已登记");

    // 2) 用会话 id 推流 → 受控中继应接受，并上报 StreamStarted
    let (mut push, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/push"))
        .await
        .expect("连接推流端点");
    push.send(Message::Text(hello(&session.id).to_text().into()))
        .await
        .unwrap();
    let welcome = push.next().await.unwrap().unwrap();
    assert!(welcome.is_text(), "应收到 Welcome");
    assert_eq!(
        ControlMessage::from_text(&welcome.into_text().unwrap()).unwrap(),
        ControlMessage::Welcome {
            stream_id: session.id.clone()
        }
    );
    push.send(Message::Binary(video_frame().into()))
        .await
        .unwrap();
    expect_event!(events, KernelEvent::StreamStarted { session_id, .. } => {
        assert_eq!(session_id, session.id);
    });

    // 3) 观看端接入 → WatchersChanged（计数 ≥ 1）
    let (mut watch, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/ws/watch?stream={}",
        session.id
    ))
    .await
    .expect("连接观看端点");
    // 消费 Ready（首个控制消息）
    let first = watch.next().await.unwrap().unwrap();
    assert_eq!(
        ControlMessage::from_text(&first.into_text().unwrap()).unwrap(),
        ControlMessage::Ready {
            stream_id: session.id.clone()
        }
    );
    expect_event!(events, KernelEvent::WatchersChanged { session_id, watchers } => {
        assert_eq!(session_id, session.id);
        assert!(watchers >= 1, "观看者计数应 ≥ 1，实际 {watchers}");
    });

    // 4) 未授权 id 推流 → 拒绝（先会话后传输）
    let (mut bad_push, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/push"))
            .await
            .expect("连接推流端点");
    bad_push
        .send(Message::Text(hello("not-authorized").to_text().into()))
        .await
        .unwrap();
    let err = bad_push.next().await.unwrap().unwrap();
    let ctrl = ControlMessage::from_text(&err.into_text().unwrap()).unwrap();
    assert!(
        matches!(ctrl, ControlMessage::Error { .. }),
        "未授权推流应收到 Error，得到 {ctrl:?}"
    );

    // 5) 会话拆除 → 预授权撤销 + 流被同步拆除（SessionEnded 内核直发、
    //    StreamEnded 经数据面转发，顺序不保证，收集两者）
    kernel.teardown(&session.id).await.unwrap();
    assert!(!kernel.has_session(&session.id), "会话应已拆除");
    let mut seen_session_ended = false;
    let mut seen_stream_ended = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline && !(seen_session_ended && seen_stream_ended) {
        let ev = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .expect("等待拆除事件超时")
            .expect("内核事件通道关闭");
        match ev {
            KernelEvent::SessionEnded { session_id } => {
                assert_eq!(session_id, session.id);
                seen_session_ended = true;
            }
            KernelEvent::StreamEnded { session_id } => {
                assert_eq!(session_id, session.id);
                seen_stream_ended = true;
            }
            other => panic!("期望 SessionEnded/StreamEnded，得到 {other:?}"),
        }
    }
    assert!(seen_session_ended, "应收到 SessionEnded");
    assert!(seen_stream_ended, "应收到 StreamEnded（流被同步拆除）");

    // 6) 拆除后同一 id 再推流 → 拒绝（预授权已撤销）
    let (mut re_push, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/push"))
            .await
            .unwrap();
    re_push
        .send(Message::Text(hello(&session.id).to_text().into()))
        .await
        .unwrap();
    let err = re_push.next().await.unwrap().unwrap();
    let ctrl = ControlMessage::from_text(&err.into_text().unwrap()).unwrap();
    assert!(
        matches!(ctrl, ControlMessage::Error { .. }),
        "拆除后的 id 不应再可推流，得到 {ctrl:?}"
    );

    watch.close(None).await.unwrap();
    bad_push.close(None).await.unwrap();
    re_push.close(None).await.unwrap();
    drop(push);
    relay.stop().await;
}

/// 非受控中继（默认 `start`）行为不变：任意 id 可推流（现状兼容）。
#[tokio::test]
async fn uncontrolled_relay_keeps_open_push() {
    let relay = RelayServer::start(0).await.unwrap();
    let port = relay.port;
    let (mut push, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/push"))
        .await
        .unwrap();
    push.send(Message::Text(hello("any-id").to_text().into()))
        .await
        .unwrap();
    let welcome = push.next().await.unwrap().unwrap();
    assert_eq!(
        ControlMessage::from_text(&welcome.into_text().unwrap()).unwrap(),
        ControlMessage::Welcome {
            stream_id: "any-id".into()
        }
    );
    push.close(None).await.unwrap();
    relay.stop().await;
}
