# Fast BLF Converter — 接口契约（冻结版 v1.0, 2026-10-16）

**本文件是实现契约。任何实现（无论由谁写）不得偏离其中的签名、字段名、错误码、回调载荷。**
改动契约必须先改本文件。

项目根：`/Users/wuzeqin/Projects/fast_blf_converter`

---

## 0. 环境事实（已实测，不要猜）

| 项 | 事实 |
|---|---|
| Rust | 本机 `~/.cargo`（rustup stable，minimal profile）；目标 edition **2021** |
| Python | `.venv`（3.12，`--system-site-packages`，可见 PySide6 6.10） |
| 目标平台 | Windows 10/11 办公机（本机 macOS 只验证 Rust 核心 + 扩展可导入 + GUI 可构造） |
| BLF crate | **`vector-blf` 不存在**（crates.io 404）。候选：`blf_asc 0.2.0` / `ablf 0.2.1` / `autors-blf 0.1.1`——由 Step 0 POC 定标 |
| DBC crate | `can-dbc 10.0.0` ✅ 真实存在 |
| 测试素材 | 无真实 BLF/DBC。解码正确性靠**合成 BLF + 单元测试**自证 |

---

## 1. Python 可见 API（PyO3 模块 `fast_blf_core`）

```python
fast_blf_core.version() -> str
    # 形如 "0.1.0"

fast_blf_core.scan(blf_paths: list[str], dbc_by_channel: dict[int, str],
                   progress_cb: Callable[[dict], None] | None = None) -> dict
    # 只读扫描：给 GUI 建三级信号树。dbc_by_channel 的 key=通道号(int)，value=dbc 绝对路径
    # blf_paths 允许为**空列表**：此时只按已绑 DBC 列出报文/信号，frame_count=0、cycle_ms=None、
    # is_fd=false（用于"还没加 BLF 就先解析 DBC 看信号树"的场景）。
    # 返回:
    # {
    #   "files":   [{"path": str, "size": int, "ok": bool, "error": str|None}],
    #   "channels": {
    #      "1": {"dbc": str|None, "dbc_error": str|None,
    #            "dbc_note": str|None,   # 该通道 DBC 被规范化时的说明（去 BOM / 按 GB18030 解码 /
    #                                    # 忽略 N 条不支持的语句…）；未规范化则为 None
    #            "messages": [{"id": int,       # 原始 CAN ID（含扩展帧标志位则按原始值）
    #                          "id_hex": str,   # "0x1A2"，GUI 直接显示
    #                          "name": str,     # 无 DBC 时为 "ID_0x1A2"
    #                          "cycle_ms": float|None,   # 由时间戳中位数估计；样本不足为 None
    #                          "frame_count": int,
    #                          "is_fd": bool,
    #                          "signals": [{"name": str, "unit": str,
    #                                       "bit_len": int, "is_signed": bool,
    #                                       "byte_order": "intel"|"motorola",
    #                                       "factor": float, "offset": float,
    #                                       "min": float|None, "max": float|None}]}]}}
    # }
    # 注意：scan 只解析 BLF 中出现的 (channel, id) 集合与 DBC；不落盘、不解信号值。
    #      frame_count / cycle_ms 都是【观测值】，必须包含未绑定 DBC 的通道与报文
    #      （GUI 要靠它显示"未绑定 DBC，显示原始 ID"的节点）；`selected` 只影响 convert 阶段。

fast_blf_core.convert(config: dict, progress_cb: Callable[[dict], None] | None = None) -> dict
    # 全量转换（阻塞调用；重活内部已 allow_threads）
```

### 1.1 `convert` 的 config 字段（全部必填，除非标注可选）

