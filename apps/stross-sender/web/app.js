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

const LS_RELAY = 'stross.lastRelay';
const LS_TITLE = 'stross.lastTitle';
const LS_RECENT = 'stross.recentRelays';

let devices = { cameras: [], audioInputs: [], systemAudio: [] };
let running = false;
let starting = false; // Android 采集启动中（等待真实状态回报）
let startingSince = 0; // 启动开始时间戳（超时兜底用）
const START_TIMEOUT_MS = 60000; // 采集启动超时
let connection = null; // { url, wsUrl, relayUrls }
let currentTab = 'send';
let IS_ANDROID = false;
let MY_IPS = [];

// ---------------------------------------------------------------- 初始化

async function init() {
  if (!invoke) {
    showFatal('当前页面未运行在 Stross 桌面应用中。\n请通过 `cargo tauri dev` 或安装包启动。');
    return;
  }
  try {
    const info = await invoke('app_info');
    IS_ANDROID = info.platform === 'android';
    MY_IPS = info.ips || [];
    $('ver-badge').textContent = 'v' + info.version;
    const fb = $('ffmpeg-badge');
    if (IS_ANDROID) {
      fb.textContent = '原生采集';
      fb.classList.add('ok');
      // Android：视频源固定为屏幕（MediaProjection），无系统声音采集；
      // 「打开观看端」依赖系统浏览器，Android 上隐藏（用「观看」页内嵌播放器）
      $('video-seg-row').classList.add('hidden');
      $('android-video-note').classList.remove('hidden');
      $('sys-row').classList.add('hidden');
      $('viewer-btn').classList.add('hidden');
      $('mic-hint').textContent = '需要麦克风权限；拒绝则仅推流屏幕';
    } else if (info.ffmpeg) {
      fb.textContent = 'ffmpeg ✓';
      fb.classList.add('ok');
    } else {
      fb.textContent = '未检测到 ffmpeg';
      fb.classList.add('err');
    }
    renderIps(info.ips);
    restorePrefs();
    await loadDevices();
  } catch (e) {
    showFatal(String(e));
  }
}

/** 恢复上次的连接地址/流名称偏好，并渲染最近连接历史。 */
function restorePrefs() {
  const last = localStorage.getItem(LS_RELAY);
  if (last) {
    $('relay-addr').value = last;
    document.querySelector('input[name="conn"][value="remote"]').checked = true;
    $('remote-row').classList.remove('hidden');
  }
  const title = localStorage.getItem(LS_TITLE);
  if (title) $('title-input').value = title;
  renderRecent();
}

function savePrefs() {
  localStorage.setItem(LS_RELAY, $('relay-addr').value.trim());
  localStorage.setItem(LS_TITLE, $('title-input').value.trim());
}

// ---------------- 最近连接历史 ----------------

function getRecent() {
  try {
    return JSON.parse(localStorage.getItem(LS_RECENT) || '[]');
  } catch {
    return [];
  }
}

function saveRecent(url) {
  const list = getRecent().filter((u) => u !== url);
  list.unshift(url);
  localStorage.setItem(LS_RECENT, JSON.stringify(list.slice(0, 5)));
}

/** 渲染"最近连接"列表，点击即填入并自动连接。 */
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
    li.textContent = u;
    li.title = '点击连接';
    li.onclick = () => {
      $('relay-addr').value = u;
      document.querySelector('input[name="conn"][value="remote"]').checked = true;
      $('remote-row').classList.remove('hidden');
      connect();
    };
    ul.appendChild(li);
  });
}

// ---------------------------------------------------------------- 提示

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
function hideConnectError() {
  $('connect-error').classList.add('hidden');
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
  const btn = $('connect-btn');
  btn.disabled = true;
  btn.textContent = '连接中…';
  try {
    if (mode === 'local') {
      const info = await invoke('start_relay');
      connection = {
        url: `http://127.0.0.1:${info.port}`,
        wsUrl: `ws://127.0.0.1:${info.port}/ws/push`,
        relayUrls: info.urls,
      };
    } else {
      const addr = normAddr($('relay-addr').value);
      if (!addr) {
        showConnectError('请输入中继地址，例如 http://192.168.1.100:8777');
        return;
      }
      savePrefs();
      saveRecent(addr);
      // 探测中继是否可达
      const resp = await fetch(addr + '/api/streams', { cache: 'no-store' });
      if (!resp.ok) throw new Error('中继返回 HTTP ' + resp.status);
      await resp.json();
      connection = { url: addr, wsUrl: addr.replace(/^http/, 'ws') + '/ws/push', relayUrls: [addr + '/'] };
    }
    enterApp();
  } catch (e) {
    const hint = e.message.includes('Failed to fetch') || e.message.includes('NetworkError')
      ? '无法访问该地址。请检查：地址是否正确、设备是否在同一局域网、中继是否启动、防火墙是否放行。'
      : '连接失败：' + e.message;
    showConnectError(hint);
  } finally {
    btn.disabled = false;
    btn.textContent = '连接';
  }
}

function enterApp() {
  $('connect-view').classList.add('hidden');
  $('app-view').classList.remove('hidden');
  $('conn-badge').textContent = '已连接';
  $('conn-badge').classList.add('ok');
  $('disconnect-btn').classList.remove('hidden');
  $('tab-conn-label').textContent = '已连接：' + connection.url;
  $('watch-relay-url').textContent = connection.url;
  // 连接成功即可展示中继观看地址（推流前页面显示"暂无串流"）
  if (connection.relayUrls && connection.relayUrls.length) {
    renderUrls(connection.relayUrls);
  }
  setTab('send');
  loadWatchFrame();
  pollStatus();
}

