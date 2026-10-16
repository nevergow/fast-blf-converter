# Step 12 任务书（DBC 编码回退策略修正：cp1252 vs GB18030 判别）—— 交给 DSH headless 执行

你是 Rust 系统工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先读：`crates/fast_blf_core/src/dbc_norm.rs`、`crates/fast_blf_core/tests/e2e_synthetic.rs`、`docs/tasks/step9_dbc_normalize.md`。

## 问题（用户真机 DBC 的字节级证据，已双向复现）

用户的真实 DBC（`CAN_Matrix_MAL_P35_Hybrid_V3.2.0_Hybrid.dbc`，73,545 字节）解析失败的根因**不是语法**，而是：

- 文件里有 **2 个孤立的高位字节 `0xB0`**，位于两行单位串里：`SG_ PID05_TEngineCoolant : ... "°C" HV_Battery_NIO`
- 该字节是 **cp1252 / ISO-8859-1** 环境写出的 `°`（Vector 工具在非 UTF-8 语言环境下写出），因此整份文件**不是合法 UTF-8** → `can-dbc` 在 UTF-8 校验阶段就失败（`valid_up_to=7597`），根本没进语法解析
- 把文件转成 UTF-8 后（任意无损方式）立即能解析出 **39 条报文** ⇒ 编码是**唯一**阻断因素

**而 Step 9 当前的实现（UTF-8 失败即按 GB18030 解码）在这份文件上会“解析成功但语义错误”**：

```
原始字节:  ... "·C" ECU       （·= 0xB0，后跟 ASCII 的 C）
GB18030  →  ... "癈"  ECU     ← 0xB0 被当成 GB18030 首字节，把后面的 'C' 当作尾字节吃掉，单位被破坏
cp1252   →  ... "°C" ECU      ← 正确
```

即：GB18030 解码**会吞掉紧随其后的 ASCII 字符**，在“孤立高位字节 + ASCII”这种模式上是错的。

## 修复：按“高位字节的连续性”判别编码

在 `dbc_norm.rs` 里把“UTF-8 失败 → 一律 GB18030”改成：

1. 严格 UTF-8 解码成功 → 直接用（不变）。
2. 失败时，统计**连续高位字节（≥0x80）的最长游程** `max_run`：
   - `max_run >= 2` → 按 **GB18030** 解码（中文注释/枚举值在 GBK/GB18030 里必然是 ≥2 字节的连续序列）
   - 所有高位字节都是**孤立的**（`max_run == 1`）→ 按 **Windows-1252（cp1252）** 解码
     （这种模式在真实文件里几乎总是 `°` `µ` `±` `²` 之类的拉丁符号）
3. 两种情况都要写 note，且**要能区分**（GUI 悬停会显示）：
   - GB18030：`原文件不是 UTF-8，已按 GB18030 解码（检测到连续多字节序列，判定为中文编码）`
   - cp1252：`原文件不是 UTF-8，已按 Windows-1252 解码（检测到 N 处孤立高位字节，判定为 °C/µ 等符号）`
4. 依赖：`encoding_rs` 已有 `GB18030`，请改用/新增 `encoding_rs::WINDOWS_1252`（同一 crate，**不要**加新依赖）。

## 本次只做 Step 12，只允许改这两个文件

1. `crates/fast_blf_core/src/dbc_norm.rs`
2. `crates/fast_blf_core/tests/e2e_synthetic.rs`

不要动 `gui/`、`docs/`、`Cargo.toml`、`decoder.rs`、`lib.rs`、`exporter.rs`。

## 测试要求（全部要有，期望值不许改）

1. **`dbcnorm_isolated_degree_sign_uses_cp1252`（本次核心用例，直接复现用户文件）**
   构造一个 DBC，其中一行是（按 **cp1252 编码写出**，即 `°` 就是单字节 `0xB0`）：
   ` SG_ PID05_TEngineCoolant : 48|8@1+ (0.75,-48) [-48.00|143.25] "°C" HV_Battery_NIO`
   断言：
   - 解析成功（不报 DBC_PARSE）
   - 该信号的**单位字符串精确等于 `"°C"`**（**不是** `癈`、**不是** U+FFFD、**不是** `�C`）
   - 规范化 note 里出现 `Windows-1252`
   - 报文数与信号数正确（别把整行吃掉）
2. **`dbcnorm_cjk_run_still_uses_gb18030`**
   DBC 含 GBK 编码的中文 `CM_` 注释与中文 `VAL_`（连续多字节）：
   - 断言解析成功、中文可读（含预期的中文字符）、note 里出现 `GB18030`
3. **`dbcnorm_run_detection_boundary`**
   直接对判别函数做单测：全 ASCII（不该走任何回退）、单个孤立高位字节（→cp1252）、
   两个连续高位字节（→GB18030）、孤立与连续混在（→GB18030，因为 `max_run>=2`）
4. **回归**：Step 9/10 的既有用例全部仍绿，特别是
   - `NS_ :` 段列关键字**不得**被当成待剔除语句（报文数必须保持 1）
   - `BO_`/`SG_` 行数不变量
   - BOM、`CM_ BA_`、`BA_DEF_REL_` 三个用例
   - 数据驱动的剔除清单一致性用例

## 自检（必须跑，贴真实输出）

```
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -50
```
失败就修到全绿。（DYLD 变量必带；dyld 报错是环境问题，不要改代码。）

## 回复格式（只回复这些）

- 改动文件与行数、`cargo test` 真实输出、判别函数最终规则（一句话）、偏离说明（没有就写"无偏离"）。
