//! 纯函数估值内核。
//!
//! 硬性约束：无 I/O、无时钟、无随机数、无全局状态。相同输入必然得到相同输出，
//! 因此可以被单测、被回放、被拿去和实际净值做逐日对账。
//!
//! 三条路径：
//! 1. [`estimate_transparent`]：`估值 = 前一日净值 × (1 + 基准涨跌 × 跟涨系数 + 偏差)`
//! 2. [`estimate_semi_transparent`]：`Σ(权重 × 个股涨跌) + 残余仓位 × 行业代理 + 偏差`
//! 3. [`estimate_opaque_fallback`]：退化为基准跟随，置信度打对折并打标记
//!
//! 偏差项（EWMA）由 [`super::calibration`] 提供，本模块只负责把它加进去。

use std::collections::HashMap;

use serde::Serialize;
use thiserror::Error;

use super::types::{HoldingWeight, ValuationMode};

/// 跟涨系数默认值：指数联接基通常 93% 左右仓位持有目标 ETF。
pub const DEFAULT_TRACKING_BETA: f64 = 0.93;
/// 跟涨系数的可接受区间，越界即认为回归结果不可信并夹紧。
pub const MIN_TRACKING_BETA: f64 = 0.50;
/// 商品/指数联接在极端行情下可能略微超过 1，留 10% 余量。
pub const MAX_TRACKING_BETA: f64 = 1.10;

/// 内核错误：全部是「输入不合法」，一旦发生就不应产出估值。
#[derive(Debug, Clone, Error, PartialEq)]
pub enum KernelError {
    #[error("前一日净值必须为正，实际为 {0}")]
    InvalidPrevNav(f64),
    #[error("透明资产缺少基准涨跌")]
    MissingBenchmark,
    #[error("主动权益基缺少持仓且无残余仓位代理")]
    NoHoldings,
    #[error("输入非法：{0}")]
    NonFinite(&'static str),
    #[error("持仓权重越界：{instrument} = {weight}")]
    InvalidWeight { instrument: String, weight: f64 },
    #[error("权益总仓位越界：{0}")]
    InvalidStockWeight(f64),
}

/// 估值过程中的可解释标记，直接进 API 响应，Dashboard 用来解释“为什么这个数不可信”。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    /// 某个重仓股当前没有行情，权重被并入残余仓位。
    MissingHoldingQuote,
    /// 残余仓位没有行业 ETF / 宽基代理，按 0 涨跌处理。
    ResidualProxyMissing,
    /// 跟涨系数缺失，使用了默认值。
    BetaMissing,
    /// 跟涨系数被夹紧到合理区间。
    BetaClamped,
    /// EWMA 偏差项观测数不足或已过期，本次未启用。
    BiasInactive,
    /// 该基金只能退化为基准跟随。
    OpaqueFallback,
    /// 有行情的重仓覆盖不足 50%，估值仅供参考。
    LowCoverage,
    /// QDII：用的是上一夜美股收盘涨跌（T-1），A 股时段内不是实时联动。
    QdiiLag,
    /// QDII：缺少 USD/CNY 汇率变动，汇率项按 0 处理。
    FxMissing,
}

/// 一项涨跌贡献，用于分解「今天这 1.2% 是怎么来的」。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Contribution {
    pub label: String,
    pub name: String,
    pub weight: f64,
    pub pct: f64,
    pub contribution: f64,
}

/// 估值结果。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Estimate {
    pub mode: ValuationMode,
    pub prev_nav: f64,
    pub estimated_nav: f64,
    /// 未加偏差项的估算涨跌。
    pub raw_pct: f64,
    /// 实际启用的偏差项（未启用即 0）。
    pub bias_pct: f64,
    /// 最终估算涨跌。
    pub estimated_pct: f64,
    /// 有实时行情的资产占净值比例。
    pub coverage: f64,
    /// 启发式置信度 0..1，只用于排序/展示，不参与数值计算。
    pub confidence: f64,
    pub contributions: Vec<Contribution>,
    pub flags: Vec<Flag>,
}

