# Step 7d 任务书（相对时间列按微秒取整）—— 交给 DSH headless 执行

你是 Rust 系统工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先读：`docs/INTERFACE.md` §1.1 的 `time_format` 语义（**刚更新，最后一条是本次要求**）、`docs/DEPS.md` §8、`crates/fast_blf_core/src/exporter.rs`。

## 背景（主模型实测发现）

时间口径 4 种都实现了，`absolute` 与 `unix_s` **精确命中**独立定值。但 `relative_s` 输出是
`0.009999990463256836`（期望 `0.01`），`relative_ms` 输出 `9.999990463256836`（期望 `10`）。

**已查明的真相（不是实现的 bug，也不是夹具的问题）**：
- 夹具里每帧时间戳精确等于期望值（用 python-can 独立读回，差值 `0.000e+00`）；
- 但 BLF 时间戳是 epoch 量级（~1.76e9），该量级下 f64 的 ulp ≈ 2.4e-7 s，相邻帧相减必然带出表示噪声；
- 因此**不能靠数学手段恢复**精确的 10ms，只能控制写出格式。

## 本次只做 Step 7d，只允许改这两个文件

1. `crates/fast_blf_core/src/exporter.rs`
2. `crates/fast_blf_core/tests/e2e_synthetic.rs`

不要动 `gui/`、`docs/`、`Cargo.toml`、`decoder.rs`、`lib.rs`。

## 1. 要求

在**写出前**对时间列做取整（只针对这两种口径，`unix_s`/`absolute` 一律不取整）：

| 口径 | 取整规则 | 目的 |
|---|---|---|
| `relative_s` | `(v * 1e6).round() / 1e6` | 微秒精度，`0.01` 输出就是 `0.01` |
| `relative_ms` | `(v * 1e3).round() / 1e3` | 同上（1e-3 ms = 1µs） |

要点：
- 用 `.round()`（四舍五入）而不是截断；取整必须**单调**（不能把相邻帧顺序弄反，也不能产生重复时间戳把两帧压成一行）
- 取整只作用于输出列，**不得**影响合并/去重/重采样用的内部时间戳（否则会改变合并语义）
- 取整后仍需保证：单文件首帧为 `0`、多文件全局首帧为 `0`、整体单调不减

## 2. 测试（`tests/e2e_synthetic.rs`，期望值见 `docs/DEPS.md` §8，**禁止改期望值**）

把既有的 `timefmt_relative_s` / `timefmt_relative_ms` 断言改成**精确相等**（不再是容差）：
- `relative_s`：第 0 帧 `0`、第 1 帧 `0.01`、第 100 帧 `1`（浮点精确相等）
- `relative_ms`：`0` / `10` / `1000`（精确相等）
新增：
1. `timefmt_relative_rounding_is_monotonic`：对 101 帧断言时间列**严格递增**（不存在相等或倒序）
2. `timefmt_unix_and_absolute_not_rounded`：`unix_s` 仍是 `1760623800.123456` 这类原值（断言打印字符串含 6 位小数且不等于取整后的值）；`absolute` 字符串不变
3. 回归：全部既有测试仍绿（尤其合并/去重、重采样那些用例）

## 3. 自检（必须跑，贴真实输出）

```
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -45
```
失败就修到全绿。（DYLD 变量必带；dyld 报错是环境问题，不要改代码。）

## 4. 回复格式（只回复这些）

- 改动文件与行数、`cargo test` 真实输出、偏离说明（没有就写"无偏离"）。
