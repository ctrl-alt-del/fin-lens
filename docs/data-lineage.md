# FinLens 数据血缘与数据管线

> 一页说明：数据从哪来、经过谁、写进哪张表、被谁读、以及“新增一类数据”该怎么接。
> 本文只描述事实，不定义规则；与 [AGENTS.md](../AGENTS.md) 冲突时以 AGENTS.md 为准。
> 规模数字随本机组合变化，不在文档里复述；真实数据只存 `data/private/`、`seeds/`、`data/*.db`（均不入库）。
> 接入自己的组合见 [onboarding.md](onboarding.md)。

## 1. 管线总览

```
东方财富公开接口                       collector/                          seeds/                          SQLite              读方
─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
push2his kline ───────────┐
api.fund.eastmoney lsjz ──┼──→ build_seed.py ───────────→ funds.sql ────────┐
pingzhongdata ────────────┘                                                      │
fundmobapi 持仓 ─────────────→ build_etf_lookthrough.py → etf_lookthrough_*.sql ─┤
fundmobapi 持仓 ─────────────→ build_portfolio.py* ─────→ portfolio_*.sql ───────┼→ fin-lens seed → data/finlens.db → db::repo → service ─→ API / UI
pingzhongdata + kline ───────→ build_history.py ────────→ history_*.sql ─────────┘                                              └→ validate ─→ calibration + data/daily/*.md
```

`*` `build_portfolio.py` 复用 `build_seed.py` 的抓取与 secid 解析函数；Python 只产出 SQL，
**运行时不碰数据库、不联网**。

日期与路径来自私有 JSON：

- `funds_source.json`：`report_date`、`validation_date`、`snapshot_date`。
- `portfolio_<as_of>.json`：`as_of`、`nav_date`、`report_date`。
- CLI 可用 `--private-dir`、`--portfolio`、`--funds`、`--out` 覆盖；默认自动发现唯一的
  `data/private/portfolio_*.json`。

## 2. 数据源

| 数据 | 接口 | 抓取方 | 缓存 | 口径 |
|---|---|---|---|---|
| 个股/ETF/指数日线 | `push2his.eastmoney.com/api/qt/stock/kline/get`，`secid=<市场>.<代码>` | build_seed / build_portfolio / build_history | `.cache/seed/kline_*`、`.cache/history/` | 前复权 `fqt=1`；当日返回的是盘中最新价 |
| 基金净值/涨跌 | `api.fund.eastmoney.com/f10/lsjz`（`pageSize≤20`） | build_seed / build_portfolio | `.cache/seed/nav_*` | `JZZZL` 百分数 ÷100 |
| 基金全量历史净值 | `fund.eastmoney.com/pingzhongdata/<code>.js` 的 `Data_netWorthTrend` | build_history | `.cache/history/` | 一次请求拿全历史；x 毫秒按 +8h 取日期 |
| 资产配置 | 同上，`Data_assetAllocation` | build_seed / build_portfolio | `.cache/seed/alloc_*` | 取最后一期股票/债券/现金占比 |
| 基金前十大持仓 | `fundmobapi.eastmoney.com/FundMNewApi/FundMNInverstPosition` | build_portfolio | 无（低频） | `JZBL` 百分数 ÷100，只到前十大 |
| ETF 成分 | 同上，传 ETF 代码 | build_etf_lookthrough | 无 | 前十大，构成单票归因的下限；`INDEXNAME` 可作行业回退 |
| USD/CNH、美股 | `push2his`，`secid=133.USDCNH` / `106.*` / `105.*` / `107.*` | build_portfolio | 同 kline | QDII 用 T-1 美股收盘 + 汇率变动 |

secid 市场前缀：`1`=上交所（6/688）、`0`=深交所（0/3）、`116`=港股、`106`=NYSE、`105`=NASDAQ、
`107`=美股 ETF、`133`=外汇。指数：沪深300 `1.000300`、创业板指 `0.399006`、科创50 `1.000688`。

美股/汇率的 `secid` 从私有 `instruments` 读取，不在脚本里写死 ticker。

## 3. 生成物与执行顺序

