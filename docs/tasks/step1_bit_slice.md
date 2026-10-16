# Step 1 任务书（Rust 位运算核心）—— 交给 DSH headless 执行

你是 Rust 系统工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写，已确认）。

先完整读这三个文件（**唯一事实来源，禁止凭记忆写 API**）：
- `docs/SPEC.md` §3.A（解码内核需求）
- `docs/INTERFACE.md` §3 §4（冻结的函数签名、错误模型、错误码）
- `docs/DEPS.md`（依赖版本与 API 实测事实；**§7 Motorola 位序约定是已用参考实现对拍验证过的，必须照做**）

## 本次只做 Step 1，只允许新建/修改这三个文件

1. 新建 `crates/fast_blf_core/src/error.rs`
2. 新建 `crates/fast_blf_core/src/bit_slice.rs`
3. 修改 `crates/fast_blf_core/src/lib.rs`（只加模块声明与异常映射，保留现有 `version()`）

不要动 `Cargo.toml`、`pyproject.toml`、`gui/`、`docs/`。（另有并行任务在写 `gui/`，**不要碰那目录**。）

## 1. error.rs

```rust
pub struct CoreError { pub code: String, pub message: String, pub detail: Option<String> }
```
用 `thiserror` 实现 `std::error::Error` + `Display`（Display 输出 `"{code}: {message}"`）。提供构造辅助函数与**稳定错误码常量**：`BLF_OPEN`、`BLF_PARSE`、`DBC_PARSE`、`NO_DBC_FOR_CHANNEL`、`EMPTY_RESULT`、`DISK_FULL`、`EXPORT_FAILED`、`BAD_CONFIG`。

## 2. bit_slice.rs —— 语义必须逐条照做（位序约定已对拍验证，别自己"修正"）

```rust
pub enum ByteOrder { Intel, Motorola }
pub fn extract_bits(data: &[u8], start_bit: u32, bit_len: u32, order: ByteOrder) -> Result<u64, CoreError>;
pub fn decode_physical(raw: u64, bit_len: u32, is_signed: bool, factor: f64, offset: f64) -> f64;
```

**Intel（小端）规则：**
- 越界/非法输入必须返回 `Err`（`BAD_CONFIG` 或 `BLF_PARSE` 码），**绝不 panic、绝不越界索引**：
  `bit_len == 0`、`bit_len > 64`、`start_bit + bit_len > data.len()*8`（用 u64 运算防溢出）都要报错。
- `start_bit` 是信号 **LSB** 的位位置（LSB0 连续编号）。第 i 位取自 `pos = start_bit + i`：`byte = pos/8`、`bit = pos%8`，`result |= ((data[byte] >> bit) & 1) << i`（第 i 位权值 2^i）。

**Motorola（大端）规则 —— 注意跨字节处的走法，这是全项目最容易写错的地方：**
- DBC 的 `start_bit` 是"**字节内从 LSB 数**"的位置。先把它转成 **MSB0 索引**：
  ```
  let i0 = 8 * (start_bit / 8) + (7 - (start_bit % 8));
  ```
- 信号的各位（**从 MSB 到 LSB**）占据 **连续的 MSB0 索引** `i0, i0+1, ..., i0+bit_len-1`。
- 第 `p` 个 MSB0 索引对应：`byte = p/8`、`lsb0_bit = 7 - (p % 8)`，取值 `(data[byte] >> lsb0_bit) & 1`。
- 累加方式：`result = (result << 1) | bit`（**先取出的位是 MSB**）。
- **边界检查必须用 `i0`**：`i0 + bit_len > data.len()*8` 要返回 `Err`（`start_bit + bit_len` 在 Motorola 下**不是**正确的越界判据，例如 `start=0,len=8` 时 `i0=7`，实际会用到第 1 个字节的第 7 位）。
- 参考实现对照：`cantools`（Python DBC 生态权威实现）的 `Message._check_signal` 就是按这个位树构建的；本项目的算法已用 1440 组随机 `(start,len,signed,intel/motorola)` 与之逐一对拍，0 不一致。
- `bit_len == 64` 时不能移位溢出（注意 `1u64 << 64`，用 `checked_shl` 或分支规避）。

**decode_physical 规则：**
- `is_signed == true`：按二进制补码还原。若 `bit_len < 64` 且 `raw` 的第 `bit_len-1` 位为 1，则符号扩展 `raw |= !0u64 << bit_len`，再 `raw as i64 as f64`。（`bit_len == 64` 时直接 `raw as i64`。）
- **先符号扩展，再算物理量**：`(value as f64) * factor + offset`。
- `is_signed == false`：`raw as f64 * factor + offset`。

