// Stross 前端 —— 发现域（script 全局作用域）：
// 本机锚定（start_relay + mDNS 广播）+ 局域网设备扫描 + 手动添加 +
// 设备图渲染（本机卡片 + 设备卡片）。
//
// 分层（docs/layering-architecture.md）：mDNS 浏览 + `/api/info` `/api/streams`
// 探测 + 聚合全部收敛在 Rust（`scan_devices` 命令 → `stross_app::devices::scan`）；
// 本文件只做**渲染与手动地址持久化**，不再自带 fetch 探测客户端。

function normAddr(addr: string): string | null {
  let a = addr.trim();
  if (!a) return null;
  // 自动规范化全角标点与数字（应对中文输入法误输入：。/：/／/全角数字）
  a = a
    .replace(/[。．]/g, '.')
    .replace(/[：]/g, ':')
    .replace(/[／]/g, '/')
    .replace(/[，、]/g, ',')
    .replace(/[\uff10-\uff19]/g, (c) => String.fromCharCode(c.charCodeAt(0) - 0xfee0));
  if (!/^https?:\/\//i.test(a)) a = 'http://' + a;
  return a.replace(/\/+$/, '');
}

/** 局域网设备探测超时（ms；Rust 侧聚合按此探测每台设备）。
 *  缩短以加快节点状态刷新（上线/下线及时可见）；LAN 内 1.5s 足以完成探测。 */
const PROBE_TIMEOUT_MS = 1500;

/** 免先连核心：自动锚定本机（`start_relay` 幂等，启动受控中继 + mDNS 广播）。 */
async function ensureAnchor(): Promise<void> {
  try {
    const info = await call<RelayInfo>('start_relay');
    anchor = {
      port: info.port,
      urls: info.urls,
      srtUrl: null,
      quicUrl: null,
    };
    renderLocalCard(); // 本机卡片状态更新（SRT/QUIC 端口随下一轮扫描到位）
    void refreshDevices(); // 锚点端口 + 本机/对端在线共享随扫描结果到位
  } catch (e) {
    anchor = null;
    const box = $('grid-error');
    box.textContent = '本机锚定失败：' + errMsg(e) + '（仍可接收局域网共享）';
    box.classList.remove('hidden');
    const retry = document.createElement('button');
    retry.type = 'button';
    retry.innerHTML = icon('refresh') + '<span>重试锚定</span>';
    retry.onclick = () => void ensureAnchor();
    box.appendChild(retry);
  }
}

/** 手动添加设备地址（免 mDNS）：探测可达后进入设备列表。
 *  探测走 `probe_relay` 命令（core 官方客户端），前端不再 fetch。 */
async function addManualRelay(): Promise<void> {
  hideGridError();
  const addr = normAddr($input('manual-addr').value);
  if (!addr) {
    showGridError('请输入节点 IP 或 IP:端口（无需 http://），例如 192.168.1.100 或 192.168.1.100:8777');
    return;
  }
  savePrefs();
  saveRecent(addr);
  // 探测中继是否可达（/api/streams 是受控/普通中继都提供的只读端点）
  try {
    const ok = await call<boolean>('probe_relay', { base: addr });
    if (!ok) throw new Error('中继不可达（无 /api/streams）');
  } catch (e) {
    showGridError('无法访问 ' + addr + '：' + errMsg(e));
    return;
  }
  manualRelays = [addr, ...manualRelays.filter((u) => u !== addr)];
  renderRecent();
  void refreshDevices(true); // 设备列表出现该设备（含其在线共享）
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
    const raw = JSON.parse(localStorage.getItem(LS_RECENT) || '[]') as string[];
    const valid: string[] = [];
    const seen = new Set<string>();
    for (const item of raw) {
      const normalized = normAddr(item);
      if (normalized && !seen.has(normalized)) {
        // 过滤掉包含畸形端口或非法字符的测试脏数据
        const hostPort = normalized.replace(/^https?:\/\//, '');
        if (/^[a-zA-Z0-9_.-]+(:\d{1,5})?$/.test(hostPort)) {
          seen.add(normalized);
          valid.push(normalized);
        }
      }
    }
    return valid.slice(0, 5);
  } catch {}
  return [];
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
  const container = $('recent-list');
  container.innerHTML = '';
  list.forEach((u) => {
    const chip = document.createElement('div');
    chip.className = 'recent-chip';
    chip.title = `点击填入并连接：${u}`;

    const main = document.createElement('span');
    main.className = 'recent-chip-label';
    main.textContent = u;
    main.onclick = () => {
      $input('manual-addr').value = u;
      void addManualRelay();
    };

    const del = document.createElement('button');
    del.type = 'button';
    del.className = 'recent-chip-del';
    del.title = '移除此历史地址';
    del.setAttribute('aria-label', '移除 ' + u);
    del.innerHTML = icon('x');
    del.onclick = (e) => {
      e.stopPropagation();
      removeRecent(u);
    };

    chip.appendChild(main);
    chip.appendChild(del);
    container.appendChild(chip);
  });
}

// ---------------------------------------------------------------------------
// 设备列表（左栏）：本机 + 局域网设备
// ---------------------------------------------------------------------------

/** 扫描条目 → 节点卡片基址（http://ip:port）。 */
function baseOf(d: ScannedNode): string {
  return `http://${d.ip}:${d.port}`;
}

/** 全量刷新设备列表 + 锚点端口。
 *
 * mDNS 浏览 + `/api/info` `/api/streams` 探测 + 聚合全部在 Rust
 * `scan_devices` 命令（`stross_app::devices::scan`）；前端只渲染结果，
 * 手动地址通过 `extraBaseUrls` 一并探测并入。
 */
async function refreshDevices(force = false): Promise<void> {
  if (scanInFlight) return;
  if (!force && discoverCacheAt && Date.now() - discoverCacheAt < DISCOVER_TTL_MS) return;
  scanInFlight = true;
  try {
    const devs = await call<ScannedNode[]>('scan_nodes', {
      probeMs: PROBE_TIMEOUT_MS,
      extraBaseUrls: manualRelays.map((a) => a.replace(/\/+$/, '')),
    });
    // 本机条目（isSelf，按回环探测）：同步锚点 SRT/QUIC 端口
    const local = devs.find((d) => d.isSelf) || null;
    if (local && local.online && anchor) {
      anchor.srtUrl = local.srtPort ? `srt://127.0.0.1:${local.srtPort}` : null;
      anchor.quicUrl = local.quicPort ? `quic://127.0.0.1:${local.quicPort}` : null;
    }
    // 远端设备卡片（探测已在 Rust 完成：含在线共享 / SRT / QUIC）
    // 仅保留 `online`（能探测到 /api/info）的设备——剔除离线/已关闭的节点，
    // 避免 mDNS TTL 未到期时手机/PC 列表残留「已关闭的节点」卡片。
    const cards: DeviceView[] = devs
      .filter((d) => !d.isSelf)
      .filter((d) => d.online)
      .map((d) => ({
        key: baseOf(d),
        name: d.name || 'Stross 节点',
        meta: d.ip + ':' + d.port,
        isLocal: false,
        roles: d.roles || [],
        manual: manualRelays.some((a) => a.replace(/\/+$/, '') === baseOf(d)),
        base: baseOf(d),
        srtUrl: d.srtPort ? `srt://${d.ip}:${d.srtPort}` : null,
        quicUrl: d.quicPort ? `quic://${d.ip}:${d.quicPort}` : null,
        quicPort: d.quicPort,
        streams: d.streams || [],
      }));
    // 手动添加但当前不可达的地址保留在列表（提示不可达而非消失）
    manualRelays.forEach((addr) => {
      const base = addr.replace(/\/+$/, '');
      if (!cards.some((c) => c.base === base)) {
        const hostPort = base.replace(/^https?:\/\//, '');
        cards.push({
          key: base,
          name: hostPort + '（手动，不可达）',
          meta: hostPort,
          isLocal: false,
          roles: [],
          manual: true,
          base,
          srtUrl: null,
          quicUrl: null,
          quicPort: null,
          streams: [],
        });
      }
    });
    // 保留已展开状态；本机卡片由渲染器恒置首位
    const keepExpanded = expandedDevice;
    const before = deviceListSignature();
    deviceViews = cards;
    if (keepExpanded && !deviceViews.some((d) => d.key === keepExpanded)) expandedDevice = null;
    // 数据未变则跳过重建——5s 轮询扫描结果相同时整树重绘会导致卡片闪烁
    if (deviceListSignature() !== before) {
      renderDeviceList();
    }
    // 对端目录随扫描周期刷新：正在展开的卡片及时反映对端「新共享/取消共享」
    const expandedDev = expandedDevice
      ? deviceViews.find((d) => d.key === expandedDevice)
      : null;
    if (expandedDev) void loadRemoteDir(expandedDev);
  } catch (e) {
    showGridError('扫描失败：' + errMsg(e));
  } finally {
    scanInFlight = false;
    discoverCacheAt = Date.now();
  }
}

/** 设备列表渲染签名：设备视图 + 锚点端口（数据未变 → 跳过重建，消灭闪烁）。 */
function deviceListSignature(): string {
  return (
    deviceViews
      .map((d) => `${d.key}|${d.name}|${d.meta}|${d.roles.join(',')}|${d.srtUrl ?? ''}|${d.quicUrl ?? ''}`)
      .join(';') +
    '#' +
    `${anchor?.port ?? ''}|${anchor?.srtUrl ?? ''}|${anchor?.quicUrl ?? ''}`
  );
}

/** 兼容入口（初始化 / 强制刷新）。 */
function scanRelays(): Promise<void> {
  return refreshDevices(true);
}

/** 渲染左栏设备列表：各设备卡片（纯净局域网节点，本机已释放至独立 #local-pane）。 */
function renderDeviceList(): void {
  // 保持本机端点管理树实时渲染
  renderLocalDevices();
  const box = $('device-list');
  if (!box) return;
  box.innerHTML = '';
  if (!deviceViews.length) {
    box.appendChild(
      emptyState(
        'wifi',
        '未发现局域网其它节点',
        '请确保节点连接同一 Wi-Fi 并已开启「可被发现」，或在上方手动输入 IP 添加。',
        {
          label: '重新扫描',
          icon: 'refresh',
          onClick: () => void scanRelays(),
        },
      ),
    );
    return;
  }
  const filtered = deviceFilterQuery
    ? deviceViews.filter(
        (d) =>
          d.name.toLowerCase().includes(deviceFilterQuery) ||
          d.meta.toLowerCase().includes(deviceFilterQuery),
      )
    : deviceViews;
  if (deviceFilterQuery && !filtered.length) {
    box.appendChild(
      emptyState(
        'filter',
        '未找到匹配的节点',
        `未找到与「${deviceFilterQuery}」匹配的节点名称或 IP 地址`,
        {
          label: '清除搜索',
          icon: 'x',
          onClick: () => {
            const inp = $input('dev-filter-input');
            if (inp) inp.value = '';
            deviceFilterQuery = '';
            $('dev-filter-clear')?.classList.add('hidden');
            renderDeviceList();
          },
        },
      ),
    );
    return;
  }
  // 同步桌面端分段导航徽标数量
  const segCount = $('seg-discover-count');
  if (segCount) {
    segCount.textContent = String(filtered.length);
    segCount.classList.toggle('hidden', filtered.length === 0);
  }

  // 节点分页控制器（免纵向滑动，一键快速翻页）
  const pager = $('device-pager');
  const pagerInfo = $('pager-info');
  const prevBtn = $btn('pager-prev-btn');
  const nextBtn = $btn('pager-next-btn');

  const pageSize = uiFSM.devicePageSize || 3;
  const totalPages = Math.ceil(filtered.length / pageSize) || 1;
  if (uiFSM.devicePage > totalPages) {
    uiFSM.devicePage = totalPages;
  }
  const startIndex = (uiFSM.devicePage - 1) * pageSize;
  const pageItems = filtered.slice(startIndex, startIndex + pageSize);

  for (const dev of pageItems) {
    box.appendChild(deviceCard(dev));
  }

  if (pager && pagerInfo && prevBtn && nextBtn) {
    if (totalPages > 1) {
      pager.classList.remove('hidden');
      pagerInfo.textContent = `第 ${uiFSM.devicePage} / ${totalPages} 页 (共 ${filtered.length} 节点)`;
      prevBtn.disabled = uiFSM.devicePage <= 1;
      nextBtn.disabled = uiFSM.devicePage >= totalPages;
      prevBtn.onclick = () => {
        if (uiFSM.devicePage > 1) {
          dispatchUIAction({ type: 'SET_DEVICE_PAGE', page: uiFSM.devicePage - 1 });
          renderDeviceList();
        }
      };
      nextBtn.onclick = () => {
        if (uiFSM.devicePage < totalPages) {
          dispatchUIAction({ type: 'SET_DEVICE_PAGE', page: uiFSM.devicePage + 1 });
          renderDeviceList();
        }
      };
    } else {
      pager.classList.add('hidden');
    }
  }
}

/** 局域网设备卡片：点击头部展开 → 对端目录（可订阅端点）+ TA 的在线共享（点流接收）。 */
function deviceCard(dev: DeviceView): HTMLElement {
  const card = document.createElement('div');
  card.className = 'dev-card' + (expandedDevice === dev.key ? ' expanded' : '');
  card.dataset.key = dev.key;

  const head = document.createElement('div');
  head.className = 'dev-head';
  head.setAttribute('role', 'button');
  head.tabIndex = 0;
  const isPhone = /android|phone|手机/i.test(dev.name) || /android/i.test(dev.meta);
  const iconName = dev.manual ? 'link' : isPhone ? 'phone' : 'monitor';
   const ic = document.createElement('span');
   ic.className = 'card-ic';
  const isOnline = !dev.name.includes('不可达');
  ic.innerHTML =
    icon(iconName) +
     `<span class="card-status-dot${isOnline ? '' : ' offline'}"></span>`;
  const body = document.createElement('span');
   body.className = 'card-body';
  const nameLine = document.createElement('span');
  nameLine.className = 'scan-name';
  nameLine.textContent = dev.name;
  const metaLine = document.createElement('span');
  metaLine.className = 'scan-meta';
  metaLine.appendChild(document.createTextNode(dev.meta + (dev.manual ? '（手动）' : '')));
  body.appendChild(nameLine);
  body.appendChild(metaLine);
  const chevron = document.createElement('span');
  chevron.className = 'dev-chevron';
  chevron.innerHTML = icon('chevron-right');
  head.appendChild(ic);
  head.appendChild(body);
  head.appendChild(chevron);
   const toggle = () => {
     expandedDevice = expandedDevice === dev.key ? null : dev.key;
     renderDeviceList();
    if (expandedDevice === dev.key) {
      updateStageForDevice(dev);
    }
   };
  head.addEventListener('click', () => {
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
  // 对端目录（L2：设备 + 可订阅端点；展开时经 endpoint_ls 拉取渲染）
  const dirBox = document.createElement('div');
  dirBox.className = 'dev-dir';
  dirBox.dataset.role = 'remote-dir';
  dirBox.dataset.key = dev.key;
  const dirTitle = document.createElement('h3');
  dirTitle.textContent = '可订阅的内容';
  const dirStatus = document.createElement('div');
  dirStatus.className = 'dir-status hint';
  dirStatus.textContent = '加载中…';
  dirBox.appendChild(dirTitle);
  dirBox.appendChild(dirStatus);
  detail.appendChild(dirBox);
  if (expandedDevice === dev.key) {
    // 展开即拉取对端目录（幂等：缓存命中直接渲染）。
    // 延后一帧：卡片此刻尚未插入 DOM，立即渲染查不到容器。
    setTimeout(() => void loadRemoteDir(dev), 0);
  }
  card.appendChild(detail);
  return card;
}

/** 刷新本机卡片锚点状态行。 */
function renderLocalCard(): void {
  const meta = $('anchor-box');
  if (meta) {
    meta.textContent = anchor ? '已就绪' : '未就绪';
  }
}

/** 当前被选中的远端设备。 */
let selectedDevice: DeviceView | null = null;

function getSelectedDevice(): DeviceView | null {
  return selectedDevice;
}
winObj.getSelectedDevice = getSelectedDevice;

/** 选中节点后同步更新右侧订阅工作台顶栏。 */
function updateStageForDevice(dev: DeviceView): void {
  selectedDevice = dev;
  const title = $('stage-title');
  const sub = $('stage-sub');
  const avatar = $('stage-avatar');
  if (title) title.textContent = `订阅 · ${dev.name}`;
  if (sub) sub.textContent = `${dev.meta} · 在线可订阅接收`;
  if (avatar) {
    const isPhone = /android|phone|手机/i.test(dev.name) || /android/i.test(dev.meta);
    avatar.innerHTML = icon(dev.manual ? 'link' : isPhone ? 'phone' : 'monitor');
  }

}
