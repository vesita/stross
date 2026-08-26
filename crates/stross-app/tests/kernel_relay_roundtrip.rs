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
        share_token: None,
    }
}

/// 带接入凭证的 Hello（跨设备推流）。
fn hello_with_token(stream_id: &str, token: &str) -> ControlMessage {
    ControlMessage::Hello {
        stream_id: stream_id.into(),
        title: "跨设备推流".into(),
        video: None,
        audio: None,
        share_token: Some(token.to_string()),
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
    kernel.teardown(&session.id).unwrap();
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

/// 凭证式跨设备推流（B 阶段，docs/iteration-plan.md B0/B1）：
/// 受控中继未预授权该 id（内核不接数据面），推流端仅出示内核签发的
/// [`ShareToken`] 即可接入；无凭证 / 篡改凭证被拒绝。
#[tokio::test]
async fn share_token_grants_cross_device_push() {
    use stross_core::relay::RelayHandle;
    use stross_proto::message::MediaKind;

    let relay: RelayHandle = RelayServer::start_controlled(0).await.unwrap();
    let port = relay.port;
    let kernel = Kernel::new();
    // 不 attach_data_plane：会话创建**不会**预授权给中继，只有凭证能放行
    let session = kernel
        .create_session("local", &["local".into()], &SessionPrefs::default())
        .unwrap();
    assert!(relay.is_controlled(), "受控模式");
    // 注入内核的凭证校验器（attach_data_plane 之外直接注入，模拟"凭证接入"路径）
    relay
        .state()
        .set_token_validator(Some(kernel.token_validator()));
    let token = kernel
        .create_share_token(&session.id, vec![MediaKind::Mic], Duration::from_secs(300))
        .unwrap();
    let token_str = token.to_token_string();

    // 1) 未授权 id + 无凭证 → 拒绝（F2.2 语义保持）
    let (mut plain, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/push"))
        .await
        .unwrap();
    plain
        .send(Message::Text(hello(&session.id).to_text().into()))
        .await
        .unwrap();
    let err = plain.next().await.unwrap().unwrap();
    assert!(
        matches!(
            ControlMessage::from_text(&err.into_text().unwrap()).unwrap(),
            ControlMessage::Error { .. }
        ),
        "未授权且无凭证应拒绝"
    );
    plain.close(None).await.unwrap();

    // 2) 出示有效凭证 → 接受（Welcome），流可建立
    let (mut push, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/push"))
        .await
        .unwrap();
    push.send(Message::Text(
        hello_with_token(&session.id, &token_str).to_text().into(),
    ))
    .await
    .unwrap();
    let welcome = push.next().await.unwrap().unwrap();
    assert_eq!(
        ControlMessage::from_text(&welcome.into_text().unwrap()).unwrap(),
        ControlMessage::Welcome {
            stream_id: session.id.clone()
        },
        "有效凭证应放行推流"
    );
    push.send(Message::Binary(video_frame().into()))
        .await
        .unwrap();

    // 3) 篡改凭证（PIN 改掉）→ 拒绝（服务端以签发时存储为准，逐字比对）
    let mut forged = token.clone();
    forged.pin = "000000".into();
    let (mut bad, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/push"))
        .await
        .unwrap();
    bad.send(Message::Text(
        hello_with_token(&session.id, &forged.to_token_string())
            .to_text()
            .into(),
    ))
    .await
    .unwrap();
    let err = bad.next().await.unwrap().unwrap();
    assert!(
        matches!(
            ControlMessage::from_text(&err.into_text().unwrap()).unwrap(),
            ControlMessage::Error { .. }
        ),
        "篡改凭证应拒绝"
    );
    bad.close(None).await.unwrap();

    // 4) 凭证过期 → 拒绝
    let expired = kernel
        .create_share_token(&session.id, vec![MediaKind::Mic], Duration::ZERO)
        .unwrap();
    let (mut stale, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/push"))
        .await
        .unwrap();
    stale
        .send(Message::Text(
            hello_with_token(&session.id, &expired.to_token_string())
                .to_text()
                .into(),
        ))
        .await
        .unwrap();
    let err = stale.next().await.unwrap().unwrap();
    assert!(
        matches!(
            ControlMessage::from_text(&err.into_text().unwrap()).unwrap(),
            ControlMessage::Error { .. }
        ),
        "过期凭证应拒绝"
    );
    stale.close(None).await.unwrap();

    push.close(None).await.unwrap();
    relay.stop().await;
}

/// 来源感知门控（B 阶段完善）：**跨设备（非回环）来源即使 id 已被内核预授权，
/// 不出示凭证也拒绝**——预授权只服务本机（回环）流程；远程推流必须凭证。
#[tokio::test]
async fn remote_source_requires_token_even_when_authorized() {
    use stross_core::net::local_ips;
    use stross_core::relay::RelayHandle;
    use stross_proto::message::MediaKind;

    let relay: RelayHandle = RelayServer::start_controlled(0).await.unwrap();
    let port = relay.port;
    let kernel = Kernel::new();
    // 正常接线：create_session 会把 id 预授权给受控中继
    kernel.attach_data_plane(Arc::new(RelayDataPlane::new(&relay)));
    let session = kernel
        .create_session("local", &["local".into()], &SessionPrefs::default())
        .unwrap();
    let lan_ip = local_ips()
        .into_iter()
        .next()
        .expect("测试环境应有局域网 IP（非回环来源模拟）");
    assert!(!lan_ip.is_loopback(), "需要非回环 IP 模拟跨设备来源");

    // 1) 非回环来源 + 已预授权 + 无凭证 → 拒绝（门控核心语义）
    let (mut remote, _) = tokio_tungstenite::connect_async(format!("ws://{lan_ip}:{port}/ws/push"))
        .await
        .expect("经局域网 IP 连接（模拟另一台设备）");
    remote
        .send(Message::Text(hello(&session.id).to_text().into()))
        .await
        .unwrap();
    let err = remote.next().await.unwrap().unwrap();
    assert!(
        matches!(
            ControlMessage::from_text(&err.into_text().unwrap()).unwrap(),
            ControlMessage::Error { .. }
        ),
        "跨设备来源无凭证应拒绝（即使 id 已预授权）"
    );
    remote.close(None).await.unwrap();

    // 2) 同一来源出示凭证 → 放行
    let token = kernel
        .create_share_token(&session.id, vec![MediaKind::Mic], Duration::from_secs(300))
        .unwrap();
    let (mut push, _) = tokio_tungstenite::connect_async(format!("ws://{lan_ip}:{port}/ws/push"))
        .await
        .unwrap();
    push.send(Message::Text(
        hello_with_token(&session.id, &token.to_token_string())
            .to_text()
            .into(),
    ))
    .await
    .unwrap();
    let welcome = push.next().await.unwrap().unwrap();
    assert_eq!(
        ControlMessage::from_text(&welcome.into_text().unwrap()).unwrap(),
        ControlMessage::Welcome {
            stream_id: session.id.clone()
        },
        "跨设备来源出示凭证应放行"
    );
    push.close(None).await.unwrap();
    relay.stop().await;
}
