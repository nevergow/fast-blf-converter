#!/usr/bin/env python3
"""用 python-can（独立实现）生成大 BLF 测试文件 + 期望计数 sidecar。

目的：
  1. 互操作验证 —— 我们的 Rust 解析器要能读"别的实现写出来的 BLF"（不是自己写自己读）；
  2. 性能基准 —— 给出 MB/s、帧/s 的真实数字；
  3. 覆盖扩展帧（is_extended_id）与 CAN FD（is_fd / 64 字节负载）。

用法：/tmp/vcheck/bin/python tests/make_bench_blf.py
产物：fixtures/bench_large.blf(.blf) 与 fixtures/bench_large.expected.json
"""
from __future__ import annotations

import json
import os
import time
from pathlib import Path

import can

ROOT = Path(__file__).resolve().parent.parent
FIX = ROOT / "fixtures"
FIX.mkdir(exist_ok=True)

# 可用环境变量覆盖（用于生成"100MB 级"大文件做性能测量）
#   FBLF_BENCH_OUT=bench_huge.blf FBLF_BENCH_DURATION=6000 FBLF_BENCH_COMPRESSION=0
DURATION_S = float(os.environ.get("FBLF_BENCH_DURATION", "600"))
COMPRESSION = int(os.environ.get("FBLF_BENCH_COMPRESSION", "2"))
OUT_NAME = os.environ.get("FBLF_BENCH_OUT", "bench_large.blf")
EXPECT_NAME = OUT_NAME.replace(".blf", ".expected.json")
CH1_IDS = list(range(0x100, 0x114))          # 20 条标准帧
CH2_IDS = [0x200, 0x1ABCDEF, 0x300, 0x400]   # 含 1 条扩展帧
CH3_ID = 0x5F0                               # CAN FD
PERIODS = {**{i: 0.010 for i in CH1_IDS}, **{i: 0.100 for i in CH2_IDS}, CH3_ID: 1.000}
EXT_IDS = {0x1ABCDEF, 0x400}

out = FIX / OUT_NAME
expected = {
    "file": out.name,
    "duration_s": DURATION_S,
    "total_frames": 0,
    "per_channel": {},
    "per_channel_id": {},
    "extended_ids": sorted(EXT_IDS),
    "fd_ids": [CH3_ID],
}

t0 = time.time()
with can.BLFWriter(str(out), channel=1, compression_level=COMPRESSION) as writer:
    for i in range(int(DURATION_S / 0.010)):
        now = i * 0.010
        for cid in CH1_IDS:
            data = bytes(((i + cid) % 256, (i >> 8) % 256, cid % 256, 0, 0, 0, 0, 0))
            msg = can.Message(timestamp=now, arbitration_id=cid, data=data, channel=0,
                              is_extended_id=False)
            writer.on_message_received(msg)
            expected["total_frames"] += 1
            expected["per_channel"]["1"] = expected["per_channel"].get("1", 0) + 1
            k = f"1:{cid}"
            expected["per_channel_id"][k] = expected["per_channel_id"].get(k, 0) + 1
        if i % 10 == 0:
            for cid in CH2_IDS:
                data = bytes(((i >> 3) % 256, 0, 0, 1, 2, 3, 4, 5))
                msg = can.Message(timestamp=now, arbitration_id=cid, data=data, channel=1,
                                  is_extended_id=cid in EXT_IDS)
                writer.on_message_received(msg)
                expected["total_frames"] += 1
                expected["per_channel"]["2"] = expected["per_channel"].get("2", 0) + 1
                k = f"2:{cid}"
                expected["per_channel_id"][k] = expected["per_channel_id"].get(k, 0) + 1
        if i % 100 == 0:
            payload = bytes(range(64))          # CAN FD 64 字节
            msg = can.Message(timestamp=now, arbitration_id=CH3_ID, data=payload, channel=2,
                              is_fd=True, bitrate_switch=True, is_extended_id=False)
            writer.on_message_received(msg)
            expected["total_frames"] += 1
            expected["per_channel"]["3"] = expected["per_channel"].get("3", 0) + 1
            k = f"3:{CH3_ID}"
            expected["per_channel_id"][k] = expected["per_channel_id"].get(k, 0) + 1

elapsed = time.time() - t0
size = out.stat().st_size
expected["size_bytes"] = size
(FIX / EXPECT_NAME).write_text(json.dumps(expected, indent=1, ensure_ascii=False))

print(f"写出 {out.name}: {size/1024/1024:.1f} MB, {expected['total_frames']:,} 帧, 用时 {elapsed:.1f}s")
print(f"通道分布: {expected['per_channel']}")
print(f"扩展帧 ID: {[hex(i) for i in expected['extended_ids']]}  FD 报文: {[hex(i) for i in expected['fd_ids']]}")
print(f"sidecar: {EXPECT_NAME}")
