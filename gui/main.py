#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Fast BLF Converter —— PySide6 图形界面（Step 3）。

* 内核延迟导入：``fast_blf_core`` 未编译时 GUI 仍可构造与自检，只禁用转换按钮。
* 线程安全：``scan``/``convert`` 的 ``progress_cb`` 由 Rust 工作线程调用，
  统一经 ``Worker.progress``（``Signal(dict)``）转发到主线程，绝不在工作线程碰控件。
* 接口对齐：全部字段严格遵循 ``docs/INTERFACE.md`` §1 / §1.1 / §1.2 / §2。
"""

from __future__ import annotations

import os
import sys
from dataclasses import dataclass, field
from pathlib import Path

from PySide6.QtCore import (
    QModelIndex, QObject, QSortFilterProxyModel, QThread, QTimer, Qt, Signal,
)
from PySide6.QtGui import QAction, QColor, QFont, QStandardItem, QStandardItemModel
from PySide6.QtWidgets import (
    QAbstractItemView, QApplication, QCheckBox, QComboBox, QFileDialog, QHBoxLayout,
    QHeaderView, QLabel, QLineEdit, QMainWindow, QMessageBox, QProgressBar, QPushButton,
    QSizePolicy, QSplitter, QStatusBar, QToolBar, QTreeView, QTreeWidget, QTreeWidgetItem,
    QVBoxLayout, QWidget,
)

# --------------------------------------------------------------- 内核延迟导入 --
try:  # pragma: no cover - 取决于是否已 maturin develop
    import fast_blf_core as core  # type: ignore
except Exception as _import_error:  # noqa: BLE001 - 内核缺失原因不限
    core = None  # type: ignore
    CORE_IMPORT_ERROR: str | None = str(_import_error)
else:
    CORE_IMPORT_ERROR = None

KERNEL_READY: bool = core is not None

# ------------------------------------------------------------------- 常量 ----
APP_TITLE = "Fast BLF Converter · 多核 BLF 极速转换"
CHANNEL_CHOICES = ["Ch1", "Ch2", "Ch3", "Ch4", "Ch5", "Ch6", "Ch7", "Ch8"]
KERNEL_MISSING_TEXT = "⚠ Rust 内核未就绪（请先 maturin develop）"
STATUS_PENDING, STATUS_PARSING, STATUS_DONE, STATUS_FAILED = "待处理", "解析中", "✓ 已完成", "✗ 失败"
RESAMPLE_CHOICES: list[tuple[str, float | None]] = [
    ("保持原始时序", None), ("1 Hz", 1.0), ("10 Hz", 10.0), ("50 Hz", 50.0), ("100 Hz", 100.0),
]
FORMAT_CHOICES: list[tuple[str, str]] = [
    ("CSV (通用表格)", "csv"), ("Parquet (极速列存)", "parquet"), ("Excel (.xlsx 多Sheet)", "xlsx"),
]
NODE_ROLE = Qt.ItemDataRole.UserRole + 1
TEXT_ROLE = Qt.ItemDataRole.UserRole + 2
WARN_MERGE_HINT = "合并为单张宽表时建议选择重采样频率，否则会因周期不同产生空值"
PHASE_FALLBACK = {"scan": "正在扫描", "decode": "正在并行解码", "merge": "正在流式合并", "export": "正在导出"}
STATUS_COLORS = {
    STATUS_PENDING: QColor("#9A9AB0"), STATUS_PARSING: QColor("#0078D4"),
    STATUS_DONE: QColor("#4CC26B"), STATUS_FAILED: QColor("#F05A5A"),
}


# --------------------------------------------------------------- 资源路径 ----
# 主题等只读资源需要同时适配开发态与 PyInstaller 冻结态，定位逻辑统一收敛在此处。
def _resource_roots() -> list[Path]:
    """返回只读资源的候选根目录（冻结态优先，开发态兜底）。

    * 冻结态（PyInstaller onedir）：可执行文件同级目录优先（绿色包在该处复制了一份
      可手工改的 ``gui/theme.qss``），其次 ``sys._MEIPASS``（onedir 下即 ``_internal``，
      spec 的 datas 落点），最后同级 ``_internal`` 目录，以兼容不同 PyInstaller 版本。
    * 开发态：本文件所在目录（``gui/``）与当前工作目录。
    """
    roots: list[Path] = []
    if getattr(sys, "frozen", False):
        exe_dir = Path(sys.executable).resolve().parent
        for candidate in (exe_dir, getattr(sys, "_MEIPASS", None), exe_dir / "_internal"):
            if candidate:
                path = Path(candidate)
                if path not in roots:
                    roots.append(path)
    for candidate in (Path(__file__).resolve().parent, Path.cwd()):
        if candidate not in roots:
            roots.append(candidate)
    return roots


def resolve_theme_path() -> Path:
    """定位 ``theme.qss``：``FBLF_THEME`` 环境变量 > 资源根 ``gui/theme.qss`` > 资源根 ``theme.qss``。

    spec 把 qss 放进产物的 ``gui/`` 子目录（与开发态相对路径一致），
    因此同一套解析逻辑在开发态与冻结态都成立；全部候选都不存在时返回首选路径，
    由 :func:`load_theme` 负责降级（缺主题不影响启动）。
    """
    override = os.environ.get("FBLF_THEME")
    candidates: list[Path] = []
    if override:
        candidates.append(Path(override).expanduser())
    for root in _resource_roots():
        candidates.append(root / "gui" / "theme.qss")  # spec datas 目标目录
        candidates.append(root / "theme.qss")          # 扁平布局兜底
    for path in candidates:
        if path.is_file():
            return path
    return candidates[0]


THEME_PATH = resolve_theme_path()


# ------------------------------------------------------------------- 小工具 --
def human_size(num_bytes: int) -> str:
    """把字节数格式化成便于阅读的字符串。"""
    try:
        size = float(num_bytes)
    except (TypeError, ValueError):
        return "—"
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if size < 1024.0 or unit == "TB":
            return f"{int(size)} {unit}" if unit == "B" else f"{size:.1f} {unit}"
        size /= 1024.0
    return f"{size:.1f} TB"


def fmt_float(value: float, digits: int = 3) -> str:
    """去掉无意义的小数尾零（100ms 而不是 100.00ms）。"""
    return f"{value:.{digits}f}".rstrip("0").rstrip(".") or "0"


if sys.version_info < (3, 10):  # PySide6 6.10 与 Rust 扩展都要求 3.10+
    raise SystemExit("Fast BLF Converter 需要 Python 3.10 或更高版本，当前为 "
                     f"{sys.version.split()[0]}")


def tune(widget, **props) -> None:
    """批量设置控件属性（setText / setObjectName / setRange …）；元组表示多参数。"""
    for name, value in props.items():
        setter = getattr(widget, "set" + name[0].upper() + name[1:])  # setUpX → setXxx
        setter(*value) if isinstance(value, tuple) else setter(value)


# ------------------------------------------------------- 左侧：文件列表条目 --
@dataclass
class BlfFileEntry:
    """单个待处理 BLF（与文件树一行一一对应）。"""

    path: str
    size: int
    status: str = STATUS_PENDING
    error: str | None = None
    item: QTreeWidgetItem | None = field(default=None, repr=False)

    @property
    def name(self) -> str:
        return os.path.basename(self.path)

    def apply_status(self, status: str, error: str | None = None) -> None:
        self.status = status
        if error is not None:
            self.error = error
        if self.item is None:
            return
        self.item.setText(2, status); self.item.setForeground(2, STATUS_COLORS.get(status, QColor("#E8E8F0")))
        self.item.setToolTip(0, self.path if not self.error else f"{self.path}\n{self.error}")


class BlfFileTree(QTreeWidget):
    """左树：支持从资源管理器拖入多个 ``.blf``。"""

    files_dropped = Signal(list)

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setAcceptDrops(True)
        self.setColumnCount(3)
        self.setHeaderLabels(["文件名", "大小", "状态"])
        self.setRootIsDecorated(False)
        self.setAlternatingRowColors(True)
        self.setUniformRowHeights(True)
        self.setSelectionMode(QAbstractItemView.SelectionMode.ExtendedSelection)
        self.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
        header = self.header()
        header.setSectionResizeMode(0, QHeaderView.ResizeMode.Stretch)
        header.setSectionResizeMode(1, QHeaderView.ResizeMode.ResizeToContents)
        header.setSectionResizeMode(2, QHeaderView.ResizeMode.ResizeToContents)
        self.setMinimumWidth(280)

    @staticmethod
    def _blf_paths(mime) -> list[str]:
        """只接受 ``.blf`` 的本地文件 URL。"""
        found: list[str] = []
        if not mime.hasUrls():
            return found
        for url in mime.urls():
            local = url.toLocalFile()
            if local and local.lower().endswith(".blf") and local not in found:
                found.append(local)
        return found

    def dragEnterEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        event.acceptProposedAction() if self._blf_paths(event.mimeData()) else event.ignore()

    def dragMoveEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        event.acceptProposedAction() if self._blf_paths(event.mimeData()) else event.ignore()

    def dropEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        paths = self._blf_paths(event.mimeData())
        if paths:
            self.files_dropped.emit(paths)
            event.acceptProposedAction()
        else:
            event.ignore()


# ------------------------- 右侧：信号筛选代理（模糊过滤，保留匹配项的祖先） --
class SignalFilterProxy(QSortFilterProxyModel):
    """大小写不敏感的子串过滤；父节点只要存在匹配后代就保留。"""

    def __init__(self, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self.setDynamicSortFilter(True)
        self.setRecursiveFilteringEnabled(False)
        self._keyword = ""

    def set_keyword(self, text: str) -> None:
        self._keyword = (text or "").strip().lower(); self.invalidateFilter()

    def keyword(self) -> str:
        return self._keyword

    def filterAcceptsRow(self, source_row: int, source_parent: QModelIndex) -> bool:  # noqa: N802
        if not self._keyword:
            return True
        model = self.sourceModel()
        if model is None:
            return True
        index = model.index(source_row, 0, source_parent)
        if not index.isValid():
            return False
        if self._keyword in str(index.data(TEXT_ROLE) or "").lower():
            return True
        return any(self.filterAcceptsRow(row, index) for row in range(model.rowCount(index)))


# --------------------------- 工作线程：内核调用全部在此，UI 只通过信号接收 ---
class Worker(QThread):
    """``mode="scan"`` 调 ``core.scan``；``mode="convert"`` 调 ``core.convert``。"""

    progress = Signal(dict)
    finished_ok = Signal(dict)
    failed = Signal(str)

    def __init__(self, mode: str, config: dict | None = None, blf_paths: list[str] | None = None,
                 dbc_map: dict[int, str] | None = None, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self.mode = mode
        self.config = config or {}
        self.blf_paths = list(blf_paths or [])
        self.dbc_map = dict(dbc_map or {})
        self._cancelled = False

    def cancel(self) -> None:
        """请求取消：内核不支持抢占，只在回调里抛异常以中止后续阶段。"""
        self._cancelled = True

    def _progress_bridge(self, payload: dict) -> None:
        """运行在 Rust 工作线程：只允许 emit，禁止触碰任何控件。"""
        if self._cancelled:
            raise RuntimeError("用户已取消任务")
        self.progress.emit(dict(payload))

    def run(self) -> None:  # noqa: D401 - QThread 入口
        if core is None:
            self.failed.emit(KERNEL_MISSING_TEXT)
            return
        try:
            report = (core.scan(self.blf_paths, self.dbc_map, self._progress_bridge)
                      if self.mode == "scan"
                      else core.convert(self.config, self._progress_bridge))
        except Exception as exc:  # noqa: BLE001 - 任何内核异常都要变成提示
            self.failed.emit(f"{type(exc).__name__}: {exc}")
            return
        if isinstance(report, dict):
            self.finished_ok.emit(report)
        else:
            self.failed.emit(f"内核返回了非预期类型：{type(report).__name__}")


# ----------------------------------------------------------------- 主窗口 ----
class MainWindow(QMainWindow):
    """工具栏 + QSplitter 左右分栏 + 底部控制面板 + 状态栏。"""

    def __init__(self) -> None:
        super().__init__()
        self.setWindowTitle(APP_TITLE)
        self.resize(1280, 820)
        self.setMinimumSize(1000, 640)
        self.setAcceptDrops(True)
        self.entries: list[BlfFileEntry] = []
        self.entry_by_path: dict[str, BlfFileEntry] = {}
        self.dbc_by_channel: dict[int, str] = {}
        self.channel_summary: dict[str, dict] = {}
        self.scan_worker = self.convert_worker = None
        self._checks_guard = False
        self._build_toolbar()
        self._build_central()
        self._build_statusbar()
        self._connect_signals()
        self._sync_resample_hint()
        self._refresh_action_state()
        if not KERNEL_READY:
            detail = f"（{CORE_IMPORT_ERROR}）" if CORE_IMPORT_ERROR else ""
            self.statusBar().showMessage(f"{KERNEL_MISSING_TEXT}{detail}")

    # ----------------------------------------------------------- 构建 UI ----
    @staticmethod
    def _panel(parent: QWidget | None = None) -> tuple[QWidget, QVBoxLayout]:
        """分栏用的轻量面板：无外边距 + 6px 行距。"""
        panel = QWidget(parent)
        layout = QVBoxLayout(panel)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(6)
        return panel, layout

    @staticmethod
    def _column(margins: tuple[int, int, int, int], spacing: int) -> QVBoxLayout:
        """带边距的纵向布局。"""
        layout = QVBoxLayout()
        layout.setContentsMargins(*margins)
        layout.setSpacing(spacing)
        return layout

    def _build_toolbar(self) -> None:
        bar = QToolBar("主工具栏", self)
        bar.setMovable(False)
        bar.setFloatable(False)
        self.addToolBar(Qt.ToolBarArea.TopToolBarArea, bar)
        self.act_add_blf = QAction("📂 批量添加 BLF", self)
        tune(self.act_add_blf, shortcut="Ctrl+O", statusTip="选择多个 .blf 加入队列（也可直接拖拽到左侧列表）")
        bar.addAction(self.act_add_blf)
        bar.addSeparator()
        bar.addWidget(QLabel("目标通道："))
        self.channel_combo = QComboBox(self)
        self.channel_combo.addItems(CHANNEL_CHOICES)
        tune(self.channel_combo, toolTip="选择要给哪个通道绑定 DBC")
        bar.addWidget(self.channel_combo)
        self.act_bind_dbc = QAction("🔗 绑定 DBC 到该通道", self)
        tune(self.act_bind_dbc, statusTip="为下拉框选中的通道指定 .dbc 文件")
        bar.addAction(self.act_bind_dbc)
        bar.addSeparator()
        self.dbc_label = QLabel("DBC 绑定：无")
        tune(self.dbc_label, objectName="hintLabel")
        self.dbc_label.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Preferred)
        bar.addWidget(self.dbc_label)
        self.act_clear_dbc = QAction("清空 DBC 绑定", self)
        tune(self.act_clear_dbc, statusTip="移除所有通道的 DBC 绑定并清空信号树")
        bar.addAction(self.act_clear_dbc)

    def _build_central(self) -> None:
        splitter = QSplitter(Qt.Orientation.Horizontal, self)
        splitter.setChildrenCollapsible(False)
        splitter.setHandleWidth(2)
        splitter.addWidget(self._build_file_panel())
        splitter.addWidget(self._build_signal_panel())
        splitter.setStretchFactor(0, 3)
        splitter.setStretchFactor(1, 7)
        splitter.setSizes([400, 860])
        central = QWidget(self)
        layout = self._column((10, 10, 10, 10), 10)
        central.setLayout(layout)
        layout.addWidget(splitter, 1)
        layout.addWidget(self._build_control_panel())
        self.setCentralWidget(central)

    def _build_file_panel(self) -> QWidget:
        panel, layout = self._panel(self)
        title = QLabel("BLF 文件列表（可拖拽 .blf 到此处）")
        tune(title, objectName="hintLabel")
        layout.addWidget(title)
        self.file_tree = BlfFileTree(panel)
        layout.addWidget(self.file_tree, 1)
        self.btn_remove_selected = QPushButton("移除选中")
        self.btn_clear_files = QPushButton("清空列表")
        row = QHBoxLayout()
        [row.addWidget(w) for w in (self.btn_remove_selected, self.btn_clear_files)]
        row.addStretch(1)
        layout.addLayout(row)
        return panel

    def _build_signal_panel(self) -> QWidget:
        panel, layout = self._panel(self)
        title = QLabel("信号筛选（通道 → 报文 → 信号，只有勾选的节点才会参与转换）")
        tune(title, objectName="hintLabel")
        layout.addWidget(title)
        self.search_edit = QLineEdit(panel)
        tune(self.search_edit, placeholderText="🔍 搜索报文名 / 信号名（大小写不敏感的模糊匹配）", clearButtonEnabled=True)
        self.btn_check_visible = QPushButton("全选当前过滤项")
        self.btn_uncheck_all = QPushButton("一键清空")
        row = QHBoxLayout()
        row.addWidget(self.search_edit, 1)
        row.addWidget(self.btn_check_visible)
        row.addWidget(self.btn_uncheck_all)
        layout.addLayout(row)
        self.signal_model = QStandardItemModel(0, 1, self)
        self.signal_model.setHeaderData(0, Qt.Orientation.Horizontal, "通道 / 报文 / 信号")
        self.signal_proxy = SignalFilterProxy(self)
        self.signal_proxy.setSourceModel(self.signal_model)
        self.signal_view = QTreeView(panel)
        self.signal_view.setModel(self.signal_proxy)
        tune(self.signal_view, alternatingRowColors=True, uniformRowHeights=True, indentation=18,
             editTriggers=QAbstractItemView.EditTrigger.NoEditTriggers,
             selectionMode=QAbstractItemView.SelectionMode.ExtendedSelection)
        self.signal_view.header().setSectionResizeMode(0, QHeaderView.ResizeMode.Stretch)
        layout.addWidget(self.signal_view, 1)
        tip = QLabel("提示：勾选通道会联动其下全部报文与信号；只勾选部分信号时父节点显示半选。")
        tune(tip, objectName="hintLabel")
        layout.addWidget(tip)
        return panel

    def _build_control_panel(self) -> QWidget:
        panel = QWidget(self)
        panel.setObjectName("controlPanel")
        outer = self._column((12, 10, 12, 10), 8)
        panel.setLayout(outer)
        row = QHBoxLayout()
        row.setSpacing(14)
        self.chk_split = QCheckBox("按报文周期拆分为独立时序子表 (推荐，彻底规避空值)")
        self.chk_split.setChecked(True)
        row.addWidget(self.chk_split)
        row.addWidget(QLabel("重采样频率："))
        self.combo_resample = QComboBox(panel)
        self.combo_resample.addItems([label for label, _hz in RESAMPLE_CHOICES])
        tune(self.combo_resample, currentIndex=0, toolTip="仅在不拆表（单张宽表）或需要等间隔时间轴时才有意义")
        row.addWidget(self.combo_resample)
        row.addWidget(QLabel("导出格式："))
        self.combo_format = QComboBox(panel)
        self.combo_format.addItems([label for label, _fmt in FORMAT_CHOICES])
        self.combo_format.setCurrentIndex(0)
        row.addWidget(self.combo_format)
        row.addStretch(1)
        self.btn_convert = QPushButton("🚀 启动多核极速转换")
        self.btn_convert.setObjectName("primaryButton")
        row.addWidget(self.btn_convert)
        outer.addLayout(row)
        self.lbl_merge_warning = QLabel(WARN_MERGE_HINT)
        tune(self.lbl_merge_warning, objectName="warnHint", wordWrap=True, visible=False)
        outer.addWidget(self.lbl_merge_warning)
        return panel

    def _build_statusbar(self) -> None:
        bar = QStatusBar(self)
        self.setStatusBar(bar)
        self.lbl_status = QLabel("就绪")
        tune(self.lbl_status, objectName="statusPrimary", minimumWidth=320)
        self.progress_bar = QProgressBar(self)
        tune(self.progress_bar, range=(0, 100), value=0, format="%p%", fixedWidth=260, visible=False)
        self.lbl_elapsed = QLabel("—")
        bar.addWidget(self.lbl_status, 1)
        [bar.addPermanentWidget(w) for w in (self.progress_bar, self.lbl_elapsed)]

    def _connect_signals(self) -> None:
        self.act_add_blf.triggered.connect(self.add_blf_dialog)
        self.act_bind_dbc.triggered.connect(self.bind_dbc_dialog)
        self.act_clear_dbc.triggered.connect(self.clear_dbc_bindings)
        self.channel_combo.currentIndexChanged.connect(lambda _i: self._refresh_dbc_label())
        self.file_tree.files_dropped.connect(self.add_blf_paths)
        self.btn_remove_selected.clicked.connect(self.remove_selected_files)
        self.btn_clear_files.clicked.connect(self.clear_files)
        self.search_edit.textChanged.connect(self._on_search_changed)
        self.btn_check_visible.clicked.connect(self.check_visible_items)
        self.btn_uncheck_all.clicked.connect(self.uncheck_all_items)
        self.signal_model.itemChanged.connect(self._on_item_changed)
        self.chk_split.toggled.connect(lambda _c: self._sync_resample_hint())
        self.btn_convert.clicked.connect(self.start_convert)

    # --------------------------------------------------------- 文件管理 ----
    def add_blf_dialog(self) -> None:
        paths, _filter = QFileDialog.getOpenFileNames(
            self, "批量添加 BLF 文件", "", "Vector BLF 日志 (*.blf);;所有文件 (*)")
        if paths:
            self.add_blf_paths(paths)

    def add_blf_paths(self, paths: list[str]) -> int:
        """把 ``.blf`` 加入左侧列表，返回新增数量；已存在的自动跳过。"""
        added = 0
        for raw in paths:
            if not raw or not raw.lower().endswith(".blf"):
                continue
            path = str(Path(raw).resolve())
            if path in self.entry_by_path:
                continue
            try:
                size = os.path.getsize(path)
            except OSError:
                size = 0
            entry = BlfFileEntry(path=path, size=size)
            item = QTreeWidgetItem([entry.name, human_size(size), STATUS_PENDING])
            item.setToolTip(0, path)
            item.setForeground(2, STATUS_COLORS[STATUS_PENDING])
            entry.item = item
            self.file_tree.addTopLevelItem(item)
            self.entries.append(entry); self.entry_by_path[path] = entry
            added += 1
        if added:
            self.lbl_status.setText(f"已加入 {added} 个 BLF，共 {len(self.entries)} 个待处理")
            self.statusBar().showMessage(f"已加入 {added} 个 BLF 文件")
            self.start_scan()
        self._refresh_action_state()
        return added

    def remove_selected_files(self) -> None:
        items = self.file_tree.selectedItems()
        if not items:
            self.statusBar().showMessage("未选中任何文件")
            return
        for item in items:
            index = self.file_tree.indexOfTopLevelItem(item)
            if index < 0:
                continue
            entry = next((e for e in self.entries if e.item is item), None)
            self.file_tree.takeTopLevelItem(index)
            if entry is not None:
                self.entries.remove(entry)
                self.entry_by_path.pop(entry.path, None)
        self.lbl_status.setText(f"剩余 {len(self.entries)} 个待处理 BLF")
        self._refresh_action_state()

    def clear_files(self) -> None:
        self.file_tree.clear()
        self.entries.clear(); self.entry_by_path.clear()
        self.lbl_status.setText("文件列表已清空"); self._refresh_action_state()

    def _mark_all(self, status: str, error: str | None = None) -> None:
        for entry in self.entries:
            if error is not None:
                entry.error = None
            entry.apply_status(status, error)

    # --------------------------------------------------------- DBC 绑定 ----
    def bind_dbc_dialog(self) -> None:
        channel = self.channel_combo.currentIndex() + 1
        path, _filter = QFileDialog.getOpenFileName(
            self, f"为 {CHANNEL_CHOICES[channel - 1]} 选择 DBC", "",
            "DBC 数据库 (*.dbc);;所有文件 (*)")
        if not path:
            return
        self.dbc_by_channel[channel] = str(Path(path).resolve())
        self._refresh_dbc_label()
        self.statusBar().showMessage(
            f"已为 Ch{channel} 绑定 DBC：{os.path.basename(self.dbc_by_channel[channel])}")
        if self.entries:
            self.start_scan()

    def clear_dbc_bindings(self) -> None:
        self.dbc_by_channel.clear(); self.channel_summary.clear()
        self.signal_model.removeRows(0, self.signal_model.rowCount())
        self._refresh_dbc_label(); self.statusBar().showMessage("已清空全部 DBC 绑定")

    def _refresh_dbc_label(self) -> None:
        if not self.dbc_by_channel:
            self.dbc_label.setText("DBC 绑定：无")
            return
        self.dbc_label.setText("DBC 绑定：" + "  |  ".join(
            f"Ch{ch}:{os.path.basename(p)}" for ch, p in sorted(self.dbc_by_channel.items())))
        self.dbc_label.setToolTip("\n".join(
            f"Ch{ch} → {p}" for ch, p in sorted(self.dbc_by_channel.items())))

    # ------------------------------------------------------------- 扫描 ----
    def _spawn(self, worker: Worker, on_progress, on_ok, on_failed, on_done) -> None:
        """统一接线：进度/成功/失败/线程结束四个信号全部转回主线程。"""
        worker.progress.connect(on_progress)
        worker.finished_ok.connect(on_ok)
        worker.failed.connect(on_failed)
        worker.finished.connect(on_done)
        worker.start()

    def start_scan(self) -> None:
        """重建信号树：scan 只读扫描，不落盘、不解信号值。"""
        if not KERNEL_READY or not self.entries:
            return
        if self.scan_worker is not None and self.scan_worker.isRunning():
            return
        self._mark_all(STATUS_PARSING)
        self.progress_bar.setVisible(True); self.progress_bar.setRange(0, 0)
        self.scan_worker = Worker("scan", blf_paths=[e.path for e in self.entries],
                                  dbc_map=dict(self.dbc_by_channel), parent=self)
        self._spawn(self.scan_worker, self._on_scan_progress, self._on_scan_ok,
                    self._on_scan_failed, self._on_scan_thread_done)

    def _on_scan_progress(self, payload: dict) -> None:
        """扫描进度同样来自工作线程，只更新文本与进度条。"""
        message = str(payload.get("message") or "正在扫描 BLF 与 DBC…")
        done, total = int(payload.get("done", 0) or 0), int(payload.get("total", 0) or 0)
        if total > 0 and "(" not in message:
            message = f"{message} ({done}/{total})"
        self.lbl_status.setText(f"扫描中：{message}")
        if payload.get("file"):
            self._set_file_status(str(payload["file"]), STATUS_PARSING)

    def _on_scan_ok(self, report: dict) -> None:
        for info in report.get("files") or []:
            path = str(info.get("path", ""))
            entry = self.entry_by_path.get(path) or next(
                (e for e in self.entries if e.name == os.path.basename(path)), None)
            if entry is None:
                continue
            if info.get("ok"):
                entry.apply_status(STATUS_DONE)
            else:
                entry.apply_status(STATUS_FAILED, str(info.get("error") or "扫描失败"))
        self.channel_summary = report.get("channels") or {}
        self._populate_signal_tree()
        ok_count = sum(1 for e in self.entries if e.status == STATUS_DONE)
        self.lbl_status.setText(f"扫描完成：{ok_count}/{len(self.entries)} 个文件可用")
        self.statusBar().showMessage("扫描完成，可在右侧勾选需要导出的信号")

    def _on_scan_failed(self, message: str) -> None:
        self._mark_all(STATUS_FAILED, message)
        self.lbl_status.setText(f"扫描失败：{message}"); self.statusBar().showMessage(message)

    def _on_scan_thread_done(self) -> None:
        if self.convert_worker is None or not self.convert_worker.isRunning():
            tune(self.progress_bar, range=(0, 100), value=0, visible=False)
        self.scan_worker = None
        self._refresh_action_state()

    # ------------------------------------------------- 信号树构建/勾选 ----
    def _make_node(self, text: str, node_type: str, match_text: str, tooltip: str = "",
                   *, tristate: bool = False) -> QStandardItem:
        """统一构造带复选框的树节点。"""
        item = QStandardItem(text)
        flags = (Qt.ItemFlag.ItemIsEnabled | Qt.ItemFlag.ItemIsSelectable
                 | Qt.ItemFlag.ItemIsUserCheckable)
        if tristate:
            flags |= Qt.ItemFlag.ItemIsAutoTristate
        item.setFlags(flags)
        item.setCheckState(Qt.CheckState.Checked)
        item.setData(node_type, NODE_ROLE)
        item.setData(match_text, TEXT_ROLE)
        if tooltip:
            item.setToolTip(tooltip)
        return item

    def _populate_signal_tree(self) -> None:
        """按 ``scan`` 返回的 ``channels`` 构建三级树（字段见 INTERFACE §1）。"""
        self._checks_guard = True
        self.signal_model.removeRows(0, self.signal_model.rowCount())
        for ch_key in sorted(self.channel_summary, key=lambda k: int(k) if str(k).isdigit() else 0):
            ch_info = self.channel_summary.get(ch_key) or {}
            dbc_name = os.path.basename(ch_info["dbc"]) if ch_info.get("dbc") else None
            suffix = f"（DBC: {dbc_name}）" if dbc_name else "（未绑定 DBC，显示原始 ID）"
            ch_item = self._make_node(f"通道 Ch{ch_key}  {suffix}", "channel",
                                      f"Ch{ch_key} {dbc_name or ''}", tristate=True)
            for msg in ch_info.get("messages") or []:
                cycle = msg.get("cycle_ms")
                cycle_text = f" · {fmt_float(float(cycle), 2)}ms" if cycle else ""
                tips = [f"报文：{msg.get('name', '?')}", f"ID：{msg.get('id_hex', '')}",
                        f"帧数：{msg.get('frame_count', 0)}"]
                if cycle:
                    tips.append(f"周期：约 {fmt_float(float(cycle), 2)} ms")
                if msg.get("is_fd"):
                    tips.append("CAN FD")
                msg_item = self._make_node(
                    f"{msg.get('name', '?')}  {msg.get('id_hex', '')}{cycle_text}", "message",
                    str(msg.get("name", "")), "\n".join(tips), tristate=True)
                for sig in msg.get("signals") or []:
                    unit = sig.get("unit") or ""
                    sig_item = self._make_node(
                        f"{sig.get('name', '?')} ({unit})", "signal", str(sig.get("name", "")),
                        f"信号：{sig.get('name', '?')}\n位长：{sig.get('bit_len', '?')} bit\n"
                        f"字节序：{sig.get('byte_order', '?')}\n"
                        f"符号：{'有符号' if sig.get('is_signed') else '无符号'}\n"
                        f"factor：{sig.get('factor', 1)}  offset：{sig.get('offset', 0)}")
                    msg_item.appendRow(sig_item)
                ch_item.appendRow(msg_item)
            self.signal_model.appendRow(ch_item)
        self._checks_guard = False
        self.signal_view.expandToDepth(0)

    def _on_search_changed(self, text: str) -> None:
        self.signal_proxy.set_keyword(text)
        self.signal_view.expandAll() if text else self.signal_view.collapseAll()
        if not text:
            self.signal_view.expandToDepth(0)

    def _on_item_changed(self, item: QStandardItem) -> None:
        """勾选联动：向下广播，再向上重算父节点三态。"""
        if self._checks_guard:
            return
        self._checks_guard = True
        try:
            state = item.checkState()
            for row in range(item.rowCount()):
                self._apply_state_down(item.child(row), state)
            parent = item.parent()
            while parent is not None:
                self._recalc_parent(parent)
                parent = parent.parent()
        finally:
            self._checks_guard = False

    def _apply_state_down(self, item: QStandardItem, state: Qt.CheckState) -> None:
        item.setCheckState(state)
        for row in range(item.rowCount()):
            self._apply_state_down(item.child(row), state)

    def _recalc_parent(self, parent: QStandardItem) -> None:
        states = {parent.child(row).checkState() for row in range(parent.rowCount())}
        target = next(iter(states)) if len(states) == 1 else Qt.CheckState.PartiallyChecked
        if states and parent.checkState() != target:
            parent.setCheckState(target)

    def _sync_resample_hint(self) -> None:
        """取消“分表”后：重采样下拉高亮 + 显示 #warnHint 提示。"""
        need_hint = not self.chk_split.isChecked()
        self.lbl_merge_warning.setVisible(need_hint)
        self.combo_resample.setEnabled(need_hint)
        self.combo_resample.setProperty("warnHint", "true" if need_hint else "false")
        style = self.combo_resample.style()
        style.unpolish(self.combo_resample); style.polish(self.combo_resample)

    def check_visible_items(self) -> None:
        """把当前过滤结果中可见的通道/报文/信号全部勾选。"""
        self._checks_guard = True
        try:
            count = sum(self._check_subtree(self.signal_model.itemFromIndex(
                self.signal_proxy.mapToSource(self.signal_proxy.index(row, 0))))
                for row in range(self.signal_proxy.rowCount()))
        finally:
            self._checks_guard = False
        self._cascade_checks()
        self.statusBar().showMessage(f"已勾选 {count} 个节点（含未展开层级）")

    def _check_subtree(self, item: QStandardItem | None) -> int:
        if item is None:
            return 0
        item.setCheckState(Qt.CheckState.Checked)
        return 1 + sum(self._check_subtree(item.child(row)) for row in range(item.rowCount()))

    def uncheck_all_items(self) -> None:
        self._checks_guard = True
        try:
            for row in range(self.signal_model.rowCount()):
                self._apply_state_down(self.signal_model.item(row), Qt.CheckState.Unchecked)
        finally:
            self._checks_guard = False
        self.statusBar().showMessage("已清空全部勾选（此时启动转换将不会导出任何信号）")

    def _cascade_checks(self) -> None:
        """兜底：重算所有父节点勾选态（“全选当前过滤项”之后调用）。"""
        self._checks_guard = True
        try:
            for row in range(self.signal_model.rowCount()):
                channel_item = self.signal_model.item(row)
                for m in range(channel_item.rowCount()):
                    self._recalc_parent(channel_item.child(m))
                self._recalc_parent(channel_item)
        finally:
            self._checks_guard = False

    # --------------------------------------------------- 转换参数/启动 -----
    def build_selection(self) -> dict[str, dict[str, list[str]]]:
        """按当前勾选生成 ``selected``（INTERFACE §1.1；空 dict = 全选）。"""
        selection: dict[str, dict[str, list[str]]] = {}
        for row in range(self.signal_model.rowCount()):
            ch_item = self.signal_model.item(row)
            if ch_item.checkState() == Qt.CheckState.Unchecked:
                continue
            ch_key = str(ch_item.data(TEXT_ROLE) or "").split(" ", 1)[0].replace("Ch", "")
            msgs = {str(ch_item.child(m).data(TEXT_ROLE) or ""): [
                str(ch_item.child(m).child(x).data(TEXT_ROLE) or "")
                for x in range(ch_item.child(m).rowCount())
                if ch_item.child(m).child(x).checkState() != Qt.CheckState.Unchecked]
                for m in range(ch_item.rowCount())
                if ch_item.child(m).checkState() != Qt.CheckState.Unchecked}
            if any(msgs.values()):
                selection[ch_key] = {k: v for k, v in msgs.items() if v}
        return selection

    def _refresh_action_state(self) -> None:
        busy = bool(self.convert_worker and self.convert_worker.isRunning()) or bool(
            self.scan_worker and self.scan_worker.isRunning())
        self.btn_convert.setEnabled(KERNEL_READY and bool(self.entries) and not busy)
        hint = (KERNEL_MISSING_TEXT if not KERNEL_READY else
                "请先添加至少一个 .blf 文件" if not self.entries else
                "任务进行中…" if busy else "并行解码全部 BLF 并按选择导出结果")
        self.btn_convert.setToolTip(hint)
        for widget in (self.btn_remove_selected, self.btn_clear_files):
            widget.setEnabled(bool(self.entries) and not busy)
        for action in (self.act_add_blf, self.act_bind_dbc, self.act_clear_dbc):
            action.setEnabled(not busy)

    def start_convert(self) -> None:
        if not KERNEL_READY:
            QMessageBox.warning(self, "内核未就绪", KERNEL_MISSING_TEXT)
            return
        selection = self.build_selection()
        if not self.entries or not selection:
            QMessageBox.warning(self, "缺少输入或信号", "请先添加 .blf 文件，并在右侧勾选要导出的报文/信号。")
            return
        out_dir = QFileDialog.getExistingDirectory(
            self, "选择输出目录", str(Path(self.entries[0].path).parent))
        if not out_dir:
            return
        config = {
            "blf_paths": [e.path for e in self.entries],
            "dbc_by_channel": dict(self.dbc_by_channel),
            "selected": selection,
            "split_by_message": bool(self.chk_split.isChecked()),
            "resample_hz": RESAMPLE_CHOICES[max(0, self.combo_resample.currentIndex())][1],
            "export_format": FORMAT_CHOICES[max(0, self.combo_format.currentIndex())][1],
            "out_dir": out_dir,
            "temp_dir": os.path.join(out_dir, ".fast_blf_tmp"),
            "excel_row_limit": 1_000_000,
            "memory_budget_mb": 1536,
        }
        self._mark_all(STATUS_PARSING)
        tune(self.progress_bar, visible=True, range=(0, 100), value=0)
        self.lbl_elapsed.setText("进行中…")
        self.lbl_status.setText("任务已启动，正在并行解码…")
        self.convert_worker = Worker("convert", config=config, parent=self)
        self._spawn(self.convert_worker, self._on_convert_progress, self._on_convert_ok,
                    self._on_convert_failed, self._on_convert_thread_done)
        self._refresh_action_state()

    # -------------------------------------------------------- 进度/收尾 ----
    def _on_convert_progress(self, payload: dict) -> None:
        """进度回调（已转发回主线程）：刷新状态文本、进度条与文件状态。"""
        phase, done, total = payload.get("phase", 1), int(payload.get("done", 0) or 0), int(
            payload.get("total", 0) or 0)
        label = str(payload.get("message")
                    or PHASE_FALLBACK.get(str(payload.get("stage") or ""), "处理中"))
        if total > 0 and "(" not in label:
            label = f"{label} ({done}/{total})"
        self.lbl_status.setText(f"Phase {phase}/2: {label}")
        if total > 0:
            tune(self.progress_bar, range=(0, 100),
                 value=max(0, min(100, int(done * 100 / total))))
        else:
            self.progress_bar.setRange(0, 0)
        if payload.get("file"):
            self._set_file_status(str(payload["file"]), STATUS_PARSING)
        if phase == 2 or str(payload.get("stage")) in ("merge", "export"):
            [e.apply_status(STATUS_DONE) for e in self.entries if e.status == STATUS_PARSING]

    def _set_file_status(self, file_path: str, status: str) -> None:
        entry = self.entry_by_path.get(file_path) or next(
            (e for e in self.entries if e.name == os.path.basename(file_path)), None)
        if entry is not None and entry.status != status:
            entry.apply_status(status)

    def _on_convert_ok(self, report: dict) -> None:
        """成功回调：降级走 warning（唯一用户可见出口），否则 information。"""
        for entry in self.entries:
            if entry.status == STATUS_PARSING:
                entry.apply_status(STATUS_DONE)
        tables, rows = report.get("tables") or [], int(report.get("total_rows", 0) or 0)
        elapsed, fmt_used = float(report.get("elapsed_s", 0.0) or 0.0), report.get("format_used", "")
        errors, warnings = report.get("errors") or [], report.get("warnings") or []
        paths = [str(t.get("path", "")) for t in tables if t.get("path")]
        out_display = paths[0] if paths and len(set(paths)) == 1 else (
            os.path.dirname(paths[0]) if paths else "（无输出）")
        tune(self.progress_bar, range=(0, 100), value=100)
        self.lbl_elapsed.setText(f"{elapsed:.1f}s")
        self.lbl_status.setText(f"完成：{len(tables)} 个子表 / {rows:,} 行 / {elapsed:.1f}s")
        self.statusBar().showMessage(f"输出：{out_display}")
        summary = (f"子表数量：{len(tables)}\n总行数：{rows:,}\n耗时：{elapsed:.1f} 秒\n"
                   f"输出位置：{out_display}")
        if report.get("downgraded"):
            reason = str(report.get("downgrade_reason") or "Excel 行数超过上限，已自动降级为 CSV。")
            QMessageBox.warning(
                self, "已自动降级为 CSV",
                f"Excel 单表行数超过上限，已自动降级为 CSV 输出，数据未截断。\n\n"
                f"原因：{reason}\n\n实际输出格式：{fmt_used}\n{summary}")
        else:
            QMessageBox.information(self, "转换完成", f"输出格式：{fmt_used}\n{summary}")
        for line in warnings:
            print(f"[fast_blf_core] 警告：{line}", file=sys.stderr)
        if errors:
            first = errors[0]
            self.statusBar().showMessage(
                f"有 {len(errors)} 个文件失败，例如 {os.path.basename(str(first.get('file', '')))}："
                f"{first.get('code', '')} {first.get('message', '')}")

    def _on_convert_failed(self, message: str) -> None:
        for entry in self.entries:
            if entry.status in (STATUS_PARSING, STATUS_PENDING):
                entry.apply_status(STATUS_FAILED, message)
        tune(self.progress_bar, range=(0, 100), value=0)
        self.lbl_status.setText(f"转换失败：{message}")
        QMessageBox.critical(self, "转换失败", f"内核返回错误：\n\n{message}")

    def _on_convert_thread_done(self) -> None:
        self.convert_worker = None
        if self.lbl_elapsed.text() == "进行中…":
            self.lbl_elapsed.setText("—")
        if self.scan_worker is None or not self.scan_worker.isRunning():
            self.progress_bar.setVisible(False)
        self._refresh_action_state()

    # ------------------------------------------------------ 拖拽/关闭 ------
    def dragEnterEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        event.acceptProposedAction() if event.mimeData().hasUrls() else event.ignore()

    def dropEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        paths = [url.toLocalFile() for url in event.mimeData().urls()
                 if url.toLocalFile().lower().endswith(".blf")]
        if paths:
            self.add_blf_paths(paths)
            event.acceptProposedAction()
        else:
            event.ignore()

    def closeEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        if self.convert_worker is not None and self.convert_worker.isRunning():
            self.convert_worker.cancel(); self.convert_worker.wait(3000)
        event.accept()


