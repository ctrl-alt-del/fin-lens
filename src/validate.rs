//! 首日验证闭环：用 `quote_daily(D)` + `nav(D 之前)` 估算，再和 `nav(D)` 实际涨跌对照。
//!
//! 产出两件事：
//! 1. `data/daily/YYYY-MM/YYYY-MM-DD.md` —— 人对账用的日报；
//! 2. `calibration` 行 —— 下一交易日起的 EWMA 偏差输入（首日 n_obs=1，
//!    低于 `min_obs=3`，因此次日估算不会启用偏差，避免前视/自激）。
//!
//! 这里刻意**不**复用 API 的响应结构：验证报告需要实际净值、误差 bp、
//! 跳过原因等 API 不关心的字段。

use std::{fmt::Write as _, path::PathBuf};

use chrono::NaiveDate;
use sqlx::SqlitePool;

use crate::{
    config::Config,
    db::repo,
    domain::calibration::{self, EwmaState, Observation},
    service,
};

/// 单只基金的验证结果。
#[derive(Debug, Clone)]
pub struct FundResult {
    pub code: String,
    pub name: String,
    pub mode: String,
    pub actual_pct: f64,
    pub estimated_pct: f64,
    pub raw_pct: f64,
    pub bias_pct: f64,
    /// 估算 − 实际，单位 bp（正 = 高估）。
    pub error_bp: f64,
    pub coverage: f64,
    pub confidence: f64,
    pub flags: Vec<String>,
    pub prev_nav_date: NaiveDate,
    pub prev_nav: f64,
    pub estimated_nav: f64,
    pub actual_nav: f64,
}

pub struct Report {
    pub trade_date: NaiveDate,
    pub results: Vec<FundResult>,
    pub skipped: Vec<(String, String)>,
    pub report_path: PathBuf,
}

impl Report {
    pub fn mean_abs_error_bp(&self) -> Option<f64> {
        if self.results.is_empty() {
            return None;
        }
        let sum: f64 = self.results.iter().map(|r| r.error_bp.abs()).sum();
        Some(sum / self.results.len() as f64)
    }

    pub fn worst(&self) -> Option<&FundResult> {
        self.results
            .iter()
            .max_by(|a, b| a.error_bp.abs().total_cmp(&b.error_bp.abs()))
    }

    /// 控制台摘要：不追求排版，只保证一眼能看到最差的几只。
    pub fn print_summary(&self) {
        println!("\n=== fin-lens 验证 {} ===", self.trade_date);
        for r in self.results.iter() {
            println!(
                "{} {:<22} actual={:+.2}% est={:+.2}% err={:+.1}bp cov={:.2} conf={:.2} {}",
                r.code,
                r.name,
                r.actual_pct * 100.0,
                r.estimated_pct * 100.0,
                r.error_bp,
                r.coverage,
                r.confidence,
                r.flags.join(","),
            );
        }
        if let Some(mean) = self.mean_abs_error_bp() {
            println!("命中 {} 只，平均绝对误差 {mean:.1} bp", self.results.len());
        }
        if let Some(worst) = self.worst() {
            println!(
                "最大偏差: {} {} {:+.1} bp",
                worst.code, worst.name, worst.error_bp
            );
        }
        if !self.skipped.is_empty() {
            println!("跳过 {} 只:", self.skipped.len());
            for (code, reason) in &self.skipped {
                println!("  - {code}: {reason}");
            }
        }
        println!("日报: {}", self.report_path.display());
    }
}

/// 跑一天的验证并落盘日报与校准行。
pub async fn run(pool: &SqlitePool, cfg: &Config, date: NaiveDate) -> anyhow::Result<Report> {
    let raw_quotes = repo::quotes_on(pool, date).await?;
    if raw_quotes.is_empty() {
        anyhow::bail!("{date} 没有任何 quote_daily 行情；先执行 `fin-lens seed seeds/funds.sql`");
    }

    let mut results = Vec::new();
    let mut skipped = Vec::new();

    for fund in repo::list_funds(pool).await? {
        let Some(nav) = repo::nav_on(pool, &fund.code, date).await? else {
            skipped.push((fund.code.clone(), format!("缺少 {date} 净值")));
            continue;
        };

        let ctx = match service::estimate_fund(pool, cfg, &fund.code, date, &raw_quotes).await {
            Ok(ctx) => ctx,
            Err(err) => {
                skipped.push((fund.code.clone(), err.to_string()));
                continue;
            }
        };

        // 净值表里的 daily_pct 是 API 直接给的；缺失时用相邻净值回填。
        let actual_pct = match nav.daily_pct {
            Some(pct) => pct,
            None => match repo::nav_before(pool, &fund.code, date).await? {
                Some(prev) => nav.unit_nav / prev.unit_nav - 1.0,
                None => {
                    skipped.push((fund.code.clone(), "缺净值且无法回填涨跌".to_string()));
                    continue;
                }
            },
        };

        let estimate = &ctx.estimate;
        let error_bp = (estimate.estimated_pct - actual_pct) * 10_000.0;

        update_calibration(
            pool,
            cfg,
            &fund.code,
            date,
            actual_pct,
            nav.unit_nav,
            estimate,
        )
        .await?;

        results.push(FundResult {
            code: fund.code,
            name: fund.name,
            mode: fund.mode,
            actual_pct,
            estimated_pct: estimate.estimated_pct,
            raw_pct: estimate.raw_pct,
            bias_pct: estimate.bias_pct,
            error_bp,
            coverage: estimate.coverage,
            confidence: estimate.confidence,
            flags: estimate
                .flags
                .iter()
                .filter_map(|flag| serde_json::to_value(flag).ok())
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            prev_nav_date: ctx.prev_nav.trade_date,
            prev_nav: ctx.prev_nav.unit_nav,
            estimated_nav: estimate.estimated_nav,
            actual_nav: nav.unit_nav,
        });
    }

    let report_path = write_markdown(cfg, date, &results, &skipped)?;
    Ok(Report {
        trade_date: date,
        results,
        skipped,
        report_path,
    })
}

