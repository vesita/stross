//! 内存传输（测试 / 示例用）。
//!
//! 通过共享 [`MemoryHub`] 按 `addr/session_id` 配对：`connect` 注册对端，
//! `accept` 轮询取走，形成一对双向数据会话。与真实传输共用同一套
//! [`Transport`] / [`DataSession`] 接口，用于验证传输抽象本身。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use tokio::sync::{Mutex as AsyncMutex, mpsc};

use stross_proto::message::ReliabilityProfile;

use super::{
    DataSession, PeerAddr, SessionPacket, SessionParams, Transport, TransportError, TransportStats,
};

/// 共享配对中心。
#[derive(Default)]
pub struct MemoryHub {
    listeners: Mutex<HashMap<String, Box<dyn DataSession>>>,
}

impl MemoryHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

/// 内存传输：绑定一个 hub 与监听地址（`accept` 用）。
pub struct MemoryTransport {
    hub: Arc<MemoryHub>,
    addr: String,
    stats: Arc<AsyncMutex<TransportStats>>,
}

impl MemoryTransport {
    pub fn new(hub: Arc<MemoryHub>, addr: impl Into<String>) -> Self {
        Self {
            hub,
            addr: addr.into(),
            stats: Arc::new(AsyncMutex::new(TransportStats::default())),
        }
    }
}

#[async_trait]
impl Transport for MemoryTransport {
    fn id(&self) -> &'static str {
        "memory"
    }

    fn profile(&self) -> ReliabilityProfile {
        ReliabilityProfile::Lossless
    }

    async fn connect(
        &self,
        peer: &PeerAddr,
        params: &SessionParams,
    ) -> Result<Box<dyn DataSession>, TransportError> {
        let key = format!("{}/{}", peer.addr, params.session_id);
        // 两条单向通道交叉配对：
        //   a_tx → a_rx：客户端发送、服务端接收
        //   b_tx → b_rx：服务端发送、客户端接收
        let (a_tx, a_rx) = mpsc::channel::<SessionPacket>(64);
        let (b_tx, b_rx) = mpsc::channel::<SessionPacket>(64);
        let accept_session = MemorySession {
            rx: AsyncMutex::new(a_rx),
            tx: AsyncMutex::new(Some(b_tx)),
        };
        {
            let mut guard = self.hub.listeners.lock().unwrap();
            if guard.contains_key(&key) {
                return Err(TransportError::Connect(format!("地址已占用: {key}")));
            }
            guard.insert(key, Box::new(accept_session));
        }
        let my_session = MemorySession {
            rx: AsyncMutex::new(b_rx),
            tx: AsyncMutex::new(Some(a_tx)),
        };
        Ok(Box::new(my_session))
    }

    async fn accept(&self, params: &SessionParams) -> Result<Box<dyn DataSession>, TransportError> {
        let key = format!("{}/{}", self.addr, params.session_id);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let mut guard = self.hub.listeners.lock().unwrap();
                if let Some(session) = guard.remove(&key) {
                    return Ok(session);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(TransportError::Connect(format!("等待对端超时: {key}")));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn stats(&self) -> TransportStats {
        self.stats.blocking_lock().clone()
    }
}

/// 内存会话：一端持有收/发通道。
struct MemorySession {
    rx: AsyncMutex<mpsc::Receiver<SessionPacket>>,
    tx: AsyncMutex<Option<mpsc::Sender<SessionPacket>>>,
}

#[async_trait]
impl DataSession for MemorySession {
    async fn send(&self, pkt: SessionPacket) -> Result<(), TransportError> {
        let tx = self
            .tx
            .lock()
            .await
            .as_ref()
            .ok_or(TransportError::Closed)?
            .clone();
        tx.send(pkt).await.map_err(|_| TransportError::Closed)
    }

    async fn recv(&self) -> Result<Option<SessionPacket>, TransportError> {
        let mut rx = self.rx.lock().await;
        Ok(rx.recv().await)
    }

    async fn close(&self) -> Result<(), TransportError> {
        self.tx.lock().await.take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::{Frame, TRACK_VIDEO};
    use stross_proto::message::ControlMessage;

    #[tokio::test]
    async fn memory_transport_roundtrip() {
        let hub = MemoryHub::new();
        let server = MemoryTransport::new(hub.clone(), "memory://relay");
        let client = MemoryTransport::new(hub, "memory://client");
        let peer = PeerAddr {
            transport: "memory".into(),
            addr: "memory://relay".into(),
        };
        let params = SessionParams {
            session_id: "s1".into(),
            profile: ReliabilityProfile::Lossless,
        };

        let accept_task = tokio::spawn({
            let params = params.clone();
            async move { server.accept(&params).await.unwrap() }
        });
        let client_session = client.connect(&peer, &params).await.unwrap();
        let server_session = accept_task.await.unwrap();

        // 客户端 → 服务端：控制消息
        client_session
            .send(SessionPacket::Control(ControlMessage::Hello {
                stream_id: "s1".into(),
                title: "t".into(),
                video: None,
                audio: None,
            }))
            .await
            .unwrap();
        let pkt = server_session.recv().await.unwrap().unwrap();
        assert!(matches!(
            pkt,
            SessionPacket::Control(ControlMessage::Hello { .. })
        ));

        // 服务端 → 客户端：媒体帧
        let frame = Frame::new(TRACK_VIDEO, 1, 0, 0, vec![1, 2, 3]);
        server_session
            .send(SessionPacket::Media(frame))
            .await
            .unwrap();
        let pkt = client_session.recv().await.unwrap().unwrap();
        assert!(matches!(pkt, SessionPacket::Media(_)));

        // 关闭后 recv 返回 None
        client_session.close().await.unwrap();
        assert!(server_session.recv().await.unwrap().is_none());
    }
}
