//! BLF 扫描与 DBC 解码计划（Step 2a）。
//!
//! 本模块负责两件事：
//! 1. 把若干通道的 DBC 预编译成 `DecodePlan`（`(channel_1based, frame_id) -> MessagePlan` 的
//!    O(1) 查表），供 convert 阶段"即时过滤"用；
//! 2. `scan_file` 只读 BLF 帧头，统计**全部**出现的 `(channel, id)` 计数与时间戳中位数间隔，
//!    不做任何信号位提取。
//!
//! 两条不可动摇的铁律（见 `docs/DEPS.md` §1）：
//! * **通道号**：`blf_asc` 给 0-based（Ch1 → 0），本模块在入口处**只转换一次**
//!   `let ch = msg.channel as u32 + 1;`，之后全用 1-based。
//! * **帧 ID 归一化**：`id | if is_extended { 1<<31 }`，与 `can_dbc::MessageId::raw()` 对齐，
//!   标准帧 `0x123` 绝不能命中扩展帧 key。
//!
//! 契约：`docs/INTERFACE.md` §1/§2/§3，任务书 `docs/tasks/step2a_decoder.md`。

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use blf_asc::BlfReader;
use can_dbc::{
    ByteOrder as DbcByteOrder, Dbc, NumericValue, Signal as DbcSignal, ValueType as DbcValueType,
};
use chrono::{DateTime, NaiveDate, Utc};
use polars::prelude::*;

use crate::bit_slice::{decode_physical, extract_bits, ByteOrder};
use crate::dbc_norm::normalize_dbc_bytes;
use crate::error::CoreError;

/// 绝对时间口径要求 BLF 文件头携带可用墙钟，但文件头无效（Step 7b）。
pub const BAD_TIME_HEADER: &str = "BAD_TIME_HEADER";

/// 进度回调载荷（字段与 `docs/INTERFACE.md` §2 一一对应）。
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    /// 1 = 并行解码/扫描，2 = 合并与导出。
    pub phase: u8,
    /// `"scan"` / `"decode"` / `"merge"` / `"export"`。
    pub stage: String,
    /// 已完成计数（phase 1 = 已完成文件数，由 `lib.rs` 汇总；phase 2 = 已完成步数）。
    pub done: u64,
    /// 总数（phase 1 = 文件总数；phase 2 = 总步数）。
    pub total: u64,
    /// 当前文件（decode/scan 阶段）。
    pub file: Option<String>,
    /// 当前文件内已解码/已读取的帧数（单调递增；phase 2 恒为 0）。
    pub frames_done: u64,
    /// 可直接显示的短句。
    pub message: String,
}

/// BLF 文件头里的录制机墙钟时间（`SYSTEMTIME`，**不含时区**）。
///
/// 字段顺序与 `docs/DEPS.md` §8 一致；`day_of_week` 被刻意丢弃（不参与任何计算）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WallClock {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub millis: u16,
}

impl WallClock {
    /// 字段范围校验：magic 之外的无效判定全部收敛在这里。
    ///
    /// 只按任务书列出的 `year < 1601` 与 `month/day/hour/minute/second` 越界判定；
    /// `millis` 不参与判定（0..=999 之外也接受，原样格式化）。
    pub fn is_valid(&self) -> bool {
        self.year >= 1601
            && (1..=12).contains(&self.month)
            && (1..=31).contains(&self.day)
            && self.hour <= 23
            && self.minute <= 59
            && self.second <= 59
    }

    /// 按 **UTC** 把墙钟解释成 epoch 秒（精度到毫秒）；字段本身非法（如 2 月 30 日）返回 `None`。
    ///
    /// 这里刻意只做"解释"，绝不做本地时区换算：`absolute` 口径要求原样输出墙钟。
    pub fn epoch_seconds(&self) -> Option<f64> {
        let naive = self.naive_datetime()?;
        let dt: DateTime<Utc> = DateTime::from_naive_utc_and_offset(naive, Utc);
        Some(dt.timestamp() as f64 + self.millis as f64 / 1000.0)
    }

    /// 墙钟的微秒级整数 epoch（`secs * 1e6 + millis * 1000`），供 `absolute` 整数运算使用。
    pub fn epoch_micros(&self) -> Option<i64> {
        let naive = self.naive_datetime()?;
        let dt: DateTime<Utc> = DateTime::from_naive_utc_and_offset(naive, Utc);
        Some(dt.timestamp() * 1_000_000 + self.millis as i64 * 1000)
    }

    fn naive_datetime(&self) -> Option<chrono::NaiveDateTime> {
        let date = NaiveDate::from_ymd_opt(self.year as i32, self.month as u32, self.day as u32)?;
        date.and_hms_opt(self.hour as u32, self.minute as u32, self.second as u32)
    }
}

/// 单个输入文件的头部时间基准。
///
/// * `wall`：文件头墙钟；头部无效（magic 不符 / 读不到 16 字节 / 字段越界）时为 `None`；
/// * `header_epoch`：墙钟按 **UTC** 解释的 epoch 秒；`wall = None` 时用该文件第一帧的
///   `msg.timestamp` 兜底（保证 `unix_s` / `relative_*` 不受影响）。
#[derive(Debug, Clone)]
pub struct FileTiming {
    pub path: PathBuf,
    pub wall: Option<WallClock>,
    pub header_epoch: f64,
}

