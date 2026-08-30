//! 音频分享端点（Audio 能力族，docs/endpoint-model-v2.md §3）：麦克风 /
//! 系统声音。
//!
//! load 探测音频采集可用性（ffmpeg 依赖，probe 由平台注入）；
//! 分享端走 [`MediaSourceEndpoint`] 统一实现（纯音频源组流推流）。订阅端
//! （播放器）是独立契约 [`SubscribeEndpoint`]，见
//! [`crate::subscribe::media::MediaReceiveEndpoint`]。

use std::result::Result as StdResult;

use stross_proto::message::MediaKind;

use crate::contract::{EndpointBase, MediaSourceEndpoint, Probe};
use crate::impl_media_source_endpoint;
use crate::pipeline::{AudioSourceConfig, VideoSource};

/// 麦克风分享端点（Audio 能力族）。
pub struct MicEndpoint {
    base: EndpointBase,
    probe: Probe,
}

impl MicEndpoint {
    pub fn new(name: impl Into<String>, probe: Probe) -> Self {
        Self {
            base: EndpointBase {
                id: "mic:builtin".into(),
                kind: MediaKind::Mic,
                name: name.into(),
                available: false,
                last_error: None,
            },
            probe,
        }
    }
}

impl MediaSourceEndpoint for MicEndpoint {
    fn video(&self) -> Option<VideoSource> {
        None
    }
    fn audio(&self) -> Option<AudioSourceConfig> {
        Some(AudioSourceConfig::default())
    }
}

impl_media_source_endpoint!(MicEndpoint {

        fn id(&self) -> &str {
            &self.base.id
        }
        fn kind(&self) -> MediaKind {
            self.base.kind
        }
        fn name(&self) -> &str {
            &self.base.name
        }
}, {

        fn available(&self) -> bool {
            self.base.available
        }
        fn last_error(&self) -> Option<&str> {
            self.base.last_error.as_deref()
        }
        fn load(&mut self) -> StdResult<(), String> {
            match (self.probe)() {
                Ok(()) => {
                    self.base.available = true;
                    self.base.last_error = None;
                    Ok(())
                }
                Err(e) => {
                    self.base.mark_failed(e.clone());
                    Err(e)
                }
            }
        }
});

/// 系统声音分享端点（Audio 能力族）：load 探测系统声音采集可用性。
pub struct SystemAudioEndpoint {
    base: EndpointBase,
    probe: Probe,
}

impl SystemAudioEndpoint {
    pub fn new(name: impl Into<String>, probe: Probe) -> Self {
        Self {
            base: EndpointBase {
                id: "sysaudio:builtin".into(),
                kind: MediaKind::SystemAudio,
                name: name.into(),
                available: false,
                last_error: None,
            },
            probe,
        }
    }
}

impl MediaSourceEndpoint for SystemAudioEndpoint {
    fn video(&self) -> Option<VideoSource> {
        None
    }
    fn audio(&self) -> Option<AudioSourceConfig> {
        let device = crate::devices::list_system_audio().into_iter().next();
        Some(AudioSourceConfig {
            system_audio: device,
            ..Default::default()
        })
    }
}

impl_media_source_endpoint!(SystemAudioEndpoint {

        fn id(&self) -> &str {
            &self.base.id
        }
        fn kind(&self) -> MediaKind {
            self.base.kind
        }
        fn name(&self) -> &str {
            &self.base.name
        }
}, {

        fn available(&self) -> bool {
            self.base.available
        }
        fn last_error(&self) -> Option<&str> {
            self.base.last_error.as_deref()
        }
        fn load(&mut self) -> StdResult<(), String> {
            match (self.probe)() {
                Ok(()) => {
                    self.base.available = true;
                    self.base.last_error = None;
                    Ok(())
                }
                Err(e) => {
                    self.base.mark_failed(e.clone());
                    Err(e)
                }
            }
        }
});
