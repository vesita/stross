//! 集成测试：`GET /api/peers` 设备发现端点。
//!
//! 验证中继把设备缓存暴露给观看端页面（局域网设备列表）；
//! mDNS 浏览本身依赖真实局域网，这里通过 `insert_peer` 手动注入验证整条链路。

use stross_kernel::relay::{PeerInfo, RelayServer};
use stross_proto::message::{RoleId, TransportId};

#[tokio::test]
async fn peers_api_lists_inserted_peers() {
    let handle = RelayServer::start(0).await.expect("启动中继");
    let url = format!("http://127.0.0.1:{}/api/peers", handle.port);

    // 初始为空列表
    let resp = reqwest::get(&url).await.expect("GET /api/peers");
    assert!(resp.status().is_success());
    assert_eq!(resp.text().await.unwrap().trim(), "[]");

    // 手动注册一台局域网设备 → API 应返回其信息（camelCase 契约）
    handle.insert_peer(PeerInfo {
        id: "192.168.1.9:8777".into(),
        name: "客厅电脑".into(),
        ip: "192.168.1.9".into(),
        port: 8777,
        roles: vec![RoleId::Sender, RoleId::Relay],
        transports: vec![TransportId::Ws, TransportId::WebRtc],
        url: "http://192.168.1.9:8777/".into(),
    });
    let resp = reqwest::get(&url).await.expect("GET /api/peers");
    let peers: Vec<serde_json::Value> = resp.json().await.expect("解析 JSON");
    assert_eq!(peers.len(), 1);
    let p = &peers[0];
    assert_eq!(p["name"], "客厅电脑");
    assert_eq!(p["ip"], "192.168.1.9");
    assert_eq!(p["port"], 8777);
    assert_eq!(p["roles"][0], "sender");
    assert_eq!(p["roles"][1], "relay");
    assert_eq!(p["url"], "http://192.168.1.9:8777/");

    handle.stop().await;
}
