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

import bisect
import csv
import json
import logging
import os
import re
import shutil
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

from PySide6.QtCore import (
    QModelIndex, QObject, QSortFilterProxyModel, QThread, QTimer, Qt, QUrl, Signal,
)
from PySide6.QtGui import (
    QAction, QColor, QDesktopServices, QFont, QFontDatabase, QPixmap, QStandardItem,
    QStandardItemModel,
)
from PySide6.QtWidgets import (
    QAbstractItemView, QApplication, QCheckBox, QComboBox, QDialog, QDoubleSpinBox,
    QFileDialog, QFormLayout, QFrame, QHBoxLayout, QHeaderView, QInputDialog, QLabel,
    QLineEdit, QMainWindow, QMenu, QMessageBox, QProgressBar, QPushButton, QSizePolicy,
    QSpinBox, QSplitter, QStatusBar, QToolBar, QTreeView, QTreeWidget, QTreeWidgetItem,
    QVBoxLayout, QWidget,
)

try:  # 以脚本方式运行（python gui/main.py）时 gui/ 已在 sys.path 首位
    from check_delegate import CheckStateDelegate
    import presets as preset_store
    from plot_view import PreviewWindow, TriStateCheckBox
    import report_html as report_builder
    from log_setup import LOGGER_NAME, log_dir, log_path, setup_logging, shutdown_logging
except ImportError:  # 作为包导入（gui.main）时
    from gui.check_delegate import CheckStateDelegate
    from gui import presets as preset_store
    from gui.plot_view import PreviewWindow, TriStateCheckBox
    from gui import report_html as report_builder
    from gui.log_setup import LOGGER_NAME, log_dir, log_path, setup_logging, shutdown_logging

#: Step 22：全应用唯一 logger。窗口化 exe 下 stderr 不可用，一切诊断信息走日志文件。
logger = logging.getLogger(LOGGER_NAME)

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
#: Step 13：手动添加通道的统一文案（工具栏按钮 + 信号树空白处右键菜单共用）。
ADD_CHANNEL_TEXT = "➕ 添加通道…"
ADD_CHANNEL_STATUS_TIP = ("手动添加一个通道节点（通道号 1~64，默认预填当前最大通道号 +1）；"
                          "添加后可在该节点上右键「📄 添加 DBC…」")
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
#: Step 14：填充方式（写入 config 的 ``fill_policy``，取值与 INTERFACE §1.1 一一对应）。
FILL_CHOICES: list[tuple[str, str]] = [
    ("阶梯保持（推荐）", "hold"),
    ("线性插值", "linear"),
    ("不填充", "none"),
]
FILL_INDEX: dict[str, int] = {value: idx for idx, (_label, value) in enumerate(FILL_CHOICES)}
FILL_TOOLTIP = (
    "阶梯保持：把最近一次真实采样保持到下一拍，绝不造新值（状态 / 枚举 / 计数类信号必须用它）；"
    "线性插值：会在两个真实采样点之间造出从未采样过的中间值；"
    "不填充：原样留空")
FILL_SPLIT_TOOLTIP = "拆表模式下同一报文周期一致，不产生空值，无需填充"
FILL_GAP_TOOLTIP = (
    "向前保持超过该时长仍未收到新采样时，该点留空——避免通信中断/丢帧被旧值掩盖")
