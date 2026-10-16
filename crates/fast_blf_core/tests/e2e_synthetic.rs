//! Step 2b 端到端集成测试：解码落分片 → 合并 → CSV / Parquet。
//!
//! 复用 `examples/make_fixtures.rs` 的合成素材（通过 `#[path]` 包含，保证两边内容永远一致）：
//! Ch1 `VehSpd` 0x123 @10ms ×500、Ch1 `BattTemp` 0x1A2 @100ms ×50、
//! Ch2 `Insulation` 0x200 @1000ms ×5，共 5 秒；其中最后一帧 Insulation 是 CAN FD。

#[path = "../examples/make_fixtures.rs"]
mod make_fixtures;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fast_blf_core::{
    export, process_file, DecodePlan, ExportFormat, ExportOptions, ExportReport, Progress,
    Selection, ShardSink,
};
use polars::prelude::*;

/// 一个隔离的临时测试现场。
struct Fixture {
    _dir: tempfile::TempDir,
    blf: PathBuf,
    /// Ch1 与 Ch2 都绑定同一份 demo.dbc（DBC 本身不区分通道，绑定关系在 config 里）。
    dbc_map: BTreeMap<u32, PathBuf>,
    out_root: PathBuf,
}

fn setup() -> Fixture {
    let dir = tempfile::tempdir().expect("创建临时目录");
    let fixtures = dir.path().join("fixtures");
    make_fixtures::write_fixtures(&fixtures).expect("生成合成夹具");

    let blf = fixtures.join("multi_channel.blf");
    let dbc = fixtures.join("demo.dbc");
    let mut dbc_map = BTreeMap::new();
    dbc_map.insert(1, dbc.clone());
    dbc_map.insert(2, dbc.clone());

    let out_root = dir.path().join("out");
    Fixture {
        _dir: dir,
        blf,
        dbc_map,
        out_root,
    }
}

/// 跑一遍「解码落分片 → 合并导出」的通用入口，返回 `(report, 解码命中帧数)`。
///
/// * `resample_hz` / `ffill` 透传给 `ExportOptions`；
/// * 结束后清理分片，模拟 `convert` 的行为。
#[allow(clippy::too_many_arguments)]
fn run_export_full(
    blf: &Path,
    dbc_map: &BTreeMap<u32, PathBuf>,
    root: &Path,
    out_name: &str,
    split: bool,
    format: ExportFormat,
    excel_row_limit: u64,
    resample_hz: Option<f64>,
    ffill: bool,
) -> (ExportReport, u64) {
    let plan = DecodePlan::build(dbc_map, &Selection::default()).expect("构建解码计划");
    let temp = root.join(format!("{out_name}_shards"));
    let sink = ShardSink::new(temp).expect("创建分片 sink");
    let noop = |_p: Progress| {};

    let stat = process_file(blf, &plan, &sink, 256 * 1024 * 1024, &noop).expect("解码文件");

    let tables = sink.tables_snapshot();
    let out_dir = root.join(out_name);
    let opts = ExportOptions {
        split_by_message: split,
        resample_hz,
        format,
        out_dir,
        excel_row_limit,
        ffill,
    };
    let report = export(&tables, &opts, &noop).expect("导出");

    // convert 结束时会只删自己写出的 parquet 分片。
    for shard in sink.shard_paths() {
        let _ = std::fs::remove_file(shard);
    }
    (report, stat.rows)
}

/// 常规夹具 + 保持原始时序的便捷封装（既有用例使用）。
fn run_export(
    fx: &Fixture,
    out_name: &str,
    split: bool,
    format: ExportFormat,
    excel_row_limit: u64,
) -> ExportReport {
    let (report, stat_rows) = run_export_full(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        out_name,
        split,
        format,
        excel_row_limit,
        None,
        false,
    );
    assert_eq!(stat_rows, 555, "三种周期共 555 帧应全部命中");
    report
}

/// 用 Polars 读回 CSV。
fn read_csv(path: &Path) -> DataFrame {
    CsvReadOptions::default()
        .with_has_header(true)
        .try_into_reader_with_file_path(Some(path.to_path_buf()))
        .expect("构造 CSV reader")
        .finish()
        .expect("读取 CSV")
}

