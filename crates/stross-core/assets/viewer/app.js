'use strict';

// Stross 观看端：拉取流列表 → 连接 WebSocket → jmuxer 封包 → MSE 播放。
// 借鉴 MediaMTX 的 Web 播放器交互与 jmuxer 的用法。

const $ = (id) => document.getElementById(id);
const listEl = $('stream-list');
const logEl = $('log');
const connBadge = $('conn-state');
const placeholder = $('placeholder');

let video = $('player');
let state = {
  ws: null,
  jmuxer: null,
  stream: null,      // 当前流的 StreamInfo
  lastStream: null,  // 断线重连目标
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
  while (logEl.childNodes.length > 200) logEl.removeChild(logEl.firstChild);
  logEl.scrollTop = logEl.scrollHeight;
}

function setConn(cls, text) {
  connBadge.className = 'badge' + (cls ? ' ' + cls : '');
  connBadge.textContent = text;
}

function fmtMeta(s) {
  const parts = [];
  if (s.video) parts.push(`视频 ${s.video.width}×${s.video.height}@${s.video.fps} ${s.video.codec}`);
  if (s.audio) parts.push(`音频 ${s.audio.sampleRate}Hz ${s.audio.channels}ch ${s.audio.codec}`);
  if (s.watchers !== undefined) parts.push(`${s.watchers} 人观看`);
  return parts.join(' · ');
}

// ---------------------------------------------------------------- 流列表

async function refresh() {
  try {
    const resp = await fetch('/api/streams', { cache: 'no-store' });
    if (!resp.ok) throw new Error('HTTP ' + resp.status);
    renderList(await resp.json());
  } catch (e) {
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

// ---------------------------------------------------------------- 播放

function teardown() {
  state.closing = true;
  if (state.ws) {
    try { state.ws.close(); } catch (_) { /* ignore */ }
    state.ws = null;
  }
  state.jmuxer = null;
  // 换一个新的 video 元素，彻底重置 jmuxer 持有的 MediaSource
  const wrap = video.parentElement;
  const fresh = document.createElement('video');
  fresh.id = 'player';
  fresh.autoplay = true;
  fresh.controls = true;
  fresh.muted = true;
  fresh.playsInline = true;
  video.replaceWith(fresh);
  video = fresh;
}

function connect(s) {
  if (state.stream && state.stream.streamId === s.streamId && state.ws && state.ws.readyState === WebSocket.OPEN) {
    return; // 已在播放
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
      try { msg = JSON.parse(ev.data); } catch (_) { return; }
      if (msg.type === 'ready') setConn('ok', '播放中');
      else if (msg.type === 'error') log('中继: ' + msg.message, 'err');
      return;
    }
    // 二进制帧：前 16 字节为协议头，跳过
    if (ev.data.byteLength < 16) return;
    const header = new Uint8Array(ev.data, 0, 16);
    const track = header[5];
    const payload = new Uint8Array(ev.data, 16);
    state.bytes += payload.length;
    if (track === 0) {
      state.frameCount++;
      state.jmuxer.feed({ video: payload });
    } else if (track === 1) {
      state.jmuxer.feed({ audio: payload });
    }
  };

  ws.onerror = () => log('WebSocket 错误', 'err');

  ws.onclose = () => {
    setConn('err', '已断开');
    log('连接关闭', 'warn');
    if (!state.closing && state.lastStream) {
      log('3 秒后重连…', 'warn');
      setTimeout(() => { if (!state.closing) connect(state.lastStream); }, 3000);
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
    $('st-video').textContent = state.stream.video ? `${state.stream.video.width}×${state.stream.video.height}` : '无视频';
    $('st-audio').textContent = state.stream.audio ? '有音频' : '无音频';
    try {
      const b = video.buffered;
      const latency = b.length ? b.end(b.length - 1) - video.currentTime : 0;
      $('st-latency').textContent = latency > 0 ? `缓冲 ${latency.toFixed(1)}s` : '同步中…';
    } catch (_) { /* ignore */ }
  }
}, 1000);

$('refresh-btn').onclick = refresh;
refresh();
setInterval(refresh, 5000);
