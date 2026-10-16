# Step 16：画图功能（桌面预览窗 + 离线 HTML 报告）

> **前置**：Step 15 已交付 `fast_blf_core.series_for_plot(config)`（见 `docs/INTERFACE.md` §1.3）。
> 本步**只做前端**：把已经抽稀好的数据画出来。**绝对禁止在 GUI 里遍历 BLF 或解码信号**。

## 你只能改这些文件
* `gui/plot_view.py`（**新建**：自绘预览控件 + 预览窗口）
* `gui/report_html.py`（**新建**：离线 HTML 报告生成）
* `gui/main.py`（接入：工具栏按钮 / 信号树右键「📈 预览」/ 「📤 导出 HTML 报告」入口 + 新自检）
* `gui/theme.qss`（预览窗需要配色时；优先复用已有变量）
* `gui/assets/echarts.min.js`、`gui/assets/NOTICE.txt`（**只读，禁止修改**）

**禁止改**：`crates/**`（契约见 §1.3，Step 15 已实现）、`docs/INTERFACE.md`、`.github/**`、
`README.md`、`使用说明.txt`、`tests/acceptance.py`、`packaging/**`。

## 已有资产（不要重复造）
* **`gui/assets/echarts.min.js`**：Apache ECharts 5.5.1（Apache-2.0，1,030,855 字节，
  sha256 `e84270bd0cd5bdf60fefc26d00c2a391cb2e81f4d26a7a9ee16185a54773a3cf`）。
  溯源见 `gui/assets/NOTICE.txt`。**HTML 报告必须把这个文件的内容内嵌**（`<script>` 里直接写进去），
  不许 `<script src="https://...">`（用户公司电脑可能断网，**禁止 CDN**）。
* 桌面预览**不许用 QtCharts / pyqtgraph / matplotlib**——**必须自绘**（`QWidget.paintEvent` + `QPainter`），
  理由：零新依赖、零打包风险，且 min/max 包络渲染与多量纲分轴需要精确控制。

## A. 桌面预览窗（`gui/plot_view.py`）

### A1 数据来源
只调 `fast_blf_core.series_for_plot(config)`（config 与 §1.3 一致；`buckets` 取预览控件宽度像素级，
默认 2000）。**不得**在 GUI 里打开 BLF、不得调用 `convert` 后再读 CSV 来画图。

### A2 渲染方式（**min/max 包络，不丢峰**）
* 每个信号用 `t`/`min`/`max` 画成"梳状包络"：按桶顺序把 `(t, min)` 与 `(t, max)` 交替连成折线
  （竖条即桶内包络），**不允许只用 mean 画一条线**（会丢尖峰）。
* 可选叠加一条半透明的 `mean` 折线（默认开）。
* **空桶跳过**（三数组等长，直接按索引画），不得用 0 或前值填补断口。

### A3 交互（最低要求）
* 滚轮 = 以鼠标位置为中心缩放横轴；按住拖动 = 平移；双击 = 复位全览。
* 十字光标 + 悬停显示：当前时间（按 `time_format` 口径，`absolute` 用 `t_axis`/`t_min_str` 换算显示）
  与**各信号在该时刻的取值**（多信号时用列表显示，颜色与曲线对应）。
* 右侧/底部**图例**：信号名 + 单位 + 原始采样点数（`count`），可点击显隐。
* 顶部信息条：`时间范围`（`absolute` 时显示 `t_min_str → t_max_str`）、
  `共 N 个信号 / 总原始点数 M`、以及**必须显示**的抽稀说明：
  `已抽稀为 {buckets} 桶 · min/max 包络（不丢峰）· 每桶 {原始点数} 点`。
  —— 绝不能让用户以为看到的是全部原始点。

### A4 多量纲分轴（**硬要求**）
* 按 `unit` 分组：同一量纲共用一条 Y 轴，不同量纲各自独立 Y 轴并**在左侧依次排列**（最多 4 条，超出时
  第 5 个及以后并入"归一化"模式：各自缩放到 0~100% 并标注原量程）。
* 每个 Y 轴标注单位；颜色与图例一致。
* 若信号 `unit` 为 `None`/空，归入"无量纲"轴。

### A5 性能与稳定性
* 重绘必须只依赖已缓存的 `series`（`paintEvent` 里不得重新调用 Rust）。
* 缩放/平移只做坐标变换重绘，不重新取数（除非可见桶数 > 屏宽 4 倍时才提示"细节已抽稀"）。
* 关闭窗口要干净（与其他窗口一致，不留线程/不留僵尸）。

