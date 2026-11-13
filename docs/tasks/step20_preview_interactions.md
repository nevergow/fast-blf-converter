[Target]    /tmp/slice-plot  （git worktree，分支 slice/plot-fx，基点 b36b0a9）
[Executor]  Pi RPC（常驻会话 plotfx）
[Attempt]   1
[Anchor]    b36b0a9 · 派发前 `git status --porcelain` 必须为空

## 1. Goal

给预览窗口（`gui/plot_view.py`，QPainter 自绘）加 5 项能力，并删掉重复/解释性文案。
完成后可验证为真的状态：
- 滚轮在 X 轴带 / Y 轴带分别只缩放该轴；在其他区域保持现在的整体行为
- 右键拖动（或 Shift+左键拖动）在绘图区拉出选框，松手放大到所选范围
- 有 FIT X / FIT Y / FIT ALL 三个按钮，双击画布 = FIT ALL
- 画布下方有一条固定读数条，跟随光标实时显示 X（时间）与 Y 数值
- 指定的 12 处重复/解释性文案已删或缩短
- `--selftest-plot` 仍然通过（不许改 main.py 里的自检）

## 2. Context（已侦察好，不要再全库乱翻）

渲染技术：**QPainter 自绘**，没有 pyqtgraph、没有 QWebEngine。`gui/assets/echarts.min.js` 只用于导出 HTML 报告，与本切片无关。
`gui/plot_view.py` 共 2358 行。关键位置（已核实）：

- 视图状态：`self._view = [t0, t1]`（`plot_view.py:629`）、`_full`、`_cache`（当前窗口的 min/max 桶）、`_stats`（全文件 vmin/vmax）
- 取数：`_request_view(actual_width)`；`zoom_at(factor, x, fetch=..., coarse=...)`（自检 API，**签名必须向后兼容**，`main.py:4363` 在用它）
- 几何缓存键：`_geometry` 只由 `data_version`/`layout_version` 决定（`1260-1317`），改 Y 范围必须 `_invalidate_layout()`（bump layout_version），**不要** bump `data_version`
- 轴绘制：`_draw_axes`（X，`TICK_COUNT=5`，`TICK_COUNT` 在 `86`）、`_draw_lane_axis`（Y，固定 3 刻度，`1449`）、`_time_rect()`/`_overlay_rect()` 现算，**不要读 `_plot_rect`**（它只在 paint 时赋值，press 时可能是空 QRectF）
- 鼠标：`mousePressEvent`/`mouseMoveEvent`（`1612` 起，第一分支是"有 anchor 就是平移"）/`mouseReleaseEvent`/`mouseDoubleClickEvent`（`1645`，双击复位）/`wheelEvent`（`PlotScrollArea.wheelEvent` 在 `2354`，只读 `position().x()`，纵向滚动条会开启 → 坐标必须 `mapFrom(viewport, ...)`）
- 读数：`_draw_crosshair`（`1504`）+ `_draw_readout`（`1542`，浮动框，显示 `时间：…` + 每信号 mean(min~max)）
- Y 范围：`_series_range`（`1186`）返回全文件 vmin/vmax；`_y_range`（`1230` 附近）做并集与 `_finite`/`hi<=lo` 保护；`normalized` 轴（`1206-1211`，vmin/vmax 硬编码 0/100）**不允许**被覆盖
- 三种显示模式：`lanes`（每车道独立 Y）/`multi_axis`（按 unit 分轴）/`single_axis`（共享一根轴）——写 Y 覆盖时要按模式分别写满可见 key
- 面板控件：`controls` 行在 `1891-1901`
- `--selftest-plot`（在 `gui/main.py`，**不在你的白名单里**）会断言：`chk_all.text()` 含 `96/96`、错误页三个按钮文本、离屏单帧 ≤30ms、`view_call_count` 不涨

## 3. Scope & Boundaries

[Scope_Allow]
- gui/plot_view.py
- gui/theme.qss（只允许新增 FIT 按钮的样式选择器）

[Scope_Forbid] 其他所有文件，尤其 `gui/main.py`、`tests/`、`docs/`、`crates/`、依赖清单