#: 进度区（Step 7c）文案。
PROGRESS_IDLE_TEXT = "待命"
PROGRESS_IDLE_META = "—"
PHASE_STAGE_LABELS = {"scan": "扫描中", "decode": "解码中", "merge": "合并中", "export": "导出中"}
#: Step 19 A1：单条进度条的加权份额。阶段 1 占 0..70%，阶段 2 占 70..100%。
#: 两个常量集中在这里，以后要调整阶段权重只改这一处（selftest 也按它推导）。
_PHASE1_SHARE = 0.70
_PHASE2_SHARE = 1.0 - _PHASE1_SHARE
#: 非终态进度条永远不超过该值，100 只由「成功终态」写入（A2）。
_PROGRESS_RUNNING_MAX = 99
#: 不确定态（busy）乒乓动画的上限：绝不能提前扫到 100（否则会被误读成"已完成"）。
_BUSY_SWEEP_MAX = 95
_PHASE_HEADINGS = {1: "阶段 1/2 · 解码", 2: "阶段 2/2 · 合并导出"}
#: 绝对时间输出的正则（用于 ``--selftest-progress`` 校验）。
ABSOLUTE_TIME_RE = re.compile(r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{6}$")
NODE_ROLE = Qt.ItemDataRole.UserRole + 1
TEXT_ROLE = Qt.ItemDataRole.UserRole + 2
#: Step 18：预览设置写在预设 JSON 的一个附加字段里（不改变 presets.py 的 schema）。
PREVIEW_PRESET_KEY = "preview"
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


class SignalTreeView(QTreeView):
    """信号树视图：视图挂的是过滤代理模型，但全应用按**源模型索引**定位节点。

    Qt 6 起 ``QTreeView.isExpanded(index)`` 只认视图自己的模型（这里是
    :class:`SignalFilterProxy`），传入 ``signal_model`` 的索引一律返回 False；
    ``expand()`` / ``scrollTo()`` 同理。而“通道 / DBC 节点是否展开在 DBC 名称
    层级”这件事在代码与自检里都是拿源索引说话的，所以这层把外部传入的源索引
    先映射到代理侧再交给 Qt，避免调用方每处都手写 ``mapFromSource``。
    """

    def _view_index(self, index: QModelIndex) -> QModelIndex:
        """把源模型索引映射到代理侧；不可见（被搜索框过滤）时返回无效索引。"""
        proxy = self.model()
        if not index.isValid():
            return index
        if index.model() is proxy or not isinstance(proxy, QSortFilterProxyModel):
            return index
        return proxy.mapFromSource(index)

    def isExpanded(self, index: QModelIndex) -> bool:  # noqa: N802 - 对齐 Qt 命名
        return super().isExpanded(self._view_index(index))

    def expand(self, index: QModelIndex) -> None:  # noqa: N802 - 对齐 Qt 命名
        super().expand(self._view_index(index))

    def scrollTo(self, index: QModelIndex,  # noqa: N802 - 对齐 Qt 命名
                 hint: QAbstractItemView.ScrollHint =
                 QAbstractItemView.ScrollHint.EnsureVisible) -> None:
        super().scrollTo(self._view_index(index), hint)


# --------------------------- 工作线程：内核调用全部在此，UI 只通过信号接收 ---
class Worker(QThread):
    """``mode="scan"`` 调 ``core.scan``；``mode="convert"`` 调 ``core.convert``。"""

    progress = Signal(dict)
    finished_ok = Signal(dict)
    failed = Signal(str)

    def __init__(self, mode: str, config: dict | None = None, blf_paths: list[str] | None = None,
                 dbc_map: dict[int, str] | None = None,
                 initial_keys: list[str] | None = None,
                 parent: QObject | None = None) -> None:
        super().__init__(parent)
        self.mode = mode
        self.config = config or {}
        self.blf_paths = list(blf_paths or [])
        self.dbc_map = dict(dbc_map or {})
        #: Step 18：打开 PlotSession 时首屏激活的信号（最多 3 条，按勾选顺序）。
        self.initial_keys = list(initial_keys or [])
        #: 内核异常的稳定错误码（如 CACHE_BUDGET_EXCEEDED），供预览窗口走内联横幅。
        self.error_code: str | None = None
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
            if self.mode == "scan":
                report = core.scan(self.blf_paths, self.dbc_map, self._progress_bridge)
            elif self.mode == "series":
                # Step 16：画图数据只走 series_for_plot（内核侧抽稀），GUI 不解码 BLF。
                report = core.series_for_plot(self.config)
            elif self.mode == "session":
                report = self._open_plot_session()
            else:
                report = core.convert(self.config, self._progress_bridge)
        except BaseException as exc:  # noqa: BLE001 - pyo3 PanicException 是 BaseException 子类
            # Step 22：``pyo3_runtime.PanicException`` 不走 ``Exception``，必须抓 BaseException，
            # 否则 QThread 里 panic 会静默吞掉（窗口化下连 stderr 都没有）。先落盘再按原路径上报。
            text = str(exc)
            self.error_code = next((code for code in (
                "CACHE_BUDGET_EXCEEDED", "BAD_CONFIG", "EMPTY_RESULT", "DISK_FULL",
                "BLF_OPEN", "BLF_PARSE", "DBC_PARSE") if code in text), None)
            logger.exception("worker failed mode=%s code=%s", self.mode, self.error_code)
            self.failed.emit(f"{type(exc).__name__}: {text}")
            return
        if isinstance(report, dict):
            self.finished_ok.emit(report)
        else:
            self.failed.emit(f"内核返回了非预期类型：{type(report).__name__}")

    def _open_plot_session(self) -> dict:
        """Step 18：在 Worker 线程里打开 PlotSession，带确定进度 + 首屏预激活。

        返回给主线程的 dict 里直接携带 PlotSession 对象；**所有** ``view()`` 都由
        预览窗口在窗口变化时调用，Worker 这里只做 open / add_signals / stats。
        """
        session = core.open_plot_session(self.config, self._progress_bridge)
        try:
            time_format = str(self.config.get("time_format") or "relative_s")
            info = session.info()
            # 在 Worker 里一次性探明 absolute 墙钟字符串与 unix 原始偏移；之后切口径
            # 只改枚举，不再调 info() / view()（E2 ≤16ms）。
            abs_info = info if time_format == "absolute" else None
            if abs_info is None:
                session.set_time_format("absolute")
                abs_info = session.info()
            if time_format == "unix_s":
                raw_info = info
            else:
                session.set_time_format("unix_s")
                raw_info = session.info()
            session.set_time_format(time_format)
            raw_min = float(raw_info.get("t_min") or 0.0)
            raw_max = float(raw_info.get("t_max") or (raw_min + 1.0))
            keys = [str(key) for key in self.initial_keys if key]
            if not keys:
                keys = [str(item.get("key")) for item in (info.get("signals") or [])
                        if item.get("cached")][:3]
            if keys:
                session.add_signals(keys)
            stats = session.stats(keys) if keys else []
            info = session.info()
            return {
                "session": session,
                "info": info,
                "stats": stats,
                "initial_keys": keys,
                "config": dict(self.config),
                "format_meta": {
                    "raw_min": raw_min,
                    "raw_max": raw_max,
                    "abs_min_str": abs_info.get("t_min_str"),
                    "abs_max_str": abs_info.get("t_max_str"),
                },
            }
        except Exception:
            # 任何失败都必须释放句柄，避免缓存文件句柄泄漏。
            try:
                session.close()
            except Exception:  # noqa: BLE001
                pass
            raise


# ------------------------------------------------------- 缓存设置对话框（C2）
class CacheSettingsDialog(QDialog):
    """帧缓存设置：上限 32..8192 MiB + 自定义缓存目录（空 = 系统临时目录）。

    校验失败**不弹错误框**：在对话框内就地显示红字，保持输入焦点不动。
    """

    def __init__(self, budget: int, cache_dir: str | None,
                 parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setWindowTitle("帧缓存设置")
        self.setModal(True)
        self.setMinimumWidth(520)
        self._values: tuple[int, str | None] = (
            int(budget), str(cache_dir) if cache_dir else None)
        outer = QVBoxLayout(self)
        outer.setContentsMargins(16, 14, 16, 12)
        outer.setSpacing(10)
        form = QFormLayout()
        form.setHorizontalSpacing(12)
        form.setVerticalSpacing(10)
        self.spin_cache = QSpinBox(self)
        tune(self.spin_cache, range=(preset_store.CACHE_BUDGET_MIN_MB,
                                     preset_store.CACHE_BUDGET_MAX_MB),
             value=int(budget), suffix=" MiB")
        self.spin_cache.setToolTip(
            f"帧缓存磁盘上限，允许 {preset_store.CACHE_BUDGET_MIN_MB}.."
            f"{preset_store.CACHE_BUDGET_MAX_MB} MiB（默认 "
            f"{preset_store.DEFAULT_CACHE_BUDGET_MB} MiB）")
        form.addRow("帧缓存上限 (MiB)：", self.spin_cache)
        dir_row = QHBoxLayout()
        dir_row.setSpacing(8)
        self.edit_cache_dir = QLineEdit(self)
        self.edit_cache_dir.setPlaceholderText("留空 = 系统临时目录")
        self.edit_cache_dir.setText(str(cache_dir) if cache_dir else "")
        self.edit_cache_dir.setToolTip("帧缓存落盘目录；留空时使用系统临时目录")
        dir_row.addWidget(self.edit_cache_dir, 1)
        self.btn_pick_dir = QPushButton("选择…", self)
        self.btn_pick_dir.clicked.connect(self._pick_dir)
        dir_row.addWidget(self.btn_pick_dir, 0)
        form.addRow("缓存目录：", dir_row)
        outer.addLayout(form)
        hint = QLabel("设置保存后立即生效：下一次打开预览即使用新的上限 / 目录。")
        tune(hint, objectName="hintLabel", wordWrap=True)
        outer.addWidget(hint)
        self.lbl_error = QLabel("")
        self.lbl_error.setWordWrap(True)
        self.lbl_error.setStyleSheet("color: #F05A5A;")
        self.lbl_error.setVisible(False)
        outer.addWidget(self.lbl_error)
        buttons = QHBoxLayout()
        buttons.addStretch(1)
        self.btn_cancel = QPushButton("取消", self)
        self.btn_cancel.clicked.connect(self.reject)
        buttons.addWidget(self.btn_cancel)
        self.btn_save = QPushButton("保存", self)
        self.btn_save.setObjectName("primaryButton")
        self.btn_save.clicked.connect(self.accept)
        buttons.addWidget(self.btn_save)
        outer.addLayout(buttons)

    def _pick_dir(self) -> None:
        chosen = QFileDialog.getExistingDirectory(
            self, "选择帧缓存目录", self.edit_cache_dir.text().strip() or str(Path.home()))
        if chosen:
            self.edit_cache_dir.setText(chosen)

    def accept(self) -> None:  # noqa: D401 - QDialog 命名
        """保存前校验范围；非法值就地红字，不关闭对话框。"""
        budget = int(self.spin_cache.value())
        if not (preset_store.CACHE_BUDGET_MIN_MB <= budget
                <= preset_store.CACHE_BUDGET_MAX_MB):
            self.lbl_error.setText(
                f"帧缓存上限必须在 {preset_store.CACHE_BUDGET_MIN_MB}.."
                f"{preset_store.CACHE_BUDGET_MAX_MB} MiB 之间，当前为 {budget} MiB。")
            self.lbl_error.setVisible(True)
            self.spin_cache.setFocus()
            return
        self._values = (budget, self.edit_cache_dir.text().strip() or None)
        self.lbl_error.clear()
        self.lbl_error.setVisible(False)
        super().accept()

    def values(self) -> tuple[int, str | None]:
        """返回 ``(cache_budget_mb, cache_dir|None)``（仅在 accept 后有效）。"""
        return self._values


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
        #: Step 16：画图 / HTML 报告的取数线程 + 预览窗口 + 最近一次上下文。
        self.plot_worker = None
        self.preview_window: PreviewWindow | None = None
        self._plot_selection: dict | None = None
        self._plot_action: str | None = None
        self._plot_out_path: str | None = None
        self._plot_time_format: str | None = None
        #: Step 18/19：预览会话的缓存预算 / 缓存目录（来自 settings.json）/ 预览视图设置。
        self._cache_settings = preset_store.read_settings()
        self.preview_cache_budget_mb: int = int(
            self._cache_settings.get("cache_budget_mb",
                                     preset_store.DEFAULT_CACHE_BUDGET_MB))
        self.preview_cache_dir: str | None = self._cache_settings.get("cache_dir")
        self._preview_view_settings: dict = {}
        self._preview_settings_from_preset: bool = False
        self._last_preview_config: dict | None = None
        self._checks_guard = False
        #: 解析前的勾选快照（Step 6a §4：按名字恢复勾选）。
        self._check_snapshot: dict[str, set] | None = None
        #: Step 6c：当前配置名 / 缺失 DBC 的通道 / 加载后待恢复的勾选。
        self.current_preset_name: str | None = None
        self.missing_dbc_channels: set[int] = set()
        #: Step 13：通道节点粘性 —— 已出现过的通道号集合（预置 / 手动添加 / 扫描观测），
        #: 树重建时并集参与，只增不减；`_seed_channels()` 之后预置 Ch1~Ch8。
        self.channel_slots: set[int] = set()
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
        #: Step 14：填充方式是否被用户/预设显式设置过（未设置时跟随路径默认）。
        self._fill_touched = False
        self._fill_guard = False       # 程序化改下拉时抑制 currentIndexChanged 递归
        self._progress_timer = QTimer(self)
        self._progress_timer.setInterval(200)
        self._progress_timer.timeout.connect(self._on_progress_tick)
        self._build_toolbar()
        self._build_central()
        self._build_statusbar()
        self._connect_signals()
        self._seed_channels()
        # Step 13 B1：预置 Ch1~Ch8 常驻通道槽位（`_seed_channels` 已逐个登记一次，
        # 这里显式补齐，保证"先绑 DBC 再统一解析"时通道节点永不因重建而消失）。
        self.channel_slots = set(range(1, len(CHANNEL_CHOICES) + 1))
        self._sync_resample_hint()
        self._sync_select_all_checkbox()
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
        # Step 16：画图入口（工具栏 + 信号树右键；数据统一走 Rust 侧 series_for_plot）。
        self.act_preview = QAction("📈 预览", self)
        tune(self.act_preview, statusTip="用抽稀后的 min/max 包络在独立窗口预览已勾选信号（不丢峰）")
        bar.addAction(self.act_preview)
        self.act_export_html = QAction("📤 导出 HTML 报告", self)
        tune(self.act_export_html,
             statusTip="把已勾选信号导出为离线自包含 HTML 报告（图表库内嵌，断网也能打开）")
        bar.addAction(self.act_export_html)
        # Step 13 B4：手动添加通道（工具入口之一；另一处在信号树空白处右键）。
        self.act_add_channel = QAction(ADD_CHANNEL_TEXT, self)
        tune(self.act_add_channel, statusTip=ADD_CHANNEL_STATUS_TIP)
        bar.addAction(self.act_add_channel)
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
        # Step 19 C2：缓存设置入口（工具栏；不新增菜单层级，保持入口一眼可见）。
        self.act_settings = QAction("⚙ 设置", self)
        tune(self.act_settings, statusTip="帧缓存上限与缓存目录（默认 200 MiB / 系统临时目录）")
        self._refresh_settings_tooltip()
        bar.addAction(self.act_settings)
        # Step 22：窗口化 exe 不弹黑框，出问题时靠日志文件排查 —— 给它一个入口。
        self.act_open_logs = QAction("📂 打开日志目录", self)
        tune(self.act_open_logs, statusTip="在文件管理器中打开日志目录（gui.log / crash.log）")
        bar.addAction(self.act_open_logs)
        bar.addSeparator()
        self.dbc_label = QLabel("DBC 绑定：无")
        tune(self.dbc_label, objectName="hintLabel")
        self.dbc_label.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Preferred)
        bar.addWidget(self.dbc_label)

    def _refresh_settings_tooltip(self) -> None:
        """把当前缓存设置写进「⚙ 设置」的 tooltip（C4：改完一眼能核对）。"""
        cache_dir = self.preview_cache_dir or "系统临时目录（默认）"
        self.act_settings.setToolTip(
            f"帧缓存上限：{self.preview_cache_budget_mb} MiB（允许 "
            f"{preset_store.CACHE_BUDGET_MIN_MB}..{preset_store.CACHE_BUDGET_MAX_MB}）\n"
            f"缓存目录：{cache_dir}\n保存位置：{preset_store.settings_path()}")

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
        # Step 19 D1/D3：标题行只保留一个三态复选框（删除原先的「全选当前过滤项 /
        # 一键清空」两个按钮）。语义与预览面板完全一致：部分/未选 → 全选；全选 → 全不选。
        title_row = QHBoxLayout()
        title_row.setSpacing(8)
        title_row.addWidget(title, 1)
        self.chk_select_all = TriStateCheckBox("全选（0/0）", panel)
        self.chk_select_all.setObjectName("treeSelectAll")
        self.chk_select_all.setToolTip(
            "点「部分选 / 未选」→ 全选当前可见（未被搜索过滤）的行；点「全选」→ 全不选")
        title_row.addWidget(self.chk_select_all, 0)
        layout.addLayout(title_row)
        self.search_edit = QLineEdit(panel)
        tune(self.search_edit, placeholderText="🔍 搜索 DBC / 报文名 / 信号名（大小写不敏感的模糊匹配）", clearButtonEnabled=True)
        row = QHBoxLayout()
        row.addWidget(self.search_edit, 1)
        layout.addLayout(row)
        self.signal_model = QStandardItemModel(0, 1, self)
        self.signal_model.setHeaderData(0, Qt.Orientation.Horizontal, "通道 / DBC / 报文 / 信号")
        self.signal_proxy = SignalFilterProxy(self)
        self.signal_proxy.setSourceModel(self.signal_model)
        self.signal_view = SignalTreeView(panel)
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
        # Step 14：填充方式（默认跟随路径；拆表模式禁用，网格模式禁用「不填充」）。
        row.addWidget(QLabel("填充方式："))
        self.combo_fill = QComboBox(panel)
        self.combo_fill.addItems([label for label, _policy in FILL_CHOICES])
        tune(self.combo_fill, currentIndex=FILL_INDEX["hold"], toolTip=FILL_TOOLTIP)
        row.addWidget(self.combo_fill)
        row.addStretch(1)
        self.btn_convert = QPushButton("🚀 启动多核极速转换")
        self.btn_convert.setObjectName("primaryButton")
        row.addWidget(self.btn_convert)
        outer.addLayout(row)
        # Step 14：最大保持时长（0 = 不限；仅「阶梯保持」启用）。
        gap_row = QHBoxLayout()
        gap_row.setSpacing(14)
        gap_row.addWidget(QLabel("最大保持时长："))
        # Step 19 B1/B3：用 QDoubleSpinBox 保存 ``ffill_max_gap_ms`` 的 float 小数位，
        # 避免预设 round-trip 把 12.5 ms 截成 13 ms。
        self.spin_fill_gap = QDoubleSpinBox(panel)
        tune(self.spin_fill_gap, range=(0.0, 60_000.0), value=0.0, decimals=1,
             singleStep=10.0, suffix=" ms", toolTip=FILL_GAP_TOOLTIP)
        gap_row.addWidget(self.spin_fill_gap)
        gap_row.addStretch(1)
        outer.addLayout(gap_row)
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
        # Step 16：预览 / 导出 HTML 报告（均复用 Worker 线程取数）。
        self.act_preview.triggered.connect(self.start_plot_preview)
        self.act_export_html.triggered.connect(self.start_export_html)
        self.act_add_channel.triggered.connect(self.add_channel_dialog)
        self.act_save_config.triggered.connect(self.save_config_dialog)
        self.act_load_config.triggered.connect(self.load_config_dialog)
        self.act_delete_config.triggered.connect(self.delete_config_dialog)
        self.act_settings.triggered.connect(self.open_settings_dialog)
        self.act_open_logs.triggered.connect(self.open_log_dir)
        self.file_tree.files_dropped.connect(self.add_blf_paths)
        self.btn_remove_selected.clicked.connect(self.remove_selected_files)
        self.btn_clear_files.clicked.connect(self.clear_files)
        self.search_edit.textChanged.connect(self._on_search_changed)
        self.chk_select_all.clicked.connect(self._on_select_all_clicked)
        self.signal_model.itemChanged.connect(self._on_item_changed)
        self.signal_view.customContextMenuRequested.connect(self._on_signal_context_menu)
        self.chk_split.toggled.connect(lambda _c: self._sync_resample_hint())
        # Step 14：重采样档位 / 填充方式变化时刷新可用项与默认值。
        self.combo_resample.currentIndexChanged.connect(lambda _i: self._sync_fill_hint())
        self.combo_fill.currentIndexChanged.connect(self._on_fill_changed)
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
        # Step 13 B3：新建节点即登记槽位，保证手动添加的通道在下次解析重建后依然存在。
        self.channel_slots.add(int(channel))
        return item

    def add_channel_dialog(self) -> None:
        """「➕ 添加通道…」：取通道号后交给 :meth:`_add_channel`（弹窗只负责取号）。"""
        default = (max(self.channel_slots) + 1) if self.channel_slots else 1
        channel, accepted = QInputDialog.getInt(
            self, "添加通道", "通道号（1~64）：", value=default, min=1, max=64)
        if not accepted:
            return
        self._add_channel(int(channel))

    def _add_channel(self, channel: int) -> bool:
        """把通道号加入 ``channel_slots`` 并确保树里有节点；返回是否为新建。

        已存在时不重复建节点、不弹错误框，只在状态栏提示（Step 13 B4）。
        """
        channel = int(channel)
        if channel in self.channel_slots:
            self.statusBar().showMessage(f"通道 Ch{channel} 已存在")
            return False
        self.channel_slots.add(channel)
        self._channel_item(channel)
        self.signal_view.expandToDepth(0)
        self.statusBar().showMessage(f"已添加通道 Ch{channel}，可在其上右键添加 DBC")
        return True

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
        """通道节点上弹「📄 添加 DBC…」；空白处弹「➕ 添加通道…」（Step 13 B4）。"""
        index = self.signal_view.indexAt(pos)
        if not index.isValid():
            # 点在信号树空白处：只提供"添加通道"入口（与工具栏同一个动作文案）。
            menu = QMenu(self.signal_view)
            action = menu.addAction(ADD_CHANNEL_TEXT)
            chosen = menu.exec(self.signal_view.viewport().mapToGlobal(pos))
            if chosen is action:
                self.add_channel_dialog()
            return
        item = self.signal_model.itemFromIndex(self.signal_proxy.mapToSource(index))
        if item is None:
            return
        kind = item.data(NODE_ROLE)
        # Step 16：在信号 / 报文节点上右键 → 预览 / 导出 HTML 报告（对当前勾选生效）。
        if kind in (NODE_SIGNAL, NODE_MESSAGE):
            menu = QMenu(self.signal_view)
            preview_action = menu.addAction(self.act_preview.text())
            export_action = menu.addAction(self.act_export_html.text())
            chosen = menu.exec(self.signal_view.viewport().mapToGlobal(pos))
            if chosen is preview_action:
                self.start_plot_preview()
            elif chosen is export_action:
                self.start_export_html()
            return
        if kind != NODE_CHANNEL:
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
        self._reveal_dbc_node(channel)
        self._refresh_dbc_label()
        self._refresh_action_state()
        self.statusBar().showMessage(
            f"已为 Ch{channel} 添加 DBC：{os.path.basename(resolved)}"
            "（点「🔍 解析信号」后生效）")

    def _reveal_dbc_node(self, channel: int) -> None:
        """展开到 DBC 名称层级：展开通道节点并把它滚到视野中央。

        DBC 节点是通道节点的**子节点**，所以"展开到 DBC 名称那一层"就是展开通道
        节点本身；绝不再往下展开报文 / 信号（报文可能成百上千，展开后完全看不清）。
        视图挂的是过滤代理模型，``SignalTreeView`` 会把源索引自动映射到代理侧。
        """
        channel_item = self._channel_item(channel)
        index = self.signal_model.indexFromItem(channel_item)
        if not index.isValid():
            return
        if not self.signal_proxy.mapFromSource(index).isValid():
            # 搜索框过滤掉了该通道（当前不可见）：展开状态留给用户清空关键字后再看。
            return
        self.signal_view.expand(index)
        self.signal_view.scrollTo(index, QAbstractItemView.ScrollHint.PositionAtCenter)

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
        """当前导出策略（6 个字段），与配置里的 ``export`` 完全同构（B3）。

        ``schema == 2`` 起保存：拆表 / 重采样 / 导出格式 / 时间列格式 /
        填充方式 / 最大保持时长。加载时由 :meth:`apply_export_policy` 逐项写回控件。
        """
        return {
            "split_by_message": bool(self.chk_split.isChecked()),
            "resample_hz": RESAMPLE_CHOICES[max(0, self.combo_resample.currentIndex())][1],
            "export_format": FORMAT_CHOICES[max(0, self.combo_format.currentIndex())][1],
            "time_format": self._current_time_format(),
            "fill_policy": self._fill_policy(),
            "ffill_max_gap_ms": self._fill_gap_ms(),
        }

    def _fill_gap_ms(self) -> float | None:
        """最大保持时长（毫秒）；0 = 不限 → ``None``。"""
        value = float(self.spin_fill_gap.value())
        return None if value <= 0 else value

    def apply_export_policy(self, export: dict | None) -> None:
        """把配置里的导出策略写回控件（6 个字段逐个同步；缺字段用默认值兜底）。"""
        export = export if isinstance(export, dict) else {}
        self.chk_split.setChecked(bool(export.get("split_by_message", True)))
        hz = export.get("resample_hz")
        self.combo_resample.setCurrentIndex(next(
            (i for i, (_label, value) in enumerate(RESAMPLE_CHOICES) if value == hz), 0))
        fmt = export.get("export_format")
        self.combo_format.setCurrentIndex(next(
            (i for i, (_label, value) in enumerate(FORMAT_CHOICES) if value == fmt), 0))
        time_format = export.get("time_format")
        self.combo_time_format.setCurrentIndex(next(
            (i for i, (_label, value) in enumerate(TIME_FORMAT_CHOICES)
             if value == time_format), 0))
        self._sync_resample_hint()
        # 若调用方直接给了填充字段就用它（旧配置不含这两项 → 回落路径默认）。
        policy = export.get("fill_policy")
        if policy in FILL_INDEX:
            self._fill_touched = True
            self._set_fill_policy(str(policy))
        else:
            self._fill_touched = False
        gap = export.get("ffill_max_gap_ms")
        self.spin_fill_gap.setValue(float(gap) if isinstance(gap, (int, float))
                                    and not isinstance(gap, bool) and gap > 0 else 0.0)
        self._sync_fill_hint()

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
        # Step 22：批量降入 DBC（加载预设 / 应用映射）后同样展开到 DBC 名称层级。
        for channel in sorted(self.dbc_by_channel):
            self._reveal_dbc_node(channel)
        self._refresh_dbc_label()
        self._refresh_action_state()
        return self.missing_dbc_channels

    def _write_preview_settings(self, path: Path) -> None:
        """把预览设置附加到预设 JSON（Step 18 的"写进预设"；不改 schema / 其它字段）。"""
        if not self._preview_view_settings:
            return
        try:
            data = json.loads(Path(path).read_text(encoding="utf-8"))
            if not isinstance(data, dict):
                return
            data[PREVIEW_PRESET_KEY] = self._preview_view_settings
            tmp = Path(path).with_name(Path(path).name + ".tmp")
            tmp.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n",
                           encoding="utf-8")
            os.replace(tmp, Path(path))
        except (OSError, ValueError):
            try:
                tmp.unlink()
            except (OSError, UnboundLocalError):
                pass

    def _read_preview_settings(self, path: Path | str) -> dict:
        """从预设 JSON 读取预览设置；旧预设没有该字段时返回空 dict。"""
        try:
            data = json.loads(Path(path).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {}
        value = data.get(PREVIEW_PRESET_KEY) if isinstance(data, dict) else None
        return value if isinstance(value, dict) else {}

    # --------------------------------------------------------- 缓存设置 ----
    def open_settings_dialog(self) -> None:
        """「⚙ 设置」：编辑帧缓存上限 / 缓存目录（C2/C4）。"""
        dialog = CacheSettingsDialog(self.preview_cache_budget_mb,
                                     self.preview_cache_dir, self)
        if dialog.exec() != QDialog.DialogCode.Accepted:
            return
        budget, cache_dir = dialog.values()
        if not self._apply_cache_settings(budget, cache_dir):
            return
        self.lbl_status.setText(f"缓存上限已设为 {self.preview_cache_budget_mb} MiB")

    def open_log_dir(self) -> None:
        """「📂 打开日志目录」：用系统文件管理器定位到日志目录（gui.log / crash.log）。"""
        target = log_dir()
        try:
            target.mkdir(parents=True, exist_ok=True)
        except OSError as exc:
            logger.warning("[GUI] 无法创建日志目录 %s：%s", target, exc)
            self.statusBar().showMessage(f"日志目录不可用：{target}")
            return
        opened = QDesktopServices.openUrl(QUrl.fromLocalFile(str(target)))
        if not opened:
            logger.warning("[GUI] 打开日志目录失败：%s", target)
        self.lbl_status.setText(f"日志目录：{target}")
        self.statusBar().showMessage(f"日志目录：{target}")

    def _apply_cache_settings(self, budget: int, cache_dir: str | None,
                              *, persist: bool = True) -> bool:
        """更新内存中的缓存设置（可选持久化），并刷新状态栏 / tooltip。

        校验失败返回 ``False``；调用方负责红字提示（本方法绝不弹窗）。
        """
        budget = int(budget)
        if not (preset_store.CACHE_BUDGET_MIN_MB <= budget
                <= preset_store.CACHE_BUDGET_MAX_MB):
            return False
        cache_dir = str(Path(cache_dir).expanduser()) if cache_dir else None
        self.preview_cache_budget_mb = budget
        self.preview_cache_dir = cache_dir
        self._cache_settings = {"cache_budget_mb": budget, "cache_dir": cache_dir}
        self._refresh_settings_tooltip()
        if persist:
            try:
                preset_store.save_settings(self._cache_settings)
            except preset_store.PresetError as exc:
                self.statusBar().showMessage(
                    f"缓存上限已设为 {budget} MiB（但设置写入失败：{exc}）")
                return True
        self.statusBar().showMessage(f"缓存上限已设为 {budget} MiB")
        return True

    def save_preset(self, name: str) -> Path:
        """把当前 DBC 映射 + 选中信号 + 导出策略保存为命名配置，返回落盘路径。"""
        path = preset_store.save_preset(
            name, dict(self.dbc_by_channel), self.build_selection(), self.current_export_policy())
        self.current_preset_name = path.stem
        self._write_preview_settings(path)
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
        preview_settings = self._read_preview_settings(preset.path)
        if preview_settings:
            self._preview_view_settings = preview_settings
            self._preview_settings_from_preset = True
        else:
            self._preview_settings_from_preset = False
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
        self._sync_select_all_checkbox()
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
            # Step 13 A2：有 BLF 时 scan 的进度回调带真实文件级 done/total，
            # 必须走确定进度条，禁止再用 0→100→0 的不确定态乒乓动画。
            self._begin_determinate("解析中…")
        else:
            # 无 BLF（只按 DBC 列信号）时 total 恒为 0，才允许不确定态。
            self._begin_busy("阶段 1/2 · 扫描中…")
        self.lbl_status.setText("正在解析信号…")
        self.statusBar().showMessage("正在解析信号…")
        self.scan_worker = Worker("scan", blf_paths=[e.path for e in self.entries],
                                  dbc_map=merged_dbc, parent=self)
        self._spawn(self.scan_worker, self._on_scan_progress, self._on_scan_ok,
                    self._on_scan_failed, self._on_scan_thread_done)
        self._refresh_action_state()

    def _on_scan_progress(self, payload: dict) -> None:
        """解析进度来自工作线程：按文件级 ``done/total`` 驱动确定进度条（Step 13 A3）。"""
        message = str(payload.get("message") or "正在解析信号…")
        done, total = int(payload.get("done", 0) or 0), int(payload.get("total", 0) or 0)
        frames = int(payload.get("frames_done", 0) or 0)
        file_path = payload.get("file")
        pv = self._pv
        if total > 0:
            # 确定态：busy 必须关掉，否则 200ms 心跳又会把进度条来回扫动（A5）。
            pv["active"] = True
            pv["busy"] = False
            pv["done"], pv["total"], pv["frames"] = done, total, frames
            if file_path:
                pv["file"] = str(file_path)
            # 单调不减：新值小于当前值就保持当前值；done 追平/超过 total 直接满格。
            value = int(100 * done / total)
            if done >= total:
                value = 100
            value = max(self.progress_bar.value(), value)
            pv["value"] = value
            pv["label_override"] = (f"解析中 · {os.path.basename(str(file_path))}"
                                    if file_path else "解析中")
            pv["meta_override"] = f"文件 {done}/{total} · 本文件已扫 {frames:,} 帧"
            self.progress_bar.setRange(0, 100)
            self.progress_bar.setValue(value)
            self._render_progress()
        label = f"正在解析信号… ({done}/{total})" if total > 0 else "正在解析信号…"
        self.lbl_status.setText(label)
        self.statusBar().showMessage(f"正在解析信号… {message}")
        if file_path:
            self._set_file_status(str(file_path), STATUS_PARSING)

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
        # Step 13 A4：解析终态必须明确停在 100%，不能被 `_on_scan_thread_done` 复位。
        self._progress_timer.stop()
        pv = self._pv
        pv["active"] = True
        pv["finished"] = True
        pv["busy"] = False
        pv["value"] = 100
        pv["label_override"] = None
        pv["meta_override"] = None
        self.progress_bar.setRange(0, 100)
        self.progress_bar.setValue(100)
        self.lbl_progress_status.setText("解析完成")
        self.lbl_progress_status.setToolTip("解析完成")
        self.lbl_progress_meta.setText("100%")
        self._set_progress_state("done")
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
        # Step 13 A4：失败时进度区回到待命，绝不停在中间百分比；红色失败文案保留在
        # 文件列表行（STATUS_FAILED 红字 + tooltip）与状态栏/lbl_status。
        self._progress_idle()
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
            # Step 13 B2：重建不得丢掉"没有帧观测 / 没绑 DBC"的通道 —— 并上常驻槽位。
            keys |= set(self.channel_slots)
            self.channel_slots |= keys  # 只增不减：扫描观测到的新通道也变成常驻槽位
            for channel in sorted(keys):
                ch_info = channels.get(str(channel))
                observed = ch_info is not None
                if ch_info is None:  # 兼容 int 键的报告
                    ch_info = channels.get(channel)
                    observed = ch_info is not None
                ch_info = ch_info or {}
                dbc_error = ch_info.get("dbc_error")
                dbc_path = ch_info.get("dbc") or self.dbc_by_channel.get(channel)
                is_missing = channel in self.missing_dbc_channels
                # Step 13 B5：有 DBC（含失败/缺失）维持现状；无 DBC 再分"有/无帧观测"。
                if dbc_error:
                    suffix = "（DBC 解析失败）"
                elif is_missing:
                    suffix = DBC_MISSING_SUFFIX
                elif not dbc_path and observed:
                    suffix = "（未绑定 DBC，显示原始 ID）"
                elif not dbc_path:
                    suffix = "（无帧观测，右键可添加 DBC）"
                else:
                    suffix = ""
                if dbc_error:
                    tip = str(dbc_error)
                elif is_missing:
                    tip = f"Ch{channel}\nDBC 文件不存在：{dbc_path or '（未记录路径）'}"
                elif dbc_path:
                    tip = f"Ch{channel}\nDBC：{dbc_path}"
                elif observed:
                    tip = f"Ch{channel}\n未绑定 DBC，显示原始 ID"
                else:
                    tip = (f"Ch{channel}\n该通道在当前 BLF 中没有帧，或尚未添加 BLF；\n"
                           "添加 DBC 后点「🔍 解析信号」即可列出报文")
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
        # Step 22：重建树后把绑定了 DBC 的通道自动展开到 DBC 名称层级（不展开到报文）。
        for channel in sorted(self._collect_dbc_by_channel()):
            self._reveal_dbc_node(channel)
        # Step 19 D2：树重建后重算标题行的三态复选框（全选 / 部分 / 全不选）。
        self._sync_select_all_checkbox()

    def _on_search_changed(self, text: str) -> None:
        self.signal_proxy.set_keyword(text)
        self.signal_view.expandAll() if text else self.signal_view.collapseAll()
        if not text:
            self.signal_view.expandToDepth(0)
        # 过滤后「当前可见」集合变了，复选框的 x/y 计数必须同步（D2）。
        self._sync_select_all_checkbox()

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
        # Step 19 D2：树勾选变化 → 重算标题行三态（blockSignals，绝不递归）。
        self._sync_select_all_checkbox()
        # Step 16：勾选数变化会直接影响「📈 预览」/「📤 导出 HTML」是否可用。
        self._refresh_action_state()

    def _iter_visible_items(self, parent=QModelIndex()):
        """按代理模型递归产出「当前未被搜索过滤」的树节点（D2 的可见行）。"""
        for row in range(self.signal_proxy.rowCount(parent)):
            index = self.signal_proxy.index(row, 0, parent)
            item = self.signal_model.itemFromIndex(self.signal_proxy.mapToSource(index))
            if item is None:
                continue
            yield item
            yield from self._iter_visible_items(index)

    @staticmethod
    def _collect_signal_items(item: QStandardItem):
        """递归产出节点下全部信号叶子（跨过不可勾选的 DBC 容器）。"""
        for row in range(item.rowCount()):
            child = item.child(row)
            if child.data(NODE_ROLE) == NODE_SIGNAL:
                yield child
            else:
                yield from MainWindow._collect_signal_items(child)

    def _visible_signal_items(self) -> list[QStandardItem]:
        """当前可见（未被搜索过滤）范围内的信号叶子，按对象去重。

        只要一个通道/报文行仍可见，它下面的信号就视为可被这个复选框统一操作
        （报文行勾选本来就会向下广播到全部信号，包括被过滤隐藏的同级信号）。
        """
        seen: set[int] = set()
        result: list[QStandardItem] = []
        for item in self._iter_visible_items():
            kind = item.data(NODE_ROLE)
            if kind == NODE_SIGNAL:
                candidates = [item]
            elif kind in (NODE_CHANNEL, NODE_DBC, NODE_MESSAGE):
                candidates = list(self._collect_signal_items(item))
            else:
                candidates = []
            for signal in candidates:
                key = id(signal)
                if key not in seen:
                    seen.add(key)
                    result.append(signal)
        return result

    def _visible_signal_states(self) -> tuple[int, int]:
        """返回当前可见信号叶子的 ``(已勾选数, 总数)``（用于「全选（3/96）」文案）。"""
        signals = self._visible_signal_items()
        checked = sum(1 for item in signals
                      if item.checkState() == Qt.CheckState.Checked)
        return checked, len(signals)

    def _sync_select_all_checkbox(self) -> None:
        """把树里可见信号的勾选态映射到标题行三态复选框（不触发 clicked）。"""
        checkbox = getattr(self, "chk_select_all", None)
        if checkbox is None:
            return
        checked, total = self._visible_signal_states()
        if total and checked == total:
            state = Qt.CheckState.Checked
        elif checked == 0:
            state = Qt.CheckState.Unchecked
        else:
            state = Qt.CheckState.PartiallyChecked
        checkbox.blockSignals(True)
        try:
            checkbox.setTristate(True)
            checkbox.setCheckState(state)
            checkbox.setText(f"全选（{checked}/{total}）")
        finally:
            checkbox.blockSignals(False)

    def _set_visible_checked(self, checked: bool) -> None:
        """把当前可见行的勾选态广播为全选 / 全不选，然后重算父节点三态。"""
        state = Qt.CheckState.Checked if checked else Qt.CheckState.Unchecked
        self._checks_guard = True
        try:
            for item in self._iter_visible_items():
                if not self._is_checkable(item):
                    continue
                item.setCheckState(state)
                # 可见的通道/报文行代表其整棵子树：被搜索过滤隐藏的同级信号也一起
                # 广播（否则「全选」在按报文名过滤时只改了父行）。
                for row in range(item.rowCount()):
                    self._apply_state_down(item.child(row), state)
        finally:
            self._checks_guard = False
        self._cascade_checks()
        self._sync_select_all_checkbox()
        self._refresh_action_state()

    def _on_select_all_clicked(self, checked: bool) -> None:
        """三态复选框点击：部分/未选 → 全选；全选 → 全不选（与预览面板一致）。"""
        if self._checks_guard:
            return
        self._set_visible_checked(bool(checked))
        checked_count, total = self._visible_signal_states()
        if checked:
            self.statusBar().showMessage(
                f"已全选当前可见信号（{checked_count}/{total}）")
        else:
            self.statusBar().showMessage(
                f"已全不选当前可见信号（当前仍选中 {checked_count}/{total}）")

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
        self._sync_fill_hint()

    def _fill_policy(self) -> str:
        """当前下拉选中的填充方式（`hold` / `linear` / `none`）。"""
        index = max(0, self.combo_fill.currentIndex())
        return FILL_CHOICES[index][1]

    def _fill_default_policy(self) -> str:
        """本路径的默认填充方式：拆表 / 网格 → hold；原时序宽表 → none。"""
        if self.chk_split.isChecked():
            return "hold"
        hz = RESAMPLE_CHOICES[max(0, self.combo_resample.currentIndex())][1]
        return "hold" if hz is not None else "none"

    def _set_fill_policy(self, policy: str) -> None:
        """程序化切换填充方式（抑制 currentIndexChanged 造成的“用户已设置”误判）。"""
        index = FILL_INDEX.get(policy, FILL_INDEX["hold"])
        if self.combo_fill.currentIndex() == index:
            return
        self._fill_guard = True
        try:
            self.combo_fill.setCurrentIndex(index)
        finally:
            self._fill_guard = False

    def _on_fill_changed(self, _index: int) -> None:
        """用户手动切换填充方式：记住选择并刷新联动控件。"""
        if self._fill_guard:
            return
        self._fill_touched = True
        self._sync_fill_hint()

    def _sync_fill_hint(self) -> None:
        """按「拆表 / 网格 / 原时序」三态刷新填充下拉与最大保持时长输入。

        * 拆表模式：下拉禁用 + tooltip 说明；最大保持时长置灰；
        * 网格模式：只允许「阶梯保持 / 线性插值」，「不填充」置灰；
        * 原时序宽表：三者可选。
        未显式设置过时，选中值跟随路径默认。
        """
        split = self.chk_split.isChecked()
        hz = RESAMPLE_CHOICES[max(0, self.combo_resample.currentIndex())][1]
        grid = (not split) and hz is not None

        model = self.combo_fill.model()
        none_item = model.item(FILL_INDEX["none"]) if hasattr(model, "item") else None
        if none_item is not None:
            none_item.setEnabled(not grid)

        if not split and not self._fill_touched:
            self._set_fill_policy(self._fill_default_policy())
        elif grid and self._fill_policy() == "none":
            # 网格路径上「不填充」无意义：强制回到推荐值。
            self._set_fill_policy("hold")

        self.combo_fill.setEnabled(not split)
        self.combo_fill.setToolTip(FILL_SPLIT_TOOLTIP if split else FILL_TOOLTIP)
        hold = (not split) and self._fill_policy() == "hold"
        self.spin_fill_gap.setEnabled(hold)
        style = self.spin_fill_gap.style()
        style.unpolish(self.spin_fill_gap); style.polish(self.spin_fill_gap)

    def check_visible_items(self) -> None:
        """把当前过滤结果中可见的通道/报文/信号全部勾选（复选框与自检共用）。"""
        self._set_visible_checked(True)
        count, total = self._visible_signal_states()
        self.statusBar().showMessage(f"已勾选当前可见信号（{count}/{total}）")

    def _check_subtree(self, item: QStandardItem | None) -> int:
        """兼容旧调用：勾选一个子树并返回可勾选节点数（新代码走可见集合）。"""
        if item is None:
            return 0
        count = 0
        if self._is_checkable(item):
            item.setCheckState(Qt.CheckState.Checked)
            count = 1
        return count + sum(self._check_subtree(item.child(row)) for row in range(item.rowCount()))

    def uncheck_all_items(self) -> None:
        """全不选**整棵树**（不只是当前可见行；配置加载清场用）。"""
        self._checks_guard = True
        try:
            for row in range(self.signal_model.rowCount()):
                self._apply_state_down(self.signal_model.item(row), Qt.CheckState.Unchecked)
        finally:
            self._checks_guard = False
        self._cascade_checks()
        self._sync_select_all_checkbox()
        self.statusBar().showMessage("已清空全部勾选（此时启动转换将不会导出任何信号）")
        self._refresh_action_state()

    def _cascade_checks(self) -> None:
        """兜底：重算所有父节点勾选态（全选 / 全不选 / 配置恢复之后调用）。"""
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
            self.scan_worker and self.scan_worker.isRunning()) or bool(
            self.plot_worker and self.plot_worker.isRunning())
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
        # Step 16：预览 / 导出报告需要「内核就绪 + 有 BLF + 至少勾选 1 个信号 + 空闲」。
        has_selection = bool(self.build_selection()) if KERNEL_READY else False
        plot_ready = KERNEL_READY and bool(self.entries) and has_selection and not busy
        plot_hint = (KERNEL_MISSING_TEXT if not KERNEL_READY else
                     "请先添加至少一个 .blf 文件" if not self.entries else
                     "任务进行中…" if busy else
                     "请先勾选至少一个信号（可点「🔍 解析信号」后勾选）" if not has_selection else
                     "用抽稀后的 min/max 包络预览已勾选信号（不丢峰）")
        for action in (self.act_preview, self.act_export_html):
            action.setEnabled(plot_ready)
            action.setToolTip(plot_hint)
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
            # Step 14：填充方式与最大保持时长（0 → None）；不再写兼容字段 `ffill`。
            "fill_policy": self._fill_policy(),
            "ffill_max_gap_ms": self._fill_gap_ms(),
            # Step 22：日志路径交给 Rust 侧写 phase 2 预检与 panic（窗口化 exe 必看）。
            "log_path": str(log_path()),
        }
        logger.info("convert start files=%d split=%s resample_hz=%s format=%s "
                    "time_format=%s fill=%s budget_mb=%s",
                    len(config["blf_paths"]), config["split_by_message"],
                    config["resample_hz"], config["export_format"],
                    config["time_format"], config["fill_policy"],
                    config["memory_budget_mb"])
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
            "meta_override": None,
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

    def _begin_determinate(self, text: str) -> None:
        """确定态（Step 13 A1）：解析信号按文件级 ``done/total`` 推进。

        与 :meth:`_begin_busy` 的区别：这里由 ``_on_scan_progress`` 用真实进度驱动
        进度条，``busy`` 会在首个回调里关掉，因此不会出现 0→100→0 的乒乓动画。
        """
        self._progress_timer.stop()
        pv = self._new_progress_state()
        self._pv = pv
        pv["active"] = True
        pv["busy"] = True
        pv["started"] = time.monotonic()
        pv["label_override"] = text
        # 复位不确定态乒乓位置/方向：即便首个真实回调到来前心跳触发过，也只会从 0
        # 单调向上（残留的负 step 会让 `_on_progress_tick` 把进度条往回扫）。
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
        """单条进度条的加权映射（Step 19 A1）。

        * 阶段 1（解码）：``(done + within) / total`` 映射到 ``0..70``；
          ``within`` 是当前文件的帧内估算（拿不到时为 0）。
        * 阶段 2（合并导出）：``done / total`` 映射到 ``70..100``。

        返回值上限固定为 :data:`_PROGRESS_RUNNING_MAX`（99）：100 只允许由
        :meth:`_finish_progress_ok` 在成功终态写入。``total == 0`` 返回 ``None``
        （尚无确定进度，走 busy 动画）。
        """
        if total <= 0:
            return None
        if phase == 1:
            within = self._current_file_fraction(frames) or 0.0
            fraction = (done + within) / total
            # 文件完成事件有时 done == total，within 仍可能 >0；先夹到 0..1，
            # 否则阶段 1 会越过 70 的份额侵占阶段 2 的区间。
            fraction = max(0.0, min(1.0, fraction))
            raw = _PHASE1_SHARE * fraction
        else:
            fraction = max(0.0, min(1.0, done / total))
            raw = _PHASE1_SHARE + _PHASE2_SHARE * fraction
        raw = max(0.0, min(float(_PROGRESS_RUNNING_MAX), raw * 100.0))
        return int(round(raw))

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
                # A3：阶段名一眼可读；阶段 1 负责解码、阶段 2 负责合并导出。
                parts.append(_PHASE_HEADINGS.get(int(pv["phase"]),
                                                 f"阶段 {int(pv['phase'])}/2"))
            else:
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
        if pv.get("meta_override"):
            # Step 13 A3：解析副文本固定为「文件 d/t · 本文件已扫 n 帧」，心跳重绘不得覆盖。
            self.lbl_progress_meta.setText(str(pv["meta_override"]))
        else:
            percent_text = f"{percent}%" if percent is not None else "—"
            bytes_text = self._bytes_progress_text()
            self.lbl_progress_meta.setText(f"{percent_text} · {bytes_text}" if bytes_text
                                           else percent_text)
        if percent is not None and not pv["busy"]:
            self.progress_bar.setRange(0, 100)
            # A2：setValue(max(当前值, 新值)) —— 任何路径都不得让进度条回退。
            self.progress_bar.setValue(max(self.progress_bar.value(), int(percent)))

    def _on_progress_tick(self) -> None:
        """200ms 心跳：busy 时让进度条来回扫动（保证"已经在动"），并刷新「已用」。"""
        pv = self._pv
        if not pv["active"] or pv["finished"]:
            return
        if pv["busy"] or pv["value"] is None:
            self._busy_pos += self._busy_step
            if self._busy_pos >= _BUSY_SWEEP_MAX:
                self._busy_pos = _BUSY_SWEEP_MAX
                self._busy_step = -abs(self._busy_step)
            elif self._busy_pos <= 0:
                self._busy_pos = 0
                self._busy_step = abs(self._busy_step)
            self.progress_bar.setRange(0, 100)
            self.progress_bar.setValue(max(self.progress_bar.value(), self._busy_pos))
        self._render_progress()

    def _on_convert_progress(self, payload: dict) -> None:
        """进度回调（已转发回主线程）：刷新进度区与文件状态 → 原样记录 payload。"""
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
        # --selftest-progress-monotonic 通过 progress_hook 读到的必须是**已渲染后**的
        # 进度条数值（A2），因此回调放在 _render_progress 之后。
        if self.progress_hook is not None:
            self.progress_hook(dict(payload))
        label = str(payload.get("message")
                    or PHASE_FALLBACK.get(stage, "处理中"))
        if total > 0 and "(" not in label:
            label = f"{label} ({done}/{total})"
        heading = _PHASE_HEADINGS.get(phase) or f"阶段 {phase}/2"
        self.lbl_status.setText(f"{heading}：{label}")
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
        logger.info("convert done ok=True rows=%d tables=%d warnings=%d elapsed_s=%.2f",
                    rows, len(tables), len(warnings), elapsed)
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
            logger.warning("[fast_blf_core] 警告：%s", line)
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
        logger.error("[fast_blf_core] 转换失败：%s（code=%s）", message,
                     getattr(self.convert_worker, "error_code", None))

    def _on_convert_thread_done(self) -> None:
        self.convert_worker = None
        if not self._pv.get("finished"):  # 线程结束却没收到 ok/failed 时的兜底
            self._progress_idle()
        self._refresh_action_state()

    # ------------------------------------------- 画图预览 / HTML 报告（Step 16） ----
    def _current_time_format(self) -> str:
        """界面「时间列格式」下拉的当前口径（预览 / 报告与导出口径一致）。"""
        index = max(0, self.combo_time_format.currentIndex())
        return TIME_FORMAT_CHOICES[index][1]

    def _merged_dbc_map(self) -> dict[int, str]:
        """``self.dbc_by_channel`` 与树里实际绑定的并集（互为兜底）。"""
        merged = {int(k): str(v) for k, v in self.dbc_by_channel.items()}
        merged.update(self._collect_dbc_by_channel())
        return merged

    def _series_config(self, selection: dict, time_format: str,
                       buckets: int = 2000) -> dict:
        """构造 ``series_for_plot`` 的 config（§1.3：在 convert config 之上多 ``buckets``）。"""
        return {
            "blf_paths": [entry.path for entry in self.entries],
            "dbc_by_channel": self._merged_dbc_map(),
            "selected": selection,
            "time_format": time_format,
            "buckets": int(buckets),
            "log_path": str(log_path()),
        }

    def _plot_guard(self) -> tuple[bool, dict]:
        """预览 / 导出的前置检查：不满足时只提示状态栏（不弹窗），返回 ``(是否继续, selection)``。"""
        if not KERNEL_READY:
            self.statusBar().showMessage(KERNEL_MISSING_TEXT)
            return False, {}
        if not self.entries:
            self.statusBar().showMessage("请先添加至少一个 .blf 文件（预览 / 报告都需要原始数据）")
            return False, {}
        selection = self.build_selection()
        if not selection:
            self.statusBar().showMessage("请先勾选至少一个信号（可点「🔍 解析信号」后勾选）")
            return False, {}
        return True, selection

    def start_plot_preview(self, selection: dict | None = None) -> None:
        """工具栏 / 右键「📈 预览」：在 Worker 线程打开 ``PlotSession``，再开独立预览窗。

        Step 18 起预览不再逐次调 ``series_for_plot``：会话在 open 时一次性把帧缓存
        落到磁盘，之后缩放 / 平移 / 切口径都只读缓存。
        """
        if self.plot_worker is not None and self.plot_worker.isRunning():
            self.statusBar().showMessage("取数进行中，请稍候…")
            return
        allowed, current = self._plot_guard()
        if not allowed:
            return
        # 再预览 = 新会话（旧窗口关闭时已经 session.close()；磁盘缓存命中即秒开）。
        self._close_preview_window()
        self._plot_selection = selection or current
        self._plot_action = "preview"
        self._plot_out_path = None
        self._plot_time_format = self._current_time_format()
        self._launch_preview_session_worker()

    def _close_preview_window(self) -> None:
        """关闭现有预览窗（会触发 session.close()），不弹任何确认框。"""
        window = self.preview_window
        self.preview_window = None
        if window is not None:
            try:
                if getattr(window, "_error_code", None) is None:
                    self._preview_view_settings = window.plot_settings()
                window.close()
            except RuntimeError:  # C++ 对象已销毁
                pass

    @staticmethod
    def _selection_keys(selection: dict | None) -> list[str]:
        """把 ``selected`` 按用户勾选顺序展开成 ``ch{ch}/{msg}/{sig}`` 列表。"""
        keys: list[str] = []
        for channel, messages in (selection or {}).items():
            if not isinstance(messages, dict):
                continue
            for message, signals in messages.items():
                for signal in (signals or []):
                    keys.append(f"ch{channel}/{message}/{signal}")
        return list(dict.fromkeys(keys))

    def _preview_session_config(self, selection: dict, time_format: str) -> dict:
        """``open_plot_session`` 的 config（§1.4）。

        Step 19 C3：``cache_budget_mb`` / ``cache_dir`` 一律来自 ``settings.json``
        （``cache_dir`` 为空时用稳定的系统临时目录），打开预览时**不读硬编码常量**。
        """
        cache_dir = (Path(self.preview_cache_dir).expanduser() if self.preview_cache_dir
                     else Path(tempfile.gettempdir()) / "fblf_preview_cache")
        return {
            "blf_paths": [entry.path for entry in self.entries],
            "dbc_by_channel": self._merged_dbc_map(),
            "selected": selection,
            "time_format": time_format,
            "temp_dir": tempfile.gettempdir(),
            "cache_dir": str(cache_dir),
            "cache_budget_mb": int(self.preview_cache_budget_mb),
            # Step 22：与 convert config 一致，Rust 侧需要知道日志落哪里。
            "log_path": str(log_path()),
        }

    def _launch_preview_session_worker(self) -> None:
        """A4：打开会话在 Worker 线程里做，带确定进度（progress_cb）。"""
        selection = self._plot_selection or {}
        time_format = self._plot_time_format or self._current_time_format()
        config = self._preview_session_config(selection, time_format)
        initial_keys = self._selection_keys(selection)[:3]
        self._begin_determinate("正在打开预览会话（缓存命中则秒开）…")
        self.lbl_status.setText("正在打开预览会话…")
        self.statusBar().showMessage(
            "正在准备帧缓存（首次建缓存有确定进度；已缓存则直接命中）…")
        self.plot_worker = Worker("session", config=config,
                                  initial_keys=initial_keys, parent=self)
        self._spawn(self.plot_worker, self._on_plot_progress,
                    self._on_plot_ok, self._on_plot_failed, self._on_plot_thread_done)
        self._refresh_action_state()

    def start_export_html(self, selection: dict | None = None) -> None:
        """工具栏 / 右键「📤 导出 HTML 报告」：先让用户选文件名，再在 Worker 里取数并落盘。"""
        if self.plot_worker is not None and self.plot_worker.isRunning():
            self.statusBar().showMessage("取数进行中，请稍候…")
            return
        allowed, current = self._plot_guard()
        if not allowed:
            return
        selection = selection or current
        default_dir = str(Path(self.entries[0].path).parent) if self.entries else str(Path.cwd())
        default_name = report_builder.default_report_name()
        if self.quiet_dialogs:
            target = str(Path(tempfile.mkdtemp(prefix="fblf_report_")) / default_name)
        else:
            target, _filter = QFileDialog.getSaveFileName(
                self, "导出离线 HTML 报告", str(Path(default_dir) / default_name),
                "HTML 报告 (*.html);;所有文件 (*)")
        if not target:
            self.statusBar().showMessage("已取消导出 HTML 报告")
            return
        if not target.lower().endswith(".html"):
            target += ".html"
        self._plot_selection = selection
        self._plot_action = "export"
        self._plot_out_path = target
        self._plot_time_format = self._current_time_format()
        self._launch_series_worker()

    def _launch_series_worker(self) -> None:
        """``series_for_plot`` 的一次性取数 Worker（HTML 报告 / 不建缓存的降级预览）。"""
        buckets = 2000
        if self.preview_window is not None:
            try:
                buckets = max(100, min(20000, int(self.preview_window.canvas.width())))
            except RuntimeError:  # 窗口 C++ 对象已销毁
                buckets = 2000
        config = self._series_config(self._plot_selection or {}, self._plot_time_format
                                     or self._current_time_format(), buckets)
        self._begin_busy("正在解码并抽稀（min/max 包络）…")
        self.lbl_status.setText("正在准备画图数据…")
        self.statusBar().showMessage(
            "正在调用 series_for_plot 取数（数据降维在内核完成，界面不解码 BLF）")
        self.plot_worker = Worker("series", config=config, parent=self)
        self._spawn(self.plot_worker, self._on_plot_progress,
                    self._on_plot_ok, self._on_plot_failed, self._on_plot_thread_done)
        self._refresh_action_state()

    def _on_plot_progress(self, payload: dict) -> None:
        """预览会话用确定进度；``series_for_plot`` 无 progress_cb，只刷新文案。"""
        if self._plot_action == "preview":
            self._on_preview_progress(payload)
            return
        label = str(payload.get("message") or "正在抽稀…")
        self.lbl_progress_status.setText(f"正在准备画图数据… {label}")

    def _on_preview_progress(self, payload: dict) -> None:
        """A4 / §2：``open_plot_session`` 的 progress_cb → 文件级确定进度条。"""
        message = str(payload.get("message") or "正在准备帧缓存…")
        done = int(payload.get("done") or 0)
        total = int(payload.get("total") or 0)
        frames = int(payload.get("frames_done") or 0)
        file_path = payload.get("file")
        pv = self._pv
        if total > 0:
            pv["active"] = True
            pv["busy"] = False
            pv["finished"] = False
            pv["done"], pv["total"], pv["frames"] = done, total, frames
            if file_path:
                pv["file"] = str(file_path)
            value = 100 if done >= total else int(round(100.0 * done / total))
            value = max(self.progress_bar.value(), max(0, min(100, value)))
            pv["value"] = value
            name = os.path.basename(str(file_path)) if file_path else ""
            pv["label_override"] = (f"准备预览缓存 · {name}" if name
                                    else "准备预览缓存")
            pv["meta_override"] = f"文件 {done}/{total} · 本文件已扫 {frames:,} 帧"
            self.progress_bar.setRange(0, 100)
            self.progress_bar.setValue(value)
            self._render_progress()
            self.lbl_status.setText(f"准备预览缓存… ({done}/{total})")
            self.statusBar().showMessage(f"准备预览缓存… {message}")
            if file_path:
                self._set_file_status(str(file_path), STATUS_PARSING)

    def _finish_plot_ok(self, text: str) -> None:
        """画图 / 报告成功：进度区绿色完成行。"""
        self._progress_timer.stop()
        pv = self._pv
        pv["active"] = True
        pv["finished"] = True
        pv["busy"] = False
        pv["value"] = 100
        self.progress_bar.setRange(0, 100)
        self.progress_bar.setValue(100)
        self.lbl_progress_status.setText(text)
        self.lbl_progress_status.setToolTip(text)
        self.lbl_progress_meta.setText("100%")
        self._set_progress_state("success")

    def _finish_plot_failed(self, message: str) -> None:
        """画图 / 报告失败：进度区红字（不弹窗）。"""
        self._progress_timer.stop()
        pv = self._pv
        pv["active"] = True
        pv["finished"] = True
        pv["busy"] = False
        pv["value"] = None
        self.progress_bar.setRange(0, 100)
        self.progress_bar.setValue(0)
        text = f"✗ {message}"
        self.lbl_progress_status.setText(text)
        self.lbl_progress_status.setToolTip(text)
        self.lbl_progress_meta.setText("失败")
        self._set_progress_state("error")
        self.lbl_status.setText(f"画图 / 报告失败：{message}")
        logger.error("[fast_blf_core] 画图 / 报告失败：%s", message)

    def _collect_plot_meta(self, time_format: str) -> dict:
        """HTML 报告需要的界面上下文（文件清单 / DBC 绑定 / 版本 / 生成时间）。"""
        files = [{"path": entry.path, "size": entry.size,
                  "ok": entry.status != STATUS_FAILED, "error": entry.error}
                 for entry in self.entries]
        return {
            "title": "Fast BLF Converter · 数据预览报告",
            "generated_at": time.strftime("%Y-%m-%d %H:%M:%S"),
            "version": core.version() if KERNEL_READY else "—",
            "files": files,
            "dbc_by_channel": self._merged_dbc_map(),
            "channel_summary": dict(self.channel_summary or {}),
            "time_format": time_format,
        }

    def _on_plot_ok(self, report: dict) -> None:
        action = self._plot_action
        if (isinstance(report, dict) and report.get("session") is not None
                and action != "preview"):
            # 会话打开完成时用户已经把预览窗关掉：释放句柄，不重开窗口。
            try:
                report["session"].close()
            except Exception:  # noqa: BLE001
                pass
            self._progress_idle()
            return
        if action == "preview":
            if isinstance(report, dict) and report.get("session") is not None:
                self._open_preview_session(report)
            else:
                self._finish_plot_failed("打开预览会话失败：内核返回为空")
            return
        if not isinstance(report, dict) or not report.get("ok"):
            self._finish_plot_failed("取数失败：内核返回 ok=False")
            return
        time_format = self._plot_time_format or self._current_time_format()
        if action == "export":
            meta = self._collect_plot_meta(time_format)
            out_path = self._plot_out_path or report_builder.default_report_name()
            result = report_builder.generate_report(report, meta, out_path)
            if not result.get("ok"):
                self._finish_plot_failed(str(result.get("error") or "报告生成失败"))
                return
            size_text = human_size(int(result.get("bytes") or 0))
            self._finish_plot_ok(
                f"✓ 报告已导出：{os.path.basename(str(result.get('path')))}（{size_text}）")
            self.lbl_status.setText(f"HTML 报告：{result.get('path')}（{size_text}）")
            self.statusBar().showMessage(f"离线自包含报告已生成：{result.get('path')}")
        elif action == "preview_static":
            self._open_static_preview(report)
        else:
            # 预览窗在取数期间被用户关掉：丢弃结果，不无端重开窗口。
            self._progress_idle()

    def _on_plot_failed(self, message: str) -> None:
        if self._plot_action == "preview" and (
                "CACHE_BUDGET_EXCEEDED" in str(message)
                or getattr(self.plot_worker, "error_code", None) == "CACHE_BUDGET_EXCEEDED"):
            self._open_preview_cache_error(str(message))
            return
        self._finish_plot_failed(message)

    def _on_plot_thread_done(self) -> None:
        self.plot_worker = None
        if not self._pv.get("finished"):  # 线程结束却没收到 ok/failed 时的兜底
            self._progress_idle()
        self._plot_action = None
        self._refresh_action_state()

    # ------------------------------------------------- 预览窗生命周期 ----
    def _wire_preview_signals(self, window: PreviewWindow) -> None:
        """预览窗口的关闭 / 设置 / F1 三个动作统一接到主窗。"""
        window.closed.connect(self._on_preview_closed)
        window.settings_changed.connect(self._on_preview_settings_changed)
        window.budget_retry_requested.connect(self._on_preview_budget_retry)
        window.reduce_selection_requested.connect(self._on_preview_reduce_selection)
        window.no_cache_requested.connect(self._on_preview_no_cache)

    @staticmethod
    def _clean_core_message(message: str) -> str:
        text = str(message or "")
        for prefix in ("CoreError: ", "ValueError: "):
            if text.startswith(prefix):
                text = text[len(prefix):]
        return text.strip()

    def _open_preview_session(self, result: dict) -> None:
        """A1/A4：把 Worker 打开的 PlotSession 交给预览窗口（窗口持有会话）。"""
        session = result.get("session")
        config = dict(result.get("config") or {})
        info = dict(result.get("info") or {})
        if session is None:
            self._finish_plot_failed("打开预览会话失败：session 为空")
            return
        budget = int(config.get("cache_budget_mb") or self.preview_cache_budget_mb)
        time_format = str(config.get("time_format") or self._current_time_format())
        view_settings = dict(self._preview_view_settings)
        if not self._preview_settings_from_preset:
            # 非预设场景：只恢复模式 / 颜色 / 线型；可见性按 E4 从勾选顺序重新取前 3。
            view_settings = {key: value for key, value in view_settings.items()
                             if key not in ("visible", "order")}
        try:
            window = PreviewWindow(
                session, info=info, stats=result.get("stats") or [], config=config,
                initial_keys=result.get("initial_keys") or [],
                time_format=time_format, time_format_choices=TIME_FORMAT_CHOICES,
                cache_budget_mb=budget, format_meta=result.get("format_meta") or {},
                view_settings=view_settings, parent=None)
        except Exception as exc:  # noqa: BLE001 - 构造失败必须释放会话句柄
            try:
                session.close()
            except Exception:  # noqa: BLE001
                pass
            self._finish_plot_failed(f"预览窗口构造失败：{type(exc).__name__}: {exc}")
            return
        self._wire_preview_signals(window)
        self.preview_window = window
        self._last_preview_config = config
        self._preview_view_settings = window.plot_settings()
        window.show()
        window.raise_()
        window.activateWindow()
        count = len(window.canvas.keys())
        hit = "缓存命中" if info.get("cache_hit") else "已建缓存"
        self._finish_plot_ok(f"✓ 预览就绪：{count} 个信号 · {hit} · "
                             f"首屏可见 {window.canvas.visible_count()} 条")
        self.statusBar().showMessage(
            f"预览窗口已打开（{count} 个信号；缩放/平移/切口径不再重新解码）")

    def _open_static_preview(self, report: dict) -> None:
        """F1「不建缓存继续（较慢）」：一次性 ``series_for_plot`` 的降级预览。"""
        time_format = self._plot_time_format or self._current_time_format()
        window = self.preview_window
        if window is not None and getattr(window, "_session", None) is None:
            window.set_static_data(report, time_format=time_format)
        else:
            if window is not None:
                self._close_preview_window()
            window = PreviewWindow(
                static_data=report, time_format=time_format,
                time_format_choices=TIME_FORMAT_CHOICES,
                cache_budget_mb=self.preview_cache_budget_mb,
                view_settings=({key: value for key, value in self._preview_view_settings.items()
                                if key not in ("visible", "order")}
                               if not self._preview_settings_from_preset
                               else self._preview_view_settings),
                on_refetch=self._refetch_preview, parent=None)
            self._wire_preview_signals(window)
            self.preview_window = window
        self._preview_view_settings = window.plot_settings()
        window.show()
        window.raise_()
        window.activateWindow()
        series = report.get("series") or []
        total = sum(int(item.get("count", 0) or 0) for item in series)
        buckets = int(report.get("buckets") or 0)
        self._finish_plot_ok(f"✓ 降级预览就绪（未建缓存）：{len(series)} 个信号 · "
                             f"{total:,} 个原始点 · {buckets} 桶")
        self.statusBar().showMessage("降级预览：未建帧缓存，缩放/平移仍可用；"
                                     "切换时间口径需要重新抽稀")

    def _open_preview_cache_error(self, message: str) -> None:
        """F1：``CACHE_BUDGET_EXCEEDED`` 在预览窗口内显示内联横幅，不弹模态错误框。"""
        self._close_preview_window()
        window = PreviewWindow(
            error_message=self._clean_core_message(message),
            error_code="CACHE_BUDGET_EXCEEDED",
            time_format=self._plot_time_format or self._current_time_format(),
            time_format_choices=TIME_FORMAT_CHOICES,
            cache_budget_mb=self.preview_cache_budget_mb, parent=None)
        self._wire_preview_signals(window)
        self.preview_window = window
        window.show()
        window.raise_()
        window.activateWindow()
        self._finish_plot_failed("预览缓存预算不足（已在预览窗口内联提示，未弹窗）")
        self.lbl_status.setText("缓存预算不足：请在预览窗口选择「提高上限 / 减少报文 / 不建缓存」")

    # ------------------------------------------------- F1 三个动作 ----
    def _on_preview_budget_retry(self, budget: int) -> None:
        """F1-1：提高上限并重试（预算来自预览窗口里的设置对话框，并持久化）。"""
        if not self._apply_cache_settings(int(budget), self.preview_cache_dir):
            self.statusBar().showMessage(
                f"缓存上限 {budget} MiB 不在 "
                f"{preset_store.CACHE_BUDGET_MIN_MB}..{preset_store.CACHE_BUDGET_MAX_MB} 范围内")
            return
        self._close_preview_window()
        if not self._plot_selection:
            self.statusBar().showMessage("预览上下文已丢失，请重新点「📈 预览」")
            return
        self._plot_action = "preview"
        self._launch_preview_session_worker()

    def _on_preview_reduce_selection(self) -> None:
        """F1-2：只预览更少报文——回到主窗口，让用户减少勾选。"""
        self._close_preview_window()
        self.showNormal()
        self.raise_()
        self.activateWindow()
        self.statusBar().showMessage(
            "请在主窗口减少勾选的报文 / 信号，然后重新点「📈 预览」")
        self.lbl_status.setText("请减少勾选的报文 / 信号后重新预览")

    def _on_preview_no_cache(self) -> None:
        """F1-3：不建缓存继续（较慢）——一次性 ``series_for_plot`` 降级预览。"""
        self._close_preview_window()
        if not self._plot_selection:
            self.statusBar().showMessage("预览上下文已丢失，请重新点「📈 预览」")
            return
        self._plot_action = "preview_static"
        self._plot_time_format = self._plot_time_format or self._current_time_format()
        self._launch_series_worker()

    def _on_preview_settings_changed(self, settings: dict) -> None:
        """模式 / 颜色 / 线型 / 可见性变化：主窗记住；保存预设时写进 JSON。"""
        if isinstance(settings, dict):
            self._preview_view_settings = dict(settings)

    def _refetch_preview(self, time_format: str) -> None:
        """降级预览才需要：切口径 → 重新走 ``series_for_plot``（会话模式不走这里）。"""
        if self.plot_worker is not None and self.plot_worker.isRunning():
            self.statusBar().showMessage("取数进行中，请稍候再切换时间格式")
            return
        if not self._plot_selection:
            self.statusBar().showMessage("预览上下文已丢失，请重新点「📈 预览」")
            return
        self._plot_action = "preview_static"
        self._plot_time_format = time_format
        self._launch_series_worker()

    def _on_preview_closed(self) -> None:
        """预览窗关闭：会话已在 closeEvent 里 ``session.close()``；保留最近上下文。"""
        window = self.preview_window
        if window is not None:
            try:
                if getattr(window, "_error_code", None) is None:
                    self._preview_view_settings = window.plot_settings()
                    self._last_preview_config = dict(window._config or {})  # noqa: SLF001
            except RuntimeError:  # C++ 对象已销毁
                pass
        self.preview_window = None
        # 若关闭时恰好有 Worker 在开新一轮会话，丢弃结果，避免窗口自己弹回来。
        if self._plot_action in ("preview", "preview_static"):
            self._plot_action = None

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
        # 关闭窗口时把三类工作线程都收掉：只处理转换线程时，扫描/取数中关窗可能让进程退不出去。
        for worker in (self.convert_worker, self.scan_worker, self.plot_worker):
            if worker is None or not worker.isRunning():
                continue
            cancel = getattr(worker, "cancel", None)
            if callable(cancel):
                try:
                    cancel()
                except Exception:  # noqa: BLE001 - 关闭阶段不允许再抛
                    pass
            worker.wait(3000)
        # Step 16：预览窗是独立顶层窗口，主窗关闭时一并关掉，不留僵尸窗口。
        if self.preview_window is not None:
            try:
                self.preview_window.close()
            except RuntimeError:  # 底层 C++ 对象已销毁
                pass
            self.preview_window = None
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
        logger.warning("[GUI] 未能加载主题 %s：%s", THEME_PATH, exc)
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
        # Step 13 B：通道集合并上常驻 channel_slots（Ch1~Ch8），故不再是 2 个通道。
        if channels != len(CHANNEL_CHOICES):
            print(f"DEEP_FAIL channels={channels} expected={len(CHANNEL_CHOICES)}")
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


