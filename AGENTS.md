# AGENTS.md — FinLens 工作约定

本文件写给所有在本仓库工作的 AI Agent（Claude Code / Codex / DSH 等）和人类协作者。
目标只有一个：**别让确定性估值系统被“顺手改坏”**。工作流类任务见 `.agent/skills/`。

## 项目是什么

FinLens 是一个本地基金穿透监控系统：本机配置的基金池的持仓（季报）+ 盘中行情，
推算出组合层面“你以为分散、实际重仓同一批光模块/算力股”的暴露。

- 后端：Rust + Axum + sqlx/SQLite，单二进制，常驻内存很小。
- 前端：HTMX 服务端渲染（`src/ui.rs`），无 Node 构建链。
- 数据：东方财富公开接口，由 `collector/build_*.py` 抓取并生成 SQL；
  **Rust 运行时只读 SQLite，不直接访问网络。**
- 校准：每日盘后对比“估算涨跌 vs 实际净值涨跌”，EWMA 偏差项在收益率空间累积。

真实基金池、持仓、金额、成本、盈亏、截图与日报**不在仓库里**。接入自己的组合见
[docs/onboarding.md](docs/onboarding.md)；本机状态见 `AGENTS.local.md`（忽略，不提交）。

## 常用命令

```bash
export CARGO_HOME="$PWD/.cargo-home"   # 沙箱/受限环境需要

cargo fmt
cargo check --all-targets
cargo test

# 校验私有输入（不联网、不写库）
python3 collector/validate_inputs.py

# 生成 / 刷新真实数据（日期与路径来自私有 JSON）
python3 collector/build_seed.py --refresh
python3 collector/build_portfolio.py
python3 collector/build_etf_lookthrough.py
python3 collector/build_history.py

# 灌库（按同一顺序；文件名由脚本按 as_of 生成）
./target/debug/fin-lens seed seeds/funds.sql
./target/debug/fin-lens seed seeds/portfolio_YYYY-MM-DD.sql
./target/debug/fin-lens seed seeds/etf_lookthrough_YYYY-MM-DD.sql
./target/debug/fin-lens seed seeds/history_YYYY-MM-DD.sql

# 盘后单日验证（写 data/daily/YYYY-MM/YYYY-MM-DD.md + calibration 行）
./target/debug/fin-lens validate YYYY-MM-DD

# 起服务：http://127.0.0.1:8787/
FINLENS_DB=data/finlens.db FINLENS_BIND=127.0.0.1:8787 cargo run
```

Dashboard 路由：`/`（页面）、`/partials/overview`（htmx 局部刷新）、
`/api/funds`、`/api/estimates`（批量估值）、`/api/funds/{code}/estimate`、`/api/lookthrough`、`/healthz`。

## 目录与依赖方向

```
src/domain/    纯函数内核：估值公式、EWMA、穿透、回归。禁止 I/O / 时钟 / 随机数。
src/db/        仓储层：repo.rs 是唯一允许写 SQL 的地方（全部运行时 query_as）。
src/service.rs 装配层：估值/穿透/组合汇总/1 年历史与体检；API、UI、validate 共用。
src/api/       HTTP JSON 适配层。
src/ui.rs      One-pager 渲染（Hero/体检/资源桶/趋势/Reference），只读，不做业务计算。
src/validate.rs 盘后对账 + Markdown 日报。
collector/     Python 采集/种子生成，只产出 SQL，不碰数据库。
seeds/         生成物，可重复生成，不要手改。
data/          SQLite、日报快照、原始采集缓存与私有输入（全部忽略）。
```

依赖只能向下：`api/ui/validate → service → db::repo → domain`。

## 接入自己的组合

1. 读 [docs/onboarding.md](docs/onboarding.md) 与 `docs/examples/inputs/`。
2. 在 `data/private/` 创建 `funds_source.json` 与 `portfolio_<date>.json`。
3. 先跑 `collector/validate_inputs.py`，再按顺序生成种子。
4. 不要提交 `data/private/`、`seeds/`、`data/*.db`、`data/daily/`、`state/`、`AGENTS.local.md`。

## 硬约束（改代码前先读）

1. **domain 保持纯净**：不 import sqlx/axum、不读时钟、不读环境变量。新算法先写纯函数 + 单测。
2. **repo 是唯一 SQL 入口**：不要用编译期 `query!`/`query_as!` 宏（构建机没有库也能编译），
   不要在其他模块拼 SQL。所有日期字段是 TEXT ISO（`YYYY-MM-DD`）。
3. **真实数据不进 migrations**：`sqlx::migrate!` 会在测试内存库执行；真实基金与持仓走 `seeds/` + `fin-lens seed`。
4. **单一估值路径**：页面、API、`validate` 必须都走 `service::estimate_fund`。
   任何“为了页面快一点”的旁路都会让校准闭环失效。
5. **无前视偏差**：取历史一律严格 `< as_of`（`nav_before`、`latest_calibration_before`）。
   回补某天时不能看见该天之后的数据。
6. **ETF/黄金联接基的基准是基金净值，不是场内价格**。场内价含折溢价，溢价波动日会放大成数百 bp 的假误差。
   `quote_daily.source = 'eastmoney_fund_nav'` 记录口径。
7. **偏差在收益率空间**：残差 = 实际涨跌 − **未加偏差项**的原始估算（避免正反馈），
   `calibration` 表同时留 `residual_nav` 备查。
