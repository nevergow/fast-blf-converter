# Step 15：Rust 侧画图数据接口 `series_for_plot`（min/max 分桶抽稀 + 统计）

## 你只能改这些文件
* `crates/fast_blf_core/src/series.rs`（**新建**，本步主战场）
* `crates/fast_blf_core/src/lib.rs`（导出 `series_for_plot` 这个 pyfunction + `mod series;`）
* `crates/fast_blf_core/tests/e2e_synthetic.rs`（集成测试）

**禁止改**：`docs/INTERFACE.md`（契约已冻结，**先通读 §1.3 与 §5 第 14/15/16 条**）、
`gui/**`（Step 16 才动）、`.github/**`、`README.md`、`使用说明.txt`、`tests/acceptance.py`、
`crates/fast_blf_core/src/exporter.rs`、`crates/fast_blf_core/src/decoder.rs`（**可读不可改**；
若确需改动必须在交付说明里单独说明并给出理由）。

## 目标一句话
百万级采样点**绝不能**搬进 Python 再画。本步在 Rust 里完成「解码 + 按时间分桶抽稀 + 统计」，
只把每信号 ≤ `buckets` 个点的结果交给前端。**不丢尖峰**是硬要求。

## 契约（照抄 `docs/INTERFACE.md` §1.3 实现，不要自己发明字段名）

```
fast_blf_core.series_for_plot(config: dict) -> dict
```

config：`blf_paths`（list[str]）、`dbc_by_channel`（dict[int,str]）、`selected`（同 convert）、
`time_format`（str）、`buckets`（int，默认 2000，允许 100..20000，越界报 `BAD_CONFIG`）、`temp_dir`（str，可不用）。

返回（字段名逐字一致）：
```python
{
  "ok": bool,
  "t_min": float, "t_max": float,          # 全局时间范围（秒；口径见下）
  "t_min_str": str | None, "t_max_str": str | None,   # absolute 口径必给
  "t_axis": [{"t": float, "label": str}, ...],        # absolute 口径给 8~12 个等间隔刻度；其他口径 []
  "buckets": int,                          # 实际桶数
  "series": [
    {"channel": int, "message": str, "signal": str, "unit": str | None,
     "count": int,                         # 原始采样点数（不是桶数）
     "t": [float, ...], "min": [float, ...], "max": [float, ...], "mean": [float, ...],
     "vmin": float, "vmax": float, "first": float, "last": float},
  ],
  "dbc_errors": {"1": str}, "warnings": [str],
  "errors": [{"file": str, "code": str, "message": str}]
}
```

### 时间口径（**最容易做错，逐字照做**）
* `t` 恒为 **float 秒**：`relative_s` → 相对秒；`relative_ms` → **也要转成秒**（不要返回毫秒）；
  `unix_s` → epoch 秒；`absolute` → **相对秒**（0 = 全局最早时间戳），绝对时间只出现在
  `t_min_str`/`t_max_str`/`t_axis[].label` 里。
* 复用 `exporter.rs` 里已有的时间换算与绝对时间格式化实现（**读它、抄它的口径**，不要另写一套导致两处不一致）。

### 抽稀（**本步的核心正确性**）
* 桶数 `buckets`（默认 2000）；按**全局** `t_min..t_max` **等宽**划分，**所有信号共用同一分桶网格**
  （这样多信号可对齐比较）。
* 桶索引：`idx = ((t - t_min) / (t_max - t_min) * buckets)` 向下取整，**clamp 到 `0..buckets-1`**
  （浮点边界必须 cl，否则最后一帧会越界）。
* 每桶输出：`t` = 桶中心时间（`t_min + (idx + 0.5) * 桶宽`）、`min`/`max`/`mean` = 桶内**全部原始采样点**的
  最小/最大/均值。
* **空桶（该信号在该桶内无采样）→ 该桶位置整体不出现在三个数组里**（三数组等长且一一对应）；
  不许补 0、不许前向填充（画图链路不做任何填充）。
* `count`/`vmin`/`vmax`/`first`/`last` 必须来自**全量原始数据**，不是抽稀后算的。
* `t`/`min`/`max`/`mean` 四个数组长度 = 该信号的**非空桶数** ≤ `buckets`。**绝不返回百万级数组**。

