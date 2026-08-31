// Stross 前端 —— 订阅（入站接收）域（script 全局作用域）：
// 订阅对端端点 → start_receive_link 接收 → canvas 绘制 / 扬声器播放。
//
// **多端点链接**（通信模式 v2 Phase C「接收端多流化」）：一次可同时订阅
// 多个端点（如屏幕 + 系统声音同播），每条链独立启停/统计，停一条不级联
// 其它链；画布显示最近活跃的视频链路，纯音频链只出声不占画面。
// Android 播放链为单链（Kotlin MediaCodec 插件竞态），订阅新端点会先停旧
// 链路（兼容现状）。

/** 接收结束判定宽限期（ms）：流切换/刚启动时新接收器可能短暂 `!running`
 *  （连接窗口），过早收尾会把 UI 拉回空闲。此窗口内不判定为「流已结束」。 */
const RECV_END_GRACE_MS = 3000;

/** 链路 id（host + endpointId 稳定键：同端点重复订阅复用同链，幂等重启）。 */
function linkIdOf(host: string, endpointId: string): string {
  return host + '/' + endpointId;
}

/** 链路 id → 对端主机（endReceiveLink 刷目录用）。 */
function hostOfLink(linkId: string): string {
  const i = linkId.lastIndexOf('/');
  return i > 0 ? linkId.slice(0, i) : '';
}

/** 当前接收目标中继（点选的局域网设备锚点优先，否则本机锚点；均无则 null）。 */
function currentRelay(): TargetRelay | null {
  if (targetRelay) return targetRelay;
  if (!anchor) return null;
  return {
    wsBase: `ws://127.0.0.1:${anchor.port}`,
    srtUrl: anchor.srtUrl,
    quicUrl: anchor.quicUrl,
  };
}

/** 按流媒体类型自动选传输：统一无损优先（QUIC > WS）。视频是帧粒度 H.264，
 *  有损路径（SRT）丢一帧即撕裂整个 GOP → 花屏直到下一关键帧（最长 2s），
 *  因此默认不走 SRT（SRT 仅显式 `--relay srt://` 场景用）。 */
function autoRelayUrl(stream: RemoteStream | null): string {
  const r = currentRelay();
  if (!r) return '';
  if (r.quicUrl) return r.quicUrl;
  return r.wsBase;
}

/** 链路展示名（设备名 · 端点名；无设备视图时回退主机）。 */
function recvLinkName(host: string, endpointName: string): string {
  const dev = deviceViews.find((d) => d.key && deviceHostOf(d) === host);
  return (dev ? dev.name : host) + ' · ' + endpointName;
}

// ---------------------------------------------------------------------------
// receive-frame 事件路由（全局监听一次；载荷带 linkId，旧单链为 main）
// ---------------------------------------------------------------------------

let recvFrameUnlisten: (() => void) | null = null;

async function ensureRecvFrameListener(): Promise<void> {
  if (recvFrameUnlisten) return;
  recvFrameUnlisten = await listen(
    'receive-frame',
    (p: { linkId?: string; pts: number; width: number; height: number; data: string }) => {
      const linkId = p.linkId || 'main';
      const link = recvLinks.get(linkId);
      if (!link) return; // 已停止链路的迟到帧
      link.frames += 1;
      // 画布显示最近活跃的视频链路（多视频链并存时后者接管画面）
      activeVideoLink = linkId;
      drawReceiveFrame(p.width, p.height, p.data);
      renderRecvLinks();
    },
  );
}

// ---------------------------------------------------------------------------
// 链路启停（多端点链接：互不级联）
// ---------------------------------------------------------------------------

/** 开始接收流并登记为链路（不停止其它链路；同链路重复订阅 = 幂等重启）。
 *  返回是否真正启动（握手成功但接收启动失败时调用方不标记已订阅）。 */
