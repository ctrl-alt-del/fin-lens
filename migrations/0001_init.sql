-- =============================================================================
-- fin-lens 初始 Schema
-- =============================================================================
-- 设计原则
--   1. 一张表只存一类「结构化事实」，不存可推导的中间结果（估值分解存 JSON，
--      但原始持仓/净值/行情永远分开存）。
--   2. 所有比率统一用小数（0.05 = 5%）：权重、涨跌幅、覆盖率、偏差。
--   3. 日期统一 TEXT ISO-8601（`YYYY-MM-DD`），时间戳统一 UTC RFC3339。
--      SQLite 的 STRICT 表不允许 DATE 类型，且 TEXT 日期可直接做字典序比较。
--   4. 主键尽量用业务自然键 + 报告期，保证采集脚本重复投递时幂等
--      （`INSERT ... ON CONFLICT DO UPDATE`）。
--   5. 全部使用 STRICT 表：类型错误在写入时就炸，而不是等到 Rust 反序列化。
-- =============================================================================

-- -----------------------------------------------------------------------------
-- 0. 标的字典：股票 / 指数 / ETF / 商品 / 汇率 统一命名空间
-- -----------------------------------------------------------------------------
CREATE TABLE instrument (
    code          TEXT PRIMARY KEY,              -- 内部统一代码：SH.000000 / IDX.000300 / AU.GOLD / FX.USDCNH
    name          TEXT NOT NULL,
    kind          TEXT NOT NULL CHECK (kind IN ('stock','index','etf','commodity','fx','bond','cash')),
    exchange      TEXT,                          -- SSE / SZSE / SGE / HKEX ...
    currency      TEXT NOT NULL DEFAULT 'CNY',
    sector        TEXT,                          -- 申万一级或自定义：光模块 / 半导体 / 食品饮料
    source        TEXT,                          -- 采集来源：akshare / sina / tiantian
    source_symbol TEXT,                          -- 采集端原始符号（避免两边反复映射）
    is_active     INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0,1)),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
) STRICT;

CREATE INDEX idx_instrument_kind ON instrument(kind, is_active);
CREATE INDEX idx_instrument_sector ON instrument(sector);

-- -----------------------------------------------------------------------------
-- 1. 基金主档：基金身份、估值模式、基准
-- -----------------------------------------------------------------------------
CREATE TABLE fund (
    code           TEXT PRIMARY KEY,             -- 6 位基金代码
    name           TEXT NOT NULL,
    short_name     TEXT,
    -- 估值模式决定走哪条内核路径，见 domain::types::ValuationMode
    mode           TEXT NOT NULL CHECK (mode IN ('transparent','semi_transparent','opaque')),
    category       TEXT,                         -- index_feeder / commodity / active_equity / hybrid / bond
    benchmark_code TEXT REFERENCES instrument(code),  -- 指数 / 金价 / 目标 ETF
    benchmark_name TEXT,
    etf_code       TEXT REFERENCES instrument(code),  -- 指数联接基实际持有的目标 ETF
    tracking_beta  REAL,                         -- 人工覆盖值；为空则用 tracking_fit 最新回归
    notes          TEXT,                         -- 风格漂移等人工备注
    is_active      INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0,1)),
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
) STRICT;

CREATE INDEX idx_fund_mode ON fund(mode, is_active);

-- 只在业务字段变更时刷新，避免每次 UPDATE 都改时间戳
CREATE TRIGGER trg_fund_touch AFTER UPDATE OF name, short_name, mode, category,
    benchmark_code, benchmark_name, etf_code, tracking_beta, notes, is_active ON fund
BEGIN
    UPDATE fund SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE code = NEW.code;
END;

-- -----------------------------------------------------------------------------
-- 2. 季报持仓（最新报告期由视图选出；保留历史便于回测与调仓对比）
-- -----------------------------------------------------------------------------
CREATE TABLE holding (
    fund_code       TEXT NOT NULL REFERENCES fund(code) ON DELETE CASCADE,
    report_date     TEXT NOT NULL,               -- 报告期 YYYY-MM-DD
    instrument_code TEXT NOT NULL REFERENCES instrument(code),
    weight          REAL NOT NULL CHECK (weight >= 0 AND weight <= 1),
    rank            INTEGER CHECK (rank IS NULL OR rank >= 1),
    shares          REAL,                        -- 万股
    market_value    REAL,                        -- 万元
    source          TEXT,
    fetched_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (fund_code, report_date, instrument_code)
) STRICT;