[Invariants]
1. `zoom_at` 现有调用方式（`factor, x, fetch=, coarse=`）必须仍然可用；要加轴参数只能是 keyword-only 且带默认值
2. **Y 轴操作一律不得重新取数**：FIT Y、Y 滚轮缩放只改 Y 覆盖 + `_invalidate_layout()` + `update()`，不得调用 `_request_view` / 不得让 `view_call_count` 增加
3. 离屏单帧绘制仍 ≤30ms（`--selftest-plot` 会量）
4. 普通左键拖动仍然是平移（`docs/tasks/step16_plot_ui.md` / `step18_preview_rewrite.md` / `使用说明.txt` 把"按住拖动=平移"写成了用户契约，**不许改左键语义**）
5. 不新增任何依赖
6. `normalized` 轴的 0–100 语义不许被 Y 覆盖破坏
7. 框选期间**不许**动 `_view`（只画框），松手才提交——否则每次 mouseMove 重建全部 QPainterPath，直接破 30ms

## 4. 五项功能的具体要求

### A. 轴命中测试的滚轮单轴缩放
- X 轴带 = 时间轴区域（`_time_rect()` 附近，下缘约 24px 内）；命中时滚轮只缩放 X（anchor = 光标处时间，公式与现有 `zoom_at` 的 X 一致）
- Y 轴带 = 每根 Y 轴的绘制区域（`lanes` 模式每车道一根；`multi_axis` 每 unit 一根；`single_axis` 一根）；命中时只缩放**该轴**的 Y
- 其他区域：保持现在的行为
- 新增 `self._y_override: dict[str, tuple[float, float]]`（key 用该轴的 series key 或 axis id），在 `set_mode` / `set_time_format` / 新会话时必须清空
- Y 缩放 clamp：`hi-lo >= full_span*1e-6` 且 `<= full_span`；Y 缩到 span 很小时刻度要能自适应精度（现在是 `_fmt_num(value, 3)` 固定 3 位，会出现 3 个相同标签 —— 改成按 span 定精度的 nice-number 刻度）

### B. 选框放大
- 手势：**右键拖动** 或 **Shift+左键拖动**（左键单独仍是平移；右键双击原本不触发复位，安全）
- 起手后必须先判选框状态，再判平移 anchor，两个状态机不许互相打架
- 视觉：虚线框 + 半透明填充 `QColor(79,195,247,40)`，`setClipRect` 防溢出到轴带
- 松手：像素 → 数据域换算，X 必须排序；若 Y 方向跨度 > 4px 则同时写 Y 覆盖；`< 4px` 或 `< 2px²` 视为误触，取消
- 提交后 `_request_view(width)` 重新取数（X 变了，必须取）

### C. 删掉/缩短这些文案（只删这些，其余保留）
| 位置 | 现状 | 改成 |
|---|---|---|
| 1729 | `📈 数据预览 · PlotSession · min/max 包络` | `📈 数据预览` |
| 1803 | tooltip `纯显示层切换：只调 session.set_time_format()，不重新取数` | `切换时间显示方式` |
| 1811 | tooltip `车道 / 多 Y 轴叠加 / 单 Y 轴；切换只重算布局，不取数` | 删除 tooltip |
| 1995-1997 | tooltip `cache_bytes=… cache_hit=… 预算=… MiB` | 删除 tooltip |
| 2018 | `⚠ 量纲混杂，仅建议同量纲信号（单 Y 轴会把 km/h 与 degC 画到同一刻度）` | `⚠ 量纲混杂` |
| 2021 | `超出 4 个量纲，其余信号已归一化到 0–100%（见图例标注）` | `⚠ 超出 4 个量纲，已归一化到 0–100%` |
| 1777-1779 | `未建缓存（较慢）：本次为一次性抽稀数据，缩放 / 平移仍可用，但切换时间口径需要重新取数。` | `未建缓存：切换时间口径需重新取数` |
| 1854/1858/1862 | 三个 tooltip（`打开缓存上限设置，用更大的 cache_budget_mb 重试` 等） | 全删 |
| 410 | placeholder `按名字过滤…（不影响全选统计）` | `按名字过滤…` |
| 424 | `行首勾选 = 显示/隐藏；色块改颜色；右键改线型` | 删除这一行 hint |
| 309 | tooltip `点击修改曲线颜色（QColorDialog）` | `修改颜色` |
| 334-339 | tooltip 里的 `缓存：已缓存|未缓存（勾选后后台加载）/ 右键：折线 / 阶梯 / 散点` | 只保留 `通道 Ch… · 单位：…` |
| 1898-1900 | `滚轮缩放 · 左键拖动平移 · 双击复位 · 十字线读数不取数` | 改成 `滚轮缩放(轴内单轴) · 左键平移 · 右键框选 · 双击复位` |
| 1325/1328 | 两处重复写 `无可见信号` | 合并成一处 |
必须**保留**：错误页三个按钮及其文本（`提高上限并重试`/`只预览更少报文`/`不建缓存继续（较慢）`）、`chk_all.text()` 文案、动态状态 chip、轴刻度、车道标题。

