#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Step 16：桌面自绘预览控件 + 预览窗口（``gui/plot_view.py``）。

为什么自绘而不是 QtCharts / pyqtgraph / matplotlib（见任务书 §A）：
* **零新依赖、零打包风险**：PySide6 自带 ``QPainter`` 即可，不引入任何 Python 包；
* **min/max 包络需要精确控制**：每桶要画成"梳状"竖条（``(t,min)`` 与 ``(t,max)`` 交替连线），
  而不是被库自动抽稀成一条 mean 折线（那样会丢尖峰）；
* **多量纲分轴需要精确控制**：同量纲共用一条 Y 轴、不同量纲各自独立并在左侧依次排列，
  第 5 个量纲起归一化到 0~100%，这些布局与坐标变换都必须在自己的 ``paintEvent`` 里算。

数据来源只有 :func:`fast_blf_core.series_for_plot`（在 ``gui/main.py`` 的 Worker 线程里取），
本模块**不**打开 BLF、**不**解码、**不**调用 ``convert``；``paintEvent`` 只读缓存的
``series``。缩放 / 平移只做坐标变换重绘，不重新取数。
"""

from __future__ import annotations

import bisect
import math
import re
from datetime import datetime, timedelta

from PySide6.QtCore import QPointF, QRectF, Qt, Signal
from PySide6.QtGui import (
    QBrush, QColor, QFont, QFontMetrics, QIcon, QPainter, QPen, QPixmap, QPolygonF,
)
from PySide6.QtWidgets import (
    QCheckBox, QComboBox, QHBoxLayout, QLabel, QListWidget, QListWidgetItem,
    QSizePolicy, QVBoxLayout, QWidget,
)

try:  # 以脚本方式运行（python gui/main.py）时 gui/ 已在 sys.path 首位
    from report_html import decimation_note, series_color
except ImportError:  # 作为包导入（gui.plot_view）时
    from gui.report_html import decimation_note, series_color

#: 时间格式下拉（与 ``gui/main.py`` 的 ``TIME_FORMAT_CHOICES`` 同构；main 会传入权威副本）。
TIME_FORMAT_CHOICES: list[tuple[str, str]] = [
    ("相对时间（秒）", "relative_s"),
    ("相对时间（毫秒）", "relative_ms"),
    ("绝对时间（日期 时间）", "absolute"),
    ("Unix 时间戳（秒）", "unix_s"),
]

#: Y 轴相关几何常量。
AXIS_WIDTH = 64
TICK_COUNT = 5
MEAN_ALPHA = 140
#: 缩放下限（相对全量跨度）与"细节已抽稀"阈值（可见桶数 > 屏宽 × 该倍数）。
MIN_SPAN_RATIO = 1e-6
DECIMATION_HINT_FACTOR = 4

_ABS_RE = re.compile(r"^(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?$")


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


def format_axis_time(t: float, time_format: str, t_min_str: str | None) -> str:
    """按当前 ``time_format`` 口径把相对秒 ``t`` 显示成人话。

    ``absolute`` 用 ``t_min_str``（BLF 头墙钟）加偏移换算；解析失败时退回相对秒。
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


def _is_normalized_unit_axis(axis: dict) -> bool:
    return bool(axis.get("normalized"))


