//! `stross adb`：经 USB（adb）查看/操作已连接手机——局域网被隔离（AP
//! isolation / 客户端隔离）、mDNS 不可达时的可靠通道，与 `stross devices`
//! （LAN mDNS 扫描）互补：
//!
//! * `stross adb status` —— 手机运行状态：型号 / 系统 / WiFi IP / 中继端口
//!   （WS/SRT/QUIC）/ 在线共享（复用中继 `/api/info` + `/api/streams` 探测，
//!   经 `adb forward` 直通，无需依赖手机与 PC 同网段）；
//! * `stross adb screenshot` —— 截取手机屏幕到 PNG（UI 状态一眼可见）。
//!
//! 网络说明：`adb forward`（PC 监听 → 手机）在本环境可用，`adb reverse`
//! （手机监听 → PC）在部分 adb 版本/传输上注册却不生效，故统一用 forward。

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use serde::Serialize;
use stross_core::relay::client as relay_http;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use stross_app::devices::StreamView;

#[derive(Args, Debug)]
pub struct AdbArgs {
    #[command(subcommand)]
    pub command: AdbCommand,
}

#[derive(Subcommand, Debug)]
pub enum AdbCommand {
    /// 已连接手机的状态（型号/网络/中继端口/在线共享；经 USB 通道）
    Status {
        /// JSON 输出（脚本化）
        #[arg(long)]
        json: bool,
        /// 中继 WS 端口探测列表（逗号分隔；GUI 固定 8777，CLI serve 18777）
        #[arg(long, default_value = "8777,18777")]
        ports: String,
    },
    /// 截取手机屏幕到 PNG
    Screenshot {
        /// 输出路径（默认 /tmp/stross-phone.png）
        #[arg(long, default_value = "/tmp/stross-phone.png")]
        out: String,
    },
    /// 手机 UI 状态：截图 + 视图树文本（uiautomator dump，WebView 页面内
    /// 文本在部分系统可见；看不到 DOM 时至少确认 WebView 在渲染/URL）。
    /// 调试用：一行命令看手机界面在显示什么。
    UiStatus {
        /// 截图输出路径（默认 /tmp/stross-phone-ui.png）
        #[arg(long, default_value = "/tmp/stross-phone-ui.png")]
        out: String,
    },
    /// 点按屏幕：按可见文本（视图树 text/content-desc 精确匹配，自动取
    /// 元素中心）或直接坐标 "x y"。配合 ui-status 做无头交互驱动。
    Tap {
        /// 匹配的可见文本（如 "共享麦克风（广播）"；WebView 常合并子文本，
        /// 必要时用 --fuzzy 子串匹配）
        text: Option<String>,
        /// 直接坐标（空格分隔 "x y"；与 --text 二选一）
        #[arg(long)]
        xy: Option<String>,
        /// 子串匹配（WebView 把名字/IP/角色并入一个节点时用）
        #[arg(long)]
        fuzzy: bool,
    },
    /// 滑动（起点 x1 y1 → 终点 x2 y2，可选时长 ms）
    Swipe {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long, default_value_t = 200)]
        ms: u64,
    },
    /// 输入文本（adb input text 仅支持 ASCII/URL 转义字符；中文需用
    /// `stross adb scan` 之外的 IME 方案，暂不支持）
    Type { text: String },
    /// 发送 keyevent（如 BACK=4 / HOME=3 / ENTER=66）
    Key { code: u16 },
}

/// 手机运行状态聚合（与 `stross devices` 的 DeviceStatus 同构，USB 通道来源）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PhoneStatus {
    serial: String,
    model: String,
    android: String,
    /// wlan0 IPv4（LAN 身份；AP 隔离时不可达，仅作信息展示）。
    wifi_ip: Option<String>,
    online: bool,
    relay_port: Option<u16>,
    srt_port: Option<u16>,
    quic_port: Option<u16>,
    streams: Vec<StreamView>,
}

