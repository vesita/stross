// Stross 前端 —— 初始化与事件绑定（script 全局作用域，按依赖序最后加载）。
//
// 界面模型：本机能力 → 发现对端 → 订阅对端推送的流（「设备 × 共享流」双栏）。
//   左栏「设备」：本机 + 局域网设备卡片，点设备展开 → 发起共享（广播/定向）
//             与该设备的在线共享（点条目即订阅接收）；B2 接收手机麦克风入口在本机卡片。
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
    // 免先连：先渲染本机卡片骨架（含锚点状态位），
    // 再自动锚定本机（受控中继 + mDNS 广播）→ 扫描设备与在线共享
    void renderDeviceList();
    await ensureAnchor();
    startStatusPolling();
    void refreshDevices();
    void renderShares();
    // 权限自动化：防火墙自检（缺放行则提示一键放行）+ 协商授权事件桥
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
    void pollStatus();
    // 设备列表 + 在线共享周期刷新（refreshDevices 自带 5s TTL + in-flight
    // 守卫：mDNS + 探测 + 聚合在 Rust `scan_devices` 内一次完成）。
    void refreshDevices();
  }, 2000);
}

// ---------------------------------------------------------------- 设备能力

async function loadDevices(): Promise<void> {
  devices = (await call('list_devices')) as DeviceList;
}

// ---------------------------------------------------------------- 权限自动化

/** 电脑端收到设备接入请求：展示授权确认弹窗（首次人工确认，信任门控）。 */
function onApproveRequest(req: PendingRequest): void {
  pendingApprove = req;
  $('approve-device').textContent =
    `${req.deviceName}（${req.deviceId.slice(0, 12)}…）`;
  $('approve-media').textContent =
    '申请共享：' + (req.media.length ? req.media.join('、') : '未知媒体');
  $('approve-error').classList.add('hidden');
  $('approve-status').textContent = '';
  $('approve-modal').classList.remove('hidden');
}

/** 应答协商请求：允许（可勾选记住）或拒绝。允许成功后自动监听接收该流。 */
async function respondApprove(allow: boolean): Promise<void> {
  if (!pendingApprove) return;
  const reqId = pendingApprove.id;
  const remember = $input('approve-remember') as HTMLInputElement;
  let grant: ShareGrant | null = null;
  try {
    grant = (await call('negotiator_respond', {
      reqId,
      allow,
      remember: remember.checked,
    })) as ShareGrant | null;
  } catch (e) {
    $('approve-error').textContent = '应答失败：' + (e as Error).message;
    $('approve-error').classList.remove('hidden');
    return;
  }
  pendingApprove = null;
  if (allow && grant && grant.streamId) {
    // 电脑端自动监听该会话流：出现在 /api/streams 即原生接收
    // （与「接收手机麦克风」共用同一等待-订阅链路；手机随后自动推流进入）
    beginAwaitMicStream(grant.streamId);
    $('approve-status').textContent = '已允许，等待手机接入…';
  } else {
    $('approve-status').textContent = allow ? '已允许并通知设备' : '已拒绝';
  }
  $('approve-modal').classList.add('hidden');
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
// 「自动获取凭证」重试（自动协商失败后手动重试，免粘贴）
$btn('mic-auto-btn').onclick = async () => {
  if (!micShare) return;
  const dev = deviceViews.find((d) => d.base === micShare!.base);
  if (!dev) return;
  $('mic-error').classList.add('hidden');
  $('mic-status').textContent = '正在向设备申请凭证…';
  const r = await autoNegotiateMic(dev);
  if (r.ok && r.token && r.streamId && micShare) {
    try {
      await startMicShareWith({
        token: r.token,
        streamId: r.streamId,
        base: micShare.base,
        quicPort: micShare.quicPort,
      });
      $('mic-status').textContent = '已自动获取凭证，推流中…';
      setMicRunning(true);
    } catch (_) { /* 错误已显示在弹窗 */ }
  } else {
    $('mic-status').textContent = '自动协商未成功（' + (r.error || '未知原因') + '），请粘贴凭证';
  }
};

// 设备接入授权确认（权限自动化：首次人工确认）
$btn('approve-allow-btn').onclick = () => void respondApprove(true);
$btn('approve-deny-btn').onclick = () => void respondApprove(false);
// 防火墙一键放行
$btn('fw-allow-btn').onclick = () => void allowFirewall();
$btn('fw-close-btn').onclick = () => $('fw-banner').classList.add('hidden');

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