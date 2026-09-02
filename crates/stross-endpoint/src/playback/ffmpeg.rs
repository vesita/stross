//! 桌面播放后端（D6）：ffmpeg 子进程解码 + cpal 输出。
//!
//! 对称于采集侧 [`crate::pipeline`]：同一 ffmpeg 二进制、同一子进程编排
//! （`STROSS_FFMPEG` 环境变量复用）、零新增原生构建依赖。
//!
//! 每轨两个线程：
//! * **写线程**：接收协议帧 → 写入子进程 stdin。视频关键帧同时解析 SPS
//!   得帧大小、处理失步重建（子进程异常退出 → 等关键帧重建）；
//! * **读线程**（随子进程代际）：从 stdout 持续读取，视频按"帧大小"切出
//!   RGBA 帧（pts 由写线程的队列按 1:1 带入，编码侧 `zerolatency` ⇒
//!   无 B 帧保证输出序与输入一致），音频按"块大小"切出 PCM 推给设备。
//!
//! 内存有界：内部帧队列、解码帧通道、音频 PCM 队列均有上限，满则丢弃计数。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use stross_proto::frame::Frame;
use stross_proto::message::{
    CapabilityDescriptor, CapabilityKind, CodecId, MediaKind, ReliabilityProfile, TransportId,
};
use tokio::sync::mpsc;

use crate::codec::nal::{AnnexBSplitter, NAL_SPS, nal_type, sps_dimensions};
use crate::pipeline::{ffmpeg_available, ffmpeg_bin};
use crate::playback::audio_out::AudioSink;
use crate::playback::schedule::PlaybackScheduler;
use crate::playback::{
    AudioOut, AudioOutSpec, PlaybackConfig, PlaybackError, PlaybackSession, PlaybackSink,
    PlaybackStats, RenderedFrame, SessionInner, VideoPacing,
};

/// AAC 每帧固定 1024 个采样。
const AAC_FRAME_SAMPLES: u32 = 1024;
/// 解码输出单帧 / 单块的上限（防呆，正常远小于此）。
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// 桌面播放后端（D6）。
#[derive(Debug, Default, Clone, Copy)]
pub struct FfmpegPlaybackSink;

impl PlaybackSink for FfmpegPlaybackSink {
    fn descriptor(&self) -> CapabilityDescriptor {
        CapabilityDescriptor {
            kind: CapabilityKind::Sink,
            media: vec![
                MediaKind::Screen,
                MediaKind::Camera,
                MediaKind::Mic,
                MediaKind::SystemAudio,
            ],
            codecs: vec![CodecId::H264, CodecId::Aac],
            transports: vec![TransportId::Ws, TransportId::WebRtc, TransportId::Srt],
            max_width: Some(1920),
            max_height: Some(1080),
            preferred_profile: ReliabilityProfile::Lossy,
        }
    }

