"""Stross 自动化工具链（uv 管理）。

等价旧 scripts/*.sh 与 *.mjs 的单一 Python 入口：

    uv run python -m scripts --help
    uv run python -m scripts check --quick
    uv run python -m scripts build cli
    uv run python -m scripts test-e2e dual-node-file
    uv run python -m scripts phone dump
    uv run python -m scripts frontend
    uv run python -m scripts android
    uv run python -m scripts hooks [--remove]

子命令模块在 scripts/commands/ 下，共享工具在 scripts/util.py。
"""

__version__ = "0.1.0"
