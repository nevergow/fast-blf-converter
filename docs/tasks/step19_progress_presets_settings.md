# Step 19：转换进度加权单条 + 预设补全参数 + 缓存设置 + 主树三态全选

## 先读契约
`docs/INTERFACE.md` → §1.4（`cache_budget_mb` 默认 200）、§5 第 18/20 条。

## 你只能改这些文件
* `gui/main.py`
* `gui/presets.py`
* `gui/theme.qss`（克制）

**禁止改**：`gui/plot_view.py`（Step 18 的地盘）、`crates/**`、`docs/**`、`.github/**`、
`README.md`、`使用说明.txt`、`tests/acceptance.py`。

## 用户原话（验收标准）
> 「在转换时，好多次进度条都没到 100% 就进下一步了」
> 「配置增加保存导出格式、重新采样频率、时间列格式、填充方式等参数的保存」
> 「允许的临时文件大小默认 200MB，可由用户自定义」
> 「在信号选择这个地方，增加全选或全不选的选择框」（→ 用户要的是**一个三态复选框**，不是三个按钮）

## 要求

### A. 转换进度：单条加权、单调、只在结束时 100
* **A1** 改 `_progress_percent()`：不再让 phase1 / phase2 各算一套 0→100，改为**加权合成**：
  * `phase == 1` → `0..70`（`fraction = (done + within)/total` 映射到 0.70）
  * `phase == 2` → `70..100`（`fraction = done/total` 映射到 0.70→1.00）
  * 常量集中定义（如 `_PHASE1_SHARE = 0.70`），便于以后调整
* **A2** **单调不减**：`setValue(max(当前值, 新值))`；只有 `_finish_progress_ok()` 才允许 100。
* **A3** 文案：阶段名清晰（`阶段 1/2 · 解码`、`阶段 2/2 · 合并导出`），并保留 `done/total 文件`、
  `已 N 帧`、`≈ 412 MB / 1.2 GB`、速率、已用时（现有信息不许丢）。
* **A4** 解析（scan）与预览会话的进度**同样**单调且确定（不得回退）；预览进度在 Step 18 的会话里已带 `progress_cb`，
  本步只负责"主窗口进度区"的接线与终态。

### B. 预设补全（这是用户明确点名的）
* **B1** `gui/presets.py`：`DEFAULT_EXPORT` 扩展为
  `{split_by_message, resample_hz, export_format, time_format, fill_policy, ffill_max_gap_ms}`；
  `schema` 升到 **2**；读取 `schema==1` 的老配置时**补默认值**（不得报错、不得丢原有字段）。
* **B2** `_coerce_export()` 校验：`time_format ∈ {relative_s, relative_ms, absolute, unix_s}`、
  `fill_policy ∈ {none, hold, linear}`、`ffill_max_gap_ms` 为 `None` 或 `> 0` 的 float；非法值 → 回落默认并记 `warnings`。
* **B3** 保存/加载：`current_export_policy()` 与 `apply_export_policy()` 必须覆盖全部 6 个字段；
  加载后界面控件（拆表/重采样/导出格式/时间格式/填充方式/最大保持时长）**逐个同步**。
* **B4** 新增 `--selftest-config` 断言（扩展现有）：保存后 JSON 里 6 个字段都在；加载后控件状态与保存值一致；
  **schema 1 的旧配置**能被读入且补齐默认值（在 selftest 里构造一份旧 JSON 验证）。

### C. 缓存设置（默认 200 MiB，可自定义）
* **C1** 新增设置持久化：`<config_dir>/settings.json`，字段 `{cache_budget_mb: 200, cache_dir: null}`
  （`config_dir` 复用 `presets.config_dir()` 的回退链）。
* **C2** 工具栏加 `⚙ 设置`（或在"配置"菜单里）：对话框含
  `帧缓存上限 (MiB)`（默认 200，范围 32..8192，`QSpinBox`）+ `缓存目录`（默认空=用临时目录，带"选择…"按钮）；
  保存时校验范围，非法 → 就地红字提示（不弹错误框）。
* **C3** 打开预览时把 `cache_budget_mb` / `cache_dir` 传给 `open_plot_session`（与 Step 18 的入口对齐；
  若 Step 18 已实现入口，你只需保证参数来自设置而不是硬编码）。
* **C4** 设置变更需**立即生效**（下一次打开预览就用新值），并在状态栏提示"缓存上限已设为 XXX MiB"。

### D. 主信号树：一个三态复选框（与预览面板一致）
* **D1** 信号树面板标题行加**一个** `QCheckBox`（`setTristate(True)`），文案 `全选（3/96）`，
  语义与 Step 18 的 D1/D2 完全一致：点部分/未选 → 全选；点全选 → 全不选；**禁止**三个按钮、禁止"反选"。
* **D2** 与树双向联动：树勾选变化 → 复选框状态重算（不得递归）；复选框点击 → 全选/全不选广播到**当前可见（未被过滤）**的行。
* **D3** 保留现有"全选当前过滤项/一键清空"按钮吗？——**删掉**，统一由这个复选框承担（用户明确嫌按钮多）。

## 自检（必须真跑，输出贴交付说明）
1. `.venv/bin/python -m py_compile gui/main.py gui/presets.py`
2. `--selftest` / `--selftest-parse` / `--selftest-deep` / `--selftest-config` / `--selftest-progress` / `--dump-check-states` 全 exit=0
3. 新增 **`--selftest-progress-monotonic`**（必须实现）：构造一次真实转换（小夹具，parquet/csv 各一次），
   通过 `progress_hook` 记录每次 `progress_bar.value()`，断言：
   * **单调不减**；phase 从 1 切到 2 时**没有回退**；
   * 终值 100 且只在结束时出现；打印 `MONO_OK phase1_max=.. phase2_start=.. final=100 seq_len=..`
4. `--selftest-config` 里新增的 B4 断言全过（含 schema 1 兼容）

## 交付说明必须包含
* 改动函数清单（名字 + 行号）
* 每条自检的**真实命令与输出**
* 一句话说明：为什么 phase 切换后进度条不会再回落（加权映射 + max 单调）

## 禁止事项
* 不许改 `plot_view.py`、Rust、契约、CI、README/使用说明、`tests/acceptance.py`
* 不许用弹窗报错；不许加依赖；不许把设置写进注册表/用户主目录之外的位置