def run_selftest_parse() -> int:
    """``--selftest-parse``：解析信号确定进度 + 通道粘性 / 手动添加的无人值守验证。

    * 成功：打印 ``PARSE_OK values=[...] channels=N added_ch9=ok``，返回 0；
    * 断言失败：打印 ``PARSE_FAIL <原因>``，返回 2；
    * 内核 / 夹具缺失：打印 ``PARSE_SKIP ...``，返回 0（与 no_kernel 同一约定）。
    """
    if core is None:
        print("PARSE_SKIP no_kernel")
        return 0
    root = Path(__file__).resolve().parent.parent
    src = root / "fixtures" / "multi_channel.blf"
    dbc = root / "fixtures" / "demo.dbc"
    missing = [str(p) for p in (src, dbc) if not p.is_file()]
    if missing:
        print(f"PARSE_SKIP no_fixture {' '.join(missing)}")
        return 0
    app = QApplication.instance()
    if app is None:
        print("PARSE_FAIL no_qapplication")
        return 2
    window: MainWindow | None = None
    try:
        window = MainWindow()
        window.quiet_dialogs = True  # 无人值守：任何模态弹窗都会挂死自检
        temp_root = Path(tempfile.mkdtemp(prefix="fblf_parse_selftest_"))
        blf_paths = []
        for index in range(3):  # 复制 3 份：让文件级进度有 3 格
            dst = temp_root / f"copy{index + 1}.blf"
            shutil.copyfile(src, dst)
            blf_paths.append(str(dst.resolve()))
        window.clear_files()
        window.add_blf_paths(blf_paths)
        window.dbc_by_channel = {1: str(dbc.resolve()), 2: str(dbc.resolve())}
        window._refresh_dbc_label()
        # valueChanged 只在数值"变化"时触发；3 份小 BLF 的扫描可能在 200ms 心跳前
        # 就从 0 直接跳到 100，故把起始值也纳入序列，保证 ≥2 个不同取值可被观测。
        recorded: list[int] = [window.progress_bar.value()]
        window.progress_bar.valueChanged.connect(lambda value: recorded.append(int(value)))
        window.start_parse_signals()
        if window.scan_worker is None:
            print("PARSE_FAIL scan_not_started")
            return 2
        if not _pump_scan(app, window, timeout=60.0):
            print("PARSE_FAIL timeout")
            return 2
        for previous, current in zip(recorded, recorded[1:]):
            if current < previous:
                print(f"PARSE_FAIL non_monotonic values={recorded}")
                return 2
        if not recorded or recorded[-1] != 100:
            print(f"PARSE_FAIL not_100 values={recorded}")
            return 2
        unique = sorted(set(recorded))
        if len(unique) < 2:
            print(f"PARSE_FAIL too_few_values values={unique}")
            return 2
        channels = window.signal_model.rowCount()
        if channels < len(CHANNEL_CHOICES):
            print(f"PARSE_FAIL channels={channels} expected>={len(CHANNEL_CHOICES)}")
            return 2
        if window._channel_item(7) is None:
            print("PARSE_FAIL channel_7_missing")
            return 2
        before = window.signal_model.rowCount()
        if not window._add_channel(9):
            print("PARSE_FAIL add_ch9_first_false")
            return 2
        after = window.signal_model.rowCount()
        if after <= before:
            print(f"PARSE_FAIL rowcount_not_grown before={before} after={after}")
            return 2
        if window._add_channel(9):
            print("PARSE_FAIL add_ch9_second_true")
            return 2
    except Exception as exc:  # noqa: BLE001 - 自检要把任何异常变成可 grep 的失败行
        print(f"PARSE_FAIL {type(exc).__name__}: {exc}")
        return 2
    finally:
        if window is not None:
            _destroy_window(window)
    print(f"PARSE_OK values={unique} channels={channels} added_ch9=ok")
    return 0


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
        # B1/B4：6 个字段全部显式给值，保存后逐字段核对 JSON。
        export = {
            "split_by_message": False,
            "resample_hz": 10.0,
            "export_format": "parquet",
            "time_format": "absolute",
            "fill_policy": "linear",
            "ffill_max_gap_ms": 50.0,
        }
        # 1) 构造配置并保存，逐字段核对 JSON。
        saved = preset_store.save_preset("BMS项目A", {1: dbc, 2: dbc}, selected, export)
        assert saved.is_file(), "保存后文件不存在"
        print("[config] 2/6 预设已保存并落盘", flush=True)
        raw = json.loads(saved.read_text(encoding="utf-8"))
        assert raw.get("schema") == preset_store.SCHEMA == 2, \
            f"schema={raw.get('schema')!r}"
        assert raw.get("name") == "BMS项目A", f"name={raw.get('name')!r}"
        assert raw.get("dbc_by_channel") == {"1": dbc, "2": dbc}, "dbc_by_channel 不一致"
        assert raw.get("selected") == selected, f"selected={raw.get('selected')!r}"
        assert raw.get("export") == export, f"export={raw.get('export')!r}"
        assert set(raw.get("export") or {}) == set(preset_store.DEFAULT_EXPORT), \
            f"export 字段不全：{sorted((raw.get('export') or {}))!r}"
        # 2) 清空 UI 状态 → 走加载流程 → 断言 DBC / 勾选 / 6 个导出字段都被恢复。
        window.dbc_by_channel.clear()
        window._remove_dbc_nodes()
        window.uncheck_all_items()
        window.apply_export_policy(dict(preset_store.DEFAULT_EXPORT))
        assert window.build_selection() == {}, "清空后仍有勾选残留"
        window.apply_preset(preset_store.read_preset(saved))
        assert _pump_scan(app, window), "加载配置后的自动解析超时"
        assert window.dbc_by_channel == {1: dbc, 2: dbc}, f"DBC 映射={window.dbc_by_channel!r}"
        assert not window.missing_dbc_channels, f"误报缺失={window.missing_dbc_channels}"
        assert window.current_export_policy() == export, \
            f"导出策略={window.current_export_policy()!r}"
        # B3：加载后 6 个控件逐个同步（不是只回读 policy）。
        assert window.chk_split.isChecked() is False, "拆表复选框未同步"
        assert RESAMPLE_CHOICES[window.combo_resample.currentIndex()][1] == 10.0, \
            "重采样控件未同步"
        assert FORMAT_CHOICES[window.combo_format.currentIndex()][1] == "parquet", \
            "导出格式控件未同步"
        assert window._current_time_format() == "absolute", "时间格式控件未同步"
        assert window._fill_policy() == "linear", "填充方式控件未同步"
        assert window._fill_gap_ms() == 50.0, "最大保持时长控件未同步"
        assert window.last_restore.get("restored") == 1, f"恢复信号={window.last_restore!r}"
        assert window.last_restore.get("missed") == 0, f"未匹配={window.last_restore!r}"
        assert window.build_selection() == selected, f"恢复后勾选={window.build_selection()!r}"
        print("[config] 3/6 加载预设 + 自动解析 + 勾选/6 字段恢复完成", flush=True)
        # 3) UI 侧保存路径同样可往返（复用已恢复的勾选）。
        ui_saved = window.save_preset("UI往返")
        ui_raw = json.loads(ui_saved.read_text(encoding="utf-8"))
        assert ui_raw.get("selected") == selected, f"UI 保存 selected={ui_raw.get('selected')!r}"
        assert ui_raw.get("dbc_by_channel") == {"1": dbc, "2": dbc}, "UI 保存 DBC 不一致"
        assert ui_raw.get("export") == export, f"UI 保存 export={ui_raw.get('export')!r}"
        print("[config] 4/6 UI 侧保存往返完成", flush=True)
        channels = len(window.dbc_by_channel)
        signals = int(window.last_restore["restored"])
        policy = window.current_export_policy()
        # 3b) B2：非法值回落默认 + warnings；B4：schema 1 旧配置补默认值。
        legacy = {
            "schema": 1,
            "name": "旧版配置",
            "saved_at": "2026-01-01T00:00:00+08:00",
            "dbc_by_channel": {"1": dbc},
            "selected": selected,
            "export": {"split_by_message": True, "resample_hz": None,
                       "export_format": "csv"},
        }
        legacy_path = temp_root / "legacy_schema1.json"
        legacy_path.write_text(json.dumps(legacy, ensure_ascii=False, indent=2) + "\n",
                               encoding="utf-8")
        legacy_preset = preset_store.read_preset(legacy_path)
        expected_legacy = dict(preset_store.DEFAULT_EXPORT)
        expected_legacy.update({"split_by_message": True, "resample_hz": None,
                                "export_format": "csv"})
        assert legacy_preset.export == expected_legacy, \
            f"schema1 补齐结果={legacy_preset.export!r}"
        assert legacy_preset.export["time_format"] == "relative_s", "schema1 未补 time_format"
        assert legacy_preset.export["fill_policy"] == "hold", "schema1 未补 fill_policy"
        assert legacy_preset.export["ffill_max_gap_ms"] is None, \
            "schema1 未补 ffill_max_gap_ms"
        assert legacy_preset.warnings == [], f"schema1 误报 warnings={legacy_preset.warnings!r}"
        window.apply_export_policy(legacy_preset.export)
        assert window.current_export_policy() == expected_legacy, \
            f"schema1 控件同步={window.current_export_policy()!r}"
        bad_path = temp_root / "bad_export.json"
        bad_export = {"split_by_message": True, "resample_hz": -1,
                      "export_format": "bogus", "time_format": "bogus",
                      "fill_policy": "bogus", "ffill_max_gap_ms": 0}
        bad_path.write_text(json.dumps({"schema": 2, "name": "bad", "export": bad_export},
                                       ensure_ascii=False, indent=2) + "\n",
                            encoding="utf-8")
        bad_preset = preset_store.read_preset(bad_path)
        assert bad_preset.export["resample_hz"] is None, "非法重采样未回落"
        assert bad_preset.export["export_format"] == "csv", "非法导出格式未回落"
        assert bad_preset.export["time_format"] == "relative_s", "非法时间格式未回落"
        assert bad_preset.export["fill_policy"] == "hold", "非法填充方式未回落"
        assert bad_preset.export["ffill_max_gap_ms"] is None, "非法保持时长未回落"
        assert len(bad_preset.warnings) == 5, f"warnings={bad_preset.warnings!r}"
        # 3c) C1：settings.json 与预设分离，预设扫描不得把它当坏配置。
        settings_saved = preset_store.save_settings(
            {"cache_budget_mb": 512, "cache_dir": str(temp_root / "cache")})
        assert settings_saved.name == preset_store.SETTINGS_FILENAME, "设置文件名错误"
        settings_raw = json.loads(settings_saved.read_text(encoding="utf-8"))
        assert settings_raw == {"cache_budget_mb": 512,
                                "cache_dir": str(temp_root / "cache")}, \
            f"settings={settings_raw!r}"
        assert preset_store.read_settings() == settings_raw, "设置读取不一致"
        presets_now, _skipped_now = preset_store.scan_presets()
        assert all(item.name != preset_store.SETTINGS_FILENAME for item in presets_now), \
            "settings.json 被误当预设"
        print("[config] 4b/6 schema1 兼容 / 非法回落 / settings.json 完成", flush=True)
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


