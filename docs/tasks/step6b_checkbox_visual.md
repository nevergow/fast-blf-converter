# Step 6b 任务书（复选框三态视觉 + 行高亮区分）—— 交给 DSH headless 执行

你是 PySide6 GUI 工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先读：`docs/SPEC.md` §3.C、`docs/INTERFACE.md` §1、`gui/main.py`、`gui/theme.qss`。

## 背景（用户原话，逐条对应）

> "在通道/报文/信号这个框里，选中框被选中时，是涂满蓝色的，这会导致两个问题：
> 1. 当选中这一行时，也是蓝色高亮显示，这会让选中框直接隐身；
> 2. 无论这个通道或者报文下面的信号是被全选了，还是选了其中某几个，上一级的选中框都是一样的样式，无法区分。"

## 本次只做 Step 6b，只允许改/新增这三个文件

1. `gui/main.py`（接线 + 新增命令行开关）
2. `gui/theme.qss`（行高亮色阶）
3. **新增** `gui/check_delegate.py`（自定义绘制，约 80–140 行）

不要动 `crates/`、`docs/`、`packaging/`、`.github/`。不要动树的层级结构（6a 刚改完）。

## 1. 自定义复选框绘制：`gui/check_delegate.py`

实现 `class CheckStateDelegate(QStyledItemDelegate)`，**只对第 0 列、且带 `Qt.ItemIsUserCheckable` 的项**生效：

| 状态 | 画法（务必一眼可分） |
|---|---|
| `Unchecked` | 14×14 圆角 3px 方框：底色 `#2D2D38`，1.5px 描边 `#5C5C6E`，**无内部标记** |
| `PartiallyChecked` | 同尺寸方框：底色 `#2D2D38`，1.5px 描边 `#0078D4`，中间一条 **8×3 圆角横条**（`#0078D4`） |
| `Checked` | **实心** `#0078D4` 圆角方框 + **白色对勾**（两段折线，2px、圆头端帽，`QPen` + `drawPolyline`/`QPainterPath`） |

要点：
- 勾选框**左侧留 6px**、与文字垂直居中对齐；`sizeHint` 保持基类行为（不要改变行高）
- 勾选框底色**不要**用行选中高亮的颜色（见下），避免"隐身"
- 悬停态可让描边提亮一档（可选）
- 禁用态整框 50% 透明
- **不要**再用 `theme.qss` 里的 `QTreeView::indicator` 图片方案（没有图片资源）；请在 `theme.qss` 中**删除或注释**可能与 delegate 冲突的 indicator 规则，并写明原因

## 2. 行高亮色阶（`gui/theme.qss`）

- `QTreeView::item:selected`：背景改为 `#33475B`（比主色 `#0078D4` 暗、比面板亮，与实心蓝勾**明确区分**），文字保持 `#E8E8F0`
- `QTreeView::item:hover`：更浅一档（如 `#2F3A48`）
- 选中行 + 悬停行都不要用纯 `#0078D4` 铺底

## 3. 状态聚合必须真的产生 `PartiallyChecked`

检查并修复：报文/通道节点是否在"子项部分选中"时**真的**是 `PartiallyChecked`（而不是被算成全选或全不选）。若现有 `_recalc_parent` 逻辑有偏差，一并修正（这是用户抱怨的第 2 点根因）。

## 4. 必须交付可视化证据：`--dump-check-states <png>`

新增命令行开关：离屏渲染一张 PNG（默认 `docs/screenshots/check-states.png`），内容为一个 `QTreeWidget`，含：

1. 三行演示三种状态：`☐ 未选`、`▣ 部分选（应为蓝框+横条）`、`☑ 全选（应为实心蓝+白勾）`
2. 三行**同时处于"行被选中"状态**（`setSelected(True)` + 焦点在该行），用来证明"选中行高亮下勾选框依然清晰可辨"
3. 行高亮用新的 `#33475B`

要求：PNG 宽度 420、高度自适应；渲染后打印 `DUMP_OK <路径>` 并退出码 0。
（这个开关是给我做视觉验收用的：我会直接看图判断"是否一眼可分、是否隐身"。）

## 5. 自检（必须跑，贴真实输出）

```
cd /Users/wuzeqin/Projects/fast_blf_converter
.venv/bin/python -m py_compile gui/main.py gui/check_delegate.py && echo "py_compile OK"
QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py --selftest; echo "selftest exit=$?"
QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py --selftest-deep; echo "deep exit=$?"
QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py --dump-check-states; echo "dump exit=$?"
```
四条都要符合预期；`--dump-check-states` 要真的生成 PNG（打印出路径与文件大小）。

## 6. 回复格式（只回复这些）

- 三个文件的行数、四条命令的真实输出、PNG 的绝对路径与大小、偏离说明（没有就写"无偏离"）。

## 7. 三条防坑（上一轮踩过，务必遵守）

1. **禁止设置 `QT_FATAL_WARNINGS=1`**。上一轮有执行者为了调试设了它，结果撞上一条**预先存在**的字体警告（`qt.qpa.fonts: ... missing font family "Microsoft YaHei UI"`，因为主题里写的是 Windows 字体、macOS 没有）→ 进程被 abort → 误判成"deep 路径不稳定"，钻进 lldb 兔子洞烧了十几分钟。**这不是你的 bug，不要为它改测试或改逻辑。**
   顺手修掉是对的：在 `theme.qss` 的全局字体声明里给出回退串，例如
   `font-family: "Microsoft YaHei UI", "Microsoft YaHei", "PingFang SC", "Segoe UI", sans-serif;`
   （Windows 上仍然命中 YaHei，macOS 上落到 PingFang，警告自然消失。）
2. **`QMessageBox` 在 `QT_QPA_PLATFORM=offscreen` 下会永久阻塞**（模态 `exec()` 等不到点击）。如果你的自检脚本会触发「确认/覆盖」类弹窗，必须先用 `QTimer.singleShot(...)` 自动点掉：
   ```python
   def _auto_accept():
       dlg = app.activeModalWidget()
       if dlg is None: return
       btn = getattr(dlg, "button", None)
       if btn is not None:
           yes = dlg.button(QMessageBox.Yes)
           if yes is not None: yes.click(); return
       dlg.accept()
   QTimer.singleShot(400, _auto_accept)
   ```
3. **`start_parse_signals` 已经改成"树里的绑定 ∪ `self.dbc_by_channel`"两份状态合并**（此前只有字典一份，导致"绑了 DBC 却解不出信号"）。你新增代码时不要在别处再写第三份状态；要读当前绑定请用 `self._collect_dbc_by_channel()`。

