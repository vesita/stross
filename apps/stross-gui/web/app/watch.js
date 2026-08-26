"use strict";
// Stross 前端 —— 接收域 + 共享流面板（script 全局作用域）：
// 从本机/设备锚点接收共享流（直连，失败自动级联）→ canvas 绘制 / 扬声器播放；
// 右栏「共享流」面板把全部活动共享（出站广播/定向 + 入站接收）统一呈现与停止。
/** 当前接收目标中继（点选的局域网设备锚点优先，否则本机锚点；均无则 null）。 */
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
/** 按流媒体类型自动选传输：含视频 → SRT（Adaptive）> QUIC > WS；纯音频 → QUIC > WS。 */
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
/** 开始接收流 `streamId`（调用方已设置目标中继/本机锚点）。
 *  音频固定设备播放（B3）；视频帧 → canvas 绘制，纯音频 → 扬声器。 */
async function startReceive(streamId) {
    hideRecvError();
    if (!anchor && !targetRelay) {
        showRecvError('本机锚点未就绪且未选择设备共享。请从「设备」列表选择一条共享。');
        return;
    }
    if (!streamId) {
        showRecvError('缺少流 id');
        return;
    }
    try {
        const stream = remoteStreams.get(streamId) || null; // 流类型（视频/音频）供传输自动选择
        const relay = autoRelayUrl(stream);
        if (!relay) {
            showRecvError('无可用接收目标（本机锚点未就绪）');
            return;
        }
        await call('start_receive', { relay, stream: streamId, audio: 'device' });
        receiving = true;
        recvFrameCount = 0;
        recvAudioBlocks = 0;
        recvError = null;
        recvStreamId = streamId;
        setReceiving(true);
        // 订阅解码帧事件 → canvas（载荷为 base64 字符串——桌面/Android 统一格式，
        // Rust 侧编码；前端 atob 原生解码）
        recvUnlisten = await listen('receive-frame', (p) => {
            drawReceiveFrame(p.width, p.height, p.data);
            recvFrameCount += 1;
            updateRecvOverlay();
        });
        void pollReceiveStatus();
    }
    catch (e) {
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
function setReceiving(r) {
    receiving = r;
    if (!r) {
        recvAudioBlocks = 0;
        recvStreamId = null;
    }
    const line = $('recv-status-line');
    line.classList.toggle('hidden', !r);
    $('recv-dot').className = 'dot ' + (r ? 'live' : 'idle');
    $('recv-status').textContent = r ? '接收中' : '未接收';
    $('recv-meta').textContent = '';
    updateRecvOverlay();
    void renderShares();
}
/** 接收等待浮层：接收中且既无视频帧也无音频块（纯音频流 B2：有音频即算有数据）。 */
function updateRecvOverlay() {
    $('recv-overlay').classList.toggle('hidden', !receiving || recvFrameCount > 0 || recvAudioBlocks > 0);
    // 画布仅在收到视频帧时显示（纯音频流不占画面区）
    $('recv-canvas-wrap').classList.toggle('hidden', recvFrameCount === 0);
}
/** 轮询接收统计（帧数 / 解码 / 音频块）并同步共享面板。 */
async function pollReceiveStatus() {
    if (!receiving)
        return;
    try {
        const s = (await call('receive_status'));
        recvAudioBlocks = s.audioBlocks;
        if (s.error)
            recvError = s.error;
        const status = $('recv-status');
        if (recvFrameCount > 0) {
            status.textContent = '接收中';
            $('recv-dot').className = 'dot live';
        }
        else if (s.audioBlocks > 0) {
            // 纯音频流（B2）：无视频帧，音频块持续增长即视为已接通
            status.textContent = '音频播放中';
            $('recv-dot').className = 'dot live';
            updateRecvOverlay();
        }
        else if (!s.running && !s.error) {
            status.textContent = '等待流数据…';
            $('recv-dot').className = 'dot starting';
        }
        $('recv-meta').textContent = s.error
            ? '错误：' + s.error
            : `收到 ${s.received} 帧 · 解码 ${s.decodedVideo} 帧 · 音频 ${s.audioBlocks} 块`
                + (recvFrameCount ? ` · 已绘制 ${recvFrameCount} 帧` : '');
        void renderShares();
    }
    catch (_) { /* ignore */ }
    if (receiving)
        setTimeout(() => void pollReceiveStatus(), 1000);
}
// ---------------------------------------------------------------------------
// 共享流面板（右栏）：全部活动共享统一管理
// ---------------------------------------------------------------------------
/** 汇总当前活动共享条目（出站广播/定向 + 入站接收）。 */
function shareItems() {
    const items = [];
    // 出站：定向凭证共享（B2 手机麦克风 → 目标设备）
    if (micShare && micShare.active) {
        const dev = deviceViews.find((d) => d.base === micShare.base);
        items.push({
            id: 'mic-out-' + micShare.base,
            direction: 'out',
            media: 'mic',
            target: dev ? dev.name : micShare.base,
            state: 'live',
            detail: '凭证推流中（QUIC/WS）',
        });
    }
    // 出站：广播共享（屏幕 / 麦克风 → 局域网）
    if (streaming && streamInfo && !(micShare && micShare.active)) {
        const media = shareKind === 'mic' ? 'mic' : 'screen';
        items.push({
            id: 'out-' + streamInfo.streamId,
            direction: 'out',
            media,
            target: '局域网广播',
            state: starting ? 'starting' : 'live',
            detail: `已推 ${fmtElapsed(Math.floor(Date.now() / 1000) - streamInfo.startedAt)}`,
        });
    }
    // 入站：接收中的共享（观看 / 反向麦克风）
    if (receiving) {
        const stream = recvStreamId ? remoteStreams.get(recvStreamId) : undefined;
        const media = stream && stream.video ? 'screen' : 'mic';
        const source = stream ? stream.title || stream.streamId : recvStreamId || '';
        items.push({
            id: 'in-' + (recvStreamId || 'recv'),
            direction: 'in',
            media,
            target: source + ' 的共享',
            state: recvError ? 'error' : recvAudioBlocks > 0 || recvFrameCount > 0 ? 'live' : 'starting',
            detail: recvError || (recvAudioBlocks > 0
                ? `音频 ${recvAudioBlocks} 块 · 播放中`
                : recvFrameCount > 0
                    ? `已绘制 ${recvFrameCount} 帧`
                    : '等待数据…'),
        });
    }
    return items;
}
/** 当前接收中的流 id（供共享面板定位流信息）。 */
let recvStreamId = null;
const MEDIA_LABELS = {
    screen: '屏幕',
    camera: '摄像头',
    mic: '麦克风',
    systemAudio: '系统声',
};
/** 渲染右栏共享面板。 */
function renderShares() {
    const box = $('share-list');
    const items = shareItems();
    box.innerHTML = '';
    if (!items.length) {
        box.appendChild(emptyState('activity', '暂无活动共享。点左侧设备卡片发起共享，或点设备的在线共享条目接收。'));
        return;
    }
    for (const it of items)
        box.appendChild(shareItemEl(it));
}
/** 单条共享条目：方向箭头 + 媒体图标 + 对端 + 状态 + 停止。 */
function shareItemEl(it) {
    const el = document.createElement('div');
    el.className = 'share-item ' + it.direction + ' ' + it.state;
    const arrow = document.createElement('span');
    arrow.className = 'share-arrow';
    arrow.textContent = it.direction === 'out' ? '↑' : '↓';
    arrow.title = it.direction === 'out' ? '本机共享出去' : '本机接收进来';
    el.appendChild(arrow);
    const ic = document.createElement('span');
    ic.className = 'share-ic';
    ic.innerHTML = icon(it.media === 'mic' ? 'mic' : it.media === 'screen' ? 'monitor' : 'music');
    el.appendChild(ic);
    const body = document.createElement('span');
    body.className = 'card-body';
    const name = document.createElement('span');
    name.className = 'scan-name';
    name.textContent = (it.direction === 'out' ? '共享 ' : '接收 ') + MEDIA_LABELS[it.media] + ' ' + (it.direction === 'out' ? '→ ' : '← ') + it.target;
    const meta = document.createElement('span');
    meta.className = 'scan-meta';
    const dotClass = it.state === 'error' ? 'err' : it.state === 'live' ? 'ok' : 'starting';
    meta.innerHTML = `<span class="dot ${dotClass}"></span>${it.state === 'error' ? '错误' : it.state === 'live' ? '进行中' : '启动中'} · ${it.detail}`;
    body.appendChild(name);
    body.appendChild(meta);
    el.appendChild(body);
    const stop = document.createElement('button');
    stop.type = 'button';
    stop.className = 'share-stop';
    stop.innerHTML = icon('stop') + '<span>停止</span>';
    stop.onclick = () => {
        if (it.direction === 'out')
            void stopStream();
        else
            void stopReceive();
    };
    el.appendChild(stop);
    return el;
}
