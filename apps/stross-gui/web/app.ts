// Stross 推流端控制界面（Tauri 前端，TypeScript 源文件，唯一真源）。
//
// 生成 app.js：`npx tsc -p apps/stross-gui/web/tsconfig.json`
// （app.js 是构建产物，提交进仓库——Tauri 直接加载，Rust 构建不依赖 node）。
// 修改本文件后必须重新生成 app.js 并提交两者。
//
// 交互模型：先连接中继（本机或局域网），再选择「推流（发）」或「观看（收）」。

/** Tauri invoke 的弱类型契约（与 Rust 命令面逐步收紧）。 */
type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<any>;

const $ = (id: string): HTMLElement => document.getElementById(id) as HTMLElement;
const $input = (id: string): HTMLInputElement => $(id) as HTMLInputElement;
const $select = (id: string): HTMLSelectElement => $(id) as HTMLSelectElement;
const $btn = (id: string): HTMLButtonElement => $(id) as HTMLButtonElement;

const invoke: Invoke | undefined = (window as any).__TAURI__?.core?.invoke;

/** invoke 的安全封装：非 Tauri 环境下返回明确错误而非未定义调用。 */
function call(cmd: string, args?: Record<string, unknown>): Promise<any> {
  if (!invoke) return Promise.reject(new Error('当前页面未运行在 Stross 桌面应用中'));
  return invoke(cmd, args);
}

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
interface Connection { url: string; wsUrl: string; relayUrls: string[]; }
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
interface RemoteStream { streamId: string; title: string; watchers: number; }
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

let devices: DeviceList = { cameras: [], audioInputs: [], systemAudio: [] };
let running = false;
let starting = false; // Android 采集启动中（等待真实状态回报）
let startingSince = 0; // 启动开始时间戳（超时兜底用）
const START_TIMEOUT_MS = 60000; // 采集启动超时
let connection: Connection | null = null;
let currentTab: 'send' | 'watch' = 'send';
let IS_ANDROID = false;
let MY_IPS: string[] = [];

// ---------------------------------------------------------------- 初始化

async function init(): Promise<void> {
  if (!invoke) {
    showFatal('当前页面未运行在 Stross 桌面应用中。\n请通过 `cargo tauri dev` 或安装包启动。');
    return;
  }
  try {
    const info = (await call('app_info')) as AppInfo;
    IS_ANDROID = info.platform === 'android';
    MY_IPS = info.ips || [];
    $('ver-badge').textContent = 'v' + info.version;
    const fb = $('ffmpeg-badge');
    if (IS_ANDROID) {
      fb.textContent = '原生采集';
      fb.classList.add('ok');
      // Android：视频源固定为屏幕（MediaProjection），无系统声音采集
      $('video-seg-row').classList.add('hidden');
      $('android-video-note').classList.remove('hidden');
      $('sys-row').classList.add('hidden');
      $('mic-hint').textContent = '需要麦克风权限；拒绝则仅推流屏幕';
    } else if (info.ffmpeg) {
      fb.textContent = 'ffmpeg ✓';
      fb.classList.add('ok');
    } else {
      fb.textContent = '未检测到 ffmpeg';
      fb.classList.add('err');
    }
    renderIps(info.ips);
    restorePrefs();
    await loadDevices();
    // 打开即自动扫描局域网设备，免去手动输入地址
    void scanRelays();
  } catch (e) {
    showFatal(String(e));
  }
}

/** 恢复上次的连接地址/流名称偏好，并渲染最近连接历史。 */
function restorePrefs(): void {
  const last = localStorage.getItem(LS_RELAY);
  if (last) {
    $input('relay-addr').value = last;
    (document.querySelector('input[name="conn"][value="remote"]') as HTMLInputElement).checked = true;
    $('remote-row').classList.remove('hidden');
  }
  const title = localStorage.getItem(LS_TITLE);
  if (title) $input('title-input').value = title;
  renderRecent();
}

function savePrefs(): void {
  localStorage.setItem(LS_RELAY, $input('relay-addr').value.trim());
  localStorage.setItem(LS_TITLE, $input('title-input').value.trim());
}

// ---------------- 最近连接历史 ----------------

function getRecent(): string[] {
  try {
    return JSON.parse(localStorage.getItem(LS_RECENT) || '[]') as string[];
  } catch {
    return [];
  }
}