CREATE INDEX idx_holding_instrument ON holding(instrument_code, report_date DESC);
CREATE INDEX idx_holding_report ON holding(report_date);

-- -----------------------------------------------------------------------------
-- 3. 资产配置（权益/债券/现金比例）：决定主动基残余仓位怎么算
-- -----------------------------------------------------------------------------
CREATE TABLE fund_allocation (
    fund_code    TEXT NOT NULL REFERENCES fund(code) ON DELETE CASCADE,
    report_date  TEXT NOT NULL,
    stock_weight REAL NOT NULL DEFAULT 0 CHECK (stock_weight >= 0 AND stock_weight <= 1),
    bond_weight  REAL NOT NULL DEFAULT 0 CHECK (bond_weight >= 0 AND bond_weight <= 1),
    cash_weight  REAL NOT NULL DEFAULT 0 CHECK (cash_weight >= 0 AND cash_weight <= 1),
    other_weight REAL NOT NULL DEFAULT 0 CHECK (other_weight >= 0 AND other_weight <= 1),
    net_assets   REAL,                           -- 亿元
    source       TEXT,
    fetched_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (fund_code, report_date)
) STRICT;

-- -----------------------------------------------------------------------------
-- 4. 单位净值（盘后校准的唯一真值来源）
-- -----------------------------------------------------------------------------
CREATE TABLE nav (
    fund_code  TEXT NOT NULL REFERENCES fund(code) ON DELETE CASCADE,
    trade_date TEXT NOT NULL,
    unit_nav   REAL NOT NULL CHECK (unit_nav > 0),
    accum_nav  REAL CHECK (accum_nav IS NULL OR accum_nav > 0),
    daily_pct  REAL,                             -- 可空；由校准任务从相邻两日回填
    source     TEXT,
    fetched_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (fund_code, trade_date)
) STRICT;

CREATE INDEX idx_nav_date ON nav(trade_date);

-- -----------------------------------------------------------------------------
-- 5. 你的实际仓位（穿透矩阵的真正权重来源）
-- -----------------------------------------------------------------------------
CREATE TABLE position (
    fund_code    TEXT PRIMARY KEY REFERENCES fund(code) ON DELETE CASCADE,
    shares       REAL,                           -- 持有份额
    cost_basis   REAL,                           -- 成本额（元）
    market_value REAL,                           -- 最新市值（元）
    weight       REAL CHECK (weight IS NULL OR (weight >= 0 AND weight <= 1)),
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
) STRICT;

CREATE TABLE portfolio_snapshot (
    as_of_date  TEXT PRIMARY KEY,
    total_value REAL NOT NULL,                   -- 元
    cash        REAL NOT NULL DEFAULT 0,
    source      TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
) STRICT;

-- -----------------------------------------------------------------------------
-- 6. 行情：快照表给热路径，日线表给回归/回测
-- -----------------------------------------------------------------------------
CREATE TABLE quote_snapshot (
    instrument_code TEXT PRIMARY KEY REFERENCES instrument(code) ON DELETE CASCADE,
    price           REAL NOT NULL,
    prev_close      REAL,
    pct_chg         REAL NOT NULL,               -- 相对昨收，小数
    quote_ts        TEXT NOT NULL,               -- 行情自带时间（ISO8601 带时区）
    trade_date      TEXT NOT NULL,
    source          TEXT,
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
) STRICT;

CREATE INDEX idx_quote_snapshot_date ON quote_snapshot(trade_date);

CREATE TABLE quote_daily (
    instrument_code TEXT NOT NULL REFERENCES instrument(code) ON DELETE CASCADE,
    trade_date      TEXT NOT NULL,
    open            REAL,
    high            REAL,
    low             REAL,
    close           REAL NOT NULL,
    pct_chg         REAL,                        -- 小数
    volume          REAL,
    source          TEXT,
    PRIMARY KEY (instrument_code, trade_date)
) STRICT;

