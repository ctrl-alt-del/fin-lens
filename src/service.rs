//! 估值装配层：把「数据库事实 + 当日行情」翻译成内核参数。
//!
//! API 与 `validate` 子命令必须走同一条装配路径，否则「实时看到的估算」
//! 和「盘后验证的估算」会悄悄分叉，校准闭环就失去意义。
//!
//! 本层允许 I/O（查库），但不做任何业务判断；业务全部在 `domain` 里。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{Duration, NaiveDate};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::{
    config::Config,
    db::repo,
    domain::{
        calibration::{self, EwmaState},
        lookthrough::{self, HoldingLine, LookthroughReport, PositionWeight},
        risk::{self, SeriesPoint},
        types::{HoldingWeight, ValuationMode},
        valuation::{
            self, BiasInput, Estimate, Flag, QdiiParams, SemiTransparentParams, TransparentParams,
        },
    },
    error::AppError,
};

/// 一次估算的完整上下文（含所用的前一日净值，便于展示与审计）。
pub struct EstimateContext {
    pub fund: repo::FundRow,
    pub prev_nav: repo::NavRow,
    pub estimate: Estimate,
    /// QDII 专用明细（T-1 基准涨跌、汇率、β），非 QDII 为 None。
    pub qdii: Option<QdiiDetail>,
}

/// QDII 估值的可审计明细，直接进 API 与 Dashboard。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QdiiDetail {
    /// 美股基准代码（当前为 US.XOP，标普油气开采指数 ETF 代理）。
    pub benchmark: String,
    /// 上一夜美股时段涨跌，小数。
    pub us_pct: f64,
    /// 同期 USD/CNY 变动（正 = 人民币贬值），缺失时 None。
    pub fx_pct: Option<f64>,
    /// 实际使用的跟涨系数。
    pub beta: f64,
    /// 行情滞后交易日数（当前固定 1）。
    pub lag_days: u32,
}

/// 以 `as_of` 为估值日，用给定的「标的 → 当日涨跌」映射估算一只基金。
///
/// `prev_nav` 严格取 `as_of` 之前最近一个净值：盘后当天净值尚未公布时，
/// 这正好是昨日净值；回补历史某天时也不会看到未来数据。
pub async fn estimate_fund(
    pool: &SqlitePool,
    cfg: &Config,
    code: &str,
    as_of: NaiveDate,
    quotes: &HashMap<String, f64>,
) -> Result<EstimateContext, AppError> {
    let fund = repo::get_fund(pool, code)
        .await?
        .ok_or_else(|| AppError::not_found(format!("基金不存在或已停用: {code}")))?;
    let mode = ValuationMode::from_db(&fund.mode)
        .ok_or_else(|| AppError::bad_request(format!("未知估值模式: {}", fund.mode)))?;
    let prev_nav = repo::nav_before(pool, code, as_of)
        .await?
        .ok_or_else(|| AppError::not_found(format!("基金 {code} 在 {as_of} 之前还没有净值数据")))?;

    let bias = bias_input(pool, cfg, code, as_of).await?;

    // 跨境 QDII 走独立分支：DB 里 mode 仍是 transparent（SQLite CHECK 不轻易重建），
    // 用 fund.category 判定，避免为了一个产品类型迁移整张表。
    if mode == ValuationMode::Qdii || fund.category.as_deref() == Some("qdii_index") {
        let benchmark = fund.benchmark_code.clone().ok_or_else(|| {
            AppError::bad_request(format!("QDII 基金 {code} 未配置美股基准 benchmark_code"))
        })?;
        let us_pct = quotes.get(&benchmark).copied().ok_or_else(|| {
            AppError::bad_request(format!(
                "QDII 基金 {code} 缺少美股基准 T-1 行情: {benchmark}"
            ))
        })?;
        let fx_pct = quotes.get("FX.USDCNH").copied();
        let beta = repo::latest_beta(pool, code)
            .await?
            .or(fund.tracking_beta)
            .unwrap_or(1.0);
        let detail = QdiiDetail {
            benchmark: benchmark.clone(),
            us_pct,
            fx_pct,
            beta,
            lag_days: 1,
        };
        let est = valuation::estimate_qdii(
            prev_nav.unit_nav,
            &QdiiParams {
                benchmark_pct: Some(us_pct),
                beta: Some(beta),
                fx_pct,
                lag_days: 1,
            },
            bias,
        )?;
        return Ok(EstimateContext {
            fund,
            prev_nav,
            estimate: est,
            qdii: Some(detail),
        });
    }

    let estimate = match mode {
        ValuationMode::Transparent => {
            let benchmark = fund.benchmark_code.as_deref().ok_or_else(|| {
                AppError::bad_request(format!("透明资产基金 {code} 未配置 benchmark_code"))
            })?;
            let beta = repo::latest_beta(pool, code).await?.or(fund.tracking_beta);
            valuation::estimate_transparent(
                prev_nav.unit_nav,
                &TransparentParams {
                    benchmark_pct: quotes.get(benchmark).copied(),
                    beta,
                    fx_pct: 0.0,
                    bias,
                },
            )?
        }
        ValuationMode::SemiTransparent => {
            let holdings = repo::latest_holdings(pool, code)
                .await?
                .into_iter()
                .map(|h| HoldingWeight::new(h.instrument_code, h.name, h.weight))
                .collect();
            let stock_weight = repo::latest_stock_weight(pool, code).await?;
            let residual_pct = fund
                .benchmark_code
                .as_deref()
                .and_then(|b| quotes.get(b).copied());
            valuation::estimate_semi_transparent(
                prev_nav.unit_nav,
                &SemiTransparentParams {
                    holdings,
                    quotes: quotes.clone(),
                    stock_weight,
                    residual_pct,
                    bias,
                },
            )?
        }
        ValuationMode::Opaque => {
            let benchmark_pct = fund
                .benchmark_code
                .as_deref()
                .and_then(|b| quotes.get(b).copied());
            valuation::estimate_opaque_fallback(prev_nav.unit_nav, benchmark_pct, bias)?
        }
        // QDII 在上面按 category/mode 提前返回，不会走到这里。
        ValuationMode::Qdii => unreachable!("QDII 已由专用分支处理"),
    };

    Ok(EstimateContext {
        fund,
        prev_nav,
        estimate,
        qdii: None,
    })
}

