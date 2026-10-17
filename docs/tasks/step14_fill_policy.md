# Step 14：填充策略（hold / linear / none）+ 最大保持时长（防丢帧被旧值掩盖）

## 你只能改这些文件
* `crates/fast_blf_core/src/lib.rs`（config 解析与转发）
* `crates/fast_blf_core/src/exporter.rs`（填充逻辑，主战场）
* `crates/fast_blf_core/tests/e2e_synthetic.rs`（集成测试）
* `gui/main.py`（把 `ffill` 复选框换成「填充方式」下拉 + 「最大保持时长」输入）
* `gui/theme.qss`（仅当新控件需要样式；优先不改）

**禁止改**：`docs/INTERFACE.md`（契约已冻结）、`.github/**`、`README.md`、`使用说明.txt`、`docs/tasks/**`、`tests/acceptance.py`。

## 背景：用户的原话与主会话的实测结论
用户问：「增加一个功能，就是向前填充，如果多个不同频率放到一张表，周期不同而产生空值，向前填充会准吗？」

主会话用 `fixtures/multi_channel.blf` 实测（`split_by_message=false`）：
* 原时序宽表 501 行：`Spd`(10ms) 空 1、`Temp1`(100ms) 空 **451**、`Res`(1000ms) 空 **496**；
* `ffill=true` 后空值归零，但三个信号的**独立取值个数没变**（分别是 500 / 1 / 5）——
  ffill 不造假值，但它把"未采样"变成了"采样了且没变"；
* 10Hz 网格路径下 `Res`（1Hz，只有 5 个真实采样）被**线性插值**成 50 行连续斜坡 ——
  **这是我们现在的默认行为，对状态/枚举/计数类信号是危险的**（档位 1→2 会被插成 1.5）。

**所以本步要交付两件事**：① 填充**方式**可选（阶梯保持 / 线性插值 / 不填充）；② 加"最大保持时长"，
防止通信中断/丢帧被旧值掩盖成"一直正常"。

## 契约（照抄实现，不得擅自改语义；详见 `docs/INTERFACE.md` §1.1 与 §5 第 12/13 条）

### `fill_policy: str`
* 取值 `"none"` / `"hold"` / `"linear"`；其他值 → 报 `BAD_CONFIG`（错误消息要人话，附上收到的值）。
* **未显式给出时按路径取默认**（关键，不许搞错）：
  * 重采样网格路径（`resample_hz != None`）→ `hold`
  * 保持原始时序的宽表（`split_by_message == false`、`resample_hz == None`）→ `none`（= 现状 `ffill=false`）
  * 拆表模式（`split_by_message == true`）→ 填充**不生效**（同一报文内周期一致，不产生跨信号空值）
* `ffill` 字段继续有效：`ffill=true` ⇔ 显式 `hold`（仅对"保持原始时序的宽表"生效，保持向后兼容）；
  `fill_policy` 与 `ffill` 同时给出时以 `fill_policy` 为准。
* **`linear` 必须在 `warnings` 里报**：`已对 {n} 个信号做线性插值（可能包含从未采样过的中间值）`。

### `ffill_max_gap_ms: float | None`
* 默认 `None` = 不限制。仅 `hold` 生效；给了就必须 > 0，否则 `BAD_CONFIG`。
* 语义：某行往前找最近的真实采样，若 `行时间 - 真实采样时间 > ffill_max_gap_ms/1000` 秒 → **该行置空**
  （不再保持）。行首（第一个真实采样之前）本来就无值可保持 → 一律保持为空，不受本参数影响。
* 受影响行数**按信号**计入 `warnings`：
  `Ch1/VehSpd：{n} 行因超过最大保持时长（{ms}ms）被置空`。

