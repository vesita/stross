// Stross 前端 —— 接收域：观看连接、解码帧绘制、统计轮询（script 全局作用域）。

/** 当前接收目标中继（点选的局域网设备优先，否则本机锚点；均无则 null）。 */
function currentRelay(): TargetRelay | null {
  if (targetRelay) return targetRelay;
  if (!anchor) return null;
  return {
    wsBase: `ws://127.0.0.1:${anchor.port}`,
    srtUrl: anchor.srtUrl,
    quicUrl: anchor.quicUrl,
  };
}

/** 按流媒体类型自动选传输（auto 模式）：
 *  含视频 → SRT（Adaptive：丢包不阻塞、关键帧自愈）> QUIC > WS
 *  纯音频 → QUIC（无损：音频不可丢）> WS */
function autoRelayUrl(stream: RemoteStream | null): string {
  const r = currentRelay();
  if (!r) return '';
  const hasVideo = !!(stream && stream.video);
  if (hasVideo) {
    if (r.srtUrl) return r.srtUrl;
    if (r.quicUrl) return r.quicUrl;
  } else if (r.quicUrl) {
    return r.quicUrl;
  }
  return r.wsBase;
}

/** 按「接收传输」下拉 + 流媒体类型构造 relay 拨号地址；UDP 端口不可用回退 WS。 */
function pickRelayUrl(stream: RemoteStream | null): string {
  const sel = $select('recv-transport-select').value;
  const r = currentRelay();
  if (!r) return '';
  if (sel === 'srt' && r.srtUrl) return r.srtUrl;
  if (sel === 'quic' && r.quicUrl) return r.quicUrl;
  if (sel === 'auto') return autoRelayUrl(stream);
  if (sel === 'srt' || sel === 'quic') {
    showRecvError(`该中继未提供 ${sel.toUpperCase()} 端口（/api/info 不可用），已回退 WebSocket`);
  }
  return r.wsBase;
}

/** 流类型小标签（视频/音频 chip）。 */
function trackChips(s: RemoteStream): HTMLElement {
  const wrap = document.createElement('span');
  wrap.className = 'chips';
  if (s.video) wrap.appendChild(chipEl('video', '视频'));
  if (s.audio) wrap.appendChild(chipEl('audio', '音频'));
  return wrap;
}

/** 观看人数（眼睛图标 + 数字）。 */
function watcherCount(n: number): HTMLElement {
  const w = document.createElement('span');
  w.className = 'watchers';
  w.innerHTML = icon('eye') + '<span>' + n + ' 人观看</span>';
  return w;
}

/** 接收等待浮层：接收中且尚未收到首帧时显示。 */
function updateRecvOverlay(): void {
  $('recv-overlay').classList.toggle('hidden', !receiving || recvFrameCount > 0);
}

/** 串流卡片（图标 + 名称 + 元信息：流 id/中继名 + 轨道 chip + 观看人数）。 */
function streamCard(o: {
  title: string;
  sub: string;
  stream: RemoteStream;
  onPick: (card: HTMLButtonElement) => void;
}): HTMLButtonElement {
  const card = document.createElement('button');
  card.type = 'button';
  card.className = 'scan-card';
  const ic = document.createElement('span');
  ic.className = 'card-ic';
  ic.innerHTML = icon(o.stream.video ? 'video' : o.stream.audio ? 'music' : 'radio');
  const body = document.createElement('span');
  body.className = 'card-body';
  const name = document.createElement('span');
  name.className = 'scan-name';
  name.textContent = o.title;
  const meta = document.createElement('span');
  meta.className = 'scan-meta';
  meta.appendChild(document.createTextNode(o.sub));
  meta.appendChild(trackChips(o.stream));
  if (o.stream.watchers) meta.appendChild(watcherCount(o.stream.watchers));
  body.appendChild(name);
  body.appendChild(meta);
  card.appendChild(ic);
  card.appendChild(body);
  card.title = '点击接收 ' + o.stream.streamId;
  card.onclick = () => o.onPick(card);
  return card;
}

/** 清空所有串流卡片的选中态。 */
function clearCardSelection(): void {
  document.querySelectorAll('.recv-streams .scan-card').forEach((c) => c.classList.remove('selected'));
}

