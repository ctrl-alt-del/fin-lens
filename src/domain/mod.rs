//! 纯函数领域内核。
//!
//! 本模块及其子模块禁止出现 `sqlx` / `axum` / `std::time::SystemTime`。
//! 已经在 CI 心智里把它当成「数学库」看待：只吃数字，只吐数字。

pub mod calibration;
pub mod fit;
pub mod lookthrough;
pub mod risk;
pub mod types;
pub mod valuation;

pub use calibration::{EwmaConfig, EwmaState, Observation};
pub use lookthrough::{
    compute as compute_lookthrough, HoldingLine, LookthroughReport, PositionWeight,
};
pub use types::{HoldingWeight, QuoteSnapshot, ValuationMode};
pub use valuation::{
    estimate_opaque_fallback, estimate_semi_transparent, estimate_transparent, BiasInput,
    Contribution, Estimate, Flag, KernelError, SemiTransparentParams, TransparentParams,
};
