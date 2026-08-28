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
//!
//! 模块划分（平台桥——adb 是设备层的平台粘合，**无内核逻辑**；探测契约
//! 复用 `stross_kernel::relay::client` 与 `stross_kernel::devices`）：
//! * [`status`]：手机状态聚合 + 展示视图
//! * [`ui`]：uiautomator 视图树解析 + UI 状态/点按辅助
//! * [`device`]：adb 进程执行 / forward / 截屏 / 输入

pub mod device;
pub mod status;
pub mod ui;

use anyhow::Context;
use clap::{Args, Subcommand};

use self::device::{adb_sh, parse_xy, pick_device, screenshot, tap};
use self::status::{phone_status, print_status};

/// 中继 WS 端口探测列表默认值：Android GUI 固定 8777 + 协议默认 18777。
/// 端口真源在库层（`stross_kernel::relay::{GUI_PORT, DEFAULT_PORT}`），
/// 壳层不得硬编码端口号（docs/layering-architecture.md）。
fn default_probe_ports() -> String {
    format!(
        "{},{}",
        stross_kernel::relay::GUI_PORT,
        stross_kernel::relay::DEFAULT_PORT
    )
}

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
        /// 中继 WS 端口探测列表（逗号分隔；Android GUI 固定 [GUI_PORT]，协议默认 [DEFAULT_PORT]）
        #[arg(long, default_value_t = default_probe_ports())]
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
            ui::ui_status(&out).await?;
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
