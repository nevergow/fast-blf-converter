# 依赖 API 事实清单（实测自 crate 源码，禁止凭记忆改写）

> 本文件是 Step 1/2/3 实现的**唯一可信 API 依据**。任何"我记得 API 是这样的"都必须回到本文件核对；
> 与本文件冲突的写法一律以本文件为准。所有版本号都已从 crates.io 实测确认存在。

## 0. 工具链（已装好，实测）

| 项 | 值 |
|---|---|
| rustc / cargo | **1.99.1**（2026-10-13），路径 `~/.cargo/bin` |
| 导入方式 | 每步命令前置 `source ~/.cargo/env`（安装时用了 `--no-modify-path`） |
| Python | `/Users/wuzeqin/Projects/fast_blf_converter/.venv/bin/python`（3.12.7，system-site-packages） |
| PySide6 | 6.10.0（venv 可见，不要再装） |
| maturin | 1.15.0（已装在 venv） |
| 目标 edition | workspace = **2021**；注意 `blf_asc` 自身是 edition 2024，因此需要 rustc ≥ 1.85（满足） |

**跑测试的唯一正确命令（本机 macOS）**：

```bash
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core
```

为什么要那个环境变量：本机 pyo3 链接的是 anaconda 的 Python（`/opt/anaconda3/lib/libpython3.12.dylib`），
测试二进制里只有 `@rpath/libpython3.12.dylib` 且 rpath 不含 anaconda 目录，非交互 shell 里会
`dyld: Library not loaded ... SIGABRT`。conda 激活过的登录 shell（`zsh -lic`）自带
`DYLD_FALLBACK_LIBRARY_PATH` 所以能跑通 —— **不要因此以为代码有问题，补上这个变量即可**。
（`.cargo/config.toml` 里的 `[env]` 段不会被传递给测试二进制执行，别指望它。）

`.venv` 是**唯一**允许的 Python 环境（不要 `pip install` 到 anaconda base）。

## 1. BLF 解析：`blf_asc 0.2.0`（GitHub: chirikoro/blf_asc，MIT/Apache-2.0）

**为什么选它（实测排除了另两个候选）：**
- `ablf 0.2.1` 的对象枚举只有 `CanMessage86 / CanErrorExt73 / LogContainer10 / AppText65 / Unsupported`，**不支持 CAN FD**（缺 100/101 对象）→ 淘汰。
- `autors-blf 0.1.1` 功能全但 2026-08 才发布、下载量 89，风险高 → 备用。
- `blf_asc` **同时提供 reader 与 writer**（writer 用来生成合成 BLF 测试素材，一举两得），且显式支持 CAN FD。

```rust
use blf_asc::{BlfReader, BlfWriter, Message, DataBytes, ArbitrationId};

let mut reader = BlfReader::open(path)?;          // 流式，别一次性读全文件
while let Some(msg) = reader.next_message()? {   // Result<Option<Message>>
    // msg: Message
}
// 非致命解析错误用 reader.take_error() 取（Option<BlfError>），不要让它中断整个文件

// 写合成素材：
let mut w = BlfWriter::create_with_level(path, 6)?;   // 或 BlfWriter::create(path)
w.on_message_received(&msg)?;
w.finish()?;                                          // 必须调用，否则文件不完整
```

`Message` 的真实字段（源码 204-217 行）：

```rust
pub struct Message {
    pub timestamp: f64,          // 秒（绝对时间，含 start_timestamp 偏移，已换算）
    pub arbitration_id: ArbitrationId,   // .0 : u32 —— 不含扩展帧标志位
    pub is_extended_id: bool,
    pub is_remote_frame: bool,
    pub is_rx: bool,
    pub is_error_frame: bool,
    pub is_fd: bool,
    pub bitrate_switch: bool,
    pub error_state_indicator: bool,
    pub dlc: u8,                 // 数据实际长度（FD 的 dlc 已按表换算成字节数）
    pub data: DataBytes,         // .as_slice() / .to_vec() / .hex()
    pub channel: u16,            // ⚠ 0-based！BLF 容器里是 1-based，crate 内部做了 saturating_sub(1)
}
```

**⚠ 通道编号铁律（写错会静默把 DBC 挂到错误通道）：**
- `blf_asc` 给出 `channel = 0` 表示 **Ch1**。
- 契约/GUI 里一切通道号都是 **1-based**（Ch1~Ch8）。
- 全部代码统一在**入口处转换一次**：`let ch_1based = msg.channel as u32 + 1;`，之后所有 map/报告/列名都用 1-based。绝不允许混用。

