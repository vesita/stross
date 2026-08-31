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
/** 空状态占位（图标 + 文案，可选错误配色）。 */
function emptyState(iconName, text, isError = false) {
    const box = document.createElement('div');
    box.className = 'empty';
    const ic = document.createElement('span');
    ic.innerHTML = icon(iconName);
    const p = document.createElement('p');
    if (isError)
        p.className = 'err-text';
    p.textContent = text;
    box.appendChild(ic);
    box.appendChild(p);
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
    const canvas = ctx.canvas;
    if (canvas.width !== w)
        canvas.width = w;
    if (canvas.height !== h)
        canvas.height = h;
    // 尺寸突变帧防御：字节数不符则跳过（否则 ImageData 构造抛 RangeError，
    // 且发生在事件回调内无捕获，会中断整帧处理）
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
/** 把窗口级全屏状态应用到 UI：画布容器悬浮层 + 全屏按钮图标/标题。 */
function setPlayerFullscreen(fs) {
    fsActive = fs;
    $('recv-canvas-wrap').classList.toggle('fs', fs);
    const btn = $('recv-fs-btn');
    btn.title = fs ? '退出全屏' : '全屏';
    btn.innerHTML = icon(fs ? 'minimize' : 'maximize');
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
    if (!win)
        return;
    try {
        await win.setFullscreen(false);
    }
    catch { }
    setPlayerFullscreen(false);
}
// ---------------------------------------------------------------- 移动端 Tab 切换
/** 切换全局主视图模式（设备与共享管理 vs 消费播放台）。 */
function switchView(mode) {
    activeViewMode = mode;
    const viewManage = $('view-manage');
    const viewConsume = $('view-consume');
    if (viewManage)
        viewManage.classList.toggle('active', mode === 'manage');
    if (viewConsume)
        viewConsume.classList.toggle('active', mode === 'consume');
    const btnManage = $('nav-btn-manage');
    const btnConsume = $('nav-btn-consume');
    if (btnManage)
        btnManage.classList.toggle('active', mode === 'manage');
    if (btnConsume)
        btnConsume.classList.toggle('active', mode === 'consume');
}
/** 兼容旧移动端分段 Tab。 */
function switchMobileTab(tab) {
    switchView(tab === 'recv' ? 'consume' : 'manage');
}
// ---------------------------------------------------------------- 提示
function showFatal(msg) {
    const box = $('error-box');
    box.textContent = msg;
    box.classList.remove('hidden');
    attachErrClose(box);
}
function hideError() {
    $('error-box').classList.add('hidden');
}
function showGridError(msg) {
    const box = $('grid-error');
    box.textContent = msg;
    box.classList.remove('hidden');
    attachErrClose(box);
}
function hideGridError() {
    $('grid-error').classList.add('hidden');
}
function showRecvError(msg) {
    const box = $('recv-error');
    box.textContent = msg;
    box.classList.remove('hidden');
    attachErrClose(box);
}
function hideRecvError() {
    $('recv-error').classList.add('hidden');
}
/** 浮动 Toast 吐司提示。 */
function showToast(msg, kind = 'info', durationMs = 3000) {
    const container = $('toast-container');
    if (!container)
        return;
    const toast = document.createElement('div');
    toast.className = `toast toast-${kind}`;
    const iconName = kind === 'ok' ? 'check-circle' : kind === 'err' ? 'x' : 'info';
    toast.innerHTML = `<span class="toast-ic">${icon(iconName)}</span><span class="toast-msg">${msg}</span>`;
    container.appendChild(toast);
    setTimeout(() => {
        toast.style.opacity = '0';
        toast.style.transform = 'translateY(10px)';
        setTimeout(() => toast.remove(), 250);
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
const winObj = window;
winObj.showToast = showToast;
winObj.copyText = copyText;
winObj.showAudioVisualizer = showAudioVisualizer;
winObj.switchView = switchView;
winObj.switchMobileTab = switchMobileTab;
