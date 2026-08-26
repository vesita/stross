"use strict";
// Stross 前端 —— 设备域（script 全局作用域）：
// 设备 × 共享流 组合管理：左栏设备列表（本机 + 局域网设备），
// 每个设备卡片可展开 → 发起共享（广播/定向）与查看/接收该设备的在线共享。
function normAddr(addr) {
    let a = addr.trim();
    if (!a)
        return null;
    if (!/^https?:\/\//i.test(a))
        a = 'http://' + a;
    return a.replace(/\/+$/, '');
}
/** link-local / 回环地址（fe80::/10、169.254/16、127.0.0.1、::1）：不可达或
 *  仅本机可见，剔除出设备列表（Android 锚点回退回环时扫描会回显 127.0.0.1）。 */
function isLinkLocalIp(ip) {
    return (ip === '127.0.0.1' ||
        ip === '::1' ||
        /^fe80:/i.test(ip) ||
        /^169\.254\./.test(ip));
}
/** 免先连核心：自动锚定本机（`start_relay` 幂等，启动受控中继 + mDNS 广播）。 */
async function ensureAnchor() {
    setAnchorBadge('anchoring');
    try {
        const info = (await call('start_relay'));
        anchor = {
            port: info.port,
            urls: info.urls,
            srtUrl: null,
            quicUrl: null,
        };
        setAnchorBadge('ok');
        void refreshAnchorPorts();
        renderLocalCard(); // 本机卡片状态更新
    }
    catch (e) {
        anchor = null;
        setAnchorBadge('err');
        const box = $('grid-error');
        box.textContent = '本机锚定失败：' + e.message + '（仍可接收局域网共享）';
        box.classList.remove('hidden');
        const retry = document.createElement('button');
        retry.type = 'button';
        retry.innerHTML = icon('refresh') + '<span>重试锚定</span>';
        retry.onclick = () => void ensureAnchor();
        box.appendChild(retry);
    }
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
    catch (_) { /* 中继可能不支持 /api/info：保持 null，走 WS */ }
}
/** 手动添加设备地址（免 mDNS）：探测可达后进入设备列表。 */
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
    void scanRemoteStreams(true); // 强制刷新其在线共享
}
/** 恢复上次的地址偏好，并渲染手动添加历史。（共享弹窗标题在打开时从 LS_TITLE 预填。） */
function restorePrefs() {
    const last = localStorage.getItem(LS_RELAY);
    if (last)
        $input('manual-addr').value = last;
    manualRelays = getRecent();
    renderRecent();
}
function savePrefs() {
    localStorage.setItem(LS_RELAY, $input('manual-addr').value.trim());
    const title = $input('share-title');
    if (title)
        localStorage.setItem(LS_TITLE, title.value.trim());
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
function removeRecent(url) {
    const list = getRecent().filter((u) => u !== url);
    localStorage.setItem(LS_RECENT, JSON.stringify(list));
    manualRelays = list;
    renderRecent();
}
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
            e.stopPropagation();
            removeRecent(u);
        };
        li.appendChild(main);
        li.appendChild(del);
        ul.appendChild(li);
    });
}
// ---------------------------------------------------------------------------
// 设备列表（左栏）：本机 + 局域网设备
// ---------------------------------------------------------------------------
/** 归一化设备基址：取 urls[0] 去掉尾部斜杠。 */
function deviceBase(r) {
    return (r.urls[0] || '').replace(/\/+$/, '');
}
/** 扫描局域网设备（mDNS + 手动添加），重建设备列表。 */
async function scanRelays() {
    if (scanInFlight)
        return;
    scanInFlight = true;
    try {
        const relays = (await call('scan_relays'));
        // 剔除本机 + link-local（本机单独展示；fe80 无 scope 不可达）
        const others = relays.filter((r) => !r.ip || (MY_IPS.indexOf(r.ip) === -1 && !isLinkLocalIp(r.ip)));
        const cards = others.map((r) => ({
            key: deviceBase(r),
            name: r.name || 'Stross 设备',
            meta: r.ip ? r.ip + ':' + r.port : deviceBase(r),
            isLocal: false,
            roles: r.roles || [],
            manual: false,
            base: deviceBase(r),
            srtUrl: null,
            quicUrl: null,
            streams: [],
        }));
        // 手动添加的设备（历史持久化）也进设备列表
        manualRelays.forEach((addr) => {
            const base = addr.replace(/\/+$/, '');
            if (!cards.some((c) => c.base === base)) {
                const hostPort = addr.replace(/^https?:\/\//, '');
                cards.push({
                    key: base,
                    name: hostPort + '（手动）',
                    meta: hostPort,
                    isLocal: false,
                    roles: [],
                    manual: true,
                    base,
                    srtUrl: null,
                    quicUrl: null,
                    streams: [],
                });
            }
        });
        // 保留已展开状态；本机卡片由渲染器恒置首位
        const keepExpanded = expandedDevice;
        deviceViews = cards;
        if (keepExpanded && !deviceViews.some((d) => d.key === keepExpanded))
            expandedDevice = null;
        renderDeviceList();
    }
    catch (e) {
        showGridError('扫描失败：' + e.message);
    }
    finally {
        scanInFlight = false;
    }
}
/** 渲染左栏设备列表：本机卡片 + 各设备卡片（设备可展开）。 */
function renderDeviceList() {
    const box = $('device-list');
    box.innerHTML = '';
    box.appendChild(localDeviceCard());
    if (!deviceViews.length) {
        box.appendChild(emptyState('radio', '未发现局域网内其它设备（mDNS）。可手动输入地址添加。'));
        return;
    }
    for (const dev of deviceViews) {
        box.appendChild(deviceCard(dev));
    }
}
/** 本机卡片：广播共享入口 + 接收手机麦克风 + 本机入口地址。恒展开。 */
function localDeviceCard() {
    const card = document.createElement('div');
    card.className = 'dev-card local expanded';
    card.dataset.key = 'local';
    const head = document.createElement('div');
    head.className = 'dev-head';
    const ic = document.createElement('span');
    ic.className = 'card-ic local';
    ic.innerHTML = icon('logo');
    const body = document.createElement('span');
    body.className = 'card-body';
    const nameLine = document.createElement('span');
    nameLine.className = 'scan-name';
    nameLine.textContent = '本机（我）';
    const metaLine = document.createElement('span');
    metaLine.className = 'scan-meta';
    metaLine.id = 'anchor-box';
    metaLine.textContent = anchor ? `已锚定 · 中继端口 ${anchor.port} · mDNS 广播中` : '锚定中…';
    body.appendChild(nameLine);
    body.appendChild(metaLine);
    head.appendChild(ic);
    head.appendChild(body);
    card.appendChild(head);
    const detail = document.createElement('div');
    detail.className = 'dev-detail';
    // 出站共享（广播）：屏幕 / 麦克风（本机能力共享给局域网任意接收方）
    const ops = document.createElement('div');
    ops.className = 'dev-ops';
    ops.appendChild(opButton('broadcast-screen', 'monitor', '共享屏幕（广播）'));
    ops.appendChild(opButton('broadcast-mic', 'mic', '共享麦克风（广播）'));
    const recvBtn = opButton('recv-mic', 'phone', '接收手机麦克风');
    recvBtn.id = 'mic-recv-btn'; // setBtnLoading 需要引用
    ops.appendChild(recvBtn);
    detail.appendChild(ops);
    // 接收手机麦克风凭证面板（B2：电脑端签发，手机出示后自动接收播放）
    const recvPanel = document.createElement('div');
    recvPanel.className = 'mic-recv-panel hidden';
    recvPanel.id = 'mic-recv-panel';
    const hint = document.createElement('p');
    hint.className = 'hint';
    hint.textContent = '在手机上打开 Stross → 找到本机 → 共享麦克风 → 粘贴下方凭证；接入后自动通过扬声器播放。';
    const row = document.createElement('div');
    row.className = 'row';
    const pin = document.createElement('span');
    pin.className = 'pin mono';
    pin.id = 'mic-recv-pin';
    const copyBtn = document.createElement('button');
    copyBtn.type = 'button';
    copyBtn.id = 'mic-recv-copy-btn';
    copyBtn.innerHTML = icon('copy') + '<span>复制凭证</span>';
    row.appendChild(pin);
    row.appendChild(copyBtn);
    const token = document.createElement('textarea');
    token.className = 'mono';
    token.id = 'mic-recv-token';
    token.readOnly = true;
    token.rows = 3;
    const status = document.createElement('div');
    status.className = 'meta';
    status.id = 'mic-recv-status';
    recvPanel.appendChild(hint);
    recvPanel.appendChild(row);
    recvPanel.appendChild(token);
    recvPanel.appendChild(status);
    detail.appendChild(recvPanel);
    // 本机在线共享（点条目即接收；不展开设备级操作）
    const localStreams = document.createElement('div');
    localStreams.className = 'dev-streams';
    localStreams.dataset.role = 'local-streams';
    const lsTitle = document.createElement('h3');
    lsTitle.textContent = '本机在线共享';
    localStreams.appendChild(lsTitle);
    localStreams.appendChild(streamListPlaceholder());
    detail.appendChild(localStreams);
    // 本机入口地址
    const entryTitle = document.createElement('h3');
    entryTitle.textContent = '本机入口';
    detail.appendChild(entryTitle);
    const ips = document.createElement('ul');
    ips.id = 'ip-list';
    ips.className = 'url-list';
    const ipsHint = document.createElement('li');
    ipsHint.className = 'hint';
    ipsHint.textContent = '读取中…';
    ips.appendChild(ipsHint);
    detail.appendChild(ips);
    card.appendChild(detail);
    return card;
}
/** 局域网设备卡片：点击头部展开 → 共享麦克风到 TA + TA 的在线共享（点流接收）。 */
function deviceCard(dev) {
    const card = document.createElement('div');
    card.className = 'dev-card' + (expandedDevice === dev.key ? ' expanded' : '');
    card.dataset.key = dev.key;
    const head = document.createElement('div');
    head.className = 'dev-head';
    head.setAttribute('role', 'button');
    head.tabIndex = 0;
    const ic = document.createElement('span');
    ic.className = 'card-ic';
    ic.innerHTML = icon(dev.manual ? 'link' : 'radio');
    const body = document.createElement('span');
    body.className = 'card-body';
    const nameLine = document.createElement('span');
    nameLine.className = 'scan-name';
    nameLine.textContent = dev.name;
    const metaLine = document.createElement('span');
    metaLine.className = 'scan-meta';
    metaLine.appendChild(document.createTextNode(dev.meta));
    if (dev.roles.length) {
        const chips = document.createElement('span');
        chips.className = 'chips';
        dev.roles.forEach((role) => chips.appendChild(roleChip(role)));
        metaLine.appendChild(chips);
    }
    body.appendChild(nameLine);
    body.appendChild(metaLine);
    head.appendChild(ic);
    head.appendChild(body);
    const badge = document.createElement('span');
    badge.className = 'badge-streams';
    badge.textContent = dev.streams.length ? dev.streams.length + ' 条共享' : '';
    head.appendChild(badge);
    const toggle = () => {
        expandedDevice = expandedDevice === dev.key ? null : dev.key;
        renderDeviceList();
    };
    head.addEventListener('click', (e) => {
        // 麦克风操作按钮在 detail 内，不冒泡到 head
        toggle();
    });
    head.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            toggle();
        }
    });
    card.appendChild(head);
    const detail = document.createElement('div');
    detail.className = 'dev-detail' + (expandedDevice === dev.key ? '' : ' hidden');
    const ops = document.createElement('div');
    ops.className = 'dev-ops';
    ops.appendChild(opButton('mic-to', 'mic', '共享麦克风到 TA'));
    detail.appendChild(ops);
    const streamsBox = document.createElement('div');
    streamsBox.className = 'dev-streams';
    streamsBox.dataset.role = 'node-streams';
    streamsBox.dataset.key = dev.key;
    const stTitle = document.createElement('h3');
    stTitle.textContent = 'TA 的在线共享（点条目接收）';
    streamsBox.appendChild(stTitle);
    streamsBox.appendChild(devStreamsOf(dev));
    detail.appendChild(streamsBox);
    card.appendChild(detail);
    return card;
}
function opButton(act, icName, label) {
    const b = document.createElement('button');
    b.type = 'button';
    b.dataset.act = act;
    b.innerHTML = icon(icName) + '<span>' + label + '</span>';
    return b;
}
/** 设备（或本机）的在线共享条目区；空态提示。 */
function devStreamsOf(dev) {
    const box = document.createElement('div');
    if (!dev.streams.length) {
        const empty = document.createElement('p');
        empty.className = 'hint';
        empty.textContent = dev.isLocal ? '本机暂未有共享广播' : '该设备暂未有在线共享（或不可达）';
        box.appendChild(empty);
        return box;
    }
    dev.streams.forEach((s) => box.appendChild(streamItem(dev, s)));
    return box;
}
function streamListPlaceholder() {
    const box = document.createElement('div');
    const empty = document.createElement('p');
    empty.className = 'hint';
    empty.textContent = '本机暂未有共享广播';
    box.appendChild(empty);
    return box;
}
/** 单个共享流条目（点流即看：按需直连该设备锚点接收）。 */
function streamItem(dev, s) {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = 'dev-stream-item';
    b.dataset.stream = s.streamId;
    const ic = document.createElement('span');
    ic.className = 'card-ic';
    ic.innerHTML = icon(s.video ? 'video' : s.audio ? 'music' : 'radio');
    const body = document.createElement('span');
    body.className = 'card-body';
    const name = document.createElement('span');
    name.className = 'scan-name';
    name.textContent = s.title || s.streamId;
    const meta = document.createElement('span');
    meta.className = 'scan-meta';
    meta.appendChild(document.createTextNode(s.streamId + ' · ' + dev.name));
    const chips = document.createElement('span');
    chips.className = 'chips';
    if (s.video)
        chips.appendChild(chipEl('video', '视频'));
    if (s.audio)
        chips.appendChild(chipEl('audio', '音频'));
    meta.appendChild(chips);
    body.appendChild(name);
    body.appendChild(meta);
    b.appendChild(ic);
    b.appendChild(body);
    b.title = '点击接收 ' + s.streamId;
    b.onclick = () => {
        // 按需建立：目标切到该设备锚点（本机共享流 → 回本机锚点）；
        // 直连失败自动经本机级联代理
        if (dev.base) {
            targetRelay = {
                wsBase: dev.base.replace(/^http/, 'ws'),
                srtUrl: dev.srtUrl,
                quicUrl: dev.quicUrl,
            };
        }
        else {
            targetRelay = null;
        }
        remoteStreams.set(s.streamId, s);
        void startReceive(s.streamId);
    };
    return b;
}
/** 拉取所有设备的在线共享列表，填入设备视图并刷新（按设备分流展示）。 */
async function scanRemoteStreams(force = false) {
    if (discoverInFlight)
        return;
    if (!force && discoverCacheAt && Date.now() - discoverCacheAt < DISCOVER_TTL_MS)
        return;
    discoverInFlight = true;
    let relays;
    try {
        relays = (await call('scan_relays'));
    }
    catch (e) {
        showGridError('扫描失败：' + e.message);
        discoverInFlight = false;
        return;
    }
    const others = relays.filter((r) => !r.ip || (MY_IPS.indexOf(r.ip) === -1 && !isLinkLocalIp(r.ip)));
    // 手动添加的设备并入聚合（无 mDNS 时也能看到其共享）
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
    // 设备 key → 在线共享列表（保留既有流缓存，避免每次全量重建丢失流信息）
    const perDevice = {};
    for (const r of others) {
        const base = deviceBase(r);
        if (!base)
            continue;
        let info = null;
        try {
            const iresp = await fetch(base + '/api/info', { cache: 'no-store' });
            if (iresp.ok)
                info = (await iresp.json());
        }
        catch (_) { /* 该设备 /api/info 不可用 → SRT/QUIC null */ }
        try {
            const sresp = await fetch(base + '/api/streams', { cache: 'no-store' });
            if (!sresp.ok)
                continue;
            const data = (await sresp.json());
            const list = Array.isArray(data) ? data : (data.streams || []);
            const hostOnly = base.replace(/^https?:\/\//, '').replace(/:\d+$/, '');
            list.forEach((st) => {
                if (!remoteStreams.has(st.streamId))
                    remoteStreams.set(st.streamId, st);
            });
            perDevice[base] = list;
            // 同步 SRT/QUIC 拨号地址到设备视图
            const dev = deviceViews.find((d) => d.base === base);
            if (dev) {
                dev.srtUrl = info && info.srtPort ? `srt://${hostOnly}:${info.srtPort}` : null;
                dev.quicUrl = info && info.quicPort ? `quic://${hostOnly}:${info.quicPort}` : null;
                dev.streams = list;
            }
        }
        catch (_) { /* 该设备不可达，跳过 */ }
    }
    // 拉取本机在线共享（本机卡片流区）
    if (anchor) {
        try {
            const resp = await fetch(`http://127.0.0.1:${anchor.port}/api/streams`, { cache: 'no-store' });
            if (resp.ok) {
                const data = (await resp.json());
                const list = Array.isArray(data) ? data : (data.streams || []);
                list.forEach((st) => remoteStreams.set(st.streamId, st));
                localStreamsCache = list;
            }
        }
        catch (_) { /* ignore */ }
    }
    renderLocalStreams();
    // 局部刷新展开设备的流区（避免整树重绘丢焦点）
    for (const [key, list] of Object.entries(perDevice)) {
        const dev = deviceViews.find((d) => d.base === key);
        if (dev)
            dev.streams = list;
    }
    refreshNodeStreams();
    discoverInFlight = false;
    discoverCacheAt = Date.now();
}
/** 本机在线共享缓存（供本机卡片流区渲染）。 */
let localStreamsCache = [];
/** 渲染本机卡片流区（本机在线共享）。 */
function renderLocalStreams() {
    const box = document.querySelector('[data-role="local-streams"]');
    if (!box)
        return;
    box.innerHTML = '';
    const title = document.createElement('h3');
    title.textContent = '本机在线共享';
    box.appendChild(title);
    if (!localStreamsCache.length) {
        const empty = document.createElement('p');
        empty.className = 'hint';
        empty.textContent = '本机暂未有共享广播';
        box.appendChild(empty);
        return;
    }
    const localDev = {
        key: 'local',
        name: '本机（我）',
        meta: '',
        isLocal: true,
        roles: [],
        manual: false,
        base: null,
        srtUrl: anchor ? anchor.srtUrl : null,
        quicUrl: anchor ? anchor.quicUrl : null,
        streams: localStreamsCache,
    };
    localStreamsCache.forEach((s) => box.appendChild(streamItem(localDev, s)));
}
/** 局部刷新所有设备卡片的流区（保持展开/收起状态，不整树重绘）。 */
function refreshNodeStreams() {
    document.querySelectorAll('.dev-card[data-key]:not(.local)').forEach((card) => {
        const key = card.dataset.key;
        const dev = deviceViews.find((d) => d.key === key);
        const box = card.querySelector('[data-role="node-streams"]');
        if (!dev || !box)
            return;
        box.innerHTML = '';
        const title = document.createElement('h3');
        title.textContent = 'TA 的在线共享（点条目接收）';
        box.appendChild(title);
        box.appendChild(devStreamsOf(dev));
        const badge = card.querySelector('.badge-streams');
        if (badge)
            badge.textContent = dev.streams.length ? dev.streams.length + ' 条共享' : '';
    });
}
/** 本机局域网入口地址渲染（点击复制）。 */
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
/** 刷新本机卡片锚点状态行（锚定成功后调用）。 */
function renderLocalCard() {
    const meta = $('anchor-box');
    if (meta) {
        meta.textContent = anchor
            ? `已锚定 · 中继端口 ${anchor.port} · mDNS 广播中`
            : '未锚定';
    }
}
// ---------------------------------------------------------------------------
// 共享（出站）入口：广播弹窗（共享屏幕 / 共享麦克风）
// ---------------------------------------------------------------------------
/** 打开「共享屏幕（广播）」弹窗：配置音频（麦克风/系统声）后开始。 */
function openBroadcastScreen() {
    const opts = $('share-modal-opts');
    opts.innerHTML = '';
    // 音频选项：麦克风（默认开）+ 系统声音（仅桌面支持回环采集）
    const micCheck = document.createElement('label');
    micCheck.className = 'check';
    micCheck.innerHTML = `<input type="checkbox" id="share-mic" checked />
    <svg class="ic"><use href="#i-mic" /></svg><span>含麦克风${IS_ANDROID ? '（需权限）' : ''}</span>`;
    opts.appendChild(micCheck);
    if (!IS_ANDROID) {
        const sysRow = document.createElement('div');
        sysRow.className = 'row';
        const sysCheck = document.createElement('label');
        sysCheck.className = 'check';
        sysCheck.innerHTML = `<input type="checkbox" id="share-sys" />
      <svg class="ic"><use href="#i-speaker" /></svg><span>含系统声音</span>`;
        sysRow.appendChild(sysCheck);
        const sysSel = document.createElement('select');
        sysSel.id = 'share-sys-dev';
        sysSel.className = 'grow' + (devices.systemAudio.length ? '' : ' hidden');
        if (devices.systemAudio.length) {
            sysSel.innerHTML = devices.systemAudio
                .map((n) => `<option value="${n}">${n}</option>`)
                .join('');
        }
        else {
            sysSel.appendChild(new Option('未发现回环设备（系统声不可用）', ''));
        }
        sysSel.disabled = true;
        sysRow.appendChild(sysSel);
        opts.appendChild(sysRow);
        sysCheck.querySelector('input').addEventListener('change', () => {
            const on = sysCheck.querySelector('input').checked;
            sysSel.classList.toggle('hidden', !on || !devices.systemAudio.length);
            sysSel.disabled = !on || !devices.systemAudio.length;
        });
    }
    // 画质（Android 原生编码也走同一配置）
    const qRow = document.createElement('label');
    qRow.textContent = '画质 ';
    const qSel = document.createElement('select');
    qSel.id = 'share-quality';
    qSel.innerHTML = `
    <option value="LOW">低 (640×360 @24fps)</option>
    <option value="MEDIUM" selected>中 (1280×720 @30fps)</option>
    <option value="HIGH">高 (1920×1080 @30fps)</option>`;
    qRow.appendChild(qSel);
    opts.appendChild(qRow);
    const titleRow = document.createElement('label');
    titleRow.textContent = '共享名称 ';
    const titleInput = document.createElement('input');
    titleInput.type = 'text';
    titleInput.id = 'share-title';
    titleInput.value = '我的屏幕';
    titleInput.maxLength = 40;
    titleRow.appendChild(titleInput);
    opts.appendChild(titleRow);
    $('share-modal-title').textContent = '共享屏幕（广播）';
    $('share-modal-sub').textContent = '本机屏幕广播到局域网；其它设备可在其「设备」列表点本机在线共享接收。';
    openShareModal(async () => {
        // 音频：麦克风用系统默认输入（mic=null）；系统声需具体回环设备（桌面）
        const sysDev = !IS_ANDROID && sysOn() && devices.systemAudio.length
            ? $select('share-sys-dev').value.trim() || null
            : null;
        const useMic = micOn();
        const cfg = {
            streamId: 'stross-' + Date.now().toString(36),
            title: $input('share-title').value.trim() || '我的屏幕',
            video: { kind: 'screen' },
            quality: QUALITIES[$select('share-quality').value],
            audio: useMic || sysDev
                ? { mic: null, systemAudio: sysDev, sampleRate: 48000, channels: 2, bitrateKbps: 128 }
                : null,
            durationSecs: null,
            shareToken: null,
        };
        return cfg;
    });
    $input('share-title').value = localStorage.getItem(LS_TITLE) || '我的屏幕';
}
/** 打开「共享麦克风（广播）」弹窗：纯音频推流（桌面 ffmpeg / Android micOnly）。 */
function openBroadcastMic() {
    const opts = $('share-modal-opts');
    opts.innerHTML = '';
    const hint = document.createElement('p');
    hint.className = 'hint';
    hint.textContent = IS_ANDROID
        ? '纯麦克风推流：无需屏幕录制授权，只请求麦克风权限。'
        : '纯音频推流（本机默认输入设备）。';
    opts.appendChild(hint);
    const titleRow = document.createElement('label');
    titleRow.textContent = '共享名称 ';
    const titleInput = document.createElement('input');
    titleInput.type = 'text';
    titleInput.id = 'share-title';
    titleInput.value = '我的麦克风';
    titleInput.maxLength = 40;
    titleRow.appendChild(titleInput);
    opts.appendChild(titleRow);
    $('share-modal-title').textContent = '共享麦克风（广播）';
    $('share-modal-sub').textContent = '把本机麦克风声音广播到局域网；电脑/手机可在其「设备」列表点本机在线共享接收（应用场景：另一台电脑播放手机声音）。';
    openShareModal(async () => {
        const cfg = {
            streamId: 'stross-' + Date.now().toString(36),
            title: $input('share-title').value.trim() || '我的麦克风',
            video: null, // 纯音频：Android 走 micOnly（跳过屏幕授权）；桌面 ffmpeg 纯音频流
            quality: QUALITIES.LOW,
            audio: { mic: null, systemAudio: null, sampleRate: 48000, channels: 2, bitrateKbps: 128 },
            durationSecs: null,
            shareToken: null,
        };
        return cfg;
    });
    $input('share-title').value = localStorage.getItem(LS_TITLE) || '我的麦克风';
}
function micOn() {
    const c = document.getElementById('share-mic');
    return !!c && c.checked;
}
function sysOn() {
    const c = document.getElementById('share-sys');
    return !!c && c.checked;
}
/** 台账状态：当前打开共享弹窗的启动回调（点「开始」时执行并关闭）。 */
let shareModalStarter = null;
function openShareModal(starter) {
    shareModalStarter = starter;
    $('share-status').textContent = '';
    $('share-error').classList.add('hidden');
    $('share-modal').classList.remove('hidden');
}
/** 点「开始」：按弹窗配置启动广播共享（走统一 start_stream 链路）。 */
async function confirmShareModal() {
    if (!shareModalStarter)
        return;
    const cfg = await shareModalStarter();
    shareModalStarter = null;
    $('share-modal').classList.add('hidden');
    await startStreamWith(cfg, pushRelayUrl(cfg));
}
function cancelShareModal() {
    shareModalStarter = null;
    $('share-modal').classList.add('hidden');
}
/** 推流拨号地址（本机锚点；按媒体类型自动选传输：视频 SRT>QUIC>WS，纯音频 QUIC>WS）。 */
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
// ---------------------------------------------------------------------------
// B2 反向外设：手机麦克风 → 电脑（凭证式接入）
// ---------------------------------------------------------------------------
/** 由设备 http://host:port 基址构造推流拨号地址：QUIC 可用（/api/info）
 *  优先（纯音频无损），否则回退 ws://host:port/ws/push。 */
function pushUrlForDevice(base, quicPort) {
    const hostPort = base.replace(/^https?:\/\//, '').replace(/\/+$/, '');
    const idx = hostPort.lastIndexOf(':');
    const host = idx > 0 ? hostPort.slice(0, idx) : hostPort;
    if (quicPort)
        return `quic://${host}:${quicPort}`;
    return `ws://${hostPort}/ws/push`;
}
/** 设备协商端点基址（http://host:18779；与 Rust 协商端口一致）。 */
function negotiatorBase(base) {
    try {
        const u = new URL(base);
        u.port = String(NEGOTIATOR_PORT);
        u.pathname = '/';
        u.search = '';
        u.hash = '';
        return u.toString().replace(/\/+$/, '');
    }
    catch {
        return null;
    }
}
/** 向目标设备自动申请麦克风接入凭证（权限自动化：首次需对方人工允许，之后信任免问）。 */
async function autoNegotiateMic(dev) {
    if (!dev.base)
        return { ok: false, error: '设备基址不可用' };
    const negBase = negotiatorBase(dev.base);
    if (!negBase)
        return { ok: false, error: '设备基址解析失败' };
    let ident;
    try {
        ident = (await call('device_identity'));
    }
    catch (e) {
        return { ok: false, error: '无法读取本机身份：' + e.message };
    }
    // 客户端超时 15s（服务器侧挂起 60s 等人工确认；超过说明对方没响应/未就绪）
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), 15000);
    try {
        const resp = await fetch(negBase + '/api/negotiator/request', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                deviceId: ident.deviceId,
                deviceName: ident.deviceName,
                media: ['mic'],
            }),
            signal: ctrl.signal,
        });
        if (!resp.ok) {
            const err = (await resp.json().catch(() => null));
            return { ok: false, error: (err && err.error) || `协商失败（HTTP ${resp.status}）` };
        }
        const grant = (await resp.json());
        if (!grant.token || !grant.streamId) {
            return { ok: false, error: '协商响应缺少凭证字段' };
        }
        return { ok: true, token: grant.token, streamId: grant.streamId };
    }
    catch (e) {
        if (ctrl.signal.aborted)
            return { ok: false, error: '等待对方确认超时（15s）' };
        return { ok: false, error: '无法连接设备协商端点：' + e.message };
    }
    finally {
        clearTimeout(timer);
    }
}
/** 打开「共享麦克风」弹窗（手机/PC 端对目标设备）：优先自动协商免粘贴，失败回退手动。 */
async function openMicShare(dev) {
    if (dev.base == null)
        return;
    micShare = { base: dev.base, quicPort: null, active: false };
    // 拉取对端 /api/info 的 QUIC 端口（旧版本中继无此端点 → 走 WS）
    try {
        const iresp = await fetch(dev.base.replace(/\/+$/, '') + '/api/info', { cache: 'no-store' });
        if (iresp.ok) {
            const info = (await iresp.json());
            micShare.quicPort = info.quicPort || null;
        }
    }
    catch (_) { /* QUIC 不可用 */ }
    $('mic-modal-device').textContent = `推送到 ${dev.name}（${dev.meta}）`;
    if (micShareLastBase === dev.base && streaming) {
        // 正是推往该设备的定向共享（重开弹窗）：恢复进行中状态，停止按钮可用
        micShare.active = true;
        setMicRunning(true);
        $input('mic-token-input').disabled = true;
        $('mic-status').textContent = '推流中（凭证已出示）…';
    }
    else {
        // 优先自动协商：向设备申请凭证，成功直接推流（首次共享对方电脑会弹「允许」）
        const r = await autoNegotiateMic(dev);
        if (r.ok && r.token && r.streamId && micShare) {
            $input('mic-token-input').value = '';
            $('mic-error').classList.add('hidden');
            try {
                await startMicShareWith({
                    token: r.token,
                    streamId: r.streamId,
                    base: micShare.base,
                    quicPort: micShare.quicPort,
                });
                $('mic-status').textContent = '已自动获取凭证，推流中…（首次共享需对方电脑点「允许」）';
            }
            catch (_) { /* 错误已显示在弹窗 */ }
            setMicRunning(true);
        }
        else {
            // 自动协商失败 → 回退手动粘贴
            $input('mic-token-input').value = '';
            $input('mic-token-input').placeholder = '粘贴电脑端「接收手机麦克风」展示的接入凭证';
            $('mic-status').textContent =
                '自动协商未成功（' + (r.error || '未知原因') + '），可粘贴凭证，或点「自动获取凭证」重试';
            $('mic-error').classList.add('hidden');
            setMicRunning(false);
            $input('mic-token-input').disabled = false;
        }
    }
    $('mic-modal').classList.remove('hidden');
    $input('mic-token-input').focus();
}
/** 解析凭证并开始推流：stream_id 用接收端签发的 id，目标 = 该设备中继。 */
async function startMicShare() {
    const tokenStr = $input('mic-token-input').value.trim();
    const errBox = $('mic-error');
    errBox.classList.add('hidden');
    if (!tokenStr) {
        errBox.textContent = '请粘贴电脑端展示的接入凭证';
        errBox.classList.remove('hidden');
        return;
    }
    let parsed;
    try {
        parsed = JSON.parse(tokenStr);
    }
    catch {
        errBox.textContent = '凭证不是合法的 JSON（应整体复制电脑端凭证）';
        errBox.classList.remove('hidden');
        return;
    }
    if (!parsed.streamId) {
        errBox.textContent = '凭证缺少 streamId，可能已损坏';
        errBox.classList.remove('hidden');
        return;
    }
    if (!micShare)
        return;
    await startMicShareWith({
        token: tokenStr,
        streamId: parsed.streamId,
        base: micShare.base,
        quicPort: micShare.quicPort,
    });
}
/** 用已获取的凭证推流到目标设备中继（自动协商与手动粘贴共用入口）。 */
async function startMicShareWith(p) {
    const relayUrl = pushUrlForDevice(p.base, p.quicPort);
    $('mic-status').textContent = '连接 ' + relayUrl + ' …';
    try {
        const cfg = {
            streamId: p.streamId,
            title: '手机麦克风',
            video: null, // 纯音频：Android 采集跳过屏幕授权，只采麦克风
            quality: QUALITIES.LOW,
            audio: { mic: null, systemAudio: null, sampleRate: 48000, channels: 2, bitrateKbps: 128 },
            durationSecs: null,
            shareToken: p.token,
        };
        await call('start_stream', { cfg, relayUrl });
        if (micShare)
            micShare.active = true;
        micShareLastBase = p.base;
        setMicRunning(true);
        $input('mic-token-input').disabled = true;
        $('mic-status').textContent = '推流中（已出示凭证）…';
        void renderShares();
        void pollMicShareStatus();
    }
    catch (e) {
        const errBox = $('mic-error');
        errBox.textContent = '推流启动失败：' + e.message;
        errBox.classList.remove('hidden');
        $('mic-status').textContent = '';
        throw e; // 让调用方（自动协商）得知失败
    }
}
async function stopMicShare() {
    try {
        await call('stop_stream');
    }
    catch (_) { /* ignore */ }
    if (micShare)
        micShare.active = false;
    setMicRunning(false);
    $input('mic-token-input').disabled = false;
    $('mic-status').textContent = '已停止';
}
/** 共享麦克风实时状态（stream_status 常驻轮询之外补充采集真实状态）。 */
async function pollMicShareStatus() {
    if (!micShare || !micShare.active)
        return;
    // capture_status 反映 Android 原生采集真实状态（micOnly 授权失败会回传错误）
    if (IS_ANDROID) {
        try {
            const cs = (await call('capture_status'));
            if (cs.error) {
                micShare.active = false;
                setMicRunning(false);
                $('mic-error').textContent = '采集失败：' + cs.error;
                $('mic-error').classList.remove('hidden');
                $input('mic-token-input').disabled = false;
                return;
            }
            $('mic-status').textContent = cs.started ? '麦克风采集中，推流中…' : '等待麦克风授权…';
        }
        catch (_) { /* ignore */ }
    }
    const st = (await call('stream_status').catch(() => null));
    if (st && !st.running) {
        if (micShare)
            micShare.active = false;
        setMicRunning(false);
        $input('mic-token-input').disabled = false;
        $('mic-status').textContent = '推流已结束';
        return;
    }
    setTimeout(() => void pollMicShareStatus(), 2000);
}
function setMicRunning(r) {
    $btn('mic-start-btn').disabled = r;
    $btn('mic-stop-btn').disabled = !r;
}
// ---------------------------------------------------------------------------
// 电脑端「接收手机麦克风」：签发展示凭证 + 自动等待接入并播放
// ---------------------------------------------------------------------------
/** 签发凭证并展示：手机在自身设备列表点本机 → 共享麦克风 → 粘贴凭证即可。
 *  随后轮询本机串流列表，流出现即自动原生接收（扬声器播放，B3）。 */
