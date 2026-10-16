//! fast_blf_core —— Fast BLF Converter 的 Rust/PyO3 内核。
//!
//! Step 1：`error`（稳定错误模型）与 `bit_slice`（Intel/Motorola 跨字节位运算）。
//! Step 2a：`decoder`（DBC 解码计划 + BLF 只读扫描）与 Python 侧 `scan`。
//! Step 2b 会继续追加 `exporter` / `convert`（签名见 docs/INTERFACE.md）。
//!
//! 注意：本 crate 的 `extension-module` 是可选 feature，`cargo test` 不启用它。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use rayon::prelude::*;

mod bit_slice;
mod dbc_norm;
mod decoder;
mod error;
mod exporter;
mod series;

pub use bit_slice::{decode_physical, extract_bits, ByteOrder};
pub use dbc_norm::{normalize_dbc_bytes, NormOutcome};
pub use decoder::{
    process_file, read_file_timing, sanitize_identifier, scan_file, table_name, DecodePlan,
    FileScan, FileStat, FileTiming, GlobalMin, MessagePlan, Progress, Selection, ShardRef,
    ShardSink, SignalPlan, TableKey, TableShards, WallClock, BAD_TIME_HEADER,
};
pub use error::{
    CoreError, BAD_CONFIG, BLF_OPEN, BLF_PARSE, DBC_PARSE, DISK_FULL, EMPTY_RESULT, EXPORT_FAILED,
    EXPORT_INTERNAL, NO_DBC_FOR_CHANNEL,
};
pub use exporter::{
    catch_export_panic, export, ExportFormat, ExportOptions, ExportPanicContext, ExportReport,
    FillPolicy, TableResult, TimeFormat,
};

// Python 侧异常类名必须是 `CoreError`（见 INTERFACE.md §4），但 Rust 结构体已占用该名字，
// 故 PyO3 异常类型取名 `CoreErrorPy`，注册时用字符串 "CoreError" 暴露给 Python。
pyo3::create_exception!(fast_blf_core, CoreErrorPy, pyo3::exceptions::PyException);

impl From<CoreError> for PyErr {
    fn from(err: CoreError) -> Self {
        // 与 CoreError 的 Display 保持一致，保证 str(e) == "{code}: {message}"。
        CoreErrorPy::new_err(format!("{}: {}", err.code, err.message))
    }
}

/// 相邻两次 Python 进度回调的最小间隔（INTERFACE §2）。
pub const PROGRESS_THROTTLE: Duration = Duration::from_millis(50);

/// 进度回调节流器：热路径（含 rayon 工作线程）产生的 [`Progress`] 经此统一转发给 Python。
///
/// * 相邻两次真正转发间隔 ≥ [`PROGRESS_THROTTLE`]，窗口内的事件直接丢弃（不排队）。
/// * 每次转发都用 [`Python::attach`] 取 GIL；GIL 天然串行化，故允许来自工作线程。
/// * 回调抛出的异常被吞掉并记入 `warnings`，绝不中断任务。
/// * phase 1 的 `done/total` 由 `process_file`/`scan_file` 以「本文件」维度给出
///   （`0/1` → `1/1`）；本类型把它映射成「已完成文件数 / 文件总数」，让 GUI 能显示 `3/8`。
/// * 两个例外保证 GUI 一定能看到"开始"和"100%"：phase 切换时打开闸门、最后一个文件
///   完成时无条件转发一次。
pub struct ProgressThrottle {
    callback: Option<Py<PyAny>>,
    /// 上次真正转发的时间；`None` 表示尚未转发过（首个事件必放行）。
    last_emit: Mutex<Option<Instant>>,
    /// 上次转发所属的 phase；phase 切换时打开闸门，保证每个阶段都有首个事件。
    last_phase: AtomicU8,
    /// 回调异常等非致命告警，任务结束后并入 `warnings`。
    warnings: Mutex<Vec<String>>,
    /// 已完成的文件数（phase 1）。
    files_done: AtomicU64,
    /// 文件总数（phase 1）。
    files_total: u64,
}

impl ProgressThrottle {
    /// 新建节流器；`files_total` 为本次任务的文件总数（phase 1 的 `total`）。
    pub fn new(callback: Option<Py<PyAny>>, files_total: u64) -> Self {
        Self {
            callback,
            last_emit: Mutex::new(None),
            last_phase: AtomicU8::new(0),
            warnings: Mutex::new(Vec::new()),
            files_done: AtomicU64::new(0),
            files_total,
        }
    }

    /// 解码/扫描热路径事件：先做文件维度映射，再按 50ms 节流。
    ///
    /// 返回值表示事件是否被转发（`false` = 被丢弃）。即使被丢弃，文件完成计数仍会推进，
    /// 避免全局进度因节流而丢步；**每个文件的完成事件都无条件转发**（不再只有最后一个），
    /// 保证 GUI 能看到 `1/N、2/N …` 每一级台阶；帧级事件仍走 50ms 节流。
    pub fn emit_decode(&self, payload: Progress) -> bool {
        // 在映射（会推进 files_done）之前判断：本事件是否为「单个文件完成」。
        let is_file_complete =
            payload.phase == 1 && payload.total > 0 && payload.done >= payload.total;
        let mapped = self.map_file_dimension(payload);
        if is_file_complete {
            self.force(mapped);
            return true;
        }
        self.emit(mapped)
    }

    /// phase 2（合并/导出）事件：只做节流，`done/total` 已是步数。
    pub fn emit(&self, payload: Progress) -> bool {
        if !self.open_gate(payload.phase) {
            return false;
        }
        self.dispatch(&payload);
        true
    }

    /// 取走回调异常告警（供 `convert` 汇总进返回值）。
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .warnings
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }

    /// 把「本文件完成」映射成「全局已完成文件数 / 文件总数」。
    fn map_file_dimension(&self, mut payload: Progress) -> Progress {
        if payload.phase != 1 {
            return payload;
        }
        // process_file/scan_file 用 total=1、done=0/1 表示单个文件是否完成。
        if payload.total > 0 && payload.done >= payload.total {
            let done = self.files_done.fetch_add(1, Ordering::SeqCst) + 1;
            payload.done = done;
        } else {
            payload.done = self.files_done.load(Ordering::SeqCst);
        }
        if self.files_total > 0 {
            payload.total = self.files_total;
        }
        payload
    }

    /// 时间闸门：同 phase 内距上次转发不足 50ms 时返回 `false`（事件丢弃）。
    /// phase 切换时无条件打开闸门，保证每个阶段的首个事件都能到达 GUI。
    fn open_gate(&self, phase: u8) -> bool {
        let mut last = self
            .last_emit
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous_phase = self.last_phase.swap(phase, Ordering::SeqCst);
        let too_soon = previous_phase == phase
            && last
                .map(|previous| previous.elapsed() < PROGRESS_THROTTLE)
                .unwrap_or(false);
        if too_soon {
            return false;
        }
        *last = Some(Instant::now());
        true
    }

    /// 无条件转发一次（用于"最后一个文件完成"这种必须到达 GUI 的事件）。
    fn force(&self, payload: Progress) {
        self.dispatch(&payload);
        self.last_phase.store(payload.phase, Ordering::SeqCst);
        *self
            .last_emit
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(Instant::now());
    }

    /// 真正调用 Python 回调（未提供回调时静默跳过）；异常吞掉记 warnings。
    fn dispatch(&self, payload: &Progress) {
        let Some(callback) = self.callback.as_ref() else {
            return;
        };
        let called = Python::attach(|py| -> PyResult<()> {
            let event = PyDict::new(py);
            event.set_item("phase", payload.phase)?;
            event.set_item("stage", &payload.stage)?;
            event.set_item("done", payload.done)?;
            event.set_item("total", payload.total)?;
            event.set_item("file", payload.file.as_deref())?;
            event.set_item("frames_done", payload.frames_done)?;
            event.set_item("message", &payload.message)?;
            callback.call1(py, (event,))?;
            Ok(())
        });
        if let Err(err) = called {
            self.warnings
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(format!("进度回调异常（已忽略）：{err}"));
        }
    }
}

/// 返回内核版本号（来自 Cargo.toml 的 CARGO_PKG_VERSION）。
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// scan 输出前的中间结构：一条待列出的报文及其观测值。
///
/// * `plan` 命中 DBC 计划时为 `Some`（带信号列表与 DBC 名），未绑 DBC 的观测报文为 `None`；
/// * 完全没有帧观测（`blf_paths == []` 等）时，`frame_count=0` / `cycle_ms=None` / `is_fd=false`。
struct ScanMessage<'a> {
    id: u32,
    frame_count: u64,
    cycle_ms: Option<f64>,
    is_fd: bool,
    plan: Option<&'a MessagePlan>,
}

/// 只读扫描：给 GUI 建三级信号树（契约见 `docs/INTERFACE.md` §1）。
///
/// 返回结构严格为 `{"files": [...], "channels": {...}}`；单个文件失败或某个通道的 DBC
/// 解析失败都只落到对应条目的 `error` / `dbc_error`，绝不抛异常中断整批任务。
#[pyfunction]
#[pyo3(signature = (blf_paths, dbc_by_channel, progress_cb=None))]
pub fn scan<'py>(
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
    // scan 的返回契约（§1）没有 warnings 字段：解析失败时规范化 notes 已并入对应通道的
    // `dbc_error` 错误消息；解析成功但被规范化过时，经 Step 11 新增的 `dbc_note` 展示给 GUI。
    let (plan, dbc_errors, dbc_notes) = DecodePlan::build_lenient(&dbc_map, &Selection::default());

    // 重活全程 detach：释放 GIL 后用 rayon 并行扫描文件。
    let paths: Vec<PathBuf> = blf_paths.iter().map(PathBuf::from).collect();
    // 热路径回调统一走节流器：允许来自 rayon 工作线程，GIL 天然串行化。
    let throttle = Arc::new(ProgressThrottle::new(progress_cb, paths.len() as u64));
    let scans: Vec<FileScan> = py.detach(|| {
        paths
            .par_iter()
            .map(|path| {
                let throttle = Arc::clone(&throttle);
                let emit = move |payload: Progress| {
                    throttle.emit_decode(payload);
                };
                scan_file(path, &plan, &emit)
            })
            .collect()
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

    // 是否完全没有帧观测：`blf_paths == []`、所有文件都打不开、或 BLF 里 0 帧都算。
    // 此时按已绑 DBC 的计划补全报文列表（frame_count=0 / cycle_ms=None / is_fd=false），
    // 供"还没加 BLF 就先解析 DBC 看信号树"使用（INTERFACE §1）。
    let no_observations = frame_counts.is_empty();

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
        // Step 11：解析成功但被规范化过（去 BOM / GB18030 解码 / 忽略不支持语句）时，
        // 把 notes 拼成一行短文本回传给 GUI；未规范化 → None；解析失败 → None
        // （失败原因连同 notes 已在 `dbc_error` 里）。
        let dbc_note: Option<String> = if dbc_errors.contains_key(&channel) {
            None
        } else {
            dbc_notes
                .get(&channel)
                .filter(|notes| !notes.is_empty())
                .map(|notes| notes.join("；"))
        };
        match dbc_note {
            Some(note) => channel_dict.set_item("dbc_note", note)?,
            None => channel_dict.set_item("dbc_note", py.None())?,
        }

        let messages_list = PyList::empty(py);
        // DBC 解析失败的通道：按要求 `messages` 为空数组，错误只在 dbc_error 里。
        if !dbc_errors.contains_key(&channel) {
            // (id, frame_count, cycle_ms, is_fd, DBC 计划)。
            //
            // * 有帧观测时：只列观测到的 `(channel, id)`（既有语义；frame_counts 是
            //   BTreeMap，过滤后天然按 id 升序）；
            // * 完全没有观测时（含 `blf_paths == []`）：按 DBC 计划列出该通道的全部报文，
            //   frame_count=0 / cycle_ms=None / is_fd=false，绝不因"观测不到帧"而丢报文。
            let mut entries: Vec<ScanMessage<'_>> = Vec::new();
            if no_observations {
                let mut plan_messages: Vec<&MessagePlan> = plan
                    .messages()
                    .filter(|message| message.channel == channel)
                    .collect();
                plan_messages.sort_by_key(|message| message.frame_id);
                for message in plan_messages {
                    entries.push(ScanMessage {
                        id: message.frame_id,
                        frame_count: 0,
                        cycle_ms: None,
                        is_fd: false,
                        plan: Some(message),
                    });
                }
            } else {
                for (&(ch, id), &count) in
                    frame_counts.iter().filter(|((ch, _), _)| *ch == channel)
                {
                    entries.push(ScanMessage {
                        id,
                        frame_count: count,
                        cycle_ms: cycles.get(&(ch, id)).and_then(median_of),
                        is_fd: frame_is_fd.get(&(ch, id)).copied().unwrap_or(false),
                        plan: plan.lookup(ch, id),
                    });
                }
            }

            for entry in entries {
                let message_plan = entry.plan;
                let message_dict = PyDict::new(py);
                message_dict.set_item("id", entry.id)?;
                message_dict.set_item("id_hex", format!("0x{:X}", entry.id))?;
                match message_plan {
                    Some(plan_message) => message_dict.set_item("name", &plan_message.name)?,
                    None => message_dict.set_item("name", format!("ID_0x{:X}", entry.id))?,
                }
                match entry.cycle_ms {
                    Some(cycle_ms) => message_dict.set_item("cycle_ms", cycle_ms)?,
                    None => message_dict.set_item("cycle_ms", py.None())?,
                }
                message_dict.set_item("frame_count", entry.frame_count)?;
                message_dict.set_item("is_fd", entry.is_fd)?;

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

/// 构造"所选内容全部落空"时的 `EMPTY_RESULT` 人话说明。
///
/// 只在 `DecodePlan` 为空、即用户选中的报文/信号**全部**落在失败通道或未绑 DBC 的通道上时
/// 调用（INTERFACE §5.9）。说明里点名涉及的通道，并带上失败通道的原始原因关键字
/// （如 `DBC`）；未绑 DBC 的通道单独列出。
pub(crate) fn empty_selection_message(
    selection: &Selection,
    dbc_map: &BTreeMap<u32, PathBuf>,
    dbc_errors: &BTreeMap<u32, CoreError>,
) -> String {
    // 选中通道：`selected` 为空表示"全选"，此时以所有已绑 DBC 的通道为准。
    let selected_channels: BTreeSet<u32> = if selection.channels.is_empty() {
        dbc_map
            .keys()
            .copied()
            .chain(dbc_errors.keys().copied())
            .collect()
    } else {
        selection.channels.keys().copied().collect()
    };

    let mut failed: Vec<String> = Vec::new();
    let mut unbound: Vec<String> = Vec::new();
    for channel in selected_channels {
        if let Some(err) = dbc_errors.get(&channel) {
            failed.push(format!("{channel}（{}）", err.message));
        } else if !dbc_map.contains_key(&channel) {
            unbound.push(channel.to_string());
        }
    }

    let mut segments: Vec<String> = Vec::new();
    if !failed.is_empty() {
        segments.push(format!(
            "所选信号全部来自 DBC 构建失败的通道：{}",
            failed.join("、")
        ));
    }
    if !unbound.is_empty() {
        segments.push(format!(
            "所选信号全部来自未绑定 DBC 的通道：{}",
            unbound.join("、")
        ));
    }
    if segments.is_empty() {
        // plan 为空但又没有失败/未绑通道：所选报文在已解析的 DBC 里都不存在。
        "所选报文/信号在已解析的 DBC 中均不存在，结果为空".to_string()
    } else {
        segments.join("；")
    }
}

/// 全量转换（契约见 `docs/INTERFACE.md` §1）。
#[pyfunction]
#[pyo3(signature = (config, progress_cb=None))]
pub fn convert<'py>(
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
    // Step 14：填充策略与最大保持时长。
    // `fill_policy` 未显式给出时按路径取默认；`ffill` 是兼容别名（true ⇔ 显式 hold）。
    let fill_policy_raw: Option<String> = config_optional(config, "fill_policy", None)?;
    let requested_fill = match fill_policy_raw.as_deref() {
        Some(raw) => Some(FillPolicy::parse(raw).ok_or_else(|| {
            CoreError::bad_config(format!(
                "config 字段 `fill_policy` 非法：`{raw}`（合法值：{}）",
                FillPolicy::valid_values()
            ))
        })?),
        None => None,
    };
    let ffill: bool = config_optional(config, "ffill", false)?;
    let ffill_max_gap_ms: Option<f64> = config_optional(config, "ffill_max_gap_ms", None)?;
    if let Some(gap) = ffill_max_gap_ms {
        if !gap.is_finite() || gap <= 0.0 {
            return Err(CoreError::bad_config(format!(
                "config 字段 `ffill_max_gap_ms` 必须是有限正数（单位毫秒），收到 {gap}"
            ))
            .into());
        }
    }
    // `fill_policy` 优先于 `ffill`（任务书 Step 14 契约）。
    let explicit_fill = requested_fill.or(if ffill { Some(FillPolicy::Hold) } else { None });
    // 时间列口径：缺省 relative_s；非法值 BAD_CONFIG 并列出 4 个合法值。
    let time_format_raw: String = config_optional(config, "time_format", "relative_s".to_string())?;
    let time_format = TimeFormat::parse(&time_format_raw).ok_or_else(|| {
        CoreError::bad_config(format!(
            "config 字段 `time_format` 非法：`{time_format_raw}`（合法值：{}）",
            TimeFormat::valid_values()
        ))
    })?;

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

    // Step 14：把「用户请求 + 路径」解析成**本路径最终生效**的填充策略。
    // * 拆表：同一报文内周期一致、不产生跨信号空值 → 填充不生效（`None`）；
    // * 宽表 + 重采样网格：默认 `hold`；显式 `none` 无意义 → 按 `hold` 处理并记 warning；
    // * 宽表 + 保持原始时序：默认 `none`（= 现状 `ffill=false`），可显式 hold/linear。
    let mut fill_warnings: Vec<String> = Vec::new();
    let effective_fill = if split_by_message {
        FillPolicy::None
    } else if resample_hz.is_some() {
        match explicit_fill {
            Some(FillPolicy::Linear) => FillPolicy::Linear,
            Some(FillPolicy::None) => {
                fill_warnings.push(
                    "重采样网格上 `fill_policy=none` 无意义（网格点必须有值），已按 `hold` 处理"
                        .to_string(),
                );
                FillPolicy::Hold
            }
            // 显式 hold，或未显式给出 → 路径默认 hold。
            _ => FillPolicy::Hold,
        }
    } else {
        explicit_fill.unwrap_or(FillPolicy::None)
    };
    // `ffill_max_gap_ms` 仅 `hold` 生效（其它策略即使给了也忽略）。
    let effective_gap_ms = if effective_fill == FillPolicy::Hold {
        ffill_max_gap_ms
    } else {
        None
    };

    // §5.9：单通道 DBC 失败不得拖垮整体。宽松构建后，失败通道在 plan 里为空，
    // 原因按通道回传（`dbc_errors` + `warnings`）；只有"所选内容全部落空"才 EMPTY_RESULT。
    let (plan, dbc_errors, dbc_notes) = DecodePlan::build_lenient(&dbc_map, &selection);
    let dbc_error_reasons: BTreeMap<u32, String> = dbc_errors
        .iter()
        .map(|(&channel, err)| (channel, err.message.clone()))
        .collect();
    let dbc_warnings: Vec<String> = dbc_errors
        .iter()
        .map(|(&channel, err)| {
            format!(
                "通道 {channel} 的 DBC 解析失败，已跳过该通道（原因：{}）",
                err.message
            )
        })
        .collect();

    // Step 9：DBC 预规范化 notes（去 BOM / GB18030 解码 / 忽略不支持语句）逐条并入 warnings，
    // 统一带 `DBC「<文件名>」：` 前缀，便于用户定位到具体通道/文件。
    let mut dbc_note_warnings: Vec<String> = Vec::new();
    for (channel, notes) in &dbc_notes {
        let label = dbc_map
            .get(channel)
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("通道 {channel}"));
        for note in notes {
            dbc_note_warnings.push(format!("DBC「{label}」：{note}"));
        }
    }

    if plan.is_empty() {
        return Err(CoreError::empty_result(empty_selection_message(
            &selection,
            &dbc_map,
            &dbc_errors,
        ))
        .into());
    }

    let paths: Vec<PathBuf> = blf_paths.iter().map(PathBuf::from).collect();
    let total_files = paths.len() as u64;
    let mem_budget = (memory_budget_mb as usize).saturating_mul(1024 * 1024);

    // 每文件的头部墙钟：`absolute` 需要它原样格式化；失败文件也照读（wall=None）。
    let file_timings: Vec<FileTiming> = paths.iter().map(|path| read_file_timing(path)).collect();
    if time_format == TimeFormat::Absolute {
        if let Some(missing) = file_timings.iter().find(|timing| timing.wall.is_none()) {
            return Err(CoreError::new(
                BAD_TIME_HEADER,
                format!(
                    "absolute 时间口径需要 BLF 文件头墙钟，但 {} 的文件头无效",
                    missing.path.display()
                ),
            )
            .into());
        }
    }

    let opts = ExportOptions {
        split_by_message,
        resample_hz,
        format,
        out_dir: PathBuf::from(out_dir),
        excel_row_limit,
        fill_policy: effective_fill,
        ffill_max_gap_ms: effective_gap_ms,
        time_format,
        // 解码期由 ShardSink 原子统计，导出前回填。
        global_min: 0.0,
        file_timings,
    };
    let temp = PathBuf::from(temp_dir);

    // ---------------- 重活：并行解码落分片 → 合并导出 → 清理 ----------------
    let outcome: Result<ConvertOutcome, CoreError> = py.detach(|| {
        let sink = ShardSink::new(temp.clone())?;

        // 进度回调节流器：解码/导出期间由工作线程直接调用（GIL 串行化 + 50ms 节流）。
        let throttle = Arc::new(ProgressThrottle::new(progress_cb, total_files));
        let warnings: Mutex<Vec<String>> = Mutex::new(Vec::new());

        // Phase 1：rayon 并行解码；工作线程在解码循环内部直接发进度（不再 no-op）。
        let results: Vec<Result<FileStat, CoreError>> = paths
            .par_iter()
            .map(|path| {
                let throttle = Arc::clone(&throttle);
                let emit = move |payload: Progress| {
                    throttle.emit_decode(payload);
                };
                process_file(path, &plan, &sink, mem_budget, &emit)
            })
            .collect();

        // 收集错误/告警（进度已在热路径发出，这里不再补发）。
        let mut file_errors: Vec<FileError> = Vec::new();
        for (idx, result) in results.iter().enumerate() {
            let file = paths
                .get(idx)
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default();
            match result {
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
                }
                Err(err) => {
                    file_errors.push(FileError {
                        file: file.clone(),
                        code: err.code.clone(),
                        message: err.message.clone(),
                    });
                }
            }
        }

        // Phase 2：合并与导出（export 内部用 stage = merge/export 回调）。
        let tables = sink.tables_snapshot();
        let export_emit = |payload: Progress| {
            throttle.emit(payload);
        };
        // 全局限在解码期原子统计完成，这里回填后交给 exporter，绝不在导出期重算。
        let mut export_opts = opts.clone();
        export_opts.global_min = sink.global_min();
        let export_result = export(&tables, &export_opts, &export_emit);

        // 无论导出成功与否，只删自己写出的 *.parquet 分片，绝不 remove_dir_all。
        for shard in sink.shard_paths() {
            let _ = std::fs::remove_file(&shard);
        }

        let report = export_result?;
        let mut warnings = warnings
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        warnings.extend(throttle.take_warnings());
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

    // 按通道回传 DBC 构建失败原因（通道号字符串 → 人话原因），供 GUI 在解析阶段直接显示。
    let dbc_errors_dict = PyDict::new(py);
    for (channel, reason) in &dbc_error_reasons {
        dbc_errors_dict.set_item(channel.to_string(), reason)?;
    }
    out.set_item("dbc_errors", dbc_errors_dict)?;

    let mut warnings = report.warnings;
    warnings.extend(dbc_warnings);
    warnings.extend(dbc_note_warnings);
    warnings.extend(outcome.warnings);
    warnings.extend(fill_warnings);
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

/// 画图数据接口（契约见 `docs/INTERFACE.md` §1.3）：Rust 侧完成「解码 + 分桶抽稀 + 统计」，
/// 只把每信号 ≤ `buckets` 个点的结果交给 Python；绝不把百万级采样点搬进 Python。
///
/// config 字段：`blf_paths` / `dbc_by_channel` / `selected` / `time_format` / `buckets` /
/// `temp_dir`（可不用）。`buckets` 越界抛 `BAD_CONFIG`（§4 的稳定错误码）；`blf_paths` 为空返回空 series 且
/// `ok=true`。返回值的字段名逐字对应 §1.3。
#[pyfunction]
pub fn series_for_plot<'py>(
    py: Python<'py>,
    config: &Bound<'py, PyDict>,
) -> PyResult<Bound<'py, PyDict>> {
    // ---------------- 手工提取 config（与 convert 同一套 helper）----------------
    let blf_paths: Vec<String> = config_required(config, "blf_paths")?;
    let dbc_raw: HashMap<u32, String> = config_required(config, "dbc_by_channel")?;
    let selected_raw: HashMap<String, HashMap<String, Vec<String>>> =
        config_required(config, "selected")?;
    let time_format_raw: String = config_optional(config, "time_format", "relative_s".to_string())?;
    let time_format = TimeFormat::parse(&time_format_raw).ok_or_else(|| {
        CoreError::bad_config(format!(
            "config 字段 `time_format` 非法：`{time_format_raw}`（合法值：{}）",
            TimeFormat::valid_values()
        ))
    })?;
    let buckets: usize = config_optional(config, "buckets", series::DEFAULT_BUCKETS)?;
    // `temp_dir` 允许给出但本接口不用（抽稀全在内存，O(buckets×信号数)）；显式读取以兼容
    // convert 的同一份 config。
    let _temp_dir: Option<String> = config_optional(config, "temp_dir", None)?;

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

    let request = series::SeriesRequest {
        blf_paths: blf_paths.iter().map(PathBuf::from).collect(),
        dbc_by_channel: dbc_map,
        selected: selection,
        time_format,
        buckets,
    };

    // 重活（两阶段扫描 + 解码入桶）全程 detach：释放 GIL。
    let out = py.detach(|| series::series_for_plot(&request))?;

    // ---------------- 组装返回值（严格按 §1.3）----------------
    let result = PyDict::new(py);
    result.set_item("ok", out.ok)?;
    result.set_item("t_min", out.t_min)?;
    result.set_item("t_max", out.t_max)?;
    result.set_item("t_min_str", out.t_min_str.as_deref())?;
    result.set_item("t_max_str", out.t_max_str.as_deref())?;

    let axis_list = PyList::empty(py);
    for tick in &out.t_axis {
        let item = PyDict::new(py);
        item.set_item("t", tick.t)?;
        item.set_item("label", &tick.label)?;
        axis_list.append(item)?;
    }
    result.set_item("t_axis", axis_list)?;
    result.set_item("buckets", out.buckets)?;

    let series_list = PyList::empty(py);
    for s in &out.series {
        let item = PyDict::new(py);
        item.set_item("channel", s.channel)?;
        item.set_item("message", &s.message)?;
        item.set_item("signal", &s.signal)?;
        item.set_item("unit", s.unit.as_deref())?;
        item.set_item("count", s.count)?;
        item.set_item("t", PyList::new(py, s.t.iter().copied())?)?;
        item.set_item("min", PyList::new(py, s.min.iter().copied())?)?;
        item.set_item("max", PyList::new(py, s.max.iter().copied())?)?;
        item.set_item("mean", PyList::new(py, s.mean.iter().copied())?)?;
        item.set_item("vmin", s.vmin)?;
        item.set_item("vmax", s.vmax)?;
        item.set_item("first", s.first)?;
        item.set_item("last", s.last)?;
        item.set_item("mean_all", s.mean_all)?;
        series_list.append(item)?;
    }
    result.set_item("series", series_list)?;

    let dbc_errors_dict = PyDict::new(py);
    for (channel, reason) in &out.dbc_errors {
        dbc_errors_dict.set_item(channel.to_string(), reason)?;
    }
    result.set_item("dbc_errors", dbc_errors_dict)?;
    result.set_item("warnings", out.warnings.clone())?;

    let errors_list = PyList::empty(py);
    for item in &out.errors {
        let error = PyDict::new(py);
        error.set_item("file", &item.file)?;
        error.set_item("code", &item.code)?;
        error.set_item("message", &item.message)?;
        errors_list.append(error)?;
    }
    result.set_item("errors", errors_list)?;

    Ok(result)
}

#[pymodule]
fn fast_blf_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(scan, m)?)?;
    m.add_function(wrap_pyfunction!(convert, m)?)?;
    m.add_function(wrap_pyfunction!(series_for_plot, m)?)?;
    m.add("CoreError", m.py().get_type::<CoreErrorPy>())?;
    Ok(())
}

#[cfg(test)]
mod progress_tests {
    use super::{Progress, ProgressThrottle};

    /// 「单个文件完成」事件：`phase=1, done=1, total=1`。
    fn file_complete() -> Progress {
        Progress {
            phase: 1,
            stage: "decode".to_string(),
            done: 1,
            total: 1,
            file: None,
            frames_done: 1,
            message: String::new(),
        }
    }

    /// 「文件内帧进度」事件：`phase=1, done=0, total=1`。
    fn frame_progress() -> Progress {
        Progress {
            phase: 1,
            stage: "decode".to_string(),
            done: 0,
            total: 1,
            file: None,
            frames_done: 1,
            message: String::new(),
        }
    }

    /// T4/R5：同一节流窗口内，每个「文件完成」事件都必须被转发（不再只放行最后一个）。
    #[test]
    fn every_file_completion_is_forwarded_within_one_throttle_window() {
        let files = 8u64;
        let throttle = ProgressThrottle::new(None, files);
        for step in 0..files {
            assert!(
                throttle.emit_decode(file_complete()),
                "第 {} 个文件完成事件被节流吞掉",
                step + 1
            );
        }
    }

    /// T4/R5：帧级事件仍受 50ms 闸门约束，同一窗口内只应通过第一次。
    #[test]
    fn frame_progress_is_still_throttled_within_one_window() {
        let throttle = ProgressThrottle::new(None, 1);
        let forwarded = (0..8)
            .filter(|_| throttle.emit_decode(frame_progress()))
            .count();
        assert_eq!(forwarded, 1, "同一节流窗口内帧级事件只应通过第一次");
    }
}
