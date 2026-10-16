# Step 7a 任务书（进度回调下沉到热路径）—— 交给 DSH headless 执行

你是 Rust 系统工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先完整读：
- `docs/INTERFACE.md` **§2（进度回调协议，刚更新：新增 `frames_done`，允许工作线程调用 + Mutex 节流 + Python::attach）**
- `crates/fast_blf_core/src/lib.rs`（`scan` 与 `convert` 的回调现状）
- `crates/fast_blf_core/src/decoder.rs`（`process_file` / `scan_file` / `Sink` 的热路径）

## 背景（真实用户反馈）

> "转换时没看到进度条。"

根因（已定位，有代码证据）：
- `lib.rs` 里并行解码阶段传的是 **no-op 回调**：`let noop = |_p: Progress| {};` + `.map(|path| process_file(path, &plan, &sink, mem_budget, &noop))`；
- 真正的 Python 回调只在**所有文件处理完之后**的收集阶段串行触发。
- 结果：phase 1 干活的整段时间一个进度都收不到，界面表现为"卡住不动 → 突然完成"。

这是当初"回调绝不允许并发调用"造成的副作用，现在按 INTERFACE §2 的新约定修（Mutex 节流 + 每次 `Python::attach` 取 GIL，GIL 天然串行化，安全）。

## 本次只做 Step 7a，只允许改这三个文件

1. `crates/fast_blf_core/src/decoder.rs`（热路径回调）
2. `crates/fast_blf_core/src/lib.rs`（接线 + 节流器）
3. `crates/fast_blf_core/tests/e2e_synthetic.rs`（补测试）

不要动 `gui/`（另有并行任务）、`exporter.rs` 的功能逻辑（除进度接线）、`Cargo.toml`、`docs/`。

## 1. 回调下沉到热路径

- `process_file`（与 `scan_file`）：**解码循环内部**必须按节流调用传入的 `progress` 闭包，而不是只在文件结束时调一次；
  - `Progress.frames_done`：当前文件已解码帧数（每帧递增即可，用局部 `u64` 计数，零成本）；
  - `Progress.done/total`（phase 1）：**文件维度** —— 已完成文件数 / 文件总数；文件**开始**时发一次（done=已完成数），文件**结束**时再发一次（done 递增）；
  - `Progress.message`：人话，例如 `正在解码 slice_03.blf…`（含已解码帧数的格式化由 GUI 负责，Rust 只要把数字放进 `frames_done`）。
- `lib.rs` 里的节流器：一个 `Fn(Progress)` 包装，内部 `Mutex<Option<Instant>>`，仅当距上次 ≥ **50ms** 才真正 `Python::attach` 调用 Python 回调；被节流掉的事件直接丢弃（不要排队，避免内存增长）。
- 并行阶段**不再使用 no-op**：每个 rayon 工作线程都调用同一个节流闭包（闭包需 `Send + Sync`，用 `Arc<ProgressThrottle>`）。
- 回调抛异常：吞掉、记入 warnings，**绝不**中断任务（保持现有行为）。
- phase 2（合并/导出）同样要发进度：`stage:"merge"` / `"export"`，`done/total` 用步数（例如分片合并张数、写出表数）。

## 2. 测试（写进 `tests/e2e_synthetic.rs`）

1. **解码期间确实有多次回调**：用一个本地闭包（不走 Python）在 `process_file` 上计数 `AtomicUsize`；
   对一台 5 秒素材（500+50+5 帧）跑一遍，断言**至少收到 3 次回调**（文件开始/结束 + 节流触发），
   且每次回调的 `done <= total`、`frames_done` 单调不减。
2. **`frames_done` 真实反映进度**：断言最后一次回调的 `frames_done` == 该文件被选中报文的实际帧数（500+50 或 5）。
3. **节流生效**：连续 1000 次模拟调用中，实际触发次数 < 1000（节流窗口内被丢弃）。这条可以直接测节流器本身，不必真跑解码。
4. **回归**：现有全部测试仍绿。

## 3. 自检（必须跑，贴真实输出）

```
source ~/.cargo/env
cd /Users/wuzeqin/Projects/fast_blf_converter
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core 2>&1 | tail -40
```
（DYLD 变量必带，见 DEPS.md §0；dyld 报错是环境问题，不要改代码。）失败就修到全绿。
另跑 `cargo clippy -p fast_blf_core 2>&1 | tail -10` 并贴出。

## 4. 回复格式（只回复这些）

- 改动文件与行数、`cargo test` 真实输出、clippy 摘要、偏离说明（没有就写"无偏离"）。
