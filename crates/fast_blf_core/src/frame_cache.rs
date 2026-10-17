//! Step 17：磁盘帧缓存（一报文一文件、固定 stride、`read_at` 随机访问）。
//!
//! 布局（契约 `docs/INTERFACE.md` §1.4）：
//!
//! ```text
//! <cache_dir>/<key>/meta.json
//! <cache_dir>/<key>/ch{channel}_id{id}.bin
//! ```
//!
//! * 经典 CAN 记录 stride = **17**：`t f64(0..8) + dlc u8(8) + payload[8](9..17)`；
//! * CAN FD 记录 stride = **73**：`t f64(0..8) + dlc u8(8) + payload[64](9..73)`。
//!
//! 文件按时间升序写入；只有在检测到跨文件时间戳回退时才做一次内存排序（常规路径是单遍流式）。
//! 随机读取统一走 [`read_at_exact`]（Unix `FileExt::read_at` / Windows `FileExt::seek_read`），
//! 不引入 mmap 或任何新依赖。

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use blf_asc::BlfReader;
use serde::{Deserialize, Serialize};

use crate::decoder::{read_file_timing, MessagePlan, Progress};
use crate::error::{CoreError, BLF_OPEN, DISK_FULL};

/// 缓存格式版本；任何编码变化都必须递增，旧目录自然失效。
pub const CACHE_FORMAT_VERSION: &str = "fblf-frame-cache-v1";
/// 预算超限错误码（§1.4 明确要求；§4 稳定清单尚未收录，故在本模块集中定义一次）。
pub const CACHE_BUDGET_EXCEEDED: &str = "CACHE_BUDGET_EXCEEDED";
/// 默认缓存预算（MiB）。
pub const DEFAULT_CACHE_BUDGET_MB: u64 = 200;
/// 预算下界（MiB）。
pub const MIN_CACHE_BUDGET_MB: u64 = 32;
/// 预算上界（MiB）。
pub const MAX_CACHE_BUDGET_MB: u64 = 8192;
/// 经典 CAN 记录长度。
pub const CLASSIC_STRIDE: usize = 17;
/// CAN FD 记录长度。
pub const FD_STRIDE: usize = 73;

const MIB: u64 = 1024 * 1024;
const PAYLOAD_CLASSIC: usize = 8;
const PAYLOAD_FD: usize = 64;

/// 一个输入文件的稳定身份：绝对路径 + size + mtime（缓存 key 的组成部分）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceFileMeta {
    pub path: String,
    pub size: u64,
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
}

/// 一个报文的缓存段描述（`meta.json` 的 `messages[]`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedMessageMeta {
    /// 1-based 通道号。
    pub channel: u32,
    /// 归一化 raw id（扩展帧含 `1<<31`）。
    pub frame_id: u32,
    /// DBC 报文名。
    pub message: String,
    /// 相对缓存目录的文件名。
    pub file: String,
    /// 17 或 73。
    pub stride: u32,
    /// 记录条数。
    pub frames: u64,
    /// 该段是否按 FD 编码。
    pub is_fd: bool,
}

/// `meta.json`：版本、文件清单、DBC 哈希、报文集合、时间范围、生成时间、总字节。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMeta {
    pub format_version: String,
    pub key: String,
    /// DBC 内容指纹（sha256 十六进制）；与 key 里的同一份指纹一致。
    #[serde(default)]
    pub dbc_hash: String,
    /// 报文集合 `(channel, frame_id)`（排序、去重）。
    #[serde(default)]
    pub message_set: Vec<(u32, u32)>,
    /// Unix 毫秒；LRU 按它从旧到新删。
    pub created_at: u64,
    /// 整个 `<key>/` 目录的字节数（含 meta.json）。
    pub cache_bytes: u64,
    pub t_min: f64,
    pub t_max: f64,
    pub abs_min_micros: Option<i64>,
    pub abs_max_micros: Option<i64>,
    pub files: Vec<SourceFileMeta>,
    pub messages: Vec<CachedMessageMeta>,
    pub warnings: Vec<String>,
}

impl CacheMeta {
    /// 按 `(channel, frame_id)` 查报文段。
    pub fn message(&self, channel: u32, frame_id: u32) -> Option<&CachedMessageMeta> {
        self.messages
            .iter()
            .find(|message| message.channel == channel && message.frame_id == frame_id)
    }

    /// 缓存里是否已有该报文的数据文件。
    pub fn has_message(&self, channel: u32, frame_id: u32) -> bool {
        self.message(channel, frame_id).is_some()
    }
}

/// 缓存段的绝对路径。
impl CachedMessageMeta {
    pub fn path(&self, cache_dir: &Path) -> PathBuf {
        cache_dir.join(&self.file)
    }

    /// 记录条数（以文件大小与 stride 复核，防止 meta 与文件不一致）。
    pub fn actual_frames(&self, cache_dir: &Path) -> u64 {
        std::fs::metadata(self.path(cache_dir))
            .map(|meta| meta.len() / self.stride as u64)
            .unwrap_or(0)
    }
}

/// 一条从缓存读出的原始记录。
#[derive(Debug, Clone, PartialEq)]
pub struct CachedRecord {
    pub timestamp: f64,
    pub dlc: u8,
    /// 前 `data_len` 字节是真实 payload，其余为 0。
    pub payload: [u8; PAYLOAD_FD],
    /// 8（经典 CAN）或 64（CAN FD）。
    pub data_len: usize,
}

