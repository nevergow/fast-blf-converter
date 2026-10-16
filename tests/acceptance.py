#!/usr/bin/env python3
"""Fast BLF Converter —— 端到端验收脚本（纯标准库，由 Hermes 主会话执行）。

验收项（对应 docs/INTERFACE.md §6）：
  A3  扩展可导入：fast_blf_core.version()
  A4  scan 出三级树；convert 三种格式各跑一次，行数与注入帧数一致
  A5  百万行防呆：excel_row_limit 调小 -> downgraded=true 且 format_used=csv
  A6  容错：未选通道的坏 DBC 不拖垮转换；所选全落在坏通道 -> EMPTY_RESULT
  A6' 空 BLF 扫描：只按 DBC 列出报文（GUI「🔍 解析信号」的后端）
  A7  时间列口径 4 选 1：与 docs/DEPS.md §8 的独立定值精确比对
  A9  宽表同名信号列冲突：三条路径都不得 panic，列名按报文前缀消歧
  A8  GUI 自检由外部命令负责（本脚本不覆盖）

用法：.venv/bin/python tests/acceptance.py
退出码：0 = 全部通过；1 = 有失败项
"""
from __future__ import annotations

import csv
import os
import shutil
import sys
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FIXTURES = ROOT / "fixtures"
EXPECT_ROWS = {"Ch1_VehSpd": 500, "Ch1_BattTemp": 50, "Ch2_Insulation": 5}

RESULTS: list[tuple[str, bool, str]] = []


def check(name: str, ok: bool, detail: str = "") -> None:
    RESULTS.append((name, bool(ok), detail))
    print(f"[{'PASS' if ok else 'FAIL'}] {name}" + (f"  —— {detail}" if detail else ""))


def count_csv_rows(path: Path) -> tuple[int, bool]:
    """返回 (数据行数, 是否无空单元格)。"""
    with path.open(newline="", encoding="utf-8") as fh:
        rows = list(csv.reader(fh))
    if not rows:
        return 0, True
    header, body = rows[0], rows[1:]
    has_empty = any(cell.strip() == "" for row in body for cell in row)
    return len(body), not has_empty


def xlsx_sheets(path: Path) -> tuple[list[str], list[int]]:
    """从 xlsx（zip）里读出 Sheet 名与各自的行数（数据行）。"""
    with zipfile.ZipFile(path) as zf:
        wb = zf.read("xl/workbook.xml").decode("utf-8", "replace")
        names: list[str] = []
        import re
        for m in re.finditer(r"<sheet[^>]*name=\"([^\"]+)\"", wb):
            names.append(m.group(1))
        counts: list[int] = []
        for i in sorted(zf.namelist()):
            if i.startswith("xl/worksheets/sheet") and i.endswith(".xml"):
                xml = zf.read(i).decode("utf-8", "replace")
                counts.append(xml.count("<row "))
    return names, counts


