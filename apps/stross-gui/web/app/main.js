"use strict";
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
async function init() {
    if (!invoke) {
        showFatal('当前页面未运行在 Stross 桌面应用中。\n请通过 `cargo tauri dev` 或安装包启动。');
        return;
    }
    try {
        const info = await call('app_info');
        IS_ANDROID = info.platform === 'android';
        $('ver-badge').textContent = 'v' + info.version;
        const fb = $('ffmpeg-badge');
        if (IS_ANDROID) {
            // Android 走原生 MediaCodec/AAudio，无 ffmpeg 引擎——隐藏「原生采集」字样（无信息量）。
            fb.classList.add('hidden');
            fb.textContent = '';
        }
        else if (info.ffmpeg) {
            fb.textContent = '推流就绪';
            fb.classList.add('ok');
        }
        else {
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
        void listen('negotiator-request', (req) => onApproveRequest(req));
    }
    catch (e) {
        showFatal(String(e));
    }
}
/** 状态轮询：应用打开期间常驻（本机状态，无需连接前提）。 */
function startStatusPolling() {
    if (statusTimer !== null)
        return;
    statusTimer = window.setInterval(() => {
        void refreshDevices();
        void refreshLocalCatalog();
    }, 2000);
}
// ---------------------------------------------------------------- 设备能力
async function loadDevices() {
    devices = await call('list_devices');
}
// ---------------------------------------------------------------- 可被发现
/** 更新本机卡片「可被发现」开关按钮态（on=公开，off=隐藏）。
 *  el 可选：构造期本机卡片尚未入 DOM 时直接传入按钮元素，避免按 id 查不到。 */
function updateDiscoUI(on, el) {
    discoverableOn = on;
    const btn = el || $btn('disco-toggle');
    if (!btn)
        return;
    btn.classList.toggle('on', on);
    btn.setAttribute('aria-pressed', String(on));
    btn.title = on
        ? '已公开：局域网内其它节点可以发现并连接本机（点击隐身）'
        : '未公开：局域网内其它节点无法发现本机（点击公开）';
    btn.innerHTML = icon(on ? 'radio' : 'eye') +
        (on ? '<span>已公开 (广播中)</span>' : '<span>未公开 (已隐身)</span>');
}
/** 读取运行时「可被发现」状态并同步开关 UI。 */
async function refreshDiscoverable() {
    const s = await call('discoverable_status');
    updateDiscoUI(s.discoverable);
}
/** 设置「可被发现」（提交内核 + 持久化）。 */
async function setDiscoverable(on) {
    try {
        await call('set_discoverable', { on });
        updateDiscoUI(on);
    }
    catch (e) {
        showGridError('设置可被发现失败：' + errMsg(e));
        void refreshDiscoverable();
    }
}
// ---------------------------------------------------------------- 权限自动化
/** 本端（共享方）收到订阅请求：展示授权确认弹窗（首次人工确认，信任门控）。 */
function onApproveRequest(req) {
    pendingApprove = req;
    $('approve-device').textContent =
        `节点「${req.deviceName}」（${req.deviceId.slice(0, 12)}…）`;
    const mediaLabel = req.endpointName ||
        (req.media.length
            ? req.media.map((m) => labelOf(DEVICE_KIND_LABELS, m)).join('、')
            : '未知媒体');
    $('approve-media').textContent = '想订阅你共享的内容：' + mediaLabel;
    $('approve-error').classList.add('hidden');
    $('approve-status').textContent = '';
    $('approve-modal').classList.remove('hidden');
}
/** 应答协商请求：允许（可勾选记住）或拒绝。 */
async function respondApprove(allow) {
    if (!pendingApprove)
        return;
    const reqId = pendingApprove.id;
    const remember = $input('approve-remember');
    try {
        await call('negotiator_respond', { reqId, allow, remember: remember.checked });
    }
    catch (e) {
        $('approve-error').textContent = '应答失败：' + errMsg(e);
        $('approve-error').classList.remove('hidden');
        return;
    }
    pendingApprove = null;
    $('approve-status').textContent = allow ? '已允许并通知节点' : '已拒绝';
    $('approve-modal').classList.add('hidden');
}
// ---------------------------------------------------------------- 事件绑定
// 设备列表与本机管理面板事件委托：端点框架操作按钮（data-act：共享/取消共享/订阅）
const handleDeviceAction = (e) => {
    const t = e.target;
    const btn = t.closest('[data-act]');
    if (!btn)
        return;
    e.stopPropagation();
    switch (btn.dataset.act) {
        case 'toggle-disco': {
            void setDiscoverable(!discoverableOn);
            break;
        }
        case 'publish-device': {
            const deviceId = btn.dataset.device;
            if (deviceId)
                openPublishModal(deviceId);
            break;
        }
        case 'unpublish-endpoint': {
            const endpointId = btn.dataset.endpoint;
            if (endpointId)
                void unpublishEndpoint(endpointId);
            break;
        }
        case 'stop-share': {
            const endpointId = btn.dataset.endpoint;
            if (endpointId)
                void stopShare(endpointId);
            break;
        }
        case 'subscribe-endpoint': {
            const host = btn.dataset.host;
            const endpointId = btn.dataset.endpoint;
            if (host && endpointId)
                openSubscribeModal(host, endpointId);
            break;
        }
    }
};
$('device-list')?.addEventListener('click', handleDeviceAction);
$('local-pane')?.addEventListener('click', handleDeviceAction);
// 设备接入授权确认（权限自动化：首次人工确认）
$btn('approve-allow-btn').onclick = () => void respondApprove(true);
$btn('approve-deny-btn').onclick = () => void respondApprove(false);
// 端点框架弹窗（共享 / 订阅）
$btn('pub-confirm-btn').onclick = () => void confirmPublish();
$btn('pub-cancel-btn').onclick = () => $('pub-modal').classList.add('hidden');
$('pub-modal').addEventListener('click', (e) => {
    if (e.target === $('pub-modal'))
        $('pub-modal').classList.add('hidden');
});
const pubClose = $('pub-modal-close');
if (pubClose)
    pubClose.onclick = () => $('pub-modal').classList.add('hidden');
$btn('sub-confirm-btn').onclick = () => void confirmSubscribe();
$btn('sub-cancel-btn').onclick = () => $('sub-modal').classList.add('hidden');
$('sub-modal').addEventListener('click', (e) => {
    if (e.target === $('sub-modal'))
        $('sub-modal').classList.add('hidden');
});
const subClose = $('sub-modal-close');
if (subClose)
    subClose.onclick = () => $('sub-modal').classList.add('hidden');
// 接收面板：停止接收
$btn('recv-stop-btn').onclick = () => void stopReceive();
// 接收链路行：逐条停止
$('recv-links').addEventListener('click', (e) => {
    const btn = e.target.closest('[data-link]');
    if (btn && btn.dataset.link)
        void stopReceiveLink(btn.dataset.link);
});
// 播放器控制条：全屏 / 停止
$btn('recv-fs-btn').onclick = () => void togglePlayerFullscreen();
$btn('recv-fs-stop-btn').onclick = () => void stopReceive();
// 双击画面切换全屏
$('recv-canvas').addEventListener('dblclick', () => void togglePlayerFullscreen());
// ESC 退出全屏
window.addEventListener('keydown', (e) => {
    if (e.key === 'Escape')
        void exitPlayerFullscreen();
});
// 防火墙一键放行
$btn('fw-allow-btn').onclick = () => void allowFirewall();
$btn('fw-close-btn').onclick = () => $('fw-banner').classList.add('hidden');
$btn('scan-btn').onclick = () => void scanRelays();
$btn('manual-add-btn').onclick = () => void addManualRelay();
// 「可被发现」开关按钮（本机卡片头）：经 #device-list 事件委托处理（见下方 delegation）
// 手动地址输入框回车 = 添加设备
$input('manual-addr').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
        e.preventDefault();
        void addManualRelay();
    }
});
// 节点二级页底部子标签切换（浏览 vs 订阅）
const nodeTabBrowse = $('node-tab-browse');
const nodeTabPlayer = $('node-tab-player');
if (nodeTabBrowse) {
    nodeTabBrowse.onclick = () => switchNodeSubtab('browse');
}
if (nodeTabPlayer) {
    nodeTabPlayer.onclick = () => switchNodeSubtab('player');
}
// 全局主底部导航栏事件
const mainTabLocal = $('main-tab-local');
const mainTabDiscover = $('main-tab-discover');
const mainTabConsume = $('main-tab-consume');
if (mainTabLocal) {
    mainTabLocal.onclick = () => switchMainBottomTab('local');
}
if (mainTabDiscover) {
    mainTabDiscover.onclick = () => switchMainBottomTab('discover');
}
if (mainTabConsume) {
    mainTabConsume.onclick = () => switchMainBottomTab('consume');
}
// 消费舞台空状态跳转到端点浏览
const emptyGoBrowseBtn = $('empty-go-browse-btn');
if (emptyGoBrowseBtn) {
    emptyGoBrowseBtn.onclick = () => switchNodeSubtab('browse');
}
const emptyGoManageBtn = $('empty-go-manage-btn');
if (emptyGoManageBtn) {
    emptyGoManageBtn.onclick = () => {
        switchMainBottomTab('discover');
    };
}
// 移动端快速跳转到接收面板
const mobJumpBtn = $('mobile-recv-jump-btn');
if (mobJumpBtn) {
    mobJumpBtn.onclick = () => {
        switchMainBottomTab('consume');
        switchNodeSubtab('player');
        $('recv-pane')?.scrollIntoView({ behavior: 'smooth', block: 'start' });
    };
}
// 移动端返回节点列表
const mobBackBtn = $('mobile-back-btn');
if (mobBackBtn) {
    mobBackBtn.onclick = () => {
        switchMainBottomTab('discover');
    };
}
// 快速发送便签到当前节点
const chatSendBtn = $('chat-send-btn');
const chatNoteInput = document.getElementById('chat-note-input');
const sendChatNote = () => {
    if (!chatNoteInput)
        return;
    const text = chatNoteInput.value.trim();
    if (!text)
        return;
    chatNoteInput.value = '';
    appendChatTimelineMessage(text, true);
    showToast('便签已发送到当前节点', 'ok');
};
if (chatSendBtn) {
    chatSendBtn.onclick = sendChatNote;
}
if (chatNoteInput) {
    chatNoteInput.onkeydown = (e) => {
        if (e.key === 'Enter') {
            e.preventDefault();
            sendChatNote();
        }
    };
}
// 快速共享屏幕 / 共享声音 / 共享文件
const actScreen = $('chat-act-screen');
if (actScreen) {
    actScreen.onclick = () => {
        const ep = localCatalog.endpoints.find((e) => e.kind === 'screen');
        if (ep)
            openPublishModal(endpointIdStr(ep));
        else
            showToast('未找到可用的屏幕共享源', 'err');
    };
}
const actMic = $('chat-act-mic');
if (actMic) {
    actMic.onclick = () => {
        const ep = localCatalog.endpoints.find((e) => e.kind === 'audio' || e.kind === 'microphone');
        if (ep)
            openPublishModal(endpointIdStr(ep));
        else
            showToast('未找到可用的声音共享源', 'err');
    };
}
const actFile = $('chat-act-file');
if (actFile) {
    actFile.onclick = () => {
        const ep = localCatalog.endpoints.find((e) => e.kind === 'file');
        if (ep)
            openPublishModal(endpointIdStr(ep));
        else
            showToast('可通过命令行将文件公开为端点供节点订阅', 'info');
    };
}
// 设备搜索/过滤输入框事件
const filterInput = $('dev-filter-input');
const filterClear = $('dev-filter-clear');
if (filterInput) {
    filterInput.addEventListener('input', () => {
        deviceFilterQuery = filterInput.value.trim().toLowerCase();
        if (filterClear)
            filterClear.classList.toggle('hidden', !deviceFilterQuery);
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
// 播放器控制栏按钮事件绑定（静音、比例、全屏、缩放重置）
const muteBtn = $('recv-mute-btn');
if (muteBtn)
    muteBtn.onclick = () => toggleMute();
const zoomChip = $('player-zoom-chip');
if (zoomChip)
    zoomChip.onclick = () => resetZoomAndPan();
const recentClearBtn = $('recent-clear-btn');
if (recentClearBtn) {
    recentClearBtn.onclick = () => {
        localStorage.removeItem(LS_RECENT);
        manualRelays = [];
        renderRecent();
        void refreshDevices(true);
    };
}
const aspectBtn = $('player-aspect-btn');
if (aspectBtn)
    aspectBtn.onclick = () => cycleAspectRatio();
const quickAspectBtn = $('recv-aspect-quick-btn');
if (quickAspectBtn)
    quickAspectBtn.onclick = () => cycleAspectRatio();
const telemetryPill = $('player-telemetry-pill');
if (telemetryPill)
    telemetryPill.onclick = () => toggleDiagnosticsDrawer();
const diagCloseBtn = $('diag-close-btn');
if (diagCloseBtn)
    diagCloseBtn.onclick = () => toggleDiagnosticsDrawer();
initPlayerGestures();
initFSMUI();
// 原生返回键退出全屏（Android）→ 前端同步恢复全屏态。
void listen('native-fullscreen-changed', (p) => {
    void handleNativeFullscreenChanged(p.active);
});
// 尺寸/方向/滚动变化时重定位原生播放 Surface（Android Surface 路径；幂等）。
window.addEventListener('resize', () => { void syncAndroidSurface(); });
window.addEventListener('orientationchange', () => { void syncAndroidSurface(); });
window.addEventListener('scroll', () => { void syncAndroidSurface(); }, { passive: true });
// 全局播放器快捷键（F 全屏、M/Space 静音、D 诊断、A 比例、0 重置缩放、方向键 亮度/音量）
window.addEventListener('keydown', (e) => {
    const target = e.target;
    if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable)) {
        return;
    }
    switch (e.key.toLowerCase()) {
        case 'f':
            e.preventDefault();
            void togglePlayerFullscreen();
            break;
        case ' ':
        case 'm':
            e.preventDefault();
            toggleMute();
            break;
        case 'd':
            e.preventDefault();
            toggleDiagnosticsDrawer();
            break;
        case 'a':
            e.preventDefault();
            cycleAspectRatio();
            break;
        case '0':
            e.preventDefault();
            resetZoomAndPan();
            break;
        case 'arrowup':
            e.preventDefault();
            adjustVolume(0.05);
            break;
        case 'arrowdown':
            e.preventDefault();
            adjustVolume(-0.05);
            break;
        case 'arrowright':
            e.preventDefault();
            adjustBrightness(0.05);
            break;
        case 'arrowleft':
            e.preventDefault();
            adjustBrightness(-0.05);
            break;
    }
});
void init();