/// 建缓存/预取需要的输入。
pub struct CacheOptions<'a> {
    pub blf_paths: &'a [PathBuf],
    pub dbc_by_channel: &'a BTreeMap<u32, PathBuf>,
    pub cache_dir: &'a Path,
    pub budget_mb: u64,
}

/// `open_cache` 的结果。
#[derive(Debug, Clone)]
pub struct CacheOpen {
    pub dir: PathBuf,
    pub meta: CacheMeta,
    pub cache_hit: bool,
}

/// 校验 `cache_budget_mb`；越界按 §4 的稳定错误码 `BAD_CONFIG` 返回
///（§1.4 原文写作 `CONFIG_INVALID`，与 §4 的稳定清单冲突，已在交付说明中记录）。
pub fn validate_budget(budget_mb: u64) -> Result<(), CoreError> {
    if (MIN_CACHE_BUDGET_MB..=MAX_CACHE_BUDGET_MB).contains(&budget_mb) {
        Ok(())
    } else {
        Err(CoreError::bad_config(format!(
            "config 字段 `cache_budget_mb` 越界：{budget_mb}（允许 {MIN_CACHE_BUDGET_MB}..={MAX_CACHE_BUDGET_MB}）"
        )))
    }
}

/// 打开（命中）或建立帧缓存。
pub fn open_cache(
    options: &CacheOptions<'_>,
    messages: &[&MessagePlan],
    progress: &dyn Fn(Progress),
) -> Result<CacheOpen, CoreError> {
    validate_budget(options.budget_mb)?;
    std::fs::create_dir_all(options.cache_dir).map_err(|err| {
        CoreError::new(
            DISK_FULL,
            format!(
                "无法创建帧缓存目录：{}（{err}）",
                options.cache_dir.display()
            ),
        )
    })?;

    let key = compute_cache_key(options.blf_paths, options.dbc_by_channel, messages)?;
    let dir = options.cache_dir.join(&key);
    let required: BTreeSet<(u32, u32)> = messages
        .iter()
        .map(|message| (message.channel, message.frame_id))
        .collect();

    if let Some(meta) = load_matching_meta(&dir, &key, &required) {
        enforce_lru(options.cache_dir, options.budget_mb, &key);
        return Ok(CacheOpen {
            dir,
            meta,
            cache_hit: true,
        });
    }

    // 先扫描：得到帧数（预算估算）与文件级首次 FD 标志。
    let scan = scan_inputs(
        options.blf_paths,
        options.dbc_by_channel,
        &required,
        true,
        progress,
    )?;
    let (need_bytes, strides) = estimate_bytes(messages, &scan);
    if need_bytes > options.budget_mb.saturating_mul(MIB) {
        return Err(budget_error(need_bytes, options.budget_mb));
    }

    std::fs::create_dir_all(&dir).map_err(|err| {
        CoreError::new(
            DISK_FULL,
            format!("无法创建帧缓存目录：{}（{err}）", dir.display()),
        )
    })?;

    let mut meta = build_segments(
        &dir,
        options.blf_paths,
        messages,
        &scan,
        &strides,
        progress,
        false,
        None,
    )?;
    // 初始建缓存：meta 只描述本次 selected 的报文；已预取的额外段保留在磁盘上但不列出。
    meta.created_at = now_millis();
    meta.key = key.clone();
    meta.dbc_hash = dbc_fingerprint(options.dbc_by_channel);
    meta.message_set = required.iter().copied().collect();
    meta.files = scan.files.clone();
    meta.warnings.extend(scan.warnings.clone());
    write_meta(&dir, &meta)?;
    meta.cache_bytes = directory_bytes(&dir);
    write_meta(&dir, &meta)?;

    enforce_lru(options.cache_dir, options.budget_mb, &key);
    Ok(CacheOpen {
        dir,
        meta,
        cache_hit: false,
    })
}

/// 增量预取：只把 `messages` 中尚未缓存的报文写入同一个 key 目录。
///
/// 返回更新后的 meta。调用方需保证 `key` 是当前会话的 key。
pub fn prefetch_cache(
    options: &CacheOptions<'_>,
    key: &str,
    messages: &[&MessagePlan],
    progress: &dyn Fn(Progress),
) -> Result<CacheMeta, CoreError> {
    validate_budget(options.budget_mb)?;
    let dir = options.cache_dir.join(key);
    let mut meta = read_meta(&dir)?;
    if meta.key != key || meta.format_version != CACHE_FORMAT_VERSION {
        return Err(CoreError::blf_parse(format!(
            "帧缓存元数据与 key 不匹配：{}",
            dir.display()
        )));
    }

    let missing: Vec<&MessagePlan> = messages
        .iter()
        .copied()
        .filter(|message| !meta.has_message(message.channel, message.frame_id))
        .collect();
    if missing.is_empty() {
        return Ok(meta);
    }

    let required: BTreeSet<(u32, u32)> = missing
        .iter()
        .map(|message| (message.channel, message.frame_id))
        .collect();
    let scan = scan_inputs(
        options.blf_paths,
        options.dbc_by_channel,
        &required,
        true,
        progress,
    )?;
    let (need_bytes, strides) = estimate_bytes(&missing, &scan);
    let existing = directory_bytes(&dir);
    if existing.saturating_add(need_bytes) > options.budget_mb.saturating_mul(MIB) {
        return Err(budget_error(
            existing.saturating_add(need_bytes),
            options.budget_mb,
        ));
    }

    let additions = build_segments(
        &dir,
        options.blf_paths,
        &missing,
        &scan,
        &strides,
        progress,
        true,
        Some(&meta),
    )?;
    meta.messages.extend(additions.messages);
    meta.messages
        .sort_by_key(|message| (message.channel, message.frame_id));
    meta.message_set = meta
        .messages
        .iter()
        .map(|message| (message.channel, message.frame_id))
        .collect();
    meta.warnings.extend(scan.warnings);
    if scan.raw_min.is_finite() {
        meta.t_min = meta.t_min.min(scan.raw_min);
        meta.t_max = meta.t_max.max(scan.raw_max);
        if scan.abs_min_micros.is_some() {
            meta.abs_min_micros = match meta.abs_min_micros {
                Some(current) => scan.abs_min_micros.map(|value| current.min(value)),
                None => scan.abs_min_micros,
            };
        }
        if scan.abs_max_micros.is_some() {
            meta.abs_max_micros = match meta.abs_max_micros {
                Some(current) => scan.abs_max_micros.map(|value| current.max(value)),
                None => scan.abs_max_micros,
            };
        }
    }
    write_meta(&dir, &meta)?;
    meta.cache_bytes = directory_bytes(&dir);
    write_meta(&dir, &meta)?;
    enforce_lru(options.cache_dir, options.budget_mb, key);
    Ok(meta)
}

