[Target]    /tmp/slice-export  （git worktree，分支 slice/export-safety，基点 b36b0a9）
[Executor]  DSH headless
[Attempt]   1
[Anchor]    b36b0a9 · 派发前 `git status --porcelain` 必须为空
            （worktree 里没有 .venv，用主仓库解释器 /Users/wuzeqin/Projects/fast_blf_converter/.venv/bin/python）

## 1. Goal

「CSV + 多信号一张表 + 重采样 50 Hz」这条导出路径不再无声崩溃，且崩溃/成功都有日志证据。
完成后必须同时为真的状态：

1. **峰值内存不再随网格行数 G 线性增长**：phase 2 宽表重采样改成按行块（row-block）拼装与写出，
   峰值 ≈ `max(phase1 峰值, CHUNK_ROWS × S × 常数)`。
2. **分配前有预算闸门**：估算字节 > `memory_budget_mb` 时返回结构化 `BAD_CONFIG`（消息里给出可操作建议），
   而不是让 allocator abort。
3. **日志落盘**：每次 export 在 phase 2 开始前写一行预检记录（t0/t1/hz/G/S/估算MB/预算MB/chunk）；
   Rust `panic` 走 `set_hook` 写进同一文件。
4. **输出逐字节不变**：golden CSV 的 sha256 必须与改前一致。

## 2. Context（已侦察，不要自己重新摸）

**崩溃调用链**（70% = phase 1 结束、phase 2 开始的分界，`gui/main.py:115` `_PHASE1_SHARE = 0.70`）：

```
gui/main.py:392  core.convert(config, cb)
  → lib.rs:565   pub fn convert
  → lib.rs:758   py.detach → lib.rs:773 process_file（phase 1，decoder.rs:965）
  → lib.rs:815   sink.tables_snapshot() → lib.rs:822 export(...)（phase 2）
  → exporter.rs:415 export → :422 catch_export_panic → :426 export_inner
  → exporter.rs:487-492 match(split_by_message, format) → (false, Csv) → :813 export_wide_tabular
  → exporter.rs:527 prepare_wide → resample_hz=Some → :590 prepare_wide_resampled   ← 崩在这里
  → exporter.rs:832 transform.finalize → :838 write_table → :1424 write_csv
```

**`prepare_wide_resampled`（exporter.rs:590-687）为什么崩**：整张网格一次性materialize。
- `:601-613` `table_time_bounds`：`t1` 取**所有表所有分片**的最大值 → 一个坏时间戳就能把 G 放大几个数量级
- `:615` → `resample_grid`（`:1261-1270`）：`G = floor((t1-t0)*hz + tol) + 1`，**无任何上限**；
  `(scaled+tol).floor().max(0.0) as usize` 先饱和再 `+1`，饱和到 `usize::MAX` 时 release 下**回绕成 0**
- `:618-621` `Series::from_vec("timestamp", grid.clone())`：同时存在 2~3 份 G 长度缓冲
- `:631-664` 逐表逐信号：`merge_table`（`:930-938`，`sort+unique+with_streaming(true).collect()` 整表进内存）
  → `f64_column` 整列拷 `Vec<f64>`（`:1394-1399`）→ `resample_hold/linear`（`:1341`/`:1291`）产出
  `Vec<Option<f64>>`（**16 B/格**）→ `Series::new` 再 alloc 一次
- `:617` `columns: Vec<Column>` 累积**所有信号整列**，直到 `:681 DataFrame::new_infer_height(columns)`
- `memory_budget_mb`（`lib.rs:748`）**只用在 phase 1**（`decoder.rs:1059` / `:683` `FLUSH_ROWS=200_000`）
- `catch_export_panic`（`:365-390`）抓不到 OOM/abort（不是 unwind）；`Cargo.toml` 是 `panic="unwind"`

**没有日志**：`crates/` 下 `set_hook|tracing|log::|env_logger` 零命中；`Cargo.toml:26-52` 无相关依赖 → **不引入新依赖，用 `std::fs`**。

**我实测的基线（可信，你复跑应该接近）**：

| 输入 | 配置 | G | S | 峰值 RSS | 耗时 | 说明 |
|---|---|---|---|---|---|---|
| `fixtures/bench_large.blf`(600s) | 50Hz wide hold | 30,001 | 189 | 227 MB | 0.8 s | golden 基线 |
| `fixtures/bench_huge.blf`(2000s) | 50Hz wide hold | 100,001 | 189 | 574 MB | 2.5 s | |
| `fixtures/bench_huge.blf` | split（不重采样） | — | — | 512 MB | 2.3 s | 这是 phase 1 的上界 |
| `fixtures/bench_xl.blf`(12800s≈1.2GB) | 50Hz wide hold | 640,001 | 189 | **预计 ~3.6 GB** | 数十秒 | 崩溃场景等价物 |

→ 结论：**phase 2 的峰值单独就有 ~30 B/格**（`574 MB / (100001×189)`）。所以 1.2 GB 文件在
16 GB 的 Windows 办公本上会被 OOM abort，且 `catch_unwind` 抓不到 → 用户看到"70% 闪退"。

