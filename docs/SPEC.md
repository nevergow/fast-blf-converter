# Fast BLF Converter — 需求规格（原始需求摘录 + 硬性判据）

> 面向 Windows 办公环境的跨平台桌面应用：把大量 100MB 滚动切片 BLF 高速多通道解码、按报文拆分、导出 CSV/Parquet/Excel。
> 要解决的问题：CANalyzer Converter 单核串行慢、拆表后产生海量 null 空值、缺乏列式/多表导出。

## 1. 运行环境与安全合规（受限企业办公电脑）

- 零权限纯用户态：**不触发 UAC**、不读写注册表、不向 `C:\Program Files` / `System32` 写任何文件。
- 绿色免安装：PyInstaller **`--onedir`** 打包为自包含文件夹 → 压缩 `.zip`。
- **禁止 `--onefile`**（每次启动向 Temp 释放 DLL 会触发企业 EDR/杀软拦截）。

## 2. 技术栈

- GUI：Python 3.10+ / PySide6，暗色 Fluent QSS（`gui/theme.qss`）
- 核心：Rust 2021 经 PyO3 + maturin 编译为原生扩展 `fast_blf_core`
- Rust 依赖方向：BLF 流式解析（crate 见 INTERFACE.md §0）、`can-dbc`、`rayon`、`polars`（features: `parquet,csv,lazy,streaming,asof_join,interpolate`）、Excel 写出（`rust_xlsxwriter`）

## 3. 功能模块

### A. Rust 极速解码内核
1. 多通道 DBC 隔离与预编译：Ch1~Ch8 各绑独立 DBC；构建 `(channel, frame_id) -> MessageDecoder` 哈希表，O(1) 路由。
2. 跨字节位运算解包：原生 `SignalDecoder` 支持 Intel(LE) 与 Motorola(BE, MSB 标定)，含补码还原；全程寄存器运算，输出 `f64` 物理量（**不做枚举字符串转换**，保吞吐）。
3. 底层即时过滤：遍历 BLF 对象流时只读帧头，未勾选的 `(channel, frame_id)` 在字节切片层跳过，零反序列化开销。
4. 多文件并发 + 中间分片落盘：Rayon 并行消费多个 100MB BLF，打满多核；每文件按 `(channel, message_name)` 独立列式缓冲，处理完立即 Zstd Parquet 落盘；全进程内存峰值 < 2GB。

### B. 分表策略与导出流水线
- Polars `LazyFrame` 流式合并去重（按 `timestamp` 单调升序）。
- **分支 1（`split_by_message=true`，默认）**：各报文独立成表，表内无因周期不同产生的 null；可选按频率重采样（`join_asof(Backward)` + 线性插值）。
- **分支 2（`split_by_message=false`）**：全部信号合成单张宽表共用全局 `timestamp`；指定频率时生成等间隔时间轴并对齐（消除空值）；"保持原始时序"时 Outer Join + 可选前向填充。
- 导出格式三选一：CSV（默认推荐，Excel/WPS/JMP 直接可开）、Parquet（列存）、Excel（拆表模式=单文件多 Sheet，Sheet 名规范化 ≤31 字符）。
- **Excel 1,048,576 行上限防呆**：检测到 xlsx 且任一表 > 1,000,000 行 → 弹窗警告 + **自动降级 CSV**，严禁崩溃或截断丢数。

### C. PySide6 GUI
- 暗色 Fluent：底色 `#1E1E24`，高亮 `#0078D4`，圆角，清晰字体。
- 顶部工具栏：批量添加/拖拽 BLF、通道选择下拉（Ch1~Ch8）、给该通道绑定 DBC。
- 中部 `QSplitter`：左=BLF 文件列表树（文件名/大小/状态 `待处理`/`解析中`/`✓ 已完成`，支持从资源管理器拖拽多文件）；右=信号筛选树（通道→报文(标 ID 与周期)→物理信号(标单位)，三级复选框）+ 关键字实时搜索（按报文名/信号名模糊过滤，"全选当前过滤项"、"一键清空"）。
- 底部控制面板：`[✓] 按报文周期拆分为独立时序子表`、重采样频率下拉（保持原始时序/1/10/50/100 Hz）、导出格式下拉（CSV/Parquet/Excel）、主按钮 `[🚀 启动多核极速转换]`。
- 联动：取消勾选分表 → 重采样下拉高亮提示。
- 状态栏 + 平滑进度条：`Phase 1/2: 正在并行解码 (12/50)...`、`Phase 2/2: 正在流式合并与导出...`。
- 非阻塞：`QThread` 异步调 Rust 扩展，回调/线程安全通道实时回传每个 BLF 状态，CPU 满载时 UI 仍可流畅拖动。

## 4. 交付产物

```
fast_blf_converter/
├── Cargo.toml                  # workspace
├── pyproject.toml              # maturin 构建配置
├── build_windows_portable.bat  # Rust 编译 + PyInstaller onedir + 绿色 zip
├── crates/fast_blf_core/{Cargo.toml, src/{lib.rs,bit_slice.rs,decoder.rs,exporter.rs}}
└── gui/{main.py, theme.qss}
```

**无占位符**：不允许 `TODO` / `unimplemented!()` / 空实现。

## 5. 执行步骤

- Step 0（Hermes 追加）：BLF crate 选型 POC（因为 `vector-blf` 不存在）
- Step 1：骨架 + Cargo.toml + pyproject.toml + `bit_slice.rs`（含 Intel/Motorola 正负数单测）
- Step 2：`decoder.rs` + `exporter.rs`（BLF 流式、Polars 惰性拼接重采样、含防呆的三格式导出）
- Step 3：`gui/theme.qss` + `gui/main.py`（UI 交互、搜索过滤、拖拽、底部联动）
- Step 4：`build_windows_portable.bat` + 本地编译验证