function disconnect() {
  if (running || starting) {
    stopStream();
  }
  connection = null;
  $('app-view').classList.add('hidden');
  $('connect-view').classList.remove('hidden');
  $('conn-badge').textContent = '未连接';
  $('conn-badge').classList.remove('ok');
  $('disconnect-btn').classList.add('hidden');
  $('watch-frame').src = 'about:blank';
  setRunning(false);
}

// ---------------------------------------------------------------- 模式切换

function setTab(tab) {
  currentTab = tab;
  $('tab-send-btn').classList.toggle('active', tab === 'send');
  $('tab-watch-btn').classList.toggle('active', tab === 'watch');
  $('tab-send').classList.toggle('hidden', tab !== 'send');
  $('tab-watch').classList.toggle('hidden', tab !== 'watch');
  if (tab === 'watch') loadWatchFrame();
}

/** 加载（或刷新）观看页 iframe。 */
function loadWatchFrame() {
  if (!connection) return;
  $('watch-loading').classList.remove('hidden');
  $('watch-frame').src = connection.url + '/';
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
      btn.title = '点击直接连接';
      btn.onclick = () => {
        $('relay-addr').value = url;
        document.querySelector('input[name="conn"][value="remote"]').checked = true;
        $('remote-row').classList.remove('hidden');
        connect();
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

/** Android：构造原生采集参数（mobile::start_capture 的 CaptureArgs）。 */
function buildCaptureArgs() {
  const q = QUALITIES[$('quality-select').value];
  return {
    streamId: 'stross-' + Date.now().toString(36),
    title: $('title-input').value.trim() || '我的串流',
    width: q.width,
    height: q.height,
    fps: q.fps,
    bitrateKbps: q.bitrateKbps,
    withAudio: $('mic-enable').checked,
  };
}

// ---------------------------------------------------------------- 推流控制

async function startStream() {
  hideError();
  if (!connection) {
    showFatal('请先连接中继');
    return;
  }
  savePrefs();
  $('start-btn').disabled = true;
  try {
    if (IS_ANDROID) {
      starting = true;
      startingSince = Date.now();
      setRunning(true, 'starting');
      // Android：原生采集（MediaProjection + MediaCodec），经手机内嵌中继推流
      const res = await invoke('start_capture', { args: buildCaptureArgs() });
      const urls = MY_IPS.length
        ? MY_IPS.map((ip) => `http://${ip}:${res.relayPort}/`)
        : [`http://127.0.0.1:${res.relayPort}/`];
      renderUrls(urls);
      pollMobileStatus(); // 立即查一次真实采集状态
    } else {
      const res = await invoke('start_stream', { cfg: buildConfig(), relayUrl: connection.wsUrl });
      renderUrls(res.watchUrls);
      setRunning(true, 'live');
    }
  } catch (e) {
    showFatal(String(e));
    starting = false;
    setRunning(false);
  }
}

async function stopStream() {
  try {
    if (IS_ANDROID) {
      await invoke('stop_capture');
    } else {
      await invoke('stop_stream');
    }
  } catch (e) {
    showFatal(String(e));
  }
  starting = false;
  setRunning(false);
}

/** Android：轮询采集真实状态（Kotlin 控制帧 t=9 回报）。 */
async function pollMobileStatus() {
  if (!IS_ANDROID || !connection) return;
  try {
    const s = await invoke('mobile_status');
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
  } catch (_) {
    /* ignore */
  }
}

async function pollStatus() {
  if (IS_ANDROID) {
    // Android 每 2 秒轮询真实采集状态
    if (running || starting) pollMobileStatus();
    return;
  }
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

/** phase: 'idle' | 'starting' | 'live' */
function setRunning(r, phase = r ? 'live' : 'idle') {
  running = r;
  const dot = $('status-dot');
  const text = $('status-text');
  $('start-btn').disabled = r || starting;
  $('stop-btn').disabled = !(r || starting);
  $('viewer-btn').disabled = !r;
  if (phase === 'starting') {
    dot.className = 'dot starting';
    text.textContent = '采集中…';
    $('stream-meta').textContent = '等待系统授权与投影就绪（OPPO 等机型可能需 10~20 秒）';
  } else if (phase === 'live') {
    dot.className = 'dot live';
    text.textContent = IS_ANDROID ? '采集中 ✓ 推流中' : '推流中';
    // 明确告知观看地址，避免"不知道是否真的在推"
    const first = document.querySelector('#url-list li');
    $('stream-meta').textContent = first && first.textContent.includes('http')
      ? `屏幕采集已就绪 ✅ 局域网内浏览器打开 ${first.textContent.trim().replace('▶', '')} 即可观看`
      : '推流中，请在「📥 观看」页查看';
  } else {
    dot.className = 'dot idle';
    text.textContent = '未推流';
    $('stream-meta').textContent = '';
  }
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
      navigator.clipboard?.writeText(u).then(() => {
        li.style.borderColor = 'var(--ok)';
        li.textContent = '✅ 已复制';
        setTimeout(() => {
          li.style.borderColor = '';
          li.innerHTML = '';
          li.appendChild(tag);
          li.appendChild(document.createTextNode(u));
        }, 1500);
      });
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
$('disconnect-btn').onclick = disconnect;
$('tab-send-btn').onclick = () => setTab('send');
$('tab-watch-btn').onclick = () => setTab('watch');
$('watch-refresh-btn').onclick = loadWatchFrame;
$('start-btn').onclick = startStream;
$('stop-btn').onclick = stopStream;
$('viewer-btn').onclick = () => invoke('open_viewer').catch((e) => showFatal(String(e)));

// iframe 加载完成后隐藏 loading
$('watch-frame').addEventListener('load', () => {
  $('watch-loading').classList.add('hidden');
});

init();
setInterval(pollStatus, 2000);