/// 读 BLF 文件头的墙钟时间（`docs/DEPS.md` §8）。
///
/// 读前 144 字节并校验 magic `LOGG`；从**偏移 40** 起读 8 个 LE u16：
/// `(year, month, dow, day, hour, minute, second, millis)`，`dow` 丢弃。
/// 任一无效条件成立都只把 `wall` 置 `None`，绝不报错（调用方按口径决定是否失败）。
pub fn read_file_timing(path: &Path) -> FileTiming {
    let mut wall = None;
    let mut header_epoch = 0.0;

    if let Some(bytes) = read_header_bytes(path) {
        // "读不到 16 字节" ⇒ 文件不足 56 字节；magic 也必须在。
        if bytes.len() >= 56 && &bytes[0..4] == b"LOGG" {
            let mut arr = [0u16; 8];
            for i in 0..8 {
                arr[i] = u16::from_le_bytes([bytes[40 + i * 2], bytes[40 + i * 2 + 1]]);
            }
            let candidate = WallClock {
                year: arr[0],
                month: arr[1],
                day: arr[3],
                hour: arr[4],
                minute: arr[5],
                second: arr[6],
                millis: arr[7],
            };
            if candidate.is_valid() {
                if let Some(epoch) = candidate.epoch_seconds() {
                    wall = Some(candidate);
                    header_epoch = epoch;
                }
            }
        }
    }

    if wall.is_none() {
        // 兜底：用该文件第一帧的时间戳，保证相对/绝对秒口径仍可用。
        if let Ok(mut reader) = BlfReader::open(path) {
            if let Some(msg) = reader.next() {
                header_epoch = msg.timestamp;
            }
        }
    }

    FileTiming {
        path: path.to_path_buf(),
        wall,
        header_epoch,
    }
}

/// 读取文件前 144 字节（不足则返回已读部分）；打不开返回 `None`。
fn read_header_bytes(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut buf = vec![0u8; 144];
    let mut total = 0usize;
    while total < buf.len() {
        match file.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(_) => return None,
        }
    }
    buf.truncate(total);
    Some(buf)
}

/// 全局限：本次任务**全部输入文件全部帧**的最小 `msg.timestamp`。
///
/// 解码期由 rayon 工作线程并发更新，用 CAS 循环对 `f64` 的位模式取最小；
/// 导出期只读一次，绝不在导出阶段重算。
#[derive(Debug)]
pub struct GlobalMin {
    bits: AtomicU64,
}

impl GlobalMin {
    pub fn new() -> Self {
        Self {
            bits: AtomicU64::new(f64::INFINITY.to_bits()),
        }
    }

    /// 观测一个时间戳并原子地更新最小值；非有限值直接忽略。
    pub fn observe(&self, timestamp: f64) {
        if !timestamp.is_finite() {
            return;
        }
        let mut current = self.bits.load(Ordering::Relaxed);
        loop {
            let current_value = f64::from_bits(current);
            if timestamp >= current_value {
                return;
            }
            match self.bits.compare_exchange_weak(
                current,
                timestamp.to_bits(),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// 当前全局最小；一帧都没观测到时返回 `f64::INFINITY`。
    pub fn get(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Acquire))
    }
}

impl Default for GlobalMin {
    fn default() -> Self {
        Self::new()
    }
}

/// 一个已编译的信号：位序、位宽、符号与线性换算参数全部预计算，热路径不做字符串比较。
#[derive(Debug, Clone, PartialEq)]
pub struct SignalPlan {
    pub name: String,
    pub start_bit: u32,
    pub bit_len: u32,
    pub order: ByteOrder,
    pub is_signed: bool,
    pub factor: f64,
    pub offset: f64,
    pub unit: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl SignalPlan {
    /// 从一帧数据解出物理量：`raw * factor + offset`。
    ///
    /// 位提取复用 `bit_slice::extract_bits`（Intel/Motorola 位序与二补码已由单元测试锁死），
    /// 本方法只做组合，绝不重复实现位运算。
    pub fn decode(&self, data: &[u8]) -> Result<f64, CoreError> {
        let raw = extract_bits(data, self.start_bit, self.bit_len, self.order)?;
        Ok(decode_physical(
            raw,
            self.bit_len,
            self.is_signed,
            self.factor,
            self.offset,
        ))
    }
}

/// 一个已编译的报文：`channel` 恒为 1-based，`frame_id` 恒为归一化 raw id。
#[derive(Debug, Clone, PartialEq)]
pub struct MessagePlan {
    /// 1-based 通道号！入口处已转换。
    pub channel: u32,
    /// 归一化：`id | (is_extended ? 1<<31 : 0)`，与 `can_dbc::MessageId::raw()` 对齐。
    pub frame_id: u32,
    pub name: String,
    /// DBC 不携带 FD 标志，这里用 `size > 8` 推断（>8 字节必为 CAN FD）。
    pub is_fd: bool,
    pub signals: Vec<SignalPlan>,
}

/// 预编译解码计划：`(channel_1based, frame_id)` → `MessagePlan`，O(1) 查找。
#[derive(Debug, Default)]
pub struct DecodePlan {
    map: HashMap<(u32, u32), MessagePlan>,
}

impl DecodePlan {
    /// 构建计划；任一通道的 DBC 读不到/解析失败即整体失败（供 convert 使用）。
    pub fn build(
        dbc_by_channel: &BTreeMap<u32, PathBuf>,
        selected: &Selection,
    ) -> Result<Self, CoreError> {
        let (plan, errors, _notes) = Self::build_lenient(dbc_by_channel, selected);
        if let Some((channel, err)) = errors.into_iter().next() {
            return Err(CoreError::with_detail(
                err.code,
                format!("通道 {channel} 的 DBC 构建失败"),
                err.message,
            ));
        }
        Ok(plan)
    }