### 各路径实现要求
| 路径 | `none` | `hold` | `linear` |
|---|---|---|---|
| 宽表 + 网格 | 无意义 → **按 `hold` 处理并记 warning** | 网格点取「≤该点的最近真实采样」，点之前为 null | **必须与今天的 `resample_linear` 行为逐字节一致**（回归红线） |
| 宽表 + 原时序 | = 现状 `ffill=false`（留空） | = 现状 `ffill=true`（ZOH 前向填充） | 新增：在**并集时间轴**上对每个信号做线性插值（复用网格那套插值实现，把并集时间戳当作网格） |
| 拆表 | 不生效 | 不生效 | 不生效 |

## 实施要求（逐条验收）

### Rust
* **R1** `lib.rs`：解析 `fill_policy`（`Option<String>`）、`ffill_max_gap_ms`（`Option<f64>`），解析出"本路径生效的最终策略"
  （`EffectiveFill { None, Hold, Linear }` 之类），随 `ExportConfig` 传给 `exporter.rs`。**不许**在 exporter 里再读原始 dict。
* **R2** `exporter.rs`：新增/改造填充入口，保证：
  * `hold` 是**阶梯保持**：输出值必须**严格取自该信号的原始采样值集合**（不许出现任何插值出来的中间值）；
  * `linear` 网格路径与今天行为一致（**先跑一次基线再改**：把 `resample_hz=10` 的结果存成文件，改完 diff 必须为空）；
  * 行首无值 → `null`（不得用第一个值回填）；
  * `ffill_max_gap_ms` 逻辑对两条路径都生效，置空行数要能统计出来（`warnings` 用）。
* **R3** `linear` 的 warning 与 `ffill_max_gap_ms` 的 warning 都必须进 `convert` 返回值的 `warnings`（人话，带信号名/通道）。
* **R4** 不许引入新依赖；`unwrap()/expect()/panic!` 仍然禁止出现在生产路径。
* **R5（顺带修好进度粒度，独立小改动）**：`lib.rs` 的 `ProgressThrottle::emit_decode()` 目前**只对"最后一个文件"**
  无条件转发（`done >= total` 那条分支），中间文件完成的**进度跳步会被 50ms 闸门吃掉**。
  实测：8 个切片解析只看到 `[12, 50, 75, 100]` 四级台阶（应为 8 级）。
  改法（≤5 行）：在映射**之前**先判断"本事件是否为文件完成"（`payload.phase == 1 && payload.total > 0 && payload.done >= payload.total`），
  是则映射后直接 `self.force(mapped)` 并返回 `true`，否则走原 `self.emit(mapped)` 节流。
  上限分析：每任务最多 `文件数` 次无条件转发（通常 ≤ 50），帧级事件仍走 50ms 节流，运行时成本可忽略。
  **验收证据要求**：改完后用 8 个 7MB 切片实测，进度条去重取值数必须 **≥ 8**（修复前实测为 4）——
  这一条主会话会独立复跑（探针 `/tmp/probe_13c.py` 就是它，把门槛改成 ≥8 即可）。

### 测试（必须真跑）
* **T1** 单元测试（`exporter.rs` 的 `#[cfg(test)]`）：用**内存构造**的序列覆盖
  * `hold` 阶梯性：网格点值 ∈ 原始采样集合（构造一个断言"没有任何值不在原始集合里"）
  * `linear` 单调斜坡：相邻插值点在两点之间
  * `ffill_max_gap_ms`：跨越 >gap 的地方必须出现 null，`<=gap` 的地方必须保持
  * 行首（第一个采样之前的网格点）必须是 null
* **T2** 集成：`tests/e2e_synthetic.rs` 加至少 3 个用例（`fixtures/timefmt.blf` + `fixtures/demo.dbc`），
  断言 `hold` 的值全部来自原始采样、`linear` 与旧行为一致、`ffill_max_gap_ms=50` 时出现 null。
* **T4（对应 R5）**：在 `lib.rs` 的 `ProgressThrottle` 单元测试里断言——
  连续 N 次「单文件完成」事件（`phase=1, done=1, total=1`）在**同一节流窗口内**都必须返回 `true`（即全部被转发），
  而连续 N 次「文件内帧进度」事件（`phase=1, done=0, total=1`）在窗口内**只应通过第一次**（节流仍生效）。