class PlotCanvas(QWidget):
    """min/max 包络自绘控件（滚轮缩放 / 拖动平移 / 双击复位 / 十字光标读数）。"""

    def __init__(self, plot_data: dict | None = None, parent: QWidget | None = None,
                 *, time_format: str = "relative_s") -> None:
        super().__init__(parent)
        self.setObjectName("plotCanvas")
        self.setMouseTracking(True)
        self.setMinimumSize(320, 220)
        self.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Expanding)
        self.setAutoFillBackground(True)
        self._series: list[dict] = []
        self._visible: list[bool] = []
        self._axes: list[dict] = []
        self._series_axis: list[int] = []
        self._t_min = 0.0
        self._t_max = 1.0
        self._view = [0.0, 1.0]
        self._time_format = time_format or "relative_s"
        self._t_min_str: str | None = None
        self._show_mean = True
        self._hover_t: float | None = None
        self._hover_pos: QPointF | None = None
        self._drag_anchor: QPointF | None = None
        self._drag_view = [0.0, 1.0]
        self._plot_rect = QRectF()
        # 缩小到细节时需要提示"当前视图每像素 > 4 桶"（A5）。
        self._decimation_hint = False
        self.set_plot_data(plot_data)

    # ----------------------------------------------------------- 数据 ----
    def set_plot_data(self, plot_data: dict | None,
                      time_format: str | None = None) -> None:
        """缓存一份 ``series_for_plot`` 结果并复位视图（不访问内核）。"""
        data = plot_data or {}
        if time_format:
            self._time_format = time_format
        self._series = list(data.get("series") or [])
        self._visible = [True] * len(self._series)
        self._t_min_str = data.get("t_min_str")
        self._t_min = float(data.get("t_min") or 0.0)
        self._t_max = float(data.get("t_max") or 0.0)
        if not (self._t_max > self._t_min):
            self._t_max = self._t_min + 1.0
        self._view = [self._t_min, self._t_max]
        self._refresh_axes()
        self._hover_t = None
        self._hover_pos = None
        self.update()

    def series_count(self) -> int:
        return len(self._series)

    def visible_flags(self) -> list[bool]:
        return list(self._visible)

    def set_series_visible(self, index: int, visible: bool) -> None:
        if 0 <= index < len(self._visible):
            self._visible[index] = bool(visible)
            self._refresh_axes()
            self.update()

    def set_show_mean(self, show: bool) -> None:
        self._show_mean = bool(show)
        self.update()

    def reset_view(self) -> None:
        """双击复位：回到全览。"""
        self._view = [self._t_min, self._t_max]
        self.update()

    # ------------------------------------------------- 坐标 / 分轴布局 ----
    def _refresh_axes(self) -> None:
        """按 ``unit`` 分组：同量纲共用一条轴，最多 4 条实轴，其余归一化。"""
        groups: dict[str, list[int]] = {}
        order: list[str] = []
        self._series_axis = [0] * max(1, len(self._series))
        for index, item in enumerate(self._series):
            if not self._visible[index]:
                continue
            unit = (item.get("unit") or "").strip() or "无量纲"
            if unit not in groups:
                groups[unit] = []
                order.append(unit)
            groups[unit].append(index)

        axes: list[dict] = []
        for unit in order[:4]:
            indexes = groups[unit]
            vmin = min(float(self._series[i].get("vmin", 0.0) or 0.0) for i in indexes)
            vmax = max(float(self._series[i].get("vmax", 0.0) or 0.0) for i in indexes)
            pad = (vmax - vmin) * 0.05 if vmax > vmin else 1.0
            axes.append({"unit": unit, "vmin": vmin - pad, "vmax": vmax + pad,
                         "normalized": False, "color": series_color(indexes[0])})
            for i in indexes:
                self._series_axis[i] = len(axes) - 1

        normalized = [i for unit in order[4:] for i in groups[unit]]
        if normalized:
            axes.append({"unit": "归一化 %", "vmin": 0.0, "vmax": 100.0,
                         "normalized": True, "color": series_color(normalized[0])})
            for i in normalized:
                self._series_axis[i] = len(axes) - 1
        self._axes = axes

    def _prune_view(self) -> None:
        """限制视图范围：不得超出全量范围，且至少保留极小跨度。"""
        full = max(1e-12, self._t_max - self._t_min)
        min_span = full * MIN_SPAN_RATIO
        span = self._view[1] - self._view[0]
        if span <= 0 or not math.isfinite(span):
            self._view = [self._t_min, self._t_max]
            return
        span = min(max(span, min_span), full)
        start = self._view[0]
        if start < self._t_min:
            start = self._t_min
        if start + span > self._t_max:
            start = self._t_max - span
        if start < self._t_min:
            start = self._t_min
        self._view = [start, start + span]

    def _current_plot_rect(self) -> QRectF:
        """绘图区矩形：左侧按 Y 轴条数留白。"""
        self._refresh_axes()
        count = self._axes or [None]
        left = 10.0 + AXIS_WIDTH * len(count)
        top = 24.0
        right = float(self.width()) - 18.0
        bottom = float(self.height()) - 40.0
        width = max(10.0, right - left)
        height = max(10.0, bottom - top)
        return QRectF(left, top, width, height)

    def _t_to_x(self, t: float, rect: QRectF) -> float:
        span = max(1e-12, self._view[1] - self._view[0])
        return rect.left() + (float(t) - self._view[0]) / span * rect.width()

    def _series_y(self, series_index: int, value: float, rect: QRectF,
                  item: dict) -> float:
        axis_index = self._series_axis[series_index] if series_index < len(self._series_axis) else 0
        axis = self._axes[axis_index] if self._axes else {"vmin": 0.0, "vmax": 1.0,
                                                          "normalized": False}
        if _is_normalized_unit_axis(axis):
            lo = float(item.get("vmin", 0.0) or 0.0)
            hi = float(item.get("vmax", 0.0) or 0.0)
            pct = 50.0 if not (hi > lo) else (float(value) - lo) / (hi - lo) * 100.0
        else:
            lo, hi = float(axis["vmin"]), float(axis["vmax"])
            if not (hi > lo):
                hi = lo + 1.0
            pct = (float(value) - lo) / (hi - lo) * 100.0
        pct = max(-5.0, min(105.0, pct))
        return rect.bottom() - pct / 100.0 * rect.height()

    def _index_range(self, times: list[float], rect: QRectF) -> tuple[int, int]:
        """只画可见范围（含 1 个桶 padding）内的点，省掉大量无用顶点。"""
        if not times:
            return 0, 0
        start = bisect.bisect_left(times, self._view[0])
        end = bisect.bisect_right(times, self._view[1])
        return max(0, start - 1), min(len(times), end + 2)

    # ----------------------------------------------------------- 绘制 ----
    def paintEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, False)
        painter.fillRect(self.rect(), QColor("#1E1E24"))
        rect = self._current_plot_rect()
        self._plot_rect = rect
        if rect.width() <= 12 or rect.height() <= 12:
            painter.end()
            return

        self._draw_axes(painter, rect)
        painter.save()
        painter.setClipRect(rect)
        visible_buckets = 0
        for index, item in enumerate(self._series):
            if not self._visible[index]:
                continue
            times = [float(v) for v in (item.get("t") or [])]
            if not times:
                continue
            start, end = self._index_range(times, rect)
            visible_buckets += max(0, end - start)
            self._draw_envelope(painter, rect, index, item, start, end)
            if self._show_mean:
                self._draw_mean(painter, rect, index, item, start, end)
        painter.restore()

        self._decimation_hint = (rect.width() > 1
                                 and visible_buckets > rect.width() * DECIMATION_HINT_FACTOR)
        if self._decimation_hint:
            self._draw_hint(painter, rect,
                            "细节已抽稀：当前视图每像素 > 4 桶，放大地图可看更细")
        if self._hover_t is not None and self._hover_pos is not None:
            self._draw_crosshair(painter, rect)
        painter.end()

    def _draw_axes(self, painter: QPainter, rect: QRectF) -> None:
        if not self._axes:
            painter.setPen(QColor("#9A9AB0"))
            painter.drawText(rect, Qt.AlignmentFlag.AlignCenter, "无可见信号")
            return
        # 网格线（用第一条轴的范围）。
        grid = QPen(QColor("#2A2A33"))
        grid.setStyle(Qt.PenStyle.DotLine)
        painter.setPen(grid)
        for tick in range(TICK_COUNT):
            frac = tick / (TICK_COUNT - 1)
            y = rect.bottom() - frac * rect.height()
            painter.drawLine(QPointF(rect.left(), y), QPointF(rect.right(), y))

        axis_font = QFont(self.font())
        axis_font.setPointSizeF(max(6.5, axis_font.pointSizeF() - 1.5))
        for axis_index, axis in enumerate(self._axes):
            x = rect.left() - axis_index * AXIS_WIDTH
            color = QColor(axis["color"])
            pen = QPen(color)
            pen.setWidthF(1.0)
            painter.setPen(pen)
            painter.drawLine(QPointF(x, rect.top()), QPointF(x, rect.bottom()))
            lo, hi = float(axis["vmin"]), float(axis["vmax"])
            painter.setFont(axis_font)
            for tick in range(TICK_COUNT):
                frac = tick / (TICK_COUNT - 1)
                y = rect.bottom() - frac * rect.height()
                painter.drawLine(QPointF(x - 4, y), QPointF(x + 4, y))
                value = lo + frac * (hi - lo)
                label = f"{value:.0f}%" if axis["normalized"] else _fmt_num(value, 3)
                painter.drawText(QRectF(x - AXIS_WIDTH + 2, y - 8, AXIS_WIDTH - 10, 16),
                                 int(Qt.AlignmentFlag.AlignRight | Qt.AlignmentFlag.AlignVCenter),
                                 label)
            painter.drawText(QRectF(x - AXIS_WIDTH, rect.top() - 20, AXIS_WIDTH, 16),
                             int(Qt.AlignmentFlag.AlignCenter), str(axis["unit"]))
        # 边框
        painter.setPen(QPen(QColor("#3A3A46")))
        painter.drawRect(rect)

    def _draw_envelope(self, painter: QPainter, rect: QRectF, index: int,
                       item: dict, start: int, end: int) -> None:
        """梳状包络：每个桶把 ``(t,min)`` 与 ``(t,max)`` 交替连成折线。"""
        mins = item.get("min") or []
        maxs = item.get("max") or []
        times = item.get("t") or []
        if not (len(times) == len(mins) == len(maxs)):
            return
        polygon = QPolygonF()
        for position in range(start, end):
            x = self._t_to_x(times[position], rect)
            polygon.append(QPointF(x, self._series_y(index, mins[position], rect, item)))
            polygon.append(QPointF(x, self._series_y(index, maxs[position], rect, item)))
        if polygon.size() < 2:
            return
        color = QColor(series_color(index))
        pen = QPen(color)
        pen.setWidthF(1.0)
        painter.setPen(pen)
        painter.setBrush(Qt.BrushStyle.NoBrush)
        painter.drawPolyline(polygon)

    def _draw_mean(self, painter: QPainter, rect: QRectF, index: int,
                   item: dict, start: int, end: int) -> None:
        means = item.get("mean") or []
        times = item.get("t") or []
        if not means or len(times) != len(means):
            return
        polygon = QPolygonF()
        for position in range(start, end):
            polygon.append(QPointF(self._t_to_x(times[position], rect),
                                   self._series_y(index, means[position], rect, item)))
        if polygon.size() < 2:
            return
        color = QColor(series_color(index))
        color.setAlpha(MEAN_ALPHA)
        pen = QPen(color)
        pen.setWidthF(1.0)
        painter.setPen(pen)
        painter.setBrush(Qt.BrushStyle.NoBrush)
        painter.drawPolyline(polygon)

    def _draw_hint(self, painter: QPainter, rect: QRectF, text: str) -> None:
        font = QFont(self.font())
        font.setPointSizeF(max(7.0, font.pointSizeF() - 1.0))
        painter.setFont(font)
        metrics = QFontMetrics(font)
        width = metrics.horizontalAdvance(text) + 16
        box = QRectF(rect.left() + 8, rect.bottom() - 26, min(width, rect.width() - 16), 20)
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(QBrush(QColor(240, 160, 32, 40)))
        painter.drawRoundedRect(box, 4, 4)
        painter.setPen(QColor("#F0A020"))
        painter.drawText(box, int(Qt.AlignmentFlag.AlignCenter), text)

    def _draw_crosshair(self, painter: QPainter, rect: QRectF) -> None:
        hover_t = self._hover_t
        if hover_t is None:
            return
        x = self._t_to_x(hover_t, rect)
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
        for index, item in enumerate(self._series):
            if not self._visible[index]:
                continue
            times = item.get("t") or []
            mins = item.get("min") or []
            maxs = item.get("max") or []
            means = item.get("mean") or []
            if not times or not (len(times) == len(mins) == len(maxs) == len(means)):
                continue
            found = self._nearest_index(times, hover_t)
            if found < 0:
                continue
            unit = (item.get("unit") or "").strip()
            value = means[found]
            mins_v = mins[found]
            maxs_v = maxs[found]
            name = str(item.get("signal") or "?")
            suffix = f" {unit}" if unit else ""
            lines.append(f"● {name}: {_fmt_num(value, 4)} ({_fmt_num(mins_v, 4)} ~ "
                         f"{_fmt_num(maxs_v, 4)}){suffix}")
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
                # 用对应曲线颜色绘制圆点 + 文本。
                index = self._readout_series_index(offset)
                if index is not None:
                    color = QColor(series_color(index))
            painter.setPen(color)
            painter.drawText(QRectF(box.left() + 9, box.top() + 6 + offset * line_height,
                                    box.width() - 18, line_height),
                             int(Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter),
                             line)

    def _readout_series_index(self, offset: int) -> int | None:
        """把读数行号（跳过首行时间）映射回可见信号序号。"""
        visible = [i for i, flag in enumerate(self._visible) if flag and (self._series[i].get("t"))]
        position = offset - 1
        return visible[position] if 0 <= position < len(visible) else None

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
        rect = self._current_plot_rect()
        if rect.width() <= 1:
            return
        delta = event.angleDelta().y()
        if delta == 0:
            return
        fraction = (event.position().x() - rect.left()) / rect.width()
        fraction = max(0.0, min(1.0, fraction))
        span = self._view[1] - self._view[0]
        anchor = self._view[0] + fraction * span
        factor = 0.8 if delta > 0 else 1.25
        new_span = span * factor
        self._view = [anchor - fraction * new_span, anchor + (1 - fraction) * new_span]
        self._prune_view()
        self.update()
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
            rect = self._current_plot_rect()
            if rect.width() > 1:
                span = self._drag_view[1] - self._drag_view[0]
                shift = -(position.x() - self._drag_anchor.x()) / rect.width() * span
                self._view = [self._drag_view[0] + shift, self._drag_view[1] + shift]
                self._prune_view()
            self.update()
            return
        rect = self._current_plot_rect()
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
            self._drag_anchor = None
            self.setCursor(Qt.CursorShape.ArrowCursor)
        super().mouseReleaseEvent(event)

    def mouseDoubleClickEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        if event.button() == Qt.MouseButton.LeftButton:
            self.reset_view()
        super().mouseDoubleClickEvent(event)

    def leaveEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        self._hover_t = None
        self._hover_pos = None
        self.update()
        super().leaveEvent(event)