# --------------------------------------------- --selftest-fill（Step 14） ----
def _read_fill_csv(report: dict) -> dict[str, list[float | None]]:
    """把 `convert` 报告里的第一张 CSV 表读成 ``{列名: [值或 None]}``。"""
    path = str(report["tables"][0]["path"])
    with open(path, encoding="utf-8", newline="") as handle:
        reader = csv.reader(handle)
        header = next(reader)
        rows = list(reader)
    data: dict[str, list[float | None]] = {name: [] for name in header}
    for row in rows:
        for name, cell in zip(header, row):
            data[name].append(float(cell) if cell != "" else None)
    return data


def _linear_expected(times: list[float], values: list[float],
                     grid: list[float]) -> list[float]:
    """复刻 Rust `resample_linear` 的语义（命中 / 两点插值 / 首末 clamp）。"""
    out: list[float] = []
    for point in grid:
        idx = bisect.bisect_right(times, point)  # 时间 <= point 的样本数
        if idx == 0:
            out.append(values[0])
        elif idx >= len(times):
            out.append(values[-1])
        else:
            t_left, v_left = times[idx - 1], values[idx - 1]
            t_right, v_right = times[idx], values[idx]
            span = t_right - t_left
            if abs(span) <= 1e-15:
                out.append(v_left)
            else:
                out.append(v_left + (v_right - v_left) * (point - t_left) / span)
    return out


