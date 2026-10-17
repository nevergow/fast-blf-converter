#!/usr/bin/env bash
# 开发期安装 Rust 扩展：用 **maturin**（项目本来就依赖它注入 features/链接参数）
# 构建 wheel → 强制装进本仓库 .venv → 断言"链接方式 + API"都对。
#
# 三个坑（都踩过，断言必须保留）：
# 1) maturin develop（本机 1.15）会把产物装到 **anaconda** 的 site-packages
#    （PYO3_PYTHON 指向 /opt/anaconda3），venv 里那份旧 .so 仍生效
#    → 编译成功但 Python 里看不到新 API。这里用 pip --force-reinstall 显式装进 venv。
# 2) `maturin build` 默认走 **dev(debug) profile**（日志里写 `Finished dev profile`）。
#    要 release 必须显式 `--release`；debug .so≈201MB，release≈121MB，可用体积判别。
# 3) 裸 `cargo build` 不启用 extension-module → 链接 libpython → import 时 SIGSEGV。
#    所以本脚本**不用** cargo build，并断言产物未链接 libpython。
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE_FLAG="${1:-release}"      # release | debug
VENV=".venv"
PY="$VENV/bin/python"
SO="$VENV/lib/python3.12/site-packages/fast_blf_core/fast_blf_core.abi3.so"
OUT="target/wheels"

source "$HOME/.cargo/env" 2>/dev/null || true
export DYLD_FALLBACK_LIBRARY_PATH=/opt/anaconda3/lib
mkdir -p "$OUT"

if [ "$PROFILE_FLAG" = "debug" ]; then
  echo "== maturin build (debug) =="
  "$VENV/bin/maturin" build --out "$OUT"
else
  echo "== maturin build --release =="
  "$VENV/bin/maturin" build --release --out "$OUT"
fi

WHL=$(ls -t "$OUT"/*.whl | head -1)
echo "wheel: $WHL"
"$PY" -m pip install --force-reinstall --no-deps -q "$WHL"

echo "== 断言：装进去的 .so 不得链接 libpython =="
if otool -L "$SO" | grep -qi libpython; then
  echo "错误：$SO 链接了 libpython —— extension-module 未生效，import 会 SIGSEGV"
  otool -L "$SO" | grep -i libpython
  exit 1
fi
ls -l "$SO" | awk '{printf "%s  %.1f MB  %s %s %s\n", $9, $5/1048576, $6, $7, $8}'

echo "== 断言：API 齐全且能真正调用 =="
"$PY" - <<'PY'
import fast_blf_core as c
need = ["convert", "scan", "series_for_plot", "open_plot_session", "version"]
missing = [n for n in need if not hasattr(c, n)]
print("version:", c.version())
if missing:
    raise SystemExit(f"缺少导出：{missing}")
print("OK")
PY