function saveRecent(url: string): void {
  const list = getRecent().filter((u) => u !== url);
  list.unshift(url);
  localStorage.setItem(LS_RECENT, JSON.stringify(list.slice(0, 5)));
}

/** 渲染"最近连接"列表，点击即填入并自动连接。 */
function renderRecent(): void {
  const list = getRecent();
  const block = $('recent-block');
  if (!list.length) {
    block.classList.add('hidden');
    return;
  }
  block.classList.remove('hidden');
  const ul = $('recent-list');
  ul.innerHTML = '';
  list.forEach((u) => {
    const li = document.createElement('li');
    li.textContent = u;
    li.title = '点击连接';
    li.onclick = () => {
      $input('relay-addr').value = u;
      (document.querySelector('input[name="conn"][value="remote"]') as HTMLInputElement).checked = true;
      $('remote-row').classList.remove('hidden');
      void connect();
    };
    ul.appendChild(li);
  });
}

// ---------------------------------------------------------------- 提示

function showFatal(msg: string): void {
  const box = $('error-box');
  box.textContent = msg;
  box.classList.remove('hidden');
}
function hideError(): void {
  $('error-box').classList.add('hidden');
}
function showConnectError(msg: string): void {
  const box = $('connect-error');
  box.textContent = msg;
  box.classList.remove('hidden');
}
function hideConnectError(): void {
  $('connect-error').classList.add('hidden');
}

// ---------------------------------------------------------------- 连接

