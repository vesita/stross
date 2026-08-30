// Stross GUI 前端无头测试（节点 → 设备 → 端点：通告 + 订阅）。
//
// 用 jsdom 加载真实 index.html + 编译后的 app.js，mock Tauri invoke 与 fetch，
// 驱动并断言：免先连锚定 → 设备列表渲染 → 设备展开/对端目录 → 订阅端点 →
// 接收面板状态 → 遗留广播/凭证 UI 移除 → 手动添加设备 → 本机通告/取消通告
// → 设备授权弹窗（允许/记住/拒绝）→ 防火墙一键放行。
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
const appFiles = ['types', 'state', 'ui', 'discovery', 'endpoints', 'subscribe', 'firewall', 'main'];
const appSrc = appFiles
  .map((f) => readFileSync(join(root, 'apps/stross-gui/web/app', f + '.js'), 'utf8'))
  .join('\n');

const dom = new JSDOM(html, { url: 'http://localhost/', runScripts: 'outside-only', pretendToBeVisual: true });
const { window } = dom;
const { document } = window;
// jsdom 无滚动/剪贴板/canvas 实现：错误提示框、复制按钮与画布清空时兜底
window.HTMLElement.prototype.scrollIntoView = () => {};
window.HTMLCanvasElement.prototype.getContext = () => null;
Object.defineProperty(window.navigator, 'clipboard', {
  value: { writeText: async () => {} },
  configurable: true,
});