def main() -> int:
    print("=== Fast BLF Converter 验收 ===")
    try:
        import fast_blf_core  # noqa: PLC0415
    except Exception as exc:  # noqa: BLE001
        check("A3 扩展可导入", False, f"import fast_blf_core 失败：{exc}")
        return 1
    version = fast_blf_core.version()
    check("A3 扩展可导入", bool(version), f"version={version!r}")

    blf = FIXTURES / "multi_channel.blf"
    dbc = FIXTURES / "demo.dbc"
    check("夹具存在", blf.is_file() and dbc.is_file(),
          f"{blf.name}={blf.stat().st_size if blf.is_file() else 'MISSING'}B  {dbc.name}")

    out_root = Path(tempfile.mkdtemp(prefix="fblf_accept_"))
    print(f"（输出目录：{out_root}）")

    # ---------------- scan ----------------
    # 注意：多通道 DBC 隔离是设计特性 —— Ch2 的报文必须给 Ch2 也绑 DBC 才能解出信号名。
    # 真实用法就是一个 DBC 文件同时绑到多个通道。
    dbc_map = {1: str(dbc), 2: str(dbc)}
    tree = fast_blf_core.scan([str(blf)], dbc_map)
    chans = tree.get("channels") or {}
    msgs = {m["name"]: m for c in chans.values() for m in (c.get("messages") or [])}
    check("A4 scan：文件列表", len(tree.get("files") or []) == 1)
    check("A4 scan：两个通道", set(chans) == {"1", "2"}, f"channels={sorted(chans)}")
    check("A4 scan：三个报文（含未绑 DBC 的 Ch2）", set(msgs) >= {"VehSpd", "BattTemp", "Insulation"},
          f"messages={sorted(msgs)}")
    spd = msgs.get("VehSpd") or {}
    check("A4 scan：帧数/周期", spd.get("frame_count") == 500 and abs((spd.get("cycle_ms") or 0) - 10.0) < 1e-6,
          f"VehSpd frame_count={spd.get('frame_count')} cycle_ms={spd.get('cycle_ms')}")
    sig_names = {s["name"] for s in (spd.get("signals") or [])}
    check("A4 scan：信号列表", {"Spd", "SpdValid"} <= sig_names, f"signals={sorted(sig_names)}")
    unbound = chans.get("2") or {}
    check("A4 scan：Ch2 绑了 DBC（本夹具的用法）", bool(unbound.get("dbc")),
          f"Ch2 dbc={unbound.get('dbc')!r}")
    # 真正的回落检查：只给 Ch1 绑 DBC，Ch2 的报文名必须回落成 ID_0x…
    tree_unbound = fast_blf_core.scan([str(blf)], {1: str(dbc)})
    ch2u = (tree_unbound.get("channels") or {}).get("2") or {}
    ch2_names = [m.get("name") for m in (ch2u.get("messages") or [])]
    check("A4 scan：未绑 DBC 的通道仍出现且报文名回落成 ID_0x…",
          bool(ch2u) and not ch2u.get("dbc") and ch2_names
          and all(str(n).startswith("ID_0x") for n in ch2_names),
          f"Ch2 dbc={ch2u.get('dbc')!r} names={ch2_names}")

    selected = {"1": {"VehSpd": ["Spd", "SpdValid"], "BattTemp": ["Temp1", "Temp2"]},
                "2": {"Insulation": ["Res"]}}

    def base_config(out_dir: Path, fmt: str, split: bool = True, excel_limit: int = 1_000_000,
                    resample: float | None = None) -> dict:
        return {
            "blf_paths": [str(blf)],
            "dbc_by_channel": {1: str(dbc), 2: str(dbc)},
            "selected": selected,
            "split_by_message": split,
            "resample_hz": resample,
            "export_format": fmt,
            "out_dir": str(out_dir),
            "temp_dir": str(out_dir / ".tmp_shards"),
            "excel_row_limit": excel_limit,
            "memory_budget_mb": 256,
            "ffill": False,
        }

    def convert_safe(cfg: dict, label: str) -> dict:
        """把 convert 调用包起来：未实现的路径要报告成 FAIL 而不是让脚本崩掉。"""
        try:
            return fast_blf_core.convert(cfg)
        except Exception as exc:  # noqa: BLE001
            check(f"{label}：调用未抛异常", False, f"{type(exc).__name__}: {exc}")
            return {}

    # ---------------- CSV 拆表 ----------------
    csv_dir = out_root / "csv_split"
    rep = convert_safe(base_config(csv_dir, "csv"), "A4 CSV 拆表")
    tables = {t["name"]: t for t in rep.get("tables") or []}
    check("A4 CSV：三张子表", set(tables) == set(EXPECT_ROWS), f"tables={sorted(tables)}")
    for name, want in EXPECT_ROWS.items():
        t = tables.get(name) or {}
        path = Path(t.get("path") or "")
        rows, no_empty = count_csv_rows(path) if path.is_file() else (-1, False)
        check(f"A4 CSV 行数 {name}={want}", rows == want, f"实际={rows}")
        check(f"A4 CSV 无空值 {name}", no_empty)
    check("A4 CSV：总行数", rep.get("total_rows") == sum(EXPECT_ROWS.values()),
          f"total_rows={rep.get('total_rows')}")

    # ---------------- Parquet 拆表 ----------------
    pq_dir = out_root / "parquet_split"
    rep_pq = convert_safe(base_config(pq_dir, "parquet"), "A4 Parquet 拆表")
    pq_tables = {t["name"]: t for t in rep_pq.get("tables") or []}
    ok_pq = all(
        Path(t["path"]).is_file() and Path(t["path"]).read_bytes()[:4] == b"PAR1"
        and t["rows"] == EXPECT_ROWS[name]
        for name, t in pq_tables.items() if name in EXPECT_ROWS
    )
    check("A4 Parquet：三张子表 + PAR1 魔数 + 行数", ok_pq and len(pq_tables) == 3,
          f"tables={sorted(pq_tables)}")

    # ---------------- Excel 拆表（多 Sheet） ----------------
    xl_dir = out_root / "xlsx_split"
    rep_x = convert_safe(base_config(xl_dir, "xlsx"), "A4 Excel 拆表")
    xlsx_paths = {t.get("path") for t in (rep_x.get("tables") or []) if t.get("path")}
    check("A4 Excel：单文件多 Sheet", rep_x.get("downgraded") is False and len(xlsx_paths) == 1,
          f"downgraded={rep_x.get('downgraded')} paths={xlsx_paths}")
    if len(xlsx_paths) == 1:
        xp = Path(next(iter(xlsx_paths)))
        names, counts = xlsx_sheets(xp)
        check("A4 Excel：三个 Sheet 且名字 ≤31 字符",
              len(names) == 3 and all(len(n) <= 31 for n in names), f"sheets={names}")
        check("A4 Excel：Sheet 行数（含表头行 +1）",
              sorted(counts) == sorted(v + 1 for v in EXPECT_ROWS.values()), f"counts={counts}")

    # ---------------- 宽表 + 重采样 ----------------
    wide_dir = out_root / "wide_raw"
    rep_w = convert_safe(base_config(wide_dir, "csv", split=False), "A4 宽表保持原始时序")
    wide_tables = rep_w.get("tables") or []
    check("A4 宽表：单表输出", len(wide_tables) == 1,
          f"tables={[t['name'] for t in wide_tables]} rows={wide_tables[0]['rows'] if wide_tables else 0}")
    rs_dir = out_root / "wide_10hz"
    rep_rs = convert_safe(base_config(rs_dir, "csv", split=False, resample=10.0), "A4 宽表 10Hz 重采样")
    rs_rows = (rep_rs.get("tables") or [{}])[0].get("rows")
    # 网格 = 从 t0 起步长 0.1s 且不超过 t1；末样本 t≈4.99 → floor(49.9)+1 = 50（允许浮点 ±1）
    check("A4 宽表 10Hz 重采样：50 行（±1）", rs_rows in (49, 50, 51), f"rows={rs_rows}")
    if rs_rows:
        p = Path((rep_rs.get("tables") or [{}])[0]["path"])
        rows, no_empty = count_csv_rows(p)
        check("A4 宽表重采样：无空值", no_empty and rows == rs_rows, f"rows={rows} 无空值={no_empty}")

    # ---------------- 防呆降级 ----------------
    guard_dir = out_root / "guard"
    rep_g = convert_safe(base_config(guard_dir, "xlsx", excel_limit=10), "A5 防呆降级")
    check("A5 防呆：downgraded=true + format_used=csv",
          rep_g.get("downgraded") is True and rep_g.get("format_used") == "csv",
          f"downgraded={rep_g.get('downgraded')} format_used={rep_g.get('format_used')}")
    check("A5 防呆：理由是人话且提到百万行",
          "百万行" in str(rep_g.get("downgrade_reason") or ""),
          f"reason={rep_g.get('downgrade_reason')!r}")
    guard_files = [Path(t["path"]) for t in (rep_g.get("tables") or []) if t.get("path")]
    check("A5 防呆：磁盘上确实产出 CSV（无 xlsx）",
          bool(guard_files) and all(p.suffix == ".csv" and p.is_file() for p in guard_files),
          f"{[p.name for p in guard_files]}")

    # ---------------- A6 容错：未选通道的坏 DBC 不得拖垮转换（用户真机反馈）----------------
    broken_dbc = out_root / "broken_ch1.dbc"
    broken_dbc.write_text("这不是 DBC 文本\nBO_ ??? 乱码 ###\n", encoding="utf-8")

    cfg_l = base_config(out_root / "lenient", "csv")
    cfg_l["dbc_by_channel"] = {1: str(broken_dbc), 2: str(dbc)}
    cfg_l["selected"] = {"2": {"Insulation": ["Res"]}}
    rep_l = convert_safe(cfg_l, "A6 坏 DBC（Ch1）")
    check("A6 坏 DBC 在未选通道：转换照样成功", rep_l.get("ok") is True, f"ok={rep_l.get('ok')}")
    dbe = rep_l.get("dbc_errors") or {}
    check("A6 dbc_errors 点名通道 1、不含通道 2",
          "1" in dbe and "2" not in dbe, f"dbc_errors={sorted(dbe)}")
    check("A6 warnings 是人话且提到通道 1 与 DBC",
          any("通道 1" in w and "DBC" in w for w in (rep_l.get("warnings") or [])),
          f"warnings={rep_l.get('warnings')}")
    l_tables = {t["name"]: t for t in (rep_l.get("tables") or [])}
    check("A6 只产出 Ch2 子表（5 行）",
          set(l_tables) == {"Ch2_Insulation"} and l_tables["Ch2_Insulation"]["rows"] == 5,
          f"tables={sorted(l_tables)}")

    cfg_e = base_config(out_root / "lenient_empty", "csv")
    cfg_e["dbc_by_channel"] = {1: str(broken_dbc)}
    cfg_e["selected"] = {"1": {"VehSpd": ["Spd"]}}
    try:
        fast_blf_core.convert(cfg_e)
        check("A6 所选全落在坏通道：报 EMPTY_RESULT 而非静默空表", False, "没抛异常")
    except Exception as exc:  # noqa: BLE001
        text = f"{exc}"
        check("A6 所选全落在坏通道：报 EMPTY_RESULT 而非静默空表",
              "EMPTY_RESULT" in text and "1" in text and "DBC" in text, text[:110])

    # ---------------- A6' 空 BLF 扫描（GUI「🔍 解析信号」按钮的后端）----------------
    tree_empty = fast_blf_core.scan([], {2: str(dbc)})
    ch2e = (tree_empty.get("channels") or {}).get("2") or {}
    e_msgs = {m["name"]: m for m in (ch2e.get("messages") or [])}
    check("A6' 空 BLF 扫描：files 为空但仍按 DBC 列出报文",
          (tree_empty.get("files") or []) == [] and "Insulation" in e_msgs,
          f"files={tree_empty.get('files')} messages={sorted(e_msgs)}")
    ins = e_msgs.get("Insulation") or {}
    check("A6' 空 BLF 扫描：frame_count=0 且 cycle_ms=None",
          ins.get("frame_count") == 0 and ins.get("cycle_ms") is None,
          f"frame_count={ins.get('frame_count')} cycle_ms={ins.get('cycle_ms')}")

    # ---------------- A7 时间列口径 4 选 1（夹具 timefmt.blf；定值见 docs/DEPS.md §8）----------------
    tf_blf, tf_dbc = FIXTURES / "timefmt.blf", FIXTURES / "timefmt.dbc"
    check("A7 时间口径夹具存在", tf_blf.is_file() and tf_dbc.is_file(),
          f"{tf_blf.name}={tf_blf.stat().st_size if tf_blf.is_file() else 'MISSING'}B")
    if tf_blf.is_file() and tf_dbc.is_file():
        tf_tree = fast_blf_core.scan([str(tf_blf)], {1: str(tf_dbc)})
        tf_msgs = ((tf_tree.get("channels") or {}).get("1") or {}).get("messages") or []
        tf_sel = {"1": {tf_msgs[0]["name"]: [s["name"] for s in tf_msgs[0]["signals"]]}} if tf_msgs else {}
        expect = {
            "relative_s": ["0", "0.01", "1"],
            "relative_ms": ["0", "10", "1000"],
            "unix_s": ["1760623800.123456", "1760623800.133456", "1760623801.123456"],
            "absolute": ["2025-10-16 14:10:00.123456", "2025-10-16 14:10:00.133456",
                         "2025-10-16 14:10:01.123456"],
        }

        def timefmt_config(out_dir: Path, fmt: str | None) -> dict:
            cfg = {
                "blf_paths": [str(tf_blf)],
                "dbc_by_channel": {1: str(tf_dbc)},
                "selected": tf_sel,
                "split_by_message": True,
                "resample_hz": None,
                "export_format": "csv",
                "out_dir": str(out_dir),
                "temp_dir": str(out_dir / ".tmp_shards"),
                "excel_row_limit": 1_000_000,
                "memory_budget_mb": 256,
                "ffill": False,
            }
            if fmt is not None:
                cfg["time_format"] = fmt
            return cfg

        for fmt, want in expect.items():
            rep_t = convert_safe(timefmt_config(out_root / f"timefmt_{fmt}", fmt), f"A7 {fmt}")
            tabs = rep_t.get("tables") or []
            if not tabs:
                check(f"A7 {fmt}：出表", False, "无表")
                continue
            with Path(tabs[0]["path"]).open(newline="", encoding="utf-8") as fh:
                data = list(csv.reader(fh))
            header = data[0] if data else []
            got = [data[i][0] for i in (1, 2, 101) if i < len(data)]
            check(f"A7 {fmt}：帧0/帧1/帧100 精确命中独立定值",
                  len(got) == 3 and all(g == w for g, w in zip(got, want)), f"got={got}")
            check(f"A7 {fmt}：首列名为 timestamp", header[:1] == ["timestamp"], f"header={header[:3]}")

        rep_def = convert_safe(timefmt_config(out_root / "timefmt_default", None), "A7 缺省口径")
        tabs = rep_def.get("tables") or []
        if tabs:
            with Path(tabs[0]["path"]).open(newline="", encoding="utf-8") as fh:
                data = list(csv.reader(fh))
            got = [data[i][0] for i in (1, 2) if i < len(data)]
            check("A7 缺省（不传 time_format）等价于 relative_s",
                  got == ["0", "0.01"], f"got={got}")

    # ---------------- A9 宽表同名信号列冲突（真实 DBC 常见；曾导致 Rust panic 崩溃）----------------
    col_blf, col_dbc = FIXTURES / "collide.blf", FIXTURES / "collide.dbc"
    check("A9 冲突夹具存在", col_blf.is_file() and col_dbc.is_file(),
          f"{col_blf.name}={col_blf.stat().st_size if col_blf.is_file() else 'MISSING'}B")
    if col_blf.is_file() and col_dbc.is_file():
        col_sel = {"1": {"MsgA": ["Counter", "OnlyA"], "MsgB": ["Counter"]}}

        def collide_config(out_dir: Path, split: bool, resample: float | None = None,
                           ffill: bool = False) -> dict:
            return {
                "blf_paths": [str(col_blf)],
                "dbc_by_channel": {1: str(col_dbc)},
                "selected": col_sel,
                "split_by_message": split,
                "resample_hz": resample,
                "export_format": "csv",
                "out_dir": str(out_dir),
                "temp_dir": str(out_dir / ".tmp_shards"),
                "excel_row_limit": 1_000_000,
                "memory_budget_mb": 256,
                "ffill": ffill,
            }

        wide_cases = [("keep", None, False), ("10hz", 10.0, False), ("ffill", None, True)]
        for label, resample, ffill in wide_cases:
            rep_c = convert_safe(collide_config(out_root / f"collide_wide_{label}", False, resample, ffill),
                                 f"A9 宽表[{label}]")
            tabs = rep_c.get("tables") or []
            if not tabs:
                check(f"A9 宽表[{label}]：出表", False, "无表（曾经这里是 Rust panic）")
                continue
            with Path(tabs[0]["path"]).open(newline="", encoding="utf-8") as fh:
                header = next(csv.reader(fh))
            check(f"A9 宽表[{label}]：单表且不抛异常", len(tabs) == 1, f"tables={len(tabs)}")
            check(f"A9 宽表[{label}]：timestamp 只出现一次且在第一列",
                  header.count("timestamp") == 1 and header[:1] == ["timestamp"], f"header={header}")
            check(f"A9 宽表[{label}]：同名信号按报文前缀消歧",
                  {"MsgA_Counter", "MsgB_Counter", "OnlyA"} <= set(header), f"header={header}")

        rep_s = convert_safe(collide_config(out_root / "collide_split", True), "A9 拆表")
        s_tabs = rep_s.get("tables") or []
        if s_tabs:
            with Path(s_tabs[0]["path"]).open(newline="", encoding="utf-8") as fh:
                s_header = next(csv.reader(fh))
            check("A9 拆表模式：信号保留原名（表名已区分报文）",
                  "Counter" in s_header and not any(c.startswith("Msg") for c in s_header),
                  f"header={s_header}")

    # ---------------- 收尾 ----------------
    failed = [n for n, ok, _ in RESULTS if not ok]
    print("\n=== 汇总 ===")
    print(f"通过 {len(RESULTS) - len(failed)}/{len(RESULTS)}")
    if failed:
        print("失败项：" + "; ".join(failed))
    if os.environ.get("KEEP_ACCEPT_DIR") != "1":
        shutil.rmtree(out_root, ignore_errors=True)
    else:
        print(f"输出保留在：{out_root}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