/// 偏差项输入。`applied = false` 表示本次不校准（观测不足/过期），内核会打标记。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BiasInput {
    pub pct: f64,
    pub applied: bool,
}

impl BiasInput {
    pub const NONE: Self = Self {
        pct: 0.0,
        applied: false,
    };

    pub const fn applied(pct: f64) -> Self {
        Self { pct, applied: true }
    }
}

fn validate_prev_nav(prev_nav: f64) -> Result<(), KernelError> {
    if !prev_nav.is_finite() || prev_nav <= 0.0 {
        return Err(KernelError::InvalidPrevNav(prev_nav));
    }
    Ok(())
}

fn ensure_finite(name: &'static str, value: f64) -> Result<(), KernelError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(KernelError::NonFinite(name))
    }
}

fn apply_bias(bias: BiasInput, flags: &mut Vec<Flag>) -> f64 {
    if bias.applied && bias.pct.is_finite() {
        bias.pct
    } else {
        flags.push(Flag::BiasInactive);
        0.0
    }
}

// ---------------------------------------------------------------------------
// 1. 透明资产：黄金 / 指数联接基
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TransparentParams {
    /// 底层基准的当日涨跌幅（指数点数涨跌 / 金价涨跌），小数。
    pub benchmark_pct: Option<f64>,
    /// 跟涨系数；`None` 表示尚未回归出结果，用默认值。
    pub beta: Option<f64>,
    /// 汇率调整（QDII / 黄金人民币计价时由采集端预先把外盘涨幅折算成本币涨幅，
    /// 若未折算可在这里补一个汇率涨跌）。
    pub fx_pct: f64,
    pub bias: BiasInput,
}

impl TransparentParams {
    pub fn new(benchmark_pct: f64, beta: Option<f64>) -> Self {
        Self {
            benchmark_pct: Some(benchmark_pct),
            beta,
            fx_pct: 0.0,
            bias: BiasInput::NONE,
        }
    }
}

/// 透明资产估值。
///
/// `raw = beta × (benchmark + fx)`，`nav = prev_nav × (1 + raw + bias)`。
/// 这里的 beta 承载了「指数联接基约 93% 仓位持有 ETF」造成的衰减，
/// 所以它必须来自最近 20 日回归，而不是拍脑袋的 1.0。
pub fn estimate_transparent(
    prev_nav: f64,
    params: &TransparentParams,
) -> Result<Estimate, KernelError> {
    validate_prev_nav(prev_nav)?;

    let mut flags = Vec::new();
    let benchmark_pct = params.benchmark_pct.ok_or(KernelError::MissingBenchmark)?;
    ensure_finite("benchmark_pct", benchmark_pct)?;
    ensure_finite("fx_pct", params.fx_pct)?;

    let beta = match params.beta {
        Some(b) if b.is_finite() => b,
        Some(_) => return Err(KernelError::NonFinite("beta")),
        None => {
            flags.push(Flag::BetaMissing);
            DEFAULT_TRACKING_BETA
        }
    };
    let beta_clamped = beta.clamp(MIN_TRACKING_BETA, MAX_TRACKING_BETA);
    if (beta_clamped - beta).abs() > 1e-12 {
        flags.push(Flag::BetaClamped);
    }

    let underlying_pct = benchmark_pct + params.fx_pct;
    let raw_pct = beta_clamped * underlying_pct;
    let bias_pct = apply_bias(params.bias, &mut flags);
    let estimated_pct = raw_pct + bias_pct;
    let estimated_nav = prev_nav * (1.0 + estimated_pct);

    let confidence = {
        let mut c: f64 = 0.70;
        if params.beta.is_some() {
            c += 0.18;
        }
        if params.bias.applied {
            c += 0.10;
        }
        c.clamp(0.05, 0.99)
    };

    Ok(Estimate {
        mode: ValuationMode::Transparent,
        prev_nav,
        estimated_nav,
        raw_pct,
        bias_pct,
        estimated_pct,
        coverage: 1.0,
        confidence,
        contributions: vec![
            Contribution {
                label: "benchmark".to_string(),
                name: "基准涨跌 × 跟涨系数".to_string(),
                weight: beta_clamped,
                pct: underlying_pct,
                contribution: raw_pct,
            },
            Contribution {
                label: "bias".to_string(),
                name: "EWMA 系统偏差".to_string(),
                weight: 1.0,
                pct: bias_pct,
                contribution: bias_pct,
            },
        ],
        flags,
    })
}