/// 用 Polars 读回 Parquet。
fn read_parquet(path: &Path) -> DataFrame {
    let file = std::fs::File::open(path).expect("打开 Parquet");
    ParquetReader::new(file).finish().expect("读取 Parquet")
}

fn rows_of(report: &ExportReport, name: &str) -> u64 {
    report
        .tables
        .iter()
        .find(|table| table.name == name)
        .unwrap_or_else(|| panic!("缺少子表 {name}"))
        .rows
}

#[test]
fn case01_split_csv_produces_three_tables_with_expected_rows() {
    let fx = setup();
    let report = run_export(&fx, "csv_split", true, ExportFormat::Csv, 1_000_000);

    assert_eq!(report.format_used, ExportFormat::Csv);
    assert!(!report.downgraded);
    assert_eq!(report.tables.len(), 3);
    assert_eq!(rows_of(&report, "Ch1_VehSpd"), 500);
    assert_eq!(rows_of(&report, "Ch1_BattTemp"), 50);
    assert_eq!(rows_of(&report, "Ch2_Insulation"), 5);
    assert_eq!(report.total_rows, 555);

    // 文件名与命名规则：Merged_Ch{ch}_{Msg}.csv
    for table in &report.tables {
        let path = PathBuf::from(&table.path);
        assert!(path.is_file(), "输出文件不存在：{}", table.path);
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(
            file_name.starts_with("Merged_Ch") && file_name.ends_with(".csv"),
            "拆表文件名不符合规则：{file_name}"
        );
    }
}

#[test]
fn case02_split_csv_has_no_nulls_in_any_column() {
    let fx = setup();
    let report = run_export(&fx, "csv_nulls", true, ExportFormat::Csv, 1_000_000);

    // 核心卖点：拆表模式下每张表每一列都不能有空值。
    for table in &report.tables {
        let df = read_csv(Path::new(&table.path));
        assert_eq!(df.width(), df.columns().len());
        for column in df.columns() {
            assert_eq!(
                column.null_count(),
                0,
                "子表 {} 的列 {} 存在空值",
                table.name,
                column.name()
            );
        }
    }
}

#[test]
fn case03_split_parquet_has_same_rows() {
    let fx = setup();
    let report = run_export(&fx, "parquet_split", true, ExportFormat::Parquet, 1_000_000);

    assert_eq!(report.format_used, ExportFormat::Parquet);
    assert_eq!(rows_of(&report, "Ch1_VehSpd"), 500);
    assert_eq!(rows_of(&report, "Ch1_BattTemp"), 50);
    assert_eq!(rows_of(&report, "Ch2_Insulation"), 5);

    for table in &report.tables {
        let path = PathBuf::from(&table.path);
        assert!(path.is_file(), "Parquet 输出不存在：{}", table.path);
        let magic = std::fs::read(&path).expect("读文件头");
        assert!(magic.starts_with(b"PAR1"), "缺少 Parquet 魔数：{}", table.path);
        let df = read_parquet(&path);
        assert_eq!(df.height() as u64, table.rows, "Parquet 行数不符：{}", table.name);
    }
}