// —— mock：Tauri invoke 调用记录 ——
const calls = [];
let streamRunning = false; // 与 start_stream/stop_stream 联动（真实行为）
let mockRecvEnded = false; // B6 测试：流结束 → receive_status 返回 running=false 且已收帧
let fwMissing = ['18779/tcp', '33464/udp']; // 防火墙自检回放（缺 SRT? 实际缺两个）
let scanReturnOverride = null; // [5] 手动添加场景：scan_devices 返回空列表
// —— 端点框架状态（本机通告：deviceId → {visibility, delivery}，供 local_catalog 回放） ——
const localEpState = new Map();
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
    case 'request_share_token':
      // 协商端点回放：null = 设备不支持协商（404）→ 前端回退手动粘贴
      if (!negGrant) throw new Error('协商端点不存在（HTTP 404）');
      return negGrant;
    case 'start_relay':
      return { port: 8777, urls: ['http://192.168.1.50:8777/'], name: '测试电脑', kind: 'relay', roles: [], transports: [], ip: null };
    // 扫描聚合在 Rust（scan_devices）；前端只消费 ScannedDevice[]（isSelf/探测已含）
    case 'scan_devices':
      if (scanReturnOverride) return scanReturnOverride; // [5] 场景：模拟空列表
      return [
        {
          name: '测试电脑', ip: '192.168.1.50', port: 8777, isSelf: true, online: true,
          roles: [], media: [], transports: [], endpoints: [], srtPort: 9001, quicPort: 9002,
          streams: streamRunning
            ? [{ streamId: 'sess-test', title: '我的屏幕', watchers: 0, video: true, audio: false }]
            : [],
        },
        {
          name: '手机A', ip: '192.168.1.51', port: 9001, isSelf: false, online: true,
          roles: ['sender'], media: [], transports: [], endpoints: [], srtPort: null, quicPort: 9002,
          streams: [{ streamId: 's1', title: '手机麦克风', watchers: 0, video: false, audio: true }],
        },        {
          name: '电脑B', ip: '192.168.1.52', port: 9002, isSelf: false, online: true,
          roles: ['relay'], media: [], transports: [], endpoints: [], srtPort: null, quicPort: 9002,
          streams: [{ streamId: 's2', title: '电脑屏幕', watchers: 1, video: true, audio: false }],
        },
      ];
    case 'probe_relay':
      return true; // 手动添加地址可达
    case 'anchor_streams':
      return streamRunning
        ? [{ streamId: 'sess-test', title: '我的屏幕', watchers: 0, video: true, audio: false }]
        : [];
    case 'start_stream':
      streamRunning = true;
      return { relayPort: 8777, watchUrls: ['http://192.168.1.50:8777/'], streamId: 'sess-test' };
    case 'stop_stream':
      streamRunning = false;
      return undefined;
    case 'issue_share_token': {
      return {
        token: JSON.stringify({ v: 1, streamId: 'sess-share', pin: '483920', expiresAt: Math.floor(Date.now() / 1000) + 600, media: ['Mic'] }),
        streamId: 'sess-share', pin: '483920', expiresAt: Date.now() / 1000 + 600,
      };
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
    // —— 端点框架（节点 → 设备 → 端点） ——
    case 'local_catalog': {
      // 单层端点模型：平铺清单（available/lastError/published 自标注）
      const base = [
        { endpointId: 'screen:0', kind: 'screen', name: '屏幕', available: true, lastError: null },
        { endpointId: 'mic:builtin', kind: 'mic', name: '麦克风', available: true, lastError: null },
      ];
      const endpoints = base.map((d) => {
        const p = localEpState.get(d.endpointId);
        return {
          ...d,
          published: !!p,
          visibility: p ? p.visibility : 'confirm',
          delivery: p ? p.delivery : 'pull',
          transports: [], codecs: [],
          state: p ? (p.state || 'idle') : 'idle',
          subscribers: p ? (p.subscribers || 0) : 0,
          updatedAt: 0,
        };
      });
      return { endpoints };
    }
    case 'endpoint_ls':
      return {
        node: { deviceId: 'dev-b', deviceName: '电脑B' },
        endpoints: [
          {
            endpointId: 'screen:0',
            kind: 'screen',
            name: '屏幕',
            available: true,
            lastError: null,
            published: true,
            visibility: 'confirm',
            delivery: 'pull',
            transports: [{ transport: 'quic', priority: 0 }, { transport: 'ws', priority: 1 }],
            codecs: ['h264'],
            state: 'idle',
            subscribers: 0,
            updatedAt: 0,
          },
        ],
      };
    case 'endpoint_publish': {
      localEpState.set(args.deviceId, { visibility: args.visibility, delivery: args.delivery });
      return {
        endpointId: args.deviceId,
        kind: 'mic',
        name: '麦克风',
        available: true,
        lastError: null,
        published: true,
        visibility: args.visibility,
        delivery: args.delivery,
        transports: [], codecs: [], state: 'idle', subscribers: 0, updatedAt: 0,
      };
    }
    case 'endpoint_unpublish':
      localEpState.delete(args.endpointId);
      return undefined;
    case 'endpoint_subscribe_media':
      return { delivery: 'pull', relayUrl: 'ws://192.168.1.52:9002', streamId: 'sess-sub' };
    case 'discoverable_status':
      // 「可被发现」开关：refreshDiscoverable 依赖返回 Settings 对象
      return { discoverable: false };
    case 'set_discoverable':
      return undefined;
    default:
      return undefined;
  }
};
// —— mock：Tauri 事件（negotiator-request 等）触发句柄 ——
const eventHandlers = {};
let winFullscreen = false; // 播放器全屏 mock：setFullscreen 写入，isFullscreen 读取
window.__TAURI__ = {
  core: { invoke },
  event: {
    listen: (evt, cb) => {
      eventHandlers[evt] = cb;
      return Promise.resolve(() => {});
    },
  },
  window: {
    getCurrentWindow: () => ({
      isFullscreen: async () => winFullscreen,
      setFullscreen: async (v) => {
        winFullscreen = v;
      },
    }),
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
check('本机卡片有设备树容器（节点→设备→端点）', !!localCard?.querySelector('[data-role="local-devices"]'));
check('本机设备树渲染（屏幕/麦克风 两行，未通告 → 通告按钮）', localCard?.querySelectorAll('.ep-row').length === 2 && !!localCard?.querySelector('[data-act="publish-device"]'), `行数 ${localCard?.querySelectorAll('.ep-row').length}`);

console.log('\n[2] 设备卡片展开 → 对端目录（可订阅端点）');
const phoneCard = Array.from(devCards).find((c) => c.textContent.includes('手机A'));
phoneCard.querySelector('.dev-head').click();
await sleep(500);
devCards = document.querySelectorAll('#device-list .dev-card');
const phoneExpanded = Array.from(devCards).find((c) => c.textContent.includes('手机A'));
check('点头部展开设备', phoneExpanded?.classList.contains('expanded'));
check('展开区有对端目录容器', !!phoneExpanded?.querySelector('[data-role="remote-dir"]'));
const phoneDir = phoneExpanded?.querySelector('[data-role="remote-dir"]');
check('目录渲染出可订阅端点（screen:0 + 订阅按钮）', !!phoneDir?.querySelector('[data-act="subscribe-endpoint"]'));
check('目录状态行（展开即拉取）', !!phoneDir?.querySelector('.dir-status'));

console.log('\n[3] 订阅端点 → 握手 → 接收面板');
phoneDir.querySelector('[data-act="subscribe-endpoint"]').click();
await sleep(100);
check('订阅弹窗打开（端点信息 + 方向选择）', !$('sub-modal').classList.contains('hidden'));
$('sub-confirm-btn').click();
await sleep(400);
const subCalls = calls.filter((c) => c.cmd === 'endpoint_subscribe_media');
check('endpoint_subscribe_media 被调用（endpointId=screen:0, 端口缺省）', subCalls.some((c) => c.args.endpointId === 'screen:0'), JSON.stringify(subCalls[0]?.args));
const rc = calls.filter((c) => c.cmd === 'start_receive');
check('订阅握手后走 start_receive（streamId=sess-sub）', rc.some((c) => c.args.stream === 'sess-sub'), JSON.stringify(rc[rc.length - 1]?.args));
check('接收目标 = 握手返回的 wsBase', rc.some((c) => c.args.relay === 'ws://192.168.1.52:9002'), JSON.stringify(rc[rc.length - 1]?.args));
check('接收状态行显示（订阅中）', !$('recv-status-line').classList.contains('hidden'));
check('「停止接收」按钮出现', !$('recv-stop-btn').classList.contains('hidden'));

console.log('\n[3b] 断流自愈：流结束后接收 UI 自动回到空闲态');
{
  mockRecvEnded = true;
  await sleep(2500); // 轮询间隔 1s，等 2 轮
  check('B6: 断流后自动调用 stop_receive', calls.filter((c) => c.cmd === 'stop_receive').length >= 1, `stop 次数 ${calls.filter((c) => c.cmd === 'stop_receive').length}`);
  check('B6: 接收状态行隐藏（回到未接收）', $('recv-status-line').classList.contains('hidden'));
  check('B6: 等待浮层隐藏', $('recv-overlay').classList.contains('hidden'));
  check('B6: 停止按钮隐藏', $('recv-stop-btn').classList.contains('hidden'));
  mockRecvEnded = false;
}

console.log('\n[3c] 播放器全屏：按钮切换窗口全屏态 + 画布容器悬浮层 + ESC 退出');
{
  const fsBtn = $('recv-fs-btn');
  const stopBtn = $('recv-fs-stop-btn');
  const wrap = $('recv-canvas-wrap');
  check('控制条渲染（全屏/停止按钮 + 信息区）', !!fsBtn && !!stopBtn && !!$('recv-controls-info'));
  check('初始非全屏（图标 = 进入全屏）', fsBtn.innerHTML.includes('i-maximize'));
  fsBtn.click();
  await sleep(60);
  check('全屏：setFullscreen(true) 生效', winFullscreen === true, `winFullscreen=${winFullscreen}`);
  check('全屏：画布容器加 fs 悬浮层', wrap.classList.contains('fs'));
  check('全屏：按钮图标切换为退出（minimize）', fsBtn.innerHTML.includes('i-minimize'));
  window.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'Escape' }));
  await sleep(60);
  check('ESC 退出全屏（窗口态还原）', winFullscreen === false, `winFullscreen=${winFullscreen}`);
  check('ESC 退出：fs 悬浮层移除', !wrap.classList.contains('fs'));
  check('ESC 退出：按钮图标还原为进入全屏', fsBtn.innerHTML.includes('i-maximize'));
  // 全屏中停止接收 → 也应退出全屏（setReceiving(false) 兜底）
  fsBtn.click();
  await sleep(60);
  check('再次进入全屏', winFullscreen === true);
  stopBtn.click();
  await sleep(60);
  check('全屏中停止接收 → 退出全屏', winFullscreen === false, `winFullscreen=${winFullscreen}`);
  check('停止按钮走 stop_receive', calls.filter((c) => c.cmd === 'stop_receive').length >= 2);
}

