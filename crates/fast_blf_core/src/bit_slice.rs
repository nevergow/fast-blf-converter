//! 跨字节位运算解码内核：Intel（小端，LSB0 连续）与 Motorola（大端，MSB0 连续）。
//!
//! 本文件是零依赖纯函数模块，不碰 BLF/DBC/IO，便于用单元测试锁死位序。
//! 位序约定已用 `cantools` 参考实现做过 1440 组随机对拍（见 `docs/DEPS.md` §7），
//! **任何"看起来更直观"的改法都会破坏兼容性，禁止擅自修正**。
//!
//! 契约：`docs/INTERFACE.md` §3。

use crate::error::CoreError;

/// 信号字节序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    /// Intel / Little Endian：`start_bit` 是信号 LSB 的 LSB0 位位置，位在 LSB0 空间连续。
    Intel,
    /// Motorola / Big Endian：DBC 的 `start_bit` 是"字节内从 LSB 数"的位置，
    /// 信号位（MSB→LSB）在 MSB0 索引空间连续排布。
    Motorola,
}

/// 从字节切片中按位序约定取出 `bit_len` 位原始值。
///
/// * `start_bit` —— Intel 下是信号 **LSB** 的 LSB0 位位置；Motorola 下是 DBC 语义的
///   "字节内从 LSB 数"的位置（内部先转成 MSB0 索引 `i0`）。
/// * `bit_len` —— 位宽，必须落在 `1..=64`。
///
/// 返回 `Err(BAD_CONFIG)` 的情形（绝不 panic、绝不越界索引）：
/// `bit_len == 0`、`bit_len > 64`、以及按各自判据的越界
/// （Intel: `start_bit + bit_len > data.len()*8`；Motorola: `i0 + bit_len > data.len()*8`）。
pub fn extract_bits(
    data: &[u8],
    start_bit: u32,
    bit_len: u32,
    order: ByteOrder,
) -> Result<u64, CoreError> {
    if bit_len == 0 {
        return Err(CoreError::bad_config("bit_len 必须 >= 1，收到 0"));
    }
    if bit_len > 64 {
        return Err(CoreError::bad_config(format!("bit_len 必须 <= 64，收到 {bit_len}")));
    }

    // 用 u64 运算，避免 u32 加法溢出；data.len()*8 也不会溢出（切片长度受 usize 限制）。
    let total_bits = data.len() as u64 * 8;

    match order {
        ByteOrder::Intel => {
            let end = start_bit as u64 + bit_len as u64;
            if end > total_bits {
                return Err(CoreError::bad_config(format!(
                    "Intel 信号越界：start_bit={start_bit} + bit_len={bit_len} = {end} > {}（数据 {} 字节）",
                    total_bits,
                    data.len()
                )));
            }

            let mut result = 0u64;
            for i in 0..bit_len {
                let pos = start_bit as u64 + i as u64;
                let byte = (pos / 8) as usize;
                let bit = (pos % 8) as u32;
                // 第 i 位权值为 2^i；i 最大 63，不会出现 1u64 << 64。
                result |= (((data[byte] >> bit) & 1) as u64) << i;
            }
            Ok(result)
        }
        ByteOrder::Motorola => {
            // DBC start 是"字节内从 LSB 数"的位置 → 转成 MSB0 索引 i0。
            let i0 = 8 * (start_bit as u64 / 8) + (7 - (start_bit as u64 % 8));
            // 越界判据必须用 i0（start_bit + bit_len 在 Motorola 下不是正确判据）。
            if i0 + bit_len as u64 > total_bits {
                return Err(CoreError::bad_config(format!(
                    "Motorola 信号越界：i0={i0} + bit_len={bit_len} = {} > {}（数据 {} 字节）",
                    i0 + bit_len as u64,
                    total_bits,
                    data.len()
                )));
            }

            let mut result = 0u64;
            for k in 0..bit_len {
                let p = i0 + k as u64;
                let byte = (p / 8) as usize;
                let lsb0_bit = 7 - (p % 8);
                // 先取出的位是 MSB：不断左移腾位再 OR 入新位。
                // bit_len == 64 时第 64 次移位前 result 至多 63 位，不会溢出。
                result = (result << 1) | (((data[byte] >> lsb0_bit) & 1) as u64);
            }
            Ok(result)
        }
    }
}