#[test]
fn case04_wide_csv_keeps_original_timeline_with_deduplicated_rows() {
    let fx = setup();
    let report = run_export(&fx, "wide_raw", false, ExportFormat::Csv, 1_000_000);

    assert_eq!(report.tables.len(), 1, "宽表模式只应有一个输出表");
    let table = &report.tables[0];
    assert_eq!(table.name, "Merged_All_Signals");
    assert_eq!(table.channel, None);
    assert_eq!(table.message, None);
    assert!(
        table.path.ends_with("Merged_All_Signals.csv"),
        "宽表文件名：{}",
        table.path
    );
    assert!(
        report.warnings.iter().any(|w| w.contains("未重采样")),
        "宽表应提示未重采样：{:?}",
        report.warnings
    );

    // 555 = 三种周期的帧数之和（500 + 50 + 5）。但宽表按 timestamp 做 outer join，
    // 同一时间戳只保留一行；BLF 内部把时间戳量化为纳秒（blf_asc 写入时 trunc 到 1ns，
    // 读取时再乘 1e-9），于是 `i*0.1` 与 `j*0.01` 在量化后大量重合：
    //   * 5 个 Insulation 时间戳全部落在 VehSpd 的 10ms 网格上；
    //   * 50 个 BattTemp 时间戳里，49 个与 VehSpd 网格逐位相等，只有 1 个落在网格外。
    // 因此去重后的真实并集 = 500 + 1 = 501。这里用「读取器实测并集」做断言，避免把
    // 平台相关的浮点边界写死，同时再锁一次 501 这个实测值。
    let expected_union = unique_timestamps_in_blf(&fx.blf);
    assert_eq!(
        expected_union, 501,
        "理论上限是 555；纳秒量化去重后应剩 501（500 VehSpd + 1 个未对齐 BattTemp）"
    );

    let df = read_csv(Path::new(&table.path));
    assert_eq!(df.height(), expected_union, "宽表行数应等于时间戳并集");
    assert_eq!(table.rows, expected_union as u64);

    // 列契约：timestamp 只有一列（outer join 已 coalesce），其余是各报文信号名。
    let names = df.get_column_names();
    assert_eq!(
        names.iter().filter(|name| name.as_str() == "timestamp").count(),
        1,
        "宽表只应有一个 timestamp 列：{names:?}"
    );
    for expected in ["Spd", "Temp1", "Res"] {
        assert!(
            names.iter().any(|name| name.as_str() == expected),
            "宽表缺少信号列 {expected}：{names:?}"
        );
    }
}

/// 读取 BLF 里所有时间戳，返回去重后的个数（与 exporter 的 outer join 语义一致）。
fn unique_timestamps_in_blf(blf: &Path) -> usize {
    let mut reader = blf_asc::BlfReader::open(blf).expect("打开 BLF");
    let mut stamps: Vec<f64> = Vec::new();
    for msg in reader.by_ref() {
        stamps.push(msg.timestamp);
    }
    stamps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    stamps.dedup_by(|a, b| *a == *b);
    stamps.len()
}

#[test]
fn case05_excel_guard_downgrades_to_csv_with_human_reason() {
    let fx = setup();
    // excel_row_limit 调到 10：任一表（500 行）都超限，必须整体降级为 CSV。
    let report = run_export(&fx, "guard", true, ExportFormat::Xlsx, 10);

    assert!(report.downgraded, "xlsx 应被降级");
    assert_eq!(report.format_used, ExportFormat::Csv);
    let reason = report.downgrade_reason.unwrap_or_default();
    assert!(reason.contains("百万行"), "降级理由应是人话：{reason}");

    assert!(!report.tables.is_empty());
    for table in &report.tables {
        let path = PathBuf::from(&table.path);
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("csv"));
        assert!(path.is_file(), "磁盘上应确实产出 CSV：{}", table.path);
    }
}

#[test]
fn case06_numeric_values_are_correct() {
    let fx = setup();
    let report = run_export(&fx, "csv_values", true, ExportFormat::Csv, 1_000_000);

    // VehSpd：注入 data[0..2] = i，Spd = raw × 0.01。时间戳升序后第 123 行 = 第 123 帧，
    // raw = 123 → 1.23。
    let vehspd_path = PathBuf::from(
        &report
            .tables
            .iter()
            .find(|table| table.name == "Ch1_VehSpd")
            .expect("VehSpd 子表")
            .path,
    );
    let vehspd = read_csv(&vehspd_path);
    let spd = vehspd.column("Spd").expect("Spd 列").f64().expect("Spd 是 f64");
    let spd_value = spd.get(123).expect("第 123 行 Spd");
    assert!(
        (spd_value - 0.01 * 123.0).abs() < 1e-9,
        "Spd 应等于 raw×0.01 = 1.23，实际 {spd_value}"
    );
    assert_eq!(vehspd.height(), 500);

    // BattTemp：data[0] = 0xF6，Motorola signed 8bit → -10.0。
    let batt_path = PathBuf::from(
        &report
            .tables
            .iter()
            .find(|table| table.name == "Ch1_BattTemp")
            .expect("BattTemp 子表")
            .path,
    );
    let batt = read_csv(&batt_path);
    // Temp1 每行都是 -10.0，CSV 最短表示会写成 `-10`，读回时 Polars 可能推断成 Int64；
    // 先显式 cast 成 Float64 再断言物理值。
    let temp1_col = batt
        .column("Temp1")
        .expect("Temp1 列")
        .cast(&DataType::Float64)
        .expect("Temp1 转 Float64");
    let temp1 = temp1_col.f64().expect("Temp1 是 f64");
    for idx in 0..batt.height() {
        let value = temp1.get(idx).expect("Temp1 值");
        assert!(
            (value - (-10.0)).abs() < 1e-9,
            "Temp1 第 {idx} 行应为 -10.0，实际 {value}"
        );
    }
}

