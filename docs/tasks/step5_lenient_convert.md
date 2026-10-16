# Step 5 任务书（DBC 容错 + 空 BLF 扫描）—— 交给 DSH headless 执行

你是 Rust 系统工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先完整读：
- `docs/INTERFACE.md` —— **§1（scan 的 blf_paths 允许为空）、§1.2（新增 `dbc_errors` 字段）、§5.9（新硬约束：单通道 DBC 失败不得拖垮整体）**
- `crates/fast_blf_core/src/decoder.rs`（`build_lenient` 已存在，返回 `(DecodePlan, BTreeMap<u32, CoreError>)`）
- `crates/fast_blf_core/src/lib.rs`（`scan` 已用宽松构建；`convert` 目前用**严格**构建，这就是要修的点）
- `crates/fast_blf_core/tests/e2e_synthetic.rs`（现有 11 项集成测试的写法）

## 背景（真实用户报的 bug，必须修掉）

用户在 Ch1 和 Ch2 各绑一个 DBC，Ch1 的 DBC **解析失败**。他只勾选 Ch2 的信号就去转换 → 报"通道 1 的 DBC 构建失败"，**整个转换失败**。
正确行为：没勾选的通道，其 DBC 失败绝不能导致转换失败；失败原因要在结果里回传，并**在解析阶段就能显示**（GUI 负责显示，你只负责回传数据）。

## 本次只做 Step 5，只允许改这三个文件

1. `crates/fast_blf_core/src/lib.rs`（主体）
2. `crates/fast_blf_core/src/decoder.rs`（仅当 scan 空路径需要最小调整时）
3. `crates/fast_blf_core/tests/e2e_synthetic.rs`（补测试）

不要动 `gui/`、`exporter.rs`（除非 `ExportReport` 需要新增字段透传）、`Cargo.toml`、`docs/`。

## 1. `convert` 改用宽松构建

把 `DecodePlan::build(&dbc_map, &selection)?` 改成 `build_lenient`，然后：

- 失败通道的报文**一律不参与**（该通道在 plan 里就是空的）；
- 返回值的 **`dbc_errors`**：`{"<通道号字符串>": "<人话原因>"}`（原因里带上 DBC 文件名与原始错误）；
- `warnings` 里补一条人话，例如：`通道 1 的 DBC 解析失败，已跳过该通道（原因：…）`；
- **只有**当"用户选中的报文/信号全部落在失败通道或未绑 DBC 的通道"时，才返回 `EMPTY_RESULT`；其 `message` 必须点名**哪些通道** + **失败原因关键字**（例如：`所选信号全部来自 DBC 构建失败的通道：1（DBC 解析失败：…）`）；
- 其它情况一律正常转换，绝不因为某个没勾选通道的坏 DBC 而失败；
- 未绑 DBC 的通道：若用户没选它 → 完全忽略；若选了它 → 计入上面的 EMPTY_RESULT 判定逻辑。

## 2. `scan` 支持空 `blf_paths`

`scan([], {1: "x.dbc"})` 必须合法：只按已绑 DBC 列出报文与信号，`frame_count=0`、`cycle_ms=None`、`is_fd=false`、`files=[]`。
注意：此时**不能**因为"观测不到帧"就把报文丢掉 —— 计划要收录 DBC 里的全部报文（`Selection::default()` 即全选）。

## 3. 测试（写进 `tests/e2e_synthetic.rs`，沿用现有夹具生成逻辑）

1. **坏 DBC 不拖垮**：Ch1 绑一个**故意损坏**的 DBC（写一段乱码文本即可），Ch2 绑正常 DBC；`selected` 只选 Ch2 的报文 →
   断言：`convert` 成功（`Ok`）、子表行数与选中报文一致、`report["dbc_errors"]` 含 key `"1"`、`warnings` 里有提到通道 1 的条目。
2. **全选失败通道才报错**：同夹具，`selected` 只选 Ch1 的报文 → 断言返回 `Err`，错误码 `EMPTY_RESULT`，且 message 里同时出现 `"1"` 与失败原因关键字（如 `DBC`）。
3. **空路径扫描**：`scan` 等价调用（在 Rust 层调 `build_lenient` + 组装）→ 或直接断言 `DecodePlan` 对空观测仍含全部 DBC 报文；
   若 scan 的 Python 入口不便在 Rust 测试里调，就测 `build_lenient` 的行为并注释说明。
4. **回归**：现有全部测试必须仍绿（尤其 11 项 e2e）。

## 4. 自检（必须跑，贴真实输出）

```
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -40
```
（`DYLD_FALLBACK_LIBRARY_PATH` 必带，见 `docs/DEPS.md` §0；dyld 报错是环境问题，不要改代码。）
失败就修到全绿。另跑 `cargo clippy -p fast_blf_core 2>&1 | tail -10` 并贴出。

## 5. 回复格式（只回复这些）

- 改动文件与行数、`cargo test` 真实输出（测试名 + 通过/失败数）、clippy 摘要、偏离说明（没有就写"无偏离"）。
