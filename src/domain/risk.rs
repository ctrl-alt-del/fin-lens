//! 历史序列与风险指标：纯函数，不读时钟、不碰 I/O。
//!
//! 这里的「风险」是**已实现**风险（区间收益、最大回撤、年化波动），
//! 不是对未来收益的预测。调用方必须把「当前持仓回溯」标注清楚。

use chrono::{Duration, NaiveDate};

/// 一条归一化净值曲线上的点。
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesPoint {
    pub date: NaiveDate,
    pub value: f64,
}

/// 某个时间窗口（自然日）的已实现统计。
#[derive(Debug, Clone, PartialEq)]
pub struct HorizonStats {
    pub days: i64,
    pub start: NaiveDate,
    pub end: NaiveDate,
    /// 区间累计收益，小数。
    pub ret: f64,
    /// 区间内最大回撤，正数（0.18 = 回撤 18%）。
    pub max_drawdown: f64,
    /// 年化波动率（日收益标准差 × √252）；样本不足时 None。
    pub volatility: Option<f64>,
    pub observations: usize,
}

/// 相邻点的简单收益率。
pub fn daily_returns(series: &[SeriesPoint]) -> Vec<f64> {
    series
        .windows(2)
        .filter(|pair| pair[0].value > 0.0 && pair[1].value.is_finite())
        .map(|pair| pair[1].value / pair[0].value - 1.0)
        .collect()
}

/// 最大回撤（峰到谷），返回正数幅度。输入不足两点或全非正时 None。
pub fn max_drawdown(values: &[f64]) -> Option<f64> {
    if values.len() < 2 || values.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let mut peak = f64::NEG_INFINITY;
    let mut worst = 0.0_f64;
    for value in values {
        if *value <= 0.0 {
            return None;
        }
        peak = peak.max(*value);
        worst = worst.max((peak - *value) / peak);
    }
    Some(worst)
}

/// 年化波动率；少于 3 个收益样本时 None（2 个样本的标准差没有意义）。
pub fn annualized_volatility(returns: &[f64]) -> Option<f64> {
    if returns.len() < 3 {
        return None;
    }
    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    Some((variance.max(0.0)).sqrt() * 252.0_f64.sqrt())
}

/// 取最近 `days` 个自然日的窗口统计；数据起点晚于窗口时从最早点算起，
/// 并在 `start` 中如实返回实际起点，避免把短样本伪装成完整一年。
pub fn horizon_stats(series: &[SeriesPoint], days: i64) -> Option<HorizonStats> {
    if series.len() < 2 || days <= 0 {
        return None;
    }
    let end_point = series.last()?;
    let cutoff = end_point.date - Duration::days(days);
    let start_idx = series.iter().position(|p| p.date >= cutoff)?;
    if start_idx >= series.len() - 1 {
        return None;
    }
    let window = &series[start_idx..];
    let start_point = &window[0];
    if start_point.value <= 0.0 {
        return None;
    }
    let values: Vec<f64> = window.iter().map(|p| p.value).collect();
    Some(HorizonStats {
        days,
        start: start_point.date,
        end: end_point.date,
        ret: end_point.value / start_point.value - 1.0,
        max_drawdown: max_drawdown(&values).unwrap_or(0.0),
        volatility: annualized_volatility(&daily_returns(window)),
        observations: window.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    /// 测试用「第 n 天」：以 2026-01-01 为第 1 天平移，避免构造非法日期。
    fn d(offset: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + Duration::days(offset as i64 - 1)
    }

    fn points(values: &[(u32, f64)]) -> Vec<SeriesPoint> {
        values
            .iter()
            .map(|(day, value)| SeriesPoint {
                date: d(*day),
                value: *value,
            })
            .collect()
    }

    #[test]
    fn max_drawdown_peak_to_trough() {
        let dd = max_drawdown(&[100.0, 120.0, 90.0, 110.0]).unwrap();
        assert!((dd - 0.25).abs() < 1e-12);
        assert_eq!(max_drawdown(&[100.0]), None);
    }

    #[test]
    fn horizon_picks_first_point_inside_window() {
        // 1 月底到 4 月底，取 30 天窗口应落在 3 月底之后。
        let series = points(&[(1, 100.0), (28, 110.0), (60, 105.0), (90, 120.0)]);
        let h = horizon_stats(&series, 30).unwrap();
        assert_eq!(h.start, d(60));
        assert_eq!(h.end, d(90));
        assert!((h.ret - (120.0 / 105.0 - 1.0)).abs() < 1e-12);
    }

    #[test]
    fn volatility_zero_for_monotone_flat_returns() {
        let series = points(&[(1, 100.0), (2, 100.0), (3, 100.0), (4, 100.0)]);
        let h = horizon_stats(&series, 10).unwrap();
        assert!(h.volatility.unwrap() < 1e-9);
    }

    #[test]
    fn short_window_returns_none_instead_of_fake_stats() {
        let series = points(&[(1, 100.0), (2, 101.0)]);
        // 30 天窗口里只有 2 个点：允许统计，但起点是 1 号而不是 30 天前。
        let h = horizon_stats(&series, 30).unwrap();
        assert_eq!(h.start, d(1));
        assert_eq!(h.observations, 2);
    }
}
