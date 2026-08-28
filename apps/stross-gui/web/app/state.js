"use strict";
// Stross 前端 —— 全局状态与类型（script 全局作用域共享，勿加 import/export）。
//
// 界面模型（设备 × 共享流 组合管理）：
//   · 设备（Device）是实体：本机 + 局域网发现/手动添加的设备；
//   · 共享流（Share）是设备之间的连接实例：方向（出站共享 / 入站接收）、
//     媒体（屏幕/摄像头/麦克风/系统声）、对端（广播或具体设备）、状态。
// 左栏设备列表发起共享，右栏共享流统一管理（含停止）。
const QUALITIES = {
    LOW: { width: 640, height: 360, fps: 24, bitrateKbps: 800 },
    MEDIUM: { width: 1280, height: 720, fps: 30, bitrateKbps: 2500 },
    HIGH: { width: 1920, height: 1080, fps: 30, bitrateKbps: 6000 },
};
const LS_RELAY = 'stross.lastRelay';
const LS_TITLE = 'stross.lastTitle';
const LS_RECENT = 'stross.recentRelays';
// ---------------------------------------------------------------------------
// 单一状态源：全部运行时状态集中在此，各域文件显式读写；
// 渲染是状态（state）的纯函数，消灭「散文件 let 全局变量 + 脆弱互斥拼真相」。
// ---------------------------------------------------------------------------
/** 运行平台 / 环境。 */
let IS_ANDROID = false;
/** 本机采集设备（相机/音频输入/系统声；由 list_devices 填充）。 */
let devices = { cameras: [], audioInputs: [], systemAudio: [] };
/** 本机锚点（免先连：init 自动 `start_relay`；推流/级联兜底的数据面入口）。 */
let anchor = null;
/** 手动添加的设备地址（http://host:port，免 mDNS；与最近历史共享持久化）。 */
let manualRelays = [];
/** 设备列表（本机 + 局域网设备；渲染左栏）。 */
let deviceViews = [];
/** 当前选中的设备 key（展开态保持）。 */
let expandedDevice = null;
/** 流 id → 流信息缓存（接收传输自动选择按 video/audio 类型决策）。 */
const remoteStreams = new Map();
/** 本机在线共享缓存（供本机卡片流区渲染）。 */
let localStreams = [];
// —— 交互状态（轮询句柄 / in-flight 防重入） ——
let statusTimer = null; // 状态轮询句柄（应用打开期间常驻）
let scanInFlight = false; // 「扫描设备」in-flight
let discoverInFlight = false; // 设备流聚合 in-flight
let discoverCacheAt = 0;
const DISCOVER_TTL_MS = 5000;
// —— 发布（出站共享）状态 ——
/** 本机共享是否进行中（流层：广播/定向共用同一推流引擎）。 */
let publishing = false;
/** Android 采集启动中（等待 capture_status 真实回报）。 */
let publishStarting = false;
let publishStartingSince = 0;
const START_TIMEOUT_MS = 60000;
/** 当前共享的媒体类型（屏幕/麦克风）。 */
let shareKind = null;
/** 当前共享的流元信息。 */
let publishInfo = null;
/** B2 定向共享（手机麦克风 → 目标设备；null = 未打开弹窗）。 */
let micShare = null;
/** 最近一次凭证共享的目标设备基址（重开弹窗时恢复进行中状态用）。 */
let micShareLastBase = null;
/** 电脑端「接收手机麦克风」凭证与接入轮询状态（null = 未签发）。 */
let micRecv = null;
// —— 订阅（入站接收）状态 ——
/** 本机是否正在接收（订阅）流。 */
let receiving = false;
/** 当前订阅中的流 id（供共享面板定位流信息）。 */
let recvStreamId = null;
let recvFrameCount = 0;
let recvAudioBlocks = 0; // 纯音频流（B2）：收到音频块即视为"有数据"
let recvError = null;
let recvUnlisten = null;
/** 当前接收目标中继（点选设备的锚点；null = 本机锚点）。 */
let targetRelay = null;
// —— 协商 / 授权状态 ——
/** 当前等待人工确认的协商请求（negotiator-request 事件送达；null = 无）。 */
let pendingApprove = null;
/** 角色英文 → 中文显示（mDNS TXT `roles`）。 */
const ROLE_LABELS = {
    sender: '共享',
    viewer: '接收',
    relay: '中继',
};
/** 共享媒体 → 中文显示。 */
const MEDIA_LABELS = {
    screen: '屏幕',
    camera: '摄像头',
    mic: '麦克风',
    systemAudio: '系统声',
};
