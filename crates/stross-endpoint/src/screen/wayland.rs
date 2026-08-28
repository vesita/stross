//! Wayland 屏幕采集：xdg-desktop-portal ScreenCast + PipeWire（CPU/SHM 路径）。
//!
//! 链路：portal 会话（lamco-portal + ashpd 选源）→ 取得 PipeWire fd + 节点 →
//! lamco-pipewire 低层 [`PipeWireThreadManager`] 建流（`dmabuf=false` 强制
//! **SHM/CPU 路径**——合成器无关，KWin / Mutter / wlroots 全支持，同时避开
//! DMA-BUF linear mmap 在部分 GPU 上读回全零的厂商差异）→ BGRA 帧 →
//! 双线性缩放到编码目标分辨率 → yuv420p → 按目标帧率节流 →
//! 写入 ffmpeg rawvideo stdin（H.264 编码与 Annex-B 读循环复用既有链路）。
//!
//! 错误回报：本模块不阻塞 [`crate::capture::FfmpegBackend::start`]——portal
//! 授权是异步系统对话框；启动/运行错误经 [`WaylandCapture::errors`] 通道
//! 转发到 `CaptureStatus.error`（流会随 ffmpeg stdin 关闭而自然结束）。

use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::mpsc;

use crate::pipeline::Quality;

/// Wayland 采集控制器（持有以保活采集任务）。
pub struct WaylandCapture {
    /// 采集任务句柄（终止方式：kill ffmpeg 子进程 → stdin 关闭 → 任务自清理）。
    pub handle: tokio::task::JoinHandle<()>,
}

/// 当前会话是否为 Wayland（采集路由用）。
pub fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// 启动 Wayland 屏幕采集任务（portal 授权为异步系统对话框，不阻塞调用方）。
///
/// 必须在 tokio 运行时内调用；`cfg` 携带编码目标（分辨率/帧率）。
pub fn start(
    cfg: &crate::pipeline::StreamConfig,
    stdin: ChildStdin,
    error_tx: mpsc::Sender<String>,
) -> WaylandCapture {
    let quality = cfg.quality;
    let handle = tokio::spawn(async move {
        if let Err(e) = run(quality, stdin).await {
            let _ = error_tx.send(e).await;
        }
    });
    WaylandCapture { handle }
}

async fn run(quality: Quality, mut stdin: ChildStdin) -> Result<(), String> {
    lamco_pipewire::init();
    let result = capture_loop(quality, &mut stdin).await;
    lamco_pipewire::deinit();
    result
}

async fn capture_loop(quality: Quality, stdin: &mut ChildStdin) -> Result<(), String> {
    use lamco_pipewire::FrameBuffer;
    use lamco_pipewire::pw_thread::{PipeWireThreadCommand, PipeWireThreadManager};
    use lamco_pipewire::stream::StreamConfig as LamcoStreamConfig;

    // ---- 1. portal 会话 + 选源（系统对话框；KWin 记住授权后不再弹）----
    let portal = lamco_portal::PortalManager::with_default()
        .await
        .map_err(|e| format!("portal 初始化失败: {e}"))?;
    let screencast = portal.screencast();
    let session = screencast
        .create_session()
        .await
        .map_err(|e| format!("创建 ScreenCast 会话失败: {e}"))?;

    // lamco-portal 的 start 不先 select_sources（KWin 要求先选源）——
    // 用 ashpd 代理在同一个 session 上先选源
    use ashpd::desktop::PersistMode;
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
    let proxy = Screencast::new()
        .await
        .map_err(|e| format!("连接 portal 代理失败: {e}"))?;
    let options = SelectSourcesOptions::default()
        .set_cursor_mode(CursorMode::Hidden)
        .set_sources(SourceType::Monitor | SourceType::Window)
        .set_multiple(false)
        .set_persist_mode(PersistMode::DoNot);
    let req = proxy
        .select_sources(&session, options)
        .await
        .map_err(|e| format!("选择采集源失败: {e}"))?;
    req.response()
        .map_err(|e| format!("采集源选择被拒绝: {e}"))?;

    let (fd, streams) = screencast
        .start(&session)
        .await
        .map_err(|e| format!("启动 ScreenCast 失败: {e}"))?;
    let info = streams
        .first()
        .ok_or_else(|| "portal 未返回流（用户拒绝或未选择屏幕）".to_string())?;
    tracing::info!(
        "Wayland 屏幕采集: node={} size={:?}",
        info.node_id,
        info.size
    );

    // ---- 2. pipewire 低层采集（SHM/CPU 路径，合成器无关）----
    tracing::info!("[wayland] 启动 PipeWire 线程");
    let mut mgr =
        PipeWireThreadManager::new(fd).map_err(|e| format!("PipeWire 线程启动失败: {e}"))?;
    tracing::info!("[wayland] PipeWire 线程就绪");
    let lcfg = LamcoStreamConfig::new("stross-screen".to_string())
        .with_resolution(info.size.0, info.size.1)
        .with_dmabuf(false)
        .with_buffer_count(4);
    let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
    mgr.send_command(PipeWireThreadCommand::CreateStream {
        stream_id: 0,
        node_id: info.node_id,
        config: lcfg,
        response_tx,
    })
    .map_err(|e| format!("发送建流命令失败: {e}"))?;
    tracing::info!("[wayland] 发送建流命令");
    response_rx
        .recv()
        .map_err(|_| "建流响应通道关闭".to_string())?
        .map_err(|e| format!("建流失败: {e}"))?;
    tracing::info!("[wayland] 建流成功，进入帧循环");

    // ---- 3. 帧循环：缩放 → yuv420p → 节流 → ffmpeg stdin ----
    // Wayland portal 是**伤害驱动**（桌面静止即停发帧），而中继数据面有
    // `PUSH_SILENCE_TIMEOUT`（10s 无消息判失联拆流）。因此静止时按目标
    // 帧率**持续重发上一帧**：流保持活跃、GOP 正常推进（关键帧每 2s 一次，
    // 新观看端随时可接入；重发帧解码为相同画面，带宽 ~6KB/s 可忽略）。
    let (dst_w, dst_h) = (quality.width as usize, quality.height as usize);
    let yuv_len = dst_w * dst_h + dst_w * dst_h / 2;
    let mut yuv = vec![0u8; yuv_len];
    let mut last_frame: Option<Vec<u8>> = None;
    let interval = Duration::from_secs_f64(1.0 / quality.fps.max(1) as f64);
    let mut next_write = Instant::now();
    let mut sent = 0u32;

    loop {
        // 轮询新帧（阻塞式 std mpsc，短超时；命中立即返回）
        let frame = mgr.recv_frame_timeout(Duration::from_millis(30));
        if let Some(frame) = frame {
            if sent == 0 {
                let dlen = match &frame.buffer {
                    lamco_pipewire::FrameBuffer::Memory(d) => d.len(),
                    _ => usize::MAX,
                };
                tracing::info!(
                    "[wayland] 收到首帧 {}x{} stride={} data_len={}",
                    frame.width,
                    frame.height,
                    frame.stride,
                    dlen
                );
            }
            let FrameBuffer::Memory(data) = &frame.buffer else {
                continue; // 不应出现 DMA-BUF 帧（已强制 SHM）；跳过
            };
            if !data.is_empty() && Instant::now() >= next_write {
                crate::convert::yuv::bgra_to_yuv420p_scaled(
                    data,
                    frame.stride as usize,
                    frame.width as usize,
                    frame.height as usize,
                    dst_w,
                    dst_h,
                    &mut yuv,
                )
                .map_err(|e| format!("帧转换失败: {e}"))?;
                last_frame = Some(yuv.clone());
            }
        }
        // 到达节流点：写新帧（或静止时重发上一帧）
        let now = Instant::now();
        if now < next_write {
            tokio::task::yield_now().await;
            continue;
        }
        next_write = now + interval;
        let Some(frame_bytes) = last_frame.as_deref() else {
            tokio::task::yield_now().await;
            continue; // 尚未收到首帧（portal 授权中）
        };
        tracing::trace!(
            "[wayland] 写帧 #{sent} ({:.1}ms)",
            next_write.elapsed().as_secs_f64() * 1000.0
        );
        if stdin.write_all(frame_bytes).await.is_err() {
            // ffmpeg 已退出（会话停止 / 接收端关闭）；结束采集
            break;
        }
        sent += 1;
        if sent.is_multiple_of(30) {
            tracing::debug!("Wayland 采集已送 {sent} 帧");
        }
    }

    let _ = mgr.shutdown();
    tracing::debug!("Wayland 采集结束，共送 {sent} 帧");
    Ok(())
}
