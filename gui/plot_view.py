#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Step 18：基于 ``PlotSession`` 的预览窗口（``gui/plot_view.py``）。

设计要点（与 ``docs/INTERFACE.md`` §1.4 / §5 第 17–20 条一一对应）：

* **唯一数据通道**：窗口持有 ``fast_blf_core.PlotSession``。可见窗口的数据只在
  "窗口变化"时调用一次 ``session.view(t0, t1, width_px)``，返回的数组缓存在
  :class:`PlotCanvas` 里；``paintEvent`` 只读缓存，**绝不访问内核**。
* **恒定绘制量**：``view()`` 每信号最多返回 ``width_px`` 个桶；画布再把桶几何
  缓存成 ``QPainterPath`` / ``QPolygonF``，十字线和图例高亮只触发 ``update()``。
  因此单帧成本是 O(可见信号 × 画布宽度)，与文件里有多少采样点无关。
* **粗粒度交互**：滚轮 / 拖动期间用 ``width_px=400`` 取数，松手 / 停手后用实际
  像素宽度再取一次精化；模式切换和时间口径切换是纯布局 / 纯显示层变化，不取数。
* **三种显示模式**：车道（默认）/ 多 Y 轴叠加 / 单 Y 轴。
* **每信号样式**：颜色（QColorDialog）、线型（折线 / 阶梯 / 散点）、可见性。
* **单复选框全选**：面板顶部只有一个三态 ``QCheckBox``，没有"全选/全不选/反选"
  三个按钮；三态口径 = 全部信号行（与名字过滤无关）。
* **缓存超额**：不弹模态错误框；在窗口内显示红色内联横幅 + 三个动作按钮。

