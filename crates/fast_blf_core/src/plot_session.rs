//! Step 17：`PlotSession` —— 预览的唯一数据通道（契约 `docs/INTERFACE.md` §1.4）。
//!
//! 设计要点：
//! * `open` 只做一次「扫描 → 预算 → 落盘帧缓存」；之后任何窗口都走磁盘缓存；
//! * 每个信号在其报文缓存段上按需构建多分辨率包络金字塔（10/100/1000… 帧一桶），
//!   `view()` 只读少量桶 + 二分时间戳，绝不整段读入、绝不逐帧解码整个窗口；
//! * `add_signals` 对已缓存报文立即构建金字塔；未缓存报文进入后台增量预取，
//!   已可用信号照常绘制；
//! * `set_time_format` 只改一个枚举，零解码、零 I/O。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::DateTime;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use serde_json::json;

use crate::decoder::{
    decode_signal_bytes, DecodePlan, MessagePlan, Progress, Selection, SignalPlan,
};
use crate::error::{CoreError, EMPTY_RESULT};
use crate::exporter::TimeFormat;
use crate::frame_cache::{self, CacheMeta, CacheOptions, CachedMessageMeta};

/// `open_plot_session` 的 Rust 侧输入（Python config 已解析成强类型）。
pub struct SessionConfig {
    pub blf_paths: Vec<PathBuf>,
    pub dbc_by_channel: BTreeMap<u32, PathBuf>,
    pub selection: Selection,
    pub time_format: TimeFormat,
    pub cache_dir: PathBuf,
    pub cache_budget_mb: u64,
}

/// 一个可绘制信号的稳定身份。
struct CatalogSignal {
    key: String,
    channel: u32,
    message: String,
    frame_id: u32,
    unit: Option<String>,
    signal: Arc<SignalPlan>,
}

/// 全量原始统计（f64；构建金字塔时缓存，`stats()` 直接返回）。
#[derive(Debug, Clone, Copy, Default)]
struct SignalStats {
    count: u64,
    vmin: f64,
    vmax: f64,
    mean: f64,
    first: f64,
    last: f64,
}

/// 金字塔的一层：四个并行数组，合计 20 字节/桶（4+4+8+4）。
#[derive(Debug, Clone)]
struct Level {
    bucket_size: u64,
    min: Vec<f32>,
    max: Vec<f32>,
    sum: Vec<f64>,
    count: Vec<u32>,
}

impl Level {
    fn len(&self) -> usize {
        self.count.len()
    }

    fn bytes(&self) -> usize {
        self.min.len() * 4 + self.max.len() * 4 + self.sum.len() * 8 + self.count.len() * 4
    }
}

/// 一个信号的多分辨率包络金字塔。
#[derive(Debug, Clone)]
struct SignalPyramid {
    levels: Vec<Level>,
}

impl SignalPyramid {
    fn bytes(&self) -> usize {
        self.levels.iter().map(Level::bytes).sum()
    }
}

#[derive(Debug, Clone, Copy)]
struct BucketAgg {
    min: f32,
    max: f32,
    sum: f64,
    count: u64,
}

impl BucketAgg {
    fn empty() -> Self {
        Self {
            min: f32::INFINITY,
            max: f32::NEG_INFINITY,
            sum: 0.0,
            count: 0,
        }
    }

    fn merge(&mut self, min: f32, max: f32, sum: f64, count: u64) {
        if count == 0 {
            return;
        }
        self.min = self.min.min(min);
        self.max = self.max.max(max);
        self.sum += sum;
        self.count = self.count.saturating_add(count);
    }

    fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum / self.count as f64
        }
    }
}

struct PyramidEntry {
    pyramid: SignalPyramid,
    stats: SignalStats,
}

#[derive(Debug, Clone, Default)]
struct PrefetchStatus {
    busy: bool,
    done: u64,
    total: u64,
    message: String,
}

struct SessionState {
    meta: CacheMeta,
    dir: PathBuf,
    cache_hit: bool,
    /// 本会话（open + 后台预取）因目录上限 LRU 删除的其它 key 字节数。
    evicted_bytes: u64,
    active: BTreeSet<String>,
    pyramids: HashMap<String, Arc<PyramidEntry>>,
    time_format: TimeFormat,
    prefetch: PrefetchStatus,
    prefetch_queue: BTreeSet<(u32, u32)>,
    warnings: Vec<String>,
    closed: bool,
}

struct SessionInner {
    state: Mutex<SessionState>,
    cache_dir: PathBuf,
    key: String,
    blf_paths: Vec<PathBuf>,
    dbc_by_channel: BTreeMap<u32, PathBuf>,
    budget_mb: u64,
    catalog: Vec<CatalogSignal>,
    catalog_by_key: HashMap<String, usize>,
    messages: BTreeMap<(u32, u32), Arc<MessagePlan>>,
}

impl SessionInner {
    fn cache_options(&self) -> CacheOptions<'_> {
        CacheOptions {
            blf_paths: &self.blf_paths,
            dbc_by_channel: &self.dbc_by_channel,
            cache_dir: &self.cache_dir,
            budget_mb: self.budget_mb,
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn build_for_message(
        &self,
        message_key: (u32, u32),
        wanted: &[String],
    ) -> Result<Vec<(String, PyramidEntry)>, CoreError> {
        let state = self.lock_state();
        let Some(message_meta) = state.meta.message(message_key.0, message_key.1).cloned() else {
            return Ok(Vec::new());
        };
        let dir = state.dir.clone();
        drop(state);

        let mut signals: Vec<(String, Arc<SignalPlan>)> = Vec::with_capacity(wanted.len());
        for key in wanted {
            if let Some(&index) = self.catalog_by_key.get(key) {
                signals.push((key.clone(), Arc::clone(&self.catalog[index].signal)));
            }
        }
        build_pyramids(&dir, &message_meta, &signals)
    }

    fn compute_view(&self, t0: f64, t1: f64, width_px: usize) -> Result<ViewData, CoreError> {
        let state = self.lock_state();
        let time_info = TimeInfo::from_meta(&state.meta, state.time_format);
        if state.closed || width_px == 0 {
            return Ok(ViewData {
                t: Vec::new(),
                series: Vec::new(),
                truncated: width_px == 0,
                t_min_str: time_info.t_min_str,
                t_max_str: time_info.t_max_str,
                t_axis: time_info.t_axis,
            });
        }
        let (t0, t1) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
        if !t0.is_finite() || !t1.is_finite() || t1 <= t0 {
            return Ok(ViewData {
                t: Vec::new(),
                series: Vec::new(),
                truncated: true,
                t_min_str: time_info.t_min_str,
                t_max_str: time_info.t_max_str,
                t_axis: time_info.t_axis,
            });
        }
        // 上限保护：GUI 实际只用 ≤2000，但接口允许调用方传更大值。
        let n_out = width_px.clamp(1, 100_000);
        let meta = state.meta.clone();
        let format = state.time_format;
        let raw_min = if meta.t_min.is_finite() {
            meta.t_min
        } else {
            0.0
        };
        let raw_t0 = to_raw_seconds(t0, raw_min, format);
        let raw_t1 = to_raw_seconds(t1, raw_min, format);
        let span = t1 - t0;

        let mut by_message: BTreeMap<
            (u32, u32),
            Vec<(String, Arc<PyramidEntry>, Arc<SignalPlan>)>,
        > = BTreeMap::new();
        let mut missing = false;
        for key in &state.active {
            let Some(&index) = self.catalog_by_key.get(key) else {
                continue;
            };
            let catalog = &self.catalog[index];
            let Some(entry) = state.pyramids.get(key) else {
                missing = true;
                continue;
            };
            by_message
                .entry((catalog.channel, catalog.frame_id))
                .or_default()
                .push((key.clone(), Arc::clone(entry), Arc::clone(&catalog.signal)));
        }

        struct PendingSeries {
            key: String,
            values: Vec<Option<BucketAgg>>,
        }
        let mut pending: Vec<PendingSeries> = Vec::new();

        for (message_key, entries) in by_message {
            if entries.is_empty() {
                continue;
            }
            let Some(message_meta) = meta.message(message_key.0, message_key.1) else {
                continue;
            };
            let path = message_meta.path(&state.dir);
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(_) => {
                    missing = true;
                    continue;
                }
            };
            let stride = message_meta.stride as usize;
            let boundaries =
                time_boundaries(&file, stride, message_meta.frames, raw_t0, raw_t1, n_out)?;
            let first = boundaries.first().copied().unwrap_or(0);
            let last_exclusive = boundaries.last().copied().unwrap_or(0);
            if last_exclusive <= first {
                continue;
            }
            let i0 = first;
            let i1 = last_exclusive - 1;
            let (level_index, _, _, _) = choose_level(&entries[0].1.pyramid, i0, i1, width_px);
            let level_index = level_index.min(entries[0].1.pyramid.levels.len().saturating_sub(1));
            let signals: Vec<Arc<SignalPlan>> = entries
                .iter()
                .map(|(_, _, signal)| Arc::clone(signal))
                .collect();
            let levels = match entries[0].1.pyramid.levels.get(..=level_index) {
                Some(levels) => levels,
                None => &entries[0].1.pyramid.levels[..],
            };
            let mut values: Vec<Vec<Option<BucketAgg>>> =
                vec![Vec::with_capacity(n_out); entries.len()];
            for bucket in 0..n_out {
                let start = boundaries.get(bucket).copied().unwrap_or(0);
                let next = boundaries.get(bucket + 1).copied().unwrap_or(0);
                let end = next.saturating_sub(1);
                if start > end {
                    for series in &mut values {
                        series.push(None);
                    }
                    continue;
                }
                // 同报文的全部信号共享一次边界帧读取与一次桶遍历。
                let aggs = exact_range_multi(levels, &file, &message_meta, &signals, start, end);
                for (index, agg) in aggs.into_iter().enumerate() {
                    if let Some(series) = values.get_mut(index) {
                        series.push(if agg.count == 0 { None } else { Some(agg) });
                    }
                }
            }
            for (index, (key, _, _)) in entries.into_iter().enumerate() {
                let values = values
                    .get_mut(index)
                    .map(std::mem::take)
                    .unwrap_or_default();
                pending.push(PendingSeries { key, values });
            }
        }

        let mut out_t: Vec<f64> = Vec::new();
        let mut out_series: Vec<ViewSeries> = pending
            .iter()
            .map(|series| ViewSeries {
                key: series.key.clone(),
                min: Vec::new(),
                max: Vec::new(),
                mean: Vec::new(),
            })
            .collect();
        for bucket in 0..n_out {
            let keep = pending
                .iter()
                .any(|series| series.values.get(bucket).and_then(|value| *value).is_some());
            if !keep {
                continue;
            }
            out_t.push(t0 + (bucket as f64 + 0.5) * span / n_out as f64);
            for (index, series) in pending.iter().enumerate() {
                let Some(target) = out_series.get_mut(index) else {
                    continue;
                };
                match series.values.get(bucket).and_then(|value| *value) {
                    Some(agg) => {
                        target.min.push(agg.min as f64);
                        target.max.push(agg.max as f64);
                        target.mean.push(agg.mean());
                    }
                    None => {
                        target.min.push(f64::NAN);
                        target.max.push(f64::NAN);
                        target.mean.push(f64::NAN);
                    }
                }
            }
        }

        let truncated = missing || out_t.len() < width_px;
        Ok(ViewData {
            t: out_t,
            series: out_series,
            truncated,
            t_min_str: time_info.t_min_str,
            t_max_str: time_info.t_max_str,
            t_axis: time_info.t_axis,
        })
    }

    fn add_signals_with_arc(self: Arc<Self>, keys: &[String]) -> Result<(u64, u64), CoreError> {
        let mut added = 0u64;
        let mut pending = 0u64;
        let mut to_build: BTreeMap<(u32, u32), Vec<String>> = BTreeMap::new();
        let mut need_prefetch = false;
        {
            let mut state = self.lock_state();
            if state.closed {
                return Ok((0, 0));
            }
            for key in keys {
                let Some(&catalog_index) = self.catalog_by_key.get(key) else {
                    continue;
                };
                let catalog = &self.catalog[catalog_index];
                let message_key = (catalog.channel, catalog.frame_id);
                if state.active.insert(key.clone()) {
                    added += 1;
                }
                if state.meta.has_message(catalog.channel, catalog.frame_id) {
                    if !state.pyramids.contains_key(key) {
                        to_build.entry(message_key).or_default().push(key.clone());
                    }
                } else if self.messages.contains_key(&message_key) {
                    pending += 1;
                    state.prefetch_queue.insert(message_key);
                    need_prefetch = true;
                }
            }
        }

        for (message_key, wanted) in to_build {
            let built = self.build_for_message(message_key, &wanted)?;
            let mut state = self.lock_state();
            for (key, entry) in built {
                state.pyramids.insert(key, Arc::new(entry));
            }
        }

        if need_prefetch {
            start_prefetch(Arc::clone(&self));
        }
        Ok((added, pending))
    }

    fn stats_impl(&self, keys: &[String]) -> Result<Vec<(String, SignalStats)>, CoreError> {
        let mut wanted: Vec<String> = if keys.is_empty() {
            let state = self.lock_state();
            state.active.iter().cloned().collect()
        } else {
            keys.to_vec()
        };
        wanted.sort();
        wanted.dedup();
        let mut out = Vec::new();
        for key in wanted {
            let entry = {
                let state = self.lock_state();
                state.pyramids.get(&key).cloned()
            };
            let entry = match entry {
                Some(entry) => entry,
                None => {
                    let Some(&index) = self.catalog_by_key.get(&key) else {
                        continue;
                    };
                    let catalog = &self.catalog[index];
                    let built = self
                        .build_for_message((catalog.channel, catalog.frame_id), &[key.clone()])?;
                    let mut state = self.lock_state();
                    let mut found = None;
                    for (built_key, entry) in built {
                        let entry = Arc::new(entry);
                        found = Some(Arc::clone(&entry));
                        state.pyramids.insert(built_key, entry);
                    }
                    match found {
                        Some(entry) => entry,
                        None => continue,
                    }
                }
            };
            out.push((key, entry.stats));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// 打开
// ---------------------------------------------------------------------------

/// 打开一个 PlotSession（Rust 侧；PyO3 胶水见 `lib.rs::open_plot_session`）。
pub fn open_session(
    config: SessionConfig,
    progress: &dyn Fn(Progress),
) -> Result<PlotSession, CoreError> {
    frame_cache::validate_budget(config.cache_budget_mb)?;

    let (catalog_plan, dbc_errors, dbc_notes) =
        DecodePlan::build_lenient(&config.dbc_by_channel, &Selection::default());
    if catalog_plan.is_empty() {
        return Err(CoreError::new(
            EMPTY_RESULT,
            crate::empty_selection_message(&config.selection, &config.dbc_by_channel, &dbc_errors),
        ));
    }
    let (cache_plan, _, _) = DecodePlan::build_lenient(&config.dbc_by_channel, &config.selection);
    if !config.selection.channels.is_empty() && cache_plan.is_empty() {
        return Err(CoreError::new(
            EMPTY_RESULT,
            crate::empty_selection_message(&config.selection, &config.dbc_by_channel, &dbc_errors),
        ));
    }

    let mut messages: BTreeMap<(u32, u32), Arc<MessagePlan>> = BTreeMap::new();
    for message in catalog_plan.messages() {
        messages.insert(
            (message.channel, message.frame_id),
            Arc::new(message.clone()),
        );
    }

    let mut initial: Vec<&MessagePlan> = if config.selection.channels.is_empty() {
        catalog_plan.messages().collect()
    } else {
        cache_plan.messages().collect()
    };
    initial.sort_by_key(|message| (message.channel, message.frame_id));

    let mut catalog: Vec<CatalogSignal> = Vec::new();
    for message in catalog_plan.messages() {
        for signal in &message.signals {
            let key = format!("ch{}/{}/{}", message.channel, message.name, signal.name);
            catalog.push(CatalogSignal {
                key,
                channel: message.channel,
                message: message.name.clone(),
                frame_id: message.frame_id,
                unit: if signal.unit.is_empty() {
                    None
                } else {
                    Some(signal.unit.clone())
                },
                signal: Arc::new(signal.clone()),
            });
        }
    }
    catalog.sort_by(|a, b| a.key.cmp(&b.key));
    let mut catalog_by_key = HashMap::new();
    for (index, signal) in catalog.iter().enumerate() {
        catalog_by_key.insert(signal.key.clone(), index);
    }

    let options = CacheOptions {
        blf_paths: &config.blf_paths,
        dbc_by_channel: &config.dbc_by_channel,
        cache_dir: &config.cache_dir,
        budget_mb: config.cache_budget_mb,
    };
    let opened = frame_cache::open_cache(&options, &initial, progress)?;
    let key = opened
        .dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();
    let missing = opened.missing.clone();
    let evicted_bytes = opened.evicted_bytes;

    let mut warnings: Vec<String> = dbc_errors
        .iter()
        .map(|(&channel, err)| {
            format!(
                "通道 {channel} 的 DBC 解析失败，已跳过该通道（原因：{}）",
                err.message
            )
        })
        .collect();
    for (channel, notes) in &dbc_notes {
        let label = config
            .dbc_by_channel
            .get(channel)
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("通道 {channel}"));
        for note in notes {
            warnings.push(format!("DBC「{label}」：{note}"));
        }
    }
    warnings.extend(opened.meta.warnings.clone());

    let state = SessionState {
        meta: opened.meta,
        dir: opened.dir,
        cache_hit: opened.cache_hit,
        evicted_bytes,
        active: BTreeSet::new(),
        pyramids: HashMap::new(),
        time_format: config.time_format,
        prefetch: PrefetchStatus::default(),
        prefetch_queue: BTreeSet::new(),
        warnings,
        closed: false,
    };

    let inner = Arc::new(SessionInner {
        state: Mutex::new(state),
        cache_dir: config.cache_dir,
        key,
        blf_paths: config.blf_paths,
        dbc_by_channel: config.dbc_by_channel,
        budget_mb: config.cache_budget_mb,
        catalog,
        catalog_by_key,
        messages,
    });

    // 部分命中：open 已立即返回；缺失段转后台预取（不阻塞调用方）。
    if !missing.is_empty() {
        {
            let mut state = inner.lock_state();
            for message_key in missing {
                state.prefetch_queue.insert(message_key);
            }
        }
        start_prefetch(Arc::clone(&inner));
    }

    Ok(PlotSession { inner })
}

// ---------------------------------------------------------------------------
// PyO3 类
// ---------------------------------------------------------------------------

/// Python 侧 `PlotSession`。
#[pyclass(module = "fast_blf_core", name = "PlotSession")]
pub struct PlotSession {
    inner: Arc<SessionInner>,
}

#[pymethods]
impl PlotSession {
    /// 打开后立刻可用：时间范围、文件、缓存字节、信号清单。
    pub fn info<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let state = self.inner.lock_state();
        let out = PyDict::new(py);
        let time_info = TimeInfo::from_meta(&state.meta, state.time_format);
        out.set_item("t_min", time_info.t_min)?;
        out.set_item("t_max", time_info.t_max)?;
        out.set_item("t_min_str", time_info.t_min_str.as_deref())?;
        out.set_item("t_max_str", time_info.t_max_str.as_deref())?;
        out.set_item("t_axis", axis_to_pylist(py, &time_info.t_axis)?)?;
        out.set_item(
            "files",
            state
                .meta
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>(),
        )?;
        out.set_item("cache_bytes", state.meta.cache_bytes)?;
        out.set_item("cache_hit", state.cache_hit)?;
        out.set_item("evicted_bytes", state.evicted_bytes)?;

        let signals = PyList::empty(py);
        for signal in &self.inner.catalog {
            let item = PyDict::new(py);
            item.set_item("channel", signal.channel)?;
            item.set_item("message", &signal.message)?;
            item.set_item("signal", &signal.signal.name)?;
            item.set_item("unit", signal.unit.as_deref())?;
            item.set_item(
                "cached",
                state.meta.has_message(signal.channel, signal.frame_id),
            )?;
            // 全链路唯一标识，与 stats/add_signals/remove_signals 入参一致。
            item.set_item("key", &signal.key)?;
            signals.append(item)?;
        }
        out.set_item("signals", signals)?;
        out.set_item("warnings", state.warnings.clone())?;
        let pyramid_bytes: usize = state
            .pyramids
            .values()
            .map(|entry| entry.pyramid.bytes())
            .sum();
        out.set_item("pyramid_bytes", pyramid_bytes as u64)?;
        out.set_item("prefetch_busy", state.prefetch.busy)?;
        Ok(out)
    }

    /// 纯显示层切换：只改枚举，零解码、零 I/O（≤16 ms）。
    pub fn set_time_format(&self, format: &str) -> PyResult<()> {
        let parsed = TimeFormat::parse(format).ok_or_else(|| {
            CoreError::bad_config(format!(
                "config 字段 `time_format` 非法：`{format}`（合法值：{}）",
                TimeFormat::VALID.join(" / ")
            ))
        })?;
        let mut state = self.inner.lock_state();
        state.time_format = parsed;
        Ok(())
    }

    /// 已缓存信号立即生效；未缓存报文转后台预取。
    pub fn add_signals<'py>(
        &self,
        py: Python<'py>,
        items: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let keys = extract_keys(items)?;
        let inner = Arc::clone(&self.inner);
        let (added, pending) = py.detach(move || inner.add_signals_with_arc(&keys))?;
        let out = PyDict::new(py);
        out.set_item("added", added)?;
        out.set_item("pending", pending)?;
        Ok(out)
    }

    /// 只影响绘制与统计。
    pub fn remove_signals<'py>(
        &self,
        py: Python<'py>,
        items: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let keys = extract_keys(items)?;
        let removed = {
            let mut state = self.inner.lock_state();
            let mut removed = 0u64;
            for key in keys {
                if state.active.remove(&key) {
                    removed += 1;
                }
            }
            removed
        };
        let out = PyDict::new(py);
        out.set_item("removed", removed)?;
        Ok(out)
    }

    pub fn prefetch_status<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let state = self.inner.lock_state();
        let out = PyDict::new(py);
        out.set_item("busy", state.prefetch.busy)?;
        out.set_item("done", state.prefetch.done)?;
        out.set_item("total", state.prefetch.total)?;
        out.set_item("message", &state.prefetch.message)?;
        Ok(out)
    }

    pub fn view<'py>(
        &self,
        py: Python<'py>,
        t0: f64,
        t1: f64,
        width_px: usize,
    ) -> PyResult<Bound<'py, PyDict>> {
        let inner = Arc::clone(&self.inner);
        let view = py.detach(move || inner.compute_view(t0, t1, width_px))?;
        let out = PyDict::new(py);
        out.set_item("t", PyList::new(py, view.t.iter().copied())?)?;
        let series = PyList::empty(py);
        for item in &view.series {
            let dict = PyDict::new(py);
            dict.set_item("key", &item.key)?;
            dict.set_item("min", PyList::new(py, item.min.iter().copied())?)?;
            dict.set_item("max", PyList::new(py, item.max.iter().copied())?)?;
            dict.set_item("mean", PyList::new(py, item.mean.iter().copied())?)?;
            series.append(dict)?;
        }
        out.set_item("series", series)?;
        out.set_item("truncated", view.truncated)?;
        out.set_item("t_min_str", view.t_min_str.as_deref())?;
        out.set_item("t_max_str", view.t_max_str.as_deref())?;
        out.set_item("t_axis", axis_to_pylist(py, &view.t_axis)?)?;
        Ok(out)
    }

    pub fn stats<'py>(
        &self,
        py: Python<'py>,
        items: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyList>> {
        let keys = extract_keys(items)?;
        let inner = Arc::clone(&self.inner);
        let stats = py.detach(move || inner.stats_impl(&keys))?;
        let out = PyList::empty(py);
        for (key, value) in stats {
            let dict = PyDict::new(py);
            dict.set_item("key", key)?;
            dict.set_item("count", value.count)?;
            dict.set_item("vmin", value.vmin)?;
            dict.set_item("vmax", value.vmax)?;
            dict.set_item("mean", value.mean)?;
            dict.set_item("first", value.first)?;
            dict.set_item("last", value.last)?;
            out.append(dict)?;
        }
        Ok(out)
    }

    pub fn export_html<'py>(
        &self,
        py: Python<'py>,
        path: &str,
        title: &str,
    ) -> PyResult<Bound<'py, PyDict>> {
        let inner = Arc::clone(&self.inner);
        let bytes = py.detach(move || export_html_report(&inner, path, title))?;
        let out = PyDict::new(py);
        out.set_item("ok", true)?;
        out.set_item("bytes", bytes)?;
        Ok(out)
    }

    pub fn close(&self) {
        let mut state = self.inner.lock_state();
        state.closed = true;
        state.active.clear();
        state.pyramids.clear();
        state.prefetch_queue.clear();
        state.prefetch.busy = false;
        state.prefetch.message = "已关闭会话".to_string();
    }
}

// ---------------------------------------------------------------------------
// 时间 / f32 覆盖
// ---------------------------------------------------------------------------

fn to_raw_seconds(t: f64, raw_min: f64, format: TimeFormat) -> f64 {
    match format {
        TimeFormat::UnixS => t,
        TimeFormat::RelativeS | TimeFormat::RelativeMs | TimeFormat::Absolute => t + raw_min,
    }
}

/// `absolute` 口径下的一个时间刻度（与 `series_for_plot` 的 `t_axis` 同形）。
#[derive(Debug, Clone)]
struct AxisTick {
    /// 相对秒（`absolute` 口径下 0 = 全局最早时间戳）。
    t: f64,
    /// 该时刻的绝对墙钟字符串。
    label: String,
}

/// `info()` / `view()` / HTML 报告共用的时间轴视图。
///
/// 非 `absolute` 口径只填 `t_min` / `t_max`；`absolute` 口径额外给出墙钟端点与刻度，
/// 保证三个出口（info / view / HTML）永远是同一套口径。
#[derive(Debug, Clone)]
struct TimeInfo {
    t_min: f64,
    t_max: f64,
    t_min_str: Option<String>,
    t_max_str: Option<String>,
    t_axis: Vec<AxisTick>,
}

impl TimeInfo {
    fn from_meta(meta: &CacheMeta, format: TimeFormat) -> Self {
        let raw_min = if meta.t_min.is_finite() {
            meta.t_min
        } else {
            0.0
        };
        let raw_max = if meta.t_max.is_finite() {
            meta.t_max
        } else {
            raw_min
        };
        let (t_min, t_max) = match format {
            TimeFormat::UnixS => (raw_min, raw_max),
            TimeFormat::RelativeS | TimeFormat::RelativeMs | TimeFormat::Absolute => {
                (0.0, raw_max - raw_min)
            }
        };
        if format != TimeFormat::Absolute {
            return Self {
                t_min,
                t_max,
                t_min_str: None,
                t_max_str: None,
                t_axis: Vec::new(),
            };
        }

        // 优先用缓存扫描时按 `exporter::TimeTransform` 同一公式算出的头部墙钟
        // （`wall_us + round((raw - header_epoch) * 1e6)`）。只有文件头墙钟缺失
        // （例如历史夹具/损坏头）时才退回原始 epoch 微秒，仍保持“绝对时间可显示”，
        // 不改变无墙钟时 `unix_s` 的既有语义。
        let Some((abs_min, abs_max)) = absolute_micros_bounds(meta) else {
            return Self {
                t_min,
                t_max,
                t_min_str: None,
                t_max_str: None,
                t_axis: Vec::new(),
            };
        };
        Self {
            t_min,
            t_max,
            t_min_str: Some(format_wall_clock_micros(abs_min)),
            t_max_str: Some(format_wall_clock_micros(abs_max)),
            t_axis: build_time_axis(t_min, t_max, abs_min, abs_max),
        }
    }
}

fn finite_micros(seconds: f64) -> Option<i64> {
    if seconds.is_finite() {
        Some((seconds * 1_000_000.0).round() as i64)
    } else {
        None
    }
}

/// `absolute` 的端点微秒：优先取缓存里的 BLF 头部墙钟；头无效时退回原始 epoch。
fn absolute_micros_bounds(meta: &CacheMeta) -> Option<(i64, i64)> {
    let min = meta.abs_min_micros.or_else(|| finite_micros(meta.t_min))?;
    let max = meta
        .abs_max_micros
        .or_else(|| finite_micros(meta.t_max))
        .unwrap_or(min);
    Some((min, max.max(min)))
}

/// 10 个等间隔刻度（含首尾）：跨度 <2 秒给到微秒，否则到秒。
fn build_time_axis(t_min: f64, t_max: f64, abs_min: i64, abs_max: i64) -> Vec<AxisTick> {
    const TICKS: usize = 10;
    let span = t_max - t_min;
    let with_micros = span < 2.0;
    let mut axis = Vec::with_capacity(TICKS);
    for index in 0..TICKS {
        let frac = index as f64 / (TICKS - 1) as f64;
        let t = t_min + frac * span;
        // 首尾直接落到缓存里的两个墙钟端点，避免浮点换算让端点差 1 微秒。
        let micros = if index == 0 {
            abs_min
        } else if index + 1 == TICKS {
            abs_max
        } else {
            abs_min + ((t - t_min) * 1_000_000.0).round() as i64
        };
        let label = if with_micros {
            format_wall_clock_micros(micros)
        } else {
            format_wall_clock_seconds(micros)
        };
        axis.push(AxisTick { t, label });
    }
    axis
}

fn axis_to_pylist<'py>(py: Python<'py>, axis: &[AxisTick]) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for tick in axis {
        let item = PyDict::new(py);
        item.set_item("t", tick.t)?;
        item.set_item("label", &tick.label)?;
        list.append(item)?;
    }
    Ok(list)
}

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

/// 金字塔包络形状用 f32；向下/向上取到最近的 f32，保证包络**覆盖**真实极值。
fn f32_min_cover(value: f64) -> f32 {
    let rounded = value as f32;
    if (rounded as f64) <= value {
        rounded
    } else {
        f32_next_down(rounded)
    }
}

fn f32_max_cover(value: f64) -> f32 {
    let rounded = value as f32;
    if (rounded as f64) >= value {
        rounded
    } else {
        f32_next_up(rounded)
    }
}

fn f32_next_down(value: f32) -> f32 {
    if value.is_nan() || value == f32::NEG_INFINITY {
        return value;
    }
    if value == 0.0 {
        return -f32::from_bits(1);
    }
    let bits = value.to_bits();
    if value > 0.0 {
        f32::from_bits(bits - 1)
    } else {
        f32::from_bits(bits + 1)
    }
}

fn f32_next_up(value: f32) -> f32 {
    if value.is_nan() || value == f32::INFINITY {
        return value;
    }
    if value == 0.0 {
        return f32::from_bits(1);
    }
    let bits = value.to_bits();
    if value > 0.0 {
        f32::from_bits(bits + 1)
    } else {
        f32::from_bits(bits - 1)
    }
}

// ---------------------------------------------------------------------------
// 选层 / 聚合
// ---------------------------------------------------------------------------

/// 选层：取**最大**（bucket 最大 = 最粗）的层，使该层在窗口内桶数 ≥ width_px；
/// 没有满足条件的层时退到最粗层（契约 §1.4 / 任务书 Step 17）。
fn choose_level(
    pyramid: &SignalPyramid,
    i0: u64,
    i1: u64,
    width_px: usize,
) -> (usize, u64, u64, u64) {
    let width = width_px.max(1) as u64;
    let mut chosen = 0usize;
    let mut chosen_b0 = 0u64;
    let mut chosen_b1 = 0u64;
    let mut chosen_nb = 0u64;
    let mut found = false;
    for (index, level) in pyramid.levels.iter().enumerate() {
        let size = level.bucket_size.max(1);
        let b0 = i0 / size;
        let b1 = i1 / size;
        let nb = b1.saturating_sub(b0).saturating_add(1);
        if nb >= width {
            chosen = index;
            chosen_b0 = b0;
            chosen_b1 = b1;
            chosen_nb = nb;
            found = true;
        }
    }
    if !found {
        if let Some(level) = pyramid.levels.last() {
            let size = level.bucket_size.max(1);
            chosen = pyramid.levels.len().saturating_sub(1);
            chosen_b0 = i0 / size;
            chosen_b1 = i1 / size;
            chosen_nb = chosen_b1.saturating_sub(chosen_b0).saturating_add(1);
        }
    }
    (chosen, chosen_b0, chosen_b1, chosen_nb)
}

/// 精确聚合任意帧区间（同一报文的多个信号共享原始帧读取）。
///
/// 贪心用最粗的“对齐且完整”的桶；两个边界最多各剩 <10 帧才逐帧解码。
fn exact_range_multi(
    levels: &[Level],
    file: &File,
    message: &CachedMessageMeta,
    signals: &[Arc<SignalPlan>],
    start: u64,
    end: u64,
) -> Vec<BucketAgg> {
    let mut aggs = vec![BucketAgg::empty(); signals.len()];
    let mut index = start;
    while index <= end {
        let mut consumed = false;
        for level in levels.iter().rev() {
            let size = level.bucket_size.max(1);
            if index % size != 0 {
                continue;
            }
            let bucket_end = index.saturating_add(size).saturating_sub(1);
            if bucket_end > end {
                continue;
            }
            let bucket_index = (index / size) as usize;
            let Some(&count) = level.count.get(bucket_index) else {
                continue;
            };
            if count == 0 {
                index = index.saturating_add(size);
                consumed = true;
                break;
            }
            let min = level
                .min
                .get(bucket_index)
                .copied()
                .unwrap_or(f32::INFINITY);
            let max = level
                .max
                .get(bucket_index)
                .copied()
                .unwrap_or(f32::NEG_INFINITY);
            let sum = level.sum.get(bucket_index).copied().unwrap_or(0.0);
            for agg in &mut aggs {
                agg.merge(min, max, sum, count as u64);
            }
            index = index.saturating_add(size);
            consumed = true;
            break;
        }
        if consumed {
            continue;
        }
        // 原始帧兜底（最多 9 帧）；同一帧只读一次、解全部信号。
        let path = Path::new(&message.file);
        match frame_cache::read_record_at_file(file, message, index, path) {
            Ok(record) => {
                let data_len = record.data_len.min(record.dlc as usize);
                for (signal_index, signal) in signals.iter().enumerate() {
                    if let Ok(value) =
                        decode_signal_bytes(signal, record.dlc, &record.payload[..data_len])
                    {
                        if let Some(agg) = aggs.get_mut(signal_index) {
                            agg.merge(f32_min_cover(value), f32_max_cover(value), value, 1);
                        }
                    }
                }
            }
            Err(_) => {}
        }
        index = index.saturating_add(1);
    }
    aggs
}

fn lower_bound_time(
    file: &File,
    stride: usize,
    frames: u64,
    target: f64,
) -> Result<u64, CoreError> {
    let mut lo = 0u64;
    let mut hi = frames;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let ts = frame_cache::read_timestamp_at(file, stride, mid)?;
        if ts < target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

fn upper_bound_time(
    file: &File,
    stride: usize,
    frames: u64,
    target: f64,
) -> Result<u64, CoreError> {
    let mut lo = 0u64;
    let mut hi = frames;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let ts = frame_cache::read_timestamp_at(file, stride, mid)?;
        if ts <= target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

fn time_boundaries(
    file: &File,
    stride: usize,
    frames: u64,
    raw_t0: f64,
    raw_t1: f64,
    n_out: usize,
) -> Result<Vec<u64>, CoreError> {
    let span = raw_t1 - raw_t0;
    let mut boundaries = Vec::with_capacity(n_out.saturating_add(1));
    for bucket in 0..n_out {
        let target = raw_t0 + span * bucket as f64 / n_out as f64;
        boundaries.push(lower_bound_time(file, stride, frames, target)?);
    }
    boundaries.push(upper_bound_time(file, stride, frames, raw_t1)?);
    for value in &mut boundaries {
        *value = (*value).min(frames);
    }
    for index in 1..boundaries.len() {
        if let (Some(previous), Some(current)) = (
            boundaries.get(index - 1).copied(),
            boundaries.get(index).copied(),
        ) {
            if current < previous {
                if let Some(slot) = boundaries.get_mut(index) {
                    *slot = previous;
                }
            }
        }
    }
    Ok(boundaries)
}

// ---------------------------------------------------------------------------
// 金字塔构建
// ---------------------------------------------------------------------------

struct LevelBuilder {
    bucket_size: u64,
    min: Vec<f32>,
    max: Vec<f32>,
    sum: Vec<f64>,
    count: Vec<u32>,
}

impl LevelBuilder {
    fn new(bucket_size: u64, buckets: usize) -> Self {
        Self {
            bucket_size,
            min: vec![f32::INFINITY; buckets],
            max: vec![f32::NEG_INFINITY; buckets],
            sum: vec![0.0; buckets],
            count: vec![0; buckets],
        }
    }

    fn push(&mut self, frame_index: u64, value: f64) {
        let bucket = (frame_index / self.bucket_size) as usize;
        let Some(min) = self.min.get_mut(bucket) else {
            return;
        };
        let covered_min = f32_min_cover(value);
        let covered_max = f32_max_cover(value);
        *min = min.min(covered_min);
        if let Some(max) = self.max.get_mut(bucket) {
            *max = max.max(covered_max);
        }
        if let Some(sum) = self.sum.get_mut(bucket) {
            *sum += value;
        }
        if let Some(count) = self.count.get_mut(bucket) {
            *count = count.saturating_add(1);
        }
    }

    fn finish(self) -> Level {
        Level {
            bucket_size: self.bucket_size,
            min: self.min,
            max: self.max,
            sum: self.sum,
            count: self.count,
        }
    }
}

fn reduce_level(previous: &Level) -> Level {
    let buckets = previous.len().div_ceil(10);
    let mut min = vec![f32::INFINITY; buckets];
    let mut max = vec![f32::NEG_INFINITY; buckets];
    let mut sum = vec![0.0f64; buckets];
    let mut count = vec![0u32; buckets];
    for index in 0..previous.len() {
        let bucket = index / 10;
        let child_count = previous.count.get(index).copied().unwrap_or(0);
        if child_count == 0 {
            continue;
        }
        if let (Some(target), Some(&source)) = (min.get_mut(bucket), previous.min.get(index)) {
            *target = target.min(source);
        }
        if let (Some(target), Some(&source)) = (max.get_mut(bucket), previous.max.get(index)) {
            *target = target.max(source);
        }
        if let (Some(target), Some(&source)) = (sum.get_mut(bucket), previous.sum.get(index)) {
            *target += source;
        }
        if let Some(target) = count.get_mut(bucket) {
            *target = target.saturating_add(child_count);
        }
    }
    Level {
        bucket_size: previous.bucket_size.saturating_mul(10),
        min,
        max,
        sum,
        count,
    }
}

fn build_pyramids(
    dir: &Path,
    message: &CachedMessageMeta,
    signals: &[(String, Arc<SignalPlan>)],
) -> Result<Vec<(String, PyramidEntry)>, CoreError> {
    use std::io::Read;

    if signals.is_empty() {
        return Ok(Vec::new());
    }
    let path = message.path(dir);
    let file = File::open(&path).map_err(|err| {
        CoreError::blf_parse(format!("打开帧缓存失败：{}（{err}）", path.display()))
    })?;
    let mut reader = std::io::BufReader::new(file);
    let stride = message.stride as usize;
    let base_buckets = message.frames.div_ceil(10) as usize;
    let mut builders: Vec<LevelBuilder> = signals
        .iter()
        .map(|_| LevelBuilder::new(10, base_buckets))
        .collect();
    let mut stats: Vec<SignalStats> = vec![SignalStats::default(); signals.len()];

    let mut buffer = [0u8; frame_cache::FD_STRIDE];
    for index in 0..message.frames {
        let Some(record) = buffer.get_mut(..stride) else {
            return Err(CoreError::blf_parse("帧缓存 stride 非法"));
        };
        if let Err(err) = reader.read_exact(record) {
            return Err(CoreError::blf_parse(format!(
                "读取帧缓存失败：{}（{err}）",
                path.display()
            )));
        }
        let dlc = record.get(8).copied().unwrap_or(0);
        let data_len = if stride == frame_cache::FD_STRIDE {
            64
        } else {
            8
        };
        let payload_start = 9.min(record.len());
        let payload_end = (payload_start + data_len).min(record.len());
        let payload = record.get(payload_start..payload_end).unwrap_or(&[]);
        for (signal_index, (_, signal)) in signals.iter().enumerate() {
            let Ok(value) = decode_signal_bytes(signal, dlc, payload) else {
                continue;
            };
            if !value.is_finite() {
                continue;
            }
            let entry = &mut stats[signal_index];
            if entry.count == 0 {
                entry.first = value;
                entry.vmin = value;
                entry.vmax = value;
            } else {
                entry.vmin = entry.vmin.min(value);
                entry.vmax = entry.vmax.max(value);
            }
            entry.last = value;
            entry.count = entry.count.saturating_add(1);
            entry.mean += value;
            if let Some(builder) = builders.get_mut(signal_index) {
                builder.push(index, value);
            }
        }
    }

    let mut output = Vec::with_capacity(signals.len());
    let mut builder_iter = builders.into_iter();
    for (signal_index, (key, _)) in signals.iter().enumerate() {
        let mut levels = Vec::new();
        let first = match builder_iter.next() {
            Some(builder) => builder.finish(),
            None => LevelBuilder::new(10, 0).finish(),
        };
        levels.push(first);
        loop {
            let over = levels.last().map(|level| level.len() > 64).unwrap_or(false);
            if !over {
                break;
            }
            let next = match levels.last() {
                Some(previous) => reduce_level(previous),
                None => break,
            };
            levels.push(next);
        }
        let mut entry = stats.get(signal_index).copied().unwrap_or_default();
        if entry.count > 0 {
            entry.mean /= entry.count as f64;
        }
        output.push((
            key.clone(),
            PyramidEntry {
                pyramid: SignalPyramid { levels },
                stats: entry,
            },
        ));
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// view 输出
// ---------------------------------------------------------------------------

struct ViewSeries {
    key: String,
    min: Vec<f64>,
    max: Vec<f64>,
    mean: Vec<f64>,
}

struct ViewData {
    t: Vec<f64>,
    series: Vec<ViewSeries>,
    truncated: bool,
    t_min_str: Option<String>,
    t_max_str: Option<String>,
    t_axis: Vec<AxisTick>,
}

// ---------------------------------------------------------------------------
// 后台增量预取
// ---------------------------------------------------------------------------

fn start_prefetch(inner: Arc<SessionInner>) {
    let should_start = {
        let mut state = inner.lock_state();
        if state.closed || state.prefetch.busy || state.prefetch_queue.is_empty() {
            false
        } else {
            state.prefetch.busy = true;
            state.prefetch.done = 0;
            state.prefetch.total = state.prefetch_queue.len() as u64;
            state.prefetch.message = "正在预取缺失报文…".to_string();
            true
        }
    };
    if !should_start {
        return;
    }
    let thread_inner = Arc::clone(&inner);
    let spawned = std::thread::Builder::new()
        .name("fblf-prefetch".to_string())
        .spawn(move || prefetch_worker(thread_inner));
    if let Err(err) = spawned {
        let mut state = inner.lock_state();
        state.prefetch.busy = false;
        state.prefetch.message = format!("无法启动后台预取线程：{err}");
        state.warnings.push(format!("无法启动后台预取线程：{err}"));
    }
}

fn prefetch_worker(inner: Arc<SessionInner>) {
    loop {
        let batch: Vec<(u32, u32)> = {
            let mut state = inner.lock_state();
            if state.closed {
                state.prefetch.busy = false;
                state.prefetch.message = "已关闭会话".to_string();
                return;
            }
            if state.prefetch_queue.is_empty() {
                state.prefetch.busy = false;
                state.prefetch.message = String::new();
                return;
            }
            let batch: Vec<(u32, u32)> = state.prefetch_queue.iter().copied().collect();
            state.prefetch_queue.clear();
            state.prefetch.done = 0;
            state.prefetch.total = batch.len() as u64;
            state.prefetch.message = format!("正在预取 {} 个报文…", batch.len());
            batch
        };

        let plans: Vec<Arc<MessagePlan>> = batch
            .iter()
            .filter_map(|key| inner.messages.get(key).cloned())
            .collect();
        let refs: Vec<&MessagePlan> = plans.iter().map(|plan| plan.as_ref()).collect();
        let options = inner.cache_options();
        let progress_inner = Arc::clone(&inner);
        let progress = move |payload: Progress| {
            let mut state = progress_inner.lock_state();
            state.prefetch.message = payload.message;
        };
        let result = frame_cache::prefetch_cache(&options, &inner.key, &refs, &progress);

        match result {
            Ok((meta, evicted)) => {
                let jobs: Vec<((u32, u32), Vec<String>)> = {
                    let mut state = inner.lock_state();
                    // 预取阶段新增的告警（含 LRU 删除说明）并入会话级 warnings。
                    for warning in &meta.warnings {
                        if !state.warnings.contains(warning) {
                            state.warnings.push(warning.clone());
                        }
                    }
                    state.meta = meta;
                    state.evicted_bytes = state.evicted_bytes.saturating_add(evicted);
                    let mut jobs = Vec::new();
                    for message_key in &batch {
                        let wanted: Vec<String> = state
                            .active
                            .iter()
                            .filter(|key| {
                                inner
                                    .catalog_by_key
                                    .get(*key)
                                    .map(|&index| {
                                        let catalog = &inner.catalog[index];
                                        (catalog.channel, catalog.frame_id) == *message_key
                                            && !state.pyramids.contains_key(*key)
                                    })
                                    .unwrap_or(false)
                            })
                            .cloned()
                            .collect();
                        if !wanted.is_empty() {
                            jobs.push((*message_key, wanted));
                        }
                    }
                    jobs
                };

                for (message_key, wanted) in jobs {
                    {
                        let state = inner.lock_state();
                        if state.closed {
                            return;
                        }
                    }
                    match inner.build_for_message(message_key, &wanted) {
                        Ok(built) => {
                            let mut state = inner.lock_state();
                            for (key, entry) in built {
                                state.pyramids.insert(key, Arc::new(entry));
                            }
                        }
                        Err(err) => {
                            let mut state = inner.lock_state();
                            state
                                .warnings
                                .push(format!("构建包络金字塔失败：{}", err.message));
                        }
                    }
                }

                let mut state = inner.lock_state();
                state.prefetch.done = state.prefetch.done.saturating_add(batch.len() as u64);
                state.prefetch.message = if state.prefetch_queue.is_empty() {
                    "预取完成".to_string()
                } else {
                    "继续预取…".to_string()
                };
            }
            Err(err) => {
                let mut state = inner.lock_state();
                state.prefetch.busy = false;
                state.prefetch.message = err.to_string();
                state.warnings.push(err.to_string());
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTML 报告
// ---------------------------------------------------------------------------

fn export_html_report(
    inner: &Arc<SessionInner>,
    path: &str,
    title: &str,
) -> Result<u64, CoreError> {
    let (time_info, active, files, signal_meta) = {
        let state = inner.lock_state();
        if state.closed {
            return Err(CoreError::bad_config("PlotSession 已关闭"));
        }
        let signal_meta: BTreeMap<String, (String, Option<String>)> = inner
            .catalog
            .iter()
            .map(|signal| {
                (
                    signal.key.clone(),
                    (signal.signal.name.clone(), signal.unit.clone()),
                )
            })
            .collect();
        (
            TimeInfo::from_meta(&state.meta, state.time_format),
            state.active.iter().cloned().collect::<Vec<_>>(),
            state.meta.files.len(),
            signal_meta,
        )
    };
    let stats = inner.stats_impl(&active)?;
    let stats_by_key: BTreeMap<String, SignalStats> = stats
        .iter()
        .map(|(key, value)| (key.clone(), *value))
        .collect();

    // 报告与界面走同一条取数路径：从 view(t_min, t_max, width) 拿抽稀包络，
    // 绝不在这里另算一套桶。先按契约推荐的 width=2000 取；极端多信号导致
    // 超过 8 MiB 时才逐级降宽（仍是 view，不改变口径），保证报告始终可离线打开。
    let mut html = String::new();
    for width in [2000usize, 1200, 600, 300, 150, 75, 32, 8] {
        let view = inner.compute_view(time_info.t_min, time_info.t_max, width)?;
        html = render_html(
            title,
            &time_info,
            files,
            &view,
            &stats,
            &stats_by_key,
            &signal_meta,
        );
        if html.len() <= 8 * 1024 * 1024 {
            return write_html_report(path, &html);
        }
    }
    Err(CoreError::export_failed(format!(
        "HTML 报告超过 8 MiB 上限（当前 {} 字节）",
        html.len()
    )))
}

fn write_html_report(path: &str, html: &str) -> Result<u64, CoreError> {
    let target = PathBuf::from(path);
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| {
                CoreError::export_failed(format!(
                    "创建 HTML 报告目录失败：{}（{err}）",
                    parent.display()
                ))
            })?;
        }
    }
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("report.html");
    let tmp = target.with_file_name(format!(".{file_name}.tmp"));
    std::fs::write(&tmp, html.as_bytes()).map_err(|err| {
        CoreError::export_failed(format!("写入 HTML 报告失败：{}（{err}）", tmp.display()))
    })?;
    std::fs::rename(&tmp, &target).map_err(|err| {
        CoreError::export_failed(format!("替换 HTML 报告失败：{}（{err}）", target.display()))
    })?;
    Ok(html.len() as u64)
}

/// 全精度数字文本（Rust 的 f64 显示是 round-trip 最短形式，不截断有效数字）。
fn number_text(value: f64) -> String {
    serde_json::json!(value).to_string()
}

fn render_html(
    title: &str,
    info: &TimeInfo,
    files: usize,
    view: &ViewData,
    stats: &[(String, SignalStats)],
    stats_by_key: &BTreeMap<String, SignalStats>,
    signal_meta: &BTreeMap<String, (String, Option<String>)>,
) -> String {
    let series_json: Vec<serde_json::Value> = view
        .series
        .iter()
        .map(|series| {
            let stat = stats_by_key.get(&series.key).copied().unwrap_or_default();
            let (signal, unit) = signal_meta
                .get(&series.key)
                .cloned()
                .unwrap_or_else(|| (series.key.clone(), None));
            json!({
                "key": &series.key,
                "signal": signal,
                "unit": unit,
                // 契约：每条曲线都带自己的 t；与 view.t 完全一致（同一桶网格）。
                "t": &view.t,
                "min": &series.min,
                "max": &series.max,
                "mean": &series.mean,
                "count": stat.count,
                "vmin": stat.vmin,
                "vmax": stat.vmax,
                "mean_all": stat.mean,
                "first": stat.first,
                "last": stat.last,
            })
        })
        .collect();
    let axis_json: Vec<serde_json::Value> = view
        .t_axis
        .iter()
        .map(|tick| json!({"t": tick.t, "label": &tick.label}))
        .collect();
    let data = json!({
        "title": title,
        "files": files,
        "t_min": info.t_min,
        "t_max": info.t_max,
        "t_min_str": info.t_min_str,
        "t_max_str": info.t_max_str,
        "t": &view.t,
        "series": series_json,
        "t_axis": axis_json,
        "truncated": view.truncated,
    });
    // JSON 中不得出现 `</script>`，把所有 `<` 转义掉。
    let data_text = data.to_string().replace('<', "\\u003c");
    let safe_title = escape_html(title);
    let absolute_line = match (&info.t_min_str, &info.t_max_str) {
        (Some(start), Some(end)) => format!(
            "<div class=\"meta\">绝对时间（BLF 头墙钟）：{} ~ {}</div>",
            escape_html(start),
            escape_html(end)
        ),
        _ => String::new(),
    };
    let stats_rows: String = stats
        .iter()
        .map(|(key, stat)| {
            let (signal, unit) = signal_meta
                .get(key)
                .cloned()
                .unwrap_or_else(|| (key.clone(), None));
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape_html(key),
                escape_html(&signal),
                escape_html(unit.as_deref().unwrap_or("")),
                stat.count,
                number_text(stat.vmin),
                number_text(stat.vmax),
                number_text(stat.mean),
            )
        })
        .collect();
    let points = view.t.len();
    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{safe_title}</title>
<style>
body{{font-family:-apple-system,Segoe UI,Roboto,"Helvetica Neue",Arial,sans-serif;margin:0;background:#0f1115;color:#e8eaed}}
header{{padding:18px 24px;border-bottom:1px solid #262a33}}
h1{{font-size:18px;margin:0 0 6px}} .meta{{color:#9aa0a6;font-size:12px;line-height:1.7}}
table.stats{{margin-top:12px;border-collapse:collapse;font-size:12px;color:#cfd3d8}}
table.stats th,table.stats td{{border:1px solid #2a2f3a;padding:4px 8px;text-align:right;white-space:nowrap}}
table.stats th{{background:#191d24;color:#e8eaed}} table.stats td:first-child,table.stats td:nth-child(2),table.stats td:nth-child(3){{text-align:left}}
main{{padding:16px 24px 40px}} .lane{{margin:18px 0;border:1px solid #262a33;border-radius:8px;overflow:hidden;background:#14171d}}
.lane h2{{font-size:13px;margin:0;padding:8px 12px;border-bottom:1px solid #262a33;color:#cfd3d8}}
.lane canvas{{display:block;width:100%;height:200px}} .empty{{padding:30px;color:#9aa0a6}}
</style></head><body>
<header><h1>{safe_title}</h1>
<div class="meta">文件数：{files} ／ 时间范围：{t_min:.6} s ~ {t_max:.6} s ／ 已抽稀为 {points} 桶（min/max 包络，不丢峰）</div>
{absolute_line}
<table class="stats"><thead><tr><th>key</th><th>signal</th><th>unit</th><th>count</th><th>vmin</th><th>vmax</th><th>mean</th></tr></thead><tbody>{stats_rows}</tbody></table>
</header>
<main id="lanes"></main>
<script>
const DATA={data_text};
const lanes=document.getElementById('lanes');
if(!DATA.series.length){{lanes.innerHTML='<div class="empty">当前会话没有可绘制的信号。</div>';}}
const PALETTE=['#4dabf7','#f783ac','#ffd43b','#69db7c','#b197fc','#ffa94d','#63e6be','#ff6b6b'];
function fmt(v){{return Number.isFinite(v)?v.toPrecision(12):'-';}}
DATA.series.forEach((s,index)=>{{
  const box=document.createElement('div');box.className='lane';
  const head=document.createElement('h2');
  head.textContent=s.key+'  ·  '+s.signal+(s.unit?' ['+s.unit+']':'')+'  ·  '+s.count+' 点  ·  min='+fmt(s.vmin)+' max='+fmt(s.vmax)+' mean='+fmt(s.mean_all);
  const canvas=document.createElement('canvas');canvas.width=1200;canvas.height=200;
  box.appendChild(head);box.appendChild(canvas);lanes.appendChild(box);
  const ctx=canvas.getContext('2d');const W=canvas.width,H=canvas.height,pad=34;
  let lo=Infinity,hi=-Infinity;
  for(const v of s.min){{if(Number.isFinite(v)){{lo=Math.min(lo,v);hi=Math.max(hi,v);}}}}
  for(const v of s.max){{if(Number.isFinite(v)){{lo=Math.min(lo,v);hi=Math.max(hi,v);}}}}
  if(!Number.isFinite(lo)){{lo=0;hi=1;}} if(hi===lo){{hi=lo+1;}}
  const t0=DATA.t_min,t1=DATA.t_max,dt=(t1-t0)||1;
  const x=t=>pad+(t-t0)/dt*(W-pad*2);
  const y=v=>H-pad-(v-lo)/(hi-lo)*(H-pad*2);
  ctx.clearRect(0,0,W,H);ctx.strokeStyle='#2a2f3a';ctx.lineWidth=1;
  ctx.beginPath();ctx.moveTo(pad,pad);ctx.lineTo(pad,H-pad);ctx.lineTo(W-pad,H-pad);ctx.stroke();
  const color=PALETTE[index%PALETTE.length];
  const ts=(s.t&&s.t.length)?s.t:(DATA.t||[]);
  if(ts.length){{
    ctx.fillStyle=color+'55';
    ctx.beginPath();
    ts.forEach((t,i)=>{{const v=s.max[i];if(Number.isFinite(v))ctx.lineTo(x(t),y(v));}});
    for(let i=ts.length-1;i>=0;i--){{const v=s.min[i];if(Number.isFinite(v))ctx.lineTo(x(ts[i]),y(v));}}
    ctx.closePath();ctx.fill();
    ctx.strokeStyle=color;ctx.lineWidth=1.4;ctx.beginPath();
    ts.forEach((t,i)=>{{const v=s.mean[i];if(Number.isFinite(v)){{if(i===0)ctx.moveTo(x(t),y(v));else ctx.lineTo(x(t),y(v));}}}});
    ctx.stroke();
  }}
  ctx.fillStyle='#9aa0a6';ctx.font='11px monospace';ctx.textAlign='left';ctx.textBaseline='middle';
  ctx.fillText(fmt(hi),4,pad-10);ctx.fillText(fmt(lo),4,H-pad+10);
  ctx.font='10px monospace';ctx.textAlign='center';ctx.textBaseline='top';
  const axis=DATA.t_axis||[];
  if(axis.length){{
    axis.forEach(tick=>{{
      const px=x(tick.t);
      ctx.save();ctx.translate(px,H-pad+6);ctx.rotate(-Math.PI/8);
      ctx.fillText(tick.label,0,0);ctx.restore();
    }});
  }} else {{
    for(let i=0;i<=5;i++){{const tv=t0+dt*i/5;ctx.fillText(tv.toPrecision(6)+'s',x(tv),H-pad+6);}}
  }}
}});
</script></body></html>"#,
        safe_title = safe_title,
        files = files,
        t_min = info.t_min,
        t_max = info.t_max,
        points = points,
        absolute_line = absolute_line,
        stats_rows = stats_rows,
        data_text = data_text,
    )
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

// ---------------------------------------------------------------------------
// Python 参数解析
// ---------------------------------------------------------------------------

fn extract_keys(items: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    if items.is_none() {
        return Ok(Vec::new());
    }
    if let Ok(text) = items.extract::<String>() {
        return Ok(vec![text]);
    }
    if let Ok(dict) = items.cast::<PyDict>() {
        return Ok(key_from_dict(dict).into_iter().collect());
    }
    let mut keys = Vec::new();
    for item in items.try_iter()? {
        let item = item?;
        if let Ok(text) = item.extract::<String>() {
            keys.push(text);
        } else if let Ok(dict) = item.cast::<PyDict>() {
            if let Some(key) = key_from_dict(dict) {
                keys.push(key);
            }
        }
    }
    Ok(keys)
}

fn key_from_dict(dict: &Bound<'_, PyDict>) -> Option<String> {
    if let Ok(Some(value)) = dict.get_item("key") {
        if let Ok(text) = value.extract::<String>() {
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    let channel = dict
        .get_item("channel")
        .ok()
        .flatten()
        .and_then(|value| value.extract::<u32>().ok())?;
    let message = dict
        .get_item("message")
        .ok()
        .flatten()
        .and_then(|value| value.extract::<String>().ok())?;
    let signal = dict
        .get_item("signal")
        .ok()
        .flatten()
        .and_then(|value| value.extract::<String>().ok())?;
    Some(format!("ch{channel}/{message}/{signal}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_level(bucket_size: u64, buckets: usize) -> Level {
        Level {
            bucket_size,
            min: vec![0.0; buckets],
            max: vec![0.0; buckets],
            sum: vec![0.0; buckets],
            count: vec![0; buckets],
        }
    }

    fn pyramid_for(frames: u64) -> SignalPyramid {
        let mut levels = vec![flat_level(10, frames.div_ceil(10) as usize)];
        while levels.last().map(|level| level.len() > 64).unwrap_or(false) {
            let previous = match levels.last() {
                Some(previous) => previous,
                None => break,
            };
            levels.push(flat_level(
                previous.bucket_size.saturating_mul(10),
                previous.len().div_ceil(10),
            ));
        }
        SignalPyramid { levels }
    }

    /// 同一 width 下，窗口越大必须选到越粗的层（bucket_size 单调不减）。
    #[test]
    fn wider_window_chooses_coarser_level() {
        let pyramid = pyramid_for(1_000_000);
        let width = 200usize;
        let mut previous_size = 0u64;
        for (i0, i1) in [(0u64, 9_999u64), (0, 99_999), (0, 999_999)] {
            let (index, _, _, nb) = choose_level(&pyramid, i0, i1, width);
            let size = pyramid.levels[index].bucket_size;
            assert!(nb >= width as u64, "窗口 {i0}..{i1} 桶数 {nb} 应 ≥ {width}");
            assert!(
                size >= previous_size,
                "窗口变大后层必须更粗：{previous_size} -> {size}"
            );
            previous_size = size;
        }
        assert!(previous_size >= 100, "最宽窗口应使用 ≥100 帧/桶的层");
    }

    /// 窗口内桶数不足 width 时退到最粗层，且不会返回空数据。
    #[test]
    fn tiny_window_falls_back_to_coarsest_level() {
        let pyramid = pyramid_for(1_000_000);
        let (index, b0, b1, nb) = choose_level(&pyramid, 500, 600, 2000);
        assert_eq!(index, pyramid.levels.len() - 1);
        assert!(nb >= 1);
        assert!(b1 >= b0);
    }

    #[test]
    fn f32_envelope_never_shrinks_real_extrema() {
        let values = [
            1.234_567_890_123_f64,
            -987_654.321_f64,
            0.1,
            f64::MIN_POSITIVE,
        ];
        for value in values {
            let min = f32_min_cover(value);
            let max = f32_max_cover(value);
            assert!((min as f64) <= value, "min {min} 未覆盖 {value}");
            assert!((max as f64) >= value, "max {max} 未覆盖 {value}");
        }
    }
}
