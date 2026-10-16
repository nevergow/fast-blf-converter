//! fast_blf_core —— Fast BLF Converter 的 Rust/PyO3 内核。
//!
//! Step 1：`error`（稳定错误模型）与 `bit_slice`（Intel/Motorola 跨字节位运算）。
//! Step 2a：`decoder`（DBC 解码计划 + BLF 只读扫描）与 Python 侧 `scan`。
//! Step 2b 会继续追加 `exporter` / `convert`（签名见 docs/INTERFACE.md）。
//!
//! 注意：本 crate 的 `extension-module` 是可选 feature，`cargo test` 不启用它。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use rayon::prelude::*;

mod bit_slice;
mod decoder;
mod error;
mod exporter;

pub use bit_slice::{decode_physical, extract_bits, ByteOrder};
pub use decoder::{
    process_file, sanitize_identifier, scan_file, table_name, DecodePlan, FileScan, FileStat,
    MessagePlan, Progress, Selection, ShardSink, SignalPlan, TableKey, TableShards,
};
pub use error::{
    CoreError, BAD_CONFIG, BLF_OPEN, BLF_PARSE, DBC_PARSE, DISK_FULL, EMPTY_RESULT, EXPORT_FAILED,
    NO_DBC_FOR_CHANNEL,
};
pub use exporter::{export, ExportFormat, ExportOptions, ExportReport, TableResult};

// Python 侧异常类名必须是 `CoreError`（见 INTERFACE.md §4），但 Rust 结构体已占用该名字，
// 故 PyO3 异常类型取名 `CoreErrorPy`，注册时用字符串 "CoreError" 暴露给 Python。
pyo3::create_exception!(fast_blf_core, CoreErrorPy, pyo3::exceptions::PyException);

impl From<CoreError> for PyErr {
    fn from(err: CoreError) -> Self {
        // 与 CoreError 的 Display 保持一致，保证 str(e) == "{code}: {message}"。
        CoreErrorPy::new_err(format!("{}: {}", err.code, err.message))
    }
}

