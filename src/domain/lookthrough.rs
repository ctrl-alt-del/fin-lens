//! 穿透重叠矩阵（Look-Through）。
//!
//! 这是本项目真正要回答的问题：多只基金名字不同，但穿透到底层持仓后，
//! 光模块/算力硬件是不是已经成了同一个赌注？
//!
//! 组合层面单一标的的真实权重：
//!
//! ```text
//! exposure(i) = Σ_fund  组合持仓权重(fund) × 该基金持有 i 的净值占比
//! ```
//!
//! 注意「组合持仓权重」不是基金净值的权重，而是**你自己**在这只基金上投入的
//! 市值占比——否则多只基金等权假设会严重误导（实际你很可能重仓了其中几只）。
//! 没有录入 `position` 时调用方应显式传入等权，并让 API 把它标成 normalized。

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

/// 你在单只基金上的组合权重。所有基金权重之和应为 1（不含现金）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PositionWeight {
    pub fund: String,
    pub weight: f64,
}

impl PositionWeight {
    pub fn new(fund: impl Into<String>, weight: f64) -> Self {
        Self {
            fund: fund.into(),
            weight,
        }
    }
}

/// 某只基金的一条底层持仓（带元数据，供行业汇总）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HoldingLine {
    pub instrument: String,
    pub name: String,
    pub sector: Option<String>,
    /// 占该基金净值的比例。
    pub weight: f64,
}