CREATE INDEX idx_quote_daily_date ON quote_daily(trade_date);

-- -----------------------------------------------------------------------------
-- 7. 跟涨系数回归结果（20 日窗口，每日滚动）
-- -----------------------------------------------------------------------------
CREATE TABLE tracking_fit (
    fund_code      TEXT NOT NULL REFERENCES fund(code) ON DELETE CASCADE,
    benchmark_code TEXT NOT NULL REFERENCES instrument(code),
    as_of_date     TEXT NOT NULL,
    window_days    INTEGER NOT NULL,
    beta           REAL NOT NULL,
    alpha          REAL NOT NULL DEFAULT 0,
    r2             REAL,
    n_obs          INTEGER NOT NULL,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (fund_code, benchmark_code, as_of_date, window_days)
) STRICT;

CREATE INDEX idx_tracking_fit_latest ON tracking_fit(fund_code, as_of_date DESC);

-- -----------------------------------------------------------------------------
-- 8. 盘中估值快照（保留分解明细，便于事后解释“这 1.2% 是怎么来的”）
-- -----------------------------------------------------------------------------
CREATE TABLE estimate (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    fund_code     TEXT NOT NULL REFERENCES fund(code) ON DELETE CASCADE,
    trade_date    TEXT NOT NULL,
    as_of_ts      TEXT NOT NULL,                 -- 本次估算的时刻（UTC RFC3339）
    prev_nav      REAL NOT NULL,
    estimated_nav REAL NOT NULL CHECK (estimated_nav > 0),
    raw_pct       REAL NOT NULL,                 -- 未加偏差项
    bias_pct      REAL NOT NULL DEFAULT 0,
    estimated_pct REAL NOT NULL,
    coverage      REAL NOT NULL DEFAULT 0,
    confidence    REAL NOT NULL DEFAULT 0,
    flags_json    TEXT,                          -- ["missing_holding_quote", ...]
    detail_json   TEXT,                          -- [{label, weight, pct, contribution}]
    is_final      INTEGER NOT NULL DEFAULT 0 CHECK (is_final IN (0,1)),  -- 收盘后定稿
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
) STRICT;

CREATE INDEX idx_estimate_fund_date ON estimate(fund_code, trade_date, as_of_ts DESC);
CREATE INDEX idx_estimate_date ON estimate(trade_date);

-- -----------------------------------------------------------------------------
-- 9. 预测 → 验证 → 校准闭环
-- -----------------------------------------------------------------------------
CREATE TABLE calibration (
    fund_code     TEXT NOT NULL REFERENCES fund(code) ON DELETE CASCADE,
    trade_date    TEXT NOT NULL,
    estimated_nav REAL NOT NULL,
    actual_nav    REAL NOT NULL,
    residual_nav  REAL NOT NULL,                 -- 实际 − 估算（交接文档口径，保留备查）
    residual_pct  REAL NOT NULL,                 -- 实际涨跌 − 原始估算涨跌（校准输入）
    ewma_bias_pct REAL NOT NULL,                 -- 更新后的偏差（次日起使用）
    ewma_abs_pct  REAL NOT NULL,                 -- 更新后的误差带
    n_obs         INTEGER NOT NULL,
    lambda        REAL NOT NULL,
    outlier       INTEGER NOT NULL DEFAULT 0 CHECK (outlier IN (0,1)),  -- 1 = 被判为异常值未采纳
    note          TEXT,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (fund_code, trade_date)
) STRICT;

CREATE INDEX idx_calibration_date ON calibration(trade_date);

-- -----------------------------------------------------------------------------
-- 10. 穿透暴露矩阵（每个交易日一份，用于趋势与历史回看）
-- -----------------------------------------------------------------------------
CREATE TABLE lookthrough_exposure (
    trade_date         TEXT NOT NULL,
    instrument_code    TEXT NOT NULL,
    instrument_name    TEXT,
    sector             TEXT,
    total_weight       REAL NOT NULL CHECK (total_weight >= 0 AND total_weight <= 1),
    fund_count         INTEGER NOT NULL,
    fund_breakdown_json TEXT,                    -- [{"fund":"F1","weight":0.05}]
    is_alert           INTEGER NOT NULL DEFAULT 0 CHECK (is_alert IN (0,1)),
    computed_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (trade_date, instrument_code)
) STRICT;

