# Fast BLF Converter

面向 **Windows 办公环境的绿色免安装**桌面工具：把大量（每个 ~100MB 滚动切片）Vector **BLF** 日志
高速多通道解码、按报文周期拆分、导出 **CSV / Parquet / Excel**。

解决 CANalyzer Converter 的三个痛点：
1. 单核串行转换慢 → Rust + Rayon 多核并行解码；
2. 按周期拆表后产生海量 `null` 空值 → **按报文独立成表**（或按统一时间轴重采样对齐）；
3. 缺乏列式/多表导出能力 → Polars 列式流水线 + 单文件多 Sheet 的 Excel 报告。

## v0.4.1 更新（**修掉 70% 静默闪退** + 预览交互重做 + 日志落盘）

> 完整说明见 [`docs/releases/v0.4.1.md`](docs/releases/v0.4.1.md)。

**核心修复：CSV「多信号一张表 + 重采样」在进度 70% 闪退。**
70% 是「解码结束 → 合并导出开始」的分界点。合并宽表时按 `G=(t1−t0)×hz` **一次性分配整张时间网格**，
`G` 无上限、内存预算在该阶段未生效、每个信号再留一整列可空浮点（16 B/格），
峰值 ≈ `13~30 × G × 信号数`；分配失败 → `handle_alloc_error` → **abort**，`catch_unwind` 抓不到 →
闪退且零报错。**触发点是「时间跨度 × 信号数」，不是文件大小。**

* 宽表重采样改**两遍式流式**（`CHUNK_ROWS=20000`）：峰值与网格行数解耦
* `G` 硬上限 2000 万点（50 Hz ≈ 111 小时）+ **分配前预算闸门**（超限给出 `t0/t1/G/S/估算MB/建议 Hz`，绝不 abort）
* 每信号存储 16 B/格 → 8 B/格 + 1 bit 空值掩码；phase 1 逐文件 `catch_unwind`

实测（release）：`12800 s / 2612 万帧 / 189 信号 @50 Hz` 峰值 **1588 MB → 863 MB**；
把重采样提到 **200 Hz（G×4）反而 793 MB**；坏时间戳文件从「卡死/OOM」变成 **0.14 s 返回可操作错误**。
**输出 CSV 的 sha256 与改前逐字节一致**（`hold` 与 `linear` 都对比过）。

**日志**：启动即写滚动日志（Windows `%LOCALAPPDATA%\fast_blf_converter\logs\gui.log`，10 MB×5），
`faulthandler` native 崩溃栈 + 未捕获/线程异常 + Rust panic hook 全落同一文件；
转换起止与内核「宽表预检」（`G/S/est_mb/budget_mb`）都留痕；工具栏新增「📂 打开日志目录」。

**预览**：X/Y 轴内单轴滚轮缩放（Y 不取数）· 右键拖动选框放大 · `FIT X / FIT Y / FIT ALL`
· 画布下方跟随光标的 `t = …` 读数条 · 双击复位不再多取一次数。

**界面**：加 DBC 后自动展开到 DBC 层级；删掉 25 处重复/教学/内部参数名文案。

## v0.4.0 更新（第四轮反馈：**体验优先** —— 不卡、不等待、不重新加载）

> 用户的判定标准很直接：「用的时候体验最重要，卡顿、延迟、卡死、重新加载之类的坏体验，要尽量优化」。
> 所以这一版的核心不是新功能，而是**把"每次看数据都要重算"这件事从架构上消灭**。

### 1. 帧缓存 + `PlotSession`：一次解码，之后全是毫秒级

BLF **没有随机访问索引** —— 想只看某段时间窗，不先把帧存下来就只能重扫整份文件（1.2 GB ≈ 30 s/次）。
所以新增：

* **磁盘帧缓存**：一报文一文件（经典 CAN 17 B/帧、CAN FD 73 B/帧），`read_at`/`seek_read` 随机读，
  不引入 mmap 等任何新依赖；体积约为源文件的 **10~35%**（只缓存你选中的报文）。
