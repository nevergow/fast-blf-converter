//! 合并与导出（Step 2b / 2c）：Parquet 分片 → CSV / Parquet / Excel。
//!
//! 本模块负责：
//! 1. 把 [`TableShards`] 登记的分片按 `timestamp` 升序合并、按时间戳去重；
//! 2. 拆表 / 宽表两种模式，CSV / Parquet / XLSX 三种落盘格式；
//! 3. `resample_hz = Some(hz)` 时的等间隔重采样（内部线性插值 + 端点 clamp）；
//! 4. Excel 防呆（超行数上限整体降级 CSV）与多 Sheet 写出。
//!
//! 关键约定（契约见 `docs/INTERFACE.md` §1.1 / §1.2 / §5）：
//! * 输出列顺序固定 `timestamp` 在最左，其后严格按 DBC 信号出现顺序；
//! * 拆表文件名 `Merged_Ch{ch}_{MsgName}.csv|.parquet`，子表名 `Ch{ch}_{MsgName}`；
//! * 宽表 `Merged_All_Signals`：保持原始时序时按 `timestamp` 做 outer join
//!   （Polars 0.55.2 没有 `JoinType::Outer`，等价于 `JoinType::Full` +
//!   `JoinCoalesce::CoalesceColumns`）；重采样时各表落到同一全局网格后横向拼接；
//! * **同名信号消歧对两条分支共用同一套规则**（见 [`plan_wide_names`]）：某信号名
//!   出现在多于一个报文里 → 该信号在**每个**报文里都改名为 `{MsgName}_{Signal}` 并逐条
//!   给出 warning；只出现在单一报文的信号保持原名。因此宽表恒有且仅有一个 `timestamp`
//!   列（所有信号共用第一列）。拆表模式不受影响（子表内信号保留原名）。
//! * xlsx 拆表写**单个** `Merged_Report.xlsx`，每个 `(channel, message)` 一个 Sheet；
//!   xlsx 宽表写单个 `Merged_All_Signals.xlsx` 单 Sheet。Sheet 名只保留
//!   `[A-Za-z0-9_]`、截断到 31 字符、同 workbook 内去重（`_2` / `_3` 后缀）。
//! * Excel 防呆：`export_format == "xlsx"` 且任一表行数 > `excel_row_limit` →
//!   整体降级 CSV，`downgraded = true` + 人话原因，不崩溃、不截断。
//!
//! # 为什么宽表改名走 lazy `select(alias)` 而不是 `DataFrame::rename_many`
//!
//! 实测 Polars 0.55.2：对 `DataFrame` 调用 `rename_many` 之后再 `.lazy().join(...)`，
//! 投影下推（projection pushdown）会拿着**改名前的列映射**去找**改名后的列**，直接
//! `unwrap()` 成 `ColumnNotFound` panic（`projection_pushdown/mod.rs`）。改用
//! `df.lazy().select([col(src).alias(dst), ..])` 把改名表达成投影节点即可绕开该缺陷，
//! 同时 [`export`] 外层还有 [`catch_export_panic`] 兜底，任何漏网 panic 都会变成
//! `EXPORT_INTERNAL` 结构化错误，绝不把 `PanicException` 抛给 Python。
//!
//! # 重采样为什么不用 Polars asof join
//!
//! `docs/DEPS.md` §3 已实测 Polars 0.55.2 **没有** `LazyFrame::join_asof`，只能手写
//! `join(...).how(JoinType::AsOf(AsOfOptions { .. }))`，其字段集不稳定、需反复试编译。
//! 即使编译通过，本任务要求的语义（区间内部线性插值 + 首末点 clamp + 行数公式
//! `floor((t1-t0)*hz)+1` + 保证零空值）仍要在 asof 之后追加插值与填充逻辑，控制点分散。
//! 因此这里选择**纯 Rust 网格重采样**：`resample_linear` 是无 I/O、无状态、可单元测试
//! 的纯函数，边界语义（命中样本、两点插值、前/后 clamp、单样本、空输入）全部用向量
//! 测试锁死（见文件末尾 `#[cfg(test)]`）。`resample_hz` 对外的行为契约与任务书一致。

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::DateTime;
use polars::prelude::*;
use rust_xlsxwriter::{Workbook, Worksheet, XlsxError};

use crate::decoder::{FileTiming, Progress, ShardRef, TableKey, TableShards, BAD_TIME_HEADER};
use crate::error::{CoreError, EXPORT_INTERNAL};

/// 重采样网格点数的硬上限（约 50 Hz 下 111 小时）。
///
/// 超过该值一律返回结构化 `BAD_CONFIG`，绝不把 `usize` 分配交给 allocator abort。
pub const MAX_GRID_POINTS: usize = 20_000_000;

/// 宽表重采样流式写出的行块大小：每块只持有 `CHUNK_ROWS × S` 个单元格。
pub const CHUNK_ROWS: usize = 20_000;

/// 预算闸门里每个网格单元的保守字节数（f64 列 + 临时值 / 位图余量）。
const EST_BYTES_PER_CELL: u64 = 16;

/// 实际落盘格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Parquet,
    Xlsx,
}

impl ExportFormat {
    /// 契约里的字符串表示（`docs/INTERFACE.md` §1.2）。
    pub fn as_str(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Parquet => "parquet",
            ExportFormat::Xlsx => "xlsx",
        }
    }
}

/// 填充策略（`docs/INTERFACE.md` §1.1 的 `fill_policy`）。
///
/// * [`FillPolicy::None`]：不填充，原样留空；
/// * [`FillPolicy::Hold`]：阶梯保持（ZOH）——把最近一次**真实采样**保持到下一拍，
///   绝不产生原始采样集合之外的值；
/// * [`FillPolicy::Linear`]：线性插值——会在两个真实采样点之间造出从未采样过的值。
///
/// `lib.rs` 负责结合路径（拆表 / 宽表原时序 / 宽表网格）把用户请求解析成这里的
/// **最终生效策略**，`exporter` 只按它执行，绝不再回读原始 config。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillPolicy {
    None,
    Hold,
    Linear,
}

impl FillPolicy {
    /// 三个合法取值（错误消息里按此顺序列出）。
    pub const VALID: [&'static str; 3] = ["none", "hold", "linear"];

    /// 解析 config 字符串；非法值返回 `None`（由调用方转 `BAD_CONFIG`）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "none" => Some(FillPolicy::None),
            "hold" => Some(FillPolicy::Hold),
            "linear" => Some(FillPolicy::Linear),
            _ => None,
        }
    }

    /// 合法值拼接串，供错误消息使用。
    pub fn valid_values() -> String {
        Self::VALID.join(" / ")
    }
}

/// `timestamp` 列的四种口径（`docs/INTERFACE.md` §1.1 的 `time_format`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeFormat {
    /// `msg.timestamp - global_min`，单位秒（f64）。
    RelativeS,
    /// `(msg.timestamp - global_min) * 1000`，单位毫秒（f64）。
    RelativeMs,
    /// `msg.timestamp` 原样（f64，epoch 秒）。
    UnixS,
    /// 该帧所属文件头墙钟 + 偏移，格式 `YYYY-MM-DD HH:MM:SS.ffffff`（Utf8）。
    Absolute,
}

impl TimeFormat {
    /// 四个合法取值（错误消息里按此顺序列出）。
    pub const VALID: [&'static str; 4] =
        ["relative_s", "relative_ms", "absolute", "unix_s"];

    /// 解析 config 字符串；非法值返回 `None`（由调用方转 `BAD_CONFIG`）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "relative_s" => Some(TimeFormat::RelativeS),
            "relative_ms" => Some(TimeFormat::RelativeMs),
            "unix_s" => Some(TimeFormat::UnixS),
            "absolute" => Some(TimeFormat::Absolute),
            _ => None,
        }
    }

    /// 合法值拼接串，供错误消息使用。
    pub fn valid_values() -> String {
        Self::VALID.join(" / ")
    }
}

/// 导出参数。
#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub split_by_message: bool,
    pub resample_hz: Option<f64>,
    pub format: ExportFormat,
    pub out_dir: PathBuf,
    pub excel_row_limit: u64,
    /// `lib.rs` 结合路径解析出的**最终生效**填充策略（见 [`FillPolicy`]）。
    pub fill_policy: FillPolicy,
    /// 仅 `hold` 生效：向前保持超过该毫秒数仍未收到新采样时，该点置空。
    /// `None` = 不限制。
    pub ffill_max_gap_ms: Option<f64>,
    /// `timestamp` 列口径；缺省 `relative_s`。
    pub time_format: TimeFormat,
    /// 本次任务全部输入文件全部帧的最小 `msg.timestamp`（解码期原子统计）。
    pub global_min: f64,
    /// 每个输入文件的头部时间基准（`absolute` 需要其中的墙钟）。
    pub file_timings: Vec<FileTiming>,
}

/// 单张输出表的结果。
#[derive(Debug, Clone)]
pub struct TableResult {
    pub name: String,
    pub channel: Option<u32>,
    pub message: Option<String>,
    pub rows: u64,
    pub path: String,
    pub sheet: Option<String>,
}

/// 导出汇总。
#[derive(Debug, Clone)]
pub struct ExportReport {
    pub format_used: ExportFormat,
    pub downgraded: bool,
    pub downgrade_reason: Option<String>,
    pub total_rows: u64,
    pub tables: Vec<TableResult>,
    pub warnings: Vec<String>,
}

/// 导出运行期参数（`memory_budget_mb` / `log_path` / 取消标志）。
///
/// 这些参数不放进 [`ExportOptions`]，以保持既有 Rust 调用方（含集成测试）的
/// 结构体字面量兼容；[`export`] 继续使用默认值，`lib.rs` 的 `convert` 走
/// [`export_with_runtime`] 把 config 里的值透传进来。
#[derive(Debug, Clone)]
pub struct ExportRuntime {
    /// phase 2 的内存预算（MB），语义与 phase 1 的 `memory_budget_mb` 一致。
    pub memory_budget_mb: u64,
    /// 给出时，phase 2 预检 / 错误会追加到该文件；`None` 完全不写日志。
    pub log_path: Option<PathBuf>,
    /// 用户取消标志（由 `ProgressThrottle` 在回调抛取消异常时置位）。
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for ExportRuntime {
    fn default() -> Self {
        Self {
            memory_budget_mb: 1536,
            log_path: None,
            cancel: None,
        }
    }
}

impl ExportRuntime {
    /// 取消标志是否已置位。
    fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    /// 追加一行 phase 2 日志；`log_path = None` 时是空操作。
    fn log(&self, message: &str) {
        if let Some(path) = &self.log_path {
            crate::append_log_line(path, message);
        }
    }
}

/// 取出取消检查的公共错误文案。
fn cancelled_error() -> CoreError {
    CoreError::export_failed("已取消：用户在导出阶段取消了任务")
}

/// `timestamp` 列改写器：把分片里的原始 epoch 秒按 `time_format` 口径换算。
///
/// * `relative_s` / `relative_ms` / `unix_s` 只依赖全局最小时间戳，作用在 lazy 表达式上；
/// * `absolute` 依赖每帧所属文件的墙钟，因此按**分片**取 `file` 对应的 [`FileTiming`]，
///   用 i64 微秒整数运算避免浮点漂移。
struct TimeTransform {
    format: TimeFormat,
    global_min: f64,
    timings: BTreeMap<PathBuf, FileTiming>,
}

impl TimeTransform {
    fn new(opts: &ExportOptions) -> Result<Self, CoreError> {
        if opts.time_format == TimeFormat::Absolute {
            // 任一输入文件的墙钟无效都必须点名报错，绝不静默降级。
            if let Some(missing) = opts.file_timings.iter().find(|timing| timing.wall.is_none()) {
                return Err(CoreError::new(
                    BAD_TIME_HEADER,
                    format!(
                        "absolute 时间口径需要 BLF 文件头墙钟，但 {} 的文件头无效",
                        missing.path.display()
                    ),
                ));
            }
        }
        let timings = opts
            .file_timings
            .iter()
            .map(|timing| (timing.path.clone(), timing.clone()))
            .collect();
        Ok(Self {
            format: opts.time_format,
            global_min: opts.global_min,
            timings,
        })
    }

