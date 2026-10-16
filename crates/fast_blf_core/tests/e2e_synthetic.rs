//! Step 2b 端到端集成测试：解码落分片 → 合并 → CSV / Parquet。
//!
//! 复用 `examples/make_fixtures.rs` 的合成素材（通过 `#[path]` 包含，保证两边内容永远一致）：
//! Ch1 `VehSpd` 0x123 @10ms ×500、Ch1 `BattTemp` 0x1A2 @100ms ×50、
//! Ch2 `Insulation` 0x200 @1000ms ×5，共 5 秒；其中最后一帧 Insulation 是 CAN FD。

#[path = "../examples/make_fixtures.rs"]
mod make_fixtures;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use fast_blf_core::{
    catch_export_panic, export, process_file, read_file_timing, CoreError, DecodePlan,
    ExportFormat, ExportOptions, ExportPanicContext, ExportReport, Progress, ProgressThrottle,
    Selection, ShardSink, TimeFormat, EXPORT_INTERNAL,
};
use polars::prelude::*;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

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
        time_format: TimeFormat::RelativeS,
        global_min: sink.global_min(),
        file_timings: vec![read_file_timing(blf)],
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

// ---------------------------------------------------------------------------
// Step 5：DBC 容错 + 空 BLF 扫描（经真实 PyO3 入口 convert / scan 验证）
// ---------------------------------------------------------------------------

/// Python 解释器只需显式初始化一次（`Python::initialize` 幂等，这里仍用 Once 收敛）。
static PY_INIT: std::sync::Once = std::sync::Once::new();

/// 在 GIL 下执行闭包（首次调用时初始化 Python）。
///
/// `convert` / `scan` 是 PyO3 入口，这里直接调用它们并断言 Python 侧真实返回的 dict，
/// 因此覆盖的就是 GUI 会走的那条路径。`cargo test` 未启用 `extension-module`，测试二进制
/// 已链接 libpython（需 `DYLD_FALLBACK_LIBRARY_PATH`，见 docs/DEPS.md §0）。
fn with_python<R>(f: impl for<'py> FnOnce(Python<'py>) -> R) -> R {
    PY_INIT.call_once(Python::initialize);
    Python::attach(f)
}

/// Step 5 现场：Ch1 绑损坏 DBC，Ch2 绑正常 demo.dbc，BLF 用既有 multi_channel 夹具。
struct Step5Fixture {
    _dir: tempfile::TempDir,
    blf: PathBuf,
    dbc_map: BTreeMap<u32, PathBuf>,
    out_root: PathBuf,
}

fn setup_step5() -> Step5Fixture {
    let dir = tempfile::tempdir().expect("创建临时目录");
    let fixtures = dir.path().join("fixtures");
    make_fixtures::write_fixtures(&fixtures).expect("生成合成夹具");

    // Ch1 故意写一段乱码文本：can-dbc 必然解析失败。
    let bad_dbc = fixtures.join("broken_ch1.dbc");
    std::fs::write(&bad_dbc, "这不是 DBC 文本\nBO_ ??? 乱码 ###\n").expect("写入坏 DBC");

    let mut dbc_map = BTreeMap::new();
    dbc_map.insert(1, bad_dbc);
    dbc_map.insert(2, fixtures.join("demo.dbc"));

    let out_root = dir.path().join("out");
    Step5Fixture {
        _dir: dir,
        blf: fixtures.join("multi_channel.blf"),
        dbc_map,
        out_root,
    }
}

/// 组装一个最小可用的 `convert` config（字段与 INTERFACE §1.1 一致）。
fn step5_config<'py>(
    py: Python<'py>,
    fx: &Step5Fixture,
    selected: HashMap<String, HashMap<String, Vec<String>>>,
    out_name: &str,
) -> PyResult<Bound<'py, PyDict>> {
    let config = PyDict::new(py);
    config.set_item("blf_paths", vec![fx.blf.to_string_lossy().to_string()])?;
    let dbc_py: HashMap<u32, String> = fx
        .dbc_map
        .iter()
        .map(|(&channel, path)| (channel, path.to_string_lossy().to_string()))
        .collect();
    config.set_item("dbc_by_channel", dbc_py)?;
    config.set_item("selected", selected)?;
    config.set_item("split_by_message", true)?;
    config.set_item("resample_hz", py.None())?;
    config.set_item("export_format", "csv")?;
    config.set_item(
        "out_dir",
        fx.out_root.join(out_name).to_string_lossy().to_string(),
    )?;
    config.set_item(
        "temp_dir",
        fx.out_root
            .join(format!("{out_name}_shards"))
            .to_string_lossy()
            .to_string(),
    )?;
    config.set_item("excel_row_limit", 1_000_000i64)?;
    config.set_item("memory_budget_mb", 256i64)?;
    config.set_item("ffill", false)?;
    Ok(config)
}

