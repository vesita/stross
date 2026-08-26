// Stross 前端 —— 初始化与事件绑定（script 全局作用域，按依赖序最后加载）。
//
// 界面模型：设备 × 共享流 组合管理（v6 界面改版）——
//   左栏「设备」：本机 + 局域网设备卡片，点设备展开 → 发起共享（广播/定向）
//             与该设备的在线共享（点条目即接收）；B2 接收手机麦克风入口在本机卡片。
//   右栏「共享流」：全部活动共享统一管理（方向 ↑↓ / 媒体 / 对端 / 状态 / 停止）。
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
    const info = (await call('app_info')) as AppInfo;
    IS_ANDROID = info.platform === 'android';
    MY_IPS = info.ips || [];
    $('ver-badge').textContent = 'v' + info.version;
    const fb = $('ffmpeg-badge');
    if (IS_ANDROID) {
      fb.textContent = '原生采集';
      fb.classList.add('ok');
    } else if (info.ffmpeg) {
      fb.textContent = 'ffmpeg';
      fb.classList.add('ok');
    } else {
      fb.textContent = '未检测到 ffmpeg';
      fb.classList.add('err');
    }
    restorePrefs();
    deviceViews = [];
    await loadDevices();
    // 免先连：先渲染本机卡片骨架（含 ip-list / 锚点状态位），
    // 再自动锚定本机（受控中继 + mDNS 广播）→ 扫描设备与在线共享
    void renderDeviceList();
    renderIps(info.ips);
    await ensureAnchor();
    startStatusPolling();
    void scanRelays();
    void scanRemoteStreams(true);
    void renderShares();
  } catch (e) {
    showFatal(String(e));
  }
}

/** 状态轮询：应用打开期间常驻（本机状态，无需连接前提）。 */
function startStatusPolling(): void {
  if (statusTimer !== null) return;
  statusTimer = window.setInterval(() => void pollStatus(), 2000);
}

// ---------------------------------------------------------------- 设备能力

async function loadDevices(): Promise<void> {
  devices = (await call('list_devices')) as DeviceList;
}

// ---------------------------------------------------------------- 事件绑定

/** 复制「接收手机麦克风」凭证到剪贴板。 */
async function copyMicToken(): Promise<void> {
  const v = $input('mic-recv-token').value;
  if (!v) return;
  try {
    await navigator.clipboard?.writeText(v);
  } catch (_) { /* 剪贴板不可用（HTTP/非安全上下文）时忽略 */ }
  const b = $btn('mic-recv-copy-btn');
  b.innerHTML = icon('check') + '<span>已复制</span>';
  setTimeout(() => {
    b.innerHTML = icon('copy') + '<span>复制凭证</span>';
  }, 1500);
}

// 设备列表事件委托：操作按钮（data-act）+ 复制凭证（本机卡片内）
$('device-list').addEventListener('click', (e) => {
  const t = e.target as HTMLElement;
  if (t.closest('#mic-recv-copy-btn')) {
    void copyMicToken();
    return;
  }
  const btn = t.closest('[data-act]') as HTMLElement | null;
  if (!btn) return;
  e.stopPropagation();
  switch (btn.dataset.act) {
    case 'broadcast-screen':
      openBroadcastScreen();
      break;
    case 'broadcast-mic':
      openBroadcastMic();
      break;
    case 'recv-mic':
      void startMicReceive();
      break;
    case 'mic-to': {
      const card = btn.closest('.dev-card') as HTMLElement | null;
      const dev = card && deviceViews.find((d) => d.key === card.dataset.key);
      if (dev) void openMicShare(dev);
      break;
    }
  }
});

// 共享弹窗（广播屏幕 / 广播麦克风）
$btn('share-start-btn').onclick = () => void confirmShareModal();
$btn('share-cancel-btn').onclick = () => {
  // 停止共享：取消弹窗时若正在启动则停止（仅弹窗态）
  cancelShareModal();
};
$('share-modal').addEventListener('click', (e) => {
  if (e.target === $('share-modal')) cancelShareModal();
});

// 共享麦克风凭证弹窗（B2 定向推流）
$btn('mic-start-btn').onclick = () => void startMicShare();
$btn('mic-stop-btn').onclick = () => void stopMicShare();
$btn('mic-close-btn').onclick = () => $('mic-modal').classList.add('hidden');
$('mic-modal').addEventListener('click', (e) => {
  if (e.target === $('mic-modal')) $('mic-modal').classList.add('hidden');
});

$btn('scan-btn').onclick = () => void scanRelays();
$btn('manual-add-btn').onclick = () => void addManualRelay();
// 手动地址输入框回车 = 添加设备
$input('manual-addr').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    void addManualRelay();
  }
});

void init();