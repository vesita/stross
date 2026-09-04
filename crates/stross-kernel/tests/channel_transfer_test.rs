//! 节点间对等通道集成测试（即时消息、双向文件互传）。
//!
//! 验证：
//! 1. 两节点通过 `/ws/channel` 建立全双工安全连接
//! 2. 双方自由双向互发文本便签/消息
//! 3. 双方自由双向互发文件（流式分块传输与落盘校验）

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use stross_kernel::channel::ChannelManager;
use stross_kernel::kernel::id::Id;
use stross_kernel::relay::RelayServer;
use stross_view::channel::ChannelEvent;
use tokio::time::timeout;

/// 构造带有随机内容的测试文件。
async fn create_test_file(path: &std::path::Path, size: usize) {
    let mut data = Vec::with_capacity(size);
    for i in 0..size {
        data.push((i % 251) as u8);
    }
    tokio::fs::write(path, data).await.unwrap();
}

#[tokio::test]
async fn channel_bidirectional_text_chat() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("stross=debug")
        .try_init();

    // 1. 启动节点 A 的中继与通道管理器
    let dir_a = std::env::temp_dir().join(format!("stross-chan-test-a-{}", fastrand::u64(..)));
    tokio::fs::create_dir_all(&dir_a).await.unwrap();
    let mgr_a = Arc::new(ChannelManager::new(dir_a.clone(), true));

    let relay_a = RelayServer::start(0).await.unwrap();
    relay_a.set_channel_manager(mgr_a.clone());
    let relay_a_base = format!("ws://127.0.0.1:{}", relay_a.port);

    // 2. 构造节点 B 的通道管理器
    let dir_b = std::env::temp_dir().join(format!("stross-chan-test-b-{}", fastrand::u64(..)));
    tokio::fs::create_dir_all(&dir_b).await.unwrap();
    let mgr_b = Arc::new(ChannelManager::new(dir_b.clone(), true));
    let mut events_a = mgr_a.subscribe_events();
    let mut events_b = mgr_b.subscribe_events();

    let id_a = Id::from("node-pc-a");
    let id_b = Id::from("node-phone-b");

    // 3. 节点 B 主动连入节点 A 的通道
    let _session_b = mgr_b
        .connect_channel(
            &relay_a_base,
            &id_b,
            "OPPO Phone",
            id_a.clone(),
            "Ubuntu PC",
        )
        .await
        .expect("节点 B 连接节点 A 失败");

    // 等待握手事件
    let ev_b = timeout(Duration::from_secs(3), events_b.recv())
        .await
        .expect("超时 B")
        .unwrap();
    assert!(matches!(ev_b, ChannelEvent::Connected { .. }));

    let ev_a = timeout(Duration::from_secs(3), events_a.recv())
        .await
        .expect("超时 A")
        .unwrap();
    assert!(matches!(ev_a, ChannelEvent::Connected { .. }));

    // 4. 双向互发文本消息
    // A -> B 发送文本
    let msg_id_1 = mgr_a.send_text(&id_b, "你好，手机！").await.unwrap();
    assert_eq!(msg_id_1.as_u64(), 1);

    // B 接收
    let recv_b = timeout(Duration::from_secs(3), events_b.recv())
        .await
        .expect("超时")
        .unwrap();
    match recv_b {
        ChannelEvent::Message { text, is_self, .. } => {
            assert_eq!(text, "你好，手机！");
            assert!(!is_self);
        }
        other => panic!("期望收到文本消息，实际收到: {:?}", other),
    }

    // B -> A 回复文本
    let msg_id_2 = mgr_b.send_text(&id_a, "收到，电脑！").await.unwrap();
    assert_eq!(msg_id_2.as_u64(), 1);

    // A 的本地事件队列先收到本机发送成功的消息
    let ev_a_self = timeout(Duration::from_secs(3), events_a.recv())
        .await
        .expect("超时")
        .unwrap();
    match ev_a_self {
        ChannelEvent::Message { text, is_self, .. } => {
            assert_eq!(text, "你好，手机！");
            assert!(is_self);
        }
        other => panic!("期望收到本机发送事件，实际收到: {:?}", other),
    }

    // A 接收来自 B 的回复文本
    let recv_a = timeout(Duration::from_secs(3), events_a.recv())
        .await
        .expect("超时")
        .unwrap();
    match recv_a {
        ChannelEvent::Message { text, is_self, .. } => {
            assert_eq!(text, "收到，电脑！");
            assert!(!is_self);
        }
        other => panic!("期望收到对端回复文本消息，实际收到: {:?}", other),
    }

    // 清理
    let _ = tokio::fs::remove_dir_all(&dir_a).await;
    let _ = tokio::fs::remove_dir_all(&dir_b).await;
}

