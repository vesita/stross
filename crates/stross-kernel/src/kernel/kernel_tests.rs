use super::anchor::relay_mdns_instance;
use super::*;
use stross_proto::frame::Frame;
use stross_proto::message::{
    CapabilityDescriptor, CodecId, Delivery, MediaKind, ReliabilityProfile, RoutePath, TransportId,
};
use tokio::sync::mpsc;

fn node(id: &str) -> NodeInfo {
    NodeInfo {
        node_id: NodeId::from(id),
        name: id.into(),
        roles: vec![NodeRole::Sender],
        caps: vec![],
        addrs: vec![],
    }
}

/// 测试用假后端：记录是否被调用。
struct MockBackend(std::sync::atomic::AtomicBool);
#[async_trait::async_trait]
impl CaptureBackend for MockBackend {
    async fn start(&self, _cfg: &StreamConfig, _tx: mpsc::Sender<Frame>) -> anyhow::Result<()> {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn stop(&self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    fn status(&self) -> stross_endpoint::capture::CaptureStatus {
        stross_endpoint::capture::CaptureStatus {
            started: self.0.load(std::sync::atomic::Ordering::SeqCst),
            error: None,
        }
    }
}

#[test]
fn app_info_and_sources_never_panic() {
    let kernel = Kernel::new(Platform::Desktop);
    let info = kernel.app_info();
    assert_eq!(info.platform, "desktop");
    let _ = kernel.list_endpoint_sources();
}

#[test]
fn capture_status_requires_backend() {
    let kernel = Kernel::new(Platform::Desktop);
    // 未注入后端时采集状态应为未激活
    let st = kernel.capture_status();
    assert!(!st.active);
    assert!(!st.started);
}

#[test]
fn set_backend_then_query() {
    let kernel = Kernel::new(Platform::Android);
    kernel.set_backend(Arc::new(MockBackend(std::sync::atomic::AtomicBool::new(
        false,
    ))));
    let st = kernel.capture_status();
    assert!(!st.active); // 未推流
}

#[test]
fn graph_upsert_and_capability() {
    let k = Kernel::new(Platform::Desktop);
    k.upsert_node(node("a"));
    k.upsert_node(node("b"));
    assert_eq!(k.nodes().len(), 2);
    k.register_capability(&NodeId::from("a"), CapabilityDescriptor::unknown());
    k.register_capability(&NodeId::from("a"), CapabilityDescriptor::unknown()); // 去重
    let a = k
        .nodes()
        .into_iter()
        .find(|n| n.node_id == NodeId::from("a"))
        .unwrap();
    assert_eq!(a.caps.len(), 1);
}

#[tokio::test]
async fn create_session_requires_sinks() {
    let k = Kernel::new(Platform::Desktop);
    // v3 P3 方法面收敛：门面无 create_session，测试直连共享构建核心
    assert!(
        k.build_session(StreamId::new("sess-a"), "a", &[], &SessionPrefs::default())
            .is_err()
    );
}

/// 显式 id 会话幂等（docs/framework-v3.md §6「配套改动」）：
/// 语义 id 派生路径重复建会话返回既有会话，不重复登记。
#[tokio::test]
async fn ensure_session_with_id_is_idempotent() {
    let k = Kernel::new(Platform::Desktop);
    let prefs = SessionPrefs::default();
    let s1 = k
        .ensure_session_with_id("ep-screen-ly-rt-x", "local", &["local".into()], &prefs)
        .unwrap();
    assert_eq!(s1.id, "ep-screen-ly-rt-x");
    // 同 id 二次调用：返回既有会话，不新增
    let s2 = k
        .ensure_session_with_id("ep-screen-ly-rt-x", "local", &["local".into()], &prefs)
        .unwrap();
    assert_eq!(s2.id, s1.id);
    assert_eq!(k.sessions().len(), 1, "幂等：同 id 不重复建会话");
    // 不同派生 id → 各自会话（不同端点互不干扰）
    let s3 = k
        .ensure_session_with_id("ep-mic-ly-rt-y", "local", &["local".into()], &prefs)
        .unwrap();
    assert_ne!(s3.id, s1.id);
    assert_eq!(k.sessions().len(), 2);
}

/// 会话生命周期（创建 / 组播 / 改道 / 拆除）——§7.1 后 Session* 事件变体
/// 已随会话类方法面收敛删除（事件统一 stross_view 八概念变体），本测试
/// 只断言会话状态本身（事件断言随旧变体删除）。
///
/// v3 P3 方法面收敛：门面无 create_session/route/session，测试直连内部
/// 会话表验证行为（鉴权门禁 require_authorized + 路径更新 + 拆除）。
#[tokio::test]
async fn session_lifecycle() {
    let k = Kernel::new(Platform::Desktop);

    let s = k
        .build_session(
            StreamId::new("sess-a"),
            "a",
            &["b".into()],
            &SessionPrefs::default(),
        )
        .unwrap();
    assert_eq!(s.path, RoutePath::Direct { node: "b".into() });
    assert_eq!(k.sessions.get(&Id::from(s.id.as_str())).unwrap().id, s.id);

    // 多接收端 → 组播
    let m = k
        .build_session(
            StreamId::new("sess-m"),
            "a",
            &["b".into(), "c".into()],
            &SessionPrefs::default(),
        )
        .unwrap();
    assert!(matches!(m.path, RoutePath::Mesh { .. }));

    // 改道（内部行为：鉴权门禁 + 路径更新）
    let sid = Id::from(s.id.as_str());
    k.sessions.require_authorized(&sid).unwrap();
    let mut updated = k.sessions.get(&sid).unwrap();
    updated.path = RoutePath::ViaRelay {
        node: "relay-1".into(),
    };
    k.sessions.insert(updated);
    assert_eq!(
        k.sessions.get(&sid).unwrap().path,
        RoutePath::ViaRelay {
            node: "relay-1".into()
        }
    );

    // 拆除
    k.teardown(&s.id).unwrap();
    assert!(k.sessions.get(&sid).is_none());
}

#[tokio::test]
async fn route_unknown_session_fails() {
    let k = Kernel::new(Platform::Desktop);
    // 未知会话：鉴权门禁与拆除都报错（route 门面无 pub 方法，验证内部行为）
    assert!(k.sessions.require_authorized(&Id::from("nope")).is_err());
    assert!(k.teardown("nope").is_err());
}

#[tokio::test]
async fn negotiate_picks_transport_and_codec() {
    use stross_proto::message::CapabilityKind;
    let k = Kernel::new(Platform::Desktop);
    k.upsert_node(NodeInfo {
        node_id: "a".into(),
        name: "a".into(),
        roles: vec![NodeRole::Sender],
        caps: vec![CapabilityDescriptor {
            kind: CapabilityKind::Source,
            media: vec![MediaKind::Screen],
            codecs: vec![CodecId::H264, CodecId::Aac],
            transports: vec![TransportId::Ws],
            max_width: Some(1920),
            max_height: Some(1080),
            preferred_profile: ReliabilityProfile::Lossy,
        }],
        addrs: vec![],
    });
    // 源只支持 ws → 协商出 ws + h264
    let s = k
        .build_session(
            StreamId::new("sess-n1"),
            "a",
            &["b".into()],
            &SessionPrefs::default(),
        )
        .unwrap();
    assert_eq!(s.negotiated.transport, TransportId::Ws);
    assert_eq!(s.negotiated.codec, CodecId::H264);
    // 显式偏好 webrtc 但源不支持 → 回退 ws
    let prefs = SessionPrefs {
        profile: ReliabilityProfile::Lossy,
        preferred_transport: Some(TransportId::WebRtc),
        access_code: None,
        title: String::new(),
    };
    let s2 = k
        .build_session(StreamId::new("sess-n2"), "a", &["b".into()], &prefs)
        .unwrap();
    assert_eq!(s2.negotiated.transport, TransportId::Ws);
}

#[tokio::test]
async fn pin_gates_control_operations() {
    let k = Kernel::new(Platform::Desktop);
    // 设置访问码创建会话
    let prefs = SessionPrefs {
        profile: ReliabilityProfile::Lossy,
        preferred_transport: None,
        access_code: Some("1234".into()),
        title: String::new(),
    };
    let s = k
        .build_session(StreamId::new("sess-pin"), "a", &["b".into()], &prefs)
        .unwrap();
    assert!(s.requires_pin);
    let sid = Id::from(s.id.as_str());
    // 未授权：route / teardown 都被拒绝（门面无 route/authorize，验证内部行为）
    assert!(
        k.sessions.require_authorized(&sid).is_err(),
        "未授权 route 应被拒绝"
    );
    assert!(k.teardown(&s.id).is_err(), "未授权 teardown 应被拒绝");
    // 错误访问码
    assert!(k.auth.authorize(&s.id, Some("9999")).is_err());
    assert!(k.sessions.require_authorized(&sid).is_err());
    // 正确访问码 → 放行（auth 校验 + 会话标记已授权，与旧 Kernel::authorize 同两步）
    assert!(k.auth.authorize(&s.id, Some("1234")).is_ok());
    k.sessions.mark_authorized(&sid).unwrap();
    assert!(k.sessions.require_authorized(&sid).is_ok());
    assert!(k.teardown(&s.id).is_ok());
    // 会话不存在：auth 校验放行（无访问码），但会话表标记失败
    // （旧 Kernel::authorize 的报错来自 mark_authorized，语义等价）
    assert!(k.auth.authorize("nope", Some("1234")).is_ok());
    assert!(k.sessions.mark_authorized(&Id::from("nope")).is_err());
}

#[tokio::test]
async fn force_teardown_cleans_pin_session_without_auth() {
    let k = Kernel::new(Platform::Desktop);
    let prefs = SessionPrefs {
        profile: ReliabilityProfile::Lossy,
        preferred_transport: None,
        access_code: Some("8888".into()),
        title: "受保护会话".into(),
    };
    let s = k
        .build_session(StreamId::new("sess-f"), "a", &["b".into()], &prefs)
        .unwrap();
    assert!(s.requires_pin);
    // 普通 teardown 因未授权被拒绝
    assert!(k.teardown(&s.id).is_err());
    assert!(k.sessions.get(&Id::from(s.id.as_str())).is_some());
    // 内部生命周期 force_teardown 无阻碍彻底清理会话
    assert!(k.force_teardown(&s.id).is_ok());
    assert!(k.sessions.get(&Id::from(s.id.as_str())).is_none());
}

#[tokio::test]
async fn no_pin_session_stays_open() {
    let k = Kernel::new(Platform::Desktop);
    let s = k
        .build_session(
            StreamId::new("sess-open"),
            "a",
            &["b".into()],
            &SessionPrefs::default(),
        )
        .unwrap();
    assert!(!s.requires_pin);
    assert!(
        k.sessions
            .require_authorized(&Id::from(s.id.as_str()))
            .is_ok(),
        "无访问码会话应直接放行"
    );
}

#[tokio::test]
async fn share_token_lifecycle() {
    let k = Kernel::new(Platform::Desktop);
    let s = k
        .build_session(
            StreamId::new("sess-tok"),
            "a",
            &["b".into()],
            &SessionPrefs::default(),
        )
        .unwrap();

    // 未知会话 → 拒绝
    assert!(
        k.create_share_token("nope", vec![MediaKind::Mic], Duration::from_secs(60))
            .is_err()
    );

    // 签发：stream_id 与会话一致、PIN 为 6 位数字、有效期正确
    let token = k
        .create_share_token(&s.id, vec![MediaKind::Mic], Duration::from_secs(60))
        .unwrap();
    assert_eq!(token.stream_id, s.id);
    assert_eq!(token.v, ShareToken::VERSION);
    assert!(token.pin.len() == 6 && token.pin.chars().all(|c| c.is_ascii_digit()));
    assert_eq!(token.expires_at, now_secs().saturating_add(60));

    // 校验通过
    assert!(
        super::session_api::verify_share_token(&k.share_tokens.lock().unwrap(), &token).is_ok()
    );

    // 篡改 PIN → 拒绝（逐字比对）
    let mut forged = token.clone();
    forged.pin = "000000".into();
    assert!(
        super::session_api::verify_share_token(&k.share_tokens.lock().unwrap(), &forged).is_err()
    );

    // 篡改 stream_id → 拒绝（查不到签发记录）
    let mut forged2 = token.clone();
    forged2.stream_id = "sess-other".into();
    assert!(
        super::session_api::verify_share_token(&k.share_tokens.lock().unwrap(), &forged2).is_err()
    );

    // 重新签发覆盖旧凭证（同会话最新凭证有效）
    let token2 = k
        .create_share_token(&s.id, vec![MediaKind::Mic], Duration::from_secs(60))
        .unwrap();
    assert!(
        super::session_api::verify_share_token(&k.share_tokens.lock().unwrap(), &token2).is_ok()
    );
    assert!(
        super::session_api::verify_share_token(&k.share_tokens.lock().unwrap(), &token).is_err(),
        "旧凭证应失效"
    );

    // ttl=0 → 立即过期
    let expired = k
        .create_share_token(&s.id, vec![MediaKind::Mic], Duration::ZERO)
        .unwrap();
    assert!(
        super::session_api::verify_share_token(&k.share_tokens.lock().unwrap(), &expired).is_err()
    );
}

#[test]
fn relay_mdns_instance_unique_per_node_same_port() {
    // 不同 device_id、同端口：实例名必须不同（mdns-sd 同名互覆盖的根因）
    let a = relay_mdns_instance(Some("0123456789abcdef0123456789abcdef"), 8777);
    let b = relay_mdns_instance(Some("fedcba9876543210fedcba9876543210"), 8777);
    assert_ne!(a, b, "同端口不同设备实例名必须不同");
    assert!(
        a.starts_with("stross-01234567-8777"),
        "实例名携带设备前缀: {a}"
    );
}

#[test]
fn relay_mdns_instance_same_node_stable() {
    // 同一设备（同 device_id）跨启动实例名稳定（端口不变时）
    let id = "deadbeefcafe0123deadbeefcafe0123";
    assert_eq!(
        relay_mdns_instance(Some(id), 8777),
        relay_mdns_instance(Some(id), 8777)
    );
    // 端口变化只影响后缀（设备身份恒在前缀）
    assert_ne!(
        relay_mdns_instance(Some(id), 8777),
        relay_mdns_instance(Some(id), 33462)
    );
}

#[test]
fn relay_mdns_instance_fallback_without_identity() {
    // 未注入身份：回退旧格式（兼容无 UI 接入方）
    assert_eq!(relay_mdns_instance(None, 8777), "sender-8777");
    assert_eq!(relay_mdns_instance(Some(""), 8777), "sender-8777");
}

/// 端点共享登记 → 查询 → 显式停止（stop_endpoint_share）→ 登记清除。
/// （契约层 note_share_active 已删除——生命周期治理归 P2e ShareService；
/// 内核自有方法保留，测试直接调用。）
#[tokio::test]
async fn active_share_register_query_and_stop() {
    let k = Arc::new(Kernel::new(Platform::Desktop));
    let weak: std::sync::Weak<Kernel> = Arc::downgrade(&k);
    k.note_share_active(
        weak,
        EndpointId::new(MediaKind::Mic, 0),
        "sess-1",
        Delivery::Pull,
    );
    let got = k
        .active_share_by_endpoint(EndpointId::new(MediaKind::Mic, 0))
        .expect("登记后可查询");
    assert_eq!(got.0, "sess-1");
    assert_eq!(got.1, Delivery::Pull);

    k.stop_endpoint_share(EndpointId::new(MediaKind::Mic, 0))
        .unwrap();
    assert!(
        k.active_share_by_endpoint(EndpointId::new(MediaKind::Mic, 0))
            .is_none(),
        "停止后登记应清除"
    );
    // 幂等：无活动共享时停止直接成功
    assert!(
        k.stop_endpoint_share(EndpointId::new(MediaKind::Mic, 0))
            .is_ok()
    );
    assert!(
        k.stop_endpoint_share(EndpointId::new(MediaKind::Screen, 0))
            .is_ok()
    );
}

/// 会话拆除联动清除接入凭证（凭证随会话失效，防重放）。
#[tokio::test]
async fn teardown_clears_share_token() {
    let k = Kernel::new(Platform::Desktop);
    let s = k
        .build_session(
            StreamId::new("sess-td"),
            "a",
            &["b".into()],
            &SessionPrefs::default(),
        )
        .unwrap();
    let t = k
        .create_share_token(&s.id, vec![MediaKind::Mic], Duration::from_secs(60))
        .unwrap();
    assert!(super::session_api::verify_share_token(&k.share_tokens.lock().unwrap(), &t).is_ok());
    k.teardown(&s.id).unwrap();
    assert!(
        super::session_api::verify_share_token(&k.share_tokens.lock().unwrap(), &t).is_err(),
        "teardown 后凭证应失效（签发表移除）"
    );
}

/// 「可被发现」门控统一发现清单：`discoverable=false` 时 `/api/discovery`
/// 不可见（子网单播扫描回退也探测不到），关闭 = 所有发现路径不可见。
#[tokio::test]
async fn discovery_manifest_gated_by_discoverable() {
    use crate::negotiator::NodeIdentity;
    let k = Arc::new(Kernel::new(Platform::Desktop));
    k.set_identity(NodeIdentity {
        node_id: "node-gated".into(),
        node_name: "pico".into(),
    });
    let _ = k.start_relay_on(0, "pico").await.unwrap();
    // 默认 discoverable=false → 清单不可见
    assert!(
        k.discovery_manifest().is_none(),
        "可被发现默认关闭时不应对外提供发现清单"
    );
    // 开启 → 清单可见（mDNS + 子网扫描都据此找到本节点）
    k.set_discoverable(true);
    let m = k.discovery_manifest().expect("开启后可被发现应返回清单");
    assert_eq!(m.node_id, NodeId::from("node-gated"));
    assert!(m.relay_port > 0, "已锚定中继才有入口");
    // 再关闭 → 清单重新不可见
    k.set_discoverable(false);
    assert!(k.discovery_manifest().is_none());
}

// -----------------------------------------------------------------------
// 多端点链接接收（通信模式 v2 Phase C「接收端多流化」）
// -----------------------------------------------------------------------

/// 推流辅助：WS 建流 + 关键帧（载荷带区分字节，供断言「哪条流」）。
async fn push_keyframe_payload(
    base: &str,
    stream_id: &str,
    payload: Vec<u8>,
) -> Box<dyn crate::DataSession> {
    use crate::transport::{PeerAddr, SessionParams, Transport};
    use stross_proto::frame::{CODEC_H264, FLAG_KEYFRAME, TRACK_VIDEO};
    use stross_proto::message::ControlMessage;
    let transport = crate::transport::ws::WsTransport::new();
    let peer = PeerAddr {
        transport: stross_proto::message::TransportId::Ws,
        addr: format!("{base}/ws/push"),
    };
    let params = SessionParams {
        session_id: stream_id.into(),
        profile: ReliabilityProfile::Lossless,
    };
    let push = transport.connect(&peer, &params).await.unwrap();
    push.send(crate::SessionPacket::Control(ControlMessage::Hello {
        stream_id: stream_id.into(),
        title: "多链路测试流".into(),
        video: None,
        audio: None,
        share_token: None,
    }))
    .await
    .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), push.recv()).await {
            Ok(Ok(Some(crate::SessionPacket::Control(ControlMessage::Welcome { .. })))) => {
                break;
            }
            Ok(Ok(Some(_))) => continue,
            Ok(Ok(None)) => panic!("推流连接提前关闭"),
            Ok(Err(e)) => panic!("推流 recv 错误: {e}"),
            Err(_) => panic!("等 Welcome 超时"),
        }
    }
    push.send(crate::SessionPacket::Media(Frame::new(
        TRACK_VIDEO,
        CODEC_H264,
        FLAG_KEYFRAME,
        0,
        payload,
    )))
    .await
    .unwrap();
    push
}

