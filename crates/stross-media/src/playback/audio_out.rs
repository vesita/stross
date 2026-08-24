//! cpal 音频输出：默认输出设备的回调拉取模型（D6 的音频输出侧）。
//!
//! 解码线程把 PCM 推入有界队列，设备回调按音频时钟拉取；队列空 → 静音
//! （underrun 由设备补零）。设备打开失败时上层回退 Discard
//! （见 [`crate::playback::AudioOut`]），保证"无声卡环境不崩"。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Data, Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};

/// 已打开的音频输出设备。
pub struct AudioSink {
    /// 保持存活（drop 即停止输出）。
    _stream: Stream,
    /// PCM 队列（f32，交织声道），由解码线程推入、设备回调拉取。
    queue: Arc<Mutex<VecDeque<f32>>>,
    /// 队列上限（样本数；约 1 秒，内存有界）。
    queue_limit: usize,
    /// 实际输出采样率 / 声道（ffmpeg 解码按此参数重采样对齐）。
    pub rate: u32,
    pub channels: u8,
}

impl AudioSink {
    /// 打开默认输出设备。失败返回 `Err`（上层回退 Discard）。
    pub fn open() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("无默认输出设备")?;
        let default_cfg = device.default_output_config().map_err(|e| e.to_string())?;
        let config = default_cfg.config();
        let rate = config.sample_rate;
        let channels = config.channels as u8;
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::with_capacity(8192)));
        let queue_limit = rate as usize * channels as usize; // ~1 秒

        // 按设备采样格式泛型构建：f32 → T 转换在回调里完成（FromSample）
        fn build<T: SizedSample + FromSample<f32>>(
            device: &Device,
            config: StreamConfig,
            queue: Arc<Mutex<VecDeque<f32>>>,
        ) -> Result<Stream, cpal::Error> {
            device.build_output_stream_raw(
                config,
                T::FORMAT,
                move |data: &mut Data, _info| {
                    if let Some(out) = data.as_slice_mut::<T>() {
                        let mut q = queue.lock().unwrap();
                        for s in out.iter_mut() {
                            *s = q
                                .pop_front()
                                .map(T::from_sample)
                                .unwrap_or_else(|| T::from_sample(0.0f32));
                        }
                    }
                },
                |e| tracing::error!("音频输出流错误: {e}"),
                Some(Duration::from_secs(5)),
            )
        }

        let stream = (match default_cfg.sample_format() {
            SampleFormat::F32 => build::<f32>(&device, config, queue.clone()),
            SampleFormat::I16 => build::<i16>(&device, config, queue.clone()),
            SampleFormat::U16 => build::<u16>(&device, config, queue.clone()),
            SampleFormat::I32 => build::<i32>(&device, config, queue.clone()),
            SampleFormat::I8 => build::<i8>(&device, config, queue.clone()),
            SampleFormat::U8 => build::<u8>(&device, config, queue.clone()),
            other => return Err(format!("不支持的设备采样格式: {other:?}")),
        })
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(Self {
            _stream: stream,
            queue,
            queue_limit,
            rate,
            channels,
        })
    }

    /// 推入 PCM（f32，交织声道）。队列满则丢弃最旧样本（滑动窗口，
    /// 保持延迟有界；实时流的正确行为）。
    pub fn push(&self, samples: &[f32]) {
        let mut q = self.queue.lock().unwrap();
        for &s in samples {
            if q.len() >= self.queue_limit {
                q.pop_front();
            }
            q.push_back(s);
        }
    }
}
