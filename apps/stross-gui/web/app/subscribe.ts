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
// 视频帧显示路径（桌面多链路）：每链路一个二进制 Channel → 只存最新帧 →
// requestAnimationFrame 节流绘制（丢中间帧，显示管线不积压）。
// ---------------------------------------------------------------------------

/** 待绘帧（RAF 消费；只保留最新，到达率高于显示刷新率时丢中间帧）。 */
let pendingFrame: { w: number; h: number; rgba: Uint8Array } | null = null;
let drawLoopStarted = false;

/** 启动绘制循环（全局一次）：每显示帧画一次最新待绘帧。 */
function ensureVideoDrawLoop(): void {
  if (drawLoopStarted) return;
  drawLoopStarted = true;
  const tick = (): void => {
    if (pendingFrame) {
      const f = pendingFrame;
      pendingFrame = null;
      drawReceiveFrame(f.w, f.h, f.rgba);
    }
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
}

// Android 兼容路径：Kotlin MediaCodec 解码帧经 JNI 回 Rust，base64
// `receive-frame` 事件推前端（`mobile_jni.rs`）。同一 RAF 节流绘制。
let recvFrameUnlisten: (() => void) | null = null;

async function ensureRecvFrameListener(): Promise<void> {
  if (recvFrameUnlisten) return;
  recvFrameUnlisten = await listen(
    'receive-frame',
    (p: { linkId?: string; pts: number; width: number; height: number; data: string }) => {
      const link = (p.linkId ? recvLinks.get(p.linkId) : null) || Array.from(recvLinks.values())[0];
      if (!link) return;
      link.frames += 1;
      activeVideoLink = link.linkId;
      const bin = atob(p.data);
      if (bin.length !== p.width * p.height * 4) return;
      const u8 = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
      pendingFrame = { w: p.width, h: p.height, rgba: u8 };
      updateRecvOverlay();
    },
  );
}

/** 二进制帧载荷解析（Rust `pack_frame`：magic "STRF" + w + h + pts 各 u32 LE，后接 RGBA）。 */
function onVideoFrame(linkId: string, payload: Uint8Array): void {
  const link = recvLinks.get(linkId);
  if (!link) return;
  link.frames += 1;
  activeVideoLink = linkId;
  if (payload.length < 16) return;
  const dv = new DataView(payload.buffer, payload.byteOffset, payload.length);
  if (dv.getUint32(0, true) !== 0x53545246) return; // "STRF"
  const w = dv.getUint32(4, true);
  const h = dv.getUint32(8, true);
  pendingFrame = { w, h, rgba: payload.subarray(16) };
}

/** 创建一条链路的二进制帧通道（tauri v2 `core.Channel`；Android 分支 Rust 忽略）。 */
function newFrameChannel(linkId: string): unknown {
  const ChannelCtor = (window as unknown as WindowWithTauri).__TAURI__?.core?.Channel;
  if (!ChannelCtor) throw new Error('当前页面缺少 Tauri Channel 支持');
  const ch = new ChannelCtor();
  ch.onmessage = (payload: Uint8Array) => onVideoFrame(linkId, payload);
  return ch;
}

// ---------------------------------------------------------------------------
// 链路启停（多端点链接：互不级联）
// ---------------------------------------------------------------------------

/** 开始接收流并登记为链路（不停止其它链路；同链路重复订阅 = 幂等重启）。
 *  返回是否真正启动。 */
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
  if (recvLinks.has(linkId)) await stopReceiveLink(linkId);
  let started = false;
  try {
    const onFrame = newFrameChannel(linkId);
    await call('start_receive_link', { linkId, relay, stream: opts.streamId, audio: 'device', onFrame });
    if (IS_ANDROID) {
      await ensureRecvFrameListener();
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
    ensureVideoDrawLoop();
    syncRecvUI();
    renderRecvLinks();
    void pollReceiveLinks();
    // 订阅达成自动切入消费播放台
    switchView('consume');
    const recvPane = $('recv-pane');
    if (recvPane && window.innerWidth <= 900) {
      setTimeout(() => recvPane.scrollIntoView({ behavior: 'smooth', block: 'start' }), 100);
    }
    return true;
  } catch (e) {
    if (started) {
      try {
        await call('stop_receive_link', { linkId });
      } catch {}
    }
    showRecvError('接收失败：' + errMsg(e));
    syncRecvUI();
    return false;
  }
}

/** 停止指定链路（其它链路不受影响）。 */
async function stopReceiveLink(linkId: string): Promise<void> {
  try {
    await call('stop_receive_link', { linkId });
  } catch {}
  recvLinks.delete(linkId);
  subscribedEndpoints.delete(linkId);
  if (activeVideoLink === linkId) activeVideoLink = null;
  syncRecvUI();
  renderRecvLinks();
  if (recvLinks.size === 0) {
    const ctx = canvasCtx();
    if (ctx) ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height);
    void exitPlayerFullscreen();
  }
}

