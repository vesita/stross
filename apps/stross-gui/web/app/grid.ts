// Stross 前端 —— 设备网格域：本机锚点、局域网设备扫描、串流聚合（script 全局作用域）。

function normAddr(addr: string): string | null {
  let a = addr.trim();
  if (!a) return null;
  if (!/^https?:\/\//i.test(a)) a = 'http://' + a;
  return a.replace(/\/+$/, '');
}

/** 免先连核心：自动锚定本机（`start_relay` 幂等，启动受控中继 + mDNS 广播）。 */
async function ensureAnchor(): Promise<void> {
  const box = $('anchor-box');
  box.classList.remove('err');
  box.innerHTML = '<span class="spinner"></span><span>锚定中…</span>';
  setAnchorBadge('anchoring');
  try {
    const info = (await call('start_relay')) as RelayInfo;
    anchor = {
      port: info.port,
      urls: info.urls,
      srtUrl: null,
      quicUrl: null,
    };
    renderAnchor(info);
    setAnchorBadge('ok');
    void refreshAnchorPorts();
  } catch (e) {
    anchor = null;
    setAnchorBadge('err');
    box.classList.add('err');
    box.innerHTML = '';
    box.appendChild(emptyState('server', '本机锚定失败：' + (e as Error).message + '（推流不可用；仍可观看局域网串流）', true));
    const retry = document.createElement('button');
    retry.type = 'button';
    retry.innerHTML = icon('refresh') + '<span>重试锚定</span>';
    retry.onclick = () => void ensureAnchor();
    box.appendChild(retry);
  }
}

/** 网格页「本机锚点」卡片：端口 + 可复制的入口地址。 */
function renderAnchor(info: RelayInfo): void {
  const box = $('anchor-box');
  box.innerHTML = '';
  const line = document.createElement('div');
  line.className = 'anchor-line';
  line.innerHTML = icon('server') + '<span>已锚定 · 中继端口 ' + info.port + ' · mDNS 广播中</span>';
  box.appendChild(line);
  const ul = document.createElement('ul');
  ul.className = 'url-list';
  info.urls.forEach((u) => ul.appendChild(urlListItem(u)));
  box.appendChild(ul);
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
  } catch (_) {
    // 中继可能不支持 /api/info（旧版本）：保持 null，观看端走 WS
  }
}

/** 手动添加设备地址（免 mDNS）：探测可达后进入设备/串流聚合。 */
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
  void scanRemoteStreams(true); // 强制刷新串流聚合（含手动设备）
}

/** 恢复上次的地址/流名称偏好，并渲染手动添加历史。 */
function restorePrefs(): void {
  const last = localStorage.getItem(LS_RELAY);
  if (last) $input('manual-addr').value = last;
  const title = localStorage.getItem(LS_TITLE);
  if (title) $input('title-input').value = title;
  manualRelays = getRecent();
  renderRecent();
}

function savePrefs(): void {
  localStorage.setItem(LS_RELAY, $input('manual-addr').value.trim());
  localStorage.setItem(LS_TITLE, $input('title-input').value.trim());
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

/** 删除一条手动添加历史（不触发添加），空列表时隐藏区块。 */
function removeRecent(url: string): void {
  const list = getRecent().filter((u) => u !== url);
  localStorage.setItem(LS_RECENT, JSON.stringify(list));
  manualRelays = list;
  renderRecent();
}

/** 渲染"手动添加历史"：点击重新添加该设备，右侧 ✕ 删除单条记录。 */
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
      e.stopPropagation(); // 不触发添加
      removeRecent(u);
    };
    li.appendChild(main);
    li.appendChild(del);
    ul.appendChild(li);
  });
}

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

/** 归一化设备基址：取 urls[0] 并去掉尾部斜杠。 */
function deviceBase(r: { urls: string[] }): string {
  return (r.urls[0] || '').replace(/\/+$/, '');
}

