//! 端点订阅驱动（docs/endpoint-model.md §5 联动）：公开方收到订阅事件后
//! 按端点类型自动开推。
//!
//! * 文件端点 → 文件泵（[`file_xfer::push_file`]）：pull 推入自己的受控中继
//!   （回环 + 内核预授权会话），push 凭订阅方凭证出站推入订阅方中继；
//! * 媒体端点（屏幕 / 麦克风）→ 复用 [`Kernel::start_stream`]：pull 推本机
//!   中继（可多订阅者观看），push 带订阅方凭证出站（复用既有 B2 路径）。
//!
//! CLI serve 启动时安装一次；GUI 桌面本轮不接线（前端交互未落地）。

use std::sync::Arc;

use stross_media::pipeline::{AudioSourceConfig, StreamConfig, VideoSource};
use stross_proto::message::{Delivery, MediaKind};

use crate::Kernel;
use crate::file_xfer::{FilePushOptions, push_file};
use crate::kernel::{SubscribeCtx, SubscribeHook};

/// 在 `app` 上安装订阅驱动（幂等：重复调用覆盖 hook）。
pub fn install_endpoint_driver(app: &Arc<Kernel>) {
    let a = app.clone();
    let hook: Arc<SubscribeHook> = Arc::new(move |endpoint_id, ctx| {
        let Some(manifest) = a.endpoint_manifest(endpoint_id) else {
            return;
        };
        let kind = manifest.device.kind;
        match kind {
            MediaKind::File => spawn_file_push(a.clone(), endpoint_id.to_string(), ctx.clone()),
            MediaKind::Screen | MediaKind::Mic => {
                spawn_media_push(a.clone(), endpoint_id.to_string(), ctx.clone(), kind)
            }
            other => {
                tracing::warn!("端点 {endpoint_id}（{other:?}）暂不支持订阅自动推流，忽略");
            }
        }
    });
    app.set_subscribe_hook(Some(hook));
}

/// 驱动各方向的推送地址：
/// * pull → 自己的受控中继（回环地址；会话已由协商层签发并预授权）；
/// * push → 订阅方中继基址 + `/ws/push` 路径。
fn resolve_push_url(app: &Kernel, ctx: &SubscribeCtx) -> Option<String> {
    match ctx.delivery {
        Delivery::Push => {
            let base = ctx.relay_addr.as_deref()?;
            Some(format!("{base}/ws/push"))
        }
        Delivery::Pull | Delivery::Both => {
            let port = app.relay_port()?;
            Some(format!("ws://127.0.0.1:{port}/ws/push"))
        }
    }
}

/// 观看数轮询基址（泵等观看者用）：push = 订阅方中继；pull = 自己中继。
fn resolve_watcher_base(app: &Kernel, ctx: &SubscribeCtx) -> Option<String> {
    match ctx.delivery {
        Delivery::Push => ctx.relay_addr.clone(),
        Delivery::Pull | Delivery::Both => app.relay_port().map(|p| format!("ws://127.0.0.1:{p}")),
    }
}

fn spawn_file_push(app: Arc<Kernel>, endpoint_id: String, ctx: SubscribeCtx) {
    tokio::spawn(async move {
        let Some(src) = app.file_source(&endpoint_id) else {
            tracing::warn!("文件端点 {endpoint_id} 无本地文件源，无法推送");
            return;
        };
        let Some(url) = resolve_push_url(&app, &ctx) else {
            tracing::warn!(
                "文件端点 {endpoint_id} 无可用推送地址（pull 未锚定中继 / push 缺订阅方地址）"
            );
            return;
        };
        let watcher_base = resolve_watcher_base(&app, &ctx);
        let opts = FilePushOptions {
            push_url: url,
            stream_id: ctx.stream_id.clone(),
            title: format!("文件 {}", src.name),
            share_token: if ctx.delivery == Delivery::Push {
                ctx.share_token.clone()
            } else {
                None
            },
            watcher_base,
        };
        match push_file(&src.path, &opts).await {
            Ok(sent) => tracing::info!(
                "文件端点 {endpoint_id} 已推送「{}」({sent} 字节, stream={}) 给订阅方 {}",
                src.name,
                ctx.stream_id,
                ctx.subscriber,
            ),
            Err(e) => tracing::warn!(
                "文件端点 {endpoint_id} 推送失败（订阅方 {}）: {e:#}",
                ctx.subscriber
            ),
        }
    });
}

/// 媒体端点自动推流：复用 `start_stream`（含凭证出站）。
fn spawn_media_push(app: Arc<Kernel>, endpoint_id: String, ctx: SubscribeCtx, kind: MediaKind) {
    tokio::spawn(async move {
        let manifest = match app.endpoint_manifest(&endpoint_id) {
            Some(m) => m,
            None => return,
        };
        let cfg = media_config(
            manifest.device.name.clone(),
            kind,
            ctx.stream_id.clone(),
            if ctx.delivery == Delivery::Push {
                ctx.share_token.clone()
            } else {
                None
            },
        );
        let relay_url = match ctx.delivery {
            Delivery::Push => resolve_push_url(&app, &ctx),
            Delivery::Pull | Delivery::Both => None, // 推本机中继，地址自动
        };
        match app.start_stream(cfg, relay_url).await {
            Ok(r) => tracing::info!(
                "端点 {endpoint_id} 已自动推流（{kind:?}）: stream={} 订阅方 {}",
                r.stream_id,
                ctx.subscriber
            ),
            Err(e) => tracing::warn!("端点 {endpoint_id} 自动推流失败: {e:#}"),
        }
    });
}

/// 按设备类型组推流配置（屏幕 → 视频；麦克风 → 音频；时长无限，直到停止）。
fn media_config(
    title: String,
    kind: MediaKind,
    stream_id: String,
    share_token: Option<String>,
) -> StreamConfig {
    let (video, audio) = match kind {
        MediaKind::Screen => (Some(VideoSource::Screen), None),
        MediaKind::Mic => (None, Some(AudioSourceConfig::default())),
        _ => (None, None),
    };
    StreamConfig {
        stream_id,
        title,
        video,
        quality: stross_media::pipeline::Quality::MEDIUM,
        audio,
        duration_secs: None,
        share_token,
    }
}
