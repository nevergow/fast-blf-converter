//! DBC 预规范化（Step 9 / Step 10）：BOM / 编码 / can-dbc 不支持的段。
//!
//! 真实中国项目的 DBC 常见两类问题，都会让 `can-dbc 10.0.0` 直接解析失败：
//!
//! 1. **编码**：文件是 GBK/ANSI（中文注释、中文 `VAL_` 值表），严格 UTF-8 解码即失败；
//!    或文件带 UTF-8 BOM，导致第一行报 `expected file`。
//! 2. **不支持的元数据段**：`CM_ BA_`（属性注释）、相对属性定义 / 值的多行写法。这些段与
//!    信号解码无关，却是纯元数据。
//!
//! 本模块在**不改动 can-dbc 依赖**的前提下做一层**留痕的**预规范化：
//!
//! * 去 BOM、按需从 GB18030（GBK 超集）解码；
//! * 按**语句**（不是按行）整段剔除 can-dbc 不支持的段，剔除处保留换行以维持行号；
//! * 每一步都产出 `notes`，由调用方并入 `warnings`，**绝不静默改数据**。
//!
//! ## Step 12 修正：cp1252 vs GB18030 判别（孤立高位字节 ≠ 中文）
//!
//! Step 9 的「UTF-8 失败即 GB18030」在真机上会**解析成功但语义错误**：Vector 在非 UTF-8 语言
//! 环境下把 `°C` 写成单字节 `0xB0` + ASCII `C`，GB18030 会把 `0xB0` 当首字节、把 `C` 当尾字节
//! 吃掉（`"°C"` → `"癈"`），单位被破坏。Step 12 改为按**高位字节（≥0x80）的连续性**判别：
//!
//! * `max_run >= 2`（存在连续多字节序列）→ GB18030（中文注释/枚举值必然是成串多字节）；
//! * 所有高位字节都孤立（`max_run == 1`）→ Windows-1252（cp1252，真实文件里几乎总是
//!   `°` / `µ` / `±` / `²` 这类拉丁符号）。
//!
//! 两种回退都写**可区分**的 note（GUI 悬停显示），见 [`GB18030_NOTE`] / [`WINDOWS_1252_NOTE_PREFIX`]。
//!
//! ## Step 10 修正：误伤 `NS_ :` 段导致静默丢数据
//!
//! Step 9 把 `NS_ :` 段里逐行列出的元关键字也当成待剔除语句的起点；而剔除规则是「吃到下一个
//! `;` 为止」，`BO_`/`SG_` 行并不以 `;` 结尾，于是整段报文定义被吞掉。Step 10 用三重保护修复：
//!
//! 1. **列 0 规则**：只有行首无缩进（第 0 列）出现的关键字才可能是待剔除语句的起点；
//! 2. **整段跳过 `NS_ :`**：从 `NS_ :` 到下一个顶层段为止，段内不做任何剔除；
//! 3. **语句边界不越界**：剔除语句绝不跨过任何第 0 列开始的顶层定义行；找不到 `;` 时只剔起始行；
//! 4. **不变量自检**：剔除前后 `BO_`/`SG_` 行数必须一致，否则放弃剔除、返回原文并留痕。
//!
//! 剔除清单本身也改为**证据驱动**（见 `strip_list_matches_evidence_table` 与
//! `tests/e2e_synthetic.rs::dbcnorm_strip_list_matches_evidence`）：只有 `can_dbc::Dbc::try_from`
//! 在原始文本上被证明会失败的关键字才保留。
//!
//! 契约：任务书 Step 9 / Step 10；本模块不做任何 I/O，纯函数。

use std::collections::BTreeMap;

use encoding_rs::{GB18030, WINDOWS_1252};

/// 不变量自检失败时的留痕文案（剔除会破坏报文/信号定义时返回原文）。
///
/// 独立导出为常量，便于测试直接断言，避免「文案漂移但测试不同步」。
pub const NORM_SKIPPED_NOTE: &str = "规范化被跳过：剔除会破坏报文定义";

/// GB18030 回退解码的 note。中文编码的高位字节必然连续成串（`max_run >= 2`）。
const GB18030_NOTE: &str = "原文件不是 UTF-8，已按 GB18030 解码（检测到连续多字节序列，判定为中文编码）";

