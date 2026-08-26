"use strict";
// Stross 前端 —— 协商与定向推流域（script 全局作用域）：
// B2 反向外设（手机麦克风 → 电脑）凭证式接入 + B2.5 凭证自动协商（免粘贴）。
// 设备「共享麦克风到 TA」→ 优先自动申请凭证，失败回退手动粘贴。
/** 由设备 http://host:port 基址构造推流拨号地址：QUIC 可用（/api/info）
 *  优先（纯音频无损），否则回退 ws://host:port/ws/push。 */
function pushUrlForDevice(base, quicPort) {
    const hostPort = base.replace(/^https?:\/\//, '').replace(/\/+$/, '');
    const idx = hostPort.lastIndexOf(':');
    const host = idx > 0 ? hostPort.slice(0, idx) : hostPort;
    if (quicPort)
        return `quic://${host}:${quicPort}`;
    return `ws://${hostPort}/ws/push`;
}
/** 设备协商端点基址（http://host:18779；与 Rust 协商端口一致）。 */
function negotiatorBase(base) {
    try {
        const u = new URL(base);
        u.port = String(NEGOTIATOR_PORT);
        u.pathname = '/';
        u.search = '';
        u.hash = '';
        return u.toString().replace(/\/+$/, '');
    }
    catch {
        return null;
    }
}
/** 向目标设备自动申请麦克风接入凭证（权限自动化：首次需对方人工允许，之后信任免问）。 */
async function autoNegotiateMic(dev) {
    if (!dev.base)
        return { ok: false, error: '设备基址不可用' };
    const negBase = negotiatorBase(dev.base);
    if (!negBase)
        return { ok: false, error: '设备基址解析失败' };
    let ident;
    try {
        ident = (await call('device_identity'));
    }
    catch (e) {
        return { ok: false, error: '无法读取本机身份：' + e.message };
    }
    // 客户端超时 15s（服务器侧挂起 60s 等人工确认；超过说明对方没响应/未就绪）
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), 15000);
    try {
        const resp = await fetch(negBase + '/api/negotiator/request', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                deviceId: ident.deviceId,
                deviceName: ident.deviceName,
                media: ['mic'],
            }),
            signal: ctrl.signal,
        });
        if (!resp.ok) {
            const err = (await resp.json().catch(() => null));
            return { ok: false, error: (err && err.error) || `协商失败（HTTP ${resp.status}）` };
        }
        const grant = (await resp.json());
        if (!grant.token || !grant.streamId) {
            return { ok: false, error: '协商响应缺少凭证字段' };
        }
        return { ok: true, token: grant.token, streamId: grant.streamId };
    }
    catch (e) {
        if (ctrl.signal.aborted)
            return { ok: false, error: '等待对方确认超时（15s）' };
        return { ok: false, error: '无法连接设备协商端点：' + e.message };
    }
    finally {
        clearTimeout(timer);
    }
}
/** 打开「共享麦克风」弹窗（手机/PC 端对目标设备）：优先自动协商免粘贴，失败回退手动。 */
async function openMicShare(dev) {
    if (dev.base == null)
        return;
    micShare = { base: dev.base, quicPort: null, active: false };
    // 拉取对端 /api/info 的 QUIC 端口（旧版本中继无此端点 → 走 WS）
    try {
        const iresp = await fetch(dev.base.replace(/\/+$/, '') + '/api/info', { cache: 'no-store' });
        if (iresp.ok) {
            const info = (await iresp.json());
            micShare.quicPort = info.quicPort || null;
        }
    }
    catch (_) { /* QUIC 不可用 */ }
    $('mic-modal-device').textContent = `推送到 ${dev.name}（${dev.meta}）`;
    if (micShareLastBase === dev.base && publishing) {
        // 正是推往该设备的定向共享（重开弹窗）：恢复进行中状态，停止按钮可用
        micShare.active = true;
        setMicRunning(true);
        $input('mic-token-input').disabled = true;
        $('mic-status').textContent = '推流中（凭证已出示）…';
    }
    else {
        // 优先自动协商：向设备申请凭证，成功直接推流（首次共享对方电脑会弹「允许」）
        const r = await autoNegotiateMic(dev);
        if (r.ok && r.token && r.streamId && micShare) {
            $input('mic-token-input').value = '';
            $('mic-error').classList.add('hidden');
            try {
                await startMicShareWith({
                    token: r.token,
                    streamId: r.streamId,
                    base: micShare.base,
                    quicPort: micShare.quicPort,
                });
                $('mic-status').textContent = '已自动获取凭证，推流中…（首次共享需对方电脑点「允许」）';
            }
            catch (_) { /* 错误已显示在弹窗 */ }
            setMicRunning(true);
        }
        else {
            // 自动协商失败 → 回退手动粘贴
            $input('mic-token-input').value = '';
            $input('mic-token-input').placeholder = '粘贴电脑端「接收手机麦克风」展示的接入凭证';
            $('mic-status').textContent =
                '自动协商未成功（' + (r.error || '未知原因') + '），可粘贴凭证，或点「自动获取凭证」重试';
            $('mic-error').classList.add('hidden');
            setMicRunning(false);
            $input('mic-token-input').disabled = false;
        }
    }
    $('mic-modal').classList.remove('hidden');
    $input('mic-token-input').focus();
}
/** 解析凭证并开始推流：stream_id 用接收端签发的 id，目标 = 该设备中继。 */
async function startMicShare() {
    const tokenStr = $input('mic-token-input').value.trim();
    const errBox = $('mic-error');
    errBox.classList.add('hidden');
    if (!tokenStr) {
        errBox.textContent = '请粘贴电脑端展示的接入凭证';
        errBox.classList.remove('hidden');
        return;
    }
    let parsed;
    try {
        parsed = JSON.parse(tokenStr);
    }
    catch {
        errBox.textContent = '凭证不是合法的 JSON（应整体复制电脑端凭证）';
        errBox.classList.remove('hidden');
        return;
    }
    if (!parsed.streamId) {
        errBox.textContent = '凭证缺少 streamId，可能已损坏';
        errBox.classList.remove('hidden');
        return;
    }
    if (!micShare)
        return;
    await startMicShareWith({
        token: tokenStr,
        streamId: parsed.streamId,
        base: micShare.base,
        quicPort: micShare.quicPort,
    });
}
/** 用已获取的凭证推流到目标设备中继（自动协商与手动粘贴共用入口）。 */
async function startMicShareWith(p) {
    const relayUrl = pushUrlForDevice(p.base, p.quicPort);
    $('mic-status').textContent = '连接 ' + relayUrl + ' …';
    try {
        const cfg = {
            streamId: p.streamId,
            title: '手机麦克风',
            video: null, // 纯音频：Android 采集跳过屏幕授权，只采麦克风
            quality: QUALITIES.LOW,
            audio: { mic: null, systemAudio: null, sampleRate: 48000, channels: 2, bitrateKbps: 128 },
            durationSecs: null,
            shareToken: p.token,
        };
        await call('start_stream', { cfg, relayUrl });
        if (micShare)
            micShare.active = true;
        micShareLastBase = p.base;
        setMicRunning(true);
        $input('mic-token-input').disabled = true;
        $('mic-status').textContent = '推流中（已出示凭证）…';
        void renderShares();
        void pollMicShareStatus();
    }
    catch (e) {
        const errBox = $('mic-error');
        errBox.textContent = '推流启动失败：' + e.message;
        errBox.classList.remove('hidden');
        $('mic-status').textContent = '';
        throw e; // 让调用方（自动协商）得知失败
    }
}
async function stopMicShare() {
    try {
        await call('stop_stream');
    }
    catch (_) { /* ignore */ }
    if (micShare)
        micShare.active = false;
    setMicRunning(false);
    $input('mic-token-input').disabled = false;
    $('mic-status').textContent = '已停止';
}
/** 共享麦克风实时状态（stream_status 常驻轮询之外补充采集真实状态）。 */
async function pollMicShareStatus() {
    if (!micShare || !micShare.active)
        return;
    // capture_status 反映 Android 原生采集真实状态（micOnly 授权失败会回传错误）
    if (IS_ANDROID) {
        try {
            const cs = (await call('capture_status'));
            if (cs.error) {
                micShare.active = false;
                setMicRunning(false);
                $('mic-error').textContent = '采集失败：' + cs.error;
                $('mic-error').classList.remove('hidden');
                $input('mic-token-input').disabled = false;
                return;
            }
            $('mic-status').textContent = cs.started ? '麦克风采集中，推流中…' : '等待麦克风授权…';
        }
        catch (_) { /* ignore */ }
    }
    const st = (await call('stream_status').catch(() => null));
    if (st && !st.running) {
        if (micShare)
            micShare.active = false;
        setMicRunning(false);
        $input('mic-token-input').disabled = false;
        $('mic-status').textContent = '推流已结束';
        return;
    }
    setTimeout(() => void pollMicShareStatus(), 2000);
}
function setMicRunning(r) {
    $btn('mic-start-btn').disabled = r;
    $btn('mic-stop-btn').disabled = !r;
}
// ---------------------------------------------------------------------------