| 顺序 | 命令 | 产出 | 说明 |
|---|---|---|---|
| 1 | `python3 collector/build_seed.py --refresh` | `seeds/funds.sql` | 基金主档、季报持仓、资产配置、验证日行情 |
| 2 | `python3 collector/build_portfolio.py` | `seeds/portfolio_<as_of>.sql` | 份额/成本/交易/A-C/资源桶；新标的行情 |
| 3 | `python3 collector/build_etf_lookthrough.py` | `seeds/etf_lookthrough_<as_of>.sql` | **先 DELETE** 相关基金旧 holding，再写 ETF 前十大/黄金代理 |
| 4 | `python3 collector/build_history.py` | `seeds/history_<as_of>.sql` | 一年净值 + 日线；`ON CONFLICT DO NOTHING` |
| 5 | `./target/debug/fin-lens seed <文件>` | 写库 | 按 1→4 顺序灌，幂等 |

顺序不能乱：第 3 步依赖第 1、2 步已经写入的 holding 行（它先删后写）；第 4 步是补历史，
`DO NOTHING` 不会修正错误的历史行，口径错了要删库重灌。

## 4. 表血缘

| 表 | 写入方 | 读取方 / 用途 | 频率 |
|---|---|---|---|
| `instrument` | build_seed、build_portfolio、build_etf_lookthrough、demo | repo 名称/行业解析、lookthrough | 新增标的时 |
| `fund` | build_seed、build_portfolio、demo | 基金字典、benchmark/ETF 映射 | 新增基金时 |
| `fund_family` | build_portfolio | 组合域视图 `v_family_position` | 新增 A/C 时 |
| `holding` | build_seed（季报）、build_portfolio（手工）、build_etf_lookthrough（替换） | `latest_holdings`/`all_latest_holdings` → 半透明估值、穿透 | 季报 + ETF 成分刷新 |
| `fund_allocation` | build_seed、build_portfolio | `latest_stock_weight` → 半透明残余仓位 | 季报 |
| `nav` | build_seed、build_portfolio、build_history | `latest_nav`/`nav_before`/`nav_on`/`all_nav_history` → 估值、验证、趋势 | 每日 |
| `quote_snapshot` | build_seed、build_portfolio、demo | `all_quotes` → 实时估算；`latest_snapshot` → 页面时点 | 盘中 |
| `quote_daily` | build_seed、build_portfolio、build_history | `quotes_on` → `validate`；`all_quote_history` → 趋势 | 每日收盘 |
| `position` | build_portfolio | `positions` → 总市值、穿透权重 | 持仓变动时 |
| `portfolio_snapshot` | build_portfolio | `latest_portfolio_snapshot` → 在途现金、成本 | 每次重算 |
| `trade` | build_portfolio | 组合域视图、Phase C 流水 | 有交易记录时 |
| `calibration` | `fin-lens validate`（Rust） | `latest_calibration_before` → EWMA 偏差 | 每交易日盘后 |
| `tracking_fit` | 仅 demo | `latest_beta` → 透明基金 β | 预留（真实基金暂用 `fund.tracking_beta`） |
| `bucket` | migration 0003 + 0005 | `service::bucket_of` | 固定 |
| `bucket_member` | build_portfolio | `service::bucket_of` → 资源桶切片/归因 | 持仓变动时 |
| `position_lot` | 未写 | `v_lot_position`（预留 FIFO） | Phase C |
| `estimate` | 未写 | 预留（当前 API 实时计算，不落库） | — |
| `alert` / `lookthrough_exposure` | 未写 | 预留（当前告警在 API/UI 内存计算） | — |
| `ingest_batch` | 未使用 | 预留（幂等记录） | — |
| `setting` | migration 0002 | 未读（配置走 `FINLENS_*` 环境变量） | — |

`bucket_member` 是资源桶的唯一来源：Rust 不写死基金/ETF 代码。没有成员的 ETF/联接基显示「未分类」。

## 5. 新增数据源 / 新数据类别

1. **先归类**：日线、净值、持仓、份额、汇率——已有表就复用；没有就新增 `migrations/000N_*.sql`
   （STRICT 表、TEXT 日期；不要改已应用的迁移）。
2. **接解析**：东财代码 → `instrument.code` 用前缀式 `SH./SZ./HK./US./IDX./ETF./AU./FX.`；
   在 `build_seed.py` 的 `secid_for_stock`/`exchange_of`/`currency_of` 里补规则。
3. **抓取与缓存**：放 `collector/build_*.py`，缓存到 `.cache/<name>/`，提供 `--refresh`；
   缓存键必须含日期/URL，避免拿旧数据当新数据。
4. **写 SQL 要幂等**：`INSERT … ON CONFLICT DO UPDATE`（可修正）或 `DO NOTHING`（只补历史）；
   生成物放 `seeds/`，不手改。