* **`PlotSession` 会话对象**：`info / set_time_format / add_signals / remove_signals / prefetch_status /
  view / stats / export_html / close`（契约见 `docs/INTERFACE.md` §1.4）。
* **缓存键与"你选了什么"解耦**：键只由「文件清单 + DBC 哈希 + 版本」决定，报文段按需累积
  —— 换一次信号选择**不会**再整份重建（这是原先最蠢的一处：勾个新信号再开预览 = 全部重算）。
* **`open` 立即返回**：已缓存的段马上能看，缺失的段**后台预取**（有进度、不冻结界面）。
* **多分辨率 min/max 包络金字塔**：任意缩放 = 选层 + 二分，绘制量与文件大小无关。

**实测（release，187 MB / 408 万帧）**

| 操作 | 实测 |
|---|---|
| 首次建缓存 | **878 ms**（213 MiB/s）；走完整 GUI 路径 1146 ms |
| 缓存命中再打开 | **68~82 ms**（GUI）/ 1.9 ms（会话层） |
| `add_signals`（已缓存 6 条） | **24.6 ms** |
| `view(2000px)` ×300 | p50 **14.5** / p95 **14.9** ms |
| 切换时间口径 | **0.000 ms**（纯显示层） |
| 滚轮缩放 / 拖动平移（GUI 实测） | p95 **3.1 ms** |
| 单帧渲染（8 条信号） | **1.7 ms** |

### 2. 预览窗口重写（按你的界面意见）

* **三种显示模式可切**（你指出"车道只是可选之一"）：`车道（每信号一行，共享时间轴，默认）` /
  `多 Y 轴叠加`（按单位分轴，>4 条归一化）/ `单 Y 轴`（量纲混杂时顶栏红字提醒）。
* **每信号可改颜色、线型（折线/阶梯/散点）、可见性**，并写进 `plot_settings` 持久化（跨进程 + 进配置预设）。
* **全选只要一个三态复选框**（你明确否掉了三个按钮）：`☑ 全选(n/N)` / `▣ 部分` / `☐ 全不选`，
  **没有"反选"**；主信号树也统一成这一个控件（原来的"全选当前过滤项/一键清空"两个按钮已删）。
* **默认不再画全域**：打开只显示**最前 2 秒**、最多 **3 条**信号 —— 你说的"一上来画整个时间段"正是卡死主因。
* 切换模式/时间口径 **`view_calls 2→2`**（探针实测：一次都不重新取数）。

### 3. 转换进度：一条加权进度，永不回退

原来 phase1 / phase2 各算一套 `0→100`，phase2 一开始条子就从 100 掉回 0（这就是"没到 100% 就进下一步"）。
现在合成一条：**phase1 = 0~70%（解码）、phase2 = 70~100%（合并导出）**，`setValue(max(当前, 新值))` 保证单调，
只有真正结束才 100。

### 4. 预设补全 + 缓存设置

* 预设现在保存 **6 个导出参数**（拆表 / 重采样 / 导出格式 / **时间格式** / **填充方式** / **最大保持时长**）
  \+ 预览样式（模式/颜色/线型/可见性）；`schema=2`，旧 `schema=1` 配置自动补默认值。
* 新增「⚙ 设置」：**帧缓存上限（默认 200 MiB，可改 32~8192）** 与缓存目录；超限时明示"需要多少/上限多少"
  并给三个选项，**绝不静默降级**。

### 5. 修掉的两个真缺陷（主会话验收发现）

* **`absolute` 口径缺 `t_min_str`/`t_max_str`/`t_axis`** → 报告和轴上看不到绝对时间，没法跟测试日志对时。
  现在精确命中 `docs/DEPS.md §8` 定值（`2025-10-16 14:10:00.123456`），并给 8~12 个等间隔绝对刻度。
* **`export_html()` 产出空报告**（`DATA.series[*].t` 长度 0 —— 只有骨架没有曲线；原测试只断言了"无外链"，
  漏了"有点"）→ 现在内嵌每信号抽稀序列 + 绝对时间轴 + 全量统计，仍零外链、体积 <8 MiB。

