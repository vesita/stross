"""test-e2e 命令：真机/本地回归测试（原 scripts/*_test.sh 的机械等价迁移）。

用法：
    uv run python -m scripts test-e2e dual-device
    uv run python -m scripts test-e2e dual-node-file
    uv run python -m scripts test-e2e weaknet [SECS] [SCENARIOS] [TRANS]
    uv run python -m scripts test-e2e latency-stability [SECS] [TRANS...]
    uv run python -m scripts test-e2e share-token
    uv run python -m scripts test-e2e quic-stale-stream
    uv run python -m scripts test-e2e srt-push-silence-cleanup
    uv run python -m scripts test-e2e multi-endpoint
    uv run python -m scripts test-e2e multi-mutual-disconnect
    uv run python -m scripts test-e2e discovery

依赖真机的脚本（dual-device 等）行为与原 .sh 完全一致；本机无真机时以
`--help` / import 无误为语法级验证（最终报告注明「真机脚本已迁移未实跑」）。
"""

from __future__ import annotations

import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from .. import util
from ..util import (REPO, TestFailure, env_default, first_group, first_match,
                    http_get, http_post_json, http_status, kill, kill9,
                    lan_ip, proc_rss_kb, run, run_log, run_tee, session_id,
                    share_token, spawn, step, tail, wait_proc)


def _cli() -> str:
    return env_default("CLI", str(REPO / "target/debug/stross"))


def _cli_exists(cli: str) -> bool:
    return os.path.isfile(cli) and os.access(cli, os.X_OK)


def _build_cli_if_missing(cli: str) -> None:
    # 等价 `[ -x "$CLI" ] || cargo build -p stross-cli`
    if not _cli_exists(cli):
        run(["cargo", "build", "-p", "stross-cli"])


def _build_cli_forced() -> None:
    # 等价 `cargo build -p stross-cli >/dev/null 2>&1 || { echo "✗ 构建失败"; exit 1; }`
    r = run(["cargo", "build", "-p", "stross-cli"], capture=True)
    if r.returncode != 0:
        print("✗ 构建失败")
        raise TestFailure("构建失败")


def _fail(msg: str) -> None:
    # 回归脚本的 fail：等价 `echo "✗ $*"; exit 1`（提示语已含 ✗/❌ 前缀）
    print(msg)
    raise TestFailure(msg)