/** 拉取本机锚点的在线串流列表（GET /api/streams），渲染可选卡片。 */
async function loadRemoteStreams(force = false): Promise<void> {
  const box = $('recv-streams');
  if (!anchor) {
    box.innerHTML = '';
    return;
  }
  // TTL 缓存：3 秒内不重复请求；force（推流后/手动）绕过缓存
  if (!force && streamsCache && Date.now() - streamsCache.at < STREAMS_TTL_MS) {
    box.innerHTML = '';
    for (const s of streamsCache.list) {
      remoteStreams.set(s.streamId, s);
      box.appendChild(streamCard({
        title: s.title || s.streamId,
        sub: s.streamId,
        stream: s,
        onPick: (card) => {
          clearCardSelection();
          card.classList.add('selected');
          targetRelay = null; // 回本机锚点
          remoteStreams.set(s.streamId, s);
          $input('recv-stream-input').value = s.streamId;
          void startReceive();
        },
      }));
    }
    return;
  }
  try {
    const resp = await fetch(`http://127.0.0.1:${anchor.port}/api/streams`, { cache: 'no-store' });
    if (!resp.ok) {
      box.innerHTML = '';
      box.appendChild(emptyState('video', '本机锚点未提供串流列表（HTTP ' + resp.status + '）', true));
      return;
    }
    const data = (await resp.json()) as { streams?: RemoteStream[] } | RemoteStream[];
    const list = Array.isArray(data) ? data : (data.streams || []);
    streamsCache = { at: Date.now(), list };
    box.innerHTML = '';
    if (!list.length) {
      box.appendChild(emptyState('video', '本机锚点暂无在线串流。可先在「推流」页开始推流。'));
      return;
    }
    for (const s of list) {
      remoteStreams.set(s.streamId, s);
      box.appendChild(streamCard({
        title: s.title || s.streamId,
        sub: s.streamId,
        stream: s,
        onPick: (card) => {
          clearCardSelection();
          card.classList.add('selected');
          targetRelay = null; // 回本机锚点
          remoteStreams.set(s.streamId, s);
          $input('recv-stream-input').value = s.streamId;
          void startReceive();
        },
      }));
    }
  } catch (e) {
    box.innerHTML = '';
    box.appendChild(emptyState('video', '拉取串流列表失败：' + (e as Error).message, true));
  }
}

/** 开始原生接收：watch（WS/SRT/QUIC）→ 解码 → canvas 绘制。
 *  目标 = 网格页点选的设备锚点（`targetRelay`）或本机锚点；直连失败自动级联。 */
async function startReceive(): Promise<void> {
  hideRecvError();
  if (!anchor && !targetRelay) {
    showRecvError('本机锚点未就绪且未选择局域网串流。请从「网格」页选择一串流。');
    return;
  }
  const streamId = $input('recv-stream-input').value.trim();
  if (!streamId) {
    showRecvError('请输入流 id，或从上方选择一串流');
    return;
  }
  const btn = $btn('recv-start-btn');
  setBtnLoading(btn, true);
  try {
    const audio = $select('recv-audio-select').value; // 'device' | 'discard'（与 AudioOut serde 一致）
    const stream = remoteStreams.get(streamId) || null; // 流类型（video/audio）供传输自动选择
    const relay = pickRelayUrl(stream); // 按传输选择 + 流媒体类型：ws / srt / quic（UDP 不可用回退）
    if (!relay) {
      showRecvError('无可用接收目标（本机锚点未就绪或未选择局域网串流）');
      return;
    }
    await call('start_receive', {
      relay,
      stream: streamId,
      audio,
    });
    receiving = true;
    recvFrameCount = 0;
    $('recv-status').textContent = '接收中…';
    $('recv-dot').className = 'dot starting';
    $btn('recv-stop-btn').disabled = false;
    setBtnLoading(btn, false);
    btn.disabled = true; // 接收中不可重复开始
    updateRecvOverlay(); // 等待首帧 → 显示浮层
    // 订阅解码帧事件 → canvas
    recvUnlisten = await listen('receive-frame', (p: { pts: number; width: number; height: number; data: number[] }) => {
      drawReceiveFrame(p.width, p.height, p.data);
      recvFrameCount += 1;
      updateRecvOverlay();
    });
    void pollReceiveStatus();
  } catch (e) {
    setBtnLoading(btn, false);
    showRecvError('接收失败：' + (e as Error).message);
    setReceiving(false);
  }
}

/** 停止接收并清空画面。 */
async function stopReceive(): Promise<void> {
  try {
    await call('stop_receive');
  } catch (_) { /* ignore */ }
  if (recvUnlisten) {
    recvUnlisten();
    recvUnlisten = null;
  }
  setReceiving(false);
  const ctx = canvasCtx();
  if (ctx) ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height);
}

function setReceiving(r: boolean): void {
  receiving = r;
  $btn('recv-start-btn').disabled = r;
  $btn('recv-stop-btn').disabled = !r;
  $('recv-dot').className = 'dot ' + (r ? 'live' : 'idle');
  $('recv-status').textContent = r ? '接收中' : '未接收';
  if (!r) $('recv-meta').textContent = '';
  updateRecvOverlay();
}

/** 轮询接收统计（帧数 / 解码 / 音频块）。 */
async function pollReceiveStatus(): Promise<void> {
  if (!receiving) return;
  try {
    const s = (await call('receive_status')) as ReceiveStats;
    if (!s.running && recvFrameCount === 0 && !s.error) {
      $('recv-dot').className = 'dot starting';
      $('recv-status').textContent = '等待流数据…';
    } else if (recvFrameCount > 0 && $('recv-status').textContent === '等待流数据…') {
      // 帧已在绘制（Android 解码在 Kotlin 侧，Rust 的 running 可能滞后）：
      // 翻回接收中，避免状态卡死在「等待流数据」
      $('recv-dot').className = 'dot live';
      $('recv-status').textContent = '接收中';
    }
    $('recv-meta').textContent = s.error
      ? '错误：' + s.error
      : `收到 ${s.received} 帧 · 解码 ${s.decodedVideo} 帧 · 音频 ${s.audioBlocks} 块 · 已绘制 ${recvFrameCount} 帧`;
  } catch (_) { /* ignore */ }
  if (receiving) setTimeout(() => void pollReceiveStatus(), 1000);
}
