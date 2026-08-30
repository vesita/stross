// Stross 前端 —— 类型与共享字符串定义（script 全局作用域共享，勿加 import/export）。
//
// 本文件是**唯一**的类型/字符串定义源：所有 interface、标签映射（*_LABELS）、
// 字符串字面量联合与 wire 键常量集中在此，各域文件只消费不重定义
// （docs/layering-architecture.md：前端不持有端口等常量，wire 形状以 Rust 为真源，
// 这里仅做类型镜像 + 展示标签）。加载顺序：types.js 必须先于其它 app/*.js。

/** Tauri invoke 的弱类型契约（与 Rust 命令面逐步收紧）。 */
type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<any>;

// ---------------------------------------------------------------------------
// 字符串字面量联合（与 Rust 枚举 wire 值一一对应；单一真源）
// ---------------------------------------------------------------------------

/** 设备/端点种类（Rust `MediaKind` wire 值：camelCase）。 */
type EndpointKind = 'screen' | 'window' | 'camera' | 'mic' | 'systemAudio' | 'input' | 'clipboard' | 'file' | 'service';
/** 可见性（Rust `Visibility` wire 值）。 */
type VisibilityKind = 'public' | 'confirm' | 'private';
/** 数据面方向（Rust `Delivery` wire 值）。 */
type DeliveryKind = 'pull' | 'push' | 'both';
/** 角色（Rust `RoleId` wire 值）。 */
type RoleKind = 'sender' | 'viewer' | 'relay';
/** 共享媒体类型（与设备能力徽标一致；端点 kind 的实时媒体子集）。 */
type ShareMedia = Extract<EndpointKind, 'screen' | 'camera' | 'mic' | 'systemAudio'>;

// ---------------------------------------------------------------------------
// 标签映射（wire 值 → 中文展示；未知值回退原文）
// ---------------------------------------------------------------------------

/** 可见性中文显示。 */
const VISIBILITY_LABELS: Record<VisibilityKind, string> = {
  public: '公开',
  confirm: '需确认',
  private: '私密',
};

/** delivery 中文显示。 */
const DELIVERY_LABELS: Record<DeliveryKind, string> = {
  pull: '拉取',
  push: '推送',
  both: '双向',
};

/** 设备/端点种类中文显示。 */
const DEVICE_KIND_LABELS: Record<EndpointKind, string> = {
  screen: '屏幕',
  window: '窗口',
  camera: '摄像头',
  mic: '麦克风',
  systemAudio: '系统声',
  input: '输入',
  clipboard: '剪贴板',
  file: '文件',
  service: '服务',
};

/** 角色英文 → 中文显示（mDNS TXT `roles`）。 */
const ROLE_LABELS: Record<RoleKind, string> = {
  sender: '共享',
  viewer: '接收',
  relay: '中继',
};

/** 查标签：未知 wire 值回退原文（后端枚举可能先于前端扩展）。 */
function labelOf<T extends string>(map: Record<T, string>, key: string): string {
  return map[key as T] || key;
}

// ---------------------------------------------------------------------------
// localStorage 键（与 Rust 无关的前端持久化常量）
// ---------------------------------------------------------------------------

const LS_RELAY = 'stross.lastRelay';
const LS_TITLE = 'stross.lastTitle';
const LS_RECENT = 'stross.recentRelays';

// ---------------------------------------------------------------------------
// 与 Rust 端 Quality 预设保持一致（镜像 stross_media::pipeline 预设）
// ---------------------------------------------------------------------------

interface Quality { width: number; height: number; fps: number; bitrateKbps: number; }
const QUALITIES: Record<string, Quality> = {
  LOW: { width: 640, height: 360, fps: 24, bitrateKbps: 800 },
  MEDIUM: { width: 1280, height: 720, fps: 30, bitrateKbps: 2500 },
  HIGH: { width: 1920, height: 1080, fps: 30, bitrateKbps: 6000 },
};

// ---------------------------------------------------------------------------
// 端点种类 → 图标名（雪碧图）；与 DEVICE_KIND_LABELS 共用 EndpointKind 键
// ---------------------------------------------------------------------------

/** 设备类型 → 图标名（雪碧图）。 */
const KIND_ICONS: Partial<Record<EndpointKind, string>> = {
  screen: 'monitor',
  window: 'monitor',
  camera: 'camera',
  mic: 'mic',
  systemAudio: 'speaker',
  file: 'download',
};