// ---------------------------------------------------------------------------
// Step 2c：Excel 多 Sheet + 重采样
// ---------------------------------------------------------------------------

/// Case 1：xlsx 拆表 → 单个 `Merged_Report.xlsx`，3 个独立 Sheet，名字合法且互不相同。
#[test]
fn case07_xlsx_split_is_single_workbook_with_unique_sheets() {
    let fx = setup();
    let report = run_export(&fx, "xlsx_split", true, ExportFormat::Xlsx, 1_000_000);

    assert_eq!(report.format_used, ExportFormat::Xlsx);
    assert!(!report.downgraded, "未超行数上限不应降级");
    assert_eq!(report.tables.len(), 3);
    assert_eq!(report.total_rows, 555);

    // 全部表共享同一个 .xlsx 路径。
    let first_path = report.tables[0].path.clone();
    assert!(first_path.ends_with("Merged_Report.xlsx"), "路径：{first_path}");
    let xlsx_path = PathBuf::from(&first_path);
    assert!(xlsx_path.is_file(), "xlsx 不存在：{first_path}");
    for table in &report.tables {
        assert_eq!(table.path, first_path, "所有表必须指向同一个 xlsx");
    }

    // xlsx 是一个 zip 容器，魔数应为 "PK"。
    let magic = std::fs::read(&xlsx_path).expect("读取 xlsx");
    assert!(magic.starts_with(b"PK"), "xlsx 缺少 zip 魔数");

    // Sheet 名：存在、<=31 字符、互不相同。
    let mut sheets = std::collections::BTreeSet::new();
    for table in &report.tables {
        let sheet = table.sheet.as_ref().expect("xlsx 表必须有 sheet 名");
        assert!(
            sheet.chars().count() <= 31,
            "Sheet 名超过 31 字符：{sheet}"
        );
        assert!(
            sheet
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'),
            "Sheet 名含非法字符：{sheet}"
        );
        assert!(sheets.insert(sheet.clone()), "Sheet 名重复：{sheet}");
    }
    assert_eq!(sheets.len(), 3);
}

/// Case 2：xlsx 宽表 → 单个 `Merged_All_Signals.xlsx`、单 Sheet。
#[test]
fn case08_xlsx_wide_is_single_sheet() {
    let fx = setup();
    let report = run_export(&fx, "xlsx_wide", false, ExportFormat::Xlsx, 1_000_000);

    assert_eq!(report.format_used, ExportFormat::Xlsx);
    assert!(!report.downgraded);
    assert_eq!(report.tables.len(), 1, "宽表只应有一个输出表");
    let table = &report.tables[0];
    assert_eq!(table.name, "Merged_All_Signals");
    assert!(
        table.path.ends_with("Merged_All_Signals.xlsx"),
        "宽表路径：{}",
        table.path
    );
    assert!(PathBuf::from(&table.path).is_file());
    let sheet = table.sheet.as_ref().expect("宽表应有 sheet 名");
    assert!(sheet.chars().count() <= 31);
}

