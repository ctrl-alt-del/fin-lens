//! 集成测试：验证「迁移 → 演示数据 → 纯函数内核 → 穿透矩阵」整条链路。
//!
//! 这里刻意不启动 HTTP 服务：HTTP 层只是薄适配，真正的风险在 schema 与内核。

use std::collections::HashMap;

use chrono::NaiveDate;
use fin_lens::{
    db::{self, repo},
    demo,
    domain::{
        lookthrough::{self, HoldingLine, PositionWeight},
        types::ValuationMode,
        valuation::{self, BiasInput, Flag, SemiTransparentParams, TransparentParams},
    },
};

#[tokio::test]
async fn migrations_and_demo_seed_work() {
    let pool = db::init_memory().await.expect("init memory db");
    demo::seed_demo(&pool).await.expect("seed demo");
    // 二次调用必须幂等
    assert!(!demo::seed_demo(&pool).await.expect("second seed"));

    let funds = repo::list_funds(&pool).await.expect("list funds");
    assert_eq!(funds.len(), 2);

    let nav = repo::latest_nav(&pool, "DEMO02")
        .await
        .expect("latest nav")
        .expect("nav exists");
    assert_eq!(nav.unit_nav, 2.0);
    assert_eq!(nav.trade_date.to_string(), "2026-11-05");

    let holdings = repo::latest_holdings(&pool, "DEMO02")
        .await
        .expect("holdings");
    assert_eq!(holdings.len(), 3);
    assert_eq!(holdings[0].instrument_code, "DEMO.HJ");
    assert_eq!(holdings[0].sector.as_deref(), Some("光模块"));

    // 跟涨系数回归结果应能被取出
    let beta = repo::latest_beta(&pool, "DEMO01").await.expect("beta");
    assert!((beta.expect("beta exists") - 0.927).abs() < 1e-12);

    // 校准状态可读出（严格取目标日之前最近一条，避免回测时看到未来）
    let cal = repo::latest_calibration_before(
        &pool,
        "DEMO02",
        NaiveDate::from_ymd_opt(2026, 11, 6).unwrap(),
    )
    .await
    .expect("calibration")
    .expect("calibration row");
    assert_eq!(cal.n_obs, 8);
    assert!((cal.ewma_bias_pct - 0.0012).abs() < 1e-12);
}

#[tokio::test]
async fn transparent_fund_end_to_end_numbers() {
    let pool = db::init_memory().await.expect("init memory db");
    demo::seed_demo(&pool).await.expect("seed demo");

    let fund = repo::get_fund(&pool, "DEMO01")
        .await
        .expect("fund")
        .expect("fund exists");
    assert_eq!(fund.mode, ValuationMode::Transparent.as_str());

    let nav = repo::latest_nav(&pool, "DEMO01")
        .await
        .expect("nav")
        .expect("nav exists");
    let quotes: HashMap<String, f64> = repo::all_quotes(&pool)
        .await
        .expect("quotes")
        .into_iter()
        .map(|q| (q.instrument_code, q.pct_chg))
        .collect();

    let beta = repo::latest_beta(&pool, "DEMO01")
        .await
        .expect("beta")
        .or(fund.tracking_beta);
    let benchmark = fund.benchmark_code.expect("benchmark");
    let est = valuation::estimate_transparent(
        nav.unit_nav,
        &TransparentParams {
            benchmark_pct: quotes.get(&benchmark).copied(),
            beta,
            fx_pct: 0.0,
            bias: BiasInput::NONE,
        },
    )
    .expect("estimate");

    // 1.5 × (1 + 0.927 × 0.012) = 1.516686
    assert!((est.estimated_nav - 1.516686).abs() < 1e-9);
    assert_eq!(est.mode, ValuationMode::Transparent);
}

#[tokio::test]
async fn semi_transparent_fund_end_to_end_numbers() {
    let pool = db::init_memory().await.expect("init memory db");
    demo::seed_demo(&pool).await.expect("seed demo");

    let fund = repo::get_fund(&pool, "DEMO02")
        .await
        .expect("fund")
        .expect("fund exists");
    let nav = repo::latest_nav(&pool, "DEMO02")
        .await
        .expect("nav")
        .expect("nav exists");
    let holdings = repo::latest_holdings(&pool, "DEMO02")
        .await
        .expect("holdings")
        .into_iter()
        .map(|h| fin_lens::domain::types::HoldingWeight::new(h.instrument_code, h.name, h.weight))
        .collect();
    let quotes: HashMap<String, f64> = repo::all_quotes(&pool)
        .await
        .expect("quotes")
        .into_iter()
        .map(|q| (q.instrument_code, q.pct_chg))
        .collect();
    let stock_weight = repo::latest_stock_weight(&pool, "DEMO02")
        .await
        .expect("alloc");
    let residual_pct = fund
        .benchmark_code
        .as_deref()
        .and_then(|b| quotes.get(b).copied());

    let est = valuation::estimate_semi_transparent(
        nav.unit_nav,
        &SemiTransparentParams {
            holdings,
            quotes,
            stock_weight,
            residual_pct,
            bias: BiasInput::applied(0.0012),
        },
    )
    .expect("estimate");

    // 重仓贡献 0.29362% + 残余 0.7788% + 偏差 0.12% = 1.19242%
    assert!((est.estimated_pct - 0.0119242).abs() < 1e-12);
    assert!((est.estimated_nav - 2.0238484).abs() < 1e-9);
    assert!((est.coverage - 0.251).abs() < 1e-12);
    // coverage 0.251 < 0.5，必须如实标注「覆盖不足」，不能假装精确
    assert!(est.flags.contains(&Flag::LowCoverage));
}

#[tokio::test]
async fn lookthrough_matrix_marks_concentration() {
    let pool = db::init_memory().await.expect("init memory db");
    demo::seed_demo(&pool).await.expect("seed demo");

    // DEMO01 是透明资产，没有股票持仓；DEMO02 的组合权重 50% 持有三只票。
    let positions = vec![
        PositionWeight::new("DEMO01", 0.5),
        PositionWeight::new("DEMO02", 0.5),
    ];
    let mut holdings: HashMap<String, Vec<HoldingLine>> = HashMap::new();
    for row in repo::all_latest_holdings(&pool).await.expect("holdings") {
        holdings
            .entry(row.fund_code)
            .or_default()
            .push(HoldingLine {
                instrument: row.instrument_code,
                name: row.name,
                sector: row.sector,
                weight: row.weight,
            });
    }

    let report = lookthrough::compute(&positions, &holdings, 0.05);
    // 50% × 9.8% = 4.9%，差一点点才到 5%，因此不应告警
    let top = &report.exposures[0];
    assert_eq!(top.instrument, "DEMO.HJ");
    assert!((top.total_weight - 0.049).abs() < 1e-12);
    assert!(!top.is_alert);

    // 阈值降到 3% 后，三只票（4.9% / 4.55% / 3.1%）全部越线
    let strict = lookthrough::compute(&positions, &holdings, 0.03);
    assert_eq!(strict.alerts.len(), 3);
    assert_eq!(strict.alerts[0].instrument, "DEMO.HJ");
}
