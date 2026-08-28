"use strict";
// Stross 前端 —— 订阅（入站接收）域（script 全局作用域）：
// 订阅对端端点 → start_receive 接收 → canvas 绘制 / 扬声器播放；
// 接收状态与停止按钮在右栏「接收」面板（电脑端授权手机接入后也走此链路）。
/** 当前接收目标中继（点选的局域网设备锚点优先，否则本机锚点；均无则 null）。 */
function currentRelay() {
    if (targetRelay)
        return targetRelay;
    if (!anchor)
        return null;
    return {
        wsBase: `ws://127.0.0.1:${anchor.port}`,
        srtUrl: anchor.srtUrl,
        quicUrl: anchor.quicUrl,
    };
}
/** 按流媒体类型自动选传输：统一无损优先（QUIC > WS）。视频是帧粒度 H.264，
 *  有损路径（SRT）丢一帧即撕裂整个 GOP → 花屏直到下一关键帧（最长 2s），
 *  因此默认不走 SRT（SRT 仅显式 `--relay srt://` 场景用）。 */
function autoRelayUrl(stream) {
    const r = currentRelay();
    if (!r)
        return '';
    if (r.quicUrl)
        return r.quicUrl;
    return r.wsBase;
}
/** 开始接收流 `streamId`（调用方已设置目标中继/本机锚点）。
 *  音频固定设备播放（B3）；视频帧 → canvas 绘制，纯音频 → 扬声器。 */
async function startReceive(streamId) {
    hideRecvError();
    if (!anchor && !targetRelay) {
        showRecvError('本机锚点未就绪且未选择设备共享。请从「设备」列表选择一条共享。');
        return;
    }
    if (!streamId) {
        showRecvError('缺少流 id');
        return;
    }
    try {
        const stream = remoteStreams.get(streamId) || null; // 流类型（视频/音频）供传输自动选择
        const relay = autoRelayUrl(stream);
        if (!relay) {
            showRecvError('无可用接收目标（本机锚点未就绪）');
            return;
        }
        await call('start_receive', { relay, stream: streamId, audio: 'device' });
        receiving = true;
        recvFrameCount = 0;
        recvAudioBlocks = 0;
        recvError = null;
        recvStreamId = streamId;
        setReceiving(true);
        // 订阅解码帧事件 → canvas（载荷为 base64 字符串——桌面/Android 统一格式，
        // Rust 侧编码；前端 atob 原生解码）
        recvUnlisten = await listen('receive-frame', (p) => {
            drawReceiveFrame(p.width, p.height, p.data);
            recvFrameCount += 1;
            updateRecvOverlay();
        });
        void pollReceiveStatus();
    }
    catch (e) {
        showRecvError('接收失败：' + e.message);
        setReceiving(false);
    }
}
/** 停止接收并清空画面。 */
async function stopReceive() {
    try {
        await call('stop_receive');
    }
    catch (_) { /* ignore */ }
    if (recvUnlisten) {
        recvUnlisten();
        recvUnlisten = null;
    }
    setReceiving(false);
    const ctx = canvasCtx();
    if (ctx)
        ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height);
}
function setReceiving(r) {
    receiving = r;
    if (!r) {
        recvAudioBlocks = 0;
        recvStreamId = null;
    }
    const line = $('recv-status-line');
    line.classList.toggle('hidden', !r);
    $('recv-dot').className = 'dot ' + (r ? 'live' : 'idle');
    $('recv-status').textContent = r ? '接收中' : '未接收';
    $('recv-meta').textContent = '';
    const stopBtn = $('recv-stop-btn');
    if (stopBtn)
        stopBtn.classList.toggle('hidden', !r);
    updateRecvOverlay();
}
/** 接收等待浮层：接收中且既无视频帧也无音频块（纯音频流 B2：有音频即算有数据）。 */
function updateRecvOverlay() {
    $('recv-overlay').classList.toggle('hidden', !receiving || recvFrameCount > 0 || recvAudioBlocks > 0);
    // 画布仅在收到视频帧时显示（纯音频流不占画面区）
    $('recv-canvas-wrap').classList.toggle('hidden', recvFrameCount === 0);
}
/** 轮询接收统计（帧数 / 解码 / 音频块）并同步共享面板。 */
async function pollReceiveStatus() {
    if (!receiving)
        return;
    try {
        const s = (await call('receive_status'));
        recvAudioBlocks = s.audioBlocks;
        if (s.error)
            recvError = s.error;
        const status = $('recv-status');
        if (s.error) {
            // 连接失败 / 流不存在等：明确错误态
            status.textContent = '错误';
            $('recv-dot').className = 'dot err';
            $('recv-meta').textContent = '错误：' + s.error;
        }
        else if (!s.running && s.received > 0) {
            // 流已自然结束（对方停止 / 中继回收 / 断流，非错误）：清理接收会话
            void endReceiveStatus();
            return;
        }
        else if (recvFrameCount > 0) {
            status.textContent = '接收中';
            $('recv-dot').className = 'dot live';
        }
        else if (s.audioBlocks > 0) {
            // 纯音频流（B2）：无视频帧，音频块持续增长即视为已接通
            status.textContent = '音频播放中';
            $('recv-dot').className = 'dot live';
            updateRecvOverlay();
        }
        else {
            status.textContent = '等待流数据…';
            $('recv-dot').className = 'dot starting';
        }
        $('recv-meta').textContent = s.error
            ? '错误：' + s.error
            : `收到 ${s.received} 帧 · 解码 ${s.decodedVideo} 帧 · 音频 ${s.audioBlocks} 块`
                + (recvFrameCount ? ` · 已绘制 ${recvFrameCount} 帧` : '');
    }
    catch (_) { /* ignore */ }
    if (receiving)
        setTimeout(() => void pollReceiveStatus(), 1000);
}
/** 流已结束（非错误）的收尾：停止接收会话并回到空闲态。
 *  修复：此前只要绘制过帧就永远显示「进行中」，断流后 UI 卡死。 */