| 字段 | 类型 | 含义 |
|---|---|---|
| `blf_paths` | `list[str]` | 待处理 BLF 绝对路径（顺序无关） |
| `dbc_by_channel` | `dict[int, str]` | 通道 → DBC 路径 |
| `selected` | `dict[str, dict[str, list[str]]]` | `{"1": {"BMS_Voltage": ["Volt_Cell1", ...]}}`；key 是通道号字符串，第二层 key 是报文名，value 是信号名列表。**空 dict = 全部选中** |
| `split_by_message` | `bool` | True=按 (channel, message) 拆独立子表；False=全局宽表 |
| `resample_hz` | `float \| None` | `None`=保持原始时序；否则等间隔重采样（1/10/50/100 Hz） |
| `export_format` | `str` | `"csv"` \| `"parquet"` \| `"xlsx"` |
| `out_dir` | `str` | 输出目录（不存在则创建） |
| `temp_dir` | `str` | Parquet 分片临时目录（不存在则创建；结束前清空其内容） |
| `excel_row_limit` | `int` | 默认 `1_000_000`；xlsx 模式超过即降级 |
| `memory_budget_mb` | `int` | 默认 `1536`；分片缓冲达到该值即 flush 落盘 |
| `ffill` | `bool` | **兼容别名**（Step 14 起，等价于 `fill_policy="hold"`），默认 `false`。仅在 `split_by_message == false` 且 `resample_hz == None`（保持原始时序）时生效：对 outer join 后的宽表做前向填充 |
| `fill_policy` | `str` | 填充策略：`none`（不填充，留空）\| `hold`（阶梯保持/ZOH：把最近一次**真实采样**保持到下一拍，**不造新值**）\| `linear`（线性插值：**会在两个真实采样点之间造出从未采样过的数值**）。**未显式给出时按路径取默认**：重采样网格路径（`resample_hz != None`）= `hold`；保持原始时序的宽表 = `none`（即 `ffill=false` 的现状）。`ffill=true` 等价于显式指定 `hold`。拆表模式下不生效（同一报文内周期一致、不产生跨信号空值）；宽表-原时序与宽表-网格按上述默认生效。**状态/枚举/计数类信号必须用 `hold`**——线性插值会把档位 1→2 插成 1.5、把累加计数插成小数。`none` 在网格路径上无意义（网格点必须有值）→ 按 `hold` 处理并记 `warnings` |
| `ffill_max_gap_ms` | `float \| None` | 默认 `None`。仅 `hold` 生效：某行往前超过该毫秒数找不到新采样时**不再保持**，该行置空——防止"通信中断/丢帧"被旧值掩盖成"一直正常"。受影响行数按信号计入 `warnings` |
| `time_format` | `str` | 时间列口径，默认 `"relative_s"`。取值：`relative_s`（相对秒，float）\| `relative_ms`（相对毫秒，float）\| `absolute`（`YYYY-MM-DD HH:MM:SS.ffffff` 字符串）\| `unix_s`（绝对秒，float）。列名统一保持 `timestamp` |

**`time_format` 的语义（务必逐条实现，口径错了等于导出错数据）：**
- `relative_s` / `relative_ms`：相对**本次任务全部输入文件中最早的时间戳**（全局基准 = 0）。单文件时等价于"相对测量开始"；多文件切片合并后仍保持单调递增。
- `absolute`：= **该帧所属文件的 BLF 头部墙钟时间（原样，不做时区换算）** + 相对偏移，格式 `YYYY-MM-DD HH:MM:SS.ffffff`。这就是 CANalyzer 显示/导出的那套时间。
- `unix_s`：绝对秒（float）= BLF 头部时间按 **UTC** 解释的 epoch 秒 + 相对偏移（与 `blf_asc` 的 `Message.timestamp` 口径一致，即不加偏移直接输出）。
- 时间列永远是输出表的**第一列**，名字永远叫 `timestamp`（脚本兼容性）。
- **相对时间列的精度**：`relative_s` 按 **1e-6 s（微秒）** 取整、`relative_ms` 按 **1e-3 ms（=1 微秒）** 取整后再写出。
  原因：BLF 的时间戳在 epoch 量级（~1.76e9）上以 f64 表示，相邻帧相减会带出 ~1e-7 s 的表示噪声，
  直接写出会得到 `0.009999990463256836` 这类尾数；取整到微秒后输出干净且**不损失有效精度**
  （CAN 帧间隔远大于 1µs，取整单调、不会造成乱序或重复）。`absolute` / `unix_s` 不做取整。

### 1.3 `series_for_plot(config) -> dict`（Step 15 新增：画图与统计的唯一数据来源）