#[tokio::test]
async fn channel_bidirectional_file_transfer() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("stross=debug")
        .try_init();

    let temp_root = std::env::temp_dir().join(format!("stross-chan-xfer-{}", fastrand::u64(..)));
    let dir_a = temp_root.join("downloads-a");
    let dir_b = temp_root.join("downloads-b");
    tokio::fs::create_dir_all(&dir_a).await.unwrap();
    tokio::fs::create_dir_all(&dir_b).await.unwrap();

    let mgr_a = Arc::new(ChannelManager::new(dir_a.clone(), true));
    let relay_a = RelayServer::start(0).await.unwrap();
    relay_a.set_channel_manager(mgr_a.clone());
    let relay_a_base = format!("ws://127.0.0.1:{}", relay_a.port);

    let mgr_b = Arc::new(ChannelManager::new(dir_b.clone(), true));

    let mut events_a = mgr_a.subscribe_events();
    let mut events_b = mgr_b.subscribe_events();

    let id_a = Id::from("pc-node");
    let id_b = Id::from("phone-node");

    // 建立通道
    let _session_b = mgr_b
        .connect_channel(&relay_a_base, &id_b, "Phone", id_a.clone(), "PC")
        .await
        .expect("连接通道失败");

    // 等待双方 Connected
    let _ = timeout(Duration::from_secs(3), events_b.recv())
        .await
        .unwrap();
    let _ = timeout(Duration::from_secs(3), events_a.recv())
        .await
        .unwrap();

    // 1. A 发送 128KB 文件到 B
    let file_a_path = temp_root.join("test-file-from-a.bin");
    create_test_file(&file_a_path, 128 * 1024).await;
    let expected_bytes_a = tokio::fs::read(&file_a_path).await.unwrap();

    let transfer_a = mgr_a.send_file(&id_b, &file_a_path).await.unwrap();
    assert_eq!(transfer_a.as_u32(), 1);

    // 等待 B 接收完成
    let mut b_received_path: Option<PathBuf> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(500), events_b.recv()).await {
            Ok(Ok(ChannelEvent::FileCompleted {
                transfer_id,
                path,
                is_upload,
                ..
            })) => {
                assert_eq!(transfer_id, transfer_a);
                assert!(!is_upload);
                b_received_path = path.map(PathBuf::from);
                break;
            }
            Ok(Ok(_)) => {}
            _ => {}
        }
    }

    let b_path = b_received_path.expect("B 未在超时时间内收到文件完成事件");
    assert!(b_path.exists(), "B 接收文件物理上应存在: {:?}", b_path);
    let b_bytes = tokio::fs::read(&b_path).await.unwrap();
    assert_eq!(
        expected_bytes_a, b_bytes,
        "B 收到的文件内容必须与 A 发送的内容完全一致"
    );

    // 2. B 发送 96KB 文件回传到 A
    let file_b_path = temp_root.join("response-file-from-b.bin");
    create_test_file(&file_b_path, 96 * 1024).await;
    let expected_bytes_b = tokio::fs::read(&file_b_path).await.unwrap();

    let transfer_b = mgr_b.send_file(&id_a, &file_b_path).await.unwrap();
    assert_eq!(transfer_b.as_u32(), 1);

    // 等待 A 接收完成
    let mut a_received_path: Option<PathBuf> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(500), events_a.recv()).await {
            Ok(Ok(ChannelEvent::FileCompleted {
                transfer_id,
                path,
                is_upload: false,
                ..
            })) => {
                assert_eq!(transfer_id, transfer_b);
                a_received_path = path.map(PathBuf::from);
                break;
            }
            Ok(Ok(_)) => {}
            _ => {}
        }
    }

    let a_path = a_received_path.expect("A 未在超时时间内收到回传文件完成事件");
    assert!(a_path.exists(), "A 接收文件物理上应存在: {:?}", a_path);
    let a_bytes = tokio::fs::read(&a_path).await.unwrap();
    assert_eq!(
        expected_bytes_b, a_bytes,
        "A 收到的文件内容必须与 B 发送的内容完全一致"
    );

    // 清理测试目录
    let _ = tokio::fs::remove_dir_all(&temp_root).await;
}

