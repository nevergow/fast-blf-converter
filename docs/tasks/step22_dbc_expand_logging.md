[Target]    /tmp/slice-main  （git worktree，分支 slice/main-ux，基点 b36b0a9）
[Executor]  Pi RPC（常驻会话 mainux，可中途 steer）
[Attempt]   1
[Anchor]    b36b0a9 · 派发前 `git status --porcelain` 必须为空

## 1. Goal

两件事，做完都要能验证为真：

1. **加 DBC 后树自动展开到 DBC 名称层级**：在通道上「📄 添加 DBC…」之后，该通道节点自动展开，DBC 节点
   （文件名那一层）立即可见并滚动到视野内；「🔍 解析信号」重建树之后，凡是绑定了 DBC 的通道同样自动展开到
   DBC 节点那一层。**不要**再往下展开到报文/信号（那是下一层，报文可能很多）。
2. **日志落盘**：程序启动即写日志文件（滚动），窗口化 exe 下也能事后排查；Python 未捕获异常、
   QThread 里抛出的异常、native 崩溃栈全部落到同一个文件；原来的 `print(..., file=sys.stderr)` 全部改成 logger；
   convert 的 config 里带上 `log_path`（Rust 侧会写 phase 2 预检与 panic，另一切片在实现，字段已对好）；
   菜单加「📂 打开日志目录」。

## 2. Context（已侦察，别全库乱翻）

- 加 DBC 入口：`gui/main.py:1073 add_dbc_dialog(channel)` → `:1082 _upsert_dbc_node(channel, path, DBC_STATUS_PENDING)`
  → `:1095` 造节点 `📄 <文件名>   <状态>`；**不自动扫描**。树是 `self.signal_model`（QStandardItemModel），
  视图是 `self.signal_view`（`NODE_ROLE`/`DBC_PATH_ROLE`/`CH_ROLE` 是自定义 role）
- 扫描后重建树：`_setup_scan_tree(...)`（`gui/main.py:3691` 附近的测试辅助函数同源逻辑），通道节点由
  `_channel_item(channel)` 取
- 树里 DBC 节点是通道的**子节点**（`channel_item.appendRow(node)`），所以「展开到 DBC 名称层级」= 展开通道节点本身
- 现有 stderr 输出点（全部要改成 logger）：`main.py:2537`（`[fast_blf_core] 警告：{line}`）、`:2552`（转换失败）、
  `:2791`（画图/报告失败）、`:3147`（主题加载失败）、`:4555`（版本号）、以及 `gui/plot_view.py:2150`
  —— **plot_view.py 不在你的白名单里**，那条先留着别动
- Worker 线程：`Worker.run`（`main.py:376-402`）里 `except Exception`（`:393`）**抓不到**
  `pyo3_runtime.PanicException`（它是 `BaseException` 子类）→ 改成 `except BaseException`，先记日志再按原路径上报
- 自检入口（**这些函数体一行都不许改**，它们是裁判）：
  `--selftest` / `--selftest-deep` / `--selftest-parse` / `--selftest-plot` / `--selftest-fill` / `--selftest-progress`
- 打包是 PyInstaller `console=False`（`packaging/FastBLFConverter.spec:119`）→ 窗口化时 `sys.stderr is None`，
  `print` 静默丢弃。**这是本次要解决的根源**
- 版本/依赖：只有 PySide6，不许新增依赖；`pathlib`/`logging.handlers` 标准库够用

## 3. Scope & Boundaries

[Scope_Allow]
- gui/main.py
- gui/log_setup.py   ← **新建文件**

[Scope_Forbid] 其他一切：`gui/plot_view.py`、`crates/`、`tests/`、`docs/`、`packaging/`、依赖清单

[Invariants]
1. 现有六个 `--selftest*` 全部仍然通过，**函数体不许改**（可以新增别的函数）
2. `Worker.run` 的对外行为不变（成功/失败信号、report/error 字段）
3. convert 的 config 新增字段名必须正好是 `log_path`（字符串绝对路径或 `None`）
4. 日志文件路径：
   - Windows: `%LOCALAPPDATA%\fast_blf_converter\logs\gui.log`
   - macOS: `~/Library/Logs/fast_blf_converter/gui.log`
   - 其他: `$XDG_STATE_HOME/fast_blf_converter/logs/gui.log`（缺省 `~/.local/state/...`）
   可用环境变量 `FAST_BLF_LOG_DIR` 覆盖目录（**验收要用它**）
5. 滚动：单文件 10 MB、保留 5 份
6. 日志里不许写 BLF 文件内容；只写路径、配置摘要、计数、错误文本
7. 不改变任何界面布局的既有控件顺序（只允许新增一个菜单项 + 在重建后调用展开）

## 4. 实现要求

### 4.1 DBC 自动展开
- 新增 `_reveal_dbc_node(channel: int) -> None`：取 `_channel_item(channel)`，`signal_view.expand(channel_index)`
  并 `scrollTo(channel_index, QAbstractItemView.PositionAtCenter)`；对 DBC 子节点无需展开（它没有子节点）