async function startReceiveLink(opts: {
  host: string;
  endpointId: string;
  endpointName: string;
  streamId: string;
}): Promise<boolean> {
  hideRecvError();
  const linkId = linkIdOf(opts.host, opts.endpointId);
  const stream = remoteStreams.get(opts.streamId) || null;
  const relay = autoRelayUrl(stream);
  if (!relay) {
    showRecvError('无可用接收目标（本机锚点未就绪）');
    return false;
  }
  // 同链路重启：先停旧的（Rust 会话 + 前端状态一次清理）
  if (recvLinks.has(linkId)) await stopReceiveLink(linkId);
  let started = false;
  try {
    if (IS_ANDROID) {
      // Android 单链播放：先清前端旧链路，再走旧命令（内部停旧接收并等旧
      // 播放链收尾——防同一 Kotlin 插件上 start/stop 竞态，真机崩溃点）。
      for (const id of [...recvLinks.keys()]) {
        recvLinks.delete(id);
        subscribedEndpoints.delete(id);
      }
      activeVideoLink = null;
      await call('start_receive', { relay, stream: opts.streamId, audio: 'device' });
    } else {
      await call('start_receive_link', { linkId, relay, stream: opts.streamId, audio: 'device' });
    }
    started = true;
    recvLinks.set(linkId, {
      linkId,
      name: recvLinkName(opts.host, opts.endpointName),
      streamId: opts.streamId,
      startedAt: Date.now(),
      frames: 0,
      audioBlocks: 0,
      status: 'starting',
      error: null,
    });
    await ensureRecvFrameListener();
    syncRecvUI();
    renderRecvLinks();
    void pollReceiveLinks();
    return true;
  } catch (e) {
    if (started) {
      try {
        if (IS_ANDROID) {
          await call('stop_receive');
        } else {
          await call('stop_receive_link', { linkId });
        }
      } catch (_) { /* ignore */ }
    }
    showRecvError('接收失败：' + errMsg(e));
    syncRecvUI();
    return false;
  }
}

/** 停止指定链路（其它链路不受影响）。 */
async function stopReceiveLink(linkId: string): Promise<void> {
  try {
    if (IS_ANDROID) {
      // Android 单链：播放链由旧 stop_receive 收尾（Kotlin stopPlayback）
      await call('stop_receive');
    } else {
      await call('stop_receive_link', { linkId });
    }
  } catch (_) { /* ignore */ }
  recvLinks.delete(linkId);
  subscribedEndpoints.delete(linkId);
  if (activeVideoLink === linkId) activeVideoLink = null;
  syncRecvUI();
  renderRecvLinks();
  if (recvLinks.size === 0) {
    // 全部链路停止：清画面 + 退出播放器全屏
    const ctx = canvasCtx();
    if (ctx) ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height);
    void exitPlayerFullscreen();
  }
}

/** 停止全部链路（右栏「停止接收」按钮 / 播放器控制条停止）。 */
async function stopReceive(): Promise<void> {
  const ids = [...recvLinks.keys()];
  for (const id of ids) await stopReceiveLink(id);
  // 无链路时也兜底退出播放器全屏（旧行为：停止接收 = 退出播放器全屏）
  if (ids.length === 0) void exitPlayerFullscreen();
}

// ---------------------------------------------------------------------------
// 面板渲染（链路行 + 空状态 + 状态行）
// ---------------------------------------------------------------------------

function dotClass(status: RecvLinkState['status']): string {
  switch (status) {
    case 'live': return 'live';
    case 'error': return 'err';
    case 'ended': return 'idle';
    default: return 'starting';
  }
}

function statusText(link: RecvLinkState): string {
  switch (link.status) {
    case 'live': return link.frames > 0 ? '接收中' : '音频播放中';
    case 'error': return '错误';
    case 'ended': return '已结束';
    default: return '等待流数据…';
  }
}

/** 重建接收链路行（#recv-links；状态是渲染的纯函数）。 */
function renderRecvLinks(): void {
  const container = $('recv-links');
  container.innerHTML = '';
  for (const link of recvLinks.values()) {
    const row = document.createElement('div');
    row.className = 'recv-link-row';
    const dot = document.createElement('span');
    dot.className = 'dot ' + dotClass(link.status);
    const body = document.createElement('span');
    body.className = 'recv-link-body';
    const name = document.createElement('span');
    name.className = 'recv-link-name';
    name.textContent = link.name;
    const meta = document.createElement('span');
    meta.className = 'meta';
    meta.textContent = link.error
      ? '错误：' + link.error
      : `${statusText(link)} · 收到 ${link.frames} 帧 · 音频 ${link.audioBlocks} 块`;
    body.appendChild(name);
    body.appendChild(meta);
    const stop = document.createElement('button');
    stop.type = 'button';
    stop.className = 'sm danger recv-link-stop';
    stop.innerHTML = icon('stop');
    stop.title = '停止该链路';
    stop.dataset.link = link.linkId;
    row.appendChild(dot);
    row.appendChild(body);
    row.appendChild(stop);
    container.appendChild(row);
  }
  syncRecvUI();
}

