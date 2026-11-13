//! Step：CSV + 多信号宽表 + 50 Hz 重采样的安全网 / 分块导出守卫测试。
//!
//! 三个用例分别锁死：
//! 1. 超上限 / 溢出的 G → `BAD_CONFIG`，不 panic、不 abort；
//! 2. 跨 `CHUNK_ROWS` 边界的小规模 hold / linear 结果与手工期望逐点相等（含 null 位置）；
//! 3. 给出 `log_path` 时，预检行真实落盘。

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use fast_blf_core::{
    export_with_runtime, ExportFormat, ExportOptions, ExportRuntime, FillPolicy, ShardRef,
    TableKey, TableShards, TimeFormat, BAD_CONFIG,
};
use polars::prelude::*;
use tempfile::TempDir;

/// 写一个最小 Parquet 分片：`timestamp` + 若干 f64 信号列。
fn write_shard(path: &Path, times: &[f64], signals: &[(&str, Vec<f64>)]) {
    let mut columns: Vec<Column> = Vec::with_capacity(1 + signals.len());
    columns.push(Column::from(Series::from_vec(
        "timestamp".into(),
        times.to_vec(),
    )));
    for (name, values) in signals {
        columns.push(Column::from(Series::from_vec(
            (*name).into(),
            values.clone(),
        )));
    }
    let mut df = DataFrame::new_infer_height(columns).expect("构造测试分片");
    let file = File::create(path).expect("创建测试分片");
    ParquetWriter::new(file)
        .finish(&mut df)
        .expect("写入测试分片");
}

fn table(
    channel: u32,
    message: &str,
    signals: Vec<&str>,
    shard: PathBuf,
    rows: u64,
) -> (TableKey, TableShards) {
    let key = TableKey {
        channel,
        message: message.to_string(),
    };
    let table = TableShards {
        name: format!("Ch{channel}_{message}"),
        channel,
        message: message.to_string(),
        signal_names: signals.into_iter().map(str::to_string).collect(),
        shards: vec![ShardRef {
            path: shard,
            file: PathBuf::from("synthetic.blf"),
        }],
        rows,
    };
    (key, table)
}

fn options(out_dir: &Path, fill_policy: FillPolicy, hz: f64) -> ExportOptions {
    ExportOptions {
        split_by_message: false,
        resample_hz: Some(hz),
        format: ExportFormat::Csv,
        out_dir: out_dir.to_path_buf(),
        excel_row_limit: 1_000_000,
        fill_policy,
        ffill_max_gap_ms: None,
        time_format: TimeFormat::RelativeS,
        global_min: 0.0,
        file_timings: Vec::new(),
    }
}

/// 读 CSV：返回 `(header, rows)`，空字段解析为 `None`。
fn read_csv_rows(path: &Path) -> (Vec<String>, Vec<Vec<Option<f64>>>) {
    let text = std::fs::read_to_string(path).expect("读取 CSV");
    let mut lines = text.lines();
    let header: Vec<String> = lines
        .next()
        .expect("CSV 表头")
        .split(',')
        .map(str::to_string)
        .collect();
    let rows = lines
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split(',')
                .map(|field| {
                    if field.is_empty() {
                        None
                    } else {
                        Some(field.parse::<f64>().expect("CSV 浮点字段"))
                    }
                })
                .collect()
        })
        .collect();
    (header, rows)
}

/// 用例 1：`t1 - t0` 极大 → 网格点数超过硬上限，必须是结构化 `BAD_CONFIG`。
#[test]
fn oversized_grid_returns_bad_config_without_panic() {
    let dir = TempDir::new().expect("tempdir");
    let shard = dir.path().join("huge.parquet");
    write_shard(&shard, &[0.0, 1.0e12], &[("s", vec![1.0, 2.0])]);
    let mut tables = BTreeMap::new();
    let (key, value) = table(1, "HUGE", vec!["s"], shard, 2);
    tables.insert(key, value);

    let opts = options(&dir.path().join("out"), FillPolicy::Hold, 50.0);
    let err = export_with_runtime(&tables, &opts, &|_p| {}, &ExportRuntime::default())
        .expect_err("超大网格必须返回 Err 而不是 panic / abort");
    assert_eq!(err.code, BAD_CONFIG, "错误码应为 BAD_CONFIG，实际 {err}");
    assert!(
        err.message.contains("硬上限") || err.message.contains("上限"),
        "错误消息要点明上限：{err}"
    );
}

