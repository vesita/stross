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
                        let available = q.len().min(out.len());
                        let (s1, s2) = q.as_slices();
                        let take_s1 = s1.len().min(available);
                        for (dst, &src) in out[..take_s1].iter_mut().zip(&s1[..take_s1]) {
                            *dst = T::from_sample(src);
                        }
                        let rem = available - take_s1;
                        if rem > 0 {
                            for (dst, &src) in out[take_s1..available].iter_mut().zip(&s2[..rem]) {
                                *dst = T::from_sample(src);
                            }
                        }
                        q.drain(..available);
                        // 队列不足（欠载）时补静音（0.0）
                        let zero = T::from_sample(0.0f32);
                        for dst in &mut out[available..] {
                            *dst = zero;
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
        if samples.is_empty() {
            return;
        }
        let mut q = self.queue.lock().unwrap();
        // 超过队列上限时，丢弃最旧样本以控制延迟
        let total = q.len() + samples.len();
        if total > self.queue_limit {
            let overflow = total - self.queue_limit;
            let drain_len = overflow.min(q.len());
            q.drain(..drain_len);
        }
        // 若单次推入仍超过上限（极长样本），仅保留最新部分
        let samples_to_push = if samples.len() > self.queue_limit {
            &samples[samples.len() - self.queue_limit..]
        } else {
            samples
        };
        q.extend(samples_to_push.iter().copied());
    }
}
