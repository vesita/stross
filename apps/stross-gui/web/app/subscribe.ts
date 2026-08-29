// Stross 前端 —— 订阅（入站接收）域（script 全局作用域）：
// 订阅对端端点 → start_receive 接收 → canvas 绘制 / 扬声器播放；
// 接收状态与停止按钮在右栏「接收」面板（电脑端授权手机接入后也走此链路）。

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

/** 开始接收流 `streamId`（调用方已设置目标中继/本机锚点）。
 *  音频固定设备播放（B3）；视频帧 → canvas 绘制，纯音频 → 扬声器。 */
async function startReceive(streamId: string): Promise<void> {
  hideRecvError();
  if (!anchor && !targetRelay) {
    showRecvError('本机锚点未就绪且未选择设备共享。请从「设备」列表选择一条共享。');
    return;
  }
  if (!streamId) {
    showRecvError('缺少流 id');
    return;
  }
  // 防重入：已在接收时先停旧会话（Rust 会话 + 监听器 + 轮询链一次清理），
  // 避免覆盖 recvUnlisten 造成监听器泄漏与 pollReceiveStatus 双链。
  if (receiving) await stopReceive();
  // Rust 接收会话是否已启动：启动后任何接线失败都必须回滚 stop_receive，
  // 否则内核继续收流/发声而前端停止按钮被隐藏（无法停止的泄漏会话）。
  let started = false;
  try {
    const stream = remoteStreams.get(streamId) || null; // 流类型（视频/音频）供传输自动选择
    const relay = autoRelayUrl(stream);
    if (!relay) {
      showRecvError('无可用接收目标（本机锚点未就绪）');
      return;
    }
    await call('start_receive', { relay, stream: streamId, audio: 'device' });
    started = true;
    receiving = true;
    recvFrameCount = 0;
    recvAudioBlocks = 0;
    recvError = null;
    recvStreamId = streamId;
    setReceiving(true);
    // 订阅解码帧事件 → canvas（载荷为 base64 字符串——桌面/Android 统一格式，
    // Rust 侧编码；前端 atob 原生解码）
    recvUnlisten = await listen('receive-frame', (p: { pts: number; width: number; height: number; data: string }) => {
      drawReceiveFrame(p.width, p.height, p.data);
      recvFrameCount += 1;
      updateRecvOverlay();
    });
    void pollReceiveStatus();
  } catch (e) {
    if (started) {
      try { await call('stop_receive'); } catch (_) { /* ignore */ }
    }
    showRecvError('接收失败：' + errMsg(e));
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
  if (!r) {
    recvAudioBlocks = 0;
    recvStreamId = null;
    // 停止接收时退出播放器全屏（若在）
    void exitPlayerFullscreen();
  }
  const line = $('recv-status-line');
  line.classList.toggle('hidden', !r);
  $('recv-dot').className = 'dot ' + (r ? 'live' : 'idle');
  $('recv-status').textContent = r ? '接收中' : '未接收';
  $('recv-meta').textContent = '';
  const stopBtn = $('recv-stop-btn');
  if (stopBtn) stopBtn.classList.toggle('hidden', !r);
  updateRecvOverlay();
}

/** 接收等待浮层：接收中且既无视频帧也无音频块（纯音频流 B2：有音频即算有数据）。 */
function updateRecvOverlay(): void {
  $('recv-overlay').classList.toggle(
    'hidden',
    !receiving || recvFrameCount > 0 || recvAudioBlocks > 0,
  );
  // 画布仅在收到视频帧时显示（纯音频流不占画面区）
  $('recv-canvas-wrap').classList.toggle('hidden', recvFrameCount === 0);
}

/** 轮询接收统计（帧数 / 解码 / 音频块）并同步共享面板。 */
async function pollReceiveStatus(): Promise<void> {
  if (!receiving) return;
  try {
    const s = (await call('receive_status')) as ReceiveStats;
    // await 期间可能已被停止：停止后不再写 DOM（避免过期统计回填已清空的 meta）
    if (!receiving) return;
    recvAudioBlocks = s.audioBlocks;
    if (s.error) recvError = s.error;
    const status = $('recv-status');
    if (s.error) {
      // 连接失败 / 流不存在等：明确错误态
      status.textContent = '错误';
      $('recv-dot').className = 'dot err';
      $('recv-meta').textContent = '错误：' + s.error;
    } else if (!s.running) {
      // 会话不在运行（对方停止 / 中继回收 / 断流 / 未接通）：结束接收会话。
      // 不要求 received>0——`!running && received==0`（从未收到数据）也须
      // 收尾，否则 UI 永久卡「等待流数据…」且轮询链不终止。
      void endReceiveStatus();
      return;
    } else if (recvFrameCount > 0) {
      status.textContent = '接收中';
      $('recv-dot').className = 'dot live';
    } else if (s.audioBlocks > 0) {
      // 纯音频流（B2）：无视频帧，音频块持续增长即视为已接通
      status.textContent = '音频播放中';
      $('recv-dot').className = 'dot live';
      updateRecvOverlay();
    } else {
      status.textContent = '等待流数据…';
      $('recv-dot').className = 'dot starting';
    }
    const pacing = s.pacedDropped > 0 || s.pacedReanchors > 0
      ? ` · 调度 ${s.pacedHeld} 帧等待` +
        (s.pacedDropped > 0 ? ` · 丢 ${s.pacedDropped}` : '') +
        (s.pacedReanchors > 0 ? ` · 重锚 ${s.pacedReanchors}` : '')
      : '';
    $('recv-meta').textContent = s.error
      ? '错误：' + s.error
      : `收到 ${s.received} 帧 · 解码 ${s.decodedVideo} 帧 · 音频 ${s.audioBlocks} 块`
        + (recvFrameCount ? ` · 已绘制 ${recvFrameCount} 帧` : '') + pacing;
  } catch (e) {
    // 轮询失败不中断链路（下轮重试）；留诊断日志便于排查连续失败
    console.warn('[stross] receive_status 轮询失败', e);
  }
  if (receiving) setTimeout(() => void pollReceiveStatus(), 1000);
}

/** 流已结束（非错误）的收尾：停止接收会话并回到空闲态。
 *  修复：此前只要绘制过帧就永远显示「进行中」，断流后 UI 卡死。 */
async function endReceiveStatus(): Promise<void> {
  if (!receiving) return;
  receiving = false;
  recvStreamId = null;
  recvFrameCount = 0;
  recvAudioBlocks = 0;
  if (recvUnlisten) {
    recvUnlisten();
    recvUnlisten = null;
  }
  try {
    await call('stop_receive');
  } catch (_) { /* ignore */ }
  // 复用统一清理：status-line / dot / meta / 画布容器隐藏 / 面板刷新
  setReceiving(false);
  const ctx = canvasCtx();
  if (ctx) ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height);
}