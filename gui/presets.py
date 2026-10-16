#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""配置预设的目录解析与读写（Step 6c）。

目录优先级：``FBLF_CONFIG_DIR`` → 绿色包 ``configs/`` → 用户目录；每级先创建并做写
测试，失败静默回退。配置为 UTF-8 JSON（``schema == 1``），坏文件只跳过并计数。
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

SCHEMA = 1
MAX_NAME_LEN = 60
ILLEGAL_NAME_CHARS = '/\\:*?"<>|'
DEFAULT_EXPORT = {"split_by_message": True, "resample_hz": None, "export_format": "csv"}
EXPORT_FORMATS = ("csv", "parquet", "xlsx")


class PresetError(Exception):
    """配置名非法 / 内容不可读写时抛出，``str(exc)`` 可直接显示给用户。"""


def safe_name(name: str) -> str:
    """清洗配置名：去掉文件系统非法字符、折叠换行、长度 ≤ 60；空名拒绝。"""
    cleaned = "".join(c for c in str(name or "") if c not in ILLEGAL_NAME_CHARS)
    cleaned = cleaned.replace("\r", " ").replace("\n", " ").strip()[:MAX_NAME_LEN].strip()
    if not cleaned:
        raise PresetError("配置名不能为空，也不能只包含 / \\ : * ? \" < > | 等非法字符")
    return cleaned


def _candidate_dirs() -> list[Path]:
    """按优先级返回候选目录（最后一级必为用户目录，保证总有落点）。"""
    dirs: list[Path] = []
    if os.environ.get("FBLF_CONFIG_DIR"):
        dirs.append(Path(os.environ["FBLF_CONFIG_DIR"]).expanduser())
    base = (Path(sys.executable).resolve().parent if getattr(sys, "frozen", False)
            else Path(__file__).resolve().parent.parent)
    dirs.append(base / "configs")
    home = Path.home()
    if sys.platform.startswith("win"):
        appdata = os.environ.get("APPDATA") or str(home / "AppData" / "Roaming")
        dirs.append(Path(appdata) / "FastBLFConverter" / "configs")
    elif sys.platform == "darwin":
        dirs.append(home / "Library" / "Application Support" / "FastBLFConverter" / "configs")
    else:
        dirs.append(home / ".config" / "FastBLFConverter" / "configs")
    return dirs


def _is_writable(path: Path) -> bool:
    """创建目录并真实写一个探针文件，验证「可写」而不只是「已存在」。"""
    try:
        path.mkdir(parents=True, exist_ok=True)
        fd, probe = tempfile.mkstemp(prefix=".fblf_write_", dir=str(path))
        os.close(fd)
        os.unlink(probe)
        return True
    except OSError:
        return False


_DIR_CACHE: dict[str | None, Path] = {}


def config_dir() -> Path:
    """返回最终选定的配置目录（供状态栏 tooltip 显示文件位置）；按环境变量缓存。"""
    key = os.environ.get("FBLF_CONFIG_DIR")
    if key not in _DIR_CACHE:
        candidates = _candidate_dirs()
        _DIR_CACHE[key] = next((p for p in candidates if _is_writable(p)), candidates[-1])
    return _DIR_CACHE[key]


def preset_path(name: str) -> Path:
    """配置名 → 落盘路径（名先安全化）。"""
    return config_dir() / f"{safe_name(name)}.json"


@dataclass
class Preset:
    """一份配置预设的内存形态。"""

    name: str
    path: Path
    saved_at: str = ""
    dbc_by_channel: dict[int, str] = field(default_factory=dict)
    selected: dict[str, dict[str, list[str]]] = field(default_factory=dict)
    export: dict = field(default_factory=lambda: dict(DEFAULT_EXPORT))
    mtime: float = 0.0

    @property
    def display(self) -> str:
        """下拉框显示文本：``名称（修改时间）``。"""
        when = (datetime.fromtimestamp(self.mtime).strftime("%Y-%m-%d %H:%M")
                if self.mtime else "—")
        return f"{self.name}（{when}）"


def _from_json_dbc(raw) -> dict[int, str]:
    """JSON 的字符串通道号 → int；非法项直接丢弃。"""
    result: dict[int, str] = {}
    for ch, path in (raw.items() if isinstance(raw, dict) else ()):
        try:
            channel = int(ch)
        except (TypeError, ValueError):
            continue
        if isinstance(path, str) and path:
            result[channel] = path
    return result


