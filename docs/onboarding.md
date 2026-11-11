# 接入你自己的组合

这份指南把 FinLens 从「DEMO」变成「你自己的基金池」。所有真实数据只放在
`data/private/`，不会进入 Git。

> 先读 [private-data.md](private-data.md)。如果你只想知道项目能不能跑，先用
> `FINLENS_SEED_DEMO=1` 看 DEMO。

## 1. 先跑 DEMO

```bash
export CARGO_HOME="$PWD/.cargo-home"   # 受限环境需要

FINLENS_DB=/tmp/finlens-demo.db \
FINLENS_SEED_DEMO=1 \
FINLENS_BIND=127.0.0.1:8788 \
cargo run
```

打开 <http://127.0.0.1:8788/>，确认页面、`/api/funds`、`/api/lookthrough` 都有数据。
DEMO 只有 `DEMO01`/`DEMO02` 两只假基金，不会碰你的真实库。

## 2. 创建私有目录

```bash
mkdir -p data/private/notes
chmod 700 data/private
```

需要两个输入文件：

```text
data/private/
├── funds_source.json          # 基金池、标的、基准、季报持仓
└── portfolio_<date>.json      # 市值、成本/收益率、A/C、资源桶
```

## 3. `funds_source.json`

最小结构：

```json
{
  "report_date": "YYYY-MM-DD",
  "validation_date": "YYYY-MM-DD",
  "snapshot_date": "YYYY-MM-DD",
  "sector_map": { "000000": "示例行业" },
  "instruments": {
    "IDX.000300": {
      "name": "沪深300",
      "kind": "index",
      "sector": "宽基",
      "secid": "1.000300",
      "source_symbol": "000300"
    }
  },
  "funds": [
    {
      "code": "000000",
      "name": "示例基金",
      "type": "active",
      "mode": "semi_transparent",
      "category": "active_equity",
      "benchmark": "IDX.000300",
      "tracking_beta": 0.9,
      "notes": "可选",
      "holdings": [
        ["000001", "示例成分股", 0.05]
      ]
    }
  ]
}
```

字段说明：

| 字段 | 必填 | 说明 |
|---|---|---|
| `report_date` | 是 | 季报持仓日期，必须与 `portfolio.report_date` 一致 |
| `validation_date` | 是 | 首次验证用的净值日；`validate` 传同一天 |
| `snapshot_date` | 是 | 盘中快照日；`quote_snapshot.trade_date` |
| `sector_map` | 是 | 原始股票代码 → 行业；缺失的 ETF 成分可回退到东财 `INDEXNAME` |
| `instruments` | 是 | 内部代码 → 名称/kind/sector/secid/source_symbol |
| `funds` | 是 | 基金数组 |

基金字段：

| 字段 | 必填 | 说明 |
|---|---|---|
| `code` | 是 | 6 位基金代码 |
| `name` | 是 | 展示名 |
| `type` | 建议 | `etf_fund` / `active` / `commodity` 等，供 ETF/黄金脚本识别 |
| `mode` | 是 | `transparent` / `semi_transparent` / `opaque`；QDII 仍写 `transparent` |
| `category` | 是 | `index_feeder` / `active_tech` / `active_equity` / `commodity` / `qdii_index` 等 |
| `benchmark` | 是 | `instruments` 里的代码；如 `IDX.000300` / `ETF.512400` / `AU.GOLD` / `US.XOP` |
| `etf_code` | 联接基 | 目标 ETF 的内部代码；`build_etf_lookthrough.py` 用它展开前十大 |
| `tracking_beta` | 否 | 静态 β 回退；有 `tracking_fit` 时优先用回归值 |
| `holdings` | 主动基 | `[原始代码, 名称, 权重]`；权重是小数（0.05 = 5%），合计 ≤ 1 |

### 三种常见基金

**ETF 联接基**

```json
{
  "code": "000000",
  "name": "某ETF联接C",
  "type": "etf_fund",
  "mode": "transparent",
  "category": "index_feeder",
  "benchmark": "ETF.512400",
  "etf_code": "ETF.512400",
  "tracking_beta": 0.95,
  "holdings": []
}
```

`build_etf_lookthrough.py` 会抓目标 ETF 前十大，按 `联接基持有 ETF 比例 × ETF 成分权重`
写 `holding(source='etf_proxy')`。只展开前十大，所以单票归因是**下限**。