// ---------------------------------------------------------------------------
// 2. 半透明资产：主动权益基
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SemiTransparentParams {
    /// 最新季报十大重仓（权重和为基金净值占比，通常 0.3~0.6）。
    pub holdings: Vec<HoldingWeight>,
    /// 证券代码 → 当日涨跌幅。
    pub quotes: HashMap<String, f64>,
    /// 权益总仓位（股票占净值比），来自季报资产配置；`None` 视为满仓 1.0。
    pub stock_weight: Option<f64>,
    /// 十大重仓之外的残余股票仓位用什么涨跌代理（行业 ETF / 宽基指数）。
    pub residual_pct: Option<f64>,
    pub bias: BiasInput,
}

impl SemiTransparentParams {
    pub fn new(holdings: Vec<HoldingWeight>, quotes: HashMap<String, f64>) -> Self {
        Self {
            holdings,
            quotes,
            stock_weight: None,
            residual_pct: None,
            bias: BiasInput::NONE,
        }
    }
}

/// 半透明资产估值。
///
/// `raw = Σ(w_i × pct_i) + (stock_weight − Σw_i) × residual_pct`
///
/// 关键点：没有行情的重仓不会静默消失——它的权重自动并入残余仓位并打
/// [`Flag::MissingHoldingQuote`]，这样 coverage 和 confidence 会如实下降，
/// 用户能看出“这个估值今天其实有一半仓位是猜的”。
pub fn estimate_semi_transparent(
    prev_nav: f64,
    params: &SemiTransparentParams,
) -> Result<Estimate, KernelError> {
    validate_prev_nav(prev_nav)?;

    let mut flags = Vec::new();
    let stock_weight = params.stock_weight.unwrap_or(1.0);
    if !stock_weight.is_finite() || stock_weight <= 0.0 || stock_weight > 1.0 {
        return Err(KernelError::InvalidStockWeight(stock_weight));
    }
    if params.holdings.is_empty() && params.residual_pct.is_none() {
        return Err(KernelError::NoHoldings);
    }

    let mut covered = 0.0_f64;
    let mut raw_pct = 0.0_f64;
    let mut contributions = Vec::with_capacity(params.holdings.len() + 2);

    for holding in &params.holdings {
        if !holding.weight.is_finite() || !(0.0..=1.0).contains(&holding.weight) {
            return Err(KernelError::InvalidWeight {
                instrument: holding.instrument.clone(),
                weight: holding.weight,
            });
        }
        match params.quotes.get(&holding.instrument).copied() {
            Some(pct) if pct.is_finite() => {
                let contribution = holding.weight * pct;
                covered += holding.weight;
                raw_pct += contribution;
                contributions.push(Contribution {
                    label: format!("holding:{}", holding.instrument),
                    name: holding.name.clone(),
                    weight: holding.weight,
                    pct,
                    contribution,
                });
            }
            _ => flags.push(Flag::MissingHoldingQuote),
        }
    }

    let residual_weight = (stock_weight - covered).max(0.0);
    if residual_weight > 1e-9 {
        match params.residual_pct.filter(|v| v.is_finite()) {
            Some(residual_pct) => {
                let contribution = residual_weight * residual_pct;
                raw_pct += contribution;
                contributions.push(Contribution {
                    label: "residual".to_string(),
                    name: "残余权益仓位（行业代理）".to_string(),
                    weight: residual_weight,
                    pct: residual_pct,
                    contribution,
                });
            }
            None => flags.push(Flag::ResidualProxyMissing),
        }
    }

    let bias_pct = apply_bias(params.bias, &mut flags);
    let estimated_pct = raw_pct + bias_pct;
    let estimated_nav = prev_nav * (1.0 + estimated_pct);
    let coverage = covered.min(1.0);
    if coverage < 0.5 {
        flags.push(Flag::LowCoverage);
    }

    let all_quoted = params
        .holdings
        .iter()
        .all(|h| params.quotes.contains_key(&h.instrument));
    let residual_unknown = residual_weight > 1e-9 && params.residual_pct.is_none();
    let confidence = {
        let mut c = 0.25 + 0.55 * coverage;
        if all_quoted {
            c += 0.10;
        }
        if params.bias.applied {
            c += 0.06;
        }
        if residual_unknown {
            c -= 0.10;
        }
        c.clamp(0.05, 0.99)
    };

    Ok(Estimate {
        mode: ValuationMode::SemiTransparent,
        prev_nav,
        estimated_nav,
        raw_pct,
        bias_pct,
        estimated_pct,
        coverage,
        confidence,
        contributions,
        flags,
    })
}