function normAddr(addr: string): string | null {
  let a = addr.trim();
  if (!a) return null;
  if (!/^https?:\/\//i.test(a)) a = 'http://' + a;
  return a.replace(/\/+$/, '');
}

async function connect(): Promise<void> {
  hideConnectError();
  const mode = (document.querySelector('input[name="conn"]:checked') as HTMLInputElement).value;
  const btn = $btn('connect-btn');
  btn.disabled = true;
  btn.textContent = '连接中…';
  try {
    if (mode === 'local') {
      const info = (await call('start_relay')) as RelayInfo;
      connection = {
        url: `http://127.0.0.1:${info.port}`,
        wsUrl: `ws://127.0.0.1:${info.port}/ws/push`,
        relayUrls: info.urls,
      };
    } else {
      const addr = normAddr($input('relay-addr').value);
      if (!addr) {
        showConnectError('请输入中继地址，例如 http://192.168.1.100:8777');
        return;
      }
      savePrefs();
      saveRecent(addr);
      // 探测中继是否可达
      const resp = await fetch(addr + '/api/streams', { cache: 'no-store' });
      if (!resp.ok) throw new Error('中继返回 HTTP ' + resp.status);
      await resp.json();
      connection = { url: addr, wsUrl: addr.replace(/^http/, 'ws') + '/ws/push', relayUrls: [addr + '/'] };
    }
    enterApp();
  } catch (e) {
    const msg = (e as Error).message;
    const hint = msg.includes('Failed to fetch') || msg.includes('NetworkError')
      ? '无法访问该地址。请检查：地址是否正确、设备是否在同一局域网、中继是否启动、防火墙是否放行。'
      : '连接失败：' + msg;
    showConnectError(hint);
  } finally {
    btn.disabled = false;
    btn.textContent = '连接';
  }
}

function enterApp(): void {
  $('connect-view').classList.add('hidden');
  $('app-view').classList.remove('hidden');
  $('conn-badge').textContent = '已连接';
  $('conn-badge').classList.add('ok');
  $('disconnect-btn').classList.remove('hidden');
  $('tab-conn-label').textContent = '已连接：' + connection!.url;
  $('watch-relay-url').textContent = connection!.url;
  // 连接成功即可展示中继入口地址（供其它设备连接数据面）
  if (connection!.relayUrls && connection!.relayUrls.length) {
    renderUrls(connection!.relayUrls);
  }
  setTab('send');
  void loadRemoteStreams();
  void pollStatus();
}

function disconnect(): void {
  if (running || starting) {
    void stopStream();
  }
  connection = null;
  $('app-view').classList.add('hidden');
  $('connect-view').classList.remove('hidden');
  $('conn-badge').textContent = '未连接';
  $('conn-badge').classList.remove('ok');
  $('disconnect-btn').classList.add('hidden');
  void stopReceive();
  setRunning(false);
}

// ---------------------------------------------------------------- 模式切换

function setTab(tab: 'send' | 'watch'): void {
  currentTab = tab;
  $('tab-send-btn').classList.toggle('active', tab === 'send');
  $('tab-watch-btn').classList.toggle('active', tab === 'watch');
  $('tab-send').classList.toggle('hidden', tab !== 'send');
  $('tab-watch').classList.toggle('hidden', tab !== 'watch');
  if (tab === 'watch') void loadRemoteStreams();
}

// ---------------------------------------------------------------- 设备

async function loadDevices(): Promise<void> {
  devices = (await call('list_devices')) as DeviceList;
  fillSelect($select('camera-select'), devices.cameras.map((c) => ({ value: c.id, label: c.name })), '使用默认摄像头');
  fillSelect($select('mic-select'), devices.audioInputs.map((n) => ({ value: n, label: n })), '系统默认输入');
  fillSelect($select('sys-select'), devices.systemAudio.map((n) => ({ value: n, label: n })), '未发现回环设备');
  $('mic-hint').textContent = devices.audioInputs.length ? '' : '未发现麦克风（仍会使用系统默认输入）';
  $('sys-hint').textContent = devices.systemAudio.length
    ? ''
    : '未发现回环设备（Linux 需 PulseAudio monitor；Windows 需启用"立体声混音"）';
}

function fillSelect(sel: HTMLSelectElement, items: { value: string; label: string }[], emptyLabel: string): void {
  sel.innerHTML = '';
  if (!items.length) {
    const o = document.createElement('option');
    o.value = '';
    o.textContent = emptyLabel;
    sel.appendChild(o);
    return;
  }
  for (const it of items) {
    const o = document.createElement('option');
    o.value = it.value;
    o.textContent = it.label;
    sel.appendChild(o);
  }
}

function renderIps(ips: string[]): void {
  const ul = $('ip-list');
  ul.innerHTML = '';
  ips.forEach((ip) => {
    const li = document.createElement('li');
    li.textContent = ip;
    li.title = '点击填入中继地址';
    li.onclick = () => {
      (document.querySelector('input[name="conn"][value="remote"]') as HTMLInputElement).checked = true;
      $('remote-row').classList.remove('hidden');
      $input('relay-addr').value = `http://${ip}:8777`;
    };
    ul.appendChild(li);
  });
  if (!ips.length) ul.innerHTML = '<li class="hint">未获取到局域网 IP</li>';
}

// ---------------------------------------------------------------- 扫描局域网

/** 角色英文 → 中文显示（mDNS TXT `roles`）。 */
const ROLE_LABELS: Record<string, string> = {
  sender: '推流',
  viewer: '观看',
  relay: '中继',
};

function roleLabel(r: string): string {
  return ROLE_LABELS[r] || r;
}

/** 扫描局域网内其它设备（mDNS）；打开应用时自动执行一次，也可手动重扫。 */
async function scanRelays(): Promise<void> {
  const box = $('scan-results');
  box.classList.remove('hidden');
  box.innerHTML = '<p class="hint">扫描中（2 秒）…</p>';
  try {
    const relays = (await call('scan_relays')) as RelayInfo[];
    // 剔除本机（本机中继走「🖥️ 本机」选项）
    const others = relays.filter((r) => !r.ip || MY_IPS.indexOf(r.ip) === -1);
    if (!others.length) {
      box.innerHTML = '<p class="hint">未发现局域网内其它设备（mDNS）。可手动输入地址。</p>';
      return;
    }
    box.innerHTML = '';
    others.forEach((r) => {
      const url = r.urls[0];
      const card = document.createElement('button');
      card.type = 'button';
      card.className = 'scan-card';
      const nameLine = document.createElement('div');
      nameLine.className = 'scan-name';
      nameLine.textContent = r.name || 'Stross 设备';
      const metaLine = document.createElement('div');
      metaLine.className = 'scan-meta';
      metaLine.textContent =
        (r.ip ? r.ip + ':' + r.port : url) +
        (r.roles && r.roles.length ? '  ·  ' + r.roles.map(roleLabel).join(' / ') : '');
      card.appendChild(nameLine);
      card.appendChild(metaLine);
      card.title = '点击连接 ' + url;
      card.onclick = () => {
        $input('relay-addr').value = url;
        (document.querySelector('input[name="conn"][value="remote"]') as HTMLInputElement).checked = true;
        $('remote-row').classList.remove('hidden');
        void connect();
      };
      box.appendChild(card);
    });
  } catch (e) {
    box.innerHTML = `<p class="hint err-text">扫描失败：${(e as Error).message}</p>`;
  }
}

// ---------------------------------------------------------------- 推流配置

function currentVideoSource(): VideoSource {
  const kind = (document.querySelector('input[name="video"]:checked') as HTMLInputElement).value;
  // 注意：与 Rust 端 VideoSource 的 serde(rename_all="camelCase") 契约一致（小写）
  if (kind === 'screen') return { kind: 'screen' };
  if (kind === 'camera') return { kind: 'camera', device: $select('camera-select').value || null };
  return { kind: 'synthetic', pattern: 'testsrc2' };
}

function buildConfig(): StreamConfig {
  const q = QUALITIES[$select('quality-select').value];
  const micOn = $input('mic-enable').checked;
  const sysOn = $input('sys-enable').checked;
  const audio = micOn || sysOn
    ? {
        mic: micOn ? $select('mic-select').value || null : null,
        systemAudio: sysOn ? $select('sys-select').value || null : null,
        sampleRate: 48000,
        channels: 2,
        bitrateKbps: 128,
      }
    : null;
  return {
    streamId: 'stross-' + Date.now().toString(36),
    title: $input('title-input').value.trim() || '我的串流',
    video: currentVideoSource(),
    quality: q,
    audio,
    durationSecs: null,
  };
}

/** Android：与桌面统一走 start_stream（cfg 携带画质/音频；原生采集在 Rust 后端适配）。 */
async function startStream(): Promise<void> {
  hideError();
  if (!connection) {
    showFatal('请先连接中继');
    return;
  }
  savePrefs();
  $btn('start-btn').disabled = true;
  try {
    if (IS_ANDROID) {
      starting = true;
      startingSince = Date.now();
      setRunning(true, 'starting');
      // Android 原生采集启动需要系统授权，真实状态由 capture_status 轮询回报
    }
    const res = (await call('start_stream', { cfg: buildConfig(), relayUrl: connection.wsUrl })) as StartResult;
    renderUrls(res.watchUrls);
    // D4：内核签发流 id —— 预填接收面板，本机可立即原生接收
    $input('recv-stream-input').value = res.streamId || '';
    void loadRemoteStreams();
    if (IS_ANDROID) {
      void pollMobileStatus(); // 立即查一次真实采集状态
    } else {
      setRunning(true, 'live');
    }
  } catch (e) {
    showFatal(String(e));
    starting = false;
    setRunning(false);
  }
}

async function stopStream(): Promise<void> {
  try {
    await call('stop_stream');
  } catch (e) {
    showFatal(String(e));
  }
  starting = false;
  setRunning(false);
}

/** Android：轮询采集真实状态（Kotlin 控制帧 t=9 回报 → capture_status）。 */
async function pollMobileStatus(): Promise<void> {
  if (!IS_ANDROID || !connection) return;
  try {
    const s = (await call('capture_status')) as CaptureStatus;
    if (!s.active) {
      starting = false;
      setRunning(false);
      return;
    }
    if (s.started) {
      starting = false;
      setRunning(true, 'live');
      return;
    }
    if (s.error) {
      starting = false;
      showFatal('采集启动失败：' + s.error);
      setRunning(false);
      return;
    }
    // 仍在启动中：超时兜底，避免无限"采集中…"
    if (starting && Date.now() - startingSince > START_TIMEOUT_MS) {
      starting = false;
      showFatal('采集启动超时（60 秒未就绪）。请停止后重试；若反复超时，请检查系统是否限制后台屏幕录制。');
      setRunning(false);
      return;
    }
    setRunning(true, 'starting');
  } catch (_) {
    /* ignore */
  }
}

async function pollStatus(): Promise<void> {
  if (IS_ANDROID) {
    // Android 每 2 秒轮询真实采集状态
    if (running || starting) void pollMobileStatus();
    return;
  }
  try {
    const s = (await call('stream_status')) as StreamStatus;
    setRunning(s.running);
    $('stream-meta').textContent = s.running
      ? `「${s.title}」(${s.streamId}) · 中继端口 ${s.relayPort} · 开始于 ${new Date(s.startedAt! * 1000).toLocaleTimeString()}`
      : '';
  } catch (_) {
    /* ignore */
  }
}

/** phase: 'idle' | 'starting' | 'live' */
function setRunning(r: boolean, phase: 'idle' | 'starting' | 'live' = r ? 'live' : 'idle'): void {
  running = r;
  const dot = $('status-dot');
  const text = $('status-text');
  $btn('start-btn').disabled = r || starting;
  $btn('stop-btn').disabled = !(r || starting);
  if (phase === 'starting') {
    dot.className = 'dot starting';
    text.textContent = '采集中…';
    $('stream-meta').textContent = '等待系统授权与投影就绪（OPPO 等机型可能需 10~20 秒）';
  } else if (phase === 'live') {
    dot.className = 'dot live';
    text.textContent = IS_ANDROID ? '采集中 ✓ 推流中' : '推流中';
    // 明确告知去向（D1：无浏览器观看端，接收走「观看（收）」页原生播放）
    $('stream-meta').textContent = '推流中 ✅ 局域网设备可在「📥 观看（收）」页选择本机流接收';
  } else {
    dot.className = 'dot idle';
    text.textContent = '未推流';
    $('stream-meta').textContent = '';
  }
}

function renderUrls(urls: string[]): void {
  const ul = $('url-list');
  ul.innerHTML = '';
  urls.forEach((u) => {
    const li = document.createElement('li');
    const tag = document.createElement('span');
    tag.className = 'tag';
    tag.textContent = '▶';
    li.appendChild(tag);
    li.appendChild(document.createTextNode(u));
    li.title = '点击复制';
    li.onclick = () => {
      navigator.clipboard?.writeText(u).then(() => {
        li.style.borderColor = 'var(--ok)';
        li.textContent = '✅ 已复制';
        setTimeout(() => {
          li.style.borderColor = '';
          li.innerHTML = '';
          li.appendChild(tag);
          li.appendChild(document.createTextNode(u));
        }, 1500);
      });
    };
    ul.appendChild(li);
  });
}

// ---------------------------------------------------------------- 接收（原生播放，1e）

/** Tauri 事件监听（__TAURI__.event.listen）。 */
function listen<T>(event: string, cb: (payload: T) => void): Promise<() => void> {
  const api = (window as any).__TAURI__?.event;
  if (!api?.listen) return Promise.resolve(() => {});
  return api.listen(event, (e: { payload: T }) => cb(e.payload));
}

let receiving = false;
let recvFrameCount = 0;
let recvUnlisten: (() => void) | null = null;

/** 拉取当前中继的在线串流列表（GET /api/streams），渲染可选卡片。 */
async function loadRemoteStreams(): Promise<void> {
  const box = $('recv-streams');
  if (!connection) {
    box.innerHTML = '';
    return;
  }
  try {
    const resp = await fetch(connection.url + '/api/streams', { cache: 'no-store' });
    if (!resp.ok) {
      box.innerHTML = '<p class="hint">中继未提供串流列表（HTTP ' + resp.status + '）</p>';
      return;
    }
    const data = (await resp.json()) as { streams?: RemoteStream[] } | RemoteStream[];
    const list = Array.isArray(data) ? data : (data.streams || []);
    if (!list.length) {
      box.innerHTML = '<p class="hint">该中继暂无在线串流。可先在「📤 推流」页开始推流。</p>';
      return;
    }
    box.innerHTML = '';
    for (const s of list) {
      const card = document.createElement('button');
      card.type = 'button';
      card.className = 'scan-card';
      const name = document.createElement('div');
      name.className = 'scan-name';
      name.textContent = s.title || s.streamId;
      const meta = document.createElement('div');
      meta.className = 'scan-meta';
      meta.textContent = s.streamId + (s.watchers ? '  ·  ' + s.watchers + ' 人观看' : '');
      card.appendChild(name);
      card.appendChild(meta);
      card.title = '点击接收 ' + s.streamId;
      card.onclick = () => {
        $input('recv-stream-input').value = s.streamId;
        void startReceive();
      };
      box.appendChild(card);
    }
  } catch (e) {
    box.innerHTML = '<p class="hint">拉取串流列表失败：' + (e as Error).message + '</p>';
  }
}

function showRecvError(msg: string): void {
  const box = $('recv-error');
  box.textContent = msg;
  box.classList.remove('hidden');
}
function hideRecvError(): void {
  $('recv-error').classList.add('hidden');
}

/** 开始原生接收：WS watch → 解码 → canvas 绘制。 */
async function startReceive(): Promise<void> {
  hideRecvError();
  if (!connection) {
    showRecvError('请先连接中继');
    return;
  }
  const streamId = $input('recv-stream-input').value.trim();
  if (!streamId) {
    showRecvError('请输入流 id，或从上方选择一串流');
    return;
  }
  $btn('recv-start-btn').disabled = true;
  try {
    const audio = $select('recv-audio-select').value; // 'device' | 'discard'（与 AudioOut serde 一致）
    await call('start_receive', {
      relay: connection.wsUrl.replace('/ws/push', ''),
      stream: streamId,
      audio,
    });
    receiving = true;
    recvFrameCount = 0;
    $('recv-status').textContent = '接收中…';
    $('recv-dot').className = 'dot starting';
    $btn('recv-stop-btn').disabled = false;
    // 订阅解码帧事件 → canvas
    recvUnlisten = await listen('receive-frame', (p: { pts: number; width: number; height: number; data: number[] }) => {
      drawReceiveFrame(p.width, p.height, p.data);
      recvFrameCount += 1;
    });
    void pollReceiveStatus();
  } catch (e) {
    showRecvError('接收失败：' + (e as Error).message);
    setReceiving(false);
  }
}

/** 停止接收并清空画面。 */
async function stopReceive(): Promise<void> {
  try {
    await call('stop_receive');
  } catch (_) { /* ignore */ }
  if (recvUnlisten) {
    recvUnlisten();
    recvUnlisten = null;
  }
  setReceiving(false);
  const ctx = canvasCtx();
  if (ctx) ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height);
}