/** 停止全部链路（右栏「停止接收」按钮 / 播放器控制条停止）。 */
async function stopReceive(): Promise<void> {
  const ids = [...recvLinks.keys()];
  for (const id of ids) await stopReceiveLink(id);
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
    const isActive = activeVideoLink === link.linkId;
    row.className = 'recv-link-row' + (isActive ? ' active-stream' : '');
    row.onclick = (e) => {
      if ((e.target as HTMLElement).closest('.recv-link-stop')) return;
      if (link.frames > 0) {
        activeVideoLink = link.linkId;
        renderRecvLinks();
        updateRecvOverlay();
      }
    };

    const dot = document.createElement('span');
    dot.className = 'dot ' + dotClass(link.status);
    const body = document.createElement('span');
    body.className = 'recv-link-body';
    const name = document.createElement('span');
    name.className = 'recv-link-name';
    name.textContent = link.name;
    const meta = document.createElement('span');
    meta.className = 'meta';
    const fpsText =
      (link.displayFps ? ` · 显示 ~${link.displayFps}fps` : '') +
      (link.decodeFps ? ` · 解码 ~${link.decodeFps}fps` : '');
    meta.textContent = link.error
      ? '错误：' + link.error
      : `${statusText(link)} · 收到 ${link.frames} 帧${fpsText} · 音频 ${link.audioBlocks} 块`;
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

/** 同步接收面板外壳：空状态 / 头按钮 / 状态行摘要。 */
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
  // 消费播放台正在播放徽标
  const stageLiveBadge = $('stage-live-badge');
  if (stageLiveBadge) stageLiveBadge.classList.toggle('hidden', !receiving);

  // 全局消费视区徽标更新
  const consumeBadge = $('consume-badge');
  if (consumeBadge) {
    consumeBadge.textContent = String(n);
    consumeBadge.classList.toggle('hidden', n === 0);
  }
  const tabBadge = $('tab-recv-badge');
  if (tabBadge) {
    tabBadge.textContent = String(n);
    tabBadge.classList.toggle('hidden', n === 0);
  }
  // 移动端快速跳转条
  const mobBar = $('mobile-recv-bar');
  if (mobBar) {
    mobBar.classList.toggle('hidden', !receiving);
    if (receiving) {
      const activeLink = activeVideoLink ? recvLinks.get(activeVideoLink) : Array.from(recvLinks.values())[0];
      const txt = $('mobile-recv-text');
      if (txt) {
        txt.textContent = activeLink ? `正在接收：${activeLink.name}` : `正在接收（${recvLinks.size} 条链路）`;
      }
    }
  }
  // 状态机同步
  dispatchUIAction({ type: 'SYNC_LINKS' });
  calcSmartLayout();
  updateRecvOverlay();
}

/** 接收等待浮层。 */
function updateRecvOverlay(): void {
  const active = activeVideoLink ? recvLinks.get(activeVideoLink) : null;
  const hasFrames = !!active && active.frames > 0;
  const hasAudio = Array.from(recvLinks.values()).some((l) => l.audioBlocks > 0);
  $('recv-overlay').classList.toggle(
    'hidden',
    !receiving || hasFrames || (active ? active.audioBlocks > 0 : false),
  );
  $('recv-canvas-wrap').classList.toggle('hidden', !hasFrames);
  showAudioVisualizer(receiving && !hasFrames && hasAudio);
}

