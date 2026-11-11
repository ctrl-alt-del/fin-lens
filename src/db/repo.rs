//! 仓储层：**唯一**允许出现 SQL 的地方。
//!
//! 约定：
//! - 只用运行时查询（`query_as`），不用编译期宏（`query!`），否则 `cargo build`
//!   就必须连着数据库，与“单文件 SQLite + 可离线构建”的目标冲突。
//! - 返回的行结构体是数据库投影，不是领域类型；由 `api` 层负责映射到 `domain`。
//! - 所有比率字段在库里已经是小数，不做二次换算。

use chrono::NaiveDate;
use sqlx::SqlitePool;

/// 基金主档投影。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FundRow {
    pub code: String,
    pub name: String,
    pub mode: String,
    pub category: Option<String>,
    pub benchmark_code: Option<String>,
    pub benchmark_name: Option<String>,
    pub etf_code: Option<String>,
    pub tracking_beta: Option<f64>,
    /// 资源桶代码（来自 `bucket_member`）；没有配置时为 None。
    pub bucket_code: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NavRow {
    pub fund_code: String,
    pub trade_date: NaiveDate,
    pub unit_nav: f64,
    pub daily_pct: Option<f64>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HoldingRow {
    pub fund_code: String,
    pub instrument_code: String,
    pub name: String,
    pub sector: Option<String>,
    pub weight: f64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct QuoteRow {
    pub instrument_code: String,
    pub price: f64,
    pub pct_chg: f64,
    pub trade_date: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CalibrationRow {
    pub fund_code: String,
    pub trade_date: NaiveDate,
    pub ewma_bias_pct: f64,
    pub ewma_abs_pct: f64,
    pub n_obs: i64,
    pub lambda: f64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PositionRow {
    pub fund_code: String,
    pub shares: Option<f64>,
    pub cost_basis: Option<f64>,
    pub weight: Option<f64>,
    pub market_value: Option<f64>,
}

/// 连通性探针。
pub async fn ping(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(pool)
        .await?;
    Ok(())
}

pub async fn list_funds(pool: &SqlitePool) -> Result<Vec<FundRow>, sqlx::Error> {
    sqlx::query_as::<_, FundRow>(
        r#"
        SELECT f.code, f.name, f.mode, f.category, f.benchmark_code, f.benchmark_name,
               f.etf_code, f.tracking_beta,
               (
                   SELECT b.code
                   FROM bucket_member bm
                   JOIN bucket b ON b.id = bm.bucket_id
                   WHERE bm.fund_code = f.code
                   ORDER BY bm.bucket_id
                   LIMIT 1
               ) AS bucket_code
        FROM fund f
        WHERE f.is_active = 1
        ORDER BY f.code
        "#,
    )
    .fetch_all(pool)
    .await
}

pub async fn get_fund(pool: &SqlitePool, code: &str) -> Result<Option<FundRow>, sqlx::Error> {
    sqlx::query_as::<_, FundRow>(
        r#"
        SELECT f.code, f.name, f.mode, f.category, f.benchmark_code, f.benchmark_name,
               f.etf_code, f.tracking_beta,
               (
                   SELECT b.code
                   FROM bucket_member bm
                   JOIN bucket b ON b.id = bm.bucket_id
                   WHERE bm.fund_code = f.code
                   ORDER BY bm.bucket_id
                   LIMIT 1
               ) AS bucket_code
        FROM fund f
        WHERE f.code = ?1 AND f.is_active = 1
        "#,
    )
    .bind(code)
    .fetch_optional(pool)
    .await
}

/// 最近一个已公布的单位净值（估值基准）。
pub async fn latest_nav(pool: &SqlitePool, fund_code: &str) -> Result<Option<NavRow>, sqlx::Error> {
    sqlx::query_as::<_, NavRow>(
        r#"
        SELECT fund_code, trade_date, unit_nav, daily_pct
        FROM nav
        WHERE fund_code = ?1
        ORDER BY trade_date DESC
        LIMIT 1
        "#,
    )
    .bind(fund_code)
    .fetch_optional(pool)
    .await
}

/// 最新报告期的十大重仓。
pub async fn latest_holdings(
    pool: &SqlitePool,
    fund_code: &str,
) -> Result<Vec<HoldingRow>, sqlx::Error> {
    sqlx::query_as::<_, HoldingRow>(
        r#"
        SELECT h.fund_code, h.instrument_code,
               COALESCE(i.name, h.instrument_code) AS name,
               i.sector,
               h.weight
        FROM v_latest_holding h
        LEFT JOIN instrument i ON i.code = h.instrument_code
        WHERE h.fund_code = ?1
        ORDER BY h.weight DESC
        "#,
    )
    .bind(fund_code)
    .fetch_all(pool)
    .await
}

/// 所有基金最新报告期持仓（穿透矩阵一次性取全，全部基金 × 前十大，量很小）。
pub async fn all_latest_holdings(pool: &SqlitePool) -> Result<Vec<HoldingRow>, sqlx::Error> {
    sqlx::query_as::<_, HoldingRow>(
        r#"
        SELECT h.fund_code, h.instrument_code,
               COALESCE(i.name, h.instrument_code) AS name,
               i.sector,
               h.weight
        FROM v_latest_holding h
        LEFT JOIN instrument i ON i.code = h.instrument_code
        ORDER BY h.fund_code, h.weight DESC
        "#,
    )
    .fetch_all(pool)
    .await
}

/// 全部行情快照。
///
/// 刻意不做 `IN (...)` 动态拼接：快照表只有百来行，直接全取再在内存里过滤，
/// 比维护动态 SQL 更简单也更难出错。
pub async fn all_quotes(pool: &SqlitePool) -> Result<Vec<QuoteRow>, sqlx::Error> {
    sqlx::query_as::<_, QuoteRow>(
        "SELECT instrument_code, price, pct_chg, trade_date FROM quote_snapshot",
    )
    .fetch_all(pool)
    .await
}

/// 某交易日之前（严格小于）最近一次已落库的校准状态。
///
/// 刻意不用「最新一条」：回补历史某天时，未来日期的校准记录会造成前视偏差。
pub async fn latest_calibration_before(
    pool: &SqlitePool,
    fund_code: &str,
    date: NaiveDate,
) -> Result<Option<CalibrationRow>, sqlx::Error> {
    sqlx::query_as::<_, CalibrationRow>(
        r#"
        SELECT fund_code, trade_date, ewma_bias_pct, ewma_abs_pct, n_obs, lambda
        FROM calibration
        WHERE fund_code = ?1 AND trade_date < ?2
        ORDER BY trade_date DESC
        LIMIT 1
        "#,
    )
    .bind(fund_code)
    .bind(date)
    .fetch_optional(pool)
    .await
}

/// 严格早于 `date` 的最近一个已公布净值（`date` 当天盘后可能尚未公布）。
pub async fn nav_before(
    pool: &SqlitePool,
    fund_code: &str,
    date: NaiveDate,
) -> Result<Option<NavRow>, sqlx::Error> {
    sqlx::query_as::<_, NavRow>(
        r#"
        SELECT fund_code, trade_date, unit_nav, daily_pct
        FROM nav
        WHERE fund_code = ?1 AND trade_date < ?2
        ORDER BY trade_date DESC
        LIMIT 1
        "#,
    )
    .bind(fund_code)
    .bind(date)
    .fetch_optional(pool)
    .await
}

/// 指定交易日的净值（用于「预测 vs 实际」验证）。
pub async fn nav_on(
    pool: &SqlitePool,
    fund_code: &str,
    date: NaiveDate,
) -> Result<Option<NavRow>, sqlx::Error> {
    sqlx::query_as::<_, NavRow>(
        r#"
        SELECT fund_code, trade_date, unit_nav, daily_pct
        FROM nav
        WHERE fund_code = ?1 AND trade_date = ?2
        "#,
    )
    .bind(fund_code)
    .bind(date)
    .fetch_optional(pool)
    .await
}

/// 某交易日的日线涨跌（标的代码 → 小数涨跌），供历史验证使用。
pub async fn quotes_on(
    pool: &SqlitePool,
    date: NaiveDate,
) -> Result<std::collections::HashMap<String, f64>, sqlx::Error> {
    let rows: Vec<(String, f64)> = sqlx::query_as(
        "SELECT instrument_code, pct_chg FROM quote_daily WHERE trade_date = ?1 AND pct_chg IS NOT NULL",
    )
    .bind(date)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// 一日校准结果（写入 `calibration` 表）。
pub struct CalibrationUpsert {
    pub fund_code: String,
    pub trade_date: NaiveDate,
    pub estimated_nav: f64,
    pub actual_nav: f64,
    pub residual_nav: f64,
    pub residual_pct: f64,
    pub ewma_bias_pct: f64,
    pub ewma_abs_pct: f64,
    pub n_obs: u32,
    pub lambda: f64,
    pub outlier: bool,
    pub note: Option<String>,
}

pub async fn upsert_calibration(
    pool: &SqlitePool,
    row: &CalibrationUpsert,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO calibration (
            fund_code, trade_date, estimated_nav, actual_nav, residual_nav, residual_pct,
            ewma_bias_pct, ewma_abs_pct, n_obs, lambda, outlier, note
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        ON CONFLICT(fund_code, trade_date) DO UPDATE SET
            estimated_nav = excluded.estimated_nav,
            actual_nav    = excluded.actual_nav,
            residual_nav  = excluded.residual_nav,
            residual_pct  = excluded.residual_pct,
            ewma_bias_pct = excluded.ewma_bias_pct,
            ewma_abs_pct  = excluded.ewma_abs_pct,
            n_obs         = excluded.n_obs,
            lambda        = excluded.lambda,
            outlier       = excluded.outlier,
            note          = excluded.note
        "#,
    )
    .bind(&row.fund_code)
    .bind(row.trade_date)
    .bind(row.estimated_nav)
    .bind(row.actual_nav)
    .bind(row.residual_nav)
    .bind(row.residual_pct)
    .bind(row.ewma_bias_pct)
    .bind(row.ewma_abs_pct)
    .bind(i64::from(row.n_obs))
    .bind(row.lambda)
    .bind(if row.outlier { 1_i64 } else { 0_i64 })
    .bind(row.note.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

/// 最新回归出的跟涨系数。
pub async fn latest_beta(pool: &SqlitePool, fund_code: &str) -> Result<Option<f64>, sqlx::Error> {
    sqlx::query_scalar::<_, f64>(
        r#"
        SELECT beta FROM tracking_fit
        WHERE fund_code = ?1
        ORDER BY as_of_date DESC
        LIMIT 1
        "#,
    )
    .bind(fund_code)
    .fetch_optional(pool)
    .await
}

/// 最新报告期的权益总仓位。
pub async fn latest_stock_weight(
    pool: &SqlitePool,
    fund_code: &str,
) -> Result<Option<f64>, sqlx::Error> {
    sqlx::query_scalar::<_, f64>(
        r#"
        SELECT stock_weight FROM fund_allocation
        WHERE fund_code = ?1
        ORDER BY report_date DESC
        LIMIT 1
        "#,
    )
    .bind(fund_code)
    .fetch_optional(pool)
    .await
}

pub async fn positions(pool: &SqlitePool) -> Result<Vec<PositionRow>, sqlx::Error> {
    sqlx::query_as::<_, PositionRow>(
        "SELECT fund_code, shares, cost_basis, weight, market_value FROM position ORDER BY fund_code",
    )
    .fetch_all(pool)
    .await
}

/// 某只基金按日期升序的净值序列（回归/回测用）。
pub async fn nav_history(
    pool: &SqlitePool,
    fund_code: &str,
    limit: i64,
) -> Result<Vec<NavRow>, sqlx::Error> {
    sqlx::query_as::<_, NavRow>(
        r#"
        SELECT fund_code, trade_date, unit_nav, daily_pct FROM (
            SELECT fund_code, trade_date, unit_nav, daily_pct
            FROM nav
            WHERE fund_code = ?1
            ORDER BY trade_date DESC
            LIMIT ?2
        ) ORDER BY trade_date ASC
        "#,
    )
    .bind(fund_code)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 某标的按日期升序的日线。
pub async fn quote_history(
    pool: &SqlitePool,
    instrument_code: &str,
    limit: i64,
) -> Result<Vec<(NaiveDate, f64)>, sqlx::Error> {
    let rows: Vec<(String, f64)> = sqlx::query_as(
        r#"
        SELECT trade_date, close FROM (
            SELECT trade_date, close
            FROM quote_daily
            WHERE instrument_code = ?1
            ORDER BY trade_date DESC
            LIMIT ?2
        ) ORDER BY trade_date ASC
        "#,
    )
    .bind(instrument_code)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(raw, close)| {
            NaiveDate::parse_from_str(&raw, "%Y-%m-%d")
                .ok()
                .map(|d| (d, close))
        })
        .collect())
}

/// 最近一次盘中快照的 `(quote_ts, trade_date)`，供 Dashboard 显示数据时点。
/// 最近一期组合快照（用于在途申购现金等）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PortfolioSnapshotRow {
    pub as_of_date: String,
    pub total_value: f64,
    pub cash: f64,
    pub total_cost: Option<f64>,
}

/// 最近一期组合快照。
pub async fn latest_portfolio_snapshot(
    pool: &SqlitePool,
) -> Result<Option<PortfolioSnapshotRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT as_of_date, total_value, cash, total_cost FROM portfolio_snapshot \
         ORDER BY as_of_date DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
}

pub async fn latest_snapshot(pool: &SqlitePool) -> Result<Option<(String, String)>, sqlx::Error> {
    sqlx::query_as("SELECT quote_ts, trade_date FROM quote_snapshot ORDER BY quote_ts DESC LIMIT 1")
        .fetch_optional(pool)
        .await
}

/// 全量净值序列（用于组合回溯），按基金/日期升序。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NavPoint {
    pub fund_code: String,
    pub trade_date: NaiveDate,
    pub unit_nav: f64,
}

pub async fn all_nav_history(
    pool: &SqlitePool,
    since: NaiveDate,
) -> Result<Vec<NavPoint>, sqlx::Error> {
    sqlx::query_as::<_, NavPoint>(
        "SELECT fund_code, trade_date, unit_nav FROM nav WHERE trade_date >= ?1 ORDER BY fund_code, trade_date",
    )
    .bind(since)
    .fetch_all(pool)
    .await
}

/// 全量日线收盘（用于基准与穿透篮子回溯）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct QuotePoint {
    pub instrument_code: String,
    pub trade_date: NaiveDate,
    pub close: f64,
}

pub async fn all_quote_history(
    pool: &SqlitePool,
    since: NaiveDate,
) -> Result<Vec<QuotePoint>, sqlx::Error> {
    sqlx::query_as::<_, QuotePoint>(
        "SELECT instrument_code, trade_date, close FROM quote_daily WHERE trade_date >= ?1 ORDER BY instrument_code, trade_date",
    )
    .bind(since)
    .fetch_all(pool)
    .await
}
