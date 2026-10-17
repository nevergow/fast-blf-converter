# 编码代理约定（本项目）— 与 SOUL.md / skill `dsh-coding-delegation` 对齐

> **权威来源**：`~/.hermes/SOUL.md`「代码任务分工」+ skill `dsh-coding-delegation`（派活前必读）。
> 本文只是本项目落地的简表；**冲突以 SOUL.md 与 skill 为准**。

## 分工（Neil 2026-10-16 定调）

| 任务形状 | 执行者 | 依据 |
|---|---|---|
| 外部副作用（发版/发消息/改线上配置） | **我自己** | 不可逆 |
| 只读侦察（改哪、为什么坏、真实 API 名） | **Pi 只读**（`--tools read,grep,find,ls`） | 便宜、可 steer |
| 单点实现（**1–2 文件**、验收命令明确） | **Pi**（`pi -p` 或 `pi_rpc.py` 常驻可 `steer`） | 跑偏 30 秒能纠回 |
| 长链 / 跨模块 / 需设计权衡（**3+ 文件**） | **DSH headless** | 能撑长任务、有沙箱；**无中途 steer**，任务书要更窄 |
| 纯机械批处理（重命名/批量替换） | **脚本（0 token）** | 不叫 agent |

**沙箱方向相反**：DSH = workspace-write（cwd 即边界）；Pi = YOLO（**只能在 git 干净仓库里跑**，验收前必须看全 diff）。

## 七步生命周期（每个可验收切片）

1. 我写任务书（背景/不变量/硬边界/必须贴真实输出/验收命令），先自查无互斥要求
2. **侦察（Pi 只读）**：定位改动点与真实 API 名 → 改动点清单
   —— ⚠️ 本项目的两次返工（`24/80` vs `17/73` 字节口径矛盾、`CONFIG_INVALID` vs `BAD_CONFIG`）都是**跳过这步**造成的
3. 按上表选执行者实现
4. 执行者自证：真实命令输出 + 一句 note
5. **我独立验收**：跑测试/产物/**像用户一样走真实交互路径的探针**/看 diff
6. **我提交**（执行者不提交），CI 兜底
7. 清场：kill 残留进程/探针并 `ps` 复查

## 成本与错峰（会直接反映在余额上）

* 派重活前先跑 `~/.hermes/scripts/ds_price_window.py --short`。
* **北京工作日 9–12、14–18 为峰时（双倍价）**：要挪到 18:00 后或周末，**先问 Neil，不自动排队**。
* 基准：DSH 一个 step ≈ ¥1–3（峰时翻倍）；Pi 单点改动 ≈ ¥0.2–1。
* 结论：**能 Pi 就 Pi**；只有 3+ 文件的跨模块长链才上 DSH。

## 派活登记与观测

* 派 DSH/Pi 一律用包装器登记，图上才是真父子关系：
  `python3 ~/.hermes/scripts/af_dispatch.py --kind dsh|pi --label "Step NN" --task <任务书> -- <真实命令>`
* 事后查看：`~/.hermes/scripts/worker_runs.py [show|follow] <KEY>`；看板 `worker_dashboard.py --style pixel`。

## 本机环境坑（已固化进脚本）

* `maturin develop` 会装进 **anaconda** 而非 `.venv` → 用 `scripts/dev_install.sh`（含 `libpython` 断言）。
* 裸 `cargo build` 不带 `extension-module` → import 阶段 SIGSEGV；`maturin build` 默认 debug，要 `--release`。
* 窗口化 exe 在 Windows 上**不产出 stdout** → 冻结态自检只警告不阻塞，但诊断必须先打印。
* 本机**没有 `timeout` 命令**（macOS）。
* 探针必须走真实入口（历史教训：只写树节点不写 `dbc_by_channel` → 误报 `EMPTY_RESULT`）。
