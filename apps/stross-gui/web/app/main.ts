// Stross 推流端控制界面（Tauri 前端，TypeScript 源文件，唯一真源）。
//
// 生成 app/ 下的 JS：`npx tsc -p apps/stross-gui/web/tsconfig.json`
// （app/*.js 是构建产物，提交进仓库——Tauri 直接加载，Rust 构建不依赖 node）。
// 修改任一 .ts 后必须重新生成并提交 app/*.js（scripts/check.sh --quick 会校验同步）。
//
// 交互模型（P0 免先连进入网格）：打开应用即自动锚定本机（启动受控中继 +
// mDNS 广播）并扫描局域网设备/串流，直接进入「网格」页——点流卡片即看
// （直连锚点，失败自动经本机级联代理），推流锚定本机；「连接」不再是先决
// 条件，而是点流时的按需建立。
//
// 图标：统一使用内联 SVG 雪碧图（index.html 中的 <symbol> + icon() 辅助），
// 不使用 emoji。交互约定：
//   · 连接成功后启动状态轮询，断开即停止（不全局无条件轮询）
//   · 耗时操作（连接/推流/接收）按钮内嵌 spinner 加载态
//   · 扫描/串流列表请求带 in-flight guard 与 TTL 缓存，防止快速切换重复请求
//   · 断开连接为两段式确认（防误触），错误提示可关闭
//
// 模块（script 全局作用域，按依赖序加载，见 index.html）：
//   state.ts —— 类型 / 常量 / 全局状态
//   ui.ts   —— DOM 助手与通用渲染
//   watch.ts —— 接收域（观看连接、解码帧绘制、统计轮询）
//   grid.ts —— 设备网格域（本机锚点、局域网设备扫描、串流聚合）
//   send.ts —— 推流域（采集配置、推流生命周期）
//   main.ts —— 初始化与事件绑定（本文件）

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
      fb.textContent = 'ffmpeg';
      fb.classList.add('ok');
    } else {
      fb.textContent = '未检测到 ffmpeg';
      fb.classList.add('err');
    }
    renderIps(info.ips);
    restorePrefs();
    await loadDevices();
    // 免先连：自动锚定本机（受控中继 + mDNS 广播）→ 进入网格 → 自动扫描设备/串流
    await ensureAnchor();
    showApp();
    startStatusPolling();
    void loadRemoteStreams(true);
    void scanRelays();
    void scanRemoteStreams();
  } catch (e) {
    showFatal(String(e));
  }
}

/** 进入主界面（免先连：init 锚定后直接调用，无连接门槛）。 */
function showApp(): void {
  $('app-view').classList.remove('hidden');
  showView($('app-view'));
  $('watch-relay-url').textContent = anchor
    ? `http://127.0.0.1:${anchor.port}`
    : '（本机锚点未就绪）';
  // 锚定成功即展示本机锚点入口地址（供其它设备连接数据面）
  if (anchor && anchor.urls.length) {
    renderUrls(anchor.urls);
  }
  setTab('grid');
  void pollStatus();
}

/** 状态轮询：应用打开期间常驻（本机状态，无需连接前提）。 */
function startStatusPolling(): void {
  if (statusTimer !== null) return;
  statusTimer = window.setInterval(() => void pollStatus(), 2000);
}

// ---------------------------------------------------------------- 模式切换

function setTab(tab: 'grid' | 'send' | 'watch'): void {
  currentTab = tab;
  $('tab-grid-btn').classList.toggle('active', tab === 'grid');
  $('tab-send-btn').classList.toggle('active', tab === 'send');
  $('tab-watch-btn').classList.toggle('active', tab === 'watch');
  const grid = $('tab-grid');
  const send = $('tab-send');
  const watch = $('tab-watch');
  grid.classList.toggle('hidden', tab !== 'grid');
  send.classList.toggle('hidden', tab !== 'send');
  watch.classList.toggle('hidden', tab !== 'watch');
  showView(tab === 'grid' ? grid : tab === 'send' ? send : watch);
  if (tab === 'grid') {
    void scanRemoteStreams(); // TTL 内命中缓存，不重复扫描
  }
  if (tab === 'watch') {
    void loadRemoteStreams();
  }
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

// ---------------------------------------------------------------- 事件

document.querySelectorAll<HTMLInputElement>('input[name="video"]').forEach((r) =>
  r.addEventListener('change', () => {
    $('camera-row').classList.toggle('hidden', r.value !== 'camera');
  })
);

$btn('scan-btn').onclick = () => void scanRelays();
$btn('manual-add-btn').onclick = () => void addManualRelay();
$btn('tab-grid-btn').onclick = () => setTab('grid');
$btn('tab-send-btn').onclick = () => setTab('send');
$btn('tab-watch-btn').onclick = () => setTab('watch');
$btn('discover-btn').onclick = () => void scanRemoteStreams(true);
$btn('start-btn').onclick = () => void startStream();
$btn('stop-btn').onclick = () => void stopStream();
$btn('recv-start-btn').onclick = () => void startReceive();
$btn('recv-stop-btn').onclick = () => void stopReceive();

// 手动地址输入框回车 = 添加设备
$input('manual-addr').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    void addManualRelay();
  }
});

void init();
// 状态轮询由 init 锚定后启动（本机状态，无连接前提）
