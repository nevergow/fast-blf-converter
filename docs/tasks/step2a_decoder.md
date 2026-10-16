# Step 2a 任务书（decoder.rs + scan）—— 交给 DSH headless 执行

你是 Rust 系统工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先完整读：
- `docs/INTERFACE.md`（§1 `scan` 的返回结构、§2 回调协议、§3 seam、§4 错误码）
- `docs/DEPS.md`（**§1 的 `blf_asc` API 与通道 1-based 铁律、帧 ID 归一化铁律**、§2 `can-dbc` API）
- `crates/fast_blf_core/src/bit_slice.rs`（已实现并经测试，**直接复用，不要重写位运算**）
- `crates/fast_blf_core/src/error.rs`

## 本次只做 Step 2a，只允许新建/修改这两个文件

1. 新建 `crates/fast_blf_core/src/decoder.rs`
2. 修改 `crates/fast_blf_core/src/lib.rs`（加 `mod decoder;` 与 `scan` pyfunction）

不要动 `bit_slice.rs`、`error.rs`、`gui/`、`exporter.rs`（还不存在）、`Cargo.toml`。

## 1. decoder.rs 必须提供

```rust
// 一个已编译的信号（从 DBC 预编译，不在热路径做字符串比较）
pub struct SignalPlan { pub name: String, pub start_bit: u32, pub bit_len: u32,
                        pub order: ByteOrder, pub is_signed: bool,
                        pub factor: f64, pub offset: f64, pub unit: String,
                        pub min: Option<f64>, pub max: Option<f64> }

pub struct MessagePlan { pub channel: u32,      // 1-based！入口处已转换
                         pub frame_id: u32,     // 归一化：id | (is_extended ? 1<<31 : 0)
                         pub name: String, pub is_fd: bool,
                         pub signals: Vec<SignalPlan> }

pub struct DecodePlan { /* (channel, frame_id) -> MessagePlan 的 HashMap，O(1) */ }
impl DecodePlan {
    pub fn build(dbc_by_channel: &BTreeMap<u32, PathBuf>, selected: &Selection) -> Result<Self, CoreError>;
    pub fn lookup(&self, channel: u32, frame_id: u32) -> Option<&MessagePlan>;
    pub fn is_empty(&self) -> bool;
    pub fn messages(&self) -> impl Iterator<Item = &MessagePlan>;
}

// 统计一个文件。**阶段语义必须分清（这是 scan 与 convert 的关键区别）**：
//   * scan 阶段：只读帧头，统计**所有**出现的 (channel, frame_id) 计数 + 时间戳中位数间隔；
//     不做任何信号位提取（"零反序列化开销"指的就是不解信号值）。
//     必须统计全部报文，因为 scan 要报告"没绑 DBC 的通道/报文"（否则 GUI 树里看不到它们）。
//   * convert 阶段：只有 `plan.lookup(ch,id)` 命中的才解码信号值。
//     即"即时过滤"发生在 **convert**，不在 scan。
pub struct FileScan { pub path: String, pub size: u64, pub ok: bool, pub error: Option<String>,
                      pub frame_counts: HashMap<(u32,u32), u64>,     // 该文件里出现的**全部** (ch,id)
                      pub cycle_estimate: HashMap<(u32,u32), f64> }  // 中位数帧间隔(ms)，样本<3 则缺省
pub fn scan_file(path: &Path, plan: &DecodePlan, progress: &dyn Fn(Progress)) -> FileScan;
```

要点：
- **多通道 DBC 隔离**：`dbc_by_channel` 的 key 是 1-based 通道号；同一 frame_id 在不同通道必须各自独立命中。
- **帧 ID 归一化**：`msg.arbitration_id.0 | if msg.is_extended_id { 1<<31 } else { 0 }`，与 `can_dbc::MessageId::raw()` 对齐（见 DEPS.md §2）。**注意 0x123 这类标准帧绝不能和扩展帧混淆**。
- **通道转换只在入口做一次**：`let ch = msg.channel as u32 + 1;`，之后所有地方都用 1-based。
- **即时过滤**：`plan.lookup(ch, id)` 为 None 时立刻 `continue`，不得访问 `msg.data`、不得做任何反序列化/拷贝。
- 错误处理：单个文件打不开或解析失败 → 记进 `FileScan.error`（`BLF_OPEN`/`BLF_PARSE` 码），**不要**让整个批量任务失败；`reader.take_error()` 的告警累积成一个字符串。
- 周期估计：对每个 (channel,id) 收集时间戳差值，取**中位数**换算成 ms（用 Vec<f64> 排序取中位；样本 <3 返回 None）。不要用平均值（周期抖动会带偏）。
- `Progress` 结构按 INTERFACE.md §2 的字段（phase/stage/done/total/file/message），并且**节流**：同一个文件内最多每 50ms 回调一次（用 `std::time::Instant` 判断），不要每帧都回调。

## 2. lib.rs 增加 `scan`

