# Step 4 任务书（Windows 绿色打包脚本 + 冻结路径适配）—— 交给 DSH headless 执行

你是资深 Python 打包工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先读：`docs/SPEC.md`（§1 安全合规约束、§4 交付产物）、`gui/main.py`（了解主题文件是怎么加载的）。

## 背景铁律（不许违反）

1. **只允许 `--onedir`，严禁 `--onefile`**（onefile 每次启动向 %TEMP% 释放 DLL，会触发企业 EDR/杀软）。
2. **零权限纯用户态**：脚本不得请求管理员权限、不写注册表、不往 `C:\Program Files` / `System32` 写任何东西；所有产物落在项目目录内。
3. 目标机器是**受限的 Windows 办公电脑**，可能没有 pip 网络权限，因此脚本必须支持"离线增量"：能复用已存在的 `.venv-win` 与已下载的 wheel 缓存。

## 本次只做 Step 4，只允许新建/修改这几个文件

1. 新建 `build_windows_portable.bat`（主脚本）
2. 新建 `packaging/FastBLFConverter.spec`（PyInstaller spec，供脚本调用）
3. 修改 `gui/main.py` —— **只允许**改"主题文件与资源路径解析"这一处：让它同时支持开发态（`gui/theme.qss`）与 PyInstaller 冻结态（`sys._MEIPASS` / 可执行文件同级目录 `_internal`），并支持环境变量覆盖 `FBLF_THEME`。其余逻辑一律不许动。

## 1. build_windows_portable.bat 要求

- `@echo off` + `setlocal enabledelayedexpansion`，`cd /d "%~dp0"`；**每个关键步骤后检查 `errorlevel`，失败就打印红色可读的错误并 `exit /b 1`**（不要静默继续）。
- 步骤：
  1. 探测 Python：优先 `py -3.11`、其次 `py -3.12`、其次 `python`；打印实际版本；要求 ≥3.10。
  2. 创建/复用虚拟环境 `.venv-win`（若不存在则 `%PY% -m venv .venv-win`）。
  3. `pip install --upgrade pip` 后安装构建期依赖：`maturin`、`pyinstaller`、`PySide6`、`xlsxwriter`（后者虽未必用到也装上，避免运行时缺模块）。若网络不可用，打印清晰提示并尝试 `--no-index --find-links wheels` 离线目录。
  4. **编译 Rust 扩展**：`cd crates\fast_blf_core` → `maturin build --release --features extension-module --out ../../dist/wheels`（若 `maturin` 已自动启用 extension-module 也可不加该参数，但加了必须仍能通过）→ 回到根目录 → `pip install --force-reinstall dist\wheels\fast_blf_core-*.whl`。
  5. **Packaging**：`pyinstaller --noconfirm --clean packaging\FastBLFConverter.spec`。
  6. 把 `使用说明.txt` 与 `gui\theme.qss` 一起复制进产物目录（spec 里也要带 theme.qss）。
  7. 用 PowerShell 打包绿色 zip：`powershell -NoProfile -Command "Compress-Archive -Path 'dist\FastBLFConverter\*' -DestinationPath 'dist\FastBLFConverter-portable.zip' -Force"`。
  8. 结尾打印产物路径与 zip 大小。
- 脚本里**不要**用 `pause` 卡住自动化（除失败分支可加 `pause`）。
- 注释用中文，说明每一步在干什么、为什么（尤其是为什么必须 onedir）。

## 2. packaging/FastBLFConverter.spec 要求

- 入口 `gui/main.py`；`--windowed`（无控制台窗口）；`name='FastBLFConverter'`；`onedir`（`COLLECT`，**不要 ONE`FILE**）。
- `datas`：把 `gui/theme.qss` 放进 `gui/` 子目录（与开发态相对路径一致，便于同一套路径解析逻辑）。
- `hiddenimports`：至少包含 `fast_blf_core`、`PySide6.QtCore`、`PySide6.QtGui`、`PySide6.QtWidgets`。
- `excludes`：排除明显不需要的大包（`tkinter`、`matplotlib`、`numpy`、`pandas`、`IPython`、`pytest`），减小体积。
- 如果 Rust 扩展是用 abi3 构建的（我们是的：`abi3-py310`），在 spec 里加注释说明它已绑定 `fast_blf_core.pyd`，不要试图用 `collect_dynamic_libs` 去抓额外 dll。

## 3. 你**必须**做的自检（本机是 macOS，没有 Windows！）

你**无法**在 macOS 上跑 `.bat`，也**无法**产出真正的 Windows exe —— 这一点必须如实写进回复，不要假装验证过。你能做且必须做的是：

```
cd /Users/wuzeqin/Projects/fast_blf_converter
.venv/bin/python -m py_compile gui/main.py && echo "py_compile OK"
QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py --selftest; echo "selftest exit=$?"
```
（确认你对 main.py 的改动没有破坏 GUI 自检。）

另外，逐行复读你的 `.bat`：把每一条命令粘贴到回复里并**逐条说明它在 Windows 上的预期输出**（这是唯一能做的静态验证），并明确列出"未在真实 Windows 上验证"的清单。

## 4. 回复格式（只回复这些）

- 三个文件的行数
- 上述两条命令的真实输出
- `.bat` 逐条命令 + 预期输出说明
- 未能验证的部分（明确列出）
