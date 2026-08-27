"use strict";
// Stross 前端 —— 协商与定向推流域（script 全局作用域）：
// B2 反向外设（手机麦克风 → 电脑）凭证式接入 + B2.5 凭证自动协商（免粘贴）。
// 设备「共享麦克风到 TA」→ 优先自动申请凭证，失败回退手动粘贴。
/** 由设备 http://host:port 基址构造推流拨号地址：QUIC 可用（扫描带的端口）
 *  优先（纯音频无损），否则回退 ws://host:port/ws/push。 */
function pushUrlForDevice(base, quicPort) {
    const hostPort = base.replace(/^https?:\/\//, '').replace(/\/+$/, '');
    const idx = hostPort.lastIndexOf(':');
    const host = idx > 0 ? hostPort.slice(0, idx) : hostPort;
    if (quicPort)
        return `quic://${host}:${quicPort}`;
    return `ws://${hostPort}/ws/push`;
}
/** 从设备基址拆出 host（http://192.168.1.5:18777 → 192.168.1.5）。 */
function hostOf(base) {
    const hostPort = base.replace(/^https?:\/\//, '').replace(/\/+$/, '');
    const idx = hostPort.lastIndexOf(':');
    return idx > 0 ? hostPort.slice(0, idx) : hostPort;
}
/** 向目标设备自动申请麦克风接入凭证（权限自动化：首次需对方人工允许，之后信任免问）。
 *  握手在 Rust 命令 `request_share_token`（`stross_app::request_grant`）内完成——
 *  前端不再手写 18779 HTTP 客户端（docs/layering-architecture.md）。 */
async function autoNegotiateMic(dev) {
    if (!dev.base)
        return { ok: false, error: '设备基址不可用' };
    const host = hostOf(dev.base);
    try {
        const grant = (await call('request_share_token', {
            host,
            port: NEGOTIATOR_PORT,
            media: ['mic'],
        }));
        if (!grant.token || !grant.streamId) {
            return { ok: false, error: '协商响应缺少凭证字段' };
        }
        return { ok: true, token: grant.token, streamId: grant.streamId };
    }
    catch (e) {
        return {
            ok: false,
            error: '等待对方确认超时或无法连接协商端点：' + e.message,
        };
    }
}
/** 打开「共享麦克风」弹窗（手机/PC 端对目标设备）：优先自动协商免粘贴，失败回退手动。 */
async function openMicShare(dev) {
    if (dev.base == null)
        return;
    // QUIC 端口由扫描聚合带出（不再 fetch /api/info）
    micShare = { base: dev.base, quicPort: dev.quicPort ?? null, active: false };
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
