//! 运行时配置：全部来自环境变量，带安全默认值。
//!
//! 所有百分比/比率一律用「小数」表示（0.05 = 5%），与数据库、内核保持一致，
//! 只在展示层做 ×100。

use std::env;

use crate::domain::calibration::EwmaConfig;

#[derive(Debug, Clone)]
pub struct Config {
    /// 监听地址，默认只绑本机回环。
    pub bind: String,
    /// SQLite 文件路径。
    pub db_path: String,
    /// 每日验证报告目录（`YYYY-MM/YYYY-MM-DD.md`）。
    pub daily_dir: String,
    /// 穿透重叠告警阈值（组合层面单一标的权重）。
    pub overlap_alert_pct: f64,
    /// EWMA 校准参数。
    pub ewma: EwmaConfig,
    /// 首次启动是否写入演示数据（明确标注为假数据，仅用于跑通链路）。
    pub seed_demo: bool,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            bind: env::var("FINLENS_BIND").unwrap_or_else(|_| "127.0.0.1:8787".to_string()),
            db_path: env::var("FINLENS_DB").unwrap_or_else(|_| "data/finlens.db".to_string()),
            daily_dir: env::var("FINLENS_DAILY_DIR").unwrap_or_else(|_| "data/daily".to_string()),
            overlap_alert_pct: parse_env("FINLENS_OVERLAP_ALERT", 0.05)?,
            ewma: EwmaConfig {
                lambda: parse_env("FINLENS_EWMA_LAMBDA", EwmaConfig::DEFAULT_LAMBDA)?,
                min_obs: parse_env("FINLENS_EWMA_MIN_OBS", EwmaConfig::DEFAULT_MIN_OBS)?,
                clamp_pct: parse_env("FINLENS_EWMA_CLAMP", EwmaConfig::DEFAULT_CLAMP)?,
                half_life_days: parse_env(
                    "FINLENS_EWMA_HALF_LIFE",
                    EwmaConfig::DEFAULT_HALF_LIFE_DAYS,
                )?,
            },
            seed_demo: env::var("FINLENS_SEED_DEMO")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        })
    }
}

fn parse_env<T>(key: &str, default: T) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(key) {
        Ok(raw) if !raw.trim().is_empty() => raw
            .trim()
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("{key}: {e}")),
        _ => Ok(default),
    }
}