#[tokio::test]
async fn channel_cancel_file_transfer() {
    let temp_root = std::env::temp_dir().join(format!("stross-chan-cancel-{}", fastrand::u64(..)));
    let dir_a = temp_root.join("downloads-a");
    let dir_b = temp_root.join("downloads-b");
    tokio::fs::create_dir_all(&dir_a).await.unwrap();
    tokio::fs::create_dir_all(&dir_b).await.unwrap();

    let mgr_a = Arc::new(ChannelManager::new(dir_a.clone(), true));
    let relay_a = RelayServer::start(0).await.unwrap();
    relay_a.set_channel_manager(mgr_a.clone());
    let relay_a_base = format!("ws://127.0.0.1:{}", relay_a.port);

    let mgr_b = Arc::new(ChannelManager::new(dir_b.clone(), true));

    let mut events_a = mgr_a.subscribe_events();
    let mut events_b = mgr_b.subscribe_events();

    let id_a = Id::from("node-a");
    let id_b = Id::from("node-b");

    let _session_b = mgr_b
        .connect_channel(&relay_a_base, &id_b, "B", id_a.clone(), "A")
        .await
        .unwrap();

    let _ = timeout(Duration::from_secs(3), events_b.recv())
        .await
        .unwrap();
    let _ = timeout(Duration::from_secs(3), events_a.recv())
        .await
        .unwrap();

    // 构造一个 5MB 文件
    let file_path = temp_root.join("big-file.bin");
    create_test_file(&file_path, 5 * 1024 * 1024).await;

    let transfer_id = mgr_a.send_file(&id_b, &file_path).await.unwrap();

    // 等待开始收到 Progress
    let mut saw_progress = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(ChannelEvent::FileProgress { .. })) =
            timeout(Duration::from_millis(200), events_a.recv()).await
        {
            saw_progress = true;
            break;
        }
    }
    assert!(saw_progress, "应开始传输文件并产生进度");

    // 中途主动取消
    mgr_a.cancel_transfer(&id_b, transfer_id).await.unwrap();

    // A 侧应收到 FileFailed
    let failed_a = timeout(Duration::from_secs(3), events_a.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(failed_a, ChannelEvent::FileFailed { .. }));

    // B 侧也应收到 FileFailed
    let mut b_failed = false;
    let deadline_b = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline_b {
        if let Ok(Ok(ChannelEvent::FileFailed { .. })) =
            timeout(Duration::from_millis(300), events_b.recv()).await
        {
            b_failed = true;
            break;
        }
    }
    assert!(b_failed, "B 应感知到文件传输已中断/取消");
    let _ = tokio::fs::remove_dir_all(&temp_root).await;
}

#[tokio::test]
async fn channel_path_traversal_defense() {
    use stross_kernel::channel::session::sanitize_file_name;

    assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
    assert_eq!(sanitize_file_name("/etc/shadow"), "shadow");
    assert_eq!(
        sanitize_file_name("..\\..\\windows\\system32\\calc.exe"),
        "calc.exe"
    );
    assert!(sanitize_file_name("../../../").starts_with("file_"));
    assert!(sanitize_file_name("..").starts_with("file_"));
    assert!(sanitize_file_name(".").starts_with("file_"));
    assert_eq!(sanitize_file_name("hello/world.txt"), "world.txt");
    assert_eq!(sanitize_file_name("normal_file.png"), "normal_file.png");
}
