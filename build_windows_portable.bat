@echo off
rem ============================================================================
rem  Fast BLF Converter —— Windows 绿色便携版一键打包脚本（Step 4）
rem ----------------------------------------------------------------------------
rem  产出：dist\FastBLFConverter\ 目录 + dist\FastBLFConverter-portable.zip
rem
rem  安全铁律（见 docs/SPEC.md §1）：
rem    * 只允许 --onedir，严禁 --onefile。onefile 每次启动都会把 DLL 释放到
rem      %TEMP%，属于"自我释放"行为，极易触发企业 EDR/杀软的拦截规则。
rem    * 零权限纯用户态：不请求管理员、不写注册表、不往 C:\Program Files 或
rem      System32 写任何东西；所有产物都落在本项目目录内（dist\）。
rem    * 目标机可能是无外网的办公电脑，因此本脚本支持"离线增量"：
rem      复用已存在的 .venv-win 与 pip 自带 wheel 缓存，并提供 wheels\ 离线
rem      目录兜底（把依赖 wheel 预放进项目根的 wheels\ 即可离线安装）。
rem
rem  用法：双击本文件，或在项目根执行  build_windows_portable.bat
rem  自动化（失败时不要 pause）：先执行  set FBLF_NO_PAUSE=1
rem ============================================================================

rem 脚本以 UTF-8 保存；切到 65001 让 cmd 正确显示中文提示，避免乱码。
chcp 65001 >nul

setlocal enabledelayedexpansion
rem 切到脚本所在目录，保证相对路径（crates\、packaging\、dist\）都正确。
cd /d "%~dp0"

set "ROOT=%~dp0"
set "VENV=%ROOT%.venv-win"
set "VPY=%VENV%\Scripts\python.exe"
set "WHEEL_DIR=%ROOT%dist\wheels"
set "OFFLINE_DIR=%ROOT%wheels"
set "APP_NAME=FastBLFConverter"
set "DIST_DIR=%ROOT%dist\%APP_NAME%"
set "ZIP_PATH=%ROOT%dist\%APP_NAME%-portable.zip"

echo ============================================================================
echo  Fast BLF Converter —— Windows 绿色打包（onedir + zip）
echo  项目根目录：%ROOT%
echo ============================================================================

rem ============================================================================
rem [1/8] 探测 Python：优先 py -3.11，其次 py -3.12，最后 PATH 里的 python；
rem       要求 >= 3.10（PySide6 6.6 与源码类型标注的底线）。
rem ============================================================================
set "PY="
for %%V in (3.11 3.12) do (
    if not defined PY (
        py -%%V -c "import sys" >nul 2>&1 && set "PY=py -%%V"
    )
)
if not defined PY (
    python -c "import sys" >nul 2>&1 && set "PY=python"
)
if not defined PY (
    set "ERRMSG=未找到 Python 解释器。请安装 Python 3.10+（勾选 py launcher 或加入 PATH）后重试。"
    goto :fail
)

for /f "delims=" %%V in ('%PY% -c "import sys;print(sys.version.split()[0])"') do set "PYVER=%%V"
echo [1/8] 使用解释器：%PY%    版本：%PYVER%

%PY% -c "import sys; raise SystemExit(0 if sys.version_info >= (3, 10) else 1)"
if errorlevel 1 (
    set "ERRMSG=Python 版本过低（%PYVER%），需要 >= 3.10。"
    goto :fail
)

rem ============================================================================
rem [2/8] 创建/复用虚拟环境 .venv-win
rem       复用的意义：受限办公电脑可能没有 pip 外网权限，已装好的环境别重建。
rem ============================================================================
if exist "%VPY%" (
    echo [2/8] 复用已存在的虚拟环境：%VENV%
) else (
    echo [2/8] 创建虚拟环境：%VENV%
    %PY% -m venv "%VENV%"
    if errorlevel 1 (
        set "ERRMSG=创建虚拟环境失败，请确认 Python 安装包含 venv 模块。"
        goto :fail
    )
)
"%VPY%" -c "import sys;print('[2/8] venv Python', sys.version.split()[0])"
if errorlevel 1 (
    set "ERRMSG=虚拟环境不可用：%VPY%"
    goto :fail
)

rem ============================================================================
rem [3/8] 安装构建期依赖：maturin / pyinstaller / PySide6 / xlsxwriter
rem       在线失败则回落到 --no-index --find-links wheels 离线目录。
rem       pip 自身会优先命中本地 wheel 缓存（%LOCALAPPDATA%\pip\Cache），
rem       所以断网时只要依赖装过一次，这一步通常直接复用缓存、无需重下。
rem ============================================================================
echo [3/8] 升级 pip（离线时失败可忽略，继续用现有 pip）
"%VPY%" -m pip install --upgrade pip
if errorlevel 1 echo [3/8][警告] pip 升级失败（可能无外网），继续尝试使用本地已装依赖/缓存。

echo [3/8] 安装构建期依赖：maturin pyinstaller PySide6 xlsxwriter
"%VPY%" -m pip install maturin pyinstaller PySide6 xlsxwriter
if errorlevel 1 (
    echo [3/8][提示] 在线安装失败：当前机器可能没有 pip 网络权限。
    echo [3/8][提示] 尝试离线目录：%OFFLINE_DIR%
    if not exist "%OFFLINE_DIR%" (
        set "ERRMSG=在线安装依赖失败，且离线目录不存在：%OFFLINE_DIR%（请预先把 wheel 放进去）。"
        goto :fail
    )
    "%VPY%" -m pip install --no-index --find-links "%OFFLINE_DIR%" maturin pyinstaller PySide6 xlsxwriter
    if errorlevel 1 (
        set "ERRMSG=离线安装构建依赖失败，请检查 %OFFLINE_DIR% 内的 wheel 是否齐全。"
        goto :fail
    )
)

