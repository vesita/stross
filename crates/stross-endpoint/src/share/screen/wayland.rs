//! Wayland 屏幕采集：xdg-desktop-portal ScreenCast + PipeWire（CPU/SHM 路径）。
//!
//! 链路：portal 会话（lamco-portal + ashpd 选源）→ 取得 PipeWire fd + 节点 →
//! lamco-pipewire 低层 [`PipeWireThreadManager`] 建流（`dmabuf=false` 强制
//! **SHM/CPU 路径**——合成器无关，KWin / Mutter / wlroots 全支持，同时避开
//! DMA-BUF linear mmap 在部分 GPU 上读回全零的厂商差异）→ **原生 BGRA 帧** →
//! 按目标帧率节流 → 写入 ffmpeg rawvideo stdin。
//!
//! **缩放/转格式交给 ffmpeg 的 swscale**（[`crate::pipeline::args::wayland_rawvideo_command`] 的
//! `-vf scale=WxH,format=yuv420p`）：之前在 Rust 里做 1080p→720p 双线性（纯 Rust、
//! 未优化 ~85ms/帧），是 serve 单帧吞吐瓶颈（也是功耗大头）。改后 serve 只按
//! stride 规整地拷贝原生 BGRA（memcpy 级），缩放全部落在 ffmpeg（SIMD、多线程、
//! 本 crate 构建模式无关）。
//!
//! **时序**：先探测到**原生分辨率**（portal `info.size`），经 oneshot 回报给管线，
//! 管线据此构建 ffmpeg 输入参数（`-video_size` 必须与采集一致）并起动 ffmpeg，
//! 再把它的 stdin 经 oneshot 送回本模块开始喂帧。因此 [`crate::capture::FfmpegBackend::start`]
//! 需在两段之间 `await`（见 [`crate::pipeline::StreamSession::spawn_wayland`]）。
//!
//! 错误回报：本模块不阻塞 [`crate::capture::FfmpegBackend::start`]——portal
//! 授权是异步系统对话框；启动/运行错误经 [`WaylandCapture::errors`] 通道
//! 转发到 `CaptureStatus.error`（流会随 ffmpeg stdin 关闭而自然结束）。

use std::time::{Duration, Instant};

use lamco_pipewire::pw_thread::PipeWireThreadManager;
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot};

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
/// 返回采集控制器 + **原生分辨率 oneshot**（`(w, h)`；管线拿到后才构建 ffmpeg
/// 输入参数并起动 ffmpeg）。**stdin 不在此传入**：管线起动 ffmpeg 后，把其 stdin
/// 经 `stdin_rx` 送回本任务，任务随即开始喂帧。
pub fn start(
    cfg: &crate::pipeline::StreamConfig,
    stdin_rx: oneshot::Receiver<ChildStdin>,
    error_tx: mpsc::Sender<String>,
) -> (WaylandCapture, oneshot::Receiver<(u32, u32)>) {
    let quality = cfg.quality;
    let (native_tx, native_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        if let Err(e) = run(quality, stdin_rx, native_tx).await {
            let _ = error_tx.send(e).await;
        }
    });
    (WaylandCapture { handle }, native_rx)
}

async fn run(
    quality: Quality,
    stdin_rx: oneshot::Receiver<ChildStdin>,
    native_tx: oneshot::Sender<(u32, u32)>,
) -> Result<(), String> {
    lamco_pipewire::init();
    let result = capture_loop(quality, stdin_rx, native_tx).await;
    lamco_pipewire::deinit();
    result
}