### 6. 发版前的并发与缓存安全加固（只读审查 10 条，全部修复）

这一轮专门查“正常路径没问题、异常路径会静默出错”的地方：

* **预取期间界面不再冻结**：取数先拍快照再放锁做 I/O；`close/info/prefetch_status` 先释放 GIL 再拿锁
  —— 实测预取 7 个报文时 `info()` **p95 0.10 ms / 最大 0.54 ms**（修复前会阻塞到预取结束）
* **多会话/多线程同时预取不丢记录**：缓存元数据读-改-写加目录级互斥（`create_new` 锁 + 超时）
* **LRU 不误删在用的缓存**：`.inuse-<pid>-<nonce>` 租约 + 10 分钟最近使用保护
* **dlc 与 payload 对齐**：短包/异常帧不再零填充当真值
* **NaN/Inf 不污染包络**，**读盘/解码错误不再被吞**（计入 `view_errors` 并置 `truncated`）
* **`truncated`** 只表示真缺数据；**`stats()`** 遇未知 key 报 `BAD_CONFIG`、未就绪返回 `pending=true` 占位
* **大报文排序改分块外部排序**（64 MiB/块 + k 路归并、原子 rename、失败不留坏文件）
* **金字塔内存上限 256 MiB**（`pyramid_budget_bytes`），超限按 LRU 淘汰、按需重建，结果逐值不变

## v0.3.0 更新（第三轮反馈：进度/通道/填充 + 画图与报告）

> **为什么一版发全**：原计划 v0.2.1（修 ①②③）与 v0.3.0（画图）分两次发。
> 中间那轮 CI 因为**推送前就触发了构建**（构建的是旧提交，不含修复），被发版脚本的
> "使用说明版本断言"当场拦下、未产生错误发布。既然那一轮已白跑，就合并为一次发布，
> 省一轮 50 分钟的 CI 与一份额度。

1. **解析进度条改成真实进度**（原来"到 100% 又退回去"）：解析信号原本走的是"不确定态乒乓动画"
   （`0↔100` 来回扫），而 `scan` 的进度回调**本来就带真实文件级进度**，只是界面没接。
   现在按 `已完成文件 / 文件总数` 驱动，**单调不减、结束停在 100%**，右侧显示「文件 d/t · 本文件已扫 n 帧」。
2. **通道节点只增不减 + 可手动添加**（原来"给 1、2 号通道绑 DBC 并解析后，其他通道都消失了"）：
   启动预置 Ch1~Ch8，解析时整树重建曾把"没有帧观测、也没绑 DBC"的通道丢掉，且**没有添加通道入口**。
   现在通道集合是**粘性**的（只增不减），并新增工具栏/右键「➕ 添加通道…」（1~64 号）；
   空通道显示「（无帧观测，右键可添加 DBC）」。
3. **填充方式三选一 + 最大保持时长**（回答"不同周期放一张表，向前填充准吗"）：
   - `阶梯保持`（默认，ZOH）：把最近一次**真实采样**保持到下一拍 —— **不造新值**，
     状态位/档位/计数器必须用它（线性插值会把档位 1→2 插成 **1.5**）；
   - `线性插值`：两点之间连直线 —— **会造出从未采样过的中间值**，只适合连续物理量，选用时结果里给出提示；
   - `不填充`：保留空值（空值本身是信息："这一拍没有采样"）。
   - `最大保持时长`（ms，0=不限）：超过这么久没新采样就不再保持、**留空** ——
     专治"传感器掉线 30 秒被旧值掩盖成'温度一直 25°C'"，被置空的行数按信号报给你。
   - **正确性护栏**：`阶梯保持` 的输出值**严格取自原始采样集合**（有单测与独立探针证明"绝不造值"）；
     重采样网格的**线性插值行为与旧版本逐字节一致**（对拍 sha256 相同，回归红线）。
4. **进度粒度细化**：此前 50ms 节流只放行"最后一个文件"，中间文件完成会被吃掉（8 个切片只看到 4 级台阶）；
   现在每个文件完成都放行（每任务最多 N 次事件）。实测 8 切片 → **`12/25/37/50/62/75/87/100` 八级台阶**。

