"use strict";
// Stross 推流端控制界面（Tauri 前端，TypeScript 源文件，唯一真源）。
//
// 生成 app.js：`npx tsc -p apps/stross-gui/web/tsconfig.json`
// （app.js 是构建产物，提交进仓库——Tauri 直接加载，Rust 构建不依赖 node）。
// 修改本文件后必须重新生成 app.js 并提交两者。
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
/** 显示视图/面板并播放淡入动画（重进时重启动画）。 */
function showView(el) {
    el.classList.remove('hidden');
    el.classList.remove('view-enter');
    void el.offsetWidth; // 强制 reflow，重启动画
    el.classList.add('view-enter');
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
const QUALITIES = {
    LOW: { width: 640, height: 360, fps: 24, bitrateKbps: 800 },
    MEDIUM: { width: 1280, height: 720, fps: 30, bitrateKbps: 2500 },
    HIGH: { width: 1920, height: 1080, fps: 30, bitrateKbps: 6000 },
};
const LS_RELAY = 'stross.lastRelay';
const LS_TITLE = 'stross.lastTitle';
const LS_RECENT = 'stross.recentRelays';
let devices = { cameras: [], audioInputs: [], systemAudio: [] };
let running = false;
let starting = false; // Android 采集启动中（等待真实状态回报）
let startingSince = 0; // 启动开始时间戳（超时兜底用）
const START_TIMEOUT_MS = 60000; // 采集启动超时
/** 本机锚点（免先连：init 自动 `start_relay`；推流/级联兜底的数据面入口）。 */
let anchor = null;
/** 自动发现卡片选中的接收目标中继（null = 本机锚点）。 */
let targetRelay = null;
/** 网格页选中的设备（relayBase 键；只看该设备的串流，null = 全部设备）。 */
let selectedDevice = null;
/** 手动添加的设备地址（http://host:port，免 mDNS；与最近历史共享持久化）。 */
let manualRelays = [];
/** 流 id → 流信息缓存（传输自动选择按 video/audio 类型决策）。 */
const remoteStreams = new Map();
let currentTab = 'grid';
let IS_ANDROID = false;
let MY_IPS = [];
// —— 交互状态 ——
let statusTimer = null; // 状态轮询句柄（应用打开期间常驻）
let scanInFlight = false; // 「扫描局域网设备」in-flight
let discoverInFlight = false; // 「扫描局域网串流」in-flight
let discoverCacheAt = 0; // 发现结果缓存时间（TTL 防重复扫描）
const DISCOVER_TTL_MS = 5000;
let streamsCache = null; // 本机锚点串流列表缓存
const STREAMS_TTL_MS = 3000;
// ---------------------------------------------------------------- 初始化
async function init() {
    if (!invoke) {
        showFatal('当前页面未运行在 Stross 桌面应用中。\n请通过 `cargo tauri dev` 或安装包启动。');
        return;
    }
    try {
        const info = (await call('app_info'));
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
        }
        else if (info.ffmpeg) {
            fb.textContent = 'ffmpeg';
            fb.classList.add('ok');
        }
        else {
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
    }
    catch (e) {
        showFatal(String(e));
    }
}
/** 恢复上次的地址/流名称偏好，并渲染手动添加历史。 */
function restorePrefs() {
    const last = localStorage.getItem(LS_RELAY);
    if (last)
        $input('manual-addr').value = last;
    const title = localStorage.getItem(LS_TITLE);
    if (title)
        $input('title-input').value = title;
    manualRelays = getRecent();
    renderRecent();
}
function savePrefs() {
    localStorage.setItem(LS_RELAY, $input('manual-addr').value.trim());
    localStorage.setItem(LS_TITLE, $input('title-input').value.trim());
}
// ---------------- 手动添加历史 ----------------
function getRecent() {
    try {
        return JSON.parse(localStorage.getItem(LS_RECENT) || '[]');
    }
    catch {
        return [];
    }
}
function saveRecent(url) {
    const list = getRecent().filter((u) => u !== url);
    list.unshift(url);
    localStorage.setItem(LS_RECENT, JSON.stringify(list.slice(0, 5)));
}
/** 删除一条手动添加历史（不触发添加），空列表时隐藏区块。 */
function removeRecent(url) {
    const list = getRecent().filter((u) => u !== url);
    localStorage.setItem(LS_RECENT, JSON.stringify(list));
    manualRelays = list;
    renderRecent();
}
/** 渲染"手动添加历史"：点击重新添加该设备，右侧 ✕ 删除单条记录。 */
function renderRecent() {
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
        const main = document.createElement('span');
        main.className = 'recent-main';
        main.textContent = u;
        main.title = '点击重新添加';
        makeClickable(main, () => {
            $input('manual-addr').value = u;
            void addManualRelay();
        });
        const del = document.createElement('button');
        del.type = 'button';
        del.className = 'recent-del';
        del.title = '删除该记录';
        del.setAttribute('aria-label', '删除 ' + u);
        del.innerHTML = icon('x');
        del.onclick = (e) => {
            e.stopPropagation(); // 不触发添加
            removeRecent(u);
        };
        li.appendChild(main);
        li.appendChild(del);
        ul.appendChild(li);
    });
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
// ---------------------------------------------------------------- 本机锚点
function normAddr(addr) {
    let a = addr.trim();
    if (!a)
        return null;
    if (!/^https?:\/\//i.test(a))
        a = 'http://' + a;
    return a.replace(/\/+$/, '');
}
/** 头部锚点徽标状态：锚定中 / 已锚定 / 失败。 */
function setAnchorBadge(state) {
    const badge = $('anchor-badge');
    badge.className = 'badge' + (state === 'ok' ? ' ok' : state === 'err' ? ' err' : '');
    badge.textContent = state === 'ok' ? '已锚定' : state === 'err' ? '锚定失败' : '锚定中…';
}
/** 免先连核心：自动锚定本机（`start_relay` 幂等，启动受控中继 + mDNS 广播）。 */
async function ensureAnchor() {
    const box = $('anchor-box');
    box.classList.remove('err');
    box.innerHTML = '<span class="spinner"></span><span>锚定中…</span>';
    setAnchorBadge('anchoring');
    try {
        const info = (await call('start_relay'));
        anchor = {
            port: info.port,
            urls: info.urls,
            srtUrl: null,
            quicUrl: null,
        };
        renderAnchor(info);
        setAnchorBadge('ok');
        void refreshAnchorPorts();
    }
    catch (e) {
        anchor = null;
        setAnchorBadge('err');
        box.classList.add('err');
        box.innerHTML = '';
        box.appendChild(emptyState('server', '本机锚定失败：' + e.message + '（推流不可用；仍可观看局域网串流）', true));
        const retry = document.createElement('button');
        retry.type = 'button';
        retry.innerHTML = icon('refresh') + '<span>重试锚定</span>';
        retry.onclick = () => void ensureAnchor();
        box.appendChild(retry);
    }
}
/** 网格页「本机锚点」卡片：端口 + 可复制的入口地址。 */
function renderAnchor(info) {
    const box = $('anchor-box');
    box.innerHTML = '';
    const line = document.createElement('div');
    line.className = 'anchor-line';
    line.innerHTML = icon('server') + '<span>已锚定 · 中继端口 ' + info.port + ' · mDNS 广播中</span>';
    box.appendChild(line);
    const ul = document.createElement('ul');
    ul.className = 'url-list';
    info.urls.forEach((u) => ul.appendChild(urlListItem(u)));
    box.appendChild(ul);
}
/** 拉取本机锚点 `/api/info`，填充 SRT/QUIC 拨号地址（失败静默，退回 WS）。 */
async function refreshAnchorPorts() {
    if (!anchor)
        return;
    try {
        const resp = await fetch(`http://127.0.0.1:${anchor.port}/api/info`, { cache: 'no-store' });
        if (!resp.ok)
            return;
        const info = (await resp.json());
        if (info.srtPort)
            anchor.srtUrl = `srt://127.0.0.1:${info.srtPort}`;
        if (info.quicPort)
            anchor.quicUrl = `quic://127.0.0.1:${info.quicPort}`;
    }
    catch (_) {
        // 中继可能不支持 /api/info（旧版本）：保持 null，观看端走 WS
    }
}
/** 手动添加设备地址（免 mDNS）：探测可达后进入设备/串流聚合。 */
async function addManualRelay() {
    hideGridError();
    const addr = normAddr($input('manual-addr').value);
    if (!addr) {
        showGridError('请输入设备地址，例如 http://192.168.1.100:8777');
        return;
    }
    savePrefs();
    saveRecent(addr);
    // 探测中继是否可达（/api/streams 是受控/普通中继都提供的只读端点）
    try {
        const resp = await fetch(addr + '/api/streams', { cache: 'no-store' });
        if (!resp.ok)
            throw new Error('中继返回 HTTP ' + resp.status);
        await resp.json();
    }
    catch (e) {
        showGridError('无法访问 ' + addr + '：' + e.message);
        return;
    }
    manualRelays = [addr, ...manualRelays.filter((u) => u !== addr)];
    renderRecent();
    void scanRelays(); // 设备列表出现该设备
    void scanRemoteStreams(true); // 强制刷新串流聚合（含手动设备）
}
/** 当前接收目标中继（点选的局域网设备优先，否则本机锚点；均无则 null）。 */
function currentRelay() {
    if (targetRelay)
        return targetRelay;
    if (!anchor)
        return null;
    return {
        wsBase: `ws://127.0.0.1:${anchor.port}`,
        srtUrl: anchor.srtUrl,
        quicUrl: anchor.quicUrl,
    };
}
/** 按流媒体类型自动选传输（auto 模式）：
 *  含视频 → SRT（Adaptive：丢包不阻塞、关键帧自愈）> QUIC > WS
 *  纯音频 → QUIC（无损：音频不可丢）> WS */
function autoRelayUrl(stream) {
    const r = currentRelay();
    if (!r)
        return '';
    const hasVideo = !!(stream && stream.video);
    if (hasVideo) {
        if (r.srtUrl)
            return r.srtUrl;
        if (r.quicUrl)
            return r.quicUrl;
    }
    else if (r.quicUrl) {
        return r.quicUrl;
    }
    return r.wsBase;
}
/** 按「接收传输」下拉 + 流媒体类型构造 relay 拨号地址；UDP 端口不可用回退 WS。 */
function pickRelayUrl(stream) {
    const sel = $select('recv-transport-select').value;
    const r = currentRelay();
    if (!r)
        return '';
    if (sel === 'srt' && r.srtUrl)
        return r.srtUrl;
    if (sel === 'quic' && r.quicUrl)
        return r.quicUrl;
    if (sel === 'auto')
        return autoRelayUrl(stream);
    if (sel === 'srt' || sel === 'quic') {
        showRecvError(`该中继未提供 ${sel.toUpperCase()} 端口（/api/info 不可用），已回退 WebSocket`);
    }
    return r.wsBase;
}
/** 流类型小标签（视频/音频 chip）。 */
function trackChips(s) {
    const wrap = document.createElement('span');
    wrap.className = 'chips';
    if (s.video)
        wrap.appendChild(chipEl('video', '视频'));
    if (s.audio)
        wrap.appendChild(chipEl('audio', '音频'));
    return wrap;
}
function chipEl(kind, label) {
    const c = document.createElement('span');
    c.className = 'chip ' + kind;
    c.innerHTML = icon(kind === 'audio' ? 'music' : 'video') + '<span>' + label + '</span>';
    return c;
}
/** 观看人数（眼睛图标 + 数字）。 */
function watcherCount(n) {
    const w = document.createElement('span');
    w.className = 'watchers';
    w.innerHTML = icon('eye') + '<span>' + n + ' 人观看</span>';
    return w;
}
/** 进入主界面（免先连：init 锚定后直接调用，无连接门槛）。 */
function showApp() {
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
function startStatusPolling() {
    if (statusTimer !== null)
        return;
    statusTimer = window.setInterval(() => void pollStatus(), 2000);
}
// ---------------------------------------------------------------- 模式切换
function setTab(tab) {
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
async function loadDevices() {
    devices = (await call('list_devices'));
    fillSelect($select('camera-select'), devices.cameras.map((c) => ({ value: c.id, label: c.name })), '使用默认摄像头');
    fillSelect($select('mic-select'), devices.audioInputs.map((n) => ({ value: n, label: n })), '系统默认输入');
    fillSelect($select('sys-select'), devices.systemAudio.map((n) => ({ value: n, label: n })), '未发现回环设备');
    $('mic-hint').textContent = devices.audioInputs.length ? '' : '未发现麦克风（仍会使用系统默认输入）';
    $('sys-hint').textContent = devices.systemAudio.length
        ? ''
        : '未发现回环设备（Linux 需 PulseAudio monitor；Windows 需启用"立体声混音"）';
}
function fillSelect(sel, items, emptyLabel) {
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
function renderIps(ips) {
    const ul = $('ip-list');
    ul.innerHTML = '';
    ips.forEach((ip) => {
        const li = document.createElement('li');
        li.textContent = ip;
        li.title = '点击复制';
        makeClickable(li, () => {
            navigator.clipboard?.writeText(ip).then(() => {
                li.style.borderColor = 'var(--ok)';
                li.textContent = '已复制 ' + ip;
                setTimeout(() => {
                    li.style.borderColor = '';
                    li.textContent = ip;
                }, 1500);
            });
        });
        ul.appendChild(li);
    });
    if (!ips.length)
        ul.innerHTML = '<li class="hint">未获取到局域网 IP</li>';
}
// ---------------------------------------------------------------- 扫描局域网
/** 角色英文 → 中文显示（mDNS TXT `roles`）。 */
const ROLE_LABELS = {
    sender: '推流',
    viewer: '观看',
    relay: '中继',
};
function roleLabel(r) {
    return ROLE_LABELS[r] || r;
}
/** 角色小标签 chip。 */
function roleChip(role) {
    const c = document.createElement('span');
    c.className = 'chip role';
    c.textContent = roleLabel(role);
    return c;
}
/** 归一化设备基址：取 urls[0] 并去掉尾部斜杠。 */
function deviceBase(r) {
    return (r.urls[0] || '').replace(/\/+$/, '');
}
/** 扫描局域网内其它设备（mDNS + 手动添加）；打开应用时自动执行，也可手动重扫。
 *  点设备卡片切换「只看该设备串流」（「局域网串流」联动过滤）。 */
async function scanRelays() {
    if (scanInFlight)
        return; // 防并发重复扫描
    scanInFlight = true;
    const box = $('scan-results');
    box.classList.remove('hidden');
    box.innerHTML = '<p class="hint">扫描中…</p>';
    try {
        const relays = (await call('scan_relays'));
        // 剔除本机（本机锚点单独展示）
        const others = relays.filter((r) => !r.ip || MY_IPS.indexOf(r.ip) === -1);
        const cards = others.map((r) => ({
            base: deviceBase(r),
            name: r.name || 'Stross 设备',
            meta: r.ip ? r.ip + ':' + r.port : deviceBase(r),
            roles: r.roles || [],
            manual: false,
        }));
        // 手动添加的设备（历史持久化）也进设备列表
        manualRelays.forEach((addr) => {
            const base = addr.replace(/\/+$/, '');
            if (!cards.some((c) => c.base === base)) {
                const hostPort = addr.replace(/^https?:\/\//, '');
                cards.push({ base, name: hostPort, meta: hostPort, roles: [], manual: true });
            }
        });
        box.innerHTML = '';
        if (!cards.length) {
            box.appendChild(emptyState('radio', '未发现局域网内其它设备（mDNS）。可手动输入地址添加。'));
            return;
        }
        cards.forEach((c) => {
            const card = document.createElement('button');
            card.type = 'button';
            card.className = 'scan-card' + (selectedDevice === c.base ? ' selected' : '');
            const ic = document.createElement('span');
            ic.className = 'card-ic';
            ic.innerHTML = icon(c.manual ? 'link' : 'radio');
            card.appendChild(ic);
            const body = document.createElement('span');
            body.className = 'card-body';
            const nameLine = document.createElement('span');
            nameLine.className = 'scan-name';
            nameLine.textContent = c.name + (c.manual ? '（手动）' : '');
            const metaLine = document.createElement('span');
            metaLine.className = 'scan-meta';
            metaLine.appendChild(document.createTextNode(c.meta));
            if (c.roles.length) {
                const chips = document.createElement('span');
                chips.className = 'chips';
                c.roles.forEach((role) => chips.appendChild(roleChip(role)));
                metaLine.appendChild(chips);
            }
            body.appendChild(nameLine);
            body.appendChild(metaLine);
            card.appendChild(body);
            card.title = selectedDevice === c.base ? '取消只看该设备' : '只看该设备的串流';
            card.onclick = () => {
                // 按需建立：点设备 = 临时连锚点看其串流（再点一次取消）
                const next = selectedDevice === c.base ? null : c.base;
                selectedDevice = next;
                document.querySelectorAll('#scan-results .scan-card').forEach((el) => el.classList.remove('selected'));
                if (next)
                    card.classList.add('selected');
                card.title = next ? '取消只看该设备' : '只看该设备的串流';
                void scanRemoteStreams(true); // 强制刷新（过滤/取消过滤）
            };
            box.appendChild(card);
        });
    }
    catch (e) {
        box.innerHTML = '';
        box.appendChild(emptyState('radio', '扫描失败：' + e.message, true));
    }
    finally {
        scanInFlight = false;
    }
}
// ---------------------------------------------------------------- 推流配置
function currentVideoSource() {
    const kind = document.querySelector('input[name="video"]:checked').value;
    // 注意：与 Rust 端 VideoSource 的 serde(rename_all="camelCase") 契约一致（小写）
    if (kind === 'screen')
        return { kind: 'screen' };
    if (kind === 'camera')
        return { kind: 'camera', device: $select('camera-select').value || null };
    return { kind: 'synthetic', pattern: 'testsrc2' };
}
function buildConfig() {
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
/** 推流端按媒体类型自动选传输（与接收端 auto 同规则）：
 *  含视频 → SRT（Adaptive：丢包不阻塞、关键帧自愈）> QUIC > WS
 *  纯音频 → QUIC（无损：音频不可丢）> WS
 *  推流锚定本机中继（免先连：无需先连接其它设备）。 */
function pushRelayUrl(cfg) {
    if (!anchor)
        return '';
    const hasVideo = !!cfg.video;
    if (hasVideo) {
        if (anchor.srtUrl)
            return anchor.srtUrl;
        if (anchor.quicUrl)
            return anchor.quicUrl;
    }
    else if (anchor.quicUrl) {
        return anchor.quicUrl;
    }
    return `ws://127.0.0.1:${anchor.port}/ws/push`;
}
/** Android：与桌面统一走 start_stream（cfg 携带画质/音频；原生采集在 Rust 后端适配）。 */
async function startStream() {
    hideError();
    if (!anchor) {
        showFatal('本机锚点未就绪，无法推流。请到「网格」页查看锚点状态并重试。');
        return;
    }
    savePrefs();
    const btn = $btn('start-btn');
    setBtnLoading(btn, true);
    try {
        if (IS_ANDROID) {
            starting = true;
            startingSince = Date.now();
            setRunning(true, 'starting');
            // Android 原生采集启动需要系统授权，真实状态由 capture_status 轮询回报
        }
        const cfg = buildConfig();
        const res = (await call('start_stream', { cfg, relayUrl: pushRelayUrl(cfg) }));
        renderUrls(res.watchUrls);
        // D4：内核签发流 id —— 预填接收面板，本机可立即原生接收
        $input('recv-stream-input').value = res.streamId || '';
        void loadRemoteStreams(true); // 强制刷新，立即出现新流
        setBtnLoading(btn, false);
        if (IS_ANDROID) {
            void pollMobileStatus(); // 立即查一次真实采集状态
        }
        else {
            setRunning(true, 'live');
        }
    }
    catch (e) {
        setBtnLoading(btn, false);
        showFatal(String(e));
        starting = false;
        setRunning(false);
    }
}
async function stopStream() {
    try {
        await call('stop_stream');
    }
    catch (e) {
        showFatal(String(e));
    }
    starting = false;
    setRunning(false);
    void loadRemoteStreams(true); // 停止后刷新列表
}
/** Android：轮询采集真实状态（Kotlin 控制帧 t=9 回报 → capture_status）。 */
async function pollMobileStatus() {
    if (!IS_ANDROID)
        return;
    try {
        const s = (await call('capture_status'));
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
    }
    catch (_) {
        /* ignore */
    }
}
async function pollStatus() {
    if (IS_ANDROID) {
        // Android 每 2 秒轮询真实采集状态
        if (running || starting)
            void pollMobileStatus();
        return;
    }
    try {
        const s = (await call('stream_status'));
        setRunning(s.running);
        $('stream-meta').textContent = s.running
            ? `「${s.title}」(${s.streamId}) · 已推流 ${fmtElapsed((Date.now() / 1000) - s.startedAt)} · 中继端口 ${s.relayPort} · 局域网设备可在「网格」页发现并接收`
            : '';
    }
    catch (_) {
        /* ignore */
    }
}
/** 秒数 → "X 分 Y 秒"（推流时长展示）。 */
function fmtElapsed(totalSecs) {
    const s = Math.max(0, Math.floor(totalSecs));
    const m = Math.floor(s / 60);
    return m > 0 ? `${m} 分 ${s % 60} 秒` : `${s} 秒`;
}
/** phase: 'idle' | 'starting' | 'live' */
function setRunning(r, phase = r ? 'live' : 'idle') {
    running = r;
    const dot = $('status-dot');
    const text = $('status-text');
    $btn('start-btn').disabled = r || starting;
    $btn('stop-btn').disabled = !(r || starting);
    if (phase === 'starting') {
        dot.className = 'dot starting';
        text.textContent = '采集中…';
        $('stream-meta').textContent = '等待系统授权与投影就绪（OPPO 等机型可能需 10~20 秒）';
    }
    else if (phase === 'live') {
        dot.className = 'dot live';
        text.textContent = '推流中';
        // 明确告知去向（D1：无浏览器观看端，接收走原生播放；P0：推流锚定本机）
        $('stream-meta').textContent = '推流中 · 局域网设备可在「网格」页发现本机流并接收';
    }
    else {
        dot.className = 'dot idle';
        text.textContent = '未推流';
        $('stream-meta').textContent = '';
    }
}
/** 可复制的 URL 列表项（点击复制，1.5s 反馈「已复制」）。 */
function urlListItem(u) {
    const li = document.createElement('li');
    const tag = document.createElement('span');
    tag.className = 'tag';
    tag.innerHTML = icon('play');
    li.appendChild(tag);
    li.appendChild(document.createTextNode(u));
    li.title = '点击复制';
    makeClickable(li, () => {
        navigator.clipboard?.writeText(u).then(() => {
            li.style.borderColor = 'var(--ok)';
            li.innerHTML = '<span class="tag ok">' + icon('check') + '</span>已复制';
            setTimeout(() => {
                li.style.borderColor = '';
                li.innerHTML = '';
                li.appendChild(tag);
                li.appendChild(document.createTextNode(u));
            }, 1500);
        });
    });
    return li;
}
function renderUrls(urls) {
    const ul = $('url-list');
    ul.innerHTML = '';
    urls.forEach((u) => ul.appendChild(urlListItem(u)));
}
// ---------------------------------------------------------------- 接收（原生播放，1e）
/** Tauri 事件监听（__TAURI__.event.listen）。 */
function listen(event, cb) {
    const api = window.__TAURI__?.event;
    if (!api?.listen)
        return Promise.resolve(() => { });
    return api.listen(event, (e) => cb(e.payload));
}
let receiving = false;
let recvFrameCount = 0;
let recvUnlisten = null;
/** 接收等待浮层：接收中且尚未收到首帧时显示。 */
function updateRecvOverlay() {
    $('recv-overlay').classList.toggle('hidden', !receiving || recvFrameCount > 0);
}
/** 串流卡片（图标 + 名称 + 元信息：流 id/中继名 + 轨道 chip + 观看人数）。 */
function streamCard(o) {
    const card = document.createElement('button');
    card.type = 'button';
    card.className = 'scan-card';
    const ic = document.createElement('span');
    ic.className = 'card-ic';
    ic.innerHTML = icon(o.stream.video ? 'video' : o.stream.audio ? 'music' : 'radio');
    const body = document.createElement('span');
    body.className = 'card-body';
    const name = document.createElement('span');
    name.className = 'scan-name';
    name.textContent = o.title;
    const meta = document.createElement('span');
    meta.className = 'scan-meta';
    meta.appendChild(document.createTextNode(o.sub));
    meta.appendChild(trackChips(o.stream));
    if (o.stream.watchers)
        meta.appendChild(watcherCount(o.stream.watchers));
    body.appendChild(name);
    body.appendChild(meta);
    card.appendChild(ic);
    card.appendChild(body);
    card.title = '点击接收 ' + o.stream.streamId;
    card.onclick = () => o.onPick(card);
    return card;
}
/** 清空所有串流卡片的选中态。 */
function clearCardSelection() {
    document.querySelectorAll('.recv-streams .scan-card').forEach((c) => c.classList.remove('selected'));
}
/** 拉取本机锚点的在线串流列表（GET /api/streams），渲染可选卡片。 */
async function loadRemoteStreams(force = false) {
    const box = $('recv-streams');
    if (!anchor) {
        box.innerHTML = '';
        return;
    }
    // TTL 缓存：3 秒内不重复请求；force（推流后/手动）绕过缓存
    if (!force && streamsCache && Date.now() - streamsCache.at < STREAMS_TTL_MS) {
        box.innerHTML = '';
        for (const s of streamsCache.list) {
            remoteStreams.set(s.streamId, s);
            box.appendChild(streamCard({
                title: s.title || s.streamId,
                sub: s.streamId,
                stream: s,
                onPick: (card) => {
                    clearCardSelection();
                    card.classList.add('selected');
                    targetRelay = null; // 回本机锚点
                    remoteStreams.set(s.streamId, s);
                    $input('recv-stream-input').value = s.streamId;
                    void startReceive();
                },
            }));
        }
        return;
    }
    try {
        const resp = await fetch(`http://127.0.0.1:${anchor.port}/api/streams`, { cache: 'no-store' });
        if (!resp.ok) {
            box.innerHTML = '';
            box.appendChild(emptyState('video', '本机锚点未提供串流列表（HTTP ' + resp.status + '）', true));
            return;
        }
        const data = (await resp.json());
        const list = Array.isArray(data) ? data : (data.streams || []);
        streamsCache = { at: Date.now(), list };
        box.innerHTML = '';
        if (!list.length) {
            box.appendChild(emptyState('video', '本机锚点暂无在线串流。可先在「推流」页开始推流。'));
            return;
        }
        for (const s of list) {
            remoteStreams.set(s.streamId, s);
            box.appendChild(streamCard({
                title: s.title || s.streamId,
                sub: s.streamId,
                stream: s,
                onPick: (card) => {
                    clearCardSelection();
                    card.classList.add('selected');
                    targetRelay = null; // 回本机锚点
                    remoteStreams.set(s.streamId, s);
                    $input('recv-stream-input').value = s.streamId;
                    void startReceive();
                },
            }));
        }
    }
    catch (e) {
        box.innerHTML = '';
        box.appendChild(emptyState('video', '拉取串流列表失败：' + e.message, true));
    }
}
/** 网格页自动发现：扫描局域网设备（mDNS + 手动添加），聚合各设备的在线串流。
 *  点设备卡片后只显示该设备串流；点流卡片 = 按需建立连接（直连锚点，失败自动
 *  经本机级联代理），并跳转「观看（收）」页接收。 */
async function scanRemoteStreams(force = false) {
    if (discoverInFlight)
        return; // 防并发
    if (!force && discoverCacheAt && Date.now() - discoverCacheAt < DISCOVER_TTL_MS)
        return;
    discoverInFlight = true;
    const box = $('discover-streams');
    box.innerHTML = '<p class="hint">扫描局域网串流…</p>';
    let relays;
    try {
        relays = (await call('scan_relays'));
    }
    catch (e) {
        box.innerHTML = '';
        box.appendChild(emptyState('radio', '扫描失败：' + e.message, true));
        discoverInFlight = false;
        return;
    }
    try {
        const others = relays.filter((r) => !r.ip || MY_IPS.indexOf(r.ip) === -1);
        // 手动添加的设备并入聚合（无 mDNS 时也能看到其串流）
        manualRelays.forEach((addr) => {
            const base = addr.replace(/\/+$/, '');
            if (!others.some((r) => deviceBase(r) === base)) {
                others.push({
                    port: 0,
                    urls: [base + '/'],
                    name: addr.replace(/^https?:\/\//, ''),
                    kind: null,
                    roles: [],
                    transports: [],
                    ip: null,
                });
            }
        });
        if (!others.length) {
            box.innerHTML = '';
            box.appendChild(emptyState('radio', '未发现局域网其它设备（mDNS）。可手动输入地址添加。'));
            return;
        }
        const found = [];
        for (const r of others) {
            const base = deviceBase(r);
            if (!base)
                continue;
            // 传输端口：/api/info（旧版本中继无此端点 → 该设备走 WS）
            let info = null;
            try {
                const iresp = await fetch(base + '/api/info', { cache: 'no-store' });
                if (iresp.ok)
                    info = (await iresp.json());
            }
            catch (_) { /* 忽略 */ }
            try {
                const sresp = await fetch(base + '/api/streams', { cache: 'no-store' });
                if (!sresp.ok)
                    continue;
                const data = (await sresp.json());
                const list = Array.isArray(data) ? data : (data.streams || []);
                // SRT/QUIC 是独立 UDP 端口：拨号地址 = srt://<纯主机>:<srt_port>
                // （不能带 http 端口；base 形如 http://ip:8777/）
                const hostOnly = base.replace(/^https?:\/\//, '').replace(/:\d+$/, '');
                for (const st of list) {
                    found.push({
                        relayName: r.name || r.ip || base,
                        relayBase: base,
                        stream: st,
                        srtUrl: info && info.srtPort ? `srt://${hostOnly}:${info.srtPort}` : null,
                        quicUrl: info && info.quicPort ? `quic://${hostOnly}:${info.quicPort}` : null,
                    });
                }
            }
            catch (_) { /* 该设备不可达，跳过 */ }
        }
        // 设备筛选：「点设备只看其流」（selectedDevice = relayBase 键）
        const shown = selectedDevice ? found.filter((f) => f.relayBase === selectedDevice) : found;
        box.innerHTML = '';
        if (!shown.length) {
            box.appendChild(emptyState('radio', selectedDevice
                ? '该设备暂无在线串流（或不可达）。再点一次设备卡片取消筛选。'
                : '局域网内暂无在线串流（可手动输入流 id）。'));
            return;
        }
        for (const it of shown) {
            box.appendChild(streamCard({
                title: it.stream.title || it.stream.streamId,
                sub: it.relayName,
                stream: it.stream,
                onPick: (card) => {
                    clearCardSelection();
                    card.classList.add('selected');
                    // 按需建立：目标切到该设备锚点（直连失败自动经本机级联代理）
                    targetRelay = { wsBase: it.relayBase.replace(/^http/, 'ws'), srtUrl: it.srtUrl, quicUrl: it.quicUrl };
                    remoteStreams.set(it.stream.streamId, it.stream);
                    $input('recv-stream-input').value = it.stream.streamId;
                    setTab('watch'); // 点流即看：跳转接收页
                    void startReceive();
                },
            }));
        }
    }
    finally {
        discoverInFlight = false;
        discoverCacheAt = Date.now();
    }
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
/** 开始原生接收：watch（WS/SRT/QUIC）→ 解码 → canvas 绘制。
 *  目标 = 网格页点选的设备锚点（`targetRelay`）或本机锚点；直连失败自动级联。 */
async function startReceive() {
    hideRecvError();
    if (!anchor && !targetRelay) {
        showRecvError('本机锚点未就绪且未选择局域网串流。请从「网格」页选择一串流。');
        return;
    }
    const streamId = $input('recv-stream-input').value.trim();
    if (!streamId) {
        showRecvError('请输入流 id，或从上方选择一串流');
        return;
    }
    const btn = $btn('recv-start-btn');
    setBtnLoading(btn, true);
    try {
        const audio = $select('recv-audio-select').value; // 'device' | 'discard'（与 AudioOut serde 一致）
        const stream = remoteStreams.get(streamId) || null; // 流类型（video/audio）供传输自动选择
        const relay = pickRelayUrl(stream); // 按传输选择 + 流媒体类型：ws / srt / quic（UDP 不可用回退）
        if (!relay) {
            showRecvError('无可用接收目标（本机锚点未就绪或未选择局域网串流）');
            return;
        }
        await call('start_receive', {
            relay,
            stream: streamId,
            audio,
        });
        receiving = true;
        recvFrameCount = 0;
        $('recv-status').textContent = '接收中…';
        $('recv-dot').className = 'dot starting';
        $btn('recv-stop-btn').disabled = false;
        setBtnLoading(btn, false);
        btn.disabled = true; // 接收中不可重复开始
        updateRecvOverlay(); // 等待首帧 → 显示浮层
        // 订阅解码帧事件 → canvas
        recvUnlisten = await listen('receive-frame', (p) => {
            drawReceiveFrame(p.width, p.height, p.data);
            recvFrameCount += 1;
            updateRecvOverlay();
        });
        void pollReceiveStatus();
    }
    catch (e) {
        setBtnLoading(btn, false);
        showRecvError('接收失败：' + e.message);
        setReceiving(false);
    }
}
/** 停止接收并清空画面。 */
async function stopReceive() {
    try {
        await call('stop_receive');
    }
    catch (_) { /* ignore */ }
    if (recvUnlisten) {
        recvUnlisten();
        recvUnlisten = null;
    }
    setReceiving(false);
    const ctx = canvasCtx();
    if (ctx)
        ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height);
}
function canvasCtx() {
    const c = $('recv-canvas');
    return c.getContext('2d');
}
/** 把 RGBA 帧画到 canvas（宽度自适应，等比缩放）。 */
function drawReceiveFrame(w, h, data) {
    const ctx = canvasCtx();
    if (!ctx)
        return;
    const canvas = ctx.canvas;
    if (canvas.width !== w)
        canvas.width = w;
    if (canvas.height !== h)
        canvas.height = h;
    const img = new ImageData(new Uint8ClampedArray(data), w, h);
    ctx.putImageData(img, 0, 0);
}
function setReceiving(r) {
    receiving = r;
    $btn('recv-start-btn').disabled = r;
    $btn('recv-stop-btn').disabled = !r;
    $('recv-dot').className = 'dot ' + (r ? 'live' : 'idle');
    $('recv-status').textContent = r ? '接收中' : '未接收';
    if (!r)
        $('recv-meta').textContent = '';
    updateRecvOverlay();
}
/** 轮询接收统计（帧数 / 解码 / 音频块）。 */
async function pollReceiveStatus() {
    if (!receiving)
        return;
    try {
        const s = (await call('receive_status'));
        if (!s.running && recvFrameCount === 0 && !s.error) {
            $('recv-dot').className = 'dot starting';
            $('recv-status').textContent = '等待流数据…';
        }
        $('recv-meta').textContent = s.error
            ? '错误：' + s.error
            : `收到 ${s.received} 帧 · 解码 ${s.decodedVideo} 帧 · 音频 ${s.audioBlocks} 块 · 已绘制 ${recvFrameCount} 帧`;
    }
    catch (_) { /* ignore */ }
    if (receiving)
        setTimeout(() => void pollReceiveStatus(), 1000);
}
// ---------------------------------------------------------------- 事件
document.querySelectorAll('input[name="video"]').forEach((r) => r.addEventListener('change', () => {
    $('camera-row').classList.toggle('hidden', r.value !== 'camera');
}));
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
