//! 历史指标趋势分析：把 host_metrics 表里的时间序列换算成模型可读的趋势文本
//! （斜率、日变化率、外推预测），供 `query_history` 工具调用使用。

use crate::models::HostMetric;

/// 简单线性回归（最小二乘），返回 (斜率/单位x, 截距)。
/// 样本数 < 2 时返回 (0, 0)。
fn linear_slope(points: &[(f64, f64)]) -> (f64, f64) {
    let n = points.len() as f64;
    if n < 2.0 {
        return (0.0, 0.0);
    }
    let sum_x: f64 = points.iter().map(|p| p.0).sum();
    let sum_y: f64 = points.iter().map(|p| p.1).sum();
    let sum_xy: f64 = points.iter().map(|p| p.0 * p.1).sum();
    let sum_x2: f64 = points.iter().map(|p| p.0 * p.0).sum();
    let denom = n * sum_x2 - sum_x * sum_x;
    if denom.abs() < f64::EPSILON {
        return (0.0, sum_y / n);
    }
    let slope = (n * sum_xy - sum_x * sum_y) / denom;
    let intercept = (sum_y - slope * sum_x) / n;
    (slope, intercept)
}

/// 查询粒度对应的聚合桶宽度（秒）。"minute" 或未知值不聚合，直接用原始采样点
/// （适合看最近几小时的细节波动）；"hour" 按小时取均值（适合看当天走势）；
/// "day" 按天取均值（适合看长周期趋势——90 天窗口里原始采样密度可能很不均匀，
/// 按天聚合能避免密集时段的样本在线性回归里权重过高导致斜率失真）。
fn bucket_seconds(granularity: &str) -> f64 {
    match granularity {
        "hour" => 3600.0,
        "day" => 86400.0,
        _ => 0.0,
    }
}

/// 按固定时间桶宽度聚合散点（取桶内均值）。`points` 必须已按时间升序排列
/// （`list_metrics` 保证这一点）。`bucket_secs <= 0` 时原样返回，不做聚合。
fn bucket_points(points: &[(f64, f64)], bucket_secs: f64) -> Vec<(f64, f64)> {
    if bucket_secs <= 0.0 || points.is_empty() {
        return points.to_vec();
    }
    let mut buckets: Vec<(f64, f64, u32)> = Vec::new();
    for &(x, y) in points {
        let bucket_ts = (x / bucket_secs).floor() * bucket_secs;
        if let Some(last) = buckets.last_mut() {
            if (last.0 - bucket_ts).abs() < f64::EPSILON {
                last.1 += y;
                last.2 += 1;
                continue;
            }
        }
        buckets.push((bucket_ts, y, 1));
    }
    buckets
        .into_iter()
        .map(|(ts, sum, count)| (ts, sum / count as f64))
        .collect()
}

/// 将秒级时间戳格式化为可读的日期时间（本地时间）。
fn fmt_ts(ts: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// 各粒度对应的中文说明，用于在输出里告知模型当前序列是原始采样还是聚合均值。
fn granularity_label(granularity: &str) -> &'static str {
    match granularity {
        "hour" => "按小时聚合均值",
        "day" => "按天聚合均值",
        _ => "原始采样点（未聚合）",
    }
}