pub async fn run(args: AdbArgs) -> anyhow::Result<()> {
    match args.command {
        AdbCommand::Status { json, ports } => {
            let status = phone_status(&ports).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
                return Ok(());
            }
            print_status(&status);
        }
        AdbCommand::Screenshot { out } => {
            screenshot(&out).await?;
            println!("已截取手机屏幕: {out}");
        }
        AdbCommand::UiStatus { out } => {
            ui_status(&out).await?;
        }
        AdbCommand::Tap { text, xy, fuzzy } => {
            tap(&text, &xy, fuzzy).await?;
        }
        AdbCommand::Swipe { from, to, ms } => {
            let (x1, y1) =
                parse_xy(&from).with_context(|| format!("--from 需 x y 两数，得到 {from}"))?;
            let (x2, y2) = parse_xy(&to).with_context(|| format!("--to 需 x y 两数，得到 {to}"))?;
            let serial = pick_device().await?;
            adb_sh(&serial, &format!("input swipe {x1} {y1} {x2} {y2} {ms}")).await?;
            println!("已滑动 ({x1},{y1}) → ({x2},{y2}) {ms}ms");
        }
        AdbCommand::Type { text } => {
            let serial = pick_device().await?;
            // adb input text 只支持可打印 ASCII（空格用 %s 转义）
            let escaped = text.replace(' ', "%s");
            adb_sh(&serial, &format!("input text \"{escaped}\"")).await?;
            println!("已输入: {text}");
        }
        AdbCommand::Key { code } => {
            let serial = pick_device().await?;
            adb_sh(&serial, &format!("input keyevent {code}")).await?;
            println!("已发送 keyevent {code}");
        }
    }
    Ok(())
}

/// 点按：优先按可见文本（视图树 text/content-desc 精确匹配 → 元素中心），
/// 否则用 --xy 直接坐标。
#[allow(clippy::too_many_arguments)]
async fn tap(text: &Option<String>, xy: &Option<String>, fuzzy: bool) -> anyhow::Result<()> {
    let serial = pick_device().await?;
    let (x, y) = if let Some(xy) = xy {
        parse_xy(xy).context("--xy 需 x y 两数（空格分隔）")?
    } else {
        let target = text.as_deref().context("需要 --text 或 --xy 之一")?;
        let nodes = ui_nodes(&adb_sh(&serial, "uiautomator dump /sdcard/sx.xml >/dev/null 2>&1; cat /sdcard/sx.xml; rm -f /sdcard/sx.xml").await?);
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
fn parse_xy(s: &str) -> anyhow::Result<(u32, u32)> {
    let mut it = s.split_whitespace();
    let x: u32 = it.next().context("缺 x")?.parse()?;
    let y: u32 = it.next().context("缺 y")?.parse()?;
    Ok((x, y))
}

/// uiautomator dump XML 里的一个节点（文本 + 描述 + bounds）。
struct UiNode {
    text: String,
    desc: String,
    bounds: String,
}

/// 把 uiautomator dump XML 解析为节点列表（不引入 XML 依赖）。
fn ui_nodes(xml: &str) -> Vec<UiNode> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<node") {
        let seg = &rest[i + 5..];
        let end = seg.find('>').unwrap_or(seg.len());
        let tag = &seg[..end];
        out.push(UiNode {
            text: attr_value(tag, "text"),
            desc: attr_value(tag, "content-desc"),
            bounds: attr_value(tag, "bounds"),
        });
        rest = &seg[end + 1..];
    }
    out
}

/// 取标签里 `name="..."` 的属性值（简单扫描，空值=缺）。
fn attr_value(tag: &str, name: &str) -> String {
    let needle = format!("{name}=\"");
    let Some(i) = tag.find(&needle) else {
        return String::new();
    };
    let tail = &tag[i + needle.len()..];
    let end = tail.find('"').unwrap_or(0);
    tail[..end].to_string()
}