/// 单只基金对你的组合贡献了多少。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FundSlice {
    pub fund: String,
    /// 该基金通过此标的贡献的组合权重。
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Exposure {
    pub instrument: String,
    pub name: String,
    pub sector: Option<String>,
    /// 组合层面穿透权重，小数。
    pub total_weight: f64,
    /// 有多少只基金持有它。
    pub fund_count: usize,
    pub funds: Vec<FundSlice>,
    /// 是否越过告警阈值。
    pub is_alert: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OverlapAlert {
    pub instrument: String,
    pub name: String,
    pub total_weight: f64,
    pub threshold: f64,
    pub severity: String,
    pub funds: Vec<FundSlice>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SectorExposure {
    pub sector: String,
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LookthroughReport {
    /// 输入权重之和（归一化前）。
    pub total_position_weight: f64,
    /// 是否因为权重之和不为 1 而做了归一化。
    pub normalized: bool,
    /// 有持仓数据覆盖的组合权重之和（基金级）。
    pub resolved_weight: f64,
    /// 单票/单标的归因到的组合权重之和（Σ 暴露权重）。
    ///
    /// `resolved_weight` 只说明"这只基金有没有持仓行"，而 ETF 联接基只展开
    /// 前十大成分时，基金是 100% 覆盖但单票只归因到 50% 左右。两个数字必须分开，
    /// 否则 UI 会把"前十大口径"说成"完整穿透"。
    pub attributed_weight: f64,
    /// 有仓位但没有持仓数据的基金（估值时会被当作盲区）。
    pub unresolved_funds: Vec<String>,
    pub exposures: Vec<Exposure>,
    pub alerts: Vec<OverlapAlert>,
    pub sectors: Vec<SectorExposure>,
    /// 底层标的的赫芬达尔指数，越高越集中。
    pub hhi: f64,
    pub top_instrument: Option<String>,
}

/// 计算穿透暴露。纯函数：输入相同，输出一定相同。
///
/// * `positions` —— 各基金占你组合的市值权重（和为 1，允许轻微误差）。
/// * `holdings` —— 基金代码 → 最新季报持仓。
/// * `alert_threshold` —— 单一标的组合权重告警线，例如 0.05。
pub fn compute(
    positions: &[PositionWeight],
    holdings: &HashMap<String, Vec<HoldingLine>>,
    alert_threshold: f64,
) -> LookthroughReport {
    let total_position_weight: f64 = positions
        .iter()
        .filter(|p| p.weight.is_finite() && p.weight > 0.0)
        .map(|p| p.weight)
        .sum();

    // 归一化：录入时漏了某只基金或含现金都会让和 != 1，
    // 静默归一化并打标记，比直接算出一个偏小 20% 的暴露要诚实。
    let factor = if total_position_weight > 0.0 {
        1.0 / total_position_weight
    } else {
        0.0
    };
    let normalized = total_position_weight > 0.0 && (total_position_weight - 1.0).abs() > 1e-6;

    // BTreeMap 保证同权重时输出顺序稳定，便于做快照 diff。
    let mut aggregate: BTreeMap<String, ExposureBuilder> = BTreeMap::new();
    let mut resolved_weight = 0.0_f64;
    let mut unresolved_funds = Vec::new();

    for position in positions {
        if !position.weight.is_finite() || position.weight <= 0.0 {
            continue;
        }
        let fund_weight = position.weight * factor;
        match holdings.get(&position.fund) {
            Some(lines) if !lines.is_empty() => {
                resolved_weight += fund_weight;
                for line in lines {
                    if !line.weight.is_finite() || line.weight <= 0.0 {
                        continue;
                    }
                    let contribution = fund_weight * line.weight;
                    let builder = aggregate.entry(line.instrument.clone()).or_insert_with(|| {
                        ExposureBuilder {
                            name: line.name.clone(),
                            sector: line.sector.clone(),
                            total_weight: 0.0,
                            funds: Vec::new(),
                        }
                    });
                    builder.total_weight += contribution;
                    builder.funds.push(FundSlice {
                        fund: position.fund.clone(),
                        weight: contribution,
                    });
                }
            }
            _ => unresolved_funds.push(position.fund.clone()),
        }
    }
    unresolved_funds.sort();
    unresolved_funds.dedup();

    let mut exposures: Vec<Exposure> = aggregate
        .into_iter()
        .map(|(instrument, builder)| {
            let mut funds = builder.funds;
            funds.sort_by(|a, b| b.weight.partial_cmp(&a.weight).unwrap_or(Ordering::Equal));
            Exposure {
                instrument,
                name: builder.name,
                sector: builder.sector,
                total_weight: builder.total_weight,
                fund_count: funds.len(),
                is_alert: builder.total_weight >= alert_threshold,
                funds,
            }
        })
        .collect();
    exposures.sort_by(|a, b| {
        b.total_weight
            .partial_cmp(&a.total_weight)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.instrument.cmp(&b.instrument))
    });

    let hhi = exposures
        .iter()
        .map(|e| e.total_weight * e.total_weight)
        .sum::<f64>();

    let alerts: Vec<OverlapAlert> = exposures
        .iter()
        .filter(|e| e.is_alert)
        .map(|e| OverlapAlert {
            instrument: e.instrument.clone(),
            name: e.name.clone(),
            total_weight: e.total_weight,
            threshold: alert_threshold,
            // 超过阈值两倍视为 critical：这不是分散，是同一笔仓位换了个名字。
            severity: if e.total_weight >= alert_threshold * 2.0 {
                "critical".to_string()
            } else {
                "warn".to_string()
            },
            funds: e.funds.clone(),
        })
        .collect();

    let mut sectors: Vec<SectorExposure> = {
        let mut map: BTreeMap<String, f64> = BTreeMap::new();
        for exposure in &exposures {
            let key = exposure
                .sector
                .clone()
                .unwrap_or_else(|| "未分类".to_string());
            *map.entry(key).or_insert(0.0) += exposure.total_weight;
        }
        map.into_iter()
            .map(|(sector, weight)| SectorExposure { sector, weight })
            .collect()
    };
    sectors.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.sector.cmp(&b.sector))
    });

    let attributed_weight: f64 = exposures.iter().map(|e| e.total_weight).sum();

    LookthroughReport {
        total_position_weight,
        normalized,
        resolved_weight,
        attributed_weight,
        unresolved_funds,
        top_instrument: exposures.first().map(|e| e.instrument.clone()),
        exposures,
        alerts,
        sectors,
        hhi,
    }
}