console.log('\n[4] 遗留广播/凭证/共享流 UI 已移除（统一走通告/订阅）');
const legacyActs = ['broadcast-screen', 'broadcast-mic', 'recv-mic', 'mic-to'];
const legacyFound = Array.from(document.querySelectorAll('#device-list [data-act]')).filter((b) => legacyActs.includes(b.dataset.act));
check('无广播/凭证操作按钮', legacyFound.length === 0, `残留 ${legacyFound.map((b) => b.dataset.act).join(',')}`);
check('share-modal 已移除', !$('share-modal'));
check('mic-modal 已移除', !$('mic-modal'));
check('mic-recv 凭证面板/按钮已移除', !$('mic-recv-panel') && !$('mic-recv-btn') && !$('mic-recv-copy-btn'));
check('右栏为「接收」面板（无共享流列表）', !!$('recv-pane') && $('recv-pane').textContent.includes('接收') && !$('share-list'));

console.log('\n[5] 手动添加设备（免 mDNS 路径）');
window.localStorage.clear();
// 注：不能替换 window.__TAURI__.core.invoke —— ui.ts 的 call() 在 eval 时已
// 捕获 invoke 引用，属性替换无效。用 mock 内可变开关控制 scan_devices 返回。
scanReturnOverride = []; // 无 mDNS 设备；手动列表独立渲染
$('manual-addr').value = 'http://192.168.1.77:8777';
$('manual-add-btn').click();
await sleep(900);
const devCards2 = document.querySelectorAll('#device-list .dev-card');
check('手动设备卡片出现', Array.from(devCards2).some((c) => c.textContent.includes('192.168.1.77')));
check('手动设备标记（手动）', Array.from(devCards2).some((c) => c.textContent.includes('（手动')), '未标记（手动）');
check('手动添加历史持久化', !$('recent-block').classList.contains('hidden') && $('recent-block').textContent.includes('192.168.1.77'));
// 恢复扫描结果并点击「扫描」重建设备列表（含手机A/电脑B，供后续使用）
scanReturnOverride = null;
$('scan-btn').click();
await sleep(900);