/// Case 3：拆表 + 10 Hz 重采样 → 每张表在自己的时间轴上等间隔取点，且零空值。
///
/// 实际用到的 t0 / t1（BLF 写入时按规范量化到纳秒，读取时 `start + ns*1e-9`）：
/// * `Ch1_VehSpd`   t0 = 0.0，t1 = 499×0.01 = 4.99 → floor(4.99×10)+1 = 50 点；
/// * `Ch1_BattTemp` t0 = 0.0，t1 = 49×0.1  = 4.9  → floor(4.9×10)+1  = 50 点；
/// * `Ch2_Insulation` t0 = 0.0，t1 = 4×1.0 = 4.0  → floor(4.0×10)+1  = 41 点。
///
/// 浮点边界允许 ±1 行（见 `resample_grid` 的容差说明）。
#[test]
fn case09_resample_split_row_counts_and_no_nulls() {
    let fx = setup();
    let (report, _) = run_export_full(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "resample_split",
        true,
        ExportFormat::Csv,
        1_000_000,
        Some(10.0),
        false,
    );

    let expected = [
        ("Ch1_VehSpd", 50u64),
        ("Ch1_BattTemp", 50u64),
        ("Ch2_Insulation", 41u64),
    ];
    for (name, exp) in expected {
        let actual = rows_of(&report, name);
        assert!(
            actual.abs_diff(exp) <= 1,
            "{name} 重采样行数应≈{exp}（±1），实际 {actual}"
        );
    }

    // 硬要求：所有列 null_count() == 0。
    for table in &report.tables {
        let df = read_csv(Path::new(&table.path));
        assert_eq!(df.height() as u64, table.rows);
        for column in df.columns() {
            assert_eq!(
                column.null_count(),
                0,
                "重采样后 {} 的列 {} 存在空值",
                table.name,
                column.name()
            );
        }
    }

    // 线性插值正确性（走真实导出路径）：Insulation 原始样本是 t=0..4 每秒一个
    // （Res = 0,1,2,3,4），10 Hz 网格上 t=0.5 必须插值为 0.5。
    let ins_path = PathBuf::from(
        &report
            .tables
            .iter()
            .find(|table| table.name == "Ch2_Insulation")
            .expect("Insulation 子表")
            .path,
    );
    let ins = read_csv(&ins_path);
    let ts = ins
        .column("timestamp")
        .expect("timestamp 列")
        .cast(&DataType::Float64)
        .expect("timestamp 转 Float64");
    let ts = ts.f64().expect("timestamp 是 f64");
    let res = ins
        .column("Res")
        .expect("Res 列")
        .cast(&DataType::Float64)
        .expect("Res 转 Float64");
    let res = res.f64().expect("Res 是 f64");
    let mut checked = false;
    for row in 0..ins.height() {
        if (ts.get(row).expect("timestamp 值") - 0.5).abs() < 1e-9 {
            let value = res.get(row).expect("Res 值");
            assert!(
                (value - 0.5).abs() < 1e-9,
                "Insulation t=0.5 处应线性插值为 0.5，实际 {value}"
            );
            checked = true;
        }
    }
    assert!(checked, "10 Hz 网格里应包含 t=0.5 这一点");
}

/// Case 4：宽表 + 10 Hz 重采样 → 全局并集网格（t0=0.0, t1=4.99 → 50 点），零空值。
#[test]
fn case10_resample_wide_single_table_no_nulls() {
    let fx = setup();
    let (report, _) = run_export_full(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "resample_wide",
        false,
        ExportFormat::Csv,
        1_000_000,
        Some(10.0),
        false,
    );

    assert_eq!(report.tables.len(), 1);
    let table = &report.tables[0];
    assert!(
        table.rows.abs_diff(50) <= 1,
        "宽表重采样应约 50 行（±1），实际 {}",
        table.rows
    );

    let df = read_csv(Path::new(&table.path));
    assert_eq!(df.height() as u64, table.rows);
    for column in df.columns() {
        assert_eq!(
            column.null_count(),
            0,
            "宽表重采样后列 {} 存在空值",
            column.name()
        );
    }
}

