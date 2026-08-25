//! 观看端客户端：按 relay URL 的 scheme 选择传输并请求观看。
//!
//! 与 [`crate::sender::RelayClient`]（推流侧）对称：
//!
//! * `ws://host:port` → WS watch 端点（`/ws/watch?stream=`，URL 查询声明流）
//! * `srt://host:port` → SRT 拨号 + `Watch` 控制消息（relay 的 `handle_connect` 分流）
//! * `quic://host:port` → QUIC 拨号 + `Watch` 控制消息（经 control stream）
//!
//! 连接后等待中继 `Ready`（或 `Error`）回执，返回就绪的数据会话；
//! 媒体帧的消费（抖动缓冲/播放）由调用方负责。

use stross_proto::message::ControlMessage;

use crate::transport::quic::QuicTransport;
use crate::transport::srt::SrtTransport;
use crate::transport::ws::WsTransport;
use crate::transport::{DataSession, PeerAddr, SessionPacket, SessionParams, Transport};

/// 连接中继并请求观看 `stream_id`；返回已就绪（收到 `Ready`）的数据会话。
///
/// `relay_url` 支持三种 scheme：`ws://`（基址，自动拼 watch 端点）、
/// `srt://`、`quic://`（直接拨号对应传输端口）。
pub async fn connect_watch(
    relay_url: &str,
    stream_id: &str,
) -> Result<Box<dyn DataSession>, String> {
    let (transport, addr): (Box<dyn Transport>, String) = if relay_url.starts_with("srt://") {
        (Box::new(SrtTransport::new()), relay_url.to_string())
    } else if relay_url.starts_with("quic://") {
        (Box::new(QuicTransport::new()), relay_url.to_string())
    } else {
        (
            Box::new(WsTransport::new()),
            format!("{relay_url}/ws/watch?stream={stream_id}"),
        )
    };
    let peer = PeerAddr {
        transport: transport.id(),
        addr,
    };
    let params = SessionParams {
        session_id: stream_id.to_string(),
        profile: transport.profile(),
    };
    let session = transport
        .connect(&peer, &params)
        .await
        .map_err(|e| format!("连接中继失败: {e}"))?;

    // WS 由 URL 查询参数声明流；SRT/QUIC 需显式 Watch 请求
    if !relay_url.starts_with("ws://") {
        session
            .send(SessionPacket::Control(ControlMessage::Watch {
                stream_id: stream_id.to_string(),
            }))
            .await
            .map_err(|e| format!("发送 Watch 请求失败: {e}"))?;
    }

    // 等待 Ready（或 Error / 关闭）
    loop {
        match session.recv().await {
            Ok(Some(SessionPacket::Control(ControlMessage::Ready { .. }))) => {
                return Ok(session);
            }
            Ok(Some(SessionPacket::Control(ControlMessage::Error { message }))) => {
                return Err(format!("中继错误: {message}"));
            }
            Ok(Some(_)) => continue,
            Ok(None) => return Err("中继连接已关闭".into()),
            Err(e) => return Err(format!("等待就绪失败: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::{CODEC_H264, FLAG_KEYFRAME, Frame, TRACK_VIDEO};
    use tokio::time::Duration;

    use crate::relay::RelayServer;

    /// 建流辅助：推流 Hello + 关键帧。
    async fn push_frame(stream_id: &str, tx: &dyn DataSession) {
        tx.send(SessionPacket::Control(ControlMessage::Hello {
            stream_id: stream_id.into(),
            title: "watch 测试流".into(),
            video: None,
            audio: None,
        }))
        .await
        .unwrap();
        // 等 Welcome，确保流已建立（避免 watch 先到报「流不存在」）
        loop {
            match tokio::time::timeout(Duration::from_secs(5), tx.recv()).await {
                Ok(Ok(Some(SessionPacket::Control(ControlMessage::Welcome { .. })))) => break,
                Ok(Ok(Some(_))) => continue,
                Ok(Ok(None)) => panic!("推流连接提前关闭"),
                Ok(Err(e)) => panic!("推流 recv 错误: {e}"),
                Err(_) => panic!("等 Welcome 超时"),
            }
        }
        tx.send(SessionPacket::Media(Frame::new(
            TRACK_VIDEO,
            CODEC_H264,
            FLAG_KEYFRAME,
            0,
            vec![0x65, 0x88, 0x00, 0x01],
        )))
        .await
        .unwrap();
    }

    async fn watch_and_get_keyframe(relay_url: &str, stream_id: &str) -> Box<dyn DataSession> {
        let session = connect_watch(relay_url, stream_id)
            .await
            .expect("watch 连接应成功");
        loop {
            match tokio::time::timeout(Duration::from_secs(5), session.recv()).await {
                Ok(Ok(Some(SessionPacket::Media(f)))) if f.header.is_keyframe() => {
                    assert_eq!(f.payload.to_vec(), vec![0x65, 0x88, 0x00, 0x01]);
                    return session;
                }
                Ok(Ok(Some(_))) => continue,
                Ok(Ok(None)) => panic!("观看连接提前关闭"),
                Ok(Err(e)) => panic!("观看 recv 错误: {e}"),
                Err(_) => panic!("收关键帧超时"),
            }
        }
    }

    #[tokio::test]
    async fn srt_watch_receives_ws_pushed_stream() {
        let handle = RelayServer::start(0).await.unwrap();
        let srt_port = handle.srt_port.expect("relay 应启用 SRT 监听");

        let transport = crate::transport::srt::SrtTransport::new();
        let peer = PeerAddr {
            transport: stross_proto::message::TransportId::Srt,
            addr: format!("srt://127.0.0.1:{srt_port}"),
        };
        let params = SessionParams {
            session_id: "srt-watch".into(),
            profile: stross_proto::message::ReliabilityProfile::Adaptive,
        };
        let push = transport.connect(&peer, &params).await.unwrap();
        push_frame("srt-watch", push.as_ref()).await;

        let _watch =
            watch_and_get_keyframe(&format!("srt://127.0.0.1:{srt_port}"), "srt-watch").await;
        handle.stop().await;
    }

    #[tokio::test]
    async fn quic_watch_receives_ws_pushed_stream() {
        let handle = RelayServer::start(0).await.unwrap();
        let quic_port = handle.quic_port.expect("relay 应启用 QUIC 监听");

        let transport = crate::transport::quic::QuicTransport::new();
        let peer = PeerAddr {
            transport: stross_proto::message::TransportId::Quic,
            addr: format!("quic://127.0.0.1:{quic_port}"),
        };
        let params = SessionParams {
            session_id: "quic-watch".into(),
            profile: stross_proto::message::ReliabilityProfile::Lossless,
        };
        let push = transport.connect(&peer, &params).await.unwrap();
        push_frame("quic-watch", push.as_ref()).await;

        let _watch =
            watch_and_get_keyframe(&format!("quic://127.0.0.1:{quic_port}"), "quic-watch").await;
        handle.stop().await;
    }

    #[tokio::test]
    async fn ws_watch_keeps_working() {
        let handle = RelayServer::start(0).await.unwrap();
        let relay_ws = format!("ws://127.0.0.1:{}", handle.port);

        let transport = crate::transport::ws::WsTransport::new();
        let peer = PeerAddr {
            transport: stross_proto::message::TransportId::Ws,
            addr: format!("{relay_ws}/ws/push"),
        };
        let params = SessionParams {
            session_id: "ws-watch".into(),
            profile: stross_proto::message::ReliabilityProfile::Lossless,
        };
        let push = transport.connect(&peer, &params).await.unwrap();
        push_frame("ws-watch", push.as_ref()).await;

        let _watch = watch_and_get_keyframe(&relay_ws, "ws-watch").await;
        handle.stop().await;
    }
}
