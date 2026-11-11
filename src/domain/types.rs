//! 领域基础类型。
//!
//! 单位约定（全库统一，避免 0.05 / 5 的歧义）：
//! - 权重：`0.0..=1.0`
//! - 涨跌幅 / 偏差 / 覆盖率：小数，`0.012` 表示 +1.2%
//! - 净值：元，保留 4 位

use serde::{Deserialize, Serialize};

/// 估值模式：决定走哪条纯函数估值路径。
///
/// - `Transparent`：黄金 / 指数联接基，资产构成极简，跟基准走。
/// - `SemiTransparent`：主动权益基，靠十大重仓 + 残余仓位代理。
/// - `Opaque`：持仓/基准都不可得，只能退化为基准跟随并显著降权置信度。
/// - `Qdii`：跨境基金，T-1 美股行情 + 人民币汇率双重口径（见 `estimate_qdii`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValuationMode {
    Transparent,
    SemiTransparent,
    Opaque,
    Qdii,
}

impl ValuationMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transparent => "transparent",
            Self::SemiTransparent => "semi_transparent",
            Self::Opaque => "opaque",
            Self::Qdii => "qdii",
        }
    }

    pub fn from_db(raw: &str) -> Option<Self> {
        match raw {
            "transparent" => Some(Self::Transparent),
            "semi_transparent" => Some(Self::SemiTransparent),
            "opaque" => Some(Self::Opaque),
            "qdii" => Some(Self::Qdii),
            _ => None,
        }
    }
}

impl std::str::FromStr for ValuationMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_db(s.trim()).ok_or_else(|| format!("未知估值模式: {s}"))
    }
}

impl std::fmt::Display for ValuationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 十大重仓中的一条：证券 + 占基金净值的权重。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HoldingWeight {
    pub instrument: String,
    pub name: String,
    pub weight: f64,
}

impl HoldingWeight {
    pub fn new(instrument: impl Into<String>, name: impl Into<String>, weight: f64) -> Self {
        Self {
            instrument: instrument.into(),
            name: name.into(),
            weight,
        }
    }
}

/// 实时行情快照（只保留估值需要的三个字段）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuoteSnapshot {
    pub instrument: String,
    pub price: f64,
    /// 相对昨收的涨跌幅，小数。
    pub pct_chg: f64,
    pub quote_ts: String,
}
