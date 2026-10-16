# Step 6c 任务书（配置预设：保存 / 加载 / 删除）—— 交给 DSH headless 执行

你是 PySide6 GUI 工程师。工作目录 = `/Users/wuzeqin/Projects/fast_blf_converter`（可写）。

先读：`docs/SPEC.md`、`docs/INTERFACE.md` §1、`gui/main.py`（6a/6b 之后的版本）。

## 背景（用户需求原话）

> "增加保存配置的功能，不同项目所使用的 dbc 不同，需要的信号也不同，可以将某个 dbc 的搭配和选中的信号保存为一个配置，并能够自定义命名，方便下次直接调用。"

用户已拍板：**存到绿色包目录下的 `configs/*.json`**（目录不可写时回退用户目录）；**配置里存 DBC 映射 + 选中信号 + 导出策略**。

## 本次只做 Step 6c，只允许改/新增这两个文件

1. `gui/main.py`（接线 + 新增命令行开关）
2. **新增** `gui/presets.py`（配置读写与目录解析，约 120–200 行）

不要动 `gui/check_delegate.py`、`gui/theme.qss`、`crates/`、`packaging/`、`.github/`。

## 1. 配置目录解析（`gui/presets.py`）

优先级：
1. 环境变量 `FBLF_CONFIG_DIR`（若设置则用它，**不存在则创建**）
2. 绿色包目录 `configs/`：冻结态 = `Path(sys.executable).parent / "configs"`；开发态 = 项目根 / `configs`
3. 回退用户目录：Windows `%APPDATA%\FastBLFConverter\configs`、macOS `~/Library/Application Support/FastBLFConverter/configs`、其它 `~/.config/FastBLFConverter/configs`

规则：**先尝试创建/写测试，失败则静默回退下一级**；把最终选定的目录暴露为 `config_dir()`（供状态栏 tooltip 显示）。

## 2. 配置格式（JSON，UTF-8，`schema: 1`）

```json
{
  "schema": 1,
  "name": "BMS项目A",
  "saved_at": "2026-10-16T15:30:00+08:00",
  "dbc_by_channel": {"1": "/abs/path/demo.dbc", "2": "/abs/path/other.dbc"},
  "selected": {"1": {"VehSpd": ["Spd", "SpdValid"]}},
  "export": {"split_by_message": true, "resample_hz": null, "export_format": "csv"}
}
```
- 文件名：`<name>.json`；名字做安全化（去掉 `/ \ : * ? " < > |`，长度 ≤ 60，空名拒绝）
- 读取时**必须**校验 `schema`；不认识 → 报人话错误，不崩
- 目录里同时会有人工编辑的文件：解析失败只跳过该文件并计数，不中断

## 3. UI（工具栏三个入口）

- `💾 保存配置`：`QInputDialog.getText` 命名（预填当前配置名）；同名已存在 → `QMessageBox.question` 询问「覆盖 / 取消」
- `📂 加载配置`：`QInputDialog.getItem` 列出可用配置（**按修改时间倒序**，显示 `名称（修改时间）`），加载后依次：
  1. 设置 `dbc_by_channel`；**某个 DBC 路径不存在** → 该通道记入"缺失"集合（**不弹窗**，通道旁红字提示 + tooltip 写明路径）
  2. 自动触发一次「🔍 解析信号」
  3. 解析完成后按配置恢复勾选（按 `(通道, 报文名, 信号名)` 匹配；匹配不到的忽略并在状态栏提示数量）
  4. 应用导出策略：拆表开关 / 重采样下拉 / 导出格式下拉
  5. 状态栏反馈：`配置「BMS项目A」已加载：DBC 2 个（缺失 0 个）、恢复信号 N 个`
- `🗑 删除配置`：`getItem` + 二次确认 → 删除文件
- 状态栏 tooltip 显示实际配置目录（便于用户知道文件在哪）

## 4. 必须交付 `--selftest-config`（无人值守验证 round-trip）

`python gui/main.py --selftest-config` 要程序化跑：

1. 用**临时目录**当配置目录（设 `FBLF_CONFIG_DIR=<tempdir>`）
2. 构造配置：`dbc_by_channel = {1: fixtures/demo.dbc, 2: fixtures/demo.dbc}`、`selected` 里放 1 个报文 + 2 个信号、`export = {split_by_message: False, resample_hz: 10.0, export_format: "parquet"}`
3. 调保存 → 断言文件存在、JSON 可解析、`schema==1`、三块内容一致
4. 清空 UI 状态（DBC 清空、勾选清空、导出策略复位）→ 调加载 → 断言 DBC 映射、勾选、导出策略**都被正确恢复**
5. 断言**缺失 DBC** 场景：把配置里的一个路径改成不存在的文件，加载后该通道进"缺失"集合、且**没有抛异常**
6. 打印一行摘要（便于我 grep）：`CONFIG_OK saved=<文件名> channels=2 signals=2 export=parquet/10.0 missing=1`
7. 成功退出码 0；任何断言失败 → 打印 `CONFIG_FAIL <原因>` + 退出码 2；内核缺失 → `CONFIG_SKIP no_kernel` + 退出码 0

## 5. 自检（必须跑，贴真实输出）

```
cd /Users/wuzeqin/Projects/fast_blf_converter
.venv/bin/python -m py_compile gui/main.py gui/presets.py && echo "py_compile OK"
QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py --selftest; echo "selftest exit=$?"
QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py --selftest-deep; echo "deep exit=$?"
QT_QPA_PLATFORM=offscreen .venv/bin/python gui/main.py --selftest-config; echo "config exit=$?"
```
四条都要符合预期（`--selftest-config` 必须打印 `CONFIG_OK` 那行）。失败就修到通过。

## 6. 回复格式（只回复这些）

- 两个文件的行数、四条命令的真实输出、偏离说明（没有就写"无偏离"）。
