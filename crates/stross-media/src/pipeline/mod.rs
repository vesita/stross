//! ffmpeg 采集与编码管线。
//!
//! 桌面端用 ffmpeg 完成采集 + 编码（借鉴 OBS 的 ffmpeg 依赖思路，但编排在 Rust 里）：
//!
//! * 视频进程：屏幕（gdigrab / x11grab）、摄像头（dshow / v4l2）或 lavfi 测试画面
//!   → H.264 (Annex-B) → stdout
//! * 音频进程：麦克风 + 系统声音（PulseAudio monitor / Stereo Mix）
//!   → AAC (ADTS) → stdout
//!
//! Rust 侧读取两个子进程的 stdout，用 [`AnnexBSplitter`](crate::nal::AnnexBSplitter)/
//! [`AdtsSplitter`](crate::adts::AdtsSplitter) 切成帧，打上时间戳后送入
//! [`StreamSession`] 的帧通道。
//!
//! 模块划分：
//!
//! * 配置类型（[`StreamConfig`] / [`Quality`] / [`VideoSource`] / [`AudioSourceConfig`]）
//! * [`args`]：ffmpeg 命令行参数构建
//! * [`StreamSession`]：子进程生命周期与管道读取

mod args;

pub use args::{audio_command, ffmpeg_available, ffmpeg_bin, video_command};

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use stross_proto::frame::{CODEC_AAC, CODEC_H264, FLAG_KEYFRAME, Frame, TRACK_AUDIO, TRACK_VIDEO};
use stross_proto::message::CodecId;

use crate::adts::AdtsSplitter;
use crate::nal::{AccessUnitBuilder, AnnexBSplitter};

/// 画质档位。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Quality {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl Quality {
    pub const LOW: Quality = Quality {
        width: 640,
        height: 360,
        fps: 24,
        bitrate_kbps: 800,
    };
    pub const MEDIUM: Quality = Quality {
        width: 1280,
        height: 720,
        fps: 30,
        bitrate_kbps: 2500,
    };
    pub const HIGH: Quality = Quality {
        width: 1920,
        height: 1080,
        fps: 30,
        bitrate_kbps: 6000,
    };

    /// 预设列表 `(显示名, 配置)`。
    pub fn presets() -> [(&'static str, Quality); 3] {
        [
            ("低 (640×360@24)", Quality::LOW),
            ("中 (1280×720@30)", Quality::MEDIUM),
            ("高 (1920×1080@30)", Quality::HIGH),
        ]
    }

    /// GOP（关键帧间隔，帧数），默认 2 秒。
    pub fn gop(&self) -> u32 {
        (self.fps * 2).max(1)
    }
}

impl Default for Quality {
    fn default() -> Self {
        Quality::MEDIUM
    }
}

/// 视频源。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum VideoSource {
    /// 整个主屏幕（Windows: gdigrab；Linux: x11grab）。
    Screen,
    /// 摄像头；`device` 为 `CameraDevice.id`。
    Camera { device: Option<String> },
    /// lavfi 测试画面（如 `testsrc2`、`smptebars`），方便无设备时演示。
    Synthetic { pattern: String },
}

/// 音频源配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioSourceConfig {
    /// 麦克风设备；`None` = 系统默认输入。
    pub mic: Option<String>,
    /// 系统声音（回环采集设备）；`None` = 不采集。
    pub system_audio: Option<String>,
    /// 合成音源（lavfi `sine`，频率 Hz）；`Some` 时取代真实采集，
    /// 无设备环境测试 / 演示用（见播放侧解码回路的集成测试）。
    #[serde(default)]
    pub synthetic: Option<u32>,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u8,
    #[serde(default = "default_audio_bitrate")]
    pub bitrate_kbps: u32,
}

fn default_sample_rate() -> u32 {
    48_000
}
fn default_channels() -> u8 {
    2
}
fn default_audio_bitrate() -> u32 {
    128
}

impl Default for AudioSourceConfig {
    fn default() -> Self {
        Self {
            mic: None,
            system_audio: None,
            synthetic: None,
            sample_rate: default_sample_rate(),
            channels: default_channels(),
            bitrate_kbps: default_audio_bitrate(),
        }
    }
}