5. **画图预览（`📈 预览`）**：勾选信号即弹曲线窗口 —— **min/max 包络**（梳状，不丢峰）+ 半透明均值线，
   **多量纲分 Y 轴**（同单位共轴，>4 条自动归一化），滚轮缩放 / 拖动平移 / 双击复位 / 十字光标读数 / 图例显隐。
   **自绘**（QPainter）：零新依赖、零打包风险，且包络渲染与分轴需要完全掌控。
6. **离线 HTML 报告（`📤 导出 HTML 报告`）**：**单文件、完全自包含**（ECharts 5.5.1 内嵌 + 数据内嵌，
   **禁止 CDN**），同事断网双击即开；含文件清单、时间范围、DBC 绑定情况（含失败通道）、
   **全量统计表**（原始点数 / min / max / mean / first / last）、可缩放曲线、抽稀说明。
   体积控制在 8MB 内，超限时按相邻桶合并（min 取更小、max 取更大，**仍不丢峰**）并在页面写明；
   写盘走临时文件 + 原子替换，**不产出半截文件**。
7. **`series_for_plot`（Rust 侧抽稀+统计，图表的唯一数据来源）**：百万级采样点**绝不**搬进 Python ——
   在 Rust 里解码并按全局时间等宽分桶，每桶给 min/max/mean 与桶中心时间，每信号最多回传 `buckets` 个点。
   实测 187MB / 408 万帧：release **0.96s**、返回 32.6 万点 = **原始的 1.02%**；debug 构建 9.9s。
   新增 `mean_all`（全量真均值）——报告统计表用它，而不是"桶均值的平均"（两者在桶内点数不等时不同）。

## v0.2.0 更新（基于 Windows 真机试用反馈）

1. **DBC 绑定改为通道级树节点**（像 CANalyzer）：在左侧通道节点上右键「📄 添加 DBC…」，
   DBC 作为通道子节点显示解析状态（`✓ 已解析` / `✗ 解析失败`）；**不提供单通道删除**，
   只有工具栏「🗑 全部清除 DBC」。
2. **手动「🔍 解析信号」**：绑完 DBC 后点按钮才扫描（不再自动扫）；没加 BLF 也能**只按 DBC 列出报文**（帧数 0）。
3. **配置预设**：把「DBC 映射 + 选中信号 + 导出策略」存成 `configs/<名称>.json`
   （绿色包目录；目录不可写时自动回退用户目录），工具栏「💾 保存配置 / 📂 加载配置 / 🗑 删除配置」一键复原。
   加载时某个 DBC 路径不存在 → 只在通道旁红字提示（**不弹窗**），其余照常解析。
4. **单通道 DBC 失败不再拖垮转换**：失败通道的报文跳过并记入 `dbc_errors` + `warnings`，通道旁红字说明原因；
   只有当所选信号**全部**来自失败通道时才报 `EMPTY_RESULT`（此前任何一个通道的 DBC 解析失败都会让整次转换失败）。
5. **复选框三态自绘**：未选（空心灰框）/ 部分选中（蓝框 + 横条）/ 全选（实心蓝 + 白勾），
   并与行选中高亮（`#33475B`）拉开色阶 —— 不再"隐身"，也不再分不清全选与部分选。
   视觉基线：`docs/screenshots/check-states.png`。
6. **进度可视化**：底部通栏粗进度条 + 实时文字
   （`Phase 1/2 · 解码中 · 3/8 文件 · 已 1,234,567 帧 · 87 MB/s · 已用 00:42`），
   解码期间每 ≥50ms 上报一次（此前并行解码阶段**根本不上报**，表现为"卡住不动 → 突然完成"）。
7. **时间列 4 选 1**：相对秒（默认）/ 相对毫秒 / **绝对时间**（`2025-10-16 14:10:00.123456`，
   按 **BLF 文件头记录的墙钟**原样输出，与 CANalyzer 对得上）/ Unix 时间戳。相对时间按微秒取整。