    /// 宽松构建：解析失败的通道记入返回的 `errors`，但不影响其它通道（供 scan / convert 使用）。
    ///
    /// 返回值：
    /// * `.0` = 解码计划；
    /// * `.1` 的 key 是 1-based 通道号，`CoreError` 的 `Display` 即 `"{code}: {message}"`；
    /// * `.2` = 每个通道的 **DBC 规范化 notes**（去 BOM / GB18030 解码 / 忽略语句），
    ///   由调用方加 `DBC「<文件名>」：` 前缀后并入 `warnings`；解析失败的通道也会带上，
    ///   且同样的 notes 已写进 `.1` 的错误消息里，便于用户定位。
    ///
    /// 编码与不支持段的预规范化统一走 [`normalize_dbc_bytes`]，scan 与 convert 共用同一条路径。
    pub fn build_lenient(
        dbc_by_channel: &BTreeMap<u32, PathBuf>,
        selected: &Selection,
    ) -> (
        Self,
        BTreeMap<u32, CoreError>,
        BTreeMap<u32, Vec<String>>,
    ) {
        let mut map: HashMap<(u32, u32), MessagePlan> = HashMap::new();
        let mut errors: BTreeMap<u32, CoreError> = BTreeMap::new();
        let mut notes_by_channel: BTreeMap<u32, Vec<String>> = BTreeMap::new();

        for (&channel, path) in dbc_by_channel {
            // 读原始字节（不要 read_to_string：GBK/ANSI 文件在严格 UTF-8 下会直接失败）。
            let raw = match std::fs::read(path) {
                Ok(raw) => raw,
                Err(err) => {
                    errors.insert(
                        channel,
                        CoreError::dbc_parse(format!(
                            "读取 DBC 失败：{}（{err}）",
                            path.display()
                        )),
                    );
                    continue;
                }
            };

            // 文件名用于 warning/错误前缀；没有文件名时退回完整路径。
            let label = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
                .unwrap_or_else(|| path.display().to_string());

            let outcome = match normalize_dbc_bytes(&raw, &label) {
                Ok(outcome) => outcome,
                Err(reason) => {
                    errors.insert(
                        channel,
                        CoreError::dbc_parse(format!("解析 DBC 失败：{}（{reason}）", path.display())),
                    );
                    continue;
                }
            };
            let notes = outcome.notes;

            let dbc = match Dbc::try_from(outcome.text.as_str()) {
                Ok(dbc) => dbc,
                Err(err) => {
                    let mut message = format!("解析 DBC 失败：{}（{err}）", path.display());
                    if !notes.is_empty() {
                        message.push_str(&format!("；规范化说明：{}", notes.join("；")));
                    }
                    errors.insert(channel, CoreError::dbc_parse(message));
                    if !notes.is_empty() {
                        notes_by_channel.insert(channel, notes);
                    }
                    continue;
                }
            };
            if !notes.is_empty() {
                notes_by_channel.insert(channel, notes);
            }

            for msg in &dbc.messages {
                // 只收录被选中的报文；未命中即跳过，convert 阶段也就零信号解码开销。
                let Some(filter) = selected.resolve(channel, &msg.name) else {
                    continue;
                };
                let signals = msg
                    .signals
                    .iter()
                    .filter(|sig| filter.is_empty() || filter.iter().any(|n| n == &sig.name))
                    .map(convert_signal)
                    .collect();
                let frame_id = msg.id.raw();
                map.insert(
                    (channel, frame_id),
                    MessagePlan {
                        channel,
                        frame_id,
                        name: msg.name.clone(),
                        is_fd: msg.size > 8,
                        signals,
                    },
                );
            }
        }

        (Self { map }, errors, notes_by_channel)
    }

    /// O(1) 查找：`channel` 为 1-based，`frame_id` 为归一化 raw id。
    pub fn lookup(&self, channel: u32, frame_id: u32) -> Option<&MessagePlan> {
        self.map.get(&(channel, frame_id))
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn messages(&self) -> impl Iterator<Item = &MessagePlan> {
        self.map.values()
    }
}

/// 信号勾选结果：通道号(1-based) → 报文名 → 勾选的信号名列表。
///
/// 语义与 `docs/INTERFACE.md` §1.1 的 `selected` 一致：
/// * 外层为空 ⇒ **全部选中**（convert 的默认行为）；
/// * 外层非空时，只有出现在表里的 `(通道, 报文)` 才被收录；
/// * 某报文的信号列表为空 ⇒ 该报文的全部信号。
#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub channels: HashMap<u32, HashMap<String, Vec<String>>>,
}

impl Selection {
    pub fn new() -> Self {
        Self::default()
    }

    /// 判别 `(channel, message)` 是否入选，并返回信号白名单。
    ///
    /// * `Some(空切片)` ⇒ 入选且全选信号；
    /// * `Some(名单)` ⇒ 入选且只解名单内信号；
    /// * `None` ⇒ 不入选，convert 阶段就地跳过。
    pub fn resolve<'a>(&'a self, channel: u32, message: &str) -> Option<&'a [String]> {
        if self.channels.is_empty() {
            return Some(&[]);
        }
        let per_channel = self.channels.get(&channel)?;
        let signals = per_channel.get(message)?;
        Some(signals.as_slice())
    }
}

/// 单个 BLF 文件的只读扫描结果（全部为观测值，包含未绑定 DBC 的通道/报文）。
#[derive(Debug, Clone, Default)]
pub struct FileScan {
    pub path: String,
    pub size: u64,
    pub ok: bool,
    pub error: Option<String>,
    /// 该文件里出现的**全部** `(channel_1based, frame_id)` 计数，未命中 DBC 也要统计。
    pub frame_counts: HashMap<(u32, u32), u64>,
    /// 中位数帧间隔（ms），时间戳样本 < 3 则缺省。
    pub cycle_estimate: HashMap<(u32, u32), f64>,
    /// 观测到的 FD 标志：只要该 `(channel, id)` 有一帧 `is_fd` 即为 true。
    ///
    /// 任务书给出的 `FileScan` 清单未包含此字段，但 Python 返回值（INTERFACE §1）要求
    /// 每个报文带 `is_fd`，且 scan 是"观测阶段"，因此这里如实记录帧头里的 FD 标志；
    /// 相比从 DBC `size > 8` 猜测，观测值才是正确来源。
    pub frame_is_fd: HashMap<(u32, u32), bool>,
}