    /// 在 lazy 分片上改写 `timestamp` 列（列名不变、仍是第一列）。
    fn apply(&self, lf: LazyFrame, shard: &ShardRef) -> Result<LazyFrame, CoreError> {
        let expr = match self.format {
            TimeFormat::UnixS => return Ok(lf),
            TimeFormat::RelativeS => {
                (col("timestamp") - lit(self.global_min)).alias("timestamp")
            }
            TimeFormat::RelativeMs => {
                ((col("timestamp") - lit(self.global_min)) * lit(1000.0)).alias("timestamp")
            }
            TimeFormat::Absolute => {
                let timing = self.timings.get(&shard.file).ok_or_else(|| {
                    CoreError::new(
                        BAD_TIME_HEADER,
                        format!("找不到 {} 的头部时间信息", shard.file.display()),
                    )
                })?;
                let wall = timing.wall.as_ref().ok_or_else(|| {
                    CoreError::new(
                        BAD_TIME_HEADER,
                        format!(
                            "absolute 时间口径需要 BLF 文件头墙钟，但 {} 的文件头无效",
                            shard.file.display()
                        ),
                    )
                })?;
                let wall_micros = wall.epoch_micros().ok_or_else(|| {
                    CoreError::new(
                        BAD_TIME_HEADER,
                        format!("{} 的文件头墙钟无法换算成 epoch 微秒", shard.file.display()),
                    )
                })?;
                // 先算与文件头的小偏移（微秒），再加绝对墙钟微秒：
                // `wall_us + round((raw - header_epoch) * 1e6)`，避免 epoch 级浮点漂移。
                ((col("timestamp") - lit(timing.header_epoch)) * lit(1_000_000.0)
                    + lit(wall_micros as f64))
                .alias("timestamp")
            }
        };
        Ok(lf.with_columns([expr]))
    }

    /// 落盘前的最后一步（在合并 / 去重 / 重采样**之后**，只作用于输出列）：
    ///
    /// * `relative_s` / `relative_ms` 按 **1 微秒** 取整，消掉 epoch 级 f64 减法噪声
    ///   （`docs/INTERFACE.md` §1.1 的精度条款），使 `0.01 s` / `10 ms` 干净落盘；
    /// * `absolute` 把数值微秒列格式化成墙钟字符串；
    /// * `unix_s` 原样返回（绝对 epoch 秒不取整）。
    ///
    /// 这里的取整**不会**回流到 [`merge_table`] / [`resample_linear`] 使用的内部时间戳，
    /// 因此不改变合并、去重与重采样语义。
    fn finalize(&self, df: DataFrame) -> Result<DataFrame, CoreError> {
        match self.format {
            TimeFormat::UnixS => Ok(df),
            // relative_s：1e-6 s（微秒）。
            TimeFormat::RelativeS => self.round_timestamp(df, 1_000_000.0),
            // relative_ms：1e-3 ms == 1 微秒。
            TimeFormat::RelativeMs => self.round_timestamp(df, 1_000.0),
            TimeFormat::Absolute => {
                let column = df
                    .column("timestamp")
                    .map_err(|err| CoreError::export_failed(format!("缺少 timestamp 列：{err}")))?;
                let micros = column
                    .f64()
                    .map_err(|err| CoreError::export_failed(format!("timestamp 不是数字列：{err}")))?;
                let values: Vec<Option<String>> = micros
                    .iter()
                    .map(|value| value.map(|us| format_wall_clock_micros(us.round() as i64)))
                    .collect();
                let mut df = df;
                df.replace("timestamp", Column::from(Series::new("timestamp".into(), values)))
                    .map_err(|err| CoreError::export_failed(format!("改写 timestamp 列失败：{err}")))?;
                Ok(df)
            }
        }
    }

    /// 把 `timestamp` 列按 `scale` 取整到微秒：`(v * scale).round() / scale`。
    ///
    /// * 使用四舍五入（`f64::round`，half away from zero）；
    /// * `round` 是单调函数，已升序的时间列取整后仍**单调不减**，不会把相邻帧顺序弄反；
    /// * 只改写输出列，合并 / 去重 / 重采样用的内部时间戳仍是未取整值。
    fn round_timestamp(&self, df: DataFrame, scale: f64) -> Result<DataFrame, CoreError> {
        let column = df
            .column("timestamp")
            .map_err(|err| CoreError::export_failed(format!("缺少 timestamp 列：{err}")))?;
        let values = column
            .f64()
            .map_err(|err| CoreError::export_failed(format!("timestamp 不是数字列：{err}")))?;
        let rounded: Vec<Option<f64>> = values
            .iter()
            .map(|value| value.map(|v| (v * scale).round() / scale))
            .collect();
        let mut df = df;
        df.replace("timestamp", Column::from(Series::new("timestamp".into(), rounded)))
            .map_err(|err| CoreError::export_failed(format!("改写 timestamp 列失败：{err}")))?;
        Ok(df)
    }
}

/// 把 epoch 微秒整数格式化为 `%Y-%m-%d %H:%M:%S.%6f`（**UTC，原样墙钟，不做时区换算**）。
fn format_wall_clock_micros(micros: i64) -> String {
    let seconds = micros.div_euclid(1_000_000);
    let sub_micros = micros.rem_euclid(1_000_000);
    match DateTime::from_timestamp(seconds, (sub_micros * 1000) as u32) {
        Some(dt) => format!(
            "{}.{:06}",
            dt.format("%Y-%m-%d %H:%M:%S"),
            sub_micros
        ),
        None => String::new(),
    }
}

/// 导出期间的「当前阶段 / 当前表」记录，供 panic 兜底时定位问题现场。
///
/// `stage` 取 `merge` / `export` 等回调阶段名；`table` 记录最近一次进度回调里的表名
/// （如 `Ch1_VehSpd` / `Merged_All_Signals`）。
#[derive(Debug, Clone, Default)]
pub struct ExportPanicContext {
    pub stage: String,
    pub table: String,
}

impl ExportPanicContext {
    /// 用进度回调更新现场：阶段名 + 表名（进度 message 已含表名）。
    fn record(&mut self, progress: &Progress) {
        if !progress.stage.is_empty() {
            self.stage = progress.stage.clone();
        }
        if !progress.message.is_empty() {
            self.table = progress.message.clone();
        }
    }
}

/// 把 `f` 内部逃逸的 panic 收编为 [`CoreError`]（错误码 `EXPORT_INTERNAL`），
/// 绝不把 `PanicException` 抛给 Python。
///
/// `ctx` 里保存着 panic 发生前最近的阶段 / 表名；`f` 正常返回的 `Result` 原样透传。
/// 公开此函数是为了让集成测试可以直接注入必然 panic 的闭包、验证兜底路径。
pub fn catch_export_panic<T>(
    ctx: &Mutex<ExportPanicContext>,
    f: impl FnOnce() -> Result<T, CoreError>,
) -> Result<T, CoreError> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let snapshot = ctx.lock().map(|guard| guard.clone()).unwrap_or_default();
            Err(CoreError::new(
                EXPORT_INTERNAL,
                panic_context_message(&snapshot.stage, &snapshot.table, payload.as_ref()),
            ))
        }
    }
}

/// 把 panic 现场拼成一句话：阶段名 + 表名 + 截断后的 panic 字符串。
fn panic_context_message(stage: &str, table: &str, payload: &(dyn Any + Send)) -> String {
    let raw = if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "(非字符串 panic payload)".to_string()
    };
    // 截断到合理长度（按字符数，避免切坏 UTF-8）。
    const MAX_CHARS: usize = 500;
    let mut detail = raw;
    if detail.chars().count() > MAX_CHARS {
        detail = detail.chars().take(MAX_CHARS).collect::<String>() + "…（已截断）";
    }
    let stage = if stage.is_empty() { "export" } else { stage };
    let table = if table.is_empty() { "(未记录表名)" } else { table };
    format!("导出内部错误（阶段 {stage}，表 {table}）：{detail}")
}

/// 合并并导出所有表。
///
/// 整个导出阶段（合并 + 重采样 + 写文件）都跑在 [`catch_export_panic`] 里：任何逃逸的
/// panic 都会转成 `EXPORT_INTERNAL` 结构化错误，而不是让 Python 侧看到 `PanicException`。
pub fn export(
    tables: &BTreeMap<TableKey, TableShards>,
    opts: &ExportOptions,
    progress: &dyn Fn(Progress),
) -> Result<ExportReport, CoreError> {
    export_with_runtime(tables, opts, progress, &ExportRuntime::default())
}

/// 同 [`export`]，但显式给出 phase 2 的内存预算 / 日志路径 / 取消标志。
pub fn export_with_runtime(
    tables: &BTreeMap<TableKey, TableShards>,
    opts: &ExportOptions,
    progress: &dyn Fn(Progress),
    runtime: &ExportRuntime,
) -> Result<ExportReport, CoreError> {
    let ctx = Mutex::new(ExportPanicContext::default());
    // 用一层进度包装记录 panic 现场；异常照旧透传给调用方。
    let recording = |event: Progress| {
        if let Ok(mut guard) = ctx.lock() {
            guard.record(&event);
        }
        progress(event);
    };
    let result = catch_export_panic(&ctx, || export_inner(tables, opts, &recording, runtime));
    if let Err(err) = &result {
        runtime.log(&format!("export phase2 ERROR {}: {}", err.code, err.message));
    }
    result
}