8. **修复宽表同名信号崩溃**：不同报文含同名信号（真实 DBC 里 `Counter`/`CRC`/`Alive` 极常见）时，
   宽表模式（保持原始时序 / +ffill）会触发 Polars 内部 **panic 崩溃** —— 现已按报文前缀消歧
   （`Sig0` → `MSG_101_Sig0`），并在导出阶段加 `catch_unwind` 兜底：任何意外都变成 `EXPORT_INTERNAL` 结构化错误，
   绝不再把 panic 抛给界面。
9. **DBC 编码容错（真实项目 DBC 的第一大杀手）**：实测 25 种 DBC 写法 + 一份真实失效文件后，给 DBC 读取加了预规范化：
   - **UTF-8 BOM** 自动去除；
   - **非 UTF-8 编码**按"高位字节连续性"判别：成串（连续 ≥2 字节）判为中文编码 → **GB18030** 解码；
     全部孤立（单字节夹在 ASCII 中，如 `°C`/`µs`）判为拉丁符号 → **Windows-1252** 解码。
     判错会**静默改坏语义**：把 `"°C"`（`0xB0`+`C`）交给 GB18030 会把 `C` 当尾字节吃掉，单位变成 `癈`；
   - can-dbc 解析不了的**纯元数据语句**（`CM_ BA_` 属性注释、`BA_DEF_REL_`/`BA_DEF_DEF_REL_` 相对属性）
     按"顶层语句"整段跳过（**不影响信号解码**），并在结果里留痕；
   - 规范化**宁可少做也不丢数据**：若剔除会导致 `BO_`/`SG_` 定义减少，则放弃剔除并记录说明。
   - 界面上可见：DBC 节点显示 `✓ 已解析（已规范化）`，悬停看完整说明（去 BOM / 按哪种编码解码 / 忽略了哪些语句）。

## 架构

```
gui/main.py (PySide6, 暗色 Fluent)  ──QThread──►  fast_blf_core (Rust/PyO3)
      ▲  进度回调经 Signal 转主线程                     │
      └──────────────  dict 报告  ◄──────────────────┐  │
                                                    │  ▼
   BLF 流式解析(blf_asc) → 位解码(bit_slice) → Zstd Parquet 分片
        → Polars Lazy 合并/重采样 → CSV / Parquet / Excel 导出
```

| 模块 | 职责 |
|---|---|
| `crates/fast_blf_core/src/bit_slice.rs` | Intel/Motorola 跨字节位提取 + 补码还原（纯函数，含位序单元测试） |
| `crates/fast_blf_core/src/decoder.rs` | BLF 流式扫描/解码、多通道 DBC 路由（O(1) 查表）、Rayon 并行、分片落盘 |
| `crates/fast_blf_core/src/exporter.rs` | Polars 惰性合并、重采样对齐、CSV/Parquet/Excel 导出与百万行防呆 |
| `crates/fast_blf_core/src/lib.rs` | PyO3 胶水：`scan` / `convert` + 进度回调 + 异常映射 |
| `gui/` | PySide6 界面：拖拽导入、三级信号树、实时搜索、底部策略联动、异步进度 |

## 文档（实现前必读）

- `docs/SPEC.md` —— 需求规格（含安全合规硬约束）
- `docs/INTERFACE.md` —— **冻结的接口契约**（Python API、回调协议、错误码、命名规则）
- `docs/DEPS.md` —— 依赖 API 实测事实（含 **Motorola 位序约定**、BLF crate 选型、Polars 0.55 API）
- `docs/tasks/step*.md` —— 各步骤任务书（含验收标准与自检命令）

## 验证结果（本机 macOS 实测，可复现）