/// 读 `meta.json`。
pub fn read_meta(dir: &Path) -> Result<CacheMeta, CoreError> {
    let path = dir.join("meta.json");
    let text = std::fs::read(&path).map_err(|err| {
        CoreError::blf_parse(format!("读取帧缓存元数据失败：{}（{err}）", path.display()))
    })?;
    serde_json::from_slice(&text).map_err(|err| {
        CoreError::blf_parse(format!("解析帧缓存元数据失败：{}（{err}）", path.display()))
    })
}

/// DBC 内容指纹（按通道号排序；读不到的文件把错误串也计入，保证 key 稳定）。
pub fn dbc_fingerprint(dbc_by_channel: &BTreeMap<u32, PathBuf>) -> String {
    let mut hasher = Sha256::new();
    for (channel, path) in dbc_by_channel {
        hasher.update(&channel.to_le_bytes());
        match std::fs::read(path) {
            Ok(bytes) => {
                hasher.update(&(bytes.len() as u64).to_le_bytes());
                hasher.update(&bytes);
            }
            Err(err) => {
                hasher.update(b"<missing>");
                hasher.update(err.to_string().as_bytes());
            }
        }
        hasher.update(&[0xff]);
    }
    hex(&hasher.finish())
}

/// 缓存 key：sha256(绝对路径+size+mtime ‖ DBC 内容哈希 ‖ 报文集合 ‖ 格式版本)。
pub fn compute_cache_key(
    blf_paths: &[PathBuf],
    dbc_by_channel: &BTreeMap<u32, PathBuf>,
    messages: &[&MessagePlan],
) -> Result<String, CoreError> {
    let mut hasher = Sha256::new();
    hasher.update(CACHE_FORMAT_VERSION.as_bytes());
    hasher.update(&[0]);

    for path in blf_paths {
        let (absolute, meta) = source_identity(path)?;
        hasher.update(absolute.as_os_str().to_string_lossy().as_bytes());
        hasher.update(&[0]);
        hasher.update(&meta.size.to_le_bytes());
        hasher.update(&meta.mtime_secs.to_le_bytes());
        hasher.update(&meta.mtime_nanos.to_le_bytes());
        hasher.update(&[0xff]);
    }
    hasher.update(b"DBC");
    for (channel, path) in dbc_by_channel {
        hasher.update(&channel.to_le_bytes());
        match std::fs::read(path) {
            Ok(bytes) => {
                hasher.update(&(bytes.len() as u64).to_le_bytes());
                hasher.update(&bytes);
            }
            Err(err) => {
                hasher.update(b"<missing>");
                hasher.update(err.to_string().as_bytes());
            }
        }
        hasher.update(&[0xff]);
    }
    hasher.update(b"MSGS");
    let mut set: Vec<(u32, u32)> = messages
        .iter()
        .map(|message| (message.channel, message.frame_id))
        .collect();
    set.sort_unstable();
    set.dedup();
    for (channel, frame_id) in set {
        hasher.update(&channel.to_le_bytes());
        hasher.update(&frame_id.to_le_bytes());
    }
    Ok(hex(&hasher.finish()))
}

// ---------------------------------------------------------------------------
// 扫描 / 预算估算
// ---------------------------------------------------------------------------

struct ScanOutcome {
    files: Vec<SourceFileMeta>,
    counts: BTreeMap<(u32, u32), u64>,
    first_is_fd: BTreeMap<(u32, u32), bool>,
    any_is_fd: BTreeMap<(u32, u32), bool>,
    raw_min: f64,
    raw_max: f64,
    abs_min_micros: Option<i64>,
    abs_max_micros: Option<i64>,
    frames_total: u64,
    warnings: Vec<String>,
}