def run_selftest_fill() -> int:
    """``--selftest-fill``：填充策略（hold / linear / max_gap / none）端到端验证。

    * 成功：打印 ``FILL_OK hold_distinct=.. linear_distinct=.. gapped_nulls=.. none_nulls=..``；
    * 断言失败：打印 ``FILL_FAIL <原因>`` 并返回 2；
    * 内核缺失 / 夹具缺失：分别打印 ``FILL_SKIP no_kernel`` / ``FILL_SKIP no_fixture ..`` 并返回 0。

    说明：任务书写的 ``timefmt.blf`` 是 10ms 单报文、而 ``demo.dbc`` 里没有 `TimeMsg`，
    两者无法解出任何信号；这里改用同一套合成夹具里**真正多周期**的
    ``fixtures/multi_channel.blf`` + ``fixtures/demo.dbc``（Ch1 `VehSpd`/`BattTemp`、
    Ch2 `Insulation`），并把网格设成 10Hz，才能让 1Hz 的 `Res` 暴露出
    “超过最大保持时长”的空值（这正是用户场景）。
    """
    if core is None:
        print("FILL_SKIP no_kernel")
        return 0
    root = Path(__file__).resolve().parent.parent
    blf_path = (root / "fixtures" / "multi_channel.blf").resolve()
    dbc_path = (root / "fixtures" / "demo.dbc").resolve()
    absent = [str(p) for p in (blf_path, dbc_path) if not p.is_file()]
    if absent:
        print(f"FILL_SKIP no_fixture {' '.join(absent)}")
        return 0

    temp_root = Path(tempfile.mkdtemp(prefix="fblf_fill_selftest_"))
    dbc_map = {1: str(dbc_path), 2: str(dbc_path)}

    def run(label: str, resample_hz, fill_policy: str,
            gap_ms: float | None = None,
            time_format: str = "relative_s") -> dict[str, list[float | None]]:
        out_dir = temp_root / label
        config = {
            "blf_paths": [str(blf_path)],
            "dbc_by_channel": dbc_map,
            "selected": {},
            "split_by_message": False,
            "resample_hz": resample_hz,
            "export_format": "csv",
            "out_dir": str(out_dir),
            "temp_dir": str(out_dir / ".tmp"),
            "excel_row_limit": 1_000_000,
            "memory_budget_mb": 512,
            "time_format": time_format,
            "fill_policy": fill_policy,
        }
        if gap_ms is not None:
            config["ffill_max_gap_ms"] = float(gap_ms)
        report = core.convert(config)
        if not report or not report.get("ok"):
            raise AssertionError(f"{label} 转换未成功：{report!r}")
        return _read_fill_csv(report)

    try:
        # 原始采样来源：用 `unix_s` 保留 epoch 级完整 f64 精度，便于复刻 Rust 内部
        # 「先减 global_min 再做线性插值」的时间轴（避免 CSV 相对时间取整带来的 1e-7 噪声）。
        raw = run("raw_none", None, "none", time_format="unix_s")
        hold = run("grid_hold", 10.0, "hold")
        linear = run("grid_linear", 10.0, "linear")
        gapped = run("grid_hold_gap", 10.0, "hold", gap_ms=50.0)

        signals = [name for name in raw if name != "timestamp"]
        if not signals:
            raise AssertionError("原始宽表没有任何信号列")
        if len(raw["timestamp"]) <= len(hold["timestamp"]):
            raise AssertionError("原始时序行数应多于 10Hz 网格行数")
        global_min = float(raw["timestamp"][0])  # 宽表按时间升序，首行即全局最小
        internal_times = [t - global_min for t in raw["timestamp"]]

        # 1) hold：所有非空取值必须来自该信号的真实采样值集合。
        for name in signals:
            original = {value for value in raw[name] if value is not None}
            if not original:
                continue
            for value in hold[name]:
                if value is not None and value not in original:
                    raise AssertionError(
                        f"hold 在 {name} 造出原始采样集合之外的值：{value}")

        # 2) linear：网格点数量与 hold 相同，且取值与线性插值期望一致（1e-9）。
        if len(linear["timestamp"]) != len(hold["timestamp"]):
            raise AssertionError("linear 与 hold 的网格点数量不一致")
        step = 1.0 / 10.0
        grid = [i * step for i in range(len(linear["timestamp"]))]
        for name in signals:
            pairs = [(t, v) for t, v in zip(internal_times, raw[name]) if v is not None]
            if not pairs:
                continue
            times = [t for t, _v in pairs]
            values = [v for _t, v in pairs]
            expected = _linear_expected(times, values, grid)
            for idx, (actual, want) in enumerate(zip(linear[name], expected)):
                if actual is None:
                    raise AssertionError(f"linear 的 {name} 第 {idx} 行不应为空")
                if abs(actual - want) > 1e-9:
                    raise AssertionError(
                        f"linear 的 {name} 第 {idx} 行={actual}，期望 {want}")

        # 3) hold + gap=50ms：1Hz 的 Res 必须出现超限空值。
        gapped_nulls = sum(1 for row in gapped["Res"] if row is None)
        if gapped_nulls <= 0:
            raise AssertionError("hold + ffill_max_gap_ms=50 未出现任何超限空值")

        # 4) none（原时序宽表）：仍有空值。
        none_nulls = sum(1 for row in raw["Res"] if row is None)
        if none_nulls <= 0:
            raise AssertionError("原时序宽表 none 模式不应完全无空值")

        hold_distinct = len({v for name in signals for v in hold[name] if v is not None})
        linear_distinct = len({v for name in signals for v in linear[name] if v is not None})
    except Exception as exc:  # noqa: BLE001 - 自检要把异常变成可 grep 的失败行
        print(f"FILL_FAIL {type(exc).__name__}: {exc}")
        return 2
    finally:
        shutil.rmtree(temp_root, ignore_errors=True)

    print(f"FILL_OK hold_distinct={hold_distinct} linear_distinct={linear_distinct} "
          f"gapped_nulls={gapped_nulls} none_nulls={none_nulls}")
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