## 3. lib.rs 修改

```rust
mod error;      mod bit_slice;
pub use error::{CoreError, /* 错误码常量 */};
pub use bit_slice::{ByteOrder, extract_bits, decode_physical};

// Python 侧异常类名必须是 CoreError（见 INTERFACE.md §4），
// Rust 结构体已占用该名字，故 PyO3 异常类型取名 CoreErrorPy，注册时用 "CoreError" 暴露：
pyo3::create_exception!(fast_blf_core, CoreErrorPy, pyo3::exceptions::PyException);
impl From<CoreError> for pyo3::PyErr { /* -> CoreErrorPy::new_err(format!("{code}: {message}")) */ }
```
并在 `#[pymodule] fn fast_blf_core(m: &Bound<'_, PyModule>)` 里 `m.add("CoreError", m.py().get_type::<CoreErrorPy>())?;`。保留现有 `version()`。

## 4. 单元测试（`#[cfg(test)] mod tests`，写在 bit_slice.rs 内）

断言里的期望值**必须与本表一致**（这些值是经参考实现对拍确认的）。如果你认为表里有算错的，**停下来在回复里指出，绝不许偷偷改断言让测试变绿**：

| # | 调用 | 期望 | 防什么坑 |
|---|---|---|---|
| 1 | `extract_bits([0xFF,0x00], 0, 8, Intel)` | `0xFF` | 基本小端整字节 |
| 2 | `extract_bits([0xAB,0xCD], 4, 12, Intel)` | `0xCDA` | 小端跨字节（msg=0xCDAB，(0xCDAB>>4)&0xFFF） |
| 3 | `extract_bits([0x12,0x34], 7, 8, Motorola)` | `0x12` | 大端整字节恒等（start=7 → i0=0） |
| 4 | `extract_bits([0x12,0x34], 4, 8, Motorola)` | `0x91` | 大端跨字节：i0=3 → byte0 的 bit4..bit0 再接 byte1 的 bit7..bit5 |
| 5 | `extract_bits([0x12,0x34], 4, 12, Motorola)` 再 `decode_physical(raw, 12, true, 1.0, 0.0)` | raw=`0x91A`，值 `-1766.0` | 大端跨字节 + 有符号补码同时正确 |
| 6 | `extract_bits([0xF6,0x00], 7, 8, Motorola)` 再 `decode_physical(raw, 8, true, 1.0, 0.0)` | raw=`0xF6`，值 `-10.0` | 大端 + 8 位补码 |
| 7 | `decode_physical(0xFFF, 12, true, 1.0, 0.0)` | `-1.0` | 12 位全 1 的补码 |
| 8 | `extract_bits([0x12,0x34], 0, 8, Motorola)` | **`0x1A`** | **陷阱例**：naive 实现会给出 0x12 或 0x00；正确值是 0x1A（i0=7 → byte0 bit0 作 MSB，再接 byte1 的 bit7..bit1）。这一条专门锁死"没有偷偷用 start 当 MSB 位置" |
| 9 | `extract_bits([0xFF;8], 0, 64, Intel)` | `u64::MAX` | 64 位边界不移位溢出 |
| 10 | `extract_bits([0u8;8], 60, 8, Intel)` | `Err(_)` | 小端越界报错不 panic |
| 11 | `extract_bits([0u8;1], 0, 8, Motorola)` | `Err(_)` | Motorola 的越界判据必须按 i0（i0=7 → 7+8=15 > 8） |
| 12 | `extract_bits([0u8;1], 0, 0, Intel)` | `Err(_)` | bit_len=0 非法 |
| 13 | `decode_physical(100, 16, false, 0.1, -40.0)` | `-30.0`（浮点用 `(a-b).abs() < 1e-9`） | factor/offset 顺序 |

每个测试上方写一行中文注释说明它在防什么坑。

## 5. 自检（必须跑，贴出真实输出）

```
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
cargo test -p fast_blf_core 2>&1 | tail -30
```
注意：不启用 `extension-module` feature；**target 目录可能正被另一个编译占用**，若出现 `Blocking waiting for file lock on build directory` 属于正常，**耐心等待，不要中断、不要删 target**。若等待超过 10 分钟仍无输出，如实报告即可。
另外跑 `cargo clippy -p fast_blf_core 2>&1 | tail -20` 并贴出（只报告，不要求零警告）。

## 6. 回复格式（只回复这些）

- 三个文件各多少行、`cargo test` 真实输出（通过/失败数）、`cargo clippy` 摘要、偏离说明（没有就写"无偏离"）。
