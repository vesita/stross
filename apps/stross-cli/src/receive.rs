//! `stross receive`：接收并原生解码播放（D6 桌面 PlaybackSink）。
//!
//! 链路：`watch`（WS / SRT / QUIC，按 `--relay` scheme 选传输，见
//! [`stross_core::watch::connect_watch`]）→ [`SessionDataManager`] 无损通道
//! （1b）→ [`FfmpegPlaybackSink`] 解码（1c）→ 可选 RGBA 帧落盘 / 扬声器输出。
//!
//! ```text
//! stross receive --relay ws://127.0.0.1:18777 --stream demo --out /tmp/out --secs 4
//! stross receive --relay srt://127.0.0.1:18778 --stream demo --out /tmp/out --secs 4
//! # 产物: /tmp/out/frame_%04d.rgba + meta.txt
//! # 验证: ffmpeg -framerate 30 -f image2 -c:v rawvideo -pix_fmt rgba -s 1280x720 \
//! #        -i /tmp/out/frame_%04d.rgba -c:v libx264 /tmp/out/out.mp4
//! ```

use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Args;
use stross_core::SessionPacket;
use stross_core::session_channel::{ChannelKind, SessionDataManager};
use stross_core::watch;
use stross_media::playback::{
    AudioOut, AudioOutSpec, FfmpegPlaybackSink, PlaybackConfig, PlaybackSink, VideoOut,
};
use stross_proto::frame::TRACK_VIDEO;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum AudioOutArg {
    /// 扬声器 / 录音设备（cpal）
    Device,
    /// 解码但丢弃（无声卡环境 / 测试）
    Discard,
}

#[derive(Args, Debug)]
pub struct ReceiveArgs {
    /// 中继地址（ws://host:port）
    #[arg(long)]
    pub relay: String,
    /// 流 id
    #[arg(long)]
    pub stream: String,
    /// 输出目录（RGBA 帧落盘 + meta.txt）
    #[arg(long, default_value = "/tmp/stross-recv")]
    pub out: String,
    /// 接收时长（秒）
    #[arg(long, default_value_t = 4)]
    pub secs: u64,
    /// 音频输出方式
    #[arg(long, value_enum, default_value_t = AudioOutArg::Discard)]
    pub audio_out: AudioOutArg,
    /// 逐帧延迟统计：记录每帧「到达时刻 − pts」，结束输出分位数摘要 + latency.csv
    /// （相对首帧的附加延迟：含传输缓冲/重传等待，跨传输 A/B 用）
    #[arg(long)]
    pub latency: bool,
    /// 绝对端到端延迟校准：读推流端 `--report-start` 写的 JSON
    /// （`{"sessionStartUnixMs": N}`），对每视频帧计算
    /// `到达时刻 − (会话起点 + pts)`。仅双端同机同钟（本地双 PC）时绝对值
    /// 可信；跨设备需 NTP。报告 min/avg/p95/p99/max。
    #[arg(long)]
    pub calibrate: Option<String>,
    /// 不落盘 RGBA 帧（只计数；长跑/延迟测试用——落盘 ~100MB/s 的 IO
    /// 会耗尽 tmpfs 并干扰延迟测量）
    #[arg(long)]
    pub no_write: bool,
}