### 其他
* 宽松构建：单通道 DBC 失败只记 `dbc_errors` 并跳过该通道（与 `scan`/`convert` 一致，见 §5 第 9 条）。
* `selected` 为空 → 处理该通道全部报文；`blf_paths` 为空 → 返回空 `series` 且 `ok=true`（不报错）。
* 不新增依赖；生产路径禁止 `unwrap()/expect()/panic!`。
* 性能与内存（**判据是内存，不是读文件次数**）：
  * 峰值内存必须 **O(buckets × 信号数)**，**绝不许**把全量采样点攒在内存里；
  * **允许两阶段**：① 先只扫时间戳得到全局 `t_min`/`t_max`（不累积数据、不解码信号，等价于 `scan` 的轻量遍历）；
    ② 再解码 + 入桶。桶边界依赖全局时间范围，本来就无法在"只读一帧"时确定——
    只要"攒的是桶不是点"就满足要求。
  * 不许的做法：`Vec<all_samples>` 后再 `chunk_by_time`。

## 测试（必须真跑，输出贴进交付说明）

### 单元测试（`series.rs` 内 `#[cfg(test)]`，至少覆盖）
1. **桶划分边界**：`t_max` 处的最后一个点必须落进 **最后一桶**（不得越界/被丢）
2. **空桶跳过**：稀疏信号在多个桶里无采样 → 三数组长度 = 非空桶数，且 `t` 与真实有值的桶中心一致
3. **min/max 包络不丢峰**：构造一个"桶内只有一个极端尖峰"的序列，断言该桶 `max` 就是尖峰（**这是本步的红线**）
4. `count`/`vmin`/`vmax`/`first`/`last` = 全量原始数据口径（构造已知序列，逐个断言）
5. `buckets` 参数边界：=1、> 采样点数、=100、=20000；越界（99 / 20001）报 `BAD_CONFIG`
6. `time_format` 四种口径下 `t` 的关系（`relative_ms` 的值必须是 `relative_s` 的 1000 倍再除以 1000 后一致等）

### 集成测试（`tests/e2e_synthetic.rs`）
* 用 `fixtures/multi_channel.blf` + `fixtures/demo.dbc`：断言 `series` 里有 `VehSpd/Spd`、`BattTemp/Temp1`、
  `Insulation/Res` 三条；`count` 与 `scan` 报的 `frame_count` **逐个相等**；
  `vmin`/`vmax` 与"逐帧解码后手算"的结果一致（允许 1e-9 容差）。
* 用 `fixtures/timefmt.blf` + `fixtures/demo.dbc`：`time_format=absolute` 时
  `t_min_str` 必须等于 `2025-10-16 14:10:00.123456`（见 `docs/DEPS.md` §8 定值），`t[0] == 0.0`，
  `t_axis` 长度在 8..12 之间且 `label` 可被复现。

### 性能自证（**必须做，主会话会复核**）
* 用 `fixtures/bench_huge.blf`（187MB，若不存在则用 `bench_large.blf`）跑 `buckets=2000`，
  打印：总帧数、总耗时、峰值内存（用 `std::time::Instant` + 粗粒度 RSS 或说明无法测量）、
  以及**返回给 Python 的总点数**（`sum(len(s["t"]))`，必须 ≤ `2000 * 信号数`）。
* 验收线：187MB 文件 `series_for_plot` 耗时 **≤ 15 秒**（参考：`scan` 同文件约 5.7 秒），
  且返回点数远小于原始点数（打印比值）。

## 交付说明必须包含
* 新增/修改函数清单（名称 + 行号）；
* 上述每个测试的**真实命令与输出**；
* 一句话说明：为什么 `relative_ms` 口径返回的是秒、以及绝对时间从哪来的（贴出你复用的那几行代码）；
* 性能自证的实测数字。

## 禁止事项
* 不许改契约、GUI、CI、README/使用说明、`tests/acceptance.py`、`exporter.rs`/`decoder.rs`（只读）；
* 不许新增依赖；不许弹窗报错；不许在 GUI 未改的情况下写出调用方；
* 不许为了让测试通过而降低断言强度（尤其是"不丢峰"那条）。