/// 取「`as_of` 之前」最近一次校准状态并做时间衰减。
///
/// 校准参数异常不应该拖垮估值：任何错误都降级为「本次不校准」。
async fn bias_input(
    pool: &SqlitePool,
    cfg: &Config,
    code: &str,
    as_of: NaiveDate,
) -> Result<BiasInput, AppError> {
    let Some(row) = repo::latest_calibration_before(pool, code, as_of).await? else {
        return Ok(BiasInput::NONE);
    };
    let state = EwmaState {
        bias_pct: row.ewma_bias_pct,
        abs_pct: row.ewma_abs_pct,
        n_obs: row.n_obs.max(0) as u32,
        last_date: Some(row.trade_date),
    };
    match calibration::apply(&state, as_of, &cfg.ewma) {
        Ok((pct, true)) => Ok(BiasInput::applied(pct)),
        Ok((_, false)) => Ok(BiasInput::NONE),
        Err(err) => {
            tracing::warn!(fund = %code, %err, "bias evaluation failed, fallback to zero");
            Ok(BiasInput::NONE)
        }
    }
}

/// Dashboard 用的单只基金摘要：只保留页面需要的字段，避免把整份
/// `Estimate`（含每条持仓贡献）序列化进 HTML。
#[derive(Debug, Clone, Serialize)]
pub struct FundOverview {
    pub code: String,
    pub name: String,
    pub mode: ValuationMode,
    pub prev_nav: f64,
    pub prev_nav_date: NaiveDate,
    pub estimated_nav: f64,
    pub estimated_pct: f64,
    pub raw_pct: f64,
    pub bias_pct: f64,
    pub coverage: f64,
    pub confidence: f64,
    pub flags: Vec<Flag>,
    pub qdii: Option<QdiiDetail>,
}

/// 批量估算全部在售基金；单只失败不影响整页（跳过并记录原因）。
pub async fn overview_all(
    pool: &SqlitePool,
    cfg: &Config,
    as_of: NaiveDate,
    quotes: &HashMap<String, f64>,
) -> Result<(Vec<FundOverview>, Vec<(String, String)>), AppError> {
    let mut funds = Vec::new();
    let mut skipped = Vec::new();
    for fund in repo::list_funds(pool).await? {
        match estimate_fund(pool, cfg, &fund.code, as_of, quotes).await {
            Ok(ctx) => funds.push(FundOverview {
                code: ctx.fund.code,
                name: ctx.fund.name,
                mode: ctx.estimate.mode,
                prev_nav: ctx.prev_nav.unit_nav,
                prev_nav_date: ctx.prev_nav.trade_date,
                estimated_nav: ctx.estimate.estimated_nav,
                estimated_pct: ctx.estimate.estimated_pct,
                raw_pct: ctx.estimate.raw_pct,
                bias_pct: ctx.estimate.bias_pct,
                coverage: ctx.estimate.coverage,
                confidence: ctx.estimate.confidence,
                flags: ctx.estimate.flags.clone(),
                qdii: ctx.qdii,
            }),
            Err(err) => skipped.push((fund.code, err.to_string())),
        }
    }
    Ok((funds, skipped))
}

/// 穿透矩阵及其权重口径（真实仓位 / 等权退化）。
pub struct LookthroughView {
    pub weights_assumed_equal: bool,
    /// 参与穿透的组合基金数（等权假设时即分母）。
    pub fund_count: usize,
    pub report: LookthroughReport,
}