/// 格式化单个标量指标（cpu/mem/load）的趋势文本，供模型阅读。
/// `granularity` 决定是否先按小时/天聚合再做回归和展示（见 `bucket_seconds`）。
fn format_scalar_trend(label: &str, unit: &str, points: &[(f64, f64)], window_h: f64, granularity: &str) -> String {
    if points.is_empty() {
        return format!("指标: {}\n数据不足：该时间窗口内没有历史样本，建议先多使用几次监控/巡检/对话来积累数据。\n", label);
    }
    let points = bucket_points(points, bucket_seconds(granularity));
    if points.len() < 5 {
        return format!(
            "指标: {} ({} 个样本，数据偏少)\n最早: {} → {:.1}{}\n最新: {} → {:.1}{}\n样本不足 5 个，趋势斜率不可靠，建议多观察几天。\n",
            label, points.len(), fmt_ts(points[0].0 as u64), points[0].1, unit,
            fmt_ts(points.last().unwrap().0 as u64), points.last().unwrap().1, unit,
        );
    }
    let (slope, _) = linear_slope(&points);
    let values: Vec<f64> = points.iter().map(|p| p.1).collect();
    let min_v = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_v = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let avg = values.iter().sum::<f64>() / values.len() as f64;
    let latest = *values.last().unwrap();
    let first = values[0];
    // points 的 x 轴是秒级 Unix 时间戳，slope 是“值/秒”，乘以每天秒数换算为“值/天”
    let slope_per_day = slope * 86400.0;

    let mut out = format!(
        "指标: {}\n时间窗口: 最近 {:.0} 小时\n聚合粒度: {}\n样本数: {}\n最早: {} → {:.1}{}\n最新: {} → {:.1}{}\n最小值: {:.1}{}\n最大值: {:.1}{}\n平均值: {:.1}{}\n",
        label, window_h, granularity_label(granularity), points.len(),
        fmt_ts(points[0].0 as u64), first, unit,
        fmt_ts(points.last().unwrap().0 as u64), latest, unit,
        min_v, unit, max_v, unit, avg, unit,
    );

    if slope_per_day.abs() < 0.01 {
        out.push_str("趋势: 平稳（日变化 < 0.01）\n");
    } else if slope_per_day > 0.0 {
        out.push_str(&format!("趋势斜率: +{:.2}{}/天（持续上升）\n", slope_per_day, unit));
        // 外推到 90% 的天数
        if latest < 90.0 && slope_per_day > 0.0 {
            let days = (90.0 - latest) / slope_per_day;
            if days > 0.0 && days < 365.0 {
                out.push_str(&format!(
                    "线性外推: 按当前增速，约 {:.1} 天后达到 90%{}\n",
                    days, unit,
                ));
            }
        }
    } else {
        out.push_str(&format!("趋势斜率: {:.2}{}/天（下降中）\n", slope_per_day, unit));
    }

    // 完整序列（最多 30 个点，等间隔采样）
    let max_points = 30;
    let step = if points.len() > max_points {
        points.len() / max_points
    } else {
        1
    };
    out.push_str("完整序列（时间, 值）:\n");
    for p in points.iter().step_by(step) {
        out.push_str(&format!("{} {:.1}\n", fmt_ts(p.0 as u64), p.1));
    }
    out
}

/// 格式化磁盘指标趋势（按挂载点分组）。
fn format_disk_trend(rows: &[HostMetric], window_h: f64, granularity: &str) -> String {
    // 收集所有出现过的挂载点
    let mut mounts: Vec<String> = Vec::new();
    for r in rows {
        for d in &r.disks {
            if !mounts.contains(&d.mount) {
                mounts.push(d.mount.clone());
            }
        }
    }
    if mounts.is_empty() {
        return format!("指标: disk_percent（按挂载点）\n数据不足：该时间窗口内没有磁盘历史样本。\n");
    }
    mounts.sort();

    let mut out = format!(
        "指标: disk_percent（按挂载点）\n时间窗口: 最近 {:.0} 小时\n聚合粒度: {}\n样本数: {}\n\n",
        window_h, granularity_label(granularity), rows.len(),
    );
    let bucket_secs = bucket_seconds(granularity);
    for mount in &mounts {
        let points: Vec<(f64, f64)> = rows
            .iter()
            .filter_map(|r| {
                r.disks.iter().find(|d| d.mount == *mount).map(|d| (r.ts as f64, d.percent))
            })
            .collect();
        let points = bucket_points(&points, bucket_secs);
        if points.is_empty() {
            continue;
        }
        out.push_str(&format!("挂载点 {}:\n", mount));
        if points.len() < 5 {
            out.push_str(&format!(
                "  样本 {} 个，最早 {:.1}% → 最新 {:.1}%，数据偏少\n\n",
                points.len(), points[0].1, points.last().unwrap().1,
            ));
            continue;
        }
        let (slope, _) = linear_slope(&points);
        // points 的 x 轴是秒级 Unix 时间戳，slope 是“值/秒”，乘以每天秒数换算为“值/天”
        let slope_per_day = slope * 86400.0;
        let latest = points.last().unwrap().1;
        let first = points[0].1;
        out.push_str(&format!("  最早 {:.1}% → 最新 {:.1}%", first, latest));
        if slope_per_day.abs() < 0.01 {
            out.push_str("，平稳\n");
        } else if slope_per_day > 0.0 {
            out.push_str(&format!("，+{:.2}%/天（上升）\n", slope_per_day));
            if latest < 90.0 {
                let days = (90.0 - latest) / slope_per_day;
                if days > 0.0 && days < 365.0 {
                    out.push_str(&format!("  ⚠ 按当前增速，约 {:.1} 天后达到 90%\n", days));
                }
            }
        } else {
            out.push_str(&format!("，{:.2}%/天（下降）\n", slope_per_day));
        }
        out.push('\n');
    }
    out
}