fn scan_inputs(
    paths: &[PathBuf],
    _dbc_by_channel: &BTreeMap<u32, PathBuf>,
    required: &BTreeSet<(u32, u32)>,
    report_progress: bool,
    progress: &dyn Fn(Progress),
) -> Result<ScanOutcome, CoreError> {
    let mut files = Vec::with_capacity(paths.len());
    let mut counts: BTreeMap<(u32, u32), u64> = BTreeMap::new();
    let mut first_is_fd: BTreeMap<(u32, u32), bool> = BTreeMap::new();
    let mut any_is_fd: BTreeMap<(u32, u32), bool> = BTreeMap::new();
    let mut raw_min = f64::INFINITY;
    let mut raw_max = f64::NEG_INFINITY;
    let mut abs_min_micros = None;
    let mut abs_max_micros = None;
    let mut warnings = Vec::new();
    let mut frames_processed: u64 = 0;

    for path in paths {
        let (absolute, source) = source_identity(path)?;
        files.push(source);
        let label = file_label(path);
        if report_progress {
            progress(Progress {
                phase: 1,
                stage: "cache".to_string(),
                done: 0,
                total: 1,
                file: Some(path.to_string_lossy().to_string()),
                frames_done: frames_processed,
                message: format!("正在估算缓存 {label}"),
            });
        }

        let timing = read_file_timing(path);
        let wall_micros = timing.wall.as_ref().and_then(|wall| wall.epoch_micros());
        let mut reader = match BlfReader::open(path) {
            Ok(reader) => reader,
            Err(err) => {
                warnings.push(format!(
                    "{} 无法打开，已跳过缓存（{err}）",
                    absolute.display()
                ));
                // 本文件的 done=1 由写盘阶段统一发出，避免同一文件在进度里被计两次。
                if report_progress {
                    progress(Progress {
                        phase: 1,
                        stage: "cache".to_string(),
                        done: 0,
                        total: 1,
                        file: Some(path.to_string_lossy().to_string()),
                        frames_done: frames_processed,
                        message: format!("缓存估算失败 {label}"),
                    });
                }
                continue;
            }
        };

        let throttle = std::time::Duration::from_millis(50);
        let mut last_emit = std::time::Instant::now();
        let mut emitted_first = false;
        for message in reader.by_ref() {
            let channel = message.channel as u32 + 1;
            let frame_id =
                message.arbitration_id.0 | if message.is_extended_id { 1 << 31 } else { 0 };
            let key = (channel, frame_id);
            if required.contains(&key) {
                *counts.entry(key).or_insert(0) += 1;
                first_is_fd.entry(key).or_insert(message.is_fd);
                let any = any_is_fd.entry(key).or_insert(false);
                *any |= message.is_fd;
            }
            let timestamp = message.timestamp;
            if timestamp.is_finite() {
                if timestamp < raw_min {
                    raw_min = timestamp;
                    abs_min_micros = if timing.wall.is_some() {
                        absolute_micros(&timing, timestamp, wall_micros)
                    } else {
                        None
                    };
                }
                if timestamp > raw_max {
                    raw_max = timestamp;
                    abs_max_micros = if timing.wall.is_some() {
                        absolute_micros(&timing, timestamp, wall_micros)
                    } else {
                        None
                    };
                }
            }
            frames_processed += 1;
            if report_progress {
                let due = !emitted_first || last_emit.elapsed() >= throttle;
                if due {
                    emitted_first = true;
                    last_emit = std::time::Instant::now();
                    progress(Progress {
                        phase: 1,
                        stage: "cache".to_string(),
                        done: 0,
                        total: 1,
                        file: Some(path.to_string_lossy().to_string()),
                        frames_done: frames_processed,
                        message: format!("正在估算缓存 {label}"),
                    });
                }
            }
        }
        if let Some(err) = reader.take_error() {
            warnings.push(format!(
                "{} 解析告警（缓存仍已写入已读帧）：{err}",
                absolute.display()
            ));
        }
        // 本文件的「完成」留到写盘阶段再发，避免同一文件被计两次。
    }

    Ok(ScanOutcome {
        files,
        counts,
        first_is_fd,
        any_is_fd,
        raw_min,
        raw_max,
        abs_min_micros,
        abs_max_micros,
        frames_total: frames_processed,
        warnings,
    })
}

fn estimate_bytes(
    messages: &[&MessagePlan],
    scan: &ScanOutcome,
) -> (u64, BTreeMap<(u32, u32), usize>) {
    let mut need = 0u64;
    let mut strides = BTreeMap::new();
    for message in messages {
        let key = (message.channel, message.frame_id);
        let frames = scan.counts.get(&key).copied().unwrap_or(0);
        // 首帧决定 stride；若后续出现 FD 帧，契约要求“按 FD 段截断写入”，
        // 因此只要观测到任意 FD 帧就使用 73 字节段（混合帧的经典帧补零）。
        let is_fd = scan.any_is_fd.get(&key).copied().unwrap_or(message.is_fd);
        let stride = if is_fd { FD_STRIDE } else { CLASSIC_STRIDE };
        strides.insert(key, stride);
        need = need.saturating_add(frames.saturating_mul(stride as u64));
    }
    (need, strides)
}

fn budget_error(need_bytes: u64, budget_mb: u64) -> CoreError {
    let need_mb = need_bytes.div_ceil(MIB);
    CoreError::new(
        CACHE_BUDGET_EXCEEDED,
        format!(
            "需要 {need_mb} MiB，上限 {budget_mb} MiB；可选：①提高上限(cache_budget_mb) ②减少所选报文 ③不建缓存继续但较慢"
        ),
    )
}

// ---------------------------------------------------------------------------
// 写盘
// ---------------------------------------------------------------------------

struct SegmentWriter {
    meta: CachedMessageMeta,
    writer: BufWriter<File>,
    last_timestamp: f64,
    unsorted: bool,
    warned_mismatch: bool,
}

