//! 集成测试：QUIC 连接复用（通信模式 v2 Phase C，docs/comm-mode-v2.md §5）。
//!
//! 场景 = 真实中继 + 真实上层路径（`RelayClient` 推流 / `connect_watch` 观看，
//! 二者经传输层链路管理器自动共享同 (host, port) 的 QUIC 连接）：
//!
//! 1. 推流端两条会话（屏幕 + 系统声）+ 观看端两条会话（分别 watch 两路）——
//!    **同一条 QUIC 连接承载 4 条媒体流**（[连接][stream_id] demux）；
//! 2. 两路观看端各自收到对应流的关键帧（**不串流**）；
//! 3. 停一路推流 → 该路观看端收到关闭，**另一路不受影响（不级联）**；
//! 4. 中继流表：已停流移除、存活流保留。

use std::time::Duration;

use stross_kernel::relay::RelayServer;
use stross_kernel::sender::RelayClient;
use stross_kernel::watch::connect_watch;
use stross_proto::frame::{CODEC_H264, FLAG_KEYFRAME, Frame, TRACK_VIDEO};
use stross_proto::message::{CodecId, ControlMessage, TrackInfo};

fn hello(stream_id: &str, title: &str) -> ControlMessage {
    ControlMessage::Hello {
        stream_id: stream_id.into(),
        title: title.into(),
        video: Some(TrackInfo {
            codec: CodecId::H264,
            width: Some(1280),
            height: Some(720),
            fps: Some(30),
            sample_rate: None,
            channels: None,
        }),
        audio: None,
        share_token: None,
    }
}

/// 从观看会话读到载荷等于 `expect` 的关键帧（跳过控制消息/其它帧）。
async fn recv_until_payload(session: &dyn stross_kernel::DataSession, expect: &[u8]) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), session.recv()).await {
            Ok(Ok(Some(stross_kernel::SessionPacket::Media(f))))
                if f.header.is_keyframe() && f.payload.as_ref() == expect =>
            {
                return true;
            }
            Ok(Ok(Some(_))) => continue,
            Ok(Ok(None)) | Ok(Err(_)) => return false,
            Err(_) => return false,
        }
    }
    false
}

#[tokio::test]
async fn quic_one_connection_multiple_streams_demux_no_cascade() {
    let handle = RelayServer::start(0).await.unwrap();
    let quic = format!("quic://127.0.0.1:{}", handle.quic_port.expect("QUIC 监听"));

    // 推流端：两条会话（同一连接——链路管理器按 (host, port) 复用）
    let (_push_a, tx_a) = RelayClient::connect(&quic, hello("stream-a", "屏幕"))
        .await
        .unwrap();
    let (_push_b, tx_b) = RelayClient::connect(&quic, hello("stream-b", "系统声"))
        .await
        .unwrap();

    // 观看端：两条会话（同一条连接——与推流端同一连接：4 条流共享 1 连接）
    let watch_a = connect_watch(&quic, "stream-a").await.unwrap();
    let watch_b = connect_watch(&quic, "stream-b").await.unwrap();

    // 两路各推一个关键帧（载荷区分归属）
    tx_a.send(Frame::new(
        TRACK_VIDEO,
        CODEC_H264,
        FLAG_KEYFRAME,
        0,
        vec![0xAA; 8],
    ))
    .await
    .unwrap();
    tx_b.send(Frame::new(
        TRACK_VIDEO,
        CODEC_H264,
        FLAG_KEYFRAME,
        1,
        vec![0xBB; 8],
    ))
    .await
    .unwrap();

    // demux 不串流：每路观看端只收到自己流的关键帧
    assert!(
        recv_until_payload(watch_a.as_ref(), &[0xAA; 8]).await,
        "观看端 A 应收到 stream-a 关键帧"
    );
    assert!(
        recv_until_payload(watch_b.as_ref(), &[0xBB; 8]).await,
        "观看端 B 应收到 stream-b 关键帧"
    );

    // 停一路推流（stream-a）→ 流被移除；stream-b 不受影响（不级联）
    _push_a.stop().await;
    let closed = tokio::time::timeout(Duration::from_secs(5), watch_a.recv()).await;
    assert!(
        matches!(closed, Ok(Ok(None)) | Ok(Err(_))),
        "停一路后该路观看端应收到关闭: {closed:?}"
    );

    // stream-b 仍可继续推/收
    tx_b.send(Frame::new(
        TRACK_VIDEO,
        CODEC_H264,
        FLAG_KEYFRAME,
        2,
        vec![0xBB; 8],
    ))
    .await
    .unwrap();
    assert!(
        recv_until_payload(watch_b.as_ref(), &[0xBB; 8]).await,
        "另一路流不受影响（不级联）"
    );

    // 中继流表：stream-a 已移除、stream-b 仍在
    let streams = handle.streams();
    assert!(
        streams.iter().any(|s| s.stream_id == "stream-b"),
        "stream-b 应仍在流表: {streams:?}"
    );
    assert!(
        !streams.iter().any(|s| s.stream_id == "stream-a"),
        "stream-a 应已移除: {streams:?}"
    );

    _push_b.stop().await;
    handle.stop().await;
}