/// 用例 2：两个信号、25001 个网格点（跨 20000 行块边界），hold / linear 逐点对齐手工期望。
#[test]
fn chunked_hold_and_linear_match_manual_expectation() {
    let dir = TempDir::new().expect("tempdir");
    let shard_a = dir.path().join("a.parquet");
    let shard_b = dir.path().join("b.parquet");
    // t0 = 0（表 B），t1 = 500；表 A 的首样本在 1.0，因此 hold 下 [0,1) 必须是 null。
    write_shard(&shard_a, &[1.0, 500.0], &[("s_a", vec![10.0, 20.0])]);
    write_shard(
        &shard_b,
        &[0.0, 250.0, 500.0],
        &[("s_b", vec![5.0, 7.0, 9.0])],
    );
    let mut tables = BTreeMap::new();
    let (key_a, table_a) = table(1, "AAA", vec!["s_a"], shard_a, 2);
    let (key_b, table_b) = table(1, "BBB", vec!["s_b"], shard_b, 3);
    tables.insert(key_a, table_a);
    tables.insert(key_b, table_b);

    let hz = 50.0;
    let points = 25_001usize;
    let step = 1.0 / hz;
    let grid: Vec<f64> = (0..points).map(|i| i as f64 * step).collect();
    assert!(points > 20_000, "必须跨过 CHUNK_ROWS=20000 的行块边界");

    // ---- hold ----
    let out_hold = dir.path().join("out_hold");
    let opts = options(&out_hold, FillPolicy::Hold, hz);
    export_with_runtime(&tables, &opts, &|_p| {}, &ExportRuntime::default())
        .expect("hold 导出成功");
    let (header, rows) = read_csv_rows(&out_hold.join("Merged_All_Signals.csv"));
    assert_eq!(header, vec!["timestamp", "s_a", "s_b"]);
    assert_eq!(rows.len(), points);
    for (idx, point) in grid.iter().enumerate() {
        let expected_a = if *point < 1.0 {
            None
        } else if *point < 500.0 {
            Some(10.0)
        } else {
            Some(20.0)
        };
        let expected_b = if *point < 250.0 {
            Some(5.0)
        } else if *point < 500.0 {
            Some(7.0)
        } else {
            Some(9.0)
        };
        assert_eq!(rows[idx][1], expected_a, "hold s_a @ t={point}");
        assert_eq!(rows[idx][2], expected_b, "hold s_b @ t={point}");
    }

    // ---- linear ----
    let out_linear = dir.path().join("out_linear");
    let opts = options(&out_linear, FillPolicy::Linear, hz);
    export_with_runtime(&tables, &opts, &|_p| {}, &ExportRuntime::default())
        .expect("linear 导出成功");
    let (header, rows) = read_csv_rows(&out_linear.join("Merged_All_Signals.csv"));
    assert_eq!(header, vec!["timestamp", "s_a", "s_b"]);
    assert_eq!(rows.len(), points);
    for (idx, point) in grid.iter().enumerate() {
        let expected_a = if *point < 1.0 {
            10.0
        } else if *point >= 500.0 {
            20.0
        } else {
            // 与 `resample_linear` 相同的表达式顺序，保证 f64 逐位一致。
            10.0 + (20.0 - 10.0) * ((*point - 1.0) / (500.0 - 1.0))
        };
        let expected_b = if *point < 250.0 {
            5.0 + (7.0 - 5.0) * ((*point - 0.0) / (250.0 - 0.0))
        } else if *point < 500.0 {
            7.0 + (9.0 - 7.0) * ((*point - 250.0) / (500.0 - 250.0))
        } else {
            9.0
        };
        assert_eq!(rows[idx][1], Some(expected_a), "linear s_a @ t={point}");
        assert_eq!(rows[idx][2], Some(expected_b), "linear s_b @ t={point}");
    }
}

/// 用例 3：预算闸门在分配前返回结构化 `BAD_CONFIG`，并带全部可诊断字段。
#[test]
fn budget_gate_returns_bad_config_with_diagnostic_fields() {
    let dir = TempDir::new().expect("tempdir");
    let shard = dir.path().join("budget.parquet");
    write_shard(&shard, &[0.0, 10.0], &[("s", vec![1.0, 2.0])]);
    let mut tables = BTreeMap::new();
    let (key, value) = table(1, "BUDGET", vec!["s"], shard, 2);
    tables.insert(key, value);

    let opts = options(&dir.path().join("out_budget"), FillPolicy::Hold, 10_000.0);
    let runtime = ExportRuntime {
        memory_budget_mb: 1,
        ..ExportRuntime::default()
    };
    let err = export_with_runtime(&tables, &opts, &|_p| {}, &runtime)
        .expect_err("预算不足必须返回 BAD_CONFIG");
    assert_eq!(err.code, BAD_CONFIG, "错误码应为 BAD_CONFIG，实际 {err}");
    for field in [
        "t0=",
        "t1=",
        "duration_s=",
        "hz=",
        "G=",
        "S=",
        "est_mb=",
        "budget_mb=",
    ] {
        assert!(
            err.message.contains(field),
            "预算错误消息缺少 `{field}`：{err}"
        );
    }
    assert!(
        err.message.contains("建议"),
        "预算错误消息必须给出可操作建议：{err}"
    );
}

/// 用例 4：`log_path` 给出时，phase 2 预检行必须落盘。
#[test]
fn preflight_log_line_is_written_when_log_path_given() {    let dir = TempDir::new().expect("tempdir");
    let shard = dir.path().join("log.parquet");
    write_shard(&shard, &[0.0, 10.0], &[("s", vec![1.0, 2.0])]);
    let mut tables = BTreeMap::new();
    let (key, value) = table(1, "LOG", vec!["s"], shard, 2);
    tables.insert(key, value);

    let log_path = dir.path().join("export.log");
    let out_dir = dir.path().join("out_log");
    let opts = options(&out_dir, FillPolicy::Hold, 50.0);
    let runtime = ExportRuntime {
        log_path: Some(log_path.clone()),
        ..ExportRuntime::default()
    };
    export_with_runtime(&tables, &opts, &|_p| {}, &runtime).expect("带日志导出成功");

    let text = std::fs::read_to_string(&log_path).expect("日志文件应存在");
    assert!(
        text.lines()
            .any(|line| line.contains("export phase2 wide-resample")),
        "日志里必须出现预检行，实际：{text}"
    );
    assert!(
        text.contains("G=")
            && text.contains("S=")
            && text.contains("est_mb=")
            && text.contains("budget_mb=")
            && text.contains("chunk_rows="),
        "预检行字段不完整：{text}"
    );
}
