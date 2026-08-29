"use strict";
// Stross 前端 —— 初始化与事件绑定（script 全局作用域，按依赖序最后加载）。
//
// 界面模型（节点 → 设备 → 端点：通告 + 订阅）：
//   左栏「设备」：本机设备树（通告/取消通告）+ 局域网设备目录（订阅端点）；
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
        const info = (await call('app_info'));
        IS_ANDROID = info.platform === 'android';
        $('ver-badge').textContent = 'v' + info.version;
        const fb = $('ffmpeg-badge');
        if (IS_ANDROID) {
            fb.textContent = '原生采集';
            fb.classList.add('ok');
        }
        else if (info.ffmpeg) {
            fb.textContent = 'ffmpeg';
            fb.classList.add('ok');
        }
        else {
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
        // 权限自动化：防火墙自检（缺放行则提示一键放行）+ 协商授权事件桥
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
        // 设备列表周期刷新（refreshDevices 自带 5s TTL + in-flight 守卫：
        // mDNS + 探测 + 聚合在 Rust `scan_devices` 内一次完成；数据未变不重建）
        void refreshDevices();
        // 本机目录（设备 + 已公开端点）周期刷新——通告状态徽标实时可见
        void refreshLocalCatalog();
    }, 2000);
}
// ---------------------------------------------------------------- 设备能力
async function loadDevices() {
    devices = (await call('list_devices'));
}
// ---------------------------------------------------------------- 权限自动化
/** 电脑端收到设备接入请求：展示授权确认弹窗（首次人工确认，信任门控）。 */
function onApproveRequest(req) {
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
async function respondApprove(allow) {
    if (!pendingApprove)
        return;
    const reqId = pendingApprove.id;
    const remember = $input('approve-remember');
    let grant = null;
    try {
        grant = (await call('negotiator_respond', {
            reqId,
            allow,
            remember: remember.checked,
        }));
    }
    catch (e) {
        $('approve-error').textContent = '应答失败：' + errMsg(e);
        $('approve-error').classList.remove('hidden');
        return;
    }
    pendingApprove = null;
    if (allow && grant && grant.streamId) {
        // 电脑端自动监听该会话流：出现在 /api/streams 即原生接收
        // （与「接收手机麦克风」共用同一等待-订阅链路；手机随后自动推流进入）
        beginAwaitMicStream(grant.streamId);
        $('approve-status').textContent = '已允许，等待手机接入…';
    }
    else {
        $('approve-status').textContent = allow ? '已允许并通知设备' : '已拒绝';
    }
    $('approve-modal').classList.add('hidden');
}
// ---------------------------------------------------------------- 事件绑定
// 设备列表事件委托：端点框架操作按钮（data-act：通告/取消通告/订阅）
$('device-list').addEventListener('click', (e) => {
    const t = e.target;
    const btn = t.closest('[data-act]');
    if (!btn)
        return;
    e.stopPropagation();
    switch (btn.dataset.act) {
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
});
// 设备接入授权确认（权限自动化：首次人工确认）
$btn('approve-allow-btn').onclick = () => void respondApprove(true);
$btn('approve-deny-btn').onclick = () => void respondApprove(false);
// 端点框架弹窗（通告 / 订阅）
$btn('pub-confirm-btn').onclick = () => void confirmPublish();
$btn('pub-cancel-btn').onclick = () => $('pub-modal').classList.add('hidden');
$('pub-modal').addEventListener('click', (e) => {
    if (e.target === $('pub-modal'))
        $('pub-modal').classList.add('hidden');
});
$btn('sub-confirm-btn').onclick = () => void confirmSubscribe();
$btn('sub-cancel-btn').onclick = () => $('sub-modal').classList.add('hidden');
$('sub-modal').addEventListener('click', (e) => {
    if (e.target === $('sub-modal'))
        $('sub-modal').classList.add('hidden');
});
// 接收面板：停止接收
$btn('recv-stop-btn').onclick = () => void stopReceive();
// 播放器控制条：全屏 / 停止
$btn('recv-fs-btn').onclick = () => void togglePlayerFullscreen();
$btn('recv-fs-stop-btn').onclick = () => void stopReceive();
// 双击画面切换全屏
$('recv-canvas').addEventListener('dblclick', () => void togglePlayerFullscreen());
// ESC 退出全屏（Tauri 窗口全屏不拦截 ESC，需前端处理）
window.addEventListener('keydown', (e) => {
    if (e.key === 'Escape')
        void exitPlayerFullscreen();
});
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