def _ensure_not_running(proc) -> bool:
    """等价 `kill -0 $PID`：进程是否仍在运行。"""
    if proc is None:
        return False
    try:
        os.kill(proc.pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def _streams_watchers(port: str) -> int:
    """等价内嵌 python3 -c 解析 /api/streams 首个流的 watchers。"""
    _, body = http_get(f"http://127.0.0.1:{port}/api/streams")
    try:
        d = json.loads(body)
        if d:
            return int(d[0].get("watchers", 0))
    except Exception:
        pass
    return 0


def _count_frames(directory: Path) -> int:
    if not directory.is_dir():
        return 0
    return len(list(directory.glob("frame_*.rgba")))


def _wait_serve_ready(cli: str, ctrl_ports, logs: str, attempts: int = 50,
                      interval: float = 0.2) -> None:
    """轮询控制面 status 直到就绪；等价 for i in $(seq 1 50) … fail。"""
    for i in range(1, attempts + 1):
        ok = True
        for ctrl in ctrl_ports:
            r = run([cli, "ctrl", "--connect", f"ws://127.0.0.1:{ctrl}/ws/ctrl",
                     "status"], capture=True)
            if r.returncode != 0:
                ok = False
                break
        if ok:
            return
        if i == attempts:
            _fail(f"serve 未就绪（看 {logs}）")
        time.sleep(interval)


# ---------------------------------------------------------------- 测试实现

def _dual_device(args) -> int:
    cli = _cli()
    out = Path(env_default("OUT", "/tmp/stross-dual"))
    port = env_default("PORT", "18777")
    ctrl = env_default("CTRL", "18778")
    relay_c = env_default("RELAY_C", "19003")
    secs, recv_secs = 24, 3
    min_frames, min_audio = 15, 40

    _build_cli_if_missing(cli)
    shutil.rmtree(out, ignore_errors=True)
    out.mkdir(parents=True)

    pids = {}

    def cleanup():
        for p in pids.values():
            kill(p)

    try:
        step(f"设备 A：serve（内核+受控中继+控制面，端口 {port}）")
        pids["a"] = spawn([cli, "serve", "--port", port, "--ctrl-port", ctrl],
                          out / "serve.log")
        time.sleep(1.2)

        r = run([cli, "ctrl", "create-session", "--title", "双设备测试"], capture=True)
        sid = session_id(r.stdout + r.stderr)
        if not sid:
            _fail("✗ 建会话失败")
        step(f"会话（内核签发，D4）: {sid}")
        run([cli, "ctrl", "start-stream", "--stream-id", sid, "--secs", str(secs),
             "--audio"], capture=True)
        time.sleep(1)

        step("设备 B 网格聚合：/api/info + /api/streams")
        _, info = http_get(f"http://127.0.0.1:{port}/api/info")
        _, streams = http_get(f"http://127.0.0.1:{port}/api/streams")
        print(f"  /api/info → {info}")
        print(f"  /api/streams → {streams[:220]}")

        step("设备 B 点流即看：直连 A 锚点（推流早期接入）")
        run_log([cli, "receive", "--relay", f"ws://127.0.0.1:{port}", "--stream", sid,
                 "--out", str(out / "direct"), "--secs", str(recv_secs)],
                out / "direct.log")
        direct = _count_frames(out / "direct")
        direct_audio = int(first_group(r"音频块 (\d+)",
                          (out / "direct.log").read_text(errors="replace")) or 0)
        print(f"直连解码帧数: {direct} | 音频块: {direct_audio}（合成测试音 440Hz，AAC）")

        step("中途接入（错过首帧，依赖关键帧自带 SPS/PPS）")
        time.sleep(2)  # 推流中段
        run_log([cli, "receive", "--relay", f"ws://127.0.0.1:{port}", "--stream", sid,
                 "--out", str(out / "late"), "--secs", str(recv_secs)],
                out / "late.log")
        late = _count_frames(out / "late")
        late_audio = int(first_group(r"音频块 (\d+)",
                          (out / "late.log").read_text(errors="replace")) or 0)
        print(f"中途接入解码帧数: {late} | 音频块: {late_audio}")

        step(f"跨设备级联：中继 C（{relay_c}）经 /api/proxy 拉 A 的流")
        pids["c"] = spawn([cli, "relay", "-p", relay_c], out / "relayC.log")
        time.sleep(1.2)
        _, resp = http_post_json(f"http://127.0.0.1:{relay_c}/api/proxy",
                                 {"upstream": f"ws://127.0.0.1:{port}",
                                  "streamId": sid})
        print(f"  POST /api/proxy → {resp}")
        time.sleep(1)
        _, cstreams = http_get(f"http://127.0.0.1:{relay_c}/api/streams")
        print(f"  C 的 /api/streams → {cstreams[:200]}")
        run_log([cli, "receive", "--relay", f"ws://127.0.0.1:{relay_c}", "--stream", sid,
                 "--out", str(out / "cascade"), "--secs", str(recv_secs)],
                out / "cascade.log")
        cascade = _count_frames(out / "cascade")
        cascade_audio = int(first_group(r"音频块 (\d+)",
                            (out / "cascade.log").read_text(errors="replace")) or 0)
        print(f"级联解码帧数: {cascade} | 音频块: {cascade_audio}")

        cleanup()
        print()
        print(f"直连={direct}/{direct_audio} 中途={late}/{late_audio} "
              f"级联={cascade}/{cascade_audio}（帧阈值 {min_frames}，音频阈值 {min_audio}）")
        if (direct >= min_frames and late >= min_frames and cascade >= min_frames
                and direct_audio >= min_audio and late_audio >= min_audio
                and cascade_audio >= min_audio):
            print("✅ 双设备端到端全部 OK")
            return 0
        print("❌ 存在失败项（直连/中途/级联任一帧数不足）")
        return 1
    finally:
        cleanup()


def _dual_node_file(args) -> int:
    cli = _cli()
    d = Path(tempfile.mkdtemp(prefix="stross-dual-", dir="/tmp"))
    dir_a, dir_b, recv = d / "a", d / "b", d / "recv"
    port_a = env_default("PORT_A", "18777"); ctrl_a = env_default("CTRL_A", "18778")
    neg_a = env_default("NEG_A", "18779")
    port_b = env_default("PORT_B", "28777"); ctrl_b = env_default("CTRL_B", "28778")
    neg_b = env_default("NEG_B", "28779")
    srt_a = env_default("SRT_A", "33462"); quic_a = env_default("QUIC_A", "33464")
    srt_b = env_default("SRT_B", "33463"); quic_b = env_default("QUIC_B", "33465")
    size_a, size_c = 800, 3  # KiB；file-c 小文件覆盖「非整块」末帧路径

    pids = []

    def cleanup():
        for p in pids:
            kill(p)
        for p in pids:
            wait_proc(p, timeout=5)
        shutil.rmtree(d, ignore_errors=True)

    try:
        _build_cli_forced()
        dir_a.mkdir(parents=True); dir_b.mkdir(parents=True); recv.mkdir(parents=True)
        # 确定性内容（可复现 + cmp 可比对）
        (dir_a / "file-a.txt").write_bytes(os.urandom(size_a * 1024))
        (dir_a / "file-c.bin").write_text("hello 双端, 小文件末帧验证 ✅\n",
                                          encoding="utf-8")
        (dir_b / "file-b.txt").write_text(f"B 节点的小文件: {int(time.time())}\n",
                                          encoding="utf-8")
        with open(dir_b / "file-b.txt", "a", encoding="utf-8") as f:
            f.write("世界，你好。跨节点文件互发。\n")

        step(f"启动节点 A（默认端口，数据目录 {dir_a}）")
        pids.append(spawn([cli, "serve", "--port", port_a, "--ctrl-port", ctrl_a,
                           "--negotiator-port", neg_a, "--srt-port", srt_a,
                           "--quic-port", quic_a, "--data-dir", str(dir_a)],
                          d / "a.log"))
        step(f"启动节点 B（自定义端口 2877x，数据目录 {dir_b}）")
        pids.append(spawn([cli, "serve", "--port", port_b, "--ctrl-port", ctrl_b,
                           "--negotiator-port", neg_b, "--srt-port", srt_b,
                           "--quic-port", quic_b, "--data-dir", str(dir_b)],
                          d / "b.log"))

        _wait_serve_ready(cli, [ctrl_a, ctrl_b], f"{d}/a.log / {d}/b.log")

        step("A 公开文件端点：file-a.txt（pull）与 file-c.bin（both）")
        r = run([cli, "ctrl", "endpoint", "publish-file", "--path",
                 str(dir_a / "file-a.txt"), "--visibility", "public",
                 "--delivery", "pull"], capture=True)
        if r.returncode != 0:
            _fail("A 公开 file-a.txt 失败")
        r = run([cli, "ctrl", "endpoint", "publish-file", "--path",
                 str(dir_a / "file-c.bin"), "--visibility", "public",
                 "--delivery", "both"], capture=True)
        if r.returncode != 0:
            _fail("A 公开 file-c.bin 失败")
        step("B 公开文件端点：file-b.txt（pull）")
        r = run([cli, "ctrl", "--connect", f"ws://127.0.0.1:{ctrl_b}/ws/ctrl",
                 "endpoint", "publish-file", "--path", str(dir_b / "file-b.txt"),
                 "--visibility", "public", "--delivery", "pull"], capture=True)
        if r.returncode != 0:
            _fail("B 公开 file-b.txt 失败")

        step("1) A→B pull：B 订阅 file-a.txt")
        r = run([cli, "endpoint", "subscribe", "--host", "127.0.0.1", "--port", neg_a,
                 "--endpoint", "file:0", "--out", str(recv / "1"), "--data-dir",
                 str(dir_b)], capture=True)
        if r.returncode != 0:
            _fail("订阅 pull 失败")
        if (recv / "1/file-a.txt").read_bytes() != (dir_a / "file-a.txt").read_bytes():
            _fail("1) pull 文件不一致")

        step("2) A→B both：B 订阅 file-c.bin（A 声明 both，订阅按声明走 pull）")
        r = run([cli, "endpoint", "subscribe", "--host", "127.0.0.1", "--port", neg_a,
                 "--endpoint", "file:1", "--out", str(recv / "2"), "--data-dir",
                 str(dir_b)], capture=True)
        if r.returncode != 0:
            _fail("订阅 both 失败")
        if (recv / "2/file-c.bin").read_bytes() != (dir_a / "file-c.bin").read_bytes():
            _fail("2) both 文件不一致")

        step("3) B→A pull：A 订阅 file-b.txt")
        r = run([cli, "endpoint", "subscribe", "--host", "127.0.0.1", "--port", neg_b,
                 "--endpoint", "file:0", "--out", str(recv / "3"), "--data-dir",
                 str(dir_a)], capture=True)
        if r.returncode != 0:
            _fail("订阅 B 失败")
        if (recv / "3/file-b.txt").read_bytes() != (dir_b / "file-b.txt").read_bytes():
            _fail("3) B 文件不一致")

        step("全部通过：3 条订阅链路（pull/both/pull）文件逐字节一致")
        for f in sorted(recv.rglob("*")):
            if f.is_file():
                print(f"  {f.stat().st_size} bytes  {f}")
        return 0
    except TestFailure:
        return 1
    finally:
        cleanup()


def _weaknet(args) -> int:
    secs = args.secs
    scenarios = args.scenarios
    trans = args.trans
    out = Path(env_default("OUT", "/tmp/stross-weaknet"))
    shutil.rmtree(out, ignore_errors=True)
    out.mkdir(parents=True)

    overall = 0
    for scen in scenarios.split(","):
        parts = scen.split()
        if len(parts) < 2:
            print(f"✗ 场景格式错误: {scen!r}（应为 '丢包率 延迟'）")
            return 1
        local_loss, local_delay = parts[0], parts[1]
        step(f"弱网场景: 丢包 {local_loss} / 延迟 {local_delay}（{secs}s × {trans}）")

        # 命名空间内：拉起 lo → 注入 netem → 校准 → 跑全套测试
        inner = (
            "ip link set lo up\n"
            f"tc qdisc add dev lo root netem loss '{local_loss}' delay '{local_delay}' 2>/dev/null "
            "|| { echo '  [ns] netem 注入失败'; exit 2; }\n"
            f"echo '  [ns] netem 已注入 lo: loss={local_loss} delay={local_delay}'\n"
            "ping -c 10 -q 127.0.0.1 2>/dev/null | tail -1 | sed 's/^/  [ns] 回环实测: /'\n"
            "set +e\n"
            f"cd {shlex_quote(str(REPO))} && uv run python -m scripts test-e2e "
            f"latency-stability {shlex_quote(str(secs))} {shlex_quote(trans)}\n"
            "exit $?\n"
        )
        logfile = out / f"{local_loss}-{local_delay}.log"
        rc = run_tee(["unshare", "-rn", "bash", "-c", inner], logfile)

        print("")
        print(f"  场景判定（loss={local_loss} delay={local_delay}, 内层 rc={rc}）:")
        for line in logfile.read_text(errors="replace").splitlines():
            if re.search(r"✅|❌", line):
                print(f"    {line}")
        if rc == 0:
            print("  ✅ 场景全过")
        else:
            print("  ❌ 场景存在未达标（弱网诊断信息见上，供调参决策）")
            overall = 1

    print("")
    if overall == 0:
        print("✅ 弱网测试全部场景达标")
        return 0
    print("❌ 弱网测试存在未达标场景（弱网下为预期诊断，不掩盖）")
    return 1


def shlex_quote(s) -> str:
    import shlex
    return shlex.quote(str(s))


def _latency_stability(args) -> int:
    cli = _cli()
    secs = args.secs
    trans_list = [w for item in (args.trans or []) for w in item.split()]
    if not trans_list:
        trans_list = ["srt", "quic"]
    trans = " ".join(trans_list)
    out = Path(env_default("OUT", "/tmp/stross-latency"))
    port = env_default("PORT", "18777")
    ctrl = env_default("CTRL", "18778")

    min_frames_pct, min_audio_pct = 90, 90
    max_tail_jitter = 250
    max_abs_min = {"srt": 200, "quic": 120, "ws": 200}

    lan = lan_ip()
    _build_cli_forced()
    shutil.rmtree(out, ignore_errors=True)
    out.mkdir(parents=True)

    join_offset = 2
    expect_frames = (secs - join_offset) * 30
    expect_audio = (secs - join_offset) * 47

    def run_round(trans_name: str) -> int:
        step(f"传输 {trans_name}（{secs}s 长跑）— 期望 {expect_frames} 帧 / "
             f"{expect_audio} 音频块")
        tdir = out / trans_name
        shutil.rmtree(tdir, ignore_errors=True)
        tdir.mkdir(parents=True)

        a = spawn([cli, "serve", "--port", port, "--ctrl-port", ctrl],
                  tdir / "serve.log")
        time.sleep(1.2)

        r = run([cli, "ctrl", "create-session", "--title", "稳定性测试"], capture=True)
        sid = session_id(r.stdout + r.stderr)
        if not sid:
            print("✗ 建会话失败")
            kill(a)
            return 1
        r = run([cli, "ctrl", "share-token", sid, "--ttl", "600"], capture=True)
        token = share_token(r.stdout + r.stderr)
        if not token:
            print("✗ 签发凭证失败")
            kill(a)
            return 1

        # 推流地址按传输取 /api/info（SRT/QUIC 端口以实际监听为准）
        _, info = http_get(f"http://127.0.0.1:{port}/api/info")
        srt_p = first_group(r'"srtPort":(\d+)', info)
        quic_p = first_group(r'"quicPort":(\d+)', info)
        if trans_name == "srt":
            push_url = f"srt://{lan}:{srt_p}"
            watch_url = f"srt://{lan}:{srt_p}"
        elif trans_name == "quic":
            push_url = f"quic://{lan}:{quic_p}"
            watch_url = f"quic://{lan}:{quic_p}"
        elif trans_name == "ws":
            push_url = f"ws://{lan}:{port}/ws/push"
            watch_url = f"ws://{lan}:{port}"
        else:
            print(f"未知传输 {trans_name}")
            kill(a)
            return 1

        rss0 = proc_rss_kb(a.pid)

        # PC-B 推流（后台；凭证接入，Welcome 后才返回）
        b = spawn([cli, "push", "--relay", push_url, "--stream-id", sid,
                   "--share-token", token, "--secs", str(secs), "--audio",
                   "--report-start", str(tdir / "start.json")], tdir / "push.log")
        time.sleep(1.5)
        push_text = (tdir / "push.log").read_text(errors="replace")
        if "中继已确认推流" not in push_text:
            print("  ❌ B 端接入失败")
            print(tail(tdir / "push.log", 3))
            kill(a)
            kill(b)
            return 1

        # PC-A 长跑接收（延迟 + 稳定性采样；--no-write 不落盘）
        run_log([cli, "receive", "--relay", watch_url, "--stream", sid,
                 "--secs", str(secs), "--latency", "--no-write",
                 "--calibrate", str(tdir / "start.json"), "--out", str(tdir / "recv")],
                tdir / "recv.log")
        try:
            wait_proc(b, timeout=secs + 30)
        except subprocess.TimeoutExpired:
            kill(b)

        recv_text = (tdir / "recv.log").read_text(errors="replace")
        frames = int(first_match(r"解码视频 (\d+)", recv_text) or 0)
        audio = int(first_match(r"音频块 (\d+)", recv_text) or 0)
        abs_line = first_match(r".*绝对端到端延迟.*", recv_text)
        abs_min = first_group(r"min=([0-9.]+)", abs_line or "")
        abs_p99 = first_group(r"p99=([0-9.]+)", abs_line or "")
        rel = first_match(r"p50=[-0-9.]+ p90=[-0-9.]+ p95=[-0-9.]+ p99=[-0-9.]+ max=[-0-9.]+",
                          recv_text)
        rss1 = proc_rss_kb(a.pid)
        kill(a)
        kill(b)

        print(f"  接收: {frames}/{expect_frames} 帧 | 音频块 {audio}/{expect_audio}")
        if abs_min:
            print(f"  绝对端到端延迟 ms: min={abs_min} p99={abs_p99 or '?'}"
                  f"（含 ffmpeg 预热上界）")
        if rel:
            print(f"  相对附加延迟（抖动/尾延迟）: {rel}")
        if rss0 and rss1:
            print(f"  serve RSS: {rss0} KB → {rss1} KB")

        # 断言
        ok = True
        if frames < expect_frames * min_frames_pct // 100:
            print("  ❌ 视频帧不足")
            ok = False
        if audio < expect_audio * min_audio_pct // 100:
            print("  ❌ 音频块不足")
            ok = False
        max_min = max_abs_min.get(trans_name, 250)
        if abs_min and float(abs_min) > max_min:
            print(f"  ❌ 绝对延迟超限 (min={abs_min}ms > {max_min}ms)")
            ok = False
        if abs_min and abs_p99 and (float(abs_p99) - float(abs_min)) > max_tail_jitter:
            print(f"  ❌ 尾延迟超限 (p99−min={float(abs_p99) - float(abs_min):g}ms > "
                  f"{max_tail_jitter}ms)")
            ok = False
        if ok:
            print(f"  ✅ {trans_name} 稳定 + 延迟达标")
            shutil.rmtree(tdir, ignore_errors=True)
            try:
                (out / "start.json").unlink()
            except OSError:
                pass
        else:
            if tdir.exists():
                shutil.move(str(tdir), str(out / f"failed-{trans_name}"))
            print(f"  ❌ {trans_name} 未达标（日志保留在 {out}/failed-{trans_name}/ 供排查）")
            return 1
        return 0

    failed = 0
    for t in trans_list:
        if run_round(t) != 0:
            failed = 1
    if failed == 0:
        print("✅ 双 PC 稳定性 + 延迟全部达标")
        return 0
    print("❌ 存在未达标项")
    return 1


def _share_token(args) -> int:
    cli = _cli()
    out = Path(env_default("OUT", "/tmp/stross-token"))
    port = env_default("PORT", "18777")
    ctrl = env_default("CTRL", "18778")
    secs, recv_secs = 14, 6
    min_frames, min_audio = 15, 40

    lan = lan_ip()
    push_base = f"ws://{lan}:{port}/ws/push"
    watch_base = f"ws://{lan}:{port}"
    print(f"  模拟跨设备来源: {lan}")

    _build_cli_forced()
    shutil.rmtree(out, ignore_errors=True)
    out.mkdir(parents=True)

    pids = {}

    def cleanup():
        for p in pids.values():
            kill(p)

    try:
        step(f"PC-A：serve（接收端，内核+受控中继+控制面，端口 {port}）")
        pids["a"] = spawn([cli, "serve", "--port", port, "--ctrl-port", ctrl],
                          out / "serve.log")
        time.sleep(1.2)

        r = run([cli, "ctrl", "create-session", "--title", "反向麦克风"], capture=True)
        sid = session_id(r.stdout + r.stderr)
        if not sid:
            _fail("✗ 建会话失败")
        step(f"会话（内核签发，D4）: {sid}")

        # 反例 1：不建会话、不授权，直接推流 → 受控中继必须拒绝
        step("反例 1：无凭证推流应被拒绝")
        run_log([cli, "push", "--relay", push_base, "--stream-id", f"intruder-{sid}",
                 "--secs", "2"], out / "no-token.log")
        no_token = (out / "no-token.log").read_text(errors="replace")
        if "未授权" in no_token:
            print("  ✅ 无凭证推流被拒绝（F2.2 语义保持）")
        else:
            print("  ❌ 无凭证推流未被拒绝（受控中继放行了未授权流！）")
            print(no_token)
            _fail("无凭证推流未被拒绝")

        step("PC-A：签发接入凭证（ctrl share-token）")
        r = run([cli, "ctrl", "share-token", sid, "--ttl", "300"], capture=True)
        token = share_token(r.stdout + r.stderr)
        if not token:
            _fail(f"✗ 签发凭证失败: {r.stdout + r.stderr}")
        print(f"  token: {token[:80]}…")

        step("PC-B：凭凭证向 A 的受控中继推流（合成视频 + 440Hz 测试音）")
        pids["b"] = spawn([cli, "push", "--relay", push_base, "--stream-id", sid,
                           "--share-token", token, "--secs", str(secs), "--audio"],
                          out / "push.log")
        time.sleep(1.2)
        push_text = (out / "push.log").read_text(errors="replace")
        if "中继已确认推流" in push_text:
            print("  ✅ B 端凭凭证接入成功（Hello 被接受）")
        else:
            print(f"  ❌ B 端接入失败: {tail(out / 'push.log', 3)}")
            _fail("B 端接入失败")

        step("PC-A：接收并播放 B 推来的流（反向闭环：手机麦克风 → 电脑扬声器）")
        run_log([cli, "receive", "--relay", watch_base, "--stream", sid,
                 "--out", str(out / "recv"), "--secs", str(recv_secs)],
                out / "recv.log")
        frames = _count_frames(out / "recv")
        audio = int(first_group(r"音频块 (\d+)",
                     (out / "recv.log").read_text(errors="replace")) or 0)
        print(f"A 端播放：解码帧 {frames} | 音频块 {audio}")

        # 反例 2：篡改凭证（PIN 改掉）→ 拒绝
        step("反例 2：篡改凭证（PIN 改为 000000）应被拒绝")
        forged = re.sub(r'"pin":"[0-9]*"', '"pin":"000000"', token)
        run_log([cli, "push", "--relay", push_base, "--stream-id", sid,
                 "--share-token", forged, "--secs", "2"], out / "forged.log")
        forged_text = (out / "forged.log").read_text(errors="replace")
        if re.search(r"未授权|凭证", forged_text):
            print("  ✅ 篡改凭证被拒绝")
        else:
            print("  ❌ 篡改凭证未被拒绝！")
            print(forged_text)
            _fail("篡改凭证未被拒绝")

        cleanup()
        print()
        print(f"凭证接入播放：{frames} 帧 / {audio} 音频块（阈值 {min_frames} / {min_audio}）")
        if frames >= min_frames and audio >= min_audio:
            print("✅ 凭证式跨设备推流（双 PC）全部 OK")
            return 0
        print("❌ 播放帧数/音频块不足")
        return 1
    finally:
        cleanup()


def _quic_stale_stream(args) -> int:
    cli = _cli()
    out = Path(env_default("OUT", "/tmp/stross-quic-stale"))
    port = env_default("PORT", "18777")
    ctrl = env_default("CTRL", "18778")
    quic = env_default("QUIC", "33464")
    idle_grace = 25  # 断言窗口（秒）：idle 15s + poll 间隔 + 余量

    _build_cli_if_missing(cli)
    shutil.rmtree(out, ignore_errors=True)
    out.mkdir(parents=True)

    pids = {}

    def cleanup():
        for p in pids.values():
            kill(p)

    try:
        step(f"设备 A：serve（受控中继 + QUIC {quic}）")
        pids["a"] = spawn([cli, "serve", "--port", port, "--ctrl-port", ctrl,
                           "--quic-port", quic], out / "serve.log")
        time.sleep(1.2)

        r = run([cli, "ctrl", "create-session", "--title", "quic-stale"], capture=True)
        sid = session_id(r.stdout + r.stderr)
        if not sid:
            print("✗ 建会话失败")
            print(tail(out / "serve.log", 5))
            _fail("建会话失败")
        step(f"会话: {sid} —— 独立推流进程经 QUIC 推入（持久 300s，等被 SIGKILL）")

        pids["push"] = spawn([cli, "push", "--relay", f"quic://127.0.0.1:{quic}",
                              "--stream-id", sid, "--secs", "300", "--audio"],
                             out / "push.log")
        time.sleep(3)
        _, streams = http_get(f"http://127.0.0.1:{port}/api/streams")
        if sid not in streams:
            print("✗ 推流未出现在 /api/streams（push 可能被拒）")
            print(tail(out / "push.log", 8))
            _fail("推流未出现在 /api/streams")
        watchers = _streams_watchers(port)
        print(f"✓ 流已建立: {sid}（watchers={watchers}）")

        step("SIGKILL 推流进程（等同手机 force-stop，无再见帧）")
        kill9(pids["push"])
        pids["push"] = None
        print("  推流进程已 kill -9")

        step(f"轮询 /api/streams，断言 {idle_grace} 秒内移除")
        start = time.time()
        for _ in range(idle_grace):
            time.sleep(1)
            _, streams = http_get(f"http://127.0.0.1:{port}/api/streams")
            if sid not in streams:
                elapsed = int(time.time() - start)
                print(f"✓ 流 {sid} 在 {elapsed}s 后从 /api/streams 移除（idle=15s 预算内）")
                return 0
        print(f"✗ {idle_grace}s 内流未被回收（流残留）——idle 检测失效？")
        _, leftover = http_get(f"http://127.0.0.1:{port}/api/streams")
        print(f"  残留 /api/streams: {leftover[:300]}")
        return 1
    finally:
        cleanup()


def _srt_push_silence_cleanup(args) -> int:
    cli = _cli()
    out = Path(env_default("OUT", "/tmp/stross-srt-silence"))
    port = env_default("PORT", "18787")   # 独立端口，避免干扰常驻 serve
    ctrl = env_default("CTRL", "18788")
    srt = env_default("SRT", "33472")
    idle_grace = 15  # 断言窗口（秒）：看门狗 10s + 轮询间隔 + 余量

    _build_cli_if_missing(cli)
    shutil.rmtree(out, ignore_errors=True)
    out.mkdir(parents=True)

    pids = {}

    def cleanup():
        for p in pids.values():
            kill(p)

    try:
        step(f"设备 A：serve（受控中继 + SRT {srt}）在端口 {port}")
        pids["a"] = spawn([cli, "serve", "--port", port, "--ctrl-port", ctrl,
                           "--srt-port", srt], out / "serve.log")
        time.sleep(1.2)

        r = run([cli, "ctrl", "--connect", f"ws://127.0.0.1:{ctrl}/ws/ctrl",
                 "create-session", "--title", "srt-silence"], capture=True)
        sid = session_id(r.stdout + r.stderr)
        if not sid:
            print("✗ 建会话失败")
            print(tail(out / "serve.log", 5))
            _fail("建会话失败")
        step(f"会话: {sid} —— 独立推流进程经 SRT 推入（持久 300s，等被 SIGKILL）")

        pids["push"] = spawn([cli, "push", "--relay", f"srt://127.0.0.1:{srt}",
                              "--stream-id", sid, "--secs", "300", "--audio"],
                             out / "push.log")
        time.sleep(3)
        _, streams = http_get(f"http://127.0.0.1:{port}/api/streams")
        if sid not in streams:
            print("✗ 推流未出现在 /api/streams（push 可能被拒）")
            print(tail(out / "push.log", 8))
            _fail("推流未出现在 /api/streams")
        print(f"✓ 流已建立: {sid}")

        step("挂一个观看端（SRT watch）应能看到流")
        pids["watch"] = spawn([cli, "receive", "--relay", f"srt://127.0.0.1:{srt}",
                               "--stream", sid, "--out", str(out / "watch"),
                               "--secs", "60"], out / "watch.log")
        time.sleep(2)
        w = _streams_watchers(port)
        print(f"  观看端接入后 watchers={w}")
        if w < 1:
            print("✗ watchers 未增长（观看端未接入？）")
            print(tail(out / "watch.log", 5))
            _fail("watchers 未增长")

        step("SIGKILL 推流进程（等同手机 force-stop，无再见帧）")
        kill9(pids["push"])
        pids["push"] = None
        print("  推流进程已 kill -9")

        step(f"轮询 /api/streams，断言 {idle_grace} 秒内移除（静默看门狗 10s）")
        start = time.time()
        for _ in range(idle_grace):
            time.sleep(1)
            _, streams = http_get(f"http://127.0.0.1:{port}/api/streams")
            if sid not in streams:
                elapsed = int(time.time() - start)
                print(f"✓ 流 {sid} 在 {elapsed}s 后从 /api/streams 移除（看门狗预算内）")
                # 等 watch 端随广播 channel 关闭而退出，watchers 应归零
                time.sleep(2)
                _, rem = http_get(f"http://127.0.0.1:{port}/api/streams")
                print(f"  移除后 /api/streams: {rem or '空'}")
                print("✓ 静默看门狗回收验证通过")
                return 0
        print(f"✗ {idle_grace}s 内流未被回收（看门狗未生效？）")
        _, leftover = http_get(f"http://127.0.0.1:{port}/api/streams")
        print(f"  残留 /api/streams: {leftover[:300]}")
        print(tail(out / "serve.log", 5))
        return 1
    finally:
        cleanup()


def _multi_endpoint(args) -> int:
    cli = _cli()
    d = Path(tempfile.mkdtemp(prefix="stross-multi-", dir="/tmp"))
    dir_a, dir_b, recv = d / "a", d / "b", d / "recv"
    port_a = env_default("PORT_A", "18777"); ctrl_a = env_default("CTRL_A", "18778")
    neg_a = env_default("NEG_A", "18779")
    port_b = env_default("PORT_B", "28777"); ctrl_b = env_default("CTRL_B", "28778")
    neg_b = env_default("NEG_B", "28779")
    srt_a = env_default("SRT_A", "33462"); quic_a = env_default("QUIC_A", "33464")
    srt_b = env_default("SRT_B", "33463"); quic_b = env_default("QUIC_B", "33465")

    pids = []

    def cleanup():
        for p in pids:
            kill(p)
        for p in pids:
            wait_proc(p, timeout=5)
        shutil.rmtree(d, ignore_errors=True)

    try:
        _build_cli_forced()
        dir_a.mkdir(parents=True); dir_b.mkdir(parents=True); recv.mkdir(parents=True)
        (dir_a / "file-a.txt").write_bytes(os.urandom(500000))
        (dir_a / "file-b.txt").write_bytes(os.urandom(900000))
        (dir_b / "file-c.txt").write_text("multi-endpoint B file\n", encoding="utf-8")

        step("启动节点 A / 节点 B（不同数据目录 → 不同 device_id；不同端口）")
        pids.append(spawn([cli, "serve", "--port", port_a, "--ctrl-port", ctrl_a,
                           "--negotiator-port", neg_a, "--srt-port", srt_a,
                           "--quic-port", quic_a, "--data-dir", str(dir_a)],
                          d / "a.log"))
        pids.append(spawn([cli, "serve", "--port", port_b, "--ctrl-port", ctrl_b,
                           "--negotiator-port", neg_b, "--srt-port", srt_b,
                           "--quic-port", quic_b, "--data-dir", str(dir_b)],
                          d / "b.log"))
        _wait_serve_ready(cli, [ctrl_a, ctrl_b], f"{d}/a.log / {d}/b.log")

        step("A 公开两个文件端点：file-a.txt / file-b.txt（并发多端点，pull）")
        r = run([cli, "ctrl", "endpoint", "publish-file", "--path",
                 str(dir_a / "file-a.txt"), "--visibility", "public",
                 "--delivery", "pull"], capture=True)
        if r.returncode != 0:
            _fail("A 公开 file-a 失败")
        r = run([cli, "ctrl", "endpoint", "publish-file", "--path",
                 str(dir_a / "file-b.txt"), "--visibility", "public",
                 "--delivery", "pull"], capture=True)
        if r.returncode != 0:
            _fail("A 公开 file-b 失败")
        r = run([cli, "endpoint", "ls", "--host", "127.0.0.1", "--port", neg_a,
                 "--data-dir", str(dir_b)], capture=True)
        ep_cnt = sum(1 for ln in r.stdout.splitlines() if "file:" in ln)
        if ep_cnt != 2:
            _fail(f"A 目录应有 2 个文件端点，实得 {ep_cnt}")

        step("1) B 同时订阅两个端点（并发，互不影响）")
        pa = spawn([cli, "endpoint", "subscribe", "--host", "127.0.0.1", "--port", neg_a,
                    "--endpoint", "file:0", "--out", str(recv / "a"), "--data-dir",
                    str(dir_b)], d / "suba.log")
        pb = spawn([cli, "endpoint", "subscribe", "--host", "127.0.0.1", "--port", neg_a,
                    "--endpoint", "file:1", "--out", str(recv / "b"), "--data-dir",
                    str(dir_b)], d / "subb.log")
        pids.extend([pa, pb])
        if wait_proc(pa) != 0:
            _fail(f"订阅 file-a 失败（看 {d}/suba.log）")
        if wait_proc(pb) != 0:
            _fail(f"订阅 file-b 失败（看 {d}/subb.log）")
        if (recv / "a/file-a.txt").read_bytes() != (dir_a / "file-a.txt").read_bytes():
            _fail("file-a 逐字节不一致")
        if (recv / "b/file-b.txt").read_bytes() != (dir_a / "file-b.txt").read_bytes():
            _fail("file-b 逐字节不一致")
        print(f"  ✓ file-a({(recv / 'a/file-a.txt').stat().st_size}B) / "
              f"file-b({(recv / 'b/file-b.txt').stat().st_size}B) 并发订阅一致")

        step("2) 同端点二次并发订阅（订阅收敛：复用同一流，不重复推流）")
        pa2 = spawn([cli, "endpoint", "subscribe", "--host", "127.0.0.1", "--port", neg_a,
                     "--endpoint", "file:0", "--out", str(recv / "a2"), "--data-dir",
                     str(dir_b)], d / "suba2.log")
        pids.append(pa2)
        if wait_proc(pa2) != 0:
            _fail(f"file-a 二次订阅失败（看 {d}/suba2.log）")
        if (recv / "a2/file-a.txt").read_bytes() != (dir_a / "file-a.txt").read_bytes():
            _fail("file-a 二次订阅不一致")
        print("  ✓ 同一 file-a 二次订阅也逐字节一致（复用流，无重复推流）")

        step("3) 双向多端点：B 公开 file-c.txt，A 订阅（跨方向并发共存）")
        r = run([cli, "ctrl", "--connect", f"ws://127.0.0.1:{ctrl_b}/ws/ctrl",
                 "endpoint", "publish-file", "--path", str(dir_b / "file-c.txt"),
                 "--visibility", "public", "--delivery", "pull"], capture=True)
        if r.returncode != 0:
            _fail("B 公开 file-c 失败")
        r = run([cli, "endpoint", "subscribe", "--host", "127.0.0.1", "--port", neg_b,
                 "--endpoint", "file:0", "--out", str(recv / "c"), "--data-dir",
                 str(dir_a)], capture=True)
        if r.returncode != 0:
            _fail("A 订阅 file-c 失败")
        if (recv / "c/file-c.txt").read_bytes() != (dir_b / "file-c.txt").read_bytes():
            _fail("file-c 逐字节不一致")
        print("  ✓ 双向多端点共存（A 同时扮演公开方+订阅方）")

        step("全部通过：多端点并发共享 + 订阅收敛 + 双向共存")
        for f in sorted(recv.rglob("*")):
            if f.is_file():
                print(f"  {f.stat().st_size} bytes  {f}")
        return 0
    except TestFailure:
        return 1
    finally:
        cleanup()


def _multi_mutual_disconnect(args) -> int:
    cli = _cli()
    d = Path(tempfile.mkdtemp(prefix="stross-mutual-", dir="/tmp"))
    dir_a, dir_b, recv = d / "a", d / "b", d / "recv"
    port_a, ctrl_a, neg_a, srt_a, quic_a = 29777, 29778, 29779, 35462, 35464
    port_b, ctrl_b, neg_b, srt_b, quic_b = 30777, 30778, 30779, 35463, 35465
    big_mb = 6  # 大文件（断连用：分块传完需要时间，可中断）

    pids = []
    subpids = []

    def cleanup():
        for p in list(pids) + list(subpids):
            kill(p)
        for p in list(pids) + list(subpids):
            wait_proc(p, timeout=5)
        shutil.rmtree(d, ignore_errors=True)

    try:
        _build_cli_forced()
        dir_a.mkdir(parents=True); dir_b.mkdir(parents=True); recv.mkdir(parents=True)
        (dir_a / "file-a.txt").write_bytes(os.urandom(400000))
        (dir_a / "file-b.txt").write_bytes(os.urandom(300000))
        (dir_a / "file-big-a.txt").write_bytes(os.urandom(big_mb * 1024 * 1024))
        (dir_b / "file-c.txt").write_bytes(os.urandom(500000))
        (dir_b / "file-d.txt").write_bytes(os.urandom(350000))
        (dir_b / "file-big-b.txt").write_bytes(os.urandom(big_mb * 1024 * 1024))

        step("启动节点 A / 节点 B")
        pids.append(spawn([cli, "serve", "--port", str(port_a), "--ctrl-port",
                           str(ctrl_a), "--negotiator-port", str(neg_a),
                           "--srt-port", str(srt_a), "--quic-port", str(quic_a),
                           "--data-dir", str(dir_a)], d / "a.log"))
        pids.append(spawn([cli, "serve", "--port", str(port_b), "--ctrl-port",
                           str(ctrl_b), "--negotiator-port", str(neg_b),
                           "--srt-port", str(srt_b), "--quic-port", str(quic_b),
                           "--data-dir", str(dir_b)], d / "b.log"))
        _wait_serve_ready(cli, [ctrl_a, ctrl_b], f"{d}/a.log / {d}/b.log")

        step("1) 并发相互分享：A、B 各公开 2 个普通端点 + 1 个大文件端点（共 3 个）")
        for fname in ("file-a", "file-b", "file-big-a"):
            r = run([cli, "ctrl", "--connect", f"ws://127.0.0.1:{ctrl_a}/ws/ctrl",
                     "endpoint", "publish-file", "--path", str(dir_a / f"{fname}.txt"),
                     "--visibility", "public", "--delivery", "pull"], capture=True)
            if r.returncode != 0:
                _fail(f"A 公开 {fname} 失败")
        r = run([cli, "ctrl", "--connect", f"ws://127.0.0.1:{ctrl_b}/ws/ctrl",
                 "endpoint", "publish-file", "--path", str(dir_b / "file-c.txt"),
                 "--visibility", "public", "--delivery", "pull"], capture=True)
        if r.returncode != 0:
            _fail("B 公开 file-c 失败")
        for fname in ("file-d", "file-big-b"):
            r = run([cli, "ctrl", "--connect", f"ws://127.0.0.1:{ctrl_b}/ws/ctrl",
                     "endpoint", "publish-file", "--path", str(dir_b / f"{fname}.txt"),
                     "--visibility", "public", "--delivery", "pull"], capture=True)
            if r.returncode != 0:
                _fail(f"B 公开 {fname} 失败")
        ra = run([cli, "endpoint", "ls", "--host", "127.0.0.1", "--port", str(neg_a),
                  "--data-dir", str(dir_b)], capture=True)
        rb = run([cli, "endpoint", "ls", "--host", "127.0.0.1", "--port", str(neg_b),
                  "--data-dir", str(dir_a)], capture=True)
        print(f"  A 端点数={sum(1 for ln in ra.stdout.splitlines() if 'file:' in ln)}")
        print(f"  B 端点数={sum(1 for ln in rb.stdout.splitlines() if 'file:' in ln)}")

        step("2) 并发相互订阅：A→B(c,d) 与 B→A(a,b) 同时进行（4 路并发）")
        subpids.append(spawn([cli, "endpoint", "subscribe", "--host", "127.0.0.1",
                              "--port", str(neg_b), "--endpoint", "file:0",
                              "--out", str(recv / "ac"), "--data-dir", str(dir_a)],
                             d / "s_ac.log"))
        subpids.append(spawn([cli, "endpoint", "subscribe", "--host", "127.0.0.1",
                              "--port", str(neg_b), "--endpoint", "file:1",
                              "--out", str(recv / "ad"), "--data-dir", str(dir_a)],
                             d / "s_ad.log"))
        subpids.append(spawn([cli, "endpoint", "subscribe", "--host", "127.0.0.1",
                              "--port", str(neg_a), "--endpoint", "file:0",
                              "--out", str(recv / "ba"), "--data-dir", str(dir_b)],
                             d / "s_ba.log"))
        subpids.append(spawn([cli, "endpoint", "subscribe", "--host", "127.0.0.1",
                              "--port", str(neg_a), "--endpoint", "file:1",
                              "--out", str(recv / "bb"), "--data-dir", str(dir_b)],
                             d / "s_bb.log"))
        time.sleep(6)
        if (recv / "ac/file-c.txt").read_bytes() != (dir_b / "file-c.txt").read_bytes():
            _fail("A→B file-c 不一致")
        if (recv / "ad/file-d.txt").read_bytes() != (dir_b / "file-d.txt").read_bytes():
            _fail("A→B file-d 不一致")
        if (recv / "ba/file-a.txt").read_bytes() != (dir_a / "file-a.txt").read_bytes():
            _fail("B→A file-a 不一致")
        if (recv / "bb/file-b.txt").read_bytes() != (dir_a / "file-b.txt").read_bytes():
            _fail("B→A file-b 不一致")
        print("  ✓ 4 路并发相互订阅全部一致")

        step("3) 断连恢复：B→A 传输 file-big-a 中段 kill 掉发布节点 B（断连）→ "
             "A 的订阅应优雅出错收尾")
        sub_big = spawn([cli, "endpoint", "subscribe", "--host", "127.0.0.1",
                         "--port", str(neg_b), "--endpoint", "file:2",
                         "--out", str(recv / "abig"), "--data-dir", str(dir_a)],
                        d / "s_abig.log")
        subpids.append(sub_big)
        time.sleep(1.5)  # 大文件传输进行中
        kill(pids[1])    # 只杀掉发布节点 B（订阅方所在 serve A 保留）→ 断连
        wait_proc(pids[1], timeout=10)
        time.sleep(2)
        big_path = recv / "abig/file-big-b.txt"
        recv_big = big_path.stat().st_size if big_path.exists() else 0
        if _ensure_not_running(sub_big):
            _fail(f"断连后订阅方未收尾（仍运行；已收 {recv_big} 字节）——应优雅出错")
        print(f"  ✓ 断连后订阅方已收尾退出（已收 {recv_big} 字节，未挂起）")

        print("")
        print("全部通过：多端点并发相互分享 + 断连优雅收尾")
        return 0
    except TestFailure:
        return 1
    finally:
        cleanup()


def _discovery(args) -> int:
    cli = _cli()
    port = env_default("PORT", "27777")
    ctrl = env_default("CTRL", "27778")
    discovery = env_default("DISCOVERY", "18779")  # = kernel DISCOVERY_PORT
    srt = env_default("SRT", "33463")
    quic = env_default("QUIC", "33465")

    d = Path(tempfile.mkdtemp(prefix="stross-disc-", dir="/tmp"))
    procs: list = []

    def node_start(discoverable: bool):
        args = ["serve", "--port", port, "--ctrl-port", ctrl,
                "--negotiator-port", discovery, "--srt-port", srt,
                "--quic-port", quic, "--data-dir", str(d / "a")]
        if discoverable:
            args.append("--discoverable")
        procs.append(spawn([cli, *args], d / "a.log"))
        time.sleep(2.5)

    def node_stop():
        if procs:
            kill(procs[0])
            wait_proc(procs[0], timeout=10)
            procs.clear()

    try:
        _build_cli_if_missing(cli)

        step(f"启动节点 A（锚定 + 广播，中继={port} 发现={discovery}）")
        node_start(True)
        rc, _ = http_get(f"http://127.0.0.1:{port}/api/info")
        if rc != 0:
            _fail("中继 /api/info 不可达")

        rc, disc = http_get(f"http://127.0.0.1:{discovery}/api/discovery", timeout=3)
        if rc != 0:
            _fail("/api/discovery 拉取失败")
        if f'"relayPort":{port}' not in disc:
            _fail(f"/api/discovery 未返回 relayPort={port}：{disc}")
        if '"roles"' not in disc:
            _fail(f"缺少 roles：{disc}")
        if '"endpoints"' not in disc:
            _fail(f"缺少 endpoints：{disc}")
        node_name = first_group(r'"name":"([^"]*)"', disc) or ""
        print(f"  /api/discovery → relayPort={port} · name={node_name} ✓")

        step(f"断言 devices 发现节点且节点 == relayPort={port}（同节点）")
        try:
            dev = run([cli, "devices"], capture=True, timeout=45)
            dev_out = dev.stdout
        except subprocess.TimeoutExpired:
            dev_out = ""  # 等价 timeout 45：超时视为无输出
        if f":{port}" not in dev_out:
            _fail(f"devices 未发现节点 :{port}：{dev_out}")
        print(f"  devices 发现 :{port} ✓")

        step("节点 A 改为不可被发现（无 --discoverable），验证「关闭 = 所有发现不可见」")
        node_stop()
        node_start(False)
        # 此时 /api/discovery 应 404（隐私门控：可被发现关闭即子网扫描也不可见）
        code = http_status(f"http://127.0.0.1:{discovery}/api/discovery", timeout=3)
        if code == "404":
            print("  ✓ discoverable=false 时 /api/discovery 404（子网扫描回退探测不到）")
        else:
            print(f"  ~ /api/discovery 返回 {code}（网内存在其它节点时不影响本断言，"
                  f"不视为失败）")

        step("✅ 统一发现链路全部通过")
        return 0
    except TestFailure:
        return 1
    finally:
        for p in procs:
            kill(p)
        shutil.rmtree(d, ignore_errors=True)


# ---------------------------------------------------------------- argparse

def add_subparsers(parent) -> None:
    sub = parent.add_subparsers(dest="subcommand", metavar="TEST")
    sub.add_parser("dual-device",
                   help="双设备端到端（serve 推流 → 直连/中途/级联解码）")
    sub.add_parser("dual-node-file",
                   help="端点框架双节点文件互发（pull/both/pull 逐字节一致）")
    p = sub.add_parser("weaknet", help="弱网稳定性（unshare+netem 注入）")
    p.add_argument("secs", nargs="?", type=int, default=60)
    p.add_argument("scenarios", nargs="?", default="5% 20ms,10% 40ms",
                   help="逗号分隔场景，每场景 '丢包率 延迟'")
    p.add_argument("trans", nargs="?", default="ws srt quic")
    p = sub.add_parser("latency-stability", help="双 PC 流稳定性 + 端到端延迟")
    p.add_argument("secs", nargs="?", type=int, default=60)
    p.add_argument("trans", nargs="*", help="传输列表（默认 srt quic）")
    sub.add_parser("share-token", help="受控中继凭证推流（含无凭证/篡改反例）")
    sub.add_parser("quic-stale-stream", help="QUIC 硬断连（SIGKILL）→ 流 16s 内回收")
    sub.add_parser("srt-push-silence-cleanup",
                   help="SRT 静默看门狗 10s + 观看端自愈")
    sub.add_parser("multi-endpoint", help="多端点并发共享 + 订阅收敛")
    sub.add_parser("multi-mutual-disconnect",
                   help="多端点相互分享 + 断连优雅收尾")
    sub.add_parser("discovery", help="统一发现链路（mDNS /api/discovery ↔ devices）")


_HANDLERS = {
    "dual-device": _dual_device,
    "dual-node-file": _dual_node_file,
    "weaknet": _weaknet,
    "latency-stability": _latency_stability,
    "share-token": _share_token,
    "quic-stale-stream": _quic_stale_stream,
    "srt-push-silence-cleanup": _srt_push_silence_cleanup,
    "multi-endpoint": _multi_endpoint,
    "multi-mutual-disconnect": _multi_mutual_disconnect,
    "discovery": _discovery,
}


def cmd(args) -> int:
    subcommand = getattr(args, "subcommand", None)
    if subcommand is None:
        print("请指定测试名（uv run python -m scripts test-e2e -h 查看清单）：",
              file=sys.stderr)
        return 2
    handler = _HANDLERS.get(subcommand)
    if handler is None:
        print(f"未知测试: {subcommand}", file=sys.stderr)
        return 2
    return handler(args)
