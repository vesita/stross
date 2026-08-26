// Stross 前端 —— 全局状态与类型（script 全局作用域共享，勿加 import/export）。
//
// 界面模型（设备 × 共享流 组合管理）：
//   · 设备（Device）是实体：本机 + 局域网发现/手动添加的设备；
//   · 共享流（Share）是设备之间的连接实例：方向（出站共享 / 入站接收）、
//     媒体（屏幕/摄像头/麦克风/系统声）、对端（广播或具体设备）、状态。
// 左栏设备列表发起共享，右栏共享流统一管理（含停止）。

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
  /** SRT / QUIC 拨号地址（/api/info 拉取到后填充；null = 不可用）。 */
  srtUrl: string | null;
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
/** 一个可接收的中继（本机锚点 / 局域网设备）。 */
interface TargetRelay {
  /** WS 基址（ws://host:port）。 */
  wsBase: string;
  /** SRT / QUIC 拨号地址（null = 不可用）。 */
  srtUrl: string | null;
  quicUrl: string | null;
}
type VideoSource =
  | { kind: 'screen' }
  | { kind: 'camera'; device: string | null }
  | { kind: 'synthetic'; pattern: string };
interface StreamConfig {
  streamId: string;
  title: string;
  /** null = 纯音频推流（B2 手机麦克风反向推流）。 */
  video: VideoSource | null;
  quality: Quality;
  audio: { mic: string | null; systemAudio: string | null; sampleRate: number; channels: number; bitrateKbps: number } | null;
  durationSecs: number | null;
  /** 一次性接入凭证（跨设备推流到对方受控中继的 ShareToken JSON；本机推流为 null）。 */
  shareToken: string | null;
}

/** 电脑端签发的手机麦克风接入凭证（Rust `issue_share_token` 返回值）。 */
interface ShareTokenView {
  /** ShareToken JSON 字符串（手机端原样粘贴到「共享麦克风」）。 */
  token: string;
  streamId: string;
  pin: string;
  expiresAt: number;
}

// —— 权限自动化（B2.5：凭证自动协商 + 防火墙） ——

/** 协商端点固定端口（与 Rust `stross_app::DEFAULT_NEGOTIATOR_PORT` 一致）。 */
const NEGOTIATOR_PORT = 18779;

/** 本机持久化身份（Rust `device_identity` 返回值）。 */
interface DeviceIdentity {
  deviceId: string;
  deviceName: string;
}

/** 协商签发的接入凭证（Rust `ShareGrant`：ShareTokenView + trusted）。 */
interface ShareGrant {
  token: string;
  streamId: string;
  pin: string;
  expiresAt: number;
  /** 是否因设备受信任而自动签发（未人工确认）。 */
  trusted: boolean;
}

/** 待人工确认的挂起请求（Rust `PendingRequest`，经 `negotiator-request` 事件送达）。 */
interface PendingRequest {
  id: string;
  deviceId: string;
  deviceName: string;
  /** 序列化后的媒体名（camelCase）。 */
  media: string[];
  createdAt: number;
}

/** 防火墙自检结果（Rust `firewall_status`，仅 Linux 桌面）。 */
interface FirewallStatus {
  ufwActive: boolean;
  defaultDenyIncoming: boolean;
  rules: { portProto: string; from: string }[];
  /** 缺失放行的 `port/proto`（空 = 已就绪）。 */
  missing: string[];
}

/** 设备实体（左栏卡片）：本机或局域网发现/手动添加的设备。 */
interface DeviceView {
  /** 唯一键：'local' 或设备 http://host:port 基址。 */
  key: string;
  name: string;
  /** 展示用 meta（IP:端口 等）。 */
  meta: string;
  isLocal: boolean;
  /** mDNS 角色（仅非本机；手动添加为空）。 */
  roles: string[];
  /** 手动添加（无 mDNS 角色信息）。 */
  manual: boolean;
  /** 非本机设备基址 http://host:port；本机为 null。 */
  base: string | null;
  /** 设备 SRT/QUIC 拨号地址（/api/info 拉取；null = 不可用）。 */
  srtUrl: string | null;
  quicUrl: string | null;
  /** 该设备在线共享流（点流即接收）。 */
  streams: RemoteStream[];
}