/// 文件显示名（消息里用人话短句）：取 `file_name`，拿不到就退回完整路径。
fn file_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// 扫描一个 BLF 文件：只读帧头，统计全部 `(channel, id)` 计数与中位数周期。
///
/// * 不做任何信号位提取（零反序列化开销），也不按 DBC 过滤——未绑 DBC 的报文同样入账。
/// * 单文件打不开/解析失败只写进 `error`（`BLF_OPEN` / `BLF_PARSE`），不向上传播，
///   以免拖垮整批任务。
/// * `progress` 在循环内按 ≥50ms 节流触发；文件开始/首个命中帧/文件结束各额外发一次。
///   调用方需保证回调线程安全（`lib.rs` 用 `ProgressThrottle` 再统一节流）。
///
/// `_plan` 保留参数以与 convert 的 `process_file` 同构；scan 刻意不读它（全部报文都要统计）。
pub fn scan_file(path: &Path, _plan: &DecodePlan, progress: &dyn Fn(Progress)) -> FileScan {
    let path_str = path.to_string_lossy().to_string();
    let label = file_label(path);
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    let mut frame_counts: HashMap<(u32, u32), u64> = HashMap::new();
    let mut frame_is_fd: HashMap<(u32, u32), bool> = HashMap::new();
    let mut last_ts: HashMap<(u32, u32), f64> = HashMap::new();
    let mut intervals: HashMap<(u32, u32), Vec<f64>> = HashMap::new();

    // 文件开始：done=0 表示本文件尚未完成；`lib.rs` 会把 done/total 映射成全局文件计数。
    // 先于 open 发出：即使打开失败，也保证文件计数最终到达 total。
    progress(Progress {
        phase: 1,
        stage: "scan".to_string(),
        done: 0,
        total: 1,
        file: Some(path_str.clone()),
        frames_done: 0,
        message: format!("正在扫描 {label}"),
    });

    let mut reader = match BlfReader::open(path) {
        Ok(reader) => reader,
        Err(err) => {
            // 结束回调：done=1 ⇒ 文件计数递增，避免失败文件让进度永远差一格。
            progress(Progress {
                phase: 1,
                stage: "scan".to_string(),
                done: 1,
                total: 1,
                file: Some(path_str.clone()),
                frames_done: 0,
                message: format!("扫描失败 {label}"),
            });
            return FileScan {
                path: path_str,
                size,
                ok: false,
                error: Some(CoreError::blf_open(format!("无法打开 BLF 文件：{err}")).to_string()),
                frame_counts,
                cycle_estimate: HashMap::new(),
                frame_is_fd,
            };
        }
    };

    let throttle = Duration::from_millis(50);
    let mut last_emit = Instant::now();
    let mut frames: u64 = 0;
    let mut emitted_first = false;

    // 用 Iterator 而非手写 `next_message()?`：blf_asc 只在 Iterator 路径把解析错误存进
    // `reader.error`，之后 `take_error()` 才能取到（DEPS §1 要求的"非致命错误不中断"）。
    for msg in reader.by_ref() {
        // 通道 0-based → 1-based，全模块唯一一次转换。
        let channel = msg.channel as u32 + 1;
        let frame_id = normalize_frame_id(&msg);
        let key = (channel, frame_id);

        *frame_counts.entry(key).or_insert(0) += 1;
        frame_is_fd.entry(key).or_insert(msg.is_fd);
        if let Some(&prev) = last_ts.get(&key) {
            intervals.entry(key).or_default().push(msg.timestamp - prev);
        }
        last_ts.insert(key, msg.timestamp);

        frames += 1;

        // 首个命中帧立即上报一次（让界面"动起来"），之后按 50ms 节流。
        let due = !emitted_first || last_emit.elapsed() >= throttle;
        if due {
            emitted_first = true;
            last_emit = Instant::now();
            progress(Progress {
                phase: 1,
                stage: "scan".to_string(),
                done: 0,
                total: 1,
                file: Some(path_str.clone()),
                frames_done: frames,
                message: format!("扫描中 {label}"),
            });
        }
    }

    let error = reader
        .take_error()
        .map(|err| CoreError::blf_parse(format!("解析 BLF 时出错：{err}")).to_string());

    let cycle_estimate = intervals
        .into_iter()
        .filter_map(|(key, mut diffs)| median(&mut diffs).map(|median_s| (key, median_s * 1000.0)))
        .collect();

    // 收尾回调：即使 0 帧也要让调用方看到该文件已处理完毕（done=1 ⇒ 本文件完成）。
    progress(Progress {
        phase: 1,
        stage: "scan".to_string(),
        done: 1,
        total: 1,
        file: Some(path_str.clone()),
        frames_done: frames,
        message: format!("已扫描 {label}"),
    });

    FileScan {
        path: path_str,
        size,
        ok: error.is_none(),
        error,
        frame_counts,
        cycle_estimate,
        frame_is_fd,
    }
}

/// 帧 ID 归一化：`id | if is_extended_id { 1<<31 }`（与 `can_dbc::MessageId::raw()` 对齐）。
#[inline]
fn normalize_frame_id(msg: &blf_asc::Message) -> u32 {
    msg.arbitration_id.0 | if msg.is_extended_id { 1 << 31 } else { 0 }
}

/// 中位数；空切片返回 `None`。时间戳样本 < 3（即间隔数 < 2）时调用方据此缺省。
fn median(samples: &mut [f64]) -> Option<f64> {
    if samples.len() < 2 {
        return None;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = samples.len() / 2;
    Some(if samples.len() % 2 == 0 {
        (samples[mid - 1] + samples[mid]) / 2.0
    } else {
        samples[mid]
    })
}

/// DBC 信号 → 预编译信号计划。
fn convert_signal(sig: &DbcSignal) -> SignalPlan {
    let order = match sig.byte_order {
        DbcByteOrder::LittleEndian => ByteOrder::Intel,
        DbcByteOrder::BigEndian => ByteOrder::Motorola,
    };
    let is_signed = matches!(sig.value_type, DbcValueType::Signed);
    let min = numeric_to_f64(sig.min);
    let max = numeric_to_f64(sig.max);
    // DBC 里 `[0|0]` 是"未指定"的常见写法，映射为 None；否则如实给出。
    let (min, max) = if min == 0.0 && max == 0.0 {
        (None, None)
    } else {
        (Some(min), Some(max))
    };
    SignalPlan {
        name: sig.name.clone(),
        start_bit: sig.start_bit as u32,
        bit_len: sig.size as u32,
        order,
        is_signed,
        factor: sig.factor,
        offset: sig.offset,
        unit: sig.unit.clone(),
        min,
        max,
    }
}

/// `can_dbc::NumericValue` 没有公开的 f64 转换方法，按变体显式转换。
fn numeric_to_f64(value: NumericValue) -> f64 {
    match value {
        NumericValue::Uint(v) => v as f64,
        NumericValue::Int(v) => v as f64,
        NumericValue::Double(v) => v,
    }
}

// ---------------------------------------------------------------------------
// Step 2b：分片（shard）产出与登记
// ---------------------------------------------------------------------------

/// 单个表缓冲达到的行数上限：先到者触发 flush。
const FLUSH_ROWS: usize = 200_000;

/// 一张输出表的唯一标识：1-based 通道 + 报文名。
///
/// `Ord` 让 `BTreeMap` 天然按 `(channel, message)` 排序，保证输出顺序稳定。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TableKey {
    /// 1-based 通道号。
    pub channel: u32,
    /// DBC 报文名。
    pub message: String,
}

