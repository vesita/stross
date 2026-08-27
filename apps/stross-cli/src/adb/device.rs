//! adb 设备层：进程执行 / forward 直通 / 截屏 / 输入（平台桥，无内核逻辑）。

use std::process::Stdio;

use anyhow::{Context, bail};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::ui::{bounds_area, bounds_center, ui_nodes};

/// 点按：优先按可见文本（视图树 text/content-desc 精确匹配 → 元素中心），
/// 否则用 --xy 直接坐标。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn tap(
    text: &Option<String>,
    xy: &Option<String>,
    fuzzy: bool,
) -> anyhow::Result<()> {
    let serial = pick_device().await?;
    let (x, y) = if let Some(xy) = xy {
        parse_xy(xy).context("--xy 需 x y 两数（空格分隔）")?
    } else {
        let target = text.as_deref().context("需要 --text 或 --xy 之一")?;
        let nodes = ui_nodes(
            &adb_sh(
                &serial,
                "uiautomator dump /sdcard/sx.xml >/dev/null 2>&1; cat /sdcard/sx.xml; rm -f /sdcard/sx.xml",
            )
            .await?,
        );
        let hit = if fuzzy {
            nodes
                .iter()
                .filter(|n| n.text.contains(target) || n.desc.contains(target))
                .max_by_key(|n| bounds_area(&n.bounds))
        } else {
            nodes.iter().find(|n| n.text == target || n.desc == target)
        };
        let Some(hit) = hit else {
            bail!(
                "未找到文本「{target}」（fuzzy={fuzzy}）；可用 `stross adb ui-status` 看当前界面的文本列表"
            );
        };
        let c = bounds_center(&hit.bounds)
            .with_context(|| format!("节点 bounds 解析失败: {:?}", hit.bounds))?;
        if c == (0, 0) {
            bail!("文本「{target}」所在节点面积为 0（不可点），请换更具体的文本或用 --xy");
        }
        c
    };
    adb_sh(&serial, &format!("input tap {x} {y}")).await?;
    println!(
        "已点按 ({x},{y}){}",
        text.as_deref()
            .map(|t| format!("（{t}）"))
            .unwrap_or_default()
    );
    Ok(())
}

/// 解析 "x y" 坐标。
pub(crate) fn parse_xy(s: &str) -> anyhow::Result<(u32, u32)> {
    let mut it = s.split_whitespace();
    let x: u32 = it.next().context("缺 x")?.parse()?;
    let y: u32 = it.next().context("缺 y")?.parse()?;
    Ok((x, y))
}

/// 截取手机屏幕（`adb exec-out screencap -p`）到文件，返回字节数。
pub(crate) async fn screenshot(out: &str) -> anyhow::Result<u64> {
    let serial = pick_device().await?;
    screenshot_to(&serial, out).await
}

/// 截取指定手机屏幕到文件，返回字节数（`ui-status` 复用）。
pub(crate) async fn screenshot_to(serial: &str, out: &str) -> anyhow::Result<u64> {
    let mut child = Command::new("adb")
        .args(["-s", serial, "exec-out", "screencap", "-p"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 adb 失败（请安装 android-tools）")?;
    let mut buf = Vec::new();
    child
        .stdout
        .take()
        .context("adb stdout 不可用")?
        .read_to_end(&mut buf)
        .await
        .context("读取截屏失败")?;
    let st = child.wait().await.context("等待 adb 失败")?;
    if !st.success() || buf.is_empty() {
        bail!("adb screencap 失败（exit={st}，字节={}）", buf.len());
    }
    let n = buf.len();
    tokio::fs::write(out, &buf)
        .await
        .with_context(|| format!("写文件 {out} 失败"))?;
    Ok(n as u64)
}

/// 解析 `adb devices`：要求恰好一台设备（多台时报错列出，可用 adb 指定）。
pub(crate) async fn pick_device() -> anyhow::Result<String> {
    let out = Command::new("adb")
        .arg("devices")
        .output()
        .await
        .context("启动 adb 失败（请安装 android-tools）")?;
    if !out.status.success() {
        bail!("adb devices 失败: {}", String::from_utf8_lossy(&out.stderr));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let devices: Vec<String> = text
        .lines()
        .skip(1) // 表头 "List of devices attached"
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let serial = parts.next()?;
            let state = parts.next()?;
            (state == "device").then(|| serial.to_string())
        })
        .collect();
    match devices.len() {
        0 => bail!("未检测到连接的手机（`adb devices` 无 device）"),
        1 => Ok(devices[0].clone()),
        n => bail!(
            "检测到 {n} 台设备，请先 `adb devices` 确认并保留唯一连接：{}",
            devices.join(", ")
        ),
    }
}

/// 执一条 `adb shell` 只读命令，返回 stdout 文本。
pub(crate) async fn adb_sh(serial: &str, cmd: &str) -> anyhow::Result<String> {
    let out = Command::new("adb")
        .args(["-s", serial, "shell", cmd])
        .output()
        .await
        .context("adb shell 失败")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 建立 `adb forward tcp:<local> tcp:<relay>`，返回是否成功。
pub(crate) async fn adb_forward(serial: &str, local: u16, relay: u16) -> anyhow::Result<bool> {
    let out = Command::new("adb")
        .args([
            "-s",
            serial,
            "forward",
            &format!("tcp:{local}"),
            &format!("tcp:{relay}"),
        ])
        .output()
        .await
        .context("adb forward 失败")?;
    Ok(out.status.success())
}

/// 移除指定的 forward（探测结束后清理，不留僵尸监听）。
pub(crate) async fn adb_forward_remove(serial: &str, local: u16) -> anyhow::Result<()> {
    let _ = Command::new("adb")
        .args(["-s", serial, "forward", "--remove", &format!("tcp:{local}")])
        .output()
        .await?;
    Ok(())
}

/// 占一个空闲本地端口号（bind 0 后丢弃；竞争窗口极小，够用）。
pub(crate) fn free_local_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .map(|l| l.local_addr().map(|a| a.port()).unwrap_or(0))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_xy_two_numbers() {
        assert_eq!(parse_xy("238 496").unwrap(), (238, 496));
        assert!(parse_xy("238").is_err());
        assert!(parse_xy("x y").is_err());
    }
}
