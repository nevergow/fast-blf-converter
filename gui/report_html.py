#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Step 16：离线、自包含的 HTML 报告生成（``gui/report_html.py``）。

设计约束（见 ``docs/tasks/step16_plot_ui.md`` §B）：

* **只消费 ``series_for_plot`` 的结果**——本模块不打开 BLF、不调 ``convert``、不重新抽稀；
  传入的就是桌面预览用的那一份数据。
* **完全自包含**：Apache ECharts 5.5.1（``gui/assets/echarts.min.js``）的正文直接内嵌进
  ``<script>``，数据以 JSON 内嵌，样式内联；产物里**不允许出现任何外部资源引用**
  （``src``/``href``/``fetch``/``@import``/``url(...)`` 指向 http(s)）。
  注意：vendored 的 ECharts 内部保留了 ``http://www.w3.org/...`` 这类 XML 命名空间字符串，
  它们不是资源引用，也不允许改动库文件（见 ``gui/assets/NOTICE.txt``）。
* **体积 < 8MB**：超出时按相邻桶合并（min 取更小、max 取更大，不丢峰）进一步降采样到
  最低 500 桶，并在页面上显式说明；绝不截断数据。
* **原子写盘**：先写同目录临时文件再 ``os.replace``，失败不产出半截文件。