### D. FIT 按钮
- 三个 `QPushButton`：`FIT X` / `FIT Y` / `FIT ALL`，`setObjectName("plotFitButton")`，插在 `chk_mean` 之后
- `fit_x()`：`_view = list(_full)` + `_prune_view()` + `_request_view(actual_width)`；**不清** Y 覆盖
- `fit_y()`：用 `_cache` 里当前窗口的 mins/maxs 求并集（`_cache` 空时回退 `_series_range`）→ 写 `_y_override` → `_invalidate_layout()`；**不取数**
- `fit_all()`：`fit_x()` + 清 `_y_override`
- `mouseDoubleClickEvent` 改成调用 `fit_all()`
- 常值信号（vmin==vmax）要有 `pad = max(1e-9, abs(v)*0.05)` 兜底，别显示 0..1
- `_y_range` 的并集逻辑必须认 `_y_override`（否则覆盖被吃掉）

### E. 画布下方的固定读数条
- 在画布下方（X 轴刻度下方）加一条固定高度（约 26px）的读数条，显示 `t = <时间>` 与各可见轴的 `值`（`lanes` 模式显示各车道值，`single_axis` 显示单值，`multi_axis` 按 unit 显示）
- 随光标实时更新；鼠标离开画布保留最后一次数值
- 数据源只能是 `_cache` + `_nearest_index`（不取数）
- 原来画布内的浮动读数框 `_draw_readout`：因为有了固定条，**可以删掉**（`_draw_crosshair` 十字线保留）；若删它导致任何引用断裂，就保留方法但不再调用
- 数值格式：时间用现成的 `format_tick_time` 口径；数值按量程给 3~6 位有效数字

## 5. Verification Contract（按顺序执行，全部要贴真实输出）

```bash
cd /tmp/slice-plot
python -m py_compile gui/plot_view.py                                    # 期望 exit 0
QT_QPA_PLATFORM=offscreen .venv_python gui/main.py --selftest           # 期望 exit 0
QT_QPA_PLATFORM=offscreen .venv_python gui/main.py --selftest-plot      # 期望 exit 0，输出含 PASS/OK 与 ≤30ms 的单帧数字
```
> `.venv_python` = 用主仓库的解释器：`/Users/wuzeqin/Projects/fast_blf_converter/.venv/bin/python`
> （worktree 里没有 .venv；`fast_blf_core` 已装在该解释器里）
> 若 `--selftest-plot` 因 **C 项删文案** 而失败，**不要改 main.py、不要伪造断言**：保留那一条文案并在回执里写明是哪一条，其余照做。

4) 反作弊自证：`git diff --name-only` 必须恰好是 `gui/plot_view.py`（+ 可选 `gui/theme.qss`）

## 6. Stop Conditions
- 需要改 `gui/main.py` 或白名单外文件 → 停，输出 `[ABORT_CONTRACT_CONFLICT]`
- 需要新增依赖 → 停
- 同一错误签名出现 2 次 → 停
- 超过 50 分钟 → 停并交当前进度

## 7. Executor Rules
1. 禁 `git commit/push/reset/clean`；禁改白名单外文件；禁改/删/跳过自检与测试；禁 `# type: ignore`、空 catch、写死返回值凑测试
2. 冲突即停，只输出：`[ABORT_CONTRACT_CONFLICT] <file:line> | <原因>`
3. 回执格式（缺项视为失败）：
```text
[STATUS]: DONE | ABORT_CONTRACT_CONFLICT
[TOUCHED_FILES]: <与 git diff --name-only 一致>
[VERIFY]:
  1) <cmd> -> exit <code>
     <关键原始输出 5-20 行>
  2) ...
[ARTIFACTS]: <绝对路径，没有写 none>
[NOTE]: <做了什么、跳过了什么、为什么>
```