/// 返回内核版本号（来自 Cargo.toml 的 CARGO_PKG_VERSION）。
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 只读扫描：给 GUI 建三级信号树（契约见 `docs/INTERFACE.md` §1）。
///
/// 返回结构严格为 `{"files": [...], "channels": {...}}`；单个文件失败或某个通道的 DBC
/// 解析失败都只落到对应条目的 `error` / `dbc_error`，绝不抛异常中断整批任务。
#[pyfunction]
#[pyo3(signature = (blf_paths, dbc_by_channel, progress_cb=None))]
fn scan<'py>(
    py: Python<'py>,
    blf_paths: Vec<String>,
    dbc_by_channel: HashMap<u32, String>,
    progress_cb: Option<Py<PyAny>>,
) -> PyResult<Bound<'py, PyDict>> {
    // 通道 → DBC 路径；key 已经是 1-based，之后全程 1-based。
    let mut dbc_map: BTreeMap<u32, PathBuf> = BTreeMap::new();
    for (channel, path) in &dbc_by_channel {
        dbc_map.insert(*channel, PathBuf::from(path));
    }

    // 宽松构建：某个通道 DBC 解析失败不影响其它通道（scan 要能把错误显示给 GUI）。
    // 空 Selection ⇒ 计划收录全部 DBC 报文；scan 阶段不做过滤。
    let (plan, dbc_errors) = DecodePlan::build_lenient(&dbc_map, &Selection::default());

    // 重活全程 detach：释放 GIL 后用 rayon 并行扫描文件。
    let paths: Vec<PathBuf> = blf_paths.iter().map(PathBuf::from).collect();
    let scans: Vec<FileScan> = py.detach(|| {
        // 工作线程里的 progress 是 no-op：**Python 回调绝不允许并发调用**，
        // 真正的回调在下面收集阶段由本线程串行触发（见任务书 §2）。
        let file_scans: Vec<FileScan> = paths
            .par_iter()
            .map(|path| {
                let noop = |_p: Progress| {};
                scan_file(path, &plan, &noop)
            })
            .collect();

        if let Some(callback) = progress_cb.as_ref() {
            let total = file_scans.len() as u64;
            for (idx, file_scan) in file_scans.iter().enumerate() {
                let payload = Progress {
                    phase: 1,
                    stage: "scan".to_string(),
                    done: idx as u64 + 1,
                    total,
                    file: Some(file_scan.path.clone()),
                    message: format!("已扫描 {}", file_scan.path),
                };
                // 重新获取 GIL 调用 Python；回调抛出的异常必须吞掉，不得 abort 进程。
                let _ = Python::attach(|py| -> PyResult<()> {
                    let event = PyDict::new(py);
                    event.set_item("phase", payload.phase)?;
                    event.set_item("stage", &payload.stage)?;
                    event.set_item("done", payload.done)?;
                    event.set_item("total", payload.total)?;
                    event.set_item("file", payload.file.as_deref())?;
                    event.set_item("message", &payload.message)?;
                    callback.call1(py, (event,))?;
                    Ok(())
                });
            }
        }

        file_scans
    });

    // ---------------- files ----------------
    let out = PyDict::new(py);
    let files_list = PyList::empty(py);
    for file_scan in &scans {
        let file_dict = PyDict::new(py);
        file_dict.set_item("path", &file_scan.path)?;
        file_dict.set_item("size", file_scan.size)?;
        file_dict.set_item("ok", file_scan.ok)?;
        file_dict.set_item("error", file_scan.error.as_deref())?;
        files_list.append(file_dict)?;
    }
    out.set_item("files", files_list)?;

    // ---------------- 跨文件聚合 (channel, id) ----------------
    let mut frame_counts: BTreeMap<(u32, u32), u64> = BTreeMap::new();
    let mut frame_is_fd: BTreeMap<(u32, u32), bool> = BTreeMap::new();
    let mut cycles: BTreeMap<(u32, u32), Vec<f64>> = BTreeMap::new();
    for file_scan in &scans {
        for (&key, &count) in &file_scan.frame_counts {
            *frame_counts.entry(key).or_insert(0) += count;
        }
        for (&key, &is_fd) in &file_scan.frame_is_fd {
            *frame_is_fd.entry(key).or_insert(false) |= is_fd;
        }
        for (&key, &cycle) in &file_scan.cycle_estimate {
            cycles.entry(key).or_default().push(cycle);
        }
    }

    let mut channel_ids: BTreeSet<u32> = dbc_map.keys().copied().collect();
    for &(channel, _) in frame_counts.keys() {
        channel_ids.insert(channel);
    }

    // ---------------- channels ----------------
    let channels_dict = PyDict::new(py);
    for channel in channel_ids {
        let channel_dict = PyDict::new(py);
        match dbc_map.get(&channel) {
            Some(path) => channel_dict.set_item("dbc", path.to_string_lossy().to_string())?,
            None => channel_dict.set_item("dbc", py.None())?,
        }
        match dbc_errors.get(&channel) {
            Some(err) => channel_dict.set_item("dbc_error", err.to_string())?,
            None => channel_dict.set_item("dbc_error", py.None())?,
        }

        let messages_list = PyList::empty(py);
        // DBC 解析失败的通道：按要求 `messages` 为空数组，错误只在 dbc_error 里。
        if !dbc_errors.contains_key(&channel) {
            // frame_counts 是 BTreeMap，过滤后天然按 id 升序。
            for (&(ch, id), &count) in frame_counts.iter().filter(|((ch, _), _)| *ch == channel) {
                let message_plan = plan.lookup(ch, id);
                let message_dict = PyDict::new(py);
                message_dict.set_item("id", id)?;
                message_dict.set_item("id_hex", format!("0x{id:X}"))?;
                match message_plan {
                    Some(plan_message) => message_dict.set_item("name", &plan_message.name)?,
                    None => message_dict.set_item("name", format!("ID_0x{id:X}"))?,
                }
                match cycles.get(&(ch, id)).and_then(median_of) {
                    Some(cycle_ms) => message_dict.set_item("cycle_ms", cycle_ms)?,
                    None => message_dict.set_item("cycle_ms", py.None())?,
                }
                message_dict.set_item("frame_count", count)?;
                message_dict.set_item(
                    "is_fd",
                    frame_is_fd.get(&(ch, id)).copied().unwrap_or(false),
                )?;

                let signals_list = PyList::empty(py);
                if let Some(plan_message) = message_plan {
                    for signal in &plan_message.signals {
                        let signal_dict = PyDict::new(py);
                        signal_dict.set_item("name", &signal.name)?;
                        signal_dict.set_item("unit", &signal.unit)?;
                        signal_dict.set_item("bit_len", signal.bit_len)?;
                        signal_dict.set_item("is_signed", signal.is_signed)?;
                        signal_dict.set_item(
                            "byte_order",
                            match signal.order {
                                ByteOrder::Intel => "intel",
                                ByteOrder::Motorola => "motorola",
                            },
                        )?;
                        signal_dict.set_item("factor", signal.factor)?;
                        signal_dict.set_item("offset", signal.offset)?;
                        set_optional_number(py, &signal_dict, "min", signal.min)?;
                        set_optional_number(py, &signal_dict, "max", signal.max)?;
                        signals_list.append(signal_dict)?;
                    }
                }
                message_dict.set_item("signals", signals_list)?;
                messages_list.append(message_dict)?;
            }
        }
        channel_dict.set_item("messages", messages_list)?;
        channels_dict.set_item(channel.to_string(), channel_dict)?;
    }
    out.set_item("channels", channels_dict)?;

    Ok(out)
}

