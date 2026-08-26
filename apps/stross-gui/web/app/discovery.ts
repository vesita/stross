// Stross 前端 —— 发现域（script 全局作用域）：
// 本机锚定（start_relay + mDNS 广播）+ 局域网设备扫描 + 手动添加 +
// 设备图渲染（本机卡片 + 设备卡片，含在线共享聚合 /api/streams）。

function normAddr(addr: string): string | null {
  let a = addr.trim();
  if (!a) return null;
  if (!/^https?:\/\//i.test(a)) a = 'http://' + a;
  return a.replace(/\/+$/, '');
}

/** link-local / 回环地址（fe80::/10、169.254/16、127.0.0.1、::1）：不可达或
 *  仅本机可见，剔除出设备列表（Android 锚点回退回环时扫描会回显 127.0.0.1）。 */
function isLinkLocalIp(ip: string): boolean {
  return (
    ip === '127.0.0.1' ||
    ip === '::1' ||
    /^fe80:/i.test(ip) ||
    /^169\.254\./.test(ip)
  );
}

/** 免先连核心：自动锚定本机（`start_relay` 幂等，启动受控中继 + mDNS 广播）。 */
async function ensureAnchor(): Promise<void> {
  setAnchorBadge('anchoring');
  try {
    const info = (await call('start_relay')) as RelayInfo;
    anchor = {
      port: info.port,
      urls: info.urls,
      srtUrl: null,
      quicUrl: null,
    };
    setAnchorBadge('ok');
    void refreshAnchorPorts();
    renderLocalCard(); // 本机卡片状态更新
  } catch (e) {
    anchor = null;
    setAnchorBadge('err');
    const box = $('grid-error');
    box.textContent = '本机锚定失败：' + (e as Error).message + '（仍可接收局域网共享）';
    box.classList.remove('hidden');
    const retry = document.createElement('button');
    retry.type = 'button';
    retry.innerHTML = icon('refresh') + '<span>重试锚定</span>';
    retry.onclick = () => void ensureAnchor();
    box.appendChild(retry);
  }
}

/** 拉取本机锚点 `/api/info`，填充 SRT/QUIC 拨号地址（失败静默，退回 WS）。 */
async function refreshAnchorPorts(): Promise<void> {
  if (!anchor) return;
  try {
    const resp = await fetch(`http://127.0.0.1:${anchor.port}/api/info`, { cache: 'no-store' });
    if (!resp.ok) return;
    const info = (await resp.json()) as { srtPort?: number; quicPort?: number };
    if (info.srtPort) anchor.srtUrl = `srt://127.0.0.1:${info.srtPort}`;
    if (info.quicPort) anchor.quicUrl = `quic://127.0.0.1:${info.quicPort}`;
  } catch (_) { /* 中继可能不支持 /api/info：保持 null，走 WS */ }
}

/** 手动添加设备地址（免 mDNS）：探测可达后进入设备列表。 */
async function addManualRelay(): Promise<void> {
  hideGridError();
  const addr = normAddr($input('manual-addr').value);
  if (!addr) {
    showGridError('请输入设备地址，例如 http://192.168.1.100:8777');
    return;
  }
  savePrefs();
  saveRecent(addr);
  // 探测中继是否可达（/api/streams 是受控/普通中继都提供的只读端点）
  try {
    const resp = await fetch(addr + '/api/streams', { cache: 'no-store' });
    if (!resp.ok) throw new Error('中继返回 HTTP ' + resp.status);
    await resp.json();
  } catch (e) {
    showGridError('无法访问 ' + addr + '：' + (e as Error).message);
    return;
  }
  manualRelays = [addr, ...manualRelays.filter((u) => u !== addr)];
  renderRecent();
  void scanRelays(); // 设备列表出现该设备
  void scanRemoteStreams(true); // 强制刷新其在线共享
}

/** 恢复上次的地址偏好，并渲染手动添加历史。（共享弹窗标题在打开时从 LS_TITLE 预填。） */
function restorePrefs(): void {
  const last = localStorage.getItem(LS_RELAY);
  if (last) $input('manual-addr').value = last;
  manualRelays = getRecent();
  renderRecent();
}

function savePrefs(): void {
  localStorage.setItem(LS_RELAY, $input('manual-addr').value.trim());
  const title = $input('share-title');
  if (title) localStorage.setItem(LS_TITLE, title.value.trim());
}

// ---------------- 手动添加历史 ----------------

function getRecent(): string[] {
  try {
    return JSON.parse(localStorage.getItem(LS_RECENT) || '[]') as string[];
  } catch {
    return [];
  }
}

function saveRecent(url: string): void {
  const list = getRecent().filter((u) => u !== url);
  list.unshift(url);
  localStorage.setItem(LS_RECENT, JSON.stringify(list.slice(0, 5)));
}

function removeRecent(url: string): void {
  const list = getRecent().filter((u) => u !== url);
  localStorage.setItem(LS_RECENT, JSON.stringify(list));
  manualRelays = list;
  renderRecent();
}

function renderRecent(): void {
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
    const main = document.createElement('span');
    main.className = 'recent-main';
    main.textContent = u;
    main.title = '点击重新添加';
    makeClickable(main, () => {
      $input('manual-addr').value = u;
      void addManualRelay();
    });
    const del = document.createElement('button');
    del.type = 'button';
    del.className = 'recent-del';
    del.title = '删除该记录';
    del.setAttribute('aria-label', '删除 ' + u);
    del.innerHTML = icon('x');
    del.onclick = (e) => {
      e.stopPropagation();
      removeRecent(u);
    };
    li.appendChild(main);
    li.appendChild(del);
    ul.appendChild(li);
  });
}