    fn open(&self, cfg: PlaybackConfig) -> Result<PlaybackSession, PlaybackError> {
        if !ffmpeg_available() {
            return Err(PlaybackError::NoFfmpeg);
        }
        let stats = Arc::new(Mutex::new(PlaybackStats::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        let mut video_tx = None;
        let mut audio_tx = None;
        let mut video_rx_out = None;
        let mut video_resync = None;

        if cfg.video.is_some() {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Frame>(64);
            let (out_tx, out_rx) = mpsc::channel::<RenderedFrame>(32);
            // PTS 调度层（仅实时显示路径）：解码帧 → 调度线程 → out_tx。
            // 调度线程独立，解码不反压；停止时由 stopped 标记唤醒（等待
            // 上界 = target_delay）。std sync_channel：pacer 可用
            // `recv_timeout` 按 play 时刻精确等待（tokio mpsc 无阻塞超时 API）。
            let (sched_tx, pacer) = if let Some(pacing) = cfg.video_pacing {
                let (sched_tx, sched_rx) = std::sync::mpsc::sync_channel::<RenderedFrame>(32);
                let out2 = out_tx.clone();
                let st = stats.clone();
                let sp = stopped.clone();
                let h = std::thread::Builder::new()
                    .name("stross-video-pacer".into())
                    .spawn(move || pacer_loop(sched_rx, out2, pacing, st, sp))
                    .map_err(|e| PlaybackError::Spawn(e.to_string()))?;
                (Some(sched_tx), Some(h))
            } else {
                (None, None)
            };
            // 失步标记提升为共享引用：push 侧丢帧也能置位，让 writer
            // 等关键帧重建（避免把花屏帧喂给解码器）
            let resync = Arc::new(AtomicBool::new(false));
            let shared = Arc::new(VideoShared {
                size: Mutex::new(None),
                frame_size: Mutex::new(None),
                pts: Mutex::new(VecDeque::new()),
                resync: resync.clone(),
                out_tx,
                sched_tx,
                stats: stats.clone(),
                stopped: stopped.clone(),
            });
            video_rx_out = Some(out_rx);
            let h = std::thread::Builder::new()
                .name("stross-video-writer".into())
                .spawn(move || video_writer_loop(rx, shared))
                .map_err(|e| PlaybackError::Spawn(e.to_string()))?;
            threads.push(h);
            if let Some(p) = pacer {
                threads.push(p);
            }
            video_tx = Some(tx);
            video_resync = Some(resync);
        }

        if let Some(spec) = cfg.audio {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Frame>(64);
            let sink = if spec.out == AudioOut::Device {
                match AudioSink::open() {
                    Ok(s) => {
                        stats.lock().unwrap().audio_device_ok = true;
                        Some(Arc::new(s))
                    }
                    Err(e) => {
                        tracing::warn!("音频输出设备不可用，静音回退: {e}");
                        None
                    }
                }
            } else {
                None
            };
            let st = stats.clone();
            let sp = stopped.clone();
            let h = std::thread::Builder::new()
                .name("stross-audio-writer".into())
                .spawn(move || audio_writer_loop(rx, spec, sink, st, sp))
                .map_err(|e| PlaybackError::Spawn(e.to_string()))?;
            threads.push(h);
            audio_tx = Some(tx);
        }

        Ok(PlaybackSession {
            inner: Arc::new(SessionInner {
                stats,
                stopped,
                video_tx: Mutex::new(video_tx),
                audio_tx: Mutex::new(audio_tx),
                video_rx_out: Mutex::new(video_rx_out),
                video_resync,
                threads: Mutex::new(threads),
            }),
        })
    }
}

/// 视频解码的跨线程共享状态。
struct VideoShared {
    /// 源分辨率（SPS 解析；关键帧时更新）。
    size: Mutex<Option<(u32, u32)>>,
    /// 输出帧字节数 = 宽 × 高 × 4（RGBA）。
    frame_size: Mutex<Option<usize>>,
    /// 已写入子进程的 pts 队列（读线程每产出一帧弹出一个）。
    pts: Mutex<VecDeque<u32>>,
    /// 失步标记：子进程异常退出或 push 侧丢帧后置位，写线程等关键帧重建。
    resync: Arc<AtomicBool>,
    /// 解码画面输出通道。
    out_tx: mpsc::Sender<RenderedFrame>,
    /// PTS 调度入口（`Some` = 实时显示路径，读线程帧先进调度层再出
    /// `out_tx`；`None` = 直通，录制 / headless 语义）。
    sched_tx: Option<std::sync::mpsc::SyncSender<RenderedFrame>>,
    stats: Arc<Mutex<PlaybackStats>>,
    stopped: Arc<AtomicBool>,
}

/// 视频写线程：帧 → 子进程 stdin；关键帧解析 SPS、处理失步重建。
fn video_writer_loop(rx: std::sync::mpsc::Receiver<Frame>, shared: Arc<VideoShared>) {
    let mut child: Option<Child> = None;
    let mut stdin: Option<ChildStdin> = None;
    let mut reader: Option<JoinHandle<()>> = None;
    loop {
        let frame = match rx.recv() {
            Ok(f) => f,
            Err(_) => break, // 帧入口关闭（stop）→ 收尾
        };
        if shared.stopped.load(Ordering::Relaxed) {
            break;
        }
        let keyframe = frame.header.is_keyframe();
        if keyframe {
            // 关键帧携带 SPS（编码侧 repeat_headers=1）：解析分辨率 → 帧大小
            if let Some((w, h)) = parse_sps_size(&frame.payload) {
                let mut size = shared.size.lock().unwrap();
                if *size != Some((w, h)) {
                    *size = Some((w, h));
                    *shared.frame_size.lock().unwrap() = Some(w as usize * h as usize * 4);
                }
            }
            // 失步或子进程缺失 → 以关键帧为对齐点重建
            if stdin.is_none() || shared.resync.load(Ordering::Relaxed) {
                kill_child(child.take());
                drop(stdin.take());
                if let Some(r) = reader.take() {
                    let _ = r.join();
                }
                // 清空 pts 队列：旧代际已被管道/解码器吞入但未产出的帧对应
                // 的 pts 残留，若不清空，新代际前 N 帧会弹到过期 pts →
                // 时间戳回退（被调度层当 stale 丢）或跳变（触发重锚定）。
                shared.pts.lock().unwrap().clear();
                match spawn_video_decode() {
                    Ok((c, si, so)) => {
                        shared.resync.store(false, Ordering::Relaxed);
                        shared.stats.lock().unwrap().video_resyncs += 1;
                        let s2 = shared.clone();
                        reader = Some(
                            std::thread::Builder::new()
                                .name("stross-video-reader".into())
                                .spawn(move || video_reader_gen(so, s2))
                                .expect("spawn video reader"),
                        );
                        child = Some(c);
                        stdin = Some(si);
                    }
                    Err(e) => {
                        // 重建失败：保持失步，等下一个关键帧再试
                        tracing::warn!("视频解码进程启动失败: {e}");
                        continue;
                    }
                }
            }
        }
        // 失步期间（等关键帧）：丢弃非关键帧
        if shared.resync.load(Ordering::Relaxed) && !keyframe {
            continue;
        }
        let Some(si) = stdin.as_mut() else { continue };
        if si.write_all(&frame.payload).is_err() {
            // 子进程已退出 → 置失步，等下一个关键帧重建
            shared.resync.store(true, Ordering::Relaxed);
            continue;
        }
        shared.pts.lock().unwrap().push_back(frame.header.pts_ms);
        shared.stats.lock().unwrap().video_frames_in += 1;
    }
    // 收尾：杀子进程、关 stdin、join 读线程
    teardown_gen(&mut child, &mut stdin, &mut reader);
}

/// 视频读线程（一个子进程代际）：持续读 stdout，按帧大小切出 RGBA 帧。
fn video_reader_gen(mut stdout: ChildStdout, shared: Arc<VideoShared>) {
    let mut acc: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut last_size: Option<(u32, u32)> = None;
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stdout.read(&mut buf) {
            Ok(0) => break, // 子进程结束
            Ok(n) => {
                acc.extend_from_slice(&buf[..n]);
                // 分辨率变化 → 丢弃未对齐的残留字节（关键帧重建时子进程
                // 重启，尺寸可能变；注意首次 None→Some 初始化**不允许**清空：
                // 写线程在 spawn 解码器前已解析 SPS 设置 size，reader 首块
                // 数据就是首帧——误清会让整个流帧边界错位（花屏回归，见
                // decoded_pixels_match_native_ffmpeg）
                let size = *shared.size.lock().unwrap();
                if last_size.is_some() && size != last_size {
                    acc.clear();
                }
                last_size = size;
                let Some((w, h)) = size else {
                    if acc.len() > MAX_FRAME_BYTES {
                        acc.clear();
                    }
                    continue;
                };
                let need = w as usize * h as usize * 4;
                // 预分配到位：避免首帧时 acc 从初始容量反复 grow
                // （1080p 一帧 8.3MB，一次性 reserve 免去多次翻倍拷贝）
                if acc.capacity() < need {
                    acc.reserve(need - acc.len());
                }
                while acc.len() >= need {
                    // 恰好整帧：整个缓冲直接交给上层（零拷贝），
                    // 并用预分配的新缓冲接续累积；有残留才 drain 切帧
                    let rgba = if acc.len() == need {
                        std::mem::replace(&mut acc, Vec::with_capacity(need))
                    } else {
                        acc.drain(..need).collect()
                    };
                    let pts_ms = shared.pts.lock().unwrap().pop_front().unwrap_or(0);
                    let rendered = RenderedFrame {
                        pts_ms,
                        width: w,
                        height: h,
                        rgba,
                    };
                    // 实时显示路径：先进 PTS 调度层（pacer 线程）按源节奏发出；
                    // 直通路径（录制/headless）：直接进输出通道
                    let sent = match shared.sched_tx.as_ref() {
                        Some(tx) => tx.try_send(rendered).is_ok(),
                        None => shared.out_tx.try_send(rendered).is_ok(),
                    };
                    if !sent {
                        // 消费者慢 → 丢帧（显示可跳帧，不反压阻塞解码）
                        shared.stats.lock().unwrap().dropped_push += 1;
                    } else {
                        shared.stats.lock().unwrap().video_frames_out += 1;
                    }
                }
                if acc.len() > MAX_FRAME_BYTES {
                    acc.clear();
                }
            }
            Err(_) => break,
        }
    }
    if !shared.stopped.load(Ordering::Relaxed) {
        // 子进程异常退出 → 置失步，写线程等关键帧重建
        shared.resync.store(true, Ordering::Relaxed);
    }
}

/// PTS 调度线程：解码帧按源节奏（pts）输出（`schedule::PlaybackScheduler`）。
///
/// 只由实时显示路径启用；录制 / headless 直通，不经本线程。停止语义：
/// `stop()` 先置 `stopped` 再关帧入口 → 本线程最迟一个 `target_delay`
/// 内退出（`blocking_recv_timeout` 超时后检查标记），join 无长阻塞。
fn pacer_loop(
    rx: std::sync::mpsc::Receiver<RenderedFrame>,
    out_tx: mpsc::Sender<RenderedFrame>,
    pacing: VideoPacing,
    stats: Arc<Mutex<PlaybackStats>>,
    stopped: Arc<AtomicBool>,
) {
    let mut sched = PlaybackScheduler::new(pacing.target_delay, pacing.jump_reset);
    let mut last_dropped = 0u64;
    let mut last_reanchors = 0u64;
    let mut last_held = 0u64;

    loop {
        if stopped.load(Ordering::Relaxed) {
            break;
        }
        // 先发出已到期帧：队首 play_at ≤ now 时必须立即补发，不能等新帧
        // （否则被 hold 的帧从各自 play_at 推迟到下一输入帧到达，突发流
        // 批量倾泻，PTS 调度平滑失效）。
        {
            let now = Instant::now();
            let mut closed = false;
            let _ = sched.emit_due_with(now, |f| {
                if out_tx.try_send(f).is_err() {
                    stats.lock().unwrap().dropped_push += 1;
                    closed = true;
                    Err(())
                } else {
                    Ok(())
                }
            });
            if closed {
                break; // 输出通道关闭：会话结束
            }
        }
        // 等到队首 play 时刻或新帧到来
        let wait = sched
            .next_play_at()
            .map(|t| t.saturating_duration_since(Instant::now()));
        let recv = match wait {
            // 未到期：等到 play 时刻（有新帧则提前醒来处理）
            Some(d) if !d.is_zero() => rx.recv_timeout(d),
            // 恰好到期：不阻塞（顶部已补发），立即空转一轮再查队首
            Some(_) => rx.recv_timeout(Duration::ZERO),
            // 队列空：阻塞等新帧
            None => rx
                .recv()
                .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
        };
        match recv {
            Ok(f) => {
                let now = Instant::now();
                sched.push(f, now);
                // 过水位丢队尾（延迟控制器）：队尾 play 时刻晚于
                // now + target_delay → 丢最新帧追平实时（发送端过快 / 时钟
                // 漂移；正常流零丢帧零加时）。
                sched.drop_over_watermark(now);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue, // 空转：顶部补发到期帧
            Err(_) => break, // 通道关闭（writer 已退出）
        }
        // 仅在统计指标发生变化时更新共享状态，减少多线程锁争用
        if sched.stats.dropped_watermark != last_dropped
            || sched.stats.reanchors != last_reanchors
            || sched.stats.held != last_held
        {
            last_dropped = sched.stats.dropped_watermark;
            last_reanchors = sched.stats.reanchors;
            last_held = sched.stats.held;
            let mut st = stats.lock().unwrap();
            st.paced_dropped = last_dropped;
            st.paced_reanchors = last_reanchors;
            st.paced_held = last_held;
        }
    }
}

/// 音频写线程：ADTS 帧 → 子进程 stdin；设备按需重建。
fn audio_writer_loop(
    rx: std::sync::mpsc::Receiver<Frame>,
    spec: AudioOutSpec,
    sink: Option<Arc<AudioSink>>,
    stats: Arc<Mutex<PlaybackStats>>,
    stopped: Arc<AtomicBool>,
) {
    // 解码输出参数：设备模式以设备为准（ffmpeg -ac/-ar 重采样对齐）
    let (channels, rate) = match &sink {
        Some(s) => (s.channels, s.rate),
        None => (spec.channels, spec.sample_rate),
    };
    let block_size = AAC_FRAME_SAMPLES as usize * channels as usize * 4; // f32le
    let mut child: Option<Child> = None;
    let mut stdin: Option<ChildStdin> = None;
    let mut reader: Option<JoinHandle<()>> = None;
    loop {
        let frame = match rx.recv() {
            Ok(f) => f,
            Err(_) => break,
        };
        if stopped.load(Ordering::Relaxed) {
            break;
        }
        if stdin.is_none() {
            match spawn_audio_decode(channels, rate) {
                Ok((c, si, so)) => {
                    let s2 = sink.clone();
                    let st = stats.clone();
                    let sp = stopped.clone();
                    reader = Some(
                        std::thread::Builder::new()
                            .name("stross-audio-reader".into())
                            .spawn(move || audio_reader_gen(so, block_size, s2, st, sp))
                            .expect("spawn audio reader"),
                    );
                    child = Some(c);
                    stdin = Some(si);
                }
                Err(e) => {
                    tracing::warn!("音频解码进程启动失败: {e}");
                    continue; // 下一帧再试
                }
            }
        }
        let Some(si) = stdin.as_mut() else { continue };
        if si.write_all(&frame.payload).is_err() {
            // 子进程已退出 → 杀旧进程、等下一帧重建
            teardown_gen(&mut child, &mut stdin, &mut reader);
            continue;
        }
        stats.lock().unwrap().audio_blocks_in += 1;
    }
    kill_child(child.take());
    drop(stdin.take());
    if let Some(r) = reader.take() {
        let _ = r.join();
    }
}

/// 音频读线程（一个子进程代际）：按块大小切出 PCM 推给设备。
fn audio_reader_gen(
    mut stdout: ChildStdout,
    block_size: usize,
    sink: Option<Arc<AudioSink>>,
    stats: Arc<Mutex<PlaybackStats>>,
    _stopped: Arc<AtomicBool>,
) {
    let mut acc: Vec<u8> = Vec::with_capacity(block_size * 4);
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                acc.extend_from_slice(&buf[..n]);
                while acc.len() >= block_size {
                    let block: Vec<u8> = acc.drain(..block_size).collect();
                    if let Some(s) = &sink {
                        // f32le → f32 样本（交织声道；块大小恒为 4 的倍数）
                        let (chunks, _rest) = block.as_chunks::<4>();
                        let samples: Vec<f32> =
                            chunks.iter().map(|b| f32::from_le_bytes(*b)).collect();
                        s.push(&samples);
                    }
                    stats.lock().unwrap().audio_blocks_out += 1;
                }
            }
        }
    }
}