function canvasCtx(): CanvasRenderingContext2D | null {
  const c = $('recv-canvas') as HTMLCanvasElement;
  return c.getContext('2d');
}

/** 把 RGBA 帧画到 canvas（宽度自适应，等比缩放）。 */
function drawReceiveFrame(w: number, h: number, data: number[]): void {
  const ctx = canvasCtx();
  if (!ctx) return;
  const canvas = ctx.canvas;
  if (canvas.width !== w) canvas.width = w;
  if (canvas.height !== h) canvas.height = h;
  const img = new ImageData(new Uint8ClampedArray(data), w, h);
  ctx.putImageData(img, 0, 0);
}

function setReceiving(r: boolean): void {
  receiving = r;
  $btn('recv-start-btn').disabled = r;
  $btn('recv-stop-btn').disabled = !r;
  $('recv-dot').className = 'dot ' + (r ? 'live' : 'idle');
  $('recv-status').textContent = r ? '接收中 ✓' : '未接收';
  if (!r) $('recv-meta').textContent = '';
}

/** 轮询接收统计（帧数 / 解码 / 音频块）。 */
async function pollReceiveStatus(): Promise<void> {
  if (!receiving || !connection) return;
  try {
    const s = (await call('receive_status')) as ReceiveStats;
    if (!s.running && recvFrameCount === 0 && !s.error) {
      $('recv-dot').className = 'dot starting';
      $('recv-status').textContent = '等待流数据…';
    }
    $('recv-meta').textContent = s.error
      ? '错误：' + s.error
      : `收到 ${s.received} 帧 · 解码 ${s.decodedVideo} 帧 · 音频 ${s.audioBlocks} 块 · 已绘制 ${recvFrameCount} 帧`;
  } catch (_) { /* ignore */ }
  if (receiving) setTimeout(() => void pollReceiveStatus(), 1000);
}

// ---------------------------------------------------------------- 事件

document.querySelectorAll<HTMLInputElement>('input[name="conn"]').forEach((r) =>
  r.addEventListener('change', () => {
    $('remote-row').classList.toggle('hidden', r.value !== 'remote');
  })
);
document.querySelectorAll<HTMLInputElement>('input[name="video"]').forEach((r) =>
  r.addEventListener('change', () => {
    $('camera-row').classList.toggle('hidden', r.value !== 'camera');
  })
);

$btn('connect-btn').onclick = () => void connect();
$btn('scan-btn').onclick = () => void scanRelays();
$btn('disconnect-btn').onclick = disconnect;
$btn('tab-send-btn').onclick = () => setTab('send');
$btn('tab-watch-btn').onclick = () => setTab('watch');
$btn('start-btn').onclick = () => void startStream();
$btn('stop-btn').onclick = () => void stopStream();
$btn('recv-start-btn').onclick = () => void startReceive();
$btn('recv-stop-btn').onclick = () => void stopReceive();

void init();
setInterval(() => void pollStatus(), 2000);
