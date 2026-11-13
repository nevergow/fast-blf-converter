#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Fast BLF Converter —— 日志落盘（Step 22）。

窗口化（PyInstaller ``console=False``）时 ``sys.stderr is None``，原先的
``print(..., file=sys.stderr)`` 会被静默丢弃，事后完全无法排查。这里统一把
日志写进滚动文件：

* :func:`log_dir` —— 各平台的日志目录（可用 ``FAST_BLF_LOG_DIR`` 覆盖整个目录）
* :func:`log_path` —— 当前的 ``gui.log`` 路径（convert config 的 ``log_path`` 用它）
* :func:`setup_logging` —— 配置 root logger（10 MB × 5 滚动）、``faulthandler``
  （native 崩溃栈落到 ``crash.log``）、``sys.excepthook`` / ``threading.excepthook``

只依赖标准库：``logging`` / ``logging.handlers`` / ``faulthandler`` / ``pathlib``。
日志里只写路径、配置摘要、计数与错误文本 —— 绝不写 BLF 文件内容。
"""

from __future__ import annotations

import faulthandler
import logging
import logging.handlers
import os
import sys
import threading
from pathlib import Path

#: 落盘用的目录名（Windows 走 ``%LOCALAPPDATA%\<APP_DIR>``，其它平台同理）。
APP_DIR = "fast_blf_converter"
LOG_FILE_NAME = "gui.log"
CRASH_FILE_NAME = "crash.log"
#: 滚动：单文件 10 MB、保留 5 份（含当前那份共 5 个 gui.log*）。
MAX_BYTES = 10 * 1024 * 1024
BACKUP_COUNT = 5
#: 全应用统一的 logger 名（``gui.main`` / ``gui.plot_view`` 都用它取 logger）。
LOGGER_NAME = "fast_blf"
#: 覆盖日志目录的环境变量（验收与便携版排查用）。
LOG_DIR_ENV = "FAST_BLF_LOG_DIR"
LOG_FORMAT = "%(asctime)s.%(msecs)03d %(levelname)-7s [%(name)s] %(message)s"
DATE_FORMAT = "%Y-%m-%d %H:%M:%S"

#: 已配置的输出文件（重复调用 :func:`setup_logging` 时不重复挂 handler）。
_configured_path: Path | None = None
#: faulthandler 的目标文件句柄：必须保活，否则崩溃时写不进去。
_crash_handle = None
#: 安装钩子前的原始 excepthook（有终端时保持原有 traceback 输出）。
_original_excepthook = sys.excepthook


def log_dir() -> Path:
    """返回日志目录（``FAST_BLF_LOG_DIR`` 优先，其次按平台默认）。"""
    override = (os.environ.get(LOG_DIR_ENV) or "").strip()
    if override:
        return Path(override).expanduser()
    if sys.platform.startswith("win"):
        base = (os.environ.get("LOCALAPPDATA") or "").strip()
        root = Path(base) if base else Path.home() / "AppData" / "Local"
        return root / APP_DIR / "logs"
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Logs" / APP_DIR
    state_home = (os.environ.get("XDG_STATE_HOME") or "").strip()
    root = Path(state_home).expanduser() if state_home else Path.home() / ".local" / "state"
    return root / APP_DIR / "logs"


def log_path() -> Path:
    """当前的 ``gui.log`` 绝对路径（convert config 的 ``log_path`` 字段）。"""
    return log_dir() / LOG_FILE_NAME


def crash_path() -> Path:
    """native 崩溃栈的落盘文件（``faulthandler`` 追加写）。"""
    return log_dir() / CRASH_FILE_NAME


def _formatter() -> logging.Formatter:
    """带毫秒的时间戳 + 级别 + logger 名（排查时按时间对齐 Rust 侧日志）。"""
    return logging.Formatter(LOG_FORMAT, datefmt=DATE_FORMAT)


def _drop_own_handlers(root: logging.Logger) -> None:
    """移除本模块此前挂上的 handler（重复调用时避免同一行写两遍）。"""
    for handler in list(root.handlers):
        if getattr(handler, "_fast_blf_own", False):
            root.removeHandler(handler)
            handler.close()


def _install_excepthooks() -> None:
    """Python 未捕获异常与 QThread 里抛出的异常都落到同一个 logger。"""
    sys.excepthook = _handle_exception
    threading.excepthook = _handle_thread_exception


def _flush_handlers() -> None:
    """把缓冲区里的记录刷到磁盘（崩溃/退出前调用）。"""
    for handler in logging.getLogger().handlers:
        try:
            handler.flush()
        except (OSError, ValueError):
            continue


def _handle_exception(exc_type, exc_value, exc_tb) -> None:
    """主线程未捕获异常：先记日志（含 traceback），有终端时保留原有输出。"""
    logging.getLogger(LOGGER_NAME).critical(
        "未捕获异常：%s: %s", getattr(exc_type, "__name__", exc_type), exc_value,
        exc_info=(exc_type, exc_value, exc_tb))
    _flush_handlers()
    if sys.stderr is not None and _original_excepthook is not _handle_exception:
        _original_excepthook(exc_type, exc_value, exc_tb)


def _handle_thread_exception(args: threading.ExceptHookArgs) -> None:
    """``threading`` / ``QThread`` 里的未捕获异常（Python 3.8+ 的钩子）。"""
    thread = args.thread
    name = getattr(thread, "name", "?") if thread is not None else "?"
    logging.getLogger(LOGGER_NAME).critical(
        "线程未捕获异常 thread=%s", name,
        exc_info=(args.exc_type, args.exc_value, args.exc_traceback))
    _flush_handlers()


def setup_logging() -> Path:
    """配置日志落盘并返回 ``gui.log`` 路径；重复调用安全。

    * root logger：滚动文件（10 MB × 5）+ 有终端时镜像 WARNING 以上到 stderr
    * ``faulthandler``：native 崩溃栈追加到 ``crash.log``（与 gui.log 同目录）
    * ``sys.excepthook`` / ``threading.excepthook``：未捕获异常带 traceback 落盘
    """
    global _configured_path, _crash_handle

    target_dir = log_dir()
    target = target_dir / LOG_FILE_NAME
    if _configured_path == target:
        return target
    target_dir.mkdir(parents=True, exist_ok=True)

    root = logging.getLogger()
    root.setLevel(logging.INFO)
    _drop_own_handlers(root)

    file_handler = logging.handlers.RotatingFileHandler(
        str(target), maxBytes=MAX_BYTES, backupCount=BACKUP_COUNT, encoding="utf-8")
    file_handler.setLevel(logging.INFO)
    file_handler.setFormatter(_formatter())
    file_handler._fast_blf_own = True  # noqa: SLF001 - 仅本模块回收自己的 handler
    root.addHandler(file_handler)

    if sys.stderr is not None:
        # 窗口化 exe 下 stderr 为 None：此时不挂流 handler，只靠文件。
        stream_handler = logging.StreamHandler(sys.stderr)
        stream_handler.setLevel(logging.WARNING)
        stream_handler.setFormatter(_formatter())
        stream_handler._fast_blf_own = True  # noqa: SLF001
        root.addHandler(stream_handler)

    if _crash_handle is not None:
        try:
            _crash_handle.close()
        except OSError:
            _crash_handle = None
        _crash_handle = None
    try:
        handle = open(crash_path(), "a", encoding="utf-8")  # noqa: SIM115 - 需长期持有
    except OSError as exc:
        logging.getLogger(LOGGER_NAME).warning("崩溃日志 %s 不可写：%s", crash_path(), exc)
    else:
        faulthandler.enable(file=handle, all_threads=True)
        _crash_handle = handle

    _install_excepthooks()
    _configured_path = target
    logging.getLogger(LOGGER_NAME).info(
        "日志已启用 path=%s crash=%s pid=%d platform=%s python=%s",
        target, crash_path(), os.getpid(), sys.platform,
        sys.version.split()[0])
    return target


def shutdown_logging() -> None:
    """关闭日志（补 flush + 停用 faulthandler）；进程硬退出前调用。"""
    global _crash_handle, _configured_path
    faulthandler.disable()
    if _crash_handle is not None:
        try:
            _crash_handle.close()
        except OSError:
            _crash_handle = None
        _crash_handle = None
    _flush_handlers()
    _configured_path = None