pub async fn run(args: ReceiveArgs) -> anyhow::Result<()> {
    std::fs::create_dir_all(&args.out)?;
    tracing::info!("连接中继 {}（流 {}）", args.relay, args.stream);
    let data = watch::connect_watch(&args.relay, &args.stream)
        .await
        .map_err(anyhow::Error::msg)
        .context("连接中继失败")?;

    // 播放会话：视频 → RGBA 通道；音频 → 设备或丢弃
    let sink = FfmpegPlaybackSink;
    let session = sink.open(PlaybackConfig {
        video: Some(VideoOut { display: None }),
        audio: Some(AudioOutSpec {
            channels: 2,
            sample_rate: 48_000,
            out: match args.audio_out {
                AudioOutArg::Device => AudioOut::Device,
                AudioOutArg::Discard => AudioOut::Discard,
            },
        }),
    })?;
    let mut frames_rx = session.take_video_frames().expect("已配置视频轨");

    // 解码帧消费任务：默认落盘 RGBA；`--no-write` 只计数（长跑/延迟测试，
    // 避免 ~100MB/s 写盘耗尽 tmpfs 并干扰延迟测量）。无论哪种都持续消费，
    // 防止播放通道反压丢帧。
    let out_dir = args.out.clone();
    let no_write = args.no_write;
    let writer = tokio::spawn(async move {
        let mut n = 0u32;
        let mut size = (0u32, 0u32);
        while let Some(f) = frames_rx.recv().await {
            if !no_write {
                let name = format!("{out_dir}/frame_{n:04}.rgba");
                if let Err(e) = std::fs::write(&name, &f.rgba) {
                    tracing::error!("写帧失败 {name}: {e}");
                    break;
                }
            }
            size = (f.width, f.height);
            n += 1;
        }
        (n, size)
    });

    // 无损通道（全序不丢）：收帧 → 通道缓存 → 即时产出 → 播放（消息驱动）
    let mut mgr = SessionDataManager::default();
    let start = Instant::now();
    let mut received = 0u64;
    // 延迟统计样本：(pts_ms, 到达时刻相对接收起点 ms)
    // relay 观看端接入时会先补发最近关键帧（历史帧 pts 小、到达最早，
    // 与真实转播帧同批到达），该帧作为离群参考点会污染直方图尾部；
    // 因此丢弃首条视频样本，其余按 pts 单调过滤（无损传输本应有序）。
    let mut latency_samples: Vec<(u32, f64)> = Vec::new();
    let mut max_pts: Option<u32> = None;
    let mut video_seen = 0u32;
    // 绝对端到端延迟：会话起点墙上毫秒（`--calibrate` 读推流端报告）
    let session_start_ms: Option<u64> = match &args.calibrate {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("读校准文件失败 {path}: {e}"))?;
            let v: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("校准文件 JSON 非法: {e}"))?;
            v["sessionStartUnixMs"].as_u64().or_else(|| {
                v.get("sessionStartUnixMs")
                    .and_then(|x| x.as_i64())
                    .map(|x| x as u64)
            })
        }
        None => None,
    };
    // pts0 修正：首帧 pts（校准起点 = 首帧解析时刻，对应 pts0 而非 0）
    let first_pts: f64 = match &args.calibrate {
        Some(path) => {
            let raw = std::fs::read_to_string(path).unwrap_or_default();
            serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|v| v["firstPtsMs"].as_u64())
                .unwrap_or(0) as f64
        }
        None => 0.0,
    };
    let mut abs_latency: Vec<f64> = Vec::new();
    loop {
        let remaining = Duration::from_secs(args.secs).saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, data.recv()).await {
            Ok(Ok(Some(pkt))) => match pkt {
                SessionPacket::Media(frame) => {
                    received += 1;
                    // 延迟统计只取视频轨（音频帧更密，混入会污染 pts 节奏）；
                    // 首条视频样本（relay 补发的历史关键帧）与 pts 回退帧不进样本
                    if frame.header.track == TRACK_VIDEO {
                        let pts = frame.header.pts_ms;
                        video_seen += 1;
                        // 首条视频样本（relay 补发的历史关键帧）与 pts 回退帧
                        // 不进样本（见 --latency 说明）
                        if video_seen > 1 && max_pts.map(|m| pts >= m).unwrap_or(true) {
                            max_pts = Some(pts);
                            let arrival_ms = start.elapsed().as_secs_f64() * 1000.0;
                            if args.latency {
                                latency_samples.push((pts, arrival_ms));
                            }
                            // 绝对端到端延迟（校准模式）：到达墙上时刻 − (会话起点 + pts)
                            if let Some(s0) = session_start_ms {
                                let now_ms = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs_f64() * 1000.0)
                                    .unwrap_or(0.0);
                                abs_latency.push(now_ms - (s0 as f64 + pts as f64 - first_pts));
                            }
                        }
                    }
                    // 单次借用通道：push + poll 共用一个 &mut（热路径）
                    let channel = mgr.channel(&args.stream, ChannelKind::Lossless);
                    channel.push(frame, Instant::now());
                    for f in channel.poll(Instant::now()) {
                        let _ = session.push(f);
                    }
                }
                SessionPacket::Control(_) => {}
            },
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                tracing::warn!("观看连接异常: {e}");
                break;
            }
            Err(_) => break, // 接收时长到
        }
    }
    // 收尾：冲净通道 + 停止播放（后台线程收尾后画面通道关闭）
    let channel = mgr.channel(&args.stream, ChannelKind::Lossless);
    for f in channel.poll(Instant::now()) {
        let _ = session.push(f);
    }
    session.stop();

    let (frames_out, size) = match tokio::time::timeout(Duration::from_secs(3), writer).await {
        Ok(Ok(v)) => v,
        _ => (0, (0, 0)),
    };

    let s = session.stats();
    tracing::info!(
        "接收 {received} 帧 | 解码视频 {frames_out} 帧 ({}x{}) | 音频块 {}/{} | 缓冲丢弃 {}",
        size.0,
        size.1,
        s.audio_blocks_out,
        s.audio_blocks_in,
        s.dropped_push
    );
    std::fs::write(
        format!("{}/meta.txt", args.out),
        format!(
            "stream={}\nwidth={}\nheight={}\nframes={frames_out}\nreceived={received}\n",
            args.stream, size.0, size.1
        ),
    )?;

    // 延迟统计：跨端无法对齐时钟（pts 是发送端相对起点），因此以
    // (arrival − pts) 的**中位数**为基线，报告各帧相对基线的偏移分布：
    //   * p50 ≈ 0：传输层稳态无附加延迟
    //   * p99−p50 / max−p50：抖动与尾延迟（缓冲/重传等待的真实信号）
    // 样本已滤除 pts 回退的补发关键帧，中位数基线进一步消除离群影响。
    if args.latency && !latency_samples.is_empty() {
        let mut csv = String::from("pts_ms,arrival_ms\n");
        for (pts, arr) in &latency_samples {
            csv.push_str(&format!("{pts},{arr:.1}\n"));
        }
        std::fs::write(format!("{}/latency.csv", args.out), csv)?;
        let mut ds: Vec<f64> = latency_samples
            .iter()
            .map(|(pts, arr)| arr - *pts as f64)
            .collect();
        ds.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = ds[ds.len() / 2];
        let pct = |p: f64| -> f64 {
            let i = ((ds.len() as f64) * p).floor() as usize;
            ds[i.min(ds.len() - 1)]
        };
        let off = |p: f64| pct(p) - median;
        tracing::info!(
            "延迟统计（相对中位数偏移 ms，n={}）: p50={:.1} p90={:.1} p95={:.1} p99={:.1} max={:.1} min={:.1}",
            ds.len(),
            off(0.50),
            off(0.90),
            off(0.95),
            off(0.99),
            ds.last().copied().unwrap_or(0.0) - median,
            ds.first().copied().unwrap_or(0.0) - median,
        );
        tracing::info!("延迟样本已写 {}/latency.csv", args.out);
    }

    // 绝对端到端延迟摘要（校准模式）：取 min 作为稳态端到端延迟（其余帧
    // 含排队/抖动）；p95/p99 为体验尾延迟。含 ffmpeg 预热正偏差（口径=上界）。
    if !abs_latency.is_empty() {
        let mut ds = abs_latency.clone();
        ds.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| -> f64 {
            let i = ((ds.len() as f64) * p).floor() as usize;
            ds[i.min(ds.len() - 1)]
        };
        tracing::info!(
            "绝对端到端延迟 ms（校准，n={}）: min={:.1} p50={:.1} p90={:.1} p95={:.1} p99={:.1} max={:.1}",
            ds.len(),
            ds.first().copied().unwrap_or(0.0),
            pct(0.50),
            pct(0.90),
            pct(0.95),
            pct(0.99),
            ds.last().copied().unwrap_or(0.0),
        );
    }
    Ok(())
}
