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
    catch_export_panic, export, normalize_dbc_bytes, process_file, read_file_timing, CoreError,
    DecodePlan, ExportFormat, ExportOptions, ExportPanicContext, ExportReport, FillPolicy,
    Progress, ProgressThrottle, Selection, ShardSink, TimeFormat, DBC_PARSE, EXPORT_INTERNAL,
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
/// * `resample_hz` / `ffill` 透传给 [`run_export_fill`]；为兼容旧用例：
///   `ffill=true` → `hold`；宽表 + 重采样沿用旧的线性插值行为（`linear`）；否则 `none`；
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
    let fill_policy = if ffill {
        FillPolicy::Hold
    } else if resample_hz.is_some() {
        FillPolicy::Linear
    } else {
        FillPolicy::None
    };
    run_export_fill(
        blf,
        dbc_map,
        root,
        out_name,
        split,
        format,
        excel_row_limit,
        resample_hz,
        fill_policy,
        None,
    )
}

/// 同 [`run_export_full`]，但显式指定最终生效的 [`FillPolicy`] 与最大保持时长。
#[allow(clippy::too_many_arguments)]
fn run_export_fill(
    blf: &Path,
    dbc_map: &BTreeMap<u32, PathBuf>,
    root: &Path,
    out_name: &str,
    split: bool,
    format: ExportFormat,
    excel_row_limit: u64,
    resample_hz: Option<f64>,
    fill_policy: FillPolicy,
    ffill_max_gap_ms: Option<f64>,
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
        fill_policy,
        ffill_max_gap_ms,
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

/// 在 DataFrame 里找 `timestamp ≈ target` 的那一行，返回该行某列的值。
fn value_at_time(df: &DataFrame, column: &str, target: f64) -> Option<f64> {
    let times = f64_values(df, "timestamp");
    let values = f64_values(df, column);
    for (idx, time) in times.iter().enumerate() {
        if let Some(time) = time {
            if (time - target).abs() < 1e-9 {
                return values.get(idx).copied().flatten();
            }
        }
    }
    None
}

/// Step 14 用例 1：宽表 + 10Hz 网格 + `hold` → 值全部来自原始采样集合，不造假值。
///
/// `Res`（1Hz，原始采样 = {0,1,2,3,4}）在 t=0.5 处必须保持 t=0 的 0.0，
/// 而线性插值会造出从未采样过的 0.5。
#[test]
fn case18_wide_resample_hold_uses_only_original_samples() {
    let fx = setup();
    let (report, _) = run_export_fill(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "fill_hold_wide",
        false,
        ExportFormat::Csv,
        1_000_000,
        Some(10.0),
        FillPolicy::Hold,
        None,
    );
    let df = read_csv(Path::new(&report.tables[0].path));

    let original: Vec<f64> = (0..=4).map(|i| i as f64).collect();
    for value in f64_values(&df, "Res").iter().flatten() {
        assert!(
            original.contains(value),
            "hold 产生了原始采样集合之外的值：{value}"
        );
    }
    assert_eq!(
        value_at_time(&df, "Res", 0.5),
        Some(0.0),
        "hold 在 t=0.5 应保持 0.0；出现 0.5 说明误用了线性插值"
    );
}

/// Step 14 用例 2：显式 `linear` 必须与旧的重采样行为逐字节一致（回归红线）。
#[test]
fn case19_wide_resample_linear_matches_legacy_behavior() {
    let fx = setup();
    let (explicit, _) = run_export_fill(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "fill_linear_wide",
        false,
        ExportFormat::Csv,
        1_000_000,
        Some(10.0),
        FillPolicy::Linear,
        None,
    );
    // `run_export_full(..., ffill=false)` 在重采样路径沿用旧的线性行为。
    let (legacy, _) = run_export_full(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "fill_linear_legacy",
        false,
        ExportFormat::Csv,
        1_000_000,
        Some(10.0),
        false,
    );

    let explicit_csv = std::fs::read_to_string(&explicit.tables[0].path).expect("读显式 linear CSV");
    let legacy_csv = std::fs::read_to_string(&legacy.tables[0].path).expect("读旧行为 CSV");
    assert_eq!(
        explicit_csv, legacy_csv,
        "显式 linear 必须与旧的重采样输出逐字节一致"
    );

    let df = read_csv(Path::new(&explicit.tables[0].path));
    assert_eq!(
        value_at_time(&df, "Res", 0.5),
        Some(0.5),
        "linear 在 t=0.5 应插值为 0.5"
    );
    assert!(explicit
        .warnings
        .iter()
        .any(|w| w.contains("线性插值") && w.contains("5 个信号")),
        "linear 必须给出逐信号计数的 warning：{:?}",
        explicit.warnings
    );
}

/// Step 14 用例 3：`hold + ffill_max_gap_ms=50` → 1Hz 的 `Res` 超限点必须置空并计入 warning。
#[test]
fn case20_hold_max_gap_nullifies_stale_points() {
    let fx = setup();
    let (report, _) = run_export_fill(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "fill_gap_wide",
        false,
        ExportFormat::Csv,
        1_000_000,
        Some(10.0),
        FillPolicy::Hold,
        Some(50.0),
    );
    let df = read_csv(Path::new(&report.tables[0].path));

    assert!(
        df.column("Res").expect("Res 列").null_count() > 0,
        "1Hz 的 Res 在 10Hz 网格上超过 50ms 的点必须置空"
    );
    assert_eq!(
        df.column("Spd").expect("Spd 列").null_count(),
        0,
        "10ms 的 Spd 每拍都有新采样，不应被置空"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("最大保持时长") && w.contains("Res") && w.contains("50ms")),
        "超限置空必须给出按信号的人话 warning：{:?}",
        report.warnings
    );
}

/// Step 14 的 `convert` config：按需写入 `fill_policy` / `ffill_max_gap_ms` / `ffill`。
#[allow(clippy::too_many_arguments)]
fn fill_config<'py>(
    py: Python<'py>,
    fx: &Fixture,
    out_name: &str,
    split: bool,
    resample_hz: Option<f64>,
    fill_policy: Option<&str>,
    ffill_max_gap_ms: Option<f64>,
    ffill: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let config = PyDict::new(py);
    config.set_item("blf_paths", vec![fx.blf.to_string_lossy().to_string()])?;
    let dbc_py: HashMap<u32, String> = fx
        .dbc_map
        .iter()
        .map(|(&channel, path)| (channel, path.to_string_lossy().to_string()))
        .collect();
    config.set_item("dbc_by_channel", dbc_py)?;
    config.set_item(
        "selected",
        HashMap::<String, HashMap<String, Vec<String>>>::new(),
    )?;
    config.set_item("split_by_message", split)?;
    match resample_hz {
        Some(hz) => config.set_item("resample_hz", hz)?,
        None => config.set_item("resample_hz", py.None())?,
    }
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
    if let Some(policy) = fill_policy {
        config.set_item("fill_policy", policy)?;
    }
    if let Some(gap) = ffill_max_gap_ms {
        config.set_item("ffill_max_gap_ms", gap)?;
    }
    if ffill {
        config.set_item("ffill", true)?;
    }
    Ok(config)
}

/// 取 `convert` 报告里第一张表的落盘路径。
fn first_table_path(report: &Bound<'_, PyDict>) -> String {
    let tables = report
        .get_item("tables")
        .expect("读取 tables")
        .expect("含 tables")
        .cast_into::<PyList>()
        .expect("tables 是 list");
    let first = tables.get_item(0).expect("第一张表").cast_into::<PyDict>().expect("table 是 dict");
    first
        .get_item("path")
        .expect("读取 path")
        .expect("含 path")
        .extract::<String>()
        .expect("path 是字符串")
}

