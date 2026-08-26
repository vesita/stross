// Stross 前端 —— DOM 助手与通用渲染（script 全局作用域，勿加 import/export）。

const $ = (id: string): HTMLElement => document.getElementById(id) as HTMLElement;
const $input = (id: string): HTMLInputElement => $(id) as HTMLInputElement;
const $select = (id: string): HTMLSelectElement => $(id) as HTMLSelectElement;
const $btn = (id: string): HTMLButtonElement => $(id) as HTMLButtonElement;

const invoke: Invoke | undefined = (window as any).__TAURI__?.core?.invoke;

/** invoke 的安全封装：非 Tauri 环境下返回明确错误而非未定义调用。 */
function call(cmd: string, args?: Record<string, unknown>): Promise<any> {
  if (!invoke) return Promise.reject(new Error('当前页面未运行在 Stross 桌面应用中'));
  return invoke(cmd, args);
}

/** 内联 SVG 图标（引用 index.html 雪碧图中的 <symbol>）。 */
function icon(name: string, cls = ''): string {
  return `<svg class="ic${cls ? ' ' + cls : ''}" viewBox="0 0 24 24" aria-hidden="true"><use href="#i-${name}"></use></svg>`;
}

/** 空状态占位（图标 + 文案，可选错误配色）。 */
function emptyState(iconName: string, text: string, isError = false): HTMLElement {
  const box = document.createElement('div');
  box.className = 'empty';
  const ic = document.createElement('span');
  ic.innerHTML = icon(iconName);
  const p = document.createElement('p');
  if (isError) p.className = 'err-text';
  p.textContent = text;
  box.appendChild(ic);
  box.appendChild(p);
  return box;
}

/** 让列表项可点击且可键盘操作（Enter/Space 触发）。 */
function makeClickable(el: HTMLElement, fn: () => void): void {
  el.tabIndex = 0;
  el.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      fn();
    }
  });
  el.onclick = fn;
}

/** 按钮加载态：内嵌 spinner 并禁用；loading=false 恢复原内容。 */
function setBtnLoading(btn: HTMLButtonElement, loading: boolean): void {
  if (loading) {
    if (btn.dataset.loading === '1') return;
    btn.dataset.loading = '1';
    btn.dataset.label = btn.innerHTML;
    btn.innerHTML = '<span class="spinner"></span>' + btn.textContent;
    btn.disabled = true;
  } else {
    if (btn.dataset.loading !== '1') return;
    delete btn.dataset.loading;
    btn.innerHTML = btn.dataset.label || '';
    delete btn.dataset.label;
    btn.disabled = false;
  }
}

/** 显示视图/面板并播放淡入动画（重进时重启动画）。 */
function showView(el: HTMLElement): void {
  el.classList.remove('hidden');
  el.classList.remove('view-enter');
  void el.offsetWidth; // 强制 reflow，重启动画
  el.classList.add('view-enter');
}

/** 给错误框挂上「关闭」按钮并滚动到可见处。 */
function attachErrClose(box: HTMLElement): void {
  if (box.querySelector('.err-close')) return;
  const close = document.createElement('button');
  close.type = 'button';
  close.className = 'err-close';
  close.title = '关闭';
  close.innerHTML = icon('x');
  close.onclick = () => box.classList.add('hidden');
  box.appendChild(close);
  box.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
}