async fn capture_loop(
    quality: Quality,
    stdin_rx: oneshot::Receiver<ChildStdin>,
    native_tx: oneshot::Sender<(u32, u32)>,
) -> Result<(), String> {
    use lamco_pipewire::pw_thread::PipeWireThreadCommand;
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
    let (src_w, src_h) = (info.size.0, info.size.1);
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
        .with_resolution(src_w, src_h)
        .with_dmabuf(false)
        .with_buffer_count(4)
        // 把关流帧率压到目标编码帧率：DRIVER 模式会按协商帧率持续出帧，
        // 若用默认（显示器刷新率，常 60fps）会产生 60 > 30 的产>消费 差，
        // 填满帧通道 → 丢帧 + 多余 CPU。设为目标 fps 后产=消费，不再积压。
        .with_framerate(quality.fps);
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

    // ---- 3. 回传原生分辨率 → 等 ffmpeg stdin → 节流喂帧（原生 BGRA）----
    // 原生尺寸必须先给管线：ffmpeg 的 `-video_size` 必须与采集尺寸一致，否则
    // 像素错位。管线拿到后起动 ffmpeg，再把 stdin 经 oneshot 送回。
    let _ = native_tx.send((src_w, src_h));
    let mut stdin = stdin_rx
        .await
        .map_err(|_| "等待 ffmpeg stdin 失败".to_string())?;

    // lamco_pipewire 用 DRIVER 标志驱动时钟：静止桌面也按协商帧率持续出帧。
    // 因此以 interval(30fps) 稳定喂帧，PTS 与 GOP（`-g=fps*2`=2s 关键帧）保持
    // 正确，新观看端随时可接入。**缩放交给 ffmpeg swscale**，这里只把最新的
    // 原生 BGRA（按 stride 规整为紧密布局）写入 stdin；静止时复用上一帧以保持
    // PTS。不再做内容指纹/缩放（那是 CPU 瓶颈来源）。
    feed_loop(quality, &mut stdin, &mut mgr, src_w, src_h).await?;

    let _ = mgr.shutdown();
    tracing::debug!("Wayland 采集结束");
    Ok(())
}

/// 帧循环：把 pipewire 原生 BGRA 按 stride 规整为紧密布局，按目标帧率节流写入 stdin。
///
/// 不再做任何缩放/颜色转换——那是 ffmpeg（`swscale`）的事。这里只保证
/// **行紧密**（`stride` 可能带 padding，rawvideo 不认），即每帧至多一次
/// `memcpy`（紧排时整块拷贝，有 padding 时逐行拷贝），约 8MB/帧，远低于此前
/// 双线性缩放的 ~85ms/帧。
async fn feed_loop(
    quality: Quality,
    stdin: &mut ChildStdin,
    mgr: &mut PipeWireThreadManager,
    src_w: u32,
    src_h: u32,
) -> Result<(), String> {
    use lamco_pipewire::FrameBuffer;
    let interval = Duration::from_secs_f64(1.0 / f64::from(quality.fps.max(1)));
    let native_len = (src_w as usize) * (src_h as usize) * 4;
    let row = (src_w as usize) * 4;
    let mut last = vec![0u8; native_len];
    let mut got = false;
    let mut next_write = Instant::now();
    let mut sent = 0u32;
    loop {
        if let Some(frame) = mgr.recv_frame_timeout(Duration::from_millis(30)) {
            if sent == 0 && !got {
                let dlen = match &frame.buffer {
                    FrameBuffer::Memory(d) => d.len(),
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
            let stride = frame.stride as usize;
            // 尺寸须与原生一致（ffmpeg 输入固定），不一致时丢弃这一帧
            if frame.width != src_w || frame.height != src_h {
                continue;
            }
            if data.len() < native_len {
                continue;
            }
            if stride == row {
                last.copy_from_slice(&data[..native_len]);
            } else {
                // 行间距可能有 padding：逐行拷贝为紧密布局
                for y in 0..src_h as usize {
                    let src = y * stride;
                    let dst = y * row;
                    if src + row > data.len() || dst + row > native_len {
                        continue; // 越界保护（不应发生）
                    }
                    last[dst..dst + row].copy_from_slice(&data[src..src + row]);
                }
            }
            got = true;
        }
        // 到达节流点：写最新帧（尚未收到首帧时跳过）
        let now = Instant::now();
        if now < next_write {
            tokio::task::yield_now().await;
            continue;
        }
        next_write = now + interval;
        if !got {
            tokio::task::yield_now().await;
            continue;
        }
        if stdin.write_all(&last).await.is_err() {
            // ffmpeg 已退出（会话停止 / 接收端关闭）；结束采集
            break;
        }
        sent += 1;
        if sent.is_multiple_of(30) {
            tracing::debug!("Wayland 采集已送 {sent} 帧");
        }
    }
    Ok(())
}