本模块不 import ``fast_blf_core``：会话对象由 ``gui/main.py`` 的 Worker 线程创建
后注入，保证打开会话 / 建缓存都不在 GUI 线程里阻塞。
"""

from __future__ import annotations

import bisect
import json
import math
import os
import re
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path

from PySide6.QtCore import QEvent, QPointF, QRectF, Qt, QThread, QTimer, Signal
from PySide6.QtGui import (
    QBrush, QColor, QFont, QFontMetrics, QPainter, QPainterPath, QPen, QPolygonF,
)
from PySide6.QtWidgets import (
    QCheckBox, QColorDialog, QComboBox, QFrame, QHBoxLayout, QInputDialog,
    QLabel, QLineEdit, QListWidget, QListWidgetItem, QMenu, QPushButton,
    QScrollArea, QSizePolicy, QVBoxLayout, QWidget,
)

#: 时间格式下拉（与 ``gui/main.py`` 的 ``TIME_FORMAT_CHOICES`` 同构；main 会传入权威副本）。
TIME_FORMAT_CHOICES: list[tuple[str, str]] = [
    ("相对时间（秒）", "relative_s"),
    ("相对时间（毫秒）", "relative_ms"),
    ("绝对时间（日期 时间）", "absolute"),
    ("Unix 时间戳（秒）", "unix_s"),
]

#: 三种显示模式（B1/B2/B3）。
DISPLAY_MODES: list[tuple[str, str]] = [
    ("车道（每信号一行）", "lanes"),
    ("多 Y 轴叠加", "multi_axis"),
    ("单 Y 轴", "single_axis"),
]

#: 每信号线型（C2）。
LINE_STYLES: list[tuple[str, str]] = [
    ("折线", "line"),
    ("阶梯（ZOH）", "step"),
    ("散点", "scatter"),
]
LINE_STYLE_LABELS = {value: label for label, value in LINE_STYLES}

#: 高对比调色板：深色背景 ``#1E1E24`` 上亮度/饱和度都足够，相邻颜色色相拉开。
PALETTE: list[str] = [
    "#4FC3F7", "#FFB74D", "#81C784", "#E57373",
    "#BA68C8", "#4DD0E1", "#FFD54F", "#F06292",
    "#AED581", "#FF8A65", "#7986CB", "#A1887F",
]

#: 几何常量。
AXIS_WIDTH = 64
TICK_COUNT = 5
MEAN_ALPHA = 120
LANE_MIN_HEIGHT = 44.0
LANE_LEFT = 168.0
PLOT_TOP = 30.0
PLOT_BOTTOM = 36.0
COARSE_WIDTH = 400
MIN_SPAN_RATIO = 1e-6
MAX_FETCH_WIDTH = 4000

_ABS_RE = re.compile(r"^(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?$")

#: 关闭预览时若后台 ``add_signals`` 还没跑完，先脱离父对象并在这里保活，
#: 避免 QThread 在窗口销毁后被 C++ 析构（等待太久又会卡住关闭按钮）。
_ORPHAN_WORKERS: set = set()


# ---------------------------------------------------------------------------
# 小工具
# ---------------------------------------------------------------------------
def series_color(index: int) -> str:
    """默认调色板（确定性、高对比）。"""
    return PALETTE[int(index) % len(PALETTE)]


def _fmt_num(value, digits: int = 4) -> str:
    """紧凑数值显示（与报告模块同一口径）。"""
    try:
        number = float(value)
    except (TypeError, ValueError):
        return "—"
    if math.isnan(number) or math.isinf(number):
        return "—"
    if number == 0:
        return "0"
    magnitude = abs(number)
    if magnitude >= 1e7 or magnitude < 1e-4:
        return f"{number:.{max(1, digits)}g}"
    text = f"{number:.{max(1, digits)}f}".rstrip("0").rstrip(".")
    return text or "0"


def _fmt_points(count: int) -> str:
    """``3.2k`` / ``1.2M`` 这类紧凑点数。"""
    count = int(max(0, count))
    if count >= 1_000_000:
        return f"{count / 1_000_000:.1f}M"
    if count >= 1_000:
        return f"{count / 1_000:.1f}k"
    return str(count)


def _fmt_mib(value: float) -> str:
    return f"{float(value) / (1024.0 * 1024.0):.0f}"


def format_axis_time(t: float, time_format: str, t_min_str: str | None) -> str:
    """按当前 ``time_format`` 口径把相对秒 ``t`` 显示成人话。

    ``absolute`` 用 ``t_min_str``（BLF 头墙钟）加偏移换算；头里没有墙钟时退回相对秒。
    """
    if time_format == "absolute" and t_min_str:
        match = _ABS_RE.match(str(t_min_str))
        if match:
            year, month, day, hour, minute, second = (int(g) for g in match.groups()[:6])
            frac = match.group(7) or ""
            micro = int((frac + "000000")[:6]) if frac else 0
            base = datetime(year, month, day, hour, minute, second, micro)
            return (base + timedelta(seconds=float(t))).strftime("%Y-%m-%d %H:%M:%S.%f")
    if time_format == "relative_ms":
        return f"{float(t) * 1000.0:.3f} ms"
    if time_format == "unix_s":
        return f"{float(t):.6f} s (unix)"
    return f"{float(t):.3f} s"


def format_tick_time(t: float, time_format: str, t_min_str: str | None) -> str:
    """刻度标签：绝对时间只显示时分秒，避免在窄轴上塞满日期。"""
    if time_format == "absolute" and t_min_str:
        text = format_axis_time(t, time_format, t_min_str)
        return text[11:23] if len(text) >= 23 else text
    if time_format == "relative_ms":
        return f"{float(t) * 1000.0:.1f}"
    if time_format == "unix_s":
        return f"{float(t):.3f}"
    return f"{float(t):.3f}"


def _finite(values) -> bool:
    try:
        return math.isfinite(float(values))
    except (TypeError, ValueError):
        return False


def _series_key(item: dict) -> str:
    """为 ``series_for_plot`` 的返回项构造与 PlotSession 一致的 key。"""
    key = item.get("key")
    if key:
        return str(key)
    return (f"ch{item.get('channel', '?')}/{item.get('message', '?')}/"
            f"{item.get('signal', '?')}")


def _allowed_keys_from_selection(selection: dict | None, info: dict) -> list[str]:
    """把 ``selected`` 展开成 key 列表；空 dict = 全部。"""
    info_keys = [str(item.get("key")) for item in (info.get("signals") or [])
                 if item.get("key")]
    if not selection:
        return info_keys
    wanted: set[str] = set()
    for channel, messages in (selection or {}).items():
        if not isinstance(messages, dict):
            continue
        for message, signals in messages.items():
            for signal in (signals or []):
                wanted.add(f"ch{channel}/{message}/{signal}")
    ordered = [key for key in info_keys if key in wanted]
    # 选择里的 key 可能不在 info（理论上不应发生）：保留 info 里全部 key，避免白屏。
    return ordered or info_keys


def _settings_path() -> Path | None:
    """预览偏好的旁路存储（不污染 ``*.json`` 预设扫描）。"""
    try:
        try:
            import presets as preset_store  # type: ignore
        except ImportError:
            from gui import presets as preset_store  # type: ignore
        return Path(preset_store.config_dir()) / "preview_view.v1"
    except Exception:  # noqa: BLE001 - 偏好存储失败绝不能影响预览
        return None


def load_view_settings() -> dict:
    """读取上次的显示模式 / 颜色 / 线型 / 可见性（跨进程）。坏文件静默忽略。"""
    path = _settings_path()
    if path is None:
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def save_view_settings(settings: dict) -> None:
    """保存预览偏好；失败静默（预览功能不受影响）。"""
    path = _settings_path()
    if path is None:
        return
    tmp = path.with_name(path.name + ".tmp")
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(json.dumps(settings, ensure_ascii=False, indent=2) + "\n",
                       encoding="utf-8")
        os.replace(tmp, path)
    except OSError:
        try:
            tmp.unlink()
        except OSError:
            pass


# ---------------------------------------------------------------------------
# 会话数据缓存
# ---------------------------------------------------------------------------
@dataclass
class SeriesCache:
    """一个信号在当前窗口的抽稀数组（引用同一份 ``times`` 时零拷贝）。"""

    key: str
    times: list
    mins: list
    maxs: list
    means: list


@dataclass
class SeriesStyle:
    color: str
    line_style: str = "line"


# ---------------------------------------------------------------------------
# 控件：三态复选框（点击语义 = 用户要的"一个框搞定"）
# ---------------------------------------------------------------------------
class TriStateCheckBox(QCheckBox):
    """三态全选框：Checked → 全不选；Partially/Unchecked → 全选。"""

    def __init__(self, text: str = "", parent: QWidget | None = None) -> None:
        super().__init__(text, parent)
        self.setTristate(True)

    def nextCheckState(self) -> None:  # noqa: N802 - Qt 命名
        # 禁止 Qt 默认的 Checked→Partially→Unchecked 三态循环：用户只要两态动作。
        if self.checkState() == Qt.CheckState.Checked:
            self.setCheckState(Qt.CheckState.Unchecked)
        else:
            self.setCheckState(Qt.CheckState.Checked)


class SignalRow(QWidget):
    """一行：可见性勾选框 + 颜色色块 + 名字 / 单位 / 缓存状态。"""

    toggled = Signal(str, bool)
    color_clicked = Signal(str)
    context_requested = Signal(str, object)

    def __init__(self, signal: dict, style: SeriesStyle, visible: bool,
                 parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.key = str(signal.get("key") or "")
        self._signal = dict(signal)
        self._guard = False
        self.setObjectName("signalRow")
        row = QHBoxLayout(self)
        row.setContentsMargins(4, 2, 4, 2)
        row.setSpacing(6)

        self.check = QCheckBox(self)
        self.check.setObjectName("signalVisible")
        self.check.setChecked(bool(visible))
        self.check.toggled.connect(self._on_toggled)
        row.addWidget(self.check, 0)

        self.color_button = QPushButton(self)
        self.color_button.setObjectName("colorSwatch")
        self.color_button.setFixedSize(22, 18)
        self.color_button.setToolTip("点击修改曲线颜色（QColorDialog）")
        self.color_button.clicked.connect(lambda: self.color_clicked.emit(self.key))
        row.addWidget(self.color_button, 0)

        self.name_label = QLabel(self)
        self.name_label.setObjectName("signalName")
        self.name_label.setTextFormat(Qt.TextFormat.PlainText)
        row.addWidget(self.name_label, 1)
        self._line_style = style.line_style
        self.set_color(style.color)
        self.set_signal(signal, style)
        self.setToolTip(self._tooltip())
        for child in (self.check, self.color_button, self.name_label):
            child.installEventFilter(self)
            child.setContextMenuPolicy(Qt.ContextMenuPolicy.NoContextMenu)

    def eventFilter(self, watched, event) -> bool:  # noqa: N802 - Qt 命名
        if event.type() == QEvent.Type.ContextMenu:
            self.context_requested.emit(self.key, event.globalPos())
            return True
        return super().eventFilter(watched, event)

    def contextMenuEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        self.context_requested.emit(self.key, event.globalPos())

    def _tooltip(self) -> str:
        item = self._signal
        unit = (item.get("unit") or "").strip() or "无量纲"
        cached = "已缓存" if item.get("cached") else "未缓存（勾选后后台加载）"
        return (f"通道 Ch{item.get('channel', '?')} / {item.get('message', '?')}\n"
                f"单位：{unit}\n缓存：{cached}\n右键：折线 / 阶梯 / 散点")

    def set_color(self, color: str) -> None:
        self.color_button.setStyleSheet(
            f"QPushButton#colorSwatch {{ background-color: {color}; "
            f"border: 1px solid #6B6B7C; border-radius: 4px; }}")
        self._color = color

    def set_signal(self, signal: dict, style: SeriesStyle,
                   normalized: bool = False) -> None:
        self._signal = dict(signal)
        self._line_style = style.line_style
        unit = (signal.get("unit") or "").strip()
        name = str(signal.get("signal") or "?")
        suffix = f"  [{unit}]" if unit else ""
        if normalized:
            suffix += "  （归一化）"
        self.name_label.setText(f"{name}{suffix}")
        marker = "" if signal.get("cached") else " · 未缓存"
        self.name_label.setToolTip(
            f"{signal.get('message', '?')} · {LINE_STYLE_LABELS.get(style.line_style, '折线')}"
            f"{marker}\n" + self._tooltip())
        self.setToolTip(self._tooltip())

    def set_visible(self, visible: bool) -> None:
        self._guard = True
        try:
            self.check.setChecked(bool(visible))
        finally:
            self._guard = False

    def is_visible(self) -> bool:
        return self.check.isChecked()

    def _on_toggled(self, checked: bool) -> None:
        if not self._guard:
            self.toggled.emit(self.key, bool(checked))


# ---------------------------------------------------------------------------
# 右侧信号面板：一个三态复选框 + 名字过滤 + 每行色块 / 线型 / 可见性
# ---------------------------------------------------------------------------
class SignalPanel(QWidget):
    """信号面板（同时承担图例职责：颜色、线型、可见性、归一化标注）。"""

    all_toggled = Signal(bool)
    visibility_toggled = Signal(str, bool)
    color_changed = Signal(str, str)
    line_style_changed = Signal(str, str)

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("signalPanel")
        self.setMinimumWidth(300)
        self.setMaximumWidth(360)
        self._rows: dict[str, SignalRow] = {}
        self._items: dict[str, QListWidgetItem] = {}
        self._order: list[str] = []
        self._guard = False

        outer = QVBoxLayout(self)
        outer.setContentsMargins(8, 8, 8, 8)
        outer.setSpacing(6)

        self.chk_all = TriStateCheckBox("全选（0/0）", self)
        self.chk_all.setObjectName("plotSelectAll")
        self.chk_all.clicked.connect(self._on_all_clicked)
        outer.addWidget(self.chk_all)

        self.filter_edit = QLineEdit(self)
        self.filter_edit.setObjectName("plotFilter")
        self.filter_edit.setPlaceholderText("按名字过滤…（不影响全选统计）")
        self.filter_edit.setClearButtonEnabled(True)
        self.filter_edit.textChanged.connect(self._apply_filter)
        outer.addWidget(self.filter_edit)

        self.list = QListWidget(self)
        self.list.setObjectName("plotSignals")
        self.list.setAlternatingRowColors(True)
        self.list.setUniformItemSizes(False)
        self.list.setSelectionMode(QListWidget.SelectionMode.SingleSelection)
        self.list.setContextMenuPolicy(Qt.ContextMenuPolicy.CustomContextMenu)
        self.list.customContextMenuRequested.connect(self._context_menu)
        outer.addWidget(self.list, 1)

        self.hint = QLabel("行首勾选 = 显示/隐藏；色块改颜色；右键改线型", self)
        self.hint.setObjectName("hintLabel")
        self.hint.setWordWrap(True)
        outer.addWidget(self.hint)

    # ----------------------------------------------------------- 构建 ----
    def set_signals(self, signals: list[dict], styles: dict[str, SeriesStyle],
                    visible: dict[str, bool], normalized: set[str] | None = None) -> None:
        """重建列表（只在打开会话 / 恢复预设时调用，不参与交互热路径）。"""
        normalized = normalized or set()
        self._guard = True
        try:
            self.list.clear()
            self._rows.clear()
            self._items.clear()
            self._order = [str(item.get("key")) for item in signals]
            for item in signals:
                key = str(item.get("key"))
                style = styles.get(key) or SeriesStyle(series_color(len(self._rows)))
                row = SignalRow(item, style, bool(visible.get(key, False)), self)
                row.toggled.connect(self._on_row_toggled)
                row.color_clicked.connect(self._pick_color)
                row.context_requested.connect(self._show_context_menu)
                row.set_signal(item, style, key in normalized)
                list_item = QListWidgetItem(self.list)
                list_item.setData(Qt.ItemDataRole.UserRole, key)
                list_item.setSizeHint(row.sizeHint())
                self.list.addItem(list_item)
                self.list.setItemWidget(list_item, row)
                self._rows[key] = row
                self._items[key] = list_item
        finally:
            self._guard = False
        self._sync_tristate()
        self._apply_filter(self.filter_edit.text())

    def set_visible(self, key: str, visible: bool) -> None:
        row = self._rows.get(key)
        if row is None:
            return
        self._guard = True
        try:
            row.set_visible(visible)
        finally:
            self._guard = False
        self._sync_tristate()
        self.visibility_toggled.emit(key, bool(visible))

    def select_all(self) -> None:
        self._set_all(True)

    def select_none(self) -> None:
        self._set_all(False)

    def _set_all(self, visible: bool) -> None:
        if not self._rows:
            self._sync_tristate()
            return
        self._guard = True
        try:
            for row in self._rows.values():
                row.set_visible(visible)
        finally:
            self._guard = False
        self._sync_tristate()
        self.all_toggled.emit(bool(visible))

    def _on_all_clicked(self, checked: bool) -> None:
        if self._guard:
            return
        # 用户点击后 Qt 已按 nextCheckState 把状态设好：True=全选，False=全不选。
        self._set_all(bool(checked))

    def _on_row_toggled(self, key: str, checked: bool) -> None:
        if self._guard:
            return
        self._sync_tristate()
        self.visibility_toggled.emit(key, bool(checked))

    def _sync_tristate(self) -> None:
        total = len(self._rows)
        checked = sum(1 for row in self._rows.values() if row.is_visible())
        if total and checked == total:
            state = Qt.CheckState.Checked
        elif checked == 0:
            state = Qt.CheckState.Unchecked
        else:
            state = Qt.CheckState.PartiallyChecked
        self.chk_all.blockSignals(True)
        try:
            self.chk_all.setTristate(True)
            self.chk_all.setCheckState(state)
            self.chk_all.setText(f"全选（{checked}/{total}）")
        finally:
            self.chk_all.blockSignals(False)

    def checked_count(self) -> int:
        return sum(1 for row in self._rows.values() if row.is_visible())

    def total_count(self) -> int:
        return len(self._rows)

    def check_state(self) -> Qt.CheckState:
        return self.chk_all.checkState()

    def set_color(self, key: str, color: str) -> None:
        row = self._rows.get(key)
        if row is not None:
            row.set_color(color)

    def set_line_style(self, key: str, style: str) -> None:
        row = self._rows.get(key)
        if row is not None:
            row.set_signal(row._signal, SeriesStyle(row._color, style))

    def set_normalized(self, keys: set[str]) -> None:
        for key, row in self._rows.items():
            row.set_signal(row._signal, SeriesStyle(row._color, row._line_style),
                           key in keys)

    def update_cache(self, signals: list[dict]) -> None:
        for item in signals:
            key = str(item.get("key"))
            row = self._rows.get(key)
            if row is None:
                continue
            row.set_signal(item, SeriesStyle(row._color, row._line_style))

    def _apply_filter(self, text: str) -> None:
        keyword = (text or "").strip().lower()
        for key, item in self._items.items():
            signal = self._rows[key]._signal
            haystack = " ".join(str(signal.get(name) or "") for name in
                                ("signal", "message", "unit", "channel")).lower()
            item.setHidden(bool(keyword) and keyword not in haystack)

    def _pick_color(self, key: str) -> None:
        row = self._rows.get(key)
        current = QColor(row._color if row is not None else series_color(0))
        chosen = QColorDialog.getColor(current, self, "选择曲线颜色")
        if chosen.isValid():
            self.set_color(key, chosen.name())
            self.color_changed.emit(key, chosen.name())

    def _context_menu(self, pos) -> None:
        item = self.list.itemAt(pos)
        if item is None:
            return
        self._show_context_menu(str(item.data(Qt.ItemDataRole.UserRole)),
                                self.list.viewport().mapToGlobal(pos))

    def _show_context_menu(self, key: str, global_pos) -> None:
        row = self._rows.get(key)
        if row is None:
            return
        menu = QMenu(self)
        current = row._line_style
        for label, value in LINE_STYLES:
            action = menu.addAction(("✓ " if value == current else "   ") + label)
            action.setData(value)
        chosen = menu.exec(global_pos)
        if chosen is None:
            return
        value = str(chosen.data())
        if value:
            self.set_line_style(key, value)
            self.line_style_changed.emit(key, value)


# ---------------------------------------------------------------------------
# 画布：纯自绘、只读缓存
# ---------------------------------------------------------------------------
class PlotCanvas(QWidget):
    """min/max 包络自绘控件（三种模式 / 滚轮缩放 / 拖动平移 / 双击复位 / 十字线）。"""

    view_refreshed = Signal()

    def __init__(self, parent: QWidget | None = None, *,
                 time_format: str = "relative_s") -> None:
        super().__init__(parent)
        self.setObjectName("plotCanvas")
        self.setMouseTracking(True)
        self.setMinimumSize(320, 220)
        self.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Expanding)
        self.setAutoFillBackground(True)

        self._session = None
        self._info: dict = {}
        self._config: dict = {}
        self._static_data: dict | None = None
        self._signal_meta: dict[str, dict] = {}
        self._keys: list[str] = []
        self._stats: dict[str, dict] = {}
        self._styles: dict[str, SeriesStyle] = {}
        self._visible: dict[str, bool] = {}
        self._active_keys: set[str] = set()
        self._cache: dict[str, SeriesCache] = {}

        self._time_format = time_format or "relative_s"
        self._time_choices = list(TIME_FORMAT_CHOICES)
        self._t_min_str: str | None = None
        self._t_max_str: str | None = None
        self._raw_min = 0.0
        self._raw_max = 1.0
        self._full: list[float] = [0.0, 1.0]
        self._view: list[float] = [0.0, 1.0]
        self._mode = "lanes"
        self._show_mean = True

        self._hover_t: float | None = None
        self._hover_pos: QPointF | None = None
        self._drag_anchor: QPointF | None = None
        self._drag_view = [0.0, 1.0]
        self._plot_rect = QRectF()

        self._geometry: dict[str, dict] = {}
        self._data_version = 0
        self._layout_version = 0
        self._last_width = -1
        self._last_window_size: tuple[int, int] | None = None
        self._last_fetch_width = 0
        self._view_call_count = 0
        self._last_error: str | None = None

        self._refine_timer = QTimer(self)
        self._refine_timer.setSingleShot(True)
        self._refine_timer.timeout.connect(self._refine_now)
        self._coarse = False

    # ----------------------------------------------------------- 属性 ----
    @property
    def view_call_count(self) -> int:
        """``session.view()`` 调用次数（自检用来证明切模式 / 切口径不取数）。"""
        return self._view_call_count

    @property
    def session(self):
        return self._session

    @property
    def mode(self) -> str:
        return self._mode

    @property
    def time_format(self) -> str:
        return self._time_format

    def keys(self) -> list[str]:
        return list(self._keys)

    def visible_keys(self) -> list[str]:
        return [key for key in self._keys if self._visible.get(key)]

    def active_keys(self) -> list[str]:
        return [key for key in self._keys if key in self._active_keys]

    def visible_count(self) -> int:
        return len(self.visible_keys())

    def all_visible(self) -> bool:
        return bool(self._keys) and all(self._visible.get(key) for key in self._keys)

    def any_visible(self) -> bool:
        return any(self._visible.get(key) for key in self._keys)

    def style(self, key: str) -> SeriesStyle:
        return self._styles.get(key) or SeriesStyle(series_color(self._keys.index(key)
                                                                 if key in self._keys else 0))

    def stats(self, key: str) -> dict:
        return self._stats.get(key) or {}

    # ---------------------------------------------- 会话 / 静态数据注入 ----
    def set_session(self, session, info: dict, stats: list[dict] | None = None,
                    *, allowed_keys: list[str] | None = None,
                    initial_keys: list[str] | None = None,
                    format_meta: dict | None = None,
                    styles: dict[str, SeriesStyle] | None = None,
                    settings: dict | None = None) -> None:
        """注入 ``PlotSession`` 并完成首次取数（只在打开会话时调用）。"""
        self._session = session
        self._static_data = None
        self._info = dict(info or {})
        self._signal_meta = {str(item.get("key")): dict(item)
                             for item in (self._info.get("signals") or [])
                             if item.get("key")}
        allowed = list(allowed_keys or self._signal_meta.keys())
        allowed_set = set(allowed)
        info_order = [key for key in self._signal_meta if key in allowed_set]
        first = [str(key) for key in (initial_keys or []) if str(key) in allowed_set]
        first_set = set(first)
        self._keys = first + [key for key in info_order if key not in first_set]
        if not self._keys:
            self._keys = info_order
        self._stats = {str(item.get("key")): dict(item)
                       for item in (stats or []) if item.get("key")}
        self._styles = dict(styles or {})
        for index, key in enumerate(self._keys):
            self._styles.setdefault(key, SeriesStyle(series_color(index)))
        self._visible = {key: False for key in self._keys}
        self._active_keys = set(first)
        self._apply_format_meta(format_meta)
        self._apply_session_range(self._info)
        self._set_initial_visible(initial_keys or self._keys)
        self._cache.clear()
        self._geometry.clear()
        self._data_version += 1
        self._layout_version += 1
        self._apply_settings_to_style(settings or {})
        self._update_min_height()
        self._request_view(self._actual_width(), force=True)

    def set_static_data(self, data: dict | None, time_format: str | None = None,
                        settings: dict | None = None) -> None:
        """静态回退通道（"不建缓存继续"）：一份一次性抽稀数据，不持有会话。"""
        self._session = None
        self._static_data = dict(data or {})
        if time_format:
            self._time_format = str(time_format)
        self._info = {}
        self._config = {}
        series = list(self._static_data.get("series") or [])
        self._signal_meta = {}
        self._keys = []
        for item in series:
            key = _series_key(item)
            self._keys.append(key)
            self._signal_meta[key] = {
                "key": key,
                "channel": item.get("channel"),
                "message": item.get("message"),
                "signal": item.get("signal"),
                "unit": item.get("unit"),
                "cached": True,
            }
        self._stats = {}
        for key, item in zip(self._keys, series):
            self._stats[key] = {
                "key": key, "count": int(item.get("count") or 0),
                "vmin": item.get("vmin"), "vmax": item.get("vmax"),
                "mean": item.get("mean_all"), "first": item.get("first"),
                "last": item.get("last"),
            }
        self._styles = {}
        for index, key in enumerate(self._keys):
            self._styles[key] = SeriesStyle(series_color(index))
        self._visible = {key: False for key in self._keys}
        self._active_keys = set(self._keys)
        self._t_min_str = self._static_data.get("t_min_str")
        self._t_max_str = self._static_data.get("t_max_str")
        # 静态数据没有 session 的"unix 偏移"语义：t 数组已经是当前口径坐标。
        self._raw_min = 0.0
        self._raw_max = float(self._static_data.get("t_max") or 0.0)
        self._full = [float(self._static_data.get("t_min") or 0.0),
                      float(self._static_data.get("t_max") or 0.0)]
        if self._full[1] <= self._full[0]:
            self._full[1] = self._full[0] + 1.0
        self._set_initial_window()
        self._set_initial_visible(self._keys)
        self._cache.clear()
        self._geometry.clear()
        self._data_version += 1
        self._layout_version += 1
        self._apply_settings_to_style(settings or {})
        self._update_min_height()
        self._request_view(self._actual_width(), force=True)
        self.update()

    def close_session(self) -> None:
        """关闭会话句柄（缓存文件保留，供下次 ``open_plot_session`` 命中）。"""
        session = self._session
        self._session = None
        if session is not None:
            try:
                session.close()
            except Exception:  # noqa: BLE001 - 关闭阶段不允许把异常抛给 UI
                pass

    # ----------------------------------------------------------- 数据范围 ----
    def _apply_format_meta(self, meta: dict | None) -> None:
        meta = meta or {}
        self._raw_min = float(meta.get("raw_min") or 0.0)
        self._raw_max = float(meta.get("raw_max") or (self._raw_min + 1.0))
        self._t_min_str = meta.get("abs_min_str") or meta.get("t_min_str")
        self._t_max_str = meta.get("abs_max_str") or meta.get("t_max_str")

    def _apply_session_range(self, info: dict) -> None:
        t_min = float(info.get("t_min") or 0.0)
        t_max = float(info.get("t_max") or 0.0)
        if self._time_format == "unix_s":
            self._raw_min = t_min
            self._raw_max = t_max if t_max > t_min else t_min + 1.0
            self._full = [0.0, self._raw_max - self._raw_min]
            self._t_min_str = info.get("t_min_str")
            self._t_max_str = info.get("t_max_str")
        else:
            self._full = [0.0, t_max - t_min] if t_max > t_min else [0.0, 1.0]
            if info.get("t_min_str"):
                self._t_min_str = info.get("t_min_str")
                self._t_max_str = info.get("t_max_str")
        if self._full[1] <= self._full[0]:
            self._full[1] = self._full[0] + 1.0
        self._set_initial_window()

    def _set_initial_window(self) -> None:
        """E3：初始窗口 = 最前 2 秒（或全量 2%，取小者），绝不是全域。"""
        span = max(1e-12, self._full[1] - self._full[0])
        initial = min(2.0, span * 0.02)
        if not (initial > 0):
            initial = span
        self._view = [self._full[0], min(self._full[1], self._full[0] + initial)]
        if self._view[1] <= self._view[0]:
            self._view = [self._full[0], self._full[1]]

    def _set_initial_visible(self, ordered_keys: list[str]) -> None:
        """E4：初始最多 3 条，按传入的勾选顺序取前 3。"""
        chosen: list[str] = []
        for key in ordered_keys:
            if key in self._keys and key not in chosen:
                chosen.append(key)
            if len(chosen) >= 3:
                break
        for key in self._keys:
            self._visible[key] = key in chosen

    def _apply_settings_to_style(self, settings: dict) -> None:
        styles = settings.get("styles") if isinstance(settings, dict) else None
        if isinstance(styles, dict):
            for key, value in styles.items():
                if key not in self._styles or not isinstance(value, dict):
                    continue
                color = value.get("color")
                line = value.get("line") or value.get("line_style")
                if isinstance(color, str) and QColor(color).isValid():
                    self._styles[key].color = color
                if line in LINE_STYLE_LABELS:
                    self._styles[key].line_style = str(line)
        visible = settings.get("visible") if isinstance(settings, dict) else None
        order = settings.get("order") if isinstance(settings, dict) else None
        if isinstance(visible, list):
            wanted = {str(key) for key in visible}
            known = set(wanted)
            if isinstance(order, list):
                known.update(str(key) for key in order)
            # 只覆盖预设里出现过的 key；用户换了勾选后新增的 key 保留 E4 的初始可见性。
            for key in self._keys:
                if key in known:
                    self._visible[key] = key in wanted
        if isinstance(order, list):
            ordered = [str(key) for key in order if str(key) in self._keys]
            existing = set(ordered)
            self._keys = ordered + [key for key in self._keys if key not in existing]

    # ----------------------------------------------------------- 取数 ----
    def _actual_width(self) -> int:
        return max(100, min(MAX_FETCH_WIDTH, int(self.width())))

    def _coarse_width(self) -> int:
        return max(100, min(COARSE_WIDTH, self._actual_width()))

    def _display_range(self) -> tuple[float, float]:
        offset = self._raw_min if self._time_format == "unix_s" else 0.0
        return self._view[0] + offset, self._view[1] + offset

    def request_view(self, *, coarse: bool = False) -> None:
        """请求一次取数（仅窗口变化 / 新信号加载后调用，不在 paintEvent 里）。"""
        width = self._coarse_width() if coarse else self._actual_width()
        self._request_view(width, force=True)

    def _request_view(self, width_px: int, *, force: bool = False) -> None:
        width_px = max(1, int(width_px))
        self._last_fetch_width = width_px
        if self._session is not None:
            t0, t1 = self._display_range()
            try:
                result = self._session.view(float(t0), float(t1), width_px)
            except Exception as exc:  # noqa: BLE001 - 视图失败不能打断窗口
                self._last_error = f"{type(exc).__name__}: {exc}"
                result = {}
            self._view_call_count += 1
            self._cache = self._cache_from_view(result)
        elif self._static_data is not None:
            self._cache = self._cache_from_static()
        else:
            self._cache = {}
        self._data_version += 1
        self._geometry.clear()
        self._layout_version += 1
        self.view_refreshed.emit()
        self.update()

    def _cache_from_view(self, result: dict) -> dict[str, SeriesCache]:
        times = list(result.get("t") or [])
        cache: dict[str, SeriesCache] = {}
        for item in (result.get("series") or []):
            key = str(item.get("key") or "")
            if not key:
                continue
            cache[key] = SeriesCache(
                key, times,
                list(item.get("min") or []),
                list(item.get("max") or []),
                list(item.get("mean") or []),
            )
        return cache

    def _cache_from_static(self) -> dict[str, SeriesCache]:
        cache: dict[str, SeriesCache] = {}
        data = self._static_data or {}
        for item in (data.get("series") or []):
            key = _series_key(item)
            times = [float(value) for value in (item.get("t") or [])]
            if not times:
                continue
            start = max(0, bisect.bisect_left(times, self._view[0]) - 1)
            end = min(len(times), bisect.bisect_right(times, self._view[1]) + 2)
            cache[key] = SeriesCache(
                key, times[start:end],
                list(item.get("min") or [])[start:end],
                list(item.get("max") or [])[start:end],
                list(item.get("mean") or [])[start:end],
            )
        return cache

    def _schedule_refine(self, delay_ms: int = 120) -> None:
        if self._session is None:
            return
        self._coarse = True
        self._refine_timer.start(max(1, int(delay_ms)))

    def _refine_now(self) -> None:
        self._coarse = False
        if self._session is not None:
            self._request_view(self._actual_width(), force=True)

    # ----------------------------------------------------------- 视图操作 ----
    def _prune_view(self) -> None:
        full = max(1e-12, self._full[1] - self._full[0])
        min_span = full * MIN_SPAN_RATIO
        span = self._view[1] - self._view[0]
        if span <= 0 or not math.isfinite(span):
            self._view = [self._full[0], self._full[1]]
            return
        span = min(max(span, min_span), full)
        start = self._view[0]
        if start < self._full[0]:
            start = self._full[0]
        if start + span > self._full[1]:
            start = self._full[1] - span
        if start < self._full[0]:
            start = self._full[0]
        self._view = [start, start + span]

    def zoom_at(self, factor: float, x: float | None = None, *, fetch: bool = True,
                coarse: bool = True) -> None:
        """以画布 x 坐标为中心缩放（滚轮 / 自检共用），默认粗粒度取数。"""
        if self._mode == "lanes":
            rect = self._time_rect()
        else:
            rect = self._overlay_rect()
        if rect.width() <= 1:
            return
        px = rect.center().x() if x is None else float(x)
        fraction = (px - rect.left()) / rect.width()
        fraction = max(0.0, min(1.0, fraction))
        span = self._view[1] - self._view[0]
        anchor = self._view[0] + fraction * span
        new_span = span * float(factor)
        self._view = [anchor - fraction * new_span, anchor + (1.0 - fraction) * new_span]
        self._prune_view()
        if fetch:
            self._request_view(self._coarse_width() if coarse else self._actual_width(),
                               force=True)
            if coarse:
                self._schedule_refine()
        else:
            self.update()

    def pan_pixels(self, delta_px: float, *, fetch: bool = True,
                   coarse: bool = True) -> None:
        """按像素平移（拖动 / 自检共用）。"""
        rect = self._time_rect() if self._mode == "lanes" else self._overlay_rect()
        if rect.width() <= 1:
            return
        span = self._view[1] - self._view[0]
        shift = -float(delta_px) / rect.width() * span
        self._view = [self._view[0] + shift, self._view[1] + shift]
        self._prune_view()
        if fetch:
            self._request_view(self._coarse_width() if coarse else self._actual_width(),
                               force=True)
            if coarse:
                self._schedule_refine()
        else:
            self.update()

    def reset_view(self, *, fetch: bool = True) -> None:
        self._view = [self._full[0], self._full[1]]
        self._prune_view()
        if fetch:
            self._request_view(self._actual_width(), force=True)
        else:
            self.update()

    def set_mode(self, mode: str) -> None:
        """B4：切模式 = 纯布局重算，**不取数**。"""
        if mode not in {value for _label, value in DISPLAY_MODES}:
            mode = "lanes"
        if mode == self._mode:
            return
        self._mode = mode
        self._update_min_height()
        self._invalidate_layout()

    def set_time_format(self, time_format: str) -> float:
        """E2：切换时间口径 → ``session.set_time_format`` + 重绘，**不取数**。

        返回本次 ``set_time_format`` 调用的毫秒数（供自检证明 ≤16ms）。
        """
        time_format = str(time_format or "relative_s")
        if time_format == self._time_format:
            return 0.0
        started = time.perf_counter()
        if self._session is not None:
            try:
                self._session.set_time_format(time_format)
            except Exception as exc:  # noqa: BLE001 - 保留旧口径继续显示
                self._last_error = f"{type(exc).__name__}: {exc}"
                return (time.perf_counter() - started) * 1000.0
        self._time_format = time_format
        self.update()
        return (time.perf_counter() - started) * 1000.0

    def set_series_visible(self, key: str, visible: bool) -> None:
        if key not in self._visible:
            return
        if self._visible[key] == bool(visible):
            return
        self._visible[key] = bool(visible)
        self._update_min_height()
        self._invalidate_layout()

    def set_all_visible(self, visible: bool) -> None:
        for key in self._keys:
            self._visible[key] = bool(visible)
        self._update_min_height()
        self._invalidate_layout()

    def set_series_color(self, key: str, color: str) -> None:
        if key in self._styles and QColor(color).isValid():
            self._styles[key].color = color
            self._geometry.pop(key, None)
            self._layout_version += 1
            self.update()

    def set_series_line_style(self, key: str, line_style: str) -> None:
        if key in self._styles and line_style in LINE_STYLE_LABELS:
            self._styles[key].line_style = line_style
            self._geometry.pop(key, None)
            self._layout_version += 1
            self.update()

    def set_stats(self, stats: list[dict] | None) -> None:
        for item in (stats or []):
            key = str(item.get("key") or "")
            if key:
                self._stats[key] = dict(item)
        self._invalidate_layout()

    def set_active_keys(self, keys: list[str]) -> None:
        self._active_keys = {str(key) for key in keys}

    def refresh_view(self) -> None:
        self._request_view(self._actual_width(), force=True)

    def _invalidate_layout(self) -> None:
        self._layout_version += 1
        self._geometry.clear()
        self.update()

    # ----------------------------------------------------------- 信息文本 ----
    def window_label(self) -> str:
        fmt = self._time_format
        if fmt == "relative_ms":
            return f"窗口 {self._view[0] * 1000.0:.1f}–{self._view[1] * 1000.0:.1f} ms"
        if fmt == "absolute" and self._t_min_str:
            return (f"窗口 {format_axis_time(self._view[0], fmt, self._t_min_str)}–"
                    f"{format_axis_time(self._view[1], fmt, self._t_min_str)}")
        if fmt == "unix_s":
            lo = self._view[0] + self._raw_min
            hi = self._view[1] + self._raw_min
            return f"窗口 {lo:.3f}–{hi:.3f} s"
        return f"窗口 {self._view[0]:.2f}–{self._view[1]:.2f} s"

    def full_label(self) -> str:
        span = self._full[1] - self._full[0]
        if self._time_format == "relative_ms":
            return f"全量 {span * 1000.0:.1f} ms"
        return f"全量 {span:.1f} s"

    def visible_point_count(self) -> int:
        total = 0
        for key in self.visible_keys():
            cache = self._cache.get(key)
            if cache is None:
                continue
            total += sum(1 for value in cache.mins if _finite(value))
        return total

    def cache_for(self, key: str) -> SeriesCache | None:
        return self._cache.get(key)

    def normalized_keys(self) -> set[str]:
        if self._mode != "multi_axis":
            return set()
        axes, mapping = self._multi_axes()
        return {key for key, index in mapping.items()
                if index < len(axes) and axes[index].get("normalized")}

    def mixed_units(self) -> bool:
        units = set()
        for key in self.visible_keys():
            unit = (self._signal_meta.get(key, {}).get("unit") or "").strip()
            if unit:
                units.add(unit)
        return len(units) > 1

    # ----------------------------------------------------------- 布局 ----
    def _time_rect(self) -> QRectF:
        if self._mode == "lanes":
            return QRectF(LANE_LEFT, PLOT_TOP, max(10.0, self.width() - LANE_LEFT - 16.0),
                          max(10.0, self.height() - PLOT_TOP - PLOT_BOTTOM))
        return self._overlay_rect()

    def _overlay_rect(self) -> QRectF:
        axes, _mapping = self._multi_axes() if self._mode == "multi_axis" else ([], {})
        axis_count = max(1, len(axes)) if self._mode == "multi_axis" else 1
        left = 10.0 + AXIS_WIDTH * axis_count
        return QRectF(left, PLOT_TOP, max(10.0, self.width() - left - 16.0),
                      max(10.0, self.height() - PLOT_TOP - PLOT_BOTTOM))

    def _lane_rects(self) -> list[tuple[str, QRectF]]:
        keys = self.visible_keys()
        if not keys:
            return []
        top, bottom = PLOT_TOP, max(PLOT_TOP + 10.0, self.height() - PLOT_BOTTOM)
        height = (bottom - top) / len(keys)
        gap = min(3.0, max(1.0, height * 0.08))
        left = LANE_LEFT
        width = max(10.0, self.width() - left - 16.0)
        return [(key, QRectF(left, top + index * height, width,
                             max(8.0, height - gap)))
                for index, key in enumerate(keys)]

    def _rect_for_key(self, key: str) -> QRectF | None:
        if self._mode != "lanes":
            return self._overlay_rect()
        for lane_key, rect in self._lane_rects():
            if lane_key == key:
                return rect
        return None

    def _series_range(self, key: str) -> tuple[float, float]:
        item = self._stats.get(key) or {}
        lo, hi = item.get("vmin"), item.get("vmax")
        if not (_finite(lo) and _finite(hi)):
            cache = self._cache.get(key)
            if cache is not None:
                vals = [float(v) for v in (cache.mins + cache.maxs) if _finite(v)]
                if vals:
                    lo, hi = min(vals), max(vals)
        if not (_finite(lo) and _finite(hi)):
            return 0.0, 1.0
        lo, hi = float(lo), float(hi)
        if hi < lo:
            lo, hi = hi, lo
        pad = (hi - lo) * 0.05 if hi > lo else max(1.0, abs(lo) * 0.05)
        return lo - pad, hi + pad

    def _multi_axes(self) -> tuple[list[dict], dict[str, int]]:
        groups: dict[str, list[str]] = {}
        order: list[str] = []
        for key in self.visible_keys():
            unit = (self._signal_meta.get(key, {}).get("unit") or "").strip() or "无量纲"
            if unit not in groups:
                groups[unit] = []
                order.append(unit)
            groups[unit].append(key)
        axes: list[dict] = []
        mapping: dict[str, int] = {}
        for unit in order[:4]:
            indexes = groups[unit]
            ranges = [self._series_range(key) for key in indexes]
            lo = min(value[0] for value in ranges)
            hi = max(value[1] for value in ranges)
            if hi <= lo:
                hi = lo + 1.0
            axes.append({"unit": unit, "vmin": lo, "vmax": hi,
                         "normalized": False,
                         "color": self._styles.get(indexes[0], SeriesStyle(series_color(0))).color})
            for key in indexes:
                mapping[key] = len(axes) - 1
        normalized = [key for unit in order[4:] for key in groups[unit]]
        if normalized:
            axes.append({"unit": "归一化 %", "vmin": 0.0, "vmax": 100.0,
                         "normalized": True,
                         "color": self._styles.get(normalized[0], SeriesStyle(series_color(0))).color})
            for key in normalized:
                mapping[key] = len(axes) - 1
        return axes, mapping

    def _y_range(self, key: str) -> tuple[float, float]:
        if self._mode == "lanes":
            return self._series_range(key)
        if self._mode == "multi_axis":
            axes, mapping = self._multi_axes()
            index = mapping.get(key, 0)
            if index < len(axes):
                axis = axes[index]
                if axis.get("normalized"):
                    return self._series_range(key)
                return float(axis["vmin"]), float(axis["vmax"])
            return self._series_range(key)
        keys = self.visible_keys() or [key]
        ranges = [self._series_range(item) for item in keys]
        return min(value[0] for value in ranges), max(value[1] for value in ranges)

    def _update_min_height(self) -> None:
        if self._mode == "lanes":
            count = max(1, self.visible_count())
            desired = int(PLOT_TOP + PLOT_BOTTOM + count * LANE_MIN_HEIGHT)
            self.setMinimumHeight(max(220, desired))
        else:
            self.setMinimumHeight(220)

    # ----------------------------------------------------------- 几何 ----
    def _ensure_geometry(self, key: str) -> dict | None:
        cache = self._cache.get(key)
        rect = self._rect_for_key(key)
        if cache is None or rect is None or rect.width() <= 2 or rect.height() <= 2:
            return None
        entry = self._geometry.get(key)
        if (entry is not None and entry.get("data") == self._data_version
                and entry.get("layout") == self._layout_version):
            return entry
        lo, hi = self._y_range(key)
        if not (math.isfinite(lo) and math.isfinite(hi)) or hi <= lo:
            hi = lo + 1.0
        scale = rect.height() / (hi - lo)
        bottom = rect.bottom()
        x0 = rect.left()
        width = rect.width()
        t0, t1 = self._view
        span = max(1e-12, t1 - t0)
        line = QPainterPath()
        step = QPainterPath()
        mean = QPainterPath()
        scatter = QPolygonF()
        previous: tuple[float, float, float, float] | None = None
        times = cache.times
        mins, maxs, means = cache.mins, cache.maxs, cache.means
        count = min(len(times), len(mins), len(maxs), len(means))
        for index in range(count):
            t = float(times[index])
            vmin, vmax, vmean = mins[index], maxs[index], means[index]
            if not (_finite(vmin) and _finite(vmax)):
                previous = None
                continue
            x = x0 + (t - t0) / span * width
            ymin = bottom - (float(vmin) - lo) * scale
            ymax = bottom - (float(vmax) - lo) * scale
            ymean = bottom - (float(vmean) - lo) * scale if _finite(vmean) else (ymin + ymax) * 0.5
            if previous is None:
                line.moveTo(x, ymin)
                step.moveTo(x, ymin)
                mean.moveTo(x, ymean)
            else:
                prev_x, prev_ymin, _prev_ymax, _prev_mean = previous
                line.lineTo(x, ymin)
                # ZOH：先沿上一桶的值水平走到当前 x，再跳到当前值。
                step.moveTo(prev_x, prev_ymin)
                step.lineTo(x, prev_ymin)
                step.lineTo(x, ymin)
                mean.lineTo(x, ymean)
            line.lineTo(x, ymax)
            step.moveTo(x, ymax)
            step.lineTo(x, ymin)
            scatter.append(QPointF(x, ymean))
            previous = (x, ymin, ymax, ymean)
        entry = {"data": self._data_version, "layout": self._layout_version,
                 "line": line, "step": step, "mean": mean, "scatter": scatter}
        self._geometry[key] = entry
        return entry

    # ----------------------------------------------------------- 绘制 ----
    def paintEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, False)
        painter.fillRect(self.rect(), QColor("#1E1E24"))
        if not self._keys:
            self._draw_center_message(painter, "无可见信号")
            painter.end()
            return
        if not self.any_visible():
            self._draw_center_message(painter, "无可见信号")
            painter.end()
            return
        if self._mode == "lanes":
            self._paint_lanes(painter, event.rect())
        else:
            self._paint_overlay(painter)
        if self._hover_t is not None and self._hover_pos is not None:
            self._draw_crosshair(painter, self._time_rect())
        painter.end()

    def _paint_overlay(self, painter: QPainter) -> None:
        rect = self._overlay_rect()
        self._plot_rect = rect
        if rect.width() <= 12 or rect.height() <= 12:
            return
        if self._mode == "multi_axis":
            axes, mapping = self._multi_axes()
        else:
            axes, mapping = [], {}
        if self._mode == "single_axis":
            lo, hi = self._y_range(self.visible_keys()[0]) if self.visible_keys() else (0.0, 1.0)
            axes = [{"unit": "混合量纲" if self.mixed_units() else
                     ((self._signal_meta.get(self.visible_keys()[0], {}).get("unit") or "数值")
                      if self.visible_keys() else "数值"),
                     "vmin": lo, "vmax": hi, "normalized": False,
                     "color": self._styles.get(self.visible_keys()[0], SeriesStyle("#4FC3F7")).color
                     if self.visible_keys() else "#4FC3F7"}]
        self._draw_grid(painter, rect)
        self._draw_axes(painter, rect, axes)
        self._draw_time_axis(painter, rect, at_top=False)
        painter.save()
        painter.setClipRect(rect)
        for key in self.visible_keys():
            self._draw_series(painter, key)
        painter.restore()
        painter.setPen(QPen(QColor("#3A3A46")))
        painter.drawRect(rect)

    def _paint_lanes(self, painter: QPainter, clip: QRectF) -> None:
        lanes = self._lane_rects()
        if not lanes:
            return
        self._plot_rect = QRectF(LANE_LEFT, PLOT_TOP,
                                 max(10.0, self.width() - LANE_LEFT - 16.0),
                                 max(10.0, self.height() - PLOT_TOP - PLOT_BOTTOM))
        # 共享时间轴放在顶部，竖向滚动时仍能看到自己看的是哪一段。
        self._draw_time_axis(painter, self._plot_rect, at_top=True)
        for index, (key, rect) in enumerate(lanes):
            if rect.bottom() < clip.top() - 2 or rect.top() > clip.bottom() + 2:
                continue
            painter.fillRect(rect, QColor("#22222A" if index % 2 == 0 else "#25252E"))
            self._draw_time_grid(painter, rect)
            painter.setPen(QPen(QColor("#3A3A46")))
            painter.drawRect(rect)
            self._draw_lane_axis(painter, key, rect)
            painter.save()
            painter.setClipRect(rect)
            self._draw_series(painter, key)
            painter.restore()

    def _draw_center_message(self, painter: QPainter, text: str) -> None:
        painter.setPen(QColor("#9A9AB0"))
        painter.drawText(self.rect(), Qt.AlignmentFlag.AlignCenter, text)

    def _draw_time_grid(self, painter: QPainter, rect: QRectF) -> None:
        pen = QPen(QColor("#2A2A33"))
        pen.setStyle(Qt.PenStyle.DotLine)
        painter.setPen(pen)
        for tick in range(TICK_COUNT + 1):
            fraction = tick / TICK_COUNT
            x = rect.left() + fraction * rect.width()
            painter.drawLine(QPointF(x, rect.top()), QPointF(x, rect.bottom()))

    def _draw_grid(self, painter: QPainter, rect: QRectF) -> None:
        pen = QPen(QColor("#2A2A33"))
        pen.setStyle(Qt.PenStyle.DotLine)
        painter.setPen(pen)
        for tick in range(TICK_COUNT):
            fraction = tick / (TICK_COUNT - 1)
            y = rect.bottom() - fraction * rect.height()
            painter.drawLine(QPointF(rect.left(), y), QPointF(rect.right(), y))

    def _draw_axes(self, painter: QPainter, rect: QRectF, axes: list[dict]) -> None:
        font = QFont(self.font())
        font.setPointSizeF(max(6.5, font.pointSizeF() - 1.5))
        painter.setFont(font)
        for axis_index, axis in enumerate(axes):
            x = rect.left() - axis_index * AXIS_WIDTH
            color = QColor(axis.get("color") or "#9A9AB0")
            pen = QPen(color)
            pen.setWidthF(1.0)
            painter.setPen(pen)
            painter.drawLine(QPointF(x, rect.top()), QPointF(x, rect.bottom()))
            lo, hi = float(axis["vmin"]), float(axis["vmax"])
            for tick in range(TICK_COUNT):
                fraction = tick / (TICK_COUNT - 1)
                y = rect.bottom() - fraction * rect.height()
                painter.drawLine(QPointF(x - 4, y), QPointF(x + 4, y))
                value = lo + fraction * (hi - lo)
                label = (f"{value:.0f}%" if axis.get("normalized")
                         else _fmt_num(value, 3))
                painter.drawText(QRectF(x - AXIS_WIDTH + 2, y - 8, AXIS_WIDTH - 10, 16),
                                 int(Qt.AlignmentFlag.AlignRight
                                     | Qt.AlignmentFlag.AlignVCenter), label)
            painter.drawText(QRectF(x - AXIS_WIDTH, rect.top() - 20, AXIS_WIDTH, 16),
                             int(Qt.AlignmentFlag.AlignCenter), str(axis.get("unit") or ""))

    def _draw_lane_axis(self, painter: QPainter, key: str, rect: QRectF) -> None:
        meta = self._signal_meta.get(key, {})
        style = self._styles.get(key) or SeriesStyle(series_color(0))
        lo, hi = self._series_range(key)
        font = QFont(self.font())
        font.setPointSizeF(max(6.5, font.pointSizeF() - 1.5))
        painter.setFont(font)
        color = QColor(style.color)
        painter.setPen(color)
        name = str(meta.get("signal") or key)
        unit = (meta.get("unit") or "").strip()
        title = f"{name}" + (f"  [{unit}]" if unit else "")
        painter.drawText(QRectF(8, rect.top() + 2, LANE_LEFT - 16, 16),
                         int(Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter),
                         title)
        painter.setPen(QColor("#9A9AB0"))
        for tick in range(3):
            fraction = tick / 2.0
            y = rect.bottom() - fraction * rect.height()
            value = lo + fraction * (hi - lo)
            painter.drawText(QRectF(8, y - 8, LANE_LEFT - 18, 16),
                             int(Qt.AlignmentFlag.AlignRight | Qt.AlignmentFlag.AlignVCenter),
                             _fmt_num(value, 3))

    def _draw_time_axis(self, painter: QPainter, rect: QRectF, *, at_top: bool) -> None:
        font = QFont(self.font())
        font.setPointSizeF(max(6.5, font.pointSizeF() - 1.5))
        painter.setFont(font)
        metrics = QFontMetrics(font)
        painter.setPen(QColor("#9A9AB0"))
        span = self._view[1] - self._view[0]
        for tick in range(TICK_COUNT + 1):
            fraction = tick / TICK_COUNT
            t = self._view[0] + fraction * span
            label = format_tick_time(t, self._time_format, self._t_min_str)
            x = rect.left() + fraction * rect.width()
            width = metrics.horizontalAdvance(label) + 8
            left = min(max(rect.left(), x - width / 2.0), rect.right() - width)
            y = (rect.top() - 22.0) if at_top else (rect.bottom() + 5.0)
            painter.drawText(QRectF(left, y, width, 16),
                             int(Qt.AlignmentFlag.AlignCenter), label)

    def _draw_series(self, painter: QPainter, key: str) -> None:
        entry = self._ensure_geometry(key)
        if entry is None:
            return
        style = self._styles.get(key) or SeriesStyle(series_color(0))
        color = QColor(style.color)
        pen = QPen(color)
        pen.setWidthF(1.15)
        pen.setCosmetic(True)
        painter.setPen(pen)
        painter.setBrush(Qt.BrushStyle.NoBrush)
        if style.line_style == "step":
            painter.drawPath(entry["step"])
        elif style.line_style == "scatter":
            painter.drawPoints(entry["scatter"])
        else:
            painter.drawPath(entry["line"])
        if self._show_mean and not entry["mean"].isEmpty():
            mean_color = QColor(color)
            mean_color.setAlpha(MEAN_ALPHA)
            mean_pen = QPen(mean_color)
            mean_pen.setWidthF(1.0)
            mean_pen.setCosmetic(True)
            painter.setPen(mean_pen)
            painter.drawPath(entry["mean"])

    def _draw_crosshair(self, painter: QPainter, rect: QRectF) -> None:
        hover_t = self._hover_t
        if hover_t is None or rect.width() <= 1:
            return
        span = max(1e-12, self._view[1] - self._view[0])
        x = rect.left() + (hover_t - self._view[0]) / span * rect.width()
        if not (rect.left() - 1 <= x <= rect.right() + 1):
            return
        pen = QPen(QColor("#7A7A8C"))
        pen.setStyle(Qt.PenStyle.DashLine)
        painter.setPen(pen)
        painter.drawLine(QPointF(x, rect.top()), QPointF(x, rect.bottom()))
        if self._hover_pos is not None:
            y = max(rect.top(), min(rect.bottom(), self._hover_pos.y()))
            painter.drawLine(QPointF(rect.left(), y), QPointF(rect.right(), y))
        lines = [f"时间：{format_axis_time(hover_t, self._time_format, self._t_min_str)}"]
        shown = 0
        for key in self.visible_keys():
            cache = self._cache.get(key)
            if cache is None or not cache.times:
                continue
            index = self._nearest_index(cache.times, hover_t)
            if index < 0:
                continue
            unit = (self._signal_meta.get(key, {}).get("unit") or "").strip()
            name = str(self._signal_meta.get(key, {}).get("signal") or key)
            value = cache.means[index] if index < len(cache.means) else None
            lo = cache.mins[index] if index < len(cache.mins) else None
            hi = cache.maxs[index] if index < len(cache.maxs) else None
            suffix = f" {unit}" if unit else ""
            lines.append(f"● {name}: {_fmt_num(value, 4)} "
                         f"({_fmt_num(lo, 4)} ~ {_fmt_num(hi, 4)}){suffix}")
            shown += 1
            if shown >= 10:
                lines.append(f"… 还有 {max(0, self.visible_count() - shown)} 条可见信号")
                break
        self._draw_readout(painter, rect, lines)

    def _draw_readout(self, painter: QPainter, rect: QRectF, lines: list[str]) -> None:
        font = QFont(self.font())
        font.setPointSizeF(max(7.0, font.pointSizeF() - 1.0))
        painter.setFont(font)
        metrics = QFontMetrics(font)
        line_height = metrics.height() + 2
        width = max(metrics.horizontalAdvance(line) for line in lines) + 18
        width = min(width, max(160.0, rect.width() - 20))
        height = line_height * len(lines) + 12
        anchor = self._hover_pos or QPointF(rect.left(), rect.top())
        bx = anchor.x() + 14
        by = anchor.y() + 12
        if bx + width > rect.right():
            bx = anchor.x() - width - 14
        if by + height > rect.bottom():
            by = anchor.y() - height - 12
        bx = max(rect.left() + 4, bx)
        by = max(rect.top() + 4, by)
        box = QRectF(bx, by, width, height)
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(QBrush(QColor(37, 37, 46, 235)))
        painter.drawRoundedRect(box, 5, 5)
        painter.setPen(QPen(QColor("#3A3A46")))
        painter.setBrush(Qt.BrushStyle.NoBrush)
        painter.drawRoundedRect(box, 5, 5)
        for offset, line in enumerate(lines):
            color = QColor("#E8E8F0") if offset == 0 else QColor("#C8C8D8")
            if offset > 0 and line.startswith("●"):
                visible = self.visible_keys()
                position = offset - 1
                if 0 <= position < len(visible):
                    style = self._styles.get(visible[position])
                    if style is not None:
                        color = QColor(style.color)
            painter.setPen(color)
            painter.drawText(QRectF(box.left() + 9, box.top() + 6 + offset * line_height,
                                    box.width() - 18, line_height),
                             int(Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter),
                             line)

    @staticmethod
    def _nearest_index(times: list, t: float) -> int:
        if not times:
            return -1
        position = bisect.bisect_left(times, t)
        if position <= 0:
            return 0
        if position >= len(times):
            return len(times) - 1
        before, after = times[position - 1], times[position]
        return position if abs(after - t) < abs(before - t) else position - 1

    # ----------------------------------------------------------- 交互 ----
    def wheelEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        delta = event.angleDelta().y()
        if delta == 0:
            return
        factor = 0.8 if delta > 0 else 1.25
        self.zoom_at(factor, event.position().x(), fetch=True, coarse=True)
        event.accept()

    def mousePressEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        if event.button() == Qt.MouseButton.LeftButton:
            self._drag_anchor = event.position()
            self._drag_view = [self._view[0], self._view[1]]
            self.setCursor(Qt.CursorShape.ClosedHandCursor)
        super().mousePressEvent(event)

    def mouseMoveEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        position = event.position()
        if self._drag_anchor is not None:
            rect = self._time_rect()
            if rect.width() > 1:
                span = self._drag_view[1] - self._drag_view[0]
                shift = -(position.x() - self._drag_anchor.x()) / rect.width() * span
                self._view = [self._drag_view[0] + shift, self._drag_view[1] + shift]
                self._prune_view()
                self._request_view(self._coarse_width(), force=True)
                self._schedule_refine(140)
            return
        rect = self._time_rect()
        fraction = (position.x() - rect.left()) / max(1.0, rect.width())
        if -0.05 <= fraction <= 1.05:
            self._hover_t = self._view[0] + max(0.0, min(1.0, fraction)) * (
                self._view[1] - self._view[0])
            self._hover_pos = position
        else:
            self._hover_t = None
            self._hover_pos = None
        self.update()

    def mouseReleaseEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        if event.button() == Qt.MouseButton.LeftButton:
            was_dragging = self._drag_anchor is not None
            self._drag_anchor = None
            self.setCursor(Qt.CursorShape.ArrowCursor)
            if was_dragging:
                # A3：松手后用实际像素宽度精化一次（取消还在排队的粗粒度精化）。
                self._refine_timer.stop()
                self._coarse = False
                self._request_view(self._actual_width(), force=True)
        super().mouseReleaseEvent(event)

    def mouseDoubleClickEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        if event.button() == Qt.MouseButton.LeftButton:
            self._refine_timer.stop()
            self.reset_view(fetch=True)
        super().mouseDoubleClickEvent(event)

    def leaveEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        self._hover_t = None
        self._hover_pos = None
        self.update()
        super().leaveEvent(event)

    def resizeEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        width = int(event.size().width())
        width_changed = width != self._last_width
        self._last_width = width
        self._invalidate_layout()
        # B4：模式切换可能让竖向滚动条出现/消失，从而改变画布宽度；
        # 这不是"窗口变化"，绝不能因此重新取数。用顶层窗口尺寸区分两者。
        window = self.window()
        window_size = ((int(window.width()), int(window.height()))
                       if window is not None else (width, int(self.height())))
        first_layout = self._last_window_size is None
        window_changed = window_size != self._last_window_size
        self._last_window_size = window_size
        if (width_changed and self._session is not None
                and (window_changed or first_layout)):
            self._schedule_refine(40)
        super().resizeEvent(event)


# ---------------------------------------------------------------------------
# 后台 add_signals：勾选未缓存信号时绝不冻结已有信号的缩放/平移
# ---------------------------------------------------------------------------
class AddSignalsWorker(QThread):
    """在后台线程调用 ``session.add_signals`` / ``stats``，结果通过 Signal 回主线程。"""

    done = Signal(dict)
    failed = Signal(str)

    def __init__(self, session, keys: list[str], parent=None) -> None:
        super().__init__(parent)
        self._session = session
        self._keys = list(keys)

    def run(self) -> None:
        try:
            result = self._session.add_signals(self._keys)
            stats = self._session.stats(self._keys)
            info = self._session.info()
            self.done.emit({"result": dict(result), "stats": stats,
                            "info": info, "keys": list(self._keys)})
        except Exception as exc:  # noqa: BLE001 - 后台失败只提示，不影响已有曲线
            self.failed.emit(f"{type(exc).__name__}: {exc}")


# ---------------------------------------------------------------------------
# 预览窗口
# ---------------------------------------------------------------------------
class PreviewWindow(QWidget):
    """独立预览窗口：顶栏信息 + 画布 + 单复选框信号面板 + 内联缓存横幅。"""

    closed = Signal()
    settings_changed = Signal(dict)
    #: F1：三个动作通过信号交给 main（session 生命周期在 main 里管理）。
    budget_retry_requested = Signal(int)
    reduce_selection_requested = Signal()
    no_cache_requested = Signal()

    def __init__(self, session=None, *, info: dict | None = None,
                 stats: list[dict] | None = None, config: dict | None = None,
                 initial_keys: list[str] | None = None,
                 time_format: str = "relative_s",
                 time_format_choices: list[tuple[str, str]] | None = None,
                 cache_budget_mb: int = 200,
                 format_meta: dict | None = None,
                 static_data: dict | None = None,
                 view_settings: dict | None = None,
                 on_refetch=None,
                 error_message: str | None = None,
                 error_code: str | None = None,
                 parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("plotWindow")
        self.setWindowTitle("📈 数据预览 · PlotSession · min/max 包络")
        self.resize(1280, 760)

        self._session = session
        self._info = dict(info or {})
        self._stats = list(stats or [])
        self._config = dict(config or {})
        self._initial_keys = list(initial_keys or [])
        self._format_meta = dict(format_meta or {})
        self._static_data = static_data
        self._on_refetch = on_refetch
        self._cache_budget_mb = int(cache_budget_mb or 200)
        self._cache_bytes = int(self._info.get("cache_bytes") or 0)
        self._cache_hit = bool(self._info.get("cache_hit"))
        self._time_format = str(time_format or "relative_s")
        self._choices = list(time_format_choices or TIME_FORMAT_CHOICES)
        if view_settings:
            self._view_settings = dict(view_settings)
        else:
            # 跨进程记住"显示模式 / 颜色 / 线型"，但不恢复可见性：
            # E4 要求新开预览默认最多 3 条可见，可见性只由本次勾选 / 命名预设决定。
            saved = load_view_settings()
            self._view_settings = {key: value for key, value in saved.items()
                                   if key not in ("visible", "order")}
        self._error_code = error_code
        self._closed = False
        self._add_worker: AddSignalsWorker | None = None
        self._pending_keys: list[str] = []
        self._prefetch_idle_ticks = 0
        self._loading_keys: set[str] = set()
        self._last_time_format_ms = 0.0

        self._build_ui()
        self._prefetch_timer = QTimer(self)
        self._prefetch_timer.setInterval(250)
        self._prefetch_timer.timeout.connect(self._poll_prefetch)
        self._prefetch_seen_busy = False
        # 设置落盘做 300ms 防抖：勾选 / 改色不该每次都同步写磁盘。
        self._settings_timer = QTimer(self)
        self._settings_timer.setSingleShot(True)
        self._settings_timer.setInterval(300)
        self._settings_timer.timeout.connect(self._save_view_settings_now)

        if error_message:
            self._show_error(error_message, error_code)
        elif static_data is not None:
            self.set_static_data(static_data, time_format=self._time_format,
                                 settings=self._view_settings)
            self.show_notice("未建缓存（较慢）：本次为一次性抽稀数据，"
                             "缩放 / 平移仍可用，但切换时间口径需要重新取数。")
        elif session is not None:
            self.set_session(session, self._info, self._stats, self._config,
                             self._initial_keys, format_meta=self._format_meta,
                             settings=self._view_settings)
        else:
            self._show_error("没有可用的预览会话", error_code)

    # ----------------------------------------------------------- UI ----
    def _build_ui(self) -> None:
        outer = QVBoxLayout(self)
        outer.setContentsMargins(10, 10, 10, 10)
        outer.setSpacing(7)

        top = QFrame(self)
        top.setObjectName("plotTopBar")
        top_layout = QVBoxLayout(top)
        top_layout.setContentsMargins(10, 7, 10, 7)
        top_layout.setSpacing(5)
        row = QHBoxLayout()
        row.setSpacing(8)
        row.addWidget(QLabel("时间口径："))
        self.combo_time = QComboBox(self)
        for label, value in self._choices:
            self.combo_time.addItem(label, value)
        self.combo_time.setToolTip("纯显示层切换：只调 session.set_time_format()，不重新取数")
        self.combo_time.currentIndexChanged.connect(self._on_time_format_changed)
        row.addWidget(self.combo_time)
        row.addSpacing(8)
        row.addWidget(QLabel("显示模式："))
        self.combo_mode = QComboBox(self)
        for label, value in DISPLAY_MODES:
            self.combo_mode.addItem(label, value)
        self.combo_mode.setToolTip("车道 / 多 Y 轴叠加 / 单 Y 轴；切换只重算布局，不取数")
        self.combo_mode.currentIndexChanged.connect(self._on_mode_changed)
        row.addWidget(self.combo_mode)
        row.addStretch(1)
        self.lbl_window = QLabel("窗口 —")
        self.lbl_window.setObjectName("plotWindowLabel")
        self.lbl_full = QLabel("全量 —")
        self.lbl_full.setObjectName("plotFullLabel")
        self.lbl_visible = QLabel("可见 0 条 · 0 点")
        self.lbl_visible.setObjectName("plotVisibleLabel")
        self.lbl_cache = QLabel("缓存 —")
        self.lbl_cache.setObjectName("plotCacheLabel")
        for label in (self.lbl_window, self.lbl_full, self.lbl_visible, self.lbl_cache):
            label.setSizePolicy(QSizePolicy.Policy.Preferred, QSizePolicy.Policy.Preferred)
            row.addWidget(label)
        top_layout.addLayout(row)

        self.lbl_warn = QLabel("")
        self.lbl_warn.setObjectName("plotWarn")
        self.lbl_warn.setWordWrap(True)
        self.lbl_warn.setVisible(False)
        top_layout.addWidget(self.lbl_warn)
        self.lbl_loading = QLabel("")
        self.lbl_loading.setObjectName("plotLoading")
        self.lbl_loading.setWordWrap(True)
        self.lbl_loading.setVisible(False)
        top_layout.addWidget(self.lbl_loading)
        outer.addWidget(top)

        # F1：缓存超额的内联横幅（绝不是 QMessageBox）。
        self.banner = QFrame(self)
        self.banner.setObjectName("plotBanner")
        banner_layout = QVBoxLayout(self.banner)
        banner_layout.setContentsMargins(10, 7, 10, 7)
        banner_layout.setSpacing(5)
        self.banner_label = QLabel("")
        self.banner_label.setObjectName("plotBannerText")
        self.banner_label.setWordWrap(True)
        banner_layout.addWidget(self.banner_label)
        actions = QHBoxLayout()
        actions.setSpacing(8)
        self.btn_raise = QPushButton("提高上限并重试", self)
        self.btn_raise.setObjectName("plotBannerButton")
        self.btn_raise.setToolTip("打开缓存上限设置，用更大的 cache_budget_mb 重试")
        self.btn_raise.clicked.connect(self._on_raise_budget)
        self.btn_reduce = QPushButton("只预览更少报文", self)
        self.btn_reduce.setObjectName("plotBannerButton")
        self.btn_reduce.setToolTip("回到主窗口减少勾选的报文 / 信号后重新预览")
        self.btn_reduce.clicked.connect(self.reduce_selection_requested.emit)
        self.btn_nocache = QPushButton("不建缓存继续（较慢）", self)
        self.btn_nocache.setObjectName("plotBannerButton")
        self.btn_nocache.setToolTip("不建帧缓存，一次性抽稀后继续预览（较慢）")
        self.btn_nocache.clicked.connect(self.no_cache_requested.emit)
        actions.addWidget(self.btn_raise)
        actions.addWidget(self.btn_reduce)
        actions.addWidget(self.btn_nocache)
        actions.addStretch(1)
        banner_layout.addLayout(actions)
        self.banner.setVisible(False)
        outer.addWidget(self.banner)

        body = QHBoxLayout()
        body.setSpacing(8)
        self.scroll = PlotScrollArea(self)
        self.scroll.setObjectName("plotScroll")
        self.scroll.setWidgetResizable(True)
        self.scroll.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        self.canvas = PlotCanvas(self, time_format=self._time_format)
        self.canvas.setObjectName("plotCanvas")
        self.canvas.view_refreshed.connect(self._update_top_info)
        self.scroll.setWidget(self.canvas)
        body.addWidget(self.scroll, 1)
        self.panel = SignalPanel(self)
        self.panel.all_toggled.connect(self._on_all_toggled)
        self.panel.visibility_toggled.connect(self._on_visibility_toggled)
        self.panel.color_changed.connect(self._on_color_changed)
        self.panel.line_style_changed.connect(self._on_line_style_changed)
        body.addWidget(self.panel, 0)
        outer.addLayout(body, 1)

        controls = QHBoxLayout()
        controls.setSpacing(10)
        self.chk_mean = QCheckBox("叠加均值线（半透明）", self)
        self.chk_mean.setChecked(True)
        self.chk_mean.toggled.connect(self._on_show_mean)
        controls.addWidget(self.chk_mean)
        controls.addStretch(1)
        hint = QLabel("滚轮缩放 · 左键拖动平移 · 双击复位 · 十字线读数不取数", self)
        hint.setObjectName("hintLabel")
        controls.addWidget(hint)
        outer.addLayout(controls)

    # ------------------------------------------------- 数据注入 / 同步 ----
    def _allowed_keys(self) -> list[str]:
        return _allowed_keys_from_selection(self._config.get("selected"), self._info)

    def set_session(self, session, info: dict, stats: list[dict] | None = None,
                    config: dict | None = None,
                    initial_keys: list[str] | None = None, *,
                    format_meta: dict | None = None,
                    settings: dict | None = None) -> None:
        self._session = session
        self._info = dict(info or {})
        self._stats = list(stats or [])
        if config is not None:
            self._config = dict(config)
        self._cache_bytes = int(self._info.get("cache_bytes") or 0)
        self._cache_hit = bool(self._info.get("cache_hit"))
        self._format_meta = dict(format_meta or self._format_meta)
        self._initial_keys = list(initial_keys or self._initial_keys)
        allowed = self._allowed_keys()
        first = [key for key in self._initial_keys if key in set(allowed)]
        if not first:
            first = allowed[:3]
        self.canvas.set_session(
            session, self._info, self._stats, allowed_keys=allowed,
            initial_keys=first, format_meta=self._format_meta,
            settings=settings or self._view_settings)
        self._initial_keys = first
        self._sync_panel()
        self._sync_combos()
        self._update_top_info()
        self._update_warning()
        self._schedule_prefetch_poll()
        # 预设里恢复出来的可见信号若尚未激活（例如 96 条里勾了 10 条），后台补加载。
        self._activate_keys(self.canvas.visible_keys())
        self._emit_settings()

    def set_static_data(self, data: dict | None, *, time_format: str | None = None,
                        settings: dict | None = None) -> None:
        self._session = None
        self._static_data = dict(data or {})
        if time_format:
            self._time_format = str(time_format)
        self._info = {}
        self._cache_bytes = 0
        self._cache_hit = False
        self.canvas.set_static_data(self._static_data, self._time_format,
                                    settings=settings or self._view_settings)
        self._sync_panel()
        self._sync_combos()
        self._update_top_info()
        self._update_warning()
        self._sync_prefetch_stopped()
        self._emit_settings()

    def _sync_panel(self) -> None:
        signals = []
        for key in self.canvas.keys():
            meta = dict(self.canvas._signal_meta.get(key) or {})
            meta.setdefault("key", key)
            signals.append(meta)
        styles = {key: self.canvas.style(key) for key in self.canvas.keys()}
        visible = {key: bool(self.canvas._visible.get(key)) for key in self.canvas.keys()}
        self.panel.set_signals(signals, styles, visible, self.canvas.normalized_keys())

    def _sync_combos(self) -> None:
        index = next((i for i, (_label, value) in enumerate(self._choices)
                      if value == self._time_format), 0)
        self.combo_time.blockSignals(True)
        try:
            self.combo_time.setCurrentIndex(index)
        finally:
            self.combo_time.blockSignals(False)
        mode_index = next((i for i, (_label, value) in enumerate(DISPLAY_MODES)
                           if value == self.canvas.mode), 0)
        self.combo_mode.blockSignals(True)
        try:
            self.combo_mode.setCurrentIndex(mode_index)
        finally:
            self.combo_mode.blockSignals(False)

    def _sync_prefetch_stopped(self) -> None:
        self._prefetch_timer.stop()
        self.lbl_loading.setVisible(False)

    def _update_top_info(self) -> None:
        self.lbl_window.setText(self.canvas.window_label())
        self.lbl_full.setText(self.canvas.full_label())
        points = self.canvas.visible_point_count()
        self.lbl_visible.setText(f"可见 {self.canvas.visible_count()} 条 · "
                                 f"{_fmt_points(points)} 点")
        hit = " · 命中" if self._cache_hit else ""
        self.lbl_cache.setText(f"缓存 {_fmt_mib(self._cache_bytes)}/{self._cache_budget_mb} MiB{hit}")
        self.lbl_cache.setToolTip(
            f"cache_bytes={self._cache_bytes} B\ncache_hit={self._cache_hit}\n"
            f"预算={self._cache_budget_mb} MiB")
        self._update_loading_text()

    def _update_loading_text(self) -> None:
        pending = len(self._loading_keys)
        if self._add_worker is not None and self._add_worker.isRunning():
            done = max(0, len(self._add_worker._keys) - pending)
            total = len(self._add_worker._keys)
            text = f"正在加载新信号… {done}/{total}"
        elif pending:
            text = f"正在加载新信号… 0/{pending}"
        else:
            text = ""
        if text:
            self.lbl_loading.setText(text)
            self.lbl_loading.setVisible(True)
        else:
            self.lbl_loading.setVisible(False)

    def _update_warning(self) -> None:
        if self.canvas.mode == "single_axis" and self.canvas.mixed_units():
            self.lbl_warn.setText("⚠ 量纲混杂，仅建议同量纲信号（单 Y 轴会把 km/h 与 degC 画到同一刻度）")
            self.lbl_warn.setVisible(True)
        elif self.canvas.normalized_keys():
            self.lbl_warn.setText("超出 4 个量纲，其余信号已归一化到 0–100%（见图例标注）")
            self.lbl_warn.setVisible(True)
        else:
            self.lbl_warn.setVisible(False)

    # ------------------------------------------------- 模式 / 口径切换 ----
    def _on_time_format_changed(self, index: int) -> None:
        value = self.combo_time.itemData(index)
        if not value or value == self._time_format:
            return
        self.apply_time_format(str(value))

    def apply_time_format(self, value: str) -> None:
        """E2：切口径——会话模式只调 set_time_format；静态回退才重新取数。"""
        if value == self._time_format:
            return
        if self._session is not None:
            self._last_time_format_ms = self.canvas.set_time_format(value)
            self._time_format = self.canvas.time_format
        else:
            self.canvas.set_time_format(value)
            self._time_format = str(value)
            if callable(self._on_refetch):
                self.lbl_loading.setText(f"正在按「{self.combo_time.currentText()}」重新取数…")
                self.lbl_loading.setVisible(True)
                self._on_refetch(str(value))
        self._sync_combos()
        self._update_top_info()

    def _on_mode_changed(self, index: int) -> None:
        value = self.combo_mode.itemData(index)
        if not value or value == self.canvas.mode:
            return
        self.apply_mode(str(value))

    def apply_mode(self, value: str) -> None:
        """B4：切模式只重算布局；不取数。"""
        self.canvas.set_mode(value)
        self._sync_combos()
        self.panel.set_normalized(self.canvas.normalized_keys())
        self._update_top_info()
        self._update_warning()
        self._emit_settings()

    def _on_show_mean(self, checked: bool) -> None:
        self.canvas._show_mean = bool(checked)  # noqa: SLF001 - 同模块控件
        self.canvas.update()

    # ------------------------------------------------- 可见性 / 加载 ----
    def _on_all_toggled(self, checked: bool) -> None:
        self.canvas.set_all_visible(checked)
        if checked:
            self._activate_keys(self.canvas.keys())
        self._update_top_info()
        self._update_warning()
        self.canvas.update()
        self._emit_settings()

    def _on_visibility_toggled(self, key: str, visible: bool) -> None:
        self.canvas.set_series_visible(key, visible)
        if visible:
            self._activate_keys([key])
        self._update_top_info()
        self._update_warning()
        self._emit_settings()

    def _on_color_changed(self, key: str, color: str) -> None:
        self.canvas.set_series_color(key, color)
        self._emit_settings()

    def _on_line_style_changed(self, key: str, style: str) -> None:
        self.canvas.set_series_line_style(key, style)
        self._emit_settings()

    def _activate_keys(self, keys: list[str]) -> None:
        if self._session is None:
            return
        active = set(self.canvas.active_keys())
        wanted = [key for key in keys if key not in active and key not in self._loading_keys]
        if not wanted:
            return
        self._loading_keys.update(wanted)
        self._pending_keys.extend(wanted)
        self._update_loading_text()
        self._start_add_worker()

    def _start_add_worker(self) -> None:
        if self._add_worker is not None and self._add_worker.isRunning():
            return
        if not self._pending_keys or self._session is None:
            return
        keys = list(dict.fromkeys(self._pending_keys))
        self._pending_keys.clear()
        worker = AddSignalsWorker(self._session, keys, self)
        self._add_worker = worker
        worker.done.connect(self._on_add_done)
        worker.failed.connect(self._on_add_failed)
        worker.finished.connect(self._on_add_finished)
        worker.start()
        self._update_loading_text()

    def _on_add_done(self, payload: dict) -> None:
        keys = [str(key) for key in (payload.get("keys") or [])]
        for key in keys:
            self._loading_keys.discard(key)
        self.canvas.set_active_keys(self.canvas.active_keys() + keys)
        self.canvas.set_stats(payload.get("stats") or [])
        info = payload.get("info") or {}
        if info:
            self._info = dict(info)
            self._cache_bytes = int(info.get("cache_bytes") or self._cache_bytes)
            self._cache_hit = bool(info.get("cache_hit", self._cache_hit))
            for item in (info.get("signals") or []):
                key = str(item.get("key"))
                if key in self.canvas._signal_meta:
                    self.canvas._signal_meta[key] = dict(item)
        self.panel.update_cache(self._info.get("signals") or [])
        # 新信号已缓存则立即出现在 view() 里；仍在预取的等预取完成后再刷新。
        self.canvas.refresh_view()
        self._update_top_info()
        self._schedule_prefetch_poll()
        if self._pending_keys:
            self._start_add_worker()

    def _on_add_failed(self, message: str) -> None:
        for key in list(self._loading_keys):
            self._loading_keys.discard(key)
        self.lbl_loading.setText(f"加载新信号失败：{message}")
        self.lbl_loading.setVisible(True)
        print(f"[preview] 加载新信号失败：{message}", file=sys.stderr)

    def _on_add_finished(self) -> None:
        worker = self._add_worker
        self._add_worker = None
        if worker is not None:
            worker.deleteLater()
        if self._pending_keys:
            self._start_add_worker()
        else:
            self._update_loading_text()

    def _schedule_prefetch_poll(self) -> None:
        if self._session is None:
            return
        self._prefetch_idle_ticks = 0
        if not self._prefetch_timer.isActive():
            self._prefetch_timer.start()

    def _poll_prefetch(self) -> None:
        if self._session is None:
            self._prefetch_timer.stop()
            return
        try:
            status = self._session.prefetch_status()
        except Exception:  # noqa: BLE001 - 会话已关闭
            self._prefetch_timer.stop()
            return
        busy = bool(status.get("busy"))
        done = int(status.get("done") or 0)
        total = int(status.get("total") or 0)
        if busy:
            self._prefetch_seen_busy = True
            self._prefetch_idle_ticks = 0
            self.lbl_loading.setText(f"正在加载新信号… {done}/{total}")
            self.lbl_loading.setVisible(True)
            return
        if self._add_worker is not None and self._add_worker.isRunning():
            self._prefetch_idle_ticks = 0
            return
        self._prefetch_idle_ticks += 1
        if self._prefetch_idle_ticks >= 2:
            self._prefetch_timer.stop()
            self._loading_keys.clear()
            self.lbl_loading.setVisible(False)
            if self._prefetch_seen_busy:
                self._prefetch_seen_busy = False
                self._refresh_after_prefetch()

    def _refresh_after_prefetch(self) -> None:
        if self._session is None:
            return
        try:
            info = self._session.info()
            stats = self._session.stats(self.canvas.active_keys())
        except Exception:  # noqa: BLE001 - 会话已关闭
            return
        self._info = dict(info)
        self._cache_bytes = int(info.get("cache_bytes") or 0)
        self._cache_hit = bool(info.get("cache_hit"))
        self.canvas.set_stats(stats)
        for item in (info.get("signals") or []):
            key = str(item.get("key"))
            if key in self.canvas._signal_meta:
                self.canvas._signal_meta[key] = dict(item)
        self.panel.update_cache(info.get("signals") or [])
        self.canvas.refresh_view()
        self._update_top_info()

    # ------------------------------------------------- F1 横幅 / 错误 ----
    def _show_error(self, message: str, code: str | None) -> None:
        self._error_code = code
        self.canvas.setEnabled(False)
        self.panel.setEnabled(False)
        self.combo_mode.setEnabled(False)
        self.combo_time.setEnabled(False)
        self.lbl_window.setText("窗口 —")
        self.lbl_full.setText("全量 —")
        self.lbl_visible.setText("可见 0 条 · 0 点")
        self.lbl_cache.setText(f"缓存 —/{self._cache_budget_mb} MiB")
        self._sync_combos()
        self.banner_label.setText(
            f"<b>预览会话未创建</b><br>{message}")
        self.banner.setVisible(True)
        cache_error = (code == "CACHE_BUDGET_EXCEEDED"
                       or "CACHE_BUDGET_EXCEEDED" in str(message))
        self.btn_raise.setVisible(cache_error)
        self.btn_reduce.setVisible(cache_error)
        self.btn_nocache.setVisible(cache_error)
        if not cache_error:
            self.btn_raise.setVisible(False)
            self.btn_reduce.setText("返回主窗口")
            self.btn_reduce.setVisible(True)
            self.btn_nocache.setVisible(False)

    def show_notice(self, text: str) -> None:
        self.banner_label.setText(text)
        self.banner.setVisible(True)
        self.btn_raise.setVisible(False)
        self.btn_reduce.setVisible(False)
        self.btn_nocache.setVisible(False)

    def _on_raise_budget(self) -> None:
        suggested = max(self._cache_budget_mb * 2, self._cache_budget_mb + 64)
        value, accepted = QInputDialog.getInt(
            self, "提高缓存上限", "设置新的缓存上限（MiB，32–8192）：",
            int(min(8192, suggested)), 32, 8192, 16)
        if accepted:
            self._cache_budget_mb = int(value)
            self.budget_retry_requested.emit(int(value))

    # ------------------------------------------------- 预设 / 自检接口 ----
    def plot_settings(self) -> dict:
        """可序列化的预览设置（Step 18 写入预设；Step 19 可继续扩展）。"""
        styles = {key: {"color": style.color, "line": style.line_style}
                  for key, style in self.canvas._styles.items()}
        return {
            "mode": self.canvas.mode,
            "styles": styles,
            "visible": self.canvas.visible_keys(),
            "order": self.canvas.keys(),
        }

    def apply_plot_settings(self, settings: dict | None) -> None:
        """恢复预设里的模式 / 颜色 / 线型 / 可见性（不取数）。"""
        if not isinstance(settings, dict):
            return
        mode = settings.get("mode")
        if mode in {value for _label, value in DISPLAY_MODES}:
            self.canvas.set_mode(str(mode))
        self.canvas._apply_settings_to_style(settings)  # noqa: SLF001
        self._sync_panel()
        self._sync_combos()
        self._update_top_info()
        self._update_warning()
        self._activate_keys(self.canvas.visible_keys())
        self.canvas.update()

    def _emit_settings(self) -> None:
        try:
            settings = self.plot_settings()
        except RuntimeError:
            return
        self._view_settings = settings
        self.settings_changed.emit(settings)
        self._settings_timer.start()

    def _save_view_settings_now(self) -> None:
        try:
            save_view_settings(self._view_settings)
        except Exception:  # noqa: BLE001 - 偏好落盘失败不影响预览
            pass

    # ------------------------------------------------- 自检辅助 ----
    def visible_points(self) -> dict[str, int]:
        result = {}
        for key in self.canvas.visible_keys():
            cache = self.canvas.cache_for(key)
            result[key] = 0 if cache is None else len(cache.times)
        return result

    def select_all(self) -> None:
        self.panel.select_all()

    def select_none(self) -> None:
        self.panel.select_none()

    def set_signal_visible(self, key: str, visible: bool) -> None:
        self.panel.set_visible(key, visible)

    # ------------------------------------------------- 关闭 ----
    def closeEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        if self._closed:
            super().closeEvent(event)
            return
        self._closed = True
        self._prefetch_timer.stop()
        self._settings_timer.stop()
        self._save_view_settings_now()
        self.canvas._refine_timer.stop()  # noqa: SLF001
        worker = self._add_worker
        if worker is not None and worker.isRunning():
            # 先让 session.close() 把状态置为 closed；后台线程可能还在建最后一层
            # 金字塔，不阻塞关闭按钮：脱离父对象 + 全局保活，等它自己 finished。
            if self._session is not None:
                try:
                    self._session.close()
                except Exception:  # noqa: BLE001
                    pass
            worker.setParent(None)
            _ORPHAN_WORKERS.add(worker)
        if self._session is not None:
            try:
                self._session.close()
            except Exception:  # noqa: BLE001
                pass
            self._session = None
        self.closed.emit()
        super().closeEvent(event)


class PlotScrollArea(QScrollArea):
    """滚轮交给画布做缩放（车道多行时用滚动条拖动查看，而不是滚轮）。"""

    def wheelEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        canvas = self.widget()
        if isinstance(canvas, PlotCanvas):
            canvas.wheelEvent(event)
            event.accept()
            return
        super().wheelEvent(event)
