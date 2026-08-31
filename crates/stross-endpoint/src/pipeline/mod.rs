//! ffmpeg 采集与编码管线。
//!
//! 桌面端用 ffmpeg 完成采集 + 编码（借鉴 OBS 的 ffmpeg 依赖思路，但编排在 Rust 里）：
//!
//! * 视频进程：屏幕（gdigrab / x11grab）、摄像头（dshow / v4l2）或 lavfi 测试画面
//!   → H.264 (Annex-B) → stdout
//! * 音频进程：麦克风 + 系统声音（PulseAudio monitor / Stereo Mix）
//!   → AAC (ADTS) → stdout
//!
//! Rust 侧读取两个子进程的 stdout，用 [`AnnexBSplitter`](crate::codec::nal::AnnexBSplitter)/
//! [`AdtsSplitter`](crate::codec::adts::AdtsSplitter) 切成帧，打上时间戳后送入
//! [`StreamSession`] 的帧通道。
//!
//! 模块划分：
//!
//! * 配置类型（[`StreamConfig`] / [`Quality`] / [`VideoSource`] / [`AudioSourceConfig`]）
//! * [`args`]：ffmpeg 命令行参数构建
//! * [`StreamSession`]：子进程生命周期与管道读取

mod args;

pub use args::{
    audio_command, ffmpeg_available, ffmpeg_bin, rawvideo_video_command, video_command,
};

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use stross_proto::frame::{CODEC_AAC, CODEC_H264, FLAG_KEYFRAME, Frame, TRACK_AUDIO, TRACK_VIDEO};

use crate::codec::adts::AdtsSplitter;
use crate::codec::nal::{AccessUnitBuilder, AnnexBSplitter};

/// 数据契约（推流配置）单一真源在 stross-types（分享端与订阅端之间传输的
/// 纯数据载荷）；此处重导出保持 `stross_endpoint::pipeline::*` 路径兼容。
pub use stross_types::contract::{AudioSourceConfig, Quality, StreamConfig, VideoSource};

// ---------------------------------------------------------------------------
// 会话：启动子进程 + 读取管道
// ---------------------------------------------------------------------------

/// 一个推流会话：管理 ffmpeg 子进程，把解析好的帧送入通道。
pub struct StreamSession {
    video: Option<Child>,
    audio: Option<Child>,
    /// Wayland 屏幕采集控制器（仅 Wayland 屏幕共享时存在；持有以保活任务，
    /// 停止经 ffmpeg 子进程 stdin 关闭自清理）。
    #[cfg(all(target_os = "linux", feature = "wayland-capture"))]
    #[allow(dead_code)] // 只持有不读取，用于维持采集任务存活
    wayland: Option<crate::share::screen::wayland::WaylandCapture>,
    /// 采集侧错误通道（portal 授权失败 / 协商失败等；FfmpegBackend 转发到
    /// CaptureStatus.error——桌面侧 CaptureStatusView 轮询展示）。
    error_rx: Option<mpsc::Receiver<String>>,
    started: Instant,
    /// 会话起点墙上时刻（与 [`Self::started`] 同一时刻）；延迟校准用
    /// （`receive --calibrate` 读推流端 `--report-start` 的同一文件）。
    pub started_wall: SystemTime,
    /// 首个视频帧产出时的墙上时刻 + 其 pts（比 `started_wall` 精确：排除
    /// ffmpeg 预热，`(wall, pts0)` 即首帧的编码时刻；校准延迟时优先用它）。
    pub first_frame: Arc<std::sync::Mutex<Option<(SystemTime, u32)>>>,
    /// 持有发送端，保证推流通道在会话存续期间一直打开
    /// （读循环只持 clone；若此处不持有，读循环一结束推流就会被判定为结束）。
    #[allow(dead_code)] // 只持有不读取，用于维持通道存活
    tx: mpsc::Sender<Frame>,
}