fn build_segments(
    dir: &Path,
    paths: &[PathBuf],
    messages: &[&MessagePlan],
    scan: &ScanOutcome,
    strides: &BTreeMap<(u32, u32), usize>,
    progress: &dyn Fn(Progress),
    append: bool,
    existing: Option<&CacheMeta>,
) -> Result<CacheMeta, CoreError> {
    let required: BTreeSet<(u32, u32)> = messages
        .iter()
        .map(|message| (message.channel, message.frame_id))
        .collect();

    let mut writers: BTreeMap<(u32, u32), SegmentWriter> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();

    for message in messages {
        let key = (message.channel, message.frame_id);
        let stride = strides.get(&key).copied().unwrap_or(if message.is_fd {
            FD_STRIDE
        } else {
            CLASSIC_STRIDE
        });
        let file = format!("ch{}_id{}.bin", message.channel, message.frame_id);
        let path = dir.join(&file);
        if scan.first_is_fd.get(&key).copied() == Some(false) && stride == FD_STRIDE {
            warnings.push(format!(
                "报文 Ch{}/{} 首帧为经典 CAN、后续出现 FD 帧；已按 FD 段（73 字节）截断写入",
                message.channel, message.name
            ));
        }
        let already = append
            && existing
                .map(|meta| meta.has_message(message.channel, message.frame_id))
                .unwrap_or(false);
        if already {
            continue;
        }
        let handle = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|err| {
                CoreError::new(
                    DISK_FULL,
                    format!("创建帧缓存文件失败：{}（{err}）", path.display()),
                )
            })?;
        writers.insert(
            key,
            SegmentWriter {
                meta: CachedMessageMeta {
                    channel: message.channel,
                    frame_id: message.frame_id,
                    message: message.name.clone(),
                    file,
                    stride: stride as u32,
                    frames: 0,
                    is_fd: stride == FD_STRIDE,
                },
                writer: BufWriter::new(handle),
                last_timestamp: f64::NEG_INFINITY,
                unsorted: false,
                warned_mismatch: false,
            },
        );
    }

    // 继承扫描阶段的帧计数，保证 frames_done 在整个建缓存过程中单调递增。
    let mut frames_processed: u64 = scan.frames_total;
    for path in paths {
        let label = file_label(path);
        progress(Progress {
            phase: 1,
            stage: "cache".to_string(),
            done: 0,
            total: 1,
            file: Some(path.to_string_lossy().to_string()),
            frames_done: frames_processed,
            message: format!("正在写入缓存 {label}"),
        });

        let mut reader = match BlfReader::open(path) {
            Ok(reader) => reader,
            Err(err) => {
                warnings.push(format!("{} 无法打开，已跳过缓存（{err}）", path.display()));
                progress(Progress {
                    phase: 1,
                    stage: "cache".to_string(),
                    done: 1,
                    total: 1,
                    file: Some(path.to_string_lossy().to_string()),
                    frames_done: frames_processed,
                    message: format!("缓存写入失败 {label}"),
                });
                continue;
            }
        };

        let throttle = std::time::Duration::from_millis(50);
        let mut last_emit = std::time::Instant::now();
        let mut emitted_first = false;
        for message in reader.by_ref() {
            let channel = message.channel as u32 + 1;
            let frame_id =
                message.arbitration_id.0 | if message.is_extended_id { 1 << 31 } else { 0 };
            let key = (channel, frame_id);
            frames_processed += 1;

            if required.contains(&key) {
                if let Some(segment) = writers.get_mut(&key) {
                    let fd_segment = segment.meta.stride == FD_STRIDE as u32;
                    if message.is_fd != fd_segment {
                        if !segment.warned_mismatch {
                            segment.warned_mismatch = true;
                            warnings.push(format!(
                                "报文 Ch{channel}/{} 首帧与后续帧的 FD 标志不一致；按 {} 字节 stride 截断写入",
                                segment.meta.message,
                                segment.meta.stride
                            ));
                        }
                    }
                    if message.timestamp < segment.last_timestamp {
                        segment.unsorted = true;
                    }
                    segment.last_timestamp = message.timestamp;
                    write_record(
                        &mut segment.writer,
                        message.timestamp,
                        message.dlc,
                        message.data.as_slice(),
                        segment.meta.stride == FD_STRIDE as u32,
                    )?;
                    segment.meta.frames = segment.meta.frames.saturating_add(1);
                }
            }

            let due = !emitted_first || last_emit.elapsed() >= throttle;
            if due {
                emitted_first = true;
                last_emit = std::time::Instant::now();
                progress(Progress {
                    phase: 1,
                    stage: "cache".to_string(),
                    done: 0,
                    total: 1,
                    file: Some(path.to_string_lossy().to_string()),
                    frames_done: frames_processed,
                    message: format!("正在写入缓存 {label}"),
                });
            }
        }
        if let Some(err) = reader.take_error() {
            warnings.push(format!(
                "{} 解析告警（缓存已写入已读帧）：{err}",
                path.display()
            ));
        }
        // done=1 ⇒ ProgressThrottle 把该文件计为完成。
        progress(Progress {
            phase: 1,
            stage: "cache".to_string(),
            done: 1,
            total: 1,
            file: Some(path.to_string_lossy().to_string()),
            frames_done: frames_processed,
            message: format!("已缓存 {label}"),
        });
    }

    let mut metas = Vec::with_capacity(writers.len());
    for (_, mut segment) in writers {
        segment.writer.flush().map_err(|err| {
            CoreError::new(
                DISK_FULL,
                format!(
                    "刷新帧缓存失败：{}（{err}）",
                    dir.join(&segment.meta.file).display()
                ),
            )
        })?;
        drop(segment.writer);
        if segment.unsorted {
            sort_segment(
                &dir.join(&segment.meta.file),
                segment.meta.stride as usize,
                segment.meta.frames,
            )?;
        }
        metas.push(segment.meta);
    }
    metas.sort_by_key(|message| (message.channel, message.frame_id));

    Ok(CacheMeta {
        format_version: CACHE_FORMAT_VERSION.to_string(),
        key: String::new(),
        dbc_hash: String::new(),
        message_set: Vec::new(),
        created_at: now_millis(),
        cache_bytes: 0,
        t_min: if scan.raw_min.is_finite() {
            scan.raw_min
        } else {
            0.0
        },
        t_max: if scan.raw_max.is_finite() {
            scan.raw_max
        } else {
            0.0
        },
        abs_min_micros: scan.abs_min_micros,
        abs_max_micros: scan.abs_max_micros,
        files: scan.files.clone(),
        messages: metas,
        warnings,
    })
}