/// 一个已落盘的 Parquet 分片：路径 + 它所属的输入 BLF 文件。
///
/// 一个分片内的所有行都来自同一个输入文件（分片按 `(file, channel, message)` 写），
/// 因此导出期可以**按分片**取该文件的 [`FileTiming`] 来改写 `timestamp` 列。
#[derive(Debug, Clone)]
pub struct ShardRef {
    pub path: PathBuf,
    /// 写出该分片的输入 BLF 文件路径。
    pub file: PathBuf,
}

/// 一张表在临时目录里的全部分片及其元信息。
#[derive(Debug, Clone)]
pub struct TableShards {
    /// 清洗后的子表名，如 `Ch1_VehSpd`。
    pub name: String,
    /// 1-based 通道号。
    pub channel: u32,
    /// DBC 报文名。
    pub message: String,
    /// 信号名，顺序严格等于 DBC 里信号出现顺序（也是输出列顺序）。
    pub signal_names: Vec<String>,
    /// 该表已落盘的全部 Parquet 分片。
    pub shards: Vec<ShardRef>,
    /// 分片里的原始行数之和（去重前的上界，供 Excel 防呆估计用）。
    pub rows: u64,
}

/// 单个文件的解码结果。
#[derive(Debug, Clone)]
pub struct FileStat {
    pub path: String,
    /// 成功落盘的帧数（值已进分片）。
    pub rows: u64,
    /// 非致命解析错误（`BLF_PARSE`），不中断整批任务。
    pub error: Option<String>,
    /// 因信号位提取失败而跳过的帧数。
    pub decode_errors: u64,
}