/// 等编码帧通道出现载荷等于 `expect` 的关键帧（区分流归属）。
async fn recv_raw_payload(rx: &mut mpsc::Receiver<Frame>, expect: &[u8], label: &str) {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(f)) if f.payload.as_ref() == expect => break,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("链路 {label} 通道提前关闭"),
            Err(_) => panic!("链路 {label} 收期望载荷超时"),
        }
    }
}

/// 多端点链接：同进程同时接收两条流（不同 link_id），每条链独立收帧 /
/// 统计 / 停止——停一条不级联另一条；旧单流 API（main 槽）保持
/// 「启新停旧」兼容语义。
#[tokio::test]
async fn multi_link_receive_independent_start_stop() {
    let handle = crate::relay::RelayServer::start(0).await.unwrap();
    let base = format!("ws://127.0.0.1:{}", handle.port);
    let kernel = Kernel::new(Platform::Desktop);

    let push_a = push_keyframe_payload(&base, "stream-a", vec![0xaa; 8]).await;
    let push_b = push_keyframe_payload(&base, "stream-b", vec![0xbb; 8]).await;

    // 两条链路并发接收（多端点链接：不再「第二路停第一路」）
    let ra = kernel
        .start_receive_raw_link("link-a".into(), base.clone(), "stream-a".into())
        .await
        .expect("链路 a 启动");
    let rb = kernel
        .start_receive_raw_link("link-b".into(), base.clone(), "stream-b".into())
        .await
        .expect("链路 b 启动");
    let mut fa = ra.take_raw_frames().expect("链路 a 帧通道");
    let mut fb = rb.take_raw_frames().expect("链路 b 帧通道");

    // 每条链收到各自流的关键帧（互不串流）
    recv_raw_payload(&mut fa, &[0xaa; 8], "a").await;
    recv_raw_payload(&mut fb, &[0xbb; 8], "b").await;

    // 链路注册表：两条都在
    let links = kernel.receive_links();
    assert_eq!(links.len(), 2, "两条链路并存");
    assert!(links.iter().all(|l| l.stats.running), "两条都在接收中");

    // 停链路 a：链路 b 不受影响（不级联）
    kernel.stop_receive_link("link-a");
    let links = kernel.receive_links();
    assert_eq!(links.len(), 1, "停一条后只剩链路 b");
    assert_eq!(links[0].link_id, "link-b");
    assert!(links[0].stats.running);
    // 链路 b 仍能继续收帧（再推一帧）
    push_b
        .send(crate::SessionPacket::Media(Frame::new(
            stross_proto::frame::TRACK_VIDEO,
            stross_proto::frame::CODEC_H264,
            stross_proto::frame::FLAG_KEYFRAME,
            1,
            vec![0xbb; 8],
        )))
        .await
        .unwrap();
    recv_raw_payload(&mut fb, &[0xbb; 8], "b").await;
    assert!(
        kernel.receive_links()[0].stats.received >= 2,
        "链路 b 持续收帧"
    );

    // 停链路 b：注册表清空
    kernel.stop_receive_link("link-b");
    assert!(kernel.receive_links().is_empty());

    drop(push_a);
    drop(push_b);
    handle.stop().await;
}