/// 把原始位值按二补码还原后换算成物理量：`value * factor + offset`。
///
/// * `is_signed == true` 且 `bit_len < 64`：若第 `bit_len-1` 位为 1，先符号扩展再转 `i64`。
/// * `bit_len >= 64`：直接 `raw as i64`（规避 `!0u64 << 64` 的移位溢出）。
/// * `is_signed == false`：直接 `raw as f64`（保留 u64 全量精度，不走 i64）。
pub fn decode_physical(raw: u64, bit_len: u32, is_signed: bool, factor: f64, offset: f64) -> f64 {
    let value = if is_signed {
        let signed = if bit_len == 0 {
            // 规范上 extract_bits 已拒绝 bit_len=0；此处兜底不 panic。
            0i64
        } else if bit_len >= 64 {
            raw as i64
        } else {
            let sign_bit = 1u64 << (bit_len - 1);
            if raw & sign_bit != 0 {
                // 先符号扩展，再算物理量。
                (raw | (!0u64 << bit_len)) as i64
            } else {
                raw as i64
            }
        };
        signed as f64
    } else {
        raw as f64
    };

    value * factor + offset
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "浮点不等：actual={actual}, expected={expected}"
        );
    }

    #[test]
    fn case01_intel_basic_single_byte() {
        // 防坑：最基本的小端整字节提取。
        let raw = extract_bits(&[0xFF, 0x00], 0, 8, ByteOrder::Intel).unwrap();
        assert_eq!(raw, 0xFF);
    }

    #[test]
    fn case02_intel_cross_byte() {
        // 防坑：小端跨字节。msg=0xCDAB（LE），(0xCDAB >> 4) & 0xFFF = 0xCDA。
        let raw = extract_bits(&[0xAB, 0xCD], 4, 12, ByteOrder::Intel).unwrap();
        assert_eq!(raw, 0xCDA);
    }

    #[test]
    fn case03_motorola_whole_byte_identity() {
        // 防坑：大端整字节恒等（start=7 → i0=0），不该发生任何位重排。
        let raw = extract_bits(&[0x12, 0x34], 7, 8, ByteOrder::Motorola).unwrap();
        assert_eq!(raw, 0x12);
    }

    #[test]
    fn case04_motorola_cross_byte() {
        // 防坑：大端跨字节。i0=3 → byte0 的 bit4..bit0 之后接 byte1 的 bit7..bit5。
        let raw = extract_bits(&[0x12, 0x34], 4, 8, ByteOrder::Motorola).unwrap();
        assert_eq!(raw, 0x91);
    }

    #[test]
    fn case05_motorola_cross_byte_signed() {
        // 防坑：大端跨字节 + 二补码还原同时正确（0x91A → -1766）。
        let raw = extract_bits(&[0x12, 0x34], 4, 12, ByteOrder::Motorola).unwrap();
        assert_eq!(raw, 0x91A);
        let value = decode_physical(raw, 12, true, 1.0, 0.0);
        assert_close(value, -1766.0);
    }

    #[test]
    fn case06_motorola_8bit_signed_twos_complement() {
        // 防坑：大端整字节 + 8 位补码（0xF6 → -10）。
        let raw = extract_bits(&[0xF6, 0x00], 7, 8, ByteOrder::Motorola).unwrap();
        assert_eq!(raw, 0xF6);
        let value = decode_physical(raw, 8, true, 1.0, 0.0);
        assert_close(value, -10.0);
    }

    #[test]
    fn case07_decode_12bit_all_ones_signed() {
        // 防坑：12 位全 1 的补码必须是 -1，符号扩展不能漏。
        let value = decode_physical(0xFFF, 12, true, 1.0, 0.0);
        assert_close(value, -1.0);
    }

    #[test]
    fn case08_motorola_start_zero_trap() {
        // 防坑（陷阱例）：naive 实现会给出 0x12 或 0x00；
        // 正确值 0x1A：i0=7，byte0 bit0 作 MSB，再接 byte1 的 bit7..bit1。
        // 这一条专门锁死"没有偷偷把 start 当 MSB 位置用"。
        let raw = extract_bits(&[0x12, 0x34], 0, 8, ByteOrder::Motorola).unwrap();
        assert_eq!(raw, 0x1A);
    }

    #[test]
    fn case09_intel_64bit_no_shift_overflow() {
        // 防坑：64 位边界不能在 `1u64 << 64` 处移位溢出。
        let raw = extract_bits(&[0xFF; 8], 0, 64, ByteOrder::Intel).unwrap();
        assert_eq!(raw, u64::MAX);
    }

    #[test]
    fn case10_intel_out_of_range_err() {
        // 防坑：小端越界返回 Err 而不是 panic/越界索引。
        // start=60 + len=8 = 68 > 64。
        let result = extract_bits(&[0u8; 8], 60, 8, ByteOrder::Intel);
        assert!(result.is_err(), "越界必须报错，实际得到 {result:?}");
    }

    #[test]
    fn case11_motorola_out_of_range_uses_i0() {
        // 防坑：Motorola 的越界判据必须按 i0。i0=7 → 7+8=15 > 8，必须 Err；
        // 若误用 start+len=8 <= 8 就会放行并越界索引。
        let result = extract_bits(&[0u8; 1], 0, 8, ByteOrder::Motorola);
        assert!(result.is_err(), "Motorola 越界必须报错，实际得到 {result:?}");
    }

    #[test]
    fn case12_bit_len_zero_is_error() {
        // 防坑：bit_len=0 非法，必须报错（否则符号扩展会下溢）。
        let result = extract_bits(&[0u8; 1], 0, 0, ByteOrder::Intel);
        assert!(result.is_err(), "bit_len=0 必须报错，实际得到 {result:?}");
    }

    #[test]
    fn case13_decode_physical_factor_offset_order() {
        // 防坑：factor/offset 的作用顺序必须是 value*factor+offset。
        let value = decode_physical(100, 16, false, 0.1, -40.0);
        assert_close(value, -30.0);
    }
}
