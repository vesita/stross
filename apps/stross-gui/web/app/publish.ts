// Stross 前端 —— 发布（出站共享）域（script 全局作用域）：
// 本机把能力共享出去：广播（屏幕/麦克风 → 局域网）与定向（B2 凭证直推设备）。
// 统一入口 startStreamWith(cfg, relayUrl)；一条推流引擎，右栏「共享流」统一呈现与停止。

// ---------------------------------------------------------------------------
// 推流生命周期
// ---------------------------------------------------------------------------

/** 启动共享推流（统一入口）。失败时抛错（调用方负责展示错误）。 */
async function startStreamWith(cfg: StreamConfig, relayUrl: string): Promise<void> {
  if (IS_ANDROID) {
    publishStarting = true;
    publishStartingSince = Date.now();
  }
  const res = (await call('start_stream', { cfg, relayUrl })) as StartResult;
  publishing = true;
  shareKind = cfg.video ? 'screen' : 'mic';
  publishInfo = {
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
  publishing = false;
  publishInfo = null;
  shareKind = null;
  publishStarting = false;
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
      publishStarting = false;
      publishing = false;
      publishInfo = null;
      void renderShares();
      return;
    }
    if (s.started) {
      publishStarting = false;
    } else if (s.error) {
      publishStarting = false;
      showFatal('采集启动失败：' + s.error);
      void stopStream();
      return;
    } else if (publishStarting && Date.now() - publishStartingSince > START_TIMEOUT_MS) {
      publishStarting = false;
      showFatal('采集启动超时（60 秒未就绪）。请停止后重试；若反复超时，请检查系统是否限制后台屏幕录制。');
      void stopStream();
      return;
    }
  } catch (_) { /* ignore */ }
}