async function startMicReceive() {
    const btn = $btn('mic-recv-btn');
    setBtnLoading(btn, true);
    hideGridError();
    try {
        const v = (await call('issue_share_token', { ttlSecs: 600 }));
        micRecv = { streamId: v.streamId, checking: false, received: false };
        $('mic-recv-panel').classList.remove('hidden');
        $('mic-recv-pin').textContent = 'PIN ' + v.pin;
        $input('mic-recv-token').value = v.token;
        setBtnLoading(btn, false);
        $('mic-recv-status').textContent = '等待手机接入…（凭证 ' + fmtSecs(v.expiresAt) + ' 过期）';
        void pollMicRecv();
    }
    catch (e) {
        setBtnLoading(btn, false);
        showGridError('签发凭证失败：' + e.message);
    }
}
/** 到期时间倒计时文案（Unix 秒 → "约 N 分钟"）。 */
function fmtSecs(expiresAt) {
    const mins = Math.max(1, Math.round((expiresAt - Date.now() / 1000) / 60));
    return `约 ${mins} 分钟`;
}
/** 轮询本机受控中继串流列表：凭证对应的流接入后自动开始原生接收。 */
async function pollMicRecv() {
    if (!micRecv || micRecv.checking || micRecv.received)
        return;
    micRecv.checking = true;
    try {
        if (anchor) {
            const resp = await fetch(`http://127.0.0.1:${anchor.port}/api/streams`, { cache: 'no-store' });
            if (resp.ok) {
                const data = (await resp.json());
                const list = Array.isArray(data) ? data : (data.streams || []);
                if (list.some((s) => s.streamId === micRecv.streamId)) {
                    micRecv.received = true;
                    $('mic-recv-status').textContent = '手机已接入，正在通过电脑扬声器播放…';
                    $('mic-recv-status').style.color = 'var(--ok)';
                    // 自动原生接收（音频设备输出；纯音频流无画面属正常）
                    void startReceive(micRecv.streamId);
                    return;
                }
            }
        }
    }
    catch (_) { /* 中继短暂不可达，下一轮重试 */ }
    micRecv.checking = false;
    if (micRecv)
        setTimeout(() => void pollMicRecv(), 2000);
}
