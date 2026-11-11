//! 服务入口与运维子命令。
//!
//! ```text
//! fin-lens                     # 起 HTTP 服务（默认）
//! fin-lens seed <file.sql>     # 把离线生成的种子 SQL 灌进库（可重复执行）
//! fin-lens validate <YYYY-MM-DD>  # 用某日行情回测估算、落校准行、写日报
//! ```
//!
//! 没有 Redis、没有消息队列、没有独立调度进程：一个 tokio 进程 + 一个 SQLite
//! 文件就是全部。后续的轮询任务（`tokio::spawn` 的长生命周期 Task）也挂在这里。

use std::sync::Arc;

use chrono::NaiveDate;
use fin_lens::{
    api::{self, AppState},
    config::Config,
    db, demo, validate,
};
use tokio::net::TcpListener;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,fin_lens=debug")),
        )
        .with(tracing_subscriber::fmt::layer().compact())
        .init();

    let cfg = Config::from_env()?;
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "serve".to_string());

    match command.as_str() {
        "serve" => serve(cfg).await,
        "seed" => {
            let path = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("用法: fin-lens seed <file.sql>"))?;
            seed(cfg, &path).await
        }
        "validate" => {
            let raw = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("用法: fin-lens validate <YYYY-MM-DD>"))?;
            let date = NaiveDate::parse_from_str(&raw, "%Y-%m-%d")
                .map_err(|_| anyhow::anyhow!("日期格式应为 YYYY-MM-DD，收到: {raw}"))?;
            run_validate(cfg, date).await
        }
        "help" | "-h" | "--help" => {
            println!(
                "fin-lens 0.1.0\n\n\
                 用法:\n  \
                 fin-lens [serve]                    启动 HTTP 服务\n  \
                 fin-lens seed <file.sql>            执行种子 SQL（幂等）\n  \
                 fin-lens validate <YYYY-MM-DD>      估算回测 + 校准 + 日报\n\n\
                 环境变量: FINLENS_BIND, FINLENS_DB, FINLENS_DAILY_DIR,\n\
                 FINLENS_OVERLAP_ALERT, FINLENS_EWMA_*, FINLENS_SEED_DEMO"
            );
            Ok(())
        }
        other => anyhow::bail!("未知子命令: {other}（试试 fin-lens --help）"),
    }
}

async fn serve(cfg: Config) -> anyhow::Result<()> {
    let pool = db::init(&cfg.db_path).await?;
    tracing::info!(db = %cfg.db_path, "sqlite ready");

    if cfg.seed_demo {
        let seeded = demo::seed_demo(&pool).await?;
        tracing::info!(seeded, "demo seed evaluated");
    }

    let state = AppState {
        db: pool,
        cfg: Arc::new(cfg.clone()),
    };
    let app = api::router(state).layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = TcpListener::bind(&cfg.bind).await?;
    tracing::info!(addr = %listener.local_addr()?, "fin-lens listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn seed(cfg: Config, path: &str) -> anyhow::Result<()> {
    let sql = std::fs::read_to_string(path)
        .map_err(|err| anyhow::anyhow!("读取种子文件失败 {path}: {err}"))?;
    let pool = db::init(&cfg.db_path).await?;
    sqlx::raw_sql(&sql).execute(&pool).await?;

    let funds: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fund")
        .fetch_one(&pool)
        .await?;
    let holdings: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM holding")
        .fetch_one(&pool)
        .await?;
    let navs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nav")
        .fetch_one(&pool)
        .await?;
    let quotes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quote_daily")
        .fetch_one(&pool)
        .await?;
    println!(
        "seed 完成 {path}: fund={funds}, holding={holdings}, nav={navs}, quote_daily={quotes}"
    );
    Ok(())
}

async fn run_validate(cfg: Config, date: NaiveDate) -> anyhow::Result<()> {
    let pool = db::init(&cfg.db_path).await?;
    let report = validate::run(&pool, &cfg, date).await?;
    report.print_summary();
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}