/// Case 5：保持原始时序 + ffill=true → 宽表内部空值被前向填充。
///
/// 用一条**只有 1 个 Insulation 样本**（t=0，Res=42）+ 11 个 VehSpd 样本（t=0..1.0）的
/// 合成 BLF：outer join 后 Res 在第 1..10 行为空，ffill 必须把它们填成 42。
#[test]
fn case11_wide_ffill_fills_internal_nulls() {
    let dir = tempfile::tempdir().expect("临时目录");
    let dbc = dir.path().join("demo.dbc");
    std::fs::write(&dbc, make_fixtures::DEMO_DBC).expect("写入 demo.dbc");
    let mut dbc_map = BTreeMap::new();
    dbc_map.insert(1, dbc.clone());
    dbc_map.insert(2, dbc.clone());

    let blf = dir.path().join("single_sample.blf");
    write_single_sample_blf(&blf);
    let root = dir.path().join("out");

    // 先验证不 ffill 时确实存在空值（否则本用例没有意义）。
    let (raw, _) = run_export_full(
        &blf,
        &dbc_map,
        &root,
        "wide_raw_for_ffill",
        false,
        ExportFormat::Csv,
        1_000_000,
        None,
        false,
    );
    let raw_df = read_csv(Path::new(&raw.tables[0].path));
    assert_eq!(raw_df.height(), 11, "11 个唯一时间戳");
    assert!(
        raw_df.column("Res").expect("Res 列").null_count() > 0,
        "不 ffill 时 Res 应有空值"
    );

    // ffill = true：内部空值必须被前向填充。
    let (filled, _) = run_export_full(
        &blf,
        &dbc_map,
        &root,
        "wide_ffill",
        false,
        ExportFormat::Csv,
        1_000_000,
        None,
        true,
    );
    let df = read_csv(Path::new(&filled.tables[0].path));
    assert_eq!(df.height(), 11);
    let res = df
        .column("Res")
        .expect("Res 列")
        .cast(&DataType::Float64)
        .expect("Res 转 Float64");
    let res = res.f64().expect("Res 是 f64");
    // 首行本身有样本，ffill 后整列都不应再有空值。
    assert_eq!(
        df.column("Res").expect("Res 列").null_count(),
        0,
        "ffill 后 Res 不应有空值"
    );
    for row in 1..df.height() {
        let value = res.get(row).expect("Res 非空");
        assert!(
            (value - 42.0).abs() < 1e-9,
            "第 {row} 行 Res 应被前向填充为 42，实际 {value}"
        );
    }
}

/// 写入一帧（测试辅助）。
fn push_msg(
    writer: &mut blf_asc::BlfWriter,
    channel0: u16,
    id: u32,
    timestamp: f64,
    data: Vec<u8>,
) {
    let msg = blf_asc::Message {
        timestamp,
        arbitration_id: id.into(),
        is_extended_id: false,
        is_fd: false,
        channel: channel0,
        dlc: data.len() as u8,
        data: data.into(),
        ..Default::default()
    };
    writer.on_message_received(&msg).expect("写入合成帧");
}

/// 合成只有 1 个 Insulation 样本（t=0，Res=42）+ 11 个 VehSpd 样本（t=0..1.0）的 BLF。
fn write_single_sample_blf(path: &Path) {
    let mut writer = blf_asc::BlfWriter::create(path).expect("创建合成 BLF");

    // Ch2 Insulation @ t=0：Res = 42（Intel 32bit，data[0..4] 小端）。
    let mut data = vec![0u8; 8];
    data[0..4].copy_from_slice(&42u32.to_le_bytes());
    push_msg(&mut writer, 1, 0x200, 0.0, data);

    // Ch1 VehSpd @ t=0..1.0 每 100ms：制造 Res 之后还有 10 个时间戳。
    for i in 0u32..=10 {
        let mut data = vec![0u8; 8];
        data[0] = (i & 0xFF) as u8;
        data[1] = ((i >> 8) & 0xFF) as u8;
        push_msg(&mut writer, 0, 0x123, i as f64 * 0.1, data);
    }

    writer.finish().expect("完成合成 BLF");
}
