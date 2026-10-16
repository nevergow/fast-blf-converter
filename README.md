# Fast BLF Converter

面向 **Windows 办公环境的绿色免安装**桌面工具：把大量（每个 ~100MB 滚动切片）Vector **BLF** 日志
高速多通道解码、按报文周期拆分、导出 **CSV / Parquet / Excel**。

解决 CANalyzer Converter 的三个痛点：
1. 单核串行转换慢 → Rust + Rayon 多核并行解码；
2. 按周期拆表后产生海量 `null` 空值 → **按报文独立成表**（或按统一时间轴重采样对齐）；
3. 缺乏列式/多表导出能力 → Polars 列式流水线 + 单文件多 Sheet 的 Excel 报告。

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
| 单元 + 集成测试 | **39 项全绿**（28 单元 + 11 集成 `tests/e2e_synthetic.rs`） |
| 端到端验收 `tests/acceptance.py` | **27/27 通过**（含 CSV/Parquet/Excel 三格式、无空值、百万行防呆、scan 三级树） |
| 位序正确性 | Intel/Motorola 与 `cantools` **1440 组随机对拍 0 不一致**（见 `docs/DEPS.md` §7） |
| 互操作（别的实现写的文件） | `python-can 4.6.1` 写的 **187 MB / 4,082,000 帧** BLF（含扩展帧 + CAN FD）：我们的 scan 逐通道帧数与写入方**完全一致** |
| 性能（单文件 187MB） | scan 32.7 MB/s（5.7s / 408 万帧，单文件=单线程） |
| 性能（8 个切片并行） | **162–172 MB/s ≈ 5.0–5.3× 单文件**（两次实测）；convert 输出墙钟 **242–246 MB/s** |
| 冻结打包（PyInstaller onedir） | spec 实跑通过；**release 扩展下产物 172MB**（debug 扩展下为 296MB，不代表出货体积）；打包后的二进制可运行、Rust 内核就绪、主题从 `_internal/gui/theme.qss` 正常加载 |
| 出货构建路径（`.bat` 同款命令） | `maturin build --release --features extension-module` → **21MB abi3 wheel**（`cp310-abi3`）；装该 wheel 后端到端验收仍 **27/27** |

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