/// 把 `Option<f64>` 写入 dict：`Some` → float，`None` → Python `None`。
fn set_optional_number(
    py: Python<'_>,
    dict: &Bound<'_, PyDict>,
    key: &str,
    value: Option<f64>,
) -> PyResult<()> {
    match value {
        Some(number) => dict.set_item(key, number),
        None => dict.set_item(key, py.None()),
    }
}

/// 一组样本的中位数（用于跨文件聚合周期估计）；空切片返回 `None`。
fn median_of(samples: &Vec<f64>) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    Some(if sorted.len() % 2 == 0 {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    })
}

/// 从 config dict 里取**必需**字段；缺失或类型不符都返回 `BAD_CONFIG`，并写明字段名。
fn config_required<'py, T>(config: &Bound<'py, PyDict>, key: &str) -> Result<T, CoreError>
where
    T: for<'a> FromPyObject<'a, 'py>,
{
    let value = config
        .get_item(key)
        .map_err(|err| CoreError::bad_config(format!("读取 config 字段 `{key}` 失败：{err}")))?
        .ok_or_else(|| CoreError::bad_config(format!("config 缺少必需字段 `{key}`")))?;
    value.extract::<T>().map_err(|err| {
        let py_err: PyErr = err.into();
        CoreError::bad_config(format!("config 字段 `{key}` 类型错误：{py_err}"))
    })
}

/// 取**可选**字段；字段缺失或值为 `None` 时返回默认值。
fn config_optional<'py, T>(
    config: &Bound<'py, PyDict>,
    key: &str,
    default: T,
) -> Result<T, CoreError>
where
    T: for<'a> FromPyObject<'a, 'py>,
{
    match config
        .get_item(key)
        .map_err(|err| CoreError::bad_config(format!("读取 config 字段 `{key}` 失败：{err}")))?
    {
        None => Ok(default),
        Some(value) if value.is_none() => Ok(default),
        Some(value) => value.extract::<T>().map_err(|err| {
            let py_err: PyErr = err.into();
            CoreError::bad_config(format!("config 字段 `{key}` 类型错误：{py_err}"))
        }),
    }
}

/// 单个文件的失败记录（`errors[]` 元素）。
struct FileError {
    file: String,
    code: String,
    message: String,
}

/// `convert` 重活阶段的结果。
struct ConvertOutcome {
    report: ExportReport,
    errors: Vec<FileError>,
    warnings: Vec<String>,
}

/// 把 `"{code}: {message}"` 拆回两部分；没有冒号时整串当 message。
fn split_code_message(text: &str) -> (String, String) {
    match text.split_once(": ") {
        Some((code, message)) => (code.to_string(), message.to_string()),
        None => (BLF_PARSE.to_string(), text.to_string()),
    }
}

