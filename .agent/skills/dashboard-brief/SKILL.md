---
name: dashboard-brief
description: 只读叙述层：读 FinLens One-pager/API，用确定性数字生成"今天发生了什么 / 哪个数字不可信 / 下一步查什么"三段简报；不做算术、不给投资建议、不修改任何数据。
---

# dashboard-brief — 把一页数字讲成三段话

## 何时用

- 用户问"今天怎么样""帮我看看这页""为什么跌/为什么跑赢"。
- 每日收盘后、`daily-validation` 之前，需要一段可转述的摘要。
- 需要跨天对比，但不想手工翻截图。

## 不该用

- 需要改数据、改估值口径、跑 seed → 用 `market-refresh` / `onboard-fund`。
- 净值已公布、要判断"估得准不准" → 用 `daily-validation`（它才有误差口径）。
- 用户要买卖建议 → 明确拒绝：这是监控台，不是投顾。

## 铁律（违反任何一条就停下来重做）

1. **所有数字必须来自当前响应**：`curl /api/lookthrough`、`/api/funds`，
   或页面里明确标注的同一字段。不许凭上一轮记忆、不许心算复利/加权。
2. **算差值可以，但必须写公式**：例如 `今日盈亏 / 今日市值 = 今日涨跌幅`；不要口算或心算后只给结果。
   如果 API 已给百分比，直接用 API 的，不重新算。
3. **区分事实 / 推断 / 推测**：
   - 事实：字段原值（如 `attributed_weight=<value>`、`alerts[0].total_weight=<value>`）；
   - 推断：基于字段的规则结论（"黄金代理越过 5% 单标的口径"）；
   - 推测：需要外部信息的（"可能因为隔夜美股"）——必须标"推测"，且不写进数字句。
4. **不给投资建议**：不说"该减仓/该加仓/建议买入"，只说"需要核对/需要补数据/值得关注"。
5. **不改任何文件、不跑 seed、不重启服务**：这是只读技能。

## 输入（按顺序，缺一项就在简报里明说）

```bash
# 批量估值（全部基金；与 Dashboard 共用 service::overview_all）
curl -s http://127.0.0.1:8787/api/estimates | jq '{
  as_of, count, skipped,
  worst: [.funds | sort_by(.estimated_pct) | .[0:5][]
          | {code, name, pct: (.estimated_pct * 100), coverage, flags}]
}'

# 穿透（真实市值权重 + ETF 前十大口径）
curl -s http://127.0.0.1:8787/api/lookthrough | jq '{
  as_of,
  weights_assumed_equal,
  resolved: .report.resolved_weight,
  attributed: .report.attributed_weight,
  top: [.report.exposures[0:8][] | {name, instrument, w: .total_weight, funds: .fund_count, sector}],
  alerts: [.report.alerts[] | {name, w: .total_weight, severity}],
  sectors: [.report.sectors[0:6][] | {sector, w: .weight}]
}'

# 基金字典（只有元数据，没有估值）；估值的单只接口：/api/funds/{code}/estimate
curl -s http://127.0.0.1:8787/api/funds | jq 'length'
```

页面 `/` 只需读三处：Hero（总市值/今日/1 年/回撤）、② 风险（5 条体检 + Action items）、③ 归因。
`/partials/overview` 是同一份数据的局部刷新，不要当成新时点。

## 步骤

1. **先报时点**：`as_of`/`quote_ts` 与当前 CST 差值；超过 15 分钟就写"盘中数据非最新"。
2. **再报总量**：总市值、今日估算盈亏及百分比、成本覆盖只数（`cost_known/positions`）。
3. **解释涨跌**：用 `/api/estimates` 的 `funds[].estimated_pct` 取拖累前三，*不解释原因*，除非归因表有对应桶。
4. **解释 1 年超额**：只用 ③ 的 headline/attribution 原话；强调"当前份额回溯、非真实历史"。
5. **列数据可信度**：`attributed_weight`（ETF 仅前十大，属**下限**）、
   快照时点、`skipped` 列表、缺成本只数。
6. **给 3 条下一步**：优先取 Action items 的 title；没有就按"补数据 > 核对口径 > 观察"排序。

## 输出模板（最多 3 段，每段 ≤3 条）

```text
【时点】11-06 17:38 CST；quote_ts 距今 X 分钟；快照/净值口径：…

【今天】总市值 ¥…；今日估算 −¥…（−X.XX%）。
拖累前三：…（−X.XX%）、…、…。
一句话结论（原文引用）：…

【哪个数字不可信】单票归因覆盖率（ETF 只展开前十大，下限）；缺成本 N 只，累计盈亏只覆盖 …；
…（快照过期/跳过基金/覆盖率下降，有则写，无则写"未发现"）。

【下一步】① … ② … ③ …（来自 Action items，不新增建议）
```

## 失败处理

- **接口非 200**：先 `curl /healthz`；服务没起就说明"Dashboard 不可用"，不要用缓存数字硬写。
- **`attributed_weight` < 0.5**：在简报里把"集中度"降级为"仅参考"，并明确写覆盖不足。
- **`weights_assumed_equal=true`**：开篇就写"穿透权重是等权假设，不是真实仓位"，不要复述具体百分比结论。
- **数字对不上**（Hero 与 ③ 的收益差 >0.1pp）：停下，报告差异与两个来源，不要圆场。
- **用户追问"为什么"但没有对应桶**：回答"当前数据只能归因到资源桶，不能归因到单只基金/新闻"，不要编。

## 不要做

- 不要把某个桶的贡献改写成"你选股能力强"——归因是仓位×净值，不是能力证据。
- 不要把黄金桶的告警说成"个股集中"；它是商品代理敞口。
- 不要用 LLM 重算收益率、权重或误差；只引用、只排序、只措辞。
- 不要把本简报写回数据库或当成 `calibration` 输入。