**主动权益**

```json
{
  "code": "000000",
  "name": "某主动科技C",
  "type": "active",
  "mode": "semi_transparent",
  "category": "active_tech",
  "benchmark": "IDX.399006",
  "holdings": [
    ["000001", "示例成分股A", 0.098],
    ["000002", "示例成分股B", 0.091]
  ]
}
```

把最新季报前十大填进 `holdings`。剩余仓位由 `fund_allocation` 的股票/债券/现金占比
推导，再用基准指数代理。

**QDII / 黄金**

- QDII：`category='qdii_index'`、`mode='transparent'`、`benchmark` 指向配置的美股行业指数代理；
  另需 `FX.USDCNH` 标的。美股用 T-1 收盘，净值可能晚一天公布。
- 黄金/商品：`type`/`category='commodity'`、`benchmark='AU.GOLD'`；
  `build_etf_lookthrough.py` 会用非股票/债券/现金仓位作为黄金代理权重。

### 可选字段

| 字段 | 用途 |
|---|---|
| `ground_truth_pct` | `{基金代码: 当日涨跌百分数}`；`build_seed.py` 用它做交叉核对，缺失则跳过 |
| `notes` | 任意备注，会写进 `fund.notes` |

> 已带 `US.` 前缀的持仓必须在 `instruments` 里提供 `secid`（例如 `US.XOP` → `107.XOP`）；
> `build_history.py` 不会在代码里写死 ticker。

## 4. `portfolio_<date>.json`

最小结构：

```json
{
  "as_of": "YYYY-MM-DD",
  "nav_date": "YYYY-MM-DD",
  "report_date": "YYYY-MM-DD",
  "positions": [
    {
      "code": "000000",
      "market_value": 10000.0,
      "cost_basis": 9500.0,
      "pending_amount": 0.0,
      "note": "可选"
    }
  ],
  "new_funds": {},
  "fetch_holdings": [],
  "instruments": {},
  "families": [],
  "buckets": {
    "broad_index": ["000000"]
  }
}
```

字段说明：

| 字段 | 必填 | 说明 |
|---|---|---|
| `as_of` | 是 | 组合市值时点；也是快照日 |
| `nav_date` | 是 | 用哪一天的净值反推份额；必须是已公布净值的交易日 |
| `report_date` | 是 | 与 `funds_source.report_date` 一致 |
| `positions[].code` | 是 | 必须出现在 `funds` 或 `new_funds` |
| `positions[].market_value` | 是 | 当前市值（元），> 0 |
| `positions[].cost_basis` | 否 | 绝对成本（元）；有则 `cost_source='screenshot'` |
| `positions[].pnl_pct` | 否 | 收益率小数（0.05 = +5%）；无成本时用它反推成本 |
| `positions[].pending_amount` | 否 | 在途申购/现金，计入 `portfolio_snapshot.cash` |
| `new_funds` | 否 | 不在 `funds_source` 里的基金元数据 + 手工持仓 |
| `fetch_holdings` | 否 | 要自动抓季报前十大的新基金代码 |
| `instruments` | 否 | 新增 ETF/美股/汇率标的 |
| `families` | 否 | A/C 合并：`{code, name, members:{基金代码: 'A'|'C'}, note}` |
| `buckets` | 否 | 桶代码 → 基金代码数组；桶代码见下 |

成本口径：

- `cost_basis` 优先；`cost_source='screenshot'`。
- 只有 `pnl_pct` 时：`cost = market_value / (1 + pnl_pct)`；`cost_source='derived_pnl'`。
- 两者都没有：`cost_basis=NULL`、`cost_source='unknown'`；页面显示「—」，**绝不填 0**。

资源桶代码：

```text
gold, nonferrous, broad_index, oil, semiconductor, medical, optical, compute
```

一只基金建议只属于一个桶；`service` 取 `bucket_member` 中 `bucket_id` 最小的一个。
没有桶成员的 ETF/联接基会显示「未分类」。

## 5. 校验输入

```bash
python3 collector/validate_inputs.py
```

它会检查必填字段、日期格式、基金代码、权重范围、基准引用、桶成员、A/C 成员等。
不联网、不写库、不打印金额。示例：

```bash
python3 collector/validate_inputs.py \
  --funds docs/examples/inputs/funds_source.json \
  --portfolio docs/examples/inputs/portfolio_example.json \
  --allow-demo
```

