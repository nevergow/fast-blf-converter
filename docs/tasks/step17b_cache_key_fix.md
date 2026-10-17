# Step 17b：缓存键不再包含"报文集合" + `info()` 补 `key` 字段（修 G3）

## 背景（主会话独立验收发现的两个缺陷）

### 缺陷 1（体验致命）：**换一次信号选择就整份重建缓存**
当前缓存键 = 文件清单 ‖ DBC 哈希 ‖ **报文集合** ‖ 版本。于是：
1. 用户开着预览，勾了一个新信号 → 触发增量预取，**段已写进缓存目录** ✅
2. 用户关掉预览、又打开一次（此时 `selected` 多了那个报文）→ **键变了 → 全量重建**（187MB 实测 3.3~5 秒）
   —— 这正是用户最反感的"重新加载"。实测证据：`G3` 场景 `cache_hit=False`。

### 缺陷 2（接口不完整）：`info().signals[]` 缺少 `key`
契约 §1.4 定义了 `key = "ch{channel}/{message}/{signal}"` 是全链路唯一标识，但 `info()` 只返回
`{channel,message,signal,unit,cached}`，调用方（GUI）得自己拼字符串 —— 格式一旦漂移就静默失配。

## 你只能改这些文件
* `crates/fast_blf_core/src/frame_cache.rs`
* `crates/fast_blf_core/src/plot_session.rs`
* `crates/fast_blf_core/src/lib.rs`（如需）
* `crates/fast_blf_core/tests/e2e_synthetic.rs`

**禁止改**：`docs/**`（契约由主会话改）、`gui/**`、`.github/**`、`exporter.rs`、`series.rs`、
`README.md`、`使用说明.txt`、`tests/acceptance.py`。

## 要求

### R1 缓存键：**只由"数据源"决定，与用户选了什么无关**
```
key = sha256( CACHE_FORMAT_VERSION ‖ 按路径排序的 [abs_path, size, mtime] 列表 ‖ DBC 内容哈希 )
```
* **不再**包含 `selected` / 报文集合。
* 一个 key 目录内，报文段 `ch{c}_id{id}.bin` **按需累积**：谁被需要就写谁，互不影响。
* `meta.json` 里的 `messages` 改成"**已存在的段清单**"，只用于统计与 LRU，不再参与键计算。

### R2 `open` 的命中语义（这是修体验的关键）
* `open` 时计算"本次 `selected` 需要的段集合"。
* **需要的段全部已存在** → 立即返回（`cache_hit=true`，≤300ms，不读 BLF）。
* **部分缺失** → **立刻返回**（已存在的段立即可用，用户可马上看已缓存信号），缺失段转**后台预取**；
  `cache_hit=false`，`prefetch_status()` 能查到进度。
  ⚠️ **禁止**在 `open` 里同步等缺失段建完（那就是"打开预览卡 5 秒"）。
* 进度回调仍然只在**真正需要读 BLF** 时发（`phase=1, stage="cache"`）。

### R3 预算语义（两件事分开，不要混）
* **估算闸门**：本次**新增需要的字节**估算 > `cache_budget_mb` → `CACHE_BUDGET_EXCEEDED`（三项动作照旧）。
  已存在的段**不计入**这次的估算（已经付过磁盘代价了）。
* **目录上限**：`cache_dir` 总字节超 `cache_budget_mb` → 按 `meta.json` 的 `created_at` **LRU 删别的 key**，
  **绝不删当前正在用的 key**；删了多少要记 `warnings` 并在 `info()` 里能看出（如 `evicted_bytes`）。

### R4 `info()` 补字段
* `signals[]` 里每条**加 `key`**（`"ch{channel}/{message}/{signal}"` 字符串，与 `stats`/`add_signals` 入参完全一致）。
* `info()` 增加 `cache_bytes`（保持）、`cache_hit`（保持）、**`evicted_bytes`**（本次 LRU 删掉的字节，默认 0）。

### R5 不要动的行为
* `view()` / `stats()` / `set_time_format()` / `export_html()` 的既有语义与性能不许变（延迟硬指标见 §5 第 18 条）。
* 帧缓存段格式（17/73 字节 stride）不许变。

## 自检（必须真跑，输出贴交付说明）

1. **G3 回归（核心）**：先 `selected={VehSpd}` 开会话 → `add_signals(BattTemp 的信号)` 触发预取 → 等预取完成 →
   `close()` → 用 `selected={VehSpd, BattTemp}` 重新 `open` → **`cache_hit=true` 且耗时 ≤ 300ms**。
2. **键稳定性**：同一批文件、不同 `selected` 组合 → 键必须相同（打印两个键的前 12 位并断言相等）；
   文件 mtime 变化 → 键必须变化。
3. **R2 异步**：`selected` 里含一个未缓存报文时，`open` 必须**立即返回**（打印 open 耗时，
   必须 < 500ms，且此时 `prefetch_status().busy == true` 或已完成）；已缓存信号在 open 后**立即可 view**。
4. **R3 估算闸门**：新增需要 26 MiB、预算 16 MiB → `CACHE_BUDGET_EXCEEDED`（消息含三项动作）；
   **已存在的段不计入**：同一目录再 open（段已齐）时不报错。
5. **R3 LRU**：造两个 key 目录、把预算压到只够一个 → 旧 key 被删、当前 key 保留（打印删了哪个）。
6. **R4**：`info().signals[*].key` 存在，且 `stats([key])` 能直接用该 key。
7. 既有 Step 17 测试**全部保持通过**（80 项左右，一个不许少）。

## 交付说明必须包含
* 改动函数清单（名称 + 行号）；
* 上面 1~6 的**真实命令与输出**（禁止写"预期"）；
* 一句话说明：为什么"换信号选择"不再导致重建（键与选择解耦 + 段的增量累积）。

## 禁止事项
* 不许在 `open` 里同步建缺失段；不许改段格式；不许改 `view/stats` 的语义与性能；
* 不许加依赖；不许 `unwrap()/expect()/panic!` 出现在生产路径；
* 不许动契约文档与 GUI（GUI 在 Step 18 里按新字段适配）。
