#!/usr/bin/env python3
"""多文件并行加速测量：把一份大 BLF 复制成 N 份（模拟 CANalyzer 滚动切片），
测量 scan/convert 的墙钟时间，并与单文件基线对比，给出加速比与有效吞吐。

用法：.venv/bin/python tests/bench_parallel.py [N]
"""
from __future__ import annotations

import shutil
import sys
import time
from pathlib import Path

import fast_blf_core

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "fixtures" / "bench_huge.blf"
DBC = ROOT / "fixtures" / "bench.dbc"
SLICE_DIR = Path("/tmp/fblf_slices")
N = int(sys.argv[1]) if len(sys.argv) > 1 else 8


def main() -> int:
    if not SRC.is_file():
        print(f"缺夹具 {SRC}，先跑 tests/make_bench_blf.py")
        return 1
    SLICE_DIR.mkdir(parents=True, exist_ok=True)
    slices = []
    for i in range(N):
        dst = SLICE_DIR / f"slice_{i:02d}.blf"
        if not dst.is_file() or dst.stat().st_size != SRC.stat().st_size:
            shutil.copy2(SRC, dst)
        slices.append(dst)
    total_mb = sum(p.stat().st_size for p in slices) / 1024 / 1024
    print(f"切片: {N} 份 × {SRC.stat().st_size/1024/1024:.0f} MB = {total_mb:.0f} MB")

    t0 = time.perf_counter()
    tree = fast_blf_core.scan([str(p) for p in slices], {1: str(DBC), 2: str(DBC), 3: str(DBC)})
    t_scan = time.perf_counter() - t0
    frames = sum(m["frame_count"] for c in (tree.get("channels") or {}).values()
                 for m in (c.get("messages") or []))
    print(f"[scan  {N} 文件] {t_scan:.2f}s  {total_mb/t_scan:6.1f} MB/s  {frames/t_scan:,.0f} 帧/s")

    sel = {"1": {f"MSG_{m:X}": [f"Sig{i}" for i in range(8)] for m in (0x100, 0x101)}}
    out = Path("/tmp/fblf_bench_par")
    out.mkdir(parents=True, exist_ok=True)
    cfg = {"blf_paths": [str(p) for p in slices],
           "dbc_by_channel": {1: str(DBC), 2: str(DBC), 3: str(DBC)},
           "selected": sel, "split_by_message": True, "resample_hz": None,
           "export_format": "parquet", "out_dir": str(out), "temp_dir": str(out / ".tmp"),
           "excel_row_limit": 1_000_000, "memory_budget_mb": 1536, "ffill": False}
    t0 = time.perf_counter()
    rep = fast_blf_core.convert(cfg)
    t_conv = time.perf_counter() - t0
    rows = rep.get("total_rows", 0)
    print(f"[convert {N} 文件] {t_conv:.2f}s  {total_mb/t_conv:6.1f} MB/s  "
          f"{rows:,} 行  ({len(rep.get('tables') or [])} 张子表, parquet)")
    print(f"\n对比单文件基线（tests/bench_report.py）：scan 187MB/5.7s ≈ 32.7 MB/s")
    print(f"本次有效吞吐 {total_mb/t_scan:.1f} MB/s → 约 {total_mb/t_scan/32.7:.1f}× 单文件基线")
    return 0


if __name__ == "__main__":
    sys.exit(main())