function fillSelect(sel: HTMLSelectElement, items: { value: string; label: string }[], emptyLabel: string): void {
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

/** 角色小标签 chip。 */
function roleChip(role: string): HTMLElement {
  const c = document.createElement('span');
  c.className = 'chip role';
  c.textContent = roleLabel(role);
  return c;
}

function roleLabel(r: string): string {
  return ROLE_LABELS[r] || r;
}

/** 轨道小标签（视频/音频 chip）。 */
function chipEl(kind: string, label: string): HTMLElement {
  const c = document.createElement('span');
  c.className = 'chip ' + kind;
  c.innerHTML = icon(kind === 'audio' ? 'music' : 'video') + '<span>' + label + '</span>';
  return c;
}

/** 可复制的 URL 列表项（点击复制，1.5s 反馈「已复制」）。 */
function urlListItem(u: string): HTMLLIElement {
  const li = document.createElement('li');
  const tag = document.createElement('span');
  tag.className = 'tag';
  tag.innerHTML = icon('play');
  li.appendChild(tag);
  li.appendChild(document.createTextNode(u));
  li.title = '点击复制';
  makeClickable(li, () => {
    navigator.clipboard?.writeText(u).then(() => {
      li.style.borderColor = 'var(--ok)';
      li.innerHTML = '<span class="tag ok">' + icon('check') + '</span>已复制';
      setTimeout(() => {
        li.style.borderColor = '';
        li.innerHTML = '';
        li.appendChild(tag);
        li.appendChild(document.createTextNode(u));
      }, 1500);
    });
  });
  return li;
}

function renderUrls(urls: string[]): void {
  const ul = $('url-list');
  ul.innerHTML = '';
  urls.forEach((u) => ul.appendChild(urlListItem(u)));
}

/** 秒数 → "X 分 Y 秒"（推流时长展示）。 */
function fmtElapsed(totalSecs: number): string {
  const s = Math.max(0, Math.floor(totalSecs));
  const m = Math.floor(s / 60);
  return m > 0 ? `${m} 分 ${s % 60} 秒` : `${s} 秒`;
}

/** Tauri 事件监听（__TAURI__.event.listen）。 */
function listen<T>(event: string, cb: (payload: T) => void): Promise<() => void> {
  const api = (window as any).__TAURI__?.event;
  if (!api?.listen) return Promise.resolve(() => {});
  return api.listen(event, (e: { payload: T }) => cb(e.payload));
}

function canvasCtx(): CanvasRenderingContext2D | null {
  const c = $('recv-canvas') as HTMLCanvasElement;
  return c.getContext('2d');
}

/** 把 RGBA 帧画到 canvas（宽度自适应，等比缩放）。
 *  `data` 为 base64 字符串（Rust 侧编码，桌面/Android 统一；atob 原生解码）。 */
function drawReceiveFrame(w: number, h: number, data: string): void {
  const ctx = canvasCtx();
  if (!ctx) return;
  const canvas = ctx.canvas;
  if (canvas.width !== w) canvas.width = w;
  if (canvas.height !== h) canvas.height = h;
  const bin = atob(data);
  const rgba = new Uint8ClampedArray(bin.length);
  for (let i = 0; i < bin.length; i++) rgba[i] = bin.charCodeAt(i);
  const img = new ImageData(rgba, w, h);
  ctx.putImageData(img, 0, 0);
}

// ---------------------------------------------------------------- 提示

function showFatal(msg: string): void {
  const box = $('error-box');
  box.textContent = msg;
  box.classList.remove('hidden');
  attachErrClose(box);
}
function hideError(): void {
  $('error-box').classList.add('hidden');
}
function showGridError(msg: string): void {
  const box = $('grid-error');
  box.textContent = msg;
  box.classList.remove('hidden');
  attachErrClose(box);
}
function hideGridError(): void {
  $('grid-error').classList.add('hidden');
}
function showRecvError(msg: string): void {
  const box = $('recv-error');
  box.textContent = msg;
  box.classList.remove('hidden');
  attachErrClose(box);
}
function hideRecvError(): void {
  $('recv-error').classList.add('hidden');
}

/** 头部锚点徽标状态：锚定中 / 已锚定 / 失败。 */
function setAnchorBadge(state: 'anchoring' | 'ok' | 'err'): void {
  const badge = $('anchor-badge');
  badge.className = 'badge' + (state === 'ok' ? ' ok' : state === 'err' ? ' err' : '');
  badge.textContent = state === 'ok' ? '已锚定' : state === 'err' ? '锚定失败' : '锚定中…';
}