```python
fast_blf_core.series_for_plot(config) -> dict
```

**为什么单独一个 API**：百万级采样点**绝不能**搬进 Python 再画（内存与卡顿都不可接受）。
本接口在 Rust 侧完成"解码 + 按时间分桶抽稀"，只把抽稀结果交给 GUI/HTML。
即：**数据降维在内核里做，画图只画结果**。

config 字段（在 `convert` 的 config 之上）：

| 字段 | 类型 | 含义 |
|---|---|---|
| `blf_paths` | `list[str]` | 同 `convert` |
| `dbc_by_channel` | `dict[int, str]` | 同 `convert` |
| `selected` | `dict[str, dict[str, list[str]]]` | 同 `convert`（只处理选中的信号） |
| `time_format` | `str` | 时间轴口径，与导出口径**完全一致**的取值（`relative_s`/`relative_ms`/`absolute`/`unix_s`）。**但返回的 `t` 永远是 float 秒**：`relative_s`→相对秒、`relative_ms`→**秒**（不要返回毫秒，避免两处口径打架）、`unix_s`→epoch 秒、`absolute`→**相对秒**（0 = 全局最早时间戳），同时必须给出 `t_min_str`/`t_max_str`（`YYYY-MM-DD HH:MM:SS.ffffff`，BLF 头墙钟，不换时区）与刻度标签数组（见下） |
| `buckets` | `int` | 抽稀桶数（屏幕像素宽度量级）。默认 `2000`，允许 `100..20000`，越界报 `BAD_CONFIG`（错误码以 §4 的稳定清单为准：本文件曾误写为 `CONFIG_INVALID`，已更正） |
| `temp_dir` | `str` | 临时目录（复用 convert 语义） |

返回值：

```python
{
  "ok": bool,
  "t_min": float, "t_max": float,        # 全局时间范围（秒；absolute 口径时另给 t_min_str/t_max_str）
  "buckets": int,                        # 实际使用的桶数
  "series": [
    {
      "channel": int, "message": str, "signal": str, "unit": str | None,
      "count": int,                      # 该信号的**原始采样点数**（不是桶数）
      "t":    [float, ...],              # 桶中心时间，长度 = 非空桶数
      "min":  [float, ...], "max": [float, ...], "mean": [float, ...],
      "vmin": float, "vmax": float,      # 全量原始数据的最小/最大（不是抽稀后的）
      "first": float, "last": float,     # 首/末原始采样值
      "mean_all": float,                 # **全量原始数据的算术平均**（sum/count）。
                                         # 报告/统计必须用它；桶均值数组 `mean` 只适合画曲线，
                                         # 二者不可互换（桶内点数不等时，桶均值的平均 ≠ 真均值）。
    }, ...
  ],
  "dbc_errors": {"1": str}, "warnings": [str],
  "errors": [{"file": str, "code": str, "message": str}]
}
```

**时间轴与刻度（`absolute` 口径专用，避免前端做字符串数学）**
* `t_axis`: `[{"t": float, "label": str}, ...]` —— 约 `8~12` 个**等间隔**刻度，`t` 为相对秒，
  `label` 为该时刻的绝对墙钟字符串（`YYYY-MM-DD HH:MM:SS`，秒级即可，若时间跨度 < 2 秒则给到微秒）。
  仅当 `time_format == "absolute"` 时返回（其他口径返回 `[]`，前端用数字轴）。
* `t_min_str` / `t_max_str`：`absolute` 口径时必给；其他口径给 `None`。

**契约要点（逐条实现，缺一条算不合格）**
1. **抽稀必须是 min/max 包络**：每个桶给出桶内所有采样点的 `min`/`max`/`mean` 与桶中心时间。
   渲染方按 `min`/`max` 画包络（或用两条折线），**保证不丢峰**——降采样绝不能丢尖峰。
2. **所有信号共用同一分桶网格**：按全局 `t_min..t_max` 等宽切分，这样多信号可对齐比较；
   某信号在某桶内无采样 → 该桶位置**跳过不输出**（`t`/`min`/`max`/`mean` 三个数组等长且一一对应）。
