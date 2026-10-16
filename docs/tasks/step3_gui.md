# Step 3 任务书（GUI）—— 交给 DSH headless 执行

你是资深 PySide6 GUI 工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写，已确认）。

先完整读这两个文件：
- `docs/SPEC.md` §3.C（GUI 需求，含布局与文案）
- `docs/INTERFACE.md`（Rust 内核的 Python API 与回调协议 —— **必须严格对齐，不要自己发明接口名/字段名**）

## 本次只做 Step 3，只允许新建/修改这两个文件

`gui/theme.qss` 和 `gui/main.py`。不要动 `crates/`、`Cargo.toml`、`pyproject.toml`。

## 1. gui/theme.qss

暗色 Fluent Design 风格，必须覆盖这些选择器：`QWidget`、`QMainWindow`、`QToolBar`、`QPushButton`（含 `#primaryButton` 主按钮、`:hover`、`:pressed`、`:disabled`）、`QComboBox`（含 `::drop-down`、`QAbstractItemView`）、`QCheckBox`、`QTreeWidget`/`QTreeView`（含 `QTreeView::indicator` 三态复选框样式）、`QLineEdit`、`QProgressBar`（含 `::chunk` 渐变）、`QStatusBar`、`QSplitter::handle`、`QScrollBar`（vertical+horizontal 全套）、`QHeaderView::section`、`QMenu`、`QMessageBox`。

配色：底色 `#1E1E24`，面板 `#25252E`，卡片/输入框 `#2D2D38`，高亮与主按钮 `#0078D4`（hover `#1A88E0`，pressed `#006CBE`），主文字 `#E8E8F0`，次要文字 `#9A9AB0`，边框 `#3A3A46`，圆角 6–8px。
字体：`font-family: "Microsoft YaHei UI", "Segoe UI", "PingFang SC", sans-serif;`（Windows 为主目标）。
另外定义两个可复用的对象名样式：`#warnHint`（琥珀色 `#F0A020` 文字+边框，用于"建议重采样"提示）与 `#primaryButton`。

## 2. gui/main.py

单文件，600–900 行，结构清晰（类：`BlfFileTreeItem` 可省、`SignalFilterProxy`、`Worker`(QThread)、`MainWindow`、`main()`）。

必须实现：

a. **内核延迟导入 + 优雅降级**：启动时 `try: import fast_blf_core except Exception: core = None`。为 None 时状态栏显示「⚠ Rust 内核未就绪（请先 maturin develop）」，并把「🚀 启动多核极速转换」按钮 `setEnabled(False)`。**不允许因此崩溃** —— 这样 GUI 能在内核未编译时先被测试。

b. **顶部工具栏**：`📂 批量添加 BLF`（QFileDialog 多选 `*.blf`）、通道下拉 `Ch1`~`Ch8`、`🔗 绑定 DBC 到该通道`（QFileDialog 选 `*.dbc`，存进 `self.dbc_by_channel: dict[int, str]`，并在信号树里体现 DBC 基名）。

c. **中部 QSplitter 左右分栏**：
   - 左：BLF 文件列表树（列：文件名 / 大小 / 状态），状态取值 `待处理`、`解析中`、`✓ 已完成`、`✗ 失败`。**支持从资源管理器拖拽多个 .blf**（`setAcceptDrops(True)` + `dragEnterEvent`/`dropEvent` 接受 `event.mimeData().urls()` 并过滤 `.blf` 后缀）。
   - 右：信号筛选树，三级：`通道 → 报文 → 信号`。报文节点显示 `名称  0x1A2 · 100ms`，信号节点显示 `Volt_Cell1 (V)`。三级全部带复选框（`Qt.ItemIsUserCheckable | Qt.ItemIsAutoTristate`），父子勾选状态联动。顶部一行：搜索框（按报文名/信号名模糊过滤，大小写不敏感，过滤时隐藏不匹配节点）+ 按钮「全选当前过滤项」+「一键清空」。
   - 树的构建数据来自 `fast_blf_core.scan(blf_paths, dbc_by_channel, progress_cb)`，字段严格按 `docs/INTERFACE.md` §1（`channels` 的 key 是字符串通道号，值是含 `messages` 的 dict，依此类推）。

d. **底部控制面板**：复选框「按报文周期拆分为独立时序子表 (推荐，彻底规避空值)」（默认勾选）、重采样频率下拉（`保持原始时序` / `1 Hz` / `10 Hz` / `50 Hz` / `100 Hz`）、导出格式下拉（`CSV (通用表格)` / `Parquet (极速列存)` / `Excel (.xlsx 多Sheet)`）、主按钮 `🚀 启动多核极速转换`（objectName = `primaryButton`）。
   **联动**：取消勾选分表 → 重采样下拉高亮（`setProperty("warnHint")` + `style().unpolish/polish` 或直接 setStyleSheet），并显示一行 `#warnHint` 提示「合并为单张宽表时建议选择重采样频率，否则会因周期不同产生空值」。

e. **QThread 异步**：`Worker(QThread)` 里调用 `fast_blf_core.convert(config, progress_cb)`；`progress_cb` 在**工作线程**被调用，必须通过 `Signal(dict)` 发到主线程再更新 UI（严禁跨线程直接改控件）。进度条 + 状态文本按 `docs/INTERFACE.md` §2 显示，示例：`Phase 1/2: 正在并行解码 (12/50)...`、`Phase 2/2: 正在流式合并与导出...`。同时按 progress 的 `file` 字段把左侧对应文件状态改为 `解析中` / `✓ 已完成`。

f. **完成反馈**：`convert` 返回后 —— 若 `downgraded` 为真，`QMessageBox.warning` 显示 `downgrade_reason`（这是百万行防呆的唯一用户可见出口）；否则 `QMessageBox.information` 显示子表数量与总行数、耗时；状态栏显示输出路径（多表时显示输出目录）。

g. **`--selftest`**：构造主窗口 → `show()` → `QTimer.singleShot(400, app.quit)` → 退出码 0。**不得依赖 Rust 内核**（内核缺失时也要能跑通）。

h. 禁止 `TODO` / 占位实现 / 空函数。所有中文文案直接写死在代码里（不引 i18n）。

## 3. 自检（必须跑，并贴出真实输出）

```
cd /Users/wuzeqin/Projects/fast_blf_converter
.venv/bin/python -m py_compile gui/main.py && echo "py_compile OK"
QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py --selftest; echo "selftest exit=$?"
```
若第二条报错，必须修到 exit=0 为止（你可以反复改、反复跑）。

## 4. 回复格式（只回复这些，不要写总结文章）

- 两个文件各多少行
- 上述两条命令的真实输出（原样粘贴）
- 偏离本任务书的地方（没有就写"无偏离"）
