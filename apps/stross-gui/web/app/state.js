"use strict";
// Stross 前端 —— 全局状态与类型（script 全局作用域共享，勿加 import/export）。
//
// 拆分说明：原单体 app.ts 按域拆为 state / ui / watch / grid / send / main，
// 保持无模块语法的 script 语义（浏览器多个 <script> 共享全局词法环境，
// tsc files 列表整体编译、类型跨文件可见），因此不引入 bundler 依赖。
const QUALITIES = {
    LOW: { width: 640, height: 360, fps: 24, bitrateKbps: 800 },
    MEDIUM: { width: 1280, height: 720, fps: 30, bitrateKbps: 2500 },
    HIGH: { width: 1920, height: 1080, fps: 30, bitrateKbps: 6000 },
};
const LS_RELAY = 'stross.lastRelay';
const LS_TITLE = 'stross.lastTitle';
const LS_RECENT = 'stross.recentRelays';
let devices = { cameras: [], audioInputs: [], systemAudio: [] };
let running = false;
let starting = false; // Android 采集启动中（等待真实状态回报）
let startingSince = 0; // 启动开始时间戳（超时兜底用）
const START_TIMEOUT_MS = 60000; // 采集启动超时
/** 本机锚点（免先连：init 自动 `start_relay`；推流/级联兜底的数据面入口）。 */
let anchor = null;
/** 自动发现卡片选中的接收目标中继（null = 本机锚点）。 */
let targetRelay = null;
/** 网格页选中的设备（relayBase 键；只看该设备的串流，null = 全部设备）。 */
let selectedDevice = null;
/** 手动添加的设备地址（http://host:port，免 mDNS；与最近历史共享持久化）。 */
let manualRelays = [];
/** 流 id → 流信息缓存（传输自动选择按 video/audio 类型决策）。 */
const remoteStreams = new Map();
let currentTab = 'grid';
let IS_ANDROID = false;
let MY_IPS = [];
// —— 交互状态 ——
let statusTimer = null; // 状态轮询句柄（应用打开期间常驻）
let scanInFlight = false; // 「扫描局域网设备」in-flight
let discoverInFlight = false; // 「扫描局域网串流」in-flight
let discoverCacheAt = 0; // 发现结果缓存时间（TTL 防重复扫描）
const DISCOVER_TTL_MS = 5000;
let streamsCache = null; // 本机锚点串流列表缓存
const STREAMS_TTL_MS = 3000;
// —— 接收状态 ——
let receiving = false;
let recvFrameCount = 0;
let recvUnlisten = null;
/** 角色英文 → 中文显示（mDNS TXT `roles`）。 */
const ROLE_LABELS = {
    sender: '推流',
    viewer: '观看',
    relay: '中继',
};
