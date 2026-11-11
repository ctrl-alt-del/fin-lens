//! 跟涨系数回归（20 日窗口）。
//!
//! 指数联接基名义上 1:1 跟踪目标指数，实际因为 93% 左右仓位持有目标 ETF、
//! 外加现金/管理费拖累，真实跟涨系数稳定地小于 1。这里用最小二乘把
//! `fund_pct = α + β · index_pct` 拟合出来，β 就是估值要用的跟涨系数。
//!
//! 纯函数：输入是成对的日涨跌幅序列，输出拟合结果，不碰数据库。

use serde::Serialize;

/// 最小样本量：少于这个数不给出 β，宁可让调用方用默认值并打标记。
pub const MIN_OBS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TrackingFit {
    /// 跟涨系数（斜率）。
    pub beta: f64,
    /// 超额收益（截距）。
    pub alpha: f64,
    /// 拟合优度。
    pub r2: f64,
    pub n: usize,
}

/// 用成对日涨跌幅拟合 `fund_pct = α + β · benchmark_pct`。
///
/// 返回 `None` 当且仅当：样本不足、存在非有限数、或基准完全没有波动
/// （此时 β 不可识别）。这些情况都不应该被静默当成 β=1。
pub fn fit_tracking_beta(pairs: &[(f64, f64)]) -> Option<TrackingFit> {
    let pairs: Vec<(f64, f64)> = pairs
        .iter()
        .copied()
        .filter(|(f, b)| f.is_finite() && b.is_finite())
        .collect();
    if pairs.len() < MIN_OBS {
        return None;
    }
    let n = pairs.len() as f64;
    let mean_f = pairs.iter().map(|(f, _)| f).sum::<f64>() / n;
    let mean_b = pairs.iter().map(|(_, b)| b).sum::<f64>() / n;

    let mut sxx = 0.0;
    let mut sxy = 0.0;
    let mut syy = 0.0;
    for (f, b) in &pairs {
        let db = b - mean_b;
        let df = f - mean_f;
        sxx += db * db;
        sxy += db * df;
        syy += df * df;
    }
    if sxx <= f64::EPSILON || !sxx.is_finite() {
        return None;
    }

    let beta = sxy / sxx;
    let alpha = mean_f - beta * mean_b;
    let r2 = if syy > f64::EPSILON {
        (sxy * sxy) / (sxx * syy)
    } else {
        0.0
    };

    if !beta.is_finite() || !alpha.is_finite() {
        return None;
    }
    Some(TrackingFit {
        beta,
        alpha,
        r2: r2.clamp(0.0, 1.0),
        n: pairs.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_known_beta() {
        // 构造 β=0.93, α=0.0005 的完美线性关系
        let mut pairs = Vec::new();
        for i in 0..20 {
            let b = -0.02 + 0.002 * i as f64;
            let f = 0.0005 + 0.93 * b;
            pairs.push((f, b));
        }
        let fit = fit_tracking_beta(&pairs).unwrap();
        assert!((fit.beta - 0.93).abs() < 1e-9);
        assert!((fit.alpha - 0.0005).abs() < 1e-9);
        assert!((fit.r2 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_flat_benchmark() {
        let pairs: Vec<(f64, f64)> = (0..10).map(|_| (0.001, 0.0)).collect();
        assert!(fit_tracking_beta(&pairs).is_none());
    }

    #[test]
    fn rejects_small_sample() {
        let pairs = vec![(0.01, 0.01), (0.02, 0.02)];
        assert!(fit_tracking_beta(&pairs).is_none());
    }

    #[test]
    fn ignores_non_finite_pairs() {
        let mut pairs: Vec<(f64, f64)> = (0..6)
            .map(|i| (0.001 * i as f64, 0.001 * i as f64))
            .collect();
        pairs.push((f64::NAN, 0.5));
        assert_eq!(fit_tracking_beta(&pairs).unwrap().n, 6);
    }
}