impl AudioSourceConfig {
    /// 合成测试音（440Hz sine）：无设备环境下验证音频链路。
    ///
    /// `--audio` 类 CLI 参数用它——此前直接用 [`AudioSourceConfig::default`]
    /// 导致 synthetic/mic/system_audio 全为 `None`，ffmpeg 无音频输入，
    /// 推流实际无声（音频链路从未被真实数据验证，D3 反向音频验收的前提）。
    pub fn synthetic_test() -> Self {
        Self {
            synthetic: Some(440),
            ..Self::default()
        }
    }
}

/// 一次推流的完整配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamConfig {
    pub stream_id: String,
    pub title: String,
    #[serde(default)]
    pub video: Option<VideoSource>,
    #[serde(default)]
    pub quality: Quality,
    #[serde(default)]
    pub audio: Option<AudioSourceConfig>,
    /// 限制推流时长（秒）；`None` = 无限。测试/演示用。
    #[serde(default)]
    pub duration_secs: Option<u32>,
    /// 一次性接入凭证（跨设备推流到对方受控中继用；本机推流为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_token: Option<String>,
}

impl StreamConfig {
    /// CLI 合成源推流配置（测试 / 演示：testsrc2 画面 + 可选 440Hz 测试音）。
    ///
    /// `push` / `ctrl start-stream` / `demo_push` 共用，避免各处手拼字段
    /// （重复实现，曾出现 `--audio` 无声等不一致）。
    pub fn cli_synthetic(
        stream_id: String,
        title: String,
        quality: Quality,
        secs: u32,
        audio: bool,
        share_token: Option<String>,
    ) -> Self {
        let mut cfg = Self {
            stream_id,
            title,
            video: Some(VideoSource::Synthetic {
                pattern: "testsrc2".into(),
            }),
            quality,
            audio: None,
            duration_secs: Some(secs),
            share_token,
        };
        if audio {
            cfg.audio = Some(AudioSourceConfig::synthetic_test());
        }
        cfg
    }

    /// 生成推流端注册用的 `Hello` 控制消息。
    pub fn hello(&self) -> stross_proto::message::ControlMessage {
        stross_proto::message::ControlMessage::Hello {
            stream_id: self.stream_id.clone(),
            title: self.title.clone(),
            video: self.video_track_info(),
            audio: self.audio_track_info(),
            share_token: self.share_token.clone(),
        }
    }

    /// 生成 Hello 消息里的轨道信息（供观看端展示）。
    pub fn video_track_info(&self) -> Option<stross_proto::message::TrackInfo> {
        self.video
            .as_ref()
            .map(|_| stross_proto::message::TrackInfo {
                codec: CodecId::H264,
                width: Some(self.quality.width),
                height: Some(self.quality.height),
                fps: Some(self.quality.fps),
                sample_rate: None,
                channels: None,
            })
    }

    pub fn audio_track_info(&self) -> Option<stross_proto::message::TrackInfo> {
        self.audio
            .as_ref()
            .map(|a| stross_proto::message::TrackInfo {
                codec: CodecId::Aac,
                width: None,
                height: None,
                fps: None,
                sample_rate: Some(a.sample_rate),
                channels: Some(a.channels),
            })
    }
}

// ---------------------------------------------------------------------------
// 会话：启动子进程 + 读取管道
// ---------------------------------------------------------------------------

/// 一个推流会话：管理 ffmpeg 子进程，把解析好的帧送入通道。
pub struct StreamSession {
    video: Option<Child>,
    audio: Option<Child>,
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

        if cfg.video.is_some() {
            let args = video_command(cfg)?;
            let mut child = spawn_ffmpeg(&args)?;
            let stdout = child.stdout.take().context("视频进程没有 stdout")?;
            let tx2 = tx.clone();
            let ff = first_frame.clone();
            tokio::spawn(read_video_loop(stdout, tx2, started, ff));
            video = Some(child);
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

    /// 停止所有子进程（会触发读循环结束）。
    pub async fn stop(&mut self) {
        for child in [&mut self.video, &mut self.audio].into_iter().flatten() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
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