5. **口径写清楚**：ETF/黄金用基金净值不用场内价；QDII 用 T-1 + 汇率；缺失不填 0，打 flag。
6. **验证**：`fin-lens seed` 前后对比 `count(*)`；跑 `validate`；`curl /api/estimates` 看覆盖率；
   趋势数据用 `history_view` 抽查端点。
7. **文档**：更新 [AGENTS.md](../AGENTS.md)、[README.md](../README.md)、
   [collector/README.md](../collector/README.md) 与对应 skill。

### 新增资源桶

1. 在新 migration 里 `INSERT INTO bucket (code, name, kind, note)`（例如 `0005_bucket_medical.sql`）。
2. 在私有 `portfolio_<date>.json` 的 `buckets` 里加上 `桶代码 → [基金代码]`。
3. 重跑 `build_portfolio.py` 并 seed；它会先删除 `note LIKE 'handoff_%'` 的旧成员，再写当前成员。
4. 验证：`/partials/overview` 的资源桶 donut 不再出现「未分类」，桶权重与持仓一致。

## 6. 已知的坑（都是实际踩过的）

| 坑 | 后果 | 正确做法 |
|---|---|---|
| 用场内 ETF 价格当联接基基准 | 单日误差可达数百 bp | 用 ETF 净值 `JZZZL`，`quote_daily.source='eastmoney_fund_nav'` |
| 把“已公布净值”当成当天 | 首日验证日期错一天 | 净值 T 日盘后公布，验证日 = 净值日，不是自然日 |
| 迁移新增后不重新编译 | `sqlx::migrate!` 不感知新文件 | `touch src/db/mod.rs && cargo build` |
| 服务与 seed 并发启动 | `duplicate column name` 迁移竞态 | 先单独启动/编译一次，再跑 seed |
| holding 里手工行与 ETF 展开行并存 | 穿透权重重复 | `build_etf_lookthrough` 先 DELETE 再写 |
| 缺行情填 0% | 估值“看起来正常” | 滚入残余仓位 + `missing_holding_quote`，覆盖率下降 |
| 缓存不刷新 | 拿到昨天的行情 | `--refresh`；缓存键含日期 |
| 重生成持仓不清旧 trade 行 | 同一只基金残留多个版本的流水 | seed 前 `DELETE FROM trade WHERE source='handoff' AND trade_date=<D>`；真实流水不删 |
| 重生成桶成员不清旧行 | 基金换桶后旧成员残留 | `build_portfolio.py` 先 `DELETE FROM bucket_member WHERE note LIKE 'handoff_%'` |
| 改了已应用的迁移 | `sqlx` checksum 失败 | 新增 migration；首次公开提交前必须重建数据库并重跑 `validate` |

## 7. 验证命令

```bash
export CARGO_HOME="$PWD/.cargo-home"

# 行数与最新时点
sqlite3 data/finlens.db "
SELECT 'fund', count(*) FROM fund
UNION ALL SELECT 'holding', count(*) FROM holding
UNION ALL SELECT 'nav', count(*) FROM nav
UNION ALL SELECT 'quote_daily', count(*) FROM quote_daily
UNION ALL SELECT 'quote_snapshot', count(*) FROM quote_snapshot
UNION ALL SELECT 'position', count(*) FROM position
UNION ALL SELECT 'bucket_member', count(*) FROM bucket_member;"
sqlite3 data/finlens.db "SELECT max(trade_date) FROM nav; SELECT max(quote_ts) FROM quote_snapshot;"

# 估值与穿透
curl -s localhost:8787/api/estimates | jq '{as_of, count, skipped}'
curl -s localhost:8787/api/lookthrough | jq '{attributed: .report.attributed_weight, alerts: [.report.alerts[] | {name, severity}]}'

# 盘后对账
./target/debug/fin-lens validate <published-nav-date>
```

## 8. 不变量

- 运行时（Rust）不访问网络，只读 SQLite；采集全部在 `collector/`。
- `db/repo.rs` 是唯一写 SQL 的地方；`domain/` 不依赖 sqlx/axum/时钟。
- 估值只有一条路径：API、UI、`validate` 都走 `service::estimate_fund`。
- 历史读取严格 `< as_of`，回补不引入前视。
- 生成物可重复生成，`seeds/` 不手改，`data/` 不提交。
- 页面上的判断句是规则生成，不是 LLM。
- 资源桶来自 `bucket_member`，不在 Rust 里写死标的代码。
