//! 稳定错误模型（`thiserror`）。
//!
//! 全部错误码集中在本文件定义（契约见 `docs/INTERFACE.md` §4），其它模块禁止散落魔法字符串。
//! `Display` 输出固定为 `"{code}: {message}"`，与 Python 侧 `str(e)` 的契约一致。

use thiserror::Error;

/// BLF 文件无法打开（不存在、无权限、文件头损坏等）。
pub const BLF_OPEN: &str = "BLF_OPEN";
/// BLF 对象流解析失败（对象损坏、不支持的压缩容器等）。
pub const BLF_PARSE: &str = "BLF_PARSE";
/// DBC 文本解析失败。
pub const DBC_PARSE: &str = "DBC_PARSE";
/// 某通道未绑定 DBC，却要求解码该通道的报文。
pub const NO_DBC_FOR_CHANNEL: &str = "NO_DBC_FOR_CHANNEL";
/// 全部输入被过滤或无有效信号，结果集为空。
pub const EMPTY_RESULT: &str = "EMPTY_RESULT";
/// 磁盘写满。
pub const DISK_FULL: &str = "DISK_FULL";
/// 导出阶段失败（CSV/Parquet/Excel 写出错误等）。
pub const EXPORT_FAILED: &str = "EXPORT_FAILED";
/// 调用参数非法（config 缺字段、位运算参数越界等）。
pub const BAD_CONFIG: &str = "BAD_CONFIG";

/// 统一错误模型：稳定错误码 + 人话说明 + 可选原始细节。
///
/// `Display` 实现固定输出 `"{code}: {message}"`；`detail` 不参与 `Display`，
/// 仅供日志/诊断使用。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{code}: {message}")]
pub struct CoreError {
    /// 稳定错误码，取值限定为本文件中的常量。
    pub code: String,
    /// 面向用户/日志的短句。
    pub message: String,
    /// 可选的原始错误串等补充信息。
    pub detail: Option<String>,
}

impl CoreError {
    /// 用错误码与说明构造，`detail = None`。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), detail: None }
    }

    /// 用错误码、说明与原始细节构造。
    pub fn with_detail(
        code: impl Into<String>,
        message: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self { code: code.into(), message: message.into(), detail: Some(detail.into()) }
    }

    /// 追加原始细节（消费式链式调用）。
    #[must_use]
    pub fn detail(self, detail: impl Into<String>) -> Self {
        Self { detail: Some(detail.into()), ..self }
    }

    /// `BLF_OPEN`
    pub fn blf_open(message: impl Into<String>) -> Self {
        Self::new(BLF_OPEN, message)
    }

    /// `BLF_PARSE`
    pub fn blf_parse(message: impl Into<String>) -> Self {
        Self::new(BLF_PARSE, message)
    }

    /// `DBC_PARSE`
    pub fn dbc_parse(message: impl Into<String>) -> Self {
        Self::new(DBC_PARSE, message)
    }

    /// `NO_DBC_FOR_CHANNEL`
    pub fn no_dbc_for_channel(message: impl Into<String>) -> Self {
        Self::new(NO_DBC_FOR_CHANNEL, message)
    }

    /// `EMPTY_RESULT`
    pub fn empty_result(message: impl Into<String>) -> Self {
        Self::new(EMPTY_RESULT, message)
    }

    /// `DISK_FULL`
    pub fn disk_full(message: impl Into<String>) -> Self {
        Self::new(DISK_FULL, message)
    }

    /// `EXPORT_FAILED`
    pub fn export_failed(message: impl Into<String>) -> Self {
        Self::new(EXPORT_FAILED, message)
    }

    /// `BAD_CONFIG`
    pub fn bad_config(message: impl Into<String>) -> Self {
        Self::new(BAD_CONFIG, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_code_colon_message() {
        let err = CoreError::with_detail(BAD_CONFIG, "bit_len 必须是 1..=64", "got 0");
        assert_eq!(err.to_string(), "BAD_CONFIG: bit_len 必须是 1..=64");
        assert_eq!(err.detail.as_deref(), Some("got 0"));
    }

    #[test]
    fn helper_constructors_use_stable_codes() {
        assert_eq!(CoreError::blf_open("x").code, BLF_OPEN);
        assert_eq!(CoreError::blf_parse("x").code, BLF_PARSE);
        assert_eq!(CoreError::dbc_parse("x").code, DBC_PARSE);
        assert_eq!(CoreError::no_dbc_for_channel("x").code, NO_DBC_FOR_CHANNEL);
        assert_eq!(CoreError::empty_result("x").code, EMPTY_RESULT);
        assert_eq!(CoreError::disk_full("x").code, DISK_FULL);
        assert_eq!(CoreError::export_failed("x").code, EXPORT_FAILED);
        assert_eq!(CoreError::bad_config("x").code, BAD_CONFIG);
    }
}
