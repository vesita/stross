'use strict';

// Stross 推流端控制界面（Tauri 前端，零构建步骤）
// 交互模型：先连接中继（本机或局域网），再选择「推流（发）」或「观看（收）」。

const $ = (id) => document.getElementById(id);
const invoke = window.__TAURI__?.core?.invoke;

// 与 Rust 端 Quality 预设保持一致
const QUALITIES = {
  LOW: { width: 640, height: 360, fps: 24, bitrateKbps: 800 },
  MEDIUM: { width: 1280, height: 720, fps: 30, bitrateKbps: 2500 },
  HIGH: { width: 1920, height: 1080, fps: 30, bitrateKbps: 6000 },
};

let devices = { cameras: [], audioInputs: [], systemAudio: [] };
let running = false;
let connection = null; // { url: "http://host:port", wsUrl: "ws://host:port/ws/push" }
let currentTab = 'send';

// ---------------------------------------------------------------- 初始化

async function init() {
  if (!invoke) {
    showFatal('当前页面未运行在 Stross 桌面应用中。\n请通过 `cargo tauri dev` 或安装包启动。');
    return;
  }
  try {
    const info = await invoke('app_info');
    $('ver-badge').textContent = 'v' + info.version;
    const fb = $('ffmpeg-badge');
    if (info.ffmpeg) {
      fb.textContent = 'ffmpeg ✓';
      fb.classList.add('ok');
    } else {
      fb.textContent = '未检测到 ffmpeg';
      fb.classList.add('err');
    }
    renderIps(info.ips);
    await loadDevices();
  } catch (e) {
    showFatal(String(e));
  }
}

function showFatal(msg) {
  const box = $('error-box');
  box.textContent = msg;
  box.classList.remove('hidden');
}
function hideError() {
  $('error-box').classList.add('hidden');
}
function showConnectError(msg) {
  const box = $('connect-error');
  box.textContent = msg;
  box.classList.remove('hidden');
}

// ---------------------------------------------------------------- 连接

function normAddr(addr) {
  let a = addr.trim();
  if (!a) return null;
  if (!/^https?:\/\//i.test(a)) a = 'http://' + a;
  return a.replace(/\/+$/, '');
}

async function connect() {
  hideConnectError();
  const mode = document.querySelector('input[name="conn"]:checked').value;
  try {
    if (mode === 'local') {
      const info = await invoke('start_relay');
      connection = {
        url: `http://127.0.0.1:${info.port}`,
        wsUrl: `ws://127.0.0.1:${info.port}/ws/push`,
      };
    } else {
      const addr = normAddr($('relay-addr').value);
      if (!addr) {
        showConnectError('请输入中继地址，例如 http://192.168.1.100:8777');
        return;
      }
      // 探测中继是否可达
      const resp = await fetch(addr + '/api/streams', { cache: 'no-store' });
      if (!resp.ok) throw new Error('HTTP ' + resp.status);
      await resp.json();
      connection = { url: addr, wsUrl: addr.replace(/^http/, 'ws') + '/ws/push' };
    }
    enterApp();
  } catch (e) {
    showConnectError('连接失败：' + e.message);
  }
}

function enterApp() {
  $('connect-view').classList.add('hidden');
  $('app-view').classList.remove('hidden');
  $('conn-badge').textContent = '已连接';
  $('conn-badge').classList.add('ok');
  $('tab-conn-label').textContent = '已连接：' + connection.url;
  $('watch-relay-url').textContent = connection.url;
  setTab('send');
  // 观看页：iframe 直接加载中继托管的观看端页面（复用同一播放器）
  $('watch-frame').src = connection.url + '/';
  pollStatus();
}

function disconnect() {
  if (running) {
    stopStream();
  }
  connection = null;
  $('app-view').classList.add('hidden');
  $('connect-view').classList.remove('hidden');
  $('conn-badge').textContent = '未连接';
  $('conn-badge').classList.remove('ok');
  $('watch-frame').src = 'about:blank';
}

// ---------------------------------------------------------------- 模式切换

function setTab(tab) {
  currentTab = tab;
  $('tab-send-btn').classList.toggle('active', tab === 'send');
  $('tab-watch-btn').classList.toggle('active', tab === 'watch');
  $('tab-send').classList.toggle('hidden', tab !== 'send');
  $('tab-watch').classList.toggle('hidden', tab !== 'watch');
}

// ---------------------------------------------------------------- 设备

async function loadDevices() {
  devices = await invoke('list_devices');
  fillSelect($('camera-select'), devices.cameras.map((c) => ({ value: c.id, label: c.name })), '使用默认摄像头');
  fillSelect($('mic-select'), devices.audioInputs.map((n) => ({ value: n, label: n })), '系统默认输入');
  fillSelect($('sys-select'), devices.systemAudio.map((n) => ({ value: n, label: n })), '未发现回环设备');
  $('mic-hint').textContent = devices.audioInputs.length ? '' : '未发现麦克风（仍会使用系统默认输入）';
  $('sys-hint').textContent = devices.systemAudio.length
    ? ''
    : '未发现回环设备（Linux 需 PulseAudio monitor；Windows 需启用“立体声混音”）';
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
    li.title = '点击填入中继地址';
    li.onclick = () => {
      document.querySelector('input[name="conn"][value="remote"]').checked = true;
      $('remote-row').classList.remove('hidden');
      $('relay-addr').value = `http://${ip}:8777`;
    };
    ul.appendChild(li);
  });
  if (!ips.length) ul.innerHTML = '<li class="hint">未获取到局域网 IP</li>';
}