## B. 离线 HTML 报告（`gui/report_html.py`）

### B1 输入与输出
* 输入：与预览相同的 `series_for_plot` 结果（同一份数据，**不要**为 HTML 再算一遍）。
* 输出：单个 `.html` 文件（用户在 `out_dir` 里选名字，默认 `BLF_Report_{时间戳}.html`）。
* **必须完全自包含**：ECharts 内嵌 + 数据内嵌（JSON）+ 样式内嵌；**文件中不得出现任何外部 URL**
  （不许 `http://`/`https://` 的 `src`/`href`/`fetch`）。生成后自检这一条。

### B2 内容
* 顶部：报告标题、生成时间、BLF 文件清单（文件名 + 大小）、时间范围、
  各通道 DBC 绑定情况（含 `dbc_errors`，失败通道要显式列出——同界面的红字原则）。
* **统计表**：每个信号一行：通道 / 报文 / 信号 / 单位 / **原始点数** / min / max / mean /
  first / last。（统计来自全量原始数据，不是抽稀后算的 ✅ 见 §1.3）
* **图表**：与桌面预览同样的 min/max 包络；支持 legend 显隐、dataZoom 缩放、
  tooltip 十字光标读数（ECharts 原生能力即可）；横轴 `absolute` 时用 `t_axis` 的标签。
* 页脚：抽稀说明（同 A3 那句）+ 版本号 + "本报告为离线自包含文件"。
* 中文字体：不指定特殊字体（用系统默认），不得因为字体缺失导致乱码（UTF-8 + `<meta charset>`）。

### B3 体积与失败处理
* 文件体积 **< 8MB**；超出时：减少桶数（最低 500）并在页面上说明，而不是截断数据。
* 写文件失败/权限不足 → 返回明确错误，**不得产出半截文件**（先写临时文件再原子改名）。

## C. 接入 `gui/main.py`
* **C1** 工具栏新增「📈 预览」（勾选 ≥1 个信号才可用）和「📤 导出 HTML 报告」。
* **C2** 信号树右键菜单：在选中信号/报文上右键 → 这两项（可只对叶子信号生效）。
* **C3** 两者都要：解析未完成/无选中信号时给出状态栏提示（不弹窗）；取数期间显示进度（复用现有进度区，
  `series_for_plot` 在主线程会很慢 → **必须放 Worker 线程**，与现有 `Worker` 机制一致）。
* **C4** 预览窗口与主窗共享「时间格式」下拉的口径；用户在预览窗里改口径时**重新取数**（不是前端换算）。

## D. 自检（必须真跑，输出贴进交付说明）
1. `python -m py_compile gui/main.py gui/plot_view.py gui/report_html.py`
2. 新增 `--selftest-plot`：
   * 用 `fixtures/multi_channel.blf` + `fixtures/demo.dbc`，构造主窗口（offscreen）；
   * 调 `series_for_plot` 拿数据 → 断言 `series` 非空、每信号 `len(t) <= buckets`；
   * 构造预览控件、`resize(1200, 600)`、`grab()` 一次（离屏渲染必须不崩）；
   * 生成 HTML 报告到临时目录 → **断言：文件存在、大小 < 8MB、
     内容里不含 `http://`/`https://` 的 src/href、含 `echarts` 标识、含至少 3 个信号名**；
   * 打印 `PLOT_OK series={n} buckets={b} html_bytes={k} png_bytes={p}` 并 `return 0`；失败打印 `PLOT_FAIL ...` 返回 2。
3. 既有 6 条自检全部不回归：`--selftest` / `--selftest-parse` / `--selftest-deep` / `--selftest-config` /
   `--selftest-progress` / `--dump-check-states`。
4. 打包相关：`packaging/FastBLFConverter.spec` 若需要包含 `gui/assets/**`，先检查现状
   （**允许改 spec 里与 assets 有关的那一行**，这是本步唯一的 spec 例外），并说明改了什么。

## 交付说明必须包含
* 新增文件与函数清单（名称 + 行号）；
* 每个自检的**真实命令与输出**；
* 一句"为什么自绘而不是 QtCharts"，一句"HTML 为什么必须内嵌 ECharts"；
* HTML 报告的自包含性你是怎么验的（贴出检查命令）。

## 禁止事项
* 不许改 Rust；不许在 GUI 里解码/遍历 BLF；不许用 CDN；不许新增 Python 依赖；
* 不许修改 `gui/assets/echarts.min.js`（内容哈希必须保持不变）；
* 不许把"抽稀说明"藏起来或省略——用户必须知道自己在看抽稀结果。
