"use strict";
// Stross 前端 —— DOM 助手与通用渲染（script 全局作用域，勿加 import/export）。
const $ = (id) => document.getElementById(id);
const $input = (id) => $(id);
const $select = (id) => $(id);
const $btn = (id) => $(id);
const invoke = window.__TAURI__?.core?.invoke;
/** invoke 的安全封装：非 Tauri 环境下返回明确错误而非未定义调用。 */
function call(cmd, args) {
    if (!invoke)
        return Promise.reject(new Error('当前页面未运行在 Stross 桌面应用中'));
    return invoke(cmd, args);
}
/** 统一错误消息提取：Tauri 命令失败时 rejection 是命令 Err 序列化的**字符串**
 *  （非 Error 对象），直接 `(e as Error).message` 会得到 undefined。
 *  覆盖 字符串 / Error / 其它（fallback String()）三种 rejection 形态。 */
function errMsg(e) {
    if (typeof e === 'string')
        return e;
    if (e instanceof Error)
        return e.message;
    return String(e);
}
/** 内联 SVG 图标（引用 index.html 雪碧图中的 <symbol>）。 */
function icon(name, cls = '') {
    return `<svg class="ic${cls ? ' ' + cls : ''}" viewBox="0 0 24 24" aria-hidden="true"><use href="#i-${name}"></use></svg>`;
}
/** 空状态占位组件（支持图标、主标题、辅助说明、可选行动按钮与错误样式）。 */
function emptyState(iconName, title, subText, action, isError = false) {
    const box = document.createElement('div');
    box.className = 'empty' + (isError ? ' empty-err' : '');
    const ic = document.createElement('div');
    ic.className = 'empty-ic';
    ic.innerHTML = icon(iconName);
    box.appendChild(ic);
    const textWrap = document.createElement('div');
    textWrap.className = 'empty-text-wrap';
    const p = document.createElement('p');
    p.className = 'empty-title' + (isError ? ' err-text' : '');
    p.textContent = title;
    textWrap.appendChild(p);
    if (subText) {
        const sub = document.createElement('p');
        sub.className = 'empty-sub';
        sub.textContent = subText;
        textWrap.appendChild(sub);
    }
    box.appendChild(textWrap);
    if (action) {
        const btn = document.createElement('button');
        btn.type = 'button';
        btn.className = 'sm empty-btn' + (isError ? ' danger' : ' primary');
        btn.innerHTML = (action.icon ? icon(action.icon) : '') + `<span>${action.label}</span>`;
        btn.onclick = (e) => {
            e.stopPropagation();
            action.onClick();
        };
        box.appendChild(btn);
    }
    return box;
}
/** 让列表项可点击且可键盘操作（Enter/Space 触发）。 */
function makeClickable(el, fn) {
    el.tabIndex = 0;
    el.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            fn();
        }
    });
    el.onclick = fn;
}
/** 按钮加载态：内嵌 spinner 并禁用；loading=false 恢复原内容。 */
function setBtnLoading(btn, loading) {
    if (loading) {
        if (btn.dataset.loading === '1')
            return;
        btn.dataset.loading = '1';
        btn.dataset.label = btn.innerHTML;
        btn.innerHTML = '<span class="spinner"></span>' + btn.textContent;
        btn.disabled = true;
    }
    else {
        if (btn.dataset.loading !== '1')
            return;
        delete btn.dataset.loading;
        btn.innerHTML = btn.dataset.label || '';
        delete btn.dataset.label;
        btn.disabled = false;
    }
}
/** 给错误框挂上「关闭」按钮并滚动到可见处。 */
function attachErrClose(box) {
    if (box.querySelector('.err-close'))
        return;
    const close = document.createElement('button');
    close.type = 'button';
    close.className = 'err-close';
    close.title = '关闭';
    close.innerHTML = icon('x');
    close.onclick = () => box.classList.add('hidden');
    box.appendChild(close);
    box.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
}
/** Tauri 事件监听（__TAURI__.event.listen）。 */
function listen(event, cb) {
    const api = window.__TAURI__?.event;
    if (!api?.listen)
        return Promise.resolve(() => { });
    return api.listen(event, (e) => cb(e.payload));
}
function canvasCtx() {
    const c = $('recv-canvas');
    return c.getContext('2d');
}
/** 绘制热路径缓存：ImageData 复用（尺寸不变时零重复分配）。
 *  `ImageData` 构造引用传入的 Uint8ClampedArray（不拷贝）；尺寸不变时
 *  `data.set` 拷贝像素到复用缓冲（不依赖 Channel 载荷的生命周期）。 */
