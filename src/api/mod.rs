//! HTTP 适配层（axum）。
//!
//! 本层只做四件事：取参数、查库、调纯函数、序列化。任何业务判断都不允许
//! 写在这里——它应该落在 `domain` 里并能被单测覆盖。
//!
//! 当前是 MVP 的第一个切片：健康检查、基金列表、单只基金实时估算、
//! 全组合穿透矩阵。SSE 推送和 HTMX 页面在其后的迭代里挂到同一个 Router 上。

use std::{collections::HashMap, sync::Arc};

use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use chrono::{Local, NaiveDate};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::{
    config::Config,
    db::repo,
    domain::{
        lookthrough::LookthroughReport,
        types::ValuationMode,
        valuation::{Contribution, Flag},
    },
    error::AppError,
    service, ui,
};

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub cfg: Arc<Config>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/api/funds", get(list_funds))
        .route("/api/estimates", get(fund_estimates))
        .route("/api/funds/{code}/estimate", get(fund_estimate))
        .route("/api/lookthrough", get(lookthrough_report))
        .merge(ui::routes())
        .with_state(state)
}

// ---------------------------------------------------------------------------
// 健康检查
// ---------------------------------------------------------------------------

async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    repo::ping(&state.db).await?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

// ---------------------------------------------------------------------------
// 全部基金估值（给 Agent/简报用的批量视图，与 Dashboard 共用 overview_all）
// ---------------------------------------------------------------------------

async fn fund_estimates(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let quotes: HashMap<String, f64> = repo::all_quotes(&state.db)
        .await?
        .into_iter()
        .map(|q| (q.instrument_code, q.pct_chg))
        .collect();
    let now = Local::now();
    let (funds, skipped) =
        service::overview_all(&state.db, &state.cfg, now.date_naive(), &quotes).await?;
    Ok(Json(serde_json::json!({
        "as_of": now.to_rfc3339(),
        "count": funds.len(),
        "funds": funds,
        "skipped": skipped,
    })))
}

// ---------------------------------------------------------------------------
// 基金列表
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct FundListItem {
    pub code: String,
    pub name: String,
    pub mode: String,
    pub category: Option<String>,
    pub benchmark_code: Option<String>,
    pub latest_nav: Option<f64>,
    pub latest_nav_date: Option<NaiveDate>,
}

async fn list_funds(State(state): State<AppState>) -> Result<Json<Vec<FundListItem>>, AppError> {
    let funds = repo::list_funds(&state.db).await?;
    let mut items = Vec::with_capacity(funds.len());
    for fund in funds {
        let nav = repo::latest_nav(&state.db, &fund.code).await?;
        items.push(FundListItem {
            code: fund.code,
            name: fund.name,
            mode: fund.mode,
            category: fund.category,
            benchmark_code: fund.benchmark_code,
            latest_nav: nav.as_ref().map(|n| n.unit_nav),
            latest_nav_date: nav.as_ref().map(|n| n.trade_date),
        });
    }
    Ok(Json(items))
}

// ---------------------------------------------------------------------------
// 单只基金实时估算
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct EstimateResponse {
    pub fund_code: String,
    pub fund_name: String,
    pub mode: ValuationMode,
    pub as_of: String,
    pub prev_nav: f64,
    pub prev_nav_date: NaiveDate,
    pub estimated_nav: f64,
    pub estimated_pct: f64,
    pub raw_pct: f64,
    pub bias_pct: f64,
    pub coverage: f64,
    pub confidence: f64,
    pub flags: Vec<Flag>,
    pub contributions: Vec<Contribution>,
    /// QDII 专用：美股 T-1 基准与汇率明细；境内基金为 null。
    pub qdii: Option<service::QdiiDetail>,
}

async fn fund_estimate(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<EstimateResponse>, AppError> {
    // 盘中用 quote_snapshot（实时快照）；盘后 validate 用 quote_daily。
    // 两者共用 service::estimate_fund，保证「看到的」与「验证的」是同一套口径。
    let quotes: HashMap<String, f64> = repo::all_quotes(&state.db)
        .await?
        .into_iter()
        .map(|q| (q.instrument_code, q.pct_chg))
        .collect();
    let today = chrono::Local::now().date_naive();
    let ctx = service::estimate_fund(&state.db, &state.cfg, &code, today, &quotes).await?;
    let estimate = ctx.estimate;

    Ok(Json(EstimateResponse {
        fund_code: ctx.fund.code,
        fund_name: ctx.fund.name,
        mode: estimate.mode,
        as_of: chrono::Local::now().to_rfc3339(),
        prev_nav: estimate.prev_nav,
        prev_nav_date: ctx.prev_nav.trade_date,
        // 只裁展示精度，计算与落库仍用全精度；否则 JSON 里会出现
        // 0.8799999999999999 这种浮点噪声，没法肉眼核对。
        estimated_nav: round(estimate.estimated_nav, 6),
        estimated_pct: round(estimate.estimated_pct, 8),
        raw_pct: round(estimate.raw_pct, 8),
        bias_pct: round(estimate.bias_pct, 8),
        coverage: round(estimate.coverage, 6),
        confidence: round(estimate.confidence, 4),
        flags: estimate.flags,
        contributions: estimate
            .contributions
            .into_iter()
            .map(|mut c| {
                c.weight = round(c.weight, 6);
                c.pct = round(c.pct, 8);
                c.contribution = round(c.contribution, 8);
                c
            })
            .collect(),
        qdii: ctx.qdii,
    }))
}

/// 把 `x` 四舍五入到 `digits` 位小数，仅用于响应展示。
fn round(x: f64, digits: i32) -> f64 {
    let f = 10f64.powi(digits);
    (x * f).round() / f
}

// ---------------------------------------------------------------------------
// 穿透重叠矩阵
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct LookthroughResponse {
    pub as_of: String,
    pub threshold: f64,
    /// 是否因为 `position` 表为空而退化成等权假设（此时矩阵只能看相对结构）。
    pub weights_assumed_equal: bool,
    pub report: LookthroughReport,
}

async fn lookthrough_report(
    State(state): State<AppState>,
) -> Result<Json<LookthroughResponse>, AppError> {
    let view = service::lookthrough_view(&state.db, &state.cfg).await?;

    Ok(Json(LookthroughResponse {
        as_of: chrono::Local::now().to_rfc3339(),
        threshold: state.cfg.overlap_alert_pct,
        weights_assumed_equal: view.weights_assumed_equal,
        report: view.report,
    }))
}
