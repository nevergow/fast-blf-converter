# Step 2c 任务书（Excel 多 Sheet + 重采样对齐）—— 交给 DSH headless 执行

你是 Rust 系统工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先完整读：
- `docs/INTERFACE.md`（§1 config/返回值、§2 回调、§5 硬约束 4「Excel 防呆」与 7「命名规则」）
- `docs/DEPS.md`（**§3 Polars API 事实**、§4 `rust_xlsxwriter` API、§0 跑测试的命令）
- `crates/fast_blf_core/src/exporter.rs`（Step 2b 已实现 CSV/Parquet 与防呆）
- `crates/fast_blf_core/src/decoder.rs`、`error.rs`

## 本次只做 Step 2c，只允许修改这三个文件

1. `crates/fast_blf_core/src/exporter.rs`（主体）
2. `crates/fast_blf_core/src/lib.rs`（仅当 config 需要透传新字段时）
3. `crates/fast_blf_core/tests/e2e_synthetic.rs`（补测试）

不要动 `Cargo.toml`（不要加依赖）、不要动 `gui/`、`bit_slice.rs`、`decoder.rs`（除必要的最小公开可见性调整）。

## 1. Excel 多 Sheet 导出（替换 Step 2b 的降级路径）

- 拆表模式：生成**单个** `Merged_Report.xlsx`，每个 `(channel, message)` 子表写到**独立 Sheet**。
- 宽表模式：单个 `Merged_All_Signals.xlsx`，单 Sheet。
- 必须用 `workbook.add_worksheet_with_constant_memory()`（百万行级别内存可控）。
- Sheet 名规范化：只保留 `[A-Za-z0-9_]`，截断到 **31 字符**，同 workbok 内**去重**（重名加 `_2`、`_3`）；表头行写列名，从第 1 行开始写数据（`write_string` / `write_number`，每个调用都要处理 `Result`）。
- 未降级时 `report.format_used == Xlsx`、`tables[i].sheet` 填实际 Sheet 名、全部 `tables[i].path` 都是同一个 `.xlsx` 路径。
- 防呆保持不变：任一表行数 > `excel_row_limit` 时**整体**改走 CSV，`downgraded=true`，理由文案不变。
- 空表（0 行）跳过并在 `warnings` 记一条，不要生成空 Sheet。

## 2. 重采样（`resample_hz = Some(hz)`）

两条分支都必须实现：

**拆表模式**（每张表独立）：
- 时间轴 = `t0` 到 `t1`（该表自身首末时间戳），步长 `1/hz`，共 `floor((t1-t0)*hz) + 1` 个点。
- 对齐策略：`join_asof(Backward)` 语义 + **对区间内部点做线性插值**（即：找到前后两个样本，按时间比例线性插值）。
- 首个网格点之前 / 最后一个网格点之后：取最近样本（backward/forward clamp），**不产生空值**。

**宽表模式**（`split_by_message = false`）：
- 网格取所有表的全局并集区间，步长 `1/hz`。
- 各表先 `join_asof(Backward)` 到网格上，再做内部线性插值 + 端点 clamp，最终宽表**不含空值**。
- `resample_hz = None` 时（保持原始时序）：执行 Outer Join（Step 2b 已实现），若 `ffill = true` 则再做前向填充（`fill_null(Strategy::Forward)`）。

**实现建议（按此优先级）**：
1. 先用 Polars 的 asof join（`join(...).how(JoinType::AsOf(...))`，字段名以编译器为准；DEPS.md §3 明确 0.55.2 **没有** `LazyFrame::join_asof`，不要凭记忆写）。
2. 若 asof 在 0.55.2 上不可用或行为不可控，**允许**改为纯 Rust 网格重采样：把 `(timestamp, value)` 读成 `Vec<(f64,f64)>`（已排序），对每个网格点用二分找前驱/后继并线性插值，再组装成 Polars DataFrame。这条路必须实现为**纯函数**并附单元测试：
   ```
   fn resample_linear(times: &[f64], values: &[f64], grid: &[f64]) -> Vec<f64>
   ```
   测试用例：①网格点正好命中样本 → 原值；②两样本之间 → 线性插值（手工算好期望值）；③网格早于首样本 → 取首样本；④晚于末样本 → 取末样本；⑤只有 1 个样本 → 全部等于该样本；⑥空输入 → 返回空（不 panic）。
   若走这条路，请在代码注释里写明"为什么不用 Polars asof"，并保留 `resample_hz` 对外的行为契约不变。
- 删除 Step 2b 里那句 `Err(CoreError::bad_config("重采样将在 Step 2c 实现"))` 占位。

## 3. 测试（扩展 `tests/e2e_synthetic.rs`）

沿用既有合成素材（Ch1 `VehSpd` @10ms×500、Ch1 `BattTemp` @100ms×50、Ch2 `Insulation` @1000ms×5，共 5 秒）：

1. **xlsx 拆表**：`Merged_Report.xlsx` 存在、`report.tables.len() == 3`、每个 `sheet` 名 ≤31 字符且互不相同、`format_used == Xlsx`、`downgraded == false`。（Rust 侧不引入解压依赖，只断言结构与文件存在；内容核验由外部 Python `zipfile` 脚本做。）
2. **xlsx 宽表**：生成单个 `Merged_All_Signals.xlsx`，`tables.len() == 1`。
3. **重采样（拆表）**：`resample_hz = Some(10.0)`（网格定义：**从该表 t0 起，步长 1/hz，取不超过 t1 的最大点**，即 `floor((t1-t0)*hz) + 1` 个点）。
   本素材的末样本时间是 `(帧数-1)×周期`，不是 5.0 秒，所以期望行数是：
   - `VehSpd`（t_last=4.99）→ `floor(49.9)+1` = **50**
   - `BattTemp`（t_last=4.9）→ `floor(49.0)+1` = **50**
   - `Insulation`（t_last=4.0）→ `floor(40)+1` = **41**
   断言时允许 ±1 行的浮点边界差异，但**必须在测试注释里写明实际用到的 t0/t1 与计算式**。所有列 `null_count() == 0` 是硬要求。
4. **重采样（宽表）**：`split_by_message=false` + `resample_hz=Some(10.0)` → 单表 **50 行（允许 ±1）**、无空值。
5. **线性插值正确性**：构造一条只有两个样本的输入（`t=0 → 0.0`、`t=1 → 10.0`），10 Hz 重采样后 `t=0.5` 处的值必须是 **5.0**（浮点容差 1e-9）。
6. **ffill（保持原始时序 + ffill=true）**：宽表模式下断言内部空值被前向填充（用只有单点样本的信号验证非首行不为 null）。

## 4. 自检（必须跑，贴真实输出）

```
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -40
```
（`DYLD_FALLBACK_LIBRARY_PATH` 必须带，见 DEPS.md §0；dyld 报错是环境问题，**不要改代码**。）
失败就修到全绿为止。另外跑 `cargo clippy -p fast_blf_core 2>&1 | tail -15` 并贴出。

## 5. 回复格式（只回复这些）

- 改动文件与行数、`cargo test` 真实输出（测试名 + 通过/失败数）、clippy 摘要、偏离说明（没有就写"无偏离"）。
