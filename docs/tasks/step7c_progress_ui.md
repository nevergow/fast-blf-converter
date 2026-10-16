# Step 7c 任务书（进度可视化重做 + 时间格式下拉）—— 交给 DSH headless 执行

你是 PySide6 GUI 工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先读：`docs/INTERFACE.md` §1.1（`time_format`）与 **§2（进度回调协议：新增 `frames_done`，工作线程 + 50ms 节流）**、`docs/SPEC.md` §3.C、`gui/main.py`（6a/6b/6c 之后的版本）。

## 背景（用户真实反馈，两条）

> 1. "转换时没看到进度条。"

根因（已定位）：进度条是塞在**状态栏右侧**的 260px 小控件（`addPermanentWidget`，默认隐藏），且解码开始先切成不确定态 —— 用户根本注意不到。
（内核侧已由 Step 7a 修好：解码期间每 ≥50ms 回调一次，带 `frames_done`。）

> 2. "可以选择导出的时间类型和 canalyzer 一样。"

内核侧已由 Step 7b 实现 4 种 `time_format`；本次把它接到 UI。

## 本次只做 Step 7c，允许改这两个文件

1. `gui/main.py`
2. `gui/theme.qss`（只加进度区的样式）

不要动 `gui/check_delegate.py`、`gui/presets.py`、`crates/`、`docs/`、`packaging/`。

## 1. 进度区重做（关键：必须显眼）

- **位置**：底部控制面板与状态栏之间的**通栏区域**（不是状态栏小控件）。废弃现在那个 `addPermanentWidget` 的小进度条。
- **外观**：进度条高 **18–22px**、圆角 9px、填充色沿用主色 `#0078D4`，槽色 `#2D2D38`；左侧一行状态文字（`#E8E8F0`），右侧一行百分比/耗时（等宽字体）。
- **文字要显示**（同一行内拼接，信息密度高但不啰嗦）：
  `Phase 1/2 · 解码中 · 3/8 文件 · slice_03.blf · 已 1,234,567 帧 · 87 MB/s · 已用 00:42`
  - 速率 = `frames_done` 差值 / 回调间隔时间（GUI 侧按时间差算，不要核心里算）
  - 文件字节总数已知（BLF 列表里有大小）→ 可显示 `412 MB / 1.2 GB`（用"已完成文件字节 + 当前文件按帧占比估算"即可，估算值要标 `≈`）
- **两种态**：`total > 0` 时确定性进度（显示百分比）；`total == 0` / 尚无回调时不确定态（busy），**但必须已经在动**，不能是空白。
- **可见性**：进度区**始终占据一行高度**（待命时显示灰字 `待命`），转换一开始就进入工作态 —— 不要用 `setVisible` 造成布局跳动。
- **结束**：成功后进度条满格 + 绿色 `✓ 完成：3 张表 · 用时 00:12 · 输出：<out_dir>`（保留到下次转换）；失败显示红色错误消息（**不弹窗**，与既有约定一致）。
- 回调 → UI 必须经 `Signal` 转发到主线程（不得在回调里碰控件）。

## 2. 时间列格式下拉（接到内核 `time_format`）

底部控制面板新增下拉框 `时间列格式`：

| 显示文字 | config 值 |
|---|---|
| `相对时间（秒）` | `relative_s`（默认选中） |
| `相对时间（毫秒）` | `relative_ms` |
| `绝对时间（日期 时间）` | `absolute` |
| `Unix 时间戳（秒）` | `unix_s` |

- tooltip 写明：`绝对时间 = BLF 文件头记录的墙钟时间（与 CANalyzer 显示一致）；相对时间以全部输入文件中最早的时间戳为 0`
- 转换时写进 config 的 `time_format` 字段（**这是必填项，不能漏**）
- 若内核返回 `BAD_TIME_HEADER`（文件头缺有效时间）→ 进度区红字提示，建议改用其它时间格式（不弹窗）

## 3. 必须交付 `--selftest-progress`（无人值守验证）

`QT_QPA_PLATFORM=offscreen python gui/main.py --selftest-progress` 要程序化跑：

1. 用 `fixtures/multi_channel.blf` + `fixtures/demo.dbc`（绑 Ch1/Ch2）自动填充 UI，选中全部信号
2. 注册一个记录器把**每个进度回调 payload 原样存下来**
3. 用 `relative_s` 真跑一次转换（输出到临时目录）：
   - 断言：phase 1 回调 **≥ 2** 次、phase 2 回调 **≥ 1** 次、`frames_done` 单调不减
   - 断言：CSV 首列首行是 `0`（relative_s 生效）
4. 切到 `absolute` 再跑一次：
   - 断言：CSV 首列首行匹配正则 `^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{6}$`
5. 若 `fixtures/bench_large.blf` 存在，再跑一次（单文件、relative_s）并额外断言 phase 1 回调 **≥ 5** 次、打印 `frames_done` 最大值（证明"解码期间确实持续在报进度"）；不存在则打印 `SKIP bench`
6. 打印一行摘要（便于我 grep）：
   `PROGRESS_OK phase1=<n> phase2=<n> frames_max=<n> timefmt_relative=ok timefmt_absolute=ok bench=<n|SKIP>`
7. 成功退出码 0；任何断言失败 → `PROGRESS_FAIL <原因>` + 退出码 2；内核缺失 → `PROGRESS_SKIP no_kernel` + 退出码 0

## 4. 自检（必须跑，贴真实输出）

```
cd /Users/wuzeqin/Projects/fast_blf_converter
.venv/bin/python -m py_compile gui/main.py && echo "py_compile OK"
for m in --selftest --selftest-deep --selftest-config --selftest-progress; do
  QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py $m; echo "$m exit=$?"
done
```
四条都要符合预期（`--selftest-progress` 必须打印 `PROGRESS_OK` 那行）。失败就修到通过。

## 5. 回复格式（只回复这些）

- 改动文件与行数、五条命令的真实输出、偏离说明（没有就写"无偏离"）。