def _coerce_selected(raw) -> dict[str, dict[str, list[str]]]:
    """只保留 ``{通道: {报文: [信号…]}}`` 形状的合法内容。"""
    result: dict[str, dict[str, list[str]]] = {}
    for ch, messages in (raw.items() if isinstance(raw, dict) else ()):
        if not isinstance(messages, dict):
            continue
        clean: dict[str, list[str]] = {}
        for message, signals in messages.items():
            names = ([signals] if isinstance(signals, str) else
                     [str(s) for s in signals if isinstance(s, str)]
                     if isinstance(signals, list) else [])
            if names:
                clean[str(message)] = names
        if clean:
            result[str(ch)] = clean
    return result


def _coerce_export(raw) -> dict:
    """导出策略：缺字段用默认值，非法值忽略。"""
    export = dict(DEFAULT_EXPORT)
    if isinstance(raw, dict):
        if "split_by_message" in raw:
            export["split_by_message"] = bool(raw["split_by_message"])
        hz = raw.get("resample_hz")
        export["resample_hz"] = float(hz) if isinstance(hz, (int, float)) else None
        if raw.get("export_format") in EXPORT_FORMATS:
            export["export_format"] = raw["export_format"]
    return export


def read_preset(path) -> Preset:
    """读取单个配置；schema 不符 / JSON 损坏 → :class:`PresetError`。"""
    path = Path(path)
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except OSError as exc:
        raise PresetError(f"无法读取配置文件：{exc}") from exc
    except json.JSONDecodeError as exc:
        raise PresetError(f"JSON 解析失败（第 {exc.lineno} 行）：{exc.msg}") from exc
    if not isinstance(data, dict):
        raise PresetError("配置内容必须是 JSON 对象")
    if data.get("schema") != SCHEMA:
        raise PresetError(f"不支持的配置版本 schema={data.get('schema')!r}"
                          f"（本程序仅支持 schema={SCHEMA}）")
    try:
        mtime = path.stat().st_mtime
    except OSError:
        mtime = 0.0
    return Preset(name=str(data.get("name") or path.stem), path=path,
                  saved_at=str(data.get("saved_at") or ""),
                  dbc_by_channel=_from_json_dbc(data.get("dbc_by_channel")),
                  selected=_coerce_selected(data.get("selected")),
                  export=_coerce_export(data.get("export")), mtime=mtime)


def save_preset(name: str, dbc_by_channel: dict | None, selected: dict | None,
                export: dict | None) -> Path:
    """把 DBC 映射 / 选中信号 / 导出策略写为 ``<安全名>.json``（先写临时文件再替换）。"""
    directory = config_dir()
    path = directory / f"{safe_name(name)}.json"
    payload = {"schema": SCHEMA, "name": path.stem,
               "saved_at": datetime.now().astimezone().isoformat(timespec="seconds"),
               "dbc_by_channel": {str(int(ch)): str(p)
                                  for ch, p in (dbc_by_channel or {}).items()},
               "selected": _coerce_selected(selected), "export": _coerce_export(export)}
    tmp = path.with_name(path.name + ".tmp")
    try:
        directory.mkdir(parents=True, exist_ok=True)
        tmp.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        os.replace(tmp, path)
    except OSError as exc:
        try:
            tmp.unlink()
        except OSError:
            pass
        raise PresetError(f"写入配置失败（{directory}）：{exc}") from exc
    return path


def scan_presets() -> tuple[list[Preset], int]:
    """列出全部可用配置（按修改时间倒序）；返回 ``(配置列表, 被跳过的坏文件数)``。"""
    directory = config_dir()
    presets: list[Preset] = []
    skipped = 0
    for path in sorted(directory.glob("*.json")):
        if not path.is_file():
            continue
        try:
            presets.append(read_preset(path))
        except PresetError:
            skipped += 1  # 人工编辑坏的文件：只跳过并计数，绝不上抛
    presets.sort(key=lambda preset: preset.mtime, reverse=True)
    return presets, skipped


def delete_preset(path) -> bool:
    """删除配置文件；文件本就不存在返回 ``False``，其它 IO 失败抛 :class:`PresetError`。"""
    try:
        Path(path).unlink()
        return True
    except FileNotFoundError:
        return False
    except OSError as exc:
        raise PresetError(f"删除配置失败：{exc}") from exc