/** 扫描局域网内其它设备（mDNS + 手动添加）；打开应用时自动执行，也可手动重扫。
 *  点设备卡片切换「只看该设备串流」（「局域网串流」联动过滤）。 */
async function scanRelays(): Promise<void> {
  if (scanInFlight) return; // 防并发重复扫描
  scanInFlight = true;
  const box = $('scan-results');
  box.classList.remove('hidden');
  box.innerHTML = '<p class="hint">扫描中…</p>';
  try {
    const relays = (await call('scan_relays')) as RelayInfo[];
    // 剔除本机（本机锚点单独展示）
    const others = relays.filter((r) => !r.ip || MY_IPS.indexOf(r.ip) === -1);
    const cards: DeviceCard[] = others.map((r) => ({
      base: deviceBase(r),
      name: r.name || 'Stross 设备',
      meta: r.ip ? r.ip + ':' + r.port : deviceBase(r),
      roles: r.roles || [],
      manual: false,
    }));
    // 手动添加的设备（历史持久化）也进设备列表
    manualRelays.forEach((addr) => {
      const base = addr.replace(/\/+$/, '');
      if (!cards.some((c) => c.base === base)) {
        const hostPort = addr.replace(/^https?:\/\//, '');
        cards.push({ base, name: hostPort, meta: hostPort, roles: [], manual: true });
      }
    });
    box.innerHTML = '';
    if (!cards.length) {
      box.appendChild(emptyState('radio', '未发现局域网内其它设备（mDNS）。可手动输入地址添加。'));
      return;
    }
    cards.forEach((c) => {
      const card = document.createElement('button');
      card.type = 'button';
      card.className = 'scan-card' + (selectedDevice === c.base ? ' selected' : '');
      const ic = document.createElement('span');
      ic.className = 'card-ic';
      ic.innerHTML = icon(c.manual ? 'link' : 'radio');
      card.appendChild(ic);
      const body = document.createElement('span');
      body.className = 'card-body';
      const nameLine = document.createElement('span');
      nameLine.className = 'scan-name';
      nameLine.textContent = c.name + (c.manual ? '（手动）' : '');
      const metaLine = document.createElement('span');
      metaLine.className = 'scan-meta';
      metaLine.appendChild(document.createTextNode(c.meta));
      if (c.roles.length) {
        const chips = document.createElement('span');
        chips.className = 'chips';
        c.roles.forEach((role) => chips.appendChild(roleChip(role)));
        metaLine.appendChild(chips);
      }
      body.appendChild(nameLine);
      body.appendChild(metaLine);
      card.appendChild(body);
      card.title = selectedDevice === c.base ? '取消只看该设备' : '只看该设备的串流';
      card.onclick = () => {
        // 按需建立：点设备 = 临时连锚点看其串流（再点一次取消）
        const next = selectedDevice === c.base ? null : c.base;
        selectedDevice = next;
        document.querySelectorAll('#scan-results .scan-card').forEach((el) => el.classList.remove('selected'));
        if (next) card.classList.add('selected');
        card.title = next ? '取消只看该设备' : '只看该设备的串流';
        void scanRemoteStreams(true); // 强制刷新（过滤/取消过滤）
      };
      box.appendChild(card);
    });
  } catch (e) {
    box.innerHTML = '';
    box.appendChild(emptyState('radio', '扫描失败：' + (e as Error).message, true));
  } finally {
    scanInFlight = false;
  }
}

/** 网格页自动发现：扫描局域网设备（mDNS + 手动添加），聚合各设备的在线串流。
 *  点设备卡片后只显示该设备串流；点流卡片 = 按需建立连接（直连锚点，失败自动
 *  经本机级联代理），并跳转「观看（收）」页接收。 */
async function scanRemoteStreams(force = false): Promise<void> {
  if (discoverInFlight) return; // 防并发
  if (!force && discoverCacheAt && Date.now() - discoverCacheAt < DISCOVER_TTL_MS) return;
  discoverInFlight = true;
  const box = $('discover-streams');
  box.innerHTML = '<p class="hint">扫描局域网串流…</p>';
  let relays: RelayInfo[];
  try {
    relays = (await call('scan_relays')) as RelayInfo[];
  } catch (e) {
    box.innerHTML = '';
    box.appendChild(emptyState('radio', '扫描失败：' + (e as Error).message, true));
    discoverInFlight = false;
    return;
  }
  try {
    const others = relays.filter((r) => !r.ip || MY_IPS.indexOf(r.ip) === -1);
    // 手动添加的设备并入聚合（无 mDNS 时也能看到其串流）
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
    if (!others.length) {
      box.innerHTML = '';
      box.appendChild(emptyState('radio', '未发现局域网其它设备（mDNS）。可手动输入地址添加。'));
      return;
    }
    interface Found {
      relayName: string;
      relayBase: string;
      stream: RemoteStream;
      srtUrl: string | null;
      quicUrl: string | null;
    }
    const found: Found[] = [];
    for (const r of others) {
      const base = deviceBase(r);
      if (!base) continue;
      // 传输端口：/api/info（旧版本中继无此端点 → 该设备走 WS）
      let info: { srtPort?: number; quicPort?: number } | null = null;
      try {
        const iresp = await fetch(base + '/api/info', { cache: 'no-store' });
        if (iresp.ok) info = (await iresp.json()) as { srtPort?: number; quicPort?: number };
      } catch (_) { /* 忽略 */ }
      try {
        const sresp = await fetch(base + '/api/streams', { cache: 'no-store' });
        if (!sresp.ok) continue;
        const data = (await sresp.json()) as { streams?: RemoteStream[] } | RemoteStream[];
        const list = Array.isArray(data) ? data : (data.streams || []);
        // SRT/QUIC 是独立 UDP 端口：拨号地址 = srt://<纯主机>:<srt_port>
        // （不能带 http 端口；base 形如 http://ip:8777/）
        const hostOnly = base.replace(/^https?:\/\//, '').replace(/:\d+$/, '');
        for (const st of list) {
          found.push({
            relayName: r.name || r.ip || base,
            relayBase: base,
            stream: st,
            srtUrl: info && info.srtPort ? `srt://${hostOnly}:${info.srtPort}` : null,
            quicUrl: info && info.quicPort ? `quic://${hostOnly}:${info.quicPort}` : null,
          });
        }
      } catch (_) { /* 该设备不可达，跳过 */ }
    }
    // 设备筛选：「点设备只看其流」（selectedDevice = relayBase 键）
    const shown = selectedDevice ? found.filter((f) => f.relayBase === selectedDevice) : found;
    box.innerHTML = '';
    if (!shown.length) {
      box.appendChild(emptyState(
        'radio',
        selectedDevice
          ? '该设备暂无在线串流（或不可达）。再点一次设备卡片取消筛选。'
          : '局域网内暂无在线串流（可手动输入流 id）。',
      ));
      return;
    }
    for (const it of shown) {
      box.appendChild(streamCard({
        title: it.stream.title || it.stream.streamId,
        sub: it.relayName,
        stream: it.stream,
        onPick: (card) => {
          clearCardSelection();
          card.classList.add('selected');
          // 按需建立：目标切到该设备锚点（直连失败自动经本机级联代理）
          targetRelay = { wsBase: it.relayBase.replace(/^http/, 'ws'), srtUrl: it.srtUrl, quicUrl: it.quicUrl };
          remoteStreams.set(it.stream.streamId, it.stream);
          $input('recv-stream-input').value = it.stream.streamId;
          setTab('watch'); // 点流即看：跳转接收页
          void startReceive();
        },
      }));
    }
  } finally {
    discoverInFlight = false;
    discoverCacheAt = Date.now();
  }
}
