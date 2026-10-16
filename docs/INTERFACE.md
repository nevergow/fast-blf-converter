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
    # 返回:
    # {
    #   "files":   [{"path": str, "size": int, "ok": bool, "error": str|None}],
    #   "channels": {
    #      "1": {"dbc": str|None, "dbc_error": str|None,
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
| `ffill` | `bool` | 默认 `false`。仅在 `split_by_message == false` 且 `resample_hz == None`（保持原始时序）时生效：对 outer join 后的宽表做前向填充 |

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
  "warnings": [str],                  # 非致命问题（如空信号、无 DBC 的报文被跳过）
  "errors":   [{"file": str, "code": str, "message": str}]   # 单文件失败不中断整体
}
```

## 2. 进度回调协议（唯一线程接口）

`progress_cb` 由 **Rust 工作线程**调用，参数是单个 dict：

```python
{"phase": 1|2,          # 1=并行解码, 2=合并与导出
 "stage": "scan"|"decode"|"merge"|"export",
 "done": int, "total": int,
 "file": str|None,                      # 当前文件（decode 阶段）
 "message": str}                        # 可直接显示的短句
```

- 回调**必须**被 `py.allow_threads()` 包裹的重活中调用，调用频率上限 ~20 次/秒（内部节流）。
- 回调抛出的 Python 异常不得导致进程崩溃：Rust 侧捕获、记录到 `warnings`、继续跑。
- GUI 侧禁止在回调里直接碰控件：必须 `Signal` 转发到主线程。

## 3. Rust 内部 seam（文件归属冻结，禁止跨文件重命名/搬运）

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

## 6. 验收标准（由 Hermes 主会话执行，DSH 交付后必须自查一遍）

| # | 命令/动作 | 通过条件 |
|---|---|---|
| A1 | `cargo test -p fast_blf_core` | 全绿；bit_slice 覆盖 Intel/Motorola × 有符号/无符号 × 跨字节边界 |
| A2 | `cargo build --release` | 退出码 0 |
| A3 | `python -c "import fast_blf_core; print(fast_blf_core.version())"` | 打印版本号 |
| A4 | 合成 BLF 端到端 | `scan` 出三级树；`convert` 三种格式各跑一次，行数与注入帧数一致 |
| A5 | 防呆 | 构造 > 1e6 行输入 → 返回 `downgraded=true`、`format_used="csv"`，无异常 |
| A6 | GUI | `QT_QPA_PLATFORM=offscreen python gui/main.py --selftest` 能构造主窗口并退出，退出码 0 |
