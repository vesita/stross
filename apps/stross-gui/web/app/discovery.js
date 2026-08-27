"use strict";
// Stross 前端 —— 发现域（script 全局作用域）：
// 本机锚定（start_relay + mDNS 广播）+ 局域网设备扫描 + 手动添加 +
// 设备图渲染（本机卡片 + 设备卡片）。
//
// 分层（docs/layering-architecture.md）：mDNS 浏览 + `/api/info` `/api/streams`
// 探测 + 聚合全部收敛在 Rust（`scan_devices` 命令 → `stross_app::devices::scan`）；
// 本文件只做**渲染与手动地址持久化**，不再自带 fetch 探测客户端。
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
/** 局域网设备探测超时（ms；Rust 侧聚合按此探测每台设备）。 */
const PROBE_TIMEOUT_MS = 2000;
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
        renderLocalCard(); // 本机卡片状态更新（SRT/QUIC 端口随下一轮扫描到位）
        void refreshDevices(); // 锚点端口 + 本机/对端在线共享随扫描结果到位
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
/** 手动添加设备地址（免 mDNS）：探测可达后进入设备列表。
 *  探测走 `probe_relay` 命令（core 官方客户端），前端不再 fetch。 */
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
        const ok = (await call('probe_relay', { base: addr }));
        if (!ok)
            throw new Error('中继不可达（无 /api/streams）');
    }
    catch (e) {
        showGridError('无法访问 ' + addr + '：' + e.message);
        return;
    }
    manualRelays = [addr, ...manualRelays.filter((u) => u !== addr)];
    renderRecent();
    void refreshDevices(true); // 设备列表出现该设备（含其在线共享）
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
/** 扫描条目 → 设备卡片基址（http://ip:port）。 */
function baseOf(d) {
    return `http://${d.ip}:${d.port}`;
}
/** 全量刷新设备列表 + 在线共享 + 锚点端口。
 *
 * mDNS 浏览 + `/api/info` `/api/streams` 探测 + 聚合全部在 Rust
 * `scan_devices` 命令（`stross_app::devices::scan`）；前端只渲染结果，
 * 手动地址通过 `extraBaseUrls` 一并探测并入。
 */
async function refreshDevices(force = false) {
    if (scanInFlight)
        return;
    if (!force && discoverCacheAt && Date.now() - discoverCacheAt < DISCOVER_TTL_MS)
        return;
    scanInFlight = true;
    try {
        const devs = (await call('scan_devices', {
            probeMs: PROBE_TIMEOUT_MS,
            extraBaseUrls: manualRelays.map((a) => a.replace(/\/+$/, '')),
        }));
        // 本机条目（isSelf，按回环探测）：同步锚点 SRT/QUIC 端口 + 本机在线共享
        const local = devs.find((d) => d.isSelf) || null;
        if (local && local.online) {
            if (anchor) {
                anchor.srtUrl = local.srtPort ? `srt://127.0.0.1:${local.srtPort}` : null;
                anchor.quicUrl = local.quicPort ? `quic://127.0.0.1:${local.quicPort}` : null;
            }
            localStreams = local.streams;
        }
        else {
            localStreams = [];
        }
        // 远端设备卡片（探测已在 Rust 完成：含在线共享 / SRT / QUIC）
        const cards = devs
            .filter((d) => !d.isSelf)
            .map((d) => ({
            key: baseOf(d),
            name: d.name || 'Stross 设备',
            meta: d.ip + ':' + d.port,
            isLocal: false,
            roles: d.roles || [],
            manual: manualRelays.some((a) => a.replace(/\/+$/, '') === baseOf(d)),
            base: baseOf(d),
            srtUrl: d.srtPort ? `srt://${d.ip}:${d.srtPort}` : null,
            quicUrl: d.quicPort ? `quic://${d.ip}:${d.quicPort}` : null,
            quicPort: d.quicPort,
            streams: d.streams || [],
        }));
        // 手动添加但当前不可达的地址保留在列表（提示不可达而非消失）
        manualRelays.forEach((addr) => {
            const base = addr.replace(/\/+$/, '');
            if (!cards.some((c) => c.base === base)) {
                const hostPort = base.replace(/^https?:\/\//, '');
                cards.push({
                    key: base,
                    name: hostPort + '（手动，不可达）',
                    meta: hostPort,
                    isLocal: false,
                    roles: [],
                    manual: true,
                    base,
                    srtUrl: null,
                    quicUrl: null,
                    quicPort: null,
                    streams: [],
                });
            }
        });
        // 填充远端流缓存（按需接收时取流元数据）
        cards.forEach((c) => c.streams.forEach((s) => remoteStreams.set(s.streamId, s)));
        // 保留已展开状态；本机卡片由渲染器恒置首位
        const keepExpanded = expandedDevice;
        deviceViews = cards;
        if (keepExpanded && !deviceViews.some((d) => d.key === keepExpanded))
            expandedDevice = null;
        renderDeviceList();
        renderLocalStreams();
    }
    catch (e) {
        showGridError('扫描失败：' + e.message);
    }
    finally {
        scanInFlight = false;
        discoverCacheAt = Date.now();
    }
}
/** 兼容入口（初始化 / 强制刷新）。 */
function scanRelays() {
    return refreshDevices(true);
}
/** 渲染左栏设备列表：本机卡片 + 各设备卡片（设备可展开）。 */
function renderDeviceList() {
    const box = $('device-list');
    box.innerHTML = '';
    box.appendChild(localDeviceCard());
    // 本机卡片重建会重置 ip-list 为「读取中…」占位——重渲染 IP 列表
    // （scanRelays/scanRemoteStreams 每次重建设备列表都会走到这里）
    renderIps(MY_IPS);
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
    const localStreamsBox = document.createElement('div');
    localStreamsBox.className = 'dev-streams';
    localStreamsBox.dataset.role = 'local-streams';
    const lsTitle = document.createElement('h3');
    lsTitle.textContent = '本机在线共享';
    localStreamsBox.appendChild(lsTitle);
    localStreamsBox.appendChild(streamListPlaceholder());
    detail.appendChild(localStreamsBox);
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
/** 兼容入口：周期刷新在线共享/设备列表（TTL 与 in-flight 守卫在
 *  `refreshDevices` 内；探测已收敛到 Rust，不再按设备 fetch）。 */
function scanRemoteStreams(force = false) {
    return refreshDevices(force);
}
/** 渲染本机卡片流区（本机在线共享）。 */
function renderLocalStreams() {
    const box = document.querySelector('[data-role="local-streams"]');
    if (!box)
        return;
    box.innerHTML = '';
    const title = document.createElement('h3');
    title.textContent = '本机在线共享';
    box.appendChild(title);
    if (!localStreams.length) {
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
        quicPort: null,
        streams: localStreams,
    };
    localStreams.forEach((s) => box.appendChild(streamItem(localDev, s)));
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
/** 刷新本机卡片锚点状态行（锚定成功后调用；SRT/QUIC 就绪状态在
 *  `refreshAnchorPorts` 拉取后二次刷新）。 */
function renderLocalCard() {
    const meta = $('anchor-box');
    if (meta) {
        const transports = anchor && (anchor.srtUrl || anchor.quicUrl)
            ? (anchor.srtUrl ? ' · SRT' : '') + (anchor.quicUrl ? ' · QUIC' : '')
            : '';
        meta.textContent = anchor
            ? `已锚定 · 中继端口 ${anchor.port} · mDNS 广播中${transports}`
            : '未锚定';
    }
}