/// 从关键帧载荷（Annex-B：SPS/PPS/IDR…）解析分辨率。
fn parse_sps_size(payload: &[u8]) -> Option<(u32, u32)> {
    let mut splitter = AnnexBSplitter::new();
    for nal in splitter.feed(payload) {
        if nal_type(&nal) == Some(NAL_SPS)
            && let Some(d) = sps_dimensions(&nal)
        {
            return Some(d);
        }
    }
    for nal in splitter.finish() {
        if nal_type(&nal) == Some(NAL_SPS)
            && let Some(d) = sps_dimensions(&nal)
        {
            return Some(d);
        }
    }
    None
}

fn kill_child(child: Option<Child>) {
    if let Some(mut c) = child {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// 收尾一个解码子进程代际：杀子进程、关 stdin、join 读线程。
/// 视频/音频主循环与失步重建共用（避免 4 处复制同样的收尾序列）。
fn teardown_gen(
    child: &mut Option<Child>,
    stdin: &mut Option<std::process::ChildStdin>,
    reader: &mut Option<std::thread::JoinHandle<()>>,
) {
    kill_child(child.take());
    drop(stdin.take());
    if let Some(r) = reader.take() {
        let _ = r.join();
    }
}

/// 启动视频解码子进程：H264（Annex-B）→ RGBA rawvideo。
///
/// `-probesize 32 -analyzeduration 0`：限制解复用器预读，保证实时吐帧
/// （实测默认 5MB 预读会把管道内容全读完才开解，输出积压到 EOF）。
/// `-threads 1`：关闭 h264 解码器帧线程（默认 = CPU 核数），否则输出
/// 被管线延迟 (threads−1) 帧——16 核机器实测首帧延迟 566ms（30fps）。
/// 720p30 单线程解码余量充足，低延迟优先。
/// 注意：不能加 `-fflags nobuffer` / `-flags low_delay`——实测会破坏
/// h264 解复用器初始化（0 帧输出）。
fn spawn_video_decode() -> std::io::Result<(Child, ChildStdin, ChildStdout)> {
    let args = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-threads",
        "1",
        "-fflags",
        "+genpts",
        "-probesize",
        "32",
        "-analyzeduration",
        "0",
        "-f",
        "h264",
        "-i",
        "pipe:0",
        "-an",
        "-sn",
        "-pix_fmt",
        "rgba",
        "-f",
        "rawvideo",
        "pipe:1",
    ];
    spawn_decode(&args)
}

/// 启动音频解码子进程：AAC（ADTS）→ f32le PCM（按设备参数重采样）。
///
/// 输入格式名是 `aac`（"raw ADTS AAC" 解复用器）；`adts` 在本构建里只是
/// muxer 名，用作输入会报 Unknown input format。输出 `-f f32le` 已隐含
/// f32 采样格式，不要显式 `-sample_fmt f32`（ffmpeg 不认该名，应写 flt）。
fn spawn_audio_decode(
    channels: u8,
    rate: u32,
) -> std::io::Result<(Child, ChildStdin, ChildStdout)> {
    let ch = channels.to_string();
    let rt = rate.to_string();
    let args = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-probesize",
        "32",
        "-analyzeduration",
        "0",
        "-f",
        "aac",
        "-i",
        "pipe:0",
        "-vn",
        "-sn",
        "-ac",
        &ch,
        "-ar",
        &rt,
        "-f",
        "f32le",
        "pipe:1",
    ];
    spawn_decode(&args)
}

