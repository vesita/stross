// Stross 前端 —— 共享（出站）域：广播 / 定向推流生命周期与状态轮询。
//
// 共享（出站）统一入口：startStreamWith(cfg, relayUrl)。广播共享锚定本机
// （relayUrl = 本机锚点自动选传输）；定向共享（B2 手机麦克风）经凭证直推
// 对方设备中继（relayUrl = 设备 QUIC/WS）。两者都占用同一推流引擎，
// 由右栏「共享流」面板统一呈现与停止。

/** 启动共享推流（统一入口）。失败时抛错（调用方负责展示错误）。 */
async function startStreamWith(cfg: StreamConfig, relayUrl: string): Promise<void> {
  if (IS_ANDROID) {
    starting = true;
    startingSince = Date.now();
  }
  const res = (await call('start_stream', { cfg, relayUrl })) as StartResult;
  streaming = true;
  shareKind = cfg.video ? 'screen' : 'mic';
  streamInfo = {
    streamId: res.streamId,
    title: cfg.title,
    startedAt: Math.floor(Date.now() / 1000),
  };
  void scanRemoteStreams(true); // 本机在线共享立即出现
  void renderShares();
  if (IS_ANDROID) void pollMobileStatus();
}

/** 停止本机共享（广播 / 定向共用同一推流引擎）。 */
async function stopStream(): Promise<void> {
  try {
    await call('stop_stream');
  } catch (e) {
    showFatal(String(e));
  }
  streaming = false;
  streamInfo = null;
  shareKind = null;
  starting = false;
  if (micShare) {
    micShare.active = false;
    setMicRunning(false);
    $input('mic-token-input').disabled = false;
    $('mic-status').textContent = '已停止';
  }
  void scanRemoteStreams(true);
  void renderShares();
}

/** Android：轮询采集真实状态（Kotlin 控制帧 t=9 回报 → capture_status）。 */
async function pollMobileStatus(): Promise<void> {
  if (!IS_ANDROID) return;
  try {
    const s = (await call('capture_status')) as CaptureStatus;
    if (!s.active) {
      starting = false;
      streaming = false;
      streamInfo = null;
      void renderShares();
      return;
    }
    if (s.started) {
      starting = false;
    } else if (s.error) {
      starting = false;
      showFatal('采集启动失败：' + s.error);
      void stopStream();
      return;
    } else if (starting && Date.now() - startingSince > START_TIMEOUT_MS) {
      starting = false;
      showFatal('采集启动超时（60 秒未就绪）。请停止后重试；若反复超时，请检查系统是否限制后台屏幕录制。');
      void stopStream();
      return;
    }
  } catch (_) { /* ignore */ }
}

/** 状态轮询（应用打开期间常驻）：stream_status → 共享面板。 */
async function pollStatus(): Promise<void> {
  if (IS_ANDROID) {
    if (streaming || starting) {
      // 采集真实状态优先由 pollMobileStatus 回报，这里仅同步流层
      starting = false;
    }
    void renderShares();
    return;
  }
  try {
    const s = (await call('stream_status')) as StreamStatus;
    if (s.running && !streaming) {
      // 外部（CLI / 其它入口）启动的流也反映到共享面板
      streaming = true;
      shareKind = shareKind || 'screen';
      streamInfo = {
        streamId: s.streamId || '',
        title: s.title || '共享',
        startedAt: s.startedAt || Math.floor(Date.now() / 1000),
      };
    } else if (!s.running && streaming) {
      streaming = false;
      streamInfo = null;
      starting = false;
      if (micShare) {
        micShare.active = false;
        setMicRunning(false);
        $input('mic-token-input').disabled = false;
        $('mic-status').textContent = '推流已结束';
      }
    }
    void renderShares();
  } catch (_) { /* ignore */ }
}