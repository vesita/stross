//! ffmpeg 采集与编码管线。
//!
//! 桌面端用 ffmpeg 完成采集 + 编码（借鉴 OBS 的 ffmpeg 依赖思路，但编排在 Rust 里）：
//!
//! * 视频进程：屏幕（gdigrab / x11grab）、摄像头（dshow / v4l2）或 lavfi 测试画面
//!   → H.264 (Annex-B) → stdout
//! * 音频进程：麦克风 + 系统声音（PulseAudio monitor / Stereo Mix）
//!   → AAC (ADTS) → stdout
//!
//! Rust 侧读取两个子进程的 stdout，用 [`AnnexBSplitter`]/[`AdtsSplitter`]
//! 切成帧，打上时间戳后送入 [`StreamSession`] 的帧通道。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use stross_proto::frame::{Frame, CODEC_AAC, CODEC_H264, FLAG_KEYFRAME, TRACK_AUDIO, TRACK_VIDEO};

use crate::adts::AdtsSplitter;
use crate::nal::{AccessUnitBuilder, AnnexBSplitter};

/// ffmpeg 可执行文件：优先 `STROSS_FFMPEG` 环境变量，否则 PATH 中的 `ffmpeg`。
pub fn ffmpeg_bin() -> PathBuf {
    std::env::var_os("STROSS_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

/// ffmpeg 是否可用。
pub fn ffmpeg_available() -> bool {
    std::process::Command::new(ffmpeg_bin())
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

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
            sample_rate: default_sample_rate(),
            channels: default_channels(),
            bitrate_kbps: default_audio_bitrate(),
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
}

impl StreamConfig {
    /// 生成 Hello 消息里的轨道信息（供观看端展示）。
    pub fn video_track_info(&self) -> Option<stross_proto::message::TrackInfo> {
        self.video.as_ref().map(|_| stross_proto::message::TrackInfo {
            codec: "h264".into(),
            width: Some(self.quality.width),
            height: Some(self.quality.height),
            fps: Some(self.quality.fps),
            sample_rate: None,
            channels: None,
        })
    }

    pub fn audio_track_info(&self) -> Option<stross_proto::message::TrackInfo> {
        self.audio.as_ref().map(|a| stross_proto::message::TrackInfo {
            codec: "aac".into(),
            width: None,
            height: None,
            fps: None,
            sample_rate: Some(a.sample_rate),
            channels: Some(a.channels),
        })
    }
}

// ---------------------------------------------------------------------------
// ffmpeg 参数构建
// ---------------------------------------------------------------------------

/// 视频输入参数。
fn video_input_args(src: &VideoSource, q: &Quality) -> Result<Vec<String>> {
    let s = |v: &str| v.to_string();
    match src {
        VideoSource::Screen => screen_input_args(q),
        VideoSource::Camera { device } => camera_input_args(device.as_deref(), q),
        VideoSource::Synthetic { pattern } => Ok(vec![
            s("-f"),
            s("lavfi"),
            s("-i"),
            format!("{pattern}=size={}x{}:rate={}", q.width, q.height, q.fps),
        ]),
    }
}

#[cfg(target_os = "linux")]
fn screen_input_args(q: &Quality) -> Result<Vec<String>> {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    Ok(vec![
        "-f".into(),
        "x11grab".into(),
        "-video_size".into(),
        format!("{}x{}", q.width, q.height),
        "-framerate".into(),
        q.fps.to_string(),
        "-i".into(),
        display,
    ])
}

#[cfg(target_os = "windows")]
fn screen_input_args(q: &Quality) -> Result<Vec<String>> {
    Ok(vec![
        "-f".into(),
        "gdigrab".into(),
        "-framerate".into(),
        q.fps.to_string(),
        "-video_size".into(),
        format!("{}x{}", q.width, q.height),
        "-i".into(),
        "desktop".into(),
    ])
}

#[cfg(any(target_os = "macos", target_os = "android", not(any(target_os = "linux", target_os = "windows"))))]
fn screen_input_args(_q: &Quality) -> Result<Vec<String>> {
    bail!("当前平台不支持 ffmpeg 屏幕采集，请使用原生采集路径")
}

#[cfg(target_os = "linux")]
fn camera_input_args(device: Option<&str>, q: &Quality) -> Result<Vec<String>> {
    let dev = device
        .map(|d| d.to_string())
        .unwrap_or_else(|| "/dev/video0".into());
    Ok(vec![
        "-f".into(),
        "v4l2".into(),
        "-video_size".into(),
        format!("{}x{}", q.width, q.height),
        "-framerate".into(),
        q.fps.to_string(),
        "-i".into(),
        dev,
    ])
}

#[cfg(target_os = "windows")]
fn camera_input_args(device: Option<&str>, q: &Quality) -> Result<Vec<String>> {
    let dev = device.context("Windows 下必须指定摄像头设备名")?;
    Ok(vec![
        "-f".into(),
        "dshow".into(),
        "-video_size".into(),
        format!("{}x{}", q.width, q.height),
        "-framerate".into(),
        q.fps.to_string(),
        "-i".into(),
        format!("video={dev}"),
    ])
}

#[cfg(any(target_os = "macos", target_os = "android", not(any(target_os = "linux", target_os = "windows"))))]
fn camera_input_args(_device: Option<&str>, _q: &Quality) -> Result<Vec<String>> {
    bail!("当前平台不支持 ffmpeg 摄像头采集，请使用原生采集路径")
}

/// 视频编码参数（H.264 Annex-B，SPS/PPS 重复在关键帧前，方便观看端随时接入）。
fn video_encode_args(q: &Quality) -> Vec<String> {
    let gop = q.gop().to_string();
    let br = q.bitrate_kbps.to_string();
    vec![
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-tune".into(),
        "zerolatency".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-g".into(),
        gop.clone(),
        "-keyint_min".into(),
        gop,
        "-sc_threshold".into(),
        "0".into(),
        "-x264-params".into(),
        // repeat_headers：每个关键帧前重复 SPS/PPS，观看端随时可接入
        // slices=1：关闭 slice 线程，保证一帧只有一个 slice，便于按帧切分
        "repeat_headers=1:slices=1".into(),
        "-b:v".into(),
        format!("{br}k"),
        "-maxrate".into(),
        format!("{br}k"),
        "-bufsize".into(),
        format!("{}k", q.bitrate_kbps * 2),
        "-f".into(),
        "h264".into(),
        "pipe:1".into(),
    ]
}

/// 音频输入计划：输入参数组 + 混音过滤器。
struct AudioPlan {
    inputs: Vec<Vec<String>>,
    filter: Option<String>,
    map: Vec<String>,
}

fn audio_plan(a: &AudioSourceConfig) -> Result<AudioPlan> {
    let mut inputs: Vec<Vec<String>> = Vec::new();
    if a.mic.is_some() || a.system_audio.is_none() {
        // 采集麦克风（未指定时用默认输入）
        inputs.push(mic_input_args(a.mic.as_deref())?);
    }
    if let Some(sys) = &a.system_audio {
        inputs.push(system_input_args(sys)?);
    }
    match inputs.len() {
        0 => bail!("没有可用的音频输入"),
        1 => Ok(AudioPlan {
            inputs,
            filter: None,
            map: vec!["0:a".into()],
        }),
        2 => Ok(AudioPlan {
            inputs,
            filter: Some("[0:a][1:a]amix=inputs=2:duration=longest:normalize=0[aout]".into()),
            map: vec!["[aout]".into()],
        }),
        _ => bail!("最多支持两路音频输入"),
    }
}

#[cfg(target_os = "linux")]
fn mic_input_args(device: Option<&str>) -> Result<Vec<String>> {
    let name = device.unwrap_or("default").to_string();
    Ok(vec!["-f".into(), "pulse".into(), "-i".into(), name])
}

#[cfg(target_os = "windows")]
fn mic_input_args(device: Option<&str>) -> Result<Vec<String>> {
    let dev = device.context("Windows 下必须指定麦克风设备名")?;
    Ok(vec!["-f".into(), "dshow".into(), "-i".into(), format!("audio={dev}")])
}

#[cfg(any(target_os = "macos", target_os = "android", not(any(target_os = "linux", target_os = "windows"))))]
fn mic_input_args(_device: Option<&str>) -> Result<Vec<String>> {
    bail!("当前平台不支持 ffmpeg 音频采集")
}

#[cfg(target_os = "linux")]
fn system_input_args(monitor: &str) -> Result<Vec<String>> {
    Ok(vec!["-f".into(), "pulse".into(), "-i".into(), monitor.to_string()])
}

#[cfg(target_os = "windows")]
fn system_input_args(device: &str) -> Result<Vec<String>> {
    Ok(vec!["-f".into(), "dshow".into(), "-i".into(), format!("audio={device}")])
}

#[cfg(any(target_os = "macos", target_os = "android", not(any(target_os = "linux", target_os = "windows"))))]
fn system_input_args(_device: &str) -> Result<Vec<String>> {
    bail!("当前平台不支持 ffmpeg 回环采集")
}

/// 音频编码参数（AAC ADTS）。
fn audio_encode_args(a: &AudioSourceConfig) -> Vec<String> {
    vec![
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        format!("{}k", a.bitrate_kbps),
        "-ar".into(),
        a.sample_rate.to_string(),
        "-ac".into(),
        a.channels.to_string(),
        "-f".into(),
        "adts".into(),
        "pipe:1".into(),
    ]
}

/// 构建视频子进程的完整命令行。
pub fn video_command(cfg: &StreamConfig) -> Result<Vec<String>> {
    let src = cfg.video.as_ref().context("没有视频源")?;
    // `-re`：按原生帧率读取输入（lavfi 测试源默认会全速生成，必须节流；
    // 真实采集源本身已按帧率驱动，加 `-re` 无副作用）
    let mut args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-nostdin".into(),
        "-re".into(),
    ];
    args.extend(video_input_args(src, &cfg.quality)?);
    if let Some(d) = cfg.duration_secs {
        args.push("-t".into());
        args.push(d.to_string());
    }
    args.extend(video_encode_args(&cfg.quality));
    Ok(args)
}

/// 构建音频子进程的完整命令行。
pub fn audio_command(cfg: &StreamConfig) -> Result<Vec<String>> {
    let a = cfg.audio.as_ref().context("没有音频源")?;
    let plan = audio_plan(a)?;
    let mut args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-nostdin".into(),
        "-re".into(),
    ];
    for input in &plan.inputs {
        args.extend(input.iter().cloned());
    }
    if let Some(f) = &plan.filter {
        args.push("-filter_complex".into());
        args.push(f.clone());
    }
    args.push("-map".into());
    args.extend(plan.map.iter().cloned());
    args.extend(audio_encode_args(a));
    Ok(args)
}

