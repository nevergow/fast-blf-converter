#!/usr/bin/env python3
"""性能与互操作基准：对 187MB / 408 万帧的 BLF 跑 scan 与 convert，输出真实吞吐数字。

用法：.venv/bin/python tests/bench_report.py [blf 路径]
依赖：fixtures/bench_huge.blf（由 tests/make_bench_blf.py 生成）+ 自动生成的 fixtures/bench.dbc
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

import fast_blf_core

ROOT = Path(__file__).resolve().parent.parent
FIX = ROOT / "fixtures"


def ensure_dbc() -> Path:
    """为基准文件生成 DBC：Ch1 20 条报文各 8 个信号，Ch2 4 条，Ch3 1 条 FD。"""
    path = FIX / "bench.dbc"
    lines = ['VERSION ""', "", "NS_ :", "BS_:", "", "BU_: Vector__XXX", ""]
    for mid in range(0x100, 0x114):
        lines.append(f"BO_ {mid} MSG_{mid:X}: 8 Vector__XXX")
        for i in range(8):                      # 8 个 Intel/unsigned 信号，覆盖 64 位
            lines.append(f' SG_ Sig{i} : {i * 8}|8@1+ (0.5,{i}) [0|127] "u{i}" Vector__XXX')
        lines.append("")
    for mid in (0x200, 0x1ABCDEF, 0x300, 0x400):
        lines.append(f"BO_ {mid} MSG_{mid:X}: 8 Vector__XXX")
        lines.append(f' SG_ Sig0 : 0|8@1+ (1,0) [0|255] "" Vector__XXX')
        lines.append("")
    lines.append("BO_ 1520 FD_MSG: 64 Vector__XXX")
    lines.append(' SG_ Pay : 0|8@1+ (1,0) [0|255] "V" Vector__XXX')
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def main() -> int:
    blf = Path(sys.argv[1]) if len(sys.argv) > 1 else FIX / "bench_huge.blf"
    dbc = ensure_dbc()
    sidecar = blf.with_name(blf.name.replace(".blf", ".expected.json"))
    expect = json.loads(sidecar.read_text()) if sidecar.is_file() else {}
    size_mb = blf.stat().st_size / 1024 / 1024
    cpus = os.cpu_count() or 1

    print(f"文件: {blf.name}  {size_mb:.1f} MB   CPU 核数: {cpus}")
    if expect:
        print(f"期望: {expect['total_frames']:,} 帧  通道分布 {expect['per_channel']}")

    # ---------- scan ----------
    progress_events = []
    t0 = time.perf_counter()
    tree = fast_blf_core.scan([str(blf)], {1: str(dbc), 2: str(dbc), 3: str(dbc)},
                              lambda p: progress_events.append(p))
    t_scan = time.perf_counter() - t0
    msgs = {m["name"]: m for c in (tree.get("channels") or {}).values()
            for m in (c.get("messages") or [])}
    got = {m["name"]: m["frame_count"] for m in msgs.values()}
    print(f"\n[scan]  {t_scan:.2f}s   {size_mb / t_scan:.1f} MB/s")
    print(f"        识别通道 {sorted((tree.get('channels') or {}).keys())} / 报文 {len(msgs)} 个"
          f"  进度回调 {len(progress_events)} 次")
    if expect:
        # 逐通道对账：我们报的 1-based 通道号必须等于 python-can 写入时的通道语义
        our = {}
        for ch, info in (tree.get("channels") or {}).items():
            our[ch] = sum(m["frame_count"] for m in (info.get("messages") or []))
        want = {str(k): v for k, v in expect["per_channel"].items()}
        match = our == want
        print(f"        与写入方对账: {'一致 ✅' if match else '不一致 ❌'}")
        print(f"          我们={dict(sorted(our.items()))}")
        print(f"          期望={dict(sorted(want.items()))}")

    # ---------- convert（只取 Ch1 的 4 条报文，避免输出过大）----------
    sel = {"1": {f"MSG_{m:X}": [f"Sig{i}" for i in range(8)]
                 for m in (0x100, 0x101, 0x102, 0x103)}}
    out = Path("/tmp/fblf_bench_out")
    out.mkdir(parents=True, exist_ok=True)
    cfg = {
        "blf_paths": [str(blf)], "dbc_by_channel": {1: str(dbc), 2: str(dbc), 3: str(dbc)},
        "selected": sel, "split_by_message": True, "resample_hz": None,
        "export_format": "csv", "out_dir": str(out),
        "temp_dir": str(out / ".tmp"), "excel_row_limit": 1_000_000,
        "memory_budget_mb": 1536, "ffill": False,
    }
    t0 = time.perf_counter()
    rep = fast_blf_core.convert(cfg)
    t_conv = time.perf_counter() - t0
    rows = rep.get("total_rows", 0)
    csv_bytes = sum(Path(t["path"]).stat().st_size for t in (rep.get("tables") or [])
                    if Path(t["path"]).is_file())
    print(f"\n[convert] {t_conv:.2f}s  读取 {size_mb / t_conv:.1f} MB/s  "
          f"解码+写出 {rows / t_conv:,.0f} 行/s")
    print(f"          子表 {len(rep.get('tables') or [])} 张 / {rows:,} 行 / CSV {csv_bytes/1024/1024:.1f} MB"
          f"  降级={rep.get('downgraded')}")
    print(f"\n（对比参考：文件未压缩 {size_mb:.0f}MB，含 408 万帧；串行 Python 解析同量级文件通常需要数十秒）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
