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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// 包络金字塔常驻内存上限（生产值）：256 MiB。
pub const PYRAMID_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// 实际生效的预算。
///
/// 单元测试把它压到 1 MiB，否则触发淘汰需要上亿个样本；生产构建永远是
/// [`PYRAMID_BUDGET_BYTES`]（`cfg(test)` 不参与库的正常编译）。
#[cfg(test)]
pub(crate) const PYRAMID_BUDGET_LIMIT: u64 = 1024 * 1024;
#[cfg(not(test))]
pub(crate) const PYRAMID_BUDGET_LIMIT: u64 = PYRAMID_BUDGET_BYTES;

const MIB: u64 = 1024 * 1024;

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

    /// 合并一个样本/桶；**拒绝** NaN/inf：
    /// 只要 min/max/sum 任一非有限就整样本跳过，`mean` 永远不会被 NaN 污染。
    fn merge(&mut self, min: f32, max: f32, sum: f64, count: u64) {
        if count == 0 {
            return;
        }
        if !min.is_finite() || !max.is_finite() || !sum.is_finite() {
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
    /// 不可变快照（`Arc`）：`compute_view` 只在锁内 clone 它，真正读盘全在锁外。
    meta: Arc<CacheMeta>,
    dir: PathBuf,
    cache_hit: bool,
    /// 本会话（open + 后台预取）因目录上限 LRU 删除的其它 key 字节数。
    evicted_bytes: u64,
    active: BTreeSet<String>,
    pyramids: HashMap<String, Arc<PyramidEntry>>,
    /// `pyramids` 的字节账本（与插入/淘汰一一对应，`info()` 直接读它）。
    pyramid_bytes: u64,
    /// LRU：key → 最后使用（Unix 毫秒）。
    pyramid_used: HashMap<String, u64>,
    /// 曾经建成的 key（用于区分「首次构建」与「淘汰后重建」）。
    pyramid_seen: std::collections::HashSet<String>,
    /// 累计被淘汰的信号数 / 字节数 / 重建次数（`warnings` 汇总后上报）。
    pyramid_evictions: u64,
    pyramid_evicted_bytes: u64,
    pyramid_rebuilds: u64,
    time_format: TimeFormat,
    prefetch: PrefetchStatus,
    prefetch_queue: BTreeSet<(u32, u32)>,
    warnings: Vec<String>,
    /// `view()` 真实读盘/解码错误累计数（A6：不许静默）。
    view_errors: u64,
    /// 首次读盘/解码错误的描述（含段文件名）。
    view_error_first: Option<String>,
    closed: bool,
}

impl SessionState {
    /// 记一笔金字塔占用的字节（插入/替换都走这里），随后按 LRU 预算淘汰。
    ///
    /// `protect` 是「本次 `view()` / `stats()` 正在使用的信号」，绝不淘汰。
    fn insert_pyramid(&mut self, key: String, entry: PyramidEntry, protect: &BTreeSet<String>) {
        let bytes = entry.pyramid.bytes() as u64;
        if let Some(previous) = self.pyramids.insert(key.clone(), Arc::new(entry)) {
            self.pyramid_bytes = self
                .pyramid_bytes
                .saturating_sub(previous.pyramid.bytes() as u64);
        }
        if !self.pyramid_seen.insert(key.clone()) {
            // 相同 key 已经有金字塔 → 这是淘汰后的按需重建。
            self.pyramid_rebuilds = self.pyramid_rebuilds.saturating_add(1);
        }
        self.pyramid_bytes = self.pyramid_bytes.saturating_add(bytes);
        self.pyramid_used.insert(key.clone(), now_millis());
        self.enforce_pyramid_budget(&key, protect);
    }

    /// 刷新 LRU 时间戳（信号被 `view()` / `stats()` 真正用到时调用）。
    fn touch_pyramid(&mut self, key: &str) {
        if self.pyramids.contains_key(key) {
            let now = now_millis();
            self.pyramid_used.insert(key.to_string(), now);
        }
    }

    /// 常驻金字塔字节超预算 → 按 LRU 淘汰**其它**信号。
    ///
    /// 若剩余候选全在 `protect` / `keep` 里（本次调用本身就需要它们），
    /// 宁可不淘汰 —— 正确性优先于预算，且不写告警噪音。
    fn enforce_pyramid_budget(&mut self, keep: &str, protect: &BTreeSet<String>) {
        let mut evicted = 0u64;
        let mut evicted_bytes = 0u64;
        while self.pyramid_bytes > PYRAMID_BUDGET_LIMIT {
            let victim = self
                .pyramid_used
                .iter()
                .filter(|(key, _)| key.as_str() != keep && !protect.contains(key.as_str()))
                .min_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)))
                .map(|(key, _)| key.clone());
            let Some(victim) = victim else {
                break;
            };
            if let Some(entry) = self.pyramids.remove(&victim) {
                let bytes = entry.pyramid.bytes() as u64;
                self.pyramid_bytes = self.pyramid_bytes.saturating_sub(bytes);
                evicted = evicted.saturating_add(1);
                evicted_bytes = evicted_bytes.saturating_add(bytes);
            }
            self.pyramid_used.remove(&victim);
        }
        if evicted > 0 {
            self.pyramid_evictions = self.pyramid_evictions.saturating_add(evicted);
            self.pyramid_evicted_bytes = self
                .pyramid_evicted_bytes
                .saturating_add(evicted_bytes);
        }
    }
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
    /// `.inuse-<pid>-<nonce>` 租约文件；`close`/`Drop` 时尽力删。
    lease: Mutex<Option<PathBuf>>,
    /// `last_used_at` 刷新节流（Unix 毫秒）。
    last_touch_millis: AtomicU64,
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

    fn lock_lease(&self) -> std::sync::MutexGuard<'_, Option<PathBuf>> {
        self.lease
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// `close()` 的实体：纯内存状态 + 删一个租约文件，**不做任何长 I/O**。
    ///
    /// 预取线程在每个进度检查点（≈ 50 ms）看 `closed`，因此它会在下一次检查点退出。
    fn close_impl(&self) {
        {
            let mut state = self.lock_state();
            state.closed = true;
            state.active.clear();
            state.pyramids.clear();
            state.pyramid_bytes = 0;
            state.pyramid_used.clear();
            state.pyramid_seen.clear();
            state.prefetch_queue.clear();
            state.prefetch.busy = false;
            state.prefetch.message = "已关闭会话".to_string();
        }
        let lease = self.lock_lease().take();
        if let Some(path) = lease {
            frame_cache::release_lease(&path);
        }
    }

    /// 锁内只取内存快照（供 `info()`），锁全部在 `py.detach` 内取。
    fn info_snapshot(&self) -> InfoSnapshot {
        let state = self.lock_state();
        let mut warnings = state.warnings.clone();
        if let Some(first) = &state.view_error_first {
            warnings.push(format!(
                "视图读取共 {} 个错误（首次：{}）",
                state.view_errors, first
            ));
        }
        // B2：金字塔淘汰 / 重建都必须上报（正常命中时一条都不多）。
        if state.pyramid_evictions > 0 || state.pyramid_rebuilds > 0 {
            warnings.push(format!(
                "包络金字塔超 {} MiB 上限：LRU 已淘汰 {} 个信号（共 {} 字节），已按需重建 {} 次",
                PYRAMID_BUDGET_LIMIT / MIB,
                state.pyramid_evictions,
                state.pyramid_evicted_bytes,
                state.pyramid_rebuilds
            ));
        }
        InfoSnapshot {
            time_info: TimeInfo::from_meta(&state.meta, state.time_format),
            files: state
                .meta
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect(),
            cache_bytes: state.meta.cache_bytes,
            cache_hit: state.cache_hit,
            evicted_bytes: state.evicted_bytes,
            warnings,
            message_set: state
                .meta
                .messages
                .iter()
                .map(|message| (message.channel, message.frame_id))
                .collect(),
            pyramid_bytes: state.pyramid_bytes,
            pyramid_budget_bytes: PYRAMID_BUDGET_LIMIT,
            prefetch_busy: state.prefetch.busy,
        }
    }

    /// 金字塔「按需重建」入口：把 active 里**已缓存、但金字塔被 LRU 淘汰**的信号补回来。
    ///
    /// * 只重建 `meta` 里已有报文段的信号（未缓存报文仍走预取，`view()` 记为 pending）；
    /// * 失败只记 warning，不把 `view()` 变成异常（与预取路径一致）；
    /// * 本次调用重建的全部信号都受保护，不会被自己这次插入挤掉。
    fn ensure_active_pyramids(&self) {
        let jobs: BTreeMap<(u32, u32), Vec<String>> = {
            let state = self.lock_state();
            if state.closed {
                return;
            }
            let mut jobs: BTreeMap<(u32, u32), Vec<String>> = BTreeMap::new();
            for key in &state.active {
                if state.pyramids.contains_key(key) {
                    continue;
                }
                let Some(&index) = self.catalog_by_key.get(key) else {
                    continue;
                };
                let catalog = &self.catalog[index];
                if !state.meta.has_message(catalog.channel, catalog.frame_id) {
                    continue;
                }
                jobs.entry((catalog.channel, catalog.frame_id))
                    .or_default()
                    .push(key.clone());
            }
            jobs
        };
        if jobs.is_empty() {
            return;
        }
        let protect: BTreeSet<String> = jobs.values().flatten().cloned().collect();
        for (message_key, wanted) in jobs {
            match self.build_for_message(message_key, &wanted) {
                Ok(built) => {
                    let mut state = self.lock_state();
                    if state.closed {
                        return;
                    }
                    for (key, entry) in built {
                        state.insert_pyramid(key, entry, &protect);
                    }
                }
                Err(err) => {
                    let mut state = self.lock_state();
                    state
                        .warnings
                        .push(format!("重建包络金字塔失败：{}", err.message));
                }
            }
        }
    }

    /// 节流刷新 `meta.last_used_at`（≤ 60 s 一次）；抢不到 key 锁就放弃，绝不阻塞 `view`。
    fn touch_if_due(&self) {
        let now = now_millis();
        let previous = self.last_touch_millis.load(Ordering::Relaxed);
        if now.saturating_sub(previous) < frame_cache::TOUCH_INTERVAL_MILLIS {
            return;
        }
        if self
            .last_touch_millis
            .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let _ = frame_cache::touch_meta(
            &self.cache_dir,
            &self.key,
            Duration::from_millis(150),
        );
    }

    /// 记录 `view()` 期间的真实错误（锁内只改两个字段，无 I/O）。
    fn record_view_errors(&self, diag: &ViewDiag) {
        if diag.errors == 0 {
            return;
        }
        let mut state = self.lock_state();
        state.view_errors = state.view_errors.saturating_add(diag.errors);
        if state.view_error_first.is_none() {
            state.view_error_first = diag.first.clone();
        }
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

    /// 计算一个窗口的抽稀包络。
    ///
    /// **锁粒度**：锁内只拿一份快照（`Arc<CacheMeta>` + 金字塔句柄 + 时间参数），
    /// 立即放锁；所有 `read_at` / 解码都在锁外进行。因此预取线程持锁时，
    /// `view()` 不会把锁竞争带回到持有 GIL 的 UI 线程上。
    fn compute_view(&self, t0: f64, t1: f64, width_px: usize) -> Result<ViewData, CoreError> {
        // B2：金字塔被 LRU 淘汰后，在本次 view 前按需重建（只重建已缓存报文）。
        self.ensure_active_pyramids();
        let snapshot = self.view_snapshot();
        let time_info = snapshot.time_info.clone();
        if snapshot.closed || width_px == 0 {
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
        let meta = Arc::clone(&snapshot.meta);
        let format = snapshot.format;
        let raw_min = if meta.t_min.is_finite() {
            meta.t_min
        } else {
            0.0
        };
        let raw_t0 = to_raw_seconds(t0, raw_min, format);
        let raw_t1 = to_raw_seconds(t1, raw_min, format);
        let span = t1 - t0;

        struct PendingSeries {
            key: String,
            values: Vec<Option<BucketAgg>>,
        }
        let mut pending: Vec<PendingSeries> = Vec::new();
        let mut diag = ViewDiag::default();

        // 锁外：打开段文件 / 二分定位 / 逐桶读盘。
        for (message_key, entries) in &snapshot.by_message {
            if entries.is_empty() {
                continue;
            }
            let Some(message_meta) = meta.message(message_key.0, message_key.1) else {
                diag.record(format!(
                    "缓存 meta 缺少报文段 Ch{}/id{:#x}",
                    message_key.0, message_key.1
                ));
                continue;
            };
            let path = message_meta.path(&snapshot.dir);
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(err) => {
                    diag.record(format!(
                        "打开帧缓存失败：{}（{err}）",
                        message_meta.file
                    ));
                    continue;
                }
            };
            let stride = message_meta.stride as usize;
            let boundaries = match time_boundaries(
                &file,
                stride,
                message_meta.frames,
                raw_t0,
                raw_t1,
                n_out,
            ) {
                Ok(boundaries) => boundaries,
                Err(err) => {
                    diag.record(format!("{}（{}）", err.message, message_meta.file));
                    continue;
                }
            };
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
                let aggs = exact_range_multi(
                    levels,
                    &file,
                    message_meta,
                    &signals,
                    start,
                    end,
                    &mut diag,
                );
                for (index, agg) in aggs.into_iter().enumerate() {
                    if let Some(series) = values.get_mut(index) {
                        series.push(if agg.count == 0 { None } else { Some(agg) });
                    }
                }
            }
            for (index, (key, _, _)) in entries.iter().enumerate() {
                let values = values
                    .get_mut(index)
                    .map(std::mem::take)
                    .unwrap_or_default();
                pending.push(PendingSeries {
                    key: key.clone(),
                    values,
                });
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

        // A7：`truncated` **只**由“真缺数据”（金字塔未就绪 / 读盘或解码出错）置位；
        // 稀疏（桶为空但数据可信）不再算 truncated。
        let truncated = snapshot.pending > 0 || diag.errors > 0;
        self.record_view_errors(&diag);
        self.touch_if_due();
        Ok(ViewData {
            t: out_t,
            series: out_series,
            truncated,
            t_min_str: time_info.t_min_str,
            t_max_str: time_info.t_max_str,
            t_axis: time_info.t_axis,
        })
    }

    /// 锁内取一份轻量快照：`meta` 是 `Arc`，金字塔是 `Arc`，全程零 I/O。
    fn view_snapshot(&self) -> ViewSnapshot {
        let mut state = self.lock_state();
        let mut by_message: BTreeMap<
            (u32, u32),
            Vec<(String, Arc<PyramidEntry>, Arc<SignalPlan>)>,
        > = BTreeMap::new();
        let mut pending = 0u64;
        let used: Vec<String> = state
            .active
            .iter()
            .filter(|key| state.pyramids.contains_key(*key))
            .cloned()
            .collect();
        // 本次 view 真正用到的信号刷新 LRU 时间戳，避免“正在看的信号”被后续淘汰。
        for key in &used {
            state.touch_pyramid(key);
        }
        for key in &state.active {
            let Some(&index) = self.catalog_by_key.get(key) else {
                continue;
            };
            let catalog = &self.catalog[index];
            match state.pyramids.get(key) {
                Some(entry) => by_message
                    .entry((catalog.channel, catalog.frame_id))
                    .or_default()
                    .push((key.clone(), Arc::clone(entry), Arc::clone(&catalog.signal))),
                None => pending = pending.saturating_add(1),
            }
        }
        ViewSnapshot {
            closed: state.closed,
            time_info: TimeInfo::from_meta(&state.meta, state.time_format),
            dir: state.dir.clone(),
            format: state.time_format,
            meta: Arc::clone(&state.meta),
            by_message,
            pending,
        }
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
            let protect: BTreeSet<String> = wanted.into_iter().collect();
            let mut state = self.lock_state();
            for (key, entry) in built {
                state.insert_pyramid(key, entry, &protect);
            }
        }

        if need_prefetch {
            start_prefetch(Arc::clone(&self));
        }
        Ok((added, pending))
    }

    /// 全量统计。
    ///
    /// 严格性（A8）：
    /// * 未知 key → `BAD_CONFIG`，消息里点名每个不存在的 key（绝不静默少一条）；
    /// * 已知但尚未缓存/金字塔未就绪 → 占位行（`count = 0`, `pending = true`），
    ///   与入参**等长**且**同序**，调用方按位置对齐不会错位。
    fn stats_impl(&self, keys: &[String]) -> Result<Vec<StatRow>, CoreError> {
        let wanted: Vec<String> = if keys.is_empty() {
            let state = self.lock_state();
            state.active.iter().cloned().collect()
        } else {
            keys.to_vec()
        };
        let unknown: Vec<&str> = wanted
            .iter()
            .filter(|key| !self.catalog_by_key.contains_key(key.as_str()))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(CoreError::bad_config(format!(
                "未知信号 key：{}（请用 info().signals[*].key 的取值）",
                unknown.join("、")
            )));
        }

        let mut out = Vec::with_capacity(wanted.len());
        for key in wanted {
            let entry = {
                let mut state = self.lock_state();
                let entry = state.pyramids.get(&key).cloned();
                // 本次 stats 用到的信号刷新 LRU。
                state.touch_pyramid(&key);
                entry
            };
            // 未知已在上面拦截，这里的 `else` 理论上不会走到，但绝不允许静默少一条。
            let Some(&index) = self.catalog_by_key.get(&key) else {
                out.push(StatRow {
                    key,
                    stats: SignalStats::default(),
                    pending: true,
                });
                continue;
            };
            let entry = match entry {
                Some(entry) => Some(entry),
                None => {
                    let catalog = &self.catalog[index];
                    let built = self
                        .build_for_message((catalog.channel, catalog.frame_id), &[key.clone()])?;
                    let protect: BTreeSet<String> = [key.clone()].into_iter().collect();
                    let mut state = self.lock_state();
                    let mut found = None;
                    for (built_key, entry) in built {
                        let tracked = built_key == key;
                        state.insert_pyramid(built_key, entry, &protect);
                        if tracked {
                            // `key` 在 protect 里，不会被自己这次插入挤掉。
                            found = state.pyramids.get(&key).cloned();
                        }
                    }
                    found
                }
            };
            match entry {
                Some(entry) => out.push(StatRow {
                    key,
                    stats: entry.stats,
                    pending: false,
                }),
                // 已知 key 但报文段还没缓存 / 金字塔未就绪：占位而不是跳过。
                None => out.push(StatRow {
                    key,
                    stats: SignalStats::default(),
                    pending: true,
                }),
            }
        }
        Ok(out)
    }
}

