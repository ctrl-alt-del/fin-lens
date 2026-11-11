#!/usr/bin/env python3
"""ETF 联接基的成分穿透 + 黄金基金的商品映射。

联接基只把目标 ETF 的净值记进「其他资产」，季报里没有股票持仓，所以主种子
（build_portfolio.py）里这些基金没有 holding 行，穿透矩阵会把它们当成盲区。

本脚本做两件事，只产出 SQL，不碰数据库：

1. 对每只带 `etf_code` 的联接基，抓目标 ETF 的前十大成分股，按
   `基金持有 ETF 比例 × ETF 内成分权重` 写成 holding 行（source='etf_proxy'）。
   注意：只展开前十大（ETF 只披露前十大），所以这是**下限**，不是完整成分。
2. 对黄金基金（type/category = commodity），以 `AU.GOLD` 作为商品敞口代理，
   权重 = 基金的非股票/债券/现金仓位（≈ 持有黄金 ETF 的比例）。

用法：
    python3 collector/build_etf_lookthrough.py [--refresh]
    fin-lens seed seeds/etf_lookthrough_<as_of>.sql
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import build_seed as bs
import config

ETF_HOLD_URL = (
    "https://fundmobapi.eastmoney.com/FundMNewApi/FundMNInverstPosition"
    "?FCODE={code}&deviceid=Wap&plat=Wap&product=EFund&version=2.0.0"
)
ETF_REFERER = "https://fundf10.eastmoney.com/"


def load_funds(funds_path: Path, portfolio_path: Path) -> tuple[dict[str, dict], dict, dict]:
    """合并基金主档与组合新增基金，返回 code -> 元数据。"""
    base = json.loads(funds_path.read_text(encoding="utf-8"))
    portfolio = json.loads(portfolio_path.read_text(encoding="utf-8"))
    funds: dict[str, dict] = {}
    for f in base["funds"]:
        funds[f["code"]] = f
    for code, meta in portfolio["new_funds"].items():
        funds.setdefault(code, {"code": code, **meta})
    return funds, base, portfolio


def etf_holdings(etf_code: str) -> list[dict]:
    """目标 ETF 的前十大成分股（GPDM/GPJC/JZBL）。"""
    raw_code = etf_code.split(".", 1)[1]

    def producer() -> str:
        return bs.fetch(ETF_HOLD_URL.format(code=raw_code), referer=ETF_REFERER)

    text = bs.cached(f"etf_holdings_{raw_code}", producer)
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        return []
    return (payload.get("Datas") or {}).get("fundStocks") or []


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="生成 ETF 联接基穿透 + 黄金代理种子 SQL")
    parser.add_argument("--portfolio", help="portfolio_*.json 路径")
    parser.add_argument("--funds", help="funds_source.json 路径")
    config.add_path_args(parser)
    args = parser.parse_args(argv)
    bs.REFRESH = args.refresh

    if bs.REFRESH:
        print("refresh: 重新抓取 ETF 成分", flush=True)

    private = config.private_dir(args.private_dir)
    funds_path = config.funds_source_path(private, args.funds)
    portfolio_path = config.portfolio_path(private, args.portfolio)

    funds, base, portfolio = load_funds(funds_path, portfolio_path)
    report_date = base["report_date"]
    as_of = portfolio["as_of"]
    out_path = config.output_path(
        args.out, config.seed_dir() / f"etf_lookthrough_{as_of}.sql"
    )
    sector_map: dict[str, str] = base.get("sector_map") or {}

    feeders = {c: f for c, f in funds.items() if f.get("etf_code")}
    commodities = {
        c: f
        for c, f in funds.items()
        if f.get("type") == "commodity" or f.get("category") == "commodity"
    }

    # 目标 ETF 只抓一次（同一目标 ETF 可能被多只联接基共用）。
    etf_codes = sorted({f["etf_code"] for f in feeders.values()})
    etf_rows = {code: etf_holdings(code) for code in etf_codes}

    # 联接基「其他资产」≈ 持有的目标 ETF 比例；用 1 - 股票 - 债券 - 现金 估算。
    allocs: dict[str, dict] = {}
    for code in list(feeders) + list(commodities):
        try:
            alloc = bs.allocation(code) or {}
        except Exception as err:  # noqa: BLE001 - 单个基金失败不阻断全部
            print(f"  warn {code}: allocation 失败 {err}", flush=True)
            alloc = {}
        allocs[code] = alloc

    def etf_weight(code: str) -> float:
        alloc = allocs.get(code) or {}
        other = 1.0 - (alloc.get("stock") or 0.0) - (alloc.get("bond") or 0.0) - (
            alloc.get("cash") or 0.0
        )
        return max(0.0, min(1.0, other))

    lines: list[str] = []
    add = lines.append

    add("-- 由 collector/build_etf_lookthrough.py 生成，不要手改。")
    add(f"-- 数据源: Eastmoney 目标 ETF 前十大成分（{report_date} 季报口径）")
    add("-- 口径:   ETF 联接基 = 联接基持有 ETF 比例 × ETF 成分权重（前十大，下限）")
    add("--         黄金基金 = AU.GOLD 代理，权重 = 非股票/债券/现金仓位")
    add(f"-- 用法:   fin-lens seed {out_path.name}")
    add("")
    add("BEGIN;")
    add("")

    # 先清掉这些基金在同一个报告期的旧 holding 行：手工录入的单票权重
    # 会被 ETF 真实前十大口径替代，避免同一标的重复计权。
    all_codes = sorted(set(feeders) | set(commodities))
    if all_codes:
        code_list = ", ".join(bs.q(c) for c in all_codes)
        add(f"DELETE FROM holding WHERE report_date = {bs.q(report_date)} AND fund_code IN ({code_list});")
        add("")

    # 成分股标的字典（sector 优先用人工 sector_map，缺失记「其他」）。
    instrument_seen: dict[str, str] = {}
    add("-- 1. ETF 成分股标的")
    for etf_code in etf_codes:
        for row in etf_rows[etf_code]:
            raw = str(row.get("GPDM") or "").strip()
            if not raw:
                continue
            try:
                icode = bs.instrument_code(raw)
            except ValueError:
                continue
            name = str(row.get("GPJC") or raw)
            sector = (
                sector_map.get(raw)
                or str(row.get("INDEXNAME") or "").strip()
                or "其他"
            )
            instrument_seen[icode] = sector
            add(
                "INSERT INTO instrument (code, name, kind, exchange, currency, sector, source, source_symbol) "
                f"VALUES ({bs.q(icode)}, {bs.q(name)}, 'stock', {bs.q(bs.exchange_of(icode))}, "
                f"{bs.q(bs.currency_of(icode))}, {bs.q(sector)}, 'eastmoney', {bs.q(raw)}) "
                "ON CONFLICT(code) DO UPDATE SET name=excluded.name, sector=excluded.sector, "
                "source_symbol=excluded.source_symbol, is_active=1;"
            )
    add("")

    add("-- 2. 联接基 → 目标 ETF 前十大（权重 = ETF 仓位 × 成分权重）")
    for code, fund in sorted(feeders.items()):
        ew = etf_weight(code)
        rows = etf_rows.get(fund["etf_code"]) or []
        total = sum(float(r.get("JZBL") or 0.0) / 100.0 for r in rows)
        add(f"-- {code} {fund.get('name', '')}：ETF 仓位 {ew:.4f}，前十大合计 {total:.4f}")
        for rank, row in enumerate(rows, start=1):
            raw = str(row.get("GPDM") or "").strip()
            if not raw:
                continue
            try:
                icode = bs.instrument_code(raw)
            except ValueError:
                continue
            w = ew * float(row.get("JZBL") or 0.0) / 100.0
            add(
                "INSERT INTO holding (fund_code, report_date, instrument_code, weight, rank, source) "
                f"VALUES ({bs.q(code)}, {bs.q(report_date)}, {bs.q(icode)}, {bs.fnum(w)}, "
                f"{rank}, 'etf_proxy') "
                "ON CONFLICT(fund_code, report_date, instrument_code) DO UPDATE SET "
                "weight=excluded.weight, rank=excluded.rank, source=excluded.source;"
            )
    add("")

    add("-- 3. 黄金基金 → AU.GOLD 商品敞口")
    for code, fund in sorted(commodities.items()):
        w = etf_weight(code)
        add(
            "INSERT INTO holding (fund_code, report_date, instrument_code, weight, rank, source) "
            f"VALUES ({bs.q(code)}, {bs.q(report_date)}, 'AU.GOLD', {bs.fnum(w)}, 1, 'gold_proxy') "
            "ON CONFLICT(fund_code, report_date, instrument_code) DO UPDATE SET "
            "weight=excluded.weight, rank=excluded.rank, source=excluded.source;"
        )
        add(f"-- {code} {fund.get('name', '')}：黄金代理权重 {w:.4f}")
    add("")
    add("COMMIT;")
    add("")

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text("\n".join(lines), encoding="utf-8")
    print(
        f"wrote {out_path} ({len(lines)} lines): "
        f"feeders={len(feeders)} commodities={len(commodities)} etfs={len(etf_codes)}",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