/** 共享媒体类型（与设备能力徽标一致）。 */
type ShareMedia = 'screen' | 'camera' | 'mic' | 'systemAudio';

/** 活动共享流条目（右栏统一管理；方向 × 媒体 × 对端）。 */
interface ShareItem {
  /** 唯一 id（流 id / 会话 id）。 */
  id: string;
  /** out = 本机共享出去（广播或定向）；in = 本机从对端接收。 */
  direction: 'out' | 'in';
  media: ShareMedia;
  /** 对端展示：'局域网广播' / 设备名。 */
  target: string;
  /** starting / live / error。 */
  state: 'starting' | 'live' | 'error';
  /** 状态详情（统计/错误）。 */
  detail: string;
}

let devices: DeviceList = { cameras: [], audioInputs: [], systemAudio: [] };
/** 是否正在启动共享（Android 采集启动中，等待 capture_status 真实回报）。 */
let starting = false;
let startingSince = 0;
const START_TIMEOUT_MS = 60000;
/** 本机锚点（免先连：init 自动 `start_relay`；共享/级联兜底的数据面入口）。 */
let anchor: Anchor | null = null;
/** 手动添加的设备地址（http://host:port，免 mDNS；与最近历史共享持久化）。 */
let manualRelays: string[] = [];
/** 当前接收目标中继（点选设备的锚点；null = 本机锚点）。 */
let targetRelay: TargetRelay | null = null;
/** 设备列表（本机 + 局域网设备；渲染左栏）。 */
let deviceViews: DeviceView[] = [];
/** 当前选中的设备 key（展开态保持）。 */
let expandedDevice: string | null = null;
/** 流 id → 流信息缓存（接收传输自动选择按 video/audio 类型决策）。 */
const remoteStreams = new Map<string, RemoteStream>();

let IS_ANDROID = false;
let MY_IPS: string[] = [];

// —— 交互状态 ——
let statusTimer: number | null = null; // 状态轮询句柄（应用打开期间常驻）
let scanInFlight = false;              // 「扫描设备」in-flight
let discoverInFlight = false;          // 设备流聚合 in-flight
let discoverCacheAt = 0;
const DISCOVER_TTL_MS = 5000;

// —— 共享（出站）状态 ——
let streaming = false;                 // 本机广播/定向共享是否进行中（流层）
let shareKind: ShareMedia | null = null; // 当前广播共享的媒体类型（屏幕/麦克风）
let streamInfo: { streamId: string; title: string; startedAt: number } | null = null;

// —— 接收（入站）状态 ——
let receiving = false;
let recvFrameCount = 0;
let recvAudioBlocks = 0; // 纯音频流（B2）：收到音频块即视为"有数据"
let recvError: string | null = null;
let recvUnlisten: (() => void) | null = null;

// —— B2 反向外设状态 ——
/** 手机端「共享麦克风」目标设备与推流状态（null = 未打开弹窗）。 */
let micShare: { base: string; quicPort: number | null; active: boolean } | null = null;
/** 最近一次凭证共享的目标设备基址（重开弹窗时恢复进行中状态用）。 */
let micShareLastBase: string | null = null;
/** 电脑端「接收手机麦克风」凭证与接入轮询状态（null = 未签发）。 */
let micRecv: { streamId: string; checking: boolean; received: boolean } | null = null;

/** 角色英文 → 中文显示（mDNS TXT `roles`）。 */
const ROLE_LABELS: Record<string, string> = {
  sender: '共享',
  viewer: '接收',
  relay: '中继',
};