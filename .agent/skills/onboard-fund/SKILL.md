---
name: onboard-fund
description: 往 FinLens 新增或替换一只基金：补 funds_source.json 的元数据/持仓/基准 → 重生成种子 → 灌库 → 用最近交易日验证 → 更新文档里的数量。
---

# onboard-fund — 新增/替换一只基金

## 何时用

- 用户给出新的基金代码、季报前十持仓、目标 ETF 或黄金基准。
- 替换某只基金的持仓（新季报发布），或修正错误的 benchmark/beta。
- 把 `position` 表补齐后，需要重算穿透口径（这一步只改库，不改 `funds_source.json`）。

## 前置

先读 `data/private/funds_source.json` 的结构；所有真实数据只能从这里进入系统。
`funds_source.json` 字段：`code / name / type / mode / category / benchmark /
tracking_beta / notes / holdings[[原始代码, 名称, 权重]]`。顶层还有
`instruments`（标的字典，含 `secid`）与 `sector_map`（个股 → 行业）。

## 步骤

1. **确认基金类型 → 估值模式**

   - 93%+ 持有目标 ETF 的联接基 → `type=etf_fund`、`mode=transparent`、
     `benchmark=ETF.xxxxxx`、`tracking_beta≈0.95`，直接持仓只填季报里的股票（通常极小）。
   - 黄金/商品 → `mode=transparent`、`benchmark=AU.GOLD`、`beta=1.0`。
   - 主动权益 → `mode=semi_transparent`、`benchmark=IDX.000300/399006/000688`，
     填最新季报前十持仓权重（小数，合计 ≤1，剩余自动进残余仓位）。

2. **补 `instruments` 与 `sector_map`**：每个新代码要有 `secid`（沪 6/688→`1.`、深 0/3→`0.`、
   港股 5 位→`116.`）；ETF 用 `1.<6位>`；行业缺失时先归到最接近的板块，不要留空。

3. **改文件**：`data/private/funds_source.json`（保留已有基金，不要整文件重写）。

4. **重生成 + 灌库**：

   ```bash
   export CARGO_HOME="$PWD/.cargo-home"
   python3 collector/validate_inputs.py
   python3 collector/build_seed.py --refresh
   ./target/debug/fin-lens seed seeds/funds.sql
   ```

5. **回归验证**（用最近一个已公布净值的交易日）：

   ```bash
   ./target/debug/fin-lens validate YYYY-MM-DD
   ```

   新基金若没有 11-04 净值，会被列入"跳过"——在答复里说明，不要伪造前值。

## 验证

```bash
sqlite3 data/finlens.db "SELECT code, name, mode, benchmark_code FROM fund ORDER BY code;"
curl -s http://127.0.0.1:8787/api/funds/CODE/estimate | head -c 600
```

期望：`/api/funds` 数量 +1；estimate 返回 200，`prev_nav_date` 是最近交易日，
`coverage` 与实际持仓权重相符（主动基通常 0.25–0.75）。

## 失败处理

- **estimate 404**：`fund` 没插进去——检查 SQL 是否在 `fund` 表插入之后再引用它。
- **`coverage` 很低但持仓不少**：个股缺 `quote_daily`/`quote_snapshot`，检查 `instruments`
  的 `secid` 或代码前缀是否写错。
- **联接基误差爆炸**：确认 `quote_daily` 该 ETF 行的 `source='eastmoney_fund_nav'`；
  场内价含折溢价，不能作为联接基基准。
- **validate 因缺净值跳过**：只影响验证，不影响盘中估算；用下一个交易日再补。

## 收尾

- 在私有 `portfolio_<date>.json` 的 `buckets` 里补新基金的桶归属（`gold` / `nonferrous` /
  `broad_index` / `oil` / `semiconductor` / `medical` / `optical` / `compute`）。
- 需要时重跑 `build_portfolio.py` 并 seed，确认资源桶 donut 不再出现「未分类」。
- 本机状态/基线更新到 `AGENTS.local.md` 或 `data/private/notes/`，不要写进提交文件。
- 不要改已应用的 `migrations/`；真实数据永远走种子文件。接入指南见 `docs/onboarding.md`。