## 6. 生成种子并灌库

按顺序执行：

```bash
export CARGO_HOME="$PWD/.cargo-home"

python3 collector/build_seed.py --refresh
python3 collector/build_portfolio.py
python3 collector/build_etf_lookthrough.py
python3 collector/build_history.py

./target/debug/fin-lens seed seeds/funds.sql
./target/debug/fin-lens seed seeds/portfolio_YYYY-MM-DD.sql
./target/debug/fin-lens seed seeds/etf_lookthrough_YYYY-MM-DD.sql
./target/debug/fin-lens seed seeds/history_YYYY-MM-DD.sql
```

默认输出：

```text
seeds/funds.sql
seeds/portfolio_YYYY-MM-DD.sql
seeds/etf_lookthrough_YYYY-MM-DD.sql
seeds/history_YYYY-MM-DD.sql
```

可以用 `--private-dir`、`--portfolio`、`--out` 覆盖路径；脚本会自动发现唯一的
`data/private/portfolio_*.json`，有多个时要求显式指定。

## 7. 起服务并检查

```bash
FINLENS_DB=data/finlens.db FINLENS_BIND=127.0.0.1:8787 cargo run
```

```bash
curl -s localhost:8787/healthz
curl -s localhost:8787/api/funds | jq length
curl -s localhost:8787/api/estimates | jq '{as_of,count,skipped}'
curl -s localhost:8787/api/lookthrough | jq '{weights_assumed_equal,attributed:.report.attributed_weight,alerts:.report.alerts}'
```

检查：

- `/api/funds` 数量与 `funds_source.json` 一致。
- `/api/estimates` 的 `skipped` 只包含缺净值/新基金。
- `/api/lookthrough` 的 `weights_assumed_equal: false`，`alerts` 符合预期。
- 页面资源桶没有大量「未分类」；有则补 `buckets` 后重跑 `build_portfolio.py`。

## 8. 首次盘后验证

净值公布后（通常 20:00–23:00 CST）：

```bash
./target/debug/fin-lens validate YYYY-MM-DD
```

它会：

1. 取 `quote_daily` 当天涨跌；
2. 逐只基金用 `service::estimate_fund` 估算；
3. 对比 `nav(当日).daily_pct`；
4. 写 `data/daily/YYYY-MM/YYYY-MM-DD.md`；
5. 用 `calibration::update` 写 EWMA 状态。

`n_obs < 3` 时偏差项不启用；满 3 个交易日后自动生效。重复执行是幂等的。

## 9. 常见问题

| 现象 | 原因 | 处理 |
|---|---|---|
| `quotes_on 为空` | 当天没有 `quote_daily` | 先 `build_seed`/`build_portfolio`，确认日期是交易日 |
| 某只基金被跳过 | 缺前一日净值/新基金 | 换下一个已公布净值日；不要手工补 |
| 联接基误差数百 bp | 用了场内价而不是 ETF 净值 | 确认 `quote_daily.source='eastmoney_fund_nav'` |
| QDII 显示「美股T-1」 | 美股休市 + 净值滞后 | 正常；缺汇率会额外打 `fx_missing` |
| 资源桶大量「未分类」 | `buckets` 没有覆盖 ETF/联接基 | 补 `buckets`，重跑 `build_portfolio.py` 并 seed |
| 成本显示「—」 | `cost_basis` 与 `pnl_pct` 都缺失 | 正常；不要填 0 |
| 历史曲线短 | 回填日期晚于净值历史 | 重跑 `build_history.py`，或换更长历史 |
| 迁移 checksum 失败 | 改了已应用的迁移 | 不要改已应用迁移；新增 `000N_*.sql` |

## 10. 隐私检查

```bash
git check-ignore -v data/private/funds_source.json \
  data/private/portfolio_<date>.json \
  AGENTS.local.md seeds/funds.sql data/finlens.db

git ls-files -ci --exclude-standard
# 必须为空：没有任何被忽略却仍被跟踪的文件
```

- 不要 `git add -f data/private`。
- 不要把终端输出里的金额/成本贴进 issue、README 或截图。
- 截图只放 `data/private/`；公开文档只用 `docs/dashboard-demo.png`（DEMO）。
- 备份 `data/private/`（输入 + notes）和 `data/finlens.db`；数据库可从输入 + seeds 重建。