/// 旧单流 API 兼容：`start_receive_raw` 落 main 槽，启新停旧；`stop_receive`
/// 只停 main，不影响多链路并存。
#[tokio::test]
async fn legacy_main_slot_keeps_stop_old_semantics() {
    let handle = crate::relay::RelayServer::start(0).await.unwrap();
    let base = format!("ws://127.0.0.1:{}", handle.port);
    let kernel = Kernel::new(Platform::Desktop);

    let _push1 = push_keyframe_payload(&base, "legacy-1", vec![0x11; 4]).await;
    let _push2 = push_keyframe_payload(&base, "legacy-2", vec![0x22; 4]).await;

    // 先启一条多链路（并存验证：旧 API 不清它）
    let r_extra = kernel
        .start_receive_raw_link("extra".into(), base.clone(), "legacy-1".into())
        .await
        .unwrap();
    let mut fx = r_extra.take_raw_frames().unwrap();
    recv_raw_payload(&mut fx, &[0x11; 4], "extra").await;

    // 旧 API：main 槽启新停旧
    kernel
        .start_receive_raw(base.clone(), StreamId::from("legacy-1"))
        .await
        .unwrap();
    let r1 = kernel
        .start_receive_raw(base.clone(), StreamId::from("legacy-2"))
        .await
        .unwrap();
    let _ = r1; // 第二次启动应停掉第一次（main 槽单链）
    let links = kernel.receive_links();
    assert_eq!(links.len(), 2, "main + extra 并存");
    let main_stats = links.iter().find(|l| l.link_id == "main").unwrap();
    assert_eq!(
        main_stats.stats.received, 0,
        "main 槽收到的是 legacy-2 流（新链）"
    );

    // stop_receive 只停 main，extra 不受影响
    kernel.stop_receive();
    let links = kernel.receive_links();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].link_id, "extra");
    kernel.stop_receive_link("extra");
    assert!(kernel.receive_links().is_empty());
    handle.stop().await;
}
