// Stross 前端 —— 全局运行时状态（script 全局作用域共享，勿加 import/export）。
//
// 界面模型（设备 × 共享流 组合管理）：
//   · 设备（Device）是实体：本机 + 局域网发现/手动添加的设备；
//   · 共享流（Share）是设备之间的连接实例：方向（出站共享 / 入站接收）、
//     媒体（屏幕/摄像头/麦克风/系统声）、对端（广播或具体设备）、状态。
// 左栏设备列表发起共享，右栏共享流统一管理（含停止）。
//
// 类型与标签映射见 types.ts（本文件只放运行时可变状态 + 少量状态常量；
// 各域文件显式读写，渲染是状态（state）的纯函数）。

// —— 交互状态（轮询句柄 / in-flight 防重入） ——
let statusTimer: number | null = null; // 状态轮询句柄（应用打开期间常驻）
let scanInFlight = false;              // 「扫描设备」in-flight
let discoverInFlight = false;          // 设备流聚合 in-flight
let discoverCacheAt = 0;
const DISCOVER_TTL_MS = 3000;

// —— 移动端选项卡状态 ——
let currentMobileTab: MobileTab = 'devices';

// —— 订阅（入站接收）状态 ——
/** 本机是否正在接收（订阅）流（= recvLinks 非空；多端点链接）。 */
let receiving = false;
/** 接收链路表（多端点链接：一次可同时接收多条流，如屏幕 + 系统声音同播；
 *  每条链独立启停/统计，停一条不级联其它链）。key = link_id。 */
interface RecvLinkState {
  linkId: string;
  /** 展示名（设备 + 端点，如「手机A · 屏幕」）。 */
  name: string;
  streamId: string;
  /** 链路启动时刻（宽限期判定用：新链可能短暂 !running 连接窗口）。 */
  startedAt: number;
  /** 已接收视频帧数（桌面二进制 Channel / Android receive-frame 事件路由）。 */
  frames: number;
  /** 音频块数（receive_links 轮询回写）。 */
  audioBlocks: number;
  /** 显示帧率（前端绘制速率，poll 差分估算；0/未定 = 不显示）。 */
  displayFps?: number;
  /** 解码帧率（Rust `decodedVideo` 差分；与 displayFps 同屏对比——「解码高、
   *  显示低」即显示管线瓶颈的直观信号，第三十轮修复后两者应趋近）。 */
  decodeFps?: number;
  /** poll 差分游标（frames / decodedVideo 上次采样值）。 */
  lastFrames?: number;
  lastDecoded?: number;
  /** 已解码视频帧数（receive_links 轮询回写；Android Surface 路径据它判断
   *  「有画面」——canvas 像素路径不再有帧经 Channel 回传）。 */
  decodedVideo?: number;
  /** 上次 poll 时刻（fps 差分按实际间隔归一化）。 */
  lastPollAt?: number;
  /** 帧率滑动窗口（最近 4 次 poll 差分；平均后显示，差分 0 不覆盖历史）。 */
  fpsSamples?: number[];
  decodeSamples?: number[];
  status: 'starting' | 'live' | 'error' | 'ended';
  error: string | null;
}
const recvLinks = new Map<string, RecvLinkState>();
/** 画布当前显示的链路（最近收到视频帧的链路；纯音频链不占画面）。 */
let activeVideoLink: string | null = null;
/** 已订阅端点集合（`host:endpointId`；对端卡片「已订阅 · 接收中」态）。
 *  旧单变量 `subscribedEndpoint` 只记一条——多端点链接下须为集合。 */
const subscribedEndpoints = new Set<string>();
/** 当前接收目标中继（点选设备的锚点；null = 本机锚点）。 */
let targetRelay: TargetRelay | null = null;

// —— 播放器（接收画面）状态 ——
/** 播放器全屏状态（Tauri 窗口级全屏；前端自维护，切换前经 isFullscreen 校准）。 */
let fsActive = false;

// —— 协商 / 授权状态 ——
/** 当前等待人工确认的协商请求（negotiator-request 事件送达；null = 无）。 */
let pendingApprove: PendingRequest | null = null;

// —— 端点框架状态（节点 → 端点） ——
/** 本机目录（端点清单；local_catalog 填充，渲染本机端点树）。 */
let localCatalog: LocalCatalog = { endpoints: [] };
/** 共享弹窗目标（null = 未打开）。 */
let publishTarget: { ep: EndpointManifest } | null = null;
/** 订阅弹窗目标（远端端点；null = 未打开）。 */
let subscribeTarget: { host: string; ep: EndpointManifest } | null = null;
/** 正在订阅的端点（握手进行中）；「订阅」键显示「正在订阅…」进行态，防重复点击。 */
let subscribingEndpoint: { host: string; endpointId: string } | null = null;
/** 远端目录缓存（设备 base → RemoteDir；TTL 内命中直接渲染）。 */
const remoteDirs = new Map<string, RemoteDir>();
/** 远端目录缓存时间戳（TTL ~20s：对端新共享/取消共享及时可见）。 */
const remoteDirAt = new Map<string, number>();
/** 远端目录拉取中（按设备 base；防重入）。 */
const remoteDirLoading = new Set<string>();