/// Step 5 用例 1：未勾选通道的坏 DBC 绝不能拖垮整体转换。
///
/// Ch1 DBC 损坏、Ch2 DBC 正常，只勾选 Ch2 的 Insulation：
/// `convert` 必须成功、只产出 Ch2 子表、`dbc_errors` 点名通道 1、`warnings` 有提示。
#[test]
fn case12_broken_dbc_on_unselected_channel_does_not_fail_convert() {
    let fx = setup_step5();
    with_python(|py| -> PyResult<()> {
        let mut ch2 = HashMap::new();
        ch2.insert("Insulation".to_string(), vec!["Res".to_string()]);
        let mut selected = HashMap::new();
        selected.insert("2".to_string(), ch2);

        let config = step5_config(py, &fx, selected, "step5_ok")?;
        let report = fast_blf_core::convert(py, &config, None)
            .expect("坏 DBC 位于未勾选的 Ch1，convert 必须成功");

        let dbc_errors = report
            .get_item("dbc_errors")?
            .expect("返回值必须含 dbc_errors 字段")
            .cast_into::<PyDict>()?;
        assert!(dbc_errors.contains("1")?, "dbc_errors 应含通道 1");
        assert!(!dbc_errors.contains("2")?, "通道 2 的 DBC 正常，不应报错");

        let warnings = report
            .get_item("warnings")?
            .expect("返回值必须含 warnings 字段")
            .cast_into::<PyList>()?;
        let warning_texts: Vec<String> = warnings
            .iter()
            .map(|item| item.extract::<String>())
            .collect::<PyResult<Vec<String>>>()?;
        assert!(
            warning_texts
                .iter()
                .any(|text| text.contains("通道 1") && text.contains("DBC")),
            "warnings 应提示通道 1 的 DBC 失败：{warning_texts:?}"
        );

        // 子表行数应等于选中报文（Ch2 Insulation）的观测帧数 5。
        let tables = report
            .get_item("tables")?
            .expect("返回值必须含 tables 字段")
            .cast_into::<PyList>()?;
        assert_eq!(tables.len(), 1, "只勾选一个报文应只产出一张子表");
        let table = tables.get_item(0)?.cast_into::<PyDict>()?;
        assert_eq!(
            table.get_item("name")?.unwrap().extract::<String>()?,
            "Ch2_Insulation"
        );
        assert_eq!(table.get_item("rows")?.unwrap().extract::<u64>()?, 5);
        assert_eq!(report.get_item("total_rows")?.unwrap().extract::<u64>()?, 5);
        assert!(report.get_item("ok")?.unwrap().extract::<bool>()?);
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// Step 5 用例 2：所选内容全部落在失败通道时才报 `EMPTY_RESULT`，且消息点名通道与原因。
#[test]
fn case13_convert_returns_empty_result_when_all_selected_on_broken_channel() {
    let fx = setup_step5();
    with_python(|py| -> PyResult<()> {
        let mut ch1 = HashMap::new();
        ch1.insert("VehSpd".to_string(), vec!["Spd".to_string()]);
        let mut selected = HashMap::new();
        selected.insert("1".to_string(), ch1);

        let config = step5_config(py, &fx, selected, "step5_empty")?;
        let err = match fast_blf_core::convert(py, &config, None) {
            Ok(_) => panic!("所选信号全部落在坏 DBC 通道，convert 必须报错"),
            Err(err) => err,
        };
        let text = err.to_string();
        assert!(text.contains("EMPTY_RESULT"), "错误码应为 EMPTY_RESULT：{text}");
        assert!(text.contains('1'), "错误消息应点名通道 1：{text}");
        assert!(text.contains("DBC"), "错误消息应带失败原因关键字 DBC：{text}");
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// Step 5 用例 3：`scan([], {2: dbc})` 只按 DBC 列出全部报文与信号。
///
/// `frame_count=0`、`cycle_ms=None`、`is_fd=false`、`files=[]`；绝不因"观测不到帧"丢报文。
#[test]
fn case14_scan_with_empty_blf_paths_lists_all_dbc_messages() {
    let fx = setup_step5();
    with_python(|py| -> PyResult<()> {
        // 只用 Ch2 的正常 DBC，验证"空观测仍按 DBC 列出全部报文"。
        let dbc = fx.dbc_map.get(&2).cloned().expect("Ch2 DBC");
        let mut dbc_py = HashMap::new();
        dbc_py.insert(2u32, dbc.to_string_lossy().to_string());

        let tree = fast_blf_core::scan(py, Vec::<String>::new(), dbc_py, None)?;

        let files = tree
            .get_item("files")?
            .expect("scan 必须含 files")
            .cast_into::<PyList>()?;
        assert!(files.is_empty(), "blf_paths 为空时 files 必须是空列表");

        let channels = tree
            .get_item("channels")?
            .expect("scan 必须含 channels")
            .cast_into::<PyDict>()?;
        let ch2 = channels
            .get_item("2")?
            .expect("通道 2 必须出现")
            .cast_into::<PyDict>()?;
        let messages = ch2
            .get_item("messages")?
            .expect("通道 2 必须含 messages")
            .cast_into::<PyList>()?;
        assert!(
            !messages.is_empty(),
            "观测不到帧也必须按 DBC 列出全部报文，不能丢"
        );

        let mut names: Vec<String> = Vec::new();
        for message in messages.iter() {
            let message = message.cast_into::<PyDict>()?;
            names.push(message.get_item("name")?.unwrap().extract::<String>()?);
            assert_eq!(message.get_item("frame_count")?.unwrap().extract::<u64>()?, 0);
            assert!(
                message.get_item("cycle_ms")?.unwrap().is_none(),
                "无观测时 cycle_ms 必须为 None"
            );
            assert!(!message.get_item("is_fd")?.unwrap().extract::<bool>()?);
            let signals = message
                .get_item("signals")?
                .expect("报文必须含 signals")
                .cast_into::<PyList>()?;
            assert!(!signals.is_empty(), "报文必须带 DBC 信号");
        }
        assert!(
            names.contains(&"Insulation".to_string()),
            "应列出 DBC 里的全部报文：{names:?}"
        );
        Ok(())
    })
    .expect("Python 侧断言失败");
}

// ---------------------------------------------------------------------------
// Step 7a：进度回调下沉到热路径 + 50ms 节流
// ---------------------------------------------------------------------------

/// Step 7a 用例 1+2：`process_file` 在解码循环内部就持续回调，`frames_done` 如实反映进度。
///
/// 断言：
/// * 5 秒素材（500+50+5 帧）至少收到 3 次回调（文件开始 / 首个解码帧 / 文件结束）；
/// * 每次 `done <= total`、`frames_done` 单调不减；
/// * 最后一次 `frames_done` 等于该文件实际解码（被选中）的帧数。
#[test]
fn case15_decode_progress_emitted_from_hot_path() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let fx = setup();
    let plan = DecodePlan::build(&fx.dbc_map, &Selection::default()).expect("构建解码计划");
    let sink = ShardSink::new(fx.out_root.join("progress_shards")).expect("创建分片 sink");

    // 本地闭包（不走 Python）计数并记录完整载荷。
    let calls = AtomicUsize::new(0);
    let events: Mutex<Vec<Progress>> = Mutex::new(Vec::new());
    let stat = process_file(
        &fx.blf,
        &plan,
        &sink,
        256 * 1024 * 1024,
        &|payload: Progress| {
            calls.fetch_add(1, Ordering::Relaxed);
            events.lock().expect("进度事件锁").push(payload);
        },
    )
    .expect("解码文件");

    let events = events.into_inner().expect("取出进度事件");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        events.len(),
        "原子计数与记录的事件数必须一致"
    );
    assert!(
        events.len() >= 3,
        "解码期间应至少 3 次回调（开始/首个帧/结束），实际 {}",
        events.len()
    );

    let mut previous_frames = 0u64;
    for (idx, event) in events.iter().enumerate() {
        assert_eq!(event.phase, 1, "事件 {idx} 的 phase 错误：{event:?}");
        assert_eq!(event.stage, "decode", "事件 {idx} 的 stage 错误：{event:?}");
        assert!(
            event.done <= event.total,
            "事件 {idx} 的 done={} > total={}",
            event.done,
            event.total
        );
        assert!(
            event.frames_done >= previous_frames,
            "frames_done 必须单调不减：{event:?}"
        );
        previous_frames = event.frames_done;
    }

    // 本文件维度：结束时 done=1/total=1；frames_done = 实际解码帧数。
    let last = events.last().expect("至少一个事件");
    assert_eq!(last.done, 1, "本文件结束时应 done=1：{last:?}");
    assert_eq!(last.total, 1, "本文件维度 total 应为 1：{last:?}");
    assert_eq!(stat.rows, 555, "两种通道共 555 帧应全部命中");
    assert_eq!(
        last.frames_done, stat.rows,
        "最后一次 frames_done 应等于实际解码帧数：{last:?}"
    );

    for shard in sink.shard_paths() {
        let _ = std::fs::remove_file(shard);
    }

    // 子场景：只选中 Ch2 Insulation（5 帧）→ 最后一次 frames_done 必须是 5。
    let mut ch2 = HashMap::new();
    ch2.insert("Insulation".to_string(), Vec::<String>::new());
    let mut selection = Selection::default();
    selection.channels.insert(2, ch2);
    let plan_ch2 = DecodePlan::build(&fx.dbc_map, &selection).expect("构建 Ch2 解码计划");
    let sink_ch2 = ShardSink::new(fx.out_root.join("progress_shards_ch2")).expect("创建 Ch2 sink");
    let events_ch2: Mutex<Vec<Progress>> = Mutex::new(Vec::new());
    let stat_ch2 = process_file(
        &fx.blf,
        &plan_ch2,
        &sink_ch2,
        256 * 1024 * 1024,
        &|payload: Progress| {
            events_ch2.lock().expect("Ch2 进度事件锁").push(payload);
        },
    )
    .expect("解码 Ch2 文件");
    assert_eq!(stat_ch2.rows, 5, "只选中 Insulation 时应解出 5 帧");
    let events_ch2 = events_ch2.into_inner().expect("取出 Ch2 进度事件");
    assert!(
        events_ch2.len() >= 3,
        "只选中 5 帧也应至少 3 次回调，实际 {}",
        events_ch2.len()
    );
    assert_eq!(
        events_ch2.last().expect("Ch2 至少一个事件").frames_done,
        5,
        "只选中 5 帧时 frames_done 应等于 5"
    );

    for shard in sink_ch2.shard_paths() {
        let _ = std::fs::remove_file(shard);
    }
}

/// Step 7a 用例 3：节流器本身生效 —— 连续 1000 次调用实际触发数 < 1000。
#[test]
fn case16_progress_throttle_drops_rapid_events() {
    // callback=None：只验证时间闸门，不触碰 Python。
    let throttle = ProgressThrottle::new(None, 0);
    let mut fired = 0usize;
    for index in 0..1000u64 {
        let payload = Progress {
            phase: 2,
            stage: "merge".to_string(),
            done: index,
            total: 1000,
            file: None,
            frames_done: 0,
            message: format!("合并 {index}"),
        };
        if throttle.emit(payload) {
            fired += 1;
        }
    }
    assert!(fired >= 1, "首个事件必须放行");
    assert!(
        fired < 1000,
        "50ms 窗口内的连续调用必须被丢弃，实际触发 {fired}"
    );
}

/// Step 7a 用例 4：经真实 PyO3 `convert` 入口验证 —— 解码期间（phase 1）就有回调，
/// 且负载含 `frames_done` 键（GUI 靠它显示"在动"）。
#[test]
fn case17_convert_emits_phase1_progress_during_decode() {
    let fx = setup_step5();
    with_python(|py| -> PyResult<()> {
        // 定义 Python 回调，把所有 payload 原样收集进 events 列表。
        let globals = PyDict::new(py);
        py.run(
            c"events = []\ndef cb(payload):\n    events.append(payload)\n",
            Some(&globals),
            Some(&globals),
        )?;
        let callback = globals.get_item("cb")?.expect("cb 函数");

        let mut ch2 = HashMap::new();
        ch2.insert("Insulation".to_string(), Vec::<String>::new());
        let mut selected = HashMap::new();
        selected.insert("2".to_string(), ch2);
        let config = step5_config(py, &fx, selected, "step7a_progress")?;
        let _ = fast_blf_core::convert(py, &config, Some(callback.unbind()))?;

        let events = globals
            .get_item("events")?
            .expect("events 列表")
            .cast_into::<PyList>()?;
        let mut phase1 = 0usize;
        let mut phase2 = 0usize;
        let mut previous_frames = 0u64;
        let mut saw_frames_done = false;
        for item in events.iter() {
            let payload = item.cast_into::<PyDict>()?;
            let phase = payload.get_item("phase")?.unwrap().extract::<u8>()?;
            if phase != 1 {
                if phase == 2 {
                    phase2 += 1;
                }
                continue;
            }
            phase1 += 1;
            let frames = payload
                .get_item("frames_done")?
                .expect("phase 1 负载必须含 frames_done")
                .extract::<u64>()?;
            assert!(
                frames >= previous_frames,
                "frames_done 必须单调不减：{frames} < {previous_frames}"
            );
            previous_frames = frames;
            saw_frames_done = true;
        }
        assert!(saw_frames_done, "convert 必须在解码期间发 phase 1 回调");
        assert!(
            phase1 >= 2,
            "解码期间至少应有 2 次 phase 1 回调，实际 {phase1}"
        );
        assert!(phase2 >= 1, "合并/导出阶段至少应有 1 次 phase 2 回调，实际 {phase2}");
        Ok(())
    })
    .expect("Python 侧断言失败");
}

// ---------------------------------------------------------------------------
// Step 7b：timestamp 列四种口径（relative_s / relative_ms / unix_s / absolute）
// ---------------------------------------------------------------------------

/// Step 7b 现场：单通道单报文 `TimeMsg`（0x300）夹具 + 对应 DBC。
struct TimeFmtFixture {
    _dir: tempfile::TempDir,
    fixtures: PathBuf,
    dbc_map: BTreeMap<u32, PathBuf>,
    out_root: PathBuf,
}

fn setup_timefmt() -> TimeFmtFixture {
    let dir = tempfile::tempdir().expect("创建临时目录");
    let fixtures = dir.path().join("fixtures");
    make_fixtures::write_fixtures(&fixtures).expect("生成合成夹具");
    let mut dbc_map = BTreeMap::new();
    dbc_map.insert(1, fixtures.join("timefmt.dbc"));
    TimeFmtFixture {
        out_root: dir.path().join("out"),
        _dir: dir,
        fixtures,
        dbc_map,
    }
}

/// 用指定的 `time_format` 把若干 BLF 解码后拆表导出成 CSV，返回 report。
fn run_timefmt_export(
    blfs: &[PathBuf],
    dbc_map: &BTreeMap<u32, PathBuf>,
    root: &Path,
    out_name: &str,
    time_format: TimeFormat,
) -> Result<ExportReport, fast_blf_core::CoreError> {
    run_timefmt_export_full(
        blfs,
        dbc_map,
        root,
        out_name,
        time_format,
        true,
        ExportFormat::Csv,
    )
}

/// 同 [`run_timefmt_export`]，但可指定拆表/宽表与落盘格式。
fn run_timefmt_export_full(
    blfs: &[PathBuf],
    dbc_map: &BTreeMap<u32, PathBuf>,
    root: &Path,
    out_name: &str,
    time_format: TimeFormat,
    split: bool,
    format: ExportFormat,
) -> Result<ExportReport, fast_blf_core::CoreError> {
    let plan = DecodePlan::build(dbc_map, &Selection::default()).expect("构建解码计划");
    let temp = root.join(format!("{out_name}_shards"));
    let sink = ShardSink::new(temp).expect("创建分片 sink");
    let noop = |_p: Progress| {};
    for blf in blfs {
        process_file(blf, &plan, &sink, 256 * 1024 * 1024, &noop).expect("解码文件");
    }
    let tables = sink.tables_snapshot();
    let out_dir = root.join(out_name);
    let opts = ExportOptions {
        split_by_message: split,
        resample_hz: None,
        format,
        out_dir,
        excel_row_limit: 1_000_000,
        ffill: false,
        time_format,
        global_min: sink.global_min(),
        file_timings: blfs.iter().map(|path| read_file_timing(path)).collect(),
    };
    let report = export(&tables, &opts, &noop);
    for shard in sink.shard_paths() {
        let _ = std::fs::remove_file(shard);
    }
    report
}

/// 读一列并强制转成 `Vec<Option<f64>>`（CSV 可能把整数推断成整型）。
fn f64_values(df: &DataFrame, name: &str) -> Vec<Option<f64>> {
    let column = df.column(name).expect("列存在");
    let casted = column.cast(&DataType::Float64).expect("转 Float64");
    casted
        .f64()
        .expect("是 f64")
        .iter()
        .collect()
}

/// 读一张单表 CSV 的 timestamp 字符串列。
fn string_values(df: &DataFrame, name: &str) -> Vec<Option<String>> {
    let column = df.column(name).expect("列存在");
    let ca = column.str().expect("是 Utf8");
    ca.iter().map(|value| value.map(str::to_string)).collect()
}

#[test]
fn timefmt_relative_s() {
    let fx = setup_timefmt();
    let blf = fx.fixtures.join("timefmt.blf");
    let report = run_timefmt_export(
        &[blf],
        &fx.dbc_map,
        &fx.out_root,
        "tf_relative_s",
        TimeFormat::RelativeS,
    )
    .expect("relative_s 导出");
    assert_eq!(report.tables.len(), 1);
    let df = read_csv(Path::new(&report.tables[0].path));
    let ts = f64_values(&df, "timestamp");
    assert_eq!(df.height(), make_fixtures::TIME_FMT_FRAMES as usize);

    // Step 7d：写出口径对 relative_s 按微秒取整，帧 1 必须精确等于 0.01（不再有
    // epoch 级 f64 减法噪声）。这里用浮点精确相等锁死。
    assert_eq!(ts[0], Some(0.0), "帧 0 应为 0：{:?}", ts[0]);
    assert_eq!(ts[1], Some(0.01), "帧 1 应精确等于 0.01：{:?}", ts[1]);
    assert_eq!(ts[100], Some(1.0), "帧 100 应精确等于 1.0：{:?}", ts[100]);
}

#[test]
fn timefmt_relative_ms() {
    let fx = setup_timefmt();
    let blf = fx.fixtures.join("timefmt.blf");
    let report = run_timefmt_export(
        &[blf],
        &fx.dbc_map,
        &fx.out_root,
        "tf_relative_ms",
        TimeFormat::RelativeMs,
    )
    .expect("relative_ms 导出");
    let df = read_csv(Path::new(&report.tables[0].path));
    let ts = f64_values(&df, "timestamp");

    // Step 7d：relative_ms 按 1e-3 ms（=1µs）取整，必须精确等于 10 / 1000。
    assert_eq!(ts[0], Some(0.0), "帧 0 应为 0：{:?}", ts[0]);
    assert_eq!(ts[1], Some(10.0), "帧 1 应精确等于 10ms：{:?}", ts[1]);
    assert_eq!(ts[100], Some(1000.0), "帧 100 应精确等于 1000ms：{:?}", ts[100]);
}

/// Step 7d：两种相对时间口径取整后 101 帧时间列**严格递增**（无相等、无倒序）。
#[test]
fn timefmt_relative_rounding_is_monotonic() {
    let fx = setup_timefmt();
    let blf = fx.fixtures.join("timefmt.blf");

    for (idx, time_format) in [TimeFormat::RelativeS, TimeFormat::RelativeMs]
        .into_iter()
        .enumerate()
    {
        let report = run_timefmt_export(
            &[blf.clone()],
            &fx.dbc_map,
            &fx.out_root,
            &format!("tf_monotonic_{idx}"),
            time_format,
        )
        .expect("相对时间口径导出");
        let df = read_csv(Path::new(&report.tables[0].path));
        let ts = f64_values(&df, "timestamp");
        assert_eq!(
            ts.len(),
            make_fixtures::TIME_FMT_FRAMES as usize,
            "{time_format:?} 应有 101 帧"
        );

        // 单文件首帧必须为 0。
        assert_eq!(ts[0], Some(0.0), "{time_format:?} 首帧必须为 0");

        let mut previous = f64::NEG_INFINITY;
        for (row, value) in ts.iter().enumerate() {
            let value = value.expect("时间戳非空");
            assert!(
                value > previous,
                "{time_format:?} 取整后必须严格递增：第 {row} 行 {value} <= {previous}"
            );
            previous = value;
        }
    }
}

#[test]
fn timefmt_unix_s() {
    let fx = setup_timefmt();
    let blf = fx.fixtures.join("timefmt.blf");
    let report = run_timefmt_export(
        &[blf],
        &fx.dbc_map,
        &fx.out_root,
        "tf_unix_s",
        TimeFormat::UnixS,
    )
    .expect("unix_s 导出");
    let df = read_csv(Path::new(&report.tables[0].path));
    let ts = f64_values(&df, "timestamp");

    let expected = [
        (0usize, 1760623800.123456_f64),
        (1, 1760623800.133456),
        (100, 1760623801.123456),
    ];
    for (row, value) in expected {
        let actual = ts[row].expect("时间戳");
        assert!(
            (actual - value).abs() < 1e-5,
            "帧 {row} unix_s 应为 {value}，实际 {actual}"
        );
    }
}

#[test]
fn timefmt_absolute() {
    let fx = setup_timefmt();
    let blf = fx.fixtures.join("timefmt.blf");
    let report = run_timefmt_export(
        &[blf],
        &fx.dbc_map,
        &fx.out_root,
        "tf_absolute",
        TimeFormat::Absolute,
    )
    .expect("absolute 导出");
    let df = read_csv(Path::new(&report.tables[0].path));
    let ts = string_values(&df, "timestamp");

    // 期望值直接取自 DEPS §8 表格，字符串完全相等。
    let expected = [
        (0usize, "2025-10-16 14:10:00.123456"),
        (1, "2025-10-16 14:10:00.133456"),
        (100, "2025-10-16 14:10:01.123456"),
    ];
    for (row, value) in expected {
        assert_eq!(
            ts[row].as_deref(),
            Some(value),
            "帧 {row} absolute 字符串必须完全相等"
        );
    }
}

/// Step 7d：`unix_s` / `absolute` **一律不取整**。
///
/// * `unix_s` 必须与 BLF 原始时间戳逐位相等，打印串保留 6 位小数（原值 `…800.123456`）；
/// * 亚微秒回归：构造一帧原始值非微秒整数倍的 BLF，断言 `unix_s` 输出既等于原始值、
///   又不等于它按相对时间口径取整后的值（若误把微秒取整套到 `unix_s` 上就会失败）；
/// * `absolute` 墙钟字符串保持 DEPS §8 的定值不变。
#[test]
fn timefmt_unix_and_absolute_not_rounded() {
    let fx = setup_timefmt();
    let blf = fx.fixtures.join("timefmt.blf");

    // unix_s：打印串必须保留 6 位小数。
    let report = run_timefmt_export(
        &[blf.clone()],
        &fx.dbc_map,
        &fx.out_root,
        "tf_unix_not_rounded",
        TimeFormat::UnixS,
    )
    .expect("unix_s 导出");
    let csv_path = PathBuf::from(&report.tables[0].path);
    assert_eq!(
        csv_timestamp_text(&csv_path, 0),
        "1760623800.123456",
        "unix_s 打印串必须保留 6 位小数"
    );
    let df = read_csv(&csv_path);
    let ts = f64_values(&df, "timestamp");
    let raw = raw_timestamps(&blf);
    // 与 BLF 原始值逐位相等：既没有减 global_min，也没有取整。
    assert_eq!(ts[0], Some(raw[0]), "unix_s 必须等于 BLF 原始时间戳");
    assert_eq!(ts[1], Some(raw[1]), "unix_s 必须等于 BLF 原始时间戳");
    assert_eq!(ts[100], Some(raw[100]), "unix_s 必须等于 BLF 原始时间戳");

    // 亚微秒回归：原始值不是微秒整数倍 ⇒ 取整会改变它，未取整则保持原样。
    let dir = tempfile::tempdir().expect("临时目录");
    let submicro = dir.path().join("submicro.blf");
    write_submicro_blf(&submicro);
    let mut dbc_map = BTreeMap::new();
    dbc_map.insert(1, fx.fixtures.join("timefmt.dbc"));
    let report = run_timefmt_export(
        std::slice::from_ref(&submicro),
        &dbc_map,
        &fx.out_root,
        "tf_unix_submicro",
        TimeFormat::UnixS,
    )
    .expect("unix_s 亚微秒导出");
    let df = read_csv(Path::new(&report.tables[0].path));
    let ts = f64_values(&df, "timestamp");
    let raw0 = raw_timestamps(&submicro)[0];
    let micros_rounded = (raw0 * 1e6).round() / 1e6;
    assert_ne!(
        raw0, micros_rounded,
        "夹具必须真的带亚微秒分量，否则本用例无意义"
    );
    assert_eq!(ts[0], Some(raw0), "unix_s 必须原样输出亚微秒时间戳");
    assert_ne!(
        ts[0],
        Some(micros_rounded),
        "unix_s 绝不能套用相对时间的微秒取整（raw={raw0}，取整后={micros_rounded}）"
    );

    // absolute：墙钟字符串必须与 DEPS §8 定值完全一致（不被取整、不被改写）。
    let report = run_timefmt_export(
        &[blf],
        &fx.dbc_map,
        &fx.out_root,
        "tf_abs_not_rounded",
        TimeFormat::Absolute,
    )
    .expect("absolute 导出");
    let df = read_csv(Path::new(&report.tables[0].path));
    let ts = string_values(&df, "timestamp");
    let expected = [
        (0usize, "2025-10-16 14:10:00.123456"),
        (1, "2025-10-16 14:10:00.133456"),
        (100, "2025-10-16 14:10:01.123456"),
    ];
    for (row, value) in expected {
        assert_eq!(
            ts[row].as_deref(),
            Some(value),
            "absolute 帧 {row} 字符串必须不变"
        );
    }
}

/// 读取 BLF 里全部原始 `msg.timestamp`（按解码顺序，即时间升序）。
fn raw_timestamps(blf: &Path) -> Vec<f64> {
    let mut reader = blf_asc::BlfReader::open(blf).expect("打开 BLF");
    let mut stamps = Vec::new();
    for msg in reader.by_ref() {
        stamps.push(msg.timestamp);
    }
    stamps
}

/// 读取 CSV 第 `data_row` 行（0-based，表头不算）`timestamp` 字段的打印字符串。
fn csv_timestamp_text(path: &Path, data_row: usize) -> String {
    let text = std::fs::read_to_string(path).expect("读取 CSV 文本");
    let line = text
        .lines()
        .nth(data_row + 1)
        .unwrap_or_else(|| panic!("CSV 缺少数据行 {data_row}"));
    line.split(',').next().expect("第一列").to_string()
}

/// 写一份单帧 BLF：时间戳在 `TIME_FMT_START` 上叠加 123ns 亚微秒分量。
///
/// BLF 把时间戳量化到纳秒，而 epoch 量级 f64 的分辨率约 238ns，因此这个读回值不是
/// 微秒整数倍；若 `unix_s` 被错误地套用微秒取整，输出必然与原始值不等。
fn write_submicro_blf(path: &Path) {
    let mut writer = blf_asc::BlfWriter::create(path).expect("创建合成 BLF");
    push_msg(
        &mut writer,
        0,
        0x300,
        make_fixtures::TIME_FMT_START + 123e-9,
        vec![0u8; 8],
    );
    writer.finish().expect("完成合成 BLF");
}

#[test]
fn timefmt_column_is_first_and_named_timestamp() {
    let fx = setup_timefmt();
    let blf = fx.fixtures.join("timefmt.blf");
    for (idx, time_format) in [
        TimeFormat::RelativeS,
        TimeFormat::RelativeMs,
        TimeFormat::UnixS,
        TimeFormat::Absolute,
    ]
    .into_iter()
    .enumerate()
    {
        let report = run_timefmt_export(
            &[blf.clone()],
            &fx.dbc_map,
            &fx.out_root,
            &format!("tf_column_{idx}"),
            time_format,
        )
        .expect("导出");
        let df = read_csv(Path::new(&report.tables[0].path));
        let names = df.get_column_names();
        assert_eq!(names[0].as_str(), "timestamp", "timestamp 永远第一列");
        assert_eq!(
            names.iter().filter(|name| name.as_str() == "timestamp").count(),
            1,
            "timestamp 只应出现一列：{names:?}"
        );
    }
}

#[test]
fn timefmt_relative_is_global_across_files() {
    let fx = setup_timefmt();
    let blf1 = fx.fixtures.join("timefmt.blf");
    // 第二份夹具整体后移 10s：全局最小仍应来自第一份（0）。
    let blf2 = fx.fixtures.join("timefmt2.blf");
    make_fixtures::write_time_format_blf(&blf2, make_fixtures::TIME_FMT_START + 10.0)
        .expect("生成第二份夹具");

    let report = run_timefmt_export(
        &[blf1, blf2],
        &fx.dbc_map,
        &fx.out_root,
        "tf_global_min",
        TimeFormat::RelativeS,
    )
    .expect("多文件 relative_s 导出");
    let df = read_csv(Path::new(&report.tables[0].path));
    let ts = f64_values(&df, "timestamp");
    assert_eq!(
        df.height(),
        (make_fixtures::TIME_FMT_FRAMES * 2) as usize,
        "两份夹具无时间戳重叠，应全部保留"
    );

    // Step 7d：取整后多文件全局首帧仍必须精确为 0。
    assert_eq!(ts[0], Some(0.0), "全局最小必须精确映射为 0");
    let mut previous = f64::NEG_INFINITY;
    for (row, value) in ts.iter().enumerate() {
        let value = value.expect("时间戳");
        assert!(
            value + 1e-9 >= previous,
            "relative_s 必须单调递增，第 {row} 行 {value} < {previous}"
        );
        previous = value;
    }
}

#[test]
fn timefmt_bad_header() {
    let fx = setup_timefmt();
    let good = fx.fixtures.join("timefmt.blf");
    let bad = fx.fixtures.join("timefmt_bad.blf");

    // 把 144 字节里偏移 40 起的 16 字节清零 ⇒ systemtime 全 0。
    let mut bytes = std::fs::read(&good).expect("读 timefmt.blf");
    for byte in &mut bytes[40..56] {
        *byte = 0;
    }
    std::fs::write(&bad, &bytes).expect("写坏头文件");

    // absolute：必须 BAD_TIME_HEADER，且点名该文件，禁止静默降级。
    let err = run_timefmt_export(
        std::slice::from_ref(&bad),
        &fx.dbc_map,
        &fx.out_root,
        "tf_bad_abs",
        TimeFormat::Absolute,
    )
    .expect_err("absolute 遇到无效文件头必须报错");
    assert_eq!(err.code, "BAD_TIME_HEADER", "错误码：{}", err.code);
    assert!(
        err.message.contains("timefmt_bad.blf"),
        "错误消息应点名文件：{}",
        err.message
    );

    // relative_s：坏头不影响，仍能正常出数。
    let report = run_timefmt_export(
        std::slice::from_ref(&bad),
        &fx.dbc_map,
        &fx.out_root,
        "tf_bad_relative",
        TimeFormat::RelativeS,
    )
    .expect("relative_s 不应受文件头影响");
    let df = read_csv(Path::new(&report.tables[0].path));
    assert_eq!(df.height(), make_fixtures::TIME_FMT_FRAMES as usize);
}

/// Step 7b：config 里 `time_format` 非法值必须 `BAD_CONFIG`，并列出 4 个合法值。
#[test]
fn timefmt_invalid_config_value_is_bad_config() {
    let fx = setup_step5();
    with_python(|py| -> PyResult<()> {
        let config = step5_config(py, &fx, HashMap::new(), "tf_bad_config")?;
        config.set_item("time_format", "wrong_value")?;
        let err = fast_blf_core::convert(py, &config, None).expect_err("非法 time_format 必须报错");
        let text = err.to_string();
        assert!(text.contains("BAD_CONFIG"), "错误码应为 BAD_CONFIG：{text}");
        for value in ["relative_s", "relative_ms", "absolute", "unix_s"] {
            assert!(
                text.contains(value),
                "错误消息应列出合法值 {value}：{text}"
            );
        }
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// Step 7b：缺省 `time_format` 时走 `relative_s`（与显式指定结果一致）。
#[test]
fn timefmt_missing_key_defaults_to_relative_s() {
    let fx = setup_step5();
    with_python(|py| -> PyResult<()> {
        let mut ch2 = HashMap::new();
        ch2.insert("Insulation".to_string(), Vec::<String>::new());
        let mut selected = HashMap::new();
        selected.insert("2".to_string(), ch2);

        let config = step5_config(py, &fx, selected, "tf_default")?;
        // step5_config 刻意不写 time_format，验证缺省分支。
        assert!(!config.contains("time_format")?);
        let report = fast_blf_core::convert(py, &config, None)?;
        let tables = report
            .get_item("tables")?
            .expect("tables")
            .cast_into::<PyList>()?;
        assert_eq!(tables.len(), 1);
        let table = tables.get_item(0)?.cast_into::<PyDict>()?;
        let path = table.get_item("path")?.unwrap().extract::<String>()?;
        let df = read_csv(Path::new(&path));
        let ts = f64_values(&df, "timestamp");
        // multi_channel.blf 从 0 开始且默认 relative_s ⇒ 首行 0、单调不减。
        assert!(ts[0].expect("首行").abs() < 1e-9);
        let mut previous = f64::NEG_INFINITY;
        for value in ts.iter() {
            let value = value.expect("时间戳");
            assert!(value >= previous - 1e-12, "relative_s 应单调不减");
            previous = value;
        }
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// Step 7b：`absolute` 在宽表模式与 xlsx（Utf8 时间列写 Excel）下同样正确。
#[test]
fn timefmt_absolute_wide_csv_and_xlsx() {
    let fx = setup_timefmt();
    let blf = fx.fixtures.join("timefmt.blf");

    // 宽表 CSV：single 表，时间列仍是墙钟字符串。
    let report = run_timefmt_export_full(
        std::slice::from_ref(&blf),
        &fx.dbc_map,
        &fx.out_root,
        "tf_abs_wide",
        TimeFormat::Absolute,
        false,
        ExportFormat::Csv,
    )
    .expect("absolute 宽表导出");
    assert_eq!(report.tables.len(), 1);
    assert_eq!(report.tables[0].name, "Merged_All_Signals");
    let df = read_csv(Path::new(&report.tables[0].path));
    let ts = string_values(&df, "timestamp");
    assert_eq!(ts[0].as_deref(), Some("2025-10-16 14:10:00.123456"));
    assert_eq!(ts[1].as_deref(), Some("2025-10-16 14:10:00.133456"));

    // 拆表 xlsx：Utf8 时间列必须能写进 Excel。
    let report = run_timefmt_export_full(
        &[blf],
        &fx.dbc_map,
        &fx.out_root,
        "tf_abs_xlsx",
        TimeFormat::Absolute,
        true,
        ExportFormat::Xlsx,
    )
    .expect("absolute xlsx 导出");
    assert_eq!(report.format_used, ExportFormat::Xlsx);
    assert!(PathBuf::from(&report.tables[0].path).is_file());
}

// ---------------------------------------------------------------------------
// Step 8：宽表同名信号列冲突 + panic 兜底
// ---------------------------------------------------------------------------

/// `collide.blf` 的隔离现场：Ch1 上 `MsgA`（10ms×10，信号 `Counter`/`OnlyA`）
/// 与 `MsgB`（50ms×10，信号 `Counter`），两条报文都有 `Counter`。
struct CollideFixture {
    _dir: tempfile::TempDir,
    blf: PathBuf,
    dbc_map: BTreeMap<u32, PathBuf>,
    out_root: PathBuf,
}

fn setup_collide() -> CollideFixture {
    let dir = tempfile::tempdir().expect("创建临时目录");
    let fixtures = dir.path().join("fixtures");
    make_fixtures::write_fixtures(&fixtures).expect("生成合成夹具");
    let blf = fixtures.join("collide.blf");
    let dbc = fixtures.join("collide.dbc");
    let mut dbc_map = BTreeMap::new();
    dbc_map.insert(1, dbc);
    let out_root = dir.path().join("out");
    CollideFixture {
        _dir: dir,
        blf,
        dbc_map,
        out_root,
    }
}

/// 读回宽表并把列名取成 `Vec<String>`（Polars 的 `PlSmallStr` 便于比较）。
fn column_names(df: &DataFrame) -> Vec<String> {
    df.get_column_names()
        .iter()
        .map(|name| name.to_string())
        .collect()
}

/// 宽表冲突消歧的公共断言（两条分支必须完全一致）。
fn assert_collide_wide_columns(report: &ExportReport, rows: u64) -> DataFrame {
    assert_eq!(report.tables.len(), 1, "宽表模式只应有一个输出表");
    let table = &report.tables[0];
    assert_eq!(table.name, "Merged_All_Signals");
    assert_eq!(table.rows, rows);

    let df = read_csv(Path::new(&table.path));
    let names = column_names(&df);
    assert_eq!(names.first().map(String::as_str), Some("timestamp"), "首列必须是 timestamp");
    assert_eq!(
        names.iter().filter(|name| name.as_str() == "timestamp").count(),
        1,
        "宽表只能有一个 timestamp 列：{names:?}"
    );
    for expected in ["MsgA_Counter", "OnlyA", "MsgB_Counter"] {
        assert!(
            names.iter().any(|name| name == expected),
            "宽表缺少列 {expected}：{names:?}"
        );
    }
    assert!(
        !names.iter().any(|name| name == "Counter"),
        "冲突信号不得保留裸名 Counter：{names:?}"
    );

    let renames: Vec<&String> = report
        .warnings
        .iter()
        .filter(|warning| warning.contains("宽表列名冲突"))
        .collect();
    assert_eq!(renames.len(), 2, "两条冲突信号各应给出一条 warning：{:?}", report.warnings);
    assert!(renames.iter().any(|w| w.contains("MsgA_Counter")), "{:?}", report.warnings);
    assert!(renames.iter().any(|w| w.contains("MsgB_Counter")), "{:?}", report.warnings);

    df
}

/// Step 8 用例 1：宽表 + 保持原始时序，跨报文同名信号不 panic 且列名消歧。
#[test]
fn wide_keep_timeline_with_duplicate_signal_names_does_not_panic() {
    let fx = setup_collide();
    let (report, rows) = run_export_full(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "collide_wide_raw",
        false,
        ExportFormat::Csv,
        1_000_000,
        None,
        false,
    );
    assert_eq!(rows, 20, "20 帧应全部命中");
    let df = assert_collide_wide_columns(&report, 18);

    // MsgA 时间戳 0..0.09 覆盖第 0 行；MsgB 首帧也在 0。
    let msg_a_series = df
        .column("MsgA_Counter")
        .expect("MsgA_Counter 列")
        .cast(&DataType::Float64)
        .expect("转 Float64");
    let msg_a = msg_a_series.f64().expect("MsgA_Counter 是数字");
    let only_a_series = df
        .column("OnlyA")
        .expect("OnlyA 列")
        .cast(&DataType::Float64)
        .expect("转 Float64");
    let only_a = only_a_series.f64().expect("OnlyA 是数字");
    let msg_b_series = df
        .column("MsgB_Counter")
        .expect("MsgB_Counter 列")
        .cast(&DataType::Float64)
        .expect("转 Float64");
    let msg_b = msg_b_series.f64().expect("MsgB_Counter 是数字");
    assert_eq!(msg_a.get(0), Some(0.0), "MsgA.Counter 首帧应为 0");
    assert_eq!(only_a.get(0), Some(100.0), "MsgA.OnlyA 首帧应为 100");
    assert_eq!(msg_b.get(0), Some(200.0), "MsgB.Counter 首帧应为 200");
}

/// Step 8 用例 2：宽表 + 10Hz 重采样，与保持原始时序两条分支列名必须一致。
#[test]
fn wide_resample_with_duplicate_signal_names_does_not_panic() {
    let fx = setup_collide();
    let (raw, _) = run_export_full(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "collide_wide_cmp_raw",
        false,
        ExportFormat::Csv,
        1_000_000,
        None,
        false,
    );
    // 保持原始时序：0..0.45 内 MsgA(10) ∪ MsgB(10) 去掉重合的 t=0/0.05 → 18 行。
    let raw_df = assert_collide_wide_columns(&raw, 18);
    let raw_names = column_names(&raw_df);

    // 10Hz 全局网格：t0=0、t1=0.45 → 0,0.1,0.2,0.3,0.4 共 5 行，且无空值。
    let (resampled, _) = run_export_full(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "collide_wide_resample",
        false,
        ExportFormat::Csv,
        1_000_000,
        Some(10.0),
        false,
    );
    let res_df = assert_collide_wide_columns(&resampled, 5);
    let res_names = column_names(&res_df);
    assert_eq!(
        raw_names, res_names,
        "宽表两条分支（原始时序 / 重采样）的列名必须完全一致"
    );
    for column in res_df.columns() {
        assert_eq!(
            column.null_count(),
            0,
            "宽表重采样后列 {} 存在空值",
            column.name()
        );
    }
}

/// Step 8 用例 3：宽表 + 原始时序 + ffill=true，同样不得 panic。
#[test]
fn wide_duplicate_names_ffill_does_not_panic() {
    let fx = setup_collide();
    let (report, rows) = run_export_full(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "collide_wide_ffill",
        false,
        ExportFormat::Csv,
        1_000_000,
        None,
        true,
    );
    assert_eq!(rows, 20);
    let df = assert_collide_wide_columns(&report, 18);
    // ffill 后不存在空值（MsgB 在 t=0 就有样本，无前导空段）。
    for column in df.columns() {
        assert_eq!(column.null_count(), 0, "ffill 后列 {} 不应有空值", column.name());
    }
}

/// Step 8 用例 4：拆表模式不受影响，子表内信号保留原名 `Counter`。
#[test]
fn split_mode_keeps_plain_signal_names() {
    let fx = setup_collide();
    let (report, rows) = run_export_full(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "collide_split",
        true,
        ExportFormat::Csv,
        1_000_000,
        None,
        false,
    );
    assert_eq!(rows, 20);
    assert_eq!(report.tables.len(), 2, "拆表模式应有两张子表");
    assert!(
        !report.warnings.iter().any(|w| w.contains("宽表列名冲突")),
        "拆表模式不应产生宽表改名 warning：{:?}",
        report.warnings
    );

    for table in &report.tables {
        let df = read_csv(Path::new(&table.path));
        let names = column_names(&df);
        assert_eq!(names.first().map(String::as_str), Some("timestamp"));
        assert!(
            names.iter().any(|name| name == "Counter"),
            "子表 {} 应保留原名 Counter：{names:?}",
            table.name
        );
        assert!(
            !names.iter().any(|name| name.contains("MsgA_") || name.contains("MsgB_")),
            "子表 {} 不应出现报文前缀：{names:?}",
            table.name
        );
    }
}

/// Step 8 用例 5：panic 兜底包装必须把panic 收编成 `EXPORT_INTERNAL` 结构化错误。
#[test]
fn export_internal_error_is_returned_instead_of_panic() {
    use std::sync::Mutex;

    let ctx = Mutex::new(ExportPanicContext {
        stage: "merge".to_string(),
        table: "Merged_All_Signals".to_string(),
    });

    // 必然 panic 的闭包：panic 绝不能逃逸，必须变成 EXPORT_INTERNAL。
    let result: Result<(), CoreError> = catch_export_panic(&ctx, || {
        panic!("模拟 Polars 内部 unwrap 失败");
    });
    let err = result.expect_err("panic 必须被兜底成 Err，而不是逃逸");
    assert_eq!(err.code, EXPORT_INTERNAL);
    assert!(err.message.contains("merge"), "message 应带阶段名：{}", err.message);
    assert!(
        err.message.contains("Merged_All_Signals"),
        "message 应带表名：{}",
        err.message
    );
    assert!(
        err.message.contains("模拟 Polars"),
        "message 应带 panic 字符串：{}",
        err.message
    );

    // 正常返回的 Result 原样透传。
    let ok = catch_export_panic(&ctx, || Ok::<u32, CoreError>(7)).expect("正常路径应透传");
    assert_eq!(ok, 7);
}
