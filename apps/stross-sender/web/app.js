'use strict';

// Stross 推流端控制界面（Tauri 前端，零构建步骤）

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
    ul.appendChild(li);
  });
  if (!ips.length) ul.innerHTML = '<li class="hint">未获取到局域网 IP</li>';
}

// ---------------------------------------------------------------- 配置

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
  $('start-btn').disabled = true;
  try {
    const res = await invoke('start_stream', { cfg: buildConfig() });
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

document.querySelectorAll('input[name="video"]').forEach((r) =>
  r.addEventListener('change', () => {
    $('camera-row').classList.toggle('hidden', r.value !== 'camera');
  })
);

$('start-btn').onclick = startStream;
$('stop-btn').onclick = stopStream;
$('viewer-btn').onclick = () => invoke('open_viewer').catch((e) => showFatal(String(e)));

init();
setInterval(pollStatus, 2000);