3. **`count`/`vmin`/`vmax`/`first`/`last` 来自全量原始数据**（不是抽稀后算的）——界面要显示
   "这条曲线背后有 N 个真实采样点"，让用户知道自己在看抽稀结果。
4. **不返回百万级数组**：每信号返回点数 ≤ `buckets`。
5. 数值口径与 `convert` 一致：f64 物理量，不做枚举字符串转换。
6. 宽松构建：单通道 DBC 失败只记 `dbc_errors` 并跳过该通道（同 §5 第 9 条）。

### 1.2 `convert` 返回值

```python
{
  "ok": bool,
  "format_used": "csv"|"parquet"|"xlsx",     # 实际落盘格式
  "downgraded": bool,                         # True 表示 xlsx→csv 降级发生过
  "downgrade_reason": str|None,               # 人话，直接可显示给用户
  "elapsed_s": float,
  "total_rows": int,
  "tables": [{"name": str,            # 子表名："Ch1_BMS_Voltage" / "Merged_All_Signals"
              "channel": int|None,    # 宽表模式为 None
              "message": str|None,
              "rows": int,
              "path": str,            # 绝对路径（xlsx 多 Sheet 时为同一个 .xlsx 路径）
              "sheet": str|None}],    # xlsx 时的 Sheet 名（<=31 字符、已规范化）
  "dbc_errors": {"1": str},           # 按通道报告 DBC 构建失败原因（通道号字符串 → 人话原因）
                                      # 这些通道的报文一律不参与变换；全部失败且无所选时见 §5.9
  "warnings": [str],                  # 非致命问题（如空信号、无 DBC 的报文被跳过）
  "errors":   [{"file": str, "code": str, "message": str}]   # 单文件失败不中断整体
}
```

## 2. 进度回调协议（唯一线程接口）

`progress_cb` 由 **Rust 工作线程**调用（**并行解码期间就必须持续调用**，不能等收集阶段才批量发），参数是单个 dict：

```python
{"phase": 1|2,          # 1=并行解码, 2=合并与导出
 "stage": "scan"|"decode"|"merge"|"export",
 "done": int, "total": int,          # 文件维度（phase 1）= 已完成文件数 / 文件总数；
                                     # phase 2 = 已完成步数 / 总步数
 "file": str|None,                   # 当前文件（decode 阶段）
 "frames_done": int,                 # 【新增】当前文件内已解码帧数（单调递增，用于显示"在动"+算速率）
 "message": str}                     # 可直接显示的短句
```

- 回调**必须**被 `py.detach()` 包裹的重活中调用；**允许来自 rayon 工作线程**，但必须满足：
  - 用 `Mutex<Instant>`（或等价物）做节流：同一进程内相邻两次回调间隔 ≥ 50ms；
  - 每次调用内部 `Python::attach` 取 GIL（GIL 会串行化，天然安全）；
  - 回调抛出的 Python 异常必须被吞掉并计入 `warnings`，**不得**导致进程崩溃或任务失败。
- `frames_done` 每帧更新是廉价的（原子计数），**不要**每帧都回调 Python（只更新计数，回调仍按节流触发）。
- GUI 侧禁止在回调里直接碰控件：必须 `Signal` 转发到主线程。

```
（Step 13 冻结）**GUI 消费契约**：

* `done`/`total` 是**文件级**计数（`ProgressThrottle` 已把每个文件的 0→1 映射成全局文件计数），
  **不是帧数**；帧数看 `frames_done`（本文件内已扫/已解码帧数）。`total==0` 表示"没有文件"。
* **`--selftest-parse`（解析信号）必须使用确定进度**：`percent = 100 * done / total`。
  禁止使用 `_begin_busy()` 那套"不确定态乒乓动画"（0→100→0 循环）来表现解析过程——
  用户会误读成"跑完了又回退"。`total==0`（无 BLF 文件、只按 DBC 列信号）时才允许不确定态。
* **进度条必须单调不减**：同一任务内 `progress_bar.value()` 不得出现回退；任务结束必须**停在 100%**。
* 副文本（`lbl_progress_meta`）格式：`文件 {done}/{total} · 本文件已扫 {frames_done} 帧`。
* 阶段文案：`解析中 · {文件名}`；结束时 `解析完成`（勾选恢复等文案沿用 §5 既有要求）。



```rust
// error.rs —— 稳定错误模型（thiserror），所有错误码集中在这里定义
pub struct CoreError { pub code: String, pub message: String, pub detail: Option<String> }

