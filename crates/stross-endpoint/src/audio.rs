//! 音频端点（实时目标）：麦克风 / 系统声音。
//!
//! load 探测音频采集可用性（ffmpeg 依赖，probe 由平台注入）；
//! share 采集本机音频推流（麦克风默认输入 / 系统声音回环 monitor）。

use std::result::Result as StdResult;
use std::sync::Arc;

use stross_proto::message::MediaKind;

use crate::contract::{
    Endpoint, EndpointApp, EndpointBase, Probe, SubscribeCtx, TargetKind, spawn_media_share,
};
use crate::pipeline::AudioSourceConfig;

/// 麦克风端点（实时目标）。
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

impl Endpoint for MicEndpoint {
    fn id(&self) -> &str {
        &self.base.id
    }
    fn kind(&self) -> MediaKind {
        self.base.kind
    }
    fn name(&self) -> &str {
        &self.base.name
    }
    fn target(&self) -> TargetKind {
        TargetKind::Live
    }
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
    fn share(&self, app: Arc<dyn EndpointApp>, ctx: SubscribeCtx) {
        spawn_media_share(
            app,
            ctx,
            self.id(),
            self.name().to_string(),
            None,
            Some(AudioSourceConfig::default()),
        );
    }
}

/// 系统声音端点（实时目标）：load 探测系统声音采集可用性。
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

impl Endpoint for SystemAudioEndpoint {
    fn id(&self) -> &str {
        &self.base.id
    }
    fn kind(&self) -> MediaKind {
        self.base.kind
    }
    fn name(&self) -> &str {
        &self.base.name
    }
    fn target(&self) -> TargetKind {
        TargetKind::Live
    }
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
    fn share(&self, app: Arc<dyn EndpointApp>, ctx: SubscribeCtx) {
        let device = crate::devices::list_system_audio().into_iter().next();
        let audio = Some(AudioSourceConfig {
            system_audio: device,
            ..Default::default()
        });
        spawn_media_share(app, ctx, self.id(), self.name().to_string(), None, audio);
    }
}