fn write_record(
    writer: &mut BufWriter<File>,
    timestamp: f64,
    dlc: u8,
    data: &[u8],
    fd: bool,
) -> Result<(), CoreError> {
    let payload_len = if fd { PAYLOAD_FD } else { PAYLOAD_CLASSIC };
    let mut record = [0u8; FD_STRIDE];
    record[0..8].copy_from_slice(&timestamp.to_le_bytes());
    record[8] = dlc;
    let copy_len = data.len().min(payload_len);
    if let Some(target) = record.get_mut(9..9 + copy_len) {
        target.copy_from_slice(&data[..copy_len]);
    }
    let stride = if fd { FD_STRIDE } else { CLASSIC_STRIDE };
    writer
        .write_all(&record[..stride])
        .map_err(|err| CoreError::new(DISK_FULL, format!("写入帧缓存失败：{err}")))
}

struct RawRecord {
    timestamp: f64,
    body: [u8; FD_STRIDE],
}

fn sort_segment(path: &Path, stride: usize, frames: u64) -> Result<(), CoreError> {
    if frames <= 1 {
        return Ok(());
    }
    let mut reader = BufReader::new(File::open(path).map_err(|err| {
        CoreError::new(
            DISK_FULL,
            format!("重新打开帧缓存失败：{}（{err}）", path.display()),
        )
    })?);
    let mut records: Vec<RawRecord> = Vec::with_capacity(frames.min(usize::MAX as u64) as usize);
    for _ in 0..frames {
        let mut body = [0u8; FD_STRIDE];
        let Some(buf) = body.get_mut(..stride) else {
            return Err(CoreError::blf_parse("帧缓存 stride 非法"));
        };
        reader.read_exact(buf).map_err(|err| {
            CoreError::blf_parse(format!("读取帧缓存失败：{}（{err}）", path.display()))
        })?;
        let mut ts = [0u8; 8];
        if let Some(head) = buf.get(..8) {
            ts.copy_from_slice(head);
        }
        records.push(RawRecord {
            timestamp: f64::from_le_bytes(ts),
            body,
        });
    }
    records.sort_by(|a, b| {
        a.timestamp
            .partial_cmp(&b.timestamp)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut writer = BufWriter::new(File::create(path).map_err(|err| {
        CoreError::new(
            DISK_FULL,
            format!("重写帧缓存失败：{}（{err}）", path.display()),
        )
    })?);
    for record in records {
        let Some(buf) = record.body.get(..stride) else {
            return Err(CoreError::blf_parse("帧缓存 stride 非法"));
        };
        writer
            .write_all(buf)
            .map_err(|err| CoreError::new(DISK_FULL, format!("重写帧缓存失败：{err}")))?;
    }
    writer
        .flush()
        .map_err(|err| CoreError::new(DISK_FULL, format!("刷新帧缓存失败：{err}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 命中、预算与 LRU
// ---------------------------------------------------------------------------

fn load_matching_meta(dir: &Path, key: &str, required: &BTreeSet<(u32, u32)>) -> Option<CacheMeta> {
    let meta = read_meta(dir).ok()?;
    if meta.key != key || meta.format_version != CACHE_FORMAT_VERSION {
        return None;
    }
    for (channel, frame_id) in required {
        let message = meta.message(*channel, *frame_id)?;
        let expected = message.frames.saturating_mul(message.stride as u64);
        let actual = std::fs::metadata(message.path(dir)).ok()?.len();
        if actual != expected {
            return None;
        }
    }
    // 缓存里已有的每个段都必须完整，否则宁可重建。
    for message in &meta.messages {
        let expected = message.frames.saturating_mul(message.stride as u64);
        let actual = std::fs::metadata(message.path(dir)).ok()?.len();
        if actual != expected {
            return None;
        }
    }
    Some(meta)
}

fn enforce_lru(cache_dir: &Path, budget_mb: u64, current_key: &str) {
    let budget = budget_mb.saturating_mul(MIB);
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return;
    };
    let mut total: u64 = 0;
    let mut removable: Vec<(u64, PathBuf, u64)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let bytes = directory_bytes(&path);
        total = total.saturating_add(bytes);
        let name = entry.file_name().to_string_lossy().to_string();
        if name == current_key {
            continue;
        }
        if let Ok(meta) = read_meta(&path) {
            removable.push((meta.created_at, path, bytes));
        }
    }
    if total <= budget {
        return;
    }
    removable.sort_by_key(|(created_at, _, _)| *created_at);
    for (_, path, bytes) in removable {
        if total <= budget {
            break;
        }
        // 只删 cache_dir 的直接子目录，且绝不删当前正在用的 key。
        let direct_child = path.parent() == Some(cache_dir);
        let different_key = path.file_name().and_then(|name| name.to_str()) != Some(current_key);
        if direct_child && different_key && std::fs::remove_dir_all(&path).is_ok() {
            total = total.saturating_sub(bytes);
        }
    }
}

fn directory_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
        if path.is_dir() {
            total = total.saturating_add(directory_bytes(&path));
        }
    }
    total
}

fn write_meta(dir: &Path, meta: &CacheMeta) -> Result<(), CoreError> {
    let bytes = serde_json::to_vec_pretty(meta)
        .map_err(|err| CoreError::blf_parse(format!("序列化帧缓存元数据失败：{err}")))?;
    let tmp = dir.join("meta.json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|err| {
        CoreError::new(
            DISK_FULL,
            format!("写入帧缓存元数据失败：{}（{err}）", tmp.display()),
        )
    })?;
    let target = dir.join("meta.json");
    #[cfg(windows)]
    if target.exists() {
        let _ = std::fs::remove_file(&target);
    }
    std::fs::rename(&tmp, &target).map_err(|err| {
        CoreError::new(
            DISK_FULL,
            format!("替换帧缓存元数据失败：{}（{err}）", target.display()),
        )
    })
}

fn source_identity(path: &Path) -> Result<(PathBuf, SourceFileMeta), CoreError> {
    let metadata = std::fs::metadata(path).map_err(|err| {
        CoreError::new(
            BLF_OPEN,
            format!("无法读取 BLF 文件信息：{}（{err}）", path.display()),
        )
    })?;
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let modified = metadata.modified().ok();
    let (mtime_secs, mtime_nanos) = modified
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
        .unwrap_or((0, 0));
    Ok((
        absolute.clone(),
        SourceFileMeta {
            path: absolute.to_string_lossy().to_string(),
            size: metadata.len(),
            mtime_secs,
            mtime_nanos,
        },
    ))
}

fn absolute_micros(
    timing: &crate::decoder::FileTiming,
    raw: f64,
    wall_micros: Option<i64>,
) -> Option<i64> {
    let wall_micros = wall_micros?;
    let delta = (raw - timing.header_epoch) * 1_000_000.0;
    if !delta.is_finite() {
        return None;
    }
    Some((wall_micros as f64 + delta).round() as i64)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// 随机读取
// ---------------------------------------------------------------------------

/// 从 `offset` 读满 `buf`；Unix 用 `FileExt::read_at`，Windows 用 `FileExt::seek_read`。
#[cfg(unix)]
pub fn read_at_exact(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

/// Windows 的 `seek_read` 可能短读，这里循环直到读满或 EOF。
#[cfg(windows)]
pub fn read_at_exact(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let read = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "seek_read reached EOF",
            ));
        }
        done += read;
    }
    Ok(())
}