// ---------------------------------------------------------------------------
// 3. 不透明资产：兜底
// ---------------------------------------------------------------------------

/// 持仓与基准都缺失时的兜底：只跟基准，置信度大幅下调。
/// 存在的意义是让 Dashboard 永远有数可显示，同时明确它是“低可信”的。
pub fn estimate_opaque_fallback(
    prev_nav: f64,
    benchmark_pct: Option<f64>,
    bias: BiasInput,
) -> Result<Estimate, KernelError> {
    let mut estimate = estimate_transparent(
        prev_nav,
        &TransparentParams {
            benchmark_pct,
            beta: Some(0.90),
            fx_pct: 0.0,
            bias,
        },
    )?;
    estimate.mode = ValuationMode::Opaque;
    estimate.confidence = (estimate.confidence * 0.45).clamp(0.05, 0.5);
    estimate.flags.push(Flag::OpaqueFallback);
    Ok(estimate)
}

// ---------------------------------------------------------------------------
// 4. 跨境 QDII：T-1 美股基准 + 人民币汇率
// ---------------------------------------------------------------------------

/// QDII 跟涨系数默认值：直接跟踪美股行业指数的原油基金基本满仓，
/// 但申赎/现金拖累使其略低于 1；没有回归结果时先按 1.0 处理。
pub const DEFAULT_QDII_BETA: f64 = 1.0;

/// QDII 估值参数。
///
/// 关键口径：
/// - `benchmark_pct` 是**上一夜美股交易时段**的涨跌幅（A 股白天美股休市）。
/// - `fx_pct` 是同期 USD/CNY 的变动，**正值 = 人民币贬值**，对人民币净值是正贡献。
/// - `lag_days` 仅用于解释/审计，当前固定为 1。
#[derive(Debug, Clone, PartialEq)]
pub struct QdiiParams {
    pub benchmark_pct: Option<f64>,
    pub beta: Option<f64>,
    pub fx_pct: Option<f64>,
    pub lag_days: u32,
}

impl QdiiParams {
    pub fn new(
        benchmark_pct: Option<f64>,
        beta: Option<f64>,
        fx_pct: Option<f64>,
        lag_days: u32,
    ) -> Self {
        Self {
            benchmark_pct,
            beta,
            fx_pct,
            lag_days,
        }
    }
}

