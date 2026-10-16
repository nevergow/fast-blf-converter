# Step 13：解析进度条真实化 + 通道节点粘性/可添加（用户实测反馈修复）

## 你只能改这些文件
* `gui/main.py`（主战场）
* `gui/theme.qss`（**仅在确有必要时**，例如新按钮需要样式；优先不动）

**禁止改**：`crates/**`（Rust 无需改动）、`docs/INTERFACE.md`（契约已由主会话冻结）、
`.github/workflows/**`、`tests/**`、`docs/tasks/**`。

## 背景：用户实测报的两个问题（原话）
1. 「在解析信号的时候，那个进度条，到了 100% 还没解析完，他就会退回去，然后又涨上去」
2. 「第一次进入，有很多个通道，当我例如给 1，2 号通道导入 dbc，并解析后，其他通道都消失了，不知道怎么增加通道」

## 根因（已由主会话用代码定位，照着修，不要另起炉灶）
1. `start_parse_signals()` 调的是 `self._begin_busy("Phase 1/2 · 扫描中…")`，而 `_begin_busy()` 是
   **不确定态乒乓动画**（`_busy_pos` 在 0→100→0 之间来回，`_busy_step=8`，200ms 一跳）。它跟真实进度无关，
   所以看起来"跑完了又回退"。**而 `scan` 的进度回调本来就携带真实文件级进度**
   （payload 字段：`phase/stage/done/total/file/frames_done/message`；`done/total` 是**文件级**计数，
   由 Rust `ProgressThrottle` 把每个文件的 0→1 映射成全局文件计数）。
2. `_seed_channels()` 在启动时预置 Ch1~Ch8；但 `_populate_signal_tree()` 会先 `removeRows` **整棵重建**，
   通道集合 = `channel_summary ∪ dbc_by_channel`。于是"没有帧观测、也没绑 DBC"的通道被丢掉，
   而且全程序**没有任何"添加通道"入口** → 用户被锁死（只剩已绑 DBC 的通道）。
   **Rust 侧没有问题**（`scan_file` 对所有通道计数，`scan` 的 `channel_ids = dbc_map.keys() ∪ frame_counts`），
   不要动 Rust。

## 要求（逐条实现，缺一条即不通过）

### A. 解析信号 → 确定进度（对应契约 §2 GUI 消费契约）
* **A1** 新增 `def _begin_determinate(self, text: str) -> None`：按转换那套 `_pv` 状态机初始化
  （`_new_progress_state()`、`active=True`、`busy=True`、`started=time.monotonic()`、
  `progress_bar.setRange(0, 100)`、`setValue(0)`、`_set_progress_state("working")`、
  `_render_progress()`、启动 `_progress_timer`）。
* **A2** `start_parse_signals()` 用 `self._begin_determinate("解析中…")` 替换 `self._begin_busy(...)`。
  仅当 `self.entries` 为空（无 BLF，只按 DBC 列信号，`total` 必然是 0）时才允许走 `_begin_busy`。
* **A3** `_on_scan_progress(payload)` 必须**驱动进度条**（现在它完全没碰进度条，只改了标签）：
  * `total > 0` → `self.progress_bar.setValue(max(当前值, int(100 * done / total)))`；`done >= total` 时置 100。
  * **单调不减**：新值小于当前值时忽略（不得回退）。
  * 副文本（`lbl_progress_meta`）：`文件 {done}/{total} · 本文件已扫 {frames_done:,} 帧`（千分位）。
  * 主文案（`lbl_progress_status`）：`解析中 · {basename(file)}`。
  * 仍然：`payload["file"]` 非空时把该文件状态置 `STATUS_PARSING`（保留现有行为）。
* **A4** 终态必须明确：
  * `_on_scan_ok` → `progress_bar.setValue(100)`、`lbl_progress_status` 显示 `解析完成`、
    `_set_progress_state("done")`（**保留**现有"解析完成：n/m 个文件可用"等文案逻辑，别丢功能）；
  * `_on_scan_failed` → `_progress_idle()` 并保留红色失败文案（不得停在中间百分比）；
  * `_on_scan_thread_done` → 保留现有"扫描线程结束但转换还在跑时不要复位进度"的守卫逻辑。
* **A5** 解析路径**不得再出现任何 0→100→0 的动画**。

### B. 通道节点只增不减 + 可手动添加（对应契约 §5 第 10/11 条）
* **B1** 新增实例属性 `self.channel_slots: set[int]`；在 `_seed_channels()` 之后初始化为
  `set(range(1, len(CHANNEL_CHOICES) + 1))`。