async function endReceiveStatus() {
    if (!receiving)
        return;
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
    }
    catch (_) { /* ignore */ }
    // 复用统一清理：status-line / dot / meta / 画布容器隐藏 / 面板刷新
    setReceiving(false);
    const ctx = canvasCtx();
    if (ctx)
        ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height);
}
// ---------------------------------------------------------------------------
// 电脑端授权手机接入：轮询等待流出现，出现即自动开始接收（B3）
// ---------------------------------------------------------------------------
/** 统一入口：电脑端已授权某流 id，开始轮询等待其接入，出现即自动订阅。
 *  供「协商允许」路径使用（Confirm 可见性端点订阅的人工确认）。 */
function beginAwaitMicStream(streamId) {
    micRecv = { streamId, checking: false, received: false };
    void pollMicRecv();
}
/** 到期时间倒计时文案（Unix 秒 → "约 N 分钟"）。 */
function fmtSecs(expiresAt) {
    const mins = Math.max(1, Math.round((expiresAt - Date.now() / 1000) / 60));
    return `约 ${mins} 分钟`;
}
/** 轮询本机受控中继串流列表：凭证对应的流接入后自动开始原生接收。
 *  列表走 `anchor_streams` 命令（core 官方客户端），不再直接 fetch。 */
async function pollMicRecv() {
    if (!micRecv || micRecv.checking || micRecv.received)
        return;
    micRecv.checking = true;
    try {
        if (anchor) {
            const list = (await call('anchor_streams', { port: anchor.port }));
            if (list.some((s) => s.streamId === micRecv.streamId)) {
                micRecv.received = true;
                // 自动原生接收（音频设备输出；纯音频流无画面属正常）
                void startReceive(micRecv.streamId);
                return;
            }
        }
    }
    catch (_) { /* 中继短暂不可达，下一轮重试 */ }
    micRecv.checking = false;
    if (micRecv)
        setTimeout(() => void pollMicRecv(), 2000);
}