/// 把 host_metrics 查询结果格式化为模型可读的趋势文本。供 `agent::tools::execute_tool`
/// 的 `query_history` 分支调用。
pub(crate) fn format_metric_trend(metric: &str, rows: &[HostMetric], window_h: f64, granularity: &str) -> String {
    match metric {
        "cpu" => {
            let points: Vec<(f64, f64)> = rows.iter().map(|r| (r.ts as f64, r.cpu_percent)).collect();
            format_scalar_trend("cpu_percent", "%", &points, window_h, granularity)
        }
        "mem" => {
            let points: Vec<(f64, f64)> = rows.iter().map(|r| (r.ts as f64, r.mem_percent)).collect();
            format_scalar_trend("mem_percent", "%", &points, window_h, granularity)
        }
        "load" => {
            let points: Vec<(f64, f64)> = rows.iter().map(|r| (r.ts as f64, r.load1)).collect();
            format_scalar_trend("load1 (1分钟)", "", &points, window_h, granularity)
        }
        "disk" => format_disk_trend(rows, window_h, granularity),
        _ => format!("未知指标: {}。可用: cpu, mem, load, disk\n", metric),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MetricDisk, MetricTop};

    fn metric(ts: u64, cpu: f64, mem: f64, load1: f64, disk_percent: f64) -> HostMetric {
        HostMetric {
            id: 0,
            host_id: "h1".to_string(),
            ts,
            cpu_percent: cpu,
            load1,
            mem_total_mb: 4096,
            mem_used_mb: (4096.0 * mem / 100.0) as u64,
            mem_percent: mem,
            disks: vec![MetricDisk { mount: "/".to_string(), percent: disk_percent }],
            top: Vec::<MetricTop>::new(),
            source: "test".to_string(),
        }
    }

    #[test]
    fn linear_slope_returns_zero_for_insufficient_samples() {
        assert_eq!(linear_slope(&[]), (0.0, 0.0));
        assert_eq!(linear_slope(&[(0.0, 5.0)]), (0.0, 0.0));
    }

    #[test]
    fn linear_slope_detects_upward_trend() {
        // y = 2x + 1，斜率应精确为 2
        let points: Vec<(f64, f64)> = (0..10).map(|i| (i as f64, 2.0 * i as f64 + 1.0)).collect();
        let (slope, intercept) = linear_slope(&points);
        assert!((slope - 2.0).abs() < 1e-9);
        assert!((intercept - 1.0).abs() < 1e-9);
    }

    #[test]
    fn linear_slope_flat_series_has_zero_slope() {
        let points: Vec<(f64, f64)> = (0..10).map(|i| (i as f64, 42.0)).collect();
        let (slope, intercept) = linear_slope(&points);
        assert!(slope.abs() < 1e-9);
        assert!((intercept - 42.0).abs() < 1e-9);
    }

    #[test]
    fn format_scalar_trend_reports_insufficient_data() {
        let out = format_scalar_trend("cpu_percent", "%", &[], 168.0, "minute");
        assert!(out.contains("数据不足"));
    }

    #[test]
    fn format_scalar_trend_reports_few_samples_as_unreliable() {
        let points: Vec<(f64, f64)> = (0..3).map(|i| (i as f64 * 3600.0, 10.0 + i as f64)).collect();
        let out = format_scalar_trend("cpu_percent", "%", &points, 24.0, "minute");
        assert!(out.contains("样本不足"));
    }

    #[test]
    fn format_scalar_trend_flags_upward_extrapolation() {
        // 每小时 +1%，从 50% 起步，应触发“持续上升”和“按当前增速”外推文案
        let points: Vec<(f64, f64)> = (0..10).map(|i| (i as f64 * 3600.0, 50.0 + i as f64)).collect();
        let out = format_scalar_trend("disk_percent", "%", &points, 10.0, "minute");
        assert!(out.contains("持续上升"));
        assert!(out.contains("按当前增速"));
    }

    #[test]
    fn format_scalar_trend_flags_stable_series() {
        let points: Vec<(f64, f64)> = (0..10).map(|i| (i as f64 * 3600.0, 30.0)).collect();
        let out = format_scalar_trend("cpu_percent", "%", &points, 10.0, "minute");
        assert!(out.contains("平稳"));
    }

    #[test]
    fn format_disk_trend_groups_by_mount_point() {
        let rows = vec![metric(0, 1.0, 1.0, 0.1, 40.0), metric(3600, 1.0, 1.0, 0.1, 60.0)];
        let out = format_disk_trend(&rows, 1.0, "minute");
        assert!(out.contains("挂载点 /"));
        assert!(out.contains("40.0%"));
        assert!(out.contains("60.0%"));
    }

    #[test]
    fn format_disk_trend_reports_no_data() {
        let out = format_disk_trend(&[], 1.0, "minute");
        assert!(out.contains("数据不足"));
    }

    #[test]
    fn bucket_points_averages_within_same_bucket() {
        // 一小时桶内两个样本（10 和 20），应被聚合为均值 15
        let points = vec![(0.0, 10.0), (1800.0, 20.0)];
        let bucketed = bucket_points(&points, 3600.0);
        assert_eq!(bucketed, vec![(0.0, 15.0)]);
    }

    #[test]
    fn bucket_points_noop_when_bucket_secs_zero() {
        let points = vec![(0.0, 10.0), (10.0, 20.0)];
        assert_eq!(bucket_points(&points, 0.0), points);
    }

    #[test]
    fn format_scalar_trend_hour_granularity_aggregates_dense_samples() {
        // 模拟每分钟一个样本、共 10 小时的密集数据；按小时聚合后应只剩 10 个桶。
        let points: Vec<(f64, f64)> = (0..600).map(|i| (i as f64 * 60.0, 50.0)).collect();
        let out = format_scalar_trend("cpu_percent", "%", &points, 10.0, "hour");
        assert!(out.contains("按小时聚合均值"));
        assert!(out.contains("样本数: 10"));
    }

    #[test]
    fn format_scalar_trend_day_granularity_aggregates_dense_samples() {
        // 模拟每小时一个样本、共 10 天的数据；按天聚合后应剩 10 个桶。
        let points: Vec<(f64, f64)> = (0..240).map(|i| (i as f64 * 3600.0, 50.0)).collect();
        let out = format_scalar_trend("cpu_percent", "%", &points, 240.0, "day");
        assert!(out.contains("按天聚合均值"));
        assert!(out.contains("样本数: 10"));
    }
}
