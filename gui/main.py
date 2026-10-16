#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Fast BLF Converter —— PySide6 图形界面（Step 3 + Step 6a）。

* 内核延迟导入：``fast_blf_core`` 未编译时 GUI 仍可构造与自检，只禁用转换按钮。
* 线程安全：``scan``/``convert`` 的 ``progress_cb`` 由 Rust 工作线程调用，
  统一经 ``Worker.progress``（``Signal(dict)``）转发到主线程，绝不在工作线程碰控件。
* 接口对齐：全部字段严格遵循 ``docs/INTERFACE.md`` §1 / §1.1 / §1.2 / §2。
* Step 6a：信号树改为``通道 → DBC 节点 → 报文 → 信号``四级；DBC 通过通道节点右键
  「📄 添加 DBC…」逐个绑定，再点工具栏「🔍 解析信号」统一扫描；DBC 解析失败只在
  通道 / DBC 节点上用红字 + tooltip 展示，绝不弹窗（用户明确要求）。
* Step 6c：工具栏新增「💾 保存配置 / 📂 加载配置 / 🗑 删除配置」，把 DBC 映射 + 选中
  信号 + 导出策略存成 ``configs/<名称>.json``（目录解析与读写见 :mod:`gui.presets`）。
* Step 7c：进度条从状态栏右侧的 260px 小控件改为「底部控制面板与状态栏之间」的通栏
  进度区（始终占一行高度），解码期间按 ``frames_done`` 显示确定性进度 / 速率 / 字节
  估算；底部控制面板新增「时间列格式」下拉，写入 config 的 ``time_format``（见
  ``INTERFACE.md`` §1.1）。
