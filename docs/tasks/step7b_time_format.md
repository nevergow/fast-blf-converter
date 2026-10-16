# Step 7b 任务书（时间列口径 4 选 1）—— 交给 DSH headless 执行

你是 Rust 系统工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先完整读（**必读，含全部定值**）：
- `docs/INTERFACE.md` **§1.1 的 `time_format` 行与其后的语义四条**（严格照做）
- `docs/DEPS.md` **§8 BLF 文件头布局与时间口径**（含偏移 40 的 8×u16 与独立验算好的定值表）
- `crates/fast_blf_core/src/decoder.rs`、`exporter.rs`、`lib.rs`、`examples/make_fixtures.rs`、`tests/e2e_synthetic.rs`

## 背景（用户需求原话）

> "可以选择导出的时间类型和 canalyzer 一样。"

用户已拍板：要 4 种口径全给、绝对时间**按文件头墙钟原样输出**（与 CANalyzer 对得上）。
现状问题：我们直接把 `blf_asc` 的 `Message.timestamp`（= 头部按 **UTC** 解释的 epoch 秒）写进了 CSV，对真实文件来说是"epoch 级大数"，既不是相对时间、也不是 CANalyzer 那套墙钟。

## 本次只做 Step 7b，允许改这些文件

1. `crates/fast_blf_core/src/decoder.rs`（读文件头 + 每文件时间基准）
2. `crates/fast_blf_core/src/exporter.rs`（按口径改写 `timestamp` 列）
3. `crates/fast_blf_core/src/lib.rs`（config 校验 + 把每文件的头部信息传给 exporter）
4. `crates/fast_blf_core/examples/make_fixtures.rs`（新增 `timefmt.blf` 夹具）
5. `crates/fast_blf_core/tests/e2e_synthetic.rs`（补测试）
6. `crates/fast_blf_core/Cargo.toml`（**只允许**新增 `chrono`，它已是 `blf_asc` 的传递依赖，无需下载新包；不要动别的依赖）

不要动 `gui/`（另有任务）、`docs/`、`packaging/`、`.github/`。

## 1. 读文件头墙钟（`decoder.rs`）

```rust
pub struct WallClock { pub year: u16, pub month: u16, pub day: u16, pub hour: u16, pub minute: u16, pub second: u16, pub millis: u16 }
pub struct FileTiming { pub path: PathBuf, pub wall: Option<WallClock>, pub header_epoch: f64 }
pub fn read_file_timing(path: &Path) -> FileTiming
```
- 读文件前 144 字节；校验 magic `LOGG`；从**偏移 40** 起读 8 个 **LE u16**：`(year, month, dow, day, hour, minute, second, millis)`（`dow` 丢弃）
- 无效判定：magic 不符 / 读不到 16 字节 / `year < 1601` / `month/day/hour/minute/second` 越界 → `wall = None`
- `header_epoch`：把 `wall` 按 **UTC** 解释成 epoch 秒（用 chrono：`DateTime::from_naive_utc_and_offset` → `timestamp()`）；`wall=None` 时用该文件第一帧的 `msg.timestamp` 兜底（保证 `unix_s`/`relative_*` 不受影响）
- **绝对时间必须用 `wall` 原样格式化，禁止用 `header_epoch` 转本地时间**（否则差一个时区）

## 2. 按口径改写 `timestamp` 列（`exporter.rs`）

`time_format` 四种取值的**唯一**定义（列名统一 `timestamp`，且**永远是第一列**）：

| 取值 | 计算 | 类型 |
|---|---|---|
| `relative_s` | `msg.timestamp - global_min` | f64（秒） |
| `relative_ms` | `(msg.timestamp - global_min) * 1000.0` | f64（毫秒） |
| `unix_s` | `msg.timestamp`（原样，不改） | f64 |
| `absolute` | `wall(该帧所属文件) + (msg.timestamp - header_epoch(该文件))` → `%Y-%m-%d %H:%M:%S.%6f` | Utf8 |

- `global_min` = 本次任务**全部输入文件全部帧**的最小 `msg.timestamp`（解码期用原子变量取全局限，不要在导出期重算）
- 一个分片内的所有行都来自同一个文件（分片按 `(file, channel, message)` 写），所以**按分片**取该文件的 `FileTiming` 即可；拆表模式与宽表模式都要正确
- `absolute` 的微秒精度：用 **i64 微秒整数运算**（`wall_us + round((msg.timestamp - header_epoch) * 1e6)`），避免浮点漂移导致末位乱跳
- `absolute` 模式下若**任一输入文件**的 `wall == None` → 返回错误 `BAD_TIME_HEADER`，message 里点名该文件（**不要**静默降级成别的口径）
- config 里 `time_format` 非法值 → `BAD_CONFIG`，message 列出 4 个合法值
- 缺省（config 没给这个 key）→ `relative_s`

## 3. 夹具与测试

**新增夹具**（`make_fixtures.rs`）：`fixtures/timefmt.blf`，100 帧、10ms 周期、单通道、单报文（报文名 `TimeMsg`、信号 `Val`），**首帧时间戳 `X = 1760623800.123456`**。
夹具生成后**立刻断言**自己写出的文件头 8×u16 == `(2025, 10, 4, 16, 14, 10, 0, 123)`（DEPS §8 的定值）——这条断言是为了证明夹具本身符合预期。

**测试**（`tests/e2e_synthetic.rs`，期望值直接用 DEPS §8 表格里的字符串，**禁止改期望值**）：

1. `timefmt_relative_s`：第 0 帧 `0`, 第 1 帧 `0.01`, 第 100 帧 `1.0`（±1e-9）
2. `timefmt_relative_ms`：对应 `0`, `10`, `1000`（±1e-6）
3. `timefmt_unix_s`：`1760623800.123456` / `1760623800.133456` / `1760623801.123456`（±1e-5）
4. `timefmt_absolute`：**字符串完全相等** `2025-10-16 14:10:00.123456` / `2025-10-16 14:10:00.133456` / `2025-10-16 14:10:01.123456`
5. `timefmt_column_is_first_and_named_timestamp`：四种模式下列名与列序一致
6. `timefmt_relative_is_global_across_files`：把 `timefmt.blf` 复制一份、把第二份的时间整体后移（如再写一个 `timefmt2.blf` 起点 +10s）→ 两文件一起转，`relative_s` 的全局最小为 0 且**单调递增**（不被第二个文件的起点打乱）
7. `timefmt_bad_header`：造一个头部 systemtime 全 0 的文件（直接把 144 字节里偏移 40 起的 16 字节清零即可）→ `absolute` 返回 `BAD_TIME_HEADER`；`relative_s` 仍能正常出数
8. 回归：现有全部测试仍绿（尤其原有 `case02_split_csv_has_no_nulls_in_any_column` 等断言"第一列 == timestamp"的用例）

## 4. 自检（必须跑，贴真实输出）

```
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -50
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo run -p fast_blf_core --example make_fixtures 2>&1 | tail -5
```
（DYLD 变量必带；dyld 报错是环境问题，不要改代码。）失败就修到全绿。

## 5. 回复格式（只回复这些）

- 改动文件与行数、`cargo test` 真实输出、夹具生成输出、偏离说明（没有就写"无偏离"）。