* **T3** 全量回归：`cargo test -p fast_blf_core` 必须全绿（当前 89 项；新增后不减旧项）。
  命令：`source ~/.cargo/env && DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core`

### GUI
* **G0 现状确认（主会话已核实，不要搞错）**：GUI **目前没有任何填充控件**（底部面板只有 `chk_split`），
  Rust 侧 `ffill` 字段存在但**全程序无人调用**。所以本步是**新增**控件，不是替换；不存在 `chk_ffill` 回归风险。
* **G1** 新增下拉框「填充方式」（`combo_fill`）：
  `阶梯保持（推荐）` = `hold`、`线性插值` = `linear`、`不填充` = `none`；默认选中与路径默认一致。
  * 拆表模式 → 禁用 + tooltip：`拆表模式下同一报文周期一致，不产生空值，无需填充`
  * 网格模式 → 只提供 `阶梯保持` 与 `线性插值`（`不填充` 置灰）
  * 原时序宽表 → 三个都可选
* **G2** 新增「最大保持时长」行：`QSpinBox`（0~60000 ms，后缀 ` ms`，0 = 不限，默认 0），
  tooltip 必须写清用途：`向前保持超过该时长仍未收到新采样时，该点留空——避免通信中断/丢帧被旧值掩盖`。
  仅 `填充方式 = 阶梯保持` 时启用，其他情况置灰。
* **G3** 组装 config 时写 `fill_policy`、`ffill_max_gap_ms`（0 → `None`）；**不再写 `ffill`**（保留读取兼容）。
* **G4** 新增 `--selftest-fill`（规格见下），并保证既有 6 条自检全部不回归：
  `--selftest` / `--selftest-parse` / `--selftest-deep` / `--selftest-config` / `--selftest-progress` / `--dump-check-states`
  新控件加入后这些自检的输出文案如受影响需一并更新（语义不许变）。

### `--selftest-fill` 规格（必须实现，主会话会独立复跑）
* 用 `fixtures/timefmt.blf` + `fixtures/demo.dbc`（Ch1 / TimeMsg），`resample_hz=1.0`、`split_by_message=false`、`export_format=csv`、`time_format=relative_s`；
* 跑 4 组：`hold` / `linear` / `hold + ffill_max_gap_ms=50` / `none`（宽表原时序 `resample_hz=None`）；
* 读回 CSV，断言：
  * `hold`：所有非空取值**必须属于原始采样值集合**（集合由 `scan` 之外的原始 BLF 帧解码得到，或直接用
    `convert` 的 `resample_hz=None` 输出做来源）→ 这一条是"不造假值"的证据；
  * `linear`：网格点数量与 `hold` 相同，且取值序列与"按时间线性插值"的期望一致（允许 1e-9 容差）；
  * `hold + gap=50`：出现 null（因为 1Hz 采样间隔 1000ms > 50ms）；
  * `none`（原时序宽表）：空值行数 > 0；
* 输出单行：`FILL_OK hold_distinct={n} linear_distinct={m} gapped_nulls={k} none_nulls={j}` → `return 0`；
* 断言失败：`FILL_FAIL <原因>` → `return 2`；退出统一 `os._exit()`。

## 交付说明必须包含
* 改动函数清单（名字 + 行号）与"最终策略推导"写在哪一行；
* **R2 的回归基线对比**：改 `resample_linear` 前后同一输入的输出 diff（必须为空）；
* T1/T2/T3、`--selftest-fill`、既有 6 条自检的**真实命令与输出**；
* 一句话说明你如何保证 `hold` "绝不产生原始采样集合之外的值"。

## 禁止事项
* 不许改契约文档、CI、README、使用说明、`tests/acceptance.py`；
* 不许引入新依赖；不许弹窗报错；不许为了通过测试而放宽断言；
* 不许改动 `linear` 网格路径的既有数值行为（回归红线）。