"""

from __future__ import annotations

import json
import os
import re
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

from PySide6.QtCore import (
    QModelIndex, QObject, QSortFilterProxyModel, QThread, QTimer, Qt, Signal,
)
from PySide6.QtGui import (
    QAction, QColor, QFont, QFontDatabase, QPixmap, QStandardItem, QStandardItemModel,
)
from PySide6.QtWidgets import (
    QAbstractItemView, QApplication, QCheckBox, QComboBox, QFileDialog, QFrame, QHBoxLayout,
    QHeaderView, QInputDialog, QLabel, QLineEdit, QMainWindow, QMenu, QMessageBox,
    QProgressBar, QPushButton, QSizePolicy, QSplitter, QStatusBar, QToolBar, QTreeView,
    QTreeWidget, QTreeWidgetItem, QVBoxLayout, QWidget,
)

try:  # 以脚本方式运行（python gui/main.py）时 gui/ 已在 sys.path 首位
    from check_delegate import CheckStateDelegate
    import presets as preset_store
except ImportError:  # 作为包导入（gui.main）时
    from gui.check_delegate import CheckStateDelegate
    from gui import presets as preset_store

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
#: 时间列口径（Step 7c；取值与 ``INTERFACE.md`` §1.1 的 ``time_format`` 一一对应）。
TIME_FORMAT_CHOICES: list[tuple[str, str]] = [
    ("相对时间（秒）", "relative_s"),
    ("相对时间（毫秒）", "relative_ms"),
    ("绝对时间（日期 时间）", "absolute"),
    ("Unix 时间戳（秒）", "unix_s"),
]
TIME_FORMAT_TOOLTIP = (
    "绝对时间 = BLF 文件头记录的墙钟时间（与 CANalyzer 显示一致）；"
    "相对时间以全部输入文件中最早的时间戳为 0")
#: 进度区（Step 7c）文案。
PROGRESS_IDLE_TEXT = "待命"
PROGRESS_IDLE_META = "—"
PHASE_STAGE_LABELS = {"scan": "扫描中", "decode": "解码中", "merge": "合并中", "export": "导出中"}
#: 绝对时间输出的正则（用于 ``--selftest-progress`` 校验）。
ABSOLUTE_TIME_RE = re.compile(r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{6}$")
NODE_ROLE = Qt.ItemDataRole.UserRole + 1
TEXT_ROLE = Qt.ItemDataRole.UserRole + 2
#: 节点携带的结构化数据（按名字恢复勾选 / 回读 DBC 绑定用）。
CH_ROLE = Qt.ItemDataRole.UserRole + 3       # 通道号 int
MSG_ROLE = Qt.ItemDataRole.UserRole + 4      # 报文名 str
SIG_ROLE = Qt.ItemDataRole.UserRole + 5      # 信号名 str
DBC_PATH_ROLE = Qt.ItemDataRole.UserRole + 6  # DBC 绝对路径 str

#: 节点类型（存于 NODE_ROLE）。
NODE_CHANNEL, NODE_DBC, NODE_MESSAGE, NODE_SIGNAL = "channel", "dbc", "message", "signal"

#: DBC 节点状态文案（对齐任务书 Step 6a 第 1 条）。
DBC_STATUS_OK, DBC_STATUS_FAIL, DBC_STATUS_PENDING = "✓ 已解析", "✗ 解析失败", "未解析"
DBC_ERROR_COLOR = QColor("#E5534B")  # DBC 解析失败 / 文件缺失的红字颜色
DBC_MISSING_SUFFIX = "（DBC 文件缺失）"

WARN_MERGE_HINT = "合并为单张宽表时建议选择重采样频率，否则会因周期不同产生空值"
PHASE_FALLBACK = {"scan": "正在扫描", "decode": "正在并行解码", "merge": "正在流式合并", "export": "正在导出"}
STATUS_COLORS = {
    STATUS_PENDING: QColor("#9A9AB0"), STATUS_PARSING: QColor("#0078D4"),
    STATUS_DONE: QColor("#4CC26B"), STATUS_FAILED: QColor("#F05A5A"),
}
#: `--dump-check-states` 默认落盘的视觉验收图（Step 6b）。
DUMP_CHECK_STATES_DEFAULT = (Path(__file__).resolve().parent.parent
                             / "docs" / "screenshots" / "check-states.png")
DUMP_CHECK_WIDTH = 420


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


def fmt_duration(seconds: float) -> str:
    """秒 → ``MM:SS``（不足 1 小时）或 ``H:MM:SS``（进度区的「已用 / 用时」）。"""
    try:
        total = max(0, int(seconds))
    except (TypeError, ValueError):
        total = 0
    hours, remainder = divmod(total, 3600)
    minutes, secs = divmod(remainder, 60)
    return f"{hours}:{minutes:02d}:{secs:02d}" if hours else f"{minutes:02d}:{secs:02d}"


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
        #: 解析前的勾选快照（Step 6a §4：按名字恢复勾选）。
        self._check_snapshot: dict[str, set] | None = None
        #: Step 6c：当前配置名 / 缺失 DBC 的通道 / 加载后待恢复的勾选。
        self.current_preset_name: str | None = None
        self.missing_dbc_channels: set[int] = set()
        self._pending_restore: dict | None = None
        self.last_restore: dict = {}
        #: Step 7c：进度区状态机 + 最后一次转换结果（供无人值守自检读取）。
        self._pv: dict = self._new_progress_state()
        self._busy_pos = 0
        self._busy_step = 8
        self.progress_hook = None      # 测试用：每个进度 payload 都会原样回调
        self.last_report: dict | None = None
        self.last_error: str | None = None
        self.quiet_dialogs = False     # 自检/无人值守：抑制所有模态弹窗
        self._progress_timer = QTimer(self)
        self._progress_timer.setInterval(200)
        self._progress_timer.timeout.connect(self._on_progress_tick)
        self._build_toolbar()
        self._build_central()
        self._build_statusbar()
        self._connect_signals()
        self._seed_channels()
        self._sync_resample_hint()
        self._refresh_action_state()
        self._progress_idle()
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
        """工具栏（Step 6a §3）：添加 BLF / 解析信号 / 全部清除 DBC。

        通道下拉框 + 「绑定 DBC 到该通道」已按用户要求移除——DBC 改为在左侧通道
        节点上右键「📄 添加 DBC…」逐个绑定。
        """
        bar = QToolBar("主工具栏", self)
        bar.setMovable(False)
        bar.setFloatable(False)
        self.addToolBar(Qt.ToolBarArea.TopToolBarArea, bar)
        self.act_add_blf = QAction("📂 批量添加 BLF", self)
        tune(self.act_add_blf, shortcut="Ctrl+O", statusTip="选择多个 .blf 加入队列（也可直接拖拽到左侧列表）")
        bar.addAction(self.act_add_blf)
        bar.addSeparator()
        self.btn_parse = QPushButton("🔍 解析信号", self)
        self.btn_parse.setObjectName("primaryButton")
        self.btn_parse.setToolTip("按当前 BLF 列表与各通道 DBC 扫描信号树（可在没有 BLF 时先看 DBC 信号）")
        bar.addWidget(self.btn_parse)
        self.act_clear_dbc = QAction("🗑 全部清除 DBC", self)
        tune(self.act_clear_dbc, statusTip="清空所有通道的 DBC 绑定与树里的 DBC 节点（需二次确认）")
        bar.addAction(self.act_clear_dbc)
        bar.addSeparator()
        # Step 6c：配置预设三入口（保存 / 加载 / 删除）。
        self.act_save_config = QAction("💾 保存配置", self)
        tune(self.act_save_config, shortcut="Ctrl+S",
             statusTip=f"把当前 DBC 映射 + 选中信号 + 导出策略保存为命名配置（目录：{preset_store.config_dir()}）")
        bar.addAction(self.act_save_config)
        self.act_load_config = QAction("📂 加载配置", self)
        tune(self.act_load_config, statusTip="加载配置并自动解析信号、恢复勾选与导出策略")
        bar.addAction(self.act_load_config)
        self.act_delete_config = QAction("🗑 删除配置", self)
        tune(self.act_delete_config, statusTip="删除已保存的配置（需二次确认）")
        bar.addAction(self.act_delete_config)
        bar.addSeparator()
        self.dbc_label = QLabel("DBC 绑定：无")
        tune(self.dbc_label, objectName="hintLabel")
        self.dbc_label.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Preferred)
        bar.addWidget(self.dbc_label)

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
        # Step 7c：通栏进度区位于控制面板与状态栏之间，始终占一行高度（不 setVisible）。
        layout.addWidget(self._build_progress_panel())
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
        title = QLabel("信号筛选（通道 → DBC → 报文 → 信号；在通道节点上右键添加 DBC，"
                       "只有勾选的节点才会参与转换）")
        tune(title, objectName="hintLabel")
        layout.addWidget(title)
        self.search_edit = QLineEdit(panel)
        tune(self.search_edit, placeholderText="🔍 搜索 DBC / 报文名 / 信号名（大小写不敏感的模糊匹配）", clearButtonEnabled=True)
        self.btn_check_visible = QPushButton("全选当前过滤项")
        self.btn_uncheck_all = QPushButton("一键清空")
        row = QHBoxLayout()
        row.addWidget(self.search_edit, 1)
        row.addWidget(self.btn_check_visible)
        row.addWidget(self.btn_uncheck_all)
        layout.addLayout(row)
        self.signal_model = QStandardItemModel(0, 1, self)
        self.signal_model.setHeaderData(0, Qt.Orientation.Horizontal, "通道 / DBC / 报文 / 信号")
        self.signal_proxy = SignalFilterProxy(self)
        self.signal_proxy.setSourceModel(self.signal_model)
        self.signal_view = QTreeView(panel)
        self.signal_view.setModel(self.signal_proxy)
        # Step 6b：三态复选框改为自绘（空框 / 蓝框+横条 / 实心蓝+白勾），
        # 避免 QSS 里实心蓝 indicator 在蓝色选中行上“隐身”、且无法区分部分选中。
        self.signal_view.setItemDelegate(CheckStateDelegate(self.signal_view))
        tune(self.signal_view, alternatingRowColors=True, uniformRowHeights=True, indentation=18,
             editTriggers=QAbstractItemView.EditTrigger.NoEditTriggers,
             selectionMode=QAbstractItemView.SelectionMode.ExtendedSelection,
             contextMenuPolicy=Qt.ContextMenuPolicy.CustomContextMenu)
        self.signal_view.header().setSectionResizeMode(0, QHeaderView.ResizeMode.Stretch)
        layout.addWidget(self.signal_view, 1)
        tip = QLabel("提示：在通道节点上右键「📄 添加 DBC…」；DBC 节点只作展示不可勾选，"
                     "通道勾选由其下所有报文聚合。")
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
        # Step 7c：时间列口径（写入 config 的 time_format，默认 relative_s）。
        row.addWidget(QLabel("时间列格式："))
        self.combo_time_format = QComboBox(panel)
        self.combo_time_format.addItems([label for label, _tf in TIME_FORMAT_CHOICES])
        tune(self.combo_time_format, currentIndex=0, toolTip=TIME_FORMAT_TOOLTIP)
        row.addWidget(self.combo_time_format)
        row.addStretch(1)
        self.btn_convert = QPushButton("🚀 启动多核极速转换")
        self.btn_convert.setObjectName("primaryButton")
        row.addWidget(self.btn_convert)
        outer.addLayout(row)
        self.lbl_merge_warning = QLabel(WARN_MERGE_HINT)
        tune(self.lbl_merge_warning, objectName="warnHint", wordWrap=True, visible=False)
        outer.addWidget(self.lbl_merge_warning)
        return panel

    def _build_progress_panel(self) -> QWidget:
        """Step 7c：底部通栏进度区（进度条 + 左侧状态文字 + 右侧百分比/字节）。

        * 始终占据一行高度：待命时显示灰字 ``待命``，转换开始即进入工作态，绝不
          ``setVisible`` 造成布局跳动；
        * 左侧状态文字（``#E8E8F0``）与右侧百分比（等宽字体）同一行，下方是全宽进度条；
        * 进度条高 20px、圆角 9px、填充 ``#0078D4``、槽 ``#2D2D38``（见 ``theme.qss``）。
        """
        panel = QFrame(self)
        panel.setObjectName("progressPanel")
        outer = self._column((12, 7, 12, 7), 6)
        panel.setLayout(outer)
        row = QHBoxLayout()
        row.setSpacing(10)
        self.lbl_progress_status = QLabel(PROGRESS_IDLE_TEXT)
        self.lbl_progress_status.setObjectName("progressStatus")
        # Ignored 横向策略：超长错误消息只被裁切，不会把窗口撑宽或换行撑高。
        self.lbl_progress_status.setSizePolicy(QSizePolicy.Policy.Ignored,
                                               QSizePolicy.Policy.Preferred)
        self.lbl_progress_meta = QLabel(PROGRESS_IDLE_META)
        self.lbl_progress_meta.setObjectName("progressMeta")
        self.lbl_progress_meta.setFont(_mono_font())
        row.addWidget(self.lbl_progress_status, 1)
        row.addWidget(self.lbl_progress_meta, 0)
        outer.addLayout(row)
        self.progress_bar = QProgressBar(panel)
        self.progress_bar.setObjectName("progressBar")
        tune(self.progress_bar, range=(0, 100), value=0, textVisible=False, fixedHeight=20)
        outer.addWidget(self.progress_bar)
        return panel

    def _build_statusbar(self) -> None:
        bar = QStatusBar(self)
        self.setStatusBar(bar)
        # Step 6c：状态栏 tooltip 暴露实际配置目录，用户一眼知道 json 存在哪。
        self.lbl_status = QLabel("就绪")
        tune(self.lbl_status, objectName="statusPrimary", minimumWidth=320)
        self.lbl_config_dir = QLabel("⚙ 配置目录")
        config_dir = preset_store.config_dir()
        self.lbl_config_dir.setToolTip(f"配置预设（*.json）保存位置：\n{config_dir}")
        bar.setToolTip(f"配置预设（*.json）保存位置：\n{config_dir}")
        bar.addWidget(self.lbl_status, 1)
        # Step 7c：进度条已移到通栏进度区，状态栏只保留配置目录入口。
        bar.addPermanentWidget(self.lbl_config_dir)

    def _connect_signals(self) -> None:
        self.act_add_blf.triggered.connect(self.add_blf_dialog)
        self.btn_parse.clicked.connect(self.start_parse_signals)
        self.act_clear_dbc.triggered.connect(self.clear_dbc_bindings)
        self.act_save_config.triggered.connect(self.save_config_dialog)
        self.act_load_config.triggered.connect(self.load_config_dialog)
        self.act_delete_config.triggered.connect(self.delete_config_dialog)
        self.file_tree.files_dropped.connect(self.add_blf_paths)
        self.btn_remove_selected.clicked.connect(self.remove_selected_files)
        self.btn_clear_files.clicked.connect(self.clear_files)
        self.search_edit.textChanged.connect(self._on_search_changed)
        self.btn_check_visible.clicked.connect(self.check_visible_items)
        self.btn_uncheck_all.clicked.connect(self.uncheck_all_items)
        self.signal_model.itemChanged.connect(self._on_item_changed)
        self.signal_view.customContextMenuRequested.connect(self._on_signal_context_menu)
        self.chk_split.toggled.connect(lambda _c: self._sync_resample_hint())
        # start_convert 接受可选 out_dir（自检用），用一个无参 lambda 吞掉 clicked 的 bool。
        self.btn_convert.clicked.connect(lambda: self.start_convert())

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
            self.lbl_status.setText(f"已加入 {added} 个 BLF，共 {len(self.entries)} 个待处理"
                                    "（点「🔍 解析信号」后重建信号树）")
            self.statusBar().showMessage(f"已加入 {added} 个 BLF 文件，可继续添加 DBC 后统一解析")
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

    # ------------------------------------------------- 通道节点 / DBC 绑定 ----
    def _seed_channels(self) -> None:
        """启动时预置 Ch1~Ch8 通道节点。

        通道节点是「📄 添加 DBC…」右键菜单的唯一入口；没有预置节点时，用户必须
        先加 BLF 并解析才能看到通道，无法完成"先加完所有 DBC 再统一解析"的流程。
        """
        self._checks_guard = True
        try:
            for channel in range(1, len(CHANNEL_CHOICES) + 1):
                self._channel_item(channel)
        finally:
            self._checks_guard = False

    def _channel_item(self, channel: int) -> QStandardItem:
        """按通道号查找通道节点，不存在则按序插入一个。"""
        for row in range(self.signal_model.rowCount()):
            item = self.signal_model.item(row)
            if int(item.data(CH_ROLE) or 0) == channel:
                return item
        item = self._make_node(f"通道 Ch{channel}", NODE_CHANNEL, f"Ch{channel}",
                               tristate=True, channel=channel)
        target = self.signal_model.rowCount()
        for row in range(self.signal_model.rowCount()):
            if int(self.signal_model.item(row).data(CH_ROLE) or 0) > channel:
                target = row
                break
        self.signal_model.insertRow(target, item)
        return item

    @staticmethod
    def _is_checkable(item: QStandardItem | None) -> bool:
        return bool(item is not None
                    and (item.flags() & Qt.ItemFlag.ItemIsUserCheckable))

    def _iter_messages(self, channel_item: QStandardItem):
        """产出通道节点下所有报文节点（跨 DBC 子节点聚合）。"""
        for row in range(channel_item.rowCount()):
            child = channel_item.child(row)
            kind = child.data(NODE_ROLE)
            if kind == NODE_MESSAGE:
                yield child
            elif kind == NODE_DBC:
                for sub in range(child.rowCount()):
                    grand = child.child(sub)
                    if grand.data(NODE_ROLE) == NODE_MESSAGE:
                        yield grand

    def _on_signal_context_menu(self, pos) -> None:
        """只在通道节点上弹出「📄 添加 DBC…」（Step 6a §2）。"""
        index = self.signal_view.indexAt(pos)
        if not index.isValid():
            return
        item = self.signal_model.itemFromIndex(self.signal_proxy.mapToSource(index))
        if item is None or item.data(NODE_ROLE) != NODE_CHANNEL:
            return
        channel = int(item.data(CH_ROLE) or 0)
        menu = QMenu(self.signal_view)
        action = menu.addAction("📄 添加 DBC…")
        chosen = menu.exec(self.signal_view.viewport().mapToGlobal(pos))
        if chosen is action:
            self.add_dbc_dialog(channel)

    def add_dbc_dialog(self, channel: int) -> None:
        """为指定通道选择 DBC：写入映射 + 立即插入 DBC 节点（不自动扫描）。"""
        path, _filter = QFileDialog.getOpenFileName(
            self, f"为 {CHANNEL_CHOICES[channel - 1]} 选择 DBC", "",
            "DBC 数据库 (*.dbc);;所有文件 (*)")
        if not path:
            return
        resolved = str(Path(path).resolve())
        self.dbc_by_channel[channel] = resolved
        self._upsert_dbc_node(channel, resolved, DBC_STATUS_PENDING)
        self._refresh_dbc_label()
        self._refresh_action_state()
        self.statusBar().showMessage(
            f"已为 Ch{channel} 添加 DBC：{os.path.basename(resolved)}"
            "（点「🔍 解析信号」后生效）")

    def _upsert_dbc_node(self, channel: int, path: str, status: str) -> QStandardItem:
        """在通道下插入/更新唯一的 DBC 节点（不提供移除单个 DBC 的入口）。"""
        channel_item = self._channel_item(channel)
        for row in range(channel_item.rowCount() - 1, -1, -1):
            if channel_item.child(row).data(NODE_ROLE) == NODE_DBC:
                channel_item.takeRow(row)
        node = self._make_node(f"📄 {os.path.basename(path)}   {status}", NODE_DBC,
                               os.path.basename(path), f"{path}\n{status}",
                               checkable=False, channel=channel, dbc_path=path)
        channel_item.appendRow(node)
        return node

    def _remove_dbc_nodes(self, channel: int | None = None) -> int:
        """移除树里的 DBC 节点（连带其下报文/信号）；``channel=None`` 表示全部通道。"""
        removed = 0
        for row in range(self.signal_model.rowCount()):
            channel_item = self.signal_model.item(row)
            if channel is not None and int(channel_item.data(CH_ROLE) or 0) != channel:
                continue
            for idx in range(channel_item.rowCount() - 1, -1, -1):
                if channel_item.child(idx).data(NODE_ROLE) == NODE_DBC:
                    channel_item.takeRow(idx)
                    removed += 1
        return removed

    def _collect_dbc_by_channel(self) -> dict[int, str]:
        """回读树里实际存在的 DBC 绑定（通道号 → 绝对路径）。"""
        collected: dict[int, str] = {}
        for row in range(self.signal_model.rowCount()):
            channel_item = self.signal_model.item(row)
            channel = int(channel_item.data(CH_ROLE) or 0)
            for idx in range(channel_item.rowCount()):
                node = channel_item.child(idx)
                if node.data(NODE_ROLE) == NODE_DBC:
                    path = str(node.data(DBC_PATH_ROLE) or "")
                    if path:
                        collected[channel] = path
        return collected

    def clear_dbc_bindings(self) -> None:
        """「🗑 全部清除 DBC」：二次确认后清空映射与树里的 DBC 节点。"""
        has_nodes = any(
            self.signal_model.item(row).child(idx).data(NODE_ROLE) == NODE_DBC
            for row in range(self.signal_model.rowCount())
            for idx in range(self.signal_model.item(row).rowCount()))
        if not self.dbc_by_channel and not has_nodes:
            self.statusBar().showMessage("当前没有已添加的 DBC")
            return
        answer = QMessageBox.question(
            self, "全部清除 DBC",
            "确定要清空所有通道的 DBC 绑定吗？\n\n"
            "此操作只移除 DBC（及其报文/信号），不影响已添加的 BLF 文件，且不可撤销。",
            QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
            QMessageBox.StandardButton.No)
        if answer != QMessageBox.StandardButton.Yes:
            return
        self.dbc_by_channel.clear()
        removed = self._remove_dbc_nodes()
        self._refresh_dbc_label()
        self._refresh_action_state()
        self.lbl_status.setText("已清除全部 DBC 绑定")
        self.statusBar().showMessage(f"已清除全部 DBC（移除 {removed} 个 DBC 节点）")

    def _refresh_dbc_label(self) -> None:
        if not self.dbc_by_channel:
            self.dbc_label.setText("DBC 绑定：无")
            return
        self.dbc_label.setText("DBC 绑定：" + "  |  ".join(
            f"Ch{ch}:{os.path.basename(p)}" for ch, p in sorted(self.dbc_by_channel.items())))
        self.dbc_label.setToolTip("\n".join(
            f"Ch{ch} → {p}" for ch, p in sorted(self.dbc_by_channel.items())))

    # --------------------------------------------------------- 配置预设 ----
    def current_export_policy(self) -> dict:
        """当前导出策略（拆表 / 重采样 / 格式），与配置里的 ``export`` 同构。"""
        return {
            "split_by_message": bool(self.chk_split.isChecked()),
            "resample_hz": RESAMPLE_CHOICES[max(0, self.combo_resample.currentIndex())][1],
            "export_format": FORMAT_CHOICES[max(0, self.combo_format.currentIndex())][1],
        }

    def apply_export_policy(self, export: dict | None) -> None:
        """把配置里的导出策略写回控件（缺字段用默认值兜底）。"""
        export = export if isinstance(export, dict) else {}
        self.chk_split.setChecked(bool(export.get("split_by_message", True)))
        hz = export.get("resample_hz")
        self.combo_resample.setCurrentIndex(next(
            (i for i, (_label, value) in enumerate(RESAMPLE_CHOICES) if value == hz), 0))
        fmt = export.get("export_format")
        self.combo_format.setCurrentIndex(next(
            (i for i, (_label, value) in enumerate(FORMAT_CHOICES) if value == fmt), 0))
        self._sync_resample_hint()

    def apply_dbc_map(self, dbc_by_channel: dict | None) -> set[int]:
        """用配置里的 DBC 映射替换当前绑定；路径不存在的通道记入 ``missing_dbc_channels``。"""
        self.dbc_by_channel = {int(ch): str(path) for ch, path in (dbc_by_channel or {}).items()}
        self.missing_dbc_channels = set()
        self._remove_dbc_nodes()
        for channel in sorted(self.dbc_by_channel):
            path = self.dbc_by_channel[channel]
            node = self._upsert_dbc_node(channel, path, DBC_STATUS_PENDING)
            if not os.path.isfile(path):
                self.missing_dbc_channels.add(channel)
                node.setForeground(DBC_ERROR_COLOR)
                node.setToolTip(f"{path}\nDBC 文件不存在（配置里的路径已失效）")
        self._refresh_dbc_label()
        self._refresh_action_state()
        return self.missing_dbc_channels

    def save_preset(self, name: str) -> Path:
        """把当前 DBC 映射 + 选中信号 + 导出策略保存为命名配置，返回落盘路径。"""
        path = preset_store.save_preset(
            name, dict(self.dbc_by_channel), self.build_selection(), self.current_export_policy())
        self.current_preset_name = path.stem
        return path

    def save_config_dialog(self) -> None:
        """「💾 保存配置」：命名（预填当前配置名）→ 同名询问覆盖。"""
        name, accepted = QInputDialog.getText(
            self, "保存配置", "配置名称（不同 DBC / 信号组合建议分开命名）：",
            text=self.current_preset_name or "")
        if not accepted:
            return
        try:
            safe = preset_store.safe_name(name)
        except preset_store.PresetError as exc:
            QMessageBox.warning(self, "配置名无效", str(exc))
            return
        if preset_store.preset_path(safe).exists():
            answer = QMessageBox.question(
                self, "覆盖配置", f"配置「{safe}」已存在，是否覆盖？",
                QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
                QMessageBox.StandardButton.No)
            if answer != QMessageBox.StandardButton.Yes:
                return
        try:
            path = self.save_preset(name)
        except preset_store.PresetError as exc:
            QMessageBox.warning(self, "保存失败", str(exc))
            return
        self.lbl_status.setText(f"配置「{path.stem}」已保存")
        self.statusBar().showMessage(f"配置「{path.stem}」已保存到 {path}")

    def load_config_dialog(self) -> None:
        """「📂 加载配置」：按修改时间倒序列出，选中后交给 :meth:`apply_preset`。"""
        presets, skipped = preset_store.scan_presets()
        if not presets:
            QMessageBox.information(self, "加载配置", "配置目录中还没有可用配置。")
            return
        labels = [preset.display for preset in presets]
        choice, accepted = QInputDialog.getItem(
            self, "加载配置", "选择配置（按修改时间倒序）：", labels, 0, False)
        if not accepted or choice not in labels:
            return
        self.apply_preset(presets[labels.index(choice)], skipped=skipped)

    def apply_preset(self, preset, skipped: int = 0) -> None:
        """加载配置：DBC 映射 → 自动解析 → 解析完成后恢复勾选 → 应用导出策略。"""
        self.current_preset_name = preset.name
        missing = self.apply_dbc_map(preset.dbc_by_channel)
        self.apply_export_policy(preset.export)
        if KERNEL_READY and (self.entries or self.dbc_by_channel):
            self._pending_restore = {"name": preset.name, "selected": preset.selected,
                                     "skipped": skipped}
            self.statusBar().showMessage(
                f"正在加载配置「{preset.name}」（DBC {len(self.dbc_by_channel)} 个，"
                f"缺失 {len(missing)} 个）…")
            self.start_parse_signals()
            return
        message = (f"配置「{preset.name}」已加载：DBC {len(self.dbc_by_channel)} 个"
                   f"（缺失 {len(missing)} 个）")
        if not KERNEL_READY:
            message += "；内核未就绪，暂无法恢复信号勾选"
        self.lbl_status.setText(message)
        self.statusBar().showMessage(message)

    def delete_config_dialog(self) -> None:
        """「🗑 删除配置」：选择 + 二次确认后删除文件。"""
        presets, _skipped = preset_store.scan_presets()
        if not presets:
            QMessageBox.information(self, "删除配置", "配置目录中还没有可删除的配置。")
            return
        labels = [preset.display for preset in presets]
        choice, accepted = QInputDialog.getItem(
            self, "删除配置", "选择要删除的配置：", labels, 0, False)
        if not accepted or choice not in labels:
            return
        preset = presets[labels.index(choice)]
        answer = QMessageBox.question(
            self, "确认删除", f"确定删除配置「{preset.name}」吗？\n\n{preset.path}\n\n此操作不可撤销。",
            QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
            QMessageBox.StandardButton.No)
        if answer != QMessageBox.StandardButton.Yes:
            return
        try:
            removed = preset_store.delete_preset(preset.path)
        except preset_store.PresetError as exc:
            QMessageBox.warning(self, "删除失败", str(exc))
            return
        if self.current_preset_name == preset.name:
            self.current_preset_name = None
        self.statusBar().showMessage(
            f"配置「{preset.name}」已删除" if removed else f"配置「{preset.name}」已不存在")

    def _restore_selection(self, selected: dict | None) -> tuple[int, int]:
        """按 ``(通道, 报文, 信号)`` 恢复勾选；返回 ``(恢复数, 未匹配数)``。"""
        known: dict[tuple[int, str, str], QStandardItem] = {}
        for row in range(self.signal_model.rowCount()):
            channel_item = self.signal_model.item(row)
            channel = int(channel_item.data(CH_ROLE) or 0)
            for msg_item in self._iter_messages(channel_item):
                message = str(msg_item.data(MSG_ROLE) or "")
                for idx in range(msg_item.rowCount()):
                    sig_item = msg_item.child(idx)
                    known[(channel, message, str(sig_item.data(SIG_ROLE) or ""))] = sig_item
        self._checks_guard = True
        try:  # 配置是"全量快照"：先全部取消，再精确勾选配置里的信号。
            for row in range(self.signal_model.rowCount()):
                self._apply_state_down(self.signal_model.item(row), Qt.CheckState.Unchecked)
        finally:
            self._checks_guard = False
        restored = missed = 0
        self._checks_guard = True
        try:
            for ch_key, messages in (selected or {}).items():
                try:
                    channel = int(ch_key)
                except (TypeError, ValueError):
                    continue
                for message, signals in (messages or {}).items():
                    for signal in signals or []:
                        item = known.get((channel, str(message), str(signal)))
                        if item is None:
                            missed += 1
                        else:
                            item.setCheckState(Qt.CheckState.Checked)
                            restored += 1
        finally:
            self._checks_guard = False
        self._cascade_checks()
        self.last_restore = {"restored": restored, "missed": missed}
        return restored, missed

    # ------------------------------------------------------------- 扫描 ----
    def _spawn(self, worker: Worker, on_progress, on_ok, on_failed, on_done) -> None:
        """统一接线：进度/成功/失败/线程结束四个信号全部转回主线程。"""
        worker.progress.connect(on_progress)
        worker.finished_ok.connect(on_ok)
        worker.failed.connect(on_failed)
        worker.finished.connect(on_done)
        worker.start()

    def start_parse_signals(self) -> None:
        """「🔍 解析信号」：按当前 BLF + DBC 映射重建信号树（Step 6a §4）。

        * BLF 为空也允许：只按 DBC 列出报文/信号（``frame_count=0``）。
        * 两者都为空时只提示状态栏，不弹窗。
        * 解析前记录勾选，重建后按名字恢复。
        """
        if not KERNEL_READY:
            self.statusBar().showMessage(KERNEL_MISSING_TEXT)
            return
        # 树与 self.dbc_by_channel 是两份状态（历史上有过"绑了 DBC 却解不出信号"的坑）：
        # 守卫与扫描输入都必须用**合并后**的映射，缺一侧也能自愈。
        merged_dbc = dict(self.dbc_by_channel)
        merged_dbc.update(self._collect_dbc_by_channel())
        if not self.entries and not merged_dbc:
            self.statusBar().showMessage("请先添加 BLF 或绑定 DBC")
            return
        if self.scan_worker is not None and self.scan_worker.isRunning():
            return
        self._check_snapshot = self._capture_checks()
        if self.entries:
            self._mark_all(STATUS_PARSING)
        self._begin_busy("Phase 1/2 · 扫描中…")
        self.lbl_status.setText("正在解析信号…")
        self.statusBar().showMessage("正在解析信号…")
        self.scan_worker = Worker("scan", blf_paths=[e.path for e in self.entries],
                                  dbc_map=merged_dbc, parent=self)
        self._spawn(self.scan_worker, self._on_scan_progress, self._on_scan_ok,
                    self._on_scan_failed, self._on_scan_thread_done)
        self._refresh_action_state()

    def _on_scan_progress(self, payload: dict) -> None:
        """解析进度来自工作线程，只更新文本与进度条（解析期间状态栏提示不变）。"""
        message = str(payload.get("message") or "正在解析信号…")
        done, total = int(payload.get("done", 0) or 0), int(payload.get("total", 0) or 0)
        label = "正在解析信号…"
        if total > 0:
            label = f"正在解析信号… ({done}/{total})"
        self.lbl_status.setText(label)
        self.statusBar().showMessage(f"正在解析信号… {message}")
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
        self._check_snapshot = None
        pending, self._pending_restore = self._pending_restore, None
        if pending is not None:  # Step 6c：配置加载 → 解析完成后恢复勾选
            restored, missed = self._restore_selection(pending.get("selected") or {})
            message = (f"配置「{pending.get('name', '')}」已加载："
                       f"DBC {len(self.dbc_by_channel)} 个"
                       f"（缺失 {len(self.missing_dbc_channels)} 个）、恢复信号 {restored} 个")
            if missed:
                message += f"（{missed} 个未匹配）"
            if pending.get("skipped"):
                message += f"；已跳过 {pending['skipped']} 个无法解析的配置"
            self.lbl_status.setText(message)
            self.statusBar().showMessage(message)
            return
        if self.entries:
            ok_count = sum(1 for e in self.entries if e.status == STATUS_DONE)
            self.lbl_status.setText(f"解析完成：{ok_count}/{len(self.entries)} 个文件可用")
        else:
            self.lbl_status.setText("解析完成（未添加 BLF，仅按 DBC 列出信号）")
        self.statusBar().showMessage("解析完成，可在右侧勾选需要导出的信号")

    def _on_scan_failed(self, message: str) -> None:
        self._check_snapshot = None
        self._mark_all(STATUS_FAILED, message)
        self.lbl_status.setText(f"解析失败：{message}"); self.statusBar().showMessage(message)

    def _on_scan_thread_done(self) -> None:
        if self.convert_worker is None or not self.convert_worker.isRunning():
            if not self._pv.get("finished"):
                self._progress_idle()
        self.scan_worker = None
        self._refresh_action_state()

    # ------------------------------------------------- 信号树构建/勾选 ----
    def _make_node(self, text: str, node_type: str, match_text: str, tooltip: str = "",
                   *, tristate: bool = False, checkable: bool = True,
                   channel: int | None = None, message: str | None = None,
                   signal: str | None = None, dbc_path: str | None = None) -> QStandardItem:
        """统一构造树节点；``checkable=False`` 用于只作展示的 DBC 节点。"""
        item = QStandardItem(text)
        flags = Qt.ItemFlag.ItemIsEnabled | Qt.ItemFlag.ItemIsSelectable
        if checkable:
            flags |= Qt.ItemFlag.ItemIsUserCheckable
            if tristate:
                flags |= Qt.ItemFlag.ItemIsAutoTristate
        item.setFlags(flags)
        if checkable:
            item.setCheckState(Qt.CheckState.Checked)
        item.setData(node_type, NODE_ROLE)
        item.setData(match_text, TEXT_ROLE)
        if channel is not None:
            item.setData(int(channel), CH_ROLE)
        if message is not None:
            item.setData(message, MSG_ROLE)
        if signal is not None:
            item.setData(signal, SIG_ROLE)
        if dbc_path is not None:
            item.setData(dbc_path, DBC_PATH_ROLE)
        if tooltip:
            item.setToolTip(tooltip)
        return item

    def _capture_checks(self) -> dict[str, set]:
        """记录解析前的勾选：``(通道, 报文, 信号)`` 集合 + 已知节点集合。

        ``known_*`` 用来区分"用户明确取消勾选"与"节点是新出现的"：新报文/新信号
        按默认规则勾选（Step 6a §4）。
        """
        checked_signals: set = set()
        checked_messages: set = set()
        known_signals: set = set()
        known_messages: set = set()
        for row in range(self.signal_model.rowCount()):
            channel_item = self.signal_model.item(row)
            channel = int(channel_item.data(CH_ROLE) or 0)
            for msg_item in self._iter_messages(channel_item):
                name = str(msg_item.data(MSG_ROLE) or "")
                mkey = (channel, name)
                known_messages.add(mkey)
                if msg_item.rowCount() == 0:
                    if msg_item.checkState() == Qt.CheckState.Checked:
                        checked_messages.add(mkey)
                    continue
                for idx in range(msg_item.rowCount()):
                    sig_item = msg_item.child(idx)
                    skey = (channel, name, str(sig_item.data(SIG_ROLE) or ""))
                    known_signals.add(skey)
                    if sig_item.checkState() == Qt.CheckState.Checked:
                        checked_signals.add(skey)
        return {"checked_signals": checked_signals, "checked_messages": checked_messages,
                "known_signals": known_signals, "known_messages": known_messages}

    def _build_message_item(self, channel: int, msg: dict, snapshot: dict) -> QStandardItem:
        """构造一个报文节点及其信号子节点（含按名字恢复勾选）。"""
        name = str(msg.get("name", "?"))
        cycle = msg.get("cycle_ms")
        cycle_text = f" · {float(cycle):.1f}ms" if cycle else ""
        frame_count = int(msg.get("frame_count", 0) or 0)
        tips = [f"报文：{name}", f"ID：{msg.get('id_hex', '')}", f"帧数：{frame_count}"]
        if cycle:
            tips.append(f"周期：约 {float(cycle):.1f} ms")
        if msg.get("is_fd"):
            tips.append("CAN FD")
        msg_item = self._make_node(
            f"{name}  {msg.get('id_hex', '')}{cycle_text}  {frame_count} 帧", NODE_MESSAGE,
            name, "\n".join(tips), tristate=True, channel=channel, message=name)
        mkey = (channel, name)
        known_messages = snapshot.get("known_messages") or set()
        known_signals = snapshot.get("known_signals") or set()
        checked_signals = snapshot.get("checked_signals") or set()
        checked_messages = snapshot.get("checked_messages") or set()
        for sig in msg.get("signals") or []:
            sig_name = str(sig.get("name", "?"))
            unit = sig.get("unit") or ""
            sig_item = self._make_node(
                f"{sig_name} ({unit})", NODE_SIGNAL, sig_name,
                f"信号：{sig_name}\n位长：{sig.get('bit_len', '?')} bit\n"
                f"字节序：{sig.get('byte_order', '?')}\n"
                f"符号：{'有符号' if sig.get('is_signed') else '无符号'}\n"
                f"factor：{sig.get('factor', 1)}  offset：{sig.get('offset', 0)}",
                channel=channel, message=name, signal=sig_name)
            skey = (channel, name, sig_name)
            # 已知信号：按解析前的勾选恢复；新信号：默认勾选。
            checked = skey in checked_signals or skey not in known_signals
            if mkey not in known_messages:
                checked = True  # 新报文默认全选
            sig_item.setCheckState(Qt.CheckState.Checked if checked else Qt.CheckState.Unchecked)
            msg_item.appendRow(sig_item)
        if msg_item.rowCount() == 0:
            # 无信号报文（未绑 DBC 的原始 ID 报文）：按解析前状态恢复，默认勾选。
            state = Qt.CheckState.Checked
            if mkey in known_messages and mkey not in checked_messages:
                state = Qt.CheckState.Unchecked
            msg_item.setCheckState(state)
        return msg_item

    def _populate_signal_tree(self) -> None:
        """按 ``scan`` 返回的 ``channels`` 构建四级树：通道 → DBC → 报文 → 信号。

        DBC 解析失败只在通道 / DBC 节点上红字 + tooltip 展示，绝不弹窗。
        """
        snapshot = self._check_snapshot or {}
        previous = self._checks_guard
        self._checks_guard = True
        try:
            self.signal_model.removeRows(0, self.signal_model.rowCount())
            channels = dict(self.channel_summary)
            keys = {int(k) for k in channels if str(k).isdigit()}
            keys |= {int(k) for k in self.dbc_by_channel}
            for channel in sorted(keys):
                ch_info = channels.get(str(channel)) or channels.get(channel) or {}
                dbc_error = ch_info.get("dbc_error")
                dbc_path = ch_info.get("dbc") or self.dbc_by_channel.get(channel)
                is_missing = channel in self.missing_dbc_channels
                suffix = ("（DBC 解析失败）" if dbc_error else
                          DBC_MISSING_SUFFIX if is_missing else "")
                if dbc_error:
                    tip = str(dbc_error)
                elif is_missing:
                    tip = f"Ch{channel}\nDBC 文件不存在：{dbc_path or '（未记录路径）'}"
                else:
                    tip = (f"Ch{channel}" + (f"\nDBC：{dbc_path}" if dbc_path
                                             else "\n未绑定 DBC，显示原始 ID"))
                ch_item = self._make_node(f"通道 Ch{channel}{suffix}", NODE_CHANNEL,
                                          f"Ch{channel}", tip, tristate=True, channel=channel)
                if dbc_error or is_missing:
                    ch_item.setForeground(DBC_ERROR_COLOR)
                if dbc_path:
                    parsed = bool(ch_info.get("dbc"))
                    dbc_note = ch_info.get("dbc_note")
                    status = (DBC_STATUS_FAIL if (dbc_error or is_missing) else
                              DBC_STATUS_OK if parsed else DBC_STATUS_PENDING)
                    node_tip = (str(dbc_error) if dbc_error else
                                f"{dbc_path}\nDBC 文件不存在" if is_missing
                                else f"{dbc_path}\n{status}")
                    # Step 11：解析成功且被规范化过时，状态后缀追加「（已规范化）」，
                    # 完整说明放 tooltip（悬停可见），绝不弹窗；失败时 tooltip 仍是 dbc_error。
                    node_status = status
                    if not (dbc_error or is_missing) and parsed and dbc_note:
                        node_status = f"{status}（已规范化）"
                        node_tip = f"{dbc_path}\n{status}\n规范化说明：{dbc_note}"
                    node = self._make_node(
                        f"📄 {os.path.basename(str(dbc_path))}   {node_status}", NODE_DBC,
                        os.path.basename(str(dbc_path)), node_tip,
                        checkable=False, channel=channel, dbc_path=str(dbc_path))
                    if dbc_error or is_missing:
                        node.setForeground(DBC_ERROR_COLOR)
                    ch_item.appendRow(node)
                    container = node
                else:
                    container = ch_item
                for msg in ch_info.get("messages") or []:
                    if isinstance(msg, dict):
                        container.appendRow(self._build_message_item(channel, msg, snapshot))
                self.signal_model.appendRow(ch_item)
            for row in range(self.signal_model.rowCount()):
                ch_item = self.signal_model.item(row)
                for msg_item in self._iter_messages(ch_item):
                    self._recalc_parent(msg_item)
                self._recalc_parent(ch_item)
        finally:
            self._checks_guard = previous
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
        """向下广播勾选；DBC 等不可勾选容器只递归、不写状态。"""
        if self._is_checkable(item):
            item.setCheckState(state)
        for row in range(item.rowCount()):
            self._apply_state_down(item.child(row), state)

    def _checkable_states(self, item: QStandardItem) -> list[Qt.CheckState]:
        """收集所有可勾选后代的勾选态（跨越不可勾选的 DBC 容器）。"""
        states: list[Qt.CheckState] = []
        for row in range(item.rowCount()):
            child = item.child(row)
            if self._is_checkable(child):
                states.append(child.checkState())
            else:
                states.extend(self._checkable_states(child))
        return states

    def _recalc_parent(self, parent: QStandardItem) -> None:
        """按可勾选后代重算父节点三态（通道跨 DBC 聚合其下全部报文）。"""
        if not self._is_checkable(parent):
            return
        states = self._checkable_states(parent)
        if not states:
            return
        unique = set(states)
        target = next(iter(unique)) if len(unique) == 1 else Qt.CheckState.PartiallyChecked
        if parent.checkState() != target:
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
        count = 0
        if self._is_checkable(item):
            item.setCheckState(Qt.CheckState.Checked)
            count = 1
        return count + sum(self._check_subtree(item.child(row)) for row in range(item.rowCount()))

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
        previous = self._checks_guard
        self._checks_guard = True
        try:
            for row in range(self.signal_model.rowCount()):
                channel_item = self.signal_model.item(row)
                for msg_item in self._iter_messages(channel_item):
                    self._recalc_parent(msg_item)
                self._recalc_parent(channel_item)
        finally:
            self._checks_guard = previous

    # --------------------------------------------------- 转换参数/启动 -----
    def build_selection(self) -> dict[str, dict[str, list[str]]]:
        """按当前勾选生成 ``selected``（INTERFACE §1.1；空 dict = 全选）。"""
        selection: dict[str, dict[str, list[str]]] = {}
        for row in range(self.signal_model.rowCount()):
            ch_item = self.signal_model.item(row)
            if ch_item.checkState() == Qt.CheckState.Unchecked:
                continue
            ch_key = str(ch_item.data(CH_ROLE) or "")
            msgs: dict[str, list[str]] = {}
            for msg_item in self._iter_messages(ch_item):
                if msg_item.checkState() == Qt.CheckState.Unchecked:
                    continue
                signals = [str(msg_item.child(x).data(SIG_ROLE) or "")
                           for x in range(msg_item.rowCount())
                           if msg_item.child(x).checkState() != Qt.CheckState.Unchecked]
                if signals:
                    msgs[str(msg_item.data(MSG_ROLE) or "")] = signals
            if msgs:
                selection[ch_key] = msgs
        return selection

    def _refresh_action_state(self) -> None:
        busy = bool(self.convert_worker and self.convert_worker.isRunning()) or bool(
            self.scan_worker and self.scan_worker.isRunning())
        has_input = bool(self.entries) or bool(self.dbc_by_channel)
        self.btn_convert.setEnabled(KERNEL_READY and bool(self.entries) and not busy)
        hint = (KERNEL_MISSING_TEXT if not KERNEL_READY else
                "请先添加至少一个 .blf 文件" if not self.entries else
                "任务进行中…" if busy else "并行解码全部 BLF 并按选择导出结果")
        self.btn_convert.setToolTip(hint)
        self.btn_parse.setEnabled(KERNEL_READY and has_input and not busy)
        self.btn_parse.setToolTip(
            KERNEL_MISSING_TEXT if not KERNEL_READY else
            "请先添加 BLF 或在通道上右键添加 DBC" if not has_input else
            "任务进行中…" if busy else "按当前 BLF 列表与各通道 DBC 扫描并重建信号树")
        self.act_clear_dbc.setEnabled(not busy and bool(self.dbc_by_channel))
        for action in (self.act_save_config, self.act_load_config, self.act_delete_config):
            action.setEnabled(not busy)
        for widget in (self.btn_remove_selected, self.btn_clear_files):
            widget.setEnabled(bool(self.entries) and not busy)
        self.act_add_blf.setEnabled(not busy)

    def start_convert(self, out_dir: str | None = None) -> None:
        """启动转换；``out_dir`` 为 ``None`` 时弹出目录选择框（自检传入临时目录跳过）。"""
        if not KERNEL_READY:
            if not self.quiet_dialogs:
                QMessageBox.warning(self, "内核未就绪", KERNEL_MISSING_TEXT)
            return
        selection = self.build_selection()
        if not self.entries or not selection:
            if not self.quiet_dialogs:
                QMessageBox.warning(self, "缺少输入或信号",
                                    "请先添加 .blf 文件，并在右侧勾选要导出的报文/信号。")
            return
        if out_dir is None:
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
            # Step 7c：时间列口径是 INTERFACE §1.1 的必填字段，不能漏。
            "time_format": TIME_FORMAT_CHOICES[max(0, self.combo_time_format.currentIndex())][1],
        }
        self.last_report = None
        self.last_error = None
        self._mark_all(STATUS_PARSING)
        self._begin_progress(selection)
        self.lbl_status.setText("任务已启动，正在并行解码…")
        self.convert_worker = Worker("convert", config=config, parent=self)
        self._spawn(self.convert_worker, self._on_convert_progress, self._on_convert_ok,
                    self._on_convert_failed, self._on_convert_thread_done)
        self._refresh_action_state()

    # ------------------------------------------------ 进度区（Step 7c） ----
    def _new_progress_state(self) -> dict:
        """进度区的一次任务状态（转换 / 扫描各一份，结束后保留最后一次结果）。"""
        return {
            "active": False, "finished": False, "busy": True, "phase": None, "stage": None,
            "done": 0, "total": 0, "file": None, "frames": 0, "value": None,
            "rate": 0.0, "rate_unit": "帧/s", "rate_file": None, "rate_frames": 0,
            "rate_at": 0.0, "started": 0.0, "done_files": set(), "bytes_done": 0,
            "bytes_total": 0, "est_total_frames": 0.0, "label_override": None,
            "done_exact": True, "phase1_complete": False,
        }

    def _set_progress_state(self, state: str) -> None:
        """给进度区两个标签打 ``state`` 动态属性（QSS 据此着色）并重刷样式。"""
        for widget in (self.lbl_progress_status, self.lbl_progress_meta):
            widget.setProperty("state", state)
            style = widget.style()
            style.unpolish(widget)
            style.polish(widget)

    def _progress_idle(self) -> None:
        """待命态：灰字 ``待命`` + 空进度条；进度区始终可见，不改变布局高度。"""
        self._progress_timer.stop()
        self._pv = self._new_progress_state()
        self.progress_bar.setRange(0, 100)
        self.progress_bar.setValue(0)
        self.lbl_progress_status.setText(PROGRESS_IDLE_TEXT)
        self.lbl_progress_status.setToolTip("")
        self.lbl_progress_meta.setText(PROGRESS_IDLE_META)
        self._set_progress_state("idle")

    def _begin_busy(self, text: str) -> None:
        """不确定态（busy）：解析信号等无 ``frames_done`` 的任务，进度条必须已经在动。"""
        self._progress_timer.stop()
        pv = self._new_progress_state()
        self._pv = pv
        pv["active"] = True
        pv["busy"] = True
        pv["started"] = time.monotonic()
        pv["label_override"] = text
        self._busy_pos = 0
        self._busy_step = 8
        self.progress_bar.setRange(0, 100)
        self.progress_bar.setValue(0)
        self._set_progress_state("working")
        self._render_progress()
        self._progress_timer.start()

    def _begin_progress(self, selection: dict) -> None:
        """进入转换工作态：重置状态机并预取"选中报文的预期帧数"用于文件内进度估算。"""
        self._progress_timer.stop()
        pv = self._new_progress_state()
        self._pv = pv
        pv["active"] = True
        pv["busy"] = True
        pv["phase"] = 1
        pv["stage"] = "decode"
        pv["started"] = time.monotonic()
        pv["bytes_total"] = sum(int(e.size) for e in self.entries)
        pv["est_total_frames"] = self._known_frames(selection)
        self._busy_pos = 0
        self._busy_step = 8
        self.progress_bar.setRange(0, 100)
        self.progress_bar.setValue(0)
        self._set_progress_state("working")
        self._render_progress()
        self._progress_timer.start()

    def _known_frames(self, selection: dict) -> float:
        """从 ``scan`` 结果里取"被选中报文"的观测帧总数（用于文件内百分比/字节估算）。

        取不到时返回 0，进度退化为按文件维度推进（不显示文件内百分比）。
        """
        total = 0.0
        found = False
        for ch_key, info in (self.channel_summary or {}).items():
            messages = {str(m.get("name")): m for m in (info.get("messages") or [])}
            names = (list((selection or {}).get(str(ch_key), {}).keys())
                     if selection else list(messages.keys()))
            for name in names:
                msg = messages.get(str(name))
                if msg is None:
                    continue
                total += float(msg.get("frame_count") or 0)
                found = True
        return total if found else 0.0

    def _entry_size(self, path: str | None) -> int:
        """BLF 文件大小（列表里已知）；找不到返回 0。"""
        if not path:
            return 0
        entry = self.entry_by_path.get(path) or next(
            (e for e in self.entries if e.name == os.path.basename(str(path))), None)
        return int(entry.size) if entry is not None else 0

    def _current_file_fraction(self, frames: int) -> float | None:
        """当前文件的帧占比估算：``frames_done / (全局预期帧数 × 文件字节占比)``。"""
        pv = self._pv
        if frames <= 0 or pv["est_total_frames"] <= 0 or pv["bytes_total"] <= 0 or not pv["file"]:
            return None
        size = self._entry_size(str(pv["file"]))
        if size <= 0:
            return None
        expected = pv["est_total_frames"] * size / pv["bytes_total"]
        if expected <= 0:
            return None
        return max(0.0, min(0.99, frames / expected))

    def _progress_percent(self, phase: int, done: int, total: int, frames: int) -> int | None:
        """``total > 0`` 时返回 0–100，``total == 0``（尚无进度）返回 ``None``。"""
        if total <= 0:
            return None
        if phase == 1:
            within = self._current_file_fraction(frames) or 0.0
            fraction = (done + within) / total
        else:
            fraction = done / total
        return max(0, min(100, int(round(fraction * 100))))

    def _bytes_progress_text(self) -> str | None:
        """``412 MB / 1.2 GB``；含文件内估算或无法精确归属时前面加 ``≈``。"""
        pv = self._pv
        total = int(pv["bytes_total"])
        if total <= 0:
            return None
        if pv["phase1_complete"] or (pv["phase"] == 1 and pv["total"] > 0
                                     and pv["done"] >= pv["total"]):
            return f"{human_size(total)} / {human_size(total)}"
        within = self._current_file_fraction(int(pv["frames"]))
        if within is not None and pv["file"]:
            estimated = int(pv["bytes_done"] + self._entry_size(str(pv["file"])) * within)
            return f"≈ {human_size(min(total, estimated))} / {human_size(total)}"
        if pv["bytes_done"]:
            prefix = "" if pv["done_exact"] else "≈ "
            return f"{prefix}{human_size(int(pv['bytes_done']))} / {human_size(total)}"
        return f"≈ 0 B / {human_size(total)}"

    def _update_rate(self, file_path, frames: int, now: float) -> None:
        """速率 = ``frames_done`` 差值 / 回调间隔（GUI 侧按 ``time.monotonic`` 算）。"""
        pv = self._pv
        path = str(file_path) if file_path else None
        if path != pv["rate_file"]:  # 换文件：frames_done 归零，重置基准不发速率
            pv["rate_file"] = path
            pv["rate_frames"] = frames
            pv["rate_at"] = now
            pv["rate"] = 0.0
            return
        interval = now - pv["rate_at"]
        if interval < 0.4:
            return
        delta = frames - pv["rate_frames"]
        if delta < 0:
            pv["rate_frames"] = frames
            pv["rate_at"] = now
            return
        rate = delta / interval
        pv["rate_frames"] = frames
        pv["rate_at"] = now
        if pv["est_total_frames"] > 0 and pv["bytes_total"] > 0:
            bytes_per_frame = pv["bytes_total"] / pv["est_total_frames"]
            pv["rate"] = rate * bytes_per_frame / 1_000_000.0
            pv["rate_unit"] = "MB/s"
        else:
            pv["rate"] = rate
            pv["rate_unit"] = "帧/s"

    def _rate_text(self) -> str:
        pv = self._pv
        if pv["rate"] <= 0:
            return ""
        if pv["rate_unit"] == "MB/s":
            return f"≈ {pv['rate']:.0f} MB/s"
        return f"{pv['rate']:,.0f} 帧/s"

    def _render_progress(self) -> None:
        """把状态机渲染成左侧状态文字 + 右侧百分比/字节 + 进度条。"""
        pv = self._pv
        if not pv["active"]:
            return
        if pv["label_override"]:
            status = str(pv["label_override"])
        else:
            elapsed = (time.monotonic() - pv["started"]) if pv["started"] else 0.0
            parts: list[str] = []
            if pv["phase"]:
                parts.append(f"Phase {int(pv['phase'])}/2")
            parts.append(PHASE_STAGE_LABELS.get(str(pv["stage"] or ""), "处理中"))
            if pv["total"] > 0:
                parts.append(f"{pv['done']}/{pv['total']} 文件")
            if pv["file"]:
                parts.append(os.path.basename(str(pv["file"])))
            parts.append(f"已 {int(pv['frames']):,} 帧")
            rate = self._rate_text()
            if rate:
                parts.append(rate)
            parts.append(f"已用 {fmt_duration(elapsed)}")
            status = " · ".join(parts)
        self.lbl_progress_status.setText(status)
        self.lbl_progress_status.setToolTip(status)
        percent = pv["value"]
        percent_text = f"{percent}%" if percent is not None else "—"
        bytes_text = self._bytes_progress_text()
        self.lbl_progress_meta.setText(f"{percent_text} · {bytes_text}" if bytes_text
                                       else percent_text)
        if percent is not None and not pv["busy"]:
            self.progress_bar.setRange(0, 100)
            self.progress_bar.setValue(int(percent))

    def _on_progress_tick(self) -> None:
        """200ms 心跳：busy 时让进度条来回扫动（保证"已经在动"），并刷新「已用」。"""
        pv = self._pv
        if not pv["active"] or pv["finished"]:
            return
        if pv["busy"] or pv["value"] is None:
            self._busy_pos += self._busy_step
            if self._busy_pos >= 100:
                self._busy_pos = 100
                self._busy_step = -abs(self._busy_step)
            elif self._busy_pos <= 0:
                self._busy_pos = 0
                self._busy_step = abs(self._busy_step)
            self.progress_bar.setRange(0, 100)
            self.progress_bar.setValue(self._busy_pos)
        self._render_progress()

    def _on_convert_progress(self, payload: dict) -> None:
        """进度回调（已转发回主线程）：记录原样 payload → 刷新进度区与文件状态。"""
        if self.progress_hook is not None:
            self.progress_hook(dict(payload))
        now = time.monotonic()
        phase = int(payload.get("phase", 1) or 1)
        stage = str(payload.get("stage") or "")
        done = int(payload.get("done", 0) or 0)
        total = int(payload.get("total", 0) or 0)
        frames = int(payload.get("frames_done", 0) or 0)
        file_path = payload.get("file")
        pv = self._pv
        pv["active"] = True
        pv["finished"] = False
        pv["label_override"] = None
        if file_path:
            pv["file"] = str(file_path)
        # 文件维度 done 递增 ⇒ 有文件完成。并行解码下 done 可能一次跳多格，无法精确
        # 归属时退化为"按文件数比例"估算（并标 ≈）。
        previous_done = pv["done"]
        if phase == 1 and total > 0 and done > previous_done:
            if done - previous_done == 1 and pv["file"]:
                pv["done_files"].add(str(pv["file"]))
            else:
                pv["done_exact"] = False
        if phase == 1 and total > 0 and done >= total:
            pv["phase1_complete"] = True
        pv["phase"], pv["stage"], pv["done"], pv["total"], pv["frames"] = \
            phase, stage, done, total, frames
        if pv["phase1_complete"]:
            pv["bytes_done"] = pv["bytes_total"]
        elif pv["done_exact"]:
            pv["bytes_done"] = sum(self._entry_size(p) for p in pv["done_files"])
        else:
            pv["bytes_done"] = int(pv["bytes_total"] * done / max(1, total))
        self._update_rate(file_path, frames, now)
        pv["busy"] = total <= 0
        pv["value"] = self._progress_percent(phase, done, total, frames)
        self._render_progress()
        label = str(payload.get("message")
                    or PHASE_FALLBACK.get(stage, "处理中"))
        if total > 0 and "(" not in label:
            label = f"{label} ({done}/{total})"
        self.lbl_status.setText(f"Phase {phase}/2: {label}")
        if file_path:
            self._set_file_status(str(file_path), STATUS_PARSING)
        if phase == 2 or stage in ("merge", "export"):
            [e.apply_status(STATUS_DONE) for e in self.entries if e.status == STATUS_PARSING]

    def _finish_progress_ok(self, report: dict) -> None:
        """成功：进度条满格 + 绿色完成行（保留到下次转换）。"""
        self._progress_timer.stop()
        pv = self._pv
        pv["active"] = True
        pv["finished"] = True
        pv["busy"] = False
        pv["value"] = 100
        pv["label_override"] = None
        self.progress_bar.setRange(0, 100)
        self.progress_bar.setValue(100)
        tables = report.get("tables") or []
        elapsed = float(report.get("elapsed_s", 0.0) or 0.0)
        paths = [str(t.get("path", "")) for t in tables if t.get("path")]
        out_display = paths[0] if paths and len(set(paths)) == 1 else (
            os.path.dirname(paths[0]) if paths else "（无输出）")
        text = (f"✓ 完成：{len(tables)} 张表 · 用时 {fmt_duration(elapsed)} · "
                f"输出：{out_display}")
        if report.get("downgraded"):
            text += "（Excel 超限，已自动降级为 CSV，数据未截断）"
        self.lbl_progress_status.setText(text)
        self.lbl_progress_status.setToolTip(f"{text}\n{report.get('downgrade_reason') or ''}".strip())
        self.lbl_progress_meta.setText(f"100% · {human_size(int(pv['bytes_total']))}"
                                       if pv["bytes_total"] else "100%")
        self._set_progress_state("success")
        self.statusBar().showMessage(f"输出：{out_display}")

    def _finish_progress_failed(self, message: str) -> None:
        """失败：进度区红字提示（不弹窗）；``BAD_TIME_HEADER`` 额外给出时间格式建议。"""
        self._progress_timer.stop()
        pv = self._pv
        pv["active"] = True
        pv["finished"] = True
        pv["busy"] = False
        pv["value"] = None
        self.progress_bar.setRange(0, 100)
        self.progress_bar.setValue(0)
        text = f"✗ {message}"
        if "BAD_TIME_HEADER" in message:
            text += ("｜文件头缺少有效墙钟时间：请改用「相对时间（秒/毫秒）」"
                     "或「Unix 时间戳（秒）」")
        self.lbl_progress_status.setText(text)
        self.lbl_progress_status.setToolTip(text)
        self.lbl_progress_meta.setText("失败")
        self._set_progress_state("error")

    def _set_file_status(self, file_path: str, status: str) -> None:
        entry = self.entry_by_path.get(file_path) or next(
            (e for e in self.entries if e.name == os.path.basename(file_path)), None)
        if entry is not None and entry.status != status:
            entry.apply_status(status)

    def _on_convert_ok(self, report: dict) -> None:
        """成功回调：进度区绿色完成行；仅"Excel 降级"保留弹窗（SPEC §3.B）。"""
        self.last_report = report
        for entry in self.entries:
            if entry.status == STATUS_PARSING:
                entry.apply_status(STATUS_DONE)
        tables, rows = report.get("tables") or [], int(report.get("total_rows", 0) or 0)
        elapsed, fmt_used = float(report.get("elapsed_s", 0.0) or 0.0), report.get("format_used", "")
        errors, warnings = report.get("errors") or [], report.get("warnings") or []
        self._finish_progress_ok(report)
        self.lbl_status.setText(f"完成：{len(tables)} 个子表 / {rows:,} 行 / {elapsed:.1f}s")
        if report.get("downgraded") and not self.quiet_dialogs:
            paths = [str(t.get("path", "")) for t in tables if t.get("path")]
            out_display = paths[0] if paths and len(set(paths)) == 1 else (
                os.path.dirname(paths[0]) if paths else "（无输出）")
            reason = str(report.get("downgrade_reason") or "Excel 行数超过上限，已自动降级为 CSV。")
            QMessageBox.warning(
                self, "已自动降级为 CSV",
                f"Excel 单表行数超过上限，已自动降级为 CSV 输出，数据未截断。\n\n"
                f"原因：{reason}\n\n实际输出格式：{fmt_used}\n"
                f"子表数量：{len(tables)}\n总行数：{rows:,}\n耗时：{elapsed:.1f} 秒\n"
                f"输出位置：{out_display}")
        for line in warnings:
            print(f"[fast_blf_core] 警告：{line}", file=sys.stderr)
        if errors:
            first = errors[0]
            self.statusBar().showMessage(
                f"有 {len(errors)} 个文件失败，例如 {os.path.basename(str(first.get('file', '')))}："
                f"{first.get('code', '')} {first.get('message', '')}")

    def _on_convert_failed(self, message: str) -> None:
        """失败回调：进度区红字（Step 7c 起不再弹窗）。"""
        self.last_error = message
        for entry in self.entries:
            if entry.status in (STATUS_PARSING, STATUS_PENDING):
                entry.apply_status(STATUS_FAILED, message)
        self._finish_progress_failed(message)
        self.lbl_status.setText(f"转换失败：{message}")
        print(f"[fast_blf_core] 转换失败：{message}", file=sys.stderr)

    def _on_convert_thread_done(self) -> None:
        self.convert_worker = None
        if not self._pv.get("finished"):  # 线程结束却没收到 ok/failed 时的兜底
            self._progress_idle()
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
        # 关闭窗口时把两类工作线程都收掉：只处理转换线程时，扫描中关窗可能让进程退不出去。
        for worker in (self.convert_worker, self.scan_worker):
            if worker is None or not worker.isRunning():
                continue
            cancel = getattr(worker, "cancel", None)
            if callable(cancel):
                try:
                    cancel()
                except Exception:  # noqa: BLE001 - 关闭阶段不允许再抛
                    pass
            worker.wait(3000)
        event.accept()


# --------------------------------------------------------------- 应用装配 ----
#: UI 字体候选：Windows 命中 YaHei UI，macOS 落到 PingFang SC（与 theme.qss 一致）。
UI_FONT_FAMILIES = ("Microsoft YaHei UI", "Microsoft YaHei", "PingFang SC", "Segoe UI")
#: 等宽字体候选（进度区右侧百分比/耗时）：Windows 命中 Consolas，macOS 落到 Menlo。
MONO_FONT_FAMILIES = ("Consolas", "Cascadia Mono", "Menlo", "Monaco", "Courier New")
#: theme.qss 里的全局字体声明（加载时会被替换成本机可用的族，见 load_theme）。
THEME_FONT_DECL = ('font-family: "Microsoft YaHei UI", "Microsoft YaHei", '
                   '"PingFang SC", "Segoe UI", sans-serif;')
#: theme.qss 里进度区的等宽字体声明（加载时同样替换成本机可用的族）。
THEME_MONO_DECL = ('font-family: "Consolas", "Cascadia Mono", "Menlo", "Courier New", '
                   'monospace;')


def _resolve_font_family() -> str | None:
    """返回本机真实存在的第一个 UI 字体族（都没有则 None）。"""
    available = set(QFontDatabase.families())
    return next((family for family in UI_FONT_FAMILIES if family in available), None)


def _resolve_mono_family() -> str | None:
    """返回本机真实存在的第一个等宽字体族（都没有则 None）。"""
    available = set(QFontDatabase.families())
    return next((family for family in MONO_FONT_FAMILIES if family in available), None)


def _runtime_font_decl() -> str | None:
    """只保留本机真实存在的字体族，避免 Qt 打印 ``missing font family`` 警告。"""
    available = set(QFontDatabase.families())
    families = [f'"{family}"' for family in UI_FONT_FAMILIES if family in available]
    if not families:  # 一个都没有：保持 theme.qss 原样，交给 Qt 自行替换
        return None
    return "font-family: " + ", ".join(families) + ";"


def _runtime_mono_decl() -> str | None:
    """进度区等宽字体的本机可用族声明（避免 ``missing font family`` 警告）。"""
    available = set(QFontDatabase.families())
    families = [f'"{family}"' for family in MONO_FONT_FAMILIES if family in available]
    if not families:
        return None
    return "font-family: " + ", ".join(families) + ", monospace;"


def _ui_font() -> QFont:
    """挑一个本机真实存在的 UI 字体，避免 ``missing font family`` 警告。"""
    family = _resolve_font_family()
    if family:
        return QFont(family, 9)
    font = QFont()
    font.setPointSize(9)
    return font


def _mono_font() -> QFont:
    """进度区右侧百分比/耗时的等宽字体（本机不存在候选时退回系统固定字体）。"""
    family = _resolve_mono_family()
    font = QFont(family, 9) if family else QFontDatabase.systemFont(
        QFontDatabase.SystemFont.FixedFont)
    font.setPointSize(9)
    font.setStyleHint(QFont.StyleHint.Monospace)
    return font


def load_theme(app: QApplication) -> None:
    """加载 ``gui/theme.qss``；文件缺失不影响启动。

    Qt 只要在 ``font-family`` 列表里看到缺失的族就会打印 "missing font family" 警告
    （Windows 主题里的 YaHei UI 在 macOS 上不存在）。这里保留 ``theme.qss`` 中的完整
    回退串作为源头，加载时替换成本机真实可用的族：Windows 仍命中 YaHei UI，
    macOS 落到 PingFang SC，警告随之消失；进度区的等宽字体同理。
    """
    try:
        stylesheet = THEME_PATH.read_text(encoding="utf-8")
    except OSError as exc:
        print(f"[GUI] 未能加载主题 {THEME_PATH}：{exc}", file=sys.stderr)
        return
    stylesheet = stylesheet.replace(THEME_FONT_DECL,
                                    _runtime_font_decl() or THEME_FONT_DECL, 1)
    stylesheet = stylesheet.replace(THEME_MONO_DECL,
                                    _runtime_mono_decl() or THEME_MONO_DECL, 1)
    app.setStyleSheet(stylesheet)


def build_main_window() -> MainWindow:
    """构造并显示主窗口（自检与正常运行共用）。"""
    window = MainWindow()
    window.show()
    return window


def run_selftest_deep() -> int:
    """``--selftest-deep``：程序化跑通 Step 6a 主流程（BLF + DBC → 四级信号树）。

    * 成功：打印一行 ``DEEP_OK channels=.. dbc_nodes=.. messages=..`` 摘要（便于 grep），返回 0；
    * 任何断言失败：打印 ``DEEP_FAIL <原因>`` 并返回 2；
    * 内核缺失：打印 ``DEEP_SKIP no_kernel`` 并返回 0（不算失败）。

    必须在 :class:`QApplication` 已构造后调用（会构造真实的 ``MainWindow``）。
    """
    if core is None:
        print("DEEP_SKIP no_kernel")
        return 0
    root = Path(__file__).resolve().parent.parent
    blf_path = root / "fixtures" / "multi_channel.blf"
    dbc_path = root / "fixtures" / "demo.dbc"
    missing = [str(p) for p in (blf_path, dbc_path) if not p.is_file()]
    if missing:
        # 打包后 fixtures/ 不随包发布，这时应优雅跳过而不是报失败（与 no_kernel 同一约定）。
        print(f"DEEP_SKIP no_fixture {' '.join(missing)}")
        return 0
    window: MainWindow | None = None
    try:
        window = MainWindow()
        dbc_map = {1: str(dbc_path), 2: str(dbc_path)}
        window.dbc_by_channel = dict(dbc_map)
        report = core.scan([str(blf_path)], dict(dbc_map))
        window.channel_summary = report.get("channels") or {}
        window._populate_signal_tree()

        channels = window.signal_model.rowCount()
        if channels != 2:
            print(f"DEEP_FAIL channels={channels} expected=2")
            return 2
        dbc_nodes = sum(
            1 for row in range(channels)
            for idx in range(window.signal_model.item(row).rowCount())
            if window.signal_model.item(row).child(idx).data(NODE_ROLE) == NODE_DBC)
        if dbc_nodes != 2:
            print(f"DEEP_FAIL dbc_nodes={dbc_nodes} expected=2")
            return 2
        messages = [str(msg.data(MSG_ROLE) or "") for row in range(channels)
                    for msg in window._iter_messages(window.signal_model.item(row))]
        if len(messages) < 3:
            print(f"DEEP_FAIL messages={len(messages)} expected>=3")
            return 2
        absent = {"VehSpd", "BattTemp", "Insulation"} - set(messages)
        if absent:
            print(f"DEEP_FAIL missing_messages={','.join(sorted(absent))}")
            return 2
        collected = window._collect_dbc_by_channel()
        if collected != dbc_map:
            print(f"DEEP_FAIL dbc_readback={collected!r} expected={dbc_map!r}")
            return 2

        # Step 11：DBC 节点要能反映"该 DBC 被规范化过"。
        def _dbc_node_texts(model) -> list[str]:
            return [
                str(model.item(row).child(idx).text())
                for row in range(model.rowCount())
                for idx in range(model.item(row).rowCount())
                if model.item(row).child(idx).data(NODE_ROLE) == NODE_DBC
            ]

        # 1) 纯 ASCII 夹具 DBC 未被规范化 → 节点文本绝不能出现「已规范化」。
        clean_texts = _dbc_node_texts(window.signal_model)
        if any("已规范化" in text for text in clean_texts):
            print(f"DEEP_FAIL clean_dbc_marked_normalized texts={clean_texts!r}")
            return 2
        # 2) 临时 GBK DBC（含中文 CM_ 注释）走一次扫描 → 节点文本必须出现「已规范化」。
        gbk_dir = Path(tempfile.mkdtemp(prefix="fblf_deep_gbk_"))
        gbk_dbc = gbk_dir / "gbk.dbc"
        gbk_dbc.write_bytes(
            b'VERSION ""\n\n'
            b"BO_ 291 VehSpd: 8 Vector__XXX\n"
            b' SG_ Spd : 0|16@1+ (0.01,0) [0|0] "km/h" Vector__XXX\n\n'
            b'CM_ BO_ 291 "\xB3\xB5\xCB\xD9\xD0\xC5\xBA\xC5";\n'
        )
        gbk_report = core.scan([], {1: str(gbk_dbc)})
        window.dbc_by_channel = {1: str(gbk_dbc)}
        window.channel_summary = gbk_report.get("channels") or {}
        window._populate_signal_tree()
        gbk_texts = _dbc_node_texts(window.signal_model)
        if not any("已规范化" in text for text in gbk_texts):
            print(f"DEEP_FAIL gbk_dbc_not_marked texts={gbk_texts!r}")
            return 2
    except Exception as exc:  # noqa: BLE001 - 自检要把任何异常变成可 grep 的失败行
        print(f"DEEP_FAIL {type(exc).__name__}: {exc}")
        return 2
    finally:
        if window is not None:
            _destroy_window(window)
    print(f"DEEP_OK channels={channels} dbc_nodes={dbc_nodes} messages={len(messages)}")
    return 0


def _pump_scan(app: QApplication, window: MainWindow, timeout: float = 30.0) -> bool:
    """无事件循环的自检里驱动 Qt 事件，直到扫描线程结束（成功 True / 超时 False）。"""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        app.processEvents()
        if window.scan_worker is None:
            return True
        time.sleep(0.005)
    return False


def _pump_convert(app: QApplication, window: MainWindow, timeout: float = 600.0) -> bool:
    """无事件循环的自检里驱动 Qt 事件，直到转换线程结束（成功 True / 超时 False）。"""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        app.processEvents()
        if window.convert_worker is None:
            app.processEvents()
            return True
        time.sleep(0.002)
    return False


def run_selftest_config() -> int:
    """``--selftest-config``：配置预设的保存 / 加载 / 缺失路径无人值守 round-trip。

    * 成功：打印 ``CONFIG_OK saved=.. channels=.. signals=.. export=../.. missing=..``，返回 0；
    * 断言失败：打印 ``CONFIG_FAIL <原因>`` 并返回 2；
    * 内核缺失：打印 ``CONFIG_SKIP no_kernel`` 并返回 0。
    """
    if core is None:
        print("CONFIG_SKIP no_kernel")
        return 0
    root = Path(__file__).resolve().parent.parent
    temp_root = Path(tempfile.mkdtemp(prefix="fblf_config_selftest_"))
    # 自给自足：配置自检**不依赖 fixtures/**，这样打包后的 --selftest-config 也能真跑。
    # 这里现写一个最小可用 DBC；不提供 BLF —— 内核自 Step 5 起支持"空 BLF 扫描"
    # （只按 DBC 列出报文），因此勾选恢复链路照样被完整验证。
    dbc_path = temp_root / "self_config.dbc"
    dbc_path.write_text(
        'VERSION ""\n\n\nNS_ :\n\nBS_:\n\nBU_: ECU\n\n'
        "BO_ 256 SelfMsg: 8 ECU\n"
        ' SG_ SelfVal : 0|8@1+ (1,0) [0|255] "" ECU\n',
        encoding="utf-8",
    )
    dbc = str(dbc_path)
    os.environ["FBLF_CONFIG_DIR"] = str(temp_root)  # 用临时目录当配置目录
    app = QApplication.instance()
    window: MainWindow | None = None
    print("[config] 0/6 开始（自给自足夹具已写入临时目录）", flush=True)
    try:
        assert app is not None, "QApplication 未构造"
        window = MainWindow()
        print("[config] 1/6 MainWindow 构建完成", flush=True)
        selected = {"1": {"SelfMsg": ["SelfVal"]}}
        export = {"split_by_message": False, "resample_hz": 10.0, "export_format": "parquet"}
        # 1) 构造配置并保存，逐字段核对 JSON。
        saved = preset_store.save_preset("BMS项目A", {1: dbc, 2: dbc}, selected, export)
        assert saved.is_file(), "保存后文件不存在"
        print("[config] 2/6 预设已保存并落盘", flush=True)
        raw = json.loads(saved.read_text(encoding="utf-8"))
        assert raw.get("schema") == 1, f"schema={raw.get('schema')!r}"
        assert raw.get("name") == "BMS项目A", f"name={raw.get('name')!r}"
        assert raw.get("dbc_by_channel") == {"1": dbc, "2": dbc}, "dbc_by_channel 不一致"
        assert raw.get("selected") == selected, f"selected={raw.get('selected')!r}"
        assert raw.get("export") == export, f"export={raw.get('export')!r}"
        # 2) 清空 UI 状态 → 走加载流程 → 断言 DBC / 勾选 / 导出策略都被恢复。
        window.dbc_by_channel.clear()
        window._remove_dbc_nodes()
        window.uncheck_all_items()
        window.apply_export_policy({"split_by_message": True, "resample_hz": None,
                                    "export_format": "csv"})
        assert window.build_selection() == {}, "清空后仍有勾选残留"
        window.apply_preset(preset_store.read_preset(saved))
        assert _pump_scan(app, window), "加载配置后的自动解析超时"
        assert window.dbc_by_channel == {1: dbc, 2: dbc}, f"DBC 映射={window.dbc_by_channel!r}"
        assert not window.missing_dbc_channels, f"误报缺失={window.missing_dbc_channels}"
        assert window.current_export_policy() == export, \
            f"导出策略={window.current_export_policy()!r}"
        assert window.last_restore.get("restored") == 1, f"恢复信号={window.last_restore!r}"
        assert window.last_restore.get("missed") == 0, f"未匹配={window.last_restore!r}"
        assert window.build_selection() == selected, f"恢复后勾选={window.build_selection()!r}"
        print("[config] 3/6 加载预设 + 自动解析 + 勾选恢复完成", flush=True)
        # 3) UI 侧保存路径同样可往返（复用已恢复的勾选）。
        ui_saved = window.save_preset("UI往返")
        ui_raw = json.loads(ui_saved.read_text(encoding="utf-8"))
        assert ui_raw.get("selected") == selected, f"UI 保存 selected={ui_raw.get('selected')!r}"
        assert ui_raw.get("dbc_by_channel") == {"1": dbc, "2": dbc}, "UI 保存 DBC 不一致"
        print("[config] 4/6 UI 侧保存往返完成", flush=True)
        channels = len(window.dbc_by_channel)
        signals = int(window.last_restore["restored"])
        policy = window.current_export_policy()
        # 4) 缺失 DBC：一个路径指向不存在的文件，加载不得抛异常且通道进"缺失"集合。
        missing_dbc = str(temp_root / "does_not_exist.dbc")
        missing_saved = preset_store.save_preset(
            "缺失项目", {1: dbc, 2: missing_dbc}, {},
            {"split_by_message": True, "resample_hz": None, "export_format": "csv"})
        window.apply_preset(preset_store.read_preset(missing_saved))
        assert _pump_scan(app, window), "缺失 DBC 配置的自动解析超时"
        assert 2 in window.missing_dbc_channels, f"缺失集合={window.missing_dbc_channels}"
        assert window.dbc_by_channel.get(1) == dbc, "正常通道的 DBC 被误改"
        print("[config] 5/6 缺失 DBC 场景完成", flush=True)
        missing_count = len(window.missing_dbc_channels)
    except Exception as exc:  # noqa: BLE001 - 自检要把任何异常变成可 grep 的失败行
        print(f"CONFIG_FAIL {type(exc).__name__}: {exc}")
        return 2
    finally:
        if window is not None:
            _destroy_window(window)
    hz = policy["resample_hz"]
    print("[config] 6/6 全部断言通过", flush=True)
    print(f"CONFIG_OK saved={saved.name} channels={channels} signals={signals} "
          f"export={policy['export_format']}/{hz} missing={missing_count}")
    return 0


# ------------------------------------------- --selftest-progress（Step 7c） ----
def _setup_scan_tree(window: MainWindow, blf_path: str, dbc_map: dict) -> None:
    """把一份 BLF + 通道 DBC 灌进真实 UI、扫描并勾选全部信号。"""
    window.clear_files()
    window.dbc_by_channel.clear()
    window._remove_dbc_nodes()
    window.add_blf_paths([blf_path])
    window.dbc_by_channel = {int(ch): str(p) for ch, p in dbc_map.items()}
    window._refresh_dbc_label()
    report = core.scan([blf_path], dict(window.dbc_by_channel))
    window.channel_summary = report.get("channels") or {}
    window._populate_signal_tree()
    window.check_visible_items()  # 保证"选中全部信号"


def _csv_first_cells(report: dict) -> list[tuple[str, str]]:
    """返回每张输出 CSV/表的 ``(路径, 首列首个数据值)``。"""
    cells: list[tuple[str, str]] = []
    for table in report.get("tables") or []:
        path = str(table.get("path") or "")
        if not path.lower().endswith(".csv"):
            continue
        try:
            with open(path, encoding="utf-8") as handle:
                handle.readline()  # 表头
                row = handle.readline()
        except OSError:
            continue
        cells.append((path, row.split(",", 1)[0].strip()))
    return cells


def _run_gui_convert(app: QApplication, window: MainWindow, out_dir: str,
                     time_format: str, recorder: list) -> tuple[dict | None, str | None]:
    """把时间格式写进下拉 → 真跑一次 GUI 转换，返回 ``(report, error)``。"""
    index = next((i for i, (_label, value) in enumerate(TIME_FORMAT_CHOICES)
                  if value == time_format), 0)
    window.combo_time_format.setCurrentIndex(index)
    window.progress_hook = lambda payload: recorder.append(payload)
    window.start_convert(out_dir)
    if not _pump_convert(app, window):
        raise AssertionError(f"转换超时（time_format={time_format}）")
    return window.last_report, window.last_error


def run_selftest_progress() -> int:
    """``--selftest-progress``：进度回调 + 时间格式的无人值守端到端验证。

    * 成功：打印 ``PROGRESS_OK phase1=.. phase2=.. frames_max=.. timefmt_relative=ok
      timefmt_absolute=ok bench=..``，返回 0；
    * 断言失败：打印 ``PROGRESS_FAIL <原因>`` 并返回 2；
    * 内核缺失：打印 ``PROGRESS_SKIP no_kernel`` 并返回 0。

    ``absolute`` 需要 BLF 文件头带有效墙钟时间：``fixtures/multi_channel.blf`` 的头
    没有，因此先断言它触发 ``BAD_TIME_HEADER`` 红字，再用专为该口径准备的
    ``fixtures/timefmt.blf`` 验证绝对时间输出格式。``bench_large.blf`` 的宽表模式会
    命中内核里与本次任务无关的 Polars 列名 panic，故 bench 用默认的拆表模式跑。
    """
    if core is None:
        print("PROGRESS_SKIP no_kernel")
        return 0
    root = Path(__file__).resolve().parent.parent
    blf_path = (root / "fixtures" / "multi_channel.blf").resolve()
    dbc_path = (root / "fixtures" / "demo.dbc").resolve()
    absent = [str(p) for p in (blf_path, dbc_path) if not p.is_file()]
    if absent:
        # 打包后 fixtures/ 不随包发布，这时应优雅跳过而不是报失败（与 no_kernel 同一约定）。
        print(f"PROGRESS_SKIP no_fixture {' '.join(absent)}")
        return 0
    app = QApplication.instance()
    assert app is not None, "QApplication 未构造"
    window: MainWindow | None = None
    frames_max = 0

    def _frames_peak(events: list) -> int:
        return max((int(e.get("frames_done", 0) or 0) for e in events), default=0)

    try:
        window = MainWindow()
        window.quiet_dialogs = True  # 无人值守：任何模态弹窗都会挂死自检
        dbc_map = {1: str(dbc_path), 2: str(dbc_path)}
        _setup_scan_tree(window, str(blf_path), dbc_map)

        # 1) relative_s：phase 计数 + frames_done 单调不减 + 首列首行 = 0。
        relative_dir = tempfile.mkdtemp(prefix="fblf_progress_relative_")
        events: list = []
        report, error = _run_gui_convert(app, window, relative_dir, "relative_s", events)
        if error is not None:
            raise AssertionError(f"relative_s 转换失败：{error}")
        if not report or not report.get("ok"):
            raise AssertionError("relative_s 未返回 ok=True 的报告")
        phase1 = [e for e in events if int(e.get("phase", 0) or 0) == 1]
        phase2 = [e for e in events if int(e.get("phase", 0) or 0) == 2]
        if len(phase1) < 2:
            raise AssertionError(f"phase1 回调={len(phase1)} < 2")
        if len(phase2) < 1:
            raise AssertionError(f"phase2 回调={len(phase2)} < 1")
        frames_seq = [int(e.get("frames_done", 0) or 0) for e in phase1]
        for previous, current in zip(frames_seq, frames_seq[1:]):
            if current < previous:
                raise AssertionError(f"frames_done 非单调：{frames_seq}")
        cells = _csv_first_cells(report)
        if not cells:
            raise AssertionError("relative_s 没有 CSV 输出")
        numeric = [float(value) for _path, value in cells]
        if min(numeric) != 0.0:
            raise AssertionError(f"relative_s 首列最小值={min(numeric)}（应为 0）")
        frames_max = max(frames_max, _frames_peak(events))
        relative_ok = True

        # 2) absolute：multi_channel 头缺墙钟 → BAD_TIME_HEADER 红字（不弹窗）；
        #    再用带有效头的 timefmt.blf 断言绝对时间输出格式。
        absolute_dir = tempfile.mkdtemp(prefix="fblf_progress_absolute_")
        events_abs: list = []
        _report, error = _run_gui_convert(app, window, absolute_dir, "absolute", events_abs)
        if error is not None:
            if "BAD_TIME_HEADER" not in error:
                raise AssertionError(f"absolute 意外错误：{error}")
            if window.lbl_progress_status.property("state") != "error":
                raise AssertionError("BAD_TIME_HEADER 未让进度区进入 error 红字态")
            if "时间" not in window.lbl_progress_status.text():
                raise AssertionError("BAD_TIME_HEADER 未提示改用其它时间格式")
            timefmt_blf = (root / "fixtures" / "timefmt.blf").resolve()
            timefmt_dbc = (root / "fixtures" / "timefmt.dbc").resolve()
            if not (timefmt_blf.is_file() and timefmt_dbc.is_file()):
                raise AssertionError("缺少 fixtures/timefmt.blf 或 timefmt.dbc")
            _setup_scan_tree(window, str(timefmt_blf), {1: str(timefmt_dbc)})
            absolute_dir = tempfile.mkdtemp(prefix="fblf_progress_absolute_tf_")
            report, error = _run_gui_convert(app, window, absolute_dir, "absolute", events_abs)
        if error is not None:
            raise AssertionError(f"absolute 转换失败：{error}")
        if not report or not report.get("ok"):
            raise AssertionError("absolute 未返回 ok=True 的报告")
        cells = _csv_first_cells(report)
        if not cells:
            raise AssertionError("absolute 没有 CSV 输出")
        for path, value in cells:
            if not ABSOLUTE_TIME_RE.match(value):
                raise AssertionError(f"absolute 首列首行不符：{value}（{os.path.basename(path)}）")
        frames_max = max(frames_max, _frames_peak(events_abs))
        absolute_ok = True

        # 3) bench_large：单文件 relative_s，解码期间应持续报进度（phase1 ≥ 5）。
        bench_path = (root / "fixtures" / "bench_large.blf").resolve()
        bench_dbc = (root / "fixtures" / "bench.dbc").resolve()
        if bench_path.is_file() and bench_dbc.is_file():
            _setup_scan_tree(window, str(bench_path), {1: str(bench_dbc), 2: str(bench_dbc),
                                                       3: str(bench_dbc)})
            bench_dir = tempfile.mkdtemp(prefix="fblf_progress_bench_")
            bench_events: list = []
            bench_count = 0
            report, error = _run_gui_convert(app, window, bench_dir, "relative_s", bench_events)
            if error is not None:
                raise AssertionError(f"bench 转换失败：{error}")
            if not report or not report.get("ok"):
                raise AssertionError("bench 未返回 ok=True 的报告")
            bench_count = sum(1 for e in bench_events if int(e.get("phase", 0) or 0) == 1)
            bench_frames_max = _frames_peak(bench_events)
            if bench_count < 5:
                raise AssertionError(f"bench phase1 回调={bench_count} < 5")
            frames_max = max(frames_max, bench_frames_max)
            print(f"BENCH_PROGRESS phase1={bench_count} frames_max={bench_frames_max}")
        else:
            bench_count = 0
            print("SKIP bench")
    except Exception as exc:  # noqa: BLE001 - 自检要把任何异常变成可 grep 的失败行
        print(f"PROGRESS_FAIL {type(exc).__name__}: {exc}")
        return 2
    finally:
        if window is not None:
            _destroy_window(window)
    bench_text = str(bench_count) if bench_count else "SKIP"
    print(f"PROGRESS_OK phase1={len(phase1)} phase2={len(phase2)} frames_max={frames_max} "
          f"timefmt_relative={'ok' if relative_ok else 'fail'} "
          f"timefmt_absolute={'ok' if absolute_ok else 'fail'} bench={bench_text}")
    return 0


def _destroy_window(window: QMainWindow) -> None:
    """显式销毁主窗口。

    PySide6 在解释器退出时可能以错误顺序释放 QObject（macOS/offscreen 下表现为
    ``QObject: shared QObject was deleted directly`` 警告 + 偶发段错误）。先
    ``close`` 再 ``deleteLater`` + ``processEvents`` 让 C++ 侧按父子顺序回收，
    自检与正常退出都能稳定拿到退出码 0。
    """
    try:
        window.close()
        window.deleteLater()
        app = QApplication.instance()
        if app is not None:
            app.processEvents()
    except RuntimeError:  # 底层 C++ 对象已被销毁
        pass


def run_dump_check_states(png_path: str | None = None) -> int:
    """``--dump-check-states [png]``：离屏渲染三态复选框 + 行选中高亮的证据图。

    生成一个独立 ``QTreeWidget``（6 行）：前 3 行分别演示未选 / 部分选 / 全选，
    后 3 行在同样三种状态下**同时被选中**，用来证明新行高亮 ``#33475B`` 下勾选框
    依然清晰可辨。宽度固定 420，高度按行数自适应；成功打印 ``DUMP_OK <路径>``
    并返回 0，失败打印 ``DUMP_FAIL <原因>`` 并返回 2。
    """
    out = Path(png_path).expanduser() if png_path else DUMP_CHECK_STATES_DEFAULT
    out = out.resolve()
    app = QApplication.instance()
    tree = QTreeWidget()
    tree.setColumnCount(1)
    tree.setHeaderHidden(True)
    tree.setRootIsDecorated(False)
    tree.setAlternatingRowColors(False)
    tree.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
    tree.setSelectionMode(QAbstractItemView.SelectionMode.ExtendedSelection)
    tree.setItemDelegate(CheckStateDelegate(tree))
    # offscreen 下光标默认压在首行，会画出 :hover 背景干扰判读；这里把 hover 钉回
    # 普通行底色，只保留「未选 / 部分选 / 全选」与「行选中 #33475B」两种证据。
    tree.setStyleSheet("QTreeWidget::item:hover { background-color: #25252E; }")
    specs = [
        ("☐ 未选（空框 + 灰描边）", Qt.CheckState.Unchecked, False),
        ("▣ 部分选（蓝框 + 8×3 横条）", Qt.CheckState.PartiallyChecked, False),
        ("☑ 全选（实心蓝 + 白勾）", Qt.CheckState.Checked, False),
        ("☐ 未选 · 行选中（高亮 #33475B）", Qt.CheckState.Unchecked, True),
        ("▣ 部分选 · 行选中（仍可辨）", Qt.CheckState.PartiallyChecked, True),
        ("☑ 全选 · 行选中（仍可辨）", Qt.CheckState.Checked, True),
    ]
    for text, state, selected in specs:
        item = QTreeWidgetItem([text])
        item.setFlags(Qt.ItemFlag.ItemIsEnabled | Qt.ItemFlag.ItemIsSelectable
                      | Qt.ItemFlag.ItemIsUserCheckable)
        item.setCheckState(0, state)
        tree.addTopLevelItem(item)
        item.setSelected(selected)
    # 先定位当前项再选行：setCurrentItem 会清掉其它选中，顺序反了就只剩一行高亮。
    tree.setCurrentItem(tree.topLevelItem(len(specs) - 1))
    for row in range(tree.topLevelItemCount()):
        tree.topLevelItem(row).setSelected(specs[row][2])
    tree.setFixedWidth(DUMP_CHECK_WIDTH)
    tree.show()
    tree.setFocus()
    if app is not None:
        app.processEvents()
    content = sum(tree.sizeHintForRow(row) for row in range(tree.topLevelItemCount()))
    tree.resize(DUMP_CHECK_WIDTH, max(tree.frameWidth() * 2 + content, 60))
    if app is not None:
        app.processEvents()
    pixmap = QPixmap(tree.size())
    pixmap.fill(QColor("#1E1E24"))
    tree.render(pixmap)
    try:
        out.parent.mkdir(parents=True, exist_ok=True)
        if not pixmap.save(str(out), "PNG"):
            raise OSError("QPixmap.save 返回 False")
    except OSError as exc:
        print(f"DUMP_FAIL {out}: {exc}")
        tree.close()
        return 2
    tree.close()
    print(f"DUMP_OK {out}")
    print(f"DUMP_BYTES {out.stat().st_size}")
    return 0


def _dump_target(args: list[str]) -> str | None:
    """解析 ``--dump-check-states`` 的可选 PNG 路径（支持空格与 ``=`` 两种写法）。"""
    for pos, arg in enumerate(args):
        if arg == "--dump-check-states":
            nxt = args[pos + 1] if pos + 1 < len(args) else ""
            return nxt if nxt and not nxt.startswith("-") else str(DUMP_CHECK_STATES_DEFAULT)
        if arg.startswith("--dump-check-states="):
            value = arg.split("=", 1)[1]
            return value or str(DUMP_CHECK_STATES_DEFAULT)
    return None


def _main_impl(argv: list[str] | None = None) -> int:
    args = list(sys.argv if argv is None else argv)
    selftest = "--selftest" in args[1:]
    deep = "--selftest-deep" in args[1:]
    config_test = "--selftest-config" in args[1:]
    progress_test = "--selftest-progress" in args[1:]
    dump_target = _dump_target(args[1:])
    app = QApplication.instance() or QApplication(args)
    app.setApplicationName("Fast BLF Converter")
    app.setApplicationDisplayName("Fast BLF Converter")
    app.setStyle("Fusion")
    font = _ui_font()
    font.setStyleStrategy(QFont.StyleStrategy.PreferAntialias); app.setFont(font)
    load_theme(app)
    if config_test:
        return run_selftest_config()
    if progress_test:
        return run_selftest_progress()
    if deep:
        return run_selftest_deep()
    if dump_target is not None:
        return run_dump_check_states(dump_target)
    window = build_main_window()
    print(f"[GUI] 内核就绪：{'是' if KERNEL_READY else '否'}；版本："
          f"{core.version() if KERNEL_READY else '—'}", file=sys.stderr)
    if not KERNEL_READY:
        print(f"[GUI] {KERNEL_MISSING_TEXT}")
    if selftest:
        QTimer.singleShot(400, app.quit)
    else:
        window.lbl_status.setText("就绪：请添加 BLF 文件并在通道上右键添加 DBC，然后点「🔍 解析信号」"
                                  if KERNEL_READY else KERNEL_MISSING_TEXT)
    rc = int(app.exec())
    _destroy_window(window)
    return rc


def main(argv: list[str] | None = None) -> int:
    """进程入口。

    退出时**强制终止**（``os._exit``）而不是走解释器正常关闭流程 —— 历史上出现过：
    打包态下 ``--selftest-config`` 在 CI 上迟迟不退出（残留 Qt/工作线程让 shutdown 阻塞），
    结果整轮 Windows 构建被挂住、白烧 CI 额度。GUI 程序在关闭窗口后也不需要 Python 的
    额外清理，硬退出是这里正确的语义。
    """
    code = _main_impl(argv)
    try:
        sys.stdout.flush()
        sys.stderr.flush()
    except Exception:  # noqa: BLE001 - 关闭阶段不允许再抛
        pass
    os._exit(int(code))


if __name__ == "__main__":
    raise SystemExit(main())
