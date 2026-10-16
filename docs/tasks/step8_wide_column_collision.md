# Step 8 任务书（宽表同名信号列冲突 + panic 兜底）—— 交给 DSH headless 执行

你是 Rust 系统工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先读：`docs/INTERFACE.md` §1.1（宽表语义）与 §5 硬约束、`crates/fast_blf_core/src/exporter.rs`（宽表两条分支）。

## 背景（主模型实测复现，真实用户的工作流会踩）

用 `fixtures/bench_large.blf`（20 个报文，每个都有 `Sig0..Sig7` —— 即**不同报文里信号同名**）+ `fixtures/bench.dbc` 跑宽表模式：

| 路径 | 结果 |
|---|---|
| 宽表 · **保持原始时序** | ❌ **Rust panic**：`PanicException: called Result::unwrap() on an Err value: ColumnNotFound("unable to find column \"MSG_101_Sig0\"")`，位置 `polars-plan-0.55.2/src/plans/optimizer/projection_pushdown/mod.rs:954` |
| 宽表 · 保持原始时序 + ffill | ❌ 同样的 panic |
| 宽表 · 10Hz 重采样 | ✅ 成功，且正确给出 warnings：``宽表列名冲突：`Sig0` 同时出现在多个报文中，已重命名为 `MSG_101_Sig0` `` |
| 拆表模式 | ✅ 22 张表正常 |

结论：**同一个功能的两条分支行为不一致** —— 重采样分支做了列名消歧，保持原始时序分支没做（且报错里出现两个 `timestamp` 列，说明水平拼接时没统一列名），最后由 Polars 内部 `unwrap()` 炸成 panic。

真实 DBC 里跨报文同名信号（`Counter` / `CRC` / `Alive` / `Sig0`）极其常见，**这会让普通用户一选宽表就崩**，必须修。

## 本次只做 Step 8，允许改这些文件

1. `crates/fast_blf_core/src/exporter.rs`
2. `crates/fast_blf_core/src/lib.rs`（只在需要接错误码时）
3. `crates/fast_blf_core/src/error.rs`（新增错误码）
4. `crates/fast_blf_core/examples/make_fixtures.rs`（新增冲突夹具）
5. `crates/fast_blf_core/tests/e2e_synthetic.rs`

不要动 `gui/`、`docs/`、`Cargo.toml`、`decoder.rs`。

## 1. 正确性要求（两条分支行为必须一致）

宽表模式下，**信号列名**一律按下面的规则确定（与重采样分支现有的 warnings 文案保持一致）：

- 某信号名在**多于一个报文**中出现 → 改名为 `{报文名}_{信号名}`（如 `MSG_101_Sig0`），并给出一条 warning（文案沿用现有：``宽表列名冲突：`Sig0` 同时出现在多个报文中，已重命名为 `MSG_101_Sig0` ``）
- 只在单一报文出现的信号 → 保持原名
- **`timestamp` 列只能有一个**：宽表里所有信号共用第一列 `timestamp`（这是 §1.1 的硬要求）
- 拆表模式**不受影响**（子表内信号保留原名，表名已区分报文）

## 2. 禁止 panic（兜底）

即使 Polars 内部再出意外，**也必须变成结构化错误**，不能把 `PanicException` 抛给 Python：

- 导出流水线外面包一层 `std::panic::catch_unwind`（注意 `AssertUnwindSafe`），把 panic 转成错误码 **`EXPORT_INTERNAL`**，message 里带：阶段名（merge/export）、涉及的表名、以及 panic 的字符串（截断到合理长度）
- `catch_unwind` 要覆盖**整个导出阶段**（合并 + 重采样 + 写文件），因为这些都在同一个调用栈里
- 参考现有错误码风格（见 `error.rs` 与 `docs/INTERFACE.md` §4）

## 3. 夹具与测试

**新增夹具**（`make_fixtures.rs`）：`fixtures/collide.blf` + `fixtures/collide.dbc`
- 两个报文：`MsgA`（周期 10ms，10 帧）与 `MsgB`（周期 50ms，10 帧），**两者都有同名信号 `Counter`**；`MsgA` 另有独有信号 `OnlyA`
- 帧时间戳从 0 开始，信号值给成可验证的整数（如 Counter 递增、OnlyA = 100 + i）

**测试**（`tests/e2e_synthetic.rs`，全部要用真实 `convert` 入口，不许绕）：

1. `wide_keep_timeline_with_duplicate_signal_names_does_not_panic`
   → 宽表 + `resample_hz=None`，断言：**不抛异常**、只有一张表、首列名为 `timestamp` 且只有一个 timestamp 列、列里含 `MsgA_Counter` 与 `MsgB_Counter`、`OnlyA` 保持原名、warnings 里有两句重命名提示
2. `wide_resample_with_duplicate_signal_names_does_not_panic`
   → 宽表 + 10Hz，同上断言（两条分支结果列名必须一致）
3. `wide_duplicate_names_ffill_does_not_panic`
   → 宽表 + 原始时序 + `ffill=true`
4. `split_mode_keeps_plain_signal_names`
   → 拆表模式，断言子表内的列名仍是 `Counter`（不带报文名前缀）
5. `export_internal_error_is_returned_instead_of_panic`
   → 直接对兜底包装做单测：传一个必然 panic 的闭包，断言返回的是 `CoreError`（错误码 `EXPORT_INTERNAL`）而不是让 panic 逃逸
6. 回归：全部既有测试仍绿（特别是原有的宽表 / 重采样 / 防呆用例）

## 4. 自检（必须跑，贴真实输出）

```
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -50
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo run -p fast_blf_core --example make_fixtures 2>&1 | tail -3
ls -l fixtures/collide.*
```
失败就修到全绿。（DYLD 变量必带；dyld 报错是环境问题，不要改代码。）

## 5. 回复格式（只回复这些）

- 改动文件与行数、`cargo test` 真实输出、夹具生成输出、偏离说明（没有就写"无偏离"）。
