# Step 9 任务书（DBC 预规范化：BOM / GBK 编码 / 不支持的段）—— 交给 DSH headless 执行

你是 Rust 系统工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先读：`docs/INTERFACE.md` §1 / §5、`docs/DEPS.md`、`crates/fast_blf_core/src/decoder.rs`（DBC 加载路径）、`crates/fast_blf_core/src/error.rs`。

## 背景（主模型实测 25 个变体得出的确凿结论）

真实用户（中国项目）反馈"某个通道的 DBC 解析失败"。用 25 个 DBC 变体实测 `can-dbc 10.0.0`：

**会失败的 6 种：**
1. **GBK/ANSI 编码**（中文注释或中文 `VAL_` 值表）→ `DBC_PARSE: stream did not contain valid UTF-8`
2. **UTF-8 BOM 开头** → 第 1 行 `expected file`
3. `CM_ BA_ "属性名" "注释";`（属性注释）→ 报错但给出 `行:列`
4. `BA_DEF_REL_` / `BA_DEF_DEF_REL_`（相对属性定义）→ 报错给出 `行:列`
5. 注释里含未转义 `"`（文件本身不规范）
6. （同上，编码类）

**必须保持通过的 19 种**（不要误伤）：多路复用 `M`/`m1`/`m2`、`SG_MUL_VAL_`、`SIG_VALTYPE_`、`SIG_GROUP_`、`BO_TX_BU_`、`VAL_TABLE_`、`VAL_`、`EV_`/`ENVVAR_DATA_`、扩展帧 ID（含 `0x80000000` 位）、纯 CRLF、缺 `NS_` 段、多个接收者、信号名以数字开头、ENUM 属性值越界、`BA_DEF_`/`BA_`/`BA_DEF_DEF_`、完整 `NS_` 关键字列表、空文件、只有 `VERSION` 的文件。

## 目标

在**不改动 can-dbc 依赖**的前提下，给 DBC 读取加一层**预规范化**，让中文项目的真实 DBC 能用起来；**规范化必须留痕（warning），绝不能静默改数据**。

## 本次只做 Step 9，只允许改这些文件

1. **新增** `crates/fast_blf_core/src/dbc_norm.rs`
2. `crates/fast_blf_core/src/lib.rs`（模块声明）
3. `crates/fast_blf_core/src/decoder.rs`（DBC 读取处改调规范化）
4. `crates/fast_blf_core/Cargo.toml`（**只允许**新增 `encoding_rs = "0.8"`，纯 Rust 无 C 依赖）
5. `crates/fast_blf_core/tests/e2e_synthetic.rs`（测试）

不要动 `gui/`、`docs/`、`exporter.rs`、`bit_slice.rs`。

## 1. `dbc_norm.rs` 要求

```rust
pub struct NormOutcome { pub text: String, pub notes: Vec<String> }
pub fn normalize_dbc_bytes(raw: &[u8], file_label: &str) -> Result<NormOutcome, String>
```

处理顺序：

1. **去 BOM**：若以 `EF BB BF` 开头，去掉（记 note：`已去除 UTF-8 BOM`）。
2. **编码**：先按 UTF-8 严格解码；失败则用 `encoding_rs::GB18030` 解码（GB18030 是 GBK 超集，覆盖常见中文工程文件），记 note：
   `原文件不是 UTF-8，已按 GB18030 解码（中文注释/枚举值保持可读）`。
   若 GB18030 解码仍失败 → 返回 `Err(原因)`，由调用方包成 `DBC_PARSE`。
3. **按语句剔除 can-dbc 不支持的段**（这些段**与信号解码无关**，纯元数据）：
   需要剔除的语句起始关键字：`CM_ BA_`、`BA_DEF_REL_`、`BA_DEF_DEF_REL_`、`BA_REL_`、`BU_SG_REL_`、`BU_EV_REL_`、`BU_BO_REL_`、`BA_DEF_SGTYPE_`、`BA_SGTYPE_`、`SIGTYPE_VALTYPE_`、`CAT_DEF_`、`CAT_`、`FILTER`、`EV_DATA_`、`ENVVAR_DATA_`、`SGTYPE_`、`SGTYPE_VAL_`、`SIG_TYPE_REF_`。
   **剔除规则必须按"语句"而不是按行**：一条语句从起始关键字开始，到下一条以 `;` 结尾的行为止（可能跨行），整段丢弃；丢弃后保留换行以维持行号可读性。
   记一条汇总 note：`已忽略 N 条 can-dbc 不支持的语句（CM_ BA_ ×a、BA_DEF_REL_ ×b …）`。
4. **绝对不要剔除**：`VAL_`、`VAL_TABLE_`、`SG_MUL_VAL_`、`SIG_VALTYPE_`、`SIG_GROUP_`、`BO_TX_BU_`、`CM_`（报文/信号注释）、`BA_DEF_`、`BA_`、`BA_DEF_DEF_`、`EV_`、`BO_`、`SG_`、`NS_`、`BS_`、`BU_`、`VERSION`。
5. notes 由调用方并入 `warnings`（每条前面带 `DBC「<文件名>」：` 前缀）。

## 2. 接线要求

- `scan` 与 `convert` 的 DBC 读取**必须走同一个函数**（不要出现"扫描宽松、转换严格"这种历史坑）。
- 规范化产生的 notes **必须**出现在返回值的 `warnings` 里；DBC 解析失败时错误消息里要带**文件名 + 规范化 notes（若有）**，便于用户定位。

## 3. 测试（`tests/e2e_synthetic.rs`）

把主模型的 25 变体矩阵固化成测试（可精简，但必须覆盖）：

1. `dbcnorm_gbk_encoded_dbc_parses_and_chinese_survives`
   → 用 GBK 字节写一个含中文 `CM_` 注释 + 中文 `VAL_` 值的 DBC，断言：解析成功、报文/信号可解、`warnings` 里出现 `GB18030` 字样
2. `dbcnorm_utf8_bom_is_stripped`
   → 断言解析成功 + warning 提示去 BOM
3. `dbcnorm_attribute_comment_and_relative_attrs_are_ignored`
   → DBC 含 `CM_ BA_ ...;` + `BA_DEF_REL_` + `BA_DEF_DEF_REL_`，断言：解析成功、**信号照常解出**、warning 说明忽略了哪些语句（含条数）
4. `dbcnorm_keeps_supported_sections_intact`
   → 含 `VAL_` / `VAL_TABLE_` / `SG_MUL_VAL_` / `SIG_VALTYPE_` / `SIG_GROUP_` / `BO_TX_BU_` / `EV_`：断言这些**没有被剔除**（用规范化后的文本断言关键字仍在，并断言报文/信号解出正确）
5. `dbcnorm_multiline_statement_removal_is_line_safe`
   → 一条跨 3 行的 `BA_DEF_REL_` 语句，断言其后续的 `BO_`/`SG_` 语句**完好无损**（行号与内容都不受影响）
6. `dbcnorm_invalid_utf8_and_not_gbk_reports_clear_error`
   → 写入无法用 GB18030 解释的字节序列 → 明确错误（含文件名），不是 panic
7. 回归：全部既有测试仍绿（尤其 `case12/13/14`、`timefmt_*`、`wide_*`）

## 4. 自检（必须跑，贴真实输出）

```
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -50
```
失败就修到全绿。（DYLD 变量必带；dyld 报错是环境问题，不要改代码。）

## 5. 回复格式（只回复这些）

- 改动文件与行数、`cargo test` 真实输出、偏离说明（没有就写"无偏离"）。
