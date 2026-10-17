#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""配置预设的目录解析与读写（Step 6c）。

目录优先级：``FBLF_CONFIG_DIR`` → 绿色包 ``configs/`` → 用户目录；每级先创建并做写
测试，失败静默回退。配置为 UTF-8 JSON（``schema == 2``；``schema == 1`` 旧配置自动补齐
默认值），坏文件只跳过并计数。另有一个同目录的 ``settings.json`` 保存缓存设置。
"""

from __future__ import annotations

import json
import math
import os
import sys
import tempfile
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

SCHEMA = 2
#: 兼容读取的旧版本；写入永远使用 :data:`SCHEMA`。
SUPPORTED_SCHEMAS = (1, SCHEMA)
MAX_NAME_LEN = 60
ILLEGAL_NAME_CHARS = '/\\:*?"<>|'
DEFAULT_EXPORT = {
    "split_by_message": True,
    "resample_hz": None,
    "export_format": "csv",
    "time_format": "relative_s",
    "fill_policy": "hold",
    "ffill_max_gap_ms": None,
}
EXPORT_FORMATS = ("csv", "parquet", "xlsx")
TIME_FORMATS = ("relative_s", "relative_ms", "absolute", "unix_s")
FILL_POLICIES = ("none", "hold", "linear")

#: 缓存设置（Step 19 C1）：``<config_dir>/settings.json``。与预设分离，不参与预设扫描。
SETTINGS_FILENAME = "settings.json"
DEFAULT_CACHE_BUDGET_MB = 200
CACHE_BUDGET_MIN_MB = 32
CACHE_BUDGET_MAX_MB = 8192
DEFAULT_SETTINGS = {"cache_budget_mb": DEFAULT_CACHE_BUDGET_MB, "cache_dir": None}


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
    #: 读取时被回落为默认值的非法字段说明（B2：非法值 → 默认 + warnings）。
    warnings: list[str] = field(default_factory=list)

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


def _coerce_export(raw) -> tuple[dict, list[str]]:
    """导出策略：缺字段补默认值；非法值回落默认并返回 warnings（B2）。

    返回 ``(export, warnings)``；``warnings`` 里的每条都是可直接显示的人话。
    ``schema == 1`` 的旧配置只有前三个字段，其余字段会在这里自动补齐。
    """
    export = dict(DEFAULT_EXPORT)
    warnings: list[str] = []
    if not isinstance(raw, dict):
        return export, warnings
    if "split_by_message" in raw:
        export["split_by_message"] = bool(raw["split_by_message"])
    if "resample_hz" in raw:
        hz = raw.get("resample_hz")
        if hz is None:
            export["resample_hz"] = None
        elif (isinstance(hz, (int, float)) and not isinstance(hz, bool)
              and math.isfinite(float(hz)) and float(hz) > 0):
            export["resample_hz"] = float(hz)
        else:
            warnings.append(f"重采样频率 {hz!r} 非法，已回落为「保持原始时序」")
    if "export_format" in raw:
        fmt = raw.get("export_format")
        if fmt in EXPORT_FORMATS:
            export["export_format"] = fmt
        else:
            warnings.append(f"导出格式 {fmt!r} 非法，已回落为 csv")
    if "time_format" in raw:
        time_format = raw.get("time_format")
        if time_format in TIME_FORMATS:
            export["time_format"] = time_format
        else:
            warnings.append(f"时间列格式 {time_format!r} 非法，已回落为 relative_s")
    if "fill_policy" in raw:
        fill_policy = raw.get("fill_policy")
        if fill_policy in FILL_POLICIES:
            export["fill_policy"] = fill_policy
        else:
            warnings.append(f"填充方式 {fill_policy!r} 非法，已回落为 hold")
    if "ffill_max_gap_ms" in raw:
        gap = raw.get("ffill_max_gap_ms")
        if gap is None:
            export["ffill_max_gap_ms"] = None
        elif (isinstance(gap, (int, float)) and not isinstance(gap, bool)
              and math.isfinite(float(gap)) and float(gap) > 0):
            export["ffill_max_gap_ms"] = float(gap)
        else:
            warnings.append(f"最大保持时长 {gap!r} 非法，已回落为 None（不限）")
    return export, warnings


def read_preset(path) -> Preset:
    """读取单个配置；schema 不符 / JSON 损坏 → :class:`PresetError`。

    ``schema == 1`` 的旧配置照常读入：导出策略缺的字段由 :func:`_coerce_export`
    补默认值，原有字段原样保留（不因升级而丢失）。
    """
    path = Path(path)
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except OSError as exc:
        raise PresetError(f"无法读取配置文件：{exc}") from exc
    except json.JSONDecodeError as exc:
        raise PresetError(f"JSON 解析失败（第 {exc.lineno} 行）：{exc.msg}") from exc
    if not isinstance(data, dict):
        raise PresetError("配置内容必须是 JSON 对象")
    if data.get("schema") not in SUPPORTED_SCHEMAS:
        raise PresetError(f"不支持的配置版本 schema={data.get('schema')!r}"
                          f"（本程序支持 schema={SCHEMA}，并兼容 schema=1）")
    export, warnings = _coerce_export(data.get("export"))
    try:
        mtime = path.stat().st_mtime
    except OSError:
        mtime = 0.0
    return Preset(name=str(data.get("name") or path.stem), path=path,
                  saved_at=str(data.get("saved_at") or ""),
                  dbc_by_channel=_from_json_dbc(data.get("dbc_by_channel")),
                  selected=_coerce_selected(data.get("selected")),
                  export=export, mtime=mtime, warnings=warnings)


def save_preset(name: str, dbc_by_channel: dict | None, selected: dict | None,
                export: dict | None) -> Path:
    """把 DBC 映射 / 选中信号 / 导出策略写为 ``<安全名>.json``（先写临时文件再替换）。"""
    directory = config_dir()
    path = directory / f"{safe_name(name)}.json"
    if path.name == SETTINGS_FILENAME:
        raise PresetError(f"配置名 {path.stem!r} 是保留名称（用于缓存设置），请换一个")
    export_policy, _warnings = _coerce_export(export)
    payload = {"schema": SCHEMA, "name": path.stem,
               "saved_at": datetime.now().astimezone().isoformat(timespec="seconds"),
               "dbc_by_channel": {str(int(ch)): str(p)
                                  for ch, p in (dbc_by_channel or {}).items()},
               "selected": _coerce_selected(selected), "export": export_policy}
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
        if not path.is_file() or path.name == SETTINGS_FILENAME:
            continue  # settings.json 是缓存设置，不是预设（不参与 skipped 计数）
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


# --------------------------------------------------------------- 缓存设置 ----
def settings_path() -> Path:
    """缓存设置的落盘位置：``<config_dir>/settings.json``（C1）。"""
    return config_dir() / SETTINGS_FILENAME


def _coerce_settings(raw) -> tuple[dict, list[str]]:
    """校验缓存设置：上限 32..8192 MiB；非法值回落默认并记 warnings。"""
    settings = dict(DEFAULT_SETTINGS)
    warnings: list[str] = []
    if not isinstance(raw, dict):
        return settings, warnings
    if "cache_budget_mb" in raw:
        value = raw.get("cache_budget_mb")
        if (isinstance(value, (int, float)) and not isinstance(value, bool)
                and math.isfinite(float(value)) and float(value).is_integer()
                and CACHE_BUDGET_MIN_MB <= int(value) <= CACHE_BUDGET_MAX_MB):
            settings["cache_budget_mb"] = int(value)
        else:
            warnings.append(f"帧缓存上限 {value!r} 非法（允许 32..8192 MiB），"
                            f"已回落为 {DEFAULT_CACHE_BUDGET_MB} MiB")
    if "cache_dir" in raw:
        value = raw.get("cache_dir")
        if value is None or value == "":
            settings["cache_dir"] = None
        elif isinstance(value, str) and value.strip():
            settings["cache_dir"] = str(Path(value).expanduser())
        else:
            warnings.append(f"缓存目录 {value!r} 非法，已回落为系统临时目录")
    return settings, warnings


def read_settings() -> dict:
    """读取缓存设置；文件缺失 / 损坏 / 非法值时静默回落到默认值。"""
    path = settings_path()
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return dict(DEFAULT_SETTINGS)
    settings, _warnings = _coerce_settings(raw)
    return settings


def save_settings(settings: dict | None) -> Path:
    """原子写入 ``settings.json``（只写缓存设置，不碰预设文件）。"""
    directory = config_dir()
    path = directory / SETTINGS_FILENAME
    payload, _warnings = _coerce_settings(settings or {})
    tmp = path.with_name(path.name + ".tmp")
    try:
        directory.mkdir(parents=True, exist_ok=True)
        tmp.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
                       encoding="utf-8")
        os.replace(tmp, path)
    except OSError as exc:
        try:
            tmp.unlink()
        except OSError:
            pass
        raise PresetError(f"写入缓存设置失败（{directory}）：{exc}") from exc
    return path