/** 设备类型 → 图标名（未知类型回退 server）。 */
function deviceKindIcon(kind: string): string {
  return KIND_ICONS[kind as EndpointKind] || 'server';
}

// ---------------------------------------------------------------------------
// 线协议 / 命令面类型镜像（Rust serde camelCase；字段名 = wire 键）
// ---------------------------------------------------------------------------

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
/** 应用设置（`discoverable_status` / `set_discoverable`）。 */
interface Settings { discoverable: boolean; }
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
  audioBlocks: number; dropped: number;
  pacedDropped: number; pacedReanchors: number; pacedHeld: number;
  error: string | null;
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
  /** 是否含视频/音频轨：布尔投影（Rust `StreamView`）；兼容历史 /api/streams
   *  直读的 TrackInfo 对象（真值判断两者等价）。 */
  video: boolean | TrackInfo | null;
  audio: boolean | TrackInfo | null;
}

/** L1 端点摘要（Rust `EndpointSummary`，mDNS 摘要层：id/kind/name/可挂载/已通告）。
 *  字段是 EndpointManifest 的子集——用 Pick 派生，避免双份定义漂移。 */
type L1EndpointSummary = Pick<EndpointManifest, 'endpointId' | 'kind' | 'name' | 'available' | 'published'>;

/** 扫描聚合视图（Rust `stross_app::devices::ScannedDevice`——mDNS + 探测
 *  聚合全在库层，前端只消费结果不再自写 /api/* 探测）。 */
interface ScannedDevice {
  name: string;
  ip: string;
  port: number;
  isSelf: boolean;
  roles: string[];
  media: string[];
  transports: string[];
  endpoints: L1EndpointSummary[];
  online: boolean;
  srtPort: number | null;
  quicPort: number | null;
  streams: RemoteStream[];
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
  /** null = 纯音频推流。 */
  video: VideoSource | null;
  quality: Quality;
  audio: { mic: string | null; systemAudio: string | null; sampleRate: number; channels: number; bitrateKbps: number } | null;
  durationSecs: number | null;
  /** 一次性接入凭证（跨设备推流到对方受控中继的 ShareToken JSON；本机推流为 null）。 */
  shareToken: string | null;
}

/** 协商接入凭证视图（Rust `ShareGrant` 的超集；端点订阅握手载荷/契约镜像）。 */
interface ShareTokenView {
  /** ShareToken JSON 字符串（推流端接入受控中继时出示）。 */
  token: string;
  streamId: string;
  pin: string;
  expiresAt: number;
}

/** 协商签发的接入凭证（Rust `ShareGrant`：ShareTokenView + trusted）。 */
interface ShareGrant extends ShareTokenView {
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

// —— 端点框架（节点 → 端点；docs/endpoint-model.md） ——

/** 端点清单（单层端点模型，Rust `EndpointManifest` 平铺：可挂载性 + 通告状态）。 */
interface EndpointManifest {
  endpointId: string;
  kind: string;
  name: string;
  /** load 探测结果：能否被挂载成节点（false = 不可通告、不可订阅）。 */
  available: boolean;
  /** load/share 失败原因（不可用时展示）。 */
  lastError: string | null;
  /** 是否已通告（未通告 = 仅本机可见）。 */
  published: boolean;
  visibility: string;
  delivery: string;
  transports: { transport: string; priority: number }[];
  codecs: string[];
  state: string;
  subscribers: number;
  updatedAt: number;
}

/** 本机目录（Rust `local_catalog`：全部端点，含未通告与不可挂载）。 */
interface LocalCatalog {
  endpoints: EndpointManifest[];
}

/** L2 目录（Rust `EndpointDir`：节点 + 已通告端点；服务端已滤 Private）。 */
interface RemoteDir {
  node: { deviceId: string; deviceName: string };
  endpoints: EndpointManifest[];
}

/** 媒体端点订阅结果（Rust `MediaSubscribeOutcome`：watch 入口 + 流 id）。 */
interface MediaSubscribeOutcome {
  delivery: string;
  relayUrl: string;
  streamId: string;
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
  /** 设备 SRT/QUIC 拨号地址（扫描聚合带出；null = 不可用）。 */
  srtUrl: string | null;
  quicUrl: string | null;
  /** 设备 QUIC 端口（协商/推流域选传输用，来自扫描；null = 不可用）。 */
  quicPort: number | null;
  /** 该设备在线共享流（点流即接收）。 */
  streams: RemoteStream[];
}