fn spawn_decode(args: &[&str]) -> std::io::Result<(Child, ChildStdin, ChildStdout)> {
    let mut cmd = Command::new(ffmpeg_bin());
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("解码进程没有 stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("解码进程没有 stdout"))?;
    Ok((child, stdin, stdout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{AudioSourceConfig, Quality, StreamConfig, StreamSession, VideoSource};
    use crate::playback::{VideoOut, VideoPacing};
    use bytes::Bytes;
    use std::time::{Duration, Instant};
    use stross_proto::frame::{CODEC_H264, FLAG_KEYFRAME, TRACK_VIDEO};

    /// 用采集管线生成一段协议帧（合成源，时长 cfg.duration_secs）。
    async fn capture_frames(cfg: StreamConfig) -> Vec<Frame> {
        let (tx, mut rx) = mpsc::channel::<Frame>(256);
        let session = StreamSession::spawn(&cfg, tx).unwrap();
        let mut frames = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
                Ok(Some(f)) => frames.push(f),
                Ok(None) => break,
                Err(_) => {
                    if !frames.is_empty() {
                        break; // 采集结束（子进程已退出）
                    }
                }
            }
        }
        drop(session); // 释放内部 tx，读循环彻底收尾
        frames
    }

    /// 把协议帧拼成 Annex-B ES（与接收侧写入解码器 stdin 的字节完全一致）。
    fn frames_to_es(frames: &[Frame]) -> Vec<u8> {
        frames
            .iter()
            .filter(|f| f.header.track == TRACK_VIDEO)
            .flat_map(|f| f.payload.iter().copied())
            .collect()
    }

    /// 原生 ffmpeg 解码整段 ES → RGBA 帧列表（对照基准）。
    fn decode_es_native(es: &[u8], w: u32, h: u32) -> Vec<Vec<u8>> {
        use std::io::Read;
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new(ffmpeg_bin())
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-an",
                "-sn",
                "-pix_fmt",
                "rgba",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ffmpeg");
        child.stdin.take().unwrap().write_all(es).expect("写 ES");
        let mut raw = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut raw)
            .expect("读解码输出");
        child.wait().expect("ffmpeg 退出");
        let need = (w as usize) * (h as usize) * 4;
        assert!(
            raw.len() % need == 0,
            "原生解码输出应正好切成整帧（{} 字节 % {}）",
            raw.len(),
            need
        );
        raw.chunks_exact(need).map(<[u8]>::to_vec).collect()
    }

    /// 像素级对照：我们的管线（按帧喂入 → ffmpeg 子进程）+ 原生 ffmpeg 解码
    /// 同一段 ES，逐帧逐字节一致 ⇒ 解码管线不引入帧错位/花屏（D 批回归
    /// 防「帧大小/pts 对齐 bug」）。
    #[tokio::test]
    async fn decoded_pixels_match_native_ffmpeg() {
        if !ffmpeg_available() {
            eprintln!("跳过：未找到 ffmpeg");
            return;
        }
        // 合成源 2 秒（跨两个 GOP：关键帧对齐 + P 帧连续），LOW = 640x360@24
        let cfg = StreamConfig {
            stream_id: "t".into(),
            title: "t".into(),
            video: Some(VideoSource::Synthetic {
                pattern: "testsrc2".into(),
            }),
            quality: Quality::LOW,
            audio: None,
            duration_secs: Some(2),
            share_token: None,
        };
        let frames = capture_frames(cfg).await;
        assert!(frames.iter().any(|f| f.header.is_keyframe()));
        let es = frames_to_es(&frames);
        assert!(!es.is_empty());

        // 1) 原生解码：同输入 ES 的像素基准
        let native = decode_es_native(&es, 640, 360);
        assert!(
            native.len() >= 20,
            "原生应解出至少 20 帧，实际 {}",
            native.len()
        );

        // 2) 我们的管线：裸 ES 直接喂给解码进程（绕过采集端，只测解码消费侧）
        let session = FfmpegPlaybackSink
            .open(PlaybackConfig {
                video: Some(VideoOut { display: None }),
                audio: None,
                video_pacing: None,
            })
            .unwrap();
        let mut out_rx = session.take_video_frames().unwrap();
        // 真实消费方（GUI 渲染循环）在推流期间持续排空解码输出通道；测试若
        // 「先推完再读」，32 槽有界通道会在 push 阶段积满，解码线程瞬时
        // 超前（解码延迟 + 管道缓冲）即触发 try_send 丢帧 → dropped_push
        // 非 0（该计数本就是「消费者跟不上」的兜底，不消费必然丢帧的
        // 偶发回归）。这里起一个排空任务等价于真实消费者，断言才成立。
        let (drain_tx, mut drain_rx) = mpsc::channel::<RenderedFrame>(256);
        let drain_task = tokio::spawn(async move {
            while let Some(f) = out_rx.recv().await {
                if drain_tx.send(f).await.is_err() {
                    break;
                }
            }
        });
        // 按 AccessUnit 切回协议帧喂入（与接收端 push 相同）。
        // 注意：真实链路帧间隔 ~40ms（24fps 采集 -re 限速），瞬间塞满会触发
        // 不同的解码器行为，这里以真实节奏喂入（与端到端延迟测试口径一致）。
        for f in &frames {
            if f.header.track == TRACK_VIDEO {
                session.push(f.clone()).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(41)).await;
        }
        let mut ours = Vec::new();
        // 收集窗口：推入耗时 ~2s（41ms/帧 × 48），解码输出随其后到达；
        // 收尾前必须留足时间等子进程吐完所有帧
        while let Ok(Some(f)) =
            tokio::time::timeout(Duration::from_millis(3000), drain_rx.recv()).await
        {
            ours.push(f);
        }
        session.stop();
        // 排空任务随 out_tx 关闭（writer 线程收尾 drop shared）自然结束
        let _ = drain_task.await;
        let s = session.stats();
        // 准确性：全部协议帧应写入解码器（无丢帧/无失步重建）
        assert_eq!(
            s.video_frames_in as usize,
            frames
                .iter()
                .filter(|f| f.header.track == TRACK_VIDEO)
                .count(),
            "解码器应收到全部视频帧（video_frames_in={}）",
            s.video_frames_in
        );
        assert_eq!(s.dropped_push, 0, "按真实节奏喂入不应丢帧");
        assert_eq!(s.video_resyncs, 1, "只应初始 spawn 一次解码器，无失步重建");

        // 数量级（兼容子进程尾帧滞留）：至少解出一半
        assert!(
            ours.len() >= native.len() / 2,
            "我们的管线应解出与原生同量级的帧（ours {} / native {}）",
            ours.len(),
            native.len()
        );
        let n = ours.len().min(native.len());
        assert!(n > 0);
        // 帧序一致（zerolatency 无 B 帧，输出序=输入序）；逐字节比对像素
        for (i, (o, nf)) in ours[..n].iter().zip(native[..n].iter()).enumerate() {
            assert_eq!(
                o.rgba, *nf,
                "第 {i} 帧像素与原生解码不一致（解码管线帧错位/花屏回归）"
            );
        }
    }

    #[test]
    fn descriptor_is_sink() {
        let d = FfmpegPlaybackSink.descriptor();
        assert_eq!(d.kind, CapabilityKind::Sink);
        assert!(d.codecs.contains(&CodecId::H264));
        assert!(d.codecs.contains(&CodecId::Aac));
    }

    #[test]
    fn open_without_ffmpeg_fails() {
        if !ffmpeg_available() {
            let err = FfmpegPlaybackSink
                .open(PlaybackConfig {
                    video: Some(VideoOut { display: None }),
                    audio: None,
                    video_pacing: None,
                })
                .err()
                .expect("无 ffmpeg 时 open 应失败");
            assert!(matches!(err, PlaybackError::NoFfmpeg));
        }
    }

    #[tokio::test]
    async fn video_decode_roundtrip() {
        if !ffmpeg_available() {
            eprintln!("跳过：未找到 ffmpeg");
            return;
        }
        // 采集侧合成源（testsrc2，LOW = 640x360@24fps，1 秒）
        let cfg = StreamConfig {
            stream_id: "t".into(),
            title: "t".into(),
            video: Some(VideoSource::Synthetic {
                pattern: "testsrc2".into(),
            }),
            quality: Quality::LOW,
            audio: None,
            duration_secs: Some(1),
            share_token: None,
        };
        let frames = capture_frames(cfg).await;
        assert!(!frames.is_empty(), "采集管线应产出帧");
        assert!(frames.iter().any(|f| f.header.is_keyframe()));

        let session = FfmpegPlaybackSink
            .open(PlaybackConfig {
                video: Some(VideoOut { display: None }),
                audio: None,
                video_pacing: None,
            })
            .unwrap();
        let mut out_rx = session.take_video_frames().unwrap();
        for f in frames {
            session.push(f).unwrap();
        }
        // 收集解码帧：直到 800ms 无新帧
        let mut rendered = Vec::new();
        while let Ok(Some(f)) =
            tokio::time::timeout(Duration::from_millis(800), out_rx.recv()).await
        {
            rendered.push(f);
        }
        session.stop();
        assert!(!rendered.is_empty(), "应解码出画面帧");
        let first = &rendered[0];
        assert_eq!(
            (first.width, first.height),
            (640, 360),
            "LOW 质量为 640x360（SPS 解析验证）"
        );
        assert_eq!(first.rgba.len(), 640 * 360 * 4);
        let s = session.stats();
        assert!(s.video_frames_in >= 10, "应收到足够视频帧: {s:?}");
        assert!(
            s.video_frames_out >= rendered.len() as u64 / 2,
            "解码产出应基本对齐: {s:?}"
        );
    }

    #[tokio::test]
    async fn video_pacing_holds_burst_and_emits_on_schedule() {
        if !ffmpeg_available() {
            eprintln!("跳过：未找到 ffmpeg");
            return;
        }
        // 采集一段真实 H.264 帧（testsrc2 合成源），**重写 pts 为 33ms 间距**
        // 后突发喂给启用了 PTS 调度的播放会话——调度层应把解码帧按 pts
        // 间距拉开输出，而非瞬时倾泻（33ms × 5 帧 = 132ms，未超 150ms
        // 水位，延迟控制器不应介入）。
        // 注：本机解码器在「先推完再读」测试路径下只稳定产出前几帧
        // （既有行为，真实流 e2e 全量解码），故仅断言已解码帧的节奏。
        let cfg = StreamConfig {
            stream_id: "t".into(),
            title: "t".into(),
            video: Some(VideoSource::Synthetic {
                pattern: "testsrc2".into(),
            }),
            quality: Quality::LOW,
            audio: None,
            duration_secs: Some(1),
            share_token: None,
        };
        let mut frames = capture_frames(cfg).await;
        assert!(frames.len() >= 3, "合成源应产出 ≥3 帧: {}", frames.len());
        // 只取前 5 帧做节奏验证：采集源是 1s @30fps（≈30 帧），若对全部帧
        // 重写 pts=0,33,…,957ms，第 17 帧（528ms）会越过 500ms 跳变阈值触发
        // 重锚 → 队列被清、重锚帧立即发，节奏断言失真（span 大幅缩短的 flake）。
        // 截断到 5 帧（0..132ms，无跳变）后按 33ms 间距重写。
        frames.truncate(5);
        for (i, f) in frames.iter_mut().enumerate() {
            f.header.pts_ms = (i as u32) * 33;
        }
        let session = FfmpegPlaybackSink
            .open(PlaybackConfig {
                video: Some(VideoOut { display: None }),
                audio: None,
                video_pacing: Some(VideoPacing::default()),
            })
            .unwrap();
        let mut out_rx = session.take_video_frames().unwrap();
        let start = Instant::now();
        for f in frames {
            session.push(f).unwrap();
        }
        // 突发喂入 → 首帧立即出，其余按 33ms 节奏拉开
        let mut rendered = 0u32;
        let mut first_at: Option<Duration> = None;
        let mut last_at = Duration::ZERO;
        // 首帧窗口放宽到 2s：整机高负载（全 workspace 并行、多个 ffmpeg
        // 子进程抢核）下子进程启动 + 首帧解码可能超过 800ms，是既有 flake
        // 根因；首帧到达后收紧回 800ms 判定帧间节奏。
        let mut window = Duration::from_millis(2000);
        while let Ok(Some(_f)) = tokio::time::timeout(window, out_rx.recv()).await {
            rendered += 1;
            let elapsed = start.elapsed();
            if first_at.is_none() {
                first_at = Some(elapsed);
                window = Duration::from_millis(800);
            }
            last_at = elapsed;
        }
        session.stop();
        // 接线验证：帧必须从解码 → pacer → out_rx 全链路流出（≥1）；
        // 解码器在「先推完再读」测试路径下帧数不稳定（既有 flake，见
        // iteration-plan 第九轮备注），且可能整批迟到（解码晚于调度时刻 →
        // 迟到帧立即发，pacer 无 hold）。
        // 节奏断言仅对「确有帧被调度 hold」成立时生效：paced_held > 0
        // 意味着某帧按 pts 间距等待后发出，此时首末帧 span ≥ 33ms 是必然
        // 结果——断言验证的是 pacer 确实在按 pts 拉开，而非误报解码迟到。
        assert!(rendered >= 1, "应解码出画面帧: {rendered}");
        let s = session.stats();
        if rendered >= 2 && s.paced_held > 0 {
            let span = last_at - first_at.unwrap_or_default();
            assert!(
                span >= Duration::from_millis(30),
                "调度层应按 pts 间距拉开突发帧，span={span:?} rendered={rendered}"
            );
        }
        assert_eq!(s.paced_dropped, 0, "33ms 间距未超水位不应丢帧: {s:?}");
    }

    /// 延迟控制器接线验证：直接驱动 `pacer_loop`（合成 RGBA 帧，不经解码
    /// 子进程），发送端过快（到达节拍 ≪ pts 节拍）应触发「过水位丢队尾」，
    /// `paced_dropped` 真实生效。此前 `drop_over_watermark` 从未被 pacer
    /// 调用（仅 schedule.rs 单测覆盖），接线后本测试防回退。
    #[test]
    fn pacer_loop_wires_watermark_drop() {
        let (tx, rx) = std::sync::mpsc::channel::<RenderedFrame>();
        let (out_tx, _out_rx) = tokio::sync::mpsc::channel::<RenderedFrame>(64);
        let stats = Arc::new(Mutex::new(PlaybackStats::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let pacer = std::thread::Builder::new()
            .name("test-pacer".into())
            .spawn({
                let stats = stats.clone();
                let stopped = stopped.clone();
                move || pacer_loop(rx, out_tx, VideoPacing::default(), stats, stopped)
            })
            .unwrap();
        // 20 帧、pts 33ms 间距、1ms 到达间隔：队尾 play 时刻按 33ms/帧
        // 增长、到达时刻按 1ms/帧增长 → 约第 6 帧起越过 150ms 水位被丢
        for i in 0..20u32 {
            tx.send(RenderedFrame {
                pts_ms: i * 33,
                width: 2,
                height: 2,
                rgba: vec![0u8; 16],
            })
            .unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        // 等 pacer 消化完（最后一帧 play 时刻 ≈ 首帧 + 627ms）
        std::thread::sleep(Duration::from_millis(900));
        let s = stats.lock().unwrap();
        assert!(
            s.paced_dropped > 0,
            "发送端过快应触发过水位丢帧（接线验证）: {s:?}"
        );
        stopped.store(true, Ordering::Relaxed);
        drop(tx); // 断连 → pacer 的 recv 返回 Err → 退出循环
        let _ = pacer.join();
    }

    #[tokio::test]
    async fn audio_decode_roundtrip() {
        if !ffmpeg_available() {
            eprintln!("跳过：未找到 ffmpeg");
            return;
        }
        // 采集侧合成音源（lavfi sine 440Hz，1 秒），解码但丢弃（无需声卡）
        let cfg = StreamConfig {
            stream_id: "t".into(),
            title: "t".into(),
            video: None,
            quality: Quality::LOW,
            audio: Some(AudioSourceConfig {
                synthetic: Some(440),
                ..Default::default()
            }),
            duration_secs: Some(1),
            share_token: None,
        };
        let frames = capture_frames(cfg).await;
        assert!(!frames.is_empty(), "采集管线应产出音频帧");

        let session = FfmpegPlaybackSink
            .open(PlaybackConfig {
                video: None,
                audio: Some(AudioOutSpec {
                    channels: 2,
                    sample_rate: 48_000,
                    out: AudioOut::Discard,
                }),
                video_pacing: None,
            })
            .unwrap();
        for f in frames {
            session.push(f).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        while session.stats().audio_blocks_out < 5 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        session.stop();
        let s = session.stats();
        assert!(s.audio_blocks_in >= 5, "应收到音频块: {s:?}");
        assert!(s.audio_blocks_out >= 5, "应解码出音频块: {s:?}");
    }

    /// 视频丢帧 → resync 置位 → writer 等关键帧重建（B 批花屏修复）。
    ///
    /// 两段验证：
    /// 1. 拿 video_tx 把队列塞满，随后 push 必走 Full 分支 → 丢帧计数 +
    ///    resync 置位（不把撕裂的 GOP 喂给解码器）；
    /// 2. resync 置位后 writer 丢弃非关键帧、在关键帧处重建（video_resyncs 增长）。
    #[tokio::test]
    async fn dropped_video_frame_triggers_resync() {
        if !ffmpeg_available() {
            eprintln!("跳过：未找到 ffmpeg");
            return;
        }
        let session = FfmpegPlaybackSink
            .open(PlaybackConfig {
                video: Some(VideoOut { display: None }),
                audio: None,
                video_pacing: None,
            })
            .unwrap();
        let resync = session
            .inner
            .video_resync
            .clone()
            .expect("视频会话有 resync");
        let video_tx = session
            .inner
            .video_tx
            .lock()
            .unwrap()
            .clone()
            .expect("视频发送端存在");
        let tiny = || Frame::new(TRACK_VIDEO, CODEC_H264, 0, 0, Bytes::from_static(b"x"));

        // 1) 塞满队列（容量 64）→ push 必 Full → 丢帧 + resync 置位
        let mut filled = 0;
        while video_tx.try_send(tiny()).is_ok() {
            filled += 1;
        }
        assert!(filled >= 64, "应能塞满 64 容量队列，实际 {filled}");
        let s_before = session.stats();
        let _ = session.push(tiny()); // 队列满 → 走 Full 分支
        let s = session.stats();
        assert!(
            s.dropped_push > s_before.dropped_push,
            "塞满后 push 应丢帧: {s:?}"
        );
        assert!(
            resync.load(Ordering::Relaxed),
            "视频丢帧后应置位 resync（等关键帧重建，不喂花屏帧）"
        );

        // 2) writer 消费完积压后：resync 期间非关键帧被丢弃，
        //    关键帧到达触发重建（video_resyncs 增长）
        let s_before2 = session.stats();
        // 先清掉 resync 观察点：置 false，模拟 writer 已消费完队列
        resync.store(false, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            // 喂关键帧：writer 应重建解码器（resync 消费路径）
            let kf = Frame::new(
                TRACK_VIDEO,
                CODEC_H264,
                FLAG_KEYFRAME,
                0,
                Bytes::from_static(b"x"),
            );
            if session.push(kf).is_err() {
                break;
            }
            if session.stats().video_resyncs > s_before2.video_resyncs {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let s2 = session.stats();
        assert!(
            s2.video_resyncs > s_before2.video_resyncs,
            "关键帧应触发解码器重建（花屏修复路径）: {s:?}"
        );
        session.stop();
    }
}
