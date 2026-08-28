//! Sink 能力（接收/消费侧）：设计文档 docs/plugin-architecture.md §6.2。
//!
//! [`CaptureBackend`](crate::capture::CaptureBackend) 是 Source（采集 → 帧流），
//! [`Sink`] 是反向：消费帧流做渲染 / 录制 / 注入。与 Source 共用同一套
//! [`CapabilityDescriptor`] 能力描述，向内核能力注册表上报。
//!
//! 首个实现是 [`RecordingSink`]：把帧流按轨道写成原始 ES 文件
//! （视频 Annex-B `.h264` + 音频 ADTS `.aac`），无外部依赖、可直接用
//! ffmpeg/ffplay 播放或转封装 mp4。
//!
//! Deskflow 方向（键鼠注入 / 剪贴板共享）是坐在 Lossless 会话上的
//! `InputSink` / `ClipboardSink`，复用同一套会话/路由/传输基座——架构上
//! 无新增概念，只有新能力插件（见设计文档 §6.2 与 §7 安全）。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use stross_proto::frame::{Frame, TRACK_VIDEO};
use stross_proto::message::{
    CapabilityDescriptor, CapabilityKind, CodecId, MediaKind, ReliabilityProfile, TransportId,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};

/// Sink 能力：消费一个帧流。
///
/// * `start`：开始消费 `rx` 里的帧；返回 `Ok` 只代表已发起。
/// * `stop`：停止消费（同步发起，收尾在后台任务完成）。
pub trait Sink: Send + Sync {
    /// 能力描述（能力广播 / 协商用）。
    fn descriptor(&self) -> CapabilityDescriptor;
    /// 开始消费帧流。
    fn start(&self, rx: mpsc::Receiver<Frame>) -> Result<()>;
    /// 停止消费。
    fn stop(&self);
}

/// 录制 Sink：把帧流按轨道写入原始 ES 文件。
///
/// 输出文件由 `base` 决定：`<base>.h264`（视频 Annex-B）+ `<base>.aac`（音频 ADTS）；
/// 只出现实际收到的轨道。文件内容即标准 ES 流，可用
/// `ffplay <base>.h264` 直接播放，或 `ffmpeg -i <base>.h264 -i <base>.aac -c copy out.mp4`
/// 转封装。
pub struct RecordingSink {
    state: Mutex<Option<Running>>,
    base: PathBuf,
}

struct Running {
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl RecordingSink {
    /// `base`：输出文件前缀（无扩展名，如 `/tmp/stross-rec`）。
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            state: Mutex::new(None),
            base: base.into(),
        }
    }

    /// 输出文件前缀。
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// 停止录制并等待任务收尾（测试 / 优雅关闭用）。
    ///
    /// 与 [`Sink::stop`] 的区别：`stop` 只发起停止（写循环处理完已入队的帧后
    /// 退出），本方法会等写循环真正结束（帧源关闭或已调用 `stop`）。
    pub async fn wait_idle(&self) {
        let running = self.state.lock().unwrap().take();
        if let Some(r) = running {
            let _ = r.task.await;
        }
    }
}

impl Sink for RecordingSink {
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
            transports: vec![TransportId::Ws, TransportId::WebRtc],
            max_width: Some(1920),
            max_height: Some(1080),
            preferred_profile: ReliabilityProfile::Lossy,
        }
    }

    fn start(&self, rx: mpsc::Receiver<Frame>) -> Result<()> {
        let mut guard = self.state.lock().unwrap();
        if guard.is_some() {
            bail!("已在录制，请先停止");
        }
        if let Some(parent) = self.base.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).context("创建录制目录失败")?;
        }
        let video_path = self.base.with_extension("h264");
        let audio_path = self.base.with_extension("aac");
        let video = tokio::fs::File::from_std(
            std::fs::File::create(&video_path)
                .with_context(|| format!("创建视频文件失败: {}", video_path.display()))?,
        );
        let audio = tokio::fs::File::from_std(
            std::fs::File::create(&audio_path)
                .with_context(|| format!("创建音频文件失败: {}", audio_path.display()))?,
        );
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(record_loop(rx, video, audio, shutdown_rx));
        tracing::info!("录制开始: {}", self.base.display());
        *guard = Some(Running {
            shutdown: shutdown_tx,
            task,
        });
        Ok(())
    }

    fn stop(&self) {
        let running = self.state.lock().unwrap().take();
        if let Some(r) = running {
            let _ = r.shutdown.send(true);
            tracing::info!("录制停止: {}", self.base.display());
        }
    }
}