8. **缺失行情不静默当 0**：滚入残余仓位并打 `missing_holding_quote`，覆盖率和置信度下降。
9. **QDII 的 T-1 + 汇率口径**：A 股白天美股休市，只能用上一夜美股涨跌（配置的美股行业指数代理）
   并乘以 USD/CNY 变动（`FX.USDCNH`，正 = 人民币贬值）。公式保留 `(1+β·us)×(1+fx)−1` 交叉项。
   库里 `fund.mode` 仍存 `transparent`，用 `category='qdii_index'` 判定走 `estimate_qdii`；
   缺汇率打 `fx_missing`，恒打 `qdii_lag`。QDII 当日净值未公布时，`prev_nav` 用 ≤ nav_date 的最近净值，不得伪造。
10. **盘中快照 vs 日线**：`quote_snapshot` 用于实时估算，`quote_daily` 用于盘后验证；快照缺失时不要伪造 0% 涨跌。
11. **UI 是只读渲染器**：不要在里面查库拼业务逻辑；只调用 `service`。
12. **回溯必须标注口径**：组合/穿透趋势是「当前份额回放」，不是账户真实历史收益；
    风险指标只用已实现波动/回撤，不写“未来风险预测”。页面、README、API 口径要一致。
13. **判断句必须确定性、可归因**：顶部的「一句话」和趋势结论由规则生成，不要接 LLM 自由发挥；
    归因使用「当前份额 × 期初/期末净值」，加总≈组合收益即可，差异要写明；不得把跨资产比较说成选股能力。
14. **资源桶来自 `bucket_member`**：不要在 Rust 里写死基金/ETF 代码。没有桶成员的 ETF/联接基显示「未分类」。
15. **migrations 在首次提交后不可变**：新增 schema 用新文件；真实数据不进迁移。
    首次公开提交前若必须清理已应用的迁移，先备份数据库、重建并重跑 `validate`，不要只改文件不重建。

## 数据管线（源 → 生成物 → 表）

完整血缘、表级读写方与已知坑见 [docs/data-lineage.md](docs/data-lineage.md)。改动数据前先读那一页。

```
东方财富 → collector/build_*.py → seeds/*.sql → fin-lens seed → data/finlens.db
        → db::repo → service → API/UI；validate → calibration + data/daily/*.md
```

- **顺序冻结**：`build_seed` → `build_portfolio` → `build_etf_lookthrough`（先 DELETE 再写 holding）
  → `build_history`（`DO NOTHING`，只补历史）→ 按同一顺序 `seed`。乱序会重复或漏掉穿透行。
- **写库只有两条路**：`fin-lens seed`（应用 Python 生成的 SQL）与 `validate`（写 `calibration`）；
  Python 永不直连 SQLite，Rust 运行时永不联网。
- **新增一类数据**：先归入 日线/净值/持仓/份额/汇率；没有现成表就加 `migrations/000N_*.sql`；
  抓取放 `collector/` 并带缓存与 `--refresh`；SQL 必须幂等；口径（净值 vs 场内价、T-1、缺失不填 0）
  写进注释与文档。
- **未启用的表**（`estimate`/`alert`/`lookthrough_exposure`/`ingest_batch`/`position_lot`）
  是预留结构；不要因为表存在就假设有数据。`bucket`/`bucket_member` 已启用，服务从前者读分类、从后者读成员。

## 本机私有状态

- `AGENTS.local.md`：本机基金数、持仓数、验证基线等；已忽略。
- `data/private/notes/`：状态、Agent 基线、真实代码/权重的验证经验；已忽略。
- 如果这些文件不存在，不要猜测组合细节；直接读 API/SQLite 的当前输出。

## 提交前检查

- [ ] `cargo fmt` 已跑
- [ ] `cargo test` 全绿（如果失败的是领域不变量，先质疑测试预期，别直接改数字）
- [ ] `cargo check --all-targets` 无 warning 新增
- [ ] `python3 collector/validate_inputs.py` 通过（有私有输入时）
- [ ] `git ls-files -ci --exclude-standard` 为空（没有被忽略却仍被跟踪的文件）
- [ ] 若改了 schema：新增 `migrations/000N_*.sql`，不要改已应用的迁移；`STRICT` 表
- [ ] 若改了估值口径：同步更新 `README.md`、`AGENTS.md` 与本文件提到的硬约束
- [ ] 若动了 `collector/build_seed.py`：重新生成种子并跑一次 `validate`，确认误差没有爆炸

## 不要做的事

- 不要把未知成本/未知市值填成 0 来“凑平”总市值与累计盈亏。
- 不要为了“更好看”把估值逻辑挪进 `ui.rs` 或前端 JS。
- 不要引入 Redis / Node / 前端框架：这是刻意选择的极简运行时。
- 不要在 `data/finlens.db` 里手改数字来“修”验证误差——误差本身就是产品输出。
- 不要把 `position` 权重和基金内持仓权重混为一谈。
- 不要在未确认 `quote_ts` 时点时把快照当成收盘价。
- 不要把「当前份额回溯」写成账户真实历史收益，也不要用它推断未来。
- 不要把金额、成本、盈亏、当前权重、真实基金代码或截图提交进 Git；不要 `git add -f`。

## Agent 工作流技能

见 `.agent/README.md` 与 `.agent/skills/`：

- `market-refresh` — 盘前/盘后刷新行情、ETF 成分与种子
- `daily-validation` — 净值公布后跑对账、解读误差、落校准行
- `onboard-fund` — 新增/替换一只基金
- `dashboard-triage` — 看盘：读 Dashboard/API，判断今天有没有需要处理的事
- `dashboard-brief` — 只读叙述：把一页数字写成“今天/不可信/下一步”三段，数字只引用不重算