/// Windows-1252（cp1252）回退解码 note 的前缀；完整文案见 [`windows_1252_note`]。
const WINDOWS_1252_NOTE_PREFIX: &str = "原文件不是 UTF-8，已按 Windows-1252 解码（检测到";

/// 高位字节（≥0x80）的连续性统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct HighByteStats {
    /// 最长连续高位字节游程长度；全 ASCII 时为 0。
    max_run: usize,
    /// 长度为 1 的孤立高位字节游程数量（用于 cp1252 note 的 N）。
    isolated_runs: usize,
}

/// 扫描整段字节，统计连续高位字节的最长游程与孤立游程数。
///
/// 非 UTF-8 字节流才会用到本函数；纯 ASCII 返回 `max_run == 0`。
fn high_byte_stats(bytes: &[u8]) -> HighByteStats {
    let mut stats = HighByteStats::default();
    let mut run = 0usize;
    for &byte in bytes {
        if byte >= 0x80 {
            run += 1;
            continue;
        }
        if run > 0 {
            stats.max_run = stats.max_run.max(run);
            if run == 1 {
                stats.isolated_runs += 1;
            }
            run = 0;
        }
    }
    if run > 0 {
        stats.max_run = stats.max_run.max(run);
        if run == 1 {
            stats.isolated_runs += 1;
        }
    }
    stats
}

/// 非 UTF-8 字节流的回退编码选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncodingFallback {
    /// 只含孤立高位字节 → Windows-1252（cp1252）。
    Windows1252,
    /// 含 `>=2` 连续高位字节 → GB18030。
    Gb18030,
}

/// 按高位字节连续性判定回退编码；`None` 表示无需回退（纯 ASCII / 严格 UTF-8 已可解）。
///
/// 规则（Step 12）：`max_run == 0` → `None`；`max_run >= 2` → GB18030；否则（全孤立）→ cp1252。
fn choose_encoding_fallback(bytes: &[u8]) -> Option<EncodingFallback> {
    let stats = high_byte_stats(bytes);
    if stats.max_run == 0 {
        None
    } else if stats.max_run >= 2 {
        Some(EncodingFallback::Gb18030)
    } else {
        Some(EncodingFallback::Windows1252)
    }
}

/// 生成 cp1252 回退 note（带孤立高位字节处数 N）。
fn windows_1252_note(isolated_runs: usize) -> String {
    format!("{WINDOWS_1252_NOTE_PREFIX} {isolated_runs} 处孤立高位字节，判定为 °C/µ 等符号）")
}

/// 需要整段剔除的语句起始关键字（**证据驱动**）。
///
/// 只有 `can_dbc::Dbc::try_from` 在**原始文本**上被证明会解析失败的关键字才在此列
/// （`CM_ BA_` 因空白容忍单独处理，见 [`removal_keyword`]）：
///
/// * `CM_ BA_ "attr" "comment";` —— `CM_` 后不接 `BU_`/`BO_`/`SG_`/`EV_`/数字，解析失败；
/// * `BA_DEF_REL_ …` / `BA_DEF_DEF_REL_ …` 的**多行**写法 —— can-dbc 的 pest 语法里
///   `WHITESPACE` 不含换行，属性定义必须整条同行，换行即失败（真实 Vector 文件常见换行）。
///
/// **不在**此列的（均已被证据测试证明可解析，或只作 `NS_` 段符号出现）：
/// `CAT_` / `CAT_DEF_` / `FILTER` / `SGTYPE_*` / `EV_DATA_` / `ENVVAR_DATA_` /
/// `SIGTYPE_VALTYPE_` / `SIG_TYPE_REF_` / `BA_REL_` / `BU_*_REL_` 等。
const UNSUPPORTED_KEYWORDS: &[&str] = &["BA_DEF_DEF_REL_", "BA_DEF_REL_"];

/// 剔除清单的完整关键字集合（含单独处理的 `CM_ BA_`）。
///
/// 与证据测试表里所有「期望剔除」项必须**逐字相等**，因此这里既给运行期用，也给单测用。
fn strip_keyword_set() -> Vec<&'static str> {
    let mut set = vec!["CM_ BA_"];
    set.extend_from_slice(UNSUPPORTED_KEYWORDS);
    set
}

