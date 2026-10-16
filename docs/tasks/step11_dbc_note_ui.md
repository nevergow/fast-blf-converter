# Step 11 任务书（把"DBC 已规范化"显示到界面上）—— 交给 DSH headless 执行

你是 Rust + PySide6 工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先读：`docs/INTERFACE.md` §1（scan 返回结构，**刚更新：通道信息新增 `dbc_note`**）、
`crates/fast_blf_core/src/dbc_norm.rs`、`crates/fast_blf_core/src/lib.rs`（scan 的通道组装处）、`gui/main.py`（DBC 节点渲染）。

## 背景

Step 9/10 给 DBC 读取加了预规范化（去 UTF-8 BOM、按 GB18030 解码 GBK/ANSI 文件、忽略 can-dbc 不支持的元数据语句）。
但**规范化说明目前只在 `convert` 的 `warnings` 里出现**；用户点「🔍 解析信号」时（走 `scan`）成功后看不到任何提示，
于是"我的 DBC 是 ANSI 的"这件事对用户是不可见的 —— 而这是他会想知道的信息（可能愿意重导一份 UTF-8）。

## 本次只做 Step 11，允许改这些文件

1. `crates/fast_blf_core/src/lib.rs`（scan 组装通道信息）
2. `gui/main.py`（DBC 节点显示）
3. `crates/fast_blf_core/tests/e2e_synthetic.rs`（测试）

不要动 `dbc_norm.rs`、`decoder.rs`、`exporter.rs`、`Cargo.toml`、`docs/`。

## 1. Rust：`scan` 每个通道回传 `dbc_note`

- 在 scan 的通道信息里新增 **`dbc_note`**（`str | None`）：
  - 该通道 DBC 解析**成功但被规范化过** → 填规范化 notes 拼接成的**一行短文本**，例如
    `已按 GB18030 解码（原文件非 UTF-8）；忽略 2 条不支持的语句`
  - 没有规范化 → `null`
  - DBC 解析失败 → 仍走 `dbc_error`（保持现有行为；`dbc_error` 消息里已含 notes）
- 实现要点：`scan` 与 `convert` 必须共用同一条 DBC 读取/规范化路径，不得各写一份。

## 2. GUI：在 DBC 节点上显示出来

- DBC 节点文本：解析成功且 `dbc_note` 非空时，在状态后缀里体现，例如
  `📄 demo.dbc   ✓ 已解析（已规范化）`
  （**保持现有 `✓ 已解析` / `✗ 解析失败` 文案前缀不变**，只在后面追加括号提示）
- **完整说明放 tooltip**（悬停可见），格式：
  ```
  <DBC 路径>
  ✓ 已解析
  规范化说明：已按 GB18030 解码（原文件非 UTF-8）；忽略 2 条不支持的语句
  ```
- 失败时 tooltip 仍显示完整 `dbc_error`（现有行为不变）
- 不弹窗（延续既有约定）

## 3. 测试

**Rust**（`tests/e2e_synthetic.rs`）：
1. `scan_reports_dbc_note_for_gbk_dbc`
   → 用 GBK 编码写一个 DBC（含中文 `CM_`）→ `scan([], {1: dbc})` → 断言
   `channels["1"]["dbc_note"]` 非空且包含 `GB18030`，且 `dbc_error` 为 None、报文可解
2. `scan_dbc_note_is_none_for_clean_dbc`
   → 纯 ASCII 的 DBC → 断言 `dbc_note is None`
3. `scan_dbc_note_present_for_ignored_statements`
   → 含 `CM_ BA_ ...;` 的 DBC → 断言 `dbc_note` 提到忽略了语句
4. 回归：全部既有测试仍绿。

**GUI**（我自己会另跑，你只要保证）：
- `--selftest` / `--selftest-deep` / `--selftest-config` / `--selftest-progress` 四条仍 exit=0
- 在 `--selftest-deep` 里**追加一条断言**：夹具 DBC（纯 ASCII）时 DBC 节点文本**不含** `已规范化`；
  再用一个临时 GBK DBC 走一次，断言节点文本**含** `已规范化`（用 `FBLF_CONFIG_DIR` 那种临时目录思路即可）

## 4. 自检（必须跑，贴真实输出）

```
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -40
.venv/bin/maturin develop 2>&1 | tail -3
.venv/bin/python -m py_compile gui/main.py && echo "py_compile OK"
for m in --selftest --selftest-deep --selftest-config --selftest-progress; do
  QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py $m 2>/dev/null | tail -2; echo "$m exit=$?"
done
```
失败就修到全绿。（DYLD 变量必带；dyld 报错是环境问题，不要改代码。）

## 5. 回复格式（只回复这些）

- 改动文件与行数、`cargo test` 与四条 GUI 自检的真实输出、偏离说明（没有就写"无偏离"）。