**⚠ 压缩支持**：`blf_asc` 只依赖 `flate2`（zlib）。**LOG_CONTAINER 若用 LZMA/xz 压缩则无法解析**。处理要求：
- 解析失败时必须返回 `BLF_PARSE` 错误并带上文件名与原始错误串（不要静默跳过整个文件）。
- 在 `warnings` 里给出一条人话提示："该文件可能使用了不支持的 LZMA 压缩容器"。
- 这是我们唯一无法用合成素材覆盖的已知风险（真实 Vector 文件可能触发），必须留清晰错误路径。

**帧 ID 归一化铁律**（与 `can-dbc` 的 `MessageId::raw()` 对齐，见 §2）：
```rust
let frame_key_id: u32 = msg.arbitration_id.0 | if msg.is_extended_id { 1 << 31 } else { 0 };
// (channel_1based: u32, frame_key_id: u32) 作为 O(1) 查表的 key
```

## 2. DBC 解析：`can-dbc 10.0.0`

```rust
use can_dbc::{Dbc, Message as DbcMessage, Signal as DbcSignal, MessageId, ByteOrder, ValueType};

let dbc = Dbc::try_from(dbc_text.as_str())?;      // impl TryFrom<&str> for Dbc（源码 309-315 行）
// 或 std::str::FromStr 不可用；注意不是 ::from_str，也不是 DBCFile
let messages: &Vec<DbcMessage> = &dbc.messages;   // pub 字段，直接访问
```

`Message` 的 pub 字段：`id: MessageId`、`name: String`、`size: u64`（字节数）、`transmitter: Option<String>`、`signals: Vec<Signal>`

`Signal` 的 pub 字段：`name: String`、`multiplexer_indicator: MultiplexIndicator`、`start_bit: u64`、`size: u64`（位宽）、`byte_order: ByteOrder`、`value_type: ValueType`、`factor: f64`、`offset: f64`、`min: NumericValue`、`max: NumericValue`、`unit: String`、`receivers: Vec<String>`

枚举变体（实测）：
```rust
pub enum ByteOrder { LittleEndian, BigEndian }          // Intel / Motorola
pub enum ValueType { Signed, Unsigned }
pub enum MultiplexIndicator { Multiplexor, MultiplexedSignal(u64), /* ... 其它变体用 _ => 忽略 */ }
```

`MessageId` 的取值（源码 message_id.rs 17-26 行）：
```rust
impl MessageId {
    pub fn raw(self) -> u32 {     // ⚠ 扩展帧会或上 bit31
        match self {
            Self::Standard(id) => u32::from(id),
            Self::Extended(id) => id | 1 << 31,
        }
    }
}
```
→ 正好与 §1 的 `frame_key_id` 约定一致，(channel, raw_id) 即可 O(1) 命中。

`NumericValue` 是包装类型（不是 f64），转 f64 需走它的公开转换方法（编译时确认；不要假定 `as f64`）。

## 3. Polars：`0.55.2`（features 已实测存在）

facade 的 `default = [docs, zip_with, csv, polars-parquet, temporal, fmt, dtype-slim]`，我们**保留默认并追加**：
`lazy`、`parquet`、`csv`、`streaming`、`asof_join`、`interpolate`。

已实测存在的 API：
- `LazyFrame::with_streaming(bool)` ✅（polars-lazy/src/frame/mod.rs）
- `LazyFrame::sort_by_exprs`、`LazyFrame::unique(subset, keep_strategy)` ✅
- `Expr::interpolate(InterpolationMethod)` ✅（polars-plan/src，**是 Expr 方法不是 LazyFrame 方法**）
- `LazyFrame::join(other, left_on, right_on)` + `JoinType::AsOf(AsOfOptions)` ✅（枚举变体确认存在）

**⚠ 0.55.2 没有找到 `LazyFrame::join_asof(...)` 这个便捷方法**（全 crate 源码 grep 无此 pub fn）。
因此重采样实现顺序：
1. 首选：`join(...).how(JoinType::AsOf(AsOfOptions { .. }))` —— 以**编译通过**为准，字段名以编译器报错/`cargo doc` 为准，不要凭记忆写。
2. 备选（允许的偏离，需在代码注释里说明理由）：纯 Rust 实现网格重采样 —— 先把 `(timestamp, value)` 按时间排序，再对每个目标网格点用 `partition_point`/二分找前驱样本、做线性插值。该函数必须是**有单元测试的纯函数**。
3. 无论走哪条，"保持原始时序"必须完全不重采样。

**防呆与内存：** 分片落盘用 `ParquetWriter` + zstd（`zstd 0.14`）；合并用 `LazyFrame::scan_parquet` + `with_streaming(true)` + `collect()`。

## 4. Excel：`rust_xlsxwriter 0.99.1`（feature `constant_memory`）

