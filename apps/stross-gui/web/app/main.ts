// Stross 前端 —— 初始化与事件绑定（script 全局作用域，按依赖序最后加载）。
//
// 界面模型（节点 → 设备 → 端点：共享 + 订阅）：
//   左栏「设备」：本机设备树（共享/取消共享）+ 局域网设备目录（订阅端点）；
//   右栏「接收」：订阅进来的端点流在此播放/停止。
//
// 生成 app/ 下的 JS：`npx tsc -p apps/stross-gui/web/tsconfig.json`
// （app/*.js 是构建产物，提交进仓库——Tauri 直接加载，Rust 构建不依赖 node）。
// 修改任一 .ts 后必须重新生成并提交 app/*.js（scripts/check.sh --quick 会校验同步）。

// ---------------------------------------------------------------- 初始化

async function init(): Promise<void> {
  if (!invoke) {
    showFatal('当前页面未运行在 Stross 桌面应用中。\n请通过 `cargo tauri dev` 或安装包启动。');
    return;
  }
  try {
    const info = await call<AppInfo>('app_info');
    IS_ANDROID = info.platform === 'android';
    $('ver-badge').textContent = 'v' + info.version;
    const fb = $('ffmpeg-badge');
    if (IS_ANDROID) {
      fb.textContent = '原生采集';
      fb.classList.add('ok');
    } else if (info.ffmpeg) {
      fb.textContent = '推流就绪';
      fb.classList.add('ok');
    } else {
      fb.textContent = '缺少推流引擎';
      fb.classList.add('err');
    }
    restorePrefs();
    deviceViews = [];
    await loadDevices();
    void renderDeviceList();
    await ensureAnchor();
    startStatusPolling();
    void refreshDevices();
    await refreshDiscoverable();
    void checkFirewall();
    void listen('negotiator-request', (req: PendingRequest) => onApproveRequest(req));
  } catch (e) {
    showFatal(String(e));
  }
}

/** 状态轮询：应用打开期间常驻（本机状态，无需连接前提）。 */
function startStatusPolling(): void {
  if (statusTimer !== null) return;
  statusTimer = window.setInterval(() => {
    void refreshDevices();
    void refreshLocalCatalog();
  }, 2000);
}

// ---------------------------------------------------------------- 设备能力

async function loadDevices(): Promise<void> {
  devices = await call<DeviceList>('list_devices');
}

// ---------------------------------------------------------------- 可被发现

/** 读取运行时「可被发现」状态并同步开关 UI。 */
async function refreshDiscoverable(): Promise<void> {
  const s = await call<Settings>('discoverable_status');
  $input('disco-toggle').checked = s.discoverable;
}

/** 设置「可被发现」（提交内核 + 持久化）。 */
async function setDiscoverable(on: boolean): Promise<void> {
  try {
    await call('set_discoverable', { on });
  } catch (e) {
    showGridError('设置可被发现失败：' + errMsg(e));
    void refreshDiscoverable();
  }
}

// ---------------------------------------------------------------- 权限自动化

/** 本端（共享方）收到订阅请求：展示授权确认弹窗（首次人工确认，信任门控）。 */
function onApproveRequest(req: PendingRequest): void {
  pendingApprove = req;
  $('approve-device').textContent =
    `设备「${req.deviceName}」（${req.deviceId.slice(0, 12)}…）`;
  const mediaLabel =
    req.endpointName ||
    (req.media.length
      ? req.media.map((m) => labelOf(DEVICE_KIND_LABELS, m)).join('、')
      : '未知媒体');
  $('approve-media').textContent = '想订阅你共享的内容：' + mediaLabel;
  $('approve-error').classList.add('hidden');
  $('approve-status').textContent = '';
  $('approve-modal').classList.remove('hidden');
}

/** 应答协商请求：允许（可勾选记住）或拒绝。 */
async function respondApprove(allow: boolean): Promise<void> {
  if (!pendingApprove) return;
  const reqId = pendingApprove.id;
  const remember = $input('approve-remember') as HTMLInputElement;
  try {
    await call('negotiator_respond', { reqId, allow, remember: remember.checked });
  } catch (e) {
    $('approve-error').textContent = '应答失败：' + errMsg(e);
    $('approve-error').classList.remove('hidden');
    return;
  }
  pendingApprove = null;
  $('approve-status').textContent = allow ? '已允许并通知设备' : '已拒绝';
  $('approve-modal').classList.add('hidden');
}

// ---------------------------------------------------------------- 事件绑定

// 设备列表事件委托：端点框架操作按钮（data-act：共享/取消共享/订阅）
$('device-list').addEventListener('click', (e) => {
  const t = e.target as HTMLElement;
  const btn = t.closest('[data-act]') as HTMLElement | null;
  if (!btn) return;
  e.stopPropagation();
  switch (btn.dataset.act) {
    case 'publish-device': {
      const deviceId = btn.dataset.device;
      if (deviceId) openPublishModal(deviceId);
      break;
    }
    case 'unpublish-endpoint': {
      const endpointId = btn.dataset.endpoint;
      if (endpointId) void unpublishEndpoint(endpointId);
      break;
    }
    case 'stop-share': {
      const endpointId = btn.dataset.endpoint;
      if (endpointId) void stopShare(endpointId);
      break;
    }
    case 'subscribe-endpoint': {
      const host = btn.dataset.host;
      const endpointId = btn.dataset.endpoint;
      if (host && endpointId) openSubscribeModal(host, endpointId);
      break;
    }
  }
});