/** 同步接收面板外壳：空状态 / 头按钮 / 状态行摘要（链路行由 renderRecvLinks 管）。 */
function syncRecvUI(): void {
  receiving = recvLinks.size > 0;
  const line = $('recv-status-line');
  line.classList.toggle('hidden', !receiving);
  $('recv-dot').className = 'dot ' + (receiving ? 'live' : 'idle');
  const n = recvLinks.size;
  $('recv-status').textContent = receiving ? `接收中（${n} 条链路）` : '未接收';
  $('recv-meta').textContent = '';
  const stopBtn = $('recv-stop-btn');
  if (stopBtn) stopBtn.classList.toggle('hidden', !receiving);
  // 空状态：空闲时显示占位，接收中隐藏（有内容时由画布接管）
  const empty = $('recv-empty');
  if (empty) empty.classList.toggle('hidden', receiving);
  updateRecvOverlay();
}

/** 接收等待浮层：接收中且活跃视频链路既无视频帧也无音频块（纯音频流 B2：
 *  有音频即算有数据）。 */
function updateRecvOverlay(): void {
  const active = activeVideoLink ? recvLinks.get(activeVideoLink) : null;
  const hasFrames = !!active && active.frames > 0;
  $('recv-overlay').classList.toggle(
    'hidden',
    !receiving || hasFrames || (active ? active.audioBlocks > 0 : false),
  );
  // 画布仅在收到视频帧时显示（纯音频链路不占画面区）
  $('recv-canvas-wrap').classList.toggle('hidden', !hasFrames);
}

// ---------------------------------------------------------------------------
// 统计轮询（receive_links：全部链路一次拉取；逐条更新/收尾）
// ---------------------------------------------------------------------------

let recvPollTimer: number | null = null;

async function pollReceiveLinks(): Promise<void> {
  if (recvLinks.size === 0) {
    recvPollTimer = null;
    return;
  }
  try {
    const links = (await call('receive_links')) as ReceiveLinkView[];
    const byId = new Map(links.map((l) => [l.linkId, l]));
    for (const link of recvLinks.values()) {
      const s = byId.get(link.linkId)?.stats;
      if (!s) continue; // 内核侧已无该链（防御；正常路径 stop 才删）
      link.audioBlocks = s.audioBlocks;
      // 曾收到数据后停（帧/解码/音频块）→ 真结束，立即收尾；
      // 从未收到数据（新流连接窗口）→ 给宽限期，仍未运行再判定结束——
      // 否则过早收尾把 UI 拉回空闲，对端端点也回落「订阅」。
      const hadData =
        link.frames > 0 || link.audioBlocks > 0 || s.received > 0 || s.decodedVideo > 0;
      if (s.error) {
        link.status = 'error';
        link.error = s.error;
      } else if (!s.running) {
        if (hadData) {
          void endReceiveLink(link.linkId);
          continue;
        }
        if (Date.now() - link.startedAt < RECV_END_GRACE_MS) {
          link.status = 'starting';
        } else {
          void endReceiveLink(link.linkId);
          continue;
        }
      } else if (link.frames > 0 || s.audioBlocks > 0) {
        link.status = 'live';
      } else {
        link.status = 'starting';
      }
    }
    renderRecvLinks();
  } catch (e) {
    // 轮询失败不中断链路（下轮重试）；留诊断日志便于排查连续失败
    console.warn('[stross] receive_links 轮询失败', e);
  }
  if (recvLinks.size > 0) {
    recvPollTimer = window.setTimeout(() => void pollReceiveLinks(), 1000);
  }
}

/** 流已结束（非错误）的收尾：停止该链路并回到空闲态（仅该链路）。
 *  修复：此前只要绘制过帧就永远显示「进行中」，断流后 UI 卡死。 */
async function endReceiveLink(linkId: string): Promise<void> {
  if (!recvLinks.has(linkId)) return;
  const host = hostOfLink(linkId);
  await stopReceiveLink(linkId);
  // 对端卡片「订阅」键还原（清订阅态），并即时刷下目录——共享方/网络关闭
  // 导致流结束是「关闭共享」事件，订阅方即时响应撤下该端点。
  renderDeviceList();
  const dev = deviceViews.find((d) => d.base && deviceHostOf(d) === host);
  if (dev) void loadRemoteDir(dev, true);
  const ctx = canvasCtx();
  if (ctx) ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height);
}
