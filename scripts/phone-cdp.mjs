#!/usr/bin/env node
// 手机 WebView CDP 驱动（Tauri Android 调试通道）。
// 用法：
//   node phone-cdp.mjs eval   '<js>'       在页面上下文执行 JS 并打印 JSON 结果
//   node phone-cdp.mjs text                打印页面可见文本（去掉样式）
//   node phone-cdp.mjs click  '<selector>'  点击匹配的第一个元素（文档坐标中心）
//   node phone-cdp.mjs dump                 打印所有可点击元素的 id/data-act/文本/位置
import { pathToFileURL } from 'node:url';

const PORT = process.env.CDP_PORT || '19222';
const [cmd, arg] = process.argv.slice(2);

const pages = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json();
const page = pages.find((p) => p.type === 'page');
if (!page) {
  console.error('未找到 WebView 页面');
  process.exit(1);
}
const ws = new WebSocket(page.webSocketDebuggerUrl);
let seq = 0;
const pending = new Map();
ws.onmessage = (ev) => {
  const msg = JSON.parse(ev.data);
  if (msg.id && pending.has(msg.id)) {
    pending.get(msg.id)(msg);
    pending.delete(msg.id);
  }
};
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
function send(method, params = {}) {
  return new Promise((res) => {
    const id = ++seq;
    pending.set(id, res);
    ws.send(JSON.stringify({ id, method, params }));
  });
}
async function evalJs(expression) {
  const r = await send('Runtime.evaluate', {
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  if (r.result?.exceptionDetails) {
    throw new Error('JS 异常: ' + JSON.stringify(r.result.exceptionDetails));
  }
  return r.result?.result?.value;
}

if (cmd === 'eval') {
  const v = await evalJs(arg);
  console.log(typeof v === 'string' ? v : JSON.stringify(v, null, 2));
} else if (cmd === 'text') {
  const v = await evalJs(`document.body.innerText`);
  console.log(v);
} else if (cmd === 'dump') {
  const v = await evalJs(`JSON.stringify(
    [...document.querySelectorAll('[data-act],button,[data-act] *')]
      .filter(el => el.textContent.trim() || el.id || el.dataset.act)
      .map(el => {
        const r = el.getBoundingClientRect();
        return {
          tag: el.tagName, id: el.id, act: el.dataset.act || null,
          text: (el.textContent || '').trim().slice(0, 40),
          x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2),
          w: Math.round(r.width), h: Math.round(r.height),
          visible: r.width > 0 && r.height > 0,
        };
      })
  )`);
  for (const el of JSON.parse(v)) console.log(JSON.stringify(el));
} else if (cmd === 'click') {
  const v = await evalJs(`(() => {
    const el = document.querySelector(${JSON.stringify(arg)});
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2),
             text: (el.textContent || '').trim().slice(0, 40),
             visible: r.width > 0 && r.height > 0 };
  })()`);
  if (!v) { console.error(`选择器未命中: ${arg}`); process.exit(1); }
  if (!v.visible) { console.error(`元素不可见: ${v.text}`); process.exit(1); }
  // 以真实触摸事件点击（Tauri 前端监听 click）
  await evalJs(`(() => {
    const el = document.querySelector(${JSON.stringify(arg)});
    el.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, clientX: ${v.x}, clientY: ${v.y} }));
    return true;
  })()`);
  console.log(`已点击 (${v.x},${v.y}) ${v.text}`);
} else {
  console.error('未知命令: ' + cmd);
  process.exit(1);
}
ws.close();