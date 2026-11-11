//! fin-lens：轻量级基金穿透估值与监控系统。
//!
//! 分层约定（依赖只能向下，不能反向）：
//!
//! ```text
//! api / demo      ← 适配层（HTTP、演示数据、文件快照）
//! db::repo        ← 仓储层（唯一允许写 SQL 的地方）
//! domain          ← 纯函数内核（无 I/O、无时钟、无随机数）
//! ```
//!
//! `domain` 不依赖 `sqlx` / `axum`，任何新算法都应该先在 `domain` 里以纯函数形式
//! 落地并配单测，再接到仓储层。这条边界是本项目“确定性估值”可验证的前提。

pub mod api;
pub mod config;
pub mod db;
pub mod demo;
pub mod domain;
pub mod error;
pub mod service;
pub mod ui;
pub mod validate;