/// 解析 `bounds="[x1,y1][x2,y2]"` → 中心坐标。
fn bounds_center(s: &str) -> Option<(u32, u32)> {
    let body = s.trim().strip_prefix('[')?.strip_suffix(']')?;
    let (a, b) = body.split_once("][")?;
    let (x1, y1) = a.split_once(',')?;
    let (x2, y2) = b.split_once(',')?;
    let (x1, y1, x2, y2): (u32, u32, u32, u32) = (
        x1.parse().ok()?,
        y1.parse().ok()?,
        x2.parse().ok()?,
        y2.parse().ok()?,
    );
    Some(((x1 + x2) / 2, (y1 + y2) / 2))
}

/// 手机 UI 状态：截图 + 视图树文本（uiautomator dump）。
async fn ui_status(out: &str) -> anyhow::Result<()> {
    let serial = pick_device().await?;
    let n = screenshot_to(&serial, out).await?;
    println!("截图: {out}（{n} 字节）");
    match dump_ui_text(&serial).await {
        Ok(lines) if !lines.is_empty() => {
            println!("视图树文本（uiautomator dump）：");
            for l in lines {
                println!("  {l}");
            }
        }
        Ok(_) => println!("视图树无可见文本（WebView 页面内容通常不暴露给 uiautomator；截图见上）"),
        Err(e) => println!("uiautomator dump 失败: {e:#}（截图仍可看 UI）"),
    }
    Ok(())
}

/// `adb shell uiautomator dump` → 解析 XML 里的 text / content-desc 文本节点，
/// 按视图树顺序返回非空文本行。
async fn dump_ui_text(serial: &str) -> anyhow::Result<Vec<String>> {
    let path = "/sdcard/stross_ui.xml";
    let _ = adb_sh(serial, &format!("rm -f {path}")).await;
    let dump = adb_sh(serial, &format!("uiautomator dump {path}")).await?;
    if dump.contains("ERROR") || dump.contains("error") {
        anyhow::bail!("{dump}");
    }
    let xml = adb_sh(serial, &format!("cat {path}")).await?;
    let _ = adb_sh(serial, &format!("rm -f {path}")).await;
    // text 与 content-desc 都可能承载用户可见文本；空白值跳过
    let mut out = Vec::new();
    for s in collect_attr("text", &xml)
        .into_iter()
        .chain(collect_attr("content-desc", &xml))
    {
        let s = decode_xml(&s);
        if !s.trim().is_empty() {
            out.push(s);
        }
    }
    // 去重保序（WebView 常重复暴露同文本）
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.clone()));
    Ok(out)
}

/// 扫描 XML 里所有 `attr="..."` 的属性值（顺序保持；避免为一次解析引入
/// regex 依赖）。
fn collect_attr(attr: &str, text: &str) -> Vec<String> {
    let needle = format!("{attr}=\"");
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(&needle) {
        let start = i + needle.len();
        let tail = &rest[start..];
        if let Some(end) = tail.find('"') {
            out.push(tail[..end].to_string());
            rest = &tail[end + 1..];
        } else {
            break;
        }
    }
    out
}

/// 基础 XML 实体解码（&amp; &quot; &lt; &gt;）。
fn decode_xml(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&apos;", "'")
}

