"use strict";
// Stross 前端 —— 端点框架交互域（节点 → 设备 → 端点：广播 + 订阅）。
//
// 分层（docs/layering-architecture.md）：流程全部走 Rust 命令
// （local_catalog / endpoint_publish / endpoint_unpublish /
// endpoint_ls / endpoint_subscribe_media），本文件只做渲染与参数转译。
//
// · 本机节点：设备树（local_catalog）→ 共享（选可见性/delivery）生成端点、
//   已共享设备显示徽标 + 取消共享；
// · 对端节点：展开拉目录（endpoint_ls）→ 可订阅端点 → 订阅（endpoint_subscribe_media
//   握手）→ 走既有 start_receive 观看/播放。
// ---------------------------------------------------------------------------
// 本机：目录刷新 + 设备树渲染
// ---------------------------------------------------------------------------
/** 本机目录渲染签名（数据未变则跳过重建——2s 轮询不再闪屏）。 */
let lastLocalCatalogSig = '';
/** 拉取本机目录（设备 + 已公开端点）并重渲染设备树。 */
async function refreshLocalCatalog() {
    try {
        const next = await call('local_catalog');
        const sig = JSON.stringify(next.endpoints);
        if (sig === lastLocalCatalogSig)
            return;
        lastLocalCatalogSig = sig;
        localCatalog = next;
        renderLocalDevices();
    }
    catch { }
}
/** 本机端点树渲染（写入本机卡片 [data-role="local-devices"] 容器）。 */
function renderLocalDevices() {
    const box = document.querySelector('[data-role="local-devices"] .dev-list');
    if (!box)
        return;
    box.innerHTML = '';
    if (!localCatalog.endpoints.length) {
        box.appendChild(emptyState('server', '本机暂无可共享的内容', '未检测到可用的屏幕、摄像头或音频采集源'));
        return;
    }
    for (const ep of localCatalog.endpoints) {
        const row = document.createElement('div');
        row.className =
            'ep-row' +
                (ep.available ? '' : ' ep-unavail') +
                (ep.published ? ' ep-published' : '') +
                (ep.state === 'active' ? ' ep-active' : '');
        const ic = document.createElement('span');
        ic.className = 'ep-ic';
        ic.innerHTML = icon(deviceKindIcon(ep.kind));
        const body = document.createElement('span');
        body.className = 'ep-body';
        const name = document.createElement('span');
        name.className = 'ep-name';
        const nameText = document.createElement('span');
        nameText.textContent = ep.name;
        name.appendChild(nameText);
        if (ep.published) {
            const pill = document.createElement('span');
            pill.className = 'badge ep-badge' + (ep.state === 'active' ? ' live' : ' ok');
            if (ep.state === 'active') {
                pill.innerHTML =
                    '<span class="live-dot"></span><span>' +
                        (ep.subscribers ? `${ep.subscribers} 订阅中` : '正在共享') +
                        '</span>';
            }
            else {
                pill.textContent = '已共享';
            }
            name.appendChild(pill);
        }
        const meta = document.createElement('span');
        meta.className = 'ep-meta';
        if (!ep.available) {
            meta.textContent = '不可用（' + (ep.lastError || '未知原因') + '）';
        }
        else if (ep.published) {
            meta.textContent =
                labelOf(VISIBILITY_LABELS, ep.visibility) +
                    (ep.state === 'active'
                        ? (ep.subscribers ? ` · ${ep.subscribers} 台设备正在接收` : ' · 正在传输')
                        : ' · 待连接');
        }
        else {
            meta.textContent = '未开启共享';
        }
        body.appendChild(name);
        body.appendChild(meta);
        row.appendChild(ic);
        row.appendChild(body);
        if (!ep.available) {
            const hint = document.createElement('span');
            hint.className = 'hint';
            hint.textContent = '不可用';
            row.appendChild(hint);
        }
        else if (ep.published) {
            const ops = document.createElement('span');
            ops.className = 'ep-actions';
            if (ep.state === 'active') {
                const stop = document.createElement('button');
                stop.type = 'button';
                stop.className = 'sm danger ep-act';
                stop.innerHTML = icon('stop') + '<span>停止推流</span>';
                stop.dataset.act = 'stop-share';
                stop.dataset.endpoint = endpointIdStr(ep);
                ops.appendChild(stop);
                const unpub = document.createElement('button');
                unpub.type = 'button';
                unpub.className = 'icon-btn-sm ep-act';
                unpub.title = '取消共享（完全下线）';
                unpub.innerHTML = icon('x');
                unpub.dataset.act = 'unpublish-endpoint';
                unpub.dataset.endpoint = endpointIdStr(ep);
                ops.appendChild(unpub);
            }
            else {
                const unpub = document.createElement('button');
                unpub.type = 'button';
                unpub.className = 'sm ep-act';
                unpub.innerHTML = icon('x') + '<span>取消共享</span>';
                unpub.dataset.act = 'unpublish-endpoint';
                unpub.dataset.endpoint = endpointIdStr(ep);
                ops.appendChild(unpub);
            }
            row.appendChild(ops);
        }
        else {
            const pub = document.createElement('button');
            pub.type = 'button';
            pub.className = 'sm primary ep-act';
            pub.innerHTML = icon('radio') + '<span>共享</span>';
            pub.dataset.act = 'publish-device';
            pub.dataset.device = endpointIdStr(ep);
            row.appendChild(pub);
        }
        box.appendChild(row);
    }
}
// ---------------------------------------------------------------------------
// 共享（本机设备 → 端点）
// ---------------------------------------------------------------------------
/** 打开共享弹窗（可见性由公开者声明；数据面方向由端点/系统自动决定）。 */
function openPublishModal(endpointId) {
    const ep = localCatalog.endpoints.find((x) => endpointIdStr(x) === endpointId);
    if (!ep)
        return;
    publishTarget = { ep };
    $('pub-modal-title').textContent = `共享「${ep.name}」`;
    $('pub-modal-sub').textContent = '开启后局域网其它节点可订阅并接收此内容';
    document.querySelector('input[name="pub-vis"][value="confirm"]').checked = true;
    $('pub-error').classList.add('hidden');
    $('pub-modal').classList.remove('hidden');
}
/** 确认共享。 */
async function confirmPublish() {
    if (!publishTarget)
        return;
    const vis = document.querySelector('input[name="pub-vis"]:checked').value;
    const delivery = publishTarget.ep.delivery || 'pull';
    const btn = $btn('pub-confirm-btn');
    setBtnLoading(btn, true);
    $('pub-error').classList.add('hidden');
    try {
        const name = publishTarget.ep.name;
        await call('endpoint_publish', {
            deviceId: endpointIdStr(publishTarget.ep),
            visibility: vis,
            delivery,
        });
        $('pub-modal').classList.add('hidden');
        showToast(`已共享「${name}」`, 'ok');
        await refreshLocalCatalog();
    }
    catch (e) {
        $('pub-error').textContent = '共享失败：' + errMsg(e);
        $('pub-error').classList.remove('hidden');
    }
    finally {
        setBtnLoading(btn, false);
    }
}
/** 取消共享（活动共享联动停止——取消共享 = 不再共享，踢出当前订阅者）。 */
async function unpublishEndpoint(endpointId) {
    try {
        await call('endpoint_unpublish', { endpointId });
        showToast('已取消共享', 'info');
        await refreshLocalCatalog();
    }
    catch (e) {
        showGridError('取消共享失败：' + errMsg(e));
    }
}
/** 停止端点活动共享（停流 + 拆会话，保留共享；订阅者断开后也会自动收尾）。 */
async function stopShare(endpointId) {
    try {
        await call('endpoint_stop_share', { endpointId });
        showToast('已停止共享', 'info');
        await refreshLocalCatalog();
    }
    catch (e) {
        showGridError('停止共享失败：' + errMsg(e));
    }
}
// ---------------------------------------------------------------------------
// 对端：目录拉取 + 订阅
// ---------------------------------------------------------------------------
/** 对端目录缓存 TTL：目录是共享快照，短 TTL 让对端新共享/取消共享及时可见。
 *  展开卡片的目录随扫描周期（refreshDevices）刷新，控制在 8s 内反映变化。 */
