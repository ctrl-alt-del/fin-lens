//! SQLite 连接与迁移。
//!
//! 连接级 PRAGMA 全部通过 `SqliteConnectOptions` 设置，不写进 migration
//! （`journal_mode` 在事务里执行会失败，且 sqlx 的迁移是在事务中跑的）。
//!
//! 只开 4 条连接：这是本地单用户看板，连接池不是瓶颈，WAL + 单写者是刻意选择。

use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

pub mod repo;

pub type Db = SqlitePool;

/// 打开（必要时创建）数据库并执行迁移。
pub async fn init(path: &str) -> anyhow::Result<Db> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        // NORMAL 在 WAL 下已经足够安全，且省掉每次提交的 fsync。
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// 内存库（单连接，保证同一份库），给集成测试用。
pub async fn init_memory() -> anyhow::Result<Db> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&pool)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}