/// `NS_ :` 段内的元关键字。
///
/// 真实文件里这些一律带前导空白（Tab/空格）；即便某些不规范文件把它们写在第 0 列，
/// 只要仍属于本表，就继续视为 `NS_ :` 段内容而不做剔除。
const NS_SECTION_KEYWORDS: &[&str] = &[
    "NS_DESC_",
    "CM_",
    "BA_DEF_",
    "BA_",
    "VAL_",
    "CAT_DEF_",
    "CAT_",
    "FILTER",
    "BA_DEF_DEF_",
    "EV_DATA_",
    "ENVVAR_DATA_",
    "SGTYPE_",
    "SGTYPE_VAL_",
    "BA_DEF_SGTYPE_",
    "BA_SGTYPE_",
    "SIG_TYPE_REF_",
    "VAL_TABLE_",
    "SIG_GROUP_",
    "SIG_VALTYPE_",
    "SIGTYPE_VALTYPE_",
    "BO_TX_BU_",
    "BA_DEF_REL_",
    "BA_REL_",
    "BA_DEF_DEF_REL_",
    "BU_SG_REL_",
    "BU_EV_REL_",
    "BU_BO_REL_",
    "SG_MUL_VAL_",
];

/// 规范化结果。
///
/// * `text`：规范化后的 DBC 文本（可直接交给 `can_dbc::Dbc::try_from`）；
/// * `notes`：需要并入 `warnings` 的人话说明（可能为空）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NormOutcome {
    /// 规范化后的 DBC 文本。
    pub text: String,
    /// 规范化过程中产生的说明（去 BOM / 编码转换 / 忽略语句的汇总）。
    pub notes: Vec<String>,
}

/// 判断一行是否为需剔除语句的起始行；返回命中的关键字。
///
/// 调用方必须先满足**列 0 规则**（行首无缩进）才调用本函数。
///
/// `CM_ BA_` 单独处理：`CM_` 与 `BA_` 之间允许任意空白（真实文件里可能是多个空格/制表符），
/// 但 `CM_ BO_` / `CM_ SG_` / `CM_ BU_` / `CM_ EV_` / 普通 `CM_ "..."` 都**必须保留**。
fn removal_keyword(line: &str) -> Option<&'static str> {
    if let Some(rest) = line.strip_prefix("CM_") {
        if rest.trim_start().starts_with("BA_") {
            return Some("CM_ BA_");
        }
    }
    UNSUPPORTED_KEYWORDS
        .iter()
        .find(|keyword| line.starts_with(**keyword))
        .copied()
}

/// 把一行拆成「内容 + 行尾（含 `\n` / `\r\n` / 空）」。
fn split_line_ending(segment: &str) -> (&str, &str) {
    let content = segment.strip_suffix('\n').unwrap_or(segment);
    let content = content.strip_suffix('\r').unwrap_or(content);
    (&segment[..content.len()], &segment[content.len()..])
}

/// 行首是否有缩进（空格 / 制表符）。
fn is_indented(content: &str) -> bool {
    content.starts_with(' ') || content.starts_with('\t')
}

/// 行是否为空（去除首尾空白后为空）。
fn is_blank(content: &str) -> bool {
    content.trim().is_empty()
}

/// 统计「顶层定义行」数量：`trim_start` 后以 `prefix` 开头，且紧跟一个空白字符。
///
/// 这样 `BO_` 不会把 `BO_TX_BU_` 算进去、`SG_` 不会把 `SG_MUL_VAL_` 算进去。
fn count_definition_lines(text: &str, prefix: &str) -> usize {
    text.split_inclusive('\n')
        .filter(|segment| {
            let (content, _) = split_line_ending(segment);
            match content.trim_start().strip_prefix(prefix) {
                Some(rest) => rest.starts_with(' ') || rest.starts_with('\t'),
                None => false,
            }
        })
        .count()
}

/// 不变量：剔除前后 `BO_` / `SG_` 行数必须一致。
fn definitions_preserved(original: &str, stripped: &str) -> bool {
    count_definition_lines(original, "BO_") == count_definition_lines(stripped, "BO_")
        && count_definition_lines(original, "SG_") == count_definition_lines(stripped, "SG_")
}

/// 剔除过程的可观测产物。
struct StripResult {
    text: String,
    counts: BTreeMap<&'static str, usize>,
    total: usize,
    /// 保守剔除（找不到 `;`）等额外说明。
    notes: Vec<String>,
}

