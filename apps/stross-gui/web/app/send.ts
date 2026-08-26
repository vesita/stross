// Stross 前端 —— 推流域：采集配置、推流生命周期、状态轮询（script 全局作用域）。

function currentVideoSource(): VideoSource {
  const kind = (document.querySelector('input[name="video"]:checked') as HTMLInputElement).value;
  // 注意：与 Rust 端 VideoSource 的 serde(rename_all="camelCase") 契约一致（小写）
  if (kind === 'screen') return { kind: 'screen' };
  if (kind === 'camera') return { kind: 'camera', device: $select('camera-select').value || null };
  return { kind: 'synthetic', pattern: 'testsrc2' };
}

function buildConfig(): StreamConfig {
  const q = QUALITIES[$select('quality-select').value];
  const micOn = $input('mic-enable').checked;
  const sysOn = $input('sys-enable').checked;
  const audio = micOn || sysOn
    ? {
        mic: micOn ? $select('mic-select').value || null : null,
        systemAudio: sysOn ? $select('sys-select').value || null : null,
        sampleRate: 48000,
        channels: 2,
        bitrateKbps: 128,
      }
    : null;
  return {
    streamId: 'stross-' + Date.now().toString(36),
    title: $input('title-input').value.trim() || '我的串流',
    video: currentVideoSource(),
    quality: q,
    audio,
    durationSecs: null,
  };
}

/** 推流端按媒体类型自动选传输（与接收端 auto 同规则）：
 *  含视频 → SRT（Adaptive：丢包不阻塞、关键帧自愈）> QUIC > WS
 *  纯音频 → QUIC（无损：音频不可丢）> WS
 *  推流锚定本机中继（免先连：无需先连接其它设备）。 */
function pushRelayUrl(cfg: StreamConfig): string {
  if (!anchor) return '';
  const hasVideo = !!cfg.video;
  if (hasVideo) {
    if (anchor.srtUrl) return anchor.srtUrl;
    if (anchor.quicUrl) return anchor.quicUrl;
  } else if (anchor.quicUrl) {
    return anchor.quicUrl;
  }
  return `ws://127.0.0.1:${anchor.port}/ws/push`;
}

/** Android：与桌面统一走 start_stream（cfg 携带画质/音频；原生采集在 Rust 后端适配）。 */
async function startStream(): Promise<void> {
  hideError();
  if (!anchor) {
    showFatal('本机锚点未就绪，无法推流。请到「网格」页查看锚点状态并重试。');
    return;
  }
  savePrefs();
  const btn = $btn('start-btn');
  setBtnLoading(btn, true);
  try {
    if (IS_ANDROID) {
      starting = true;
      startingSince = Date.now();
      setRunning(true, 'starting');
      // Android 原生采集启动需要系统授权，真实状态由 capture_status 轮询回报
    }
    const cfg = buildConfig();
    const res = (await call('start_stream', { cfg, relayUrl: pushRelayUrl(cfg) })) as StartResult;
    renderUrls(res.watchUrls);
    // D4：内核签发流 id —— 预填接收面板，本机可立即原生接收
    $input('recv-stream-input').value = res.streamId || '';
    void loadRemoteStreams(true); // 强制刷新，立即出现新流
    setBtnLoading(btn, false);
    if (IS_ANDROID) {
      void pollMobileStatus(); // 立即查一次真实采集状态
    } else {
      setRunning(true, 'live');
    }
  } catch (e) {
    setBtnLoading(btn, false);
    showFatal(String(e));
    starting = false;
    setRunning(false);
  }
}

async function stopStream(): Promise<void> {
  try {
    await call('stop_stream');
  } catch (e) {
    showFatal(String(e));
  }
  starting = false;
  setRunning(false);
  void loadRemoteStreams(true); // 停止后刷新列表
}

/** Android：轮询采集真实状态（Kotlin 控制帧 t=9 回报 → capture_status）。 */
async function pollMobileStatus(): Promise<void> {
  if (!IS_ANDROID) return;
  try {
    const s = (await call('capture_status')) as CaptureStatus;
    if (!s.active) {
      starting = false;
      setRunning(false);
      return;
    }
    if (s.started) {
      starting = false;
      setRunning(true, 'live');
      return;
    }
    if (s.error) {
      starting = false;
      showFatal('采集启动失败：' + s.error);
      setRunning(false);
      return;
    }
    // 仍在启动中：超时兜底，避免无限"采集中…"
    if (starting && Date.now() - startingSince > START_TIMEOUT_MS) {
      starting = false;
      showFatal('采集启动超时（60 秒未就绪）。请停止后重试；若反复超时，请检查系统是否限制后台屏幕录制。');
      setRunning(false);
      return;
    }
    setRunning(true, 'starting');
  } catch (_) {
    /* ignore */
  }
}

async function pollStatus(): Promise<void> {
  if (IS_ANDROID) {
    // Android 每 2 秒轮询真实采集状态
    if (running || starting) void pollMobileStatus();
    return;
  }
  try {
    const s = (await call('stream_status')) as StreamStatus;
    setRunning(s.running);
    $('stream-meta').textContent = s.running
      ? `「${s.title}」(${s.streamId}) · 已推流 ${fmtElapsed((Date.now() / 1000) - s.startedAt!)} · 中继端口 ${s.relayPort} · 局域网设备可在「网格」页发现并接收`
      : '';
  } catch (_) {
    /* ignore */
  }
}

/** phase: 'idle' | 'starting' | 'live' */
function setRunning(r: boolean, phase: 'idle' | 'starting' | 'live' = r ? 'live' : 'idle'): void {
  running = r;
  const dot = $('status-dot');
  const text = $('status-text');
  $btn('start-btn').disabled = r || starting;
  $btn('stop-btn').disabled = !(r || starting);
  if (phase === 'starting') {
    dot.className = 'dot starting';
    text.textContent = '采集中…';
    $('stream-meta').textContent = '等待系统授权与投影就绪（OPPO 等机型可能需 10~20 秒）';
  } else if (phase === 'live') {
    dot.className = 'dot live';
    text.textContent = '推流中';
    // 明确告知去向（D1：无浏览器观看端，接收走原生播放；P0：推流锚定本机）
    $('stream-meta').textContent = '推流中 · 局域网设备可在「网格」页发现本机流并接收';
  } else {
    dot.className = 'dot idle';
    text.textContent = '未推流';
    $('stream-meta').textContent = '';
  }
}
