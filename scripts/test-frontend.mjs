// Stross GUI 前端无头测试（设备 × 共享流 组合管理界面）。
//
// 用 jsdom 加载真实 index.html + 编译后的 app.js，mock Tauri invoke 与 fetch，
// 驱动并断言：免先连锚定 → 设备列表渲染 → 设备展开/在线共享 → 点共享条目即收
// （UDP 优先→QUIC/WS）→ 本机广播共享（弹窗）→ 手动添加设备 → B2 接收手机
// 麦克风（签凭证+面板）→ B2 共享麦克风到设备（凭证推流）→ 凭证自动协商
// （免粘贴）→ 设备授权弹窗（允许/记住/拒绝）→ 防火墙一键放行（41 项断言）。
//
// 运行（无需安装任何包，npx 临时拉 jsdom）：
//   npx -y -p jsdom@24 node scripts/test-frontend.mjs
import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import { execSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

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
const appFiles = ['state', 'ui', 'discovery', 'subscribe', 'publish', 'negotiate', 'firewall', 'main'];
const appSrc = appFiles
  .map((f) => readFileSync(join(root, 'apps/stross-gui/web/app', f + '.js'), 'utf8'))
  .join('\n');

const dom = new JSDOM(html, { url: 'http://localhost/', runScripts: 'outside-only', pretendToBeVisual: true });
const { window } = dom;
const { document } = window;
// jsdom 无滚动/剪贴板实现：错误提示框与复制按钮调用时兜底
window.HTMLElement.prototype.scrollIntoView = () => {};
Object.defineProperty(window.navigator, 'clipboard', {
  value: { writeText: async () => {} },
  configurable: true,
});

// —— mock：Tauri invoke 调用记录 ——
const calls = [];
let sharedTokenJson = null; // B2 签发结果的回放
let streamRunning = false; // 与 start_stream/stop_stream 联动（真实行为）
let mockRecvEnded = false; // B6 测试：流结束 → receive_status 返回 running=false 且已收帧
let fwMissing = ['18779/tcp', '33464/udp']; // 防火墙自检回放（缺 SRT? 实际缺两个）
const invoke = async (cmd, args) => {
  calls.push({ cmd, args: JSON.parse(JSON.stringify(args || {})) });
  switch (cmd) {
    case 'app_info':
      return { version: '0.1.0', platform: 'desktop', ffmpeg: true, ips: ['192.168.1.50'] };
    case 'list_devices':
      return { cameras: [], audioInputs: ['default'], systemAudio: [] };
    case 'device_identity':
      return { deviceId: 'dev-pc-test', deviceName: '测试电脑' };
    case 'firewall_status':
      return { ufwActive: true, defaultDenyIncoming: true, rules: [], missing: fwMissing };
    case 'firewall_allow':
      fwMissing = [];
      return undefined;
    case 'negotiator_respond':
      return undefined;
    case 'start_relay':
      return { port: 8777, urls: ['http://192.168.1.50:8777/'], name: 'Stross 本机中继', kind: 'relay', roles: [], transports: [], ip: null };
    case 'scan_relays':
      return [
        { port: 9001, urls: ['http://192.168.1.51:9001/'], name: '手机A', kind: 'relay', roles: ['sender'], transports: [], ip: '192.168.1.51' },
        { port: 9002, urls: ['http://192.168.1.52:9002/'], name: '电脑B', kind: 'relay', roles: ['relay'], transports: [], ip: '192.168.1.52' },
      ];
    case 'start_stream':
      streamRunning = true;
      return { relayPort: 8777, watchUrls: ['http://192.168.1.50:8777/'], streamId: 'sess-test' };
    case 'stop_stream':
      streamRunning = false;
      return undefined;
    case 'issue_share_token': {
      sharedTokenJson = JSON.stringify({
        v: 1, streamId: 'sess-share', pin: '483920',
        expiresAt: Math.floor(Date.now() / 1000) + 600, media: ['Mic'],
      });
      return { token: sharedTokenJson, streamId: 'sess-share', pin: '483920', expiresAt: Date.now() / 1000 + 600 };
    }
    case 'stream_status':
      return streamRunning
        ? { running: true, streamId: 'sess-test', title: '我的屏幕', relayPort: 8777, startedAt: Math.floor(Date.now() / 1000) }
        : { running: false, streamId: null, title: null, relayPort: null, startedAt: null };
    case 'capture_status':
      return { active: false, started: false, error: null };
    case 'start_receive':
      return undefined;
    case 'receive_status':
      return mockRecvEnded
        ? { running: false, received: 30, decodedVideo: 30, audioBlocks: 0, dropped: 0, error: null }
        : { running: true, received: 0, decodedVideo: 0, audioBlocks: 0, dropped: 0, error: null };
    case 'stop_receive':
      return undefined;
    default:
      return undefined;
  }
};
// —— mock：Tauri 事件（negotiator-request 等）触发句柄 ——
const eventHandlers = {};
window.__TAURI__ = {
  core: { invoke },
  event: {
    listen: (evt, cb) => {
      eventHandlers[evt] = cb;
      return Promise.resolve(() => {});
    },
  },
};

// —— mock：HTTP（/api/info + /api/streams + 协商端点；SRT/QUIC 为独立 UDP 端口）——
let negGrant = null; // 协商端点回放：null = 设备不支持协商（404），否则签发的凭证
window.fetch = async (url, opts) => {
  calls.push({ fetch: String(url), body: opts && opts.body });
  const u = String(url);
  if (u.includes('/api/negotiator/request')) {
    if (!negGrant) {
      return { ok: false, status: 404, json: async () => ({ error: '协商端点不存在' }) };
    }
    return { ok: true, status: 200, json: async () => negGrant };
  }
  if (u.includes('/api/info')) {
    return json({ srtPort: 9001, quicPort: 9002 });
  }
  if (u.startsWith('http://127.0.0.1:8777')) {
    // 本机锚点：start_stream 后的本机在线共享（供本机卡片流区渲染）
    return json({ streams: [{ streamId: 'sess-test', title: '我的屏幕', watchers: 0, video: { codec: 'h264', width: 1280, height: 720 }, audio: null }] });
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

window.eval(appSrc);

let failures = 0;
function check(name, cond, extra = '') {
  if (cond) { console.log('  ✓ ' + name); }
  else { failures++; console.log('  ✗ ' + name + (extra ? ' —— ' + extra : '')); }
}

await sleep(2500); // init：app_info → devices → ensureAnchor → 扫描 → 聚合

console.log('\n[1] 免先连锚定 + 设备列表渲染');
check('header 徽标 = 已锚定', $('anchor-badge').textContent === '已锚定');
check('init 自动调用了 start_relay（免先连锚定）', calls.some((c) => c.cmd === 'start_relay'));
let devCards = document.querySelectorAll('#device-list .dev-card');
check('设备卡片 = 3（本机 + 手机A + 电脑B）', devCards.length === 3, `实际 ${devCards.length}`);
const localCard = document.querySelector('#device-list .dev-card.local');
check('本机卡片存在且恒展开', !!localCard);
check('本机卡片有「接收手机麦克风」入口', !!localCard?.querySelector('[data-act="recv-mic"]'));
check('本机入口 IP 已渲染', $('ip-list').querySelectorAll('li').length > 0);

console.log('\n[2] 设备卡片展开 → 在线共享 + 共享操作');
const phoneCard = Array.from(devCards).find((c) => c.textContent.includes('手机A'));
check('手机A 徽标显示 1 条共享', phoneCard?.querySelector('.badge-streams')?.textContent.includes('1 条共享') || false);
phoneCard.querySelector('.dev-head').click();
await sleep(200);
devCards = document.querySelectorAll('#device-list .dev-card');
const phoneExpanded = Array.from(devCards).find((c) => c.textContent.includes('手机A'));
check('点头部展开设备', phoneExpanded?.classList.contains('expanded'));
check('展开区显示「共享麦克风到 TA」', !!phoneExpanded?.querySelector('[data-act="mic-to"]'));
const items = phoneExpanded ? phoneExpanded.querySelectorAll('.dev-stream-item') : [];
check('手机A 在线共享条目 = 1（s1 手机麦克风）', items.length === 1, `实际 ${items.length}`);
check('共享面板空态（暂无活动共享）', $('share-list').textContent.includes('暂无活动共享'));

console.log('\n[3] 点共享条目即接收（直连设备锚点，UDP 优先）');
items[0].click();
await sleep(400);
const rc = calls.filter((c) => c.cmd === 'start_receive');
check('start_receive 被调用', rc.length === 1, `实际 ${rc.length}`);
check('流 id = s1', rc[0] && rc[0].args.stream === 's1');
check('纯音频 → QUIC 优先（quic://192.168.1.51:9002）', rc[0] && rc[0].args.relay === 'quic://192.168.1.51:9002', JSON.stringify(rc[0]?.args));
check('音频输出 device（扬声器播放）', rc[0] && rc[0].args.audio === 'device');
check('共享面板出现入站条目（接收 麦克风 ← …）', $('share-list').textContent.includes('接收 麦克风'));
check('共享面板有「停止」按钮（B6 雏形：运行中可停止）', !!$('share-list').querySelector('.share-item .share-stop'));

console.log('\n[3b] 断流自愈（B6）：流结束后接收 UI 自动回到空闲态');
{
  await window.__TAURI__.core.invoke('stop_receive', {});
  const devs = document.querySelectorAll('#device-list .dev-card');
  const phoneC = Array.from(devs).find((c) => c.textContent.includes('手机A'));
  if (phoneC && !phoneC.classList.contains('expanded')) phoneC.querySelector('.dev-head').click();
  await sleep(200);
  const items2 = Array.from(document.querySelectorAll('#device-list .dev-card')).find((c) => c.textContent.includes('手机A'))?.querySelectorAll('.dev-stream-item') || [];
  items2[0]?.click();
  await sleep(400);
  check('B6: 重新开始接收', calls.filter((c) => c.cmd === 'start_receive').length >= 2);
  check('B6: 接收态显示（接收中/等待）', !$('recv-status-line').classList.contains('hidden'));
  // 流结束：mock 返回 running=false 且已收帧 → 前端应自动 stop 并清空
  mockRecvEnded = true;
  await sleep(2500); // 轮询间隔 1s，等 2 轮
  check('B6: 断流后自动调用 stop_receive', calls.filter((c) => c.cmd === 'stop_receive').length >= 1, `stop 次数 ${calls.filter((c) => c.cmd === 'stop_receive').length}`);
  check('B6: 共享面板不再有「进行中」入站条目', !$('share-list').textContent.includes('进行中'));
  check('B6: 接收状态行隐藏（回到未接收）', $('recv-status-line').classList.contains('hidden'));
  check('B6: 等待浮层隐藏', $('recv-overlay').classList.contains('hidden'));
  mockRecvEnded = false;
}

console.log('\n[4] 本机广播共享（弹窗：共享屏幕）');
document.querySelector('#device-list .dev-card.local [data-act="broadcast-screen"]').click();
await sleep(200);
check('共享弹窗打开', !$('share-modal').classList.contains('hidden'));
$('share-start-btn').click();
await sleep(400);
const sc = calls.filter((c) => c.cmd === 'start_stream');
check('开始推流被触发（无需先连接）', sc.length === 1);
check('推流 URL 锚定本机中继（quic://127.0.0.1:9002，视频默认无损 QUIC 优先）', sc[0] && sc[0].args.relayUrl === 'quic://127.0.0.1:9002', JSON.stringify(sc[0]?.args?.relayUrl));
check('cfg 含屏幕视频源', sc[0] && sc[0].args.cfg.video && sc[0].args.cfg.video.kind === 'screen');
check('共享面板出现出站条目（共享 屏幕 → 局域网广播）', $('share-list').textContent.includes('共享 屏幕 → 局域网广播'));
check('本机在线共享区出现新条目（点击即看自己）', document.querySelector('[data-role="local-streams"]')?.textContent.includes('sess-test') || false);

console.log('\n[5] 手动添加设备（免 mDNS 路径）');
window.localStorage.clear();
await window.__TAURI__.core.invoke('stop_stream', {});
await sleep(200);
const origInvoke = window.__TAURI__.core.invoke;
window.__TAURI__.core.invoke = async (cmd, args) => {
  if (cmd === 'start_relay') return { port: 8777, urls: ['http://192.168.1.50:8777/'], name: 'x', kind: 'relay', roles: [], transports: [], ip: null };
  if (cmd === 'scan_relays') return [];
  if (cmd === 'stream_status') return { running: false, streamId: null, title: null, relayPort: null, startedAt: null };
  return origInvoke(cmd, args);
};
$('manual-addr').value = 'http://192.168.1.77:8777';
$('manual-add-btn').click();
await sleep(900);
const devCards2 = document.querySelectorAll('#device-list .dev-card');
check('手动设备卡片出现', Array.from(devCards2).some((c) => c.textContent.includes('192.168.1.77')));
check('手动设备标记（手动）', Array.from(devCards2).some((c) => c.textContent.includes('（手动）')));
check('手动添加历史持久化', !$('recent-block').classList.contains('hidden') && $('recent-block').textContent.includes('192.168.1.77'));
// 恢复原 mock 并点击「扫描」重建设备列表（含手机A/电脑B，供 [6][7] 使用）
window.__TAURI__.core.invoke = origInvoke;
$('scan-btn').click();
await sleep(900);

console.log('\n[6] B2 接收手机麦克风（电脑端签发凭证 + 展示）');
const btn = $('mic-recv-btn');
btn.click();
await sleep(400);
const tok = calls.filter((c) => c.cmd === 'issue_share_token');
check('issue_share_token 被调用（ttl 600s）', tok.length === 1 && tok[0].args.ttlSecs === 600);
check('凭证面板显示', !$('mic-recv-panel').classList.contains('hidden'));
check('PIN 已展示（大写 PIN + 6 位）', /PIN \d{6}/.test($('mic-recv-pin').textContent));
check('凭证 JSON 已填入', $('mic-recv-token').value.length > 0);
check('等待手机接入状态', $('mic-recv-status').textContent.includes('等待手机接入'));

console.log('\n[7] B2 共享麦克风到电脑（手机端凭证推流）');
const pcCard = Array.from(document.querySelectorAll('#device-list .dev-card')).find((c) => c.textContent.includes('电脑B'));
pcCard.querySelector('.dev-head').click();
await sleep(200);
const pcExpanded = Array.from(document.querySelectorAll('#device-list .dev-card')).find((c) => c.textContent.includes('电脑B'));
pcExpanded.querySelector('[data-act="mic-to"]').click();
await sleep(300);
check('共享麦克风弹窗打开', !$('mic-modal').classList.contains('hidden'));
$('mic-token-input').value = sharedTokenJson;
$('mic-start-btn').click();
await sleep(500);
const sc2 = calls.filter((c) => c.cmd === 'start_stream' && c.args.cfg.shareToken);
check('凭证推流被触发', sc2.length === 1, `实际 ${sc2.length}`);
check('streamId = 凭证里的 sess-share（不写成本机会话）', sc2[0] && sc2[0].args.cfg.streamId === 'sess-share', JSON.stringify(sc2[0]?.args?.cfg));
check('cfg.shareToken = 凭证原样', sc2[0] && sc2[0].args.cfg.shareToken === sharedTokenJson);
check('纯音频（video=null）', sc2[0] && sc2[0].args.cfg.video === null);
check('推流目标 = 电脑B（quic://192.168.1.52:9002）', sc2[0] && sc2[0].args.relayUrl === 'quic://192.168.1.52:9002', JSON.stringify(sc2[0]?.args?.relayUrl));
check('共享面板出现定向出站条目（共享 麦克风 → 电脑B）', $('share-list').textContent.includes('共享 麦克风 → 电脑B'));

console.log('\n[8] 凭证自动协商（权限自动化：免粘贴，设备支持协商端点时直接推流）');
await window.__TAURI__.core.invoke('stop_stream', {});
await sleep(200);
// 设备支持协商端点 → 返回签发凭证（trusted=true 自动签发）
negGrant = {
  token: '{"v":1,"streamId":"sess-auto","pin":"111222","expiresAt":9999999999,"media":["mic"]}',
  streamId: 'sess-auto',
  pin: '111222',
  expiresAt: 9999999999,
  trusted: true,
};
const phoneCard2 = Array.from(document.querySelectorAll('#device-list .dev-card')).find((c) => c.textContent.includes('手机A'));
phoneCard2.querySelector('.dev-head').click();
await sleep(200);
const phoneExpanded2 = Array.from(document.querySelectorAll('#device-list .dev-card')).find((c) => c.textContent.includes('手机A'));
phoneExpanded2.querySelector('[data-act="mic-to"]').click();
await sleep(600);
check('协商请求 POST 到设备 18779 端口', calls.some((c) => c.fetch && c.fetch.includes(':18779/api/negotiator/request')), '未发现协商请求');
check('协商请求携带本机身份', calls.some((c) => c.body && c.body.includes('dev-pc-test')), '未携带 deviceId');
const sc3 = calls.filter((c) => c.cmd === 'start_stream' && c.args.cfg.shareToken && c.args.cfg.streamId === 'sess-auto');
check('直接用协商签发的凭证推流（streamId=sess-auto）', sc3.length === 1, `实际 ${sc3.length}`);
check('自动推流目标 = 手机A（quic://192.168.1.51:9002）', sc3[0] && sc3[0].args.relayUrl === 'quic://192.168.1.51:9002', JSON.stringify(sc3[0]?.args?.relayUrl));
check('弹窗显示自动凭证成功状态', $('mic-status').textContent.includes('自动获取凭证'));
// 停止，恢复手动粘贴路径备用
await window.__TAURI__.core.invoke('stop_stream', {});
await sleep(200);

console.log('\n[9] 设备接入授权弹窗（电脑端首次人工确认：允许/记住）');
// 设备不支持协商（404）时不弹电脑端确认——[7] 已覆盖回退；这里通过 negotiator-request
// 事件驱动电脑端弹窗（与真实路径一致：Rust 协商服务 → Tauri 事件 → 前端弹窗，
// 注意 listen 包装解包 e.payload）
await eventHandlers['negotiator-request']({ payload: { id: 'n9', deviceId: 'dev-phone-x', deviceName: '手机X', media: ['mic'], createdAt: 0 } });
await sleep(100);
check('授权弹窗打开', !$('approve-modal').classList.contains('hidden'));
check('弹窗显示设备名', $('approve-device').textContent.includes('手机X'));
check('弹窗显示申请媒体', $('approve-media').textContent.includes('mic'));
$('approve-allow-btn').click();
await sleep(100);
const ar = calls.filter((c) => c.cmd === 'negotiator_respond');
check('negotiator_respond 被调用（allow=true, remember=true）', ar.length === 1 && ar[0].args.allow === true && ar[0].args.remember === true, JSON.stringify(ar[0]?.args));
check('授权后弹窗关闭', $('approve-modal').classList.contains('hidden'));
await eventHandlers['negotiator-request']({ payload: { id: 'n10', deviceId: 'dev-phone-y', deviceName: '手机Y', media: ['mic'], createdAt: 0 } });
await sleep(100);
$('approve-deny-btn').click();
await sleep(100);
const ar2 = calls.filter((c) => c.cmd === 'negotiator_respond');
check('拒绝路径：allow=false', ar2.length === 2 && ar2[1].args.allow === false, JSON.stringify(ar2[1]?.args));

console.log('\n[10] 防火墙自动放行（权限自动化：自检横幅 + 一键放行）');
check('防火墙横幅出现（缺放行）', !$('fw-banner').classList.contains('hidden'));
check('横幅列出缺失端口', $('fw-missing').textContent.includes('18779/tcp') && $('fw-missing').textContent.includes('33464/udp'));
$('fw-allow-btn').click();
await sleep(150);
check('firewall_allow 被调用', calls.some((c) => c.cmd === 'firewall_allow'));
check('放行成功后横幅隐藏', $('fw-banner').classList.contains('hidden'));
$('fw-close-btn').click();

console.log(failures === 0 ? '\n✅ 全部通过' : `\n❌ ${failures} 项失败`);
process.exit(failures === 0 ? 0 : 1);