console.log('\n[6] 本机设备通告 → 徽标 → 取消通告（端点闭环）');
const micRow = Array.from(document.querySelectorAll('[data-role="local-devices"] .ep-row')).find((r) => r.textContent.includes('麦克风'));
check('本机设备树第二行 = 麦克风', !!micRow);
micRow?.querySelector('[data-act="publish-device"]')?.click();
await sleep(100);
check('通告弹窗打开', !$('pub-modal').classList.contains('hidden'));
const pubRadio = document.querySelector('input[name="pub-vis"][value="public"]');
if (pubRadio) pubRadio.checked = true;
$('pub-confirm-btn').click();
await sleep(400);
const pubCallsMic = calls.filter((c) => c.cmd === 'endpoint_publish');
check('endpoint_publish 被调用（deviceId=mic:builtin, public）', pubCallsMic.some((c) => c.args.deviceId === 'mic:builtin' && c.args.visibility === 'public'), JSON.stringify(pubCallsMic[pubCallsMic.length - 1]?.args));
const micRow2 = Array.from(document.querySelectorAll('[data-role="local-devices"] .ep-row')).find((r) => r.textContent.includes('麦克风'));
check('通告后行显示「已通告」徽标', !!micRow2?.querySelector('.ep-badge'), micRow2?.textContent || '行丢失');
micRow2?.querySelector('[data-act="unpublish-endpoint"]')?.click();
await sleep(400);
const unpubCalls = calls.filter((c) => c.cmd === 'endpoint_unpublish');
check('endpoint_unpublish 被调用', unpubCalls.length === 1, `实际 ${unpubCalls.length}`);
const micRow3 = Array.from(document.querySelectorAll('[data-role="local-devices"] .ep-row')).find((r) => r.textContent.includes('麦克风'));
check('取消通告后回到「通告」按钮', !!micRow3?.querySelector('[data-act="publish-device"]'));

