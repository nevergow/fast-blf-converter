# Fix B：两处资源上限（Pi 单点修复，2 个文件）

> 前置：Fix A 已合入（GIL/锁、meta 竞态、LRU 租约、dlc 截断、NaN、错误上报、truncated、stats）。
> 本步只处理**内存/磁盘资源上限**两条，不碰业务语义。

## 边界
**只能改**：`crates/fast_blf_core/src/frame_cache.rs`、`crates/fast_blf_core/src/plot_session.rs`、
`crates/fast_blf_core/tests/e2e_synthetic.rs`。
**不许改**：`lib.rs`、`exporter.rs`、`decoder.rs`、`docs/**`、`gui/**`、`.github/**`、`README.md`、`使用说明.txt`。
不许加依赖；生产路径禁止 `unwrap()/expect()/panic!`；结束必须 `cargo test -p fast_blf_core` 全绿。

## B1（HIGH）`sort_segment` 全量进内存 → 大报文 OOM
* 现状（`frame_cache.rs:913` 附近）：时间戳回退时（多切片重叠/时钟跳变）把整段读进
  `Vec::with_capacity(frames)`（每帧 `RawRecord` ≈ 88 B）再排序。帧数上亿时分配数 GB，在写盘中途 abort。
* 要求：
  1. 改成**分块外部排序**：按固定内存预算（常量 `SORT_CHUNK_BYTES = 64 * 1024 * 1024`）把记录分成 chunk，
     每块排序后写临时文件，最后做 **k 路归并**写回段文件（保持 17/73 字节的既有记录布局不变）。
  2. 临时文件放在同目录（便于 rename），结束时清理（成功/失败都要清）。
  3. 记录任何异常路径都**不许半途留下"半排序"的段文件**：先写 `<name>.tmp`，成功后 rename 覆盖。
  4. 段文件布局（17/73 B）与 `meta` 里的 `count`/`first_t`/`last_t` 必须与实际一致。
* 自证：新增测试构造 **300 万条逆序**记录（用 3 份重叠切片或直接合成）→ 排序后严格非降、
  `count` 一致、`view()` 能取到正确 min/max；并在测试里打印 chunk 数（应 > 1，证明确实分块）。

## B2（MED）金字塔内存无上限 → 常驻膨胀
* 现状（`plot_session.rs:1312` 附近）：`base_buckets = frames.div_ceil(10)`，每桶 20 B，各层常驻，
  `state.pyramids` 无字节预算，只有 `close()` 才清。
* 要求：
  1. 常量 `PYRAMID_BUDGET_BYTES = 256 * 1024 * 1024`（256 MiB）。
  2. 累计金字塔字节超预算时，按 **LRU** 淘汰其它信号的金字塔（记录最后使用时间）；
     **正在本次 `view()`/`stats()` 里用到的那个信号不许淘汰**。
  3. 被淘汰后再次需要时**按需重建**（重建成本 O(该信号帧数)，可接受）；重建次数与被淘汰字节写进 `warnings`。
  4. `info()` 已有 `pyramid_bytes` 字段：保持准确（当前常驻总量），并新增 `pyramid_budget_bytes`。
* 自证：构造一个超预算场景（例如 12 个信号 × 大帧数，或把预算常量在测试里用 cfg 覆盖），断言：
  `pyramid_bytes <= PYRAMID_BUDGET_BYTES`、`warnings` 里有淘汰记录、淘汰后 `view()` 结果仍与未淘汰时**逐值相同**。

## 验收命令（原样跑并贴真实输出）
```bash
cd /Users/wuzeqin/Projects/fast_blf_converter
source ~/.cargo/env
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -30
```
外加：`git diff --stat`（证明只改允许文件）+ 每条 B1/B2 的测试名与输出。

## 交付说明
每条一行：改在哪、怎么保证"不半途留坏文件"、"淘汰后结果不变"。