**进程外可复现手段**（我已验证可用）：`ulimit -v <MB>` 限制地址空间，可让超预算分配直接 abort。
不要用它做「修复成功」的证据，只用于复现失败态。

**黄金基线（改前已固化，你必须逐字节对上）**。`fixtures/bench_large.blf` + `fixtures/bench.dbc`，50 Hz 宽表 CSV：
- `--fill hold`  → 12.4 MB → sha256 `88a0482045562ee207d34a52d675cc4bb1154ce8cb1e79ba7eacb9808a48e10a`
- `--fill linear` → 14.7 MB → sha256 `3b1472489955315036b46b2831cf280427d616326edcf5389c7ba108a9c1e0ce`

探针脚本（我的验收工具，**只读使用，不要修改**）：
`/Users/wuzeqin/.hermes/cache/scratch/recon/probe_wide_export.py`
用法：`python probe_wide_export.py --blf <blf> --dbc fixtures/bench.dbc --hz 50 --fill hold --out <dir> --tag <name>`
它打印 `elapsed` / `peak_rss` / 每个输出文件的 `sha256` / `progress_events`。

## 3. Scope & Boundaries

[Scope_Allow]
- crates/fast_blf_core/src/exporter.rs
- crates/fast_blf_core/src/lib.rs
- crates/fast_blf_core/src/decoder.rs
- crates/fast_blf_core/src/error.rs
- crates/fast_blf_core/tests/wide_resample_guard.rs   ← **新建文件**

[Scope_Forbid] 其他一切：`gui/`、`tests/`、`docs/`、`fixtures/`、`Cargo.toml`、`Cargo.lock`、`crates/fast_blf_core/tests/e2e_synthetic.rs`

[Invariants]
1. **输出逐字节一致**（同输入同配置下 CSV 的 sha256 不变）；行数、列名、列序、浮点格式都不许变
2. Python 侧 `convert(config, cb)` / `export(...)` 的签名与返回 report 的既有字段不变（**可以新增**字段/warnings，不许删改）
3. 新增可选配置字段 `log_path: str | None`：缺省 None → 不写日志文件、行为与现在完全一致
4. **不得引入任何新 crate 依赖**（日志用 `std::fs::OpenOptions` 追加写）
5. `memory_budget_mb` 语义扩展到 phase 2（宽表网格与行块）
6. 不得删改/跳过现有测试；不得 `#[ignore]`；不得写死返回值凑断言
7. `zoom` 无关：不许碰 plot 相关代码

## 4. 实现要求（Part A → Part B 顺序固定，A 做完必须先能独立编译通过）

### Part A — 安全网（先做）

- **A1** `resample_grid`（exporter.rs:1261-1270）：用 checked 运算；`points == 0` 或溢出 → `CoreError::bad_config`；
  新增硬上限 `MAX_GRID_POINTS = 20_000_000`（≈50 Hz 下 111 小时），超限给结构化错误
- **A2 预算闸门**：在 `:615` 生成网格前算 `est_bytes = G * S * 16`（float 列 + 临时/位图上限估计），
  与 `memory_budget_mb * 1MB` 比较；超限 → `BAD_CONFIG`，消息**必须**含
  `t0= t1= duration_s= hz= G= S= est_mb= budget_mb=` 以及一句建议（如「把重采样降到 N Hz 或改用拆表模式」）
- **A3** 去掉 `grid.clone()`（`:620`）；timestamp 列复用同一份数据
- **A4** `resample_hold`/`resample_linear` 输出从 `Vec<Option<f64>>` 改成 `Vec<f64>` + 独立 null 掩码
  （8 B/格 + 1 bit/格），语义（哪里是 null）必须与现在完全一致
- **A5 预检日志**：`log_path` 给出时，phase 2 宽表重采样在**任何大分配之前**追加一行（行缓冲 flush）：
  `[<ISO8601>] export phase2 wide-resample t0=<..> t1=<..> hz=<..> G=<..> S=<..> est_mb=<..> budget_mb=<..> chunk_rows=<..>`
  出错时再追加 `[...] export phase2 ERROR <err>`
- **A6** `lib.rs` 模块初始化处装 `std::panic::set_hook`：把 `thread/location/file/line/message` 追加进 `log_path`
  （路径用 `OnceLock<String>` 存；None 时 hook 保持原样只打 stderr）
- **A7** phase 1（`process_file`）也包 `catch_unwind`：panic → 记日志 + 进 `errors[]`，不要穿透成 pyo3 `PanicException`

### Part B — 分块导出（真正的修复）

- **B1** `prepare_wide_resampled` 改成流式：
```
先构建一次最终列名/顺序计划（与现在 plan_wide_names 的结果逐项相同）
for chunk in grid.chunks(CHUNK_ROWS):
    ts = chunk（走与现在相同的 transform/round 处理）
    for each table → for each signal：只用 partition_point 算该 chunk 的 Vec<f64>+mask（不保留 G 长数组）
    拼 1 个 DataFrame（行数 = chunk.len()，列序 = 计划）
    transform.finalize(chunk_df)
    写：CSV 首块写 header，后续 append；Parquet 一个 writer 逐 row-group 追加
```
  `CHUNK_ROWS = 20_000`（可在代码里做常量并写进日志）