/// `compute_view` 的锁内快照：全部是 `Arc` / 克隆句柄，不包含任何文件 I/O。
struct ViewSnapshot {
    closed: bool,
    time_info: TimeInfo,
    dir: PathBuf,
    format: TimeFormat,
    meta: Arc<CacheMeta>,
    by_message: BTreeMap<(u32, u32), Vec<(String, Arc<PyramidEntry>, Arc<SignalPlan>)>>,
    /// `active` 中尚无金字塔的信号数（未就绪 = 真缺数据）。
    pending: u64,
}

/// `view()` 期间的读盘/解码错误记账（A6：不许静默）。
#[derive(Debug, Default)]
struct ViewDiag {
    errors: u64,
    first: Option<String>,
}

impl ViewDiag {
    fn record(&mut self, text: String) {
        self.errors = self.errors.saturating_add(1);
        if self.first.is_none() {
            self.first = Some(text);
        }
    }
}

/// `info()` 的锁内快照。
struct InfoSnapshot {
    time_info: TimeInfo,
    files: Vec<String>,
    cache_bytes: u64,
    cache_hit: bool,
    evicted_bytes: u64,
    warnings: Vec<String>,
    message_set: BTreeSet<(u32, u32)>,
    pyramid_bytes: u64,
    /// 当前生效的预算（测试下会被压小，见 [`PYRAMID_BUDGET_LIMIT`]）。
    pyramid_budget_bytes: u64,
    prefetch_busy: bool,
}