// ---------------------------------------------------------------------------
// 会话：启动子进程 + 读取管道
// ---------------------------------------------------------------------------

/// 一个推流会话：管理 ffmpeg 子进程，把解析好的帧送入通道。
pub struct StreamSession {
    video: Option<Child>,
    audio: Option<Child>,
    started: Instant,
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
        let mut video = None;
        let mut audio = None;

        if cfg.video.is_some() {
            let args = video_command(cfg)?;
            let mut child = spawn_ffmpeg(&args)?;
            let stdout = child.stdout.take().context("视频进程没有 stdout")?;
            let tx2 = tx.clone();
            tokio::spawn(read_video_loop(stdout, tx2, started));
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
            tx,
        })
    }

    /// 会话已运行时长（毫秒）。
    pub fn elapsed_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    /// 停止所有子进程（会触发读循环结束）。
    pub async fn stop(&mut self) {
        for child in [&mut self.video, &mut self.audio]
            .into_iter()
            .flatten()
        {
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
    Ok(cmd.spawn().context("启动 ffmpeg 失败")?)
}

/// 视频读循环：切 NAL → 组访问单元 → 发帧。
async fn read_video_loop(
    mut stdout: tokio::process::ChildStdout,
    tx: mpsc::Sender<Frame>,
    started: Instant,
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
                        let pts = started.elapsed().as_millis() as u32;
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
            .send(Frame::new(TRACK_VIDEO, CODEC_H264, flags, pts, unit.to_annex_b()))
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

    #[test]
    fn video_command_synthetic() {
        let cfg = StreamConfig {
            stream_id: "t".into(),
            title: "t".into(),
            video: Some(VideoSource::Synthetic {
                pattern: "testsrc2".into(),
            }),
            quality: Quality::LOW,
            audio: None,
            duration_secs: Some(2),
        };
        let args = video_command(&cfg).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("lavfi"));
        assert!(joined.contains("testsrc2"));
        assert!(joined.contains("-f h264 pipe:1"));
        assert!(joined.contains("repeat_headers=1:slices=1"));
        assert!(joined.contains("-t 2"));
    }

    #[test]
    fn audio_command_default_mic() {
        let cfg = StreamConfig {
            stream_id: "t".into(),
            title: "t".into(),
            video: None,
            quality: Quality::LOW,
            audio: Some(AudioSourceConfig::default()),
            duration_secs: None,
        };
        let args = audio_command(&cfg).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("-f adts pipe:1"), "args: {joined}");
        assert!(joined.contains("-c:a aac"), "args: {joined}");
        assert!(joined.contains("-map 0:a"), "args: {joined}");
    }
}
