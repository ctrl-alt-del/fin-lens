-- 0003: 组合域 —— A/C 类合并、多笔交易、FIFO 批次、资源桶。
--
-- 设计口径：
--   * 金额单位：元；份额单位：份；日期 TEXT ISO（YYYY-MM-DD）。
--   * 金额用 REAL（分析级精度，不做分账）。若将来接券商对账单需要逐分对账，
--     应另建 INTEGER 分表，而不是改这里的业务列。
--   * trade 是交易事实（append-only，external_id 幂等）；position 是物化快照；
--     position_lot 是可选的 FIFO 批次（用于持有期/赎回费与逐笔成本）。
--
-- A/C 类合并：
--   fund_family 表示“同一只基金的不同收费份额”（如某基金的 A / C 份额）。
--   穿透时仍按基金代码分别贡献权重（A/C 各自有净值），family_id 只用于：
--     1) 展示合并（一张卡 = 一个 family）；
--     2) 某类份额缺持仓数据时复用同 family 的持仓；
--     3) 资源桶归属与去重告警。

CREATE TABLE fund_family (
    id             INTEGER PRIMARY KEY,
    code           TEXT NOT NULL UNIQUE,          -- 内部稳定标识，如 EDITS_INFO
    name           TEXT NOT NULL,                 -- 展示名，如 某基金
    manager        TEXT,
    benchmark_code TEXT REFERENCES instrument(code),
    note           TEXT,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
) STRICT;

ALTER TABLE fund ADD COLUMN family_id INTEGER REFERENCES fund_family(id);
ALTER TABLE fund ADD COLUMN share_class TEXT
    CHECK (share_class IS NULL OR share_class IN ('A','C','E','I','O','ETF','LOF'));
ALTER TABLE fund ADD COLUMN share_class_note TEXT;

CREATE INDEX idx_fund_family ON fund(family_id);

-- 交易流水：一笔买入/卖出/分红/再投一行；多笔买入 = 多行。
-- amount 口径：buy = 支付金额（含费前/含费后由 fee 单独记）、sell = 到账金额（扣费后）、
--              dividend = 现金分红、reinvest = 再投金额（此时 shares/nav 同时给出）。
CREATE TABLE trade (
    id           INTEGER PRIMARY KEY,
    external_id  TEXT UNIQUE,                     -- 平台单号/对账单号，导入幂等
    fund_code    TEXT NOT NULL REFERENCES fund(code) ON DELETE CASCADE,
    trade_date   TEXT NOT NULL,                   -- 申请日
    confirm_date TEXT,                            -- 确认日（QDII 常滞后 1-2 天）
    kind         TEXT NOT NULL
                 CHECK (kind IN ('buy','sell','dividend','reinvest')),
    shares       REAL NOT NULL DEFAULT 0 CHECK (shares >= 0),
    nav          REAL CHECK (nav IS NULL OR nav > 0),
    amount       REAL NOT NULL CHECK (amount >= 0),
    fee          REAL NOT NULL DEFAULT 0 CHECK (fee >= 0),
    source       TEXT NOT NULL DEFAULT 'manual',  -- manual / alipay / tiantian / broker_csv
    note         TEXT,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    CHECK (kind NOT IN ('buy','sell','reinvest') OR (shares > 0 AND nav IS NOT NULL)),
    CHECK (kind <> 'dividend' OR amount > 0)
) STRICT;

CREATE INDEX idx_trade_fund_date ON trade(fund_code, trade_date, id);
CREATE INDEX idx_trade_date ON trade(trade_date);

-- FIFO 批次：每笔买入生成一个 lot；卖出按 open_date 先进先出核销 remaining_shares。
-- cost_nav = (amount + fee) / shares，cost_amount = 原始总投入。
CREATE TABLE position_lot (
    id               INTEGER PRIMARY KEY,
    trade_id         INTEGER REFERENCES trade(id) ON DELETE SET NULL,
    fund_code        TEXT NOT NULL REFERENCES fund(code) ON DELETE CASCADE,
    open_date        TEXT NOT NULL,
    shares           REAL NOT NULL CHECK (shares > 0),
    remaining_shares REAL NOT NULL CHECK (remaining_shares >= 0),
    cost_nav         REAL NOT NULL CHECK (cost_nav > 0),
    cost_amount      REAL NOT NULL CHECK (cost_amount >= 0),
    closed_at        TEXT,
    note             TEXT,
    CHECK (remaining_shares <= shares)
) STRICT;

CREATE INDEX idx_lot_open ON position_lot(fund_code, open_date)
    WHERE remaining_shares > 0;

-- 当前持仓可按批次汇总（与 position 物化快照互为校验）。
CREATE VIEW v_lot_position AS
SELECT fund_code,
       SUM(remaining_shares)            AS shares,
       SUM(remaining_shares * cost_nav) AS cost_basis,
       COUNT(*)                          AS open_lots,
       MIN(open_date)                    AS first_open_date,
       MAX(open_date)                    AS last_open_date
