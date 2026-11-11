-- 0004: 持仓成本来源与备注；成本未知时盈亏保持 NULL，不伪装成 0。
--
-- 背景：部分新纳入的基金只拿到「当前市值」，没有成本/盈亏率。
-- 反推成本 = 市值 / (1 + 盈亏率) 只对已知盈亏率的基金成立；成本未知时
-- cost_basis 留 NULL，并在 cost_source 里标记 'unknown'，页面显示「—」。

ALTER TABLE position ADD COLUMN cost_source TEXT;
ALTER TABLE position ADD COLUMN note TEXT;

DROP VIEW IF EXISTS v_family_position;
DROP VIEW IF EXISTS v_position_live;

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
       CASE
           WHEN p.cost_basis IS NULL THEN NULL
           ELSE COALESCE(p.shares * n.unit_nav, p.market_value) - p.cost_basis
       END AS pnl,
       CASE
           WHEN p.cost_basis > 0 AND p.shares IS NOT NULL AND n.unit_nav IS NOT NULL
           THEN p.shares * n.unit_nav / p.cost_basis - 1.0
       END AS pnl_pct,
       p.cost_source,
       p.note,
       p.updated_at
FROM position p
JOIN fund f ON f.code = p.fund_code
LEFT JOIN v_latest_nav n ON n.fund_code = p.fund_code;

CREATE VIEW v_family_position AS
SELECT f.family_id,
       SUM(v.market_value) AS market_value,
       SUM(v.cost_basis)   AS cost_basis,
       CASE
           WHEN SUM(v.cost_basis) IS NULL THEN NULL
           ELSE SUM(v.market_value) - SUM(v.cost_basis)
       END AS pnl
FROM v_position_live v
JOIN fund f ON f.code = v.fund_code
WHERE f.family_id IS NOT NULL
GROUP BY f.family_id;
