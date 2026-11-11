//! EWMA 系统偏差自校准。
//!
//! 动机：主动权益基每天都会因为「隐形调仓 / 十大之外的持仓 / 打新 / 现金拖累」
//! 与我们的估算产生系统误差。这个误差短期内是稳定的，因此可以用指数加权移动
//! 平均（EWMA）把它估计出来，叠加到下一日的估算上。
//!
//! 三个刻意的设计决定：
//!
//! 1. **偏差在「收益率」空间维护，不在净值空间。** 交接文档写的是“估算净值 −
//!    实际净值”的差值，但净值绝对差会随净值规模漂移，且换份额/分红后失效。
//!    这里改为维护残差百分比，是同一信息的无量纲版本，外推更稳。
//! 2. **残差用「未加偏差项」的原始估算计算。** 否则偏差会自我强化形成反馈回路。
//! 3. **偏差有夹紧和半衰期衰减。** 一次数据错误或一次极端调仓不应让偏差永久
//!    绑架估值；超过 `half_life_days` 没有新观测就按 0.5 的幂次衰减。

use chrono::NaiveDate;
use serde::Serialize;
use thiserror::Error;

/// EWMA 参数。默认值对应「约 4 日有效窗口、3 次观测后启用」。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct EwmaConfig {
    /// 平滑系数：`bias_t = λ·bias_{t-1} + (1-λ)·residual_t`。
    pub lambda: f64,
    /// 最少观测次数，未达到则偏差项不生效。
    pub min_obs: u32,
    /// 偏差绝对值上限（小数），防止单日脏数据污染。
    pub clamp_pct: f64,
    /// 半衰期（自然日）：无新观测时偏差按 0.5^(days/half_life) 衰减。
    pub half_life_days: f64,
}

impl EwmaConfig {
    pub const DEFAULT_LAMBDA: f64 = 0.25;
    pub const DEFAULT_MIN_OBS: u32 = 3;
    pub const DEFAULT_CLAMP: f64 = 0.003;
    pub const DEFAULT_HALF_LIFE_DAYS: f64 = 10.0;

    /// 单日残差超过该值视为数据异常（净值口径错误、分红、折算），不进入 EWMA。
    pub const DEFAULT_OUTLIER_PCT: f64 = 0.05;
}

impl Default for EwmaConfig {
    fn default() -> Self {
        Self {
            lambda: Self::DEFAULT_LAMBDA,
            min_obs: Self::DEFAULT_MIN_OBS,
            clamp_pct: Self::DEFAULT_CLAMP,
            half_life_days: Self::DEFAULT_HALF_LIFE_DAYS,
        }
    }
}

/// 单只基金的校准状态（持久化到 `calibration` 表的最后一行，取最近一条）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct EwmaState {
    /// 当前偏差估计，小数，正值表示「我们的估算偏低」。
    pub bias_pct: f64,
    /// 残差绝对值的 EWMA，用作误差带。
    pub abs_pct: f64,
    pub n_obs: u32,
    pub last_date: Option<NaiveDate>,
}

impl EwmaState {
    pub fn empty() -> Self {
        Self {
            bias_pct: 0.0,
            abs_pct: 0.0,
            n_obs: 0,
            last_date: None,
        }
    }
}

impl Default for EwmaState {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum CalibrationError {
    #[error("残差不是有限数：{0}")]
    NonFinite(f64),
    #[error("衰减半衰期必须为正，实际为 {0}")]
    InvalidHalfLife(f64),
}

/// 一次「预测 → 实际」对账观测。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    pub trade_date: NaiveDate,
    /// 实际净值涨跌（由相邻两日单位净值算出）。
    pub actual_pct: f64,
    /// 当日**未加偏差项**的估算涨跌。
    pub raw_estimated_pct: f64,
}

impl Observation {
    /// 残差 = 实际 − 原始估算。正值表示估算偏低。
    pub fn residual_pct(&self) -> f64 {
        self.actual_pct - self.raw_estimated_pct
    }
}

/// 一次校准更新的完整结果，直接落 `calibration` 表。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalibrationUpdate {
    pub state: EwmaState,
    pub residual_pct: f64,
    /// 是否被接受；`false` 表示视为异常值，状态原样返回。
    pub accepted: bool,
    /// 是否因为观测数不足而暂不启用。
    pub active: bool,
}

/// 用当日观测更新 EWMA 状态。
///
/// 残差超过 `outlier_pct` 时拒绝本次观测（状态不变，`accepted = false`），
/// 这样一次净值口径事故不会污染后续一周的估值。
pub fn update(
    state: &EwmaState,
    obs: &Observation,
    cfg: &EwmaConfig,
) -> Result<CalibrationUpdate, CalibrationError> {
    let residual = obs.residual_pct();
    if !residual.is_finite() || !obs.actual_pct.is_finite() || !obs.raw_estimated_pct.is_finite() {
        return Err(CalibrationError::NonFinite(residual));
    }
    if residual.abs() > EwmaConfig::DEFAULT_OUTLIER_PCT {
        return Ok(CalibrationUpdate {
            state: *state,
            residual_pct: residual,
            accepted: false,
            active: is_active(state, cfg),
        });
    }

    let lambda = cfg.lambda.clamp(0.0, 1.0);
    let (bias_pct, abs_pct) = if state.n_obs == 0 {
        (residual, residual.abs())
    } else {
        (
            lambda * state.bias_pct + (1.0 - lambda) * residual,
            lambda * state.abs_pct + (1.0 - lambda) * residual.abs(),
        )
    };

    let next = EwmaState {
        bias_pct,
        abs_pct,
        n_obs: state.n_obs.saturating_add(1),
        last_date: Some(obs.trade_date),
    };
    Ok(CalibrationUpdate {
        active: is_active(&next, cfg),
        state: next,
        residual_pct: residual,
        accepted: true,
    })
}