/// 按语句剔除 can-dbc 不支持的段。
///
/// 规则（Step 10）：
/// 1. **整段跳过 `NS_ :`**：从 `NS_ :` 行到下一个顶层段（`BS_:` / `BU_:` / `BO_` / `VERSION`
///    或首个第 0 列非 NS 关键字）为止，段内一行都不剔除；
/// 2. **列 0 规则**：只有行首无缩进的关键字才可能是剔除起点；
/// 3. **边界不越界**：一条待剔除语句从起点吃到本行或后续行的 `;`，但绝不跨过任何第 0 列
///    开始的顶层定义行；在遇到下一个顶层定义前都没找到 `;` 时，**只剔除起始行**并记 note；
/// 4. 剔除掉的行只保留换行，维持整体行号。
fn strip_unsupported_statements(text: &str) -> StripResult {
    let segments: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out = String::with_capacity(text.len());
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut total = 0usize;
    let mut notes: Vec<String> = Vec::new();
    let mut in_ns = false;
    let mut index = 0usize;

    while index < segments.len() {
        let (content, newline) = split_line_ending(segments[index]);
        let indented = is_indented(content);
        let blank = is_blank(content);

        // 1) `NS_ :` 段整段跳过：段内任何内容都不做剔除。
        if in_ns {
            if indented || blank || NS_SECTION_KEYWORDS.iter().any(|kw| content.starts_with(kw)) {
                out.push_str(segments[index]);
                index += 1;
                continue;
            }
            in_ns = false; // 顶层段开始，落到正常处理
        }
        if !indented && !blank && content.starts_with("NS_") && content.contains(':') {
            in_ns = true;
            out.push_str(segments[index]);
            index += 1;
            continue;
        }

        // 2) 列 0 规则：只有无缩进的关键字才可能是待剔除语句的起点。
        let keyword = if indented { None } else { removal_keyword(content) };
        let Some(keyword) = keyword else {
            out.push_str(segments[index]);
            index += 1;
            continue;
        };

        *counts.entry(keyword).or_insert(0) += 1;
        total += 1;

        // 单行语句（本行即以 `;` 结尾）。
        if content.trim_end().ends_with(';') {
            out.push_str(newline);
            index += 1;
            continue;
        }

        // 3) 语句边界：向后找 `;`，但绝不跨过任何第 0 列开始的顶层定义行。
        let mut end = None;
        let mut probe = index + 1;
        while probe < segments.len() {
            let (probe_content, _) = split_line_ending(segments[probe]);
            if !is_indented(probe_content) && !is_blank(probe_content) {
                break; // 顶层定义行：边界，绝不跨过
            }
            if probe_content.trim_end().ends_with(';') {
                end = Some(probe);
                break;
            }
            probe += 1;
        }

        match end {
            Some(end) => {
                // 整段剔除，仅保留换行以维持行号。
                for segment in &segments[index..=end] {
                    let (_, newline) = split_line_ending(segment);
                    out.push_str(newline);
                }
                index = end + 1;
            }
            None => {
                // 保守：未在下一个顶层定义前找到 `;`，只剔除起始行。
                out.push_str(newline);
                notes.push(format!(
                    "{keyword} 未在下一个顶层定义前找到 `;`，仅剔除该行（保守处理）"
                ));
                index += 1;
            }
        }
    }

    StripResult {
        text: out,
        counts,
        total,
        notes,
    }
}

/// 把命中计数格式化成汇总说明；`total == 0` 返回 `None`。
fn ignored_statements_note(counts: &BTreeMap<&'static str, usize>, total: usize) -> Option<String> {
    if total == 0 {
        return None;
    }
    let parts: Vec<String> = strip_keyword_set()
        .into_iter()
        .filter_map(|keyword| counts.get(keyword).map(|count| format!("{keyword} ×{count}")))
        .collect();
    Some(format!(
        "已忽略 {total} 条 can-dbc 不支持的语句（{}）",
        parts.join("、")
    ))
}