- **B2** 允许每张表的 merged 列常驻内存（那是数据本身），但**不许**再额外持有任何 `G` 长度的中间数组
- **B3** `write_csv`（`:1424-1439`）支持分块追加（第一块写 header，后续不写）
- **B4** Excel 防呆 `max_rows`（`:452`）改用**合并后**行数（resample 时 = G）；xlsx 大表仍走"降级为 csv"，不要试图分块写 xlsx
- **B5** 取消：phase 2 每个 chunk 边界检查取消标志（`lib.rs:187` `ProgressThrottle::dispatch` 现在把取消异常吞掉，
  `:198-204`）→ 改成 `AtomicBool`，可中止并返回"已取消"

### 新增守卫测试 `crates/fast_blf_core/tests/wide_resample_guard.rs`（必须自己写）
至少 3 个用例：
1. 超上限的 G（构造 t1-t0 极大或直接调内部函数）→ 返回 `BAD_CONFIG` 的 Err，不 panic、不 abort
2. 小规模宽表 50Hz resample（hold 与 linear）：分块结果与"手工算出的期望值"逐点相等（含 null 位置）
3. 给 `log_path` 时，文件里出现含 `export phase2 wide-resample` 的那一行

## 5. Verification Contract（按顺序，全部贴真实输出）

```bash
cd /tmp/slice-export
cargo build --release -p fast_blf_core            # exit 0
cargo test --release -p fast_blf_core             # exit 0，现有测试不许少
```

Python 端到端（**用隔离 venv，别碰主仓库 .venv**）：
```bash
cd /tmp/slice-export
python3 -m venv .venv-verify
.venv-verify/bin/pip install -q maturin
.venv-verify/bin/maturin develop --release        # 需要 Cargo.toml/pyproject 在 /tmp/slice-export（worktree 自带）
.venv-verify/bin/python /Users/wuzeqin/.hermes/cache/scratch/recon/probe_wide_export.py \
  --blf /Users/wuzeqin/Projects/fast_blf_converter/fixtures/bench_large.blf \
  --dbc /Users/wuzeqin/Projects/fast_blf_converter/fixtures/bench.dbc \
  --hz 50 --fill hold --out /tmp/slice_export_out --tag after_hold
```
**必须**打印出 `sha256 = 88a0482045562ee207d34a52d675cc4bb1154ce8cb1e79ba7eacb9808a48e10a`（逐字节一致）。
再跑一次 `--fill linear`，要求 `3b1472489955315036b46b2831cf280427d616326edcf5389c7ba108a9c1e0ce`。

内存验收（这是本切片的核心理由）：
```bash
.venv-verify/bin/python .../probe_wide_export.py --blf .../fixtures/bench_xl.blf \
  --dbc .../fixtures/bench.dbc --hz 50 --fill hold --out /tmp/slice_export_xl --tag after_xl
```
要求：**成功完成**，且 `peak_rss ≤ 900 MB`（改前预计 ~3.6 GB）。
若 `fixtures/bench_xl.blf` 还没生成完（它是 1.2 GB，正在生成），先用 `bench_huge.blf` 跑一遍并报数，
`bench_xl` 的复测留给我做——但**必须**在回执里写明你跑的是哪一个。

反作弊自证：`git diff --name-only` 必须恰好是 `crates/fast_blf_core/src/{exporter,lib,decoder,error}.rs`
（可能不含全部 4 个）+ 新建的 `crates/fast_blf_core/tests/wide_resample_guard.rs`；不许出现 gui/ tests/ docs/

**Pass**：以上全部满足
**Fail**：任一条失败 / 白名单外改动 / 测试或断言被改 / 逐字节对不上 / 超时

## 6. Stop Conditions（碰到就停，输出 `[ABORT_CONTRACT_CONFLICT] <file:line> | <原因>`）
- 需要改白名单外文件，或需要新增 crate 依赖
- 分块后**无法**做到逐字节一致（先把差异位置报出来，别硬凑）
- 同一错误签名重复 2 次
- 超过 70 分钟

## 7. Executor Rules
1. 禁 `git commit/push/reset/clean`；禁改白名单外文件；禁改/删/跳过测试；禁 `#[ignore]`、裸 `unwrap` 兜错、写死返回值凑测试
2. 冲突即停，只输出 `[ABORT_CONTRACT_CONFLICT] <file:line> | <原因>`
3. 回执格式（缺项视为失败）：
```text
[STATUS]: DONE | ABORT_CONTRACT_CONFLICT
[TOUCHED_FILES]: <与 git diff --name-only 一致>
[VERIFY]:
  1) <cmd> -> exit <code>
     <关键原始输出 5-20 行，禁止省略报错>
  2) ...
[ARTIFACTS]: <日志/CSV 的绝对路径>
[NOTE]: <做了什么、跳过了什么、为什么>
```