def run_selftest_progress_monotonic() -> int:
    """``--selftest-progress-monotonic``：真实转换的进度条单调性验收（Step 19 A2）。

    对 ``multi_channel.blf`` + ``demo.dbc`` 真跑 CSV / Parquet 两次转换，在
    ``progress_hook`` 里记录每次回调后的 ``progress_bar.value()``，断言：

    * 整条进度序列单调不减（phase 1 → phase 2 切换处也不回退）；
    * 终值 100 且 100 只在序列末尾出现一次；
    * phase 2 的起点不低于 phase 1 的峰值（加权映射：70% 交界）。

    成功打印 ``MONO_OK phase1_max=.. phase2_start=.. final=100 seq_len=..``，返回 0。
    """
    if core is None:
        print("MONO_SKIP no_kernel")
        return 0
    root = Path(__file__).resolve().parent.parent
    blf_path = (root / "fixtures" / "multi_channel.blf").resolve()
    dbc_path = (root / "fixtures" / "demo.dbc").resolve()
    absent = [str(p) for p in (blf_path, dbc_path) if not p.is_file()]
    if absent:
        print(f"MONO_SKIP no_fixture {' '.join(absent)}")
        return 0
    app = QApplication.instance()
    if app is None:
        print("MONO_FAIL no_qapplication")
        return 2
    window: MainWindow | None = None
    temp_dirs: list[Path] = []
    phase1_max = 0
    phase2_start = 100
    seq_len = 0

    def _run(export_format: str) -> tuple[list[tuple[int, int]], dict | None, str | None]:
        out_dir = Path(tempfile.mkdtemp(prefix=f"fblf_mono_{export_format}_"))
        temp_dirs.append(out_dir)
        index = next((i for i, (_label, value) in enumerate(FORMAT_CHOICES)
                      if value == export_format), 0)
        window.combo_format.setCurrentIndex(index)
        sequence: list[tuple[int, int]] = []  # (进度条值, payload.phase)

        def hook(payload: dict) -> None:
            sequence.append((int(window.progress_bar.value()),
                             int(payload.get("phase", 1) or 1)))

        window.progress_hook = hook
        try:
            window.start_convert(str(out_dir))
            if not _pump_convert(app, window):
                raise AssertionError(f"{export_format} 转换超时")
        finally:
            window.progress_hook = None
        # 成功终态由 _finish_progress_ok 写入 100；补记最后一格（不属于 progress_cb）。
        sequence.append((int(window.progress_bar.value()), 2))
        return sequence, window.last_report, window.last_error

    try:
        window = MainWindow()
        window.quiet_dialogs = True
        _setup_scan_tree(window, str(blf_path), {1: str(dbc_path), 2: str(dbc_path)})
        for export_format in ("csv", "parquet"):
            sequence, report, error = _run(export_format)
            if error is not None:
                raise AssertionError(f"{export_format} 转换失败：{error}")
            if not report or not report.get("ok"):
                raise AssertionError(f"{export_format} 未返回 ok=True 的报告")
            values = [value for value, _phase in sequence]
            for previous, current in zip(values, values[1:]):
                if current < previous:
                    raise AssertionError(
                        f"{export_format} 进度回退：{values}")
            if not values or values[-1] != 100:
                raise AssertionError(f"{export_format} 终值不是 100：{values}")
            if values.count(100) != 1 or values.index(100) != len(values) - 1:
                raise AssertionError(f"{export_format} 100 出现在结束前：{values}")
            run_phase1 = [value for value, phase in sequence if phase == 1]
            run_phase2 = [value for value, phase in sequence if phase == 2]
            if not run_phase1:
                raise AssertionError(f"{export_format} 没收到 phase 1 回调")
            if not run_phase2:
                raise AssertionError(f"{export_format} 没收到 phase 2 回调")
            run_phase1_max = max(run_phase1)
            run_phase2_start = run_phase2[0]
            if run_phase2_start < run_phase1_max:
                raise AssertionError(
                    f"{export_format} phase 切换回退：phase1_max={run_phase1_max} "
                    f"phase2_start={run_phase2_start} seq={values}")
            phase1_max = max(phase1_max, run_phase1_max)
            phase2_start = min(phase2_start, run_phase2_start)
            seq_len += len(values)
            print(f"MONO_RUN export={export_format} phase1_max={run_phase1_max} "
                  f"phase2_start={run_phase2_start} final={values[-1]} "
                  f"seq_len={len(values)}", flush=True)
    except Exception as exc:  # noqa: BLE001 - 自检要把任何异常变成可 grep 的失败行
        print(f"MONO_FAIL {type(exc).__name__}: {exc}")
        return 2
    finally:
        if window is not None:
            _destroy_window(window)
        for path in temp_dirs:
            shutil.rmtree(path, ignore_errors=True)
    print(f"MONO_OK phase1_max={phase1_max} phase2_start={phase2_start} "
          f"final=100 seq_len={seq_len}")
    return 0