// 设备接入授权确认（权限自动化：首次人工确认）
$btn('approve-allow-btn').onclick = () => void respondApprove(true);
$btn('approve-deny-btn').onclick = () => void respondApprove(false);
// 端点框架弹窗（共享 / 订阅）
$btn('pub-confirm-btn').onclick = () => void confirmPublish();
$btn('pub-cancel-btn').onclick = () => $('pub-modal').classList.add('hidden');
$('pub-modal').addEventListener('click', (e) => {
  if (e.target === $('pub-modal')) $('pub-modal').classList.add('hidden');
});
const pubClose = $('pub-modal-close');
if (pubClose) pubClose.onclick = () => $('pub-modal').classList.add('hidden');

$btn('sub-confirm-btn').onclick = () => void confirmSubscribe();
$btn('sub-cancel-btn').onclick = () => $('sub-modal').classList.add('hidden');
$('sub-modal').addEventListener('click', (e) => {
  if (e.target === $('sub-modal')) $('sub-modal').classList.add('hidden');
});
const subClose = $('sub-modal-close');
if (subClose) subClose.onclick = () => $('sub-modal').classList.add('hidden');

// 接收面板：停止接收
$btn('recv-stop-btn').onclick = () => void stopReceive();
// 接收链路行：逐条停止
$('recv-links').addEventListener('click', (e) => {
  const btn = (e.target as HTMLElement).closest('[data-link]') as HTMLElement | null;
  if (btn && btn.dataset.link) void stopReceiveLink(btn.dataset.link);
});
// 播放器控制条：全屏 / 停止
$btn('recv-fs-btn').onclick = () => void togglePlayerFullscreen();
$btn('recv-fs-stop-btn').onclick = () => void stopReceive();
// 双击画面切换全屏
$('recv-canvas').addEventListener('dblclick', () => void togglePlayerFullscreen());
// ESC 退出全屏
window.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') void exitPlayerFullscreen();
});
// 防火墙一键放行
$btn('fw-allow-btn').onclick = () => void allowFirewall();
$btn('fw-close-btn').onclick = () => $('fw-banner').classList.add('hidden');

$btn('scan-btn').onclick = () => void scanRelays();
$btn('manual-add-btn').onclick = () => void addManualRelay();
// 「可被发现」开关
$input('disco-toggle').addEventListener('change', (e) => {
  void setDiscoverable((e.target as HTMLInputElement).checked);
});
// 手动地址输入框回车 = 添加设备
$input('manual-addr').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    void addManualRelay();
  }
});

// 全局视图切换导航
const navBtnManage = $('nav-btn-manage');
const navBtnConsume = $('nav-btn-consume');
if (navBtnManage) {
  navBtnManage.onclick = () => switchView('manage');
}
if (navBtnConsume) {
  navBtnConsume.onclick = () => switchView('consume');
}

// 消费舞台返回管理视图按钮
const stageBackBtn = $('stage-back-btn');
if (stageBackBtn) {
  stageBackBtn.onclick = () => switchView('manage');
}
const emptyGoManageBtn = $('empty-go-manage-btn');
if (emptyGoManageBtn) {
  emptyGoManageBtn.onclick = () => switchView('manage');
}

// 移动端分段导航兼容
const tabDevBtn = $('tab-devices-btn');
const tabRecvBtn = $('tab-recv-btn');
if (tabDevBtn) {
  tabDevBtn.onclick = () => switchView('manage');
}
if (tabRecvBtn) {
  tabRecvBtn.onclick = () => switchView('consume');
}

// 移动端快速跳转到接收面板
const mobJumpBtn = $('mobile-recv-jump-btn');
if (mobJumpBtn) {
  mobJumpBtn.onclick = () => {
    switchView('consume');
    $('recv-pane')?.scrollIntoView({ behavior: 'smooth', block: 'start' });
  };
}
// 设备搜索/过滤输入框事件
const filterInput = $('dev-filter-input') as HTMLInputElement | null;
const filterClear = $('dev-filter-clear');
if (filterInput) {
  filterInput.addEventListener('input', () => {
    deviceFilterQuery = filterInput.value.trim().toLowerCase();
    if (filterClear) filterClear.classList.toggle('hidden', !deviceFilterQuery);
    renderDeviceList();
  });
}
if (filterClear && filterInput) {
  filterClear.addEventListener('click', () => {
    filterInput.value = '';
    deviceFilterQuery = '';
    filterClear.classList.add('hidden');
    renderDeviceList();
  });
}

// 播放器 AI 工具条与手势初始化
const aspectBtn = $('player-aspect-btn');
if (aspectBtn) aspectBtn.onclick = () => cycleAspectRatio();
const quickAspectBtn = $('recv-aspect-quick-btn');
if (quickAspectBtn) quickAspectBtn.onclick = () => cycleAspectRatio();

const telemetryPill = $('player-telemetry-pill');
if (telemetryPill) telemetryPill.onclick = () => toggleDiagnosticsDrawer();
const diagCloseBtn = $('diag-close-btn');
if (diagCloseBtn) diagCloseBtn.onclick = () => toggleDiagnosticsDrawer();

initPlayerGestures();
initFSMUI();

void init();
