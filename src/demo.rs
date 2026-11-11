//! 演示数据（**全部是假数据**）。
//!
//! 目的只有一个：在真实截图数据录入前，让 schema + 内核 + API + 穿透矩阵
//! 的整条链路可以跑通并被测试。所有代码带 `DEMO.` 前缀、基金名带「演示」，
//! 不会和真实持仓混淆。
//!
//! 触发方式：`FINLENS_SEED_DEMO=1`，且只在 `fund` 表为空时写入。

use sqlx::SqlitePool;

const DEMO_SQL: &str = r#"
INSERT INTO instrument (code, name, kind, exchange, sector, source) VALUES
    ('DEMO.IDX', '演示科技指数',       'index',     NULL,   '科技',   'demo'),
    ('DEMO.HJ',  '演示光模块A(假数据)', 'stock',     'SZSE', '光模块', 'demo'),
    ('DEMO.XYS', '演示光模块B(假数据)', 'stock',     'SZSE', '光模块', 'demo'),
    ('DEMO.SEMI','演示半导体C(假数据)', 'stock',     'SSE',  '半导体', 'demo'),
    ('DEMO.ETF', '演示科技ETF(假数据)', 'etf',       'SSE',  '科技',   'demo');

INSERT INTO fund (code, name, short_name, mode, category, benchmark_code, benchmark_name, tracking_beta)
VALUES
    ('DEMO01', '演示·指数联接基(假数据)', '演示联接', 'transparent',     'index_feeder',
     'DEMO.IDX', '演示科技指数', NULL),
    ('DEMO02', '演示·主动科技基(假数据)', '演示主动', 'semi_transparent', 'active_equity',
     'DEMO.ETF', '演示科技ETF', NULL);

INSERT INTO holding (fund_code, report_date, instrument_code, weight, rank, source) VALUES
    ('DEMO02', '2026-09-30', 'DEMO.HJ',   0.098, 1, 'demo'),
    ('DEMO02', '2026-09-30', 'DEMO.XYS',  0.091, 2, 'demo'),
    ('DEMO02', '2026-09-30', 'DEMO.SEMI', 0.062, 3, 'demo');

INSERT INTO fund_allocation (fund_code, report_date, stock_weight, bond_weight, cash_weight, other_weight)
VALUES
    ('DEMO01', '2026-09-30', 0.00, 0.00, 0.07, 0.93),
    ('DEMO02', '2026-09-30', 0.90, 0.00, 0.09, 0.01);

INSERT INTO nav (fund_code, trade_date, unit_nav, accum_nav, daily_pct) VALUES
    ('DEMO01', '2026-11-04', 1.4900, 1.4900, NULL),
    ('DEMO01', '2026-11-05', 1.5000, 1.5000,  0.006711),
    ('DEMO02', '2026-11-04', 1.9800, 1.9800, NULL),
    ('DEMO02', '2026-11-05', 2.0000, 2.0000,  0.010101);

INSERT INTO quote_snapshot (instrument_code, price, prev_close, pct_chg, quote_ts, trade_date, source)
VALUES
    ('DEMO.IDX',  1024.50, 1012.35,  0.0120, '2026-11-06T14:30:00+08:00', '2026-11-06', 'demo'),
    ('DEMO.ETF',     1.512,    1.494, 0.0120, '2026-11-06T14:30:00+08:00', '2026-11-06', 'demo'),
    ('DEMO.HJ',    156.20,  151.50,  0.0310, '2026-11-06T14:30:00+08:00', '2026-11-06', 'demo'),
    ('DEMO.XYS',    88.40,   89.11, -0.0080, '2026-11-06T14:30:00+08:00', '2026-11-06', 'demo'),
    ('DEMO.SEMI',   42.10,   41.68,  0.0101, '2026-11-06T14:30:00+08:00', '2026-11-06', 'demo');

INSERT INTO tracking_fit (fund_code, benchmark_code, as_of_date, window_days, beta, alpha, r2, n_obs)
VALUES
    ('DEMO01', 'DEMO.IDX', '2026-11-05', 20, 0.927, 0.00021, 0.995, 20);

-- DEMO02 连续 8 天被低估 0.12%：EWMA 偏差项应当生效
INSERT INTO calibration (fund_code, trade_date, estimated_nav, actual_nav, residual_nav,
                         residual_pct, ewma_bias_pct, ewma_abs_pct, n_obs, lambda, outlier)
VALUES
    ('DEMO02', '2026-11-05', 1.99820, 2.00000, 0.00180, 0.00090, 0.00120, 0.00350, 8, 0.25, 0);

INSERT INTO position (fund_code, shares, cost_basis, market_value, weight) VALUES
    ('DEMO01', 10000.0, 14000.0, 15000.0, 0.5),
    ('DEMO02',  5000.0,  9500.0, 10000.0, 0.5);

INSERT INTO portfolio_snapshot (as_of_date, total_value, cash, source) VALUES
    ('2026-11-06', 25000.0, 0.0, 'demo');
"#;

/// 写入演示数据。返回 `true` 表示本次确实写入，`false` 表示库里已有数据（跳过）。
pub async fn seed_demo(pool: &SqlitePool) -> anyhow::Result<bool> {
    let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fund")
        .fetch_one(pool)
        .await?;
    if existing > 0 {
        return Ok(false);
    }

    let mut tx = pool.begin().await?;
    sqlx::raw_sql(DEMO_SQL).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}
