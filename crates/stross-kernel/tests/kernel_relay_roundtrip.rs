//! 内核 ↔ 受控中继闭环集成测试（Phase 1a，需求 F2.2 / D4）。
//!
//! 验证：
//! * 内核创建会话后 id 被预授权，推流端用该 id 可接入受控中继；
//! * 未授权 id 推流被拒绝（「先会话后传输」语义）；
//! * 流生命周期事件（StreamStarted / StreamEnded / WatchersChanged）
//!   经数据面后端转发为 [`KernelEvent`]；
//! * 会话拆除后预授权撤销，同一 id 不再可推流。
//!
//! v3 P3 方法面收敛：会话 / 凭证命令一律经**控制面**（`CtrlServer` +
//! `control::client`）——Kernel 门面不再暴露 create_session / teardown /
//! create_share_token（壳层与外部测试同走控制面，docs/framework-v3.md §4）。

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use stross_kernel::KernelEvent;
use stross_kernel::control::{CtrlRequest, CtrlServer};
use stross_kernel::relay::RelayServer;
use stross_kernel::{Kernel, Platform, RelayDataPlane};
use stross_proto::frame::{Frame, TRACK_VIDEO};
use stross_proto::message::{
    CodecId, ControlMessage, EndpointId, MediaKind, ShareToken, TrackInfo,
};
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

/// 启动回环控制面（随机端口），返回服务器句柄 + 连接基址。
async fn ctrl_server(kernel: &Arc<Kernel>) -> (CtrlServer, String) {
    let server = CtrlServer::start(kernel.clone(), 0, None).await.unwrap();
    let connect = format!("ws://127.0.0.1:{}/ws/ctrl", server.port);
    (server, connect)
}

/// 经控制面创建会话（Kernel 门面无 create_session，命令收敛在控制面）。
async fn ctrl_create_session(connect: &str, title: &str) -> stross_kernel::SessionCreatedView {
    stross_kernel::control::client::request_as(
        connect,
        CtrlRequest::CreateSession {
            title: title.into(),
            sinks: vec!["local".into()],
        },
    )
    .await
    .expect("控制面建会话")
}

/// 会话是否已登记（经控制面 ListSessions 查询内部会话表）。
async fn ctrl_has_session(connect: &str, session_id: &stross_proto::message::StreamId) -> bool {
    let payload: stross_kernel::SessionsPayload =
        stross_kernel::control::client::request_as(connect, CtrlRequest::ListSessions)
            .await
            .expect("控制面列出会话");
    payload.sessions.iter().any(|s| &s.session_id == session_id)
}