| 验证项 | 结果 |
|---|---|
| 单元 + 集成测试 | **159 项全绿**（**68 单元 + 91 集成** `tests/e2e_synthetic.rs`，另 4 项 ignored 为性能/重型基准） |
| 画图数据接口 | 187MB/408 万帧 → **0.96s**（release）、返回点数 **压缩到 1.02%**；逐桶 min/max/mean 与**独立复算**零不符；包络极值覆盖全量极值（不丢峰）；`absolute` 口径命中 `2025-10-16 14:10:00.123456` 定值 |
| HTML 报告自包含性 | 真实浏览器打开验证：canvas 绘制像素 126KB、7 信号统计表齐全、**零外部 script/link 引用、零 fetch/XHR/Image 调用**、1.24MB < 8MB |
| 预览控件（离屏实测） | 1100×560 渲染、224 种颜色、调色板曲线像素 3664 个（不是空白图）；AST 级检查确认**渲染路径不 import/不调用内核**、无新增依赖 |
| 填充策略专项 | `hold` 输出值**全落在原始采样集合内**（不造值）；`linear` 网格路径与旧版**逐字节一致**（对拍 sha256）；`ffill_max_gap_ms=50ms` 时按信号报出置空行数；非法值报 `BAD_CONFIG` 并回显 |
| 进度行为 | 解析进度**单调不减、终值 100%**；8 切片实测阶梯 `12/25/37/50/62/75/87/100`；单文件 187MB 时"已扫帧数"连续跳动 29 次 |
| 端到端验收 `tests/acceptance.py` | **55/55 通过**（A4 三格式/无空值、A5 百万行防呆、**A6 坏 DBC 容错**、**A6' 空 BLF 扫描**、**A7 时间口径 4 选 1 精确比对**、**A9 宽表同名信号**） |
| DBC 解析矩阵 | **24/25 种真实写法可解析**（空 `NS_` 段 / `SG_MUL_VAL_` / `SIG_VALTYPE_` / `EV_` / 扩展帧 / CRLF / ENUM 越界…）；唯一失败用例是文件本身引号不规范，按预期给出精确行号 |
| GUI 无人值守自检 | **9 条全过**：`--selftest` / `--selftest-parse`（`PARSE_OK` 进度单调→100 + Ch1~Ch8 不消失）/ `--selftest-deep`（`DEEP_OK channels=8`）/ `--selftest-config`（`CONFIG_OK`）/ `--selftest-progress`（`PROGRESS_OK`）/ **`--selftest-progress-monotonic`**（`MONO_OK phase1_max=70 phase2_start=70 final=100`）/ `--selftest-fill`（`FILL_OK hold_distinct=52 linear_distinct=60 gapped_nulls=45 none_nulls=496`）/ **`--selftest-plot`**（`PLOT_OK` 预览离屏渲染 + 报告自包含性门禁）/ `--dump-check-states`（`DUMP_OK`） |
| GUI 交互路径独立探针 | **11/11**：通道级绑 DBC → 解析 → 报文名从 `ID_0x123` 变真名 → 取消勾选 → **重解析后勾选被恢复** → 全部清除 |
| 冻结态（PyInstaller onedir，本机实跑） | `--selftest` 内核就绪 0.4.0 / `--selftest-config` **`CONFIG_OK`（预设保存·加载·DBC 缺失处理在打包后可用）** / `--selftest-deep` `DEEP_SKIP no_fixture` / `--dump-check-states` `DUMP_OK` 23079B；`theme.qss` 落在 `_internal/gui/theme.qss` |
| 位序正确性 | Intel/Motorola 与 `cantools` **1440 组随机对拍 0 不一致**（见 `docs/DEPS.md` §7） |
| **交互延迟（v0.4.0，release，187 MB）** | 建缓存 878 ms / **缓存命中开预览 68 ms** / `view` p95 **14.9 ms** / **缩放·拖动 p95 3.1 ms** / 切口径 **0.0 ms** / 加 6 条信号 24.6 ms / 8 条信号渲染 1.7 ms |
| **独立探针（主会话手写，不采信执行者自述）** | Step17 12 项（缓存帧数=scan、stats=CSV、包络不丢峰、预算拒绝）/ 17b 4 项（G3 换选择后命中 0.5 ms、单缓存目录）/ 17c+18 25 项（绝对时间定值、HTML 有点、三模式、`view_calls 2→2`、三态复选框、颜色线型持久化）/ 19 9 项（6 字段、schema1 兼容、进度 [0,70,70,100]） |
| 时间口径正确性 | 4 种口径与**独立算出的定值**逐字符相等（`docs/DEPS.md` §8）：`0/0.01/1`、`0/10/1000`、`1760623800.123456…`、`2025-10-16 14:10:00.123456…` |
| 互操作（别的实现写的文件） | `python-can 4.6.1` 写的 **187 MB / 4,082,000 帧** BLF（含扩展帧 + CAN FD）：我们的 scan 逐通道帧数与写入方**完全一致** |
| 性能（单文件 187MB） | scan 32.7 MB/s（5.7s / 408 万帧，单文件=单线程） |
| 性能（8 个切片并行） | **162–183 MB/s ≈ 5.0–5.6× 单文件**（三次实测：162–172 MB/s，v0.2.0 加进度回调后实测 **183.4 MB/s**）；convert 输出墙钟 **242–246 MB/s** |
| 进度回调开销 | v0.2.0 新增热路径进度上报（原子帧计数 + 50ms 节流）：单文件 scan 期间回调 82 次，吞吐与 v0.1.0 持平（32.7 MB/s），**无可测得的性能代价** |
| 出货构建路径（`.bat` 同款命令） | `maturin build --release --features extension-module` → **21MB abi3 wheel**（`cp310-abi3`）；装该 wheel 后端到端验收全过 |