FROM position_lot
WHERE remaining_shares > 0
GROUP BY fund_code;

-- 组合快照补总成本，便于一眼算总盈亏。
ALTER TABLE portfolio_snapshot ADD COLUMN total_cost REAL;
ALTER TABLE portfolio_snapshot ADD COLUMN note TEXT;

-- 当前持仓 + 最新净值：市值 / 盈亏 / 盈亏率。
-- 净值缺失时回退到 position.market_value（导入时手填的市值）。
CREATE VIEW v_position_live AS
SELECT p.fund_code,
       f.name,
       f.family_id,
       f.share_class,
       p.shares,
       p.cost_basis,
       n.unit_nav,
       n.trade_date AS nav_date,
       COALESCE(p.shares * n.unit_nav, p.market_value) AS market_value,
       COALESCE(p.shares * n.unit_nav, p.market_value) - COALESCE(p.cost_basis, 0.0) AS pnl,
       CASE
           WHEN p.cost_basis > 0 AND p.shares IS NOT NULL AND n.unit_nav IS NOT NULL
           THEN p.shares * n.unit_nav / p.cost_basis - 1.0
       END AS pnl_pct,
       p.updated_at
FROM position p
JOIN fund f ON f.code = p.fund_code
LEFT JOIN v_latest_nav n ON n.fund_code = p.fund_code;

-- 按 family 合并后的持仓视图（A/C 类一起看）。
CREATE VIEW v_family_position AS
SELECT f.family_id,
       SUM(v.market_value)                AS market_value,
       SUM(v.cost_basis)                  AS cost_basis,
       SUM(v.market_value) - SUM(v.cost_basis) AS pnl
FROM v_position_live v
JOIN fund f ON f.code = v.fund_code
WHERE f.family_id IS NOT NULL
GROUP BY f.family_id;

-- 交易现金流按月汇总（后续算 XIRR / 观察加仓节奏用）。
CREATE VIEW v_trade_flow AS
SELECT fund_code,
       substr(trade_date, 1, 7) AS month,
       SUM(CASE WHEN kind = 'buy'      THEN amount + fee ELSE 0 END) AS buy_amount,
       SUM(CASE WHEN kind = 'sell'     THEN amount - fee ELSE 0 END) AS sell_amount,
       SUM(CASE WHEN kind = 'dividend' THEN amount       ELSE 0 END) AS dividend_amount,
       SUM(CASE WHEN kind = 'reinvest' THEN amount + fee ELSE 0 END) AS reinvest_amount
FROM trade
GROUP BY fund_code, substr(trade_date, 1, 7);

-- 资源/主题桶：黄金、有色、光模块、算力、半导体、宽基、原油。
-- 桶成员由导入流程或人工维护；穿透引擎负责按桶汇总真实暴露。
CREATE TABLE bucket (
    id            INTEGER PRIMARY KEY,
    code          TEXT NOT NULL UNIQUE,
    name          TEXT NOT NULL,
    kind          TEXT NOT NULL
                  CHECK (kind IN ('resource','theme','sector','broad')),
    target_weight REAL CHECK (target_weight IS NULL
                              OR (target_weight >= 0 AND target_weight <= 1)),
    note          TEXT
) STRICT;

CREATE TABLE bucket_member (
    bucket_id  INTEGER NOT NULL REFERENCES bucket(id) ON DELETE CASCADE,
    fund_code  TEXT NOT NULL REFERENCES fund(code) ON DELETE CASCADE,
    role       TEXT NOT NULL DEFAULT 'member'
               CHECK (role IN ('member','proxy','hedge')),
    weight     REAL CHECK (weight IS NULL OR (weight >= 0 AND weight <= 1)),
    note       TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (bucket_id, fund_code)
) STRICT;

CREATE INDEX idx_bucket_member_fund ON bucket_member(fund_code);

INSERT INTO bucket (code, name, kind, note) VALUES
    ('gold',          '黄金桶',     'resource', '实物金/上海金/黄金 ETF 联接，按基金净值跟金价'),
    ('nonferrous',    '有色金属桶', 'resource', '铜/铝/锌/黄金股等有色 ETF 联接'),
    ('optical',       '光模块桶',   'theme',    '光模块龙头等，穿透后自动汇总'),
    ('compute',       '算力桶',     'theme',    '算力硬件龙头等'),
    ('semiconductor', '半导体桶',   'sector',   '设备/材料/芯片设计'),
    ('broad_index',   '宽基指数桶', 'broad',    '沪深300/上证50/创业板/科创创业50/红利'),
    ('oil',           '原油桶',     'resource', 'QDII-LOF 原油');
