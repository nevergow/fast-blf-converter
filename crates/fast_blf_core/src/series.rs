//! Step 15：`series_for_plot` 的核心算法（画图数据接口，见 `docs/INTERFACE.md` §1.3）。
//!
//! 设计要点：
//! * **两阶段、峰值内存 O(buckets × 信号数)**：第一阶段只遍历 BLF 帧头求全局
//!   `t_min`/`t_max`（不解码信号、不累积任何采样点）；第二阶段解码每一帧并**立即入桶**，
//!   绝不把全量采样点攒在内存里。桶边界依赖全局时间范围，本就无法在"只读一帧"时确定，
//!   任务书明确允许两阶段（判据是内存而不是读文件次数）。
//! * **min/max 包络不丢峰**：每个桶保存桶内全部原始采样点的 `min`/`max`/`mean`。
//! * **所有信号共用同一分桶网格**：按全局 `t_min..t_max` 等宽切分，多信号可对齐比较。
//! * **空桶跳过**：某信号在某桶内无采样时，该桶整体不出现在 `t`/`min`/`max`/`mean` 中
//!   （三数组等长且一一对应，不补 0、不前向填充）。
//! * **时间口径与 `exporter.rs` 完全一致**：`t` 恒为 float 秒；绝对时间字符串复用同一套
//!   墙钟换算口径 `wall.epoch_micros() + round((raw - header_epoch) * 1e6)`。
//!
//! 固定错误码 `BAD_CONFIG`：`buckets` 越界（`docs/INTERFACE.md` §1.3）。
//! `error.rs` 的稳定错误码表暂未收录它，而本步不允许改 `error.rs`，故在本模块集中定义一次。

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use blf_asc::BlfReader;
use chrono::DateTime;

use crate::decoder::{
    read_file_timing, DecodePlan, FileTiming, MessagePlan, Selection, BAD_TIME_HEADER,
};
use crate::error::{CoreError, BLF_OPEN, BLF_PARSE};
use crate::exporter::TimeFormat;

/// `buckets` 缺省值（`docs/INTERFACE.md` §1.3）。
pub const DEFAULT_BUCKETS: usize = 2000;
/// `buckets` 允许下界（含）。
pub const MIN_BUCKETS: usize = 100;
/// `buckets` 允许上界（含）。
pub const MAX_BUCKETS: usize = 20000;

/// `buckets` 越界错误码。用冻结契约 §4 的稳定错误码 `BAD_CONFIG`
///（§1.3 曾误写为 `BAD_CONFIG`，与 §4 的稳定清单冲突，已按 §4 更正；
///  config 校验类错误在本 crate 里一律是 `BAD_CONFIG`，与 time_format/resample_hz 一致）。
pub const BAD_CONFIG: &str = "BAD_CONFIG";

/// `series_for_plot` 的输入（已从 Python config 解析成 Rust 类型）。
#[derive(Debug, Clone)]
pub struct SeriesRequest {
    pub blf_paths: Vec<PathBuf>,
    pub dbc_by_channel: BTreeMap<u32, PathBuf>,
    pub selected: Selection,
    pub time_format: TimeFormat,
    pub buckets: usize,
}

/// 单个文件的失败记录（返回值的 `errors[]`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesFileError {
    pub file: String,
    pub code: String,
    pub message: String,
}

/// 单条曲线的抽稀结果与全量统计（字段名与 §1.3 逐字一致）。
#[derive(Debug, Clone)]
pub struct SignalSeries {
    pub channel: u32,
    pub message: String,
    pub signal: String,
    pub unit: Option<String>,
    /// 原始采样点数（**不是**桶数）。
    pub count: u64,
    /// 非空桶中心时间（秒）。
    pub t: Vec<f64>,
    pub min: Vec<f64>,
    pub max: Vec<f64>,
    pub mean: Vec<f64>,
    /// 全量原始数据的最小/最大值（不是抽稀后的）。
    pub vmin: f64,
    pub vmax: f64,
    /// 全量原始数据的首/末采样值（按时间先后）。
    pub first: f64,
    pub last: f64,
}

/// `absolute` 口径下的一个时间刻度。
#[derive(Debug, Clone)]
pub struct AxisTick {
    /// 相对秒（0 = 全局最早时间戳）。
    pub t: f64,
    /// 该时刻的绝对墙钟字符串。
    pub label: String,
}

/// `series_for_plot` 的返回值。
#[derive(Debug, Clone)]
pub struct SeriesOutput {
    pub ok: bool,
    pub t_min: f64,
    pub t_max: f64,
    pub t_min_str: Option<String>,
    pub t_max_str: Option<String>,
    pub t_axis: Vec<AxisTick>,
    pub buckets: usize,
    pub series: Vec<SignalSeries>,
    pub dbc_errors: BTreeMap<u32, String>,
    pub warnings: Vec<String>,
    pub errors: Vec<SeriesFileError>,
}

