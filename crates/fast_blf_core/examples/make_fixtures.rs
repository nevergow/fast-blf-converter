//! 生成 Python 层端到端验收用的真实夹具（BLF + DBC + README）。
//!
//! 运行方式（幂等，重复运行覆盖旧文件）：
//!
//! ```bash
//! cargo run -p fast_blf_core --example make_fixtures
//! ```
//!
//! 默认输出到工作区根的 `fixtures/`；也可传一个目录参数：
//!
//! ```bash
//! cargo run -p fast_blf_core --example make_fixtures -- /tmp/xyz
//! ```
//!
//! 本文件同时被 `tests/e2e_synthetic.rs` 以 `#[path]` 方式复用，保证「夹具」与
//! 「Rust 端到端测试素材」永远一致。

// 该文件既作为 example（有 main）编译，也被集成测试当模块包含；main 在测试里用不到。
#![allow(dead_code)]

use std::error::Error;
use std::path::Path;

use blf_asc::{BlfWriter, Message};

/// 三张表的信号定义与周期，供 README 与测试复用。
pub struct FixtureSpec;

impl FixtureSpec {
    /// 预期行数：`(子表名, 帧数)`。
    pub const EXPECTED_ROWS: [(&'static str, u64); 3] = [
        ("Ch1_VehSpd", 500),
        ("Ch1_BattTemp", 50),
        ("Ch2_Insulation", 5),
    ];
}

/// 与三条报文对应的 DBC 文本（`Temp1` Motorola signed start=7 len=8，
/// `Temp2` Motorola unsigned start=4 len=12）。
pub const DEMO_DBC: &str = r#"VERSION ""

BO_ 291 VehSpd: 8 Vector__XXX
 SG_ Spd : 0|16@1+ (0.01,0) [0|0] "km/h" Vector__XXX
 SG_ SpdValid : 16|1@1+ (1,0) [0|0] "" Vector__XXX

BO_ 418 BattTemp: 8 Vector__XXX
 SG_ Temp1 : 7|8@0- (1,0) [0|0] "degC" Vector__XXX
 SG_ Temp2 : 4|12@0+ (1,0) [0|0] "" Vector__XXX

BO_ 512 Insulation: 8 Vector__XXX
 SG_ Res : 0|32@1+ (1,0) [0|0] "Ohm" Vector__XXX
"#;

/// 夹具 README（通道/报文/周期/信号/预期行数）。
pub const README: &str = r#"# fixtures —— 合成 BLF / DBC 测试素材

由 `cargo run -p fast_blf_core --example make_fixtures` 生成，幂等（可重复运行覆盖）。

## 文件

| 文件 | 说明 |
|---|---|
| `multi_channel.blf` | 5 秒合成数据；Ch1 两报文 + Ch2 一波形，含 1 帧 CAN FD |
| `demo.dbc` | 三条报文的 DBC 定义 |
| `README.md` | 本文件 |

## 通道 / 报文 / 周期 / 信号

| 通道 | 报文 | ID | 周期 | 帧数 | 信号 |
|---|---|---|---|---|---|
| Ch1 | `VehSpd` | `0x123`（标准帧） | 10 ms | 500 | `Spd`（Intel, unsigned, start=0, len=16, factor=0.01, unit km/h）、`SpdValid`（Intel, unsigned, start=16, len=1） |
| Ch1 | `BattTemp` | `0x1A2`（标准帧） | 100 ms | 50 | `Temp1`（Motorola, signed, start=7, len=8）、`Temp2`（Motorola, unsigned, start=4, len=12） |
| Ch2 | `Insulation` | `0x200`（标准帧） | 1000 ms | 5 | `Res`（Intel, unsigned, start=0, len=32, factor=1.0, unit Ohm） |

- 时间戳 = `i × 周期`，写文件时按 BLF 规范量化到纳秒。
- `multi_channel.blf` 里 **最后 1 帧 `Insulation` 是 CAN FD**（`is_fd = true`，DLC 12），用于覆盖 FD 解析路径。
- 注入值：`VehSpd.Spd` 的第 `i` 帧原始值 = `i`（物理值 `i × 0.01`）；`BattTemp.Temp1` 恒为 `0xF6`（有符号 = `-10.0`）；`Insulation.Res` 第 `i` 帧 = `i`。

## 预期子表行数

| 子表 | 行数 |
|---|---|
| `Ch1_VehSpd` | 500 |
| `Ch1_BattTemp` | 50 |
| `Ch2_Insulation` | 5 |
| 合计 | 555 |
"#;

/// 把一帧写进 BLF。
fn push_frame(
    writer: &mut BlfWriter,
    channel0: u16,
    id: u32,
    timestamp: f64,
    data: Vec<u8>,
    is_fd: bool,
) -> Result<(), Box<dyn Error>> {
    let mut msg = Message::default();
    msg.timestamp = timestamp;
    msg.arbitration_id = id.into();
    msg.is_extended_id = false;
    msg.is_fd = is_fd;
    msg.channel = channel0;
    msg.dlc = data.len() as u8;
    msg.data = data.into();
    writer.on_message_received(&msg)?;
    Ok(())
}

/// 生成全部夹具到 `dir`（不存在则创建）。幂等：直接覆盖旧文件。
pub fn write_fixtures(dir: &Path) -> Result<(), Box<dyn Error>> {
    std::fs::create_dir_all(dir)?;

    // 1) DBC
    std::fs::write(dir.join("demo.dbc"), DEMO_DBC)?;

    // 2) BLF：严格按 Step 2a 的合成场景。
    let blf_path = dir.join("multi_channel.blf");
    let mut writer = BlfWriter::create(&blf_path)?;

    // Ch1 VehSpd 0x123 @10ms × 500；data[0..2] = i（Intel），Spd = i × 0.01。
    for i in 0u32..500 {
        let mut data = vec![0u8; 8];
        data[0] = (i & 0xFF) as u8;
        data[1] = ((i >> 8) & 0xFF) as u8;
        push_frame(&mut writer, 0, 0x123, i as f64 * 0.01, data, false)?;
    }

    // Ch1 BattTemp 0x1A2 @100ms × 50；data[0] = 0xF6（Motorola signed 8bit = -10）。
    for i in 0u32..50 {
        let data = vec![0xF6u8, 0, 0, 0, 0, 0, 0, 0];
        push_frame(&mut writer, 0, 0x1A2, i as f64 * 0.1, data, false)?;
    }

    // Ch2 Insulation 0x200 @1000ms × 5；data[0..4] = i（Intel 32bit），Res = i。
    // 最后一帧刻意用 CAN FD（12 字节）覆盖 FD 路径。
    for i in 0u32..5 {
        let fd = i == 4;
        let len = if fd { 12 } else { 8 };
        let mut data = vec![0u8; len];
        data[0..4].copy_from_slice(&i.to_le_bytes());
        push_frame(&mut writer, 1, 0x200, i as f64 * 1.0, data, fd)?;
    }

    writer.finish()?;

    // 3) README
    std::fs::write(dir.join("README.md"), README)?;
    Ok(())
}

/// 默认输出目录：工作区根下的 `fixtures/`（以 `CARGO_MANIFEST_DIR` 为锚，避免依赖 CWD）。
///
/// `CARGO_MANIFEST_DIR` = `<workspace>/crates/fast_blf_core`，上溯两级即工作区根。
pub fn default_output_dir() -> std::path::PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    match manifest.parent().and_then(Path::parent) {
        Some(workspace_root) => workspace_root.join("fixtures"),
        None => manifest.join("fixtures"),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let arg = std::env::args().nth(1);
    let dir = match arg {
        Some(path) => std::path::PathBuf::from(path),
        None => default_output_dir(),
    };
    write_fixtures(&dir)?;
    println!("夹具已生成：{}", dir.display());
    for (name, rows) in FixtureSpec::EXPECTED_ROWS {
        println!("  {name}: {rows} 行");
    }
    Ok(())
}