// —— 设备 / 锚点 / 采集 ——

/** 运行平台 / 环境。 */
let IS_ANDROID = false;

/** 本机采集设备（相机/音频输入/系统声；由 list_devices 填充）。 */
let devices: DeviceList = { cameras: [], audioInputs: [], systemAudio: [] };

/** 本机锚点（免先连：init 自动 `start_relay`；推流/级联兜底的数据面入口）。 */
let anchor: Anchor | null = null;
/** 手动添加的设备地址（http://host:port，免 mDNS；与最近历史共享持久化）。 */
let manualRelays: string[] = [];

/** 设备列表（本机 + 局域网设备；渲染左栏）。 */
let deviceViews: DeviceView[] = [];
/** 当前选中的设备 key（展开态保持）。 */
let expandedDevice: string | null = null;
/** 流 id → 流信息缓存（接收传输自动选择按 video/audio 类型决策）。 */
const remoteStreams = new Map<string, RemoteStream>();
/** 设备搜索过滤关键词（为空时显示全部）。 */
let deviceFilterQuery = '';
/** 全局主视图模式（管理界面 Manage vs 消费播放台 Consume）。 */

// ===========================================================================
// UI 状态机（Finite State Machine for Predictable UI Lifecycle）
// ===========================================================================

type AppStage = 'idle' | 'managing' | 'streaming' | 'error';
type PlayerDisplayMode = 'empty' | 'buffering' | 'videoOnly' | 'audioOnly' | 'audioVisualMix';

interface UIStateMachineState {
  appStage: AppStage;
  playerMode: PlayerDisplayMode;
  viewMode: 'manage' | 'consume';
  aspectRatio: 'fit' | 'cover' | 'fill' | 'original';
  isFullscreen: boolean;
  activeModal: 'none' | 'publish' | 'subscribe' | 'diagnostics' | 'approve';
}

const uiFSM: UIStateMachineState = {
  appStage: 'idle',
  playerMode: 'empty',
  viewMode: 'manage',
  aspectRatio: 'fit',
  isFullscreen: false,
  activeModal: 'none',
};

type UIAction =
  | { type: 'SWITCH_VIEW'; mode: 'manage' | 'consume' }
  | { type: 'SET_ASPECT_RATIO'; mode: 'fit' | 'cover' | 'fill' | 'original' }
  | { type: 'SET_FULLSCREEN'; active: boolean }
  | { type: 'OPEN_MODAL'; modal: 'publish' | 'subscribe' | 'diagnostics' | 'approve' }
  | { type: 'CLOSE_MODAL' }
  | { type: 'SYNC_LINKS' };

type UIStateListener = (state: Readonly<UIStateMachineState>) => void;
const fsmListeners = new Set<UIStateListener>();

function subscribeUIFSM(listener: UIStateListener): () => void {
  fsmListeners.add(listener);
  listener(uiFSM);
  return () => fsmListeners.delete(listener);
}

function dispatchUIAction(action: UIAction): void {
  switch (action.type) {
    case 'SWITCH_VIEW':
      uiFSM.viewMode = action.mode;
      activeViewMode = action.mode;
      break;
    case 'SET_ASPECT_RATIO':
      uiFSM.aspectRatio = action.mode;
      break;
    case 'SET_FULLSCREEN':
      uiFSM.isFullscreen = action.active;
      fsActive = action.active;
      break;
    case 'OPEN_MODAL':
      uiFSM.activeModal = action.modal;
      break;
    case 'CLOSE_MODAL':
      uiFSM.activeModal = 'none';
      break;
    case 'SYNC_LINKS':
      break;
  }

  const total = recvLinks.size;
  receiving = total > 0;
  if (total === 0) {
    uiFSM.appStage = uiFSM.viewMode === 'consume' ? 'idle' : 'managing';
    uiFSM.playerMode = 'empty';
  } else {
    uiFSM.appStage = 'streaming';
    const activeVideo = activeVideoLink ? recvLinks.get(activeVideoLink) : null;
    const hasVideo = !!activeVideo && activeVideo.frames > 0;
    const hasAudio = Array.from(recvLinks.values()).some((l) => l.audioBlocks > 0);

    if (!hasVideo && !hasAudio) {
      uiFSM.playerMode = 'buffering';
    } else if (hasVideo && hasAudio) {
      uiFSM.playerMode = 'audioVisualMix';
    } else if (hasVideo) {
      uiFSM.playerMode = 'videoOnly';
    } else {
      uiFSM.playerMode = 'audioOnly';
    }
  }

  for (const fn of fsmListeners) {
    fn(uiFSM);
  }
}
let activeViewMode: 'manage' | 'consume' = 'manage';
