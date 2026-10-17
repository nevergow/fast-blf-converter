# Step 17：帧缓存 + `PlotSession`（v0.4.0 地基：让预览"永不重新解码"）

## 先读契约（唯一依据）
`docs/INTERFACE.md` → **§1.4 PlotSession**、**§1.3 series_for_plot**、**§5 第 17/18 条**。
凡与任务书冲突，以契约文档为准；若你认为契约有问题，**停下来在交付说明里提出**，不要自行改契约。

## 你只能改这些文件
* `crates/fast_blf_core/src/frame_cache.rs`（**新建**）
* `crates/fast_blf_core/src/plot_session.rs`（**新建**）
* `crates/fast_blf_core/src/lib.rs`（`mod` 声明 + 注册 `open_plot_session`）
* `crates/fast_blf_core/src/decoder.rs`（**只允许新增**一个"从字节解码单信号"的纯函数；不得改既有函数行为）
* `crates/fast_blf_core/tests/e2e_synthetic.rs`（集成测试）

**禁止改**：`exporter.rs`、`gui/**`、`docs/**`、`.github/**`、`README.md`、`使用说明.txt`、`tests/acceptance.py`。

## 目标（一句话）
BLF 没有随机访问索引 → 想"只看某段时间窗"必须先把帧缓存下来。本步交付：
**一次解码 → 落盘帧缓存 → 之后任何窗口/任何口径/任何已缓存信号都是毫秒级**。

## 关键设计（照做，别自己发挥）

### 1) 缓存布局：一个 key 一个目录，一个报文一个文件
```
<cache_dir>/<key>/
    meta.json                       # 版本/文件清单/DBC 哈希/报文集合/t_min/t_max/生成时间/总字节
    ch{channel}_id{id}.bin          # 报文数据段（固定 stride，见下）
```
* `key` = sha256(**绝对路径 + size + mtime** 的有序列表 ‖ DBC 内容哈希 ‖ 报文集合 ‖ `CACHE_FORMAT_VERSION`)
* **段内固定 stride**（便于 `read_at` 随机访问与二分）：
  * 经典 CAN：`t f64 (8) + dlc u8 (1) + payload [u8;8]` = **17 字节**
  * CAN FD ：`t f64 (8) + dlc u8 (1) + payload [u8;64]` = **73 字节**
  * 用该报文**首帧**的 `is_fd` 决定；若后续出现不一致 → 按 FD 段截断写入并在 `meta.json` 记 `warnings`
* **不用 mmap、不引入任何新依赖**：随机读取用
  `std::os::unix::fs::FileExt::read_at` / `std::os::windows::fs::FileExt::seek_read`
  （`#[cfg(unix)]` / `#[cfg(windows)]` 两个薄封装）——OS page cache 已经够快。

### 2) 建缓存（唯一慢的一步，必须有确定进度）
* 只缓存 **`selected` 里出现的 `(channel, message)`** 对应报文的帧；其它报文一律不写。
* 单遍流式：`BlfReader` 逐帧 → `(channel, id)` 命中集合 → 追加写该报文段。
* 进度：**复用 §2 已有的 `Progress` 结构与 50ms 节流**（`phase=1`、`stage="cache"`、文件级 done/total、
  `frames_done`），`open_plot_session` 的 `progress_cb` 收到的 payload 字段与 `convert` 完全一致。
* 建完后写 `meta.json`，统计 `cache_bytes`，**校验预算**（见 4）。

### 3) 多分辨率包络金字塔（这是"缩放不卡"的核心，不可省）
* 每个信号在其报文缓存段上**按需**建金字塔（`add_signals` 时建一次，结果缓存在会话里）：
  * 层级：bucket 大小 = 10、100、1000、10000 …（直到桶数 ≤ 64）
  * 每桶存：`min f32`、`max f32`、`sum f64`、`count u32`（= 20 字节/桶）
  * 内存量级：`1M 采样点 ≈ 2.2 MB/信号`；**必须打印实测值**
* `view(t0, t1, width_px)`：
  1. 选层：取**最大**的层，使该层桶数 ≥ `width_px`（层次不够就退到最粗层，不可"没数据"）
  2. 二分定位 `[t0,t1]` 的桶区间（`read_at` 读少量记录即可，禁止整段读入）
  3. 从该层聚合出**恰好 ≤ 2×width_px** 个点返回；`truncated=true` 表示实际桶数少于请求
  4. **禁止在 view 里解码逐帧原始 payload**（那是 O(n_window)，会卡）——只有 `stats()`/`export_html()` 允许全量扫描
