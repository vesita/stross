//! 真实 ffmpeg 集成回归：repeat_headers=1 的 H.264 流里**每个关键帧必须含 SPS/PPS**。
//!
//! 曾修 bug：`AccessUnitBuilder` 把配置 NAL 配给上一帧，后续关键帧变成"光杆 IDR"，
//! relay 缓存转发后，中途接入的观看端（含级联代理）无法解析分辨率，解码 0 帧。
//! 本测试用真实 ffmpeg（与推流端同参数）验证组装链路，防止回归。
use std::io::Read;
use std::process::{Command, Stdio};

use stross_media::nal::{AccessUnitBuilder, AnnexBSplitter, NAL_PPS, NAL_SPS, nal_type};

fn ffmpeg_bin() -> String {
    std::env::var("STROSS_FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}

fn ffmpeg_available() -> bool {
    Command::new(ffmpeg_bin())
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 与推流端 `video_encode_args` 同参数的合成流（2 秒 GOP，5 秒 → 2~3 个关键帧）。
fn encode_stream() -> std::io::Result<Vec<u8>> {
    let mut child = Command::new(ffmpeg_bin())
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=30",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-tune",
            "zerolatency",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "60",
            "-keyint_min",
            "60",
            "-sc_threshold",
            "0",
            "-x264-params",
            "repeat_headers=1:slices=1",
            "-b:v",
            "2500k",
            "-maxrate",
            "2500k",
            "-bufsize",
            "5000k",
            "-f",
            "h264",
            "-t",
            "5",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out)?;
    let _ = child.wait()?;
    Ok(out)
}

#[test]
fn every_keyframe_carries_sps_in_real_stream() {
    if !ffmpeg_available() {
        eprintln!("跳过：未找到 ffmpeg");
        return;
    }
    let raw = encode_stream().expect("ffmpeg 编码失败");
    let mut splitter = AnnexBSplitter::new();
    let mut au = AccessUnitBuilder::new();
    let mut kf_total = 0usize;
    let mut kf_no_sps = 0usize;
    let mut checked = 0usize;
    for chunk in raw.chunks(65536) {
        for nal in splitter.feed(chunk) {
            if let Some(unit) = au.push(nal).filter(|u| u.keyframe) {
                kf_total += 1;
                let types: Vec<u8> = unit.nals.iter().filter_map(|n| nal_type(n)).collect();
                if !(types.contains(&NAL_SPS) && types.contains(&NAL_PPS)) {
                    kf_no_sps += 1;
                    if checked < 3 {
                        eprintln!("关键帧缺 SPS/PPS，types={types:?}");
                        checked += 1;
                    }
                }
            }
        }
    }
    assert!(
        kf_total >= 2,
        "5 秒 GOP=60 的流应至少 2 个关键帧（实际 {kf_total}）——测试前提不成立"
    );
    assert_eq!(
        kf_no_sps, 0,
        "{kf_no_sps}/{kf_total} 个关键帧缺 SPS/PPS：中途接入的观看端（含级联代理）将无法解码"
    );
}
