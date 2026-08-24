"use strict";
// Stross 观看端（TypeScript 源文件，唯一真源）。
//
// 生成内嵌用 app.js：`npx tsc -p crates/stross-core/assets/viewer/tsconfig.json`
// （app.js 是构建产物，提交进仓库——Rust 侧 include_str! 内嵌，cargo 构建不依赖 node）。
// 修改本文件后必须重新生成 app.js 并提交两者。
//
// 逻辑：拉取流列表 → 连接（WebRTC 优先，WebSocket 兜底）→ jmuxer 封包 → MSE 播放。
// 借鉴 MediaMTX 的 Web 播放器交互与 jmuxer 的用法。
/** 角色英文 → 中文显示（TXT `roles` 取值）。 */
const ROLE_LABELS = {
    sender: '推流',
    viewer: '观看',
    relay: '中继',
};
const $ = (id) => document.getElementById(id);
const listEl = $('stream-list');
const peerListEl = $('peer-list');
const logEl = $('log');
const connBadge = $('conn-state');
const placeholder = $('placeholder');
// 协议头常量（与 stross-proto v2 帧头布局一致，见 docs/protocol.md）
const PROTO_HEADER_LEN = 24;
const TRACK_VIDEO = 0;
let video = $('player');
let state = {
    ws: null,
    pc: null,
    fallbackTimer: undefined,
    jmuxer: null,
    stream: null,
    lastStream: null,
    videoFrames: 0,
    bytes: 0,
    rateStart: performance.now(),
    rateBytes: 0,
    frameStart: performance.now(),
    frameCount: 0,
    closing: false,
};
// ---------------------------------------------------------------- 工具
function esc(s) {
    return String(s).replace(/[&<>"']/g, (c) => ({
        '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
    }[c]));
}
function log(msg, cls = '') {
    const line = document.createElement('div');
    line.className = cls;
    const t = new Date().toLocaleTimeString();
    line.textContent = `[${t}] ${msg}`;
    logEl.appendChild(line);
    while (logEl.childNodes.length > 200) {
        const first = logEl.firstChild;
        if (first)
            logEl.removeChild(first);
    }
    logEl.scrollTop = logEl.scrollHeight;
}
function setConn(cls, text) {
    connBadge.className = 'badge' + (cls ? ' ' + cls : '');
    connBadge.textContent = text;
}
function fmtMeta(s) {
    const parts = [];
    if (s.video)
        parts.push(`视频 ${s.video.width}×${s.video.height}@${s.video.fps} ${s.video.codec}`);
    if (s.audio)
        parts.push(`音频 ${s.audio.sampleRate}Hz ${s.audio.channels}ch ${s.audio.codec}`);
    if (s.watchers !== undefined)
        parts.push(`${s.watchers} 人观看`);
    return parts.join(' · ');
}
// ---------------------------------------------------------------- 流列表
async function refresh() {
    try {
        const resp = await fetch('/api/streams', { cache: 'no-store' });
        if (!resp.ok)
            throw new Error('HTTP ' + resp.status);
        renderList(await resp.json());
    }
    catch (e) {
        log('刷新流列表失败: ' + e.message, 'err');
    }
}
function renderList(streams) {
    listEl.innerHTML = '';
    if (!streams.length) {
        listEl.innerHTML = '<p class="hint">暂无在线串流<br>请先在推流端开始推流</p>';
        return;
    }
    for (const s of streams) {
        const card = document.createElement('div');
        card.className = 'stream-card' + (state.stream && state.stream.streamId === s.streamId ? ' active' : '');
        card.innerHTML = `
      <div class="title">${esc(s.title)} <span class="live">●</span></div>
      <div class="meta">${esc(fmtMeta(s))}</div>`;
        card.onclick = () => connect(s);
        listEl.appendChild(card);
    }
}
// ---------------------------------------------------------------- 局域网设备
/** 拉取局域网设备列表（`/api/peers`，中继周期 mDNS 发现）。 */
async function refreshPeers() {
    try {
        const resp = await fetch('/api/peers', { cache: 'no-store' });
        if (!resp.ok)
            throw new Error('HTTP ' + resp.status);
        renderPeers(await resp.json());
    }
    catch (e) {
        log('刷新设备列表失败: ' + e.message, 'err');
    }
}
function roleLabel(r) {
    return ROLE_LABELS[r] || r;
}
/** 渲染局域网设备卡片：点击打开该设备的观看页。 */
function renderPeers(peers) {
    peerListEl.innerHTML = '';
    if (!peers.length) {
        peerListEl.innerHTML = '<p class="hint">未发现其它设备<br>（本机中继不在此列）</p>';
        return;
    }
    for (const p of peers) {
        const card = document.createElement('div');
        card.className = 'peer-card';
        card.title = '打开该设备的观看页';
        const name = document.createElement('div');
        name.className = 'title';
        name.textContent = p.name;
        const meta = document.createElement('div');
        meta.className = 'meta';
        meta.textContent = p.ip + ':' + p.port;
        if (p.roles && p.roles.length) {
            const tags = document.createElement('div');
            tags.className = 'meta';
            p.roles.forEach((r) => {
                const tag = document.createElement('span');
                tag.className = 'role-tag';
                tag.textContent = roleLabel(r);
                tags.appendChild(tag);
            });
            card.appendChild(tags);
        }
        card.appendChild(name);
        card.appendChild(meta);
        card.onclick = () => { location.href = p.url; };
        peerListEl.appendChild(card);
    }
}
// ---------------------------------------------------------------- 播放
function teardown() {
    state.closing = true;
    clearTimeout(state.fallbackTimer);
    if (state.ws) {
        try {
            state.ws.close();
        }
        catch (_) { /* ignore */ }
        state.ws = null;
    }
    if (state.pc) {
        try {
            state.pc.close();
        }
        catch (_) { /* ignore */ }
        state.pc = null;
    }
    state.jmuxer = null;
    // 换一个新的 video 元素，彻底重置 jmuxer 持有的 MediaSource
    const fresh = document.createElement('video');
    fresh.id = 'player';
    fresh.autoplay = true;
    fresh.controls = true;
    fresh.muted = true;
    fresh.playsInline = true;
    video.replaceWith(fresh);
    video = fresh;
}
/** 共享帧处理：24 字节 v2 协议头 + H.264/AAC 载荷（WS 与 WebRTC 同构）。 */
function handleFrame(data) {
    if (data.byteLength < PROTO_HEADER_LEN)
        return;
    const header = new Uint8Array(data, 0, PROTO_HEADER_LEN);
    const track = header[5];
    const payload = new Uint8Array(data, PROTO_HEADER_LEN);
    state.bytes += payload.length;
    if (track === TRACK_VIDEO) {
        state.frameCount++;
        state.jmuxer.feed({ video: payload });
    }
    else if (track === 1) {
        state.jmuxer.feed({ audio: payload });
    }
}
/** 连接串流：优先 WebRTC（低延迟，UDP），失败/超时自动回退 WebSocket。 */
function connect(s) {
    if (state.stream && state.stream.streamId === s.streamId) {
        if ((state.ws && state.ws.readyState === WebSocket.OPEN) ||
            (state.pc && state.pc.connectionState === 'connected')) {
            return; // 已在播放
        }
    }
    teardown();
    state.closing = false;
    state.stream = s;
    state.lastStream = s;
    state.videoFrames = 0;
    state.bytes = 0;
    state.rateStart = performance.now();
    state.rateBytes = 0;
    state.frameStart = performance.now();
    state.frameCount = 0;
    placeholder.style.display = 'none';
    state.jmuxer = new JMuxer({
        node: video,
        mode: 'video',
        flushingTime: 0,
        fps: (s.video && s.video.fps) || 30,
        clearBuffer: true,
        debug: false,
        onError: (e) => log('解码错误: ' + e, 'err'),
    });
    if (typeof RTCPeerConnection !== 'undefined') {
        void connectWebRTC(s);
    }
    else {
        connectWS(s);
    }
}
/** WebRTC 观看路径：信令经 HTTP（/api/webrtc/start + /answer），媒体走 media datachannel。 */
async function connectWebRTC(s) {
    try {
        const pc = new RTCPeerConnection({ iceServers: [] }); // 局域网直连，无需 STUN
        state.pc = pc;
        pc.ondatachannel = (e) => {
            const ch = e.channel;
            if (ch.label === 'media') {
                ch.binaryType = 'arraybuffer';
                ch.onmessage = (ev) => handleFrame(ev.data);
            }
            else if (ch.label === 'control') {
                ch.onmessage = (ev) => {
                    let msg;
                    try {
                        msg = JSON.parse(ev.data);
                    }
                    catch (_) {
                        return;
                    }
                    if (msg.type === 'ready')
                        setConn('ok', '播放中');
                    else if (msg.type === 'error')
                        log('中继: ' + msg.message, 'err');
                };
            }
        };
        pc.onconnectionstatechange = () => {
            const st = pc.connectionState;
            if (st === 'connected')
                setConn('ok', '播放中');
            else if (st === 'failed' || st === 'closed') {
                setConn('err', '已断开');
                log('WebRTC 连接关闭', 'warn');
                if (!state.closing && state.lastStream) {
                    log('3 秒后重连…', 'warn');
                    setTimeout(() => { if (!state.closing)
                        connect(state.lastStream); }, 3000);
                }
            }
        };
        // 信令：start → offer
        const resp = await fetch('/api/webrtc/start', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ streamId: s.streamId }),
        });
        const sig = (await resp.json());
        if (!sig.sdp)
            throw new Error(sig.error || 'WebRTC 信令失败');
        await pc.setRemoteDescription({ type: 'offer', sdp: sig.sdp });
        const answer = await pc.createAnswer();
        await pc.setLocalDescription(answer);
        const ansResp = await fetch('/api/webrtc/answer', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ peerId: sig.peerId, sdp: pc.localDescription.sdp }),
        });
        if (!ansResp.ok)
            throw new Error('WebRTC answer 提交失败');
        setConn('ok', '连接中…');
        // 10s 内未连通则回退 WebSocket
        state.fallbackTimer = setTimeout(() => {
            if (state.pc && state.pc.connectionState !== 'connected') {
                log('WebRTC 连接超时，回退 WebSocket…', 'warn');
                connectWS(s);
            }
        }, 10000);
    }
    catch (e) {
        log('WebRTC 不可用，回退 WebSocket: ' + e.message, 'warn');
        connectWS(s);
    }
}
/** WebSocket 观看路径（无损兜底；媒体帧与 WebRTC 完全同构）。 */
function connectWS(s) {
    if (state.pc) {
        try {
            state.pc.close();
        }
        catch (_) { /* ignore */ }
        state.pc = null;
    }
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const url = `${proto}://${location.host}/ws/watch?stream=${encodeURIComponent(s.streamId)}`;
    const ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => {
        setConn('ok', '已连接');
        log(`已连接串流「${s.title}」`);
    };
    ws.onmessage = (ev) => {
        if (typeof ev.data === 'string') {
            let msg;
            try {
                msg = JSON.parse(ev.data);
            }
            catch (_) {
                return;
            }
            if (msg.type === 'ready')
                setConn('ok', '播放中');
            else if (msg.type === 'error')
                log('中继: ' + msg.message, 'err');
            return;
        }
        handleFrame(ev.data);
    };
    ws.onerror = () => log('WebSocket 错误', 'err');
    ws.onclose = () => {
        setConn('err', '已断开');
        log('连接关闭', 'warn');
        if (!state.closing && state.lastStream) {
            log('3 秒后重连…', 'warn');
            setTimeout(() => { if (!state.closing)
                connect(state.lastStream); }, 3000);
        }
    };
    state.ws = ws;
}
// ---------------------------------------------------------------- 统计
setInterval(() => {
    const now = performance.now();
    const dt = (now - state.rateStart) / 1000;
    if (dt >= 1) {
        $('st-rate').textContent = Math.round((state.bytes - state.rateBytes) * 8 / dt / 1000) + ' Kbps';
        state.rateStart = now;
        state.rateBytes = state.bytes;
    }
    const fdt = (now - state.frameStart) / 1000;
    if (fdt >= 2) {
        $('st-fps').textContent = Math.round(state.frameCount / fdt) + ' fps';
        state.frameStart = now;
        state.frameCount = 0;
    }
    if (state.stream) {
        $('st-stream').textContent = state.stream.title;
        $('st-video').textContent = state.stream.video
            ? `${state.stream.video.width}×${state.stream.video.height}`
            : '无视频';
        $('st-audio').textContent = state.stream.audio ? '有音频' : '无音频';
        try {
            const b = video.buffered;
            const latency = b.length ? b.end(b.length - 1) - video.currentTime : 0;
            $('st-latency').textContent = latency > 0 ? `缓冲 ${latency.toFixed(1)}s` : '同步中…';
        }
        catch (_) { /* ignore */ }
    }
}, 1000);
$('refresh-btn').onclick = () => void refresh();
void refresh();
setInterval(() => void refresh(), 5000);
void refreshPeers();
setInterval(() => void refreshPeers(), 10000);
