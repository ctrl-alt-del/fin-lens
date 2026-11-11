---
name: daily-validation
description: 盘后对账：用已公布净值跑 FinLens 单日验证，解读误差、确认校准行已落库，并在偏差累计满 3 个交易日后确认自动启用。
---

# daily-validation — 盘后单日验证

## 何时用

- 交易日 20:00–23:00（CST）之间，基金净值已公布，用户问"今天估得准不准"。
- 新增基金、换了基准、改了持仓或采集口径之后，需要一次回归。
- 需要给 `calibration` 表补当天的一行（这是偏差自校准的唯一数据来源）。

## 不该用

- 当天净值还没出（通常 19:30 前）→ 只能做盘中估算，不能 validate。
- 数据还没刷新 → 先 `market-refresh`，否则会用旧行情对账，误差没有意义。

## 步骤

```bash
cd /path/to/fin-lens
export CARGO_HOME="$PWD/.cargo-home"

# 确认二进制是最新的
cargo build

# 1) 跑验证（日期必须是已公布净值的交易日）
./target/debug/fin-lens validate YYYY-MM-DD
```

命令会：取 `quote_daily` 当天的涨跌 → 逐只基金用 `service::estimate_fund` 估算 →
对比 `nav(当日).daily_pct` → 写 `data/daily/YYYY-MM/YYYY-MM-DD.md` →
用 `calibration::update` 落 `ewma_bias_pct / ewma_abs_pct / n_obs`。

重复执行是幂等的（upsert）；`n_obs` 每个交易日只应 +1。

## 怎么看结果

- **平均绝对误差（MAE）**：全组合口径；基线从最近一次日报/`data/private/notes/` 读取，不要写进仓库。
- **ETF 联接基**（按 `fund.etf_code IS NOT NULL` 识别）：应接近 0–20 bp；
  若 >100 bp，八成是基准用了场内价而不是基金净值（见 `AGENTS.md` 硬约束 6）。
- **主动基**：20–200 bp 属正常，来自季报持仓滞后；单只 >300 bp 才值得排查。
- **误差 Top5**：先看是否是同一只重仓股缺行情，再看 `coverage` 是否异常低。
- **校准状态**：`n_obs < 3` 时偏差不启用（`bias_inactive`）；满 3 个交易日后
  按 λ=0.25 平滑、半衰期 10 天、单日夹紧 ±0.3% 生效。

## 验证

```bash
sqlite3 data/finlens.db "
  SELECT trade_date, count(*), round(avg(abs(residual_pct))*10000,1) AS mae_bp, max(n_obs)
  FROM calibration GROUP BY trade_date ORDER BY trade_date DESC LIMIT 3;"
```

期望：当天每只已公布净值的基金一行，`n_obs` 单调递增，`outlier` 全 0。
再确认日报存在：`ls data/daily/YYYY-MM/`，Markdown 里有口径、逐只明细、误差 Top5。

## 失败处理

- **"quotes_on 为空，先 seed"**：当天没有 `quote_daily`。先跑 `market-refresh`；
  若当天是非交易日，换最近一个交易日。
- **某只基金被跳过**：多为缺前一日净值（新基金）或净值尚未公布；日报的"跳过"段落有原因，
  不要手工补净值来强行凑数。
- **MAE 突然翻倍**：按顺序查 ① 基准口径（ETF/黄金是否用了净值）② 持仓日期是否过期
  ③ 个股行情是否缺 ④ 当天是否有分红/份额折算（`nav.daily_pct` 会失真）。
- **`n_obs` 重复累加**：说明同一交易日跑了两次且状态被覆盖；查 `calibration` 主键
  是否 `(fund_code, trade_date)`，不要用日期字符串手工改行。

## 不要做

- 不要为了"让误差变小"改持仓权重或 beta——误差是产品的输出，不是 bug。
- 不要把 validate 结果手工写进 `calibration`；它由 `calibration::update` 维护。
- 不要在盘中跑 `validate 今天`：净值未公布，对比的是昨天的净值。
