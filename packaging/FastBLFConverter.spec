# -*- mode: python ; coding: utf-8 -*-
"""PyInstaller 打包配置 —— Fast BLF Converter（Windows 绿色便携版）。

合规约束（见 docs/SPEC.md §1）：
* 只用 ``onedir`` + ``COLLECT``，**严禁 onefile**：onefile 每次启动都会把
  扩展模块/DLL 释放到 ``%TEMP%``，属于"自我释放"行为，极易触发企业 EDR/杀软拦截。
* ``console=False``（等价于 ``--windowed``）：不弹黑框；全程零权限纯用户态，
  不写注册表、不写 ``Program Files``/``System32``，产物只落在项目 ``dist/`` 内。

用法（由 build_windows_portable.bat 调用）::

    pyinstaller --noconfirm --clean packaging\\FastBLFConverter.spec
"""

from pathlib import Path

# ------------------------------------------------------------------ 路径定位 --
# ``SPECPATH`` 由 PyInstaller 在执行 spec 前注入。不同版本/调用方式下它可能是
# spec 所在目录、项目根或 spec 文件本身，这里统一向上回溯到含 gui/main.py 的根。
def _find_project_root(start: Path) -> Path:
    for candidate in (start, *start.parents):
        if (candidate / "gui" / "main.py").is_file():
            return candidate
    return start


_SPEC_START = Path(SPECPATH).resolve()  # noqa: F821 - PyInstaller 注入的全局变量
ROOT = _find_project_root(_SPEC_START if _SPEC_START.is_dir() else _SPEC_START.parent)


# ---------------------------------------------------------------------- 分析 --
a = Analysis(
    [str(ROOT / "gui" / "main.py")],
    pathex=[str(ROOT)],
    binaries=[],
    # 把 theme.qss 放进产物的 gui/ 子目录：与开发态 ``gui/theme.qss`` 相对路径一致，
    # gui/main.py 里的 resolve_theme_path() 无需分支即可同时命中两种形态。
    # gui/assets/ 一并收进去（v0.3 的离线 HTML 报告要内嵌里面的 echarts.min.js）：
    # 冻结态缺失该文件时报告会失败，所以必须在打包期就带上。
    datas=[
        (str(ROOT / "gui" / "theme.qss"), "gui"),
        (str(ROOT / "gui" / "assets"), "gui/assets"),
    ],
    # 内核是延迟导入（运行时 try/except import），静态分析抓不到，必须显式声明。
    hiddenimports=[
        "fast_blf_core",
        "PySide6.QtCore",
        "PySide6.QtGui",
        "PySide6.QtWidgets",
    ],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    # 排除本应用完全不用的大包，显著压缩 onedir 体积与启动扫描面。
    excludes=[
        "tkinter",
        "matplotlib",
        "numpy",
        "pandas",
        "IPython",
        "pytest",
    ],
    noarchive=False,
)

# Rust 扩展 fast_blf_core 以 abi3-py310 构建（见 crates/fast_blf_core/Cargo.toml
# 的 pyo3 features），产物是单个自包含的 ``fast_blf_core.pyd``；PyInstaller 会沿
# ``hiddenimports`` 自动抓取它以及 PySide6 自带的 Qt DLL。
# 因此这里**不要**用 ``collect_dynamic_libs("fast_blf_core")`` 之类再去捞额外 dll ——
# abi3 扩展没有旁挂的动态库，强行收集只会把无关 DLL 带进包、徒增体积与 EDR 误报面。

pyz = PYZ(a.pure)


# ------------------------------------------------------------------ 可执行体 --
exe = EXE(
    pyz,
    a.scripts,
    [],
    exclude_binaries=True,  # onedir 关键：依赖不塞进 exe，交给下面的 COLLECT
    name="FastBLFConverter",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=False,  # 不做 UPX 压壳：加壳会明显提高被杀软/EDR 误报的概率
    console=False,  # 等价 --windowed，无控制台窗口
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
)


# -------------------------------------------------------------------- 目录集 --
# 只生成目录（COLLECT），不生成单文件；这正是 SPEC.md 要求的绿色 onedir 形态。
coll = COLLECT(
    exe,
    a.binaries,
    a.datas,
    strip=False,
    upx=False,
    upx_exclude=[],
    name="FastBLFConverter",
)
