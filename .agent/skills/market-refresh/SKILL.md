---
name: market-refresh
description: 刷新 FinLens 的基金净值、持仓、真实组合与行情：抓取东方财富 → 生成种子 SQL → 幂等灌库 → 校验覆盖与快照时点。
---

# market-refresh — 刷新行情与种子

## 何时用

- 开盘前（09:15 前）或盘中需要最新的 `quote_snapshot`（Dashboard 与实时估算用）。
- 用户新增/替换基金后（先改 `data/private/funds_source.json`，见 `onboard-fund`）。
- Dashboard 的"盘中数据时点"超过 15 分钟，或 `/api/funds` 数量不对。
- 每日收盘后，为 `daily-validation` 准备当天的日线。

## 不该用

- 净值刚公布、只想做对账 → 用 `daily-validation`，不要顺便抓行情（会冲掉可复现性）。
- 只想看当前估值 → 直接读 `/` 或 `/api/lookthrough`，刷新会改变数字。

## 步骤（在仓库根目录，顺序不能换）

```bash
cd /path/to/fin-lens
export CARGO_HOME="$PWD/.cargo-home"

# 0) 先校验私有输入结构（不联网、不写库）
python3 collector/validate_inputs.py

# 1) 抓数 + 生成 SQL。--refresh 绕过 .cache/seed；不加则用缓存
python3 collector/build_seed.py --refresh

# 2) 重新生成真实组合种子（新基金净值/持仓行情 + position/trade/family/bucket）
python3 collector/build_portfolio.py

# 2.5) 展开 ETF 联接基前十大成分 + 黄金代理（前十大口径，属下限）
python3 collector/build_etf_lookthrough.py

# 3) 灌库（幂等；重复执行不会重复插入）
./target/debug/fin-lens seed seeds/funds.sql
./target/debug/fin-lens seed seeds/portfolio_YYYY-MM-DD.sql
./target/debug/fin-lens seed seeds/etf_lookthrough_YYYY-MM-DD.sql
# 若刚改过 Rust 代码：先 cargo build

# 4) 每月一次 / 首次回填：一年净值与日线（历史数据不会每天变）
python3 collector/build_history.py
./target/debug/fin-lens seed seeds/history_YYYY-MM-DD.sql
```

`build_seed.py` 会打印行数与 ground-truth 比对结果；配置了 `ground_truth_pct` 的基金应全部一致。
若出现名称不一致警告，不影响代码映射，但要在回复里提一句。

## 验证

```bash
sqlite3 data/finlens.db "
  SELECT 'funds', count(*) FROM fund
  UNION ALL SELECT 'holdings', count(*) FROM holding
  UNION ALL SELECT 'nav', count(*) FROM nav
  UNION ALL SELECT 'quote_daily', count(*) FROM quote_daily
  UNION ALL SELECT 'snapshot', count(*) FROM quote_snapshot;"
sqlite3 data/finlens.db "SELECT max(quote_ts) FROM quote_snapshot;"
```

期望：与 seed 前相比，`funds` / `holdings` / `nav` / `quote_daily` 数量不应无故下降；
`holdings` 含 `source='etf_proxy'` 的前十大与 `source='gold_proxy'` 的黄金；
`snapshot` 仅在交易时段非空；`max(quote_ts)` 与当前 CST 时间相差不超过几分钟。
Dashboard 每 30s 自动局部刷新，**不需要重启服务**（进程读的是同一个 SQLite）。

## 失败处理

- **快照为空**：在非交易时段运行属正常。不要用上一交易日收盘价伪造当天盘中，
  也不要把缺失写成 0% 涨跌；盘中估算会回落到最近一次快照，必须在答复中说明。
- **Eastmoney TLS/超时**：`push2.eastmoney.com` 实时接口已弃用（SSL handshake 超时），
  只用 `push2his` 历史 K 线与 `api.fund.eastmoney.com` 净值接口；失败先重试一次。
- **SQL 执行报错**：检查是否漏了 `fin-lens seed` 之前的 `cargo build`，或迁移未跑
  （首次启动服务会自动迁移）。不要手工 `INSERT` 修补。
- **ground-truth mismatches > 0**：先跑 `sqlite3` 看对应基金的 `nav` 日期是否错位；
  仍不一致就停下报告，不要继续验证（否则误差归因会被污染）。

## 不要做

- 不要修改 `seeds/funds.sql` 或其他生成物（会被下一次运行覆盖）。
- 不要在盘中把 `quote_daily` 的最新价当作收盘价写进去。
- 不要把 refresh 和 validate 合并成一步；两步之间需要人确认数据时点。
- 不要把 `etf_proxy` 的前十大成分当成完整持仓：它只是下限，单票归因覆盖率 ≠ 100% 穿透。
