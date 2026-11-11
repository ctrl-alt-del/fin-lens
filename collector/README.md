# collector — Python 采集/种子生成（弱耦合）

约束只有一条：**采集脚本不碰数据库**。它们把东方财富公开接口整理成可执行的 SQL 种子，
由 Rust 侧的 `fin-lens seed` 灌库。

这样做的理由：

- 行情源是整条链路里最不稳定的一环。它崩了、限流了、字段改名了，不能连带数据库和估值逻辑一起挂。
- Python 侧不需要知道表结构，也就不会绕过 `repo.rs` 的约束。
- 每次生成都是纯文本 SQL，可 diff、可回放、可重复执行。

## 输入与输出

所有真实输入都在 `data/private/`（忽略，不入库）：

```text
data/private/
├── funds_source.json
└── portfolio_YYYY-MM-DD.json
```

默认输出：

```text
seeds/funds.sql
seeds/portfolio_YYYY-MM-DD.sql
seeds/etf_lookthrough_YYYY-MM-DD.sql
seeds/history_YYYY-MM-DD.sql
```

路径与日期都来自私有 JSON；可以用 `--private-dir`、`--portfolio`、`--funds`、`--out` 覆盖。
详细字段见 [docs/onboarding.md](../docs/onboarding.md)。

## 脚本

| 脚本 | 作用 | 关键口径 |
|---|---|---|
| `build_seed.py` | 基金主档、季报持仓、资产配置、验证日净值/日线/快照 | 联接基基准用 ETF 净值；缺价格不写快照 |
| `build_portfolio.py` | 份额/成本/交易/A-C family/资源桶 | 份额 = 市值/净值；成本未知写 NULL；同基金多笔保留多行 trade |
| `build_etf_lookthrough.py` | 联接基展开目标 ETF 前十大 + 黄金代理 | 前十大是下限；先 DELETE 旧 holding 再写 |
| `build_history.py` | 一年基金净值 + 个股/指数日线 | `ON CONFLICT DO NOTHING`；只补历史 |
| `validate_inputs.py` | 离线校验私有 JSON 结构 | 不联网、不写库、不打印金额 |

## 顺序（不能换）

```bash
python3 collector/validate_inputs.py

python3 collector/build_seed.py --refresh
python3 collector/build_portfolio.py
python3 collector/build_etf_lookthrough.py
python3 collector/build_history.py

fin-lens seed seeds/funds.sql
fin-lens seed seeds/portfolio_YYYY-MM-DD.sql
fin-lens seed seeds/etf_lookthrough_YYYY-MM-DD.sql
fin-lens seed seeds/history_YYYY-MM-DD.sql
```

`build_etf_lookthrough.py` 依赖前两步已经写入的 holding 行；它会先 DELETE 同一报告期
相关基金的旧 holding，再按 ETF 前十大/黄金代理重写。

## 三条硬规则

1. **ETF/黄金联接基的日线基准取 ETF 净值，不取场内价格。**
   场内价格含折溢价；改用净值后这类误差应接近 0。行写
   `quote_daily.source='eastmoney_fund_nav'`，盘中快照仍用实时价格。
2. **缺当日价格就不写快照**，绝不伪造 0% 涨跌；估值内核会用 `MissingHoldingQuote` 明示。
3. **口径写进数据，不写进代码。** 基金代码、真实权重、日期、基准标的都来自
   `data/private/*.json`；公开脚本里不出现真实组合。

## 缓存与刷新

- `build_seed.py` / `build_portfolio.py` / `build_etf_lookthrough.py` 使用 `.cache/seed/`。
- `build_history.py` 使用 `.cache/history/`。
- `--refresh` 绕过缓存重新抓取；不刷新时用缓存，适合复现同一天的种子。
- 缓存键尽量含请求日期/URL；跨日刷新用 `--refresh`，不要把旧数据当新数据。

## 可选字段

- `funds_source.ground_truth_pct`：`{基金代码: 当日涨跌百分数}`，`build_seed.py` 用它做交叉核对；
  缺失则跳过，不影响估值。
- `instruments[*].secid`：美股/汇率标的的东财 secid（例如 `US.XOP` → `107.XOP`）；
  `build_history.py` 从私有 `instruments` 读取，不在脚本里写死 ticker。
- `portfolio.buckets`：桶代码 → 基金代码数组。桶代码：
  `gold`, `nonferrous`, `broad_index`, `oil`, `semiconductor`, `medical`, `optical`, `compute`。

## 样例

`sample/` 下是**虚构的 ingest 契约样例**，全部使用 `DEMO.*` 标的：

- `quotes_2026-11-06_143000.json` — 盘中报价快照
- `nav_2026-11-05.json` — 盘后净值
- `holdings_2026-09-30.json` — 季报前十大
- `allocation_2026-09-30.json` — 股票/债券/现金/其他占比

输入模板见 `docs/examples/inputs/`，同样是虚构数据，不可直接抓取。

## 输出契约（ingest 样例）

统一信封：

```json
{
  "kind": "quotes" | "nav" | "holdings" | "allocation",
  "generated_at": "YYYY-MM-DDTHH:MM:SS+08:00",
  "source": "eastmoney" | "manual",
  "trade_date": "YYYY-MM-DD",
  "items": [ ... ]
}
```

硬性要求：

1. **比率一律用小数**：涨 1.2% 写 `0.012`，权重 9.8% 写 `0.098`。
2. **标的用统一 code**：`SH.600519`、`SZ.300308`、`IDX.000300`、`ETF.159819`、`AU.GOLD`。
3. **日期 `YYYY-MM-DD`**，时间戳 RFC 3339 带时区。
4. 缺数据就**不写该条**，不要填 `0` 或 `null` 冒充。
5. 文件名：`{kind}_{trade_date}_{HHMMSS}.json`。

## 计划中的脚本

| 脚本 | 数据源 | 输出 |
|---|---|---|
| `fetch_quotes.py` | 东方财富实时行情 | `quotes_*.json` |
| `fetch_nav.py` | 天天基金 `f10/lsjz` | `nav_*.json` |
| `fetch_holdings.py` | 天天基金季报 + 手工校正 | `holdings_*.json` |
| `fetch_allocation.py` | 基金资产配置 | `allocation_*.json` |

调度方式：由 Rust 的轮询 Task 按需 `subprocess` 调用（只针对当前页面可见标的），或用户手动跑。
**不做常驻 Python 守护进程。**

## 幂等

`build_seed.py` / `build_portfolio.py` 用 `ON CONFLICT DO UPDATE`，可重复执行；
`build_history.py` 用 `DO NOTHING`，只补历史。重复 seed 不会重复插入，但不会修正错误的历史行。