/// 经控制面签发一次性接入凭证，还原为 [`ShareToken`]（篡改 / 过期断言用）。
async fn ctrl_issue_token(
    connect: &str,
    session_id: &stross_proto::message::StreamId,
    ttl_secs: u64,
) -> ShareToken {
    let issued: stross_kernel::IssuedShareTokenView = stross_kernel::control::client::request_as(
        connect,
        CtrlRequest::ShareToken {
            session_id: session_id.to_string(),
            ttl_secs,
        },
    )
    .await
    .expect("控制面签发凭证");
    ShareToken::from_token_string(&issued.token).expect("凭证字符串可还原为 ShareToken")
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
    let kernel = Arc::new(Kernel::new(Platform::Desktop));
    kernel.attach_data_plane(Arc::new(RelayDataPlane::new(&relay)));
    let (ctrl, connect) = ctrl_server(&kernel).await;
    let mut events = kernel.subscribe();

    // 1) 创建会话（经控制面）：id 由内核签发并预授权
    let created = ctrl_create_session(&connect, "内核会话测试").await;
    let session_id = created.session_id;
    let session_id_expected = session_id.clone();
    assert!(
        ctrl_has_session(&connect, &session_id).await,
        "会话应已登记"
    );

    // 2) 用会话 id 推流 → 受控中继应接受，并上报 StreamStarted
    let (mut push, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/push"))
        .await
        .expect("连接推流端点");
    push.send(Message::Text(hello(&session_id).to_text().into()))
        .await
        .unwrap();
    let welcome = push.next().await.unwrap().unwrap();
    assert!(welcome.is_text(), "应收到 Welcome");
    assert_eq!(
        ControlMessage::from_text(&welcome.into_text().unwrap()).unwrap(),
        ControlMessage::Welcome {
            stream_id: session_id.clone()
        }
    );
    push.send(Message::Binary(video_frame().into()))
        .await
        .unwrap();
    expect_event!(events, KernelEvent::StreamStarted { session_id, .. } => {
        assert_eq!(session_id, session_id_expected);
    });

    // 3) 观看端接入 → WatchersChanged（计数 ≥ 1）
    let (mut watch, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/ws/watch?stream={}",
        session_id
    ))
    .await
    .expect("连接观看端点");
    // 消费 Ready（首个控制消息）
    let first = watch.next().await.unwrap().unwrap();
    assert_eq!(
        ControlMessage::from_text(&first.into_text().unwrap()).unwrap(),
        ControlMessage::Ready {
            stream_id: session_id.clone()
        }
    );
    expect_event!(events, KernelEvent::WatchersChanged { session_id, watchers } => {
        assert_eq!(session_id, session_id_expected);
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
    let ctrl_msg = ControlMessage::from_text(&err.into_text().unwrap()).unwrap();
    assert!(
        matches!(ctrl_msg, ControlMessage::Error { .. }),
        "未授权推流应收到 Error，得到 {ctrl_msg:?}"
    );

    // 5) 会话拆除（经控制面）→ 预授权撤销 + 流被同步拆除（StreamEnded 经
    //    数据面转发；§7.1 后 SessionEnded 事件变体已删除，只收 StreamEnded）
    let _: stross_kernel::TeardownView = stross_kernel::control::client::request_as(
        &connect,
        CtrlRequest::Teardown {
            session_id: session_id.to_string(),
        },
    )
    .await
    .expect("控制面拆除会话");
    assert!(
        !ctrl_has_session(&connect, &session_id).await,
        "会话应已拆除"
    );
    expect_event!(events, KernelEvent::StreamEnded { session_id } => {
        assert_eq!(session_id, session_id_expected);
    });

    // 6) 拆除后同一 id 再推流 → 拒绝（预授权已撤销）
    let (mut re_push, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/push"))
            .await
            .unwrap();
    re_push
        .send(Message::Text(hello(&session_id).to_text().into()))
        .await
        .unwrap();
    let err = re_push.next().await.unwrap().unwrap();
    let ctrl_msg = ControlMessage::from_text(&err.into_text().unwrap()).unwrap();
    assert!(
        matches!(ctrl_msg, ControlMessage::Error { .. }),
        "拆除后的 id 不应再可推流，得到 {ctrl_msg:?}"
    );

    watch.close(None).await.unwrap();
    bad_push.close(None).await.unwrap();
    re_push.close(None).await.unwrap();
    drop(push);
    ctrl.stop().await;
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
async fn share_token_grants_cross_node_push() {
    use stross_kernel::relay::RelayHandle;

    let relay: RelayHandle = RelayServer::start_controlled(0).await.unwrap();
    let port = relay.port;
    let kernel = Arc::new(Kernel::new(Platform::Desktop));
    let (ctrl, connect) = ctrl_server(&kernel).await;
    // 不 attach_data_plane：会话创建**不会**预授权给中继，只有凭证能放行
    let created = ctrl_create_session(&connect, "跨设备推流").await;
    let session_id = created.session_id;
    assert!(relay.is_controlled(), "受控模式");
    // 注入内核的凭证校验器（attach_data_plane 之外直接注入，模拟"凭证接入"路径）
    relay
        .state()
        .set_token_validator(Some(kernel.token_validator()));
    let token = ctrl_issue_token(&connect, &session_id, 300).await;
    let token_str = token.to_token_string();

    // 1) 未授权 id + 无凭证 → 拒绝（F2.2 语义保持）
    let (mut plain, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/push"))
        .await
        .unwrap();
    plain
        .send(Message::Text(hello(&session_id).to_text().into()))
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
        hello_with_token(&session_id, &token_str).to_text().into(),
    ))
    .await
    .unwrap();
    let welcome = push.next().await.unwrap().unwrap();
    assert_eq!(
        ControlMessage::from_text(&welcome.into_text().unwrap()).unwrap(),
        ControlMessage::Welcome {
            stream_id: session_id.clone()
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
        hello_with_token(&session_id, &forged.to_token_string())
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
    let expired = ctrl_issue_token(&connect, &session_id, 0).await;
    let (mut stale, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/push"))
        .await
        .unwrap();
    stale
        .send(Message::Text(
            hello_with_token(&session_id, &expired.to_token_string())
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
    ctrl.stop().await;
    relay.stop().await;
}

/// 来源感知门控（B 阶段完善）：**跨设备（非回环）来源即使 id 已被内核预授权，
/// 不出示凭证也拒绝**——预授权只服务本机（回环）流程；远程推流必须凭证。
#[tokio::test]
async fn remote_source_requires_token_even_when_authorized() {
    use stross_kernel::net::local_ips;
    use stross_kernel::relay::RelayHandle;

    let relay: RelayHandle = RelayServer::start_controlled(0).await.unwrap();
    let port = relay.port;
    let kernel = Arc::new(Kernel::new(Platform::Desktop));
    let (ctrl, connect) = ctrl_server(&kernel).await;
    // 正常接线：create_session 会把 id 预授权给受控中继
    kernel.attach_data_plane(Arc::new(RelayDataPlane::new(&relay)));
    let created = ctrl_create_session(&connect, "来源感知门控").await;
    let session_id = created.session_id;
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
        .send(Message::Text(hello(&session_id).to_text().into()))
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
    let token = ctrl_issue_token(&connect, &session_id, 300).await;
    let (mut push, _) = tokio_tungstenite::connect_async(format!("ws://{lan_ip}:{port}/ws/push"))
        .await
        .unwrap();
    push.send(Message::Text(
        hello_with_token(&session_id, &token.to_token_string())
            .to_text()
            .into(),
    ))
    .await
    .unwrap();
    let welcome = push.next().await.unwrap().unwrap();
    assert_eq!(
        ControlMessage::from_text(&welcome.into_text().unwrap()).unwrap(),
        ControlMessage::Welcome {
            stream_id: session_id.clone()
        },
        "跨设备来源出示凭证应放行"
    );
    push.close(None).await.unwrap();
    ctrl.stop().await;
    relay.stop().await;
}

/// 端点共享生命周期（iteration-plan.md 第十二轮）：
/// 订阅者全部断开（watchers→0）后，端点共享自动收尾——
/// 清共享登记 + 本机会话拆除（会话生命周期 = 流生命周期）+ 流从数据面回收。
#[tokio::test]
async fn endpoint_share_stops_after_last_watcher_leaves() {
    use stross_proto::message::Delivery;

    let relay = RelayServer::start_controlled(0).await.unwrap();
    let port = relay.port;
    let mut kernel = Kernel::new(Platform::Desktop);
    // 收紧生命周期延迟：本测试只验证 watchers→0 路径（idle 窗口拉长避免干扰）
    kernel.set_share_lifecycle_delays(Duration::from_millis(150), Duration::from_secs(60));
    let kernel = Arc::new(kernel);
    kernel.attach_data_plane(Arc::new(RelayDataPlane::new(&relay)));
    let (ctrl, connect) = ctrl_server(&kernel).await;

    // 端点共享登记（真实路径：订阅达成 → share → start_stream 成功 → note_share_active）
    let screen_id = EndpointId::new(MediaKind::Screen, 0);
    let created = ctrl_create_session(&connect, "端点共享收尾").await;
    let session_id = created.session_id;
    let weak: std::sync::Weak<Kernel> = Arc::downgrade(&kernel);
    kernel.note_share_active(weak, screen_id, &session_id, Delivery::Pull);
    assert!(kernel.active_share_by_endpoint(screen_id).is_some());

    // 推流端（模拟端点自动推流进受控中继）
    let (mut push, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws/push"))
        .await
        .unwrap();
    push.send(Message::Text(hello(&session_id).to_text().into()))
        .await
        .unwrap();
    let _welcome = push.next().await.unwrap().unwrap();
    push.send(Message::Binary(video_frame().into()))
        .await
        .unwrap();

    // 观看端接入 → watchers=1（消费 Ready + 关键帧）
    let (mut watch, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{port}/ws/watch?stream={}",
        session_id
    ))
    .await
    .unwrap();
    let _ready = watch.next().await.unwrap().unwrap();
    let _kf = watch.next().await.unwrap().unwrap();

    // 观看端断开 → watchers=0 → 延迟复查后自动收尾。
    // 注：中继观看端只在「下一次广播」时发现对端已断（send 失败即退出循环），
    // 因此断开后仍需推流端继续发帧触发检测（真实端点共享恒 30fps 推流）。
    // close() 只发 Close 帧可能留半开连接，drop 强制拆 TCP（服务器端 send 立刻失败）。
    watch.close(None).await.unwrap();
    drop(watch);
    for _ in 0..10 {
        push.send(Message::Binary(video_frame().into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // 轮询等待自动收尾（watchers→0 检测 + 150ms 复查延迟）
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline
        && kernel.active_share_by_endpoint(screen_id).is_some()
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        kernel.active_share_by_endpoint(screen_id).is_none(),
        "watchers=0 后端点共享登记应清除"
    );
    assert!(
        !ctrl_has_session(&connect, &session_id).await,
        "流结束后续会话应拆除（会话生命周期 = 流生命周期）"
    );
    assert!(
        relay
            .state()
            .streams()
            .iter()
            .all(|s| s.stream_id != session_id),
        "流应从数据面回收"
    );

    push.close(None).await.unwrap();
    ctrl.stop().await;
    relay.stop().await;
}