/// 文件名/表名清洗：只保留 `[A-Za-z0-9_]`，其余字符替换成 `_`。
pub fn sanitize_identifier(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// 子表名：`Ch{channel}_{sanitized_message}`。
pub fn table_name(channel: u32, message: &str) -> String {
    format!("Ch{}_{}", channel, sanitize_identifier(message))
}

/// 线程安全的分片登记处。
///
/// `convert` 用 rayon 并行处理多个文件，每个文件在本地维护自己的列缓冲，
/// 只有「写入分片 + 登记路径」这一步需要跨线程串行化，因此把登记表放在 `Mutex` 里。
/// 文件名里的 part 序号由全局计数器分配，保证唯一（即使清洗后表名撞车也不会覆盖）。
pub struct ShardSink {
    temp_dir: PathBuf,
    inner: Mutex<ShardRegistry>,
    /// 本次任务全部输入文件全部帧的最小时间戳（解码期原子更新）。
    global_min: GlobalMin,
}

struct ShardRegistry {
    tables: BTreeMap<TableKey, TableShards>,
    next_part: u64,
}

impl ShardSink {
    /// 创建 sink；`temp_dir` 不存在则创建。
    pub fn new(temp_dir: impl Into<PathBuf>) -> Result<Self, CoreError> {
        let temp_dir = temp_dir.into();
        std::fs::create_dir_all(&temp_dir).map_err(|err| {
            CoreError::export_failed(format!(
                "创建分片临时目录失败：{}（{err}）",
                temp_dir.display()
            ))
        })?;
        Ok(Self {
            temp_dir,
            inner: Mutex::new(ShardRegistry {
                tables: BTreeMap::new(),
                next_part: 0,
            }),
            global_min: GlobalMin::new(),
        })
    }

    /// 观测一个原始 `msg.timestamp`，原子维护全局限（解码热路径调用）。
    pub fn observe_timestamp(&self, timestamp: f64) {
        self.global_min.observe(timestamp);
    }

    /// 取本次任务的全局最小时间戳（导出期只读一次）。
    pub fn global_min(&self) -> f64 {
        self.global_min.get()
    }

    /// 分配一个全局唯一的分片路径（此时尚未登记，写盘成功后由 `commit` 登记）。
    fn reserve(&self, key: &TableKey) -> (PathBuf, String) {
        let mut guard = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let part = guard.next_part;
        guard.next_part += 1;
        let name = table_name(key.channel, &key.message);
        let path = self.temp_dir.join(format!("{name}__{part}.parquet"));
        (path, name)
    }

    /// 写盘成功后登记分片（含所属文件）与行数。
    fn commit(
        &self,
        key: TableKey,
        file: PathBuf,
        name: String,
        signal_names: Vec<String>,
        path: PathBuf,
        rows: u64,
    ) {
        let channel = key.channel;
        let message = key.message.clone();
        let mut guard = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let entry = guard.tables.entry(key).or_insert_with(|| TableShards {
            name,
            channel,
            message,
            signal_names,
            shards: Vec::new(),
            rows: 0,
        });
        entry.shards.push(ShardRef { path, file });
        entry.rows += rows;
    }

    /// 取一份登记表快照（深拷贝分片路径清单，供导出阶段消费）。
    pub fn tables_snapshot(&self) -> BTreeMap<TableKey, TableShards> {
        let guard = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        guard.tables.clone()
    }

    /// 消费 sink，取出最终登记表。
    pub fn into_tables(self) -> BTreeMap<TableKey, TableShards> {
        self.inner
            .into_inner()
            .unwrap_or_else(|err| err.into_inner())
            .tables
    }

    /// 已登记的**全部**分片路径（用于任务结束后只删自己写出的 parquet）。
    pub fn shard_paths(&self) -> Vec<PathBuf> {
        let guard = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        guard
            .tables
            .values()
            .flat_map(|table| table.shards.iter().map(|shard| shard.path.clone()))
            .collect()
    }

    /// 把一张表的列缓冲落成 Zstd Parquet 分片并登记；空缓冲直接跳过。
    ///
    /// 这里用 `Series::from_vec` **接管** `Vec<f64>` 的所有权（而不是 `Column::new` 拷贝），
    /// 避免「缓冲 + DataFrame」同时存在导致内存峰值翻倍。`file` 是这些行所属的输入 BLF。
    pub fn flush_table(
        &self,
        key: &TableKey,
        file: &Path,
        signal_names: &[String],
        timestamps: &mut Vec<f64>,
        columns: &mut [Vec<f64>],
    ) -> Result<(), CoreError> {
        if timestamps.is_empty() {
            return Ok(());
        }
        let rows = timestamps.len() as u64;
        let (path, name) = self.reserve(key);

        // 用 take 把数据搬进 DataFrame，避免拷贝。
        let ts = std::mem::take(timestamps);
        let mut cols: Vec<Column> = Vec::with_capacity(1 + signal_names.len());
        cols.push(Column::from(Series::from_vec("timestamp".into(), ts)));
        for (idx, col_name) in signal_names.iter().enumerate() {
            let values = std::mem::take(&mut columns[idx]);
            cols.push(Column::from(Series::from_vec(
                col_name.as_str().into(),
                values,
            )));
        }
        let mut df = DataFrame::new_infer_height(cols).map_err(|err| {
            CoreError::export_failed(format!("组装分片 DataFrame 失败（{name}）：{err}"))
        })?;

        write_parquet(&mut df, &path)?;
        self.commit(
            key.clone(),
            file.to_path_buf(),
            name,
            signal_names.to_vec(),
            path,
            rows,
        );

        // 数据已交给 Polars，释放本地缓冲。
        timestamps.clear();
        for column in columns {
            column.clear();
        }
        Ok(())
    }
}

/// 把 DataFrame 写成 Zstd（级别 3）Parquet。
fn write_parquet(df: &mut DataFrame, path: &Path) -> Result<(), CoreError> {
    let file = File::create(path).map_err(|err| {
        CoreError::export_failed(format!("创建 Parquet 分片失败：{}（{err}）", path.display()))
    })?;
    ParquetWriter::new(file)
        .with_compression(ParquetCompression::Zstd(Some(Default::default())))
        .finish(df)
        .map_err(|err| {
            CoreError::export_failed(format!("写入 Parquet 分片失败：{}（{err}）", path.display()))
        })?;
    Ok(())
}

/// 单文件内的列缓冲。
struct TableBuffer {
    signal_names: Vec<String>,
    timestamps: Vec<f64>,
    columns: Vec<Vec<f64>>,
}

impl TableBuffer {
    fn new(plan: &MessagePlan) -> Self {
        let signal_names = plan.signals.iter().map(|sig| sig.name.clone()).collect();
        let columns = plan.signals.iter().map(|_| Vec::new()).collect();
        Self {
            signal_names,
            timestamps: Vec::new(),
            columns,
        }
    }

    /// 估算内存占用（字节）：每行 1 个 timestamp + 每信号一列，均为 f64。
    fn estimated_bytes(&self) -> usize {
        self.timestamps
            .len()
            .saturating_mul(1 + self.columns.len())
            .saturating_mul(std::mem::size_of::<f64>())
    }
}

/// 解码一个 BLF 文件：命中 `plan` 的帧即时解出信号，按 `(channel, message)` 落 Parquet 分片。
///
/// * 未命中 `plan` 的 `(channel, id)` 立刻跳过，不访问数据、不做任何拷贝。
/// * 每个表缓冲达到 `FLUSH_ROWS` 行或 `mem_budget` 字节即 flush；文件结束再 flush 一次，
///   从而保证「多个文件的全量数据不会同时留在内存里」。
/// * 单文件打不开返回 `Err(BLF_OPEN)`；解析告警写进 [`FileStat::error`]，不中断整批任务。
/// * `progress` 在解码循环内部按 ≥50ms 节流触发，`frames_done` 为当前文件已解码帧数；
///   文件开始/首个解码帧/文件结束各额外发一次（`done/total` 是本文件维度，`lib.rs`
///   会把它映射成全局文件计数）。
pub fn process_file(
    path: &Path,
    plan: &DecodePlan,
    sink: &ShardSink,
    mem_budget: usize,
    progress: &dyn Fn(Progress),
) -> Result<FileStat, CoreError> {
    let path_str = path.to_string_lossy().to_string();
    let label = file_label(path);

    // 文件开始：done=0 表示本文件尚未完成（映射见 `lib.rs::ProgressThrottle`）。
    // 先于 open 发出：即使打开失败，也保证文件计数最终到达 total。
    progress(Progress {
        phase: 1,
        stage: "decode".to_string(),
        done: 0,
        total: 1,
        file: Some(path_str.clone()),
        frames_done: 0,
        message: format!("正在解码 {label}"),
    });

    let mut reader = match BlfReader::open(path) {
        Ok(reader) => reader,
        Err(err) => {
            // 结束回调：done=1 ⇒ 文件计数递增，避免失败文件让进度永远差一格。
            progress(Progress {
                phase: 1,
                stage: "decode".to_string(),
                done: 1,
                total: 1,
                file: Some(path_str.clone()),
                frames_done: 0,
                message: format!("解码失败 {label}"),
            });
            return Err(CoreError::blf_open(format!(
                "无法打开 BLF 文件：{}（{err}）",
                path.display()
            )));
        }
    };

    let mut buffers: BTreeMap<TableKey, TableBuffer> = BTreeMap::new();
    let throttle = Duration::from_millis(50);
    let mut last_emit = Instant::now();
    let mut rows: u64 = 0;
    let mut decode_errors: u64 = 0;
    let mut emitted_first = false;

    for msg in reader.by_ref() {
        // 通道 0-based → 1-based，全模块唯一一次转换。
        let channel = msg.channel as u32 + 1;
        let frame_id = normalize_frame_id(&msg);

        // 全局限覆盖**全部输入文件全部帧**（含未命中 DBC 的帧），与口径定义一致。
        sink.observe_timestamp(msg.timestamp);

        // 即时过滤：未命中的报文零解码开销。
        let Some(message_plan) = plan.lookup(channel, frame_id) else {
            continue;
        };

        // 先把整帧解成临时 Vec，只有全部信号都成功才整帧追加，
        // 保证「行数一致、列不产生空值」。
        let mut decoded: Vec<f64> = Vec::with_capacity(message_plan.signals.len());
        let mut failed = false;
        for signal in &message_plan.signals {
            match signal.decode(msg.data.as_slice()) {
                Ok(value) => decoded.push(value),
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            decode_errors += 1;
            continue;
        }

        let key = TableKey {
            channel,
            message: message_plan.name.clone(),
        };
        let buffer = buffers
            .entry(key.clone())
            .or_insert_with(|| TableBuffer::new(message_plan));
        buffer.timestamps.push(msg.timestamp);
        for (idx, value) in decoded.into_iter().enumerate() {
            buffer.columns[idx].push(value);
        }
        rows += 1;

        let need_flush = buffer.timestamps.len() >= FLUSH_ROWS
            || buffer.estimated_bytes() >= mem_budget.max(1);
        if need_flush {
            let TableBuffer {
                signal_names,
                timestamps,
                columns,
            } = buffer;
            sink.flush_table(&key, path, signal_names, timestamps, columns)?;
        }

        // 首个解码帧立即上报一次（让界面"动起来"），之后按 50ms 节流。
        let due = !emitted_first || last_emit.elapsed() >= throttle;
        if due {
            emitted_first = true;
            last_emit = Instant::now();
            progress(Progress {
                phase: 1,
                stage: "decode".to_string(),
                done: 0,
                total: 1,
                file: Some(path_str.clone()),
                frames_done: rows,
                message: format!("解码中 {label}"),
            });
        }
    }

    // 文件结束：flush 所有剩余缓冲，确保本文件数据全部落盘后再处理下一个文件。
    for (key, buffer) in buffers.iter_mut() {
        let TableBuffer {
            signal_names,
            timestamps,
            columns,
        } = buffer;
        sink.flush_table(key, path, signal_names, timestamps, columns)?;
    }

    let error = reader
        .take_error()
        .map(|err| CoreError::blf_parse(format!("解析 BLF 时出错：{err}")).to_string());

    // 文件结束：done=1 ⇒ 本文件完成，`lib.rs` 借此把全局已完成文件数 +1。
    progress(Progress {
        phase: 1,
        stage: "decode".to_string(),
        done: 1,
        total: 1,
        file: Some(path_str.clone()),
        frames_done: rows,
        message: format!("已解码 {label}"),
    });

    Ok(FileStat {
        path: path_str,
        rows,
        error,
        decode_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use blf_asc::{BlfWriter, Message};
    use std::fs;

    /// Ch1 的 DBC：VehSpd(0x123) + BattTemp(0x1A2)。
    const DBC_CH1: &str = r#"VERSION ""

BO_ 291 VehSpd: 8 Vector__XXX
 SG_ Spd : 0|16@1+ (0.01,0) [0|0] "km/h" Vector__XXX
 SG_ SpdValid : 16|1@1+ (1,0) [0|0] "" Vector__XXX

BO_ 418 BattTemp: 8 Vector__XXX
 SG_ Temp1 : 7|8@0- (1,0) [0|0] "degC" Vector__XXX
 SG_ Temp2 : 4|12@0+ (1,0) [0|0] "" Vector__XXX
"#;

    /// Ch2 的 DBC：Insulation(0x200)。
    const DBC_CH2: &str = r#"VERSION ""

BO_ 512 Insulation: 8 Vector__XXX
 SG_ Res : 0|32@1+ (1,0) [0|0] "Ohm" Vector__XXX
"#;

    fn write_dbc(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, text).expect("写入 DBC 测试文件");
        path
    }

    /// 只给 Ch1 绑定 DBC（用于验证未绑 DBC 的 Ch2 也会被 scan 统计）。
    fn ch1_map(dir: &Path) -> BTreeMap<u32, PathBuf> {
        let mut map = BTreeMap::new();
        map.insert(1, write_dbc(dir, "ch1.dbc", DBC_CH1));
        map
    }

    /// Ch1 + Ch2 都绑定 DBC（用于验证 3 个报文都能 O(1) 命中）。
    fn both_map(dir: &Path) -> BTreeMap<u32, PathBuf> {
        let mut map = ch1_map(dir);
        map.insert(2, write_dbc(dir, "ch2.dbc", DBC_CH2));
        map
    }

    fn push_frame(
        writer: &mut BlfWriter,
        channel0: u16,
        id: u32,
        timestamp: f64,
        data: [u8; 8],
    ) {
        let mut msg = Message::default();
        msg.timestamp = timestamp;
        msg.arbitration_id = id.into();
        msg.is_extended_id = false;
        msg.is_fd = false;
        msg.channel = channel0;
        msg.dlc = 8;
        msg.data = data.into();
        writer.on_message_received(&msg).expect("写入合成帧");
    }

    /// 生成 5 秒合成 BLF：Ch1 VehSpd@10ms×500、Ch1 BattTemp@100ms×50、Ch2 Insulation@1s×5。
    fn make_blf(path: &Path) {
        let mut writer = BlfWriter::create(path).expect("创建合成 BLF");
        for i in 0..500 {
            let mut data = [0u8; 8];
            data[0] = (i & 0xFF) as u8;
            data[1] = ((i >> 8) & 0xFF) as u8;
            push_frame(&mut writer, 0, 0x123, i as f64 * 0.01, data);
        }
        for i in 0..50 {
            push_frame(&mut writer, 0, 0x1A2, i as f64 * 0.1, [0u8; 8]);
        }
        for i in 0..5 {
            push_frame(&mut writer, 1, 0x200, i as f64 * 1.0, [0u8; 8]);
        }
        writer.finish().expect("完成合成 BLF");
    }

    #[test]
    fn case01_plan_builds_three_messages_and_lookup_is_isolated() {
        let dir = tempfile::tempdir().expect("临时目录");
        let map = both_map(dir.path());
        let plan = DecodePlan::build(&map, &Selection::default()).expect("构建解码计划");

        assert_eq!(plan.messages().count(), 3, "Ch1 两个 + Ch2 一个");
        assert!(plan.lookup(1, 0x123).is_some());
        assert!(plan.lookup(1, 0x1A2).is_some());
        assert!(plan.lookup(2, 0x200).is_some());

        // 标准帧 0x123 绝不能命中扩展帧 key。
        assert!(plan.lookup(1, 0x123 | (1 << 31)).is_none());
        // 同一 frame_id 在不同通道必须各自独立命中。
        assert!(plan.lookup(2, 0x123).is_none());
    }

    #[test]
    fn case02_scan_frame_counts_use_1based_channel() {
        let dir = tempfile::tempdir().expect("临时目录");
        let map = both_map(dir.path());
        let plan = DecodePlan::build(&map, &Selection::default()).expect("构建解码计划");
        let blf = dir.path().join("synthetic.blf");
        make_blf(&blf);

        let scanned = scan_file(&blf, &plan, &|_p| {});
        assert!(scanned.ok, "扫描应成功：{:?}", scanned.error);
        assert_eq!(scanned.frame_counts.get(&(1, 0x123)).copied(), Some(500));
        assert_eq!(scanned.frame_counts.get(&(1, 0x1A2)).copied(), Some(50));
        assert_eq!(scanned.frame_counts.get(&(2, 0x200)).copied(), Some(5));
        // 防通道号 off-by-one：0-based 还原成 1-based 后必须非 0。
        assert_ne!(
            scanned.frame_counts.get(&(1, 0x123)).copied().unwrap_or(0),
            0
        );
    }

    #[test]
    fn case03_cycle_estimate_is_median_interval() {
        let dir = tempfile::tempdir().expect("临时目录");
        let map = both_map(dir.path());
        let plan = DecodePlan::build(&map, &Selection::default()).expect("构建解码计划");
        let blf = dir.path().join("synthetic.blf");
        make_blf(&blf);

        let scanned = scan_file(&blf, &plan, &|_p| {});
        let vehspd = scanned.cycle_estimate.get(&(1, 0x123)).copied().expect("VehSpd 周期");
        let batt = scanned.cycle_estimate.get(&(1, 0x1A2)).copied().expect("BattTemp 周期");
        let ins = scanned.cycle_estimate.get(&(2, 0x200)).copied().expect("Insulation 周期");
        assert!((vehspd - 10.0).abs() < 1e-6, "VehSpd cycle={vehspd}");
        assert!((batt - 100.0).abs() < 1e-6, "BattTemp cycle={batt}");
        assert!((ins - 1000.0).abs() < 1e-6, "Insulation cycle={ins}");
    }

    #[test]
    fn case04_motorola_signed_decodes_to_minus_ten() {
        let dir = tempfile::tempdir().expect("临时目录");
        let map = ch1_map(dir.path());
        let plan = DecodePlan::build(&map, &Selection::default()).expect("构建解码计划");
        let batt = plan.lookup(1, 0x1A2).expect("BattTemp 计划");
        let temp1 = batt.signals.iter().find(|s| s.name == "Temp1").expect("Temp1 信号");

        // start=7 → i0=0，整字节恒等；0xF6 的 8 位补码 = -10。
        let value = temp1.decode(&[0xF6, 0x00, 0, 0, 0, 0, 0, 0]).expect("解码 Temp1");
        assert!((value - (-10.0)).abs() < 1e-9, "Temp1={value}");
    }

    #[test]
    fn case05a_scan_counts_frames_even_without_dbc_binding() {
        let dir = tempfile::tempdir().expect("临时目录");
        let map = ch1_map(dir.path()); // 只给 Ch1 绑 DBC
        let plan = DecodePlan::build(&map, &Selection::default()).expect("构建解码计划");
        let blf = dir.path().join("synthetic.blf");
        make_blf(&blf);

        let scanned = scan_file(&blf, &plan, &|_p| {});
        // 全部出现的 (ch,id) 都要在，未绑 DBC 的 Ch2 也不例外，否则 GUI 树里看不到它。
        assert!(scanned.frame_counts.contains_key(&(1, 0x123)));
        assert!(scanned.frame_counts.contains_key(&(1, 0x1A2)));
        assert!(scanned.frame_counts.contains_key(&(2, 0x200)));
        assert_eq!(scanned.frame_counts.get(&(2, 0x200)).copied(), Some(5));
    }

    #[test]
    fn case05b_plan_only_contains_selected_messages() {
        let dir = tempfile::tempdir().expect("临时目录");
        let map = both_map(dir.path());

        let mut selection = Selection::default();
        let mut ch1 = HashMap::new();
        ch1.insert("VehSpd".to_string(), Vec::<String>::new());
        selection.channels.insert(1, ch1);

        let plan = DecodePlan::build(&map, &selection).expect("构建解码计划");
        assert_eq!(plan.messages().count(), 1, "只勾选 VehSpd 时计划里只能有 1 项");
        assert!(plan.lookup(1, 0x123).is_some());
        assert!(plan.lookup(1, 0x1A2).is_none());
        assert!(plan.lookup(2, 0x200).is_none());
    }

    #[test]
    fn case06_scan_progress_is_throttled() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().expect("临时目录");
        let map = ch1_map(dir.path());
        let plan = DecodePlan::build(&map, &Selection::default()).expect("构建解码计划");
        let blf = dir.path().join("synthetic.blf");
        make_blf(&blf);

        let calls = AtomicUsize::new(0);
        let scanned = scan_file(&blf, &plan, &|_p| {
            calls.fetch_add(1, Ordering::Relaxed);
        });
        assert!(scanned.ok);
        let n = calls.load(Ordering::Relaxed);
        // 555 帧若逐帧回调会有 555 次；节流后只应是极少数（至少收尾一次）。
        assert!((1..100).contains(&n), "回调次数应被节流，实际 {n}");
    }
}
