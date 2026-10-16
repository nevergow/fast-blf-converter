#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""三态复选框自绘 delegate（Step 6b）。

``theme.qss`` 原先用 ``QTreeView::indicator`` 把勾选框涂成实心蓝：checked(#0078D4)
与 indeterminate(#006CBE) 几乎同色，选中行又是纯蓝铺底，勾选框直接“隐身”。这里改用
QPainter 自绘：未选 = 空框，部分选 = 蓝框 + 横条，全选 = 实心蓝 + 白勾。只接管第 0
列且带 ``Qt.ItemIsUserCheckable`` 的项；行背景 / 文字 / 选中高亮仍由基类按 QSS 绘制，
不改变行高与树的层级结构。
"""

from __future__ import annotations

from PySide6.QtCore import QModelIndex, QRect, QRectF, QSize, Qt
from PySide6.QtGui import QColor, QPainter, QPainterPath, QPen
from PySide6.QtWidgets import (
    QApplication, QStyle, QStyledItemDelegate, QStyleOptionViewItem, QWidget,
)

#: 勾选框几何：14×14、圆角 3px、左侧留 6px、与文字间距 8px。
BOX_SIZE = 14
BOX_RADIUS = 3.0
BOX_LEFT_PAD = 6
BOX_TEXT_GAP = 8
_PEN_WIDTH = 1.5

#: 调色板（与 ``theme.qss`` 的暗色 Fluent 保持一致）。
BG_UNCHECKED = QColor("#2D2D38")
BORDER_UNCHECKED = QColor("#5C5C6E")
BORDER_HOVER = QColor("#8A8A9E")
ACCENT = QColor("#0078D4")
ACCENT_HOVER = QColor("#1A88E0")
CHECK_MARK = QColor("#FFFFFF")


class CheckStateDelegate(QStyledItemDelegate):
    """自绘三态复选框；行背景 / 文字 / 选中高亮仍由基类按 QSS 绘制。"""

    @staticmethod
    def applies(index: QModelIndex) -> bool:
        """仅第 0 列且用户可勾选的项启用自绘勾选框。"""
        if index is None or not index.isValid() or index.column() != 0:
            return False
        if not (index.flags() & Qt.ItemFlag.ItemIsUserCheckable):
            return False
        return index.data(Qt.ItemDataRole.CheckStateRole) is not None

    def sizeHint(self, option: QStyleOptionViewItem, index: QModelIndex) -> QSize:  # noqa: N802
        """保持基类行为：自绘勾选框不改变行高。"""
        return super().sizeHint(option, index)

    def paint(self, painter: QPainter, option: QStyleOptionViewItem,  # noqa: N802
              index: QModelIndex) -> None:
        if not self.applies(index):
            super().paint(painter, option, index)
            return
        opt = QStyleOptionViewItem(option)
        self.initStyleOption(opt, index)
        widget: QWidget | None = opt.widget
        style = widget.style() if widget is not None else QApplication.style()
        # 关掉基类的勾选框绘制（QSS 的 ::indicator 规则已删除），彻底自绘；用两遍
        # 绘制腾出勾选框位置：第一遍铺满行背景，第二遍把文字矩形右移后重绘。
        opt.features &= ~QStyleOptionViewItem.ViewItemFeature.HasCheckIndicator
        self._draw_row(style, painter, opt, widget, text_rect=None)
        self._draw_row(style, painter, opt, widget, text_rect=self._text_rect(opt))
        self._draw_box(painter, opt, index)

    # -------------------------------------------------------------- 行绘制 ----
    @staticmethod
    def _text_rect(opt: QStyleOptionViewItem) -> QRect:
        r = opt.rect
        shift = BOX_LEFT_PAD + BOX_SIZE + BOX_TEXT_GAP
        return QRect(r.left() + shift, r.top(), max(0, r.width() - shift), r.height())

    @staticmethod
    def _draw_row(style, painter: QPainter, opt: QStyleOptionViewItem, widget, *,
                  text_rect: QRect | None) -> None:
        """``text_rect=None`` 时只铺行背景 / 选中高亮；否则绘制位移后的文字。"""
        row = QStyleOptionViewItem(opt)
        if text_rect is None:
            row.text = ""
        else:
            row.rect = text_rect
        style.drawControl(QStyle.ControlElement.CE_ItemViewItem, row, painter, widget)

    # ------------------------------------------------------------- 勾选框 ----
    def _draw_box(self, painter: QPainter, opt: QStyleOptionViewItem,
                  index: QModelIndex) -> None:
        state = Qt.CheckState(int(index.data(Qt.ItemDataRole.CheckStateRole)))
        enabled = bool(opt.state & QStyle.StateFlag.State_Enabled)
        hovered = bool(opt.state & QStyle.StateFlag.State_MouseOver)
        top = opt.rect.top() + (opt.rect.height() - BOX_SIZE) // 2
        box = QRect(opt.rect.left() + BOX_LEFT_PAD, top, BOX_SIZE, BOX_SIZE)
        painter.save()
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        if not enabled:
            painter.setOpacity(0.5)  # 禁用态整框 50% 透明
        if state == Qt.CheckState.Checked:
            self._paint_checked(painter, box)
        elif state == Qt.CheckState.PartiallyChecked:
            self._paint_partial(painter, box, hovered)
        else:
            self._frame(painter, box, BORDER_HOVER if hovered else BORDER_UNCHECKED)
        painter.restore()

    @staticmethod
    def _frame(painter: QPainter, box: QRect, border: QColor) -> None:
        painter.setPen(QPen(border, _PEN_WIDTH))
        painter.setBrush(BG_UNCHECKED)
        painter.drawRoundedRect(QRectF(box).adjusted(0.75, 0.75, -0.75, -0.75),
                                BOX_RADIUS, BOX_RADIUS)

    @classmethod
    def _paint_partial(cls, painter: QPainter, box: QRect, hovered: bool) -> None:
        cls._frame(painter, box, ACCENT_HOVER if hovered else ACCENT)
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(ACCENT)
        cx, cy = box.center().x(), box.center().y()
        painter.drawRoundedRect(QRect(cx - 4, cy - 1, 8, 3), 1.5, 1.5)  # 8×3 横条

    @staticmethod
    def _paint_checked(painter: QPainter, box: QRect) -> None:
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(ACCENT)
        painter.drawRoundedRect(QRectF(box).adjusted(0.75, 0.75, -0.75, -0.75),
                                BOX_RADIUS, BOX_RADIUS)
        pen = QPen(CHECK_MARK, 2.0)
        pen.setCapStyle(Qt.PenCapStyle.RoundCap)
        pen.setJoinStyle(Qt.PenJoinStyle.RoundJoin)
        painter.setPen(pen)
        painter.setBrush(Qt.BrushStyle.NoBrush)
        path = QPainterPath()
        path.moveTo(box.left() + 3.2, box.center().y() + 0.4)
        path.lineTo(box.center().x() - 0.6, box.bottom() - 3.2)
        path.lineTo(box.right() - 2.6, box.top() + 3.2)
        painter.drawPath(path)
