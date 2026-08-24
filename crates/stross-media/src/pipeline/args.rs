//! ffmpeg 命令行参数构建。
//!
//! 把 [`super::StreamConfig`] 翻译成 ffmpeg 子进程的完整命令行
//! （输入源 + 编码器 + 输出到 stdout 管道）。

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::{AudioSourceConfig, Quality, StreamConfig, VideoSource};

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

#[cfg(any(
    target_os = "macos",
    target_os = "android",
    not(any(target_os = "linux", target_os = "windows"))
))]
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

#[cfg(any(
    target_os = "macos",
    target_os = "android",
    not(any(target_os = "linux", target_os = "windows"))
))]
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
    Ok(vec![
        "-f".into(),
        "dshow".into(),
        "-i".into(),
        format!("audio={dev}"),
    ])
}

#[cfg(any(
    target_os = "macos",
    target_os = "android",
    not(any(target_os = "linux", target_os = "windows"))
))]
fn mic_input_args(_device: Option<&str>) -> Result<Vec<String>> {
    bail!("当前平台不支持 ffmpeg 音频采集")
}

#[cfg(target_os = "linux")]
fn system_input_args(monitor: &str) -> Result<Vec<String>> {
    Ok(vec![
        "-f".into(),
        "pulse".into(),
        "-i".into(),
        monitor.to_string(),
    ])
}

#[cfg(target_os = "windows")]
fn system_input_args(device: &str) -> Result<Vec<String>> {
    Ok(vec![
        "-f".into(),
        "dshow".into(),
        "-i".into(),
        format!("audio={device}"),
    ])
}

#[cfg(any(
    target_os = "macos",
    target_os = "android",
    not(any(target_os = "linux", target_os = "windows"))
))]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{AudioSourceConfig, Quality, StreamConfig, VideoSource};

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
