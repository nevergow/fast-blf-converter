# Step 2b 任务书（exporter.rs 合并/CSV/Parquet + convert 接线）—— 交给 DSH headless 执行

你是 Rust 系统工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先完整读：
- `docs/INTERFACE.md`（§1 `convert` 的 config 字段与返回值、§2 回调协议、§4 错误码）
- `docs/DEPS.md`（§3 Polars API 实测事实 —— **尤其"0.55.2 没有 `LazyFrame::join_asof`"这条**）
- `crates/fast_blf_core/src/decoder.rs`（Step 2a 已实现，复用其 `DecodePlan` / 分片落盘结构）
- `crates/fast_blf_core/src/bit_slice.rs`、`error.rs`

## 本次只做 Step 2b，只允许新建/修改这三个文件

1. 新建 `crates/fast_blf_core/src/exporter.rs`
2. 修改 `crates/fast_blf_core/src/decoder.rs`（**只允许**加"分片（shard）产出与登记"所需的最小改动：把每个文件解码后的列式缓冲按 `(channel, message)` 落成 Zstd Parquet 分片，并返回分片路径清单 + 每表行数）
3. 修改 `crates/fast_blf_core/src/lib.rs`（加 `convert` pyfunction）

**本次不写 Excel**（Step 2c 负责）；`export_format == "xlsx"` 在本步先走"降级为 CSV"的路径并把 `downgraded=true`、`downgrade_reason` 填好。

## 1. 分片（shard）约定

- 每个输入文件解码时，按 `(channel, message_name)` 独立维护列缓冲区：一列 `timestamp: Vec<f64>` + 每信号一列 `Vec<f64>`。
- 缓冲区行数 ≥ 200_000 行 **或** 估算内存 ≥ `memory_budget_mb`（config 传入，默认 1536）时立即 flush：用 Polars `DataFrame` + `ParquetWriter`（zstd 压缩，压缩级别 3）写入 `temp_dir/shard_<fileidx>_<tableidx>.parquet`，然后清空缓冲。
- 分片文件名必须能唯一确定它属于哪张表 —— 建议 `temp_dir/{sanitized_table_name}__{file_idx}__{part_idx}.parquet`，并在返回结构里登记 `BTreeMap<TableKey, Vec<PathBuf>>`。
- **内存峰值 < 2GB** 是硬约束：不要把多个文件的全量数据同时留在内存里。
- `temp_dir` 里只允许删自己创建的 `*.parquet` 分片（任务结束前清理），**绝不允许** `remove_dir_all` 整个目录。

## 2. exporter.rs 必须提供

```rust
pub enum ExportFormat { Csv, Parquet, Xlsx }
pub struct ExportOptions { pub split_by_message: bool, pub resample_hz: Option<f64>,
                           pub format: ExportFormat, pub out_dir: PathBuf, pub excel_row_limit: u64 }
pub struct TableResult { pub name: String, pub channel: Option<u32>, pub message: Option<String>,
                         pub rows: u64, pub path: String, pub sheet: Option<String> }
pub struct ExportReport { pub format_used: ExportFormat, pub downgraded: bool,
                          pub downgrade_reason: Option<String>, pub total_rows: u64,
                          pub tables: Vec<TableResult>, pub warnings: Vec<String> }
pub fn export(tables: &BTreeMap<TableKey, TableShards>, opts: &ExportOptions,
              progress: &dyn Fn(Progress)) -> Result<ExportReport, CoreError>;
```

合并逻辑（每张表独立）：
- `LazyFrame::scan_parquet(shards, ...)` → 按 `timestamp` 升序 `sort_by_exprs` → 用 `unique(...)` 去重（同一时间戳重复帧只留一份）→ `with_streaming(true)` → `collect()`。
- **排序必须用 `timestamp` 升序**，且输出列顺序固定为 `timestamp` 在最左，其后按 DBC 里信号出现顺序。

输出（本次只做两种格式）：
- 拆表模式（`split_by_message == true`）：每张表一个文件，名字 `Merged_Ch{ch}_{MsgName}.csv` / `.parquet`。
- 宽表模式：所有表按 `timestamp` 做 **outer join**（Polars `join(..., how=JoinType::Outer)`），输出单文件 `Merged_All_Signals.csv` / `.parquet`。列名 = DBC 信号名；**不同报文的同名列必须加前缀去重**（规则：`{MsgName}_{SignalName}`）并在 `warnings` 里说明。
- 表名/文件名清洗：只保留 `[A-Za-z0-9_]`，其余字符换成 `_`。
- CSV 必须用 Polars 多线程 CSV 引擎（`CsvWriter`），带表头；浮点按普通十进制输出（不要科学计数法）。
- 本次 `split_by_message == false` 且 `resample_hz` 为 `None` 时：只做 outer join，不做前向填充（**Step 2c 再补重采样/前向填充**），但要在 `warnings` 里加一条提示"未重采样：宽表会因周期不同产生空值"。
- 若 `split_by_message == false` 且 `resample_hz.is_some()`：本次先返回 `Err(CoreError::bad_config("重采样将在 Step 2c 实现"))`，**不要**悄悄忽略这个参数。