本模块不导入 PySide6，便于在无 GUI 的环境里做单元测试 / 自检。
"""

from __future__ import annotations

import html as _html
import json
import math
import os
import re
import statistics
import sys
import tempfile
from datetime import datetime
from pathlib import Path

# ------------------------------------------------------------------- 常量 ----
ECHARTS_FILENAME = "echarts.min.js"
#: 单文件体积上限（任务书 §B3）。
MAX_REPORT_BYTES = 8 * 1024 * 1024
#: 降采样下限（任务书 §B3：最低 500 桶）。
MIN_REPORT_BUCKETS = 500
#: 默认报告文件名（用户在 ``out_dir`` 里可以改）。
DEFAULT_REPORT_BASENAME = "BLF_Report_{stamp}.html"

#: 曲线配色（与桌面预览共用，保证图例 / 曲线颜色一致）。
SERIES_PALETTE = [
    "#4FC3F7", "#FFB74D", "#81C784", "#E57373", "#BA68C8", "#4DD0E1",
    "#F06292", "#AED581", "#FFD54F", "#9575CD", "#7986CB", "#A1887F",
]
#: Y 轴最多 4 条；第 5 个量纲起并入归一化轴（任务书 §A4）。
MAX_REAL_AXES = 4
#: 颜色数量（桌面预览按信号序号取色，保持一致）。
PALETTE_SIZE = len(SERIES_PALETTE)

#: 外部资源引用检测：只盯 ``src`` / ``href`` / ``fetch`` / ``@import`` / ``url()`` 指向 http(s)。
EXTERNAL_REF_RE = re.compile(
    r"""(?is)
        (?:src|href)\s*=\s*["']\s*https?://
      | fetch\s*\(\s*["']\s*https?://
      | @import\s+(?:url\(\s*)?["']?\s*https?://
      | url\(\s*["']?\s*https?://
    """,
    re.VERBOSE,
)


# ----------------------------------------------------------------- 小工具 ----
def series_color(index: int) -> str:
    """按信号序号取一个稳定的十六进制颜色（与桌面预览一致）。"""
    return SERIES_PALETTE[int(index) % PALETTE_SIZE]


def decimation_note(buckets: int, total_points: int) -> str:
    """统一的抽稀说明（桌面预览信息条与 HTML 页脚共用，任务书 A3/B2 要求逐字一致）。"""
    buckets = max(1, int(buckets or 0))
    total = max(0, int(total_points or 0))
    per_bucket = (total / buckets) if buckets else 0.0
    return (f"已抽稀为 {buckets} 桶 · min/max 包络（不丢峰）· "
            f"每桶 {_fmt_num(per_bucket, 1)} 点")


def human_size(num_bytes: int) -> str:
    """字节数 → 人类可读（报告的文件清单用）。"""
    try:
        size = float(num_bytes)
    except (TypeError, ValueError):
        return "—"
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if size < 1024.0 or unit == "TB":
            return f"{int(size)} {unit}" if unit == "B" else f"{size:.1f} {unit}"
        size /= 1024.0
    return f"{size:.1f} TB"


def _fmt_num(value, digits: int = 4) -> str:
    """紧凑数值格式（去尾零；极大 / 极小走有效数字）。"""
    try:
        number = float(value)
    except (TypeError, ValueError):
        return "—"
    if math.isnan(number) or math.isinf(number):
        return "—"
    if number == 0:
        return "0"
    magnitude = abs(number)
    if magnitude >= 1e7 or magnitude < 1e-4:
        return f"{number:.{max(1, digits)}g}"
    text = f"{number:.{max(1, digits)}f}".rstrip("0").rstrip(".")
    return text or "0"


def default_report_name(now: datetime | None = None) -> str:
    """默认报告文件名 ``BLF_Report_{时间戳}.html``。"""
    stamp = (now or datetime.now()).strftime("%Y%m%d_%H%M%S")
    return DEFAULT_REPORT_BASENAME.format(stamp=stamp)


# --------------------------------------------------------------- 资源定位 ----
def _candidate_roots() -> list[Path]:
    """只读资源候选根目录：冻结态优先，开发态兜底（与 ``gui/main.py`` 同一策略）。"""
    roots: list[Path] = []

    def _add(candidate) -> None:
        if not candidate:
            return
        path = Path(candidate)
        if path not in roots:
            roots.append(path)

    if getattr(sys, "frozen", False):
        exe_dir = Path(sys.executable).resolve().parent
        _add(exe_dir)
        _add(getattr(sys, "_MEIPASS", None))
        _add(exe_dir / "_internal")
    here = Path(__file__).resolve().parent
    _add(here)
    _add(Path.cwd())
    return roots


def resolve_echarts_path() -> Path | None:
    """定位 ``echarts.min.js``（开发态 ``gui/assets/``，冻结态产物 ``gui/assets/``）。"""
    for root in _candidate_roots():
        for candidate in (root / "assets" / ECHARTS_FILENAME,
                          root / "gui" / "assets" / ECHARTS_FILENAME):
            if candidate.is_file():
                return candidate
    return None


def load_echarts_text() -> str:
    """读取 ECharts 正文用于内嵌；找不到时抛 :class:`FileNotFoundError`。"""
    path = resolve_echarts_path()
    if path is None:
        raise FileNotFoundError(
            f"未找到内嵌图表库 {ECHARTS_FILENAME}（应位于 gui/assets/）；"
            "离线报告需要它才能生成")
    return path.read_text(encoding="utf-8")


def is_self_contained(html_text: str) -> bool:
    """返回 True 表示没有任何指向 http(s) 的 ``src``/``href``/``fetch``/``@import``/``url()``。"""
    return EXTERNAL_REF_RE.search(html_text) is None


# --------------------------------------------------------- 数据：轴 / 降采样 --
def _build_axes(series: list[dict]) -> tuple[list[dict], list[int]]:
    """按 ``unit`` 分组生成 Y 轴定义，返回 ``(axes, series→axis 索引)``。

    * 同一量纲共用一条轴；``None``/空单位归入"无量纲"；
    * 最多 :data:`MAX_REAL_AXES` 条实轴；其余量纲共用一条 0~100% 归一化轴。
    """
    groups: dict[str, list[int]] = {}
    order: list[str] = []
    for index, item in enumerate(series):
        unit = (item.get("unit") or "").strip() or "无量纲"
        if unit not in groups:
            groups[unit] = []
            order.append(unit)
        groups[unit].append(index)

    axes: list[dict] = []
    mapping = [0] * max(1, len(series))
    for unit in order[:MAX_REAL_AXES]:
        indexes = groups[unit]
        vmin = min(float(series[i].get("vmin", 0.0) or 0.0) for i in indexes)
        vmax = max(float(series[i].get("vmax", 0.0) or 0.0) for i in indexes)
        pad = (vmax - vmin) * 0.05 if vmax > vmin else 1.0
        axes.append({
            "name": unit, "vmin": vmin - pad, "vmax": vmax + pad,
            "normalized": False, "color": series_color(indexes[0]),
        })
        for i in indexes:
            mapping[i] = len(axes) - 1

    normalized = [i for unit in order[MAX_REAL_AXES:] for i in groups[unit]]
    if normalized:
        axes.append({
            "name": "归一化 %", "vmin": 0.0, "vmax": 100.0,
            "normalized": True, "color": series_color(normalized[0]),
        })
        for i in normalized:
            mapping[i] = len(axes) - 1
    return axes, mapping


def _merge_series(series: list[dict], target_buckets: int) -> list[dict]:
    """把每条曲线的相邻桶合并到约 ``target_buckets``（min/max 包络不丢峰）。"""
    target = max(1, int(target_buckets))
    merged: list[dict] = []
    for item in series:
        clone = dict(item)
        times = list(item.get("t") or [])
        size = len(times)
        if size > target:
            step = int(math.ceil(size / target))
            out_t: list[float] = []
            out_min: list[float] = []
            out_max: list[float] = []
            out_mean: list[float] = []
            mins = list(item.get("min") or [])
            maxs = list(item.get("max") or [])
            means = list(item.get("mean") or [])
            for start in range(0, size, step):
                end = min(start + step, size)
                out_t.append((times[start] + times[end - 1]) / 2.0)
                out_min.append(min(mins[start:end]))
                out_max.append(max(maxs[start:end]))
                chunk = means[start:end]
                out_mean.append(sum(chunk) / len(chunk) if chunk else 0.0)
            clone["t"], clone["min"] = out_t, out_min
            clone["max"], clone["mean"] = out_max, out_mean
        merged.append(clone)
    return merged


# ------------------------------------------------------------- HTML 片段 ----
def _files_section(meta: dict) -> str:
    files = list(meta.get("files") or [])
    if not files:
        return "<p class='muted'>（本次未添加 BLF 文件）</p>"
    rows = []
    for info in files:
        path = str(info.get("path") or "")
        name = _html.escape(os.path.basename(path) or path or "?")
        size = info.get("size")
        size_text = human_size(size) if size is not None else "—"
        note = ""
        if info.get("ok") is False:
            note = f" <span class='bad'>✗ {_html.escape(str(info.get('error') or '读取失败'))}</span>"
        rows.append(f"<tr><td>{name}</td><td class='num'>{size_text}</td>"
                    f"<td>{_html.escape(path)}{note}</td></tr>")
    return ("<table class='grid'><thead><tr><th>文件名</th><th>大小</th><th>路径 / 状态</th>"
            "</tr></thead><tbody>" + "".join(rows) + "</tbody></table>")


def _dbc_section(meta: dict, dbc_errors: dict) -> str:
    dbc_by_channel = {str(k): str(v) for k, v in (meta.get("dbc_by_channel") or {}).items()}
    summary = {str(k): v for k, v in (meta.get("channel_summary") or {}).items()}
    errors = {str(k): str(v) for k, v in (dbc_errors or {}).items()}
    channels = {int(k) for k in list(dbc_by_channel) + list(summary) + list(errors)
                if str(k).isdigit()}
    if not channels:
        return "<p class='muted'>（未绑定任何 DBC）</p>"
    rows = []
    for channel in sorted(channels):
        key = str(channel)
        info = summary.get(key) or {}
        path = dbc_by_channel.get(key) or info.get("dbc")
        error = errors.get(key) or info.get("dbc_error")
        if error:
            # 失败通道显式列出（同界面的红字原则）。
            rows.append(f"<tr class='bad'><td>Ch{channel}</td>"
                        f"<td>{_html.escape(os.path.basename(str(path)) if path else '—')}</td>"
                        f"<td>✗ 解析失败：{_html.escape(str(error))}</td></tr>")
            continue
        if not path:
            rows.append(f"<tr class='warn'><td>Ch{channel}</td><td>—</td>"
                        "<td>未绑定 DBC（该通道报文按原始 ID 处理）</td></tr>")
            continue
        note = info.get("dbc_note")
        status = "✓ 已解析" if info.get("dbc") else "未解析（可点界面『解析信号』）"
        extra = f"；规范化说明：{_html.escape(str(note))}" if note else ""
        rows.append(f"<tr><td>Ch{channel}</td>"
                    f"<td>{_html.escape(os.path.basename(str(path)))}</td>"
                    f"<td>{status}（{_html.escape(str(path))}）{extra}</td></tr>")
    return ("<table class='grid'><thead><tr><th>通道</th><th>DBC</th><th>绑定情况</th>"
            "</tr></thead><tbody>" + "".join(rows) + "</tbody></table>")


def _stats_table(series: list[dict]) -> str:
    if not series:
        return "<p class='muted'>（没有可统计的信号）</p>"
    rows = []
    for item in series:
        means = list(item.get("mean") or [])
        # §1.3 提供 `mean_all`（全量原始数据的算术平均）→ 优先用它，这是**精确值**。
        # 只有旧内核缺该字段时才退回"桶均值的算术平均"近似，并在表下显式标注（不静默降级）。
        raw_mean = item.get("mean_all")
        approx = raw_mean is None
        mean_text = (_fmt_num(statistics.fmean(means)) if means else "—") if approx \
            else _fmt_num(raw_mean)
        unit = (item.get("unit") or "").strip() or "—"
        rows.append(
            "<tr>"
            f"<td>Ch{item.get('channel', '?')}</td>"
            f"<td>{_html.escape(str(item.get('message') or ''))}</td>"
            f"<td>{_html.escape(str(item.get('signal') or ''))}</td>"
            f"<td>{_html.escape(unit)}</td>"
            f"<td class='num'>{int(item.get('count', 0) or 0):,}</td>"
            f"<td class='num'>{_fmt_num(item.get('vmin'))}</td>"
            f"<td class='num'>{_fmt_num(item.get('vmax'))}</td>"
            f"<td class='num'>{mean_text}{' *' if approx else ''}</td>"
            f"<td class='num'>{_fmt_num(item.get('first'))}</td>"
            f"<td class='num'>{_fmt_num(item.get('last'))}</td>"
            "</tr>")
    return (
        "<table class='grid'><thead><tr>"
        "<th>通道</th><th>报文</th><th>信号</th><th>单位</th><th>原始点数</th>"
        "<th>min</th><th>max</th><th>mean</th><th>first</th><th>last</th>"
        "</tr></thead><tbody>" + "".join(rows) + "</tbody></table>"
        "<p class='muted'><strong>原始点数 / min / max / mean / first / last 全部来自全量原始数据</strong>，"
        "不是抽稀后算的（打 * 的 mean 表示该信号缺少全量均值字段，退回桶均值近似）。</p>")


# ------------------------------------------------------------- HTML 主体 ----
_TEMPLATE = """<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>@@TITLE@@</title>
<style>
  :root { color-scheme: dark; }
  body { margin: 0; padding: 24px 28px 40px; background: #1E1E24; color: #E8E8F0;
         font-family: "Microsoft YaHei", "PingFang SC", "Segoe UI", sans-serif; }
  h1 { font-size: 20px; margin: 0 0 6px; }
  h2 { font-size: 15px; margin: 26px 0 10px; border-left: 3px solid #0078D4; padding-left: 8px; }
  .muted { color: #9A9AB0; font-size: 12px; }
  .meta { background: #25252E; border: 1px solid #3A3A46; border-radius: 6px;
          padding: 10px 14px; margin: 12px 0; font-size: 13px; line-height: 1.9; }
  .meta b { color: #FFFFFF; }
  .note { color: #F0A020; }
  table.grid { width: 100%; border-collapse: collapse; font-size: 12.5px; background: #25252E; }
  table.grid th, table.grid td { border: 1px solid #3A3A46; padding: 5px 8px;
                                 text-align: left; vertical-align: top; }
  table.grid th { background: #2D2D38; color: #C8C8D8; font-weight: 600; }
  table.grid td.num { text-align: right; font-variant-numeric: tabular-nums; }
  tr.bad td { color: #E5534B; }
  tr.warn td { color: #F0A020; }
  .bad { color: #E5534B; }
  #chart { width: 100%; height: 660px; background: #1E1E24; border: 1px solid #3A3A46;
           border-radius: 6px; margin-top: 8px; }
  footer { margin-top: 28px; padding-top: 12px; border-top: 1px solid #3A3A46;
           color: #9A9AB0; font-size: 12px; line-height: 1.9; }
</style>
</head>
<body>
<h1>@@TITLE@@</h1>
<div class="meta">
  @@HEADER@@
</div>

<h2>BLF 文件清单</h2>
@@FILES@@

<h2>通道 / DBC 绑定情况</h2>
@@DBC@@

<h2>信号统计（全量原始数据）</h2>
@@STATS@@

<h2>曲线（min/max 包络）</h2>
<p class="muted">滚轮 / 底部滑块缩放，图例可点击显隐；十字光标悬停读数。</p>
<div id="chart"></div>

<footer>
  @@FOOTER@@
</footer>

<script>@@ECHARTS@@</script>
<script>
(function () {
  var DATA = @@DATA_JSON@@;
  var AXES = DATA.axes;
  var SERIES = DATA.series;
  var TIME_FORMAT = DATA.time_format;
  var T_AXIS = DATA.t_axis || [];
  var T_MIN_STR = DATA.t_min_str;

  function fmtNum(v) {
    if (v === null || v === undefined || !isFinite(v)) return '\\u2014';
    var a = Math.abs(v);
    if (a !== 0 && (a < 1e-4 || a >= 1e7)) return v.toExponential(3);
    var r = Math.round(v * 1e6) / 1e6;
    return String(r);
  }
  function pad2(n) { return (n < 10 ? '0' : '') + n; }
  function fmtAbs(t) {
    var m = /^(\\d{4})-(\\d{2})-(\\d{2}) (\\d{2}):(\\d{2}):(\\d{2})(?:\\.(\\d+))?$/.exec(T_MIN_STR || '');
    if (!m) return fmtNum(t) + ' s';
    var micro = m[7] ? parseInt((m[7] + '000000').slice(0, 6), 10) : 0;
    var base = Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +m[6], Math.floor(micro / 1000));
    var d = new Date(base + Math.round(t * 1000));
    return d.getUTCFullYear() + '-' + pad2(d.getUTCMonth() + 1) + '-' + pad2(d.getUTCDate()) +
      ' ' + pad2(d.getUTCHours()) + ':' + pad2(d.getUTCMinutes()) + ':' + pad2(d.getUTCSeconds()) +
      '.' + ('00' + d.getUTCMilliseconds()).slice(-3);
  }
  function fmtTime(t) {
    if (TIME_FORMAT === 'absolute') return fmtAbs(t);
    if (TIME_FORMAT === 'relative_ms') return fmtNum(t * 1000) + ' ms';
    if (TIME_FORMAT === 'unix_s') {
      var d = new Date(t * 1000);
      return d.toISOString().replace('T', ' ').slice(0, 23) + ' UTC';
    }
    return fmtNum(t) + ' s';
  }
  function nearestIndex(arr, t) {
    if (!arr || !arr.length) return -1;
    var lo = 0, hi = arr.length - 1;
    while (lo < hi) { var mid = (lo + hi) >> 1; if (arr[mid] < t) lo = mid + 1; else hi = mid; }
    var i = lo;
    if (i > 0 && Math.abs(arr[i - 1] - t) < Math.abs(arr[i] - t)) i = i - 1;
    return i;
  }
  function axisLabel(v) {
    if (TIME_FORMAT === 'absolute' && T_AXIS.length) {
      var best = T_AXIS[0].label, bd = Infinity;
      for (var i = 0; i < T_AXIS.length; i++) {
        var d = Math.abs(T_AXIS[i].t - v);
        if (d < bd) { bd = d; best = T_AXIS[i].label; }
      }
      return best;
    }
    if (TIME_FORMAT === 'relative_ms') return fmtNum(v * 1000);
    if (TIME_FORMAT === 'unix_s') { var d = new Date(v * 1000); return d.toISOString().slice(11, 19); }
    return fmtNum(v);
  }

  var chartSeries = [];
  var META = {};
  SERIES.forEach(function (s) {
    var comb = [];
    for (var i = 0; i < s.t.length; i++) { comb.push([s.t[i], s.min[i]]); comb.push([s.t[i], s.max[i]]); }
    chartSeries.push({
      name: s.name, type: 'line', showSymbol: false, symbol: 'none', sampling: 'none',
      lineStyle: { width: 1, color: s.color }, itemStyle: { color: s.color },
      data: comb, yAxisIndex: s.yAxis, z: 2, emphasis: { disabled: true }
    });
    var meanData = [];
    for (var j = 0; j < s.t.length; j++) { meanData.push([s.t[j], s.mean[j]]); }
    chartSeries.push({
      name: s.name + '\\u00b7mean', type: 'line', showSymbol: false, symbol: 'none',
      lineStyle: { width: 1, color: s.color, opacity: 0.45 }, itemStyle: { color: s.color },
      data: meanData, yAxisIndex: s.yAxis, z: 3, silent: true
    });
    META[s.name] = s;
  });

  var axDefs = AXES.map(function (a, i) {
    return {
      type: 'value', name: a.name, position: 'left', offset: i * 64,
      min: a.vmin, max: a.vmax,
      axisLine: { show: true, lineStyle: { color: a.color } },
      axisTick: { show: true, lineStyle: { color: a.color } },
      axisLabel: { color: a.color, fontSize: 10, formatter: function (v) { return fmtNum(v); } },
      nameTextStyle: { color: a.color, fontSize: 11 },
      splitLine: { show: i === 0, lineStyle: { color: '#2A2A33' } }
    };
  });
  if (!axDefs.length) { axDefs = [{ type: 'value', position: 'left', name: '' }]; }

  var option = {
    animation: false,
    backgroundColor: '#1E1E24',
    textStyle: { color: '#E8E8F0' },
    grid: { left: 44 + 64 * AXES.length, right: 24, top: 34, bottom: 80 },
    legend: {
      type: 'scroll', top: 0, textStyle: { color: '#E8E8F0' },
      data: SERIES.map(function (s) { return s.name; })
    },
    tooltip: {
      trigger: 'axis',
      axisPointer: { type: 'cross', label: { backgroundColor: '#0078D4' } },
      backgroundColor: '#25252E', borderColor: '#3A3A46', textStyle: { color: '#E8E8F0' },
      formatter: function (params) {
        if (!params || !params.length) return '';
        var t = params[0].value[0];
        var out = '时间：' + fmtTime(t);
        var seen = {};
        params.forEach(function (p) {
          var s = META[p.seriesName];
          if (!s || seen[p.seriesName]) return;
          seen[p.seriesName] = 1;
          var i = nearestIndex(s.t, t);
          if (i < 0) return;
          var unit = s.unit ? ' ' + s.unit : '';
          out += '<br><span style="color:' + s.color + '">\\u25cf</span> ' + p.seriesName +
            ': ' + fmtNum(s.min[i]) + ' ~ ' + fmtNum(s.max[i]) +
            ' (mean ' + fmtNum(s.mean[i]) + ')' + unit;
          if (s.normalized) {
            out += ' \\uff5c原量程 ' + fmtNum(s.orig_range[0]) + ' ~ ' + fmtNum(s.orig_range[1]);
          }
        });
        return out;
      }
    },
    xAxis: {
      type: 'value', min: DATA.t_min, max: DATA.t_max, name: '时间',
      nameTextStyle: { color: '#9A9AB0' },
      axisLabel: { color: '#9A9AB0', formatter: axisLabel, hideOverlap: true },
      axisLine: { lineStyle: { color: '#3A3A46' } }, splitLine: { show: false }
    },
    yAxis: axDefs,
    dataZoom: [
      { type: 'inside', xAxisIndex: 0 },
      { type: 'slider', xAxisIndex: 0, bottom: 26, height: 22,
        backgroundColor: '#25252E', borderColor: '#3A3A46', textStyle: { color: '#9A9AB0' },
        dataBackground: { lineStyle: { color: '#3A3A46' }, areaStyle: { color: '#25252E' } },
        fillerColor: 'rgba(0,120,212,0.20)', handleStyle: { color: '#0078D4' } }
    ],
    series: chartSeries
  };
  var chart = echarts.init(document.getElementById('chart'), null, { renderer: 'canvas' });
  chart.setOption(option);
  window.addEventListener('resize', function () { chart.resize(); });
})();
</script>
</body>
</html>
"""


def _json_for_script(payload) -> str:
    """JSON 内嵌进 ``<script>``：转义 ``</`` 防止提前闭合，保留中文。"""
    text = json.dumps(payload, ensure_ascii=False, separators=(",", ":"))
    return text.replace("</", "<\\/")


def _header_html(meta: dict, plot_data: dict, series: list[dict]) -> str:
    total_points = sum(int(item.get("count", 0) or 0) for item in series)
    time_format = str(meta.get("time_format") or plot_data.get("time_format") or "relative_s")
    if time_format == "absolute" and plot_data.get("t_min_str"):
        time_range = (f"{plot_data.get('t_min_str')} → {plot_data.get('t_max_str')}"
                      "（绝对时间，BLF 头墙钟，未做时区换算）")
    else:
        time_range = f"{_fmt_num(plot_data.get('t_min'))} s → {_fmt_num(plot_data.get('t_max'))} s"
    title = str(meta.get("title") or "Fast BLF Converter · 数据预览报告")
    generated = str(meta.get("generated_at") or datetime.now().strftime("%Y-%m-%d %H:%M:%S"))
    version = str(meta.get("version") or "—")
    lines = [
        f"<b>报告标题：</b>{_html.escape(title)}",
        f"<b>生成时间：</b>{_html.escape(generated)}",
        f"<b>时间范围：</b>{_html.escape(time_range)}",
        f"<b>时间口径：</b>{_html.escape(time_format)}",
        f"<b>信号数：</b>{len(series)} 个　<b>总原始采样点数：</b>{total_points:,}",
        f"<b>软件版本：</b>{_html.escape(version)}",
    ]
    errors = list(plot_data.get("errors") or [])
    if errors:
        detail = "；".join(
            f"{os.path.basename(str(e.get('file') or ''))}: {e.get('code')} {e.get('message')}"
            for e in errors)
        lines.append(f"<span class='bad'><b>文件错误：</b>{_html.escape(detail)}</span>")
    warnings = list(plot_data.get("warnings") or [])
    if warnings:
        lines.append(f"<span class='note'><b>警告：</b>"
                     f"{_html.escape('；'.join(str(w) for w in warnings))}</span>")
    return "<br>".join(lines)


def build_report_html(plot_data: dict, meta: dict | None = None,
                      series: list[dict] | None = None,
                      buckets: int | None = None, shrink_note: str | None = None) -> str:
    """把 ``series_for_plot`` 结果渲染成一个完全自包含的 HTML 字符串。

    * ``series`` 允许传入被 :func:`_merge_series` 进一步降采样后的曲线；
    * ``buckets`` 是页面"抽稀说明"里要显示的桶数；
    * ``shrink_note`` 非空时在页面上显式说明"为控制体积做了二次降采样"。
    """
    meta = dict(meta or {})
    series = list(series if series is not None else (plot_data.get("series") or []))
    dbc_errors = dict(plot_data.get("dbc_errors") or {})
    buckets = int(buckets or plot_data.get("buckets") or 0) or max(
        1, max((len(item.get("t") or []) for item in series), default=1))
    total_points = sum(int(item.get("count", 0) or 0) for item in series)

    axes, mapping = _build_axes(series)
    # 名称唯一化（同名信号不同报文时加报文后缀）。
    seen_names: dict[str, int] = {}
    for item in series:
        base = str(item.get("signal") or "?")
        seen_names[base] = seen_names.get(base, 0) + 1

    data_series = []
    for index, item in enumerate(series):
        base = str(item.get("signal") or "?")
        name = base if seen_names.get(base, 0) == 1 else f"{base}@{item.get('message')}"
        unit = (item.get("unit") or "").strip()
        axis_index = mapping[index] if index < len(mapping) else 0
        normalized = bool(axes[axis_index]["normalized"]) if axes else False
        axis = axes[axis_index] if axes else {"vmin": 0.0, "vmax": 1.0}

        def _scale(values):
            if not normalized:
                return [round(float(v), 6) for v in values]
            lo = float(item.get("vmin", 0.0) or 0.0)
            hi = float(item.get("vmax", 0.0) or 0.0)
            if not (hi > lo):
                return [50.0 for _ in values]
            return [round((float(v) - lo) / (hi - lo) * 100.0, 4) for v in values]

        t_values = [round(float(v), 6) for v in (item.get("t") or [])]
        data_series.append({
            "name": name,
            "unit": unit,
            "color": series_color(index),
            "count": int(item.get("count", 0) or 0),
            "yAxis": axis_index,
            "normalized": normalized,
            "orig_range": [item.get("vmin"), item.get("vmax")],
            "t": t_values,
            "min": _scale(item.get("min") or []),
            "max": _scale(item.get("max") or []),
            "mean": _scale(item.get("mean") or []),
        })

    t_min = float(plot_data.get("t_min") or 0.0)
    t_max = float(plot_data.get("t_max") or 0.0)
    if not (t_max > t_min):
        t_max = t_min + 1.0
    t_axis = []
    for tick in plot_data.get("t_axis") or []:
        try:
            t_axis.append({"t": float(tick.get("t")), "label": str(tick.get("label"))})
        except (TypeError, ValueError):
            continue

    payload = {
        "series": data_series,
        "axes": axes,
        "time_format": str(meta.get("time_format") or plot_data.get("time_format")
                           or "relative_s"),
        "t_axis": t_axis,
        "t_min": t_min,
        "t_max": t_max,
        "t_min_str": plot_data.get("t_min_str"),
        "t_max_str": plot_data.get("t_max_str"),
    }

    title = str(meta.get("title") or "Fast BLF Converter · 数据预览报告")
    footer = [
        decimation_note(buckets, total_points),
        f"版本号：{_html.escape(str(meta.get('version') or '—'))}",
        "本报告为离线自包含文件（图表库与数据均已内嵌，无需联网）。",
    ]
    if shrink_note:
        footer.insert(0, f"<span class='note'>{_html.escape(shrink_note)}</span>")
    html_text = _TEMPLATE
    html_text = html_text.replace("@@TITLE@@", _html.escape(title))
    html_text = html_text.replace("@@HEADER@@", _header_html(meta, plot_data, series))
    html_text = html_text.replace("@@FILES@@", _files_section(meta))
    html_text = html_text.replace("@@DBC@@", _dbc_section(meta, dbc_errors))
    html_text = html_text.replace("@@STATS@@", _stats_table(series))
    html_text = html_text.replace("@@FOOTER@@", "<br>".join(footer))
    html_text = html_text.replace("@@DATA_JSON@@", _json_for_script(payload))
    html_text = html_text.replace("@@ECHARTS@@", load_echarts_text())
    return html_text


# --------------------------------------------------------------- 落盘入口 ----
def generate_report(plot_data: dict, meta: dict | None,
                    out_path: str | os.PathLike[str],
                    max_bytes: int = MAX_REPORT_BYTES) -> dict:
    """生成并**原子**写入离线 HTML 报告。

    返回 ``{"ok", "path", "bytes", "buckets", "shrunk", "error"}``：
    * 体积超过 ``max_bytes`` 时按相邻桶合并降采样（最低 :data:`MIN_REPORT_BUCKETS` 桶），
      并在页面上说明；绝不截断数据；
    * 写盘失败（权限 / 磁盘）返回 ``ok=False`` + 人话错误，**不留下半截文件**。
    """
    result = {"ok": False, "path": str(out_path), "bytes": 0, "buckets": 0,
              "shrunk": False, "error": None}
    try:
        load_echarts_text()
    except (OSError, FileNotFoundError) as exc:
        result["error"] = f"缺少内嵌图表库：{exc}"
        return result

    series = list(plot_data.get("series") or [])
    effective = int(plot_data.get("buckets") or 0) or max(
        (len(item.get("t") or []) for item in series), default=0)
    shrink_note = None
    html_text = ""
    while True:
        try:
            html_text = build_report_html(plot_data, meta, series=series,
                                          buckets=effective, shrink_note=shrink_note)
        except (OSError, FileNotFoundError) as exc:
            result["error"] = f"生成报告失败：{exc}"
            return result
        size = len(html_text.encode("utf-8"))
        if size < max_bytes:
            break
        target = max(MIN_REPORT_BUCKETS, effective // 2)
        if target >= effective:
            # 已到下限仍偏大：如实写出并在页面说明，不截断数据。
            shrink_note = (f"文件体积接近上限（{human_size(size)}）：曲线已降到最低 "
                           f"{effective} 桶；统计表仍基于全量原始数据。")
            html_text = build_report_html(plot_data, meta, series=series,
                                          buckets=effective, shrink_note=shrink_note)
            size = len(html_text.encode("utf-8"))
            break
        series = _merge_series(series, target)
        effective = target
        result["shrunk"] = True
        shrink_note = (f"为把文件控制在 8MB 以内，曲线已从原始抽稀结果进一步降采样为约 "
                       f"{effective} 桶（min/max 包络仍不丢峰）；统计表不受影响。")

    if not is_self_contained(html_text):
        result["error"] = "生成的报告仍包含外部资源引用（src/href/fetch 指向 http(s)），已中止"
        return result

    out = Path(out_path).expanduser()
    temp_path: str | None = None
    try:
        out.parent.mkdir(parents=True, exist_ok=True)
        handle, temp_path = tempfile.mkstemp(prefix=".fblf_report_", suffix=".tmp",
                                             dir=str(out.parent))
        with os.fdopen(handle, "w", encoding="utf-8", newline="") as stream:
            stream.write(html_text)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp_path, out)
        temp_path = None
    except OSError as exc:
        result["error"] = f"写入报告失败：{exc}"
        return result
    finally:
        if temp_path:
            try:
                os.unlink(temp_path)
            except OSError:
                pass

    result.update({"ok": True, "bytes": size, "buckets": effective,
                   "path": str(out), "error": None})
    return result