* 精度约定：包络形状用 f32；**读数与统计必须用 f64**（`stats()` 另算并缓存）。

### 4) 预算（契约 §1.4）
* `cache_budget_mb` 默认 **200**，允许 `32..8192`。
* 建缓存前先**估算**：`总帧数 × stride`（帧数用 `scan` 的 `frame_count` 求和即可，已实现）。
  估算 > 预算 → 返回错误 `CACHE_BUDGET_EXCEEDED`，消息必须包含：
  `需要 {need_mb} MiB，上限 {budget_mb} MiB；可选：①提高上限(cache_budget_mb) ②减少所选报文 ③不建缓存继续但较慢`。
* 跨会话复用：`open` 时若 `meta.json` 存在且 key 匹配、文件齐全、大小一致 → **直接命中**，`cache_hit=true`。
* LRU：`cache_dir` 下总字节超预算时，按 `meta.json` 的 `created_at` 从旧到新删（**不能删正在用的 key**）。

### 5) 增量预取（"加信号不冻结界面"）
* `add_signals` 里出现**未缓存报文** → 立即返回 `pending>0`，并在**后台线程**里跑一次建缓存（只针对缺失报文），
  进度通过 `prefetch_status()` 暴露；已经可用的信号**照常能画**。
* 预取完成后 `add_signals` 过的信号自动可用（下一次 `view` 就能出数据），不需要用户再点一次。

## 自检（必须真跑，输出贴进交付说明；主会话会独立复跑）

### 单元/集成测试
1. **缓存一致性**：同一 `selected` 下，从缓存解码出的信号值序列 与 直接 `decoder` 解码结果**逐值相等**（1e-12）
2. **缓存命中**：第二次 `open` 不读 BLF（用 `meta.cache_hit==true` + 计时对比证明；187MB 场景首次 vs 二次 ≥ 10× 差距）
3. **预算拒绝**：把 `cache_budget_mb=32` 跑一个需要几百 MB 的场景 → `CACHE_BUDGET_EXCEEDED`，消息含三项可选动作
4. **包络正确性（红线，不丢峰）**：在缓存上对随机 200 个窗口做 `view()`，与"暴力逐帧解码后手算 min/max"**逐桶比对**
   （f32 容差 1e-5 相对）；并构造"窗口内只有一个尖峰"的用例，断言尖峰出现在包络里
5. **金字塔层选择**：`width_px` = 200 / 800 / 2000 时返回点数分别 ≤ 2×width_px，且窗口越大用越粗的层
6. **时间口径零成本**：`set_time_format` 前后 `view()` 结果（min/max 数组）完全一致，且耗时 ≤ 16 ms
7. **增量预取**：只缓存报文 A，`add_signals` 报文 B 的信号 → `pending>0`；等 `prefetch_status().busy==false` 后
   `view()` 能出 B 的数据

### 性能自证（**必须打印实测数字**）
用 `fixtures/bench_huge.blf`（187 MB；不存在则用 `bench_large.blf` 并注明）：
* `open` 建缓存耗时、`cache_bytes`、缓存/源文件体积比
* 缓存命中二次 `open` 耗时
* 100 次随机 `view(2000px)` 的 **p50 / p95 / max**
* `add_signals`（已缓存信号）耗时
* 金字塔内存实测
* **验收线**：命中 open ≤ 300 ms；`view` p95 ≤ 30 ms；已缓存 `add_signals` ≤ 100 ms；`set_time_format` ≤ 16 ms

## 交付说明必须包含
* 文件清单 + 关键函数签名（含行号）
* 上面每组测试的**真实命令与输出**（禁止"应该/预期"字样）
* 缓存格式说明（stride 与字段偏移），以及 `read_at` 封装在两个平台上的实现要点
* 一句话回答：为什么 `view()` 不会随文件变大而变慢（层次选择 + 二分 + 常量桶数）

## 禁止事项
* 不许改契约、GUI、CI、README、`tests/acceptance.py`；不许改 `exporter.rs` 既有行为
* 不许加依赖（不许 memmap、不许 polars 新特性除非已在依赖里）；不许 `unwrap()/expect()/panic!` 出现在生产路径
* 不许把"全量扫描"塞进 `view()`；不许在 GUI 线程上做任何解码
* 不许为了让延迟达标而偷偷降低包络精度（min/max 必须覆盖真实极值）