pub async fn lookthrough_view(
    pool: &SqlitePool,
    cfg: &Config,
) -> Result<LookthroughView, AppError> {
    let mut holdings_map: HashMap<String, Vec<HoldingLine>> = HashMap::new();
    for row in repo::all_latest_holdings(pool).await? {
        holdings_map
            .entry(row.fund_code)
            .or_default()
            .push(HoldingLine {
                instrument: row.instrument_code,
                name: row.name,
                sector: row.sector,
                weight: row.weight,
            });
    }

    let mut positions: Vec<PositionWeight> = repo::positions(pool)
        .await?
        .into_iter()
        .filter_map(|row| row.weight.filter(|w| *w > 0.0).map(|w| (row.fund_code, w)))
        .map(|(fund, weight)| PositionWeight::new(fund, weight))
        .collect();

    let mut weights_assumed_equal = false;
    if positions.is_empty() {
        // 没有录入真实仓位时退化为等权，并明确标记，
        // 避免用户误以为这是自己真实的穿透暴露。
        let mut codes: Vec<String> = holdings_map.keys().cloned().collect();
        codes.sort();
        if !codes.is_empty() {
            let weight = 1.0 / codes.len() as f64;
            positions = codes
                .into_iter()
                .map(|fund| PositionWeight::new(fund, weight))
                .collect();
            weights_assumed_equal = true;
        }
    }

    Ok(LookthroughView {
        weights_assumed_equal,
        fund_count: positions.len(),
        report: lookthrough::compute(&positions, &holdings_map, cfg.overlap_alert_pct),
    })
}

/// 组合层汇总：真实市值、今日估算盈亏、累计浮亏（仅成本已知部分）。
#[derive(Debug, Clone, Default)]
pub struct PortfolioSummary {
    /// position 表里的基金数（不是 fund 表）。
    pub positions: usize,
    /// 本次成功估算的持仓数。
    pub covered: usize,
    /// 今日估算市值（份额 × 今日估算净值）。
    pub total_value: f64,
    /// 按前一日净值的市值（今日盈亏的分母）。
    pub prev_value: f64,
    pub today_pnl: f64,
    pub today_pnl_pct: f64,
    /// 仅统计成本已知的基金；未知成本不能伪装成 0。
    pub cost_basis: Option<f64>,
    pub pnl_since_cost: Option<f64>,
    pub cost_known: usize,
    /// position 里有、但今天估不出来（缺净值/行情）的基金。
    pub missing_estimate: Vec<String>,
    /// 已提交但份额待确认的申购金额（现金在途，不计入市值与今日盈亏）。
    pub pending_cash: f64,
}

