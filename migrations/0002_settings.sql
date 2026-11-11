-- 运行时默认参数。改这里只影响新库；已存在的库请用 UPDATE setting。
INSERT INTO setting (key, value) VALUES
    ('overlap_alert_threshold', '0.05'),   -- 单一标的穿透权重告警线
    ('ewma_lambda',             '0.25'),   -- 偏差 EWMA 平滑系数
    ('ewma_min_obs',            '3'),      -- 偏差生效所需最少观测
    ('ewma_clamp_pct',          '0.003'),  -- 偏差绝对值上限（0.3%）
    ('ewma_half_life_days',     '10'),     -- 偏差半衰期
    ('quote_stale_seconds',     '300'),    -- 行情超过多久算过期
    ('poll_interval_seconds',   '60'),     -- 盘中轮询间隔
    ('regression_window_days',  '20');     -- 跟涨系数回归窗口