impl StreamSession {
    /// 启动 ffmpeg 子进程并把帧送入 `tx`。
    pub fn spawn(cfg: &StreamConfig, tx: mpsc::Sender<Frame>) -> Result<Self> {
        if !ffmpeg_available() {
            bail!("未找到 ffmpeg。请安装 ffmpeg，或设置 STROSS_FFMPEG 指向可执行文件。");
        }
        let started = Instant::now();
        let started_wall = SystemTime::now();
        let first_frame = Arc::new(std::sync::Mutex::new(None));
        let mut video = None;
        let mut audio = None;
        #[cfg(all(target_os = "linux", feature = "wayland-capture"))]
        let wayland = None; // 常规路径无 Wayland 采集（wayland 走 spawn_wayland）
        let (error_tx, error_rx) = mpsc::channel(4);

        if cfg.video.is_some() {
            // 常规视频路径（X11 / Windows / lavfi / 摄像头）：ffmpeg 采集。
            // **Wayland 屏幕共享不走这里**——它需先探测原生分辨率再起 ffmpeg
            // （缩放交给 swscale），由 [`Self::spawn_wayland`] 处理（见
            // [`crate::capture::FfmpegBackend::start`] 的按需路由）。
            let args = video_command(cfg)?;
            let mut child = spawn_ffmpeg(&args)?;
            let stdout = child.stdout.take().context("视频进程没有 stdout")?;
            let tx2 = tx.clone();
            let ff = first_frame.clone();
            tokio::spawn(read_video_loop(stdout, tx2, started, ff));
            video = Some(child);
            drop(error_tx); // 无 Wayland 采集：错误通道关闭
        }

        if cfg.audio.is_some() {
            let args = audio_command(cfg)?;
            let mut child = spawn_ffmpeg(&args)?;
            let stdout = child.stdout.take().context("音频进程没有 stdout")?;
            let tx2 = tx.clone();
            tokio::spawn(read_audio_loop(stdout, tx2, started));
            audio = Some(child);
        }

        if video.is_none() && audio.is_none() {
            bail!("至少需要一个视频或音频源");
        }

        Ok(Self {
            video,
            audio,
            #[cfg(all(target_os = "linux", feature = "wayland-capture"))]
            wayland,
            error_rx: Some(error_rx),
            started,
            started_wall,
            first_frame,
            tx,
        })
    }

    /// 会话已运行时长（毫秒）。
    pub fn elapsed_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    /// 取走采集侧错误通道（一次性；无 Wayland 采集时为 `None`）。
    pub const fn take_error_rx(&mut self) -> Option<mpsc::Receiver<String>> {
        self.error_rx.take()
    }