已实测 API：
- `workbook.add_worksheet()` → `&mut Worksheet`
- `workbook.add_worksheet_with_constant_memory()` → 常量内存模式（**百万行必须用这个**）
- `worksheet.write_string(row, col, s)` / `write_number(row, col, n)`（返回 `Result<(), XlsxError>`，逐个写要处理返回值）
- `workbook.save(path)`

Sheet 名规范化：Excel 限制 31 字符，且禁止 `[ ] : * ? / \`。子表名 `Ch1_BMS_Voltage` 截断到 31 字符并替换非法字符，**保证同一 workbok 内不重名**（重名加 `_2` 后缀）。

## 5. PyO3 / maturin

- `pyo3 0.30.1`，用 **Bound API**（`&Bound<'_, PyModule>`、`Bound<'_, PyDict>`），不要写 0.20 之前的 `&PyModule` 老式签名。
- **释放 GIL 的 API 在 0.30 里叫 `Python::detach(|| ...)` / 重新获取叫 `Python::attach(|py| ...)`**（`allow_threads` 是旧名，别用）。实测正确写法：
  ```rust
  let result = py.detach(|| { /* 重活，可跑 rayon */ });
  // 回调里再拿 GIL：
  let _ = Python::attach(|py| -> PyResult<()> { /* 构造 PyDict 并回调 */ Ok(()) });
  ```
  Python 回调**绝不允许并发调用**：并行阶段传 no-op 闭包，回调统一在收集阶段由单个线程串行触发。
- `IntoPyObject` 是新 trait；`IntoPy` 已废弃。构造 dict 用 `PyDict::new(py)`。
- crate 里声明了 `extension-module` 可选 feature + `crate-type = ["cdylib","rlib"]`：
  - maturin 构建 / `maturin develop` 会自动启用 `extension-module`
  - `cargo test` **不要**启用它（否则 macOS 链接失败）
- 编译验证命令：
  - `cargo test -p fast_blf_core`（不带 feature）
  - `.venv/bin/maturin develop --features extension-module`（或 `maturin develop` 让它自动加）
  - `cargo build --release`

## 6. 其它已锁版本

`rayon 1.12.0`、`serde 1.0.229`（derive）、`serde_json 1.0.154`、`thiserror 2.0.21`、`zstd 0.14.2`、`tempfile 3`（dev）。

## 7. Motorola（大端）位序约定 —— 已用参考实现对拍验证，**必须照做**

结论（`extract_bits` 的 Motorola 分支）：

```rust
// DBC 的 start 是"字节内从 LSB 数"的位置 → 先转成 MSB0 索引
let i0 = 8 * (start_bit / 8) + (7 - (start_bit % 8));
// 信号各位从 MSB 到 LSB 占据连续的 MSB0 索引 i0 .. i0+bit_len-1
// 第 p 个 MSB0 索引 → byte = p/8, lsb0_bit = 7 - (p % 8)
let mut raw = 0u64;
for k in 0..bit_len {
    let p = i0 + k;
    let byte = (p / 8) as usize;
    let lsb0 = 7 - (p % 8);
    raw = (raw << 1) | (((data[byte] >> lsb0) & 1) as u64);
}
// 越界判据必须是 i0 + bit_len > data.len()*8，不是 start + bit_len
```

**为什么**：`cantools`（Python DBC 生态的权威实现）在 `Message._check_signal` 里对 big_endian 信号正是这么构建位树的：
`padding = start_bit(signal) * [None]`，其中 `utils.start_bit(sig) = 8*(sig.start//8) + (7 - sig.start%8)`，信号位随后**连续**排布。
即 DBC 的 Motorola start 位是"字节内 LSB 计数"的位置，而信号位在 MSB0 索引空间里连续。

**验证方式（可复现）**：把每个 `(start, len, signed, byte_order)` 组合写成单信号 DBC，用 `cantools` 的 `Message.encode()` 编出字节，再用本约定反解，比较是否得到原值。
随机 260 组 `(start,len,signed,intel/motorola)` × 每组 6 个随机值 = **1440 次对拍，0 不一致**。

**典型定值（任务书里锁死用）**：
| 输入 | 期望 raw | 说明 |
|---|---|---|
| `[0x12,0x34]` start=7 len=8 Motorola | `0x12` | 整字节恒等（i0=0） |
| `[0x12,0x34]` start=4 len=8 Motorola | `0x91` | i0=3，跨字节 |
| `[0x12,0x34]` start=4 len=12 Motorola，signed | `0x91A` → `-1766` | 跨字节 + 补码 |
| `[0xF6,0x00]` start=7 len=8 Motorola，signed | `0xF6` → `-10` | 8 位补码 |
| `[0x12,0x34]` start=0 len=8 Motorola | **`0x1A`** | 陷阱例：naive 实现给 0x12 或 0x00 |
| `[0xAB,0xCD]` start=4 len=12 Intel | `0xCDA` | 小端跨字节 |
| `[0xFF,0x0F]` start=0 len=12 Intel，signed | `0xFFF` → `-1` | 小端补码 |

Intel 分支的约定不变：start 是 LSB 位位置、LSB0 连续、第 i 位权值 2^i（同样经 1440 次对拍确认）。

## 8. BLF 文件头布局与时间口径（实测自 python-can 源码，权威）

python-can 的定义：`FILE_HEADER_STRUCT = struct.Struct("<4sLBBBBBBBBQQLL8H8H")`，size **72**，文件头整体补零到 **144** 字节。

| 偏移 | 类型 | 含义 |
|---|---|---|
| 0 | `4s` | magic `"LOGG"` |
| 4 | `u32` | headerSize（通常 144） |
| 8..15 | 8×`u8` | applicationId / 版本等 |
| 16..31 | 2×`u64` | fileSize、uncompressedFileSize |
| 32..39 | 2×`u32` | objectCount、未知 |
| **40** | **8×`u16`** | **measurementStartTime（SYSTEMTIME）** |
| 56 | 8×`u16` | lastObjectTime（SYSTEMTIME） |

SYSTEMTIME 的 8 个 u16 顺序（由 `timestamp_to_systemtime()` 的返回顺序确认）：
`(year, month, isoweekday()%7, day, hour, minute, second, milliseconds)`

**时间口径要点（关键，容易错）**：
- 文件头里存的是**录制机的墙钟时间**（没有时区信息）。
- `blf_asc` 与 python-can 都把它**当作 UTC** 换算成 epoch（`systemtime_to_timestamp`），因此 `Message.timestamp` 是"绝对 epoch 秒"。
- 想要**与 CANalyzer 对得上**的绝对时间，必须**直接读偏移 40 处的 8×u16 原样格式化**，不能走 `Message.timestamp` 再转本地时间（那会差一个时区偏移）。

**独立验算好的定值**（用作单元测试期望，禁止改）：
设夹具首帧时间戳 `X = 1760623800.123456`（epoch 秒），周期 10ms：

| 帧 | relative_s | unix_s | absolute（墙钟原样） |
|---|---|---|---|
| 第 0 帧 | `0.0` | `1760623800.123456` | `2025-10-16 14:10:00.123456` |
| 第 1 帧 (+10ms) | `0.01` | `1760623800.133456` | `2025-10-16 14:10:00.133456` |
| 第 100 帧 (+1s) | `1.0` | `1760623801.123456` | `2025-10-16 14:10:01.123456` |

对应头部 8×u16 应为：`(2025, 10, 4, 16, 14, 10, 0, 123)`（day_of_week=4=周四）。

## 9. DBC 文件的非 UTF-8 编码判别（用户真机文件实测，权威）

真实项目 DBC 常见的两种"非 UTF-8"来源，**处理方法完全不同，判错会把数据改坏**：

| 来源 | 字节特征 | 正确解码 | 反例（错误处理的后果） |
|---|---|---|---|
| 中文 Windows 上 CANdb++ 写出的中文注释/枚举值 | 高位字节**成串**（连续 ≥2 字节） | **GB18030** | 按 cp1252 解 → 中文变乱码（可解析，但注释/值表不可读） |
| 非 UTF-8 语言环境下写出的拉丁符号（`°C`、`µs`、`±`） | 高位字节**孤立**（单字节夹在 ASCII 中） | **Windows-1252** | 按 GB18030 解 → **吞掉后随的 ASCII 字符**：`"·C"`（0xB0 + 'C'）解成 `"癈"`，单位语义被破坏 |

**判别规则（已实现）**：严格 UTF-8 失败时，取"连续高位字节的最长游程" `max_run`：
`max_run >= 2` → GB18030；`max_run == 1`（全部孤立）→ Windows-1252。

**实测证据（用户文件 `CAN_Matrix_MAL_P35_Hybrid_V3.2.0_Hybrid.dbc`，73,545 B）**：
- 全文件仅 2 个非 ASCII 字节，均为**孤立** `0xB0`，位于 `"°C"` 单位串（第 154、157 行）→ 文件不合法 UTF-8
- `can-dbc` 在 UTF-8 校验阶段即失败（`valid_up_to=7597`，`stream did not contain valid UTF-8`），**未进语法解析**
- 转成 UTF-8 后立即可解析出 **39 条报文** ⇒ 编码是唯一阻断因素（空 `NS_` 段、无信号报文、FLOAT 属性、
  大数值 `4294967295`/`3221225472` 均不影响）
- 同一份字节的不同解码：`cp1252 → "°C"`（正确）／`GB18030 → "癈"`（把 `C` 当尾字节吃掉）