/// 对 DBC 原始字节做预规范化（纯函数，不做 I/O）。
///
/// 处理顺序：
/// 1. **去 UTF-8 BOM**（`EF BB BF`）；命中记 note `已去除 UTF-8 BOM`。
/// 2. **编码**：先按 UTF-8 严格解码；失败则按高位字节连续性判别回退编码（Step 12）：
///    * `max_run >= 2` → GB18030，记 note [`GB18030_NOTE`]；
///    * 全部孤立（`max_run == 1`）→ Windows-1252（cp1252），记 note [`windows_1252_note`]；
///    * 所选编码仍无法无损解码 → `Err(原因)`（调用方包成 `DBC_PARSE`）。
/// 3. **按语句剔除** can-dbc 不支持的元数据段；剔除后做不变量自检，一旦 `BO_`/`SG_`
///    行数减少就**放弃剔除、返回原文**并记 note [`NORM_SKIPPED_NOTE`]。
///
/// `file_label` 用于错误信息（如文件名），便于用户定位。返回的 `notes` 由调用方
/// 加上 `DBC「<文件名>」：` 前缀后并入 `warnings`。
pub fn normalize_dbc_bytes(raw: &[u8], file_label: &str) -> Result<NormOutcome, String> {
    let mut notes: Vec<String> = Vec::new();

    // 1) 去 UTF-8 BOM（只认标准三字节 BOM）。
    let bytes = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        notes.push("已去除 UTF-8 BOM".to_string());
        &raw[3..]
    } else {
        raw
    };

    // 2) 编码：严格 UTF-8 优先；失败按高位字节连续性判别 cp1252 / GB18030（不做 BOM 嗅探，
    //    避免误判为 UTF-16）。
    let text: String = match std::str::from_utf8(bytes) {
        Ok(valid) => valid.to_string(),
        Err(_) => {
            let stats = high_byte_stats(bytes);
            // 严格 UTF-8 失败意味着存在高位字节；`None` 在此仅为防御性回退。
            match choose_encoding_fallback(bytes) {
                Some(EncodingFallback::Gb18030) => {
                    match GB18030.decode_without_bom_handling_and_without_replacement(bytes) {
                        Some(decoded) => {
                            notes.push(GB18030_NOTE.to_string());
                            decoded.into_owned()
                        }
                        None => {
                            return Err(format!(
                                "DBC「{file_label}」无法按 UTF-8 或 GB18030 解码（存在无法映射的字节序列）"
                            ));
                        }
                    }
                }
                Some(EncodingFallback::Windows1252) | None => {
                    match WINDOWS_1252.decode_without_bom_handling_and_without_replacement(bytes) {
                        Some(decoded) => {
                            notes.push(windows_1252_note(stats.isolated_runs));
                            decoded.into_owned()
                        }
                        None => {
                            return Err(format!(
                                "DBC「{file_label}」无法按 UTF-8 或 Windows-1252 解码（存在无法映射的字节序列）"
                            ));
                        }
                    }
                }
            }
        }
    };

    // 3) 按语句剔除 can-dbc 不支持的段，并做「报文/信号定义不得减少」不变量自检。
    let StripResult {
        text: stripped,
        counts,
        total,
        notes: strip_notes,
    } = strip_unsupported_statements(&text);

    let text = if definitions_preserved(&text, &stripped) {
        if let Some(note) = ignored_statements_note(&counts, total) {
            notes.push(note);
        }
        notes.extend(strip_notes);
        stripped
    } else {
        notes.push(NORM_SKIPPED_NOTE.to_string());
        text
    };

    Ok(NormOutcome { text, notes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_utf8_bom_and_notes_it() {
        let mut raw = vec![0xEF, 0xBB, 0xBF];
        raw.extend_from_slice(b"VERSION \"\"\n");
        let outcome = normalize_dbc_bytes(&raw, "bom.dbc").expect("BOM 文件应可规范化");
        assert!(outcome.text.starts_with("VERSION"));
        assert_eq!(outcome.notes, vec!["已去除 UTF-8 BOM".to_string()]);
    }

    #[test]
    fn invalid_utf8_and_invalid_gb18030_is_error() {
        // 0xFF 在 GB18030 里不是合法字节（首字节范围 0x81..=0xFE）。
        let err = normalize_dbc_bytes(&[0xFF, 0xFF, 0xFF], "bad.dbc").expect_err("必须报错");
        assert!(err.contains("bad.dbc"), "错误应带文件名：{err}");
    }

    /// Step 12：按高位字节连续性判别编码的边界用例。
    #[test]
    fn dbcnorm_run_detection_boundary() {
        // 全 ASCII：没有高位字节 → 不需要任何回退。
        let ascii = b"VERSION \"\"\nBU_: ECU1\n";
        assert_eq!(choose_encoding_fallback(ascii), None, "纯 ASCII 不应回退");
        assert_eq!(high_byte_stats(ascii).max_run, 0);
        assert_eq!(high_byte_stats(ascii).isolated_runs, 0);

        // 单个孤立高位字节（0xB0 夹在 ASCII 之间）→ cp1252。
        let isolated = [b'"', 0xB0, b'C', b'"'];
        assert_eq!(
            choose_encoding_fallback(&isolated),
            Some(EncodingFallback::Windows1252)
        );
        let stats = high_byte_stats(&isolated);
        assert_eq!(stats.max_run, 1);
        assert_eq!(stats.isolated_runs, 1);

        // 两个连续高位字节（GBK 中文首尾字节）→ GB18030。
        let cjk = [0xB3, 0xB5];
        assert_eq!(
            choose_encoding_fallback(&cjk),
            Some(EncodingFallback::Gb18030)
        );
        assert_eq!(high_byte_stats(&cjk).max_run, 2);

        // 孤立与连续混在：max_run >= 2 优先 → GB18030。
        let mixed = [0xB0, b'C', 0xB3, 0xB5];
        assert_eq!(
            choose_encoding_fallback(&mixed),
            Some(EncodingFallback::Gb18030)
        );
        let stats = high_byte_stats(&mixed);
        assert_eq!(stats.max_run, 2);
        assert_eq!(stats.isolated_runs, 1);
    }

    #[test]
    fn multiline_statement_removal_preserves_line_count() {
        let text = "VERSION \"\"\nBA_DEF_REL_ BU_SG_REL_ \"a\"\n STRING ;\nBO_ 1 M: 8 X\n SG_ S : 0|8@1+ (1,0) [0|0] \"\" X\n";
        let outcome = normalize_dbc_bytes(text.as_bytes(), "multi.dbc").expect("应可规范化");
        // 输入 6 行（含末尾换行），输出行数必须一致，且 BO_/SG_ 原样保留。
        assert_eq!(
            outcome.text.split_inclusive('\n').count(),
            text.split_inclusive('\n').count()
        );
        assert!(outcome.text.contains("BO_ 1 M: 8 X"));
        assert!(outcome.text.contains(" SG_ S : 0|8@1+ (1,0) [0|0] \"\" X"));
        assert!(!outcome.text.contains("BA_DEF_REL_"));
        let note = outcome.notes.join("\n");
        assert!(note.contains("BA_DEF_REL_ ×1"), "汇总 note：{note}");
    }

    #[test]
    fn cm_ba_is_removed_but_plain_cm_stays() {
        let text = "VERSION \"\"\nCM_ BA_ \"attr\" \"c\";\nCM_ BO_ 1 \"msg\";\nCM_ SG_ 1 S \"sig\";\n";
        let outcome = normalize_dbc_bytes(text.as_bytes(), "cm.dbc").expect("应可规范化");
        assert!(!outcome.text.contains("CM_ BA_"));
        assert!(outcome.text.contains("CM_ BO_ 1 \"msg\";"));
        assert!(outcome.text.contains("CM_ SG_ 1 S \"sig\";"));
    }

    #[test]
    fn supported_sections_are_not_removed() {
        for keyword in [
            "VAL_", "VAL_TABLE_", "SG_MUL_VAL_", "SIG_VALTYPE_", "SIG_GROUP_", "BO_TX_BU_",
            "BA_DEF_", "BA_", "BA_DEF_DEF_", "EV_",
        ] {
            let text = format!("VERSION \"\"\n{keyword} payload;\n");
            let outcome = normalize_dbc_bytes(text.as_bytes(), "keep.dbc").expect("应可规范化");
            assert!(
                outcome.text.contains(keyword),
                "{keyword} 不应被剔除：{}",
                outcome.text
            );
        }
    }

    /// 剔除清单必须与证据测试表里所有「期望剔除」项逐字相等。
    #[test]
    fn strip_list_matches_evidence_table() {
        let mut actual: Vec<&str> = strip_keyword_set();
        actual.sort_unstable();
        let mut expected = vec!["BA_DEF_DEF_REL_", "BA_DEF_REL_", "CM_ BA_"];
        expected.sort_unstable();
        assert_eq!(actual, expected, "剔除清单必须与证据表一致");
    }

    /// `NS_ :` 段里逐行列出的关键字绝不能被当成剔除语句起点。
    #[test]
    fn ns_section_keywords_are_never_stripped() {
        let text = "VERSION \"\"\n\nNS_ :\n\tCAT_DEF_\n\tCAT_\n\tFILTER\n\tSGTYPE_\n\tSGTYPE_VAL_\n\tBA_DEF_SGTYPE_\n\tSIGTYPE_VALTYPE_\n\tSIG_TYPE_REF_\n\tEV_DATA_\n\tENVVAR_DATA_\n\tBA_REL_\n\tBU_SG_REL_\n\nBU_: ECU1\n\nBO_ 291 M: 8 ECU1\n SG_ S : 0|8@1+ (1,0) [0|0] \"\" ECU1\n";
        let outcome = normalize_dbc_bytes(text.as_bytes(), "ns.dbc").expect("应可规范化");
        for keyword in [
            "CAT_DEF_", "CAT_", "FILTER", "SGTYPE_", "SGTYPE_VAL_", "BA_DEF_SGTYPE_",
            "SIGTYPE_VALTYPE_", "SIG_TYPE_REF_", "EV_DATA_", "ENVVAR_DATA_", "BA_REL_",
            "BU_SG_REL_",
        ] {
            assert!(
                outcome.text.contains(keyword),
                "{keyword} 在 NS_ 段内不应被剔除：{}",
                outcome.text
            );
        }
        assert!(outcome.text.contains("BO_ 291 M: 8 ECU1"));
    }

    /// 列 0 规则：带缩进的关键字即便不在 `NS_ :` 段，也不会被剔除。
    #[test]
    fn indented_keywords_are_not_stripped() {
        let text = "VERSION \"\"\n\nBO_ 1 M: 8 X\n\tBA_DEF_REL_ BU_SG_REL_ \"a\" STRING ;\n SG_ S : 0|8@1+ (1,0) [0|0] \"\" X\n";
        let outcome = normalize_dbc_bytes(text.as_bytes(), "indent.dbc").expect("应可规范化");
        assert!(outcome.text.contains("BA_DEF_REL_"));
        assert!(!outcome.notes.iter().any(|note| note.contains("已忽略")));
    }

    /// 语句边界不越界：找不到 `;` 时只剔除起始行，后续 `BO_`/`SG_` 完好。
    #[test]
    fn conservative_strip_stops_at_top_level_definition() {
        let text = "VERSION \"\"\n\nBA_DEF_REL_ BU_SG_REL_ \"a\"\nBO_ 1 M: 8 X\n SG_ S : 0|8@1+ (1,0) [0|0] \"\" X\n";
        let outcome = normalize_dbc_bytes(text.as_bytes(), "bounds.dbc").expect("应可规范化");
        assert!(!outcome.text.contains("BA_DEF_REL_"));
        assert!(outcome.text.contains("BO_ 1 M: 8 X"));
        assert!(outcome.text.contains(" SG_ S : 0|8@1+ (1,0) [0|0] \"\" X"));
        let note = outcome.notes.join("\n");
        assert!(note.contains("保守处理"), "应记保守剔除 note：{note}");
    }

    /// 不变量自检：剔除会减少 `BO_` 行数时放弃剔除、返回原文并留痕。
    #[test]
    fn definition_invariant_blocks_stripping() {
        // 跨行语句里夹着一行（被误缩进的）`BO_` 定义：整段剔除会连带删掉 BO_ 定义。
        let text = "VERSION \"\"\nCM_ BA_ \"a\" \"b\"\n\tBO_ 1 M: 8 X\n\t;\n SG_ S : 0|8@1+ (1,0) [0|0] \"\" X\n";
        let outcome = normalize_dbc_bytes(text.as_bytes(), "invariant.dbc").expect("应可规范化");
        assert!(
            outcome.text.contains("BO_ 1 M: 8 X"),
            "定义必须保留：{}",
            outcome.text
        );
        assert!(
            outcome.notes.iter().any(|note| note == NORM_SKIPPED_NOTE),
            "应留痕放弃剔除：{:?}",
            outcome.notes
        );
        assert!(!outcome.notes.iter().any(|note| note.contains("已忽略")));
    }
}
