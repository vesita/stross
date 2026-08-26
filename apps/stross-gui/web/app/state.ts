// Stross 前端 —— 全局状态与类型（script 全局作用域共享，勿加 import/export）。
//
// 拆分说明：原单体 app.ts 按域拆为 state / ui / watch / grid / send / main，
// 保持无模块语法的 script 语义（浏览器多个 <script> 共享全局词法环境，
// tsc files 列表整体编译、类型跨文件可见），因此不引入 bundler 依赖。

/** Tauri invoke 的弱类型契约（与 Rust 命令面逐步收紧）。 */
type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<any>;

// 与 Rust 端 Quality 预设保持一致
interface Quality { width: number; height: number; fps: number; bitrateKbps: number; }
const QUALITIES: Record<string, Quality> = {
  LOW: { width: 640, height: 360, fps: 24, bitrateKbps: 800 },
  MEDIUM: { width: 1280, height: 720, fps: 30, bitrateKbps: 2500 },
  HIGH: { width: 1920, height: 1080, fps: 30, bitrateKbps: 6000 },
};

const LS_RELAY = 'stross.lastRelay';
const LS_TITLE = 'stross.lastTitle';
const LS_RECENT = 'stross.recentRelays';

interface CameraDevice { id: string; name: string; }
interface DeviceList { cameras: CameraDevice[]; audioInputs: string[]; systemAudio: string[]; }
/** 本机锚点（`start_relay` 启动的受控中继 + mDNS 广播；推流/级联兜底的数据面入口）。 */
interface Anchor {
  port: number;
  /** http://ip:port 入口地址（供其它设备连接）。 */
  urls: string[];
  /** SRT 拨号地址（/api/info 拉取到后填充；null = 不可用）。 */
  srtUrl: string | null;
  /** QUIC 拨号地址（/api/info 拉取到后填充；null = 不可用）。 */
  quicUrl: string | null;
}
interface AppInfo { version: string; platform: string; ffmpeg: boolean; ips: string[]; }
interface RelayInfo {
  port: number;
  urls: string[];
  /** mDNS TXT 设备名（本机中继时为 null）。 */
  name: string | null;
  kind: string | null;
  roles: string[];
  transports: string[];
  ip: string | null;
}
interface StartResult { relayPort: number; watchUrls: string[]; streamId: string; }
interface StreamStatus {
  running: boolean; streamId: string | null; title: string | null;
  relayPort: number | null; startedAt: number | null;
}
interface CaptureStatus { active: boolean; started: boolean; error: string | null; }
interface ReceiveStats {
  running: boolean; received: number; decodedVideo: number;
  audioBlocks: number; dropped: number; error: string | null;
}
interface TrackInfo {
  codec: string;
  width: number | null;
  height: number | null;
  fps: number | null;
  sampleRate: number | null;
  channels: number | null;
}
interface RemoteStream {
  streamId: string;
  title: string;
  watchers: number;
  video: TrackInfo | null;
  audio: TrackInfo | null;
}
/** 一个可接收的中继（已连接中继，或自动发现卡片选中的局域网中继）。 */
interface TargetRelay {
  /** WS 基址（ws://host:port）。 */
  wsBase: string;
  /** SRT 拨号地址（null = 不可用）。 */
  srtUrl: string | null;
  /** QUIC 拨号地址（null = 不可用）。 */
  quicUrl: string | null;
}
type VideoSource =
  | { kind: 'screen' }
  | { kind: 'camera'; device: string | null }
  | { kind: 'synthetic'; pattern: string };
interface StreamConfig {
  streamId: string;
  title: string;
  video: VideoSource;
  quality: Quality;
  audio: { mic: string | null; systemAudio: string | null; sampleRate: number; channels: number; bitrateKbps: number } | null;
  durationSecs: number | null;
}

/** 设备卡片数据（mDNS 或手动添加）。 */
interface DeviceCard {
  /** http://host:port 归一化基址（设备筛选键）。 */
  base: string;
  name: string;
  meta: string;
  roles: string[];
  /** 手动添加（无 mDNS 角色信息）。 */
  manual: boolean;
}

let devices: DeviceList = { cameras: [], audioInputs: [], systemAudio: [] };
let running = false;
let starting = false; // Android 采集启动中（等待真实状态回报）
let startingSince = 0; // 启动开始时间戳（超时兜底用）
const START_TIMEOUT_MS = 60000; // 采集启动超时
/** 本机锚点（免先连：init 自动 `start_relay`；推流/级联兜底的数据面入口）。 */
let anchor: Anchor | null = null;
/** 自动发现卡片选中的接收目标中继（null = 本机锚点）。 */
let targetRelay: TargetRelay | null = null;
/** 网格页选中的设备（relayBase 键；只看该设备的串流，null = 全部设备）。 */
let selectedDevice: string | null = null;
/** 手动添加的设备地址（http://host:port，免 mDNS；与最近历史共享持久化）。 */
let manualRelays: string[] = [];
/** 流 id → 流信息缓存（传输自动选择按 video/audio 类型决策）。 */
const remoteStreams = new Map<string, RemoteStream>();
let currentTab: 'grid' | 'send' | 'watch' = 'grid';
let IS_ANDROID = false;
let MY_IPS: string[] = [];

// —— 交互状态 ——
let statusTimer: number | null = null; // 状态轮询句柄（应用打开期间常驻）
let scanInFlight = false; // 「扫描局域网设备」in-flight
let discoverInFlight = false; // 「扫描局域网串流」in-flight
let discoverCacheAt = 0; // 发现结果缓存时间（TTL 防重复扫描）
const DISCOVER_TTL_MS = 5000;
let streamsCache: { at: number; list: RemoteStream[] } | null = null; // 本机锚点串流列表缓存
const STREAMS_TTL_MS = 3000;

// —— 接收状态 ——
let receiving = false;
let recvFrameCount = 0;
let recvUnlisten: (() => void) | null = null;

/** 角色英文 → 中文显示（mDNS TXT `roles`）。 */
const ROLE_LABELS: Record<string, string> = {
  sender: '推流',
  viewer: '观看',
  relay: '中继',
};