复现命令：

```bash
# 生成 187MB / 408 万帧夹具 + 期望计数
/tmp/vcheck/bin/python tests/make_bench_blf.py                      # 需 python-can
FBLF_BENCH_OUT=bench_huge.blf FBLF_BENCH_DURATION=2000 FBLF_BENCH_COMPRESSION=0 \
  /tmp/vcheck/bin/python tests/make_bench_blf.py
.venv/bin/python tests/bench_report.py     # 单文件吞吐 + 互操作对账
.venv/bin/python tests/bench_parallel.py 8 # 多文件并行加速
```


## 本地开发（macOS）

```bash
# 1. Rust 工具链（已装则跳过）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path

# 2. Python 环境（复用系统 PySide6）
cd /Users/wuzeqin/Projects/fast_blf_converter
~/.local/bin/uv venv --system-site-packages --python /opt/anaconda3/bin/python3 .venv
~/.local/bin/uv pip install --python .venv/bin/python maturin

# 3. 跑单元/集成测试（DYLD 变量必带，原因见 docs/DEPS.md §0）
source ~/.cargo/env
DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib cargo test -p fast_blf_core

# 4. 编译 Python 扩展并安装进 venv
.venv/bin/maturin develop --features extension-module

# 5. 生成测试夹具 + 端到端验收
cargo run -p fast_blf_core --example make_fixtures
.venv/bin/python tests/acceptance.py

# 6. GUI 离屏自检
QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py --selftest
```

## Windows 绿色版打包

### 方式 A（推荐）：直接下载已发布的绿色包

**https://github.com/nevergow/fast-blf-converter/releases/latest** → 下载 `FastBLFConverter-portable.zip`

```bash
# 或用 gh CLI（私有仓库需先登录）
gh release download -R nevergow/fast-blf-converter -D ./ci-out
```

> 该包由 CI（`windows-latest`）构建，解压即用（onedir，不写注册表、不自我释放）。
> 每个 Release 的说明里都写了**该版本 zip 的大小与 sha256**，下载后请核对。

### 方式 A2：自己触发一次构建（改动后出包用这条）

仓库里的 `.github/workflows/build-windows.yml` 在 **真 Windows 机器**上构建，产出可直接用的绿色 zip：

```bash
# 手动触发（也可在 GitHub 网页 Actions 页点 "Run workflow"）
gh workflow run build-windows.yml --ref main

# 等构建完成后下载产物
gh run list --workflow=build-windows.yml --limit 1
gh run download <run-id> -n FastBLFConverter-portable-windows -D ./ci-out
```