let recvImg = null;
/** 当前视频位图尺寸（0 = 尚无帧）；供全屏自动旋转判断横/竖屏方向。 */
let recvVideoW = 0;
let recvVideoH = 0;
/** 把 RGBA 帧画到 canvas（宽度自适应，等比缩放）。
 *  `rgba` 为 Uint8Array（Rust 侧二进制 Channel 直传，无 base64/atob/
 *  逐字节拷贝——显示管线热路径）。 */
function drawReceiveFrame(w, h, rgba) {
    const ctx = canvasCtx();
    if (!ctx)
        return;
    recvVideoW = w;
    recvVideoH = h;
    const canvas = ctx.canvas;
    if (canvas.width !== w)
        canvas.width = w;
    if (canvas.height !== h)
        canvas.height = h;
    // 尺寸突变帧防御：字节数不符则跳过
    if (rgba.length !== w * h * 4)
        return;
    if (!recvImg || recvImg.width !== w || recvImg.height !== h) {
        recvImg = new ImageData(new Uint8ClampedArray(rgba.buffer, rgba.byteOffset, rgba.length), w, h);
    }
    else {
        recvImg.data.set(rgba);
    }
    ctx.putImageData(recvImg, 0, 0);
    // 控制条信息区：帧显示尺寸（仅变化时写 DOM）
    const info = $('recv-controls-info');
    const label = w + '×' + h;
    if (info.textContent !== label)
        info.textContent = label;
}
// ---------------------------------------------------------------- 播放器全屏
/** 把窗口级全屏状态应用到 UI：画布容器悬浮层 + 全屏按钮图标/标题 + 屏幕方向智能自适应。 */
function setPlayerFullscreen(fs) {
    fsActive = fs;
    $('recv-canvas-wrap').classList.toggle('fs', fs);
    const btn = $('recv-fs-btn');
    if (btn) {
        btn.title = fs ? '退出全屏' : '全屏';
        btn.innerHTML = icon(fs ? 'minimize' : 'maximize');
    }
    // 移动端/Android 智能方向自适应：根据当前视频流宽高比自动旋转
    if (IS_ANDROID) {
        try {
            if (fs) {
                const isLandscape = recvVideoW > 0 && recvVideoH > 0 ? recvVideoW >= recvVideoH : true;
                void call('set_screen_orientation', { orientation: isLandscape ? 'landscape' : 'portrait' });
            }
            else {
                void call('set_screen_orientation', { orientation: 'unspecified' });
            }
        }
        catch { }
    }
}
/** 切换播放器全屏：先查询窗口实际全屏态再取反（状态校准，防失配），
 *  然后**先应用 CSS 层全屏**再尝试 OS 窗口级全屏。
 *  - CSS 层（.canvas-wrap.fs 悬浮层）跨平台可靠；
 *  - OS 窗口级全屏在 Android WebView 无 `setFullscreen`（Web 层 Fullscreen API
 *    也不可靠），抛错时不能提前 return——否则全屏按钮点了没反应。
 *  非 Tauri 环境安全 no-op（仍退化为 CSS 全屏）。 */