// bit_slice.rs —— 零依赖，纯函数，必须自带 #[cfg(test)] 向量测试
pub enum ByteOrder { Intel, Motorola }
pub fn extract_bits(data: &[u8], start_bit: u32, bit_len: u32, order: ByteOrder) -> Result<u64, CoreError>;
// bit_len 必须传入：符号扩展需要知道位宽（原始签名缺此参数，以本文件为准）
pub fn decode_physical(raw: u64, bit_len: u32, is_signed: bool, factor: f64, offset: f64) -> f64;

// decoder.rs —— BLF 流式 + Rayon + 分片
pub struct DecodePlan { /* (channel, frame_id) -> MessageDecoder，O(1) 查找 */ }
pub fn build_plan(dbc_by_channel: &BTreeMap<u32, PathBuf>, selected: &Selection) -> Result<DecodePlan, CoreError>;
pub fn process_file(path: &Path, plan: &DecodePlan, sink: &ShardSink, mem_budget: usize,
                    progress: &dyn Fn(Progress)) -> Result<FileStat, CoreError>;

// exporter.rs —— Polars 合并/重采样/导出
pub struct ExportOptions { pub split_by_message: bool, pub resample_hz: Option<f64>,
                           pub format: ExportFormat, pub out_dir: PathBuf,
                           pub excel_row_limit: u64 }
pub fn export(shards: &[PathBuf], opts: &ExportOptions,
              progress: &dyn Fn(Progress)) -> Result<ExportReport, CoreError>;

