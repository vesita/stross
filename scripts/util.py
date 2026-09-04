"""Stross 工具链共享工具：repo 根定位、着色输出、子进程执行、node PATH 注入。

所有 helper 保持与旧 bash 脚本等价的行为（输出格式、退出码、环境语义）。
"""

from __future__ import annotations

import json
import os
import re
import shlex
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

# repo 根 = scripts/ 的上一级（scripts 包固定在仓库根目录下）
REPO = Path(__file__).resolve().parent.parent

# 旧 check.sh 注入的 fnm node 24.19.0（若存在），保证非交互 shell 也能找到 node/npx
_FNM_NODE = Path.home() / ".local/share/fnm/node-versions/v24.19.0/installation/bin"


class TestFailure(Exception):
    """回归脚本断言失败（等价 bash `exit 1`；由命令入口统一转为退出码 1）。"""


# ---------------------------------------------------------------- 输出

def step(msg: str) -> None:
    """等价 bash `log() { printf '\\n\\033[1;34m== %s ==\\033[0m\\n' "$*"; }`。"""
    print(f"\n\033[1;34m== {msg} ==\033[0m", flush=True)


def eprint(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


def tail(path: Path, n: int = 5) -> str:
    """等价 `tail -n <file>`（给失败提示用）。"""
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return ""
    return "\n".join(lines[-n:])


def head(path_or_text, n_chars: int) -> str:
    """等价 `head -c N`：对文本截断前 N 字符。"""
    return path_or_text[:n_chars]


# ---------------------------------------------------------------- 环境

def env_default(name: str, default: str) -> str:
    """等价 bash `${VAR:-default}`（空串也回退默认）。"""
    v = os.environ.get(name)
    return v if v else default


def node_env(env: dict | None = None) -> dict:
    """在子进程环境中注入 fnm node bin（若存在）；等价 check.sh 的 PATH 注入。"""
    e = dict(os.environ)
    if env:
        e.update(env)
    if _FNM_NODE.is_dir():
        e["PATH"] = str(_FNM_NODE) + os.pathsep + e.get("PATH", "")
    return e


def jdk21_env(env: dict | None = None) -> dict:
    """Android 构建 JDK 约束（AGENTS.md / docs/android-build.md）：JVM 必须 ≤ 21。

    仅当 JAVA_HOME 未设或指向不可用的高版本（≥ 25）时，收敛到 java-21-openjdk。
    """
    e = dict(os.environ)
    if env:
        e.update(env)
    j21 = Path("/usr/lib/jvm/java-21-openjdk")
    if not j21.is_dir():
        return e
    jh = e.get("JAVA_HOME", "")
    bad = (not jh) or re.search(r"jdk-?2[5-9]|java-2[5-9]|jdk-?[3-9][0-9]", jh)
    if bad:
        e["JAVA_HOME"] = str(j21)
        e["PATH"] = str(j21 / "bin") + os.pathsep + e.get("PATH", "")
    return e


def lan_ip() -> str:
    """等价 latency/share-token 的 LAN_IP 推导：第一个全局 IPv4，过滤
    fake-IP（198.18/15）、link-local 与回环；回退 hostname -I；再回退 127.0.0.1。"""
    try:
        r = subprocess.run(
            ["ip", "-4", "-o", "addr", "show", "scope", "global"],
            capture_output=True, text=True, timeout=5,
        )
        for line in r.stdout.splitlines():
            m = re.search(r"inet (\d+\.\d+\.\d+\.\d+)", line)
            if m:
                ip = m.group(1)
                if not re.match(r"^(198\.18\.|169\.254\.|127\.)", ip):
                    return ip
    except Exception:
        pass
    try:
        r = subprocess.run(["hostname", "-I"], capture_output=True, text=True, timeout=5)
        parts = r.stdout.split()
        if parts:
            return parts[0]
    except Exception:
        pass
    return "127.0.0.1"


# ---------------------------------------------------------------- 子进程

def run(cmd, *, cwd=REPO, env=None, capture=False, timeout=None, node_path=False,
        stdin=None, check=False, stderr=None):
    """运行命令（等价 bash 直接调用）。capture=True 时捕获输出。

    check=True 时非零退出码抛 TestFailure（等价 `cmd || fail`）。
    stderr=subprocess.DEVNULL 等价 `2>/dev/null`。
    """
    e = node_env(env) if node_path else dict(os.environ)
    if env:
        e.update(env)
    kw = dict(cwd=str(cwd), env=e, text=True, timeout=timeout, stdin=stdin)
    if capture:
        kw["capture_output"] = True
        r = subprocess.run(list(cmd), **kw)
    else:
        if stderr is not None:
            kw["stderr"] = stderr
        r = subprocess.run(list(cmd), **kw)
    if check and r.returncode != 0:
        raise TestFailure(f"命令失败（exit {r.returncode}）: {shlex.join(map(str, cmd))}")
    return r


def spawn(cmd, logpath: Path, *, cwd=REPO, env=None, node_path=False):
    """后台启动进程，stdout/stderr 合并写入 logpath（等价 `cmd > log 2>&1 &`）。"""
    e = node_env(env) if node_path else dict(os.environ)
    if env:
        e.update(env)
    logpath.parent.mkdir(parents=True, exist_ok=True)
    f = open(logpath, "wb")
    p = subprocess.Popen(list(cmd), cwd=str(cwd), env=e, stdout=f,
                         stderr=subprocess.STDOUT)
    p._logfile = f  # type: ignore[attr-defined]
    return p


def wait_proc(p, timeout=None) -> int:
    """等待进程退出并关闭其日志文件（等价 `wait $PID`）。"""
    try:
        rc = p.wait(timeout=timeout)
    finally:
        lf = getattr(p, "_logfile", None)
        if lf:
            try:
                lf.close()
            except OSError:
                pass
    return rc


def run_log(cmd, logpath: Path, *, cwd=REPO, env=None, node_path=False) -> int:
    """前台运行并把输出写入 logpath（等价 `cmd > log 2>&1`）。返回退出码。"""
    p = spawn(cmd, logpath, cwd=cwd, env=env, node_path=node_path)
    return wait_proc(p)


def run_tee(cmd, logpath: Path, *, cwd=REPO, env=None, node_path=False) -> int:
    """前台运行，输出实时流到终端并同时写 logpath（等价 `cmd 2>&1 | tee log`）。"""
    e = node_env(env) if node_path else dict(os.environ)
    if env:
        e.update(env)
    logpath.parent.mkdir(parents=True, exist_ok=True)
    with open(logpath, "wb") as f:
        p = subprocess.Popen(list(cmd), cwd=str(cwd), env=e,
                             stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                             text=True)
        assert p.stdout is not None
        for line in p.stdout:
            f.write(line.encode("utf-8", "replace"))
            f.flush()
            print(line, end="", flush=True)
        p.wait()
    return p.returncode


def kill(p, sig=signal.SIGTERM) -> None:
    """等价 `kill $PID 2>/dev/null || true`。"""
    if p is None or p.poll() is not None:
        return
    try:
        p.send_signal(sig)
    except ProcessLookupError:
        pass
    except OSError:
        pass


def kill9(p) -> None:
    """等价 `kill -9 $PID`。"""
    if p is None or p.poll() is not None:
        return
    try:
        p.kill()
    except ProcessLookupError:
        pass
    except OSError:
        pass


def proc_rss_kb(pid: int) -> int | None:
    """等价 `ps -o rss= -p PID`（KB）。"""
    try:
        with open(f"/proc/{pid}/status", encoding="utf-8") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except Exception:
        return None
    return None


# ---------------------------------------------------------------- HTTP（等价 curl -s --max-time N）

def http_get(url: str, timeout: float = 2.0):
    """等价 `curl -s --max-time N <url>`：返回 (curl_rc, body)。

    curl 语义：HTTP 4xx/5xx 仍为退出码 0（未加 -f），body 为空；
    连接失败/超时退出码非 0。
    """
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return 0, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError:
        return 0, ""
    except Exception:
        return 28, ""


def http_status(url: str, timeout: float = 2.0) -> str:
    """等价 `curl -s -o /dev/null -w "%{http_code}"`（连接失败返回 000）。"""
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return str(r.status)
    except urllib.error.HTTPError as e:
        return str(e.code)
    except Exception:
        return "000"


def http_post_json(url: str, payload: dict, timeout: float = 2.0):
    """等价 `curl -s -X POST -H 'Content-Type: application/json' -d '<json>'`。"""
    req = urllib.request.Request(
        url, data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"}, method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return 0, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace") if e.fp else ""
        return 0, body
    except Exception:
        return 28, ""


def first_match(pattern: str, text: str) -> str | None:
    """等价 `grep -oE <pattern> | head -1`（取第一个匹配串本身）。"""
    m = re.search(pattern, text)
    return m.group(0) if m else None


def first_group(pattern: str, text: str) -> str | None:
    """等价 `grep -oE <pattern> | awk '{print $2}'` 这类取捕获组的场景。"""
    m = re.search(pattern, text)
    return m.group(1) if m else None


def session_id(text: str) -> str | None:
    """从 `ctrl create-session` 输出取 sessionId（等价 grep+awk）。"""
    return first_group(r"sessionId: ([a-z0-9-]*)", text)


def share_token(text: str) -> str | None:
    """从 `ctrl share-token` 输出取 JSON 凭证行（等价 grep -oE '\\{"v":[0-9].*' | head -1）。"""
    for line in text.splitlines():
        if re.search(r'\{"v":[0-9]', line):
            return line.strip()
    return None