rem ============================================================================
rem [4/8] 编译 Rust 扩展并安装 wheel
rem       extension-module 必须显式带上（maturin 通常会自动启用，显式指定更稳）。
rem       --out 到 dist\wheels，便于留档与在离线机器上复用。
rem ============================================================================
if not exist "%WHEEL_DIR%" mkdir "%WHEEL_DIR%"

rem      从【仓库根目录】调用 maturin（不要 pushd 到 crate 目录）：pyproject.toml 的
rem      [tool.maturin] 指定了 manifest-path / module-name，从根目录跑才生效；
rem      这条调用路径已在本机对同一份 pyproject 实测通过。
cd /d "%ROOT%"
echo [4/8] 编译 Rust 扩展（release -> dist\wheels）
rem      显式指定 pyo3 使用的解释器。仓库里的 .cargo/config.toml 是为 macOS 开发写的
rem      （PYO3_PYTHON=/opt/anaconda3/bin/python3）；cargo 的 [env] 只在变量【未设置】时才生效，
rem      所以这里先设好，Windows 构建就不会被那份 macOS 配置带偏。
set "PYO3_PYTHON=%VPY%"
"%VPY%" -m maturin build --release --features extension-module --out "%WHEEL_DIR%"
set "MATURIN_RC=%errorlevel%"
if not "%MATURIN_RC%"=="0" (
    set "ERRMSG=Rust 扩展编译失败（请确认已安装 Rust 工具链：rustup / cargo）。"
    goto :fail
)

rem cmd 不会展开通配符，这里用 for 自己解析出唯一 wheel；--force-reinstall 覆盖旧版本。
set "WHEEL="
for %%W in ("%WHEEL_DIR%\fast_blf_core-*.whl") do set "WHEEL=%%~fW"
if not exist "!WHEEL!" (
    set "ERRMSG=未找到编译产物 %WHEEL_DIR%\fast_blf_core-*.whl"
    goto :fail
)
echo [4/8] 安装 wheel：!WHEEL!
"%VPY%" -m pip install --force-reinstall --no-deps "!WHEEL!"
if errorlevel 1 (
    set "ERRMSG=安装 fast_blf_core wheel 失败：!WHEEL!"
    goto :fail
)

rem ============================================================================
rem [5/8] PyInstaller 打包（onedir）
rem       --clean 清掉上次缓存，--noconfirm 覆盖输出、禁止交互。
rem       onedir 形态不会在启动时向 %TEMP% 释放 DLL，符合 EDR 合规要求。
rem ============================================================================
echo [5/8] PyInstaller onedir 打包
"%VPY%" -m PyInstaller --noconfirm --clean packaging\FastBLFConverter.spec
if errorlevel 1 (
    set "ERRMSG=PyInstaller 打包失败，请查看上方日志。"
    goto :fail
)

rem ============================================================================
rem [6/8] 复制使用说明与主题到产物目录
rem       theme.qss 在 spec 里已进 _internal\gui\；这里再复制一份到 exe 同级
rem       gui\，方便用户直接改主题（resolve_theme_path 会优先命中这一份）。
rem ============================================================================
echo [6/8] 复制 使用说明.txt 与 gui\theme.qss 到 %DIST_DIR%
rem 用通配符让 cmd 自行枚举根目录 .txt（当前即 使用说明.txt）：即使控制台代码页
rem 与脚本编码不一致，也不会因中文文件名匹配失败。注意 for 无匹配时会回显模式本身，
rem 所以用 DOC_FOUND 标记是否真的复制成功。
set "DOC_FOUND="
for %%F in ("%ROOT%*.txt") do (
    copy /y "%%~fF" "%DIST_DIR%\" >nul
    if errorlevel 1 (
        set "ERRMSG=复制说明文件失败：%%~fF"
        goto :fail
    )
    set "DOC_FOUND=1"
)
if not defined DOC_FOUND (
    set "ERRMSG=未找到 使用说明.txt（项目根目录没有任何 .txt 文件）。"
    goto :fail
)

if not exist "%DIST_DIR%\gui" mkdir "%DIST_DIR%\gui"
copy /y "%ROOT%gui\theme.qss" "%DIST_DIR%\gui\theme.qss" >nul
if errorlevel 1 (
    set "ERRMSG=复制 gui\theme.qss 失败。"
    goto :fail
)

rem ============================================================================
rem [7/8] 打包绿色 zip（PowerShell Compress-Archive，系统自带，无需额外工具）
rem ============================================================================
echo [7/8] 打包 zip：%ZIP_PATH%
powershell -NoProfile -Command "Compress-Archive -Path 'dist\FastBLFConverter\*' -DestinationPath 'dist\FastBLFConverter-portable.zip' -Force"
if errorlevel 1 (
    set "ERRMSG=Compress-Archive 打包 zip 失败。"
    goto :fail
)

rem ============================================================================
rem [8/8] 输出产物路径与 zip 大小
rem ============================================================================
echo.
echo ============================================================================
echo [8/8] 打包完成
echo   程序目录：%DIST_DIR%
echo   绿色 zip：%ZIP_PATH%
for %%Z in ("%ZIP_PATH%") do echo   zip 大小：%%~zZ 字节
echo ============================================================================

endlocal
exit /b 0

rem ------------------------------------------------------------------ 失败出口 --
:fail
echo.
color 0C
echo ============================================================================
echo  [错误] %ERRMSG%
echo ============================================================================
color 07
echo.
if not defined FBLF_NO_PAUSE pause
endlocal
exit /b 1