// ---------------------------------------------------------------------------
// 设备列表（左栏）：本机 + 局域网设备
// ---------------------------------------------------------------------------

/** 归一化设备基址：取 urls[0] 去掉尾部斜杠。 */
function deviceBase(r: { urls: string[] }): string {
  return (r.urls[0] || '').replace(/\/+$/, '');
}

/** 扫描局域网设备（mDNS + 手动添加），重建设备列表。 */
async function scanRelays(): Promise<void> {
  if (scanInFlight) return;
  scanInFlight = true;
  try {
    const relays = (await call('scan_relays')) as RelayInfo[];
    // 剔除本机 + link-local（本机单独展示；fe80 无 scope 不可达）
    const others = relays.filter((r) => !r.ip || (MY_IPS.indexOf(r.ip) === -1 && !isLinkLocalIp(r.ip)));
    const cards: DeviceView[] = others.map((r) => ({
      key: deviceBase(r),
      name: r.name || 'Stross 设备',
      meta: r.ip ? r.ip + ':' + r.port : deviceBase(r),
      isLocal: false,
      roles: r.roles || [],
      manual: false,
      base: deviceBase(r),
      srtUrl: null,
      quicUrl: null,
      streams: [],
    }));
    // 手动添加的设备（历史持久化）也进设备列表
    manualRelays.forEach((addr) => {
      const base = addr.replace(/\/+$/, '');
      if (!cards.some((c) => c.base === base)) {
        const hostPort = addr.replace(/^https?:\/\//, '');
        cards.push({
          key: base,
          name: hostPort + '（手动）',
          meta: hostPort,
          isLocal: false,
          roles: [],
          manual: true,
          base,
          srtUrl: null,
          quicUrl: null,
          streams: [],
        });
      }
    });
    // 保留已展开状态；本机卡片由渲染器恒置首位
    const keepExpanded = expandedDevice;
    deviceViews = cards;
    if (keepExpanded && !deviceViews.some((d) => d.key === keepExpanded)) expandedDevice = null;
    renderDeviceList();
  } catch (e) {
    showGridError('扫描失败：' + (e as Error).message);
  } finally {
    scanInFlight = false;
  }
}

/** 渲染左栏设备列表：本机卡片 + 各设备卡片（设备可展开）。 */
function renderDeviceList(): void {
  const box = $('device-list');
  box.innerHTML = '';
  box.appendChild(localDeviceCard());
  if (!deviceViews.length) {
    box.appendChild(emptyState('radio', '未发现局域网内其它设备（mDNS）。可手动输入地址添加。'));
    return;
  }
  for (const dev of deviceViews) {
    box.appendChild(deviceCard(dev));
  }
}

/** 本机卡片：广播共享入口 + 接收手机麦克风 + 本机入口地址。恒展开。 */
function localDeviceCard(): HTMLElement {
  const card = document.createElement('div');
  card.className = 'dev-card local expanded';
  card.dataset.key = 'local';

  const head = document.createElement('div');
  head.className = 'dev-head';
  const ic = document.createElement('span');
  ic.className = 'card-ic local';
  ic.innerHTML = icon('logo');
  const body = document.createElement('span');
  body.className = 'card-body';
  const nameLine = document.createElement('span');
  nameLine.className = 'scan-name';
  nameLine.textContent = '本机（我）';
  const metaLine = document.createElement('span');
  metaLine.className = 'scan-meta';
  metaLine.id = 'anchor-box';
  metaLine.textContent = anchor ? `已锚定 · 中继端口 ${anchor.port} · mDNS 广播中` : '锚定中…';
  body.appendChild(nameLine);
  body.appendChild(metaLine);
  head.appendChild(ic);
  head.appendChild(body);
  card.appendChild(head);

  const detail = document.createElement('div');
  detail.className = 'dev-detail';

  // 出站共享（广播）：屏幕 / 麦克风（本机能力共享给局域网任意接收方）
  const ops = document.createElement('div');
  ops.className = 'dev-ops';
  ops.appendChild(opButton('broadcast-screen', 'monitor', '共享屏幕（广播）'));
  ops.appendChild(opButton('broadcast-mic', 'mic', '共享麦克风（广播）'));
  const recvBtn = opButton('recv-mic', 'phone', '接收手机麦克风');
  recvBtn.id = 'mic-recv-btn'; // setBtnLoading 需要引用
  ops.appendChild(recvBtn);
  detail.appendChild(ops);

  // 接收手机麦克风凭证面板（B2：电脑端签发，手机出示后自动接收播放）
  const recvPanel = document.createElement('div');
  recvPanel.className = 'mic-recv-panel hidden';
  recvPanel.id = 'mic-recv-panel';
  const hint = document.createElement('p');
  hint.className = 'hint';
  hint.textContent = '在手机上打开 Stross → 找到本机 → 共享麦克风 → 粘贴下方凭证；接入后自动通过扬声器播放。';
  const row = document.createElement('div');
  row.className = 'row';
  const pin = document.createElement('span');
  pin.className = 'pin mono';
  pin.id = 'mic-recv-pin';
  const copyBtn = document.createElement('button');
  copyBtn.type = 'button';
  copyBtn.id = 'mic-recv-copy-btn';
  copyBtn.innerHTML = icon('copy') + '<span>复制凭证</span>';
  row.appendChild(pin);
  row.appendChild(copyBtn);
  const token = document.createElement('textarea');
  token.className = 'mono';
  token.id = 'mic-recv-token';
  token.readOnly = true;
  token.rows = 3;
  const status = document.createElement('div');
  status.className = 'meta';
  status.id = 'mic-recv-status';
  recvPanel.appendChild(hint);
  recvPanel.appendChild(row);
  recvPanel.appendChild(token);
  recvPanel.appendChild(status);
  detail.appendChild(recvPanel);

  // 本机在线共享（点条目即接收；不展开设备级操作）
  const localStreamsBox = document.createElement('div');
  localStreamsBox.className = 'dev-streams';
  localStreamsBox.dataset.role = 'local-streams';
  const lsTitle = document.createElement('h3');
  lsTitle.textContent = '本机在线共享';
  localStreamsBox.appendChild(lsTitle);
  localStreamsBox.appendChild(streamListPlaceholder());
  detail.appendChild(localStreamsBox);

  // 本机入口地址
  const entryTitle = document.createElement('h3');
  entryTitle.textContent = '本机入口';
  detail.appendChild(entryTitle);
  const ips = document.createElement('ul');
  ips.id = 'ip-list';
  ips.className = 'url-list';
  const ipsHint = document.createElement('li');
  ipsHint.className = 'hint';
  ipsHint.textContent = '读取中…';
  ips.appendChild(ipsHint);
  detail.appendChild(ips);

  card.appendChild(detail);
  return card;
}

/** 局域网设备卡片：点击头部展开 → 共享麦克风到 TA + TA 的在线共享（点流接收）。 */
function deviceCard(dev: DeviceView): HTMLElement {
  const card = document.createElement('div');
  card.className = 'dev-card' + (expandedDevice === dev.key ? ' expanded' : '');
  card.dataset.key = dev.key;

  const head = document.createElement('div');
  head.className = 'dev-head';
  head.setAttribute('role', 'button');
  head.tabIndex = 0;
  const ic = document.createElement('span');
  ic.className = 'card-ic';
  ic.innerHTML = icon(dev.manual ? 'link' : 'radio');
  const body = document.createElement('span');
  body.className = 'card-body';
  const nameLine = document.createElement('span');
  nameLine.className = 'scan-name';
  nameLine.textContent = dev.name;
  const metaLine = document.createElement('span');
  metaLine.className = 'scan-meta';
  metaLine.appendChild(document.createTextNode(dev.meta));
  if (dev.roles.length) {
    const chips = document.createElement('span');
    chips.className = 'chips';
    dev.roles.forEach((role) => chips.appendChild(roleChip(role)));
    metaLine.appendChild(chips);
  }
  body.appendChild(nameLine);
  body.appendChild(metaLine);
  head.appendChild(ic);
  head.appendChild(body);
  const badge = document.createElement('span');
  badge.className = 'badge-streams';
  badge.textContent = dev.streams.length ? dev.streams.length + ' 条共享' : '';
  head.appendChild(badge);
  const toggle = () => {
    expandedDevice = expandedDevice === dev.key ? null : dev.key;
    renderDeviceList();
  };
  head.addEventListener('click', (e) => {
    // 麦克风操作按钮在 detail 内，不冒泡到 head
    toggle();
  });
  head.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      toggle();
    }
  });
  card.appendChild(head);

  const detail = document.createElement('div');
  detail.className = 'dev-detail' + (expandedDevice === dev.key ? '' : ' hidden');
  const ops = document.createElement('div');
  ops.className = 'dev-ops';
  ops.appendChild(opButton('mic-to', 'mic', '共享麦克风到 TA'));
  detail.appendChild(ops);
  const streamsBox = document.createElement('div');
  streamsBox.className = 'dev-streams';
  streamsBox.dataset.role = 'node-streams';
  streamsBox.dataset.key = dev.key;
  const stTitle = document.createElement('h3');
  stTitle.textContent = 'TA 的在线共享（点条目接收）';
  streamsBox.appendChild(stTitle);
  streamsBox.appendChild(devStreamsOf(dev));
  detail.appendChild(streamsBox);
  card.appendChild(detail);
  return card;
}