    /// 停止所有子进程（会触发读循环结束）。
    pub async fn stop(&mut self) {
        for child in [&mut self.video, &mut self.audio].into_iter().flatten() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    /// 启动 **Wayland 屏幕共享** 的推流会话（异步）。
    ///
    /// 与 [`Self::spawn`] 的差别：Wayland 屏幕采集需在 portal/pipewire 建立后
    /// **先探测原生分辨率**，再据此构建 ffmpeg 输入参数（`-video_size` 必须与
    /// 采集尺寸一致）并起动 ffmpeg，最后把其 stdin 经 oneshot 送回采集任务开始喂帧。
    /// **缩放/转格式交给 ffmpeg swscale**（[`args::wayland_rawvideo_command`]），
    /// Rust 侧只按 stride 规整拷贝原生 BGRA（见 [`crate::share::screen::wayland`]）。
    /// 仅 Linux + `wayland-capture` feature 编译；调用方应先用
    /// [`is_wayland_screen`] 判定。
    #[cfg(all(target_os = "linux", feature = "wayland-capture"))]
    pub async fn spawn_wayland(cfg: &StreamConfig, tx: mpsc::Sender<Frame>) -> Result<Self> {
        if !ffmpeg_available() {
            bail!("未找到 ffmpeg。请安装 ffmpeg，或设置 STROSS_FFMPEG 指向可执行文件。");
        }
        if cfg.video.is_none() {
            bail!("spawn_wayland 需要视频源（Wayland 屏幕共享）");
        }
        let started = Instant::now();
        let started_wall = SystemTime::now();
        let first_frame = Arc::new(std::sync::Mutex::new(None));
        let mut audio = None;
        let (error_tx, error_rx) = mpsc::channel(4);

        // 音频（与 spawn 一致）
        if cfg.audio.is_some() {
            let args = audio_command(cfg)?;
            let mut child = spawn_ffmpeg(&args)?;
            let stdout = child.stdout.take().context("音频进程没有 stdout")?;
            let tx2 = tx.clone();
            tokio::spawn(read_audio_loop(stdout, tx2, started));
            audio = Some(child);
        }

        // Wayland 两段式：采集任务先回传原生分辨率
        let (stdin_tx, stdin_rx) = oneshot::channel();
        let (wayland, native_rx) = crate::share::screen::wayland::start(cfg, stdin_rx, error_tx);
        let (src_w, src_h) = native_rx
            .await
            .map_err(|_| anyhow::anyhow!("Wayland 采集未返回分辨率（portal 已关闭）"))?;

        // 拿到原生尺寸后再起 ffmpeg（输入尺寸一致，缩放走 swscale）
        let args = args::wayland_rawvideo_command(cfg, src_w, src_h)?;
        let mut child = spawn_ffmpeg_piped(&args)?;
        let stdout = child.stdout.take().context("视频进程没有 stdout")?;
        let stdin = child.stdin.take().context("视频进程没有 stdin")?;
        let tx2 = tx.clone();
        let ff = first_frame.clone();
        tokio::spawn(read_video_loop(stdout, tx2, started, ff));
        // 送 stdin：采集任务随即开始喂帧
        let _ = stdin_tx.send(stdin);

        Ok(Self {
            video: Some(child),
            audio,
            wayland: Some(wayland),
            error_rx: Some(error_rx),
            started,
            started_wall,
            first_frame,
            tx,
        })
    }
}

/// 是否为「Wayland 屏幕共享」路径（决定用 [`StreamSession::spawn_wayland`]
/// 而非 [`StreamSession::spawn`]）。仅 Linux + `wayland-capture` feature 下为真。
pub fn is_wayland_screen(cfg: &StreamConfig) -> bool {
    #[cfg(all(target_os = "linux", feature = "wayland-capture"))]
    {
        matches!(cfg.video, Some(VideoSource::Screen))
            && crate::share::screen::wayland::is_wayland_session()
    }
    #[cfg(not(all(target_os = "linux", feature = "wayland-capture")))]
    {
        let _ = cfg;
        false
    }
}

fn spawn_ffmpeg(args: &[String]) -> Result<Child> {
    let mut cmd = Command::new(ffmpeg_bin());
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    cmd.spawn().context("启动 ffmpeg 失败")
}

/// 带 stdin 管道的 ffmpeg 启动（Wayland 屏幕采集：Rust 侧喂 rawvideo 帧）。
#[cfg(all(target_os = "linux", feature = "wayland-capture"))]
fn spawn_ffmpeg_piped(args: &[String]) -> Result<Child> {
    let mut cmd = Command::new(ffmpeg_bin());
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    cmd.spawn().context("启动 ffmpeg 失败")
}

/// 视频读循环：切 NAL → 组访问单元 → 发帧。
///
/// `first_frame`：首个访问单元产出时记录 (墙上时刻, pts)（延迟校准用；
/// 只记录一次，消除首帧管道延迟偏差）。
async fn read_video_loop(
    mut stdout: tokio::process::ChildStdout,
    tx: mpsc::Sender<Frame>,
    started: Instant,
    first_frame: Arc<std::sync::Mutex<Option<(SystemTime, u32)>>>,
) {
    let mut splitter = AnnexBSplitter::new();
    let mut au = AccessUnitBuilder::new();
    let mut buf = vec![0u8; 128 * 1024];
    let mut sent = 0u32;
    loop {
        match stdout.read(&mut buf).await {
            Ok(0) => {
                // 正常结束（时长限制）或 ffmpeg 崩溃都会走到这里
                tracing::debug!("视频管道 EOF，已发送 {sent} 帧");
                break;
            }
            Ok(n) => {
                for nal in splitter.feed(&buf[..n]) {
                    if let Some(unit) = au.push(nal) {
                        // 首帧墙时刻（块作用域持锁：guard 在块末立即释放，
                        // 避免 std MutexGuard 跨 await 导致 future 非 Send）
                        let pts = started.elapsed().as_millis() as u32;
                        {
                            let mut ff = first_frame.lock().unwrap();
                            if ff.is_none() {
                                *ff = Some((SystemTime::now(), pts));
                            }
                        }
                        let flags = if unit.keyframe { FLAG_KEYFRAME } else { 0 };
                        let payload = unit.to_annex_b();
                        sent += 1;
                        if tx
                            .send(Frame::new(TRACK_VIDEO, CODEC_H264, flags, pts, payload))
                            .await
                            .is_err()
                        {
                            return; // 接收端已关闭
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("读取视频管道失败: {e}");
                break;
            }
        }
    }
    // 冲刷最后一个访问单元
    if let Some(unit) = au.finish() {
        let pts = started.elapsed().as_millis() as u32;
        let flags = if unit.keyframe { FLAG_KEYFRAME } else { 0 };
        sent += 1;
        let _ = tx
            .send(Frame::new(
                TRACK_VIDEO,
                CODEC_H264,
                flags,
                pts,
                unit.to_annex_b(),
            ))
            .await;
    }
    tracing::debug!("视频读循环结束，共发送 {sent} 帧");
}

/// 音频读循环：切 ADTS 帧 → 发帧。
async fn read_audio_loop(
    mut stdout: tokio::process::ChildStdout,
    tx: mpsc::Sender<Frame>,
    started: Instant,
) {
    let mut splitter = AdtsSplitter::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match stdout.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                for frame in splitter.feed(&buf[..n]) {
                    let pts = started.elapsed().as_millis() as u32;
                    if tx
                        .send(Frame::new(TRACK_AUDIO, CODEC_AAC, 0, pts, frame))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("读取音频管道失败: {e}");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_gop() {
        assert_eq!(Quality::MEDIUM.gop(), 60);
    }
}