/// 全量转换（契约见 `docs/INTERFACE.md` §1）。
#[pyfunction]
#[pyo3(signature = (config, progress_cb=None))]
fn convert<'py>(
    py: Python<'py>,
    config: &Bound<'py, PyDict>,
    progress_cb: Option<Py<PyAny>>,
) -> PyResult<Bound<'py, PyDict>> {
    let started = Instant::now();

    // ---------------- 手工提取 config（不使用 pythonize）----------------
    let blf_paths: Vec<String> = config_required(config, "blf_paths")?;
    let dbc_raw: HashMap<u32, String> = config_required(config, "dbc_by_channel")?;
    let selected_raw: HashMap<String, HashMap<String, Vec<String>>> =
        config_required(config, "selected")?;
    let split_by_message: bool = config_required(config, "split_by_message")?;
    let resample_hz: Option<f64> = config_required(config, "resample_hz")?;
    let export_format: String = config_required(config, "export_format")?;
    let out_dir: String = config_required(config, "out_dir")?;
    let temp_dir: String = config_required(config, "temp_dir")?;
    let excel_row_limit: u64 = config_optional(config, "excel_row_limit", 1_000_000)?;
    let memory_budget_mb: u64 = config_optional(config, "memory_budget_mb", 1536)?;
    // ffill 只在「宽表 + 保持原始时序」时生效，由 exporter 判断模式后应用。
    let ffill: bool = config_optional(config, "ffill", false)?;

    // ---------------- 参数转换与校验 ----------------
    let mut dbc_map: BTreeMap<u32, PathBuf> = BTreeMap::new();
    for (channel, path) in dbc_raw {
        dbc_map.insert(channel, PathBuf::from(path));
    }

    let mut selection = Selection::default();
    for (channel_text, messages) in selected_raw {
        let channel: u32 = channel_text.parse().map_err(|_| {
            CoreError::bad_config(format!("`selected` 的通道号 `{channel_text}` 不是合法整数"))
        })?;
        selection.channels.insert(channel, messages);
    }

    let format = match export_format.to_ascii_lowercase().as_str() {
        "csv" => ExportFormat::Csv,
        "parquet" => ExportFormat::Parquet,
        "xlsx" => ExportFormat::Xlsx,
        other => {
            return Err(CoreError::bad_config(format!(
                "config 字段 `export_format` 非法：`{other}`（应为 csv/parquet/xlsx）"
            ))
            .into())
        }
    };

    // 重采样在 exporter 内部实现（Step 2c）；这里只做格式校验，不再占位报错。
    if let Some(hz) = resample_hz {
        if !hz.is_finite() || hz <= 0.0 {
            return Err(CoreError::bad_config(format!(
                "config 字段 `resample_hz` 必须是有限正数，收到 {hz}"
            ))
            .into());
        }
    }

    let plan = DecodePlan::build(&dbc_map, &selection)?;
    let paths: Vec<PathBuf> = blf_paths.iter().map(PathBuf::from).collect();
    let total_files = paths.len() as u64;
    let mem_budget = (memory_budget_mb as usize).saturating_mul(1024 * 1024);
    let opts = ExportOptions {
        split_by_message,
        resample_hz,
        format,
        out_dir: PathBuf::from(out_dir),
        excel_row_limit,
        ffill,
    };
    let temp = PathBuf::from(temp_dir);

    // ---------------- 重活：并行解码落分片 → 合并导出 → 清理 ----------------
    let outcome: Result<ConvertOutcome, CoreError> = py.detach(|| {
        let sink = ShardSink::new(temp.clone())?;

        // 进度回调节流器：首个事件放行（初始时间回退 50ms）。
        let last_emit = std::sync::Mutex::new(
            Instant::now()
                .checked_sub(Duration::from_millis(50))
                .unwrap_or_else(Instant::now),
        );
        let warnings: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let emit = |payload: Progress| {
            let Some(callback) = progress_cb.as_ref() else {
                return;
            };
            {
                let mut last = last_emit.lock().unwrap_or_else(|err| err.into_inner());
                if last.elapsed() < Duration::from_millis(50) {
                    return;
                }
                *last = Instant::now();
            }
            // 回调抛异常必须吞掉、记 warnings，绝不让进程崩。
            let called = Python::attach(|py| -> PyResult<()> {
                let event = PyDict::new(py);
                event.set_item("phase", payload.phase)?;
                event.set_item("stage", &payload.stage)?;
                event.set_item("done", payload.done)?;
                event.set_item("total", payload.total)?;
                event.set_item("file", payload.file.as_deref())?;
                event.set_item("message", &payload.message)?;
                callback.call1(py, (event,))?;
                Ok(())
            });
            if let Err(err) = called {
                warnings
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .push(format!("进度回调异常（已忽略）：{err}"));
            }
        };

        // Phase 1：rayon 并行解码；工作线程内回调一律 no-op（Python 回调绝不并发）。
        let noop = |_payload: Progress| {};
        let results: Vec<Result<FileStat, CoreError>> = paths
            .par_iter()
            .map(|path| process_file(path, &plan, &sink, mem_budget, &noop))
            .collect();

        // 收集阶段主线程串行回调（done/total 按文件数）。
        let mut file_errors: Vec<FileError> = Vec::new();
        for (idx, result) in results.iter().enumerate() {
            let file = paths
                .get(idx)
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default();
            let message = match result {
                Ok(stat) => {
                    if let Some(err_text) = &stat.error {
                        let (code, detail) = split_code_message(err_text);
                        file_errors.push(FileError {
                            file: stat.path.clone(),
                            code,
                            message: detail,
                        });
                    }
                    if stat.decode_errors > 0 {
                        warnings
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .push(format!(
                                "{} 有 {} 帧信号位提取失败，已跳过",
                                stat.path, stat.decode_errors
                            ));
                    }
                    format!("已解码 {}", stat.path)
                }
                Err(err) => {
                    file_errors.push(FileError {
                        file: file.clone(),
                        code: err.code.clone(),
                        message: err.message.clone(),
                    });
                    format!("解码失败 {file}：{}", err.message)
                }
            };
            emit(Progress {
                phase: 1,
                stage: "decode".to_string(),
                done: idx as u64 + 1,
                total: total_files,
                file: Some(file),
                message,
            });
        }

        // Phase 2：合并与导出（export 内部用 stage = merge/export 回调）。
        let tables = sink.tables_snapshot();
        let export_result = export(&tables, &opts, &emit);

        // 无论导出成功与否，只删自己写出的 *.parquet 分片，绝不 remove_dir_all。
        for shard in sink.shard_paths() {
            let _ = std::fs::remove_file(&shard);
        }

        let report = export_result?;
        let warnings = warnings
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        Ok(ConvertOutcome {
            report,
            errors: file_errors,
            warnings,
        })
    });

    let outcome = outcome?;
    let report = outcome.report;

    // ---------------- 组装返回值（严格按 INTERFACE §1.2）----------------
    let out = PyDict::new(py);
    out.set_item("ok", outcome.errors.is_empty() && !report.tables.is_empty())?;
    out.set_item("format_used", report.format_used.as_str())?;
    out.set_item("downgraded", report.downgraded)?;
    out.set_item("downgrade_reason", report.downgrade_reason.as_deref())?;
    out.set_item("elapsed_s", started.elapsed().as_secs_f64())?;
    out.set_item("total_rows", report.total_rows)?;

    let tables_list = PyList::empty(py);
    for table in &report.tables {
        let item = PyDict::new(py);
        item.set_item("name", &table.name)?;
        match table.channel {
            Some(channel) => item.set_item("channel", channel)?,
            None => item.set_item("channel", py.None())?,
        }
        match &table.message {
            Some(message) => item.set_item("message", message)?,
            None => item.set_item("message", py.None())?,
        }
        item.set_item("rows", table.rows)?;
        item.set_item("path", &table.path)?;
        match &table.sheet {
            Some(sheet) => item.set_item("sheet", sheet)?,
            None => item.set_item("sheet", py.None())?,
        }
        tables_list.append(item)?;
    }
    out.set_item("tables", tables_list)?;

    let mut warnings = report.warnings;
    warnings.extend(outcome.warnings);
    out.set_item("warnings", warnings)?;

    let errors_list = PyList::empty(py);
    for item in &outcome.errors {
        let error = PyDict::new(py);
        error.set_item("file", &item.file)?;
        error.set_item("code", &item.code)?;
        error.set_item("message", &item.message)?;
        errors_list.append(error)?;
    }
    out.set_item("errors", errors_list)?;

    Ok(out)
}

#[pymodule]
fn fast_blf_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(scan, m)?)?;
    m.add_function(wrap_pyfunction!(convert, m)?)?;
    m.add("CoreError", m.py().get_type::<CoreErrorPy>())?;
    Ok(())
}