struct ExposureBuilder {
    name: String,
    sector: Option<String>,
    total_weight: f64,
    funds: Vec<FundSlice>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(instrument: &str, sector: &str, weight: f64) -> HoldingLine {
        HoldingLine {
            instrument: instrument.to_string(),
            name: instrument.to_string(),
            sector: Some(sector.to_string()),
            weight,
        }
    }

    #[test]
    fn two_funds_overlapping_on_one_name() {
        let positions = vec![
            PositionWeight::new("F1", 0.6),
            PositionWeight::new("F2", 0.4),
        ];
        let holdings = HashMap::from([
            (
                "F1".to_string(),
                vec![line("X", "光模块", 0.5), line("Y", "食品", 0.5)],
            ),
            (
                "F2".to_string(),
                vec![line("X", "光模块", 0.3), line("Z", "半导体", 0.7)],
            ),
        ]);

        let report = compute(&positions, &holdings, 0.05);
        let x = report
            .exposures
            .iter()
            .find(|e| e.instrument == "X")
            .unwrap();
        assert!((x.total_weight - 0.42).abs() < 1e-12);
        assert_eq!(x.fund_count, 2);
        assert!(x.is_alert);
        assert!((report.hhi - (0.42 * 0.42 + 0.3 * 0.3 + 0.28 * 0.28)).abs() < 1e-12);
        assert!((report.attributed_weight - 1.0).abs() < 1e-12);
        assert_eq!(report.exposures.first().unwrap().instrument, "X");
        assert_eq!(report.sectors.first().unwrap().sector, "光模块");
        // 三只票各自穿透 0.42 / 0.30 / 0.28，都超过 5% 阈值，因此三条告警；
        // 排在首位的是暴露最大的 X（0.42 ≥ 2×阈值 → critical）。
        assert_eq!(report.alerts.len(), 3);
        assert_eq!(report.alerts[0].instrument, "X");
        assert_eq!(report.alerts[0].severity, "critical");
        assert!(!report.normalized);
    }

    #[test]
    fn weights_are_normalised_when_sum_is_not_one() {
        let positions = vec![PositionWeight::new("F1", 0.5)];
        let holdings = HashMap::from([("F1".to_string(), vec![line("X", "光模块", 0.5)])]);
        let report = compute(&positions, &holdings, 0.05);
        assert!(report.normalized);
        assert!((report.exposures[0].total_weight - 0.5).abs() < 1e-12);
        assert!((report.attributed_weight - 0.5).abs() < 1e-12);
        assert!((report.resolved_weight - 1.0).abs() < 1e-12);
    }

    #[test]
    fn fund_without_holdings_is_reported_unresolved() {
        let positions = vec![
            PositionWeight::new("F1", 0.5),
            PositionWeight::new("F2", 0.5),
        ];
        let holdings = HashMap::from([("F1".to_string(), vec![line("X", "光模块", 0.4)])]);
        let report = compute(&positions, &holdings, 0.05);
        assert_eq!(report.unresolved_funds, vec!["F2".to_string()]);
        assert!((report.resolved_weight - 0.5).abs() < 1e-12);
    }

    #[test]
    fn empty_positions_do_not_panic() {
        let report = compute(&[], &HashMap::new(), 0.05);
        assert!(report.exposures.is_empty());
        assert!(report.top_instrument.is_none());
        assert_eq!(report.hhi, 0.0);
    }

    #[test]
    fn below_threshold_is_not_alert() {
        let positions = vec![PositionWeight::new("F1", 1.0)];
        let holdings = HashMap::from([("F1".to_string(), vec![line("X", "半导体", 0.02)])]);
        let report = compute(&positions, &holdings, 0.05);
        assert!(!report.exposures[0].is_alert);
        assert!(report.alerts.is_empty());
    }
}