// lib.rs —— 仅 PyO3 胶水：py.allow_threads + 回调桥 + 异常映射，不放算法
```

## 4. 错误模型

```rust
pub struct CoreError { pub code: String, pub message: String, pub detail: Option<String> }
```
- 映射为 Python 异常 `fast_blf_core.CoreError`，`str(e)` == `"{code}: {message}"`。
- 稳定错误码：`BLF_OPEN`、`BLF_PARSE`、`DBC_PARSE`、`NO_DBC_FOR_CHANNEL`、`EMPTY_RESULT`、`DISK_FULL`、`EXPORT_FAILED`、`BAD_CONFIG`。

## 5. 硬约束（违反即验收不通过）

1. **生产路径禁止 `unwrap()/expect()/panic!`**——panic 会 abort Python 进程。所有失败走 `Result`。
2. `unsafe` 仅允许出现在 PyO3 宏生成处；手写 `unsafe` 需注释理由。
3. 全进程内存峰值 < 2GB：按 `memory_budget_mb` 分片落 Parquet（Zstd），最后用 Polars Lazy 流式合并。
4. **Excel 防呆**：`export_format=="xlsx"` 且任一待导出表行数 > `excel_row_limit` → 自动降级 CSV，`downgraded=true` + 人话原因，**不崩溃、不截断**。
5. 零权限：不碰注册表、不写 `Program Files`/`System32`、不请求 UAC。
6. 打包只允许 PyInstaller `--onedir`（禁止 `--onefile`）。
7. 时间戳：输出列名统一 `timestamp`，单位秒（float64，BLF 原始分辨率）。
8. 拆表模式输出命名：`Merged_Ch{ch}_{MsgName}.csv` / `Ch{ch}_{MsgName}`（子表名）；宽表：`Merged_All_Signals`；xlsx 统一 `Merged_Report.xlsx` + Sheet 名截断到 31 字符并做非法字符替换。
9. **单通道 DBC 失败不得拖垮整体转换**（`convert` 与 `scan` 一致使用宽松构建）：
   - 某通道 DBC 读取/解析失败 → 该通道报文一律跳过，原因记入返回值的 `dbc_errors`，并在 `warnings` 里给人话提示；
   - **只有**当用户实际选中的信号全部落在"失败通道或未绑 DBC 的通道"上时，才报 `EMPTY_RESULT`，且错误消息必须点名是哪些通道、原因是什么；
   - 用户没勾选的通道，其 DBC 失败**绝不能**导致转换失败。

10. **通道集合只增不减（粘性）**：信号树重建时，通道集合 =
    `scan.channels` ∪ `dbc_by_channel` ∪ `self.channel_slots`，其中 `channel_slots` 在启动时预置
    `CHANNEL_CHOICES`（Ch1~Ch8），并随用户手动添加而增长，**只增不减**（除非整个程序重启）。
    **不得因为"该通道没有帧观测 / 没有绑 DBC"而丢弃通道节点**——用户看到的通道凭空消失且无法找回，
    是本项目已发生的真实事故（用户原话："其他通道都消失了，不知道怎么增加通道"）。
11. **必须有「➕ 添加通道…」入口**：工具栏按钮 + 信号树空白处右键菜单，允许添加 1~64 的任意通道号
    （默认预填"当前最大通道号 +1"），添加后该通道节点立即可右键「📄 添加 DBC…」。
    空通道节点的显示文案：`通道 Ch{N}（无帧观测，右键可添加 DBC）`，tooltip 说明"该通道在当前 BLF 中
    没有帧，或尚未添加 BLF；添加 DBC 后点『🔍 解析信号』即可列出报文"。
12. **填充策略**：见 §1.1 `fill_policy`。实现必须在**重采样网格**与**保持原始时序**两条路径上都遵循
    `fill_policy`；`linear` 路径必须在 `warnings` 里提示"已对 N 个信号做线性插值（可能出现未采样过的中间值）"。
13. **`ffill_max_gap_ms`**：仅在 `hold` 生效；超限置空的行数必须计入 `warnings`（人话，带信号名与行数）。

14. **画图预览（Step 16）**：勾选 ≥1 个信号后「📈 预览」打开独立窗口。
    * 横轴口径必须跟随界面上的「时间格式」下拉（与导出口径一致）；
    * 不同量纲信号（V / °C / A）**必须分 Y 轴**或归一化，不允许硬塞进同一 Y 轴造成误读；
    * 界面必须显示每信号的**原始采样点数**与"已抽稀为 N 桶（min/max 包络，不丢峰）"的说明——
      绝不能让用户以为看到的是全部原始点；
    * 渲染线程不得参与解码：数据只能来自 `series_for_plot`（见 §1.3），不得在 GUI 里遍历 BLF。
15. **导出 HTML 报告（Step 16）**：单个 `.html` 文件、**离线可开**、图表库必须内嵌（**禁止 CDN**）；
    包含：时间范围、每信号的原始点数与 min/max/mean 统计、可缩放/悬停读数的曲线；
    文件体积 < 8MB；生成失败要有明确错误码，不得产出半截文件。
16. **不新增 Python 侧依赖**：桌面预览必须走 PySide6 自带能力、自绘（QPainter）或纯 Python 实现
    （**优先自绘**：min/max 包络与量纲分轴都需要精确控制，且零打包风险）。
    HTML 报告允许内嵌**已由 Hermes 主会话审阅并 vendor 进仓库的前端库**（必须有 license 说明、
    记录在 `docs/DEPS.md`、且构建期不联网）；任何 Python/二进制依赖的新增都必须先经批准。

## 6. 验收标准（由 Hermes 主会话执行，DSH 交付后必须自查一遍）

| # | 命令/动作 | 通过条件 |
|---|---|---|
| A1 | `cargo test -p fast_blf_core` | 全绿；bit_slice 覆盖 Intel/Motorola × 有符号/无符号 × 跨字节边界 |
| A2 | `cargo build --release` | 退出码 0 |
| A3 | `python -c "import fast_blf_core; print(fast_blf_core.version())"` | 打印版本号 |
| A4 | 合成 BLF 端到端 | `scan` 出三级树；`convert` 三种格式各跑一次，行数与注入帧数一致 |
| A5 | 防呆 | 构造 > 1e6 行输入 → 返回 `downgraded=true`、`format_used="csv"`，无异常 |
| A6 | GUI | `QT_QPA_PLATFORM=offscreen python gui/main.py --selftest` 能构造主窗口并退出，退出码 0 |
