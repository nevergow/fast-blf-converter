# 编码代理约定（本项目）

## 谁写代码

**默认：Pi CLI**（`pi` v1.3.0，`deepseek-flash` = DeepSeek-V4.1-Flash）。
理由（2026-10-17 决定，Neil 指出）：

1. **成本**：简单任务 ≈ ¥0.003–0.005/次；DSH headless 一个 step ≈ ¥1–3。
2. **可中途纠偏**：Pi 的 RPC 模式支持 `steer` / `follow_up`，agent 跑着也能改方向；
   DSH headless **做不到**——只能杀掉重来。本项目历史上真的因为这点白烧过整轮
   （契约自相矛盾那次、Step 6a 钻 lldb 兔子洞那次）。

**DSH headless 仅在这些情况保留**：需要它特有的长会话/插件行为时；或者 Pi 在同一类任务上
连续两次跑偏（用 `steer` 纠正无效）后作为备选。

## 调用方式

```bash
set -a; source ~/.hermes/.env; set +a          # Hermes 的 shell 不自动加载 key
export PI_SKIP_VERSION_CHECK=1
# 一次性任务（推荐：任务书从 stdin 喂进去）
pi --no-session -p - < docs/tasks/<step>.md    # 或 pi --no-session -p "$(cat docs/tasks/<step>.md)"
# 常驻会话（需要中途 steer 时）
python3 ~/.hermes/scripts/pi_rpc.py start --name blf --workdir <repo> --no-session
python3 ~/.hermes/scripts/pi_rpc.py send  --name blf "<task>"
python3 ~/.hermes/scripts/pi_rpc.py steer --name blf "改用方案 B"
```

注意：本机 **没有 `timeout` 命令**（macOS），要用纯 shell 计时 + `kill -0` 判定。

## 流程（不变）

1. **先改契约**：`docs/INTERFACE.md`（冻结口径），再写任务书 `docs/tasks/stepNN_*.md`
   —— 任务书里必须写死：只能改哪些文件、逐条要求、**自检命令与输出要求**、禁止事项。
2. 派给 Pi，跑完看它自报。
3. **主会话独立验收**（不采信执行者自述）：
   * 自己写探针（`/tmp/probe_*.py`），**走真实入口**，不走内部捷径；
   * 至少一个"独立第二路径对账"（例如 `stats()` 对 `convert` 导出的 CSV）；
   * 硬指标实测（延迟/内存/包络不丢峰），不达标就回退。
4. 全量回归：`cargo test` + `tests/acceptance.py` + 全部 `--selftest*`。
5. 提交 → 推送 → CI 出 Windows 包 → 取件核验（sha256/结构）→ 发 Release。

## 血泪教训（已在 CI/脚本里固化）

* `maturin develop` 会装到 **anaconda** 而非 `.venv` → 用 `scripts/dev_install.sh`（含 `libpython` 断言）。
* 裸 `cargo build` 不带 `extension-module` → import 阶段 SIGSEGV。
* `maturin build` 默认 **debug** profile，要 `--release`。
* 窗口化 exe 在 Windows 上**不产出 stdout** → 冻结态自检只警告不阻塞（诊断信息仍要打印）。
* 探针走内部捷径会误报（例：只写树节点不写 `dbc_by_channel` → 转换报 `EMPTY_RESULT`）。
* 契约与任务书口径必须一致，否则执行者会卡在两者之间空转。