const REMOTE_DIR_TTL_MS = 8000;
/** 拉取对端节点目录（endpoint_ls；端口缺省 = 库层默认协商端口）。
 *  `force = true` 绕过 TTL 强制拉取。 */
async function loadRemoteDir(dev, force = false) {
    const host = deviceHostOf(dev);
    if (!host)
        return;
    const cached = remoteDirs.get(dev.key);
    const cachedAt = remoteDirAt.get(dev.key);
    if (!force && cached && cachedAt && Date.now() - cachedAt < REMOTE_DIR_TTL_MS) {
        renderRemoteDir(dev, cached);
        if (selectedDevice?.key === dev.key) {
            renderBrowsePaneEndpoints(dev, cached);
        }
        return;
    }
    if (remoteDirLoading.has(dev.key))
        return;
    remoteDirLoading.add(dev.key);
    const box = document.querySelector(`[data-role="remote-dir"][data-key="${dev.key}"] .dir-status`);
    if (box)
        box.textContent = '目录加载中…';
    const browseBox = $('node-browse-endpoints');
    if (selectedDevice?.key === dev.key && browseBox && !browseBox.querySelector('.node-ep-card')) {
        browseBox.innerHTML = '<div class="hint" style="padding: 16px; text-align: center;">加载端点中…</div>';
    }
    try {
        const dir = await Promise.race([
            call('endpoint_ls', { host }),
            new Promise((_, rej) => setTimeout(() => rej(new Error('目录拉取超时')), 4000)),
        ]);
        remoteDirs.set(dev.key, dir);
        remoteDirAt.set(dev.key, Date.now());
        renderRemoteDir(dev, dir);
        if (selectedDevice?.key === dev.key) {
            renderBrowsePaneEndpoints(dev, dir);
        }
    }
    catch (e) {
        if (box) {
            box.textContent = '目录不可用（' + errMsg(e) + '）';
            box.classList.add('hint');
        }
        if (selectedDevice?.key === dev.key && browseBox) {
            browseBox.innerHTML = `<div class="hint" style="padding: 16px; text-align: center; color: var(--err);">端点获取失败（${errMsg(e)}）</div>`;
        }
    }
    finally {
        remoteDirLoading.delete(dev.key);
    }
}
winObj.loadRemoteDir = loadRemoteDir;
/** 对端节点目录渲染（设备 + 可订阅端点；写入卡片 [data-role="remote-dir"]）。 */
function renderRemoteDir(dev, dir) {
    const container = document.querySelector(`[data-role="remote-dir"][data-key="${dev.key}"]`);
    if (!container)
        return;
    container.innerHTML = '';
    const title = document.createElement('h3');
    title.textContent = '可订阅的内容';
    container.appendChild(title);
    if (!dir.endpoints.length) {
        container.appendChild(emptyState('radio', '该节点暂未共享任何内容', '对方可以在其节点列表中点击「共享」开启推流'));
        return;
    }
    for (const ep of dir.endpoints) {
        const row = document.createElement('div');
        row.className = 'ep-row';
        const ic = document.createElement('span');
        ic.className = 'ep-ic';
        ic.innerHTML = icon(deviceKindIcon(ep.kind));
        const body = document.createElement('span');
        body.className = 'ep-body';
        const name = document.createElement('span');
        name.className = 'ep-name';
        name.textContent = ep.name;
        const meta = document.createElement('span');
        meta.className = 'ep-meta';
        meta.textContent =
            labelOf(VISIBILITY_LABELS, ep.visibility) +
                (ep.subscribers ? ` · ${ep.subscribers} 订阅中` : '');
        body.appendChild(name);
        body.appendChild(meta);
        row.appendChild(ic);
        row.appendChild(body);
        if (!ep.available) {
            const hint = document.createElement('span');
            hint.className = 'hint';
            hint.textContent = '不可订阅（' + (ep.lastError || '未知原因') + '）';
            row.appendChild(hint);
        }
        else if (ep.kind === 'file') {
            const hint = document.createElement('span');
            hint.className = 'hint';
            hint.textContent = '文件（命令行订阅）';
            row.appendChild(hint);
        }
        else if (subscribedEndpoints.has(deviceHostOf(dev) + '/' + endpointIdStr(ep)) ||
            subscribedEndpoints.has(endpointIdStr(ep)) ||
            recvLinks.has(deviceHostOf(dev) + '/' + endpointIdStr(ep))) {
            const badge = document.createElement('span');
            badge.className = 'badge ep-badge live';
            badge.textContent = '已订阅 · 接收中';
            row.appendChild(badge);
        }
        else if (subscribingEndpoint &&
            subscribingEndpoint.host === deviceHostOf(dev) &&
            subscribingEndpoint.endpointId === endpointIdStr(ep)) {
            const sub = document.createElement('button');
            sub.type = 'button';
            sub.className = 'sm ep-act';
            sub.disabled = true;
            sub.innerHTML = '<span class="spinner"></span><span>正在订阅…</span>';
            row.appendChild(sub);
        }
        else {
            const sub = document.createElement('button');
            sub.type = 'button';
            sub.className = 'sm primary ep-act';
            sub.innerHTML = icon('download') + '<span>订阅</span>';
            sub.dataset.act = 'subscribe-endpoint';
            sub.dataset.host = deviceHostOf(dev) || '';
            sub.dataset.endpoint = endpointIdStr(ep);
            row.appendChild(sub);
        }
        container.appendChild(row);
    }
    if (expandedDevice === dev.key) {
        renderBrowsePaneEndpoints(dev, dir);
    }
}
/** 专属端点浏览面板渲染（节点二级页的「端点浏览」Tab 内容）。 */
function renderBrowsePaneEndpoints(dev, dir) {
    const browseBox = $('node-browse-endpoints');
    const countEl = $('node-browse-count');
    if (countEl) {
        countEl.textContent = dir.endpoints.length ? `共 ${dir.endpoints.length} 个可订阅端点` : '暂无可订阅端点';
    }
    const specIp = $('spec-ip');
    const specPort = $('spec-port');
    const specOnline = $('spec-online');
    if (specIp)
        specIp.textContent = dev.meta;
    if (specPort)
        specPort.textContent = dev.quicPort ? `QUIC ${dev.quicPort}` : (dev.base ? dev.base.split(':')[2] || '8777' : '8777');
    if (specOnline) {
        const isOnline = !dev.name.includes('不可达');
        specOnline.textContent = isOnline ? '在线可连接' : '离线不可达';
        specOnline.className = 'specs-v ' + (isOnline ? 'ok' : 'err');
    }
    if (!browseBox)
        return;
    browseBox.innerHTML = '';
    if (!dir.endpoints.length) {
        browseBox.appendChild(emptyState('radio', '该节点暂未共享任何端点', '对方开启屏幕、麦克风或文件共享后将在此处呈现'));
        return;
    }
    for (const ep of dir.endpoints) {
        const card = document.createElement('div');
        card.className = 'node-ep-card';
        const main = document.createElement('div');
        main.className = 'node-ep-main';
        const ic = document.createElement('span');
        ic.className = 'node-ep-ic';
        ic.innerHTML = icon(deviceKindIcon(ep.kind));
        const info = document.createElement('div');
        info.className = 'node-ep-info';
        const name = document.createElement('span');
        name.className = 'node-ep-name';
        name.textContent = ep.name;
        const meta = document.createElement('span');
        meta.className = 'node-ep-meta';
        meta.textContent =
            labelOf(DEVICE_KIND_LABELS, ep.kind) +
                ' · ' +
                labelOf(VISIBILITY_LABELS, ep.visibility) +
                (ep.subscribers ? ` · ${ep.subscribers} 正在订阅` : '');
        info.appendChild(name);
        info.appendChild(meta);
        main.appendChild(ic);
        main.appendChild(info);
        card.appendChild(main);
        const isSubbed = subscribedEndpoints.has(deviceHostOf(dev) + '/' + endpointIdStr(ep)) ||
            subscribedEndpoints.has(endpointIdStr(ep)) ||
            recvLinks.has(deviceHostOf(dev) + '/' + endpointIdStr(ep));
        if (isSubbed) {
            const badge = document.createElement('span');
            badge.className = 'badge ep-badge live';
            badge.textContent = '已订阅 · 接收中';
            card.appendChild(badge);
        }
        else if (!ep.available) {
            const hint = document.createElement('span');
            hint.className = 'hint';
            hint.textContent = '不可用';
            card.appendChild(hint);
        }
        else {
            const subBtn = document.createElement('button');
            subBtn.type = 'button';
            subBtn.className = 'sm primary ep-act';
            subBtn.innerHTML = icon('download') + '<span>订阅</span>';
            subBtn.onclick = () => {
                openSubscribeModal(deviceHostOf(dev), endpointIdStr(ep));
            };
            card.appendChild(subBtn);
        }
        browseBox.appendChild(card);
    }
}
/** 设备视图 → 对端主机（http://ip:port 基址取 host）。 */
function deviceHostOf(dev) {
    if (dev.base)
        return dev.base.replace(/^https?:\/\//, '').split(':')[0];
    return '';
}
// ---------------------------------------------------------------------------
// 订阅（对端端点 → 本机接收）
// ---------------------------------------------------------------------------
/** 打开订阅弹窗：订阅者只确认「订阅并接收」。 */
function openSubscribeModal(host, endpointId) {
    const dev = deviceViews.find((d) => d.key && deviceHostOf(d) === host);
    const dir = dev ? remoteDirs.get(dev.key) : null;
    const ep = dir?.endpoints.find((e) => endpointIdStr(e) === endpointId);
    if (!ep)
        return;
    subscribeTarget = { host, ep };
    $('sub-modal-title').textContent = `订阅「${ep.name}」`;
    $('sub-modal-sub').textContent =
        '订阅后将实时接收对方共享的媒体画面' +
            (ep.visibility === 'public' ? '（免确认）' : '（需对端授权）');
    $('sub-error').classList.add('hidden');
    $('sub-modal').classList.remove('hidden');
}
/** 确认订阅：握手 → 拿到 watch 入口 → 走既有 start_receive 观看/播放。 */
async function confirmSubscribe() {
    if (!subscribeTarget)
        return;
    const btn = $btn('sub-confirm-btn');
    setBtnLoading(btn, true);
    $('sub-error').classList.add('hidden');
    try {
        subscribingEndpoint = {
            host: subscribeTarget.host,
            endpointId: endpointIdStr(subscribeTarget.ep),
        };
        renderDeviceList();
        const r = await call('endpoint_subscribe_media', {
            host: subscribeTarget.host,
            endpointId: endpointIdStr(subscribeTarget.ep),
            delivery: subscribeTarget.ep.delivery === 'push' ? 'push' : undefined,
        });
        subscribingEndpoint = null;
        $('sub-modal').classList.add('hidden');
        targetRelay = { wsBase: r.relayUrl, srtUrl: null, quicUrl: null };
        const ok = await startReceiveLink({
            host: subscribeTarget.host,
            endpointId: endpointIdStr(subscribeTarget.ep),
            endpointName: subscribeTarget.ep.name,
            streamId: r.streamId,
            kind: subscribeTarget.ep.kind,
        });
        if (ok) {
            subscribedEndpoints.add(subscribeTarget.host + '/' + endpointIdStr(subscribeTarget.ep));
            showToast(`已订阅「${subscribeTarget.ep.name}」`, 'ok');
            switchMobileTab('recv');
            switchNodeSubtab('player');
            const dot = $('node-player-dot');
            if (dot)
                dot.classList.remove('hidden');
        }
        renderDeviceList();
    }
    catch (e) {
        subscribingEndpoint = null;
        renderDeviceList();
        $('sub-error').textContent = '订阅失败：' + errMsg(e);
        $('sub-error').classList.remove('hidden');
    }
    finally {
        setBtnLoading(btn, false);
    }
}
