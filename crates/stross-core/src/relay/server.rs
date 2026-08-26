//! 中继生命周期：句柄（[`RelayHandle`]）与服务器（[`RelayServer`]）。
//!
//! 数据面转发见 [`super::data_plane`]，共享状态见 [`super::state::RelayState`]。

use std::net::SocketAddr;

use axum::serve::{Listener, ListenerExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use stross_proto::message::StreamInfo;

#[cfg(feature = "discovery")]
use super::peers;
#[cfg(feature = "discovery")]
use crate::net::local_ips;
use crate::transport::quic::QuicTransport;
use crate::transport::srt::SrtTransport;

use super::data_plane;
use super::http;
use super::state::{RelayEvent, RelayState};

/// 默认中继端口。
pub const DEFAULT_PORT: u16 = 8777;

/// 中继句柄。
pub struct RelayHandle {
    /// 实际监听端口（绑定 0 时由系统分配）。
    pub port: u16,
    /// SRT 推流端口（随中继启动，独立 UDP；`None` = 未启用）。
    pub srt_port: Option<u16>,
    /// QUIC 推流端口（随中继启动，独立 UDP；`None` = 未启用）。
    pub quic_port: Option<u16>,
    state: RelayState,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl RelayHandle {
    /// 当前流列表。
    pub fn streams(&self) -> Vec<StreamInfo> {
        self.state.streams()
    }

    /// 局域网内其它设备（mDNS 发现缓存）。
    pub fn peers(&self) -> Vec<super::peers::PeerInfo> {
        self.state.peers()
    }

    /// 手动注册一台局域网设备（调试 / 测试用）。
    pub fn insert_peer(&self, peer: super::peers::PeerInfo) {
        self.state.insert_peer(peer);
    }

    /// 订阅数据面事件（内核用）。
    pub fn subscribe_events(&self) -> broadcast::Receiver<RelayEvent> {
        self.state.subscribe_events()
    }

    /// 预授权一个 stream id 接入（受控模式）。
    pub fn authorize_stream(&self, id: &str) {
        self.state.authorize_stream(id);
    }

    /// 按媒体类型自动选择推流地址（与接收端 auto 模式同规则）：
    ///
    /// * 含视频 → `srt://host:port`（Adaptive：丢包不阻塞、关键帧自愈）> QUIC > WS
    /// * 纯音频 → `quic://host:port`（无损：音频不可丢）> WS
    ///
    /// `has_video = false` 且无 QUIC 端口时回退 WS；端口缺失时逐级回退。
    /// 返回的是可拨号 URL（WS 带 `/ws/push` 路径，UDP 无路径）。
    pub fn auto_push_url(&self, has_video: bool) -> String {
        use crate::transport::RelayUrl;
        if has_video {
            if let Some(p) = self.srt_port {
                return RelayUrl::srt("127.0.0.1", p).to_string();
            }
            if let Some(p) = self.quic_port {
                return RelayUrl::quic("127.0.0.1", p).to_string();
            }
        } else if let Some(p) = self.quic_port {
            return RelayUrl::quic("127.0.0.1", p).to_string();
        }
        RelayUrl::ws("127.0.0.1", self.port, Some("/ws/push")).to_string()
    }

    /// 撤销预授权。
    pub fn revoke_stream(&self, id: &str) {
        self.state.revoke_stream(id);
    }

    /// 是否受控模式（仅授权 id 可推流）。
    pub fn is_controlled(&self) -> bool {
        self.state.is_controlled()
    }

    /// 中继共享状态（克隆句柄，供数据面适配器等共享访问）。
    pub fn state(&self) -> RelayState {
        self.state.clone()
    }

    /// 停止中继服务（同时拆除全部代理任务）。
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        self.state.abort_proxies();
        let _ = self.task.await;
    }
}

/// 中继服务器构造器（单方法命名空间）。
pub struct RelayServer;

impl RelayServer {
    /// 绑定并启动中继（非受控：任意 stream id 可推流，现状行为）。
    ///
    /// `port == 0` 时由系统分配空闲端口（测试用），实际端口在
    /// 返回的 [`RelayHandle::port`] 上；SRT/QUIC 推流监听随机端口，
    /// 见 [`RelayHandle::srt_port`] / [`RelayHandle::quic_port`]。
    pub async fn start(port: u16) -> anyhow::Result<RelayHandle> {
        Self::start_inner(port, false, 0, 0).await
    }

    /// 启动**受控模式**中继：只有 [`RelayHandle::authorize_stream`] 预授权的
    /// stream id 才能推流（对应需求 F2.2「先会话后传输」/ D4「id 内核签发」，
    /// 内嵌中继由内核驱动时使用）。
    pub async fn start_controlled(port: u16) -> anyhow::Result<RelayHandle> {
        Self::start_inner(port, true, 0, 0).await
    }

    /// 受控模式 + SRT/QUIC 固定端口（`0` = 随机）。
    ///
    /// 固定端口便于防火墙仅放行已知端口（权限自动化）；被占用时仍回退随机，
    /// 实际端口见 [`RelayHandle::srt_port`] / [`RelayHandle::quic_port`]。
    pub async fn start_controlled_with(
        port: u16,
        srt_port: u16,
        quic_port: u16,
    ) -> anyhow::Result<RelayHandle> {
        Self::start_inner(port, true, srt_port, quic_port).await
    }

    async fn start_inner(
        port: u16,
        controlled: bool,
        srt_hint: u16,
        quic_hint: u16,
    ) -> anyhow::Result<RelayHandle> {
        // TCP_NODELAY：媒体每帧一个 WS 消息，Nagle 会叠加延迟（LAN 也受影响）
        let listener = TcpListener::bind(("0.0.0.0", port)).await?.tap_io(|s| {
            let _ = s.set_nodelay(true);
        });
        let actual_port = listener.local_addr()?.port();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        // SRT 推流/观看监听：原生端可经 srt://<host>:<srt_port> 推流或观看，
        // 数据面与 WS 完全一致（handle_connect 按首条消息分流 Hello/Watch）
        let srt_bind = if srt_hint == 0 {
            "0.0.0.0:0".to_string()
        } else {
            format!("0.0.0.0:{srt_hint}")
        };
        let srt_listener = SrtTransport::new()
            .bind(&srt_bind)
            .await
            .map_err(|e| anyhow::anyhow!("SRT 监听失败: {e}"))?;
        let srt_port = srt_listener.local_addr().port();
        if srt_hint != 0 && srt_port != srt_hint {
            tracing::warn!("SRT 端口 {srt_hint} 被占用，回退到随机端口 {srt_port}");
        }
        tracing::info!("SRT 推流/观看监听: 0.0.0.0:{srt_port}");

        // QUIC 推流/观看监听：一条连接 control/media 双 stream 多路复用，
        // 原生端可推流（Hello）或观看（Watch）（Lossless；自签名 + 局域网可信模型）
        let quic_listener = QuicTransport::new()
            .bind(if quic_hint == 0 {
                "0.0.0.0:0".parse().expect("静态地址")
            } else {
                SocketAddr::from(([0, 0, 0, 0], quic_hint))
            })
            .await
            .map_err(|e| anyhow::anyhow!("QUIC 监听失败: {e}"))?;
        let quic_port = quic_listener.local_addr().port();
        if quic_hint != 0 && quic_port != quic_hint {
            tracing::warn!("QUIC 端口 {quic_hint} 被占用，回退到随机端口 {quic_port}");
        }
        tracing::info!("QUIC 推流/观看监听: 0.0.0.0:{quic_port}");

        let state =
            RelayState::with_ports(controlled, actual_port, Some(srt_port), Some(quic_port));
        let app = http::router(state.clone());

        data_plane::spawn_accept_loop(srt_listener, state.clone(), shutdown_tx.subscribe(), "SRT");
        data_plane::spawn_accept_loop(
            quic_listener,
            state.clone(),
            shutdown_tx.subscribe(),
            "QUIC",
        );

        let task = tokio::spawn(async move {
            // ConnectInfo：WS 升级处提取对端地址（来源感知门控用）
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await;
        });
        // 周期浏览局域网内其它中继，维护设备发现缓存（feature `discovery`）
        #[cfg(feature = "discovery")]
        peers::spawn_peer_refresh(state.clone(), actual_port, shutdown_tx.subscribe());
        tracing::info!("中继已启动: 0.0.0.0:{actual_port}");
        Ok(RelayHandle {
            port: actual_port,
            srt_port: Some(srt_port),
            quic_port: Some(quic_port),
            state,
            shutdown: shutdown_tx,
            task,
        })
    }

    /// 独立常驻中继模式：启动中继、打印入口地址、可选 mDNS 广播，
    /// 然后等待 Ctrl+C，停止时拆除全部代理任务。
    ///
    /// `stross-relay` CLI 与 `stross-gui --relay-only` 共用，消除两份几乎相同的
    /// 启动样板。`advertise = false`（或未启用 `discovery` feature）时不广播；
    /// mDNS 广播失败仅告警不退出——中继本身仍可用（与内嵌模式行为一致）。
    /// `instance` 为 mDNS 实例名前缀（如 `"relay"` / `"sender-relay"`），
    /// 端口号自动拼接为 `{instance}-{port}`。
    pub async fn run_standalone(port: u16, advertise: bool, instance: &str) -> anyhow::Result<()> {
        let handle = Self::start(port).await?;
        tracing::info!("📡 Stross 中继已启动");
        for entry in crate::transport::RelayUrl::http_entries(handle.port) {
            tracing::info!("中继入口: {entry}");
        }
        tracing::info!("推流地址: ws://<中继IP>:{}/ws/push", handle.port);
        tracing::info!("流列表API: /api/streams");
        tracing::info!("Ctrl+C 退出");

        // mDNS 广播本机（TXT 统一为 kind/name/roles/transports/codecs）；
        // 句柄存活到函数结束，drop 即停止广播
        #[cfg(feature = "discovery")]
        let _discovery = if advertise {
            match crate::discovery::Discovery::start(
                &format!("{instance}-{}", handle.port),
                &local_ips(),
                handle.port,
                &stross_proto::message::DiscoveryInfo::relay_default("Stross 中继", vec![]),
            ) {
                Ok(d) => {
                    tracing::info!("mDNS 广播中…");
                    Some(d)
                }
                Err(e) => {
                    tracing::warn!("mDNS 广播失败: {e}");
                    None
                }
            }
        } else {
            tracing::info!("mDNS 广播已关闭");
            None
        };
        // 未启用 discovery feature 时不广播；参数保留以免告警
        #[cfg(not(feature = "discovery"))]
        let _ = (advertise, instance);

        tokio::signal::ctrl_c().await?;
        tracing::info!("正在停止…");
        handle.stop().await;
        Ok(())
    }
}
