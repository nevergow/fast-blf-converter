# Fix A：帧缓存/会话的正确性与 GIL 安全（Pi 单点修复，2 个文件）

## 边界
**只能改**：`crates/fast_blf_core/src/frame_cache.rs`、`crates/fast_blf_core/src/plot_session.rs`、
`crates/fast_blf_core/tests/e2e_synthetic.rs`（新增测试）。
**不许改**：`lib.rs`、`exporter.rs`、`decoder.rs`、`docs/**`、`gui/**`、`.github/**`、`README.md`、`使用说明.txt`。
**不许加依赖**；生产路径禁止 `unwrap()/expect()/panic!`；跑完必须 `cargo test -p fast_blf_core` 全绿。

## 背景
这是一个 PyO3 扩展。帧缓存把每个报文的帧写成一个磁盘段文件（经典 CAN 17 B/帧：`t f64 + dlc u8 + payload[8]`，
CAN FD 73 B），用 `read_at`/`seek_read` 随机读；`PlotSession` 在其上建 min/max 包络金字塔，
`view()` 二分定位后逐桶读盘。以下是**只读审查（Pi 自己给的）**发现的真问题，逐条修：

## A1（HIGH，最重要）GIL 与锁：`compute_view` 持锁做 I/O，其他方法带 GIL 阻塞
* 现状：`plot_session.rs:206` 的 `compute_view` 全程持有 `state` Mutex 做逐桶 `read_at`；
  `close`(:799)、`info`(:625)、`prefetch_status`(:724) 在 **GIL 下** `lock()`。
  于是预取线程持锁时，UI 线程调 `info()`/`close()` 会**带着 GIL 等锁** → 整个解释器（含 Qt 事件循环）冻结。
* 要求：
  1. `compute_view` **先取快照再放锁**：锁内只取"段清单 + 时间范围 + 所选信号 + 桶网格参数"等元数据，
     立刻释放；随后所有文件 I/O 与解码都在锁外做（需要读的段用 `Arc<File>`/路径快照保证生命周期）。
  2. `close` / `info` / `prefetch_status` / `remove_signals` 一律用 `py.detach(...)` 包住再 `lock()`；
     它们内部**不得**再做长时间 I/O（`info` 只读内存快照；需要磁盘信息就缓存）。
  3. `close()` 必须能让正在进行的预取**在下一个检查点退出**（预取线程每个 chunk 检查 `closed`），
     并在写回 `meta`/pyramids 前再检查一次 `closed`（Pi 提到了 `:1523-1531`）。
* 自证：写一个 Rust 测试或说明如何在 Python 侧验证——见我给的验收命令（下面 V1）。

## A2（HIGH）`meta` 读-改-写竞态（`frame_cache.rs:316` 附近）
* 现状：`prefetch_cache` 的 `read_meta` → 改 → `write_meta` 无互斥，同 key 两个会话/线程会丢 `messages`。
* 要求：对 **key 目录**加互斥（**不引依赖**，用 `create_new(true)` 的锁文件 + 重试/超时实现跨平台互斥即可），
  锁内**重新读盘 → 合并 → 原子写**（写临时文件 + rename），绝不回写内存旧快照。
* 自证：两个线程分别以不同报文集合对同一 key 触发缓存/预取，结束后 `read_meta` 必须同时含两边的 `messages`。

## A3（HIGH）LRU 不能删"正在用"的缓存（`frame_cache.rs:1009`）
* 要求：
  1. `PlotSession::open` 时在 key 目录写租约文件 `.inuse-<pid>-<nonce>`，`close` 时尽力删除（失败也无害）；
  2. `enforce_lru` **跳过**存在任何 `.inuse-*` 的 key，且跳过 `meta.last_used_at` 在近 10 分钟内的 key；
  3. `meta` 增加 `last_used_at`，在每次 `open` / `view`（节流到 60s 一次即可）刷新。
* 自证：会话持有一个 key 时强行把预算压到 0 触发 LRU，该 key 目录**必须存在**；另一个无租约的旧 key 应被删。

## A4（MED，静默错值）`write_record` 的 dlc 截断（`frame_cache.rs:887`）
* 现状：payload 复制长度被截到 `payload_len`，但写进记录的 `dlc` 没截 → 解码侧按 dlc 只能拿到 8 字节/零填充，
  **高位信号静默丢位或把填充零当真值**。
* 要求：写记录时 `record[8] = dlc.min(payload_len as u8)`；若源 `data.len() < dlc`，除零填充外**必须**把
  该情况计入 `warnings`（计数 + 报文名，`info().warnings` 能看到），**不许静默**。
* 自证：单测构造 `dlc=64, data.len()=8` 与 `dlc=8, data.len()=4` 两种记录，断言写入的 dlc 被夹紧且告警计数增加。

## A5（HIGH）非有限值（NaN/inf）口径不一致（`plot_session.rs:1118` vs `:1343`）
* 要求：`exact_range_multi` 的原始帧兜底路径与金字塔构建**用同一个谓词**过滤非有限值
  （`if !value.is_finite() { continue; }`），并让 `BucketAgg::merge` **拒绝**非有限 min/max
  （返回 `None`/跳过该样本，不许把 NaN 混进 `sum`）。
* 自证：单测注入 NaN/inf 序列 → `view()` 的 `mean` 不是 NaN、`min/max` 有限、`count` 只算有效样本。

## A6（MED）读帧/解码错误不许静默（`plot_session.rs:1129`）
* 要求：把 `Err(_) => {}` 改成记录错误（计数 + 首次信息），并让 `compute_view` 因真实错误置
  `truncated=true` 且把这些错误带进 `warnings`（`view()` 的返回里已有 `truncated`；警告走 `info().warnings`）。
* 自证：人为删掉某个段文件后 `view()` 必须 `truncated=true` 且 `info().warnings` 含该段名。

## A7（MED）`truncated` 语义修正（`plot_session.rs` 里 `let truncated = missing || out_t.len() < width_px;` 那行）
* 现状：把"正常稀疏"（桶为空）也算作 truncated，UI 无法区分"真缺数据"。
* 要求：`truncated` **只**由真实缺失/错误置位；稀疏就是稀疏（点少但可信）。

## A8（MED）`stats()` 不许静默少返回（`plot_session.rs` 的 `catalog_by_key` 查不到 / 未就绪分支）
* 要求：
  * 未知 key → 返回 `BAD_CONFIG`（错误消息里点名该 key）；
  * 已知道但尚未缓存/金字塔未就绪的 key → 返回占位项 `{key, count: 0, pending: true, ...}`，
    **保持与入参等长**（调用方按位置对齐才不会错位）。
* 自证：`stats(["ch1/VehSpd/Spd", "不存在/报文/信号"])` → 抛 `BAD_CONFIG` 且消息含后者；
  `stats([已缓存key, 未缓存key])` → 返回 2 条，第二条 `count==0 && pending==true`。

## 验收命令（必须原样跑并把真实输出贴回来）
```bash
cd /Users/wuzeqin/Projects/fast_blf_converter
source ~/.cargo/env
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -25
# 期望：全部通过（当前基线 60 单元 + 80 集成；你新增的测试不许减少旧测试）
```
另外**必须**贴出：
* `git diff --stat`（证明只改了允许的三个文件）
* 每条 A1~A8 对应你新增的测试名与通过情况

## 一句话 note（交付说明里写）
每条修法用一行说明"改在哪里、为什么这样改不会引入新竞态"。