/// 写循环：按轨道分流写入两个文件；rx 关闭或收到停止信号时结束。
async fn record_loop(
    mut rx: mpsc::Receiver<Frame>,
    mut video: tokio::fs::File,
    mut audio: tokio::fs::File,
    mut shutdown: watch::Receiver<bool>,
) {
    let (mut video_frames, mut audio_frames) = (0u64, 0u64);
    loop {
        tokio::select! {
            // biased：帧优先于停止信号，正常录制不因竞争丢帧
            biased;
            frame = rx.recv() => match frame {
                Some(f) => {
                    let (file, counter) = if f.header.track == TRACK_VIDEO {
                        (&mut video, &mut video_frames)
                    } else {
                        (&mut audio, &mut audio_frames)
                    };
                    // 载荷原样写入：视频是 Annex-B 访问单元、音频是 ADTS 帧，
                    // 拼接后即合法 ES 流
                    if file.write_all(&f.payload).await.is_err() {
                        tracing::warn!("录制写文件失败");
                        break;
                    }
                    *counter += 1;
                }
                None => break, // 帧源已关闭（推流结束）
            },
            _ = shutdown.changed() => break,
        }
    }
    let _ = video.flush().await;
    let _ = audio.flush().await;
    tracing::debug!("录制结束: 视频 {video_frames} 帧, 音频 {audio_frames} 帧");
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::{CODEC_AAC, CODEC_H264, FLAG_KEYFRAME, TRACK_AUDIO};

    #[tokio::test]
    async fn recording_sink_writes_raw_es_by_track() {
        let dir = std::env::temp_dir().join(format!("stross-sink-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sink = RecordingSink::new(dir.join("rec"));
        let (tx, rx) = mpsc::channel(16);
        sink.start(rx).unwrap();

        let v1 = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1f];
        let v2 = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84];
        let a1 = vec![0xff, 0xf1, 0x50, 0x80, 0x01];
        tx.send(Frame::new(
            TRACK_VIDEO,
            CODEC_H264,
            FLAG_KEYFRAME,
            0,
            v1.clone(),
        ))
        .await
        .unwrap();
        tx.send(Frame::new(TRACK_VIDEO, CODEC_H264, 0, 40, v2.clone()))
            .await
            .unwrap();
        tx.send(Frame::new(TRACK_AUDIO, CODEC_AAC, 0, 0, a1.clone()))
            .await
            .unwrap();
        drop(tx); // 帧源关闭 → 写循环自行结束

        sink.wait_idle().await;
        let video = std::fs::read(dir.join("rec.h264")).unwrap();
        let audio = std::fs::read(dir.join("rec.aac")).unwrap();
        let mut expected_v = v1;
        expected_v.extend_from_slice(&v2);
        assert_eq!(video, expected_v, "视频 ES 应按到达顺序拼接");
        assert_eq!(audio, a1, "音频 ES 应按到达顺序拼接");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn double_start_is_rejected() {
        let dir = std::env::temp_dir().join(format!("stross-sink-test2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sink = RecordingSink::new(dir.join("rec"));
        let (tx, rx) = mpsc::channel(4);
        sink.start(rx).unwrap();
        let (_, rx2) = mpsc::channel(4);
        assert!(sink.start(rx2).is_err(), "重复 start 应报错");
        drop(tx);
        sink.wait_idle().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn descriptor_is_sink() {
        let sink = RecordingSink::new("/tmp/stross-rec");
        let d = sink.descriptor();
        assert_eq!(d.kind, CapabilityKind::Sink);
        assert!(d.codecs.contains(&CodecId::H264));
        assert_eq!(d.preferred_profile, ReliabilityProfile::Lossy);
    }
}