impl PortfolioSummary {
    /// 纯函数：把持仓行与当日估算拼成组合汇总。
    pub fn build(funds: &[FundOverview], positions: &[repo::PositionRow]) -> Self {
        let by_code: HashMap<&str, &FundOverview> =
            funds.iter().map(|f| (f.code.as_str(), f)).collect();
        let mut out = Self {
            positions: positions.len(),
            ..Default::default()
        };
        let mut cost_sum = 0.0;
        let mut cost_count = 0usize;
        let mut cost_value = 0.0;

        for row in positions {
            let fallback = row.market_value.unwrap_or(0.0);
            match (row.shares, by_code.get(row.fund_code.as_str())) {
                (Some(shares), Some(fund)) => {
                    let prev = shares * fund.prev_nav;
                    let now = shares * fund.estimated_nav;
                    out.total_value += now;
                    out.prev_value += prev;
                    out.today_pnl += now - prev;
                    out.covered += 1;
                    if let Some(cost) = row.cost_basis {
                        cost_sum += cost;
                        cost_count += 1;
                        cost_value += now;
                    }
                }
                _ => {
                    out.total_value += fallback;
                    out.prev_value += fallback;
                    out.missing_estimate.push(row.fund_code.clone());
                }
            }
        }

        if out.prev_value.abs() > f64::EPSILON {
            out.today_pnl_pct = out.today_pnl / out.prev_value;
        }
        if cost_count > 0 {
            out.cost_basis = Some(cost_sum);
            out.pnl_since_cost = Some(cost_value - cost_sum);
            out.cost_known = cost_count;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::types::ValuationMode;

    fn overview(code: &str, prev_nav: f64, estimated_nav: f64) -> FundOverview {
        FundOverview {
            code: code.to_string(),
            name: code.to_string(),
            mode: ValuationMode::SemiTransparent,
            prev_nav,
            prev_nav_date: NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(),
            estimated_nav,
            estimated_pct: estimated_nav / prev_nav - 1.0,
            raw_pct: estimated_nav / prev_nav - 1.0,
            bias_pct: 0.0,
            coverage: 0.5,
            confidence: 0.5,
            flags: Vec::new(),
            qdii: None,
        }
    }

    fn position(code: &str, shares: f64, cost_basis: Option<f64>) -> repo::PositionRow {
        repo::PositionRow {
            fund_code: code.to_string(),
            shares: Some(shares),
            cost_basis,
            weight: None,
            market_value: None,
        }
    }

    #[test]
    fn summary_totals_ignore_unknown_cost_and_flag_missing_estimate() {
        let funds = vec![overview("A", 1.0, 1.1), overview("B", 2.0, 1.8)];
        let positions = vec![
            position("A", 100.0, Some(90.0)),
            position("B", 50.0, None),
            repo::PositionRow {
                fund_code: "C".to_string(),
                shares: Some(1.0),
                cost_basis: None,
                weight: None,
                market_value: Some(10.0),
            },
        ];
        let s = PortfolioSummary::build(&funds, &positions);
        assert_eq!(s.positions, 3);
        assert_eq!(s.covered, 2);
        assert!((s.total_value - 210.0).abs() < 1e-9, "110 + 90 + 10");
        // 估不出来的持仓按「市值不变」处理，所以分子分母同时 +10。
        assert!((s.prev_value - 210.0).abs() < 1e-9, "100 + 100 + 10");
        assert!((s.today_pnl - 0.0).abs() < 1e-9);
        assert!((s.today_pnl_pct - 0.0).abs() < 1e-9);
        assert_eq!(s.cost_known, 1);
        assert_eq!(s.cost_basis, Some(90.0));
        assert!((s.pnl_since_cost.unwrap() - 20.0).abs() < 1e-9);
        assert_eq!(s.missing_estimate, vec!["C".to_string()]);
    }

    fn fund_row(
        bucket_code: Option<&str>,
        category: Option<&str>,
        etf_code: Option<&str>,
    ) -> repo::FundRow {
        repo::FundRow {
            code: "F".to_string(),
            name: "基金".to_string(),
            mode: "transparent".to_string(),
            category: category.map(str::to_string),
            benchmark_code: None,
            benchmark_name: None,
            etf_code: etf_code.map(str::to_string),
            tracking_beta: None,
            bucket_code: bucket_code.map(str::to_string),
        }
    }

    #[test]
    fn bucket_membership_is_data_driven() {
        assert_eq!(bucket_of(&fund_row(Some("gold"), None, None)).0, "黄金");
        assert_eq!(bucket_of(&fund_row(Some("medical"), None, None)).0, "医药");
        assert_eq!(
            bucket_of(&fund_row(None, Some("active_tech"), None)).0,
            "科技主动"
        );
        assert_eq!(
            bucket_of(&fund_row(None, Some("index_feeder"), Some("ETF.TEST"))).0,
            "未分类"
        );
    }
}

// ---------------------------------------------------------------------------
// 组合历史 / 风险体检（One-pager 用）
// ---------------------------------------------------------------------------

/// 健康度三档：红/黄/绿。没有「分数」，避免制造虚假精度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Good,
    Warn,
    Bad,
}

/// 资源桶切片（用于环形图）。权重按 position.market_value，覆盖全部真实持仓。
#[derive(Debug, Clone)]
pub struct BucketSlice {
    pub label: &'static str,
    pub color: &'static str,
    pub value: f64,
    pub weight: f64,
}

#[derive(Debug, Clone)]
pub struct HealthItem {
    pub label: &'static str,
    pub status: HealthStatus,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct ActionItem {
    pub severity: HealthStatus,
    pub title: String,
    pub detail: String,
}

/// 组合历史视图：当前份额回溯 + 基准 + 穿透篮子 + 体检/待办。
#[derive(Debug, Clone, Default)]
pub struct HistoryView {
    pub available: bool,
    pub start: Option<NaiveDate>,
    /// 归一化到 100 的当前份额组合曲线。
    pub portfolio: Vec<SeriesPoint>,
    pub hs300: Vec<SeriesPoint>,
    pub cyb: Vec<SeriesPoint>,
    /// 当前穿透篮子（仅已解析权重）回溯曲线。
    pub basket: Vec<SeriesPoint>,
    /// 1 周 / 1 月 / 1 季 / 半年 / 1 年。
    pub horizons: Vec<risk::HorizonStats>,
    pub buckets: Vec<BucketSlice>,
    pub health: Vec<HealthItem>,
    pub actions: Vec<ActionItem>,
    /// 单票/单标的归因到的组合权重（ETF 只展开前十大，属下限）。
    pub attributed_weight: f64,
    pub unresolved_funds: usize,
    pub note: String,
    /// 按资源桶的 1 年近似归因（当前份额）。
    pub attribution: Vec<BucketAttribution>,
    /// 确定性判断句：为什么跑赢/跑输，以及哪些比较不可比。
    pub headline: String,
    pub hs300_ret_1y: Option<f64>,
    pub cyb_ret_1y: Option<f64>,
    pub basket_ret_1y: Option<f64>,
}

/// 一个资源桶在 1 年窗口内的近似贡献。
#[derive(Debug, Clone)]
pub struct BucketAttribution {
    pub label: &'static str,
    pub color: &'static str,
    /// 当前市值权重。
    pub weight: f64,
    /// 桶内基金按市值加权的 1 年收益。
    pub ret: f64,
    /// 对组合期初市值的贡献（小数，0.12 = +12pp）。
    pub contribution: f64,
}

/// 组装 One-pager 需要的全部历史数据。时间窗口固定 1 年（约 365 自然日）。
pub async fn history_view(
    pool: &SqlitePool,
    cfg: &Config,
    as_of: NaiveDate,
) -> Result<HistoryView, AppError> {
    let since = as_of - Duration::days(400);
    let positions = repo::positions(pool).await?;
    let funds = repo::list_funds(pool).await?;
    let nav_rows = repo::all_nav_history(pool, since).await?;
    let quote_rows = repo::all_quote_history(pool, since).await?;
    let lt = lookthrough_view(pool, cfg).await?;

    let mut nav_by_fund: HashMap<String, BTreeMap<NaiveDate, f64>> = HashMap::new();
    for row in &nav_rows {
        if row.unit_nav.is_finite() && row.unit_nav > 0.0 {
            nav_by_fund
                .entry(row.fund_code.clone())
                .or_default()
                .insert(row.trade_date, row.unit_nav);
        }
    }
    let mut closes: HashMap<String, BTreeMap<NaiveDate, f64>> = HashMap::new();
    for row in &quote_rows {
        if row.close.is_finite() && row.close > 0.0 {
            closes
                .entry(row.instrument_code.clone())
                .or_default()
                .insert(row.trade_date, row.close);
        }
    }

    let end = nav_rows
        .iter()
        .map(|r| r.trade_date)
        .filter(|d| *d <= as_of)
        .max();
    let portfolio = match end {
        Some(end_date) => build_portfolio_series(&positions, &nav_by_fund, end_date),
        None => Vec::new(),
    };
    let dates: BTreeSet<NaiveDate> = portfolio.iter().map(|p| p.date).collect();

    let hs300 = align_benchmark(&dates, closes.get("IDX.000300"));
    let cyb = align_benchmark(&dates, closes.get("IDX.399006"));

    let exposures: Vec<(String, f64)> = lt
        .report
        .exposures
        .iter()
        .map(|e| (e.instrument.clone(), e.total_weight))
        .collect();
    let basket = build_basket_series(&dates, &exposures, &closes);

    let horizons: Vec<risk::HorizonStats> = [7_i64, 30, 90, 182, 365]
        .into_iter()
        .filter_map(|days| risk::horizon_stats(&portfolio, days))
        .collect();

    let buckets = bucket_slices(&funds, &positions);
    let total_mv: f64 = buckets.iter().map(|b| b.value).sum();

    let one_year = horizons.iter().find(|h| h.days == 365).cloned();
    let attribution = match end {
        Some(end_date) => one_year
            .as_ref()
            .map(|h| attribute_buckets(&funds, &positions, &nav_by_fund, h.start, end_date))
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let hs300_ret_1y = hs300.last().map(|p| p.value / 100.0 - 1.0);
    let cyb_ret_1y = cyb.last().map(|p| p.value / 100.0 - 1.0);
    let basket_ret_1y = basket.last().map(|p| p.value / 100.0 - 1.0);
    let headline = build_headline(
        one_year.as_ref().map(|h| h.ret),
        hs300_ret_1y,
        basket_ret_1y,
        &attribution,
    );

    let gold_nonferrous: f64 = buckets
        .iter()
        .filter(|b| b.label == "黄金" || b.label == "有色")
        .map(|b| b.weight)
        .sum();
    let cluster: f64 = lt
        .report
        .sectors
        .iter()
        .filter(|s| s.sector == "光模块" || s.sector == "光通信")
        .map(|s| s.weight)
        .sum();
    let h1y = horizons.iter().find(|h| h.days == 365);
    let dd = h1y.map(|h| h.max_drawdown);
    let vol = h1y.and_then(|h| h.volatility);

    let mut health = Vec::new();
    health.push(health_item(
        "资源集中度",
        gold_nonferrous,
        (0.25, 0.40),
        format!(
            "黄金+有色 {:.1}%{}",
            gold_nonferrous * 100.0,
            if total_mv > 0.0 {
                format!("（{:.0} 万）", (gold_nonferrous * total_mv) / 10_000.0)
            } else {
                String::new()
            }
        ),
        "把金价/有色回撤直接映射进组合；若这是主动配置，需要写清再平衡线。",
    ));
    health.push(health_item(
        "穿透集中度",
        cluster,
        (0.10, 0.20),
        format!("光模块+光通信 {:.1}%（前十大口径下限）", cluster * 100.0),
        &format!(
            "单票只归因到 {:.1}% 权重（ETF 仅前十大），实际集中度更高；不要被「分散在 {} 只基金」骗到。",
            lt.report.attributed_weight * 100.0,
            funds.len()
        ),
    ));
    health.push(match dd {
        Some(dd) => health_item(
            "近一年回撤",
            dd,
            (0.12, 0.20),
            format!(
                "{:.1}% · 年化波动 {}",
                dd * 100.0,
                vol.map_or("—".to_string(), |v| format!("{:.1}%", v * 100.0))
            ),
            "回撤是当前份额回溯的结果，不是您账户的真实历史。",
        ),
        None => HealthItem {
            label: "近一年回撤",
            status: HealthStatus::Warn,
            detail: "历史净值不足，无法计算".to_string(),
        },
    });
    health.push(match vol {
        Some(v) => health_item(
            "波动率",
            v,
            (0.25, 0.35),
            format!("年化 {:.1}%", v * 100.0),
            "以日收益标准差估算；集中持仓会显著抬高它。",
        ),
        None => HealthItem {
            label: "波动率",
            status: HealthStatus::Warn,
            detail: "历史净值不足，无法计算".to_string(),
        },
    });
    health.push(health_item(
        "数据完整性",
        lt.report.attributed_weight,
        (0.80, 0.60), // 反向指标：越低越差
        format!(
            "单票归因 {:.1}% · {} 只基金无持仓行",
            lt.report.attributed_weight * 100.0,
            lt.report.unresolved_funds.len()
        ),
        "ETF 联接基只展开前十大，单票归因天然是下限；缺失市值会进一步压低它。",
    ));

    let mut actions = Vec::new();
    let missing_positions = funds.len().saturating_sub(positions.len());
    if missing_positions > 0 {
        actions.push(ActionItem {
            severity: HealthStatus::Bad,
            title: format!("补录 {missing_positions} 只基金的当前持仓市值"),
            detail: format!(
                "单票只归因到 {:.1}% 的权重；没有市值的基金不会进入组合权重，当前集中度被低估。",
                lt.report.attributed_weight * 100.0
            ),
        });
    }
    if gold_nonferrous >= 0.30 {
        actions.push(ActionItem {
            severity: HealthStatus::Bad,
            title: format!(
                "资源桶（黄金+有色）{:.1}%，超过 30% 集中度线",
                gold_nonferrous * 100.0
            ),
            detail:
                "这类资产同受美元/实际利率驱动，回撤相关性高；确认是配置还是套牢，并设再平衡阈值。"
                    .to_string(),
        });
    }
    if cluster >= 0.10 {
        actions.push(ActionItem {
            severity: if cluster >= 0.20 {
                HealthStatus::Bad
            } else {
                HealthStatus::Warn
            },
            title: format!("光模块+光通信穿透 {:.1}%（下限）", cluster * 100.0),
            detail: {
                let top2: f64 = lt.report.exposures.iter()
                    .filter(|e| matches!(e.sector.as_deref(), Some("光模块") | Some("光通信")))
                    .take(2).map(|e| e.total_weight).sum();
                format!(
                    "两只核心票已合计 {:.1}%，且只统计了 {:.1}% 的单票归因权重；季报持仓滞后会掩盖最新调仓。",
                    top2 * 100.0,
                    lt.report.attributed_weight * 100.0
                )
            },
        });
    }
    let cost_known = positions.iter().filter(|p| p.cost_basis.is_some()).count();
    if cost_known < positions.len() {
        actions.push(ActionItem {
            severity: HealthStatus::Warn,
            title: format!("{} 只持仓成本未知", positions.len() - cost_known),
            detail: format!(
                "累计浮亏只覆盖成本已知的 {} 只，不要把它当成账户总盈亏。",
                cost_known
            ),
        });
    }
    actions.sort_by_key(|a| match a.severity {
        HealthStatus::Bad => 0,
        HealthStatus::Warn => 1,
        HealthStatus::Good => 2,
    });
    actions.truncate(4);

    let note = match end {
        Some(end_date) => format!("当前份额回溯，非真实历史收益；净值截至 {end_date}。"),
        None => "暂无历史净值，先跑 collector/build_history.py 并 seed。".to_string(),
    };

    Ok(HistoryView {
        available: portfolio.len() >= 2,
        start: portfolio.first().map(|p| p.date),
        portfolio,
        hs300,
        cyb,
        basket,
        horizons,
        buckets,
        health,
        actions,
        attributed_weight: lt.report.attributed_weight,
        unresolved_funds: lt.report.unresolved_funds.len(),
        note,
        attribution,
        headline,
        hs300_ret_1y,
        cyb_ret_1y,
        basket_ret_1y,
    })
}

/// 健康度：数值越高越差时用 `(warn, bad)` 正向阈值；数据完整性这类
/// 「越高越好」的指标，调用方传反向阈值即可（这里用 `reverse` 语义太隐晦，
/// 直接由调用方给出正确的比较方向）。
fn health_item(
    label: &'static str,
    value: f64,
    (warn, bad): (f64, f64),
    detail: String,
    _advice: &str,
) -> HealthItem {
    // 对「越低越差」的指标（覆盖率），调用方会传 (0.80, 0.60)：warn > bad。
    let status = if warn > bad {
        if value <= bad {
            HealthStatus::Bad
        } else if value <= warn {
            HealthStatus::Warn
        } else {
            HealthStatus::Good
        }
    } else if value >= bad {
        HealthStatus::Bad
    } else if value >= warn {
        HealthStatus::Warn
    } else {
        HealthStatus::Good
    };
    HealthItem {
        label,
        status,
        detail,
    }
}

fn build_portfolio_series(
    positions: &[repo::PositionRow],
    nav_by_fund: &HashMap<String, BTreeMap<NaiveDate, f64>>,
    end: NaiveDate,
) -> Vec<SeriesPoint> {
    let held: Vec<(&str, f64)> = positions
        .iter()
        .filter_map(|row| {
            let shares = row.shares?;
            if shares <= 0.0 || !nav_by_fund.contains_key(&row.fund_code) {
                return None;
            }
            Some((row.fund_code.as_str(), shares))
        })
        .collect();
    if held.len() < 2 {
        return Vec::new();
    }
    let mut dates = BTreeSet::new();
    for (code, _) in &held {
        if let Some(map) = nav_by_fund.get(*code) {
            for date in map.keys() {
                if *date <= end {
                    dates.insert(*date);
                }
            }
        }
    }
    let dates: Vec<NaiveDate> = dates.into_iter().collect();
    if dates.len() < 2 {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(dates.len());
    let mut value = 100.0_f64;
    for (i, date) in dates.iter().enumerate() {
        if i > 0 {
            let prev_date = dates[i - 1];
            let mut prev = 0.0;
            let mut now = 0.0;
            for (code, shares) in &held {
                let Some(map) = nav_by_fund.get(*code) else {
                    continue;
                };
                if let (Some(n0), Some(n1)) = (map.get(&prev_date), map.get(date)) {
                    prev += shares * n0;
                    now += shares * n1;
                }
            }
            if prev > 0.0 && now.is_finite() {
                value *= now / prev;
            }
        }
        out.push(SeriesPoint { date: *date, value });
    }
    out
}

fn align_benchmark(
    dates: &BTreeSet<NaiveDate>,
    closes: Option<&BTreeMap<NaiveDate, f64>>,
) -> Vec<SeriesPoint> {
    let Some(closes) = closes else {
        return Vec::new();
    };
    let mut base: Option<f64> = None;
    let mut out = Vec::new();
    for date in dates {
        if let Some((_, close)) = closes.range(..=*date).next_back() {
            let b = *base.get_or_insert(*close);
            if b > 0.0 {
                out.push(SeriesPoint {
                    date: *date,
                    value: close / b * 100.0,
                });
            }
        }
    }
    out
}

fn build_basket_series(
    dates: &BTreeSet<NaiveDate>,
    exposures: &[(String, f64)],
    closes: &HashMap<String, BTreeMap<NaiveDate, f64>>,
) -> Vec<SeriesPoint> {
    let mut pcts: HashMap<&str, BTreeMap<NaiveDate, f64>> = HashMap::new();
    for (code, weight) in exposures {
        if *weight <= 0.0 {
            continue;
        }
        let Some(series) = closes.get(code) else {
            continue;
        };
        let mut map = BTreeMap::new();
        let points: Vec<(&NaiveDate, &f64)> = series.iter().collect();
        for pair in points.windows(2) {
            let (d0, c0) = pair[0];
            let (d1, c1) = pair[1];
            if *c0 > 0.0 {
                map.insert(*d1, c1 / c0 - 1.0);
            }
            let _ = d0;
        }
        pcts.insert(code.as_str(), map);
    }

    let mut out = Vec::with_capacity(dates.len());
    let mut value = 100.0_f64;
    for date in dates {
        let mut num = 0.0;
        let mut den = 0.0;
        for (code, weight) in exposures {
            if let Some(pct) = pcts.get(code.as_str()).and_then(|m| m.get(date)) {
                num += weight * pct;
                den += weight;
            }
        }
        if den > 0.0 {
            value *= 1.0 + num / den;
        }
        out.push(SeriesPoint { date: *date, value });
    }
    out
}

fn bucket_slices(funds: &[repo::FundRow], positions: &[repo::PositionRow]) -> Vec<BucketSlice> {
    let by_code: HashMap<&str, &repo::FundRow> =
        funds.iter().map(|f| (f.code.as_str(), f)).collect();
    let mut order: Vec<(&'static str, &'static str)> = vec![
        ("黄金", "#e3b341"),
        ("有色", "#a371f7"),
        ("原油", "#f0883e"),
        ("半导体指数", "#3fb950"),
        ("宽基", "#58a6ff"),
        ("科技主动", "#ff7b72"),
        ("其他主动", "#8b949e"),
        ("医药", "#ff9bce"),
        ("未分类", "#6e7681"),
    ];
    let mut values: HashMap<&'static str, f64> = HashMap::new();
    for row in positions {
        let Some(fund) = by_code.get(row.fund_code.as_str()) else {
            continue;
        };
        let (label, _) = bucket_of(fund);
        if let Some(mv) = row.market_value {
            if mv > 0.0 {
                *values.entry(label).or_default() += mv;
            }
        }
    }
    let total: f64 = values.values().sum();
    order.retain(|(label, _)| values.get(label).copied().unwrap_or(0.0) > 0.0);
    let mut out: Vec<BucketSlice> = order
        .into_iter()
        .map(|(label, color)| {
            let value = values.get(label).copied().unwrap_or(0.0);
            BucketSlice {
                label,
                color,
                value,
                weight: if total > 0.0 { value / total } else { 0.0 },
            }
        })
        .collect();
    out.sort_by(|a, b| b.value.total_cmp(&a.value));
    out
}

fn bucket_of(fund: &repo::FundRow) -> (&'static str, &'static str) {
    match fund.bucket_code.as_deref() {
        Some("gold") => ("黄金", "#e3b341"),
        Some("nonferrous") => ("有色", "#a371f7"),
        Some("oil") => ("原油", "#f0883e"),
        Some("semiconductor") => ("半导体指数", "#3fb950"),
        Some("medical") => ("医药", "#ff9bce"),
        Some("broad_index") => ("宽基", "#58a6ff"),
        Some("optical") | Some("compute") => ("科技主动", "#ff7b72"),
        Some(_) => ("其他主动", "#8b949e"),
        None if fund.etf_code.is_some() || fund.category.as_deref() == Some("index_feeder") => {
            ("未分类", "#6e7681")
        }
        None if fund.category.as_deref() == Some("active_tech") => ("科技主动", "#ff7b72"),
        None => ("其他主动", "#8b949e"),
    }
}

/// 按资源桶归因：用「当前份额 × 各基金期初/期末净值」计算贡献，
/// 加总正好等于组合区间收益（近似点在于各基金取最近可用净值）。
fn attribute_buckets(
    funds: &[repo::FundRow],
    positions: &[repo::PositionRow],
    nav_by_fund: &HashMap<String, BTreeMap<NaiveDate, f64>>,
    start: NaiveDate,
    end: NaiveDate,
) -> Vec<BucketAttribution> {
    let by_code: HashMap<&str, &repo::FundRow> =
        funds.iter().map(|f| (f.code.as_str(), f)).collect();
    let mut agg: HashMap<&'static str, (f64, f64, &'static str)> = HashMap::new();
    let mut total_start = 0.0_f64;
    for row in positions {
        let Some(shares) = row.shares else {
            continue;
        };
        if shares <= 0.0 {
            continue;
        }
        let Some(fund) = by_code.get(row.fund_code.as_str()) else {
            continue;
        };
        let Some(map) = nav_by_fund.get(&row.fund_code) else {
            continue;
        };
        let Some((_, n0)) = map.range(..=start).next_back() else {
            continue;
        };
        let Some((_, n1)) = map.range(..=end).next_back() else {
            continue;
        };
        if !n0.is_finite() || !n1.is_finite() || *n0 <= 0.0 {
            continue;
        }
        let sv = shares * n0;
        let ev = shares * n1;
        let (label, color) = bucket_of(fund);
        let entry = agg.entry(label).or_insert((0.0, 0.0, color));
        entry.0 += sv;
        entry.1 += ev;
        total_start += sv;
    }
    if total_start <= 0.0 {
        return Vec::new();
    }
    let total_end: f64 = agg.values().map(|(_, ev, _)| *ev).sum();
    let mut out: Vec<BucketAttribution> = agg
        .into_iter()
        .filter(|(_, (sv, _, _))| *sv > 0.0)
        .map(|(label, (sv, ev, color))| BucketAttribution {
            label,
            color,
            weight: if total_end > 0.0 { ev / total_end } else { 0.0 },
            ret: ev / sv - 1.0,
            contribution: (ev - sv) / total_start,
        })
        .collect();
    out.sort_by(|a, b| b.contribution.total_cmp(&a.contribution));
    out
}

/// 生成一句确定性的「所以呢」，不做预测、不给买卖建议。
fn build_headline(
    portfolio_ret: Option<f64>,
    hs300_ret: Option<f64>,
    basket_ret: Option<f64>,
    attribution: &[BucketAttribution],
) -> String {
    let (Some(p), Some(h)) = (portfolio_ret, hs300_ret) else {
        return "历史数据不足，先回填一年净值再谈相对表现。".to_string();
    };
    let excess = p - h;
    if attribution.is_empty() {
        return format!(
            "1 年 {:+.2}%，沪深300 {:+.2}%（超额 {:+.1}pp）；这是当前份额回溯，不是账户真实收益。",
            p * 100.0,
            h * 100.0,
            excess * 100.0
        );
    }
    let top = attribution.first();
    let worst = attribution.last();
    let bucket_note = match (top, worst) {
        (Some(t), Some(w)) if t.label != w.label => format!(
            "主要靠 {} 贡献 {:+.1}pp，{} 拖累 {:+.1}pp",
            t.label,
            t.contribution * 100.0,
            w.label,
            w.contribution * 100.0
        ),
        _ => "各桶贡献相近".to_string(),
    };
    let basket_note = basket_ret
        .map(|b| {
            format!(
                "穿透篮子 1 年 {:+.1}%，不可交易、仅覆盖已披露持仓。",
                b * 100.0
            )
        })
        .unwrap_or_default();
    format!(
        "1 年 {:+.2}%，沪深300 {:+.2}%（超额 {:+.1}pp）：{}。这是当前份额回溯，真实账户盈亏取决于买入时点；沪深300 不含黄金/有色，跨资产比较只作参照。{}",
        p * 100.0,
        h * 100.0,
        excess * 100.0,
        bucket_note,
        basket_note,
    )
}