function opButton(act: string, icName: string, label: string): HTMLButtonElement {
  const b = document.createElement('button');
  b.type = 'button';
  b.dataset.act = act;
  b.innerHTML = icon(icName) + '<span>' + label + '</span>';
  return b;
}

/** 设备（或本机）的在线共享条目区；空态提示。 */
function devStreamsOf(dev: DeviceView): HTMLElement {
  const box = document.createElement('div');
  if (!dev.streams.length) {
    const empty = document.createElement('p');
    empty.className = 'hint';
    empty.textContent = dev.isLocal ? '本机暂未有共享广播' : '该设备暂未有在线共享（或不可达）';
    box.appendChild(empty);
    return box;
  }
  dev.streams.forEach((s) => box.appendChild(streamItem(dev, s)));
  return box;
}

function streamListPlaceholder(): HTMLElement {
  const box = document.createElement('div');
  const empty = document.createElement('p');
  empty.className = 'hint';
  empty.textContent = '本机暂未有共享广播';
  box.appendChild(empty);
  return box;
}

/** 单个共享流条目（点流即看：按需直连该设备锚点接收）。 */
function streamItem(dev: DeviceView, s: RemoteStream): HTMLButtonElement {
  const b = document.createElement('button');
  b.type = 'button';
  b.className = 'dev-stream-item';
  b.dataset.stream = s.streamId;
  const ic = document.createElement('span');
  ic.className = 'card-ic';
  ic.innerHTML = icon(s.video ? 'video' : s.audio ? 'music' : 'radio');
  const body = document.createElement('span');
  body.className = 'card-body';
  const name = document.createElement('span');
  name.className = 'scan-name';
  name.textContent = s.title || s.streamId;
  const meta = document.createElement('span');
  meta.className = 'scan-meta';
  meta.appendChild(document.createTextNode(s.streamId + ' · ' + dev.name));
  const chips = document.createElement('span');
  chips.className = 'chips';
  if (s.video) chips.appendChild(chipEl('video', '视频'));
  if (s.audio) chips.appendChild(chipEl('audio', '音频'));
  meta.appendChild(chips);
  body.appendChild(name);
  body.appendChild(meta);
  b.appendChild(ic);
  b.appendChild(body);
  b.title = '点击接收 ' + s.streamId;
  b.onclick = () => {
    // 按需建立：目标切到该设备锚点（本机共享流 → 回本机锚点）；
    // 直连失败自动经本机级联代理
    if (dev.base) {
      targetRelay = {
        wsBase: dev.base.replace(/^http/, 'ws'),
        srtUrl: dev.srtUrl,
        quicUrl: dev.quicUrl,
      };
    } else {
      targetRelay = null;
    }
    remoteStreams.set(s.streamId, s);
    void startReceive(s.streamId);
  };
  return b;
}