async fn update_calibration(
    pool: &SqlitePool,
    cfg: &Config,
    code: &str,
    date: NaiveDate,
    actual_pct: f64,
    actual_nav: f64,
    estimate: &crate::domain::valuation::Estimate,
) -> anyhow::Result<()> {
    // 只用 date 之前的校准状态，重复跑同一天也不会把当天算两次。
    let state = match repo::latest_calibration_before(pool, code, date).await? {
        Some(row) => EwmaState {
            bias_pct: row.ewma_bias_pct,
            abs_pct: row.ewma_abs_pct,
            n_obs: row.n_obs.max(0) as u32,
            last_date: Some(row.trade_date),
        },
        None => EwmaState::empty(),
    };
    let observation = Observation {
        trade_date: date,
        actual_pct,
        // 关键：用**未加偏差项**的原始估算算残差，避免偏差自我强化。
        raw_estimated_pct: estimate.raw_pct,
    };
    let update = match calibration::update(&state, &observation, &cfg.ewma) {
        Ok(update) => update,
        Err(err) => {
            tracing::warn!(fund = %code, %err, "calibration skipped");
            return Ok(());
        }
    };

    repo::upsert_calibration(
        pool,
        &repo::CalibrationUpsert {
            fund_code: code.to_string(),
            trade_date: date,
            estimated_nav: estimate.estimated_nav,
            actual_nav,
            residual_nav: actual_nav - estimate.estimated_nav,
            residual_pct: update.residual_pct,
            ewma_bias_pct: update.state.bias_pct,
            ewma_abs_pct: update.state.abs_pct,
            n_obs: update.state.n_obs,
            lambda: cfg.ewma.lambda,
            outlier: !update.accepted,
            note: Some(format!("raw_estimated_pct={:.8}", estimate.raw_pct)),
        },
    )
    .await?;
    Ok(())
}

fn write_markdown(
    cfg: &Config,
    date: NaiveDate,
    results: &[FundResult],
    skipped: &[(String, String)],
) -> anyhow::Result<PathBuf> {
    let dir = PathBuf::from(&cfg.daily_dir).join(format!("{}", date.format("%Y-%m")));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", date.format("%Y-%m-%d")));

    let mut md = String::new();
    let mean = if results.is_empty() {
        None
    } else {
        Some(results.iter().map(|r| r.error_bp.abs()).sum::<f64>() / results.len() as f64)
    };
    let _ = writeln!(md, "# fin-lens 净值验证 · {date}\n");
    let _ = writeln!(
        md,
        "- 口径：用 {date} 的 `quote_daily` 与 {} 之前的净值估算，对比 {date} 实际净值涨跌",
        date
    );
    let _ = writeln!(
        md,
        "- 命中：{} 只；跳过：{} 只",
        results.len(),
        skipped.len()
    );
    if let Some(mean) = mean {
        let _ = writeln!(md, "- 平均绝对误差：**{mean:.1} bp**（1 bp = 0.01%）");
    }
    if let Some(worst) = results
        .iter()
        .max_by(|a, b| a.error_bp.abs().total_cmp(&b.error_bp.abs()))
    {
        let _ = writeln!(
            md,
            "- 最大偏差：{} {} **{:+.1} bp**",
            worst.code, worst.name, worst.error_bp
        );
    }
    let _ = writeln!(md, "\n## 逐只明细\n");
    let _ = writeln!(
        md,
        "| 基金 | 模式 | 实际 | 估算 | 偏差 | 覆盖率 | 置信度 | flags |"
    );
    let _ = writeln!(md, "|---|---|---:|---:|---:|---:|---:|---|");
    for r in results {
        let _ = writeln!(
            md,
            "| {} {} | {} | {:+.2}% | {:+.2}% | {:+.1} bp | {:.2} | {:.2} | {} |",
            r.code,
            r.name,
            r.mode,
            r.actual_pct * 100.0,
            r.estimated_pct * 100.0,
            r.error_bp,
            r.coverage,
            r.confidence,
            if r.flags.is_empty() {
                "-".to_string()
            } else {
                r.flags.join(", ")
            }
        );
    }

    if !results.is_empty() {
        let mut ordered: Vec<&FundResult> = results.iter().collect();
        ordered.sort_by(|a, b| b.error_bp.abs().total_cmp(&a.error_bp.abs()));
        let _ = writeln!(md, "\n## 误差最大的 5 只\n");
        let _ = writeln!(md, "| 基金 | 偏差 | 覆盖 | 说明 |");
        let _ = writeln!(md, "|---|---:|---:|---|");
        for r in ordered.into_iter().take(5) {
            let _ = writeln!(
                md,
                "| {} {} | {:+.1} bp | {:.2} | raw={:+.4}% bias={:+.4}% |",
                r.code,
                r.name,
                r.error_bp,
                r.coverage,
                r.raw_pct * 100.0,
                r.bias_pct * 100.0
            );
        }
    }

    if !skipped.is_empty() {
        let _ = writeln!(md, "\n## 跳过\n");
        for (code, reason) in skipped {
            let _ = writeln!(md, "- `{code}`：{reason}");
        }
    }

    let _ = writeln!(
        md,
        "\n> 估算明细可用 `GET /api/funds/<code>/estimate` 复核；\
         校准行已写入 `calibration` 表（首日 n_obs=1，未启用偏差项）。"
    );

    std::fs::write(&path, md)?;
    Ok(path)
}