/// 其它平台的兜底（任务只要求 Unix/Windows；保留可编译性）。
#[cfg(not(any(unix, windows)))]
pub fn read_at_exact(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};
    let mut handle = file.try_clone()?;
    handle.seek(SeekFrom::Start(offset))?;
    handle.read_exact(buf)
}

/// 读取缓存段中的一条记录。
pub fn read_record_at(
    dir: &Path,
    message: &CachedMessageMeta,
    index: u64,
) -> Result<CachedRecord, CoreError> {
    let path = message.path(dir);
    let file = File::open(&path).map_err(|err| {
        CoreError::new(
            BLF_OPEN,
            format!("打开帧缓存失败：{}（{err}）", path.display()),
        )
    })?;
    read_record_at_file(&file, message, index, &path)
}

/// 与 [`read_record_at`] 相同，但复用已打开的句柄（`view` 边界帧兜底专用）。
pub fn read_record_at_file(
    file: &File,
    message: &CachedMessageMeta,
    index: u64,
    path: &Path,
) -> Result<CachedRecord, CoreError> {
    if index >= message.frames {
        return Err(CoreError::blf_parse(format!(
            "帧缓存索引越界：{} >= {}",
            index, message.frames
        )));
    }
    let stride = message.stride as usize;
    let mut buf = [0u8; FD_STRIDE];
    let Some(target) = buf.get_mut(..stride) else {
        return Err(CoreError::blf_parse("帧缓存 stride 非法"));
    };
    read_at_exact(file, target, index.saturating_mul(stride as u64)).map_err(|err| {
        CoreError::blf_parse(format!("读取帧缓存失败：{}（{err}）", path.display()))
    })?;
    let mut timestamp = [0u8; 8];
    if let Some(head) = buf.get(..8) {
        timestamp.copy_from_slice(head);
    }
    let dlc = buf.get(8).copied().unwrap_or(0);
    let data_len = if stride == FD_STRIDE {
        PAYLOAD_FD
    } else {
        PAYLOAD_CLASSIC
    };
    let mut payload = [0u8; PAYLOAD_FD];
    if let Some(source) = buf.get(9..9 + data_len) {
        if let Some(target) = payload.get_mut(..data_len) {
            target.copy_from_slice(source);
        }
    }
    Ok(CachedRecord {
        timestamp: f64::from_le_bytes(timestamp),
        dlc,
        payload,
        data_len,
    })
}