/// 校验 `buckets` 是否落在 `100..=20000`；越界返回 `BAD_CONFIG`。
pub fn validate_buckets(buckets: usize) -> Result<(), CoreError> {
    if (MIN_BUCKETS..=MAX_BUCKETS).contains(&buckets) {
        Ok(())
    } else {
        Err(CoreError::new(
            BAD_CONFIG,
            format!(
                "config 字段 `buckets` 越界：{buckets}（允许 {MIN_BUCKETS}..={MAX_BUCKETS}）"
            ),
        ))
    }
}

/// 单个桶的聚合量：`count` 为桶内原始采样点数，`sum` 用于均值。
#[derive(Debug, Clone, Copy)]
struct BucketAgg {
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
}

impl BucketAgg {
    fn empty() -> Self {
        Self {
            count: 0,
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }
}

/// 单条曲线的抽稀结果（与 [`SignalSeries`] 的四个数组同构，便于复用）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BucketedSeries {
    pub t: Vec<f64>,
    pub min: Vec<f64>,
    pub max: Vec<f64>,
    pub mean: Vec<f64>,
}

/// 计算桶索引：`floor((t - grid_min) / grid_span * buckets)`，clamp 到 `0..=buckets-1`。
///
/// * `grid_span == 0`（全部时间戳相同）或非有限 → 一律落桶 0；
/// * 浮点边界必须 clamp：`t == t_max` 时 `ratio` 恰好等于 `buckets`，不 clamp 会越界。
#[inline]
pub(crate) fn bucket_index(t: f64, grid_min: f64, grid_span: f64, buckets: usize) -> usize {
    let last = buckets.saturating_sub(1);
    if buckets == 0 || !(grid_span > 0.0) || !t.is_finite() {
        return 0;
    }
    let ratio = (t - grid_min) / grid_span * buckets as f64;
    if !ratio.is_finite() {
        return 0;
    }
    let floored = ratio.floor();
    if floored <= 0.0 {
        0
    } else if floored >= last as f64 {
        last
    } else {
        floored as usize
    }
}

/// 单条曲线的流式累加器：一帧采样点进来就更新桶与全量统计。
///
/// 这是"攒桶不攒点"的核心：内存恒为 `O(buckets)`，与采样点数无关。
#[derive(Debug, Clone)]
pub(crate) struct SignalAccumulator {
    /// 原始采样点数。
    pub count: u64,
    /// 全量最小/最大。
    pub vmin: f64,
    pub vmax: f64,
    /// 首/末采样值（按时间先后）。
    pub first: f64,
    pub last: f64,
    first_t: f64,
    last_t: f64,
    buckets: Vec<BucketAgg>,
    grid_min: f64,
    grid_span: f64,
}

impl SignalAccumulator {
    /// 新建；`grid_span` 为全局时间跨度，`buckets` 为桶数。
    pub(crate) fn new(buckets: usize, grid_min: f64, grid_span: f64) -> Self {
        Self {
            count: 0,
            vmin: f64::INFINITY,
            vmax: f64::NEG_INFINITY,
            first: 0.0,
            last: 0.0,
            first_t: f64::INFINITY,
            last_t: f64::NEG_INFINITY,
            buckets: vec![BucketAgg::empty(); buckets],
            grid_min,
            grid_span,
        }
    }

    /// 放入一个原始采样点：更新全量统计与所属桶。
    #[inline]
    pub(crate) fn push(&mut self, t: f64, value: f64) {
        self.count += 1;
        if value < self.vmin {
            self.vmin = value;
        }
        if value > self.vmax {
            self.vmax = value;
        }
        // first/last 按时间先后取：t 是原始时间戳的仿射变换（正斜率），比较 t 即可。
        if t < self.first_t {
            self.first_t = t;
            self.first = value;
        }
        if t > self.last_t {
            self.last_t = t;
            self.last = value;
        }

        let idx = bucket_index(t, self.grid_min, self.grid_span, self.buckets.len());
        let bucket = &mut self.buckets[idx];
        if bucket.count == 0 {
            bucket.min = value;
            bucket.max = value;
        } else {
            if value < bucket.min {
                bucket.min = value;
            }
            if value > bucket.max {
                bucket.max = value;
            }
        }
        bucket.sum += value;
        bucket.count += 1;
    }

    /// 收口：只输出非空桶（按桶索引升序），三数组等长且一一对应。
    pub(crate) fn finish(&self) -> BucketedSeries {
        let width = if self.buckets.is_empty() {
            0.0
        } else {
            self.grid_span / self.buckets.len() as f64
        };
        let mut t = Vec::new();
        let mut min = Vec::new();
        let mut max = Vec::new();
        let mut mean = Vec::new();
        for (idx, bucket) in self.buckets.iter().enumerate() {
            if bucket.count == 0 {
                continue;
            }
            t.push(self.grid_min + (idx as f64 + 0.5) * width);
            min.push(bucket.min);
            max.push(bucket.max);
            mean.push(bucket.sum / bucket.count as f64);
        }
        BucketedSeries { t, min, max, mean }
    }
}