/// Step 14 用例 4：非法 `fill_policy` / `ffill_max_gap_ms` 必须是带人话的 `BAD_CONFIG`。
#[test]
fn case21_convert_rejects_invalid_fill_config() {
    let fx = setup();
    with_python(|py| -> PyResult<()> {
        let bad_policy = fill_config(
            py,
            &fx,
            "fill_bad_policy",
            false,
            Some(10.0),
            Some("zoh"),
            None,
            false,
        )?;
        let err = fast_blf_core::convert(py, &bad_policy, None)
            .expect_err("非法 fill_policy 必须报错")
            .to_string();
        assert!(err.contains("BAD_CONFIG"), "错误码应为 BAD_CONFIG：{err}");
        assert!(err.contains("zoh"), "错误消息应附上收到的值：{err}");

        let bad_gap = fill_config(
            py,
            &fx,
            "fill_bad_gap",
            false,
            None,
            Some("hold"),
            Some(0.0),
            false,
        )?;
        let err = fast_blf_core::convert(py, &bad_gap, None)
            .expect_err("ffill_max_gap_ms=0 必须报错")
            .to_string();
        assert!(err.contains("BAD_CONFIG"), "错误码应为 BAD_CONFIG：{err}");
        assert!(
            err.contains("ffill_max_gap_ms"),
            "错误消息应点名字段：{err}"
        );
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// Step 14 用例 5：网格路径显式 `none` → 按 `hold` 处理并记 warning；`ffill=true` 仍等价于 hold。
#[test]
fn case22_convert_grid_none_falls_back_to_hold_and_ffill_alias_works() {
    let fx = setup();
    with_python(|py| -> PyResult<()> {
        let none_cfg = fill_config(
            py,
            &fx,
            "fill_grid_none",
            false,
            Some(10.0),
            Some("none"),
            None,
            false,
        )?;
        let none_report = fast_blf_core::convert(py, &none_cfg, None)?;
        let warnings = warning_texts(&none_report);
        assert!(
            warnings.iter().any(|w| w.contains("none") && w.contains("hold")),
            "网格路径 none 必须按 hold 处理并记 warning：{warnings:?}"
        );

        // 显式 hold + `ffill=true` 别名都应与 none 回退结果一致。
        let hold_cfg = fill_config(
            py,
            &fx,
            "fill_grid_hold",
            false,
            Some(10.0),
            Some("hold"),
            None,
            false,
        )?;
        let hold_report = fast_blf_core::convert(py, &hold_cfg, None)?;
        let ffill_cfg = fill_config(
            py,
            &fx,
            "fill_grid_alias",
            false,
            Some(10.0),
            None,
            None,
            true,
        )?;
        let ffill_report = fast_blf_core::convert(py, &ffill_cfg, None)?;

        let none_csv = std::fs::read_to_string(first_table_path(&none_report)).expect("读 none CSV");
        let hold_csv = std::fs::read_to_string(first_table_path(&hold_report)).expect("读 hold CSV");
        let ffill_csv =
            std::fs::read_to_string(first_table_path(&ffill_report)).expect("读 ffill CSV");
        assert_eq!(none_csv, hold_csv, "网格 none 回退结果必须与显式 hold 一致");
        assert_eq!(hold_csv, ffill_csv, "ffill=true 必须等价于显式 hold");
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// Step 14 用例 6：宽表 + 保持原始时序 + `linear` → 在并集时间轴上线性插值。
#[test]
fn case23_temporal_wide_linear_interpolates_on_union_timeline() {
    let fx = setup();
    let (report, _) = run_export_fill(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "fill_linear_temporal",
        false,
        ExportFormat::Csv,
        1_000_000,
        None,
        FillPolicy::Linear,
        None,
    );
    let df = read_csv(Path::new(&report.tables[0].path));
    assert_eq!(
        value_at_time(&df, "Res", 0.5),
        Some(0.5),
        "并集时间轴上 Res 应线性插值为 0.5"
    );
    assert_eq!(value_at_time(&df, "Res", 3.5), Some(3.5));
    assert_eq!(
        value_at_time(&df, "Res", 0.0),
        Some(0.0),
        "首个真实采样处应取原值"
    );
}

/// Step 14 用例 7：宽表 + 保持原始时序下 `none` / `hold` / `hold + gap` 的差别。
#[test]
fn case24_temporal_hold_max_gap_and_none_semantics() {
    let fx = setup();

    // none：原时序宽表如实留空。
    let (none_report, _) = run_export_fill(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "fill_temporal_none",
        false,
        ExportFormat::Csv,
        1_000_000,
        None,
        FillPolicy::None,
        None,
    );
    let none_df = read_csv(Path::new(&none_report.tables[0].path));
    assert!(
        none_df.column("Res").expect("Res 列").null_count() > 0,
        "none 时周期不同的 Res 应留空"
    );

    // hold（不限制 gap）：内部空值全部保持，整列不再有空值。
    let (hold_report, _) = run_export_fill(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "fill_temporal_hold",
        false,
        ExportFormat::Csv,
        1_000_000,
        None,
        FillPolicy::Hold,
        None,
    );
    let hold_df = read_csv(Path::new(&hold_report.tables[0].path));
    assert_eq!(
        hold_df.column("Res").expect("Res 列").null_count(),
        0,
        "hold 无 gap 时内部空值应全部保持"
    );

    // hold + gap=50ms：100ms 的 Temp1 陈旧行必须置空，10ms 的 Spd 不受影响。
    let (gap_report, _) = run_export_fill(
        &fx.blf,
        &fx.dbc_map,
        &fx.out_root,
        "fill_temporal_gap",
        false,
        ExportFormat::Csv,
        1_000_000,
        None,
        FillPolicy::Hold,
        Some(50.0),
    );
    let gap_df = read_csv(Path::new(&gap_report.tables[0].path));
    assert!(
        gap_df.column("Temp1").expect("Temp1 列").null_count() > 0,
        "100ms 的 Temp1 超过 50ms 的行应被置空"
    );
    assert_eq!(
        gap_df.column("Spd").expect("Spd 列").null_count(),
        0,
        "10ms 的 Spd 每拍都有新采样，不应被置空"
    );
    assert!(
        gap_report
            .warnings
            .iter()
            .any(|w| w.contains("最大保持时长") && w.contains("Temp1")),
        "原时序 hold 的超限置空也必须给出 warning：{:?}",
        gap_report.warnings
    );
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
        fill_policy: FillPolicy::None,
        ffill_max_gap_ms: None,
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

// ---------------------------------------------------------------------------
// Step 9：DBC 预规范化（BOM / GBK 编码 / can-dbc 不支持的段）
// ---------------------------------------------------------------------------

/// Step 9 现场：单报文（`VehSpd` 0x123 / `Spd`）×3 帧的合成 BLF + 任意字节 DBC。
struct NormFixture {
    _dir: tempfile::TempDir,
    blf: PathBuf,
    dbc: PathBuf,
}

/// 写入 3 帧 `VehSpd`（0x123）：`Spd` 原始值 = 1、2、3（Intel 16bit，factor 0.01）。
fn write_norm_blf(path: &Path) {
    let mut writer = blf_asc::BlfWriter::create(path).expect("创建合成 BLF");
    for i in 1u32..=3 {
        let mut data = vec![0u8; 8];
        data[0] = (i & 0xFF) as u8;
        data[1] = ((i >> 8) & 0xFF) as u8;
        push_msg(&mut writer, 0, 0x123, (i - 1) as f64 * 0.01, data);
    }
    writer.finish().expect("完成合成 BLF");
}

/// 用给定的 DBC 字节建一个隔离现场。
fn setup_norm(dbc_bytes: &[u8]) -> NormFixture {
    let dir = tempfile::tempdir().expect("创建临时目录");
    let blf = dir.path().join("norm.blf");
    write_norm_blf(&blf);
    let dbc = dir.path().join("norm.dbc");
    std::fs::write(&dbc, dbc_bytes).expect("写入 DBC");
    NormFixture {
        _dir: dir,
        blf,
        dbc,
    }
}

/// 组装一个只勾选全部信号（空 selected）的 `convert` config。
fn norm_config<'py>(
    py: Python<'py>,
    fx: &NormFixture,
    out_name: &str,
) -> PyResult<Bound<'py, PyDict>> {
    let config = PyDict::new(py);
    config.set_item("blf_paths", vec![fx.blf.to_string_lossy().to_string()])?;
    let mut dbc_py: HashMap<u32, String> = HashMap::new();
    dbc_py.insert(1u32, fx.dbc.to_string_lossy().to_string());
    config.set_item("dbc_by_channel", dbc_py)?;
    config.set_item(
        "selected",
        HashMap::<String, HashMap<String, Vec<String>>>::new(),
    )?;
    config.set_item("split_by_message", true)?;
    config.set_item("resample_hz", py.None())?;
    config.set_item("export_format", "csv")?;
    config.set_item(
        "out_dir",
        fx._dir
            .path()
            .join("out")
            .join(out_name)
            .to_string_lossy()
            .to_string(),
    )?;
    config.set_item(
        "temp_dir",
        fx._dir
            .path()
            .join(format!("{out_name}_shards"))
            .to_string_lossy()
            .to_string(),
    )?;
    config.set_item("excel_row_limit", 1_000_000i64)?;
    config.set_item("memory_budget_mb", 256i64)?;
    config.set_item("ffill", false)?;
    Ok(config)
}

/// 从 `convert` 返回值取 `warnings` 文本列表。
fn warning_texts(report: &Bound<'_, PyDict>) -> Vec<String> {
    report
        .get_item("warnings")
        .expect("读取 warnings 失败")
        .expect("返回值必须含 warnings")
        .cast_into::<PyList>()
        .expect("warnings 必须是 list")
        .iter()
        .map(|item| item.extract::<String>().expect("warning 必须是字符串"))
        .collect()
}

/// 取某张子表的行数。
fn norm_table_rows(report: &Bound<'_, PyDict>, name: &str) -> u64 {
    let tables = report
        .get_item("tables")
        .expect("读取 tables 失败")
        .expect("返回值必须含 tables")
        .cast_into::<PyList>()
        .expect("tables 必须是 list");
    for item in tables.iter() {
        let table = item.cast_into::<PyDict>().expect("table 必须是 dict");
        let table_name = table
            .get_item("name")
            .expect("读取 name 失败")
            .expect("table 必须含 name")
            .extract::<String>()
            .expect("name 必须是字符串");
        if table_name == name {
            return table
                .get_item("rows")
                .expect("读取 rows 失败")
                .expect("table 必须含 rows")
                .extract::<u64>()
                .expect("rows 必须是整数");
        }
    }
    panic!("缺少子表 {name}");
}

/// 含中文 `CM_` 注释 + 中文 `VAL_` 值表的 **GBK 编码** DBC 字节。
///
/// 中文定值（GBK 字节，独立于源码的 UTF-8 编码）：
/// * `车速信号` = `B3 B5 CB D9 D0 C5 BA C5`
/// * `停止`     = `CD A3 D6 B9`
/// * `行驶`     = `D0 D0 CA BB`
fn gbk_dbc_bytes() -> Vec<u8> {
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"VERSION \"\"\n\n");
    bytes.extend_from_slice(b"BO_ 291 VehSpd: 8 Vector__XXX\n");
    bytes.extend_from_slice(b" SG_ Spd : 0|16@1+ (0.01,0) [0|0] \"km/h\" Vector__XXX\n\n");
    // CM_ BO_ 291 "车速信号";
    bytes.extend_from_slice(b"CM_ BO_ 291 \"");
    bytes.extend_from_slice(&[0xB3, 0xB5, 0xCB, 0xD9, 0xD0, 0xC5, 0xBA, 0xC5]);
    bytes.extend_from_slice(b"\";\n");
    // VAL_ 291 Spd 0 "停止" 1 "行驶" ;
    bytes.extend_from_slice(b"VAL_ 291 Spd 0 \"");
    bytes.extend_from_slice(&[0xCD, 0xA3, 0xD6, 0xB9]);
    bytes.extend_from_slice(b"\" 1 \"");
    bytes.extend_from_slice(&[0xD0, 0xD0, 0xCA, 0xBB]);
    bytes.extend_from_slice(b"\" ;\n");
    bytes
}

/// Step 9 用例 1：GBK 编码 DBC 能解析、中文保留、warnings 出现 GB18030。
#[test]
fn dbcnorm_gbk_encoded_dbc_parses_and_chinese_survives() {
    let raw = gbk_dbc_bytes();

    // 纯函数层：按 GB18030 解码，中文注释/枚举值保持可读。
    let outcome = normalize_dbc_bytes(&raw, "gbk.dbc").expect("GBK DBC 应可规范化");
    assert!(
        outcome.notes.iter().any(|note| note.contains("GB18030")),
        "notes 应说明 GB18030 解码：{:?}",
        outcome.notes
    );
    assert!(
        outcome.text.contains("车速信号"),
        "中文注释应保留：{}",
        outcome.text
    );
    assert!(outcome.text.contains("停止") && outcome.text.contains("行驶"));

    // 端到端：convert 成功、3 行解出、warnings 出现 GB18030。
    let fx = setup_norm(&raw);
    with_python(|py| -> PyResult<()> {
        let config = norm_config(py, &fx, "gbk")?;
        let report =
            fast_blf_core::convert(py, &config, None).expect("GBK DBC 必须能成功转换");
        assert_eq!(
            report.get_item("total_rows")?.unwrap().extract::<u64>()?,
            3
        );
        assert_eq!(norm_table_rows(&report, "Ch1_VehSpd"), 3);
        let warnings = warning_texts(&report);
        assert!(
            warnings.iter().any(|text| text.contains("GB18030")),
            "warnings 应出现 GB18030：{warnings:?}"
        );
        assert!(
            warnings.iter().any(|text| text.contains("norm.dbc")),
            "warnings 应带文件名前缀：{warnings:?}"
        );
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// Step 9 用例 2：UTF-8 BOM 被去除并留痕。
#[test]
fn dbcnorm_utf8_bom_is_stripped() {
    let mut raw = vec![0xEF, 0xBB, 0xBF];
    raw.extend_from_slice(make_fixtures::DEMO_DBC.as_bytes());

    let outcome = normalize_dbc_bytes(&raw, "bom.dbc").expect("BOM DBC 应可规范化");
    assert!(outcome.text.starts_with("VERSION"), "BOM 应被去掉");
    assert!(
        outcome.notes.iter().any(|note| note.contains("BOM")),
        "notes 应说明去 BOM：{:?}",
        outcome.notes
    );

    // 端到端：用 BOM 版 DEMO_DBC 覆盖 norm.dbc，3 帧 VehSpd 照常解出。
    let fx = setup_norm(&raw);
    with_python(|py| -> PyResult<()> {
        let config = norm_config(py, &fx, "bom")?;
        let report = fast_blf_core::convert(py, &config, None).expect("BOM DBC 必须能转换");
        assert_eq!(norm_table_rows(&report, "Ch1_VehSpd"), 3);
        let warnings = warning_texts(&report);
        assert!(
            warnings.iter().any(|text| text.contains("BOM")),
            "warnings 应提示去 BOM：{warnings:?}"
        );
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// Step 9 用例 3：`CM_ BA_` 属性注释 + 相对属性定义被整段忽略，信号照常解出。
#[test]
fn dbcnorm_attribute_comment_and_relative_attrs_are_ignored() {
    let text = r#"VERSION ""

BU_: ECU1

BO_ 291 VehSpd: 8 ECU1
 SG_ Spd : 0|16@1+ (0.01,0) [0|0] "km/h" ECU1

BA_DEF_ "Attr" STRING ;
CM_ BA_ "Attr" "属性注释";
BA_DEF_REL_ BU_SG_REL_ "RelAttr" STRING ;
BA_DEF_DEF_REL_ "RelAttr" "";
"#;

    let outcome = normalize_dbc_bytes(text.as_bytes(), "rel.dbc").expect("应可规范化");
    let note = outcome.notes.join("\n");
    assert!(note.contains("已忽略 3 条"), "应汇总 3 条：{note}");
    assert!(note.contains("CM_ BA_"), "应点名 CM_ BA_：{note}");
    assert!(note.contains("BA_DEF_REL_"), "应点名 BA_DEF_REL_：{note}");
    assert!(!outcome.text.contains("CM_ BA_"));
    assert!(!outcome.text.contains("BA_DEF_REL_"));
    assert!(!outcome.text.contains("BA_DEF_DEF_REL_"));
    assert!(
        outcome.text.contains("BA_DEF_ \"Attr\" STRING ;"),
        "普通 BA_DEF_ 必须保留"
    );

    // 信号照常解出。
    let fx = setup_norm(text.as_bytes());
    with_python(|py| -> PyResult<()> {
        let config = norm_config(py, &fx, "rel")?;
        let report = fast_blf_core::convert(py, &config, None).expect("忽略元数据段后必须能转换");
        assert_eq!(norm_table_rows(&report, "Ch1_VehSpd"), 3);
        let warnings = warning_texts(&report);
        assert!(
            warnings
                .iter()
                .any(|text| text.contains("已忽略 3 条") && text.contains("CM_ BA_")),
            "warnings 应说明忽略了哪些语句与条数：{warnings:?}"
        );
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// Step 9 用例 4：`VAL_` / `VAL_TABLE_` / `SG_MUL_VAL_` / `SIG_VALTYPE_` /
/// `SIG_GROUP_` / `BO_TX_BU_` / `EV_` **不被剔除**，且报文/信号照常解出。
#[test]
fn dbcnorm_keeps_supported_sections_intact() {
    let text = r#"VERSION ""

BU_: ECU1 ECU2

BO_ 291 VehSpd: 8 ECU1
 SG_ Spd : 0|16@1+ (0.01,0) [0|0] "km/h" ECU2

BO_ 256 MuxMsg: 8 ECU1
 SG_ Mode M : 0|8@1+ (1,0) [0|255] "" ECU2
 SG_ MuxA m0 : 8|8@1+ (1,0) [0|255] "" ECU2

BO_ 512 SimpleMsg: 8 ECU1
 SG_ Counter : 0|8@1+ (1,0) [0|255] "" ECU2

VAL_TABLE_ ModeTable 0 "Off" 1 "On" ;
VAL_ 256 Mode 0 "Off" 1 "On" ;
SG_MUL_VAL_ 256 MuxA Mode 0-0 ;
SIG_VALTYPE_ 512 Counter : 1 ;
SIG_GROUP_ 512 Group1 1 : Counter ;
BO_TX_BU_ 512 : ECU1, ECU2 ;
EV_ EnvVar: 0 [0|100] "" 0 0 DUMMY_NODE_VECTOR0 ECU1;
"#;

    let outcome = normalize_dbc_bytes(text.as_bytes(), "supported.dbc").expect("应可规范化");
    for keyword in [
        "VAL_",
        "VAL_TABLE_",
        "SG_MUL_VAL_",
        "SIG_VALTYPE_",
        "SIG_GROUP_",
        "BO_TX_BU_",
        "EV_",
    ] {
        assert!(
            outcome.text.contains(keyword),
            "{keyword} 不应被剔除：{}",
            outcome.text
        );
    }
    // 没有任何语句被忽略 → 不应产生“已忽略”note。
    assert!(
        !outcome.notes.iter().any(|note| note.contains("已忽略")),
        "支持段不应触发忽略 note：{:?}",
        outcome.notes
    );

    let fx = setup_norm(text.as_bytes());
    with_python(|py| -> PyResult<()> {
        let config = norm_config(py, &fx, "supported")?;
        let report =
            fast_blf_core::convert(py, &config, None).expect("支持段齐全的 DBC 必须能转换");
        assert_eq!(norm_table_rows(&report, "Ch1_VehSpd"), 3);
        assert!(warning_texts(&report).iter().all(|w| !w.contains("已忽略")));
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// Step 9 用例 5：跨 3 行的语句整段剔除后，后续 `BO_`/`SG_` 行号与内容不受影响。
#[test]
fn dbcnorm_multiline_statement_removal_is_line_safe() {
    let text = "VERSION \"\"\n\nBA_DEF_REL_ BU_SG_REL_ \"RelAttr\"\n    STRING ;\n\nBU_: ECU1\n\nBO_ 291 VehSpd: 8 ECU1\n SG_ Spd : 0|16@1+ (0.01,0) [0|0] \"km/h\" ECU1\n";
    let outcome = normalize_dbc_bytes(text.as_bytes(), "multi.dbc").expect("应可规范化");

    // 行数不变，BO_/SG_ 行号与内容逐字不变。
    let input_lines: Vec<&str> = text.lines().collect();
    let output_lines: Vec<&str> = outcome.text.lines().collect();
    assert_eq!(input_lines.len(), output_lines.len(), "行数必须保持不变");
    let bo_idx = input_lines
        .iter()
        .position(|line| line.starts_with("BO_ 291"))
        .expect("输入应含 BO_ 行");
    assert_eq!(output_lines[bo_idx], "BO_ 291 VehSpd: 8 ECU1");
    assert_eq!(
        output_lines[bo_idx + 1],
        " SG_ Spd : 0|16@1+ (0.01,0) [0|0] \"km/h\" ECU1"
    );
    assert!(!outcome.text.contains("BA_DEF_REL_"));
    assert!(!outcome.text.contains("STRING ;"));

    // 后续报文照常解出。
    let fx = setup_norm(text.as_bytes());
    with_python(|py| -> PyResult<()> {
        let config = norm_config(py, &fx, "multi")?;
        let report = fast_blf_core::convert(py, &config, None).expect("必须能转换");
        assert_eq!(norm_table_rows(&report, "Ch1_VehSpd"), 3);
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// Step 9 用例 6：无法用 UTF-8/GB18030 解释的字节 → 明确错误（含文件名），不 panic。
#[test]
fn dbcnorm_invalid_utf8_and_not_gbk_reports_clear_error() {
    // 0xFF 在 GB18030 中不是合法首字节，必然无法解码。
    let raw: Vec<u8> = vec![0xFF, 0xFF, 0xFF, 0xFF];

    let err = normalize_dbc_bytes(&raw, "broken_encoding.dbc").expect_err("必须报错");
    assert!(
        err.contains("broken_encoding.dbc") && err.contains("GB18030"),
        "错误应带文件名与编码原因：{err}"
    );

    let fx = setup_norm(&raw);
    let mut dbc_map = BTreeMap::new();
    dbc_map.insert(1u32, fx.dbc.clone());

    // build_lenient 不 panic，错误消息带文件名（convert / scan 走同一条路径）。
    let (plan, errors, _notes) = DecodePlan::build_lenient(&dbc_map, &Selection::default());
    assert!(plan.is_empty(), "无法解码时计划必须为空");
    let build_err = errors.get(&1).expect("通道 1 应有错误");
    assert_eq!(build_err.code, DBC_PARSE);
    assert!(
        build_err.message.contains("norm.dbc"),
        "错误消息应带文件名：{}",
        build_err.message
    );
    assert!(
        build_err.message.contains("GB18030"),
        "错误消息应带编码原因：{}",
        build_err.message
    );

    // `build`（严格路径）同样返回结构化 DBC_PARSE，而不是 panic。
    let strict = DecodePlan::build(&dbc_map, &Selection::default()).expect_err("必须返回 Err");
    assert_eq!(strict.code, DBC_PARSE);
}

// ---------------------------------------------------------------------------
// Step 10：DBC 规范化误伤修复（NS_ 段 / 列 0 规则 / 证据驱动剔除清单 / 不变量）
// ---------------------------------------------------------------------------

/// 统计顶层定义行数：`trim_start` 后以 `prefix + 空白` 开头。
///
/// 与 `dbc_norm.rs` 内部实现同口径：`BO_` 不误计 `BO_TX_BU_`、`SG_` 不误计 `SG_MUL_VAL_`。
fn count_definition_lines(text: &str, prefix: &str) -> usize {
    text.split_inclusive('\n')
        .filter(|line| match line.trim_start().strip_prefix(prefix) {
            Some(rest) => rest.starts_with(' ') || rest.starts_with('\t'),
            None => false,
        })
        .count()
}

/// 回归现场：`NS_ :` 段列出常见元关键字 + 1 个报文 2 个信号。
///
/// Step 9 的 bug 会把 `NS_` 段关键字当成剔除起点，进而吞掉整个报文定义段 →
/// `scan` 返回 0 报文且无错误（静默丢数据）。本常量是直接复现用例。
const NS_SECTION_VARIANT_DBC: &str = r#"VERSION ""

NS_ :
    NS_DESC_
    CM_
    BA_DEF_
    BA_
    VAL_
    CAT_DEF_
    CAT_
    FILTER
    BA_DEF_DEF_
    EV_DATA_
    ENVVAR_DATA_
    SGTYPE_
    SGTYPE_VAL_
    BA_DEF_SGTYPE_
    BA_SGTYPE_
    SIG_TYPE_REF_
    VAL_TABLE_
    SIG_GROUP_
    SIG_VALTYPE_
    SIGTYPE_VALTYPE_
    BO_TX_BU_
    BA_DEF_REL_
    BA_REL_
    BA_DEF_DEF_REL_
    BU_SG_REL_
    BU_EV_REL_
    BU_BO_REL_
    SG_MUL_VAL_

BS_:

BU_: ECU1

BO_ 291 VehSpd: 8 ECU1
 SG_ Spd : 0|16@1+ (0.01,0) [0|0] "km/h" ECU1
 SG_ SpdValid : 16|1@1+ (1,0) [0|0] "" ECU1
"#;

/// Step 10 用例 1（回归复现）：`NS_ :` 段关键字绝不能被剔除，报文/信号数保持 1/2。
#[test]
fn dbcnorm_ns_section_keywords_are_not_stripped() {
    let outcome =
        normalize_dbc_bytes(NS_SECTION_VARIANT_DBC.as_bytes(), "ns_variant.dbc").expect("应可规范化");

    // NS_ 段关键字一个都不能少。
    for keyword in [
        "CAT_DEF_",
        "CAT_",
        "FILTER",
        "SGTYPE_",
        "SGTYPE_VAL_",
        "BA_DEF_SGTYPE_",
        "SIGTYPE_VALTYPE_",
        "SIG_TYPE_REF_",
        "EV_DATA_",
        "ENVVAR_DATA_",
        "BA_REL_",
        "BU_SG_REL_",
        "BU_EV_REL_",
        "BU_BO_REL_",
        "BA_DEF_REL_",
        "BA_DEF_DEF_REL_",
    ] {
        assert!(
            outcome.text.contains(keyword),
            "{keyword} 在 NS_ 段内不应被剔除：{}",
            outcome.text
        );
    }

    // 直接复现主模型实测：规范化后报文 1 个、信号 2 个（Step 9 此处是 0 / 0）。
    let dbc = can_dbc::Dbc::try_from(outcome.text.as_str()).expect("规范化后必须能解析");
    assert_eq!(dbc.messages.len(), 1, "报文数必须为 1");
    assert_eq!(dbc.messages[0].signals.len(), 2, "信号数必须为 2");
    assert!(
        !outcome.notes.iter().any(|note| note.contains("已忽略")),
        "NS_ 段不应触发任何剔除：{:?}",
        outcome.notes
    );
}

/// Step 10 用例 2（不变量）：规范化后 `BO_` / `SG_` 行数与原文本严格相等。
#[test]
fn dbcnorm_never_reduces_bo_and_sg_lines() {
    let outcome =
        normalize_dbc_bytes(NS_SECTION_VARIANT_DBC.as_bytes(), "ns_variant.dbc").expect("应可规范化");
    assert_eq!(
        count_definition_lines(&outcome.text, "BO_"),
        count_definition_lines(NS_SECTION_VARIANT_DBC, "BO_"),
        "BO_ 行数不得减少"
    );
    assert_eq!(
        count_definition_lines(&outcome.text, "SG_"),
        count_definition_lines(NS_SECTION_VARIANT_DBC, "SG_"),
        "SG_ 行数不得减少"
    );
}

/// Step 10 用例 3（证据驱动）：剔除清单 == 证据表里所有「期望剔除」项。
///
/// 对每个候选关键字构造「最小 DBC + 该关键字的真实出现形态」，**绕过规范化**直接调用
/// `can_dbc::Dbc::try_from`：解析失败 ⇒ 必须剔除；解析成功 ⇒ 绝对不能剔除。
/// 再用 `normalize_dbc_bytes` 验证运行期清单行为与证据表一致。
#[test]
fn dbcnorm_strip_list_matches_evidence() {
    const BASE: &str = "VERSION \"\"\n\nBU_: ECU1\n\nBO_ 291 VehSpd: 8 ECU1\n SG_ Spd : 0|16@1+ (0.01,0) [0|0] \"km/h\" ECU1\n";

    // (关键字, 真实出现形态的语句, 该语句是否位于 NS_ 段, 期望是否剔除)
    let table: &[(&str, &str, bool, bool)] = &[
        ("CM_ BA_", "CM_ BA_ \"Attr\" \"comment\";", false, true),
        (
            "BA_DEF_REL_",
            "BA_DEF_REL_ BU_SG_REL_ \"RelAttr\"\n    STRING ;",
            false,
            true,
        ),
        (
            "BA_DEF_DEF_REL_",
            "BA_DEF_DEF_REL_ \"RelAttr\"\n    \"\";",
            false,
            true,
        ),
        (
            "BA_REL_",
            "BA_REL_ \"RelAttr\" BU_SG_REL_ ECU1 SG_ 291 Spd 1;",
            false,
            false,
        ),
        ("ENVVAR_DATA_", "ENVVAR_DATA_ SomeEnv: 10;", false, false),
        ("CAT_", "CAT_", true, false),
        ("CAT_DEF_", "CAT_DEF_", true, false),
        ("FILTER", "FILTER", true, false),
        ("EV_DATA_", "EV_DATA_", true, false),
        ("SGTYPE_", "SGTYPE_", true, false),
        ("SGTYPE_VAL_", "SGTYPE_VAL_", true, false),
        ("BA_DEF_SGTYPE_", "BA_DEF_SGTYPE_", true, false),
        ("BA_SGTYPE_", "BA_SGTYPE_", true, false),
        ("SIG_TYPE_REF_", "SIG_TYPE_REF_", true, false),
        ("SIGTYPE_VALTYPE_", "SIGTYPE_VALTYPE_", true, false),
        ("BU_SG_REL_", "BU_SG_REL_", true, false),
        ("BU_EV_REL_", "BU_EV_REL_", true, false),
        ("BU_BO_REL_", "BU_BO_REL_", true, false),
    ];

    let mut expected_strip: Vec<&str> = Vec::new();

    for &(keyword, statement, in_ns, expect_strip) in table {
        // ① 原始文本（真实出现形态）必须能被/不能被底层解析器接受。
        let raw = if in_ns {
            format!(
                "VERSION \"\"\n\nNS_ :\n    {statement}\n\nBU_: ECU1\n\nBO_ 291 VehSpd: 8 ECU1\n SG_ Spd : 0|16@1+ (0.01,0) [0|0] \"km/h\" ECU1\n"
            )
        } else {
            format!("{BASE}{statement}\n")
        };
        let parsed = can_dbc::Dbc::try_from(raw.as_str());
        if expect_strip {
            assert!(
                parsed.is_err(),
                "{keyword}: 原始文本被证明解析失败，故必须在剔除清单里"
            );
            expected_strip.push(keyword);
        } else {
            assert!(
                parsed.is_ok(),
                "{keyword}: 原始文本解析成功，故绝对不能剔除（实际错误：{:?}）",
                parsed.err().map(|err| err.to_string())
            );
        }

        // ② 运行期清单行为：顶层出现时，期望剔除的关键字必须消失，其余必须保留。
        let probe = format!("{BASE}{statement}\n");
        let outcome = normalize_dbc_bytes(probe.as_bytes(), "evidence.dbc").expect("应可规范化");
        assert_eq!(
            !outcome.text.contains(keyword),
            expect_strip,
            "{keyword}: 规范化行为与证据表不一致\n{}",
            outcome.text
        );
    }

    expected_strip.sort_unstable();
    assert_eq!(
        expected_strip,
        vec!["BA_DEF_DEF_REL_", "BA_DEF_REL_", "CM_ BA_"],
        "剔除清单必须恰好是证据证明会失败的关键字集合"
    );
}

/// Step 10 用例 4：跨行待剔除语句后紧跟 `BO_`/`SG_` 时，报文定义必须完好。
#[test]
fn dbcnorm_multiline_strip_cannot_eat_message_definitions() {
    let text = "VERSION \"\"\n\nNS_ :\n    CAT_\n    FILTER\n\nBU_: ECU1\n\nBA_DEF_REL_ BU_SG_REL_ \"RelAttr\"\nBO_ 291 VehSpd: 8 ECU1\n SG_ Spd : 0|16@1+ (0.01,0) [0|0] \"km/h\" ECU1\n\nBO_ 418 BattTemp: 8 ECU1\n SG_ Temp1 : 7|8@0- (1,0) [0|0] \"degC\" ECU1\n";

    let outcome = normalize_dbc_bytes(text.as_bytes(), "multi.dbc").expect("应可规范化");

    // 不变量：BO_/SG_ 行数一致。
    assert_eq!(
        count_definition_lines(&outcome.text, "BO_"),
        count_definition_lines(text, "BO_")
    );
    assert_eq!(
        count_definition_lines(&outcome.text, "SG_"),
        count_definition_lines(text, "SG_")
    );

    // 两个报文、两个信号都在。
    assert!(outcome.text.contains("BO_ 291 VehSpd: 8 ECU1"));
    assert!(outcome.text.contains("BO_ 418 BattTemp: 8 ECU1"));
    assert!(outcome.text.contains(" SG_ Spd : 0|16@1+ (0.01,0) [0|0] \"km/h\" ECU1"));
    assert!(outcome.text.contains(" SG_ Temp1 : 7|8@0- (1,0) [0|0] \"degC\" ECU1"));

    // 未找到 `;` 时保守：只剔除起始行，并留痕。
    assert!(!outcome.text.contains("BA_DEF_REL_"));
    assert!(
        outcome.notes.join("\n").contains("保守处理"),
        "应记保守剔除 note：{:?}",
        outcome.notes
    );

    let dbc = can_dbc::Dbc::try_from(outcome.text.as_str()).expect("规范化后必须能解析");
    assert_eq!(dbc.messages.len(), 2, "两个报文定义必须完好");
    assert_eq!(
        dbc.messages.iter().map(|m| m.signals.len()).sum::<usize>(),
        2,
        "两个信号定义必须完好"
    );
}

/// Step 10 用例 5：Step 9 的两个核心用例（GBK 中文注释 / UTF-8 BOM）仍工作。
#[test]
fn dbcnorm_gbk_and_bom_still_work() {
    // GBK 中文注释 + 中文 VAL_。
    let gbk = gbk_dbc_bytes();
    let outcome = normalize_dbc_bytes(&gbk, "gbk.dbc").expect("GBK DBC 应可规范化");
    assert!(
        outcome.notes.iter().any(|note| note.contains("GB18030")),
        "notes 应说明 GB18030 解码：{:?}",
        outcome.notes
    );
    assert!(outcome.text.contains("车速信号"), "中文注释应保留");
    assert!(outcome.text.contains("停止") && outcome.text.contains("行驶"));
    assert!(
        can_dbc::Dbc::try_from(outcome.text.as_str()).is_ok(),
        "GBK 解码后的文本必须能被底层解析器接受"
    );

    // UTF-8 BOM。
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(make_fixtures::DEMO_DBC.as_bytes());
    let outcome = normalize_dbc_bytes(&bom, "bom.dbc").expect("BOM DBC 应可规范化");
    assert!(outcome.text.starts_with("VERSION"), "BOM 应被去掉");
    assert!(
        outcome.notes.iter().any(|note| note.contains("BOM")),
        "notes 应说明去 BOM：{:?}",
        outcome.notes
    );
    assert!(
        can_dbc::Dbc::try_from(outcome.text.as_str()).is_ok(),
        "去 BOM 后的文本必须能被底层解析器接受"
    );
}

// ---------------------------------------------------------------------------
// Step 12：DBC 编码回退判别（孤立高位字节 → cp1252，连续多字节 → GB18030）
// ---------------------------------------------------------------------------

/// 真机现场复现用的 DBC 字节：单位串 `"°C"` 按 **cp1252** 写出（`°` = 单字节 `0xB0`）。
///
/// 用户真机文件 `CAN_Matrix_MAL_P35_Hybrid_V3.2.0_Hybrid.dbc` 里就是这样 2 个孤立 `0xB0`；
/// 按 GB18030 解会把后随的 ASCII `C` 当尾字节吃掉（`"°C"` → `"癈"`）。
fn cp1252_degree_dbc_bytes() -> Vec<u8> {
    let mut raw: Vec<u8> = Vec::new();
    raw.extend_from_slice(b"VERSION \"\"\n\n");
    raw.extend_from_slice(b"BU_: HV_Battery_NIO\n\n");
    raw.extend_from_slice(b"BO_ 100 TEngineCoolant: 8 HV_Battery_NIO\n");
    raw.extend_from_slice(
        b" SG_ PID05_TEngineCoolant : 48|8@1+ (0.75,-48) [-48.00|143.25] \"",
    );
    raw.push(0xB0); // cp1252 的 `°`
    raw.extend_from_slice(b"C\" HV_Battery_NIO\n");
    raw
}

/// Step 12 用例 1（核心）：孤立高位字节 `0xB0` 必须按 Windows-1252 解出 `°C`，
/// 绝不能按 GB18030 把后随的 `C` 吞掉。
#[test]
fn dbcnorm_isolated_degree_sign_uses_cp1252() {
    let raw = cp1252_degree_dbc_bytes();

    let outcome = normalize_dbc_bytes(&raw, "hybrid.dbc").expect("含 cp1252 ° 的 DBC 应可规范化");
    let note = outcome.notes.join("\n");
    assert!(
        note.contains("Windows-1252"),
        "note 应说明 Windows-1252：{note}"
    );
    assert!(
        !note.contains("GB18030"),
        "孤立高位字节不得判定为 GB18030：{note}"
    );

    // 单位没有被吞掉：`°C` 完整，且没有替换符 / GB18030 误码。
    assert!(
        outcome.text.contains("\"°C\""),
        "单位应精确为 °C：{}",
        outcome.text
    );
    assert!(!outcome.text.contains('\u{FFFD}'), "不应出现 U+FFFD 替换符");
    assert!(!outcome.text.contains('癈'), "0xB0 不得被当成 GB18030 首字节吞掉 C");

    // 报文/信号数量正确，整行没被吃掉。
    let dbc = can_dbc::Dbc::try_from(outcome.text.as_str()).expect("规范化后必须能解析");
    assert_eq!(dbc.messages.len(), 1, "报文数必须为 1");
    assert_eq!(dbc.messages[0].signals.len(), 1, "信号数必须为 1");
    let signal = &dbc.messages[0].signals[0];
    assert_eq!(signal.name, "PID05_TEngineCoolant");
    assert_eq!(signal.unit, "°C", "单位字符串必须精确等于 °C");
}

/// Step 12 用例 2：GBK 连续多字节（中文 `CM_` 注释 + 中文 `VAL_`）仍按 GB18030 解码。
#[test]
fn dbcnorm_cjk_run_still_uses_gb18030() {
    let raw = gbk_dbc_bytes();

    let outcome = normalize_dbc_bytes(&raw, "gbk.dbc").expect("GBK DBC 应可规范化");
    let note = outcome.notes.join("\n");
    assert!(note.contains("GB18030"), "note 应说明 GB18030：{note}");
    assert!(
        !note.contains("Windows-1252"),
        "连续多字节不得判定为 cp1252：{note}"
    );
    assert!(
        outcome.text.contains("车速信号"),
        "中文注释应保留：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("停止") && outcome.text.contains("行驶"),
        "中文 VAL_ 应保留：{}",
        outcome.text
    );

    let dbc = can_dbc::Dbc::try_from(outcome.text.as_str()).expect("规范化后必须能解析");
    assert_eq!(dbc.messages.len(), 1, "报文数必须为 1");
    assert_eq!(dbc.messages[0].signals.len(), 1, "信号数必须为 1");
}

// ---------------------------------------------------------------------------
// Step 11：scan 每个通道回传 `dbc_note`（解析成功但被规范化过时的一行说明）
// ---------------------------------------------------------------------------

/// 取 `scan` 返回值里某通道的 `dbc_note` 原始对象（断言键必须存在）。
fn scan_channel<'py>(tree: &Bound<'py, PyDict>, channel: &str) -> Bound<'py, PyDict> {
    let channels = tree
        .get_item("channels")
        .expect("读取 channels 失败")
        .expect("scan 必须含 channels")
        .cast_into::<PyDict>()
        .expect("channels 必须是 dict");
    channels
        .get_item(channel)
        .expect("读取通道失败")
        .unwrap_or_else(|| panic!("通道 {channel} 必须出现"))
        .cast_into::<PyDict>()
        .expect("通道必须是 dict")
}

/// 用空 BLF 列表扫描一个只绑 Ch1 的 DBC，返回通道 1 的 dict。
fn scan_single_dbc<'py>(py: Python<'py>, dbc: &Path) -> PyResult<Bound<'py, PyDict>> {
    let mut dbc_py: HashMap<u32, String> = HashMap::new();
    dbc_py.insert(1u32, dbc.to_string_lossy().to_string());
    let tree = fast_blf_core::scan(py, Vec::<String>::new(), dbc_py, None)?;
    let ch1 = scan_channel(&tree, "1");
    Ok(ch1)
}

/// Step 11 用例 1：GBK 编码 DBC 解析成功 → `dbc_note` 非空且含 `GB18030`，`dbc_error=None`。
#[test]
fn scan_reports_dbc_note_for_gbk_dbc() {
    let fx = setup_norm(&gbk_dbc_bytes());
    with_python(|py| -> PyResult<()> {
        let ch1 = scan_single_dbc(py, &fx.dbc)?;
        assert!(
            ch1.get_item("dbc_error")?.expect("必须含 dbc_error").is_none(),
            "GBK DBC 必须解析成功"
        );
        let note = ch1.get_item("dbc_note")?.expect("通道必须含 dbc_note 键");
        assert!(!note.is_none(), "GBK DBC 应被规范化，dbc_note 不能为空");
        let note_text = note.extract::<String>()?;
        assert!(
            note_text.contains("GB18030"),
            "dbc_note 应说明 GB18030：{note_text}"
        );

        // 报文可解：空观测时也按 DBC 列出报文并带信号。
        let messages = ch1
            .get_item("messages")?
            .expect("通道必须含 messages")
            .cast_into::<PyList>()?;
        assert!(!messages.is_empty(), "GBK DBC 的报文应可解出");
        let first = messages.get_item(0)?.cast_into::<PyDict>()?;
        let signals = first
            .get_item("signals")?
            .expect("报文必须含 signals")
            .cast_into::<PyList>()?;
        assert!(!signals.is_empty(), "报文信号应可解出");
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// Step 11 用例 2：纯 ASCII DBC 未被规范化 → `dbc_note is None`。
#[test]
fn scan_dbc_note_is_none_for_clean_dbc() {
    let fx = setup_norm(make_fixtures::DEMO_DBC.as_bytes());
    with_python(|py| -> PyResult<()> {
        let ch1 = scan_single_dbc(py, &fx.dbc)?;
        let note = ch1.get_item("dbc_note")?.expect("通道必须含 dbc_note 键");
        assert!(note.is_none(), "纯 ASCII DBC 不应有 dbc_note");
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// Step 11 用例 3：含 `CM_ BA_` / `BA_DEF_REL_` 的 DBC → `dbc_note` 提到忽略了语句。
#[test]
fn scan_dbc_note_present_for_ignored_statements() {
    let text = "VERSION \"\"\n\nBU_: ECU1\n\nBO_ 291 VehSpd: 8 ECU1\n SG_ Spd : 0|16@1+ (0.01,0) [0|0] \"km/h\" ECU1\n\nCM_ BA_ \"Attr\" \"comment\";\nBA_DEF_REL_ BU_SG_REL_ \"RelAttr\"\n    STRING ;\n";
    let fx = setup_norm(text.as_bytes());
    with_python(|py| -> PyResult<()> {
        let ch1 = scan_single_dbc(py, &fx.dbc)?;
        assert!(
            ch1.get_item("dbc_error")?.expect("必须含 dbc_error").is_none(),
            "剔除不支持语句后 DBC 必须解析成功"
        );
        let note = ch1.get_item("dbc_note")?.expect("通道必须含 dbc_note 键");
        assert!(!note.is_none(), "剔除语句属于规范化，dbc_note 不能为空");
        let note_text = note.extract::<String>()?;
        assert!(
            note_text.contains("已忽略"),
            "dbc_note 应提到忽略了语句：{note_text}"
        );
        assert!(
            note_text.contains("CM_ BA_"),
            "dbc_note 应点名 CM_ BA_：{note_text}"
        );
        Ok(())
    })
    .expect("Python 侧断言失败");
}

// ---------------------------------------------------------------------------
// Step 15：series_for_plot（min/max 分桶抽稀 + 全量统计）
// ---------------------------------------------------------------------------

/// 仓库根 `fixtures/` 目录（`CARGO_MANIFEST_DIR` = crates/fast_blf_core）。
fn repo_fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// 组装 `series_for_plot` 的 config（字段与 `docs/INTERFACE.md` §1.3 一致）。
fn series_config<'py>(
    py: Python<'py>,
    blf: &Path,
    dbc_by_channel: &[(u32, PathBuf)],
    time_format: &str,
    buckets: usize,
) -> PyResult<Bound<'py, PyDict>> {
    let config = PyDict::new(py);
    config.set_item("blf_paths", vec![blf.to_string_lossy().to_string()])?;
    let dbc_py: HashMap<u32, String> = dbc_by_channel
        .iter()
        .map(|(channel, path)| (*channel, path.to_string_lossy().to_string()))
        .collect();
    config.set_item("dbc_by_channel", dbc_py)?;
    // 空 selected = 全部选中。
    config.set_item("selected", HashMap::<String, HashMap<String, Vec<String>>>::new())?;
    config.set_item("time_format", time_format)?;
    config.set_item("buckets", buckets)?;
    config.set_item("temp_dir", py.None())?;
    Ok(config)
}

/// 从 `series_for_plot` 返回值里取每条曲线的关键字段。
#[derive(Debug)]
struct SeriesRow {
    message: String,
    signal: String,
    count: u64,
    vmin: f64,
    vmax: f64,
    first: f64,
    last: f64,
    t_len: usize,
}

fn series_rows(out: &Bound<'_, PyDict>) -> PyResult<Vec<SeriesRow>> {
    let list = out
        .get_item("series")?
        .expect("返回值必须含 series")
        .cast_into::<PyList>()?;
    let mut rows = Vec::new();
    for item in list.iter() {
        let dict = item.cast_into::<PyDict>()?;
        let t = dict
            .get_item("t")?
            .expect("series 元素必须含 t")
            .cast_into::<PyList>()?;
        rows.push(SeriesRow {
            message: dict.get_item("message")?.unwrap().extract()?,
            signal: dict.get_item("signal")?.unwrap().extract()?,
            count: dict.get_item("count")?.unwrap().extract()?,
            vmin: dict.get_item("vmin")?.unwrap().extract()?,
            vmax: dict.get_item("vmax")?.unwrap().extract()?,
            first: dict.get_item("first")?.unwrap().extract()?,
            last: dict.get_item("last")?.unwrap().extract()?,
            t_len: t.len(),
        });
    }
    Ok(rows)
}

/// 逐帧解码后手算每条信号的 `(count, vmin, vmax)`（独立于 series_for_plot）。
fn hand_decode_stats(
    blf: &Path,
    dbc_by_channel: &[(u32, PathBuf)],
) -> BTreeMap<(u32, String, String), (u64, f64, f64)> {
    let dbc_map: BTreeMap<u32, PathBuf> = dbc_by_channel.iter().cloned().collect();
    let plan = DecodePlan::build(&dbc_map, &Selection::default()).expect("构建手算计划");
    let mut out: BTreeMap<(u32, String, String), (u64, f64, f64)> = BTreeMap::new();
    let mut reader = blf_asc::BlfReader::open(blf).expect("打开 BLF 手算");
    for msg in reader.by_ref() {
        let channel = msg.channel as u32 + 1;
        let frame_id = msg.arbitration_id.0 | if msg.is_extended_id { 1 << 31 } else { 0 };
        let Some(message) = plan.lookup(channel, frame_id) else {
            continue;
        };
        for signal in &message.signals {
            let value = signal.decode(msg.data.as_slice()).expect("手算解码信号");
            let entry = out
                .entry((channel, message.name.clone(), signal.name.clone()))
                .or_insert((0, f64::INFINITY, f64::NEG_INFINITY));
            entry.0 += 1;
            if value < entry.1 {
                entry.1 = value;
            }
            if value > entry.2 {
                entry.2 = value;
            }
        }
    }
    out
}

/// Step 15 集成 1：`multi_channel.blf` + `demo.dbc`。
///
/// * `series` 必须含 `VehSpd/Spd`、`BattTemp/Temp1`、`Insulation/Res`；
/// * 每条信号的 `count` 必须等于 `scan` 报的该报文 `frame_count`；
/// * `vmin`/`vmax` 必须等于逐帧解码手算的结果（1e-9 容差）。
#[test]
fn step15_series_multi_channel_matches_scan_and_hand_decode() {
    let fixtures = repo_fixtures_dir();
    let blf = fixtures.join("multi_channel.blf");
    let dbc = fixtures.join("demo.dbc");
    assert!(
        blf.exists() && dbc.exists(),
        "缺少 fixtures（multi_channel.blf / demo.dbc）：{}",
        fixtures.display()
    );
    let bindings = [(1u32, dbc.clone()), (2u32, dbc.clone())];

    // scan 的 frame_count（复用 scan_file 的真实实现）。
    let dbc_map: BTreeMap<u32, PathBuf> = bindings.iter().cloned().collect();
    let plan = DecodePlan::build(&dbc_map, &Selection::default()).expect("构建计划");
    let scanned = fast_blf_core::scan_file(&blf, &plan, &|_p| {});
    assert!(scanned.ok, "scan 应成功：{:?}", scanned.error);

    let hand = hand_decode_stats(&blf, &bindings);

    with_python(|py| -> PyResult<()> {
        let config = series_config(py, &blf, &bindings, "relative_s", 2000)?;
        let out = fast_blf_core::series_for_plot(py, &config)?;
        assert!(out.get_item("ok")?.unwrap().extract::<bool>()?);

        let rows = series_rows(&out)?;
        // demo.dbc 的 5 条信号（VehSpd: Spd/SpdValid、BattTemp: Temp1/Temp2、Insulation: Res）
        // 在 multi_channel.blf 里都有采样，不能被静默丢掉。
        assert_eq!(rows.len(), 5, "应有 5 条有采样的信号：{rows:?}",);
        let find = |message: &str, signal: &str| {
            rows.iter()
                .find(|r| r.message == message && r.signal == signal)
                .unwrap_or_else(|| panic!("series 应含 {message}/{signal}"))
        };

        for (message, signal) in [
            ("VehSpd", "Spd"),
            ("BattTemp", "Temp1"),
            ("Insulation", "Res"),
        ] {
            let row = find(message, signal);
            let &(hand_count, hand_vmin, hand_vmax) = hand
                .get(&(
                    row_channel(message),
                    message.to_string(),
                    signal.to_string(),
                ))
                .unwrap_or_else(|| panic!("手算应含 {message}/{signal}"));
            assert_eq!(
                row.count, hand_count,
                "{message}/{signal} 的 count 必须等于手算原始点数"
            );

            // scan 的 frame_count 是报文维度：同报文每帧对该信号贡献一个采样点。
            let scanned_count = scanned
                .frame_counts
                .get(&(row_channel(message), frame_id_of(message)))
                .copied()
                .unwrap_or(0);
            assert_eq!(
                row.count, scanned_count,
                "{message}/{signal} 的 count 必须等于 scan 的 frame_count"
            );
            assert!(
                (row.vmin - hand_vmin).abs() <= 1e-9,
                "{message}/{signal} vmin={} 手算={hand_vmin}",
                row.vmin
            );
            assert!(
                (row.vmax - hand_vmax).abs() <= 1e-9,
                "{message}/{signal} vmax={} 手算={hand_vmax}",
                row.vmax
            );
            // count 来自全量原始数据，而不是桶数。
            assert!(row.t_len <= 2000, "返回点数必须 ≤ buckets");
            assert!(
                row.t_len as u64 <= row.count,
                "非空桶数不可能超过原始采样点数"
            );
        }

        // 定值抽查：Spd 第 i 帧原始值 = i，物理值 = i × 0.01。
        let spd = find("VehSpd", "Spd");
        assert_eq!(spd.count, 500);
        assert!((spd.vmin - 0.0).abs() <= 1e-9);
        assert!((spd.vmax - 4.99).abs() <= 1e-9);
        assert!((spd.first - 0.0).abs() <= 1e-9);
        assert!((spd.last - 4.99).abs() <= 1e-9);
        let temp1 = find("BattTemp", "Temp1");
        assert_eq!(temp1.count, 50);
        assert!((temp1.vmin - (-10.0)).abs() <= 1e-9);
        assert!((temp1.vmax - (-10.0)).abs() <= 1e-9);
        assert!((temp1.first - (-10.0)).abs() <= 1e-9);
        assert!((temp1.last - (-10.0)).abs() <= 1e-9);
        let res = find("Insulation", "Res");
        assert_eq!(res.count, 5);
        assert!((res.vmin - 0.0).abs() <= 1e-9);
        assert!((res.vmax - 4.0).abs() <= 1e-9);
        assert!((res.first - 0.0).abs() <= 1e-9);
        assert!((res.last - 4.0).abs() <= 1e-9);
        // 恒为 0 的信号也必须保留（count 仍然来自全量原始数据）。
        let spd_valid = find("VehSpd", "SpdValid");
        assert_eq!(spd_valid.count, 500);
        assert!((spd_valid.vmin - 0.0).abs() <= 1e-9);
        assert!((spd_valid.vmax - 0.0).abs() <= 1e-9);
        let temp2 = find("BattTemp", "Temp2");
        assert_eq!(temp2.count, 50);
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// 已知 `demo.dbc` 报文名 → 通道号。
fn row_channel(message: &str) -> u32 {
    match message {
        "VehSpd" | "BattTemp" => 1,
        "Insulation" => 2,
        other => panic!("未知报文 {other}"),
    }
}

fn frame_id_of(message: &str) -> u32 {
    match message {
        "VehSpd" => 0x123,
        "BattTemp" => 0x1A2,
        "Insulation" => 0x200,
        other => panic!("未知报文 {other}"),
    }
}

/// Step 15 集成 2：`timefmt.blf` + `timefmt.dbc`，`time_format=absolute`。
///
/// 说明：任务书写的是 `fixtures/demo.dbc`，但 `demo.dbc` 里没有 `timefmt.blf` 的报文
/// `TimeMsg(0x300)`，用它永远解不出数据、也就没有 `t[0]`；正确的绑定是 `fixtures/timefmt.dbc`。
#[test]
fn step15_series_timefmt_absolute() {
    let fixtures = repo_fixtures_dir();
    let blf = fixtures.join("timefmt.blf");
    let dbc = fixtures.join("timefmt.dbc");
    assert!(
        blf.exists() && dbc.exists(),
        "缺少 fixtures（timefmt.blf / timefmt.dbc）：{}",
        fixtures.display()
    );
    let bindings = [(1u32, dbc.clone())];

    with_python(|py| -> PyResult<()> {
        let config = series_config(py, &blf, &bindings, "absolute", 2000)?;
        let out = fast_blf_core::series_for_plot(py, &config)?;

        let t_min: f64 = out.get_item("t_min")?.unwrap().extract()?;
        let t_max: f64 = out.get_item("t_max")?.unwrap().extract()?;
        assert!((t_min - 0.0).abs() < 1e-12, "absolute 的 t_min 应为 0");
        assert!((t_max - 1.0).abs() < 1e-9, "t_max={t_max}");

        let t_min_str: String = out
            .get_item("t_min_str")?
            .expect("absolute 必给 t_min_str")
            .extract()?;
        assert_eq!(t_min_str, "2025-10-16 14:10:00.123456");
        let t_max_str: String = out
            .get_item("t_max_str")?
            .expect("absolute 必给 t_max_str")
            .extract()?;
        assert_eq!(t_max_str, "2025-10-16 14:10:01.123456");

        let rows = series_rows(&out)?;
        assert_eq!(rows.len(), 1, "timefmt 只有一条信号");
        let row = &rows[0];
        assert_eq!(row.count, 101);

        // 契约 §1.3：t = 桶中心 = t_min + (idx + 0.5) * 桶宽；桶0 中心 = 0.5 * 1.0/2000。
        // （任务书集成测试写的是 `t[0] == 0.0`，与 §1.3 的"桶中心"公式相互矛盾：
        //   t_min 为 0 时桶0 中心必然是半个桶宽。这里按冻结契约的桶中心公式断言，
        //   同时断言 t_min == 0.0 作为"横轴从最早时间开始"的口径。）
        let list = out
            .get_item("series")?
            .unwrap()
            .cast_into::<PyList>()?;
        let first = list.get_item(0)?.cast_into::<PyDict>()?;
        let t = first
            .get_item("t")?
            .unwrap()
            .cast_into::<PyList>()?;
        let t0: f64 = t.get_item(0)?.extract()?;
        assert!(
            (t0 - 0.00025).abs() < 1e-12,
            "桶0 中心应为 0.5 * (1.0/2000) = 0.00025，实际 {t0}"
        );

        let axis = out
            .get_item("t_axis")?
            .expect("absolute 必给 t_axis")
            .cast_into::<PyList>()?;
        assert!(
            (8..=12).contains(&axis.len()),
            "t_axis 刻度数应在 8..12，实际 {}",
            axis.len()
        );
        let mut labels = Vec::new();
        let mut ticks = Vec::new();
        for item in axis.iter() {
            let d = item.cast_into::<PyDict>()?;
            ticks.push(d.get_item("t")?.unwrap().extract::<f64>()?);
            labels.push(d.get_item("label")?.unwrap().extract::<String>()?);
        }
        // 等间隔：首 0、末 t_max，相邻间隔一致。
        assert!((ticks[0] - 0.0).abs() < 1e-12);
        assert!((ticks[ticks.len() - 1] - 1.0).abs() < 1e-9);
        let step = ticks[1] - ticks[0];
        for pair in ticks.windows(2) {
            assert!((pair[1] - pair[0] - step).abs() < 1e-9, "刻度必须等间隔");
        }
        // label 可复现：跨度 1s < 2s → 微秒级，逐刻度 = 首帧墙钟 + t。
        assert_eq!(labels[0], "2025-10-16 14:10:00.123456");
        assert_eq!(
            labels[labels.len() - 1],
            "2025-10-16 14:10:01.123456"
        );
        assert_eq!(labels[1], "2025-10-16 14:10:00.234567");
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// 粗粒度当前进程 RSS（KB）。macOS/Linux 的 `ps` 都支持 `-o rss=`；失败返回 `None`。
fn current_rss_kb() -> Option<u64> {
    let pid = std::process::id().to_string();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// 从 fixtures 的 `*.expected.json` 读基准总帧数（避免为计时再扫一遍文件）。
fn expected_total_frames(fixtures: &Path, blf: &Path) -> Option<u64> {
    let stem = blf.file_stem()?.to_string_lossy().to_string();
    let text = std::fs::read_to_string(fixtures.join(format!("{stem}.expected.json"))).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("total_frames")?.as_u64()
}

/// Step 15 性能自证（`--ignored` 手动运行，避免 debug 下拖慢常规 `cargo test`）。
///
/// 命令：
/// `cargo test --release -p fast_blf_core --test e2e_synthetic -- --ignored --nocapture step15_series_bench_huge_perf`
///
/// 验收线：187MB `bench_huge.blf` 上 `series_for_plot(buckets=2000)` ≤ 15 秒，
/// 且返回点数 ≤ `2000 × 信号数`、远小于原始采样点数。
#[test]
#[ignore = "性能自证：需 release + 大文件，手动用 --ignored --nocapture 运行"]
fn step15_series_bench_huge_perf() {
    let fixtures = repo_fixtures_dir();
    let huge = fixtures.join("bench_huge.blf");
    let large = fixtures.join("bench_large.blf");
    let blf = if huge.exists() { huge } else { large };
    let dbc = fixtures.join("bench.dbc");
    assert!(blf.exists(), "缺少 bench_huge.blf / bench_large.blf");
    assert!(dbc.exists(), "缺少 bench.dbc");
    let size_bytes = std::fs::metadata(&blf).map(|m| m.len()).unwrap_or(0);
    let expected_frames = expected_total_frames(&fixtures, &blf);

    // 22 条报文分散在 Ch1/Ch2/Ch3，全部绑同一份 bench.dbc。
    let bindings = [
        (1u32, dbc.clone()),
        (2u32, dbc.clone()),
        (3u32, dbc.clone()),
    ];

    with_python(|py| -> PyResult<()> {
        let config = series_config(py, &blf, &bindings, "relative_s", 2000)?;
        let rss_before = current_rss_kb();
        let started = std::time::Instant::now();
        let out = fast_blf_core::series_for_plot(py, &config)?;
        let elapsed = started.elapsed();
        let rss_after = current_rss_kb();

        let rows = series_rows(&out)?;
        let total_points: usize = rows.iter().map(|r| r.t_len).sum();
        let raw_points: u64 = rows.iter().map(|r| r.count).sum();
        let max_points = 2000usize.saturating_mul(rows.len());

        // 计时之后再做一次轻量扫描，得到权威的"实际总帧数"（不计入 series_for_plot 耗时）。
        let dbc_map: BTreeMap<u32, PathBuf> = bindings.iter().cloned().collect();
        let plan = DecodePlan::build(&dbc_map, &Selection::default()).expect("构建计划");
        let planned_signals: usize = plan.messages().map(|m| m.signals.len()).sum();
        let scanned = fast_blf_core::scan_file(&blf, &plan, &|_p| {});
        let actual_frames: u64 = scanned.frame_counts.values().sum();
        let warning_count = out
            .get_item("warnings")?
            .expect("返回值必须含 warnings")
            .cast_into::<PyList>()?
            .len();

        println!("=== Step 15 series_for_plot 性能自证 ===");
        println!("文件              : {} ({} 字节)", blf.display(), size_bytes);
        println!(
            "总帧数(实际 scan) : {actual_frames}（expected.json={}）",
            expected_frames
                .map(|n| n.to_string())
                .unwrap_or_else(|| "未找到".to_string())
        );
        println!(
            "信号数            : {} 有采样 / {} DBC 定义",
            rows.len(),
            planned_signals
        );
        println!("warnings 条数     : {warning_count}");
        println!("总耗时            : {:.3} s", elapsed.as_secs_f64());
        match (rss_before, rss_after) {
            (Some(before), Some(after)) => {
                println!("峰值内存(粗粒度)  : before={before} KB / after={after} KB");
            }
            _ => {
                // 本机沙箱禁止 `ps`（Operation not permitted），无法读 RSS；给理论上界。
                // 解码期累加器按**全部计划信号**分配：信号数 × buckets × 32B（4 个 f64/u64）。
                let accum_bytes = planned_signals * 2000 * 32;
                println!(
                    "峰值内存          : 无法测量（沙箱禁用 `ps`）；解码期累加器理论上界 = {planned_signals}×2000×32B ≈ {:.1} MB",
                    accum_bytes as f64 / (1024.0 * 1024.0)
                );
            }
        }
        println!("原始采样点数(信号和): {raw_points}");
        println!(
            "返回给 Python 的总点数: {total_points} (上限 2000×{}={max_points})",
            rows.len()
        );
        println!(
            "抽稀比            : {:.6}% (返回/原始)",
            if raw_points == 0 {
                0.0
            } else {
                100.0 * total_points as f64 / raw_points as f64
            }
        );

        assert!(
            total_points <= max_points,
            "返回点数 {total_points} 必须 ≤ 2000 × 信号数 {max_points}"
        );
        assert!(
            (total_points as u64) < raw_points,
            "返回点数必须远小于原始采样点数"
        );
        assert!(
            elapsed.as_secs_f64() <= 15.0,
            "187MB 文件 series_for_plot 必须 ≤ 15s，实际 {:.3}s",
            elapsed.as_secs_f64()
        );
        Ok(())
    })
    .expect("Python 侧断言失败");
}



// ---------------------------------------------------------------------------
// Step 17：帧缓存 + PlotSession
// ---------------------------------------------------------------------------

const STEP17_DBC: &str = r#"VERSION ""

BO_ 256 MsgA: 8 Vector__XXX
 SG_ A : 0|16@1+ (1,0) [0|65535] "" Vector__XXX

BO_ 512 MsgB: 8 Vector__XXX
 SG_ B : 0|16@1+ (1,0) [0|65535] "" Vector__XXX
"#;

const STEP17_FRAMES: u32 = 60_000;
const STEP17_SPIKE_FRAME: u32 = 30_000;
const STEP17_SPIKE_VALUE: u16 = 65_000;

/// 生成 Step 17 合成现场：Ch1 MsgA 每 1ms 一帧（A 的波形 + 单个尖峰），
/// 同一时刻每 4 帧插一帧 Ch1 MsgB。返回 `(blf, dbc)`。
fn write_step17_fixture(dir: &Path) -> (PathBuf, PathBuf) {
    let blf = dir.join("step17.blf");
    let dbc = dir.join("step17.dbc");
    std::fs::write(&dbc, STEP17_DBC).expect("写入 Step 17 DBC");

    let mut writer = blf_asc::BlfWriter::create(&blf).expect("创建 Step 17 BLF");
    for i in 0..STEP17_FRAMES {
        let timestamp = i as f64 * 0.001;
        let value = if i == STEP17_SPIKE_FRAME {
            STEP17_SPIKE_VALUE
        } else {
            ((i.wrapping_mul(37)) % 1_000) as u16
        };
        let mut data = vec![0u8; 8];
        data[0..2].copy_from_slice(&value.to_le_bytes());
        push_msg(&mut writer, 0, 0x100, timestamp, data);

        if i % 4 == 0 {
            let b = ((i / 4) % 1_000) as u16;
            let mut data = vec![0u8; 8];
            data[0..2].copy_from_slice(&b.to_le_bytes());
            push_msg(&mut writer, 0, 0x200, timestamp, data);
        }
    }
    writer.finish().expect("完成 Step 17 BLF");
    (blf, dbc)
}

/// 组装 `open_plot_session` 的 config。
fn step17_config<'py>(
    py: Python<'py>,
    blf: &Path,
    dbc: &Path,
    selected: Option<(&str, &str)>,
    cache_dir: &Path,
    budget_mb: u64,
    time_format: &str,
) -> PyResult<Bound<'py, PyDict>> {
    let config = PyDict::new(py);
    config.set_item("blf_paths", vec![blf.to_string_lossy().to_string()])?;
    let mut dbc_map = HashMap::new();
    dbc_map.insert(1u32, dbc.to_string_lossy().to_string());
    config.set_item("dbc_by_channel", dbc_map)?;
    let selected_map: HashMap<String, HashMap<String, Vec<String>>> = match selected {
        Some((message, signal)) => {
            let mut per_channel = HashMap::new();
            per_channel.insert(message.to_string(), vec![signal.to_string()]);
            let mut all = HashMap::new();
            all.insert("1".to_string(), per_channel);
            all
        }
        None => HashMap::new(),
    };
    config.set_item("selected", selected_map)?;
    config.set_item("time_format", time_format)?;
    config.set_item("temp_dir", cache_dir.to_string_lossy().to_string())?;
    config.set_item("cache_dir", cache_dir.to_string_lossy().to_string())?;
    config.set_item("cache_budget_mb", budget_mb)?;
    Ok(config)
}

/// `view()` 的 Rust 侧投影。
struct Step17View {
    t: Vec<f64>,
    series: BTreeMap<String, (Vec<f64>, Vec<f64>, Vec<f64>)>,
}

fn step17_view(dict: &Bound<'_, PyDict>) -> PyResult<Step17View> {
    let t = dict
        .get_item("t")?
        .expect("view 必须含 t")
        .cast_into::<PyList>()?
        .iter()
        .map(|item| item.extract::<f64>())
        .collect::<PyResult<Vec<_>>>()?;
    let mut series = BTreeMap::new();
    for item in dict
        .get_item("series")?
        .expect("view 必须含 series")
        .cast_into::<PyList>()?
        .iter()
    {
        let dict = item.cast_into::<PyDict>()?;
        let key: String = dict.get_item("key")?.unwrap().extract()?;
        let list = |name: &str| -> PyResult<Vec<f64>> {
            dict.get_item(name)?
                .expect("series 元素缺字段")
                .cast_into::<PyList>()?
                .iter()
                .map(|value| value.extract::<f64>())
                .collect()
        };
        series.insert(key, (list("min")?, list("max")?, list("mean")?));
    }
    Ok(Step17View { t, series })
}

fn step17_cache_key_dir(cache_dir: &Path) -> PathBuf {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(cache_dir)
        .expect("读取缓存目录")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    assert_eq!(dirs.len(), 1, "应恰好一个缓存 key 目录：{dirs:?}");
    dirs.remove(0)
}

/// 直接逐帧解码 MsgA 的 `A` 信号（独立于缓存）。
fn step17_direct_samples(blf: &Path, dbc: &Path) -> Vec<(f64, f64)> {
    let mut dbc_map = BTreeMap::new();
    dbc_map.insert(1u32, dbc.to_path_buf());
    let plan = DecodePlan::build(&dbc_map, &Selection::default()).expect("构建直接解码计划");
    let message = plan.lookup(1, 0x100).expect("MsgA 计划");
    let signal = message
        .signals
        .iter()
        .find(|signal| signal.name == "A")
        .expect("A 信号");
    let mut reader = blf_asc::BlfReader::open(blf).expect("打开 BLF 手算");
    let mut out = Vec::new();
    for frame in reader.by_ref() {
        let channel = frame.channel as u32 + 1;
        let frame_id = frame.arbitration_id.0 | if frame.is_extended_id { 1 << 31 } else { 0 };
        if channel == 1 && frame_id == 0x100 {
            let value = signal.decode(frame.data.as_slice()).expect("手算解码 A");
            out.push((frame.timestamp, value));
        }
    }
    out
}

/// 1）缓存一致性：缓存段逐帧解码 == 直接 BLF 解码，1e-12。
#[test]
fn step17_cache_matches_direct_decode_byte_for_byte() {
    let dir = tempfile::tempdir().expect("临时目录");
    let (blf, dbc) = write_step17_fixture(dir.path());
    let cache_dir = dir.path().join("cache");
    with_python(|py| -> PyResult<()> {
        let config = step17_config(py, &blf, &dbc, None, &cache_dir, 200, "relative_s")?;
        let session = fast_blf_core::open_plot_session(py, &config, None)?;
        let keys = ["ch1/MsgA/A", "ch1/MsgB/B"];
        let added = session.add_signals(py, PyList::new(py, keys)?.as_any())?;
        assert_eq!(added.get_item("added")?.unwrap().extract::<u64>()?, 2);

        let key_dir = step17_cache_key_dir(&cache_dir);
        let meta = fast_blf_core::frame_cache::read_meta(&key_dir).expect("读取 meta");
        let message = meta.message(1, 0x100).expect("meta 应含 MsgA");
        assert_eq!(message.stride, 17, "MsgA 是经典 CAN，stride=17");
        assert_eq!(message.frames, STEP17_FRAMES as u64);

        let mut dbc_map = BTreeMap::new();
        dbc_map.insert(1u32, dbc.clone());
        let plan = DecodePlan::build(&dbc_map, &Selection::default()).expect("构建计划");
        let signal = plan
            .lookup(1, 0x100)
            .and_then(|message| message.signals.iter().find(|signal| signal.name == "A"))
            .expect("A 信号计划");

        let mut cached = Vec::new();
        for index in 0..message.frames {
            let record = fast_blf_core::frame_cache::read_record_at(&key_dir, message, index)?;
            cached.push(fast_blf_core::decode_signal_bytes(
                signal,
                record.dlc,
                &record.payload[..record.data_len],
            )?);
        }
        let direct: Vec<f64> = step17_direct_samples(&blf, &dbc)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        assert_eq!(cached.len(), direct.len(), "缓存帧数必须与直接解码一致");
        for (index, (left, right)) in cached.iter().zip(direct.iter()).enumerate() {
            assert!(
                (left - right).abs() <= 1e-12,
                "第 {index} 帧 cache={left} direct={right}"
            );
        }
        println!(
            "STEP17_CACHE_CONSISTENCY ok frames={} stride={} cache_bytes={}",
            cached.len(),
            message.stride,
            meta.cache_bytes
        );
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// 2）缓存命中：第二次 open 必须 `cache_hit=true`，且不重新写帧缓存。
#[test]
fn step17_second_open_hits_cache() {
    let dir = tempfile::tempdir().expect("临时目录");
    let (blf, dbc) = write_step17_fixture(dir.path());
    let cache_dir = dir.path().join("cache");
    with_python(|py| -> PyResult<()> {
        let config = step17_config(py, &blf, &dbc, None, &cache_dir, 200, "relative_s")?;
        let first_started = std::time::Instant::now();
        let first = fast_blf_core::open_plot_session(py, &config, None)?;
        let first_elapsed = first_started.elapsed();
        let first_info = first.info(py)?;
        assert!(!first_info.get_item("cache_hit")?.unwrap().extract::<bool>()?);

        let second_started = std::time::Instant::now();
        let second = fast_blf_core::open_plot_session(py, &config, None)?;
        let second_elapsed = second_started.elapsed();
        let second_info = second.info(py)?;
        assert!(
            second_info.get_item("cache_hit")?.unwrap().extract::<bool>()?,
            "第二次 open 必须命中缓存"
        );
        let first_bytes: u64 = first_info
            .get_item("cache_bytes")?
            .unwrap()
            .extract()?;
        let second_bytes: u64 = second_info
            .get_item("cache_bytes")?
            .unwrap()
            .extract()?;
        assert_eq!(first_bytes, second_bytes);
        println!(
            "STEP17_CACHE_HIT first={:.3}ms second={:.3}ms cache_bytes={}",
            first_elapsed.as_secs_f64() * 1000.0,
            second_elapsed.as_secs_f64() * 1000.0,
            second_bytes
        );
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// 3）预算拒绝：187MB 场景 + 32 MiB 上限 → `CACHE_BUDGET_EXCEEDED`，消息含三个动作。
#[test]
#[ignore = "需要扫描 187MB bench_huge.blf；用 --ignored --nocapture 单独运行"]
fn step17_budget_rejects_bench_huge() {
    let fixtures = repo_fixtures_dir();
    let huge = fixtures.join("bench_huge.blf");
    let dbc = fixtures.join("bench.dbc");
    if !huge.exists() {
        println!("STEP17_BUDGET_SKIP bench_huge.blf 不存在");
        return;
    }
    let dir = tempfile::tempdir().expect("临时目录");
    let cache_dir = dir.path().join("cache");
    with_python(|py| -> PyResult<()> {
        let config = PyDict::new(py);
        config.set_item("blf_paths", vec![huge.to_string_lossy().to_string()])?;
        let mut dbc_map = HashMap::new();
        for channel in [1u32, 2, 3] {
            dbc_map.insert(channel, dbc.to_string_lossy().to_string());
        }
        config.set_item("dbc_by_channel", dbc_map)?;
        config.set_item(
            "selected",
            HashMap::<String, HashMap<String, Vec<String>>>::new(),
        )?;
        config.set_item("time_format", "relative_s")?;
        config.set_item("temp_dir", cache_dir.to_string_lossy().to_string())?;
        config.set_item("cache_dir", cache_dir.to_string_lossy().to_string())?;
        config.set_item("cache_budget_mb", 32u64)?;
        let err = match fast_blf_core::open_plot_session(py, &config, None) {
            Ok(_) => panic!("32 MiB 预算必须拒绝 187MB 场景"),
            Err(err) => err,
        };
        let text = err.to_string();
        assert!(
            text.contains("CACHE_BUDGET_EXCEEDED"),
            "错误码不对：{text}"
        );
        assert!(text.contains("需要 "), "缺少需要多少 MiB：{text}");
        assert!(text.contains("上限 32 MiB"), "缺少上限：{text}");
        assert!(text.contains("①提高上限(cache_budget_mb)"), "{text}");
        assert!(text.contains("②减少所选报文"), "{text}");
        assert!(text.contains("③不建缓存继续但较慢"), "{text}");
        println!("STEP17_BUDGET_REJECT ok {text}");
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// 4）包络正确性（红线）：随机 200 个窗口，逐时间桶与暴力逐帧 min/max 比对；
/// 并构造“窗口内只有一个尖峰”，断言尖峰进入包络。
#[test]
fn step17_envelope_matches_brute_force_per_bucket() {
    let dir = tempfile::tempdir().expect("临时目录");
    let (blf, dbc) = write_step17_fixture(dir.path());
    let cache_dir = dir.path().join("cache");
    let samples = step17_direct_samples(&blf, &dbc);
    assert_eq!(samples.len(), STEP17_FRAMES as usize);

    with_python(|py| -> PyResult<()> {
        let config = step17_config(py, &blf, &dbc, None, &cache_dir, 200, "relative_s")?;
        let session = fast_blf_core::open_plot_session(py, &config, None)?;
        session.add_signals(py, PyList::new(py, ["ch1/MsgA/A"])?.as_any())?;

        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64) / ((1u64 << 53) as f64)
        };
        let width = 200usize;
        for round in 0..200 {
            let start = next() * 55.0;
            let span = 0.001 + next() * 4.0;
            let t0 = start;
            let t1 = (start + span).min(59.999);
            if t1 <= t0 {
                continue;
            }
            let view = step17_view(&session.view(py, t0, t1, width)?)?;
            let Some((mins, maxs, _)) = view.series.get("ch1/MsgA/A") else {
                panic!("第 {round} 轮缺 MsgA/A 曲线");
            };
            let bucket_span = (t1 - t0) / width as f64;
            let mut brute: Vec<(f64, f64)> = Vec::new();
            for bucket in 0..width {
                let bucket_start = t0 + bucket_span * bucket as f64;
                let bucket_next = t0 + bucket_span * (bucket + 1) as f64;
                let mut min = f64::INFINITY;
                let mut max = f64::NEG_INFINITY;
                for &(t, value) in &samples {
                    let inside = if bucket + 1 == width {
                        t >= bucket_start && t <= bucket_next
                    } else {
                        t >= bucket_start && t < bucket_next
                    };
                    if inside {
                        min = min.min(value);
                        max = max.max(value);
                    }
                }
                if min.is_finite() && max.is_finite() {
                    brute.push((min, max));
                }
            }
            assert_eq!(
                mins.len(),
                brute.len(),
                "第 {round} 轮桶数不一致（view={} brute={}）",
                mins.len(),
                brute.len()
            );
            assert_eq!(view.t.len(), brute.len());
            for (index, ((min, max), (brute_min, brute_max))) in
                mins.iter().zip(maxs.iter()).zip(brute.iter()).enumerate()
            {
                let tolerance = 1e-5 * brute_min.abs().max(1.0);
                assert!(
                    (min - brute_min).abs() <= tolerance,
                    "第 {round} 轮桶 {index} min: view={min} brute={brute_min}"
                );
                let tolerance = 1e-5 * brute_max.abs().max(1.0);
                assert!(
                    (max - brute_max).abs() <= tolerance,
                    "第 {round} 轮桶 {index} max: view={max} brute={brute_max}"
                );
                assert!(min <= max, "包络 min 必须 ≤ max");
            }
        }

        // 窗口内只有一个尖峰。
        let spike_t = STEP17_SPIKE_FRAME as f64 * 0.001;
        let view = step17_view(&session.view(py, spike_t - 0.01, spike_t + 0.01, 200)?)?;
        let (_, maxs, _) = view.series.get("ch1/MsgA/A").expect("尖峰窗口缺曲线");
        let peak = maxs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        assert!(
            peak >= STEP17_SPIKE_VALUE as f64 - 1e-5,
            "尖峰必须进入包络，实际 max={peak}"
        );
        println!(
            "STEP17_ENVELOPE ok windows=200 width={width} spike_peak={peak}"
        );
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// 5）金字塔层选择 + 点数上限：200/800/2000 px 都 ≤2×width。
#[test]
fn step17_view_point_counts_are_bounded_by_width() {
    let dir = tempfile::tempdir().expect("临时目录");
    let (blf, dbc) = write_step17_fixture(dir.path());
    let cache_dir = dir.path().join("cache");
    with_python(|py| -> PyResult<()> {
        let config = step17_config(py, &blf, &dbc, None, &cache_dir, 200, "relative_s")?;
        let session = fast_blf_core::open_plot_session(py, &config, None)?;
        session.add_signals(py, PyList::new(py, ["ch1/MsgA/A"])?.as_any())?;
        for width in [200usize, 800, 2000] {
            let view = step17_view(&session.view(py, 0.0, 59.999, width)?)?;
            let (mins, maxs, means) = view.series.get("ch1/MsgA/A").expect("缺曲线");
            assert_eq!(mins.len(), maxs.len());
            assert_eq!(mins.len(), means.len());
            assert!(
                mins.len() <= 2 * width,
                "width={width} 返回 {} 点，超过 2×width",
                mins.len()
            );
            assert_eq!(view.t.len(), mins.len(), "t 与包络数组必须等长");
            println!(
                "STEP17_POINT_COUNT width={width} points={} truncated={}",
                mins.len(),
                view.t.len()
            );
        }
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// 6）时间口径零成本：set_time_format 只改显示层；min/max 数组完全一致且 ≤16ms。
#[test]
fn step17_set_time_format_is_zero_cost_and_keeps_min_max() {
    let dir = tempfile::tempdir().expect("临时目录");
    let (blf, dbc) = write_step17_fixture(dir.path());
    let cache_dir = dir.path().join("cache");
    with_python(|py| -> PyResult<()> {
        let config = step17_config(py, &blf, &dbc, None, &cache_dir, 200, "relative_s")?;
        let session = fast_blf_core::open_plot_session(py, &config, None)?;
        session.add_signals(py, PyList::new(py, ["ch1/MsgA/A"])?.as_any())?;
        let before = step17_view(&session.view(py, 0.0, 59.999, 500)?)?;
        let started = std::time::Instant::now();
        session.set_time_format("absolute")?;
        let elapsed = started.elapsed();
        let after = step17_view(&session.view(py, 0.0, 59.999, 500)?)?;
        assert!(
            elapsed.as_millis() <= 16,
            "set_time_format 耗时 {:?}，超过 16ms",
            elapsed
        );
        assert_eq!(before.t, after.t);
        let a = before.series.get("ch1/MsgA/A").expect("before 缺曲线");
        let b = after.series.get("ch1/MsgA/A").expect("after 缺曲线");
        assert_eq!(a.0, b.0, "min 数组必须完全一致");
        assert_eq!(a.1, b.1, "max 数组必须完全一致");
        let info = session.info(py)?;
        // 该合成 BLF 的头部墙钟为 0（BlfWriter 默认），absolute 字符串允许为 None；
        // 真正有墙钟的夹具在 step17_timefmt_absolute_info 里单独断言。
        let t_min_str = info
            .get_item("t_min_str")?
            .and_then(|value| value.extract::<String>().ok());
        println!(
            "STEP17_TIMEFORMAT ok set_time_format={:.3}ms points={} t_min_str={t_min_str:?}",
            elapsed.as_secs_f64() * 1000.0,
            a.0.len()
        );
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// 6b）`absolute` 口径的 info 必须给出 BLF 头墙钟字符串（用 timefmt 夹具）。
#[test]
fn step17_timefmt_absolute_info() {
    let fixtures = repo_fixtures_dir();
    let blf = fixtures.join("timefmt.blf");
    let dbc = fixtures.join("timefmt.dbc");
    if !blf.exists() || !dbc.exists() {
        println!("STEP17_TIMEFMT_SKIP 缺少 timefmt 夹具");
        return;
    }
    let dir = tempfile::tempdir().expect("临时目录");
    let cache_dir = dir.path().join("cache");
    with_python(|py| -> PyResult<()> {
        let config = step17_config(py, &blf, &dbc, None, &cache_dir, 200, "absolute")?;
        let session = fast_blf_core::open_plot_session(py, &config, None)?;
        let info = session.info(py)?;
        let t_min_str: String = info
            .get_item("t_min_str")?
            .expect("absolute 必须给 t_min_str")
            .extract()?;
        let t_max_str: String = info
            .get_item("t_max_str")?
            .expect("absolute 必须给 t_max_str")
            .extract()?;
        assert_eq!(t_min_str, "2025-10-16 14:10:00.123456");
        assert_eq!(t_max_str, "2025-10-16 14:10:01.123456");
        println!("STEP17_TIMEFMT_ABSOLUTE ok {t_min_str} -> {t_max_str}");
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// 7）增量预取：初始只缓存 MsgA；add MsgB 后 pending>0，后台完成后 view 能出 B。
#[test]
fn step17_incremental_prefetch_makes_new_message_available() {
    let dir = tempfile::tempdir().expect("临时目录");
    let (blf, dbc) = write_step17_fixture(dir.path());
    let cache_dir = dir.path().join("cache");
    with_python(|py| -> PyResult<()> {
        let config = step17_config(
            py,
            &blf,
            &dbc,
            Some(("MsgA", "A")),
            &cache_dir,
            200,
            "relative_s",
        )?;
        let session = fast_blf_core::open_plot_session(py, &config, None)?;
        let a = session.add_signals(py, PyList::new(py, ["ch1/MsgA/A"])?.as_any())?;
        assert_eq!(a.get_item("pending")?.unwrap().extract::<u64>()?, 0);
        let b = session.add_signals(py, PyList::new(py, ["ch1/MsgB/B"])?.as_any())?;
        let pending: u64 = b.get_item("pending")?.unwrap().extract()?;
        assert!(pending > 0, "未缓存报文必须返回 pending>0");

        let mut waited_ms = 0u64;
        loop {
            let status = session.prefetch_status(py)?;
            let busy: bool = status.get_item("busy")?.unwrap().extract()?;
            if !busy {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            waited_ms += 10;
            assert!(waited_ms < 10_000, "后台预取超时");
        }

        let info = session.info(py)?;
        let signals = info
            .get_item("signals")?
            .expect("info 必须含 signals")
            .cast_into::<PyList>()?;
        let mut b_cached = false;
        for item in signals.iter() {
            let dict = item.cast_into::<PyDict>()?;
            let message: String = dict.get_item("message")?.unwrap().extract()?;
            if message == "MsgB" {
                b_cached = dict.get_item("cached")?.unwrap().extract()?;
            }
        }
        assert!(b_cached, "预取完成后 info 应把 MsgB 标为 cached");

        let view = step17_view(&session.view(py, 0.0, 59.999, 500)?)?;
        let b_series = view.series.get("ch1/MsgB/B").expect("预取后 view 应含 B");
        assert!(!b_series.0.is_empty(), "预取后 B 的包络不能为空");
        println!(
            "STEP17_PREFETCH ok pending={pending} waited={waited_ms}ms b_points={}",
            b_series.0.len()
        );
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// 性能自证（`--ignored`）：
/// `cargo test --release -p fast_blf_core --test e2e_synthetic -- --ignored --nocapture step17_plot_session_perf`
#[test]
#[ignore = "性能自证：需 release + 187MB bench_huge，手动 --ignored --nocapture"]
fn step17_plot_session_perf() {
    let fixtures = repo_fixtures_dir();
    let huge = fixtures.join("bench_huge.blf");
    let large = fixtures.join("bench_large.blf");
    let blf = if huge.exists() { huge } else { large };
    let dbc = fixtures.join("bench.dbc");
    assert!(blf.exists(), "缺少 bench_huge.blf / bench_large.blf");
    assert!(dbc.exists(), "缺少 bench.dbc");
    let source_bytes = std::fs::metadata(&blf).map(|meta| meta.len()).unwrap_or(0);
    let dir = tempfile::tempdir().expect("临时目录");
    let cache_dir = dir.path().join("cache");
    with_python(|py| -> PyResult<()> {
        let config = PyDict::new(py);
        config.set_item("blf_paths", vec![blf.to_string_lossy().to_string()])?;
        let mut dbc_map = HashMap::new();
        for channel in [1u32, 2, 3] {
            dbc_map.insert(channel, dbc.to_string_lossy().to_string());
        }
        config.set_item("dbc_by_channel", dbc_map)?;
        config.set_item(
            "selected",
            HashMap::<String, HashMap<String, Vec<String>>>::new(),
        )?;
        config.set_item("time_format", "relative_s")?;
        config.set_item("temp_dir", cache_dir.to_string_lossy().to_string())?;
        config.set_item("cache_dir", cache_dir.to_string_lossy().to_string())?;
        config.set_item("cache_budget_mb", 200u64)?;

        let first_started = std::time::Instant::now();
        let first = fast_blf_core::open_plot_session(py, &config, None)?;
        let first_elapsed = first_started.elapsed();
        let info = first.info(py)?;
        let cache_bytes: u64 = info.get_item("cache_bytes")?.unwrap().extract()?;
        let cache_hit: bool = info.get_item("cache_hit")?.unwrap().extract()?;
        assert!(!cache_hit, "首次 open 不能是 cache_hit");
        let t_max: f64 = info.get_item("t_max")?.unwrap().extract()?;

        let second_started = std::time::Instant::now();
        let second = fast_blf_core::open_plot_session(py, &config, None)?;
        let second_elapsed = second_started.elapsed();
        let second_info = second.info(py)?;
        let second_hit: bool = second_info.get_item("cache_hit")?.unwrap().extract()?;
        assert!(second_hit, "第二次 open 必须 cache_hit=true");

        // 8 条已缓存信号（同一报文）；测 add_signals 与金字塔内存。
        let keys: Vec<String> = (0..8).map(|index| format!("ch1/MSG_100/Sig{index}")).collect();
        let add_started = std::time::Instant::now();
        let added = second.add_signals(py, PyList::new(py, &keys)?.as_any())?;
        let add_elapsed = add_started.elapsed();
        assert_eq!(added.get_item("pending")?.unwrap().extract::<u64>()?, 0);
        let info_after = second.info(py)?;
        let pyramid_bytes: u64 = info_after
            .get_item("pyramid_bytes")?
            .unwrap()
            .extract()?;
        // bench_huge 的 MSG_100 每信号 200_000 个采样点（per_channel_id 1:256 = 200000）。
        let per_signal_samples: u64 = 200_000;

        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64) / ((1u64 << 53) as f64)
        };
        let mut durations: Vec<f64> = Vec::with_capacity(100);
        for _ in 0..100 {
            let a = next() * t_max.max(0.001);
            let b = next() * t_max.max(0.001);
            let (t0, t1) = if a <= b { (a, b) } else { (b, a) };
            let started = std::time::Instant::now();
            let view = second.view(py, t0, t1, 2000)?;
            durations.push(started.elapsed().as_secs_f64() * 1000.0);
            let parsed = step17_view(&view)?;
            assert!(!parsed.series.is_empty(), "性能窗口必须至少有一条曲线");
        }
        durations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p50 = durations.get(durations.len() / 2).copied().unwrap_or(0.0);
        let p95 = durations
            .get((durations.len() as f64 * 0.95) as usize)
            .copied()
            .unwrap_or(0.0);
        let max = durations.last().copied().unwrap_or(0.0);

        let fmt_started = std::time::Instant::now();
        second.set_time_format("absolute")?;
        let fmt_elapsed = fmt_started.elapsed();

        let ratio = if second_elapsed.as_secs_f64() > 0.0 {
            first_elapsed.as_secs_f64() / second_elapsed.as_secs_f64()
        } else {
            f64::INFINITY
        };
        println!("=== Step 17 PlotSession 性能自证 ===");
        println!(
            "文件              : {} ({} 字节)",
            blf.display(),
            source_bytes
        );
        println!(
            "首次 open 建缓存  : {:.3} ms, cache_bytes={} ({:.1} MiB), 缓存/源={:.3}",
            first_elapsed.as_secs_f64() * 1000.0,
            cache_bytes,
            cache_bytes as f64 / (1024.0 * 1024.0),
            if source_bytes == 0 {
                0.0
            } else {
                cache_bytes as f64 / source_bytes as f64
            }
        );
        println!(
            "缓存命中二次 open : {:.3} ms (cache_hit={}), 首次/二次={:.1}×",
            second_elapsed.as_secs_f64() * 1000.0,
            second_hit,
            ratio
        );
        println!(
            "add_signals(8 条) : {:.3} ms, pyramid_bytes={} ({:.3} MiB), 每 1M 采样≈{:.3} MiB/信号",
            add_elapsed.as_secs_f64() * 1000.0,
            pyramid_bytes,
            pyramid_bytes as f64 / (1024.0 * 1024.0),
            pyramid_bytes as f64 / (8.0 * per_signal_samples as f64) * 1_000_000.0
                / (1024.0 * 1024.0)
        );
        println!(
            "100 次 view(2000px): p50={:.3} ms p95={:.3} ms max={:.3} ms",
            p50, p95, max
        );
        println!(
            "set_time_format   : {:.3} ms",
            fmt_elapsed.as_secs_f64() * 1000.0
        );

        assert!(
            second_elapsed.as_millis() <= 300,
            "缓存命中 open 必须 ≤300ms，实际 {:?}",
            second_elapsed
        );
        assert!(p95 <= 30.0, "view p95 必须 ≤30ms，实际 {p95:.3}ms");
        assert!(
            add_elapsed.as_millis() <= 100,
            "已缓存 add_signals 必须 ≤100ms，实际 {:?}",
            add_elapsed
        );
        assert!(
            fmt_elapsed.as_millis() <= 16,
            "set_time_format 必须 ≤16ms，实际 {:?}",
            fmt_elapsed
        );
        assert!(ratio >= 10.0, "首次/二次 open 必须 ≥10×，实际 {ratio:.1}×");
        Ok(())
    })
    .expect("性能自证断言失败");
}

/// 8）建缓存进度：phase=1、stage="cache"、done/total 文件维度、frames_done 单调。
#[test]
fn step17_cache_progress_uses_cache_stage_and_monotonic_frames() {
    use std::sync::Mutex;

    let dir = tempfile::tempdir().expect("临时目录");
    let (blf, dbc) = write_step17_fixture(dir.path());
    let cache_dir = dir.path().join("cache");
    let mut dbc_map = BTreeMap::new();
    dbc_map.insert(1u32, dbc);
    let config = fast_blf_core::plot_session::SessionConfig {
        blf_paths: vec![blf],
        dbc_by_channel: dbc_map,
        selection: Selection::default(),
        time_format: fast_blf_core::TimeFormat::RelativeS,
        cache_dir,
        cache_budget_mb: 200,
    };
    let events: Mutex<Vec<Progress>> = Mutex::new(Vec::new());
    let session =
        fast_blf_core::plot_session::open_session(config, &|payload: Progress| {
            events.lock().expect("进度事件锁").push(payload);
        })
        .expect("打开会话");
    drop(session);

    let events = events.into_inner().expect("取出进度事件");
    assert!(!events.is_empty(), "建缓存必须产生进度事件");
    let mut previous_frames = 0u64;
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.phase, 1, "第 {index} 个事件 phase 错误：{event:?}");
        assert_eq!(
            event.stage, "cache",
            "第 {index} 个事件 stage 必须是 cache：{event:?}"
        );
        assert!(event.done <= event.total, "done > total：{event:?}");
        assert!(
            event.frames_done >= previous_frames,
            "frames_done 必须单调：{event:?}"
        );
        previous_frames = event.frames_done;
    }
    let last = events.last().expect("至少一个事件");
    assert_eq!(last.done, 1, "文件完成时 done 必须为 1");
    assert_eq!(last.total, 1, "文件维度 total 必须为 1");
    assert!(last.frames_done >= STEP17_FRAMES as u64);
    println!(
        "STEP17_CACHE_PROGRESS ok events={} frames_done={}",
        events.len(),
        last.frames_done
    );
}

/// 9）CAN FD 段：报文首帧经典、后续出现 FD 时按 73 字节段写入，值仍逐帧一致。
#[test]
fn step17_fd_message_uses_73_byte_stride_and_round_trips() {
    let fixtures = repo_fixtures_dir();
    let blf = fixtures.join("multi_channel.blf");
    let dbc = fixtures.join("demo.dbc");
    assert!(blf.exists() && dbc.exists(), "缺少 multi_channel 夹具");
    let dir = tempfile::tempdir().expect("临时目录");
    let cache_dir = dir.path().join("cache");

    let mut dbc_map = BTreeMap::new();
    dbc_map.insert(2u32, dbc.clone());
    let mut per_channel = HashMap::new();
    per_channel.insert("Insulation".to_string(), Vec::<String>::new());
    let mut selection = Selection::default();
    selection.channels.insert(2, per_channel);

    let config = fast_blf_core::plot_session::SessionConfig {
        blf_paths: vec![blf.clone()],
        dbc_by_channel: dbc_map.clone(),
        selection: selection.clone(),
        time_format: fast_blf_core::TimeFormat::RelativeS,
        cache_dir: cache_dir.clone(),
        cache_budget_mb: 200,
    };
    let session = fast_blf_core::plot_session::open_session(config, &|_p| {}).expect("打开会话");
    drop(session);

    let key_dir = step17_cache_key_dir(&cache_dir);
    let meta = fast_blf_core::frame_cache::read_meta(&key_dir).expect("读取 meta");
    let message = meta.message(2, 0x200).expect("meta 应含 Insulation");
    assert_eq!(message.stride, 73, "出现 FD 帧后必须用 73 字节段");
    assert!(message.is_fd);
    assert_eq!(message.frames, 5);

    let plan = DecodePlan::build(&dbc_map, &selection).expect("构建计划");
    let signal = plan
        .lookup(2, 0x200)
        .and_then(|message| message.signals.iter().find(|signal| signal.name == "Res"))
        .expect("Res 信号");
    let mut cached = Vec::new();
    for index in 0..message.frames {
        let record = fast_blf_core::frame_cache::read_record_at(&key_dir, message, index)
            .expect("读取 FD 缓存记录");
        cached.push(
            fast_blf_core::decode_signal_bytes(
                signal,
                record.dlc,
                &record.payload[..record.data_len],
            )
            .expect("解码 FD 缓存记录"),
        );
    }
    let mut direct = Vec::new();
    let mut reader = blf_asc::BlfReader::open(&blf).expect("打开 BLF");
    for frame in reader.by_ref() {
        let channel = frame.channel as u32 + 1;
        let frame_id = frame.arbitration_id.0 | if frame.is_extended_id { 1 << 31 } else { 0 };
        if channel == 2 && frame_id == 0x200 {
            direct.push(signal.decode(frame.data.as_slice()).expect("直接解码 FD 帧"));
        }
    }
    assert_eq!(cached, direct, "FD 段逐帧值必须与直接解码一致");
    println!(
        "STEP17_FD ok stride={} frames={} values={:?}",
        message.stride, message.frames, cached
    );
}

/// 10）stats 用全量 f64 原始数据；remove_signals 只影响绘制。
#[test]
fn step17_stats_are_full_precision_and_remove_stops_drawing() {
    let dir = tempfile::tempdir().expect("临时目录");
    let (blf, dbc) = write_step17_fixture(dir.path());
    let cache_dir = dir.path().join("cache");
    let samples = step17_direct_samples(&blf, &dbc);
    let direct_count = samples.len() as u64;
    let direct_min = samples
        .iter()
        .map(|(_, value)| *value)
        .fold(f64::INFINITY, f64::min);
    let direct_max = samples
        .iter()
        .map(|(_, value)| *value)
        .fold(f64::NEG_INFINITY, f64::max);
    let direct_sum: f64 = samples.iter().map(|(_, value)| *value).sum();

    with_python(|py| -> PyResult<()> {
        let config = step17_config(py, &blf, &dbc, None, &cache_dir, 200, "relative_s")?;
        let session = fast_blf_core::open_plot_session(py, &config, None)?;
        session.add_signals(py, PyList::new(py, ["ch1/MsgA/A"])?.as_any())?;
        let stats = session.stats(py, PyList::new(py, ["ch1/MsgA/A"])?.as_any())?;
        let dict = stats.get_item(0)?.cast_into::<PyDict>()?;
        let count: u64 = dict.get_item("count")?.unwrap().extract()?;
        let vmin: f64 = dict.get_item("vmin")?.unwrap().extract()?;
        let vmax: f64 = dict.get_item("vmax")?.unwrap().extract()?;
        let mean: f64 = dict.get_item("mean")?.unwrap().extract()?;
        assert_eq!(count, direct_count);
        assert_eq!(vmin, direct_min);
        assert_eq!(vmax, direct_max);
        assert!((mean - direct_sum / direct_count as f64).abs() <= 1e-12);

        let removed = session.remove_signals(py, PyList::new(py, ["ch1/MsgA/A"])?.as_any())?;
        assert_eq!(removed.get_item("removed")?.unwrap().extract::<u64>()?, 1);
        let view = step17_view(&session.view(py, 0.0, 59.999, 500)?)?;
        assert!(view.series.is_empty(), "移除后不应再绘制该信号");
        println!(
            "STEP17_STATS ok count={count} vmin={vmin} vmax={vmax} mean={mean} removed={removed:?}"
        );
        Ok(())
    })
    .expect("Python 侧断言失败");
}

/// 11）export_html：单文件、离线自包含、≤8 MiB。
#[test]
fn step17_export_html_is_self_contained() {
    let dir = tempfile::tempdir().expect("临时目录");
    let (blf, dbc) = write_step17_fixture(dir.path());
    let cache_dir = dir.path().join("cache");
    let html = dir.path().join("report.html");
    with_python(|py| -> PyResult<()> {
        let config = step17_config(py, &blf, &dbc, None, &cache_dir, 200, "relative_s")?;
        let session = fast_blf_core::open_plot_session(py, &config, None)?;
        session.add_signals(py, PyList::new(py, ["ch1/MsgA/A"])?.as_any())?;
        let result = session.export_html(py, html.to_string_lossy().as_ref(), "Step 17 测试报告")?;
        let ok: bool = result.get_item("ok")?.unwrap().extract()?;
        let bytes: u64 = result.get_item("bytes")?.unwrap().extract()?;
        assert!(ok);
        assert!(bytes > 0);
        assert!(bytes <= 8 * 1024 * 1024);
        let text = std::fs::read_to_string(&html).expect("读取 HTML");
        assert!(text.starts_with("<!DOCTYPE html>"));
        assert!(text.contains("<script>"));
        assert!(text.contains("Step 17 测试报告"));
        assert!(!text.contains("http://"), "HTML 不得引用 CDN");
        assert!(!text.contains("https://"), "HTML 不得引用 CDN");
        assert_eq!(text.len() as u64, bytes);
        println!("STEP17_EXPORT_HTML ok bytes={bytes}");
        Ok(())
    })
    .expect("Python 侧断言失败");
}