/// 读取缓存段第 `index` 条记录的时间戳（二分专用，只读 8 字节）。
pub fn read_timestamp_at(file: &File, stride: usize, index: u64) -> Result<f64, CoreError> {
    let mut buf = [0u8; 8];
    read_at_exact(file, &mut buf, index.saturating_mul(stride as u64))
        .map_err(|err| CoreError::blf_parse(format!("读取帧缓存时间戳失败：{err}")))?;
    Ok(f64::from_le_bytes(buf))
}

/// 在已排序的缓存段上二分定位 `[t0, t1]` 的记录下标范围（闭区间）。
pub fn find_record_range(
    file: &File,
    stride: usize,
    frames: u64,
    mut t0: f64,
    mut t1: f64,
) -> Result<Option<(u64, u64)>, CoreError> {
    if frames == 0 {
        return Ok(None);
    }
    if t0 > t1 {
        std::mem::swap(&mut t0, &mut t1);
    }
    // lower_bound: 第一个 >= t0。
    let mut lo = 0u64;
    let mut hi = frames;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let ts = read_timestamp_at(file, stride, mid)?;
        if ts < t0 {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let start = lo;
    if start >= frames {
        return Ok(None);
    }
    // upper_bound: 第一个 > t1，再减一。
    let mut lo = start;
    let mut hi = frames;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let ts = read_timestamp_at(file, stride, mid)?;
        if ts <= t1 {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let end = lo.saturating_sub(1);
    if end < start {
        Ok(None)
    } else {
        Ok(Some((start, end)))
    }
}

// ---------------------------------------------------------------------------
// SHA-256（不新增依赖；只用于缓存 key）
// ---------------------------------------------------------------------------

struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffered: usize,
    length: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: [0u8; 64],
            buffered: 0,
            length: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        if self.buffered > 0 {
            let need = 64 - self.buffered;
            let take = need.min(data.len());
            if let (Some(target), Some(source)) = (
                self.buffer.get_mut(self.buffered..self.buffered + take),
                data.get(..take),
            ) {
                target.copy_from_slice(source);
            }
            self.buffered += take;
            data = data.get(take..).unwrap_or(&[]);
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            if let Some(target) = self.buffer.get_mut(..data.len()) {
                target.copy_from_slice(data);
            }
            self.buffered = data.len();
        }
    }

    fn finish(mut self) -> [u8; 32] {
        let bit_len = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buffered != 56 {
            self.update(&[0]);
        }
        self.update(&bit_len.to_be_bytes());
        let mut out = [0u8; 32];
        for (index, word) in self.state.iter().enumerate() {
            let bytes = word.to_be_bytes();
            if let Some(target) = out.get_mut(index * 4..index * 4 + 4) {
                target.copy_from_slice(&bytes);
            }
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut w = [0u32; 64];
        for i in 0..16 {
            let base = i * 4;
            if let Some(bytes) = block.get(base..base + 4) {
                w[i] = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            }
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = self.state[0];
        let mut b = self.state[1];
        let mut c = self.state[2];
        let mut d = self.state[3];
        let mut e = self.state[4];
        let mut f = self.state[5];
        let mut g = self.state[6];
        let mut h = self.state[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        let mut hasher = Sha256::new();
        hasher.update(b"abc");
        assert_eq!(
            hex(&hasher.finish()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn budget_bounds_are_32_to_8192() {
        assert!(validate_budget(32).is_ok());
        assert!(validate_budget(8192).is_ok());
        assert!(validate_budget(31).is_err());
        assert!(validate_budget(8193).is_err());
    }

    #[test]
    fn budget_error_lists_three_actions() {
        let err = budget_error(300 * MIB, 32);
        assert_eq!(err.code, CACHE_BUDGET_EXCEEDED);
        assert!(err.message.contains("需要 300 MiB"), "{}", err.message);
        assert!(err.message.contains("上限 32 MiB"), "{}", err.message);
        assert!(err.message.contains("①提高上限(cache_budget_mb)"));
        assert!(err.message.contains("②减少所选报文"));
        assert!(err.message.contains("③不建缓存继续但较慢"));
    }

    fn fake_meta(key: &str, created_at: u64) -> CacheMeta {
        CacheMeta {
            format_version: CACHE_FORMAT_VERSION.to_string(),
            key: key.to_string(),
            dbc_hash: String::new(),
            message_set: Vec::new(),
            created_at,
            cache_bytes: 0,
            t_min: 0.0,
            t_max: 1.0,
            abs_min_micros: None,
            abs_max_micros: None,
            files: Vec::new(),
            messages: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn lru_deletes_oldest_but_never_current_key() {
        let dir = tempfile::tempdir().expect("临时目录");
        for (name, created_at) in [("old", 1u64), ("middle", 2), ("current", 3)] {
            let key_dir = dir.path().join(name);
            std::fs::create_dir_all(&key_dir).expect("创建 key 目录");
            std::fs::write(key_dir.join("ch1_id1.bin"), [0u8; 8]).expect("写入数据");
            write_meta(&key_dir, &fake_meta(name, created_at)).expect("写 meta");
        }
        enforce_lru(dir.path(), 0, "current");
        assert!(!dir.path().join("old").exists(), "最旧的 key 应被删");
        assert!(!dir.path().join("middle").exists(), "次旧的 key 应被删");
        assert!(
            dir.path().join("current").exists(),
            "当前正在用的 key 绝不能删"
        );
    }
}