/** AI 智能布局与遥测状态计算。 */
function calcSmartLayout(): void {
  const aiBar = $('player-ai-bar');
  if (aiBar) aiBar.classList.toggle('hidden', !receiving);
  if (!receiving) return;

  const totalLinks = recvLinks.size;
  const activeVideo = activeVideoLink ? recvLinks.get(activeVideoLink) : null;
  const hasVideo = !!activeVideo && activeVideo.frames > 0;
  const audioCount = Array.from(recvLinks.values()).filter((l) => l.audioBlocks > 0 || l.name.includes('声音') || l.name.includes('麦克风')).length;

  const label = $('ai-layout-label');
  const tip = $('diag-ai-tip');
  if (label) {
    if (hasVideo && audioCount > 0) {
      label.textContent = 'AI 智能音画混排';
    } else if (hasVideo && totalLinks > 1) {
      label.textContent = 'AI 智能画中画 (PiP)';
    } else if (!hasVideo && audioCount > 0) {
      label.textContent = 'AI 动态音频工作台';
    } else {
      label.textContent = 'AI 极速低延迟视区';
    }
  }
  if (tip) {
    if (hasVideo && audioCount > 0) {
      tip.innerHTML = `<svg class="ic"><use href="#i-sparkles"/></svg><span>AI 优化：已检测到音视频并发链路，视频主视口硬件渲染，音频流已开启低延迟直通与防破音保护。</span>`;
    } else if (!hasVideo && audioCount > 0) {
      tip.innerHTML = `<svg class="ic"><use href="#i-sparkles"/></svg><span>AI 优化：纯音频接收模式，已自动关闭视频显示管线以节省 90% 算力与电池消耗。</span>`;
    } else {
      tip.innerHTML = `<svg class="ic"><use href="#i-sparkles"/></svg><span>AI 优化：局域网传输延迟极低（&lt;20ms），MediaCodec 硬解直通启动，屏幕常亮已激活。</span>`;
    }
  }
}

function updateAITelemetry(fps: number, drops = 0): void {
  const fpsText = $('telemetry-fps-text');
  if (fpsText) fpsText.textContent = `~${fps > 0 ? fps : 60}fps`;
  const badge = $('telemetry-health-badge');
  if (badge) {
    if (drops === 0) {
      badge.className = 'health-ok';
      badge.textContent = '极佳 (0丢包)';
    } else if (drops < 5) {
      badge.className = 'health-warn';
      badge.textContent = `轻微抖动 (${drops}丢包)`;
    } else {
      badge.className = 'health-err';
      badge.textContent = `有丢包 (${drops}丢包)`;
    }
  }
  const diagFps = $('diag-val-fps');
  if (diagFps) diagFps.textContent = `~${fps > 0 ? fps : 60} fps (平稳)`;
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
    const links = await call<ReceiveLinkView[]>('receive_links');
    const byId = new Map(links.map((l) => [l.linkId, l]));
    for (const link of recvLinks.values()) {
      const s = byId.get(link.linkId)?.stats || (IS_ANDROID ? byId.get('main')?.stats : undefined);
      if (!s) continue;
      link.audioBlocks = s.audioBlocks;
      const now = Date.now();
      const dtSec = (now - (link.lastPollAt || now)) / 1000;
      link.lastPollAt = now;
      const dfps =
        dtSec > 0 ? Math.round(((s.decodedVideo as number) - (link.lastDecoded || 0)) / dtSec) : 0;
      const fps = dtSec > 0 ? Math.round((link.frames - (link.lastFrames || 0)) / dtSec) : 0;
      link.lastDecoded = s.decodedVideo;
      link.lastFrames = link.frames;
      const win = (samples: number[] | undefined, v: number): number[] =>
        (samples || []).concat(v).slice(-4);
      const avg = (samples: number[]): number =>
        samples.reduce((a, b) => a + b, 0) / samples.length;
      const fpsWin = win(link.fpsSamples, fps);
      const decodeWin = win(link.decodeSamples, dfps);
      link.fpsSamples = fpsWin;
      link.decodeSamples = decodeWin;
      if (dfps >= 0) link.decodeFps = Math.round(avg(decodeWin));
      if (fps >= 0) link.displayFps = Math.round(avg(fpsWin));
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
    console.warn('[stross] receive_links 轮询失败', e);
  }
  if (recvLinks.size > 0) {
    recvPollTimer = window.setTimeout(() => void pollReceiveLinks(), 1000);
  }
}

/** 流已结束（非错误）的收尾：停止该链路并回到空闲态。 */
async function endReceiveLink(linkId: string): Promise<void> {
  if (!recvLinks.has(linkId)) return;
  const host = hostOfLink(linkId);
  await stopReceiveLink(linkId);
  renderDeviceList();
  const dev = deviceViews.find((d) => d.base && deviceHostOf(d) === host);
  if (dev) void loadRemoteDir(dev, true);
  const ctx = canvasCtx();
  if (ctx) ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height);
}