def _write_bulk_dbc(path: Path, message_specs: list[tuple[int, str]],
                    per_message: int) -> int:
    """生成一个含 ``len(message_specs) * per_message`` 个 1-bit 信号的临时 DBC。

    ``multi_channel.blf`` 只有 3 个报文 ID；把 96 个信号平铺到 3 个报文上即可
    验证三态复选框的 96/96 口径，不需要新夹具。
    """
    lines = ['VERSION ""', "", "NS_ :", "", "BS_:", "", "BU_: ECU", ""]
    count = 0
    for message_id, message_name in message_specs:
        lines.append(f"BO_ {message_id} {message_name}: 8 ECU")
        prefix = "".join(ch for ch in message_name if ch.isalnum()) or "S"
        for index in range(per_message):
            start_bit = (index * 2) % 64
            lines.append(f' SG_ {prefix}_{index} : {start_bit}|1@1+ (1,0) [0|1] "" ECU')
            count += 1
        lines.append("")
    path.write_text("\n".join(lines), encoding="utf-8")
    return count


def _percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = int(round((len(ordered) - 1) * fraction))
    return float(ordered[max(0, min(len(ordered) - 1, index))])


def _render_canvas_ms(canvas, width: int | None = None, height: int | None = None,
                      *, region: bool = False) -> float:
    """离屏渲染画布一帧，返回毫秒（自检用；不修改任何控件状态）。

    ``region=True`` 时只渲染左上角 ``width×height`` 区域，等价于滚动视口里
    实际会重绘的那一块（车道多行时用它证明绘制量与总行数无关）。
    """
    from PySide6.QtCore import QPoint, QRect
    from PySide6.QtGui import QColor, QImage, QPainter

    w = int(width or max(1, canvas.width()))
    h = int(height or max(1, canvas.height()))
    image = QImage(w, h, QImage.Format.Format_ARGB32)
    image.fill(QColor("#1E1E24"))
    started = time.perf_counter()
    painter = QPainter(image)
    if region:
        canvas.render(painter, QPoint(0, 0), QRect(0, 0, w, h))
    else:
        canvas.render(painter, QPoint(0, 0))
    painter.end()
    return (time.perf_counter() - started) * 1000.0


def _destroy_widget(widget) -> None:
    """显式销毁独立 QWidget（预览窗 / 错误横幅窗），避免关闭顺序导致段错误。"""
    if widget is None:
        return
    try:
        widget.close()
        widget.deleteLater()
        app = QApplication.instance()
        if app is not None:
            app.processEvents()
    except RuntimeError:
        pass


