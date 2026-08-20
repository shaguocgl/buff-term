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

/// 将秒级时间戳格式化为可读的日期时间（本地时间）。
fn fmt_ts(ts: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// 格式化单个标量指标（cpu/mem/load）的趋势文本，供模型阅读。
fn format_scalar_trend(label: &str, unit: &str, points: &[(f64, f64)], window_h: f64) -> String {
    if points.is_empty() {
        return format!("指标: {}\n数据不足：该时间窗口内没有历史样本，建议先多使用几次监控/巡检/对话来积累数据。\n", label);
    }
    if points.len() < 5 {
        return format!(
            "指标: {} ({} 个样本，数据偏少)\n最早: {} → {:.1}{}\n最新: {} → {:.1}{}\n样本不足 5 个，趋势斜率不可靠，建议多观察几天。\n",
            label, points.len(), fmt_ts(points[0].0 as u64), points[0].1, unit,
            fmt_ts(points.last().unwrap().0 as u64), points.last().unwrap().1, unit,
        );
    }
    let (slope, _) = linear_slope(points);
    let values: Vec<f64> = points.iter().map(|p| p.1).collect();
    let min_v = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_v = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let avg = values.iter().sum::<f64>() / values.len() as f64;
    let latest = *values.last().unwrap();
    let first = values[0];
    // points 的 x 轴是秒级 Unix 时间戳，slope 是“值/秒”，乘以每天秒数换算为“值/天”
    let slope_per_day = slope * 86400.0;

    let mut out = format!(
        "指标: {}\n时间窗口: 最近 {:.0} 小时\n样本数: {}\n最早: {} → {:.1}{}\n最新: {} → {:.1}{}\n最小值: {:.1}{}\n最大值: {:.1}{}\n平均值: {:.1}{}\n",
        label, window_h, points.len(),
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
fn format_disk_trend(rows: &[HostMetric], window_h: f64) -> String {
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

    let mut out = format!("指标: disk_percent（按挂载点）\n时间窗口: 最近 {:.0} 小时\n样本数: {}\n\n", window_h, rows.len());
    for mount in &mounts {
        let points: Vec<(f64, f64)> = rows
            .iter()
            .filter_map(|r| {
                r.disks.iter().find(|d| d.mount == *mount).map(|d| (r.ts as f64, d.percent))
            })
            .collect();
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
pub(crate) fn format_metric_trend(metric: &str, rows: &[HostMetric], window_h: f64) -> String {
    match metric {
        "cpu" => {
            let points: Vec<(f64, f64)> = rows.iter().map(|r| (r.ts as f64, r.cpu_percent)).collect();
            format_scalar_trend("cpu_percent", "%", &points, window_h)
        }
        "mem" => {
            let points: Vec<(f64, f64)> = rows.iter().map(|r| (r.ts as f64, r.mem_percent)).collect();
            format_scalar_trend("mem_percent", "%", &points, window_h)
        }
        "load" => {
            let points: Vec<(f64, f64)> = rows.iter().map(|r| (r.ts as f64, r.load1)).collect();
            format_scalar_trend("load1 (1分钟)", "", &points, window_h)
        }
        "disk" => format_disk_trend(rows, window_h),
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
        let out = format_scalar_trend("cpu_percent", "%", &[], 168.0);
        assert!(out.contains("数据不足"));
    }

    #[test]
    fn format_scalar_trend_reports_few_samples_as_unreliable() {
        let points: Vec<(f64, f64)> = (0..3).map(|i| (i as f64 * 3600.0, 10.0 + i as f64)).collect();
        let out = format_scalar_trend("cpu_percent", "%", &points, 24.0);
        assert!(out.contains("样本不足"));
    }

    #[test]
    fn format_scalar_trend_flags_upward_extrapolation() {
        // 每小时 +1%，从 50% 起步，应触发“持续上升”和“按当前增速”外推文案
        let points: Vec<(f64, f64)> = (0..10).map(|i| (i as f64 * 3600.0, 50.0 + i as f64)).collect();
        let out = format_scalar_trend("disk_percent", "%", &points, 10.0);
        assert!(out.contains("持续上升"));
        assert!(out.contains("按当前增速"));
    }

    #[test]
    fn format_scalar_trend_flags_stable_series() {
        let points: Vec<(f64, f64)> = (0..10).map(|i| (i as f64 * 3600.0, 30.0)).collect();
        let out = format_scalar_trend("cpu_percent", "%", &points, 10.0);
        assert!(out.contains("平稳"));
    }

    #[test]
    fn format_disk_trend_groups_by_mount_point() {
        let rows = vec![metric(0, 1.0, 1.0, 0.1, 40.0), metric(3600, 1.0, 1.0, 0.1, 60.0)];
        let out = format_disk_trend(&rows, 1.0);
        assert!(out.contains("挂载点 /"));
        assert!(out.contains("40.0%"));
        assert!(out.contains("60.0%"));
    }

    #[test]
    fn format_disk_trend_reports_no_data() {
        let out = format_disk_trend(&[], 1.0);
        assert!(out.contains("数据不足"));
    }
}