* **B2** `_populate_signal_tree()` 的通道 key 集合改为：
  `{int(k) for k in channel_summary if k.isdigit()} | set(self.dbc_by_channel) | set(self.channel_slots)`。
  重建**不得**清空 `channel_slots`。
* **B3** `_channel_item(channel)` 新建节点时把 `channel` 加入 `channel_slots`（保证手动添加的通道在下一次
  解析重建后依然存在——这正是用户踩的坑）。
* **B4** 新增「➕ 添加通道…」入口，两个地方都要有：
  * 工具栏：紧挨 `self.act_clear_dbc` 之后加一个动作（文案 `➕ 添加通道…`，statusTip 说清用途）；
  * 信号树空白处右键：`self.signal_view` 的 `customContextMenuRequested` 里，若 `indexAt(pos)` 无效
    （点在空白处）则弹出同一个菜单项。
  * 实现 `def add_channel_dialog(self) -> None`：用
    `QInputDialog.getInt(self, "添加通道", "通道号（1~64）：", value=最大通道号+1, min=1, max=64)`；
    已存在 → **只写状态栏提示**"通道 ChN 已存在"（不弹错误框、不重复建节点）；
    成功后 `channel_slots.add(n)`、`_channel_item(n)`、`expandToDepth(0)`、状态栏提示"已添加通道 ChN，
    可在其上右键添加 DBC"。
  * 为可测性：把核心逻辑拆成 `def _add_channel(self, channel: int) -> bool`（返回是否为新建），
    弹窗函数只负责取号并调用它。
* **B5** 通道节点文案：
  * 有 DBC（含失败/缺失后缀）→ 维持现状；
  * 无 DBC **且有帧观测**（即该通道出现在 `channel_summary` 里）→ `通道 Ch{N}（未绑定 DBC，显示原始 ID）`；
  * 无 DBC **且无帧观测** → `通道 Ch{N}（无帧观测，右键可添加 DBC）`，
    tooltip：`Ch{N}\n该通道在当前 BLF 中没有帧，或尚未添加 BLF；\n添加 DBC 后点「🔍 解析信号」即可列出报文`。
* **B6** `missing_dbc_channels` 红字、`dbc_error` 红字与 tooltip 逻辑不变。

## 自检（必须真跑，把命令与**真实输出**贴进交付说明；主会话会独立复跑）
1. `.venv/bin/python -m py_compile gui/main.py`
2. `QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py --selftest` → exit 0
3. 新增 `--selftest-parse`（规格见下）→ 必须打印 `PARSE_OK ...` 且 exit 0
4. 不许回归：`--selftest` / `--selftest-config` / `--selftest-progress` / `--dump-check-states` 全部 exit 0

### `--selftest-parse` 规格（必须实现）
* 临时目录里把 `fixtures/multi_channel.blf` **复制 3 份**（让文件级进度有 3 格），`fixtures/demo.dbc` 绑到 Ch1、Ch2；
* 构造主窗口（`QT_QPA_PLATFORM=offscreen`），把 3 个文件加入列表，触发 `start_parse_signals()`；
* 用 `app.processEvents()` 泵事件直到 `self.scan_worker is None`（带 60 秒硬超时，超时打印 `PARSE_FAIL timeout` 并 exit 2）；
* 用 `progress_bar.valueChanged` 记录每次变化，断言：
  * 序列**单调不减**（出现回退即 fail）；
  * **终值 == 100**；
  * 至少 2 个不同取值（证明真的在按文件推进）；
* 解析完成后断言：`signal_model.rowCount() >= 8`（Ch1~Ch8 全在）且 `_channel_item(7)` 能取到节点；
* 再断言：`_add_channel(9)` 返回 True、`rowCount()` 增加、`_add_channel(9)` 第二次返回 False；
* 输出格式：`PARSE_OK values={去重后取值列表} channels={rowCount} added_ch9=ok` → `return 0`；
* 断言失败：打印 `PARSE_FAIL <原因>` → `return 2`。**不要**用异常冒泡（要能看清是哪条断言）。
* 退出统一走 `os._exit()`（与其他自检一致，避免打包态退不出去）。

## 交付说明必须包含
* 改了哪些函数（函数名 + 行号）；
* 上面 4 条自检的**真实命令与输出**；
* 你如何保证"单调不减"与"Ch1~Ch8 不会消失"（各一句话）。

## 禁止事项
* 不许引入新依赖；不许用弹窗报错（用户要求红字/状态栏）；
* 不许删掉 `_seed_channels()` 的 Ch1~Ch8 预置（那是"先绑 DBC 再统一解析"流程的入口）；
* 不许改 Rust、契约文档、CI、测试目录。