console.log('\n[9] 设备接入授权弹窗（电脑端首次人工确认：允许/记住）');
// 通过 negotiator-request 事件驱动电脑端弹窗（与真实路径一致：Rust 协商服务 →
// Tauri 事件 → 前端弹窗，注意 listen 包装解包 e.payload）
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

console.log('\n[11] 本机通告（默认 confirm/pull）→ 徽标（端点框架主路径）');
const screenRow = Array.from(document.querySelectorAll('[data-role="local-devices"] .ep-row')).find((r) => r.textContent.includes('屏幕'));
check('本机设备树第一行 = 屏幕', !!screenRow);
screenRow?.querySelector('[data-act="publish-device"]')?.click();
await sleep(100);
check('通告弹窗打开（可见性/delivery 选择）', !$('pub-modal').classList.contains('hidden'));
$('pub-confirm-btn').click();
await sleep(300);
const pubCalls = calls.filter((c) => c.cmd === 'endpoint_publish');
check('endpoint_publish 被调用（deviceId=screen:0, confirm/pull 默认）', pubCalls.some((c) => c.args.deviceId === 'screen:0' && c.args.visibility === 'confirm' && c.args.delivery === 'pull'), JSON.stringify(pubCalls[pubCalls.length - 1]?.args));
check('通告后弹窗关闭', $('pub-modal').classList.contains('hidden'));
const screenRow2 = Array.from(document.querySelectorAll('[data-role="local-devices"] .ep-row')).find((r) => r.textContent.includes('屏幕'));
check('通告后行显示「已通告」徽标（需确认 · 拉取）', !!screenRow2?.querySelector('.ep-badge') && screenRow2.textContent.includes('需确认') && screenRow2.textContent.includes('拉取'), screenRow2?.textContent || '行丢失');
check('目录拉取走 endpoint_ls（端口缺省）', calls.some((c) => c.cmd === 'endpoint_ls' && (c.args.port === undefined || c.args.port === 18779)), JSON.stringify(calls.find((c) => c.cmd === 'endpoint_ls')?.args));

console.log('\n[12] 端点活动共享 → 「停止共享」按钮（生命周期治理）');
// 模拟端点共享已登记（真实路径：订阅达成 → 自动推流 → local_catalog.state=active）；
// 状态经 2s 轮询（startStatusPolling → refreshLocalCatalog）驱动重渲染
const screenEpState = localEpState.get('screen:0') || { visibility: 'confirm', delivery: 'pull' };
localEpState.set('screen:0', { ...screenEpState, state: 'active', subscribers: 1 });
await sleep(2300);
const activeRow = Array.from(document.querySelectorAll('[data-role="local-devices"] .ep-row')).find((r) => r.textContent.includes('屏幕'));
check('活动共享行显示 live 徽标（正在共享/订阅中）', !!activeRow?.querySelector('.ep-badge.live') && (activeRow.textContent.includes('正在共享') || activeRow.textContent.includes('订阅中')), activeRow?.textContent || '行丢失');
const stopBtn = activeRow?.querySelector('[data-act="stop-share"]');
check('「停止共享」按钮出现', !!stopBtn);
stopBtn?.click();
await sleep(300);
const stopShareCalls = calls.filter((c) => c.cmd === 'endpoint_stop_share');
check('endpoint_stop_share 被调用（endpointId=screen:0）', stopShareCalls.length === 1 && stopShareCalls[0].args.endpointId === 'screen:0', JSON.stringify(stopShareCalls[0]?.args));

console.log(failures === 0 ? '\n✅ 全部通过' : `\n❌ ${failures} 项失败`);
process.exit(failures === 0 ? 0 : 1);