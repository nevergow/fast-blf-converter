# Step 10 任务书（DBC 规范化：修误伤 + 证据驱动的剔除清单）—— 交给 DSH headless 执行

你是 Rust 系统工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先读：`crates/fast_blf_core/src/dbc_norm.rs`（Step 9 产物）、`docs/tasks/step9_dbc_normalize.md`（背景）、`crates/fast_blf_core/src/decoder.rs`。

## 问题（主模型独立复现，必须修）

Step 9 的规范化**误伤并静默丢数据**：

变体 `NS_ :` 段里逐行列出的关键字（`\tFILTER`、`\tCAT_`、`\tCAT_DEF_`、`\tSGTYPE_`、`\tSGTYPE_VAL_`、`\tBA_DEF_SGTYPE_`、`\tSIGTYPE_VALTYPE_`、`\tSIG_TYPE_REF_`、`\tEV_DATA_`、`\tENVVAR_DATA_`、`\tBA_REL_` …）
被当成"待剔除语句"的**起点**；而剔除规则是"吃到下一个 `;` 为止"，**BO_/SG_ 行并不以 `;` 结尾**（DBC 里只有 `CM_`/`BA_`/`VAL_` 等以分号结束）
→ 于是把后面的整个报文定义段吞掉：`scan` 返回 **0 报文且无任何错误**（静默丢数据）。

实测证据（规范化前后同一份文件）：
- 规范化前：`NS_` 段含常见关键字的 DBC → `✓ 报文 1 个`
- 规范化后：同一份 → `✓ 通过 报文 0 个`（`dbc_error=None`，`messages=[]`）

## 修复要求（四件事，都要有测试）

### 1. 只在"顶层语句"上做剔除（列 0 规则）

- 只有**行首无缩进**（第 0 列）出现的关键字才可能是待剔除语句的起点。
  DBC 规范里 `NS_ :` 段内的关键字一律带前导空白（Tab/空格），因此这条规则天然把它们排除。
- **显式跳过整个 `NS_ :` 段**：从 `NS_ :` 行到下一个顶层段（`BS_:` / `BU_:` / 第一个 `BO_` / `VERSION` / 或首个第 0 列开始的非 NS 关键字）为止，整段**不得做任何剔除**。

### 2. 语句边界不得跨过顶层定义（限制爆炸半径）

- 一条待剔除语句从起点开始，到**本行或后续行的 `;`** 结束；
- 但**绝不允许跨过**任何以第 0 列开始的新顶层定义行（`BO_`、`SG_`、`BU_`、`VERSION`、`NS_`、`BS_`、`CM_`、`BA_`、`VAL_`、`SG_MUL_VAL_`、`SIG_VALTYPE_` …）；
- 若在遇到下一个顶层定义前都没找到 `;`，则**只剔除这一行**（保守），并记 note 说明。

### 3. 剔除清单必须"证据驱动"（只剔除被证明解析失败的）

写一个**数据驱动单测**：对每个候选关键字构造"最小 DBC + 该语句"，直接调用底层解析器
`can_dbc::Dbc::try_from(<原始文本>)`（**绕过规范化**），断言：

- 若**原始文本解析失败** → 该关键字**必须**在剔除清单里；
- 若**原始文本解析成功** → 该关键字**绝对不能**被剔除。

测试里用一张 `(关键字, 期望是否剔除)` 表；`dbc_norm.rs` 的剔除清单必须与该表一致（清单里的关键字集合 == 表中所有"期望剔除"项的集合）。
**精简清单**：只保留被证明会失败的（主模型已知：`CM_ BA_`、`BA_DEF_REL_`、`BA_DEF_DEF_REL_`），
其余（`CAT_`、`FILTER`、`SGTYPE_*`、`EV_DATA_`、`ENVVAR_DATA_`、`SIGTYPE_VALTYPE_`、`SIG_TYPE_REF_`、`BA_REL_`、`BU_*_REL_` 等）先按测试结果决定去留。

### 4. 不变量保护：规范化不得减少报文/信号定义

在返回前做自检（**必须实现**）：

- 规范化后的文本里，`BO_` 行数、`SG_` 行数必须**与原文一致**；若不一致 → **放弃剔除**（返回原文 + 记 note「规范化被跳过：剔除会破坏报文定义」），绝不静默丢定义。
- 给出可测的 note 文案，便于断言。

## 本次只做 Step 10，只允许改这些文件

1. `crates/fast_blf_core/src/dbc_norm.rs`
2. `crates/fast_blf_core/tests/e2e_synthetic.rs`

不要动 `gui/`、`docs/`、`Cargo.toml`、`decoder.rs`、`lib.rs`、`exporter.rs`。

## 测试要求（全部要有）

1. `dbcnorm_ns_section_keywords_are_not_stripped`
   → `NS_ :` 段列出 `FILTER/CAT_/SGTYPE_/EV_DATA_/ENVVAR_DATA_/SIG_TYPE_REF_/CAT_DEF_/SGTYPE_VAL_/BA_DEF_SGTYPE_/SIGTYPE_VALTYPE_/BA_REL_` 等关键字，文件里还有 1 个报文 2 个信号：
   断言 **报文数 = 1、信号数 = 2**（这是本次回归的直接复现用例）
2. `dbcnorm_never_reduces_bo_and_sg_lines`
   → 对上面的文件断言：规范化后文本的 `BO_`/`SG_` 行数与原文本相等
3. `dbcnorm_strip_list_matches_evidence`
   → 第 3 条的数据驱动表测试（含"必须剔除"与"必须保留"两侧）
4. `dbcnorm_multiline_strip_cannot_eat_message_definitions`
   → 构造"待剔除语句后紧跟 `BO_`/`SG_`"的场景，断言报文定义完好
5. `dbcnorm_gbk_and_bom_still_work`
   → 保留 Step 9 的两个核心用例（GBK 中文注释、BOM 开头）仍通过
6. 回归：全部既有测试仍绿。

## 自检（必须跑，贴真实输出）

```
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -50
```
失败就修到全绿。（DYLD 变量必带；dyld 报错是环境问题，不要改代码。）

## 回复格式（只回复这些）

- 改动文件与行数、`cargo test` 真实输出、最终剔除清单（关键字列表）、偏离说明（没有就写"无偏离"）。