fn is_active(state: &EwmaState, cfg: &EwmaConfig) -> bool {
    state.n_obs >= cfg.min_obs
}

/// 计算给定日期应叠加的偏差项。
///
/// 返回 `(bias_pct, active)`：
/// - `active = false` 时 `bias_pct` 恒为 0，调用方应打 [`super::valuation::Flag::BiasInactive`]。
/// - 时间越久，偏差按半衰期指数衰减，避免用两个月前的调仓痕迹估值今天。
pub fn apply(
    state: &EwmaState,
    today: NaiveDate,
    cfg: &EwmaConfig,
) -> Result<(f64, bool), CalibrationError> {
    if !is_active(state, cfg) {
        return Ok((0.0, false));
    }
    if cfg.half_life_days <= 0.0 || !cfg.half_life_days.is_finite() {
        return Err(CalibrationError::InvalidHalfLife(cfg.half_life_days));
    }
    let Some(last) = state.last_date else {
        return Ok((0.0, false));
    };
    let days = (today - last).num_days().max(0) as f64;
    let decay = 0.5_f64.powf(days / cfg.half_life_days);
    let bias = (state.bias_pct * decay).clamp(-cfg.clamp_pct, cfg.clamp_pct);
    Ok((bias, true))
}

/// 误差带（约 95% 区间半宽）：`1.96 × EWMA(|residual|)`，同样随时间衰减。
pub fn error_band(state: &EwmaState, today: NaiveDate, cfg: &EwmaConfig) -> f64 {
    let Some(last) = state.last_date else {
        return 0.0;
    };
    let days = (today - last).num_days().max(0) as f64;
    let decay = 0.5_f64.powf(days / cfg.half_life_days.max(1e-6));
    (1.96 * state.abs_pct * decay).min(0.20)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, day).unwrap()
    }

    #[test]
    fn first_observation_initialises() {
        let cfg = EwmaConfig::default();
        let obs = Observation {
            trade_date: d(2),
            actual_pct: 0.012,
            raw_estimated_pct: 0.010,
        };
        let out = update(&EwmaState::empty(), &obs, &cfg).unwrap();
        assert!(out.accepted);
        assert!((out.state.bias_pct - 0.002).abs() < 1e-12);
        assert!((out.state.abs_pct - 0.002).abs() < 1e-12);
        assert_eq!(out.state.n_obs, 1);
        assert!(!out.active, "1 次观测不足 min_obs=3");
    }

    #[test]
    fn ewma_converges_to_persistent_bias() {
        let cfg = EwmaConfig::default();
        let mut state = EwmaState::empty();
        for day in 2..=8 {
            let obs = Observation {
                trade_date: d(day),
                actual_pct: 0.011,
                raw_estimated_pct: 0.010,
            };
            state = update(&state, &obs, &cfg).unwrap().state;
        }
        // 持续 +0.1% 的系统性低估，EWMA 应逼近 0.001
        assert!((state.bias_pct - 0.001).abs() < 1e-6);
        assert_eq!(state.n_obs, 7);
    }

    #[test]
    fn bias_is_inactive_below_min_obs() {
        let cfg = EwmaConfig::default();
        let state = EwmaState {
            bias_pct: 0.002,
            abs_pct: 0.002,
            n_obs: 2,
            last_date: Some(d(5)),
        };
        assert_eq!(apply(&state, d(6), &cfg).unwrap(), (0.0, false));
    }

    #[test]
    fn bias_decays_with_half_life() {
        let cfg = EwmaConfig::default();
        let state = EwmaState {
            bias_pct: 0.002,
            abs_pct: 0.002,
            n_obs: 10,
            last_date: Some(d(1)),
        };
        let (bias, active) = apply(&state, d(11), &cfg).unwrap(); // 正好一个半衰期
        assert!(active);
        assert!((bias - 0.001).abs() < 1e-12);
    }

    #[test]
    fn bias_is_clamped() {
        let cfg = EwmaConfig::default();
        let state = EwmaState {
            bias_pct: 0.05,
            abs_pct: 0.05,
            n_obs: 10,
            last_date: Some(d(5)),
        };
        let (bias, _) = apply(&state, d(5), &cfg).unwrap();
        assert!((bias - cfg.clamp_pct).abs() < 1e-12);
    }

    #[test]
    fn outlier_is_rejected() {
        let cfg = EwmaConfig::default();
        let state = EwmaState {
            bias_pct: 0.001,
            abs_pct: 0.001,
            n_obs: 5,
            last_date: Some(d(5)),
        };
        let obs = Observation {
            trade_date: d(6),
            actual_pct: 0.08,
            raw_estimated_pct: 0.0,
        };
        let out = update(&state, &obs, &cfg).unwrap();
        assert!(!out.accepted);
        assert_eq!(out.state, state);
    }

    #[test]
    fn forward_fill_today_before_first_observation() {
        let cfg = EwmaConfig::default();
        let (bias, active) = apply(&EwmaState::empty(), d(6), &cfg).unwrap();
        assert_eq!((bias, active), (0.0, false));
    }

    #[test]
    fn error_band_tracks_abs_residual() {
        let cfg = EwmaConfig::default();
        let state = EwmaState {
            bias_pct: 0.0,
            abs_pct: 0.004,
            n_obs: 10,
            last_date: Some(d(5)),
        };
        let band = error_band(&state, d(5), &cfg);
        assert!((band - 1.96 * 0.004).abs() < 1e-12);
    }
}
