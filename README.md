# Fast BLF Converter

面向 **Windows 办公环境的绿色免安装**桌面工具：把大量（每个 ~100MB 滚动切片）Vector **BLF** 日志
高速多通道解码、按报文周期拆分、导出 **CSV / Parquet / Excel**。

解决 CANalyzer Converter 的三个痛点：
1. 单核串行转换慢 → Rust + Rayon 多核并行解码；
2. 按周期拆表后产生海量 `null` 空值 → **按报文独立成表**（或按统一时间轴重采样对齐）；
3. 缺乏列式/多表导出能力 → Polars 列式流水线 + 单文件多 Sheet 的 Excel 报告。

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
| 单元 + 集成测试 | **89 项全绿**（39 单元 + 50 集成 `tests/e2e_synthetic.rs`） |
| 端到端验收 `tests/acceptance.py` | **55/55 通过**（A4 三格式/无空值、A5 百万行防呆、**A6 坏 DBC 容错**、**A6' 空 BLF 扫描**、**A7 时间口径 4 选 1 精确比对**、**A9 宽表同名信号**） |
| DBC 解析矩阵 | **24/25 种真实写法可解析**（空 `NS_` 段 / `SG_MUL_VAL_` / `SIG_VALTYPE_` / `EV_` / 扩展帧 / CRLF / ENUM 越界…）；唯一失败用例是文件本身引号不规范，按预期给出精确行号 |
| GUI 无人值守自检 | 5 条全过：`--selftest` / `--selftest-deep`（`DEEP_OK channels=2 dbc_nodes=2 messages=3`）/ `--selftest-config`（`CONFIG_OK`）/ `--selftest-progress`（`PROGRESS_OK phase1=2 phase2=2 frames_max=1212600`）/ `--dump-check-states`（`DUMP_OK`） |
| GUI 交互路径独立探针 | **11/11**：通道级绑 DBC → 解析 → 报文名从 `ID_0x123` 变真名 → 取消勾选 → **重解析后勾选被恢复** → 全部清除 |
| 冻结态（PyInstaller onedir，本机实跑） | `--selftest` 内核就绪 0.2.0 / `--selftest-config` **`CONFIG_OK`（预设保存·加载·DBC 缺失处理在打包后可用）** / `--selftest-deep` `DEEP_SKIP no_fixture` / `--dump-check-states` `DUMP_OK` 23079B；`theme.qss` 落在 `_internal/gui/theme.qss` |
| 位序正确性 | Intel/Motorola 与 `cantools` **1440 组随机对拍 0 不一致**（见 `docs/DEPS.md` §7） |
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

**https://github.com/nevergow/fast-blf-converter/releases/tag/v0.2.0** → 下载 `FastBLFConverter-portable.zip`（72.9 MB）

```bash
# 或用 gh CLI（私有仓库需先登录）
gh release download v0.2.0 -R nevergow/fast-blf-converter -D ./ci-out
```

sha256：`97fd56633c3e3176febdecc5dc933b7ac58048edb08ad562926cc282d255282d`

Release 同时附了复选框视觉基线图 `check-states.png`（三态 + 行选中高亮的渲染对照）。

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