- `add_dbc_dialog` 末尾调用它（`_upsert_dbc_node` 之后）
- 扫描/重建树完成后：遍历所有通道，凡是 `_collect_dbc_by_channel()` 里有的通道都调用 `_reveal_dbc_node`
  （注意：此时可能还没有 `entries`，要防空列表）
- 批量添加 DBC（`_write_bulk_dbc`/`apply_dbc_map` 之类，`main.py:1209` 附近）之后同样展开到 DBC 层级

### 4.2 日志
新建 `gui/log_setup.py`：
```python
def log_dir() -> Path            # 遵循上面的路径规则 + FAST_BLF_LOG_DIR 覆盖
def setup_logging() -> Path      # 配置 root logger（RotatingFileHandler, 10MB×5, 带 ms 时间戳与级别）
                                 # faulthandler.enable(file=<同目录 crash.log 追加>)
                                 # sys.excepthook / threading.excepthook 落到 logger.exception
def log_path() -> Path           # 当前的 gui.log 路径
```
- `main.py` 的 `_main_impl` 最开头调用 `setup_logging()`（在创建 QApplication 之前，越早越好）
- 4 处 `print(..., file=sys.stderr)` 改成 `logging.getLogger("fast_blf")` 的 `warning/error`（保留原文案与变量）
- `Worker.run` 的 `except Exception` → `except BaseException`，`logger.exception(...)` 后按原路径 `self.failed.emit(...)`
- convert 的 config 加 `"log_path": str(log_path())`；preview/HTML 报告的 config 也加（若结构相同）
- 每次 convert 开始记一行 INFO：`convert start files=<n> split=<bool> resample_hz=<..> format=<..> time_format=<..> fill=<..> budget_mb=<..>`
  结束记一行 INFO：`convert done ok=<bool> rows=<..> tables=<..> warnings=<n> elapsed_s=<..>`
- 菜单：在「⚙ 设置」附近加「📂 打开日志目录」，用 `QDesktopServices.openUrl(QUrl.fromLocalFile(...))`
- 失败时（`_on_convert_failed`）把错误码与消息写 ERROR

## 5. Verification Contract（按顺序，贴真实输出）

```bash
cd /tmp/slice-main
MAIN=/Users/wuzeqin/Projects/fast_blf_converter/.venv/bin/python     # worktree 里没有 .venv
$MAIN -m py_compile gui/main.py gui/log_setup.py                     # exit 0
QT_QPA_PLATFORM=offscreen $MAIN gui/main.py --selftest               # exit 0
QT_QPA_PLATFORM=offscreen $MAIN gui/main.py --selftest-deep          # exit 0
QT_QPA_PLATFORM=offscreen $MAIN gui/main.py --selftest-parse         # exit 0
QT_QPA_PLATFORM=offscreen $MAIN gui/main.py --selftest-plot          # exit 0
```
日志自证（自己写个临时脚本放 `/tmp`，**不要放进仓库**）：
```bash
FAST_BLF_LOG_DIR=/tmp/fblf_logs QT_QPA_PLATFORM=offscreen $MAIN gui/main.py --selftest-parse
ls -l /tmp/fblf_logs/                       # 必须出现 gui.log
grep -c "" /tmp/fblf_logs/gui.log           # > 0
```
再验证异常落盘：临时脚本里 `import gui.log_setup as L; L.setup_logging(); raise RuntimeError("probe")`
→ `/tmp/fblf_logs/gui.log` 里必须出现 `RuntimeError: probe` 与 traceback。

展开自证（临时脚本，`QT_QPA_PLATFORM=offscreen`）：
构造 `MainWindow(quiet_dialogs=True)` → `w._upsert_dbc_node(1, "/tmp/x.dbc", DBC_STATUS_PENDING)` →
`w._reveal_dbc_node(1)` → 断言 `w.signal_view.isExpanded(w.signal_model.index(0, 0))` 为 True，打印 `EXPAND_OK`。

反作弊自证：`git diff --name-only` 恰好 = `gui/main.py` + `gui/log_setup.py`（新文件不会出现在 diff，用 `git status --porcelain` 证明）

## 6. Stop Conditions
- 需要改白名单外文件（尤其 `gui/plot_view.py` 与 `run_selftest_*` 函数体）→ 停，输出 `[ABORT_CONTRACT_CONFLICT]`
- 需要新增依赖 → 停
- 同一错误签名 2 次 → 停
- 超过 45 分钟 → 停并交进度

## 7. Executor Rules
1. 禁 `git commit/push/reset/clean`；禁改白名单外文件；禁改/删/跳过自检；禁 `# type: ignore`、空 catch、写死返回值凑测试
2. 冲突即停：`[ABORT_CONTRACT_CONFLICT] <file:line> | <原因>`
3. 回执格式（缺项视为失败）：
```text
[STATUS]: DONE | ABORT_CONTRACT_CONFLICT
[TOUCHED_FILES]: <与 git status --porcelain 一致>
[VERIFY]:
  1) <cmd> -> exit <code>
     <关键原始输出 5-20 行>
  2) ...
[ARTIFACTS]: <日志/截图绝对路径>
[NOTE]: <做了什么、跳过了什么、为什么>
```