/// QDII 估值：`raw = (1 + beta × 美股涨跌) × (1 + 汇率) − 1`。
///
/// 比「(1 + β×美股) + 汇率」多一个交叉项，金额虽然很小（bp 级），但这是
/// 人民币净值的精确复利口径：先把美元资产按美股涨跌重估，再按汇率折算回人民币。
/// 偏差项仍然加在最终收益率上，且必须基于**未加偏差**的 raw 计算（见 calibration）。
pub fn estimate_qdii(
    prev_nav: f64,
    params: &QdiiParams,
    bias: BiasInput,
) -> Result<Estimate, KernelError> {
    validate_prev_nav(prev_nav)?;

    // QDII 必然带 T-1 滞后，这是产品结构而非数据质量问题。
    let mut flags = vec![Flag::QdiiLag];

    let benchmark_pct = params.benchmark_pct.ok_or(KernelError::MissingBenchmark)?;
    ensure_finite("benchmark_pct", benchmark_pct)?;

    let beta = match params.beta {
        Some(b) if b.is_finite() => b,
        Some(_) => return Err(KernelError::NonFinite("beta")),
        None => {
            flags.push(Flag::BetaMissing);
            DEFAULT_QDII_BETA
        }
    };
    let beta_clamped = beta.clamp(MIN_TRACKING_BETA, MAX_TRACKING_BETA);
    if (beta_clamped - beta).abs() > 1e-12 {
        flags.push(Flag::BetaClamped);
    }

    let fx_pct = match params.fx_pct {
        Some(v) if v.is_finite() => v,
        Some(_) => return Err(KernelError::NonFinite("fx_pct")),
        None => {
            flags.push(Flag::FxMissing);
            0.0
        }
    };

    let benchmark_contribution = beta_clamped * benchmark_pct;
    let raw_pct = (1.0 + benchmark_contribution) * (1.0 + fx_pct) - 1.0;
    let bias_pct = apply_bias(bias, &mut flags);
    let estimated_pct = raw_pct + bias_pct;
    let estimated_nav = prev_nav * (1.0 + estimated_pct);

    let confidence = {
        // 基准只有代理指数、行情滞后一夜、还要叠加汇率，可靠性天然低于境内透明资产。
        let mut c: f64 = 0.55;
        if params.beta.is_some() {
            c += 0.12;
        }
        if params.fx_pct.is_some() {
            c += 0.10;
        }
        if bias.applied {
            c += 0.08;
        }
        c.clamp(0.05, 0.99)
    };

    Ok(Estimate {
        mode: ValuationMode::Qdii,
        prev_nav,
        estimated_nav,
        raw_pct,
        bias_pct,
        estimated_pct,
        coverage: 1.0,
        confidence,
        contributions: vec![
            Contribution {
                label: "us_benchmark".to_string(),
                name: "上一夜美股基准 × 跟涨系数".to_string(),
                weight: beta_clamped,
                pct: benchmark_pct,
                contribution: benchmark_contribution,
            },
            Contribution {
                label: "fx".to_string(),
                name: "USD/CNY 汇率变动".to_string(),
                weight: 1.0,
                pct: fx_pct,
                contribution: fx_pct,
            },
            Contribution {
                label: "bias".to_string(),
                name: "EWMA 系统偏差".to_string(),
                weight: 1.0,
                pct: bias_pct,
                contribution: bias_pct,
            },
        ],
        flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holding(instrument: &str, weight: f64) -> HoldingWeight {
        HoldingWeight::new(instrument, instrument, weight)
    }

    #[test]
    fn transparent_index_feeder_applies_beta() {
        // 指数 +2%，跟涨系数 0.93 → 估算 +1.86%
        let params = TransparentParams::new(0.02, Some(0.93));
        let est = estimate_transparent(1.5000, &params).unwrap();
        assert!((est.estimated_pct - 0.0186).abs() < 1e-12);
        assert!((est.estimated_nav - 1.5279).abs() < 1e-12);
        assert!(est.flags.contains(&Flag::BiasInactive));
        assert_eq!(est.mode, ValuationMode::Transparent);
    }

    #[test]
    fn transparent_beta_missing_uses_default_and_flags() {
        let params = TransparentParams::new(0.01, None);
        let est = estimate_transparent(1.0, &params).unwrap();
        assert!(est.flags.contains(&Flag::BetaMissing));
        assert!((est.estimated_pct - 0.0093).abs() < 1e-12);
    }

    #[test]
    fn transparent_beta_is_clamped() {
        let params = TransparentParams::new(0.01, Some(1.8));
        let est = estimate_transparent(1.0, &params).unwrap();
        assert!(est.flags.contains(&Flag::BetaClamped));
        assert!((est.estimated_pct - MAX_TRACKING_BETA * 0.01).abs() < 1e-12);
    }

    #[test]
    fn transparent_fx_is_added_to_underlying() {
        let mut params = TransparentParams::new(0.01, Some(0.9));
        params.fx_pct = 0.005;
        let est = estimate_transparent(1.0, &params).unwrap();
        assert!((est.estimated_pct - 0.9 * 0.015).abs() < 1e-12);
    }

    #[test]
    fn transparent_requires_benchmark() {
        let mut params = TransparentParams::new(0.0, Some(0.93));
        params.benchmark_pct = None;
        assert_eq!(
            estimate_transparent(1.0, &params),
            Err(KernelError::MissingBenchmark)
        );
    }

    #[test]
    fn transparent_rejects_bad_prev_nav() {
        let params = TransparentParams::new(0.01, Some(0.93));
        assert!(matches!(
            estimate_transparent(0.0, &params),
            Err(KernelError::InvalidPrevNav(_))
        ));
    }

    #[test]
    fn semi_transparent_combines_holdings_and_residual() {
        // 两只重仓各 10%：+3% / -1%；权益仓位 90%，残余 70% 用 +1% 代理
        let quotes = HashMap::from([("A".to_string(), 0.03), ("B".to_string(), -0.01)]);
        let mut params =
            SemiTransparentParams::new(vec![holding("A", 0.10), holding("B", 0.10)], quotes);
        params.stock_weight = Some(0.90);
        params.residual_pct = Some(0.01);

        let est = estimate_semi_transparent(2.0000, &params).unwrap();
        // 0.003 - 0.001 + 0.7*0.01 = 0.009
        assert!((est.raw_pct - 0.009).abs() < 1e-12);
        assert!((est.estimated_nav - 2.018).abs() < 1e-12);
        assert!((est.coverage - 0.20).abs() < 1e-12);
        assert!(!est.flags.contains(&Flag::ResidualProxyMissing));
    }

    #[test]
    fn semi_transparent_missing_quote_moves_weight_to_residual() {
        // B 没有行情 → 不计入 covered，其权重落到残余仓位里并打标记
        let quotes = HashMap::from([("A".to_string(), 0.03)]);
        let mut params =
            SemiTransparentParams::new(vec![holding("A", 0.10), holding("B", 0.10)], quotes);
        params.stock_weight = Some(0.90);
        params.residual_pct = Some(0.0);

        let est = estimate_semi_transparent(2.0, &params).unwrap();
        assert!(est.flags.contains(&Flag::MissingHoldingQuote));
        assert!((est.raw_pct - 0.003).abs() < 1e-12);
        assert!((est.coverage - 0.10).abs() < 1e-12);
        assert!(est.flags.contains(&Flag::LowCoverage));
        assert!(est.confidence < 0.6);
    }

    #[test]
    fn semi_transparent_residual_without_proxy_is_flagged() {
        let quotes = HashMap::from([("A".to_string(), 0.02)]);
        let params = SemiTransparentParams::new(vec![holding("A", 0.10)], quotes);
        let est = estimate_semi_transparent(1.0, &params).unwrap();
        assert!(est.flags.contains(&Flag::ResidualProxyMissing));
        assert!((est.raw_pct - 0.002).abs() < 1e-12);
    }

    #[test]
    fn semi_transparent_bias_is_applied() {
        let quotes = HashMap::from([("A".to_string(), 0.02)]);
        let mut params = SemiTransparentParams::new(vec![holding("A", 0.10)], quotes);
        params.stock_weight = Some(0.10);
        params.bias = BiasInput::applied(0.0012);
        let est = estimate_semi_transparent(1.0, &params).unwrap();
        assert!((est.bias_pct - 0.0012).abs() < 1e-12);
        assert!((est.estimated_pct - 0.0032).abs() < 1e-12);
    }

    #[test]
    fn semi_transparent_rejects_invalid_stock_weight() {
        let quotes = HashMap::from([("A".to_string(), 0.02)]);
        let mut params = SemiTransparentParams::new(vec![holding("A", 0.10)], quotes);
        params.stock_weight = Some(1.5);
        assert!(matches!(
            estimate_semi_transparent(1.0, &params),
            Err(KernelError::InvalidStockWeight(_))
        ));
    }

    #[test]
    fn semi_transparent_without_anything_is_error() {
        let params = SemiTransparentParams::new(vec![], HashMap::new());
        assert_eq!(
            estimate_semi_transparent(1.0, &params),
            Err(KernelError::NoHoldings)
        );
    }

    #[test]
    fn opaque_fallback_lowers_confidence() {
        let est = estimate_opaque_fallback(1.0, Some(0.01), BiasInput::NONE).unwrap();
        assert_eq!(est.mode, ValuationMode::Opaque);
        assert!(est.flags.contains(&Flag::OpaqueFallback));
        assert!(est.confidence <= 0.5);
    }
    #[test]
    fn qdii_applies_us_benchmark_and_fx_compounding() {
        let prev_nav = 1.0;
        let params = QdiiParams::new(Some(-0.02), Some(1.0), Some(-0.001), 1);
        let est = estimate_qdii(prev_nav, &params, BiasInput::NONE).unwrap();

        let expected_raw = (1.0 - 0.02) * (1.0 - 0.001) - 1.0;
        assert!((est.raw_pct - expected_raw).abs() < 1e-12);
        assert!((est.estimated_nav - prev_nav * (1.0 + expected_raw)).abs() < 1e-9);
        assert!(est.flags.contains(&Flag::QdiiLag));
        assert!(!est.flags.contains(&Flag::FxMissing));
        assert_eq!(est.mode, ValuationMode::Qdii);
    }

    #[test]
    fn qdii_without_fx_falls_back_and_flags() {
        let params = QdiiParams::new(Some(-0.02), Some(1.0), None, 1);
        let est = estimate_qdii(1.0, &params, BiasInput::NONE).unwrap();
        assert!((est.raw_pct - (-0.02)).abs() < 1e-12);
        assert!(est.flags.contains(&Flag::FxMissing));
        assert!(est.confidence < 0.70);
    }

    #[test]
    fn qdii_rejects_missing_benchmark_and_invalid_nav() {
        let params = QdiiParams::new(None, Some(1.0), Some(0.0), 1);
        assert_eq!(
            estimate_qdii(1.0, &params, BiasInput::NONE),
            Err(KernelError::MissingBenchmark)
        );
        let bad_nav = estimate_qdii(0.0, &params, BiasInput::NONE);
        assert!(matches!(bad_nav, Err(KernelError::InvalidPrevNav(_))));
    }

    #[test]
    fn qdii_adds_bias_on_top() {
        let params = QdiiParams::new(Some(0.01), Some(1.0), Some(0.0), 1);
        let est = estimate_qdii(1.0, &params, BiasInput::applied(0.002)).unwrap();
        assert!((est.estimated_pct - 0.012).abs() < 1e-12);
        assert!((est.estimated_nav - 1.012).abs() < 1e-12);
    }
}
