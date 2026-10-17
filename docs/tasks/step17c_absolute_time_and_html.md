# Step 17c：`absolute` 时间口径 + HTML 报告真的带数据（修两个契约违反）

## 主会话独立验收发现的缺陷（有可复现证据）

复现命令（主会话已跑）：
```python
import fast_blf_core as core
cfg = dict(blf_paths=["fixtures/multi_channel.blf"], dbc_by_channel={1:"fixtures/demo.dbc"},
           selected={"1":{"VehSpd":["Spd"]}}, time_format="absolute",
           temp_dir="<tmp>", cache_budget_mb=200)
s = core.open_plot_session(cfg); s.add_signals(["ch1/VehSpd/Spd"])
s.info()["t_min_str"]     # → None    ❌ 契约 §1.4 要求 absolute 时必给
s.info().get("t_axis")    # → None    ❌ 契约要求给 8~12 个等间隔刻度标签
s.export_html(out, "报告"); # DATA.series[0]["t"] → []   ❌ 报告里一个数据点都没有
```

### 缺陷 1：`absolute` 口径缺 `t_min_str` / `t_max_str` / `t_axis`
契约 §1.4「时间轴与刻度（`absolute` 口径专用）」明确要求：
* `t_min_str` / `t_max_str`：`YYYY-MM-DD HH:MM:SS.ffffff`（BLF 头墙钟，不换时区）
* `t_axis`：`[{"t": float(相对秒), "label": str}, ...]`，约 **8~12 个等间隔刻度**，
  时间跨度 < 2 秒时 label 精确到微秒，否则到秒
* 其它口径：`t_min_str`/`t_max_str` 给 `None`、`t_axis` 给 `[]`

**要求**：复用 `exporter.rs` 里已有的绝对时间换算（BLF 头部墙钟 + 相对偏移），**不许另写一套口径**；
`view()` 与 `info()` 都要能用（推荐：`info()` 给 `t_axis`，`view()` 原样带出该字段，GUI/HTML 直接用）。

### 缺陷 2：`export_html()` 产出空报告
`DATA.series[*].t` 长度为 0 —— 报告只有骨架没有曲线。DSH 原测试只断言了"无外链"，**漏了"有数据"**。

**要求**：
* 报告必须内嵌**每个可见信号**的抽稀序列：用 `view(t_min, t_max, width=2000)` 取数（禁止用别的口径另算），
  字段含 `key/signal/unit/min/max/mean/t`。
* 报告头部必须含：标题、文件数、时间范围、每信号 `count/vmin/vmax/mean`（来自 `stats()`，全精度）、
  以及"已抽稀为 N 桶（min/max 包络，不丢峰）"的说明。
* `time_format == "absolute"` 时，横轴刻度必须用 `t_axis` 的 label（绝对墙钟），并在头部显示起止绝对时间。
* 仍然：**单文件、离线可开、零外链、零 CDN、不联网**；体积 < 8 MiB。

## 你只能改这些文件
* `crates/fast_blf_core/src/plot_session.rs`
* `crates/fast_blf_core/tests/e2e_synthetic.rs`

**禁止改**：`frame_cache.rs`（Step 17b 正在改它）、`docs/**`、`gui/**`、`.github/**`、其它。

## 自检（必须真跑，输出贴交付说明）
1. **`absolute` 定值**：用 `fixtures/timefmt.blf` + `demo.dbc`（见 `docs/DEPS.md §8`）：
   `info().t_min_str == "2025-10-16 14:10:00.123456"`、`t_max_str` 与最后一帧一致；
   `t_axis` 长度 8..12、严格单调、`label` 可复现。
2. **其它口径**：`relative_s/relative_ms/unix_s` → `t_min_str is None`、`t_axis == []`；
   且 `t` 仍是 float 秒（`relative_ms` 也必须是秒）。
3. **HTML 有数据**：`fixtures/multi_channel.blf` 三个信号 → 报告里 `DATA.series` 长度 3，
   每信号 `len(t) > 0` 且 `len(t) <= 2*2000`；`min/max/mean` 三数组与 `t` 等长。
4. **HTML 绝对时间**：`time_format="absolute"` 导出 → 文件里必须出现
   `\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}` 形状的标签（至少 2 个），且头部含起止绝对时间。
5. **离线自包含**：文件里 `fetch(`/`XMLHttpRequest`/`src=`/`http://`/`https://` 计数均为 0；体积 < 8 MiB。
6. 既有测试不回归（当前 80 项左右）。

## 交付说明必须包含
* 改动函数清单（名称 + 行号）
* 上面 6 条的**真实命令与输出**
* 一句话说明：绝对时间从哪来（贴复用的那几行）

## 禁止事项
* 不许改契约/文档/GUI/CI；不许加依赖；不许 `unwrap()/expect()/panic!` 出现在生产路径
* 不许在 `export_html` 里另起一套抽稀口径（必须走 `view()`）
* 不许为了让测试过而放宽断言