Excel 防呆（本次就要实现，且必须可测）：
- 统计每张表行数后，若 `opts.format == Xlsx` 且任一表行数 > `opts.excel_row_limit`：
  `downgraded = true`，`format_used = Csv`，`downgrade_reason = Some("检测到数据量超过 Excel 百万行上限（当前 {max_rows} 行），为防止文件损坏无法打开，已自动切换为高速 CSV 格式导出")`，**全部表改走 CSV**（不崩溃、不截断）。

## 3. lib.rs 增加 `convert`

```python
fast_blf_core.convert(config: dict, progress_cb=None) -> dict
```
- 用 `Bound<PyDict>` + `get_item().extract()` **手工提取**字段（不要新增依赖、不要用 pythonize）。字段缺失/类型错误 → `BAD_CONFIG` 异常，消息里写清是哪个字段。
- config 字段与返回值**严格**按 INTERFACE.md §1.1 / §1.2。
- 流程：build plan → 并行解码落分片（Phase 1，progress.stage="decode"，done/total 按文件数）→ 合并导出（Phase 2，stage="merge"/"export"）→ 清理分片 → 组装返回值。
- 重活全部在 `py.allow_threads(...)` 内；回调节流（≥50ms 间隔）+ 异常吞噬（记 warnings）。

## 4. 必须自带的测试（`crates/fast_blf_core/tests/e2e_synthetic.rs`，集成测试）

复用 Step 2a 的合成素材场景（Ch1 `VehSpd` 0x123 @10ms ×500、Ch1 `BattTemp` 0x1A2 @100ms ×50、Ch2 `Insulation` 0x200 @1000ms ×5，5 秒数据）：

1. 拆表 + CSV：生成 3 个 `Merged_Ch*.csv`，行数分别 500 / 50 / 5。
2. **空值检查（核心卖点）**：用 Polars 读回 CSV，断言 3 张表**每一列都没有空值**（`null_count() == 0`）。
3. 拆表 + Parquet：行数同上。
4. 宽表 + 保持原始时序：`Merged_All_Signals.csv` 行数 = **555**（5s 内三种周期的帧数之和；若时间戳有重合按去重后的真实值断言，并在注释里写清为什么）。
5. 防呆：`excel_row_limit = 10` 且 `format = Xlsx` → `downgraded == true`、`format_used == Csv`、`downgrade_reason` 含"百万行"字样，且磁盘上确实产出了 CSV 文件。
6. 数值正确性：挑一帧已知输入，断言 `Spd` = `raw × 0.01`、`Temp1` = `-10.0`（Motorola 有符号）。

## 5. 必须交付的测试夹具（供 Python 层端到端验收使用，Step 2c 与外部验收都依赖它）

除了 `tests/e2e_synthetic.rs`，再新建 `crates/fast_blf_core/examples/make_fixtures.rs`：
- 用 `blf_asc::BlfWriter` 生成 **真实的 BLF 文件**到 `fixtures/` 目录：
  - `fixtures/multi_channel.blf`：内容就是 Step 2a 的合成场景（Ch1 `VehSpd` 0x123@10ms×500、Ch1 `BattTemp` 0x1A2@100ms×50、Ch2 `Insulation` 0x200@1000ms×5，5 秒），并要求包含 **1 帧 CAN FD 报文**（`is_fd=true`，DLC 按 FD 表）以覆盖 FD 路径。
  - `fixtures/demo.dbc`：与上面三条报文对应的 DBC 文本（`VERSION` / `BO_` / `SG_`，含 `Temp1` Motorola signed start=7 len=8 与 `Temp2` Motorola unsigned start=4 len=12）。
  - `fixtures/README.md`：写清三个通道/报文/周期/信号定义与预期行数（Ch1_VehSpd=500、Ch1_BattTemp=50、Ch2_Insulation=5）。
- 运行方式：`cargo run -p fast_blf_core --example make_fixtures`，要求**幂等**（重复运行覆盖旧文件即可）。
- 在 e2e 测试里调用同一段生成逻辑（把它抽成 `pub fn write_fixtures(dir: &Path) -> Result<()>` 放在 example 与测试都能用的位置；若不便共享，至少保证两边内容一致）。

**为什么**：Python 层的验收（`fast_blf_core.scan()` / `convert()` 真实跑通）需要磁盘上真实存在的 BLF 与 DBC，不能只有 Rust 内存里的断言。

## 6. 自检（必须跑，贴真实输出）

```
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -40
```
（`DYLD_FALLBACK_LIBRARY_PATH` 必须带：见 `docs/DEPS.md` §0；dyld 报错是环境问题，不要改代码。）
若 `Blocking waiting for file lock` → 等待，不要删 target。若测试失败，**修到通过为止**（可以反复迭代）。

## 6. 回复格式（只回复这些）

- 三个文件各多少行、`cargo test` 真实输出（测试名 + 通过/失败数）、偏离说明（没有就写"无偏离"）。