/// 导出主流程（不含 panic 兜底，便于把整段调用栈放进同一个 `catch_unwind`）。
fn export_inner(
    tables: &BTreeMap<TableKey, TableShards>,
    opts: &ExportOptions,
    progress: &dyn Fn(Progress),
    runtime: &ExportRuntime,
) -> Result<ExportReport, CoreError> {
    if tables.is_empty() {
        return Err(CoreError::empty_result("没有任何命中解码计划的报文，无可导出内容"));
    }

    // 参数校验先于任何落盘动作：非法 hz 直接 BAD_CONFIG，绝不静默忽略。
    if let Some(hz) = opts.resample_hz {
        if !hz.is_finite() || hz <= 0.0 {
            return Err(CoreError::bad_config(format!(
                "resample_hz 必须是有限正数，收到 {hz}"
            )));
        }
    }

    std::fs::create_dir_all(&opts.out_dir).map_err(|err| {
        CoreError::export_failed(format!(
            "创建输出目录失败：{}（{err}）",
            opts.out_dir.display()
        ))
    })?;

    // 时间口径改写器：absolute 的头部有效性在这里统一校验，绝不静默降级。
    let transform = TimeTransform::new(opts)?;

    // Excel 防呆：重采样宽表的合并后行数是网格点数 G（而不是去重前的分片行数）。
    // G 的统计只走 lazy min/max，不 materialize 任何网格；算不出来时按超限处理，
    // 先降级到 CSV，再由 phase 2 的预算 / 上限闸门给出结构化错误。
    let max_rows = if !opts.split_by_message && opts.resample_hz.is_some() {
        match resampled_grid_points(tables, opts, &transform) {
            Ok(points) => points,
            Err(_) => u64::MAX,
        }
    } else {
        tables.values().map(|table| table.rows).max().unwrap_or(0)
    };
    let mut format_used = opts.format;
    let mut downgraded = false;
    let mut downgrade_reason = None;
    if opts.format == ExportFormat::Xlsx && max_rows > opts.excel_row_limit {
        downgraded = true;
        format_used = ExportFormat::Csv;
        downgrade_reason = Some(format!(
            "检测到数据量超过 Excel 百万行上限（当前 {max_rows} 行），\
             为防止文件损坏无法打开，已自动切换为高速 CSV 格式导出"
        ));
    }

    let mut report = ExportReport {
        format_used,
        downgraded,
        downgrade_reason,
        total_rows: 0,
        tables: Vec::new(),
        warnings: Vec::new(),
    };

    // 保持原始时序的宽表本来就会因周期不同产生空值，如实提示（已填充 / 重采样后不再是问题）。
    if !opts.split_by_message && opts.resample_hz.is_none() && opts.fill_policy == FillPolicy::None {
        report
            .warnings
            .push("未重采样：宽表会因周期不同产生空值".to_string());
    }

    match (opts.split_by_message, format_used) {
        (true, ExportFormat::Xlsx) => {
            export_split_xlsx(tables, opts, &transform, progress, &mut report)?
        }
        (true, _) => {
            export_split_tabular(tables, format_used, opts, &transform, progress, &mut report)?
        }
        (false, ExportFormat::Xlsx) => {
            export_wide_xlsx(tables, opts, &transform, progress, &mut report, runtime)?
        }
        (false, _) => export_wide_tabular(
            tables, format_used, opts, &transform, progress, &mut report, runtime,
        )?,
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// 单表准备：合并 + 可选重采样
// ---------------------------------------------------------------------------

/// 拆表模式：合并单张表，并按需在其自身时间轴上重采样。
fn prepare_split_table(
    shards: &TableShards,
    resample_hz: Option<f64>,
    transform: &TimeTransform,
) -> Result<DataFrame, CoreError> {
    let df = merge_table(&shards.shards, transform)?;
    match resample_hz {
        None => Ok(df),
        Some(hz) => {
            let (t0, t1) = frame_time_bounds(&df)?;
            let grid = resample_grid(t0, t1, hz)?;
            resample_frame(&df, &grid)
        }
    }
}

/// 宽表准备：无重采样走 outer join（按 `fill_policy` 可选 hold/linear）；有重采样走全局
/// 网格重采样（hold/linear 见 [`prepare_wide_resampled`]）。
///
/// 两条分支共用同一份 [`WideNames`] 列名规划，保证宽表列名（含冲突消歧）完全一致。
fn prepare_wide(
    tables: &BTreeMap<TableKey, TableShards>,
    opts: &ExportOptions,
    transform: &TimeTransform,
    progress: &dyn Fn(Progress),
    runtime: &ExportRuntime,
) -> Result<(DataFrame, Vec<String>), CoreError> {
    let names = plan_wide_names(tables);
    match opts.resample_hz {
        Some(hz) => prepare_wide_resampled(
            tables,
            hz,
            transform,
            &names,
            progress,
            opts.fill_policy,
            hold_gap_seconds(opts),
            runtime,
        ),
        None => {
            let wide = merge_wide(tables, transform, &names, progress)?;
            let mut warnings = names.warnings.clone();
            let wide = match opts.fill_policy {
                FillPolicy::None => wide,
                FillPolicy::Hold => {
                    let (filled, gap_counts) = forward_fill_frame(&wide, hold_gap_seconds(opts))?;
                    let labels = wide_column_labels(tables, &names);
                    let labeled: Vec<(String, u64)> = gap_counts
                        .iter()
                        .map(|(column, count)| {
                            let label = labels
                                .get(column)
                                .cloned()
                                .unwrap_or_else(|| column.clone());
                            (label, *count)
                        })
                        .collect();
                    warnings.extend(format_gap_warnings(&labeled, opts.ffill_max_gap_ms));
                    filled
                }
                FillPolicy::Linear => {
                    let (filled, count) = linear_fill_frame(&wide)?;
                    warnings.push(linear_fill_warning(count));
                    filled
                }
            };
            Ok((wide, warnings))
        }
    }
}

/// `hold` 的最大保持时长（秒）；非 `hold` 策略一律返回 `None`（契约：仅 hold 生效）。
fn hold_gap_seconds(opts: &ExportOptions) -> Option<f64> {
    if opts.fill_policy == FillPolicy::Hold {
        opts.ffill_max_gap_ms.map(|ms| ms / 1000.0)
    } else {
        None
    }
}

/// phase 2 宽表重采样的预检结果（日志与预算闸门共用）。
#[derive(Debug, Clone, Copy)]
struct ResamplePreflight {
    t0: f64,
    t1: f64,
    points: u64,
    signals: u64,
}

/// 统计宽表重采样网格点数 G 与总信号列数 S，并在**任何大分配之前**做预算闸门。
///
/// * `chunked = true`（CSV/Parquet 流式路径）时，估算的是实际峰值：
///   `min(G, CHUNK_ROWS) × S × 16`（当前行块的值 / 临时 / 位图）+ `G × 8`
///   （时间戳与临时文件索引的保守余量）；
/// * `chunked = false`（xlsx 仍一次性 materialize）时，估算整表 `G × S × 16`。
///
/// 日志行先于闸门写出：即使随后返回 `BAD_CONFIG`，磁盘上也有可复盘的预检记录。
fn resample_preflight(
    tables: &BTreeMap<TableKey, TableShards>,
    hz: f64,
    transform: &TimeTransform,
    runtime: &ExportRuntime,
    chunked: bool,
) -> Result<ResamplePreflight, CoreError> {
    let (t0, t1) = wide_time_bounds(tables, transform)?;
    let points = grid_point_count(t0, t1, hz)?;
    let signals: u64 = tables
        .values()
        .map(|table| table.signal_names.len() as u64)
        .sum();
    let chunk_cells = if chunked {
        points.min(CHUNK_ROWS as u64)
    } else {
        points
    };
    let est_bytes = chunk_cells
        .saturating_mul(signals)
        .saturating_mul(EST_BYTES_PER_CELL)
        .saturating_add(if chunked {
            points.saturating_mul(std::mem::size_of::<f64>() as u64)
        } else {
            0
        });
    let budget_bytes = runtime.memory_budget_mb.saturating_mul(1024 * 1024);
    let est_mb = est_bytes as f64 / (1024.0 * 1024.0);
    let budget_mb = runtime.memory_budget_mb;

    runtime.log(&format!(
        "export phase2 wide-resample t0={t0} t1={t1} hz={hz} G={points} S={signals} \
         est_mb={est_mb:.1} budget_mb={budget_mb} chunk_rows={CHUNK_ROWS}"
    ));

    if est_bytes > budget_bytes {
        let duration_s = t1 - t0;
        let ratio = budget_bytes as f64 / est_bytes.max(1) as f64;
        let suggest_hz = (hz * ratio).max(0.1);
        return Err(CoreError::bad_config(format!(
            "重采样内存预算不足：t0={t0} t1={t1} duration_s={duration_s} hz={hz} \
             G={points} S={signals} est_mb={est_mb:.1} budget_mb={budget_mb} \
             chunk_rows={CHUNK_ROWS}；建议把重采样频率降到 {suggest_hz:.1} Hz 或更低，\
             或改用拆表模式（split_by_message=true）"
        )));
    }

    Ok(ResamplePreflight {
        t0,
        t1,
        points,
        signals,
    })
}

/// 全局时间区间：所有表 lazy min/max 的并集（不 materialize 网格）。
fn wide_time_bounds(
    tables: &BTreeMap<TableKey, TableShards>,
    transform: &TimeTransform,
) -> Result<(f64, f64), CoreError> {
    let mut t0 = f64::INFINITY;
    let mut t1 = f64::NEG_INFINITY;
    for shards in tables.values() {
        let (lo, hi) = table_time_bounds(&shards.shards, transform)?;
        if lo < t0 {
            t0 = lo;
        }
        if hi > t1 {
            t1 = hi;
        }
    }
    if !t0.is_finite() || !t1.is_finite() {
        return Err(CoreError::empty_result("宽表没有任何时间戳，无法重采样"));
    }
    Ok((t0, t1))
}

/// 宽表重采样合并后的行数 G；无法统计时返回 `None`（供 Excel 防呆降级）。
fn resampled_grid_points(
    tables: &BTreeMap<TableKey, TableShards>,
    opts: &ExportOptions,
    transform: &TimeTransform,
) -> Result<u64, CoreError> {
    let hz = opts
        .resample_hz
        .ok_or_else(|| CoreError::export_internal("未重采样时不应统计网格点数"))?;
    let (t0, t1) = wide_time_bounds(tables, transform)?;
    grid_point_count(t0, t1, hz)
}

/// xlsx 专用的一次性宽表重采样：全局网格 = 所有表时间戳区间的并集，步长 `1/hz`。
///
/// CSV/Parquet 走 [`export_wide_resampled_tabular`] 的流式分块路径；这里只服务
/// 仍需要整张 DataFrame 的 xlsx（并在预算闸门里按整表 `G × S × 16` 估算）。
fn prepare_wide_resampled(
    tables: &BTreeMap<TableKey, TableShards>,
    hz: f64,
    transform: &TimeTransform,
    names: &WideNames,
    progress: &dyn Fn(Progress),
    fill_policy: FillPolicy,
    max_gap_s: Option<f64>,
    runtime: &ExportRuntime,
) -> Result<(DataFrame, Vec<String>), CoreError> {
    let total = tables.len() as u64;

    // xlsx 仍一次性 materialize，因此这里的预检按整表估算。
    let preflight = resample_preflight(tables, hz, transform, runtime, false)?;
    let grid = resample_grid(preflight.t0, preflight.t1, hz)?;

    let mut columns: Vec<Column> = Vec::new();
    let mut warnings: Vec<String> = names.warnings.clone();
    let mut linear_signals = 0u64;
    let mut gap_counts: Vec<(String, u64)> = Vec::new();

    for (idx, (key, shards)) in tables.iter().enumerate() {
        if runtime.is_cancelled() {
            return Err(cancelled_error());
        }
        progress(Progress {
            phase: 2,
            stage: "merge".to_string(),
            done: idx as u64,
            total,
            file: None,
            frames_done: 0,
            message: format!("重采样合并 {}", shards.name),
        });

        let df = merge_table(&shards.shards, transform)?;
        let times = f64_column(&df, "timestamp")?;
        let finals = names.per_table.get(key).ok_or_else(|| {
            CoreError::export_internal(format!("宽表列名规划缺少表 {}", shards.name))
        })?;
        for (signal, final_name) in shards.signal_names.iter().zip(finals.iter()) {
            let values = f64_column(&df, signal)?;
            match fill_policy {
                FillPolicy::Linear => {
                    let resampled = resample_linear(&times, &values, &grid);
                    columns.push(Column::from(Series::from_vec(
                        final_name.as_str().into(),
                        resampled,
                    )));
                    linear_signals += 1;
                }
                // `none` 在网格路径上无意义：由 lib.rs 归一成 `hold`，这里也按 hold 兜底。
                FillPolicy::None | FillPolicy::Hold => {
                    let (resampled, nulled) =
                        resample_hold(&times, &values, &grid, max_gap_s);
                    if nulled > 0 {
                        gap_counts.push((
                            format!("Ch{}/{}（信号 {}）", key.channel, key.message, signal),
                            nulled,
                        ));
                    }
                    columns.push(Column::from(Series::new(
                        final_name.as_str().into(),
                        resampled,
                    )));
                }
            }
        }
    }

    if linear_signals > 0 {
        warnings.push(linear_fill_warning(linear_signals));
    }
    warnings.extend(format_gap_warnings(
        &gap_counts,
        max_gap_s.map(|gap| gap * 1000.0),
    ));

    let wide = DataFrame::new_infer_height({
        columns.insert(
            0,
            Column::from(Series::from_vec("timestamp".into(), grid)),
        );
        columns
    })
    .map_err(|err| CoreError::export_failed(format!("组装重采样宽表失败：{err}")))?;
    Ok((wide, warnings))
}

// ---------------------------------------------------------------------------
// 拆表导出
// ---------------------------------------------------------------------------

/// 拆表 + CSV/Parquet：每张表一个文件。
fn export_split_tabular(
    tables: &BTreeMap<TableKey, TableShards>,
    format: ExportFormat,
    opts: &ExportOptions,
    transform: &TimeTransform,
    progress: &dyn Fn(Progress),
    report: &mut ExportReport,
) -> Result<(), CoreError> {
    let total = tables.len() as u64;
    for (idx, (key, shards)) in tables.iter().enumerate() {
        progress(Progress {
            phase: 2,
            stage: "merge".to_string(),
            done: idx as u64,
            total,
            file: None,
            frames_done: 0,
            message: format!("合并 {}", shards.name),
        });
        let mut df = transform.finalize(prepare_split_table(shards, opts.resample_hz, transform)?)?;
        let rows = df.height() as u64;

        progress(Progress {
            phase: 2,
            stage: "export".to_string(),
            done: idx as u64,
            total,
            file: None,
            frames_done: 0,
            message: format!("导出 {}", shards.name),
        });
        let stem = format!("Merged_{}", shards.name);
        let path = write_table(&mut df, &stem, format, &opts.out_dir)?;

        report.total_rows += rows;
        report.tables.push(TableResult {
            name: shards.name.clone(),
            channel: Some(key.channel),
            message: Some(key.message.clone()),
            rows,
            path: path.to_string_lossy().to_string(),
            sheet: None,
        });
    }
    Ok(())
}

/// 拆表 + xlsx：单个 `Merged_Report.xlsx`，每张表一个 Sheet（常量内存模式）。
fn export_split_xlsx(
    tables: &BTreeMap<TableKey, TableShards>,
    opts: &ExportOptions,
    transform: &TimeTransform,
    progress: &dyn Fn(Progress),
    report: &mut ExportReport,
) -> Result<(), CoreError> {
    let total = tables.len() as u64;
    let path = opts.out_dir.join("Merged_Report.xlsx");
    let path_text = path.to_string_lossy().to_string();
    let mut workbook = Workbook::new();
    let mut used_names: BTreeSet<String> = BTreeSet::new();

    for (idx, (key, shards)) in tables.iter().enumerate() {
        progress(Progress {
            phase: 2,
            stage: "merge".to_string(),
            done: idx as u64,
            total,
            file: None,
            frames_done: 0,
            message: format!("合并 {}", shards.name),
        });
        let df = transform.finalize(prepare_split_table(shards, opts.resample_hz, transform)?)?;
        let rows = df.height() as u64;
        if rows == 0 {
            report
                .warnings
                .push(format!("子表 {} 没有任何数据行，已跳过", shards.name));
            continue;
        }

        progress(Progress {
            phase: 2,
            stage: "export".to_string(),
            done: idx as u64,
            total,
            file: None,
            frames_done: 0,
            message: format!("导出 {}", shards.name),
        });

        let sheet = normalize_sheet_name(&shards.name, &mut used_names);
        let worksheet = workbook.add_worksheet_with_constant_memory();
        worksheet.set_name(&sheet).map_err(|err| {
            CoreError::export_failed(format!("设置 Excel Sheet 名 `{sheet}` 失败：{err}"))
        })?;
        write_dataframe_to_worksheet(worksheet, &df)?;

        report.total_rows += rows;
        report.tables.push(TableResult {
            name: shards.name.clone(),
            channel: Some(key.channel),
            message: Some(key.message.clone()),
            rows,
            path: path_text.clone(),
            sheet: Some(sheet),
        });
    }

    if report.tables.is_empty() {
        return Err(CoreError::empty_result("所有子表均为空，无可导出内容"));
    }
    workbook
        .save(&path)
        .map_err(|err| CoreError::export_failed(format!("保存 Excel 失败：{}（{err}）", path.display())))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 宽表导出
// ---------------------------------------------------------------------------

/// 宽表 + CSV/Parquet：单文件 `Merged_All_Signals`。
fn export_wide_tabular(
    tables: &BTreeMap<TableKey, TableShards>,
    format: ExportFormat,
    opts: &ExportOptions,
    transform: &TimeTransform,
    progress: &dyn Fn(Progress),
    report: &mut ExportReport,
    runtime: &ExportRuntime,
) -> Result<(), CoreError> {
    // 重采样宽表走真正的流式分块路径：峰值只与 CHUNK_ROWS × S 有关。
    if opts.resample_hz.is_some() {
        return export_wide_resampled_tabular(
            tables, format, opts, transform, progress, report, runtime,
        );
    }

    let (wide, warnings) = prepare_wide(tables, opts, transform, progress, runtime)?;
    report.warnings.extend(warnings);
    let mut wide = transform.finalize(wide)?;

    progress(Progress {
        phase: 2,
        stage: "export".to_string(),
        done: 0,
        total: 1,
        file: None,
        frames_done: 0,
        message: "导出 Merged_All_Signals".to_string(),
    });
    let rows = wide.height() as u64;
    let path = write_table(&mut wide, "Merged_All_Signals", format, &opts.out_dir)?;
    report.total_rows = rows;
    report.tables.push(TableResult {
        name: "Merged_All_Signals".to_string(),
        channel: None,
        message: None,
        rows,
        path: path.to_string_lossy().to_string(),
        sheet: None,
    });
    Ok(())
}

/// 临时值文件的 RAII 清理器（成功 / 失败都删）。
struct TempValueFile {
    path: PathBuf,
}

impl Drop for TempValueFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 把一段 f64 按本机字节序写入（同一进程内写读，无需转码）。
fn write_f64_slice(file: &mut File, values: &[f64]) -> Result<usize, CoreError> {
    // SAFETY: `f64` 是无 padding 的 POD；`u8` 对齐要求为 1，按字节视图读取合法。
    let bytes =
        unsafe { std::slice::from_raw_parts(values.as_ptr() as *const u8, values.len() * 8) };
    file.write_all(bytes)
        .map_err(|err| CoreError::export_failed(format!("写入重采样临时值文件失败：{err}")))?;
    Ok(values.len() * 8)
}

/// 从临时值文件的 `offset` 处读取 `len` 个 f64。
fn read_f64_slice(file: &mut File, offset: u64, len: usize) -> Result<Vec<f64>, CoreError> {
    let mut values = vec![0.0_f64; len];
    // SAFETY: 同上，已初始化的 f64 缓冲区按字节视图填充是合法的。
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(values.as_mut_ptr() as *mut u8, values.len() * 8)
    };
    file.seek(SeekFrom::Start(offset))
        .map_err(|err| CoreError::export_failed(format!("定位重采样临时值文件失败：{err}")))?;
    file.read_exact(bytes)
        .map_err(|err| CoreError::export_failed(format!("读取重采样临时值文件失败：{err}")))?;
    Ok(values)
}

/// 宽表 + 重采样 + CSV/Parquet 的流式分块导出。
///
/// 峰值内存 ≈ `max(单表 merged, CHUNK_ROWS × S × 常数)`：
/// 逐表 merge 后在**该表自己的信号列上**按 `CHUNK_ROWS` 行块重采样，把每个
/// `(信号, 块)` 的 f64 值与 null 掩码追加进临时文件；最后按块读回 S 列、拼一个
/// `CHUNK_ROWS` 行的 DataFrame 并增量写出。任何时刻都不持有 G 长度的中间数组，
/// 也不需要把全部表的时间戳/信号值同时留在内存里。
fn export_wide_resampled_tabular(
    tables: &BTreeMap<TableKey, TableShards>,
    format: ExportFormat,
    opts: &ExportOptions,
    transform: &TimeTransform,
    progress: &dyn Fn(Progress),
    report: &mut ExportReport,
    runtime: &ExportRuntime,
) -> Result<(), CoreError> {
    let hz = opts
        .resample_hz
        .ok_or_else(|| CoreError::export_internal("流式重采样路径缺少 resample_hz"))?;
    let names = plan_wide_names(tables);
    let preflight = resample_preflight(tables, hz, transform, runtime, true)?;
    let t0 = preflight.t0;
    let points = preflight.points as usize;
    let step = 1.0 / hz;
    let max_gap_s = hold_gap_seconds(opts);
    let chunk_count = points.div_ceil(CHUNK_ROWS);
    let chunk_len = |chunk: usize| (points - chunk * CHUNK_ROWS).min(CHUNK_ROWS);

    // 全局列计划：表按 BTreeMap 顺序，信号按 DBC 顺序；与旧实现逐项相同。
    let mut global_finals: Vec<String> = Vec::with_capacity(preflight.signals as usize);
    let mut column_labels: Vec<String> = Vec::with_capacity(preflight.signals as usize);
    let mut bases: BTreeMap<TableKey, usize> = BTreeMap::new();
    for (key, shards) in tables.iter() {
        let finals = names.per_table.get(key).ok_or_else(|| {
            CoreError::export_internal(format!("宽表列名规划缺少表 {}", shards.name))
        })?;
        bases.insert(key.clone(), global_finals.len());
        global_finals.extend(finals.iter().cloned());
        column_labels.extend(shards.signal_names.iter().map(|signal| {
            format!("Ch{}/{}（信号 {}）", key.channel, key.message, signal)
        }));
    }
    let total_signals = global_finals.len();
    if total_signals != preflight.signals as usize {
        return Err(CoreError::export_internal(format!(
            "宽表列数不一致：计划 {total_signals}，预检 {}",
            preflight.signals
        )));
    }

    let (ext, effective) = match format {
        ExportFormat::Parquet => ("parquet", ExportFormat::Parquet),
        _ => ("csv", ExportFormat::Csv),
    };
    let out_path = opts.out_dir.join(format!("Merged_All_Signals.{ext}"));

    // 两个临时文件：重采样后的 f64 值与 1 B/格的 null 掩码；RAII 保证退出时清理。
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let value_path = opts.out_dir.join(format!(".fblf_resample_values_{pid}_{unique}.f64"));
    let mask_path = opts.out_dir.join(format!(".fblf_resample_masks_{pid}_{unique}.u8"));
    let _value_guard = TempValueFile { path: value_path.clone() };
    let _mask_guard = TempValueFile { path: mask_path.clone() };
    let mut value_writer = File::create(&value_path).map_err(|err| {
        CoreError::export_failed(format!(
            "创建重采样临时值文件失败：{}（{err}）",
            value_path.display()
        ))
    })?;
    let mut mask_writer = File::create(&mask_path).map_err(|err| {
        CoreError::export_failed(format!(
            "创建重采样临时掩码文件失败：{}（{err}）",
            mask_path.display()
        ))
    })?;
    let mut value_offsets = vec![0u64; total_signals * chunk_count];
    let mut mask_offsets = vec![0u64; total_signals * chunk_count];
    let mut next_value_offset = 0u64;
    let mut next_mask_offset = 0u64;

    // ---------------- 第一遍：逐表 merge，逐 (信号, 行块) 重采样落临时文件 ----------------
    let total_tables = tables.len() as u64;
    for (idx, (key, shards)) in tables.iter().enumerate() {
        if runtime.is_cancelled() {
            return Err(cancelled_error());
        }
        progress(Progress {
            phase: 2,
            stage: "merge".to_string(),
            done: idx as u64,
            total: total_tables,
            file: None,
            frames_done: 0,
            message: format!("重采样合并 {}", shards.name),
        });

        let df = merge_table(&shards.shards, transform)?;
        let times = f64_column(&df, "timestamp")?;
        let base = bases.get(key).copied().ok_or_else(|| {
            CoreError::export_internal(format!("宽表列名规划缺少表 {}", shards.name))
        })?;

        for (signal_idx, signal) in shards.signal_names.iter().enumerate() {
            let values = f64_column(&df, signal)?;
            let column = base + signal_idx;
            for chunk in 0..chunk_count {
                if runtime.is_cancelled() {
                    return Err(cancelled_error());
                }
                let start = chunk * CHUNK_ROWS;
                let len = chunk_len(chunk);
                let chunk_grid: Vec<f64> = (0..len)
                    .map(|i| t0 + (start + i) as f64 * step)
                    .collect();
                let first_point = chunk_grid[0];
                let last_point = chunk_grid[len - 1];
                // 只取「块首前驱」到「块末后继」的样本切片；切片内的重采样结果
                // 与整张 G 网格逐点相同（含 null 位置）。
                let lo = times
                    .partition_point(|&timestamp| timestamp <= first_point)
                    .saturating_sub(1);
                let hi = times
                    .partition_point(|&timestamp| timestamp <= last_point)
                    .saturating_add(1)
                    .min(times.len());
                let times_slice = &times[lo..hi];
                let values_slice = &values[lo..hi];

                let (resampled, nulls) = match opts.fill_policy {
                    FillPolicy::Linear => {
                        let resampled =
                            resample_linear(times_slice, values_slice, &chunk_grid);
                        (resampled, vec![false; len])
                    }
                    // `none` 在网格路径上无意义：由 lib.rs 归一成 `hold`，这里也按 hold 兜底。
                    FillPolicy::None | FillPolicy::Hold => {
                        let (resampled, nulls, _nulled) = resample_hold_masked(
                            times_slice,
                            values_slice,
                            &chunk_grid,
                            max_gap_s,
                        );
                        (resampled, nulls)
                    }
                };

                let slot = column * chunk_count + chunk;
                value_offsets[slot] = next_value_offset;
                mask_offsets[slot] = next_mask_offset;
                next_value_offset += write_f64_slice(&mut value_writer, &resampled)? as u64;
                let mask_bytes: Vec<u8> =
                    nulls.iter().map(|is_null| u8::from(*is_null)).collect();
                mask_writer.write_all(&mask_bytes).map_err(|err| {
                    CoreError::export_failed(format!("写入重采样临时掩码文件失败：{err}"))
                })?;
                next_mask_offset += mask_bytes.len() as u64;
            }
        }
    }
    value_writer
        .flush()
        .map_err(|err| CoreError::export_failed(format!("刷新重采样临时值文件失败：{err}")))?;
    mask_writer
        .flush()
        .map_err(|err| CoreError::export_failed(format!("刷新重采样临时掩码文件失败：{err}")))?;
    drop(value_writer);
    drop(mask_writer);
    let mut value_reader = File::open(&value_path).map_err(|err| {
        CoreError::export_failed(format!(
            "打开重采样临时值文件失败：{}（{err}）",
            value_path.display()
        ))
    })?;
    let mut mask_reader = File::open(&mask_path).map_err(|err| {
        CoreError::export_failed(format!(
            "打开重采样临时掩码文件失败：{}（{err}）",
            mask_path.display()
        ))
    })?;

    // ---------------- 第二遍：按行块读回 S 列、拼 DataFrame、增量写盘 ----------------
    let mut gap_totals = vec![0u64; total_signals];
    let mut csv_file: Option<File> = None;
    let mut parquet = None;
    let mut first_chunk = true;

    for chunk in 0..chunk_count {
        if runtime.is_cancelled() {
            return Err(cancelled_error());
        }
        let start = chunk * CHUNK_ROWS;
        let len = chunk_len(chunk);
        let chunk_grid: Vec<f64> = (0..len)
            .map(|i| t0 + (start + i) as f64 * step)
            .collect();

        progress(Progress {
            phase: 2,
            stage: "export".to_string(),
            done: chunk as u64,
            total: chunk_count as u64,
            file: None,
            frames_done: 0,
            message: "导出 Merged_All_Signals".to_string(),
        });

        let mut columns: Vec<Column> = Vec::with_capacity(1 + total_signals);
        for column in 0..total_signals {
            let slot = column * chunk_count + chunk;
            let values = read_f64_slice(&mut value_reader, value_offsets[slot], len)?;
            let final_name = &global_finals[column];
            match opts.fill_policy {
                FillPolicy::Linear => {
                    columns.push(Column::from(Series::from_vec(
                        final_name.as_str().into(),
                        values,
                    )));
                }
                FillPolicy::None | FillPolicy::Hold => {
                    let mut mask_bytes = vec![0u8; len];
                    mask_reader
                        .seek(SeekFrom::Start(mask_offsets[slot]))
                        .map_err(|err| {
                            CoreError::export_failed(format!(
                                "定位重采样临时掩码文件失败：{err}"
                            ))
                        })?;
                    mask_reader.read_exact(&mut mask_bytes).map_err(|err| {
                        CoreError::export_failed(format!(
                            "读取重采样临时掩码文件失败：{err}"
                        ))
                    })?;
                    let mut nulled = 0u64;
                    let ca: Float64Chunked = values
                        .iter()
                        .zip(mask_bytes.iter())
                        .map(|(value, is_null)| {
                            if *is_null == 0 {
                                Some(*value)
                            } else {
                                nulled += 1;
                                None
                            }
                        })
                        .collect();
                    gap_totals[column] += nulled;
                    columns.push(Column::from(Series::new(
                        final_name.as_str().into(),
                        ca,
                    )));
                }
            }
        }

        columns.insert(
            0,
            Column::from(Series::from_vec("timestamp".into(), chunk_grid)),
        );
        let chunk_df = DataFrame::new_infer_height(columns).map_err(|err| {
            CoreError::export_failed(format!("组装第 {} 个重采样行块失败：{err}", chunk + 1))
        })?;
        let mut chunk_df = transform.finalize(chunk_df)?;

        match effective {
            ExportFormat::Parquet => {
                if first_chunk {
                    let file = File::create(&out_path).map_err(|err| {
                        CoreError::export_failed(format!(
                            "创建 Parquet 失败：{}（{err}）",
                            out_path.display()
                        ))
                    })?;
                    let schema = chunk_df.schema().clone();
                    let batched = ParquetWriter::new(file)
                        .with_compression(ParquetCompression::Zstd(Some(Default::default())))
                        .batched(&schema)
                        .map_err(|err| {
                            CoreError::export_failed(format!(
                                "创建 Parquet 分块写入器失败：{err}"
                            ))
                        })?;
                    parquet = Some(batched);
                }
                parquet
                    .as_mut()
                    .ok_or_else(|| CoreError::export_internal("Parquet 写入器未初始化"))?
                    .write_batch(&chunk_df)
                    .map_err(|err| {
                        CoreError::export_failed(format!(
                            "写入 Parquet 行块失败：{}（{err}）",
                            out_path.display()
                        ))
                    })?;
            }
            _ => {
                if first_chunk {
                    csv_file = Some(File::create(&out_path).map_err(|err| {
                        CoreError::export_failed(format!(
                            "创建 CSV 失败：{}（{err}）",
                            out_path.display()
                        ))
                    })?);
                }
                let file = csv_file
                    .as_mut()
                    .ok_or_else(|| CoreError::export_internal("CSV 写入器未初始化"))?;
                // 首块写表头，后续块只追加数据行；逐字节等价于一次性写出。
                write_csv_chunk(file, &mut chunk_df, first_chunk, &out_path)?;
            }
        }

        first_chunk = false;
    }

    if let Some(batched) = parquet {
        batched.finish().map_err(|err| {
            CoreError::export_failed(format!(
                "收尾 Parquet 失败：{}（{err}）",
                out_path.display()
            ))
        })?;
    }

    // ---------------- 汇总 warnings / report ----------------
    let mut warnings = names.warnings.clone();
    if opts.fill_policy == FillPolicy::Linear {
        warnings.push(linear_fill_warning(preflight.signals));
    } else {
        let gap_counts: Vec<(String, u64)> = gap_totals
            .iter()
            .enumerate()
            .filter_map(|(column, &nulled)| {
                (nulled > 0).then(|| (column_labels[column].clone(), nulled))
            })
            .collect();
        warnings.extend(format_gap_warnings(
            &gap_counts,
            max_gap_s.map(|gap| gap * 1000.0),
        ));
    }
    report.warnings.extend(warnings);

    let rows = points as u64;
    report.total_rows = rows;
    report.tables.push(TableResult {
        name: "Merged_All_Signals".to_string(),
        channel: None,
        message: None,
        rows,
        path: out_path.to_string_lossy().to_string(),
        sheet: None,
    });
    Ok(())
}

/// 宽表 + xlsx：单个 `Merged_All_Signals.xlsx` 单 Sheet（常量内存模式）。
fn export_wide_xlsx(
    tables: &BTreeMap<TableKey, TableShards>,
    opts: &ExportOptions,
    transform: &TimeTransform,
    progress: &dyn Fn(Progress),
    report: &mut ExportReport,
    runtime: &ExportRuntime,
) -> Result<(), CoreError> {
    let (wide, warnings) = prepare_wide(tables, opts, transform, progress, runtime)?;
    report.warnings.extend(warnings);
    let wide = transform.finalize(wide)?;

    if wide.height() == 0 {
        return Err(CoreError::empty_result("宽表没有任何数据行，无可导出内容"));
    }

    progress(Progress {
        phase: 2,
        stage: "export".to_string(),
        done: 0,
        total: 1,
        file: None,
        frames_done: 0,
        message: "导出 Merged_All_Signals.xlsx".to_string(),
    });

    let path = opts.out_dir.join("Merged_All_Signals.xlsx");
    let mut used_names: BTreeSet<String> = BTreeSet::new();
    let sheet = normalize_sheet_name("Merged_All_Signals", &mut used_names);

    let mut workbook = Workbook::new();
    {
        let worksheet = workbook.add_worksheet_with_constant_memory();
        worksheet.set_name(&sheet).map_err(|err| {
            CoreError::export_failed(format!("设置 Excel Sheet 名 `{sheet}` 失败：{err}"))
        })?;
        write_dataframe_to_worksheet(worksheet, &wide)?;
    }
    workbook
        .save(&path)
        .map_err(|err| CoreError::export_failed(format!("保存 Excel 失败：{}（{err}）", path.display())))?;

    report.total_rows = wide.height() as u64;
    report.tables.push(TableResult {
        name: "Merged_All_Signals".to_string(),
        channel: None,
        message: None,
        rows: wide.height() as u64,
        path: path.to_string_lossy().to_string(),
        sheet: Some(sheet),
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// 分片扫描 / 合并
// ---------------------------------------------------------------------------

/// 扫描一张表的所有分片，按口径改写 `timestamp` 后返回拼接后的 LazyFrame。
fn scan_shards(shards: &[ShardRef], transform: &TimeTransform) -> Result<LazyFrame, CoreError> {
    if shards.is_empty() {
        return Err(CoreError::export_failed("表没有任何分片，无法合并"));
    }
    let mut frames: Vec<LazyFrame> = Vec::with_capacity(shards.len());
    for shard in shards {
        let as_str = shard.path.to_string_lossy().to_string();
        let path = PlRefPath::from(as_str.as_str());
        let lf = LazyFrame::scan_parquet(path, Default::default()).map_err(|err| {
            CoreError::export_failed(format!("扫描分片失败：{}（{err}）", shard.path.display()))
        })?;
        frames.push(transform.apply(lf, shard)?);
    }
    if frames.len() == 1 {
        if let Some(single) = frames.pop() {
            return Ok(single);
        }
    }
    concat(frames, Default::default())
        .map_err(|err| CoreError::export_failed(format!("拼接分片失败：{err}")))
}

/// 单张表：scan → 按口径改写 timestamp → 升序 → 去重 → 流式 collect。
fn merge_table(shards: &[ShardRef], transform: &TimeTransform) -> Result<DataFrame, CoreError> {
    scan_shards(shards, transform)?
        .sort_by_exprs([col("timestamp")], SortMultipleOptions::default())
        .unique_stable_generic(Some(vec![col("timestamp")]), UniqueKeepStrategy::First)
        .with_streaming(true)
        .collect()
        .map_err(|err| CoreError::export_failed(format!("合并分片失败：{err}")))
}

/// 只用 lazy `min/max` 取一张表（改写口径后）的时间戳区间。
fn table_time_bounds(
    shards: &[ShardRef],
    transform: &TimeTransform,
) -> Result<(f64, f64), CoreError> {
    let out = scan_shards(shards, transform)?
        .select([
            col("timestamp").min().alias("t_min"),
            col("timestamp").max().alias("t_max"),
        ])
        .collect()
        .map_err(|err| CoreError::export_failed(format!("统计时间戳范围失败：{err}")))?;
    Ok((scalar_f64(&out, "t_min")?, scalar_f64(&out, "t_max")?))
}

/// 已排序、已去重的表直接取首末时间戳。
fn frame_time_bounds(df: &DataFrame) -> Result<(f64, f64), CoreError> {
    if df.height() == 0 {
        return Err(CoreError::empty_result("表没有任何数据行，无法重采样"));
    }
    let times = f64_column(df, "timestamp")?;
    let first = times.first().copied().unwrap_or(f64::NAN);
    let last = times.last().copied().unwrap_or(f64::NAN);
    if !first.is_finite() || !last.is_finite() {
        return Err(CoreError::export_failed("表的时间戳不是有限数，无法重采样"));
    }
    Ok((first, last))
}

/// 从单行聚合结果里取一个 f64 标量。
fn scalar_f64(df: &DataFrame, name: &str) -> Result<f64, CoreError> {
    let column = df
        .column(name)
        .map_err(|err| CoreError::export_failed(format!("缺少列 `{name}`：{err}")))?;
    let ca = column
        .f64()
        .map_err(|err| CoreError::export_failed(format!("列 `{name}` 不是 Float64：{err}")))?;
    ca.get(0)
        .ok_or_else(|| CoreError::empty_result(format!("列 `{name}` 为空，无法确定时间范围")))
}

/// 宽表列名规划结果。
struct WideNames {
    /// 每张表的信号列最终名，顺序与 `TableShards.signal_names` 一一对应。
    per_table: BTreeMap<TableKey, Vec<String>>,
    /// 冲突改名的说明（每个被改名的信号一条）。
    warnings: Vec<String>,
}

/// 为整个宽表统一规划列名（两条分支共用）。
///
/// 规则（与任务书 / `docs/INTERFACE.md` §1.1 一致）：
/// * 某信号名出现在**多于一个报文**里 → 它在每个报文里都改名为 `{报文名}_{信号名}`，
///   并逐条给出 warning；只有单一报文出现的信号保持原名。
/// * `timestamp` 是保留列名，任何信号叫这个名字都会被改名。
/// * 兜底：同一表内重复信号名、或前缀化后仍与已有列名撞车时，补 `_2` / `_3` 后缀。
fn plan_wide_names(tables: &BTreeMap<TableKey, TableShards>) -> WideNames {
    // 统计每个信号名被哪些报文占用（同一报文内重复只算一次）。
    let mut owners: BTreeMap<&str, BTreeSet<&TableKey>> = BTreeMap::new();
    for (key, shards) in tables {
        for signal in &shards.signal_names {
            owners.entry(signal.as_str()).or_default().insert(key);
        }
    }

    // 已被占用的列名；timestamp 是 join key，任何信号都不能叫这个名字。
    let mut used: BTreeSet<String> = BTreeSet::new();
    used.insert("timestamp".to_string());

    let mut warnings: Vec<String> = Vec::new();
    let mut per_table: BTreeMap<TableKey, Vec<String>> = BTreeMap::new();
    for (key, shards) in tables {
        let prefix = crate::decoder::sanitize_identifier(&key.message);
        let mut finals = Vec::with_capacity(shards.signal_names.len());
        for signal in &shards.signal_names {
            let multi = owners.get(signal.as_str()).is_some_and(|set| set.len() > 1);
            let reserved = signal == "timestamp";
            let mut candidate = if multi || reserved {
                let prefixed = format!("{prefix}_{signal}");
                warnings.push(format!(
                    "宽表列名冲突：`{signal}` 同时出现在多个报文中，已重命名为 `{prefixed}`"
                ));
                prefixed
            } else {
                signal.clone()
            };
            let mut serial = 2u32;
            while used.contains(&candidate) {
                candidate = format!("{}_{serial}", signal);
                serial += 1;
            }
            used.insert(candidate.clone());
            finals.push(candidate);
        }
        per_table.insert(key.clone(), finals);
    }
    WideNames { per_table, warnings }
}

/// 宽表合并：逐表 outer join（列名按 [`WideNames`] 用 lazy alias 投影改写）。
///
/// 不用 `DataFrame::rename_many`：实测 Polars 0.55.2 在改名后的 `DataFrame` 上做 lazy
/// join 会触发投影下推 panic（详见文件头注释）。
fn merge_wide(
    tables: &BTreeMap<TableKey, TableShards>,
    transform: &TimeTransform,
    names: &WideNames,
    progress: &dyn Fn(Progress),
) -> Result<DataFrame, CoreError> {
    let total = tables.len() as u64;

    let mut acc: Option<DataFrame> = None;
    for (idx, (key, shards)) in tables.iter().enumerate() {
        progress(Progress {
            phase: 2,
            stage: "merge".to_string(),
            done: idx as u64,
            total,
            file: None,
            frames_done: 0,
            message: format!("合并 {}", shards.name),
        });

        let df = merge_table(&shards.shards, transform)?;
        let finals = names
            .per_table
            .get(key)
            .ok_or_else(|| CoreError::export_internal(format!("宽表列名规划缺少表 {}", shards.name)))?;

        // 用投影别名改名：timestamp 恒在最左，其后按 DBC 信号顺序。
        let mut exprs: Vec<Expr> = Vec::with_capacity(1 + shards.signal_names.len());
        exprs.push(col("timestamp"));
        for (signal, final_name) in shards.signal_names.iter().zip(finals.iter()) {
            exprs.push(col(signal.as_str()).alias(final_name.as_str()));
        }
        let renamed = df.lazy().select(exprs);

        acc = Some(match acc {
            None => renamed
                .collect()
                .map_err(|err| CoreError::export_failed(format!("宽表列名改写失败：{err}")))?,
            Some(left) => left
                .lazy()
                .join(
                    renamed,
                    [col("timestamp")],
                    [col("timestamp")],
                    JoinArgs::new(JoinType::Full).with_coalesce(JoinCoalesce::CoalesceColumns),
                )
                .collect()
                .map_err(|err| CoreError::export_failed(format!("宽表 outer join 失败：{err}")))?,
        });
    }

    let wide = acc.ok_or_else(|| CoreError::empty_result("没有任何可合并的表"))?;
    wide.lazy()
        .sort_by_exprs([col("timestamp")], SortMultipleOptions::default())
        .collect()
        .map_err(|err| CoreError::export_failed(format!("宽表排序失败：{err}")))
}

/// 对宽表所有信号列做**阶梯保持**（ZOH）前向填充，并应用最大保持时长。
///
/// * 行首（第一个真实采样之前）无值可保持 → 一律 `null`，且不计入"超限置空"；
/// * 内部空值：若 `max_gap_s` 为 `Some(gap)` 且 `行时间 - 最近真实采样时间 > gap`
///   → 该行置空并计入返回的按列超限计数，否则保持最近真实采样值；
/// * 输出值**严格取自该列的原始采样值集合**，不做任何插值。
///
/// 返回 `(填充后的 DataFrame, 每列超限置空行数)`。
fn forward_fill_frame(
    df: &DataFrame,
    max_gap_s: Option<f64>,
) -> Result<(DataFrame, Vec<(String, u64)>), CoreError> {
    let times = f64_column(df, "timestamp")?;
    let mut columns: Vec<Column> = Vec::with_capacity(df.width());
    let timestamp = df
        .column("timestamp")
        .map_err(|err| CoreError::export_failed(format!("缺少列 `timestamp`：{err}")))?
        .clone();
    columns.push(timestamp);

    let mut gap_counts: Vec<(String, u64)> = Vec::new();
    for name in df.get_column_names() {
        if name.as_str() == "timestamp" {
            continue;
        }
        let values = f64_column(df, name.as_str())?;
        let mut out: Vec<Option<f64>> = Vec::with_capacity(times.len());
        // 最近一次真实采样：`(时间, 值)`。
        let mut last: Option<(f64, f64)> = None;
        let mut nulled = 0u64;
        for (idx, value) in values.iter().enumerate() {
            if value.is_nan() {
                match last {
                    Some((sample_time, held)) => {
                        let too_old = max_gap_s
                            .map(|gap| times[idx] - sample_time > gap)
                            .unwrap_or(false);
                        if too_old {
                            out.push(None);
                            nulled += 1;
                        } else {
                            out.push(Some(held));
                        }
                    }
                    None => out.push(None),
                }
            } else {
                last = Some((times[idx], *value));
                out.push(Some(*value));
            }
        }
        if nulled > 0 {
            gap_counts.push((name.to_string(), nulled));
        }
        columns.push(Column::from(Series::new(name.as_str().into(), out)));
    }

    let filled = DataFrame::new_infer_height(columns)
        .map_err(|err| CoreError::export_failed(format!("宽表前向填充失败：{err}")))?;
    Ok((filled, gap_counts))
}

/// 在并集时间轴上对宽表每个信号做线性插值（复用 [`resample_linear`] 的插值实现）。
///
/// 返回 `(插值后的 DataFrame, 被插值的信号数)`。行首（第一个真实采样之前）沿用
/// [`resample_linear`] 的 backward clamp 语义，与网格路径保持一致。
fn linear_fill_frame(df: &DataFrame) -> Result<(DataFrame, u64), CoreError> {
    let times = f64_column(df, "timestamp")?;
    let mut columns: Vec<Column> = Vec::with_capacity(df.width());
    let timestamp = df
        .column("timestamp")
        .map_err(|err| CoreError::export_failed(format!("缺少列 `timestamp`：{err}")))?
        .clone();
    columns.push(timestamp);

    let mut signal_count = 0u64;
    for name in df.get_column_names() {
        if name.as_str() == "timestamp" {
            continue;
        }
        let values = f64_column(df, name.as_str())?;
        let mut sample_times: Vec<f64> = Vec::new();
        let mut sample_values: Vec<f64> = Vec::new();
        for (idx, value) in values.iter().enumerate() {
            if !value.is_nan() {
                sample_times.push(times[idx]);
                sample_values.push(*value);
            }
        }
        let out: Vec<Option<f64>> = if sample_times.is_empty() {
            vec![None; times.len()]
        } else {
            resample_linear(&sample_times, &sample_values, &times)
                .into_iter()
                .map(Some)
                .collect()
        };
        signal_count += 1;
        columns.push(Column::from(Series::new(name.as_str().into(), out)));
    }

    let filled = DataFrame::new_infer_height(columns)
        .map_err(|err| CoreError::export_failed(format!("宽表线性插值失败：{err}")))?;
    Ok((filled, signal_count))
}

/// 宽表最终列名 → `Ch{ch}/{msg}（信号 {sig}）` 标签，供填充类 warning 定位到信号。
fn wide_column_labels(
    tables: &BTreeMap<TableKey, TableShards>,
    names: &WideNames,
) -> BTreeMap<String, String> {
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    for (key, shards) in tables {
        let Some(finals) = names.per_table.get(key) else {
            continue;
        };
        for (signal, final_name) in shards.signal_names.iter().zip(finals.iter()) {
            labels.insert(
                final_name.clone(),
                format!("Ch{}/{}（信号 {}）", key.channel, key.message, signal),
            );
        }
    }
    labels
}

/// 线性插值 warning（契约 §5.12）。
fn linear_fill_warning(signals: u64) -> String {
    format!("已对 {signals} 个信号做线性插值（可能包含从未采样过的中间值）")
}

/// 把 `(标签, 超限行数)` 列表格式化为 `ffill_max_gap_ms` 的人话 warning。
fn format_gap_warnings(counts: &[(String, u64)], max_gap_ms: Option<f64>) -> Vec<String> {
    let ms_text = max_gap_ms.map(format_gap_ms).unwrap_or_else(|| "?".to_string());
    counts
        .iter()
        .filter(|(_label, count)| *count > 0)
        .map(|(label, count)| {
            format!("{label}：{count} 行因超过最大保持时长（{ms_text}ms）被置空")
        })
        .collect()
}

/// 毫秒数的人话格式：整数不带 `.0` 尾巴，非整数保留原样。
fn format_gap_ms(ms: f64) -> String {
    if ms.is_finite() && ms.fract() == 0.0 && ms.abs() < i64::MAX as f64 {
        format!("{}", ms as i64)
    } else {
        format!("{ms}")
    }
}

// ---------------------------------------------------------------------------
// 重采样（纯 Rust）
// ---------------------------------------------------------------------------

/// 生成等间隔网格：`[t0, t0+1/hz, ...]`，点数 = `floor((t1-t0)*hz) + 1`。
///
/// 浮点容差：`(t1-t0)*hz` 的理论整数值常因二进制舍入落到 `k - 1e-13`，直接用 `floor`
/// 会少一个点。这里加一个相对 + 绝对双重容差后再取整，保证「取不超过 t1 的最大网格点」
/// 的语义（偏差不超过 ±1 个点，与任务书给定的计算公式一致）。
///
/// 所有中间量都用 `is_finite` / 上限检查，绝不把饱和转换或 `usize` 回绕交给分配器。
fn resample_grid(t0: f64, t1: f64, hz: f64) -> Result<Vec<f64>, CoreError> {
    let points = grid_point_count(t0, t1, hz)? as usize;
    let step = 1.0 / hz;
    Ok((0..points).map(|i| t0 + i as f64 * step).collect())
}

/// 计算网格点数 G（`u64`），`points == 0` / 非法输入 / 溢出 / 超硬上限一律 `BAD_CONFIG`。
fn grid_point_count(t0: f64, t1: f64, hz: f64) -> Result<u64, CoreError> {
    if !t0.is_finite() || !t1.is_finite() || !hz.is_finite() || hz <= 0.0 {
        return Err(CoreError::bad_config(format!(
            "无法生成重采样网格：t0={t0} t1={t1} hz={hz}（要求有限且 hz > 0）"
        )));
    }
    if t1 < t0 {
        return Err(CoreError::bad_config(format!(
            "无法生成重采样网格：t1 < t0（t0={t0} t1={t1}）"
        )));
    }
    let span = t1 - t0;
    if !span.is_finite() {
        return Err(CoreError::bad_config(format!(
            "重采样时间跨度不是有限数：t0={t0} t1={t1}"
        )));
    }
    let scaled = span * hz;
    if !scaled.is_finite() {
        return Err(CoreError::bad_config(format!(
            "重采样网格点数溢出：duration_s={span} hz={hz}"
        )));
    }
    let tolerance = 1e-9_f64.max(scaled.abs() * 1e-12);
    let floored = (scaled + tolerance).floor();
    if !floored.is_finite() || floored < 0.0 {
        return Err(CoreError::bad_config(format!(
            "重采样网格点数非法：duration_s={span} hz={hz} scaled={scaled}"
        )));
    }
    if floored >= MAX_GRID_POINTS as f64 {
        let hours = (MAX_GRID_POINTS as f64 / hz) / 3600.0;
        return Err(CoreError::bad_config(format!(
            "重采样网格点数超过硬上限 {MAX_GRID_POINTS}（约 {hours:.1} 小时 @ {hz} Hz）：\
             t0={t0} t1={t1} duration_s={span} hz={hz}；\
             请降低重采样频率、缩短时间范围，或改用拆表模式（split_by_message=true）"
        )));
    }
    let points = floored as u64 + 1;
    if points == 0 {
        return Err(CoreError::bad_config(format!(
            "重采样网格点数为 0：t0={t0} t1={t1} hz={hz}"
        )));
    }
    Ok(points)
}

/// 纯函数：把 `(times, values)`（按时间升序）对齐到 `grid` 上的线性插值结果。
///
/// 语义（与任务书 §2 逐条对应）：
/// 1. 网格点正好命中样本 → 返回该样本原值；
/// 2. 落在两样本之间 → 按时间比例线性插值；
/// 3. 早于首样本 → 取首样本（backward clamp）；
/// 4. 晚于末样本 → 取末样本（forward clamp）；
/// 5. 只有 1 个样本 → 所有网格点都取该样本；
/// 6. 空输入 → 返回空 `Vec`，绝不 panic。
///
/// 内部用 `partition_point` 二分定位前驱/后继，整体复杂度 `O(|grid| * log |times|)`。
pub fn resample_linear(times: &[f64], values: &[f64], grid: &[f64]) -> Vec<f64> {
    let len = times.len().min(values.len());
    if len == 0 || grid.is_empty() {
        return Vec::new();
    }
    let times = &times[..len];
    let values = &values[..len];

    let mut out = Vec::with_capacity(grid.len());
    for &point in grid {
        // 前 `idx` 个样本的时间 <= point。
        let idx = times.partition_point(|&t| t <= point);
        let value = if idx == 0 {
            // point 早于首样本。
            values[0]
        } else if idx >= len {
            // point 晚于（或等于）末样本。
            values[len - 1]
        } else {
            let (t_left, v_left) = (times[idx - 1], values[idx - 1]);
            let (t_right, v_right) = (times[idx], values[idx]);
            let span = t_right - t_left;
            if span.abs() <= f64::EPSILON {
                // 去重后不应出现重复时间戳；保险起见退回左值。
                v_left
            } else {
                let ratio = (point - t_left) / span;
                v_left + (v_right - v_left) * ratio
            }
        };
        out.push(value);
    }
    out
}

/// 纯函数：把 `(times, values)`（按时间升序）**阶梯保持**到 `grid`。
///
/// 语义：
/// 1. 网格点取「`≤` 该点的最近一个真实采样」——值严格来自 `values`，绝不插值；
/// 2. 早于首样本的网格点 → `None`（行首无值可保持，不得用首值回填）；
/// 3. `max_gap_s = Some(gap)` 时，若 `网格点 - 最近真实采样时间 > gap` → `None`，
///    并计入返回的超限行数；`None` 表示不限制；
/// 4. 空输入 → 全部 `None`，绝不 panic。
///
/// 返回 `(保持结果, 因超过最大保持时长被置空的行数)`。
pub fn resample_hold(
    times: &[f64],
    values: &[f64],
    grid: &[f64],
    max_gap_s: Option<f64>,
) -> (Vec<Option<f64>>, u64) {
    let (resampled, nulls, nulled) = resample_hold_masked(times, values, grid, max_gap_s);
    let out = resampled
        .into_iter()
        .zip(nulls)
        .map(|(value, is_null)| if is_null { None } else { Some(value) })
        .collect();
    (out, nulled)
}

/// [`resample_hold`] 的零拷贝形态：返回 `(值列, null 掩码, 超限置空行数)`。
///
/// 值列是 `Vec<f64>`（8 B/格），null 单独用 `Vec<bool>`（1 B/格）表示，
/// 供流式行块直接构造 nullable `Float64Chunked`，避免整块 `Vec<Option<f64>>`（16 B/格）
/// 与 `Series` 的二次分配同时存在。
pub fn resample_hold_masked(
    times: &[f64],
    values: &[f64],
    grid: &[f64],
    max_gap_s: Option<f64>,
) -> (Vec<f64>, Vec<bool>, u64) {
    let len = times.len().min(values.len());
    if len == 0 || grid.is_empty() {
        return (vec![0.0; grid.len()], vec![true; grid.len()], 0);
    }
    let times = &times[..len];
    let values = &values[..len];

    let mut out: Vec<f64> = vec![0.0; grid.len()];
    let mut nulls: Vec<bool> = vec![false; grid.len()];
    let mut nulled = 0u64;
    for (slot, &point) in grid.iter().enumerate() {
        // 前 `idx` 个样本的时间 <= point。
        let idx = times.partition_point(|&t| t <= point);
        if idx == 0 {
            // point 早于首样本：无值可保持。
            nulls[slot] = true;
            continue;
        }
        let sample_time = times[idx - 1];
        let too_old = max_gap_s
            .map(|gap| point - sample_time > gap)
            .unwrap_or(false);
        if too_old {
            nulls[slot] = true;
            nulled += 1;
        } else {
            out[slot] = values[idx - 1];
        }
    }
    (out, nulls, nulled)
}

/// 把一张表的全部信号列重采样到给定网格，返回 `timestamp = grid` 的新表。
fn resample_frame(df: &DataFrame, grid: &[f64]) -> Result<DataFrame, CoreError> {
    let times = f64_column(df, "timestamp")?;
    let mut columns: Vec<Column> = Vec::with_capacity(df.width());
    columns.push(Column::from(Series::from_vec(
        "timestamp".into(),
        grid.to_vec(),
    )));
    for name in df.get_column_names() {
        if name.as_str() == "timestamp" {
            continue;
        }
        let values = f64_column(df, name.as_str())?;
        let resampled = resample_linear(&times, &values, grid);
        columns.push(Column::from(Series::from_vec(
            name.as_str().into(),
            resampled,
        )));
    }
    DataFrame::new_infer_height(columns)
        .map_err(|err| CoreError::export_failed(format!("组装重采样表失败：{err}")))
}

/// 取一列为 `Vec<f64>`；空值（不应出现）映射为 `NaN`。
fn f64_column(df: &DataFrame, name: &str) -> Result<Vec<f64>, CoreError> {
    let column = df
        .column(name)
        .map_err(|err| CoreError::export_failed(format!("缺少列 `{name}`：{err}")))?;
    let ca = column
        .f64()
        .map_err(|err| CoreError::export_failed(format!("列 `{name}` 不是 Float64：{err}")))?;
    Ok(ca.iter().map(|value| value.unwrap_or(f64::NAN)).collect())
}

// ---------------------------------------------------------------------------
// 落盘：CSV / Parquet
// ---------------------------------------------------------------------------

/// 按格式写单张表；返回落盘路径。
fn write_table(
    df: &mut DataFrame,
    stem: &str,
    format: ExportFormat,
    out_dir: &Path,
) -> Result<PathBuf, CoreError> {
    let (ext, effective) = match format {
        ExportFormat::Parquet => ("parquet", ExportFormat::Parquet),
        // xlsx 已在 `export` 里分流到专用写出函数；这里再兜底一次，绝不写出伪 xlsx。
        ExportFormat::Csv | ExportFormat::Xlsx => ("csv", ExportFormat::Csv),
    };
    let path = out_dir.join(format!("{stem}.{ext}"));
    match effective {
        ExportFormat::Parquet => write_parquet(df, &path)?,
        _ => write_csv(df, &path)?,
    }
    Ok(path)
}

/// Polars 多线程 CSV 写出：带表头、普通十进制（禁止科学计数法）。
fn write_csv(df: &mut DataFrame, path: &Path) -> Result<(), CoreError> {
    let mut file = File::create(path).map_err(|err| {
        CoreError::export_failed(format!("创建 CSV 失败：{}（{err}）", path.display()))
    })?;
    write_csv_chunk(&mut file, df, true, path)
}

/// CSV 分块追加：`include_header = true` 写表头，后续块只追加数据行。
///
/// 行块首尾直接拼接，字节流与一次性 `CsvWriter::finish` 完全一致（逐字节 golden 验证）。
fn write_csv_chunk(
    file: &mut File,
    df: &mut DataFrame,
    include_header: bool,
    path: &Path,
) -> Result<(), CoreError> {
    CsvWriter::new(&mut *file)
        .include_header(include_header)
        .with_float_scientific(Some(false))
        .finish(df)
        .map_err(|err| {
            CoreError::export_failed(format!("写入 CSV 失败：{}（{err}）", path.display()))
        })
}

/// Zstd（级别 3）Parquet 写出。
fn write_parquet(df: &mut DataFrame, path: &Path) -> Result<(), CoreError> {
    let file = File::create(path).map_err(|err| {
        CoreError::export_failed(format!("创建 Parquet 失败：{}（{err}）", path.display()))
    })?;
    ParquetWriter::new(file)
        .with_compression(ParquetCompression::Zstd(Some(Default::default())))
        .finish(df)
        .map_err(|err| {
            CoreError::export_failed(format!("写入 Parquet 失败：{}（{err}）", path.display()))
        })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 落盘：Excel
// ---------------------------------------------------------------------------

/// Sheet 名规范化：只保留 `[A-Za-z0-9_]`，截断到 31 字符，同一 workbook 内去重。
///
/// 去重按 Excel 的大小写不敏感规则处理（内部用小写 key），重名依次加 `_2`、`_3`…，
/// 加后缀时同步回退基名长度，保证结果仍在 31 字符内。
fn normalize_sheet_name(raw: &str, used: &mut BTreeSet<String>) -> String {
    let sanitized: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let mut base: String = sanitized.chars().take(31).collect();
    if base.is_empty() {
        base.push('_');
    }
    if used.insert(base.to_ascii_lowercase()) {
        return base;
    }

    let mut serial = 2u32;
    loop {
        let suffix = format!("_{serial}");
        let keep = 31usize.saturating_sub(suffix.len());
        let mut candidate: String = base.chars().take(keep).collect();
        candidate.push_str(&suffix);
        if used.insert(candidate.to_ascii_lowercase()) {
            return candidate;
        }
        serial = serial.saturating_add(1);
    }
}

/// 把一张表写进工作表：第 0 行表头，数据从第 1 行起；常量内存模式要求逐行递增写入。
///
/// 支持 Float64（信号列 / 数值时间列）与 Utf8（`absolute` 的 `timestamp` 列）。
fn write_dataframe_to_worksheet(
    worksheet: &mut Worksheet,
    df: &DataFrame,
) -> Result<(), CoreError> {
    let names = df.get_column_names();
    for (col_idx, name) in names.iter().enumerate() {
        worksheet
            .write_string(0, col_idx as u16, name.as_str())
            .map_err(|err| xlsx_error("表头", err))?;
    }

    for (col_idx, name) in names.iter().enumerate() {
        let series = df.column(name.as_str()).map_err(|err| {
            CoreError::export_failed(format!("读取列 `{name}` 失败：{err}"))
        })?;
        if let Ok(ca) = series.f64() {
            for row in 0..df.height() {
                // 空值（宽表未重采样时的前导空段）留空单元格，不写入。
                if let Some(value) = ca.get(row) {
                    worksheet
                        .write_number((row + 1) as u32, col_idx as u16, value)
                        .map_err(|err| xlsx_error("数据", err))?;
                }
            }
        } else if let Ok(ca) = series.str() {
            for row in 0..df.height() {
                if let Some(value) = ca.get(row) {
                    worksheet
                        .write_string((row + 1) as u32, col_idx as u16, value)
                        .map_err(|err| xlsx_error("数据", err))?;
                }
            }
        } else {
            return Err(CoreError::export_failed(format!(
                "列 `{name}` 的类型不支持写入 Excel"
            )));
        }
    }
    Ok(())
}

/// 统一的 xlsx 写错误映射。
fn xlsx_error(what: &str, err: XlsxError) -> CoreError {
    CoreError::export_failed(format!("写入 Excel {what} 失败：{err}"))
}

#[cfg(test)]
mod tests {
    use super::{resample_hold, resample_linear};

    /// ① 网格点正好命中样本 → 原值。
    #[test]
    fn resample_hits_samples_exactly() {
        let times = [0.0, 1.0, 2.0];
        let values = [10.0, 20.0, 30.0];
        let grid = [0.0, 1.0, 2.0];
        assert_eq!(
            resample_linear(&times, &values, &grid),
            vec![10.0, 20.0, 30.0]
        );
    }

    /// ② 两样本之间 → 线性插值（手工算好期望值：t=0→0、t=1→10，t=0.5→5）。
    #[test]
    fn resample_interpolates_between_samples() {
        let times = [0.0, 1.0];
        let values = [0.0, 10.0];
        let grid: Vec<f64> = (0..=10).map(|i| i as f64 * 0.1).collect();
        let out = resample_linear(&times, &values, &grid);
        assert_eq!(out.len(), 11);
        for (idx, point) in grid.iter().enumerate() {
            let expected = point * 10.0;
            assert!(
                (out[idx] - expected).abs() < 1e-9,
                "t={point} 应为 {expected}，实际 {}",
                out[idx]
            );
        }
        assert!((out[5] - 5.0).abs() < 1e-9, "t=0.5 必须是 5.0，实际 {}", out[5]);
    }

    /// ③ 网格早于首样本 → 取首样本。
    #[test]
    fn resample_clamps_before_first_sample() {
        let times = [1.0, 2.0];
        let values = [5.0, 7.0];
        let grid = [0.0, 0.5];
        assert_eq!(resample_linear(&times, &values, &grid), vec![5.0, 5.0]);
    }

    /// ④ 网格晚于末样本 → 取末样本。
    #[test]
    fn resample_clamps_after_last_sample() {
        let times = [0.0, 1.0];
        let values = [5.0, 7.0];
        let grid = [2.0, 3.0];
        assert_eq!(resample_linear(&times, &values, &grid), vec![7.0, 7.0]);
    }

    /// ⑤ 只有 1 个样本 → 全部等于该样本。
    #[test]
    fn resample_single_sample_repeats_value() {
        let times = [2.0];
        let values = [42.0];
        let grid = [0.0, 2.0, 5.0];
        assert_eq!(resample_linear(&times, &values, &grid), vec![42.0, 42.0, 42.0]);
    }

    /// ⑥ 空输入 → 返回空（不 panic）。
    #[test]
    fn resample_empty_input_returns_empty() {
        assert!(resample_linear(&[], &[], &[1.0, 2.0]).is_empty());
        assert!(resample_linear(&[0.0], &[1.0], &[]).is_empty());
    }

    // ------------------------------------------------------------------
    // Step 14：hold / linear / ffill_max_gap_ms 的纯函数单元测试
    // ------------------------------------------------------------------

    /// T1-hold：阶梯保持的值必须**全部来自原始采样集合**，且行首（首样本之前）为 `null`。
    #[test]
    fn hold_values_stay_in_original_sample_set() {
        let times = [0.0, 1.0, 2.0];
        let values = [10.0, 20.0, 30.0];
        let grid = [-0.5, 0.0, 0.5, 1.0, 1.5, 2.0, 2.5];
        let (out, nulled) = resample_hold(&times, &values, &grid, None);

        assert_eq!(nulled, 0, "不限制 gap 时不应有超限置空");
        assert_eq!(out.len(), grid.len());
        // 行首：首个真实采样之前必须是 null（不得用第一个值回填）。
        assert_eq!(out[0], None, "首样本之前必须为 null");
        // 阶梯取值：命中样本取原值，两点之间保持左侧最近样本。
        assert_eq!(out[1], Some(10.0));
        assert_eq!(out[2], Some(10.0));
        assert_eq!(out[3], Some(20.0));
        assert_eq!(out[4], Some(20.0));
        assert_eq!(out[5], Some(30.0));
        assert_eq!(out[6], Some(30.0));
        // 核心断言：没有任何非空值不在原始采样集合里。
        for value in out.iter().flatten() {
            assert!(
                values.contains(value),
                "hold 产生了原始采样集合之外的值：{value}"
            );
        }
    }

    /// T1-gap：`行时间 - 最近真实采样时间 > gap` 处必须为 `null`，`<= gap` 处必须保持。
    #[test]
    fn hold_respects_max_gap_boundary() {
        let times = [0.0, 1.0, 3.0];
        let values = [1.0, 2.0, 3.0];
        // gap = 0.5s。
        let grid = [0.0, 0.5, 0.9, 1.0, 1.5, 2.0, 3.0, 3.4];
        let (out, nulled) = resample_hold(&times, &values, &grid, Some(0.5));

        assert_eq!(out[0], Some(1.0), "差距 0 <= gap 必须保持");
        assert_eq!(out[1], Some(1.0), "差距 == gap（边界）必须保持");
        assert_eq!(out[2], None, "差距 0.9 > 0.5 必须置空");
        assert_eq!(out[3], Some(2.0));
        assert_eq!(out[4], Some(2.0), "差距 == gap（边界）必须保持");
        assert_eq!(out[5], None, "差距 1.0 > 0.5 必须置空");
        assert_eq!(out[6], Some(3.0));
        assert_eq!(out[7], Some(3.0), "差距 0.4 <= gap 必须保持");
        assert_eq!(nulled, 2, "跨越 >gap 的行数应被统计出来");
        for value in out.iter().flatten() {
            assert!(values.contains(value), "带 gap 的 hold 也绝不造值：{value}");
        }
    }

    /// T1-linear：相邻插值点落在两端真实采样之间（此处为 0→10 的单调斜坡）。
    #[test]
    fn linear_interpolation_points_stay_between_samples() {
        let times = [0.0, 1.0];
        let values = [0.0, 10.0];
        let grid: Vec<f64> = (0..=8).map(|i| i as f64 * 0.125).collect();
        let out = resample_linear(&times, &values, &grid);
        for (idx, value) in out.iter().enumerate() {
            let point = grid[idx];
            assert!(
                *value >= 0.0 - 1e-12 && *value <= 10.0 + 1e-12,
                "t={point} 的插值 {value} 超出两端采样范围"
            );
            if idx > 0 {
                assert!(
                    *value >= out[idx - 1] - 1e-12,
                    "线性插值在单调数据上必须单调不减"
                );
            }
        }
        assert!((out[4] - 5.0).abs() < 1e-9, "t=0.5 应为 5.0，实际 {}", out[4]);
    }

    /// T1-hold：空输入 → 全部 `null`，绝不 panic。
    #[test]
    fn hold_empty_input_is_all_null() {
        let (out, nulled) = resample_hold(&[], &[], &[1.0, 2.0], Some(0.05));
        assert_eq!(out, vec![None, None]);
        assert_eq!(nulled, 0);
    }
}
