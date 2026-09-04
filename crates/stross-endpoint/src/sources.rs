//! 采集设备枚举：摄像头、麦克风、系统声音。
//!
//! 策略（尽量零额外依赖）：
//!
//! * **Windows**：解析 `ffmpeg -f dshow -list_devices true` 的输出。
//! * **Linux**：摄像头扫 `/dev/video*` + sysfs 名称；音频用 `pactl`。
//! * **Android**：返回空列表，采集走原生 Kotlin 插件。
//! * **macOS**：解析 `avfoundation` 设备列表（尽力而为）。

use std::process::Command;

#[cfg(any(target_os = "windows", target_os = "macos"))]
use crate::pipeline::ffmpeg_bin;

/// 摄像头硬件端点（纯数据 DTO，定义收敛至 stross-types——应用契约层单一真源）。
pub use stross_types::CameraEndpoint;

/// 枚举摄像头。
pub fn list_cameras() -> Vec<CameraEndpoint> {
    #[cfg(target_os = "windows")]
    {
        dshow_devices()
            .into_iter()
            .filter(|(_, kind)| kind == "video")
            .map(|(name, _)| CameraEndpoint {
                id: name.clone(),
                name,
            })
            .collect()
    }
    #[cfg(target_os = "macos")]
    {
        avfoundation_devices()
            .into_iter()
            .filter(|(_, kind)| kind == "video")
            .map(|(name, _)| CameraEndpoint {
                id: name.clone(),
                name,
            })
            .collect()
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/dev") {
            let mut paths: Vec<_> = entries
                .filter_map(std::result::Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("video"))
                })
                .collect();
            paths.sort();
            for p in paths {
                let id = p.to_string_lossy().to_string();
                let name = sysfs_name(&p).unwrap_or_else(|| id.clone());
                out.push(CameraEndpoint { id, name });
            }
        }
        out
    }
    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "linux",
        target_os = "android"
    )))]
    {
        Vec::new()
    }
}

/// 枚举麦克风（输入设备）。
pub fn list_audio_inputs() -> Vec<String> {
    #[cfg(target_os = "windows")]
    {
        dshow_devices()
            .into_iter()
            .filter(|(_, kind)| kind == "audio")
            .map(|(name, _)| name)
            .collect()
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        pulse_sources()
            .into_iter()
            .filter(|s| !s.contains(".monitor"))
            .collect()
    }
    #[cfg(target_os = "macos")]
    {
        avfoundation_devices()
            .into_iter()
            .filter(|(_, kind)| kind == "audio")
            .map(|(name, _)| name)
            .collect()
    }
    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "linux",
        target_os = "android"
    )))]
    {
        Vec::new()
    }
}

/// 系统声音枚举的缓存有效期。枚举依赖外部子进程（Linux `pactl`、
/// Windows ffmpeg dshow），`SystemAudioEndpoint::audio()` 每次组流配置都会
/// 调用；短 TTL 缓存避免高频 shell 出子进程，又能在插入/移除设备后刷新。
const SYSTEM_AUDIO_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

static SYSTEM_AUDIO_CACHE: std::sync::RwLock<Option<(std::time::Instant, Vec<String>)>> =
    std::sync::RwLock::new(None);

/// 枚举系统声音（回环采集：PulseAudio monitor / Windows Stereo Mix 等）。
///
/// 结果按 [`SYSTEM_AUDIO_CACHE_TTL`] 缓存：热路径（组流配置）不重复 fork
/// `pactl` / ffmpeg，仅缓存过期后重新枚举。采用读写锁避免并发读取锁争用。
pub fn list_system_audio() -> Vec<String> {
    let now = std::time::Instant::now();
    {
        let cache = SYSTEM_AUDIO_CACHE.read().unwrap();
        if let Some((t, devices)) = cache.as_ref()
            && now.duration_since(*t) < SYSTEM_AUDIO_CACHE_TTL
        {
            return devices.clone();
        }
    }
    let mut cache = SYSTEM_AUDIO_CACHE.write().unwrap();
    if let Some((t, devices)) = cache.as_ref()
        && now.duration_since(*t) < SYSTEM_AUDIO_CACHE_TTL
    {
        return devices.clone();
    }
    let devices = enumerate_system_audio();
    *cache = Some((now, devices.clone()));
    devices
}