```python
fast_blf_core.scan(blf_paths: list[str], dbc_by_channel: dict[int, str], progress_cb=None) -> dict
```
- 返回结构**严格**按 INTERFACE.md §1：`{"files": [...], "channels": {"1": {"dbc":.., "dbc_error":.., "messages":[{"id","id_hex","name","cycle_ms","frame_count","is_fd","signals":[...]}]}}}`。
- `id` 是**归一化后的 raw id**（含 bit31），`id_hex` 用 `"0x{:X}"` 格式；GUI 直接显示它。
- 没有 DBC 绑定的通道：仍然要出现在 `channels` 里，`dbc` 为 null，报文名用 `ID_0x1A2`。
- 有 DBC 但 DBC 解析失败：该通道 `dbc_error` 填错误串，`messages` 为空数组，**不要抛异常中断**（GUI 要能显示错误）。
- 文件级并发用 `rayon`（`blf_paths.par_iter()`），但 `progress_cb` 只能在**一个**线程里被调用 —— 用 `Mutex<Instant>` + 主线程内串行回调，或把进度聚合成原子计数后在收集阶段统一回调。**绝不允许并发调用 Python 回调**（会崩）。
- 重活全程包在 `py.allow_threads(|| ...)` 里；`progress_cb` 用 `Python::attach`（pyo3 0.30 的 API，按 DEPS.md §5）获取 GIL 调用。
- 回调抛异常：捕获并忽略（记入 warnings），**不得** abort 进程。

## 3. 必须自带的测试（写在 decoder.rs 的 `#[cfg(test)] mod tests`）

用 `blf_asc::BlfWriter` 生成合成 BLF（**这就是我们的测试素材来源，不依赖任何外部文件**），场景固定如下：

| 通道 | 报文 | ID | 周期 | 信号 |
|---|---|---|---|---|
| Ch1 | `VehSpd` | `0x123`（标准帧） | 10 ms | `Spd`（Intel, unsigned, start=0, len=16, factor=0.01, unit km/h）、`SpdValid`（Intel, unsigned, start=16, len=1） |
| Ch1 | `BattTemp` | `0x1A2`（标准帧） | 100 ms | `Temp1`（**Motorola, signed, start=7, len=8** → i0=0，整字节恒等，便于断言）、`Temp2`（Motorola, unsigned, start=4, len=12） |
| Ch2 | `Insulation` | `0x200`（标准帧） | 1000 ms | `Res`（Intel, unsigned, start=0, len=32, factor=1.0, unit Ω） |

生成 5 秒数据：`VehSpd` 500 帧、`BattTemp` 50 帧、`Insulation` 5 帧（时间戳 = i × 周期）。
DBC 文本以 Rust 字符串常量写在测试里（`VERSION`/`BO_`/`SG_` 三行即可，factor/offset 用 `(0.01,0)` 这种写法）。

**断言（写死期望值）：**
1. `DecodePlan::build` 后 `messages()` 数量 = 3；三个 (通道,ID) 都能 O(1) 命中。
2. `scan_file` 后 `frame_counts`：`(1,0x123)=500`、`(1,0x1A2)=50`、`(2,0x200)=5`。**特别断言 `(1,0x123)` 不等于 0** —— 防通道号 off-by-one（BLF 内部 1-based → blf_asc 给 0-based → 我们必须 +1 还原）。
3. `cycle_estimate`：(1,0x123)≈10.0ms、(1,0x1A2)≈100.0ms、(2,0x200)≈1000.0ms（浮点容差 1e-6）。
4. Motorola 信号解码：构造一帧 `BattTemp`，`data[0]=0xF6`（= -10 有符号），断言解出 **-10.0** —— 防大端 + 补码同时出错。
5. **scan 与 convert 的过滤语义必须分开验证**（这是两个阶段的关键区别）：
   - `scan_file` 的 `frame_counts` 必须包含**全部**出现的 `(ch,id)`：构造只给 Ch1 绑 DBC 的场景，断言 map 里同时有 `(1,0x123)`、`(1,0x1A2)`、`(2,0x200)` —— 通道 2 没绑 DBC 也必须被统计到（否则 GUI 树里看不到它）。
   - `DecodePlan::build` 只收录**被选中**的报文：构造一个只勾选 `VehSpd` 的 Selection，断言 `plan.messages()` 只有 1 项，且 `plan.lookup(1, 0x1A2)` 返回 `None`（这保证 convert 阶段对未勾选报文就地跳过、零信号解码开销）。

## 4. 自检（必须跑，贴真实输出）

```
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -30
```
（`DYLD_FALLBACK_LIBRARY_PATH` 必须带：测试二进制链接 anaconda 的 libpython，非交互 shell 里不加会 dyld SIGABRT，见 `docs/DEPS.md` §0。**看到 dyld 报错不要改代码，那是环境问题**。）
若出现 `Blocking waiting for file lock on build directory`，等待即可（有另一个编译在跑），**不要删 target、不要中断**。

## 5. 回复格式（只回复这些）

- 两个文件各多少行、`cargo test` 真实输出（通过/失败数）、偏离说明（没有就写"无偏离"）。