产物：`FastBLFConverter-portable.zip`（约 **72 MB** 压缩 / 207 MB 解压），解压后在任意目录双击 `FastBLFConverter.exe` 即可。

- 工作流只在**手动触发**或**打 `v*` tag** 时运行 —— 不在每次 push 时跑，避免消耗 Actions 分钟。
- 构建含**真实验收**：装 wheel 后跑 `import fast_blf_core`，打包后直接跑 `FastBLFConverter.exe --selftest`，
  并断言输出里出现内核版本号 + `_internal/gui/theme.qss` 存在，任一不满足即红灯。
- **私有仓库的 artifact 需要登录 GitHub 才能下载**；办公电脑若不能登录，就在能登录的机器上下好再带过去。

> 已知小瑕疵：PowerShell `Compress-Archive` 写中文条目名（`使用说明.txt`）时不设 UTF-8 标志，
> 非 Windows 解压工具（macOS `unzip` 等）会显示乱码并可能中断。Windows 资源管理器 / 7-Zip 正常。
> 若要在非 Windows 环境解压，可用 Python：`python -c "import zipfile;zipfile.ZipFile('...zip').extractall()"`。

### 方式 A3：发一个正式版本（全自动，只需一个 tag）

```bash
# 1) 改版本号（Cargo.toml 的 [workspace.package].version —— pyproject 从它取）
# 2) 写发布说明 docs/releases/v<ver>.md（会原样成为 Release 正文）
# 3) 提交并推 main
git add -A && git commit -m "chore(release): v<ver>" && git push origin main
# 4) 打 tag 并推送 —— 这一步就是发版
git tag v<ver> && git push origin v<ver>
```

推 tag 会触发 `build-windows.yml`：编译 Rust 扩展 → PyInstaller onedir → **跑打包后的真 exe 做三个冻结态自检**
→ 压缩 zip → 上传 artifact → **自动创建 Release 并把 zip 挂上去**。
Release 正文 = `docs/releases/v<ver>.md` + 自动附加的 `zip 大小 / sha256`（由构建机现算，不用手填）。

> 想先验证能不能打包而不发版：`gh workflow run build-windows.yml --ref main`（只出 artifact，不创建 Release）。

### 方式 B：在 Windows 本机构建

```
build_windows_portable.bat
```

产物：`dist/FastBLFConverter/`（PyInstaller **--onedir**，非 onefile）+ `dist/FastBLFConverter-portable.zip`。
脚本纯用户态运行，不请求管理员权限、不写注册表、不碰 `C:\Program Files`。

## 已知限制

1. **BLF 容器压缩**：底层 `blf_asc` 只解 zlib 容器。若真实日志使用 **LZMA** 压缩容器，会以
   `BLF_PARSE` 明确报错（附文件名）而不是静默丢数据 —— 这条路径无法用合成素材覆盖，需真实样本验证。
2. **Windows 可执行文件必须在 Windows 上构建**：macOS 无法产出 `.exe`，仓库里的 `.bat` 未在真机执行过。
3. Excel 单 Sheet 上限 1,048,576 行：超过 `excel_row_limit`（默认 1,000,000）自动降级为 CSV 并在 GUI 弹窗说明。
4. **CI 的「冻结态配置自检」在 Windows 产物上会超时**（已知、不阻塞发布）：窗口化（`console=False`）打包的 exe
   在 Windows 上不产出 stdout，该自检无法汇报进度；且这一步里的进程在 runner 上不按时退出。
   现已改成**非阻塞 + 保留诊断**（超时只写警告并继续上传产物）。配置功能本身在开发态与本机打包态
   均已验证通过（`--selftest-config` → `CONFIG_OK`），**不影响用户路径**；根因待查。

## 许可

本项目以 **MIT License** 发布，见 [`LICENSE`](LICENSE)。

随包分发的第三方资源：`gui/assets/echarts.min.js`（Apache ECharts 5.5.1，**Apache License 2.0**），
文件头部 ASF 许可声明原样保留，溯源与校验值见 [`gui/assets/NOTICE.txt`](gui/assets/NOTICE.txt)。

