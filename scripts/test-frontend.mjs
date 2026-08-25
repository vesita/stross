// Stross GUI 前端无头测试（P0 免先连进入网格）。
//
// 用 jsdom 加载真实 index.html + 编译后的 app.js，mock Tauri invoke 与 fetch，
// 驱动并断言：免先连锚定 → 网格渲染 → 点设备过滤 → 点流即看（跳转+接收）→
// 推流锚定本机 → 手动添加设备 → SRT/QUIC 拨号格式（srt://<ip>:<srtPort>）。
//
// 运行（无需安装任何包，npx 临时拉 jsdom）：
//   npx -y -p jsdom@24 node scripts/test-frontend.mjs
//
// 无 ffmpeg / 无 GUI 环境可跑；app.js 是构建产物，与 app.ts 同步提交
// （见 scripts/check-frontend.sh）。
import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import { execSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

// jsdom 解析：优先当前环境；缺失时自动装到 /tmp 缓存（幂等），
// 用 createRequire 指向缓存目录，避免污染项目 node_modules。
const root = join(dirname(fileURLToPath(import.meta.url)), '..');
let requireJsdom;
try {
  requireJsdom = createRequire(import.meta.url);
  requireJsdom('jsdom');
} catch {
  const cache = join(tmpdir(), 'stross-jsdom');
  execSync(`npm i --no-save --no-audit --no-fund --prefix "${cache}" jsdom@24`, { stdio: 'inherit' });
  requireJsdom = createRequire(join(cache, 'noop.js'));
}
const { JSDOM } = requireJsdom('jsdom');
const html = readFileSync(join(root, 'apps/stross-gui/web/index.html'), 'utf8');
const appJs = readFileSync(join(root, 'apps/stross-gui/web/app.js'), 'utf8');

const dom = new JSDOM(html, { url: 'http://localhost/', runScripts: 'outside-only', pretendToBeVisual: true });
const { window } = dom;
const { document } = window;

// —— mock：Tauri invoke 调用记录 ——
const calls = [];
const invoke = async (cmd, args) => {
  calls.push({ cmd, args: JSON.parse(JSON.stringify(args || {})) });
  switch (cmd) {
    case 'app_info':
      return { version: '0.1.0', platform: 'desktop', ffmpeg: true, ips: ['192.168.1.50'] };
    case 'list_devices':
      return { cameras: [], audioInputs: ['default'], systemAudio: [] };
    case 'start_relay':
      return { port: 8777, urls: ['http://192.168.1.50:8777/'], name: 'Stross 本机中继', kind: 'relay', roles: [], transports: [], ip: null };
    case 'scan_relays':
      return [
        { port: 9001, urls: ['http://192.168.1.51:9001/'], name: '手机A', kind: 'relay', roles: ['sender'], transports: [], ip: '192.168.1.51' },
        { port: 9002, urls: ['http://192.168.1.52:9002/'], name: '电脑B', kind: 'relay', roles: ['relay'], transports: [], ip: '192.168.1.52' },
      ];
    case 'start_stream':
      return { relayPort: 8777, watchUrls: ['http://192.168.1.50:8777/'], streamId: 'sess-test' };
    case 'stream_status':
      return { running: false, streamId: null, title: null, relayPort: null, startedAt: null };
    case 'capture_status':
      return { active: false, started: false, error: null };
    case 'start_receive':
      return undefined;
    case 'receive_status':
      return { running: true, received: 0, decodedVideo: 0, audioBlocks: 0, dropped: 0, error: null };
    case 'stop_receive':
    case 'stop_stream':
      return undefined;
    default:
      return undefined;
  }
};
window.__TAURI__ = {
  core: { invoke },
  event: { listen: () => Promise.resolve(() => {}) },
};

// —— mock：HTTP（GUI 网格聚合 /api/info + /api/streams；SRT/QUIC 为独立 UDP 端口）——
window.fetch = async (url) => {
  calls.push({ fetch: String(url) });
  const u = String(url);
  if (u.includes('/api/info')) {
    return json({ srtPort: 9001, quicPort: 9002 });
  }
  if (u.includes('/api/streams')) {
    if (u.startsWith('http://192.168.1.51')) {
      return json({ streams: [{ streamId: 's1', title: '手机麦克风', watchers: 0, video: null, audio: { codec: 'aac', sampleRate: 48000, channels: 1 } }] });
    }
    if (u.startsWith('http://192.168.1.52')) {
      return json({ streams: [{ streamId: 's2', title: '电脑屏幕', watchers: 1, video: { codec: 'h264', width: 1280, height: 720 }, audio: null }] });
    }
  }
  return json({ streams: [] });
};
function json(obj) {
  return Promise.resolve({ ok: true, status: 200, json: async () => obj });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const $ = (id) => document.getElementById(id);

window.eval(appJs);

let failures = 0;
function check(name, cond, extra = '') {
  if (cond) { console.log('  ✓ ' + name); }
  else { failures++; console.log('  ✗ ' + name + (extra ? ' —— ' + extra : '')); }
}

await sleep(2500); // init：app_info → devices → ensureAnchor → 扫描 → 聚合

console.log('\n[1] 免先连锚定 + 进入网格');
check('打开即进入 app-view（无连接门槛）', !$('app-view').classList.contains('hidden'));
check('header 徽标 = 已锚定', $('anchor-badge').textContent === '已锚定');
check('anchor-box 显示端口 8777', $('anchor-box').textContent.includes('8777'));
check('本机锚点入口 URL 已渲染（推流页 url-list）', $('url-list').querySelectorAll('li').length > 0);
check('init 自动调用了 start_relay（免先连锚定）', calls.some((c) => c.cmd === 'start_relay'));
check('自动扫描设备（scan_relays ≥ 1 次）', calls.filter((c) => c.cmd === 'scan_relays').length >= 1);
check('网格默认 tab 激活', $('tab-grid-btn').classList.contains('active'));

console.log('\n[2] 网格渲染：设备卡片 + 全网串流聚合');
const devCards = document.querySelectorAll('#scan-results .scan-card');
check('局域网设备卡片 = 2（手机A/电脑B）', devCards.length === 2, `实际 ${devCards.length}`);
const streamCards = document.querySelectorAll('#discover-streams .scan-card');
check('全网串流卡片 = 2（s1/s2 聚合自两台设备）', streamCards.length === 2, `实际 ${streamCards.length}`);
check('串流卡片带设备名（手机A）', Array.from(streamCards).some((c) => c.textContent.includes('手机A')));
check('串流卡片带轨道 chip（音频）', Array.from(streamCards).some((c) => c.querySelector('.chip.audio')));

console.log('\n[3] 点设备卡片 = 只看该设备串流');
devCards[0].click();
await sleep(600);
let shown = document.querySelectorAll('#discover-streams .scan-card');
check('点「手机A」后只剩其串流（s1）', shown.length === 1 && shown[0].textContent.includes('手机麦克风'), `实际 ${shown.length}`);
check('选中态高亮', devCards[0].classList.contains('selected'));
devCards[0].click(); // 再点取消
await sleep(600);
shown = document.querySelectorAll('#discover-streams .scan-card');
check('再点取消筛选，恢复全部（2）', shown.length === 2, `实际 ${shown.length}`);

console.log('\n[4] 点流卡片 = 按需建立（跳观看页 + start_receive 直连锚点）');
const s2card = Array.from(document.querySelectorAll('#discover-streams .scan-card')).find((c) => c.textContent.includes('电脑屏幕'));
s2card.click();
await sleep(400);
check('自动跳转「观看（收）」tab', !$('tab-watch').classList.contains('hidden') && $('tab-watch-btn').classList.contains('active'));
const rc = calls.filter((c) => c.cmd === 'start_receive');
check('start_receive 被调用', rc.length === 1, `实际 ${rc.length}`);
check('接收目标 = 设备锚点（srt://192.168.1.52:9001，UDP 优先）', rc[0] && rc[0].args.relay === 'srt://192.168.1.52:9001', JSON.stringify(rc[0]?.args));
check('流 id = s2', rc[0] && rc[0].args.stream === 's2');
check('recv-stream-input 已填流 id', $('recv-stream-input').value === 's2');
check('音频输出 device（默认设备播放）', rc[0] && rc[0].args.audio === 'device');

console.log('\n[5] 推流锚定本机（免先连推流）');
$('tab-send-btn').click();
await sleep(200);
$('start-btn').click();
await sleep(300);
const sc = calls.filter((c) => c.cmd === 'start_stream');
check('开始推流被触发（无需先连接）', sc.length === 1);
check('推流 URL 锚定本机中继（srt://127.0.0.1:9001，auto 优先 UDP）', sc[0] && sc[0].args.relayUrl === 'srt://127.0.0.1:9001', JSON.stringify(sc[0]?.args?.relayUrl));

console.log('\n[6] 手动添加设备（免 mDNS 路径）');
window.localStorage.clear();
const origInvoke = window.__TAURI__.core.invoke;
window.__TAURI__.core.invoke = async (cmd, args) => {
  if (cmd === 'start_relay') return { port: 8777, urls: ['http://192.168.1.50:8777/'], name: 'x', kind: 'relay', roles: [], transports: [], ip: null };
  if (cmd === 'scan_relays') return [];
  return origInvoke(cmd, args);
};
$('manual-addr').value = 'http://192.168.1.77:8777';
$('manual-add-btn').click();
await sleep(800);
const devCards2 = document.querySelectorAll('#scan-results .scan-card');
check('手动设备卡片出现', Array.from(devCards2).some((c) => c.textContent.includes('192.168.1.77')));
check('手动设备标记（手动）', Array.from(devCards2).some((c) => c.textContent.includes('（手动）')));
check('手动添加历史持久化', !$('recent-block').classList.contains('hidden') && $('recent-block').textContent.includes('192.168.1.77'));

console.log(failures === 0 ? '\n✅ 全部通过' : `\n❌ ${failures} 项失败`);
process.exit(failures === 0 ? 0 : 1);
