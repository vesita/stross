"use strict";
// Stross 前端 —— 端点框架交互域（节点 → 设备 → 端点：广播 + 订阅）。
//
// 分层（docs/layering-architecture.md）：流程全部走 Rust 命令
// （local_catalog / endpoint_publish / endpoint_unpublish /
// endpoint_ls / endpoint_subscribe_media），本文件只做渲染与参数转译。
//
// · 本机节点：设备树（local_catalog）→ 通告（选可见性/delivery）生成端点、
//   已通告设备显示徽标 + 取消通告；
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
        const next = (await call('local_catalog'));
        const sig = JSON.stringify(next.endpoints);
        if (sig === lastLocalCatalogSig)
            return;
        lastLocalCatalogSig = sig;
        localCatalog = next;
        renderLocalDevices();
    }
    catch (_) {
        // 目录拉取失败不打断主流程（设备树保留旧快照）
    }
}
/** 本机端点树渲染（写入本机卡片 [data-role="local-devices"] 容器）。 */
function renderLocalDevices() {
    const box = document.querySelector('[data-role="local-devices"] .dev-list');
    if (!box)
        return;
    box.innerHTML = '';
    if (!localCatalog.endpoints.length) {
        box.appendChild(emptyState('server', '暂无可共享的端点'));
        return;
    }
    for (const ep of localCatalog.endpoints) {
        const row = document.createElement('div');
        row.className = 'ep-row' + (ep.available ? '' : ' ep-unavail');
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
        const kindLabel = labelOf(DEVICE_KIND_LABELS, ep.kind);
        meta.textContent = ep.available
            ? kindLabel
            : `${kindLabel} · 不可用（${ep.lastError || '未知原因'}）`;
        body.appendChild(name);
        body.appendChild(meta);
        row.appendChild(ic);
        row.appendChild(body);
        if (!ep.available) {
            const hint = document.createElement('span');
            hint.className = 'hint';
            hint.textContent = '不可挂载';
            row.appendChild(hint);
        }
        else if (ep.published) {
            const badge = document.createElement('span');
            badge.className = 'badge ep-badge';
            badge.textContent =
                '已通告 · ' + labelOf(VISIBILITY_LABELS, ep.visibility) +
                    ' · ' + labelOf(DELIVERY_LABELS, ep.delivery) +
                    (ep.subscribers ? ` · ${ep.subscribers} 订阅` : '');
            row.appendChild(badge);
            const unpub = document.createElement('button');
            unpub.type = 'button';
            unpub.className = 'sm ep-act';
            unpub.innerHTML = icon('x') + '<span>取消通告</span>';
            unpub.dataset.act = 'unpublish-endpoint';
            unpub.dataset.endpoint = ep.endpointId;
            row.appendChild(unpub);
        }
        else {
            const pub = document.createElement('button');
            pub.type = 'button';
            pub.className = 'sm primary ep-act';
            pub.innerHTML = icon('radio') + '<span>通告</span>';
            pub.dataset.act = 'publish-device';
            pub.dataset.device = ep.endpointId;
            row.appendChild(pub);
        }
        box.appendChild(row);
    }
}
// ---------------------------------------------------------------------------
// 通告（本机设备 → 端点）
// ---------------------------------------------------------------------------
/** 打开通告弹窗（可见性 / delivery 由公开者声明）。 */
function openPublishModal(endpointId) {
    const ep = localCatalog.endpoints.find((x) => x.endpointId === endpointId);
    if (!ep)
        return;
    publishTarget = { ep };
    $('pub-modal-title').textContent = `通告「${ep.name}」`;
    $('pub-modal-sub').textContent =
        '端点 = 订阅入口：对端节点在目录里看到它并订阅，订阅达成后自动开推（不采集则省资源）。';
    document.querySelector('input[name="pub-vis"][value="confirm"]').checked = true;
    document.querySelector('input[name="pub-delivery"][value="pull"]').checked = true;
    $('pub-error').classList.add('hidden');
    $('pub-modal').classList.remove('hidden');
}
/** 确认通告。 */
async function confirmPublish() {
    if (!publishTarget)
        return;
    const vis = document.querySelector('input[name="pub-vis"]:checked').value;
    const delivery = document.querySelector('input[name="pub-delivery"]:checked').value;
    const btn = $btn('pub-confirm-btn');
    setBtnLoading(btn, true);
    $('pub-error').classList.add('hidden');
    try {
        await call('endpoint_publish', {
            deviceId: publishTarget.ep.endpointId,
            visibility: vis,
            delivery,
        });
        $('pub-modal').classList.add('hidden');
        await refreshLocalCatalog();
    }
    catch (e) {
        $('pub-error').textContent = '通告失败：' + e.message;
        $('pub-error').classList.remove('hidden');
    }
    finally {
        setBtnLoading(btn, false);
    }
}
/** 取消通告（已订阅会话由上层决定宽限期，P1 直接移除）。 */
async function unpublishEndpoint(endpointId) {
    try {
        await call('endpoint_unpublish', { endpointId });
        await refreshLocalCatalog();
    }
    catch (e) {
        showGridError('取消通告失败：' + e.message);
    }
}
// ---------------------------------------------------------------------------
// 对端：目录拉取 + 订阅
// ---------------------------------------------------------------------------
/** 对端目录缓存 TTL：目录是通告快照，短 TTL 让对端新通告/取消通告及时可见。 */
const REMOTE_DIR_TTL_MS = 20000;
/** 拉取对端节点目录（endpoint_ls；端口缺省 = 库层默认协商端口）。 */
async function loadRemoteDir(dev) {
    const host = deviceHostOf(dev);
    if (!host)
        return;
    const cached = remoteDirs.get(dev.key);
    const cachedAt = remoteDirAt.get(dev.key);
    if (cached && cachedAt && Date.now() - cachedAt < REMOTE_DIR_TTL_MS) {
        renderRemoteDir(dev, cached);
        return;
    }
    if (remoteDirLoading.has(dev.key))
        return;
    remoteDirLoading.add(dev.key);
    const box = document.querySelector(`[data-role="remote-dir"][data-key="${dev.key}"] .dir-status`);
    if (box)
        box.textContent = '目录加载中…';
    try {
        const dir = (await call('endpoint_ls', { host }));
        remoteDirs.set(dev.key, dir);
        remoteDirAt.set(dev.key, Date.now());
        renderRemoteDir(dev, dir);
    }
    catch (e) {
        if (box) {
            box.textContent = '目录不可用（' + e.message + '）';
            box.classList.add('hint');
        }
    }
    finally {
        remoteDirLoading.delete(dev.key);
    }
}
/** 对端节点目录渲染（设备 + 可订阅端点；写入卡片 [data-role="remote-dir"]）。 */
function renderRemoteDir(dev, dir) {
    const container = document.querySelector(`[data-role="remote-dir"][data-key="${dev.key}"]`);
    if (!container)
        return;
    container.innerHTML = '';
    const title = document.createElement('h3');
    title.textContent = '目录（设备 → 可订阅端点）';
    container.appendChild(title);
    const status = document.createElement('div');
    status.className = 'dir-status hint';
    status.textContent = '展开即拉取（endpoint_ls，协商端口缺省）';
    container.appendChild(status);
    if (!dir.endpoints.length) {
        container.appendChild(emptyState('server', '该节点暂未通告任何端点'));
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
            labelOf(VISIBILITY_LABELS, ep.visibility) + ' · ' +
                labelOf(DELIVERY_LABELS, ep.delivery) +
                (ep.subscribers ? ` · ${ep.subscribers} 订阅中` : '');
        body.appendChild(name);
        body.appendChild(meta);
        row.appendChild(ic);
        row.appendChild(body);
        if (!ep.available) {
            // 不可挂载端点：可见原因，不可订阅（屏幕获取失败等 load 探测结果）
            const hint = document.createElement('span');
            hint.className = 'hint';
            hint.textContent = '不可订阅（' + (ep.lastError || '未知原因') + '）';
            row.appendChild(hint);
        }
        else if (ep.kind === 'file') {
            const hint = document.createElement('span');
            hint.className = 'hint';
            hint.textContent = '文件端点（CLI 订阅）';
            row.appendChild(hint);
        }
        else {
            const sub = document.createElement('button');
            sub.type = 'button';
            sub.className = 'sm primary ep-act';
            sub.innerHTML = icon('download') + '<span>订阅</span>';
            sub.dataset.act = 'subscribe-endpoint';
            sub.dataset.host = deviceHostOf(dev) || '';
            sub.dataset.endpoint = ep.endpointId;
            row.appendChild(sub);
        }
        container.appendChild(row);
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
/** 打开订阅弹窗（端点声明 Both 时可选手方向）。 */
function openSubscribeModal(host, endpointId) {
    const dev = deviceViews.find((d) => d.key && deviceHostOf(d) === host);
    const dir = dev ? remoteDirs.get(dev.key) : null;
    const ep = dir?.endpoints.find((e) => e.endpointId === endpointId);
    if (!ep)
        return;
    subscribeTarget = { host, ep };
    $('sub-modal-title').textContent = `订阅「${ep.name}」`;
    $('sub-modal-sub').textContent =
        `可见性=${labelOf(VISIBILITY_LABELS, ep.visibility)} · ` +
            `方向=${labelOf(DELIVERY_LABELS, ep.delivery)} · 传输=` +
            (ep.transports.map((t) => t.transport).join('/') || '按默认');
    const sel = $('sub-delivery');
    sel.innerHTML = '';
    const opts = ep.delivery === 'both'
        ? [
            { value: 'pull', label: '拉取（连公开方中继，观看更省本机资源）' },
            { value: 'push', label: '推送（公开方凭凭证推入本机中继）' },
        ]
        : [{ value: ep.delivery, label: labelOf(DELIVERY_LABELS, ep.delivery) }];
    fillSelect(sel, opts, '');
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
        const r = (await call('endpoint_subscribe_media', {
            host: subscribeTarget.host,
            endpointId: subscribeTarget.ep.endpointId,
            delivery: subscribeTarget.ep.delivery === 'both'
                ? $('sub-delivery').value
                : undefined,
        }));
        $('sub-modal').classList.add('hidden');
        // 订阅达成：把接收目标指向握手返回的入口，走既有接收链路
        targetRelay = { wsBase: r.relayUrl, srtUrl: null, quicUrl: null };
        await startReceive(r.streamId);
    }
    catch (e) {
        $('sub-error').textContent = '订阅失败：' + e.message;
        $('sub-error').classList.remove('hidden');
    }
    finally {
        setBtnLoading(btn, false);
    }
}