/// 探测已连接手机的状态（`adb forward` 直通中继 /api/info + /api/streams）。
async fn phone_status(ports_arg: &str) -> anyhow::Result<PhoneStatus> {
    let ports: Vec<u16> = ports_arg
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    let serial = pick_device().await?;
    let model = adb_sh(&serial, "getprop ro.product.model")
        .await
        .unwrap_or_default();
    let android = adb_sh(&serial, "getprop ro.build.version.release")
        .await
        .unwrap_or_default();
    let wifi_ip = adb_sh(&serial, "ip -4 addr show wlan0")
        .await
        .ok()
        .and_then(|out| {
            out.lines()
                .find(|l| l.trim_start().starts_with("inet "))
                .map(|l| {
                    l.split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .split('/') // 去掉 /24 前缀长度
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
        })
        .filter(|ip| !ip.is_empty());

    // 经 adb forward 探测中继 HTTP：先在 PC 侧占一个空闲监听端口
    let local_port = free_local_port();
    let mut status = PhoneStatus {
        serial: serial.clone(),
        model: model.trim().to_string(),
        android: android.trim().to_string(),
        wifi_ip: wifi_ip.clone(),
        online: false,
        relay_port: None,
        srt_port: None,
        quic_port: None,
        streams: Vec::new(),
    };
    for relay_port in &ports {
        if !adb_forward(&serial, local_port, *relay_port).await? {
            continue; // forward 失败，换下一个候选端口
        }
        let probe = Duration::from_millis(1500);
        match relay_http::info("127.0.0.1", local_port, probe).await {
            Ok(info) => {
                status.online = true;
                status.relay_port = Some(*relay_port);
                status.srt_port = info.srt_port;
                status.quic_port = info.quic_port;
                // /api/streams（同一 forward 会话）
                if let Ok(list) = relay_http::streams("127.0.0.1", local_port, probe).await {
                    status.streams = stross_app::devices::to_views(list);
                }
            }
            Err(_) => {
                // 该端口不是中继（或无 HTTP），清理后试下一个
            }
        }
        let _ = adb_forward_remove(&serial, local_port).await;
        if status.online {
            break;
        }
    }
    Ok(status)
}

// 流信息 → 展示视图投影收在 `stross_app::devices::to_views`（探测契约在
// stross_core::relay::client；docs/layering-architecture.md）。

fn print_status(s: &PhoneStatus) {
    println!("手机状态（经 USB/adb，serial={}）", s.serial);
    println!(
        "  型号      {}",
        if s.model.is_empty() { "?" } else { &s.model }
    );
    println!(
        "  系统      Android {}",
        if s.android.is_empty() {
            "?"
        } else {
            &s.android
        }
    );
    match &s.wifi_ip {
        Some(ip) => println!("  WiFi IP   {ip}"),
        None => println!("  WiFi IP   未获取到 wlan0 IPv4"),
    }
    if !s.online {
        println!("  中继      未探测到（手机未运行 Stross？或中继端口非 8777/18777）");
        return;
    }
    let srt = s
        .srt_port
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".into());
    let quic = s
        .quic_port
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".into());
    println!(
        "  中继      ws://{}:{}（SRT {srt} · QUIC {quic}）",
        s.wifi_ip.as_deref().unwrap_or("<ip>"),
        s.relay_port.unwrap_or(0)
    );
    println!("  在线共享  {} 条", s.streams.len());
    for st in &s.streams {
        let kinds = match (st.video, st.audio) {
            (true, true) => "视频+音频",
            (true, false) => "视频",
            (false, true) => "音频",
            (false, false) => "?",
        };
        println!(
            "    [{kinds}] {}「{}」watchers={}",
            st.stream_id, st.title, st.watchers
        );
    }
    println!("  提示      手机与 PC 同网段时 `stross devices` 也能发现；AP 隔离时用本命令");
}

/// 截取手机屏幕（`adb exec-out screencap -p`）到文件，返回字节数。
async fn screenshot(out: &str) -> anyhow::Result<u64> {
    let serial = pick_device().await?;
    screenshot_to(&serial, out).await
}

/// 截取指定手机屏幕到文件，返回字节数（`ui-status` 复用）。
async fn screenshot_to(serial: &str, out: &str) -> anyhow::Result<u64> {
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
async fn pick_device() -> anyhow::Result<String> {
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
async fn adb_sh(serial: &str, cmd: &str) -> anyhow::Result<String> {
    let out = Command::new("adb")
        .args(["-s", serial, "shell", cmd])
        .output()
        .await
        .context("adb shell 失败")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 建立 `adb forward tcp:<local> tcp:<relay>`，返回是否成功。
async fn adb_forward(serial: &str, local: u16, relay: u16) -> anyhow::Result<bool> {
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
async fn adb_forward_remove(serial: &str, local: u16) -> anyhow::Result<()> {
    let _ = Command::new("adb")
        .args(["-s", serial, "forward", "--remove", &format!("tcp:{local}")])
        .output()
        .await?;
    Ok(())
}

/// 占一个空闲本地端口号（bind 0 后丢弃；竞争窗口极小，够用）。
fn free_local_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .map(|l| l.local_addr().map(|a| a.port()).unwrap_or(0))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_attr_picks_text_in_order() {
        // 真实 uiautomator 形态：text="" 空值后面还有其它属性，解析须跳过并继续
        let xml = r#"<node text="设备列表" class="h"/><node text="手机A" content-desc="点我"/><node text="" class="y"/>"#;
        assert_eq!(
            collect_attr("text", xml),
            vec!["设备列表".to_string(), "手机A".to_string(), "".to_string()]
        );
        assert_eq!(collect_attr("content-desc", xml), vec!["点我".to_string()]);
        // 截断/畸形输入不崩溃（值未闭合 → 该属性丢弃，正常终止）
        assert_eq!(
            collect_attr("text", "<node text=\"abc"),
            Vec::<String>::new()
        );
        assert_eq!(
            collect_attr("text", "<node text=\"abc\""),
            vec!["abc".to_string()]
        );
    }

    #[test]
    fn decode_xml_entities() {
        assert_eq!(
            decode_xml("a&amp;b&quot;c&lt;d&gt;e&apos;f"),
            "a&b\"c<d>e'f"
        );
    }

    #[test]
    fn empty_text_is_filtered() {
        let xml = r#"<node text="" /><node text="有内容" />"#;
        let mut out: Vec<String> = collect_attr("text", xml)
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        let mut seen = std::collections::HashSet::new();
        out.retain(|s| seen.insert(s.clone()));
        assert_eq!(out, vec!["有内容".to_string()]);
    }
}

#[cfg(test)]
mod tests2 {
    use super::*;

    #[test]
    fn attr_value_reads_and_misses() {
        let tag = r#"<node text="扫描" bounds="[0,1][2,3]"/>"#;
        assert_eq!(attr_value(tag, "text"), "扫描");
        assert_eq!(attr_value(tag, "bounds"), "[0,1][2,3]");
        assert_eq!(attr_value(tag, "content-desc"), "");
    }

    #[test]
    fn bounds_center_computes_middle() {
        assert_eq!(bounds_center("[0,0][100,200]"), Some((50, 100)));
        assert_eq!(bounds_center("[10,20][30,40]"), Some((20, 30)));
        assert_eq!(bounds_center("(0,0)(1,1)"), None);
        assert_eq!(bounds_center(""), None);
    }

    #[test]
    fn parse_xy_two_numbers() {
        assert_eq!(parse_xy("238 496").unwrap(), (238, 496));
        assert!(parse_xy("238").is_err());
        assert!(parse_xy("x y").is_err());
    }

    #[test]
    fn ui_nodes_extract_text_and_bounds() {
        let xml = r#"<?xml?><hierarchy><node text="设备" bounds="[1,1][2,2]"/><node content-desc="点我" bounds="[3,3][4,4]"/></hierarchy>"#;
        let nodes = ui_nodes(xml);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].text, "设备");
        assert_eq!(nodes[0].bounds, "[1,1][2,2]");
        assert_eq!(nodes[1].desc, "点我");
    }
}

/// bounds "[x1,y1][x2,y2]" 面积；无/畸形返回 0（零面积节点不可点）。
fn bounds_area(s: &str) -> u64 {
    let body = match s.trim().strip_prefix('[').and_then(|b| b.strip_suffix(']')) {
        Some(b) => b,
        None => return 0,
    };
    let Some((a, b)) = body.split_once("][") else {
        return 0;
    };
    let (x1, y1) = match a.split_once(',') {
        Some(v) => v,
        None => return 0,
    };
    let (x2, y2) = match b.split_once(',') {
        Some(v) => v,
        None => return 0,
    };
    let (Ok(x1), Ok(y1), Ok(x2), Ok(y2)): (
        Result<u64, _>,
        Result<u64, _>,
        Result<u64, _>,
        Result<u64, _>,
    ) = (x1.parse(), y1.parse(), x2.parse(), y2.parse()) else {
        return 0;
    };
    let (w, h) = (x2.abs_diff(x1), y2.abs_diff(y1));
    w * h
}