/// 实际枚举系统声音（无缓存）。
fn enumerate_system_audio() -> Vec<String> {
    #[cfg(target_os = "windows")]
    {
        dshow_devices()
            .into_iter()
            .filter(|(_, kind)| kind == "audio")
            .map(|(name, _)| name)
            .filter(is_loopback_like)
            .collect()
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        pulse_sources()
            .into_iter()
            .filter(|s| s.contains(".monitor"))
            .collect()
    }
    #[cfg(target_os = "macos")]
    {
        Vec::new()
    }
    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "linux",
        target_os = "android"
    )))]
    {
        Vec::new()
    }
}

/// Windows 常见回环设备关键字（Stereo Mix / 立体声混音 / What U Hear 等）。
#[cfg(target_os = "windows")]
fn is_loopback_like(name: &str) -> bool {
    let lower = name.to_lowercase();
    [
        "stereo mix",
        "立体声混音",
        "立体声混合",
        "what u hear",
        "loopback",
        "听得到的",
        "wave out",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// 解析 `ffmpeg -f dshow -list_devices` 的输出。
/// 返回 `(设备名, 类型)`，类型为 "video" / "audio"。
#[cfg(target_os = "windows")]
fn dshow_devices() -> Vec<(String, &'static str)> {
    let out = Command::new(ffmpeg_bin())
        .args([
            "-hide_banner",
            "-f",
            "dshow",
            "-list_devices",
            "true",
            "-i",
            "dummy",
        ])
        .output();
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stderr);
    let mut devices = Vec::new();
    for line in text.lines() {
        let Some(open) = line.find('"') else { continue };
        let rest = &line[open + 1..];
        let Some(close) = rest.find('"') else {
            continue;
        };
        let name = rest[..close].to_string();
        let kind = if line.contains("(video)") {
            "video"
        } else if line.contains("(audio)") {
            "audio"
        } else {
            continue;
        };
        devices.push((name, kind));
    }
    devices
}

/// 解析 `ffmpeg -f avfoundation -list_devices` 的输出（尽力而为）。
#[cfg(target_os = "macos")]
fn avfoundation_devices() -> Vec<(String, &'static str)> {
    let out = Command::new(ffmpeg_bin())
        .args([
            "-hide_banner",
            "-f",
            "avfoundation",
            "-list_devices",
            "true",
            "-i",
            "",
        ])
        .output();
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stderr);
    let mut devices = Vec::new();
    for line in text.lines() {
        // "[AVFoundation input device @ ...] [0] FaceTime HD Camera"
        let Some(open) = line.find('[') else { continue };
        let after = &line[open + 1..];
        let Some(close) = after.find(']') else {
            continue;
        };
        let rest = after[close + 1..].trim();
        let Some(sep) = rest.find(']') else { continue };
        let desc = rest[sep + 1..].trim().to_string();
        if desc.is_empty() {
            continue;
        }
        let kind = if line.contains("capture devices")
            || desc.contains("Camera")
            || desc.contains("camera")
        {
            "video"
        } else {
            "audio"
        };
        devices.push((desc, kind));
    }
    devices
}

/// `pactl list short sources` 的源名称列表（Linux）。
#[cfg(any(target_os = "linux", target_os = "android"))]
fn pulse_sources() -> Vec<String> {
    let out = Command::new("pactl")
        .args(["list", "short", "sources"])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter_map(|line| {
            line.split_whitespace()
                .nth(1)
                .map(std::string::ToString::to_string)
        })
        .collect()
}

/// 从 sysfs 读取 v4l2 设备名。
#[cfg(any(target_os = "linux", target_os = "android"))]
fn sysfs_name(dev: &std::path::Path) -> Option<String> {
    let video = dev.file_name()?.to_string_lossy();
    let name = std::fs::read_to_string(format!("/sys/class/video4linux/{video}/name"))
        .ok()?
        .trim()
        .to_string();
    Some(if name.is_empty() {
        video.to_string()
    } else {
        name
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cameras_never_panic() {
        // 无论环境有没有设备，枚举都不应 panic
        let _ = list_cameras();
        let _ = list_audio_inputs();
        let _ = list_system_audio();
    }
}