# --------------------------------------------------------------- 应用装配 ----
def load_theme(app: QApplication) -> None:
    """加载 ``gui/theme.qss``；文件缺失不影响启动。"""
    try:
        app.setStyleSheet(THEME_PATH.read_text(encoding="utf-8"))
    except OSError as exc:
        print(f"[GUI] 未能加载主题 {THEME_PATH}：{exc}", file=sys.stderr)


def build_main_window() -> MainWindow:
    """构造并显示主窗口（自检与正常运行共用）。"""
    window = MainWindow()
    window.show()
    return window


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv if argv is None else argv)
    selftest = "--selftest" in args[1:]
    app = QApplication.instance() or QApplication(args)
    app.setApplicationName("Fast BLF Converter")
    app.setApplicationDisplayName("Fast BLF Converter")
    app.setStyle("Fusion")
    font = QFont("Microsoft YaHei UI", 9)
    font.setStyleStrategy(QFont.StyleStrategy.PreferAntialias); app.setFont(font)
    load_theme(app)
    window = build_main_window()
    print(f"[GUI] 内核就绪：{'是' if KERNEL_READY else '否'}；版本："
          f"{core.version() if KERNEL_READY else '—'}", file=sys.stderr)
    if not KERNEL_READY:
        print(f"[GUI] {KERNEL_MISSING_TEXT}")
    if selftest:
        QTimer.singleShot(400, app.quit)
    else:
        window.lbl_status.setText("就绪：请添加 BLF 文件并绑定 DBC" if KERNEL_READY
                                  else KERNEL_MISSING_TEXT)
    return int(app.exec())


if __name__ == "__main__":
    raise SystemExit(main())