/// 把原始时间戳换算成返回值里的 `t`（**恒为 float 秒**，见 §1.3 时间口径）。
///
/// * `relative_s` → 相对秒；
/// * `relative_ms` → **秒**（不是毫秒；避免与导出侧两套口径打架）；
/// * `absolute` → 相对秒（0 = 全局最早时间戳）；
/// * `unix_s` → epoch 秒。
#[inline]
pub(crate) fn to_plot_seconds(raw: f64, raw_min: f64, format: TimeFormat) -> f64 {
    match format {
        TimeFormat::UnixS => raw,
        TimeFormat::RelativeS | TimeFormat::RelativeMs | TimeFormat::Absolute => raw - raw_min,
    }
}

/// 帧 ID 归一化：`id | if is_extended_id { 1<<31 }`（与 `can_dbc::MessageId::raw()` 对齐）。
///
/// `decoder.rs` 里同名函数是私有的（本步只读，不改），故按同口径复制一份。
#[inline]
fn normalize_frame_id(msg: &blf_asc::Message) -> u32 {
    msg.arbitration_id.0 | if msg.is_extended_id { 1 << 31 } else { 0 }
}

/// 把 epoch 微秒整数格式化为 `%Y-%m-%d %H:%M:%S.%6f`（UTC，原样墙钟，不做时区换算）。
///
/// 与 `exporter::format_wall_clock_micros` **逐字一致**：该函数在 `exporter` 内是私有的，
/// 本步不允许改 `exporter.rs`，故按口径复制，保证两处绝对时间字符串完全相同。
fn format_wall_clock_micros(micros: i64) -> String {
    let seconds = micros.div_euclid(1_000_000);
    let sub_micros = micros.rem_euclid(1_000_000);
    match DateTime::from_timestamp(seconds, (sub_micros * 1000) as u32) {
        Some(dt) => format!("{}.{:06}", dt.format("%Y-%m-%d %H:%M:%S"), sub_micros),
        None => String::new(),
    }
}

/// 秒级墙钟字符串（`absolute` 且时间跨度 ≥ 2 秒时使用）。
fn format_wall_clock_seconds(micros: i64) -> String {
    let seconds = micros.div_euclid(1_000_000);
    match DateTime::from_timestamp(seconds, 0) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => String::new(),
    }
}

/// 一帧相对其文件头的绝对墙钟微秒：
/// `wall_micros + round((raw - header_epoch) * 1e6)`（口径同 `exporter::TimeTransform`）。
fn absolute_micros(timing: &FileTiming, raw: f64) -> Option<i64> {
    let wall = timing.wall.as_ref()?;
    let wall_micros = wall.epoch_micros()?;
    let delta = (raw - timing.header_epoch) * 1_000_000.0;
    if !delta.is_finite() {
        return None;
    }
    Some((wall_micros as f64 + delta).round() as i64)
}

/// 第一阶段结果：全局时间范围 + 绝对墙钟端点 + 打不开的文件下标。
struct TimeScan {
    raw_min: f64,
    raw_max: f64,
    abs_min: Option<i64>,
    abs_max: Option<i64>,
    failed: Vec<usize>,
}

/// 第一阶段：只遍历帧头求全局 `t_min`/`t_max`（可顺带记绝对墙钟端点）。
///
/// 不访问 `msg.data`、不做信号解码、不累积采样点，因此内存为 O(1)（外加失败文件名）。
fn scan_time_range(
    paths: &[PathBuf],
    timings: &[FileTiming],
    format: TimeFormat,
    errors: &mut Vec<SeriesFileError>,
) -> TimeScan {
    let mut raw_min = f64::INFINITY;
    let mut raw_max = f64::NEG_INFINITY;
    let mut abs_min = None;
    let mut abs_max = None;
    let mut failed = Vec::new();

    for (idx, path) in paths.iter().enumerate() {
        let mut reader = match BlfReader::open(path) {
            Ok(reader) => reader,
            Err(err) => {
                errors.push(SeriesFileError {
                    file: path.to_string_lossy().to_string(),
                    code: BLF_OPEN.to_string(),
                    message: format!("无法打开 BLF 文件：{err}"),
                });
                failed.push(idx);
                continue;
            }
        };
        for msg in reader.by_ref() {
            let ts = msg.timestamp;
            if !ts.is_finite() {
                continue;
            }
            if ts < raw_min {
                raw_min = ts;
                if format == TimeFormat::Absolute {
                    abs_min = timings.get(idx).and_then(|timing| absolute_micros(timing, ts));
                }
            }
            if ts > raw_max {
                raw_max = ts;
                if format == TimeFormat::Absolute {
                    abs_max = timings.get(idx).and_then(|timing| absolute_micros(timing, ts));
                }
            }
        }
    }

    TimeScan {
        raw_min,
        raw_max,
        abs_min,
        abs_max,
        failed,
    }
}

/// 第二阶段用的一条信号槽位（顺序固定，供输出与解码索引对齐）。
struct SignalSlot {
    channel: u32,
    message: String,
    signal: String,
    unit: Option<String>,
}

