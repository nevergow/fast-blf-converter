# Step 6a 任务书（信号树 + DBC 交互改造）—— 交给 DSH headless 执行

你是 PySide6 GUI 工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先完整读：
- `docs/SPEC.md` §3.C（GUI 需求基线）
- `docs/INTERFACE.md` §1（`scan` 的返回结构：**含新增的 `dbc_error` 字段语义**；`blf_paths` 允许空列表）
- `gui/main.py`（现有实现，约 960 行；本次在其上改造）
- `fixtures/README.md`、`fixtures/demo.dbc`、`fixtures/multi_channel.blf`（**自检要用的真实夹具**）

## 本次只做 Step 6a，只允许改这两个文件

`gui/main.py`、`gui/theme.qss`。

**不要动** `crates/`（另有一个并行任务在改 Rust 侧，动它会冲突）、`docs/`、`packaging/`、`.github/`。
也**不要**改 CHECKBOX 的绘制方式（Step 6b 会换成自定义 delegate）与配置管理（Step 6c）—— 本次只做下面 5 件事。

## 背景（真实用户反馈，逐条对应）

> "类似 canalyzer，在树状的 can 通道下增加 dbc 文件，而不是现在下拉某一个通道增加 dbc；而且不能删除单个通道 dbc，只能全部清除。"
> "增加完所有的 dbc 后，增加一个按钮，点击按钮再解析信号。"

## 1. 信号树层级改造：通道 → **DBC 节点** → 报文 → 信号

- 通道节点文本：`通道 Ch1`（DBC 解析失败时追加红字 `（DBC 解析失败）`）
- **DBC 子节点**（新增层级）：文本 `📄 demo.dbc   ✓ 已解析` 或 `📄 demo.dbc   ✗ 解析失败`；
  - 解析失败时该节点文字用**红色**（`setForeground(QColor("#E5534B"))`），`tooltip` 放完整原因（来自 `scan` 返回的 `dbc_error`）；
  - DBC 节点**不可勾选**（只作展示）：`flags` 去掉 `ItemIsUserCheckable`；
  - 通道节点的三态勾选由**其下所有报文**聚合（跨 DBC 聚合）。
- 报文节点：`名称  0x1A2 · 100.0ms  500 帧`（周期缺失时省略周期段）；信号节点：`名称 (单位)`。
- 没有 BLF / 没有观测帧时，`frame_count=0` 也照常显示（不要隐藏节点）。

## 2. 右键菜单：在通道节点上「📄 添加 DBC…」

- `QTreeView.setContextMenuPolicy(Qt.CustomContextMenu)` + `customContextMenuRequested` → `QMenu`
- 只在**通道节点**上弹出，菜单项 `📄 添加 DBC…` → `QFileDialog.getOpenFileName`（过滤 `*.dbc`）
- 选定后写入 `self.dbc_by_channel[通道号] = 绝对路径`，并**立即**在该通道下插入 DBC 节点（状态显示 `未解析`），**不自动扫描**
- **不提供**任何"移除单个 DBC / 删除该通道 DBC"的菜单项或按钮（用户明确要求：只能全部清除）

## 3. 工具栏改造

- **移除**：通道下拉框 + `🔗 绑定 DBC 到该通道` 按钮（用户要求不再用这种方式）
- **新增**：`🔍 解析信号`（主按钮，objectName 可用 `primaryButton` 或不加）、`🗑 全部清除 DBC`
- **保留**：`📂 批量添加 BLF`
- `全部清除 DBC`：清空 `self.dbc_by_channel` 并清空树里的 DBC 节点（需 `QMessageBox.question` 二次确认）

## 4. 「🔍 解析信号」的行为

- 收集当前 BLF 路径列表 + DBC 映射 → 调 `fast_blf_core.scan(blf_paths, dbc_by_channel, progress_cb)`
- **BLF 为空也允许**：此时只按 DBC 列出报文/信号（`frame_count=0`）
- 若既没有 BLF 也没有 DBC → 状态栏提示"请先添加 BLF 或绑定 DBC"，不弹窗
- **解析前记录用户当前勾选**（集合：`(通道, 报文名, 信号名)`），解析重建树后**按名字恢复勾选**；匹配不到的按默认规则（新报文默认全选）
- 解析结果里某个通道 `dbc_error` 非空 → 通道节点与 DBC 节点都显示红字 + tooltip（**绝不弹窗**，这是用户的明确要求）
- 解析期间：按钮禁用、状态栏显示"正在解析信号…"

## 5. 必须新增 `--selftest-deep`（让我能在无人值守下验证这条主流程）

`python gui/main.py --selftest-deep` 要**程序化**跑通：
1. 用 `fixtures/multi_channel.blf` + `fixtures/demo.dbc`（DBC 绑到 Ch1 与 Ch2）
2. 调 `fast_blf_core.scan(...)` 并用其结果 `_populate_signal_tree()`
3. 断言树里：出现 2 个通道节点、每通道有 1 个 DBC 节点、报文节点数量 ≥ 3（`VehSpd`/`BattTemp`/`Insulation`）
4. 断言 `_collect_dbc_by_channel()` 之类的取值函数能正确回读 `{1: "...demo.dbc", 2: "..."}`
5. 打印 `DEEP_OK channels=2 dbc_nodes=2 messages=3` 之类的一行摘要（**便于我用 grep 判定**）
6. 退出码 0；任何断言失败 → 打印 `DEEP_FAIL <原因>` 并退出码 2

要求：内核缺失（`fast_blf_core` 导入失败）时 `--selftest-deep` 打印 `DEEP_SKIP no_kernel` 并退出码 0（不要因此变红）。

## 6. 自检（必须跑，贴真实输出）

```
cd /Users/wuzeqin/Projects/fast_blf_converter
.venv/bin/python -m py_compile gui/main.py && echo "py_compile OK"
QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py --selftest; echo "selftest exit=$?"
QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py --selftest-deep; echo "deep exit=$?"
```
三条都必须符合预期（deep 的输出里要能看到那行摘要）。失败就修到通过为止。

## 7. 回复格式（只回复这些）

- 两个文件的行数、三条命令的真实输出、偏离说明（没有就写"无偏离"）。