/** 拉取所有设备的在线共享列表，填入设备视图并刷新（按设备分流展示）。 */
async function scanRemoteStreams(force = false): Promise<void> {
  if (discoverInFlight) return;
  if (!force && discoverCacheAt && Date.now() - discoverCacheAt < DISCOVER_TTL_MS) return;
  discoverInFlight = true;
  let relays: RelayInfo[];
  try {
    relays = (await call('scan_relays')) as RelayInfo[];
  } catch (e) {
    showGridError('扫描失败：' + (e as Error).message);
    discoverInFlight = false;
    return;
  }
  const others = relays.filter((r) => !r.ip || (MY_IPS.indexOf(r.ip) === -1 && !isLinkLocalIp(r.ip)));
  // 手动添加的设备并入聚合（无 mDNS 时也能看到其共享）
  manualRelays.forEach((addr) => {
    const base = addr.replace(/\/+$/, '');
    if (!others.some((r) => deviceBase(r) === base)) {
      others.push({
        port: 0,
        urls: [base + '/'],
        name: addr.replace(/^https?:\/\//, ''),
        kind: null,
        roles: [],
        transports: [],
        ip: null,
      });
    }
  });

  // 设备 key → 在线共享列表（保留既有流缓存，避免每次全量重建丢失流信息）
  const perDevice: Record<string, RemoteStream[]> = {};
  for (const r of others) {
    const base = deviceBase(r);
    if (!base) continue;
    let info: { srtPort?: number; quicPort?: number } | null = null;
    try {
      const iresp = await fetch(base + '/api/info', { cache: 'no-store' });
      if (iresp.ok) info = (await iresp.json()) as { srtPort?: number; quicPort?: number };
    } catch (_) { /* 该设备 /api/info 不可用 → SRT/QUIC null */ }
    try {
      const sresp = await fetch(base + '/api/streams', { cache: 'no-store' });
      if (!sresp.ok) continue;
      const data = (await sresp.json()) as { streams?: RemoteStream[] } | RemoteStream[];
      const list = Array.isArray(data) ? data : (data.streams || []);
      const hostOnly = base.replace(/^https?:\/\//, '').replace(/:\d+$/, '');
      list.forEach((st) => {
        if (!remoteStreams.has(st.streamId)) remoteStreams.set(st.streamId, st);
      });
      perDevice[base] = list;
      // 同步 SRT/QUIC 拨号地址到设备视图
      const dev = deviceViews.find((d) => d.base === base);
      if (dev) {
        dev.srtUrl = info && info.srtPort ? `srt://${hostOnly}:${info.srtPort}` : null;
        dev.quicUrl = info && info.quicPort ? `quic://${hostOnly}:${info.quicPort}` : null;
        dev.streams = list;
      }
    } catch (_) { /* 该设备不可达，跳过 */ }
  }
  // 拉取本机在线共享（本机卡片流区）
  if (anchor) {
    try {
      const resp = await fetch(`http://127.0.0.1:${anchor.port}/api/streams`, { cache: 'no-store' });
      if (resp.ok) {
        const data = (await resp.json()) as { streams?: RemoteStream[] } | RemoteStream[];
        const list = Array.isArray(data) ? data : (data.streams || []);
        list.forEach((st) => remoteStreams.set(st.streamId, st));
        localStreams = list;
      }
    } catch (_) { /* ignore */ }
  }
  renderLocalStreams();
  // 局部刷新展开设备的流区（避免整树重绘丢焦点）
  for (const [key, list] of Object.entries(perDevice)) {
    const dev = deviceViews.find((d) => d.base === key);
    if (dev) dev.streams = list;
  }
  refreshNodeStreams();
  discoverInFlight = false;
  discoverCacheAt = Date.now();
}

/** 本机在线共享缓存（供本机卡片流区渲染）。 */

/** 渲染本机卡片流区（本机在线共享）。 */
function renderLocalStreams(): void {
  const box = document.querySelector('[data-role="local-streams"]');
  if (!box) return;
  box.innerHTML = '';
  const title = document.createElement('h3');
  title.textContent = '本机在线共享';
  box.appendChild(title);
  if (!localStreams.length) {
    const empty = document.createElement('p');
    empty.className = 'hint';
    empty.textContent = '本机暂未有共享广播';
    box.appendChild(empty);
    return;
  }
  const localDev: DeviceView = {
    key: 'local',
    name: '本机（我）',
    meta: '',
    isLocal: true,
    roles: [],
    manual: false,
    base: null,
    srtUrl: anchor ? anchor.srtUrl : null,
    quicUrl: anchor ? anchor.quicUrl : null,
    streams: localStreams,
  };
  localStreams.forEach((s) => box.appendChild(streamItem(localDev, s)));
}

/** 局部刷新所有设备卡片的流区（保持展开/收起状态，不整树重绘）。 */
function refreshNodeStreams(): void {
  document.querySelectorAll('.dev-card[data-key]:not(.local)').forEach((card) => {
    const key = (card as HTMLElement).dataset.key!;
    const dev = deviceViews.find((d) => d.key === key);
    const box = card.querySelector('[data-role="node-streams"]');
    if (!dev || !box) return;
    box.innerHTML = '';
    const title = document.createElement('h3');
    title.textContent = 'TA 的在线共享（点条目接收）';
    box.appendChild(title);
    box.appendChild(devStreamsOf(dev));
    const badge = card.querySelector('.badge-streams');
    if (badge) badge.textContent = dev.streams.length ? dev.streams.length + ' 条共享' : '';
  });
}

/** 本机局域网入口地址渲染（点击复制）。 */
function renderIps(ips: string[]): void {
  const ul = $('ip-list');
  ul.innerHTML = '';
  ips.forEach((ip) => {
    const li = document.createElement('li');
    li.textContent = ip;
    li.title = '点击复制';
    makeClickable(li, () => {
      navigator.clipboard?.writeText(ip).then(() => {
        li.style.borderColor = 'var(--ok)';
        li.textContent = '已复制 ' + ip;
        setTimeout(() => {
          li.style.borderColor = '';
          li.textContent = ip;
        }, 1500);
      });
    });
    ul.appendChild(li);
  });
  if (!ips.length) ul.innerHTML = '<li class="hint">未获取到局域网 IP</li>';
}

/** 刷新本机卡片锚点状态行（锚定成功后调用）。 */
function renderLocalCard(): void {
  const meta = $('anchor-box');
  if (meta) {
    meta.textContent = anchor
      ? `已锚定 · 中继端口 ${anchor.port} · mDNS 广播中`
      : '未锚定';
  }
}