/// 第二阶段：解码 + 入桶 + 全量统计。返回每条有采样的曲线的结果。
///
/// 内存 = `信号数 × buckets × sizeof(BucketAgg)` + 输出数组，与采样点数无关。
#[allow(clippy::too_many_arguments)]
fn decode_and_bucket(
    paths: &[PathBuf],
    failed: &[usize],
    plan: &DecodePlan,
    format: TimeFormat,
    raw_min: f64,
    grid_min: f64,
    grid_span: f64,
    buckets: usize,
    errors: &mut Vec<SeriesFileError>,
    warnings: &mut Vec<String>,
) -> Vec<SignalSeries> {
    // 把计划的报文按 (channel, frame_id) 排序，保证输出顺序稳定；信号保持 DBC 出现顺序。
    let mut messages: Vec<&MessagePlan> = plan.messages().collect();
    messages.sort_by(|a, b| {
        (a.channel, a.frame_id)
            .cmp(&(b.channel, b.frame_id))
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut slots: Vec<SignalSlot> = Vec::new();
    // (channel, frame_id) → 该报文各信号在 `slots` / `accs` 里的下标（与计划信号顺序一致）。
    let mut by_key: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
    for message in &messages {
        let key = (message.channel, message.frame_id);
        let mut indices = Vec::with_capacity(message.signals.len());
        for signal in &message.signals {
            indices.push(slots.len());
            slots.push(SignalSlot {
                channel: message.channel,
                message: message.name.clone(),
                signal: signal.name.clone(),
                unit: if signal.unit.is_empty() {
                    None
                } else {
                    Some(signal.unit.clone())
                },
            });
        }
        by_key.insert(key, indices);
    }

    // 每条信号一份累加器：buckets 个桶，初期全部为空。
    let mut accs: Vec<SignalAccumulator> = slots
        .iter()
        .map(|_| SignalAccumulator::new(buckets, grid_min, grid_span))
        .collect();

    for (file_idx, path) in paths.iter().enumerate() {
        if failed.contains(&file_idx) {
            continue;
        }
        let mut reader = match BlfReader::open(path) {
            Ok(reader) => reader,
            Err(err) => {
                errors.push(SeriesFileError {
                    file: path.to_string_lossy().to_string(),
                    code: BLF_OPEN.to_string(),
                    message: format!("无法打开 BLF 文件：{err}"),
                });
                continue;
            }
        };
        let mut file_decode_errors: u64 = 0;
        for msg in reader.by_ref() {
            // 通道 0-based → 1-based，与 decoder 全模块一致。
            let channel = msg.channel as u32 + 1;
            let frame_id = normalize_frame_id(&msg);
            let Some(message_plan) = plan.lookup(channel, frame_id) else {
                continue;
            };
            let Some(indices) = by_key.get(&(channel, frame_id)) else {
                continue;
            };

            // 先整帧解出临时 Vec，只有全部信号成功才入桶（与 process_file 的"行一致"口径一致）。
            let mut values: Vec<f64> = Vec::with_capacity(message_plan.signals.len());
            let mut failed_decode = false;
            for signal in &message_plan.signals {
                match signal.decode(msg.data.as_slice()) {
                    Ok(value) => values.push(value),
                    Err(_) => {
                        failed_decode = true;
                        break;
                    }
                }
            }
            if failed_decode {
                file_decode_errors += 1;
                continue;
            }

            let t = to_plot_seconds(msg.timestamp, raw_min, format);
            for (i, &slot_idx) in indices.iter().enumerate() {
                if let Some(value) = values.get(i) {
                    accs[slot_idx].push(t, *value);
                }
            }
        }

        if let Some(err) = reader.take_error() {
            errors.push(SeriesFileError {
                file: path.to_string_lossy().to_string(),
                code: BLF_PARSE.to_string(),
                message: format!("解析 BLF 时出错：{err}"),
            });
        }
        if file_decode_errors > 0 {
            warnings.push(format!(
                "{} 有 {} 帧信号位提取失败，已跳过",
                path.to_string_lossy(),
                file_decode_errors
            ));
        }
    }

    // 收口：只保留有采样的信号（无采样时 vmin/vmax/first/last 无定义，不返回空曲线）。
    let mut series = Vec::new();
    for (slot, acc) in slots.into_iter().zip(accs.into_iter()) {
        if acc.count == 0 {
            continue;
        }
        let bucketed = acc.finish();
        series.push(SignalSeries {
            channel: slot.channel,
            message: slot.message,
            signal: slot.signal,
            unit: slot.unit,
            count: acc.count,
            t: bucketed.t,
            min: bucketed.min,
            max: bucketed.max,
            mean: bucketed.mean,
            vmin: acc.vmin,
            vmax: acc.vmax,
            first: acc.first,
            last: acc.last,
        });
    }
    series
}

/// 构造 `absolute` 口径的等间隔刻度（10 个，含首尾）。
fn build_axis(t_min: f64, t_max: f64, abs_origin_micros: i64) -> Vec<AxisTick> {
    const TICKS: usize = 10;
    let span = t_max - t_min;
    // 时间跨度 < 2 秒时给到微秒，否则秒级即可（§1.3）。
    let with_micros = span < 2.0;
    let mut axis = Vec::with_capacity(TICKS);
    for k in 0..TICKS {
        let frac = k as f64 / (TICKS - 1) as f64;
        let t = t_min + frac * span;
        let micros = abs_origin_micros + ((t - t_min) * 1_000_000.0).round() as i64;
        let label = if with_micros {
            format_wall_clock_micros(micros)
        } else {
            format_wall_clock_seconds(micros)
        };
        axis.push(AxisTick { t, label });
    }
    axis
}

/// `series_for_plot` 的 Rust 核心（PyO3 胶水在 `lib.rs`）。
pub fn series_for_plot(request: &SeriesRequest) -> Result<SeriesOutput, CoreError> {
    // 参数校验先于任何重活：越界直接 BAD_CONFIG。
    validate_buckets(request.buckets)?;

    // 宽松构建：单通道 DBC 失败只记 dbc_errors 并跳过该通道（§5 第 9 条）。
    let (plan, dbc_errors, dbc_notes) =
        DecodePlan::build_lenient(&request.dbc_by_channel, &request.selected);
    let dbc_error_reasons: BTreeMap<u32, String> = dbc_errors
        .iter()
        .map(|(&channel, err)| (channel, err.message.clone()))
        .collect();

    let mut warnings: Vec<String> = dbc_errors
        .iter()
        .map(|(&channel, err)| {
            format!(
                "通道 {channel} 的 DBC 解析失败，已跳过该通道（原因：{}）",
                err.message
            )
        })
        .collect();
    // DBC 预规范化 notes 逐条并入 warnings（与 convert 同格式）。
    for (channel, notes) in &dbc_notes {
        let label = request
            .dbc_by_channel
            .get(channel)
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("通道 {channel}"));
        for note in notes {
            warnings.push(format!("DBC「{label}」：{note}"));
        }
    }

    let mut errors: Vec<SeriesFileError> = Vec::new();

    // 空输入：返回空 series 且 ok=true（§1.3，不报错）。
    if request.blf_paths.is_empty() {
        return Ok(SeriesOutput {
            ok: true,
            t_min: 0.0,
            t_max: 0.0,
            t_min_str: None,
            t_max_str: None,
            t_axis: Vec::new(),
            buckets: request.buckets,
            series: Vec::new(),
            dbc_errors: dbc_error_reasons,
            warnings,
            errors,
        });
    }

    // 所选内容全部落空（全部落在失败通道/未绑 DBC 通道）→ EMPTY_RESULT（§5 第 9 条）。
    if plan.is_empty() {
        return Err(CoreError::empty_result(crate::empty_selection_message(
            &request.selected,
            &request.dbc_by_channel,
            &dbc_errors,
        )));
    }

    // 每文件的头部墙钟：absolute 需要它原样格式化（与 convert 同一条路径）。
    let timings: Vec<FileTiming> = request
        .blf_paths
        .iter()
        .map(|path| read_file_timing(path))
        .collect();
    if request.time_format == TimeFormat::Absolute {
        if let Some(missing) = timings.iter().find(|timing| timing.wall.is_none()) {
            return Err(CoreError::new(
                BAD_TIME_HEADER,
                format!(
                    "absolute 时间口径需要 BLF 文件头墙钟，但 {} 的文件头无效",
                    missing.path.display()
                ),
            ));
        }
    }

    // 第一阶段：全局时间范围（只读时间戳，不累积数据）。
    let scan = scan_time_range(&request.blf_paths, &timings, request.time_format, &mut errors);

    // 一帧都没有（文件全空/全失败）：返回空结果，ok 取决于是否记录到文件错误。
    if !scan.raw_min.is_finite() || !scan.raw_max.is_finite() {
        return Ok(SeriesOutput {
            ok: errors.is_empty(),
            t_min: 0.0,
            t_max: 0.0,
            t_min_str: None,
            t_max_str: None,
            t_axis: Vec::new(),
            buckets: request.buckets,
            series: Vec::new(),
            dbc_errors: dbc_error_reasons,
            warnings,
            errors,
        });
    }

    let span = scan.raw_max - scan.raw_min;
    // 返回的 t 口径：unix_s 用 epoch 秒，其余用相对秒（absolute 也只返回相对秒）。
    let (grid_min, t_min, t_max) = match request.time_format {
        TimeFormat::UnixS => (scan.raw_min, scan.raw_min, scan.raw_max),
        TimeFormat::RelativeS | TimeFormat::RelativeMs | TimeFormat::Absolute => {
            (0.0, 0.0, span)
        }
    };

    // 第二阶段：解码 + 入桶 + 全量统计。
    let series = decode_and_bucket(
        &request.blf_paths,
        &scan.failed,
        &plan,
        request.time_format,
        scan.raw_min,
        grid_min,
        span,
        request.buckets,
        &mut errors,
        &mut warnings,
    );

    // absolute：给出绝对时间端点与刻度（其他口径为空）。
    let (t_min_str, t_max_str, t_axis) = if request.time_format == TimeFormat::Absolute {
        let origin = scan.abs_min;
        let t_min_str = origin.map(format_wall_clock_micros);
        let t_max_str = scan.abs_max.map(format_wall_clock_micros);
        let axis = match origin {
            Some(origin) => build_axis(t_min, t_max, origin),
            None => Vec::new(),
        };
        (t_min_str, t_max_str, axis)
    } else {
        (None, None, Vec::new())
    };

    if series.is_empty() {
        warnings.push("所选信号在输入 BLF 中没有任何采样点".to_string());
    }

    Ok(SeriesOutput {
        ok: errors.is_empty(),
        t_min,
        t_max,
        t_min_str,
        t_max_str,
        t_axis,
        buckets: request.buckets,
        series,
        dbc_errors: dbc_error_reasons,
        warnings,
        errors,
    })
}

// ---------------------------------------------------------------------------
// 单元测试（不依赖 Python；纯算法 + 合成 BLF 端到端）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// 用已知采样点喂一个累加器。
    fn accumulate(t_min: f64, t_max: f64, buckets: usize, samples: &[(f64, f64)]) -> SignalAccumulator {
        let mut acc = SignalAccumulator::new(buckets, t_min, t_max - t_min);
        for &(t, v) in samples {
            acc.push(t, v);
        }
        acc
    }

    /// T1：`t_max` 处的最后一个点必须落进最后一桶，不得越界/被丢。
    #[test]
    fn t_max_lands_in_last_bucket() {
        let acc = accumulate(0.0, 10.0, 100, &[(10.0, 42.0)]);
        let out = acc.finish();
        assert_eq!(out.t.len(), 1, "只有一个采样点，应只有一个非空桶");
        // 最后一桶中心 = 0 + (99 + 0.5) * 0.1 = 9.95。
        assert!((out.t[0] - 9.95).abs() < 1e-12, "桶中心={}", out.t[0]);
        assert_eq!(out.max[0], 42.0);
        // 浮点边界 + 略微越界都必须 clamp 到最后一桶。
        assert_eq!(bucket_index(10.0, 0.0, 10.0, 100), 99);
        assert_eq!(bucket_index(10.0 + 1e-9, 0.0, 10.0, 100), 99);
        assert_eq!(bucket_index(0.0, 0.0, 10.0, 100), 0);
        // 0 跨度时全部落桶 0，不 panic、不越界。
        assert_eq!(bucket_index(5.0, 5.0, 0.0, 100), 0);
    }

    /// T2：稀疏信号的空桶整体跳过，三数组长度 = 非空桶数，t 与真实有值桶中心一致。
    #[test]
    fn empty_buckets_are_skipped() {
        let samples = [(0.1, 1.0), (2.5, 2.0), (9.9, 3.0)];
        let acc = accumulate(0.0, 10.0, 10, &samples);
        let out = acc.finish();
        assert_eq!(out.t.len(), 3, "只有 3 个桶有采样");
        assert_eq!(out.min.len(), 3);
        assert_eq!(out.max.len(), 3);
        assert_eq!(out.mean.len(), 3);
        // 桶宽 1：t=0.1→桶0(中心0.5)、t=2.5→桶2(中心2.5)、t=9.9→桶9(中心9.5)。
        assert_eq!(out.t, vec![0.5, 2.5, 9.5]);
        assert_eq!(out.min, vec![1.0, 2.0, 3.0]);
        assert_eq!(out.max, vec![1.0, 2.0, 3.0]);
        assert_eq!(out.mean, vec![1.0, 2.0, 3.0]);
    }

    /// T3（红线）：桶内只有一个极端尖峰时，该桶 `max` 必须就是尖峰——min/max 包络不丢峰。
    #[test]
    fn min_max_envelope_keeps_spike() {
        let mut samples: Vec<(f64, f64)> = (0..100)
            .map(|i| (0.001 * i as f64, 1.0))
            .collect();
        // 同一个桶（桶0）里塞入一个远高于其余点的尖峰。
        samples.push((0.005, 999.0));
        // 另一个桶里的极端负峰。
        samples.push((5.5, -777.0));

        let acc = accumulate(0.0, 10.0, 10, &samples);
        let out = acc.finish();
        assert_eq!(out.max[0], 999.0, "桶0 的 max 必须保留尖峰 999，实际 {}", out.max[0]);
        assert_eq!(out.min[0], 1.0);
        assert_eq!(out.t[1], 5.5);
        assert_eq!(out.min[1], -777.0, "桶5 的 min 必须保留负峰 -777");
        assert_eq!(out.max[1], -777.0);
    }

    /// T4：`count`/`vmin`/`vmax`/`first`/`last` 必须来自全量原始数据（不是抽稀后）。
    #[test]
    fn stats_come_from_all_raw_samples() {
        let samples = [(9.0, 5.0), (0.0, -3.0), (5.0, 100.0), (1.0, 7.0)];
        let acc = accumulate(0.0, 10.0, 10, &samples);
        assert_eq!(acc.count, 4);
        assert_eq!(acc.vmin, -3.0);
        assert_eq!(acc.vmax, 100.0);
        assert_eq!(acc.first, -3.0, "first 是 t=0.0 处的值");
        assert_eq!(acc.last, 5.0, "last 是 t=9.0 处的值");
        // 抽稀后的桶统计不应污染全量统计：vmin 落在桶0、vmax 落在桶5。
        // 非空桶索引依次为 0、1、5、9，故 100.0 在输出数组的下标 2。
        let out = acc.finish();
        assert_eq!(out.t, vec![0.5, 1.5, 5.5, 9.5]);
        assert_eq!(out.min[0], -3.0);
        assert_eq!(out.max[0], -3.0);
        assert_eq!(out.max[2], 100.0);
    }

    /// T5：`buckets` 参数边界；越界报 `BAD_CONFIG`。
    #[test]
    fn bucket_parameter_bounds() {
        assert!(validate_buckets(100).is_ok());
        assert!(validate_buckets(2000).is_ok());
        assert!(validate_buckets(20000).is_ok());
        for bad in [0usize, 1, 99, 20001, usize::MAX] {
            let err = validate_buckets(bad).expect_err("越界 bucket 必须报错");
            assert_eq!(err.code, BAD_CONFIG, "bad={bad}");
        }

        // 桶数大于采样点数：只输出有值的桶，且不超过 buckets。
        let samples: Vec<(f64, f64)> = (0..5).map(|i| (i as f64, i as f64)).collect();
        for buckets in [100usize, 20000] {
            let out = accumulate(0.0, 10.0, buckets, &samples).finish();
            assert!(out.t.len() <= 5, "5 个采样点最多 5 个非空桶");
            assert!(out.t.len() <= buckets);
            assert_eq!(out.t.len(), out.min.len());
            assert_eq!(out.t.len(), out.max.len());
            assert_eq!(out.t.len(), out.mean.len());
        }

        // buckets=1（越界）时必须被 validate_buckets 拦下，而不是走到分桶逻辑。
        assert_eq!(
            validate_buckets(1).expect_err("buckets=1 越界").code,
            BAD_CONFIG
        );

        // 完整入口 `series_for_plot` 也必须在任何文件访问之前拒绝越界 buckets。
        for bad in [99usize, 20001] {
            let request = SeriesRequest {
                blf_paths: Vec::new(),
                dbc_by_channel: BTreeMap::new(),
                selected: Selection::default(),
                time_format: TimeFormat::RelativeS,
                buckets: bad,
            };
            let err = series_for_plot(&request).expect_err("越界 buckets 必须报错");
            assert_eq!(err.code, BAD_CONFIG, "buckets={bad}");
        }

        // blf_paths 为空 → 空 series 且 ok=true（不报错）。
        let empty = series_for_plot(&SeriesRequest {
            blf_paths: Vec::new(),
            dbc_by_channel: BTreeMap::new(),
            selected: Selection::default(),
            time_format: TimeFormat::RelativeS,
            buckets: 2000,
        })
        .expect("空 blf_paths 不应报错");
        assert!(empty.ok);
        assert!(empty.series.is_empty());
        assert_eq!(empty.buckets, 2000);
        assert!(empty.t_axis.is_empty());
    }

    /// T6：四种 `time_format` 下返回的 `t` 关系。
    #[test]
    fn time_formats_all_return_seconds() {
        let raw = 1_760_623_800.5_f64;
        let raw_min = 1_760_623_800.123_456_f64;
        let rel_s = to_plot_seconds(raw, raw_min, TimeFormat::RelativeS);
        let rel_ms = to_plot_seconds(raw, raw_min, TimeFormat::RelativeMs);
        let abs = to_plot_seconds(raw, raw_min, TimeFormat::Absolute);
        let unix = to_plot_seconds(raw, raw_min, TimeFormat::UnixS);

        // epoch 量级 f64 相减会带 ~1e-9 s 噪声，容忍到微秒级。
        assert!((rel_s - 0.376_544).abs() < 1e-6, "relative_s={rel_s}");
        // relative_ms 也必须返回**秒**：与 relative_s 完全一致；绝不能是它的 1000 倍。
        assert_eq!(rel_s.to_bits(), rel_ms.to_bits());
        assert_ne!(rel_ms.to_bits(), (rel_s * 1000.0).to_bits());
        // absolute 的 t 也是相对秒。
        assert_eq!(rel_s.to_bits(), abs.to_bits());
        // unix_s 是 epoch 秒。
        assert_eq!(unix.to_bits(), raw.to_bits());
    }

    // ---- 合成 BLF 端到端：验证四种 time_format 与 absolute 刻度 ----

    const TEST_DBC: &str = "VERSION \"\"\n\nBO_ 256 TestMsg: 8 Vector__XXX\n SG_ Val : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n";

    fn write_test_blf(dir: &Path, name: &str, start: f64) -> PathBuf {
        use blf_asc::{BlfWriter, Message};
        let path = dir.join(name);
        let mut writer = BlfWriter::create(&path).expect("创建合成 BLF");
        for i in 0..101u32 {
            let mut msg = Message::default();
            msg.timestamp = start + i as f64 * 0.01;
            msg.arbitration_id = 256u32.into();
            msg.channel = 0;
            msg.dlc = 8;
            let mut data = [0u8; 8];
            data[0] = (i & 0xFF) as u8;
            msg.data = data.into();
            writer.on_message_received(&msg).expect("写入帧");
        }
        writer.finish().expect("完成 BLF");
        path
    }

    fn request_for(blf: &Path, dbc: &Path, format: TimeFormat) -> SeriesRequest {
        let mut dbc_map = BTreeMap::new();
        dbc_map.insert(1, dbc.to_path_buf());
        SeriesRequest {
            blf_paths: vec![blf.to_path_buf()],
            dbc_by_channel: dbc_map,
            selected: Selection::default(),
            time_format: format,
            buckets: 200,
        }
    }

    #[test]
    fn synthetic_blf_time_formats_and_axis() {
        let dir = tempfile::tempdir().expect("临时目录");
        let dbc = dir.path().join("t.dbc");
        fs::write(&dbc, TEST_DBC).expect("写 DBC");
        let start = 1_760_623_800.123_456_f64;
        let blf = write_test_blf(dir.path(), "t.blf", start);

        // relative_s：t 从 0 开始，t_max = 1.0（101 帧 × 10ms）。
        let out_rel = series_for_plot(&request_for(&blf, &dbc, TimeFormat::RelativeS))
            .expect("relative_s 应成功");
        assert_eq!(out_rel.series.len(), 1);
        let s = &out_rel.series[0];
        assert_eq!(s.count, 101);
        assert!((out_rel.t_min - 0.0).abs() < 1e-12);
        assert!((out_rel.t_max - 1.0).abs() < 1e-9, "t_max={}", out_rel.t_max);
        // `t` 是桶中心：桶0 中心 = t_min + 0.5 * (span / buckets) = 0.0025。
        assert!((s.t[0] - 0.0025).abs() < 1e-12, "第一个桶中心={}", s.t[0]);
        assert!(out_rel.t_axis.is_empty(), "非 absolute 不返回刻度");

        // relative_ms：t 仍是秒，与 relative_s 完全一致。
        let out_ms = series_for_plot(&request_for(&blf, &dbc, TimeFormat::RelativeMs))
            .expect("relative_ms 应成功");
        assert_eq!(out_ms.series[0].t.len(), s.t.len());
        for (a, b) in out_ms.series[0].t.iter().zip(s.t.iter()) {
            assert_eq!(a.to_bits(), b.to_bits(), "relative_ms 也必须返回秒");
        }

        // unix_s：t 是 epoch 秒。
        let out_unix = series_for_plot(&request_for(&blf, &dbc, TimeFormat::UnixS))
            .expect("unix_s 应成功");
        assert!((out_unix.t_min - start).abs() < 1e-6);
        // 桶0 中心 = t_min + 0.5 * span / buckets。
        assert!((out_unix.series[0].t[0] - (start + 0.0025)).abs() < 1e-6);

        // absolute：t 是相对秒，且给出绝对字符串与 8..12 个刻度。
        let out_abs = series_for_plot(&request_for(&blf, &dbc, TimeFormat::Absolute))
            .expect("absolute 应成功");
        assert!((out_abs.t_min - 0.0).abs() < 1e-12);
        // absolute 的 t 也是桶中心（与 relative_s 同网格）。
        assert!((out_abs.series[0].t[0] - 0.0025).abs() < 1e-12);
        let t_min_str = out_abs.t_min_str.clone().expect("absolute 必给 t_min_str");
        assert_eq!(t_min_str, "2025-10-16 14:10:00.123456");
        assert_eq!(
            out_abs.t_max_str.as_deref(),
            Some("2025-10-16 14:10:01.123456")
        );
        assert!(
            (8..=12).contains(&out_abs.t_axis.len()),
            "刻度数应在 8..12，实际 {}",
            out_abs.t_axis.len()
        );
        // 刻度 t 等间隔、从 0 到 t_max；label 可复现（跨度 1s < 2s → 微秒级）。
        assert!((out_abs.t_axis.first().map(|x| x.t).unwrap_or(-1.0) - 0.0).abs() < 1e-12);
        assert!(
            (out_abs.t_axis.last().map(|x| x.t).unwrap_or(-1.0) - 1.0).abs() < 1e-9
        );
        assert_eq!(out_abs.t_axis[0].label, "2025-10-16 14:10:00.123456");
        assert_eq!(out_abs.t_axis[10 - 1].label, "2025-10-16 14:10:01.123456");
    }
}