// ---------------------------------------------------------------- 扫描局域网

async function scanRelays() {
  const box = $('scan-results');
  box.classList.remove('hidden');
  box.innerHTML = '<p class="hint">扫描中（2 秒）…</p>';
  try {
    const relays = await invoke('scan_relays');
    if (!relays.length) {
      box.innerHTML = '<p class="hint">未发现局域网内其它中继（mDNS）。可手动输入地址。</p>';
      return;
    }
    box.innerHTML = '';
    relays.forEach((r) => {
      const url = r.urls[0];
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.textContent = '📍 ' + url;
      btn.onclick = () => {
        $('relay-addr').value = url;
      };
      box.appendChild(btn);
    });
  } catch (e) {
    box.innerHTML = `<p class="hint err-text">扫描失败：${e.message}</p>`;
  }
}

// ---------------------------------------------------------------- 推流配置

function currentVideoSource() {
  const kind = document.querySelector('input[name="video"]:checked').value;
  if (kind === 'screen') return { kind: 'Screen' };
  if (kind === 'camera') return { kind: 'Camera', device: $('camera-select').value || null };
  return { kind: 'Synthetic', pattern: 'testsrc2' };
}

function buildConfig() {
  const q = QUALITIES[$('quality-select').value];
  const micOn = $('mic-enable').checked;
  const sysOn = $('sys-enable').checked;
  const audio = micOn || sysOn
    ? {
        mic: micOn ? $('mic-select').value || null : null,
        systemAudio: sysOn ? $('sys-select').value || null : null,
        sampleRate: 48000,
        channels: 2,
        bitrateKbps: 128,
      }
    : null;
  return {
    streamId: 'stross-' + Date.now().toString(36),
    title: $('title-input').value.trim() || '我的串流',
    video: currentVideoSource(),
    quality: q,
    audio,
    durationSecs: null,
  };
}

// ---------------------------------------------------------------- 推流控制

async function startStream() {
  hideError();
  if (!connection) {
    showFatal('请先连接中继');
    return;
  }
  $('start-btn').disabled = true;
  try {
    // 推到当前连接的中继
    const res = await invoke('start_stream', { cfg: buildConfig(), relayUrl: connection.wsUrl });
    renderUrls(res.watchUrls);
    setRunning(true);
  } catch (e) {
    showFatal(String(e));
    $('start-btn').disabled = false;
  }
}

async function stopStream() {
  try {
    await invoke('stop_stream');
  } catch (e) {
    showFatal(String(e));
  }
  setRunning(false);
}

async function pollStatus() {
  try {
    const s = await invoke('stream_status');
    setRunning(s.running);
    $('stream-meta').textContent = s.running
      ? `「${s.title}」(${s.streamId}) · 中继端口 ${s.relayPort} · 开始于 ${new Date(s.startedAt * 1000).toLocaleTimeString()}`
      : '';
  } catch (_) {
    /* ignore */
  }
}

function setRunning(r) {
  running = r;
  $('start-btn').disabled = r;
  $('stop-btn').disabled = !r;
  $('viewer-btn').disabled = !r;
  $('status-dot').className = 'dot ' + (r ? 'live' : 'idle');
  $('status-text').textContent = r ? '推流中' : '未推流';
}

function renderUrls(urls) {
  const ul = $('url-list');
  ul.innerHTML = '';
  urls.forEach((u) => {
    const li = document.createElement('li');
    const tag = document.createElement('span');
    tag.className = 'tag';
    tag.textContent = '▶';
    li.appendChild(tag);
    li.appendChild(document.createTextNode(u));
    li.title = '点击复制';
    li.onclick = () => {
      navigator.clipboard?.writeText(u);
      li.style.borderColor = 'var(--ok)';
      setTimeout(() => (li.style.borderColor = ''), 800);
    };
    ul.appendChild(li);
  });
}

// ---------------------------------------------------------------- 事件

document.querySelectorAll('input[name="conn"]').forEach((r) =>
  r.addEventListener('change', () => {
    $('remote-row').classList.toggle('hidden', r.value !== 'remote');
  })
);
document.querySelectorAll('input[name="video"]').forEach((r) =>
  r.addEventListener('change', () => {
    $('camera-row').classList.toggle('hidden', r.value !== 'camera');
  })
);

$('connect-btn').onclick = connect;
$('scan-btn').onclick = scanRelays;
$('tab-send-btn').onclick = () => setTab('send');
$('tab-watch-btn').onclick = () => setTab('watch');
$('start-btn').onclick = startStream;
$('stop-btn').onclick = stopStream;
$('viewer-btn').onclick = () => invoke('open_viewer').catch((e) => showFatal(String(e)));

init();