CREATE INDEX idx_lookthrough_top ON lookthrough_exposure(trade_date, total_weight DESC);
CREATE INDEX idx_lookthrough_alert ON lookthrough_exposure(is_alert, trade_date DESC);

-- -----------------------------------------------------------------------------
-- 11. 告警
-- -----------------------------------------------------------------------------
CREATE TABLE alert (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    trade_date   TEXT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('overlap','drift','stale_quote','data_gap','calibration')),
    severity     TEXT NOT NULL CHECK (severity IN ('info','warn','critical')),
    subject      TEXT NOT NULL,                  -- 例如标的代码 / 基金代码
    message      TEXT NOT NULL,
    payload_json TEXT,
    acknowledged INTEGER NOT NULL DEFAULT 0 CHECK (acknowledged IN (0,1)),
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
) STRICT;

CREATE INDEX idx_alert_open ON alert(acknowledged, trade_date DESC);

-- -----------------------------------------------------------------------------
-- 12. 采集批次：Python 弱耦合交接的幂等闸门
-- -----------------------------------------------------------------------------
CREATE TABLE ingest_batch (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    source        TEXT NOT NULL,                 -- akshare / sina / tiantian
    kind          TEXT NOT NULL CHECK (kind IN ('holdings','nav','quotes','allocation','instruments')),
    payload_path  TEXT,
    payload_sha256 TEXT NOT NULL,
    rows_ok       INTEGER NOT NULL DEFAULT 0,
    rows_failed   INTEGER NOT NULL DEFAULT 0,
    status        TEXT NOT NULL CHECK (status IN ('pending','applied','failed','skipped')),
    error         TEXT,
    ingested_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    UNIQUE (payload_sha256)                      -- 同一份文件重复投递直接 skipped
) STRICT;

CREATE INDEX idx_ingest_status ON ingest_batch(status, ingested_at DESC);

-- -----------------------------------------------------------------------------
-- 13. 运行时参数（前端可改，不必重启）
-- -----------------------------------------------------------------------------
CREATE TABLE setting (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
) STRICT;

-- =============================================================================
-- 视图：把「最新一期」这类高频取数逻辑固化，避免每个查询都写窗口函数
-- =============================================================================

CREATE VIEW v_latest_nav AS
SELECT n.*
FROM nav n
JOIN (
    SELECT fund_code, MAX(trade_date) AS max_date
    FROM nav
    GROUP BY fund_code
) latest ON latest.fund_code = n.fund_code AND latest.max_date = n.trade_date;

CREATE VIEW v_latest_holding AS
SELECT h.*
FROM holding h
JOIN (
    SELECT fund_code, MAX(report_date) AS max_date
    FROM holding
    GROUP BY fund_code
) latest ON latest.fund_code = h.fund_code AND latest.max_date = h.report_date;

CREATE VIEW v_latest_allocation AS
SELECT a.*
FROM fund_allocation a
JOIN (
    SELECT fund_code, MAX(report_date) AS max_date
    FROM fund_allocation
    GROUP BY fund_code
) latest ON latest.fund_code = a.fund_code AND latest.max_date = a.report_date;

-- 每只基金当日最后一笔盘中估值
CREATE VIEW v_latest_estimate AS
SELECT e.*
FROM estimate e
JOIN (
    SELECT fund_code, trade_date, MAX(as_of_ts) AS max_ts
    FROM estimate
    GROUP BY fund_code, trade_date
) latest ON latest.fund_code = e.fund_code
        AND latest.trade_date = e.trade_date
        AND latest.max_ts = e.as_of_ts;

-- 最新一份穿透矩阵
CREATE VIEW v_latest_lookthrough AS
SELECT x.*
FROM lookthrough_exposure x
WHERE x.trade_date = (SELECT MAX(trade_date) FROM lookthrough_exposure);