async function togglePlayerFullscreen() {
    const win = window.__TAURI__?.window?.getCurrentWindow();
    // 读实际窗口全屏态校准（读失败沿用本地状态）
    let fs = fsActive;
    if (win) {
        try {
            fs = await win.isFullscreen();
        }
        catch { }
    }
    const next = !fs;
    // CSS 层全屏先行——即使 OS 窗口级全屏失败也立即生效
    setPlayerFullscreen(next);
    // Android Surface 路径：原生 SurfaceView 铺满 + 隐藏系统栏接管全屏
    if (IS_ANDROID && hasActiveVideo()) {
        try {
            await call('set_native_fullscreen', { active: next });
        }
        catch { }
        if (!next) {
            // 退出全屏：恢复系统栏后把 Surface 重新定位回播放区
            try {
                await call('set_screen_orientation', { orientation: 'unspecified' });
            }
            catch { }
            void syncAndroidSurface();
        }
    }
    if (!win)
        return;
    try {
        await win.setFullscreen(next);
    }
    catch { }
}
/** 退出播放器全屏（ESC / 停止接收时调用）。 */
async function exitPlayerFullscreen() {
    if (!fsActive)
        return;
    const win = window.__TAURI__?.window?.getCurrentWindow();
    if (IS_ANDROID) {
        try {
            await call('set_native_fullscreen', { active: false });
        }
        catch { }
    }
    if (!win)
        return;
    try {
        await win.setFullscreen(false);
    }
    catch { }
    setPlayerFullscreen(false);
    if (IS_ANDROID)
        void syncAndroidSurface();
}
/** 原生返回键退出全屏（Android）→ 前端同步恢复全屏态并重定位 surface。 */
async function handleNativeFullscreenChanged(active) {
    if (active)
        return;
    if (!IS_ANDROID)
        return;
    setPlayerFullscreen(false);
    void syncAndroidSurface();
}
// ---------------------------------------------------------------- 移动端 Tab 切换
/** 切换全局主视图模式（设备与共享管理 vs 消费播放台）——经过状态机派发。 */
function switchView(mode) {
    dispatchUIAction({ type: 'SWITCH_VIEW', mode });
    void syncAndroidSurface();
}
/** 兼容旧移动端分段 Tab。 */
function switchMobileTab(tab) {
    switchView(tab === 'recv' ? 'consume' : 'manage');
}
/** 状态机驱动的全局 DOM 响应式同步器。 */
function initFSMUI() {
    subscribeUIFSM((state) => {
        const viewManage = $('view-manage');
        const viewConsume = $('view-consume');
        if (viewManage)
            viewManage.classList.toggle('active', state.viewMode === 'manage');
        if (viewConsume)
            viewConsume.classList.toggle('active', state.viewMode === 'consume');
        const btnManage = $('nav-btn-manage');
        const btnConsume = $('nav-btn-consume');
        if (btnManage)
            btnManage.classList.toggle('active', state.viewMode === 'manage');
        if (btnConsume)
            btnConsume.classList.toggle('active', state.viewMode === 'consume');
        void syncAndroidSurface();
        const empty = $('recv-empty');
        const canvasWrap = $('recv-canvas-wrap');
        const overlay = $('recv-overlay');
        const audioViz = $('recv-audio-viz');
        const aiBar = $('player-ai-bar');
        switch (state.playerMode) {
            case 'empty':
                if (empty)
                    empty.classList.remove('hidden');
                if (canvasWrap)
                    canvasWrap.classList.add('hidden');
                if (overlay)
                    overlay.classList.add('hidden');
                if (audioViz)
                    audioViz.classList.add('hidden');
                if (aiBar)
                    aiBar.classList.add('hidden');
                break;
            case 'buffering':
                if (empty)
                    empty.classList.add('hidden');
                if (canvasWrap)
                    canvasWrap.classList.remove('hidden');
                if (overlay)
                    overlay.classList.remove('hidden');
                if (audioViz)
                    audioViz.classList.add('hidden');
                if (aiBar)
                    aiBar.classList.remove('hidden');
                break;
            case 'videoOnly':
            case 'audioVisualMix':
                if (empty)
                    empty.classList.add('hidden');
                if (canvasWrap)
                    canvasWrap.classList.remove('hidden');
                if (overlay)
                    overlay.classList.add('hidden');
                if (audioViz)
                    audioViz.classList.add('hidden');
                if (aiBar)
                    aiBar.classList.remove('hidden');
                break;
            case 'audioOnly':
                if (empty)
                    empty.classList.add('hidden');
                if (canvasWrap)
                    canvasWrap.classList.add('hidden');
                if (overlay)
                    overlay.classList.add('hidden');
                if (audioViz)
                    audioViz.classList.remove('hidden');
                if (aiBar)
                    aiBar.classList.remove('hidden');
                break;
        }
    });
}
// ---------------------------------------------------------------- 提示
function showFatal(msg) {
    const box = $('error-box');
    if (!box)
        return;
    box.textContent = msg;
    box.classList.remove('hidden');
    attachErrClose(box);
}
function hideError() {
    const box = $('error-box');
    if (box)
        box.classList.add('hidden');
}
function showGridError(msg) {
    const box = $('grid-error');
    if (!box)
        return;
    box.textContent = msg;
    box.classList.remove('hidden');
    attachErrClose(box);
}
function hideGridError() {
    const box = $('grid-error');
    if (box)
        box.classList.add('hidden');
}
function showRecvError(msg) {
    const box = $('recv-error');
    if (!box)
        return;
    box.textContent = msg;
    box.classList.remove('hidden');
    attachErrClose(box);
}
function hideRecvError() {
    const box = $('recv-error');
    if (box)
        box.classList.add('hidden');
}
let lastToastMsg = '';
let lastToastTime = 0;
/** 浮动 Toast 吐司提示（自动去重与排队防抖）。 */
function showToast(msg, kind = 'info', durationMs = 2800) {
    const now = Date.now();
    // 1.5 秒内相同内容去重，避免网络轮询或多流事件并发触发大量重复弹窗
    if (msg === lastToastMsg && now - lastToastTime < 1500)
        return;
    lastToastMsg = msg;
    lastToastTime = now;
    const container = $('toast-container');
    if (!container)
        return;
    const toast = document.createElement('div');
    toast.className = `toast toast-${kind}`;
    const iconName = kind === 'ok' ? 'check-circle' : kind === 'err' ? 'x' : 'info';
    toast.innerHTML = `<span class="toast-ic">${icon(iconName)}</span><span class="toast-msg">${msg}</span>`;
    container.appendChild(toast);
    setTimeout(() => {
        toast.classList.add('toast-leaving');
        setTimeout(() => toast.remove(), 240);
    }, durationMs);
}
/** 复制文本到剪贴板并弹出 Toast 提示。 */
async function copyText(text, label = '已复制') {
    try {
        if (navigator.clipboard && navigator.clipboard.writeText) {
            await navigator.clipboard.writeText(text);
        }
        else {
            const ta = document.createElement('textarea');
            ta.value = text;
            ta.style.position = 'fixed';
            ta.style.opacity = '0';
            document.body.appendChild(ta);
            ta.select();
            document.execCommand('copy');
            ta.remove();
        }
        showToast(label, 'ok');
    }
    catch {
        showToast('复制失败', 'err');
    }
}
/** 控制纯音频可视化显示/隐藏。 */
function showAudioVisualizer(active, title, sub) {
    const viz = $('recv-audio-viz');
    if (!viz)
        return;
    if (active) {
        if (title)
            $('recv-audio-title').textContent = title;
        if (sub)
            $('recv-audio-sub').textContent = sub;
        viz.classList.remove('hidden');
    }
    else {
        viz.classList.add('hidden');
    }
}
let currentAspectRatio = 'fit';
const ASPECT_LABELS = {
    fit: '等比适应',
    cover: '居中铺满',
    fill: '拉伸铺满',
    original: '1:1 原生',
};
function cycleAspectRatio() {
    const modes = ['fit', 'cover', 'fill', 'original'];
    const nextIdx = (modes.indexOf(currentAspectRatio) + 1) % modes.length;
    setAspectRatio(modes[nextIdx]);
}
function setAspectRatio(mode) {
    currentAspectRatio = mode;
    const wrap = $('recv-canvas-wrap');
    if (wrap) {
        wrap.classList.remove('aspect-fit', 'aspect-cover', 'aspect-fill', 'aspect-original');
        wrap.classList.add(`aspect-${mode}`);
    }
    const label = $('player-aspect-label');
    if (label)
        label.textContent = ASPECT_LABELS[mode];
    const modes = ['fit', 'cover', 'fill', 'original'];
    showGestureHud('ratio', `画面比例: ${ASPECT_LABELS[mode]}`, modes.indexOf(mode) * 33.3);
}
let gestureHudTimer = undefined;
function showGestureHud(iconName, text, percent) {
    const hud = $('player-gesture-hud');
    if (!hud)
        return;
    const icEl = $('gesture-hud-icon');
    if (icEl)
        icEl.innerHTML = icon(iconName);
    const txtEl = $('gesture-hud-text');
    if (txtEl)
        txtEl.textContent = text;
    const barWrap = $('gesture-bar-wrap');
    const barFill = $('gesture-bar-fill');
    if (barWrap && barFill) {
        if (percent !== undefined) {
            barWrap.style.display = 'block';
            barFill.style.width = Math.min(100, Math.max(0, percent)) + '%';
        }
        else {
            barWrap.style.display = 'none';
        }
    }
    hud.classList.remove('hidden');
    clearTimeout(gestureHudTimer);
    gestureHudTimer = window.setTimeout(() => {
        hud.classList.add('hidden');
    }, 1200);
}
let playerBrightness = 1.0;
let playerVolume = 1.0;
let playerZoom = 1.0;
let panX = 0;
let panY = 0;
function updateCanvasTransform() {
    const canvas = $('recv-canvas');
    if (canvas) {
        if (playerZoom === 1.0 && panX === 0 && panY === 0) {
            canvas.style.transform = '';
        }
        else {
            canvas.style.transform = `scale(${playerZoom}) translate3d(${panX}px, ${panY}px, 0)`;
        }
    }
    updateZoomChipUI();
}
/** 画面缩放浮动芯片状态同步。 */
function updateZoomChipUI() {
    const chip = $('player-zoom-chip');
    if (!chip)
        return;
    const isZoomed = playerZoom > 1.05 || panX !== 0 || panY !== 0;
    chip.classList.toggle('hidden', !isZoomed);
    const textEl = $('player-zoom-chip-text');
    if (textEl && isZoomed) {
        textEl.textContent = `${playerZoom.toFixed(1)}x · 重置`;
    }
}
/** 静音按钮图标与状态文字同步。 */
function updateMuteButtonUI() {
    const muteBtn = $('recv-mute-btn');
    const muteIc = $('recv-mute-ic');
    if (!muteBtn || !muteIc)
        return;
    const isMuted = playerVolume <= 0.001;
    muteBtn.title = isMuted ? '恢复声音 (快捷键 M / 空格)' : '静音 (快捷键 M / 空格)';
    muteIc.innerHTML = `<use href="#i-${isMuted ? 'volume-x' : 'volume-2'}"></use>`;
}
let fsInactivityTimer = undefined;
/** 重置全屏控制栏自动隐藏倒计时。 */
function resetFsInactivityTimer() {
    const wrap = $('recv-canvas-wrap');
    if (!wrap)
        return;
    wrap.classList.remove('controls-hidden');
    clearTimeout(fsInactivityTimer);
    if (fsActive) {
        fsInactivityTimer = window.setTimeout(() => {
            if (fsActive) {
                wrap.classList.add('controls-hidden');
            }
        }, 3000);
    }
}
function initPlayerGestures() {
    const wrap = $('recv-canvas-wrap');
    if (!wrap)
        return;
    let startX = 0;
    let startY = 0;
    let isLeft = false;
    let isDragging = false;
    let startDistance = 0;
    let initialZoom = 1.0;
    let lastTapTime = 0;
    // 全屏交互时指针移动重置自动隐藏倒计时
    wrap.addEventListener('mousemove', () => resetFsInactivityTimer());
    wrap.addEventListener('pointerdown', () => resetFsInactivityTimer());
    wrap.addEventListener('touchstart', (e) => {
        resetFsInactivityTimer();
        if (e.touches.length === 1) {
            const now = Date.now();
            if (now - lastTapTime < 300) {
                if (playerZoom !== 1.0 || panX !== 0 || panY !== 0) {
                    resetZoomAndPan();
                }
                else {
                    cycleAspectRatio();
                }
                lastTapTime = 0;
                return;
            }
            lastTapTime = now;
            const t = e.touches[0];
            const rect = wrap.getBoundingClientRect();
            startX = t.clientX;
            startY = t.clientY;
            isLeft = t.clientX - rect.left < rect.width / 2;
            isDragging = true;
        }
        else if (e.touches.length === 2) {
            isDragging = false;
            const dx = e.touches[0].clientX - e.touches[1].clientX;
            const dy = e.touches[0].clientY - e.touches[1].clientY;
            startDistance = Math.hypot(dx, dy);
            initialZoom = playerZoom;
        }
    }, { passive: true });
    wrap.addEventListener('touchmove', (e) => {
        resetFsInactivityTimer();
        if (e.touches.length === 1 && isDragging) {
            const t = e.touches[0];
            const dx = t.clientX - startX;
            const dy = t.clientY - startY;
            if (playerZoom > 1.0) {
                // 放大状态下：单指拖拽平移视口
                const maxPanX = (wrap.clientWidth * (playerZoom - 1)) / (2 * playerZoom);
                const maxPanY = (wrap.clientHeight * (playerZoom - 1)) / (2 * playerZoom);
                panX = Math.min(maxPanX, Math.max(-maxPanX, panX + dx / playerZoom));
                panY = Math.min(maxPanY, Math.max(-maxPanY, panY + dy / playerZoom));
                startX = t.clientX;
                startY = t.clientY;
                updateCanvasTransform();
            }
            else if (Math.abs(dy) > 8) {
                // 正常尺寸下：左屏上下滑亮度，右屏上下滑音量
                if (isLeft) {
                    adjustBrightness(dy < 0 ? 0.03 : -0.03);
                }
                else {
                    adjustVolume(dy < 0 ? 0.03 : -0.03);
                }
                startY = t.clientY;
            }
        }
        else if (e.touches.length === 2 && startDistance > 0) {
            const dx = e.touches[0].clientX - e.touches[1].clientX;
            const dy = e.touches[0].clientY - e.touches[1].clientY;
            const dist = Math.hypot(dx, dy);
            const factor = dist / startDistance;
            playerZoom = Math.min(3.0, Math.max(1.0, initialZoom * factor));
            if (playerZoom === 1.0) {
                panX = 0;
                panY = 0;
            }
            updateCanvasTransform();
            showGestureHud('maximize', `缩放 ${playerZoom.toFixed(1)}x`, (playerZoom - 1) * 50);
        }
    }, { passive: true });
    wrap.addEventListener('touchend', () => {
        isDragging = false;
        startDistance = 0;
    });
}
function adjustBrightness(delta) {
    playerBrightness = Math.min(1.5, Math.max(0.3, playerBrightness + delta));
    const canvas = $('recv-canvas');
    if (canvas)
        canvas.style.filter = `brightness(${playerBrightness})`;
    const pct = Math.round(((playerBrightness - 0.3) / 1.2) * 100);
    showGestureHud('sun', `亮度 ${pct}%`, pct);
}
function adjustVolume(delta) {
    playerVolume = Math.min(1.0, Math.max(0.0, playerVolume + delta));
    const pct = Math.round(playerVolume * 100);
    showGestureHud(playerVolume > 0 ? 'volume-2' : 'volume-x', `音量 ${pct}%`, pct);
    updateMuteButtonUI();
}
function resetZoomAndPan() {
    playerZoom = 1.0;
    panX = 0;
    panY = 0;
    updateCanvasTransform();
    showGestureHud('maximize', '缩放已重置 (1.0x)', 0);
}
function toggleMute() {
    if (playerVolume > 0.001) {
        playerVolume = 0;
        showGestureHud('volume-x', '已静音', 0);
    }
    else {
        playerVolume = 1.0;
        showGestureHud('volume-2', '音量 100%', 100);
    }
    updateMuteButtonUI();
}
function toggleDiagnosticsDrawer() {
    const drawer = $('player-diag-drawer');
    if (drawer)
        drawer.classList.toggle('hidden');
}
const winObj = window;
winObj.showToast = showToast;
winObj.copyText = copyText;
winObj.showAudioVisualizer = showAudioVisualizer;
winObj.switchView = switchView;
winObj.switchMobileTab = switchMobileTab;
winObj.cycleAspectRatio = cycleAspectRatio;
winObj.setAspectRatio = setAspectRatio;
winObj.initPlayerGestures = initPlayerGestures;
winObj.toggleDiagnosticsDrawer = toggleDiagnosticsDrawer;
winObj.adjustBrightness = adjustBrightness;
winObj.adjustVolume = adjustVolume;
winObj.resetZoomAndPan = resetZoomAndPan;
winObj.toggleMute = toggleMute;
winObj.updateMuteButtonUI = updateMuteButtonUI;
winObj.updateZoomChipUI = updateZoomChipUI;