def run_selftest_plot() -> int:
    """``--selftest-plot``：Step 18 预览重写的无人值守验收。

    断言（全部真跑，失败打印 ``PLOT_FAIL`` 并返回 2）：

    * ``multi_channel.blf`` + ``demo.dbc`` 开的 PlotSession 能构造预览窗口（offscreen）；
    * 三种显示模式各切一次 + 时间口径 4 种各切一次，``view()`` 调用次数不增加；
    * 三态复选框：96/96 → Checked；0/96 → Unchecked；1/96 → PartiallyChecked；
    * 每信号可见点数 ≤ 2 × 画布宽度；
    * 离屏渲染一帧 ≤ 30 ms（打印实测）；
    * ``CACHE_BUDGET_EXCEEDED`` 错误窗有内联横幅 + 三个动作按钮（无模态弹窗）。
    """
    if core is None:
        print("PLOT_SKIP no_kernel")
        return 0
    root = Path(__file__).resolve().parent.parent
    blf_path = (root / "fixtures" / "multi_channel.blf").resolve()
    dbc_path = (root / "fixtures" / "demo.dbc").resolve()
    absent = [str(p) for p in (blf_path, dbc_path) if not p.is_file()]
    if absent:
        print(f"PLOT_SKIP no_fixture {' '.join(absent)}")
        return 0
    app = QApplication.instance()
    if app is None:
        print("PLOT_FAIL no_qapplication")
        return 2

    temp_root = Path(tempfile.mkdtemp(prefix="fblf_plot_selftest_"))
    os.environ["FBLF_CONFIG_DIR"] = str(temp_root / "configs")
    session = bulk_session = None
    preview = bulk_preview = error_window = None
    try:
        config = {
            "blf_paths": [str(blf_path)],
            "dbc_by_channel": {1: str(dbc_path), 2: str(dbc_path)},
            "selected": {"1": {"VehSpd": ["Spd", "SpdValid"],
                               "BattTemp": ["Temp1", "Temp2"]},
                         "2": {"Insulation": ["Res"]}},
            "time_format": "relative_s",
            "temp_dir": str(temp_root),
            "cache_dir": str(temp_root / "cache"),
            "cache_budget_mb": 200,
        }
        # 0) A4：打开会话必须在 Worker 线程里带确定进度（progress_cb）。
        progress_config = dict(config)
        progress_config["cache_dir"] = str(temp_root / "progress_cache")
        progress_config["selected"] = {"1": {"VehSpd": ["Spd"]}}
        progress_events: list[dict] = []
        progress_result: dict = {}
        progress_worker = Worker("session", config=progress_config,
                                 initial_keys=["ch1/VehSpd/Spd"])
        progress_worker.progress.connect(lambda p: progress_events.append(dict(p)))
        progress_worker.finished_ok.connect(lambda r: progress_result.update(r))
        progress_worker.failed.connect(lambda m: progress_result.update({"failed": m}))
        progress_worker.start()
        deadline = time.time() + 20
        while time.time() < deadline and not progress_result:
            app.processEvents()
            time.sleep(0.01)
        if not progress_worker.wait(5000):
            raise AssertionError("会话进度 Worker 未在 5s 内结束")
        progress_worker.deleteLater()
        if progress_result.get("failed"):
            raise AssertionError(f"会话进度自检失败：{progress_result['failed']}")
        if progress_result.get("session") is not None:
            progress_result["session"].close()
        if not progress_events:
            raise AssertionError("open_plot_session 没有发出 progress_cb")
        if not all(int(item.get("done") or 0) <= int(item.get("total") or 0)
                   for item in progress_events):
            raise AssertionError(f"进度非确定：{progress_events!r}")
        if int(progress_events[-1].get("done") or 0) != int(
                progress_events[-1].get("total") or 0):
            raise AssertionError(f"进度未停在 100%：{progress_events[-1]!r}")
        progress_frames = max(int(item.get("frames_done") or 0)
                              for item in progress_events)

        session = core.open_plot_session(config)
        info = session.info()
        # 只看本次 selected 里已缓存的 5 条真实信号（ch1 四条 + ch2 Insulation）。
        keys = [str(item.get("key")) for item in (info.get("signals") or [])
                if item.get("cached")]
        if len(keys) < 3:
            raise AssertionError(f"夹具信号不足：{keys}")
        keys = keys[:5]
        session.add_signals(keys)
        stats = session.stats(keys)

        preview = PreviewWindow(
            session, info=info, stats=stats, config=config, initial_keys=keys,
            time_format="relative_s", time_format_choices=TIME_FORMAT_CHOICES,
            cache_budget_mb=200)
        preview.resize(1200, 700)
        preview.show()
        app.processEvents()
        # 让「窗口变化 → 实际宽度精化」那一拍落地，再开始统计 view() 次数。
        time.sleep(0.08)
        app.processEvents()
        if preview.canvas.visible_count() > 3:
            raise AssertionError(f"首屏可见 {preview.canvas.visible_count()} 条 > 3")

        # 1) 三种模式 + 4 种时间口径：不得取数。
        calls_before = preview.canvas.view_call_count
        for mode in ("multi_axis", "single_axis", "lanes"):
            preview.apply_mode(mode)
            app.processEvents()
        for fmt in ("relative_ms", "absolute", "unix_s", "relative_s"):
            preview.apply_time_format(fmt)
            app.processEvents()
        calls_after = preview.canvas.view_call_count
        if calls_after != calls_before:
            raise AssertionError(
                f"切模式/口径触发了 view()：{calls_before} → {calls_after}")

        # 1b) 十字线移动只重绘，不得取数（A2）。
        from PySide6.QtCore import QPointF

        preview.canvas._hover_t = preview.canvas._view[0] + 0.01  # noqa: SLF001
        preview.canvas._hover_pos = QPointF(200, 200)  # noqa: SLF001
        preview.canvas.update()
        app.processEvents()
        if preview.canvas.view_call_count != calls_after:
            raise AssertionError("十字线移动触发了 view()")

        # 2) 三态：全选 / 全不选 / 只勾 1 条（96/96 用临时 96 信号 DBC 验证）。
        preview.select_all()
        app.processEvents()
        if preview.canvas.visible_count() != len(keys):
            raise AssertionError("全选后可见数不匹配")
        preview.select_none()
        app.processEvents()
        if preview.panel.check_state() != Qt.CheckState.Unchecked:
            raise AssertionError("全不选后复选框不是 Unchecked")

        # 3) 可见点数 ≤ 2 × width_px 每信号。
        preview.select_all()
        app.processEvents()
        width = max(1, preview.canvas.width())
        for key, points in preview.visible_points().items():
            if points > 2 * width:
                raise AssertionError(f"{key} 可见点数 {points} > 2×{width}")
        max_points = max(preview.visible_points().values() or [0])

        # 4) 离屏渲染一帧 ≤ 30 ms。
        render_ms = _render_canvas_ms(preview.canvas)
        if render_ms > 30.0:
            raise AssertionError(f"离屏渲染 {render_ms:.2f} ms > 30 ms")

        # 5) 96/96：临时 DBC 把 96 个信号平铺到夹具的 3 个报文 ID 上。
        bulk_dbc = temp_root / "bulk96.dbc"
        bulk_count = _write_bulk_dbc(
            bulk_dbc, [(291, "BulkA"), (418, "BulkB"), (512, "BulkC")], 32)
        bulk_config = {
            "blf_paths": [str(blf_path)],
            "dbc_by_channel": {1: str(bulk_dbc), 2: str(bulk_dbc)},
            "selected": {
                "1": {"BulkA": [f"BulkA_{i}" for i in range(32)],
                      "BulkB": [f"BulkB_{i}" for i in range(32)]},
                "2": {"BulkC": [f"BulkC_{i}" for i in range(32)]},
            },
            "time_format": "relative_s",
            "temp_dir": str(temp_root / "bulk_temp"),
            "cache_dir": str(temp_root / "bulk_cache"),
            "cache_budget_mb": 200,
        }
        bulk_session = core.open_plot_session(bulk_config)
        bulk_info = bulk_session.info()
        wanted_bulk: set[str] = set()
        for channel, messages in bulk_config["selected"].items():
            for message, signals in messages.items():
                for signal in signals:
                    wanted_bulk.add(f"ch{channel}/{message}/{signal}")
        bulk_keys = [str(item.get("key")) for item in (bulk_info.get("signals") or [])
                     if str(item.get("key")) in wanted_bulk]
        if len(bulk_keys) != bulk_count:
            raise AssertionError(f"临时 DBC 选中信号数 {len(bulk_keys)} != {bulk_count}")
        bulk_session.add_signals(bulk_keys)
        bulk_stats = bulk_session.stats(bulk_keys)
        bulk_preview = PreviewWindow(
            bulk_session, info=bulk_info, stats=bulk_stats, config=bulk_config,
            initial_keys=bulk_keys, time_format="relative_s",
            time_format_choices=TIME_FORMAT_CHOICES, cache_budget_mb=200)
        bulk_preview.resize(1000, 600)
        bulk_preview.show()
        app.processEvents()
        bulk_preview.select_all()
        app.processEvents()
        if bulk_preview.panel.checked_count() != 96:
            raise AssertionError(f"全选 {bulk_preview.panel.checked_count()}/96")
        if bulk_preview.panel.check_state() != Qt.CheckState.Checked:
            raise AssertionError("全选后复选框不是 Checked")
        if "96/96" not in bulk_preview.panel.chk_all.text():
            raise AssertionError(f"全选文案={bulk_preview.panel.chk_all.text()!r}")
        tri_text = bulk_preview.panel.chk_all.text()
        bulk_preview.select_none()
        app.processEvents()
        if bulk_preview.panel.check_state() != Qt.CheckState.Unchecked:
            raise AssertionError("96 全不选后复选框不是 Unchecked")
        bulk_preview.set_signal_visible(bulk_keys[0], True)
        app.processEvents()
        if bulk_preview.panel.check_state() != Qt.CheckState.PartiallyChecked:
            raise AssertionError("只勾 1 条后复选框不是 PartiallyChecked")
        # 96 条全选后只重绘视口（~13 行）也应该 ≤30ms，绘制量与总行数无关。
        bulk_preview.select_all()
        app.processEvents()
        viewport = bulk_preview.scroll.viewport().size()
        render96_ms = _render_canvas_ms(
            bulk_preview.canvas, viewport.width(), viewport.height(), region=True)
        if render96_ms > 30.0:
            raise AssertionError(f"96 车道视口渲染 {render96_ms:.2f} ms > 30 ms")

        # 6) F1：缓存超额内联横幅 + 三个动作按钮（绝不是 QMessageBox）。
        error_window = PreviewWindow(
            error_message="CACHE_BUDGET_EXCEEDED: 需要 900 MiB，上限 200 MiB；"
                          "可选：①提高上限 ②减少所选报文 ③不建缓存继续",
            error_code="CACHE_BUDGET_EXCEEDED", cache_budget_mb=200,
            time_format="relative_s", time_format_choices=TIME_FORMAT_CHOICES)
        error_window.resize(960, 320)
        error_window.show()
        app.processEvents()
        if not error_window.banner.isVisible():
            raise AssertionError("缓存超额时内联横幅未显示")
        texts = (error_window.btn_raise.text(), error_window.btn_reduce.text(),
                 error_window.btn_nocache.text())
        if texts != ("提高上限并重试", "只预览更少报文", "不建缓存继续（较慢）"):
            raise AssertionError(f"F1 动作按钮文案={texts!r}")

        # 7) 口径切换的 p95（E2 ≤16ms；会话模式不取数）。
        fmt_samples: list[float] = []
        for index in range(20):
            fmt = "relative_ms" if index % 2 == 0 else "relative_s"
            started = time.perf_counter()
            preview.apply_time_format(fmt)
            fmt_samples.append((time.perf_counter() - started) * 1000.0)
        fmt_p95 = _percentile(fmt_samples, 0.95)
        if fmt_p95 > 16.0:
            raise AssertionError(f"set_time_format p95={fmt_p95:.2f} ms > 16 ms")
    except Exception as exc:  # noqa: BLE001 - 自检要把任何异常变成可 grep 的失败行
        print(f"PLOT_FAIL {type(exc).__name__}: {exc}")
        return 2
    finally:
        _destroy_widget(error_window)
        _destroy_widget(bulk_preview)
        _destroy_widget(preview)
        if bulk_session is not None:
            try:
                bulk_session.close()
            except Exception:  # noqa: BLE001
                pass
        if session is not None:
            try:
                session.close()
            except Exception:  # noqa: BLE001
                pass
    print(f"PLOT_OK series={len(keys)} modes=3 time_formats=4 "
          f"view_calls_mode_fmt=0 tri=96/96 ({tri_text}) "
          f"progress_events={len(progress_events)} progress_frames={progress_frames} "
          f"max_points_per_signal={max_points} width={width} "
          f"render_ms={render_ms:.2f} render96_viewport_ms={render96_ms:.2f} "
          f"fmt_switch_p95_ms={fmt_p95:.3f} f1_buttons=3")
    return 0


def run_selftest_plot_perf() -> int:
    """``--selftest-plot-perf``：8 个 7MB 切片上的缩放/平移单帧与切口径实测。

    * 单帧耗时 = 粗粒度 ``view(..., 400)`` + 一次离屏 paint（拖拽期间的真实路径）；
    * 打印 p50/p95；``set_time_format`` 20 次的 p95；成功打印 ``PLOT_PERF`` 返回 0。
    """
    if core is None:
        print("PLOT_PERF_SKIP no_kernel")
        return 0
    root = Path(__file__).resolve().parent.parent
    source = (root / "fixtures" / "bench_large.blf").resolve()
    dbc = (root / "fixtures" / "bench.dbc").resolve()
    if not source.is_file() or not dbc.is_file():
        print("PLOT_PERF_SKIP no_fixture")
        return 0
    app = QApplication.instance()
    if app is None:
        print("PLOT_PERF_FAIL no_qapplication")
        return 2
    temp_root = Path(tempfile.mkdtemp(prefix="fblf_plot_perf_"))
    os.environ["FBLF_CONFIG_DIR"] = str(temp_root / "configs")
    session = preview = None
    try:
        slices: list[str] = []
        for index in range(8):
            target = temp_root / f"slice_{index}.blf"
            shutil.copyfile(source, target)
            slices.append(str(target))
        config = {
            "blf_paths": slices,
            "dbc_by_channel": {1: str(dbc), 2: str(dbc), 3: str(dbc)},
            "selected": {},
            "time_format": "relative_s",
            "temp_dir": str(temp_root / "temp"),
            "cache_dir": str(temp_root / "cache"),
            "cache_budget_mb": 1024,
        }
        opened = time.perf_counter()
        session = core.open_plot_session(config)
        open_ms = (time.perf_counter() - opened) * 1000.0
        info = session.info()
        keys = [str(item.get("key")) for item in (info.get("signals") or [])
                if item.get("cached")][:3]
        if not keys:
            raise AssertionError("性能夹具没有已缓存信号")
        session.add_signals(keys)
        stats = session.stats(keys)
        preview = PreviewWindow(
            session, info=info, stats=stats, config=config, initial_keys=keys,
            time_format="relative_s", time_format_choices=TIME_FORMAT_CHOICES,
            cache_budget_mb=1024)
        preview.resize(1200, 700)
        preview.show()
        app.processEvents()
        canvas = preview.canvas

        frame_samples: list[float] = []
        for index in range(50):
            factor = 0.8 if index % 2 == 0 else 1.25
            started = time.perf_counter()
            canvas.zoom_at(factor, canvas.width() * 0.6, fetch=True, coarse=True)
            _render_canvas_ms(canvas)
            frame_samples.append((time.perf_counter() - started) * 1000.0)
        for index in range(50):
            delta = 12.0 if index % 2 == 0 else -12.0
            started = time.perf_counter()
            canvas.pan_pixels(delta, fetch=True, coarse=True)
            _render_canvas_ms(canvas)
            frame_samples.append((time.perf_counter() - started) * 1000.0)

        fmt_samples: list[float] = []
        for index in range(20):
            fmt = "relative_ms" if index % 2 == 0 else "relative_s"
            started = time.perf_counter()
            preview.apply_time_format(fmt)
            fmt_samples.append((time.perf_counter() - started) * 1000.0)
        p50 = _percentile(frame_samples, 0.50)
        p95 = _percentile(frame_samples, 0.95)
        fmt_p95 = _percentile(fmt_samples, 0.95)
        if fmt_p95 > 16.0:
            raise AssertionError(f"set_time_format p95={fmt_p95:.2f} ms > 16 ms")

        # 关掉首次会话再开一次：同 key 的缓存命中应远低于 300 ms（A4）。
        _destroy_widget(preview)
        preview = None
        if session is not None:
            session.close()
            session = None
        cache_started = time.perf_counter()
        session = core.open_plot_session(config)
        cache_open_ms = (time.perf_counter() - cache_started) * 1000.0
        second_info = session.info()
        cache_hit_second = bool(second_info.get("cache_hit"))
        if not cache_hit_second:
            raise AssertionError("第二次 open_plot_session 未命中磁盘缓存")
        if cache_open_ms > 300.0:
            raise AssertionError(f"缓存命中打开 {cache_open_ms:.1f} ms > 300 ms")
    except Exception as exc:  # noqa: BLE001
        print(f"PLOT_PERF_FAIL {type(exc).__name__}: {exc}")
        return 2
    finally:
        _destroy_widget(preview)
        if session is not None:
            try:
                session.close()
            except Exception:  # noqa: BLE001
                pass
        shutil.rmtree(temp_root, ignore_errors=True)
    print(f"PLOT_PERF slices=8 frame_n={len(frame_samples)} p50_ms={p50:.3f} "
          f"p95_ms={p95:.3f} fmt_n={len(fmt_samples)} fmt_p95_ms={fmt_p95:.3f} "
          f"signals={len(info.get('signals') or [])} cached_keys={len(keys)} "
          f"open_ms={open_ms:.1f} cache_hit={bool(info.get('cache_hit'))} "
          f"cache_open_ms={cache_open_ms:.1f} cache_hit_second={cache_hit_second}")
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
    # Step 22：第一步就把日志落盘（早于 QApplication）：窗口化 exe 下 stderr 为 None，
    # 之后的任何异常 / 警告只有写进文件才可能被事后排查。
    try:
        setup_logging()
    except OSError as exc:
        logging.getLogger(LOGGER_NAME).warning("[GUI] 日志初始化失败：%s", exc)
    args = list(sys.argv if argv is None else argv)
    selftest = "--selftest" in args[1:]
    deep = "--selftest-deep" in args[1:]
    config_test = "--selftest-config" in args[1:]
    monotonic_test = "--selftest-progress-monotonic" in args[1:]
    progress_test = "--selftest-progress" in args[1:]
    parse_test = "--selftest-parse" in args[1:]
    fill_test = "--selftest-fill" in args[1:]
    plot_test = "--selftest-plot" in args[1:]
    plot_perf = "--selftest-plot-perf" in args[1:]
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
    if fill_test:
        return run_selftest_fill()
    if monotonic_test:
        return run_selftest_progress_monotonic()
    if progress_test:
        return run_selftest_progress()
    if parse_test:
        return run_selftest_parse()
    if plot_perf:
        return run_selftest_plot_perf()
    if plot_test:
        return run_selftest_plot()
    if deep:
        return run_selftest_deep()
    if dump_target is not None:
        return run_dump_check_states(dump_target)
    window = build_main_window()
    logger.info("[GUI] 内核就绪：%s；版本：%s", '是' if KERNEL_READY else '否',
                core.version() if KERNEL_READY else '—')
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
    shutdown_logging()
    os._exit(int(code))


if __name__ == "__main__":
    raise SystemExit(main())