class PreviewWindow(QWidget):
    """独立预览窗口：顶部信息条 + 自绘画布 + 可点击图例 + 时间口径下拉。

    * 时间口径与主窗共享：用户在窗口里改口径时调用 ``on_refetch(value)``
      （由 ``gui/main.py`` 在 Worker 线程里重新调 ``series_for_plot``），
      不是前端换算；
    * 关闭时发 :attr:`closed` 信号，主窗据此清掉引用，不留线程 / 僵尸窗口。
    """

    closed = Signal()

    def __init__(self, plot_data: dict | None, *, time_format: str = "relative_s",
                 time_format_choices: list[tuple[str, str]] | None = None,
                 on_refetch=None, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("plotWindow")
        self.setWindowTitle("📈 数据预览 · min/max 包络（已抽稀）")
        self.resize(1280, 760)
        self._data: dict = plot_data or {}
        self._time_format = time_format or "relative_s"
        self._choices = list(time_format_choices or TIME_FORMAT_CHOICES)
        self._on_refetch = on_refetch
        self._legend_guard = False
        self._build_ui()
        self.set_plot_data(self._data)

    def _build_ui(self) -> None:
        outer = QVBoxLayout(self)
        outer.setContentsMargins(10, 10, 10, 10)
        outer.setSpacing(8)

        self.lbl_info = QLabel("")
        self.lbl_info.setObjectName("plotInfo")
        self.lbl_info.setWordWrap(True)
        self.lbl_info.setTextFormat(Qt.TextFormat.RichText)
        self.lbl_info.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Preferred)
        outer.addWidget(self.lbl_info)

        body = QHBoxLayout()
        body.setSpacing(8)
        self.canvas = PlotCanvas(self._data, self, time_format=self._time_format)
        body.addWidget(self.canvas, 1)
        self.legend = QListWidget(self)
        self.legend.setObjectName("plotLegend")
        self.legend.setFixedWidth(290)
        self.legend.setAlternatingRowColors(True)
        self.legend.itemChanged.connect(self._on_legend_changed)
        body.addWidget(self.legend, 0)
        outer.addLayout(body, 1)

        controls = QHBoxLayout()
        controls.setSpacing(10)
        controls.addWidget(QLabel("时间格式："))
        self.combo_time = QComboBox(self)
        for label, value in self._choices:
            self.combo_time.addItem(label, value)
        self.combo_time.setToolTip("切换口径会重新调用 series_for_plot 取数（不是前端换算）")
        self.combo_time.currentIndexChanged.connect(self._on_time_format_changed)
        controls.addWidget(self.combo_time)
        self.chk_mean = QCheckBox("叠加均值线（半透明）")
        self.chk_mean.setChecked(True)
        self.chk_mean.toggled.connect(self.canvas.set_show_mean)
        controls.addWidget(self.chk_mean)
        controls.addStretch(1)
        hint = QLabel("滚轮缩放 · 按住左键拖动平移 · 双击复位 · 点击图例显隐")
        hint.setObjectName("hintLabel")
        controls.addWidget(hint)
        outer.addLayout(controls)

    # ----------------------------------------------------------- 数据 ----
    def set_plot_data(self, plot_data: dict | None, time_format: str | None = None) -> None:
        """更新画布与图例（重新取数成功后由主窗调用；不触发再次取数）。"""
        if time_format:
            self._time_format = time_format
        self._data = plot_data or {}
        self.canvas.set_plot_data(self._data, time_format=self._time_format)
        self._rebuild_legend()
        self._update_info()
        self._sync_combo()

    def _sync_combo(self) -> None:
        index = next((i for i, (_label, value) in enumerate(self._choices)
                      if value == self._time_format), 0)
        self.combo_time.blockSignals(True)
        try:
            self.combo_time.setCurrentIndex(index)
        finally:
            self.combo_time.blockSignals(False)

    def _rebuild_legend(self) -> None:
        self._legend_guard = True
        try:
            self.legend.clear()
            for index, item in enumerate(self._data.get("series") or []):
                color = QColor(series_color(index))
                pixmap = QPixmap(12, 12)
                pixmap.fill(color)
                unit = (item.get("unit") or "").strip()
                name = str(item.get("signal") or "?")
                count = int(item.get("count", 0) or 0)
                text = f"{name}" + (f" ({unit})" if unit else "") + f" · {count:,} 点"
                row = QListWidgetItem(QIcon(pixmap), text)
                row.setFlags(row.flags() | Qt.ItemFlag.ItemIsUserCheckable)
                row.setCheckState(Qt.CheckState.Checked)
                row.setToolTip(
                    f"通道 Ch{item.get('channel', '?')} / {item.get('message', '?')}\n"
                    f"单位：{unit or '无量纲'}\n原始采样点数：{count:,}\n"
                    f"全量 min/max：{_fmt_num(item.get('vmin'))} ~ {_fmt_num(item.get('vmax'))}\n"
                    "点击复选框显隐曲线")
                self.legend.addItem(row)
        finally:
            self._legend_guard = False

    def _on_legend_changed(self, item: QListWidgetItem) -> None:
        if self._legend_guard:
            return
        row = self.legend.row(item)
        if row >= 0:
            self.canvas.set_series_visible(row, item.checkState() == Qt.CheckState.Checked)

    def _update_info(self) -> None:
        data = self._data
        series = list(data.get("series") or [])
        buckets = int(data.get("buckets") or 0)
        total = sum(int(item.get("count", 0) or 0) for item in series)
        if self._time_format == "absolute" and data.get("t_min_str"):
            time_range = (f"{data.get('t_min_str')} → {data.get('t_max_str')}"
                          "（绝对时间，BLF 头墙钟）")
        else:
            time_range = (f"{_fmt_num(data.get('t_min'))} s → "
                          f"{_fmt_num(data.get('t_max'))} s")
        note = decimation_note(buckets, total)
        self.lbl_info.setText(
            f"<b>时间范围：</b>{time_range}　|　"
            f"<b>共 {len(series)} 个信号 / 总原始点数 {total:,}</b>"
            f"<br><span style=\"color:#F0A020\">{note}</span>"
            "　<span style=\"color:#9A9AB0\">（曲线为抽稀后的桶，非全部原始点）</span>")

    def _on_time_format_changed(self, index: int) -> None:
        value = self.combo_time.itemData(index)
        if not value or value == self._time_format:
            return
        self._time_format = str(value)
        self.lbl_info.setText(
            f"<b>正在按「{self.combo_time.currentText()}」重新取数…</b>")
        if callable(self._on_refetch):
            self._on_refetch(str(value))

    def closeEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        self.closed.emit()
        super().closeEvent(event)
