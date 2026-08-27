//! Stross 命令行工具：无头场景（本地双实例串流测试 / 脚本化 / 服务化）的入口。
//!
//! 子命令：
//! * [`relay`]   —— 启动局域网中继（等同 `stross-relay`）
//! * [`serve`]   —— Stross 常驻实例（内核 + 受控中继 + 控制面）
//! * [`ctrl`]    —— 接入运行中实例的控制面，异步下发控制命令（D7）
//! * [`push`]    —— 推流（内嵌中继，或推往外部中继）
//! * [`receive`] —— 接收并原生解码（`SessionDataManager` + `PlaybackSink`，D6）
//!
//! 本地双实例串流测试示例：
//!
//! ```text
//! stross push --port 18777 --stream-id demo --secs 6 --audio &   # 实例 A
//! stross receive --relay ws://127.0.0.1:18777 --stream demo --out /tmp/out --secs 4
//! ```
//!
//! 或者"常驻 + 接入控制"模型（D7）：
//!
//! ```text
//! stross serve --port 18777 &                                    # 实例常驻
//! stross ctrl create-session --title demo                        # → sessionId
//! stross ctrl start-stream --stream-id <sid> --secs 10 --audio   # 异步起流
//! stross ctrl events --secs 5                                    # 订阅事件
//! stross receive --relay ws://127.0.0.1:18777 --stream <sid> ...
//! ```

use clap::{Parser, Subcommand};

mod adb;
mod ctrl;
mod devices;
mod push;
mod receive;
mod relay;
mod serve;

#[derive(Parser, Debug)]
#[command(name = "stross", version, about = "Stross 局域网设备共享工具链")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 启动局域网中继（HTTP + WS 推/收 + SRT/QUIC 数据面）
    Relay(relay::RelayArgs),
    /// Stross 常驻实例：内核 + 受控中继 + 控制面（D7）
    Serve(serve::ServeArgs),
    /// 接入运行中实例的控制面，异步下发控制命令（仅回环，D7）
    Ctrl(ctrl::CtrlArgs),
    /// 扫描局域网设备（PC + 手机），展示能力与在线共享状态
    Devices(devices::DevicesArgs),
    /// 经 USB（adb）查看/操作已连接手机（局域网被隔离时的可靠通道）
    Adb(adb::AdbArgs),
    /// 推流：内嵌中继，或推往外部中继
    Push(push::PushArgs),
    /// 接收：WS 收流 → SessionDataManager → PlaybackSink 原生解码
    Receive(receive::ReceiveArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Relay(a) => relay::run(a).await,
        Command::Serve(a) => serve::run(a).await,
        Command::Ctrl(a) => ctrl::run(a).await,
        Command::Devices(a) => devices::run(a).await,
        Command::Adb(a) => adb::run(a).await,
        Command::Push(a) => push::run(a).await,
        Command::Receive(a) => receive::run(a).await,
    }
}