/** 状态轮询（应用打开期间常驻）：stream_status → 共享面板。 */
async function pollStatus(): Promise<void> {
  if (IS_ANDROID) {
    if (publishing || publishStarting) {
      // 采集真实状态优先由 pollMobileStatus 回报，这里仅同步流层
      publishStarting = false;
    }
    void renderShares();
    return;
  }
  try {
    const s = (await call('stream_status')) as StreamStatus;
    if (s.running && !publishing) {
      // 外部（CLI / 其它入口）启动的流也反映到共享面板
      publishing = true;
      shareKind = shareKind || 'screen';
      publishInfo = {
        streamId: s.streamId || '',
        title: s.title || '共享',
        startedAt: s.startedAt || Math.floor(Date.now() / 1000),
      };
    } else if (!s.running && publishing) {
      publishing = false;
      publishInfo = null;
      publishStarting = false;
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

// ---------------------------------------------------------------------------
// 广播弹窗（共享屏幕 / 共享麦克风）
// ---------------------------------------------------------------------------

/** 打开「共享屏幕（广播）」弹窗：配置音频（麦克风/系统声）后开始。 */
function openBroadcastScreen(): void {
  const opts = $('share-modal-opts');
  opts.innerHTML = '';
  // 音频选项：麦克风（默认开）+ 系统声音（仅桌面支持回环采集）
  const micCheck = document.createElement('label');
  micCheck.className = 'check';
  micCheck.innerHTML = `<input type="checkbox" id="share-mic" checked />
    <svg class="ic"><use href="#i-mic" /></svg><span>含麦克风${IS_ANDROID ? '（需权限）' : ''}</span>`;
  opts.appendChild(micCheck);
  if (!IS_ANDROID) {
    const sysRow = document.createElement('div');
    sysRow.className = 'row';
    const sysCheck = document.createElement('label');
    sysCheck.className = 'check';
    sysCheck.innerHTML = `<input type="checkbox" id="share-sys" />
      <svg class="ic"><use href="#i-speaker" /></svg><span>含系统声音</span>`;
    sysRow.appendChild(sysCheck);
    const sysSel = document.createElement('select');
    sysSel.id = 'share-sys-dev';
    sysSel.className = 'grow' + (devices.systemAudio.length ? '' : ' hidden');
    if (devices.systemAudio.length) {
      sysSel.innerHTML = devices.systemAudio
        .map((n) => `<option value="${n}">${n}</option>`)
        .join('');
    } else {
      sysSel.appendChild(new Option('未发现回环设备（系统声不可用）', ''));
    }
    sysSel.disabled = true;
    sysRow.appendChild(sysSel);
    opts.appendChild(sysRow);
    sysCheck.querySelector('input')!.addEventListener('change', () => {
      const on = (sysCheck.querySelector('input') as HTMLInputElement).checked;
      sysSel.classList.toggle('hidden', !on || !devices.systemAudio.length);
      sysSel.disabled = !on || !devices.systemAudio.length;
    });
  }
  // 画质（Android 原生编码也走同一配置）
  const qRow = document.createElement('label');
  qRow.textContent = '画质 ';
  const qSel = document.createElement('select');
  qSel.id = 'share-quality';
  qSel.innerHTML = `
    <option value="LOW">低 (640×360 @24fps)</option>
    <option value="MEDIUM" selected>中 (1280×720 @30fps)</option>
    <option value="HIGH">高 (1920×1080 @30fps)</option>`;
  qRow.appendChild(qSel);
  opts.appendChild(qRow);
  const titleRow = document.createElement('label');
  titleRow.textContent = '共享名称 ';
  const titleInput = document.createElement('input');
  titleInput.type = 'text';
  titleInput.id = 'share-title';
  titleInput.value = '我的屏幕';
  titleInput.maxLength = 40;
  titleRow.appendChild(titleInput);
  opts.appendChild(titleRow);
  $('share-modal-title').textContent = '共享屏幕（广播）';
  $('share-modal-sub').textContent = '本机屏幕广播到局域网；其它设备可在其「设备」列表点本机在线共享接收。';
  openShareModal(async () => {
    // 音频：麦克风用系统默认输入（mic=null）；系统声需具体回环设备（桌面）
    const sysDev = !IS_ANDROID && sysOn() && devices.systemAudio.length
      ? $select('share-sys-dev').value.trim() || null
      : null;
    const useMic = micOn();
    const cfg: StreamConfig = {
      streamId: 'stross-' + Date.now().toString(36),
      title: $input('share-title').value.trim() || '我的屏幕',
      video: { kind: 'screen' },
      quality: QUALITIES[$select('share-quality').value],
      audio: useMic || sysDev
        ? { mic: null, systemAudio: sysDev, sampleRate: 48000, channels: 2, bitrateKbps: 128 }
        : null,
      durationSecs: null,
      shareToken: null,
    };
    return cfg;
  });
  $input('share-title').value = localStorage.getItem(LS_TITLE) || '我的屏幕';
}

/** 打开「共享麦克风（广播）」弹窗：纯音频推流（桌面 ffmpeg / Android micOnly）。 */
function openBroadcastMic(): void {
  const opts = $('share-modal-opts');
  opts.innerHTML = '';
  const hint = document.createElement('p');
  hint.className = 'hint';
  hint.textContent = IS_ANDROID
    ? '纯麦克风推流：无需屏幕录制授权，只请求麦克风权限。'
    : '纯音频推流（本机默认输入设备）。';
  opts.appendChild(hint);
  const titleRow = document.createElement('label');
  titleRow.textContent = '共享名称 ';
  const titleInput = document.createElement('input');
  titleInput.type = 'text';
  titleInput.id = 'share-title';
  titleInput.value = '我的麦克风';
  titleInput.maxLength = 40;
  titleRow.appendChild(titleInput);
  opts.appendChild(titleRow);
  $('share-modal-title').textContent = '共享麦克风（广播）';
  $('share-modal-sub').textContent = '把本机麦克风声音广播到局域网；电脑/手机可在其「设备」列表点本机在线共享接收（应用场景：另一台电脑播放手机声音）。';
  openShareModal(async () => {
    const cfg: StreamConfig = {
      streamId: 'stross-' + Date.now().toString(36),
      title: $input('share-title').value.trim() || '我的麦克风',
      video: null, // 纯音频：Android 走 micOnly（跳过屏幕授权）；桌面 ffmpeg 纯音频流
      quality: QUALITIES.LOW,
      audio: { mic: null, systemAudio: null, sampleRate: 48000, channels: 2, bitrateKbps: 128 },
      durationSecs: null,
      shareToken: null,
    };
    return cfg;
  });
  $input('share-title').value = localStorage.getItem(LS_TITLE) || '我的麦克风';
}

function micOn(): boolean {
  const c = document.getElementById('share-mic') as HTMLInputElement | null;
  return !!c && c.checked;
}
function sysOn(): boolean {
  const c = document.getElementById('share-sys') as HTMLInputElement | null;
  return !!c && c.checked;
}

/** 台账状态：当前打开共享弹窗的启动回调（点「开始」时执行并关闭）。 */
let shareModalStarter: (() => Promise<StreamConfig>) | null = null;

function openShareModal(starter: () => Promise<StreamConfig>): void {
  shareModalStarter = starter;
  $('share-status').textContent = '';
  $('share-error').classList.add('hidden');
  $('share-modal').classList.remove('hidden');
}

/** 点「开始」：按弹窗配置启动广播共享（走统一 start_stream 链路）。 */
async function confirmShareModal(): Promise<void> {
  if (!shareModalStarter) return;
  const cfg = await shareModalStarter();
  shareModalStarter = null;
  $('share-modal').classList.add('hidden');
  await startStreamWith(cfg, pushRelayUrl(cfg));
}

function cancelShareModal(): void {
  shareModalStarter = null;
  $('share-modal').classList.add('hidden');
}

/** 推流拨号地址（本机锚点；统一无损优先：QUIC > WS。视频是帧粒度 H.264，
 *  有损路径（SRT）丢一帧即撕裂 GOP → 花屏到下一关键帧；SRT 仅显式选择用）。 */
function pushRelayUrl(_cfg: StreamConfig): string {
  if (!anchor) return '';
  if (anchor.quicUrl) return anchor.quicUrl;
  return `ws://127.0.0.1:${anchor.port}/ws/push`;
}