/// `stats()` 的一行：未就绪时 `pending = true` 且 `count == 0`（保持与入参等长）。
#[derive(Debug, Clone)]
struct StatRow {
    key: String,
    stats: SignalStats,
    pending: bool,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
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

    // A3：在 key 目录写一份租约，`enforce_lru` 看到任何 `.inuse-*` 就跳过该 key。
    let lease = frame_cache::acquire_lease(&opened.dir);

    let state = SessionState {
        meta: Arc::new(opened.meta),
        dir: opened.dir,
        cache_hit: opened.cache_hit,
        evicted_bytes,
        active: BTreeSet::new(),
        pyramids: HashMap::new(),
        pyramid_bytes: 0,
        pyramid_used: HashMap::new(),
        pyramid_seen: std::collections::HashSet::new(),
        pyramid_evictions: 0,
        pyramid_evicted_bytes: 0,
        pyramid_rebuilds: 0,
        time_format: config.time_format,
        prefetch: PrefetchStatus::default(),
        prefetch_queue: BTreeSet::new(),
        warnings,
        view_errors: 0,
        view_error_first: None,
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
        lease: Mutex::new(lease),
        last_touch_millis: AtomicU64::new(now_millis()),
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

impl Drop for SessionInner {
    fn drop(&mut self) {
        // 会话对象被释放时也尽力清租约（`close()` 已清过则这里是 None）。
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if let Some(path) = lease {
            frame_cache::release_lease(&path);
        }
    }
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
    ///
    /// 锁全在 `py.detach` 里拿：预取线程持 state 锁时，UI 线程调用 `info()`
    /// 不会带着 GIL 等锁（那会把 Qt 事件循环一起冻住）。
    pub fn info<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let inner = Arc::clone(&self.inner);
        let snapshot = py.detach(move || inner.info_snapshot());
        let out = PyDict::new(py);
        let time_info = &snapshot.time_info;
        out.set_item("t_min", time_info.t_min)?;
        out.set_item("t_max", time_info.t_max)?;
        out.set_item("t_min_str", time_info.t_min_str.as_deref())?;
        out.set_item("t_max_str", time_info.t_max_str.as_deref())?;
        out.set_item("t_axis", axis_to_pylist(py, &time_info.t_axis)?)?;
        out.set_item("files", snapshot.files)?;
        out.set_item("cache_bytes", snapshot.cache_bytes)?;
        out.set_item("cache_hit", snapshot.cache_hit)?;
        out.set_item("evicted_bytes", snapshot.evicted_bytes)?;

        let signals = PyList::empty(py);
        for signal in &self.inner.catalog {
            let item = PyDict::new(py);
            item.set_item("channel", signal.channel)?;
            item.set_item("message", &signal.message)?;
            item.set_item("signal", &signal.signal.name)?;
            item.set_item("unit", signal.unit.as_deref())?;
            item.set_item(
                "cached",
                snapshot
                    .message_set
                    .contains(&(signal.channel, signal.frame_id)),
            )?;
            // 全链路唯一标识，与 stats/add_signals/remove_signals 入参一致。
            item.set_item("key", &signal.key)?;
            signals.append(item)?;
        }
        out.set_item("signals", signals)?;
        out.set_item("warnings", snapshot.warnings)?;
        out.set_item("pyramid_bytes", snapshot.pyramid_bytes)?;
        out.set_item("pyramid_budget_bytes", snapshot.pyramid_budget_bytes)?;
        out.set_item("prefetch_busy", snapshot.prefetch_busy)?;
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
        let inner = Arc::clone(&self.inner);
        let applied = Python::try_attach(|py| {
            let inner = Arc::clone(&inner);
            py.detach(move || {
                let mut state = inner.lock_state();
                state.time_format = parsed;
            })
        });
        if applied.is_none() {
            let mut state = self.inner.lock_state();
            state.time_format = parsed;
        }
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
        let inner = Arc::clone(&self.inner);
        let removed = py.detach(move || {
            let mut state = inner.lock_state();
            let mut removed = 0u64;
            for key in &keys {
                if state.active.remove(key) {
                    removed += 1;
                }
            }
            removed
        });
        let out = PyDict::new(py);
        out.set_item("removed", removed)?;
        Ok(out)
    }

    pub fn prefetch_status<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let inner = Arc::clone(&self.inner);
        let (busy, done, total, message) = py.detach(move || {
            let state = inner.lock_state();
            (
                state.prefetch.busy,
                state.prefetch.done,
                state.prefetch.total,
                state.prefetch.message.clone(),
            )
        });
        let out = PyDict::new(py);
        out.set_item("busy", busy)?;
        out.set_item("done", done)?;
        out.set_item("total", total)?;
        out.set_item("message", message)?;
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
        for row in stats {
            let dict = PyDict::new(py);
            dict.set_item("key", row.key)?;
            dict.set_item("count", row.stats.count)?;
            dict.set_item("vmin", row.stats.vmin)?;
            dict.set_item("vmax", row.stats.vmax)?;
            dict.set_item("mean", row.stats.mean)?;
            dict.set_item("first", row.stats.first)?;
            dict.set_item("last", row.stats.last)?;
            // A8：未缓存/未就绪时是占位行（count=0, pending=true），不会少返回。
            dict.set_item("pending", row.pending)?;
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

    /// 关闭会话：纯内存标记 + 删租约文件；预取线程在下个检查点退出。
    ///
    /// Rust 侧（测试/嵌入）也直接调用它而没有 `Python` 参数，因此这里用
    /// `try_attach` 取 GIL 令牌后**立刻 detach** 再取 state 锁：绝不在持 GIL 时等锁。
    pub fn close(&self) {
        let inner = Arc::clone(&self.inner);
        let attempted = Python::try_attach(|py| {
            let work = Arc::clone(&inner);
            py.detach(move || work.close_impl())
        });
        if attempted.is_none() {
            inner.close_impl();
        }
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
/// 读盘/解码错误写入 `diag`（A6：**不许** `Err(_) => {}` 静默吞掉）。
fn exact_range_multi(
    levels: &[Level],
    file: &File,
    message: &CachedMessageMeta,
    signals: &[Arc<SignalPlan>],
    start: u64,
    end: u64,
    diag: &mut ViewDiag,
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
                    match decode_signal_bytes(signal, record.dlc, &record.payload[..data_len]) {
                        // A5：与金字塔构建同一个谓词过滤非有限值。
                        Ok(value) if value.is_finite() => {
                            if let Some(agg) = aggs.get_mut(signal_index) {
                                agg.merge(f32_min_cover(value), f32_max_cover(value), value, 1);
                            }
                        }
                        Ok(_) => {}
                        Err(err) => diag.record(format!(
                            "解码 Ch{}/{} 失败：{}（{}）",
                            message.channel, signal.name, err.message, message.file
                        )),
                    }
                }
            }
            Err(err) => diag.record(format!("{}（{}）", err.message, message.file)),
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
        // A5：金字塔构建与 view 原始帧兜底用同一个谓词过滤非有限值。
        if !value.is_finite() {
            return;
        }
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
                // 不清空 message：保留「预取完成」/上次消息，外部才能区分「完成」与「没开始」。
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
        // 每个块（≈ 50 ms）检查一次 closed：`close()` 后预取立即收手。
        let abort_inner = Arc::clone(&inner);
        let abort = move || abort_inner.lock_state().closed;
        let result = frame_cache::prefetch_cache(&options, &inner.key, &refs, &progress, &abort);

        match result {
            Ok((meta, evicted)) => {
                let jobs: Vec<((u32, u32), Vec<String>)> = {
                    let mut state = inner.lock_state();
                    // 写回 meta 前再查一次 `closed`：关闭后绝不再复活金字塔。
                    if state.closed {
                        state.prefetch.busy = false;
                        state.prefetch.message = "已关闭会话".to_string();
                        return;
                    }
                    // 预取阶段新增的告警（含 LRU 删除说明、DLC 截断说明）并入会话级 warnings。
                    for warning in &meta.warnings {
                        if !state.warnings.contains(warning) {
                            state.warnings.push(warning.clone());
                        }
                    }
                    state.meta = Arc::new(meta);
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
                            let protect: BTreeSet<String> = wanted.into_iter().collect();
                            let mut state = inner.lock_state();
                            for (key, entry) in built {
                                state.insert_pyramid(key, entry, &protect);
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
                // `close()` 主动中止不是错误，不写告警。
                if state.closed || err.code == frame_cache::CACHE_ABORTED {
                    state.prefetch.message = "已关闭会话".to_string();
                    return;
                }
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
        .map(|row| (row.key.clone(), row.stats))
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
    stats: &[StatRow],
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
        .map(|row| {
            let key = &row.key;
            let stat = &row.stats;
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

    // -----------------------------------------------------------------------
    // Fix A5：非有限值（NaN/inf）一律丢，不许混进 sum / mean / 包络
    // -----------------------------------------------------------------------

    #[test]
    fn bucket_merge_rejects_non_finite_samples() {
        let mut agg = BucketAgg::empty();
        agg.merge(f32::NAN, f32::NAN, f64::NAN, 1);
        agg.merge(f32::INFINITY, f32::NEG_INFINITY, f64::INFINITY, 1);
        agg.merge(0.0, 0.0, f64::NEG_INFINITY, 1);
        assert_eq!(agg.count, 0, "非有限样本一律不算有效采样");
        assert!(agg.mean().is_finite(), "mean 不得是 NaN：{}", agg.mean());

        agg.merge(1.0, 2.0, 3.0, 1);
        assert_eq!(agg.count, 1);
        assert_eq!(agg.min, 1.0);
        assert_eq!(agg.max, 2.0);
        assert_eq!(agg.mean(), 3.0);
    }

    /// 写一个经典 CAN 段文件（17 B/帧：`t f64 + dlc u8 + payload[8]`）。
    fn write_classic_segment(dir: &Path, raws: &[u16]) -> CachedMessageMeta {
        let mut bytes = Vec::with_capacity(raws.len() * frame_cache::CLASSIC_STRIDE);
        for (index, raw) in raws.iter().enumerate() {
            let mut record = [0u8; frame_cache::CLASSIC_STRIDE];
            record[0..8].copy_from_slice(&(index as f64).to_le_bytes());
            record[8] = 8;
            record[9..11].copy_from_slice(&raw.to_le_bytes());
            bytes.extend_from_slice(&record);
        }
        std::fs::write(dir.join("ch1_id256.bin"), &bytes).expect("写段文件");
        CachedMessageMeta {
            channel: 1,
            frame_id: 0x100,
            message: "MsgA".to_string(),
            file: "ch1_id256.bin".to_string(),
            stride: frame_cache::CLASSIC_STRIDE as u32,
            frames: raws.len() as u64,
            is_fd: false,
        }
    }

    fn plan_with_factor(name: &str, factor: f64) -> Arc<SignalPlan> {
        Arc::new(SignalPlan {
            name: name.to_string(),
            start_bit: 0,
            bit_len: 16,
            order: crate::bit_slice::ByteOrder::Intel,
            is_signed: false,
            factor,
            offset: 0.0,
            unit: String::new(),
            min: None,
            max: None,
        })
    }

    /// A5：注入 inf/NaN 序列 → 金字塔 `stats` 只算有效样本，`mean` 不是 NaN，
    /// `view()` 的原始帧兜底路径（`exact_range_multi`）也丢同样的样本。
    #[test]
    fn non_finite_samples_are_dropped_from_pyramid_and_view() {
        let dir = tempfile::tempdir().expect("临时目录");
        let message = write_classic_segment(dir.path(), &[1, 2, 3, 4, 5]);
        let finite = plan_with_factor("Finite", 1.0);
        let infinite = plan_with_factor("Infinite", f64::INFINITY);
        let nan = plan_with_factor("Nan", f64::NAN);
        let signals = vec![
            ("ch1/MsgA/Finite".to_string(), Arc::clone(&finite)),
            ("ch1/MsgA/Infinite".to_string(), Arc::clone(&infinite)),
            ("ch1/MsgA/Nan".to_string(), Arc::clone(&nan)),
        ];

        // ① 金字塔：只统计有效样本。
        let built = build_pyramids(dir.path(), &message, &signals).expect("建金字塔");
        let by_key: BTreeMap<&str, &PyramidEntry> =
            built.iter().map(|(key, entry)| (key.as_str(), entry)).collect();
        let ok = by_key.get("ch1/MsgA/Finite").expect("Finite 必须建成");
        assert_eq!(ok.stats.count, 5);
        assert!(ok.stats.mean.is_finite(), "mean={}", ok.stats.mean);
        assert!((ok.stats.mean - 3.0).abs() < 1e-12);
        assert_eq!(ok.stats.vmin, 1.0);
        assert_eq!(ok.stats.vmax, 5.0);
        for key in ["ch1/MsgA/Infinite", "ch1/MsgA/Nan"] {
            let bad = by_key.get(key).expect("非有限信号也必须建成");
            assert_eq!(bad.stats.count, 0, "{key} 不得计入任何有效样本");
            assert!(bad.stats.mean.is_finite(), "{key} 的 mean 不得是 NaN");
            assert!(bad.stats.vmin.is_finite() && bad.stats.vmax.is_finite());
            for level in &bad.pyramid.levels {
                assert!(level.count.iter().all(|count| *count == 0));
            }
        }

        // ② 原始帧兜底（view 的边界路径）：同一个谓词过滤。
        let path = message.path(dir.path());
        let file = File::open(&path).expect("打开段");
        let mut diag = ViewDiag::default();
        let aggs = exact_range_multi(
            &[],
            &file,
            &message,
            &[finite, infinite, nan],
            0,
            4,
            &mut diag,
        );
        assert_eq!(diag.errors, 0, "这里没有真实错误：{diag:?}");
        assert_eq!(aggs[0].count, 5);
        assert!(aggs[0].mean().is_finite());
        assert!((aggs[0].mean() - 3.0).abs() < 1e-12);
        for agg in &aggs[1..] {
            assert_eq!(agg.count, 0, "非有限样本不得进入桶聚合");
            assert!(agg.mean().is_finite(), "桶 mean 不得是 NaN");
        }
    }

    // -----------------------------------------------------------------------
    // Fix B2：金字塔内存预算 + LRU 淘汰 + 按需重建（结果不变）
    // -----------------------------------------------------------------------

    /// 写一个 `frames` 条的经典 CAN 段（升序时间戳），payload 随 `seed` 变化。
    fn write_b2_segment(dir: &Path, file: &str, frame_id: u32, frames: u64, seed: u16) -> CachedMessageMeta {
        let mut bytes = Vec::with_capacity(frames as usize * frame_cache::CLASSIC_STRIDE);
        for index in 0..frames {
            let mut record = [0u8; frame_cache::CLASSIC_STRIDE];
            record[0..8].copy_from_slice(&(index as f64 * 0.001).to_le_bytes());
            record[8] = 8;
            let raw = ((index as u16).wrapping_mul(7).wrapping_add(seed.wrapping_mul(13))) % 1000;
            record[9..11].copy_from_slice(&raw.to_le_bytes());
            bytes.extend_from_slice(&record);
        }
        std::fs::write(dir.join(file), &bytes).expect("写段文件");
        CachedMessageMeta {
            channel: 1,
            frame_id,
            message: format!("Msg{frame_id}"),
            file: file.to_string(),
            stride: frame_cache::CLASSIC_STRIDE as u32,
            frames,
            is_fd: false,
        }
    }

    /// 直接拼一个 `SessionInner`（不起 BLF/预取线程，方便逐字节比较 `view()`）。
    fn b2_session(dir: &Path, metas: Vec<CachedMessageMeta>, keys: &[String]) -> Arc<SessionInner> {
        let t_max = metas
            .iter()
            .map(|meta| meta.frames.saturating_sub(1) as f64 * 0.001)
            .fold(0.0f64, f64::max);
        let meta = Arc::new(CacheMeta {
            format_version: frame_cache::CACHE_FORMAT_VERSION.to_string(),
            key: "b2-key".to_string(),
            dbc_hash: String::new(),
            message_set: metas
                .iter()
                .map(|meta| (meta.channel, meta.frame_id))
                .collect(),
            created_at: 0,
            last_used_at: 0,
            cache_bytes: 0,
            t_min: 0.0,
            t_max,
            abs_min_micros: None,
            abs_max_micros: None,
            files: Vec::new(),
            messages: metas,
            warnings: Vec::new(),
        });
        let mut catalog: Vec<CatalogSignal> = Vec::new();
        for key in keys {
            let parts: Vec<&str> = key.split('/').collect();
            let channel = parts[0].trim_start_matches("ch").parse::<u32>().unwrap_or(1);
            let message = parts[1].to_string();
            let frame_id = 0x100u32 + (message.trim_start_matches("Msg").parse::<u32>().unwrap_or(0));
            catalog.push(CatalogSignal {
                key: key.clone(),
                channel,
                message,
                frame_id,
                unit: None,
                signal: plan_with_factor(parts[2], 1.0),
            });
        }
        let mut catalog_by_key = HashMap::new();
        for (index, signal) in catalog.iter().enumerate() {
            catalog_by_key.insert(signal.key.clone(), index);
        }
        let state = SessionState {
            meta,
            dir: dir.to_path_buf(),
            cache_hit: true,
            evicted_bytes: 0,
            active: BTreeSet::new(),
            pyramids: HashMap::new(),
            pyramid_bytes: 0,
            pyramid_used: HashMap::new(),
            pyramid_seen: std::collections::HashSet::new(),
            pyramid_evictions: 0,
            pyramid_evicted_bytes: 0,
            pyramid_rebuilds: 0,
            time_format: TimeFormat::RelativeS,
            prefetch: PrefetchStatus::default(),
            prefetch_queue: BTreeSet::new(),
            warnings: Vec::new(),
            view_errors: 0,
            view_error_first: None,
            closed: false,
        };
        Arc::new(SessionInner {
            state: Mutex::new(state),
            cache_dir: dir.to_path_buf(),
            key: "b2-key".to_string(),
            blf_paths: Vec::new(),
            dbc_by_channel: BTreeMap::new(),
            budget_mb: frame_cache::DEFAULT_CACHE_BUDGET_MB,
            catalog,
            catalog_by_key,
            messages: BTreeMap::new(),
            lease: Mutex::new(None),
            last_touch_millis: AtomicU64::new(now_millis()),
        })
    }

    /// B2 自证：超预算 → LRU 淘汰其它信号（不动本次用到的那个）→ 再次需要时按需重建，
    /// 重建后的 `view()` 与未淘汰时**逐值相同**；`info()` 的 `pyramid_bytes` 始终
    /// ≤ `pyramid_budget_bytes`，并在 `warnings` 里写明淘汰/重建。
    #[test]
    fn pyramid_budget_evicts_lru_and_rebuild_keeps_view_identical() {
        const FRAMES: u64 = 200_000;
        let dir = tempfile::tempdir().expect("临时目录");
        let names = ["Msg0", "Msg1", "Msg2", "Msg3"];
        let mut metas = Vec::new();
        let mut keys = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let frame_id = 0x100 + index as u32;
            let file = format!("ch1_id{frame_id}.bin");
            metas.push(write_b2_segment(
                dir.path(),
                &file,
                frame_id,
                FRAMES,
                index as u16,
            ));
            keys.push(format!("ch1/{name}/Sig"));
        }
        let inner = b2_session(dir.path(), metas, &keys);
        let t_max = (FRAMES - 1) as f64 * 0.001;

        // ① 只激活 1 个信号：拿到“从未淘汰”的基准 view，并确认 4 个信号一定超预算。
        inner.lock_state().active.insert(keys[0].clone());
        let reference = inner.compute_view(0.0, t_max, 400).expect("基准 view");
        assert!(!reference.series.is_empty(), "基准 view 必须有数据");
        assert!(!reference.truncated, "基准 view 不得为 truncated");
        let single_bytes = inner.lock_state().pyramid_bytes;
        assert!(single_bytes > 0, "单位信号金字塔必须占用字节");
        assert!(
            single_bytes < PYRAMID_BUDGET_LIMIT,
            "单个信号 {single_bytes} 必须装得下预算 {PYRAMID_BUDGET_LIMIT}"
        );
        assert!(
            single_bytes.saturating_mul(names.len() as u64) > PYRAMID_BUDGET_LIMIT,
            "{} 个信号必须超预算（{single_bytes} B × {} > {PYRAMID_BUDGET_LIMIT}）",
            names.len(),
            names.len()
        );

        // ② 全部激活，逐个 stats：每次只保护自己 → 累计必超预算 → LRU 淘汰其它信号。
        {
            let mut state = inner.lock_state();
            for key in &keys {
                state.active.insert(key.clone());
            }
        }
        for key in &keys {
            let rows = inner
                .stats_impl(std::slice::from_ref(key))
                .expect("stats 必须成功");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].stats.count, FRAMES, "{key} 必须统计到全部帧");
            let snapshot = inner.info_snapshot();
            assert!(
                snapshot.pyramid_bytes <= snapshot.pyramid_budget_bytes,
                "常驻金字塔 {} B 超预算 {} B",
                snapshot.pyramid_bytes,
                snapshot.pyramid_budget_bytes
            );
        }
        let (evictions, evicted_bytes, resident) = {
            let state = inner.lock_state();
            let actual: u64 = state
                .pyramids
                .values()
                .map(|entry| entry.pyramid.bytes() as u64)
                .sum();
            assert_eq!(
                state.pyramid_bytes, actual,
                "pyramid_bytes 账本必须与常驻金字塔一致"
            );
            assert!(state.pyramid_evictions > 0, "必须真的发生过淘汰");
            assert!(state.pyramid_evicted_bytes > 0);
            (
                state.pyramid_evictions,
                state.pyramid_evicted_bytes,
                state.pyramids.contains_key(&keys[0]),
            )
        };
        // 第一个信号是最老的 LRU，必须被淘汰。
        assert!(!resident, "最老的信号应被 LRU 淘汰：{keys:?}");
        let info = inner.info_snapshot();
        assert_eq!(info.pyramid_budget_bytes, PYRAMID_BUDGET_LIMIT);
        assert!(
            info.warnings.iter().any(|line| line.contains("LRU 已淘汰")),
            "warnings 必须写清淘汰：{:?}",
            info.warnings
        );

        // ③ 再次需要被淘汰的信号：按需重建 → view 与基准逐值相同。
        {
            let mut state = inner.lock_state();
            state.active.retain(|key| key == &keys[0]);
        }
        let rebuilt_view = inner.compute_view(0.0, t_max, 400).expect("重建后 view");
        let rebuilds = inner.lock_state().pyramid_rebuilds;
        assert!(rebuilds > 0, "淘汰后必须按需重建");
        assert!(!rebuilt_view.truncated, "重建后不得 truncated");
        assert_eq!(rebuilt_view.t.len(), reference.t.len());
        for (index, value) in rebuilt_view.t.iter().enumerate() {
            assert_eq!(
                value.to_bits(),
                reference.t[index].to_bits(),
                "t[{index}] 必须与未淘汰时一致"
            );
        }
        assert_eq!(rebuilt_view.series.len(), reference.series.len());
        for (after, before) in rebuilt_view.series.iter().zip(&reference.series) {
            assert_eq!(after.key, before.key);
            assert_eq!(after.min.len(), before.min.len());
            for index in 0..before.min.len() {
                assert_eq!(
                    after.min[index].to_bits(),
                    before.min[index].to_bits(),
                    "{} min[{index}] 重建后必须逐值相同",
                    before.key
                );
                assert_eq!(
                    after.max[index].to_bits(),
                    before.max[index].to_bits(),
                    "{} max[{index}] 重建后必须逐值相同",
                    before.key
                );
                assert_eq!(
                    after.mean[index].to_bits(),
                    before.mean[index].to_bits(),
                    "{} mean[{index}] 重建后必须逐值相同",
                    before.key
                );
            }
        }
        let final_info = inner.info_snapshot();
        assert!(
            final_info
                .warnings
                .iter()
                .any(|line| line.contains("按需重建") && line.contains("次")),
            "warnings 必须写清重建次数：{:?}",
            final_info.warnings
        );
        println!(
            "FIXB_B2 budget={PYRAMID_BUDGET_LIMIT}B single={single_bytes}B evictions={evictions} evicted_bytes={evicted_bytes} rebuilds={rebuilds} pyramid_bytes={}B",
            final_info.pyramid_bytes
        );
    }
}
