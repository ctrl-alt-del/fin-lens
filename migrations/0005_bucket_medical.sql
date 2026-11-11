-- 0005: 医药资源桶。
--
-- 桶成员由私有组合输入（portfolio_*.json 的 `buckets`）生成，
-- 不把任何真实基金代码写进迁移。

INSERT INTO bucket (code, name, kind, note)
VALUES ('medical', '医药桶', 'sector', '医药/医疗相关 ETF 联接')
ON CONFLICT(code) DO NOTHING;
