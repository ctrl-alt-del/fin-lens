#!/usr/bin/env python3
"""把私有组合的「市值 + 收益率」变成 SQLite 持仓种子。

输入（均在 `--private-dir` 下，默认 `data/private`，不入库）：
  portfolio_*.json   市值/成本/新增基金/资源桶
  funds_source.json  基金身份、基准与季报持仓

输出：
  seeds/portfolio_<as_of>.sql（可用 `--out` 覆盖）

口径：
  * 份额 = 市值 / nav_date 单位净值；
  * 已知成本时：`cost_source='screenshot'`；
  * 已知收益率时：成本 = 市值 / (1 + 收益率)，`cost_source='derived_pnl'`；
  * 收益率未知时：cost_basis=NULL，cost_source='unknown'（页面显示「—」）；
  * 同一基金多笔买入在 trade 表保留多行，position 表按基金汇总；
  * 每条持仓生成一笔期初 trade（source='handoff'），后续真实流水可覆盖/追加。

设计约束沿用 build_seed.py：只抓取 + 生成 SQL，运行时不连接数据库。
"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_seed as bs  # noqa: E402
import config  # noqa: E402

HOLDINGS_URL = (
    "https://fundmobapi.eastmoney.com/FundMNewApi/FundMNInverstPosition"
    "?FCODE={code}&deviceid=Wap&plat=Wap&product=EFund&version=2.0.0"
)


def fetch_holdings(code: str) -> list[dict]:
    payload = json.loads(bs.cached(
        f"holdings_{code}",
        lambda: bs.fetch(HOLDINGS_URL.format(code=code), referer="https://fundf10.eastmoney.com/"),
    ))
    rows = ((payload.get("Datas") or {}).get("fundStocks")) or []
    out: list[dict] = []
    for row in rows[:10]:
        raw = str(row.get("GPDM") or "").strip()
        if not raw:
            continue
        out.append({
            "raw": raw,
            "name": row.get("GPJC") or "",
            "weight": float(row.get("JZBL") or 0.0) / 100.0,
            "sector": row.get("INDEXNAME") or None,
        })
    return out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="生成组合持仓/交易/资源桶种子 SQL")
    parser.add_argument("--portfolio", help="portfolio_*.json 路径")
    parser.add_argument("--funds", help="funds_source.json 路径")
    config.add_path_args(parser)
    args = parser.parse_args(argv)
    bs.REFRESH = args.refresh

    private = config.private_dir(args.private_dir)
    portfolio_path = config.portfolio_path(private, args.portfolio)
    funds_path = config.funds_source_path(private, args.funds)

    spec = json.loads(portfolio_path.read_text(encoding="utf-8"))
    base = json.loads(funds_path.read_text(encoding="utf-8"))
    nav_date = spec["nav_date"]
    as_of = spec["as_of"]
    report_date = spec["report_date"]
    kline_beg, kline_end = bs.kline_window(as_of, before=9, after=1)
    out_path = config.output_path(
        args.out, config.seed_dir() / f"portfolio_{as_of}.sql"
    )

    base_funds = {f["code"]: f for f in base["funds"]}
    base_instruments = dict(base["instruments"])
    sector_map = dict(base["sector_map"])
    instruments = dict(base_instruments)
    for code, meta in spec.get("instruments", {}).items():
        instruments[code] = meta

    warnings: list[str] = []
    new_instruments: dict[str, dict] = {}

    # ---- 抓新增主动基的季报持仓 -------------------------------------------
    holdings_by_fund: dict[str, list[dict]] = {}
    for code in spec.get("fetch_holdings", []):
        try:
            rows = fetch_holdings(code)
        except Exception as err:  # noqa: BLE001
            warnings.append(f"{code}: 持仓抓取失败 {err}")
            continue
        if not rows:
            warnings.append(f"{code}: 持仓为空（可能是新基金/未披露）")
        holdings_by_fund[code] = rows
        for row in rows:
            icode = bs.instrument_code(row["raw"])
            row["instrument"] = icode
            if icode not in instruments:
                sector = sector_map.get(row["raw"]) or row["sector"] or "其他"
                new_instruments[icode] = {
                    "name": row["name"],
                    "kind": "stock",
                    "sector": sector,
                    "secid": bs.secid_for_stock(row["raw"]),
                    "source_symbol": row["raw"],
                }
                instruments[icode] = new_instruments[icode]
        print(f"  holdings {code}: {len(rows)} rows", flush=True)

    # ---- 新增个股的行情（否则新基金只能靠残余仓位，覆盖率会很低）----------
    stock_closes: dict[str, dict[str, float]] = {}
    for icode in sorted(new_instruments):
        meta = new_instruments[icode]
        try:
            _, closes = bs.klines(meta["secid"], kline_beg, kline_end)
            stock_closes[icode] = closes
        except Exception as err:  # noqa: BLE001
            warnings.append(f"{icode}: 个股行情抓取失败 {err}")

    # ---- 净值 / 资产配置 / 基准行情 ---------------------------------------
    codes = sorted({p["code"] for p in spec["positions"]})
    navs: dict[str, dict[str, dict]] = {}
    for code in codes:
        try:
            navs[code] = bs.norm_nav(code)
        except Exception as err:  # noqa: BLE001
            navs[code] = {}
            warnings.append(f"{code}: 净值抓取失败 {err}")
        row = navs[code].get(nav_date)
        if row is None:
            # 可能是 QDII T-1 净值尚未公布；真正的回退与告警在计算持仓时统一处理。
            earlier = [d for d in (navs.get(code) or {}) if d < nav_date]
            print(
                f"  fund {code}: 无 {nav_date} 净值"
                + (f"，最近可用 {max(earlier)}" if earlier else "，且无历史净值"),
                flush=True,
            )
        else:
            print(f"  fund {code}: {nav_date} nav={row['nav']:.4f} pct={row['pct']}", flush=True)

    allocs: dict[str, dict | None] = {}
    for code in spec.get("fetch_holdings", []):
        if code in base_funds:
            continue
        try:
            allocs[code] = bs.allocation(code)
        except Exception as err:  # noqa: BLE001
            allocs[code] = None
            warnings.append(f"{code}: 资产配置抓取失败 {err}")

    # 新增基准 ETF 的净值与盘中价（已有基金的基准不重复抓）
    new_benchmarks = sorted({
        f.get("benchmark") for f in spec["new_funds"].values()
        if (f.get("benchmark") or "").startswith("ETF.")
    } - set(base_instruments))
    bench_nav: dict[str, dict[str, dict]] = {}
    bench_closes: dict[str, dict[str, float]] = {}
    for code in new_benchmarks:
        etf_code = code.split(".", 1)[1]
        try:
            bench_nav[code] = bs.norm_nav(etf_code)
        except Exception as err:  # noqa: BLE001
            warnings.append(f"{code}: ETF 净值抓取失败 {err}")
        meta = instruments[code]
        try:
            _, closes = bs.klines(meta["secid"], kline_beg, kline_end)
            bench_closes[code] = closes
        except Exception as err:  # noqa: BLE001
            warnings.append(f"{code}: ETF 行情抓取失败 {err}")

    # ---- 计算持仓 ----------------------------------------------------------
    positions = []
    total_value = 0.0
    total_cost = 0.0
    pending_cash = sum(float(p.get("pending_amount") or 0.0) for p in spec["positions"])
    for p in spec["positions"]:
        code = p["code"]
        nav_row = (navs.get(code) or {}).get(nav_date)
        nav_day = nav_date
        if nav_row is None:
            # QDII 的 T-1 净值通常晚一天公布；盘中不能用占位净值，取「≤ nav_date 的最近净值」。
            earlier = [d for d in (navs.get(code) or {}) if d < nav_date]
            if earlier:
                nav_day = max(earlier)
                nav_row = navs[code][nav_day]
                warnings.append(
                    f"{code}: 缺 {nav_date} 净值，使用 {nav_day} 净值反推份额（QDII T-1）"
                )
        if nav_row is None:
            continue
        mv = float(p["market_value"])
        nav = float(nav_row["nav"])
        shares = mv / nav
        # 截图直接给出持仓收益时，成本 = 市值 - 收益（比收益率反推更精确）。
        cost = p.get("cost_basis")
        if cost is not None:
            cost = float(cost)
            cost_source = "screenshot"
            total_cost += cost
        else:
            pnl = p.get("pnl_pct")
            if pnl is None:
                cost = None
                cost_source = "unknown"
            else:
                cost = mv / (1.0 + float(pnl))
                cost_source = "derived_pnl"
                total_cost += cost
        total_value += mv
        positions.append({
            "code": code,
            "market_value": mv,
            "nav": nav,
            "nav_day": nav_day,
            "shares": shares,
            "cost": cost,
            "cost_source": cost_source,
            "note": p.get("note"),
            "pnl_pct": p.get("pnl_pct"),
        })

    if total_value <= 0:
        raise SystemExit("没有可用持仓（净值缺失？）")

    # 按基金汇总 position（同一基金多笔买入合并）
    by_fund: dict[str, dict] = {}
    for pos in positions:
        agg = by_fund.setdefault(pos["code"], {
            "code": pos["code"], "market_value": 0.0, "shares": 0.0,
            "cost": 0.0, "cost_known": True, "sources": set(),
        })
        agg["market_value"] += pos["market_value"]
        agg["shares"] += pos["shares"]
        if pos["cost"] is None:
            agg["cost_known"] = False
        else:
            agg["cost"] += pos["cost"]
        agg["sources"].add(pos["cost_source"])

    now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    snapshot_ts = f"{as_of}T14:30:00+08:00"
    L: list[str] = []
    add = L.append
    add("-- =============================================================================")
    add("-- fin-lens 真实组合持仓种子（生成文件，不要手改）")
    add(f"-- 生成时间: {now}")
    add(f"-- 数据源:   用户 {as_of} 组合输入（市值 + 成本/收益率）+ Eastmoney 净值/持仓")
    add(f"-- 净值日:   {nav_date}；快照日: {as_of}")
    add("-- 口径:     份额 = 市值/净值；成本未知的基金 cost_basis=NULL")
    add(f"-- 用法:     fin-lens seed {out_path.name}")
    add("-- =============================================================================")
    add("")
    add("BEGIN;")
    add("")

    add("-- 1. 新增标的（ETF 基准 + 新持仓个股 + QDII 的美股/汇率）")
    spec_new_instruments = {
        c: m for c, m in spec.get("instruments", {}).items() if c not in base_instruments
    }
    for code, meta in sorted({**new_instruments, **spec_new_instruments}.items()):
        add(
            "INSERT INTO instrument (code, name, kind, exchange, currency, sector, source, source_symbol) "
            f"VALUES ({bs.q(code)}, {bs.q(meta['name'])}, {bs.q(meta['kind'])}, "
            f"{bs.q(bs.exchange_of(code))}, {bs.q(bs.currency_of(code))}, {bs.q(meta.get('sector'))}, "
            f"'eastmoney', {bs.q(meta.get('source_symbol'))}) "
            "ON CONFLICT(code) DO UPDATE SET name=excluded.name, sector=excluded.sector, "
            "source_symbol=excluded.source_symbol, is_active=1;"
        )
    add("")

    add("-- 2. 新增基金主档")
    for code, meta in sorted(spec["new_funds"].items()):
        bench = meta.get("benchmark")
        bench_name = instruments[bench]["name"] if bench else None
        add(
            "INSERT INTO fund (code, name, mode, category, benchmark_code, benchmark_name, etf_code, "
            "tracking_beta, notes) "
            f"VALUES ({bs.q(code)}, {bs.q(meta['name'])}, {bs.q(meta['mode'])}, {bs.q(meta.get('category'))}, "
            f"{bs.q(bench)}, {bs.q(bench_name)}, {bs.q(meta.get('etf_code'))}, "
            f"{bs.fnum(meta.get('tracking_beta'))}, 'handoff_{as_of}') "
            "ON CONFLICT(code) DO UPDATE SET name=excluded.name, mode=excluded.mode, "
            "category=excluded.category, benchmark_code=excluded.benchmark_code, "
            "benchmark_name=excluded.benchmark_name, etf_code=excluded.etf_code, "
            "tracking_beta=excluded.tracking_beta, is_active=1;"
        )
    add("")

    add(f"-- 3. 新增主动基季报持仓（{report_date}）")
    for code in spec.get("fetch_holdings", []):
        for rank, row in enumerate(holdings_by_fund.get(code, []), start=1):
            add(
                "INSERT INTO holding (fund_code, report_date, instrument_code, weight, rank, source) "
                f"VALUES ({bs.q(code)}, {bs.q(report_date)}, {bs.q(row['instrument'])}, "
                f"{bs.fnum(row['weight'])}, {rank}, 'eastmoney') "
                "ON CONFLICT(fund_code, report_date, instrument_code) DO UPDATE SET "
                "weight=excluded.weight, rank=excluded.rank, source=excluded.source;"
            )
    add("")

    add(f"-- 3b. 手工/季报外持仓（QDII 等，{report_date}）")
    for code, meta in sorted(spec["new_funds"].items()):
        for rank, row in enumerate(meta.get("holdings") or [], start=1):
            icode, name, weight = row[0], row[1], row[2]
            add(
                "INSERT INTO holding (fund_code, report_date, instrument_code, weight, rank, source) "
                f"VALUES ({bs.q(code)}, {bs.q(report_date)}, {bs.q(icode)}, "
                f"{bs.fnum(weight)}, {rank}, 'handoff') "
                "ON CONFLICT(fund_code, report_date, instrument_code) DO UPDATE SET "
                "weight=excluded.weight, rank=excluded.rank, source=excluded.source;"
            )
    add("")

    add("-- 4. 新增主动基资产配置")
    for code, alloc in sorted(allocs.items()):
        if not alloc:
            continue
        stock = alloc.get("stock") or 0.0
        bond = alloc.get("bond") or 0.0
        cash = alloc.get("cash") or 0.0
        other = max(0.0, 1.0 - stock - bond - cash)
        add(
            "INSERT INTO fund_allocation (fund_code, report_date, stock_weight, bond_weight, cash_weight, "
            "other_weight, source) "
            f"VALUES ({bs.q(code)}, {bs.q(alloc['report_date'])}, {bs.fnum(stock)}, {bs.fnum(bond)}, "
            f"{bs.fnum(cash)}, {bs.fnum(other)}, 'eastmoney') "
            "ON CONFLICT(fund_code, report_date) DO UPDATE SET stock_weight=excluded.stock_weight, "
            "bond_weight=excluded.bond_weight, cash_weight=excluded.cash_weight, "
            "other_weight=excluded.other_weight, source=excluded.source;"
        )
    add("")

    add(f"-- 5. 净值（{nav_date} 及前一交易日）")
    for code in codes:
        rows = navs.get(code) or {}
        days = sorted(rows)[-2:]
        for day in days:
            row = rows[day]
            add(
                "INSERT INTO nav (fund_code, trade_date, unit_nav, accum_nav, daily_pct, source) "
                f"VALUES ({bs.q(code)}, {bs.q(day)}, {bs.fnum(row['nav'])}, NULL, {bs.fnum(row['pct'])}, "
                "'eastmoney') ON CONFLICT(fund_code, trade_date) DO UPDATE SET "
                "unit_nav=excluded.unit_nav, daily_pct=excluded.daily_pct, source=excluded.source;"
            )
    add("")

    add("-- 6. 新增基准 ETF 的日线与盘中快照")
    for code in new_benchmarks:
        nav_rows = bench_nav.get(code) or {}
        prev_day = sorted([d for d in nav_rows if d < nav_date])[-1:] or []
        for day in ([prev_day[0]] if prev_day else []) + [nav_date]:
            row = nav_rows.get(day)
            if row is None or row["pct"] is None:
                continue
            add(
                "INSERT INTO quote_daily (instrument_code, trade_date, close, pct_chg, source) "
                f"VALUES ({bs.q(code)}, {bs.q(day)}, {bs.fnum(row['nav'])}, {bs.fnum(row['pct'])}, "
                "'eastmoney_fund_nav') ON CONFLICT(instrument_code, trade_date) DO UPDATE SET "
                "close=excluded.close, pct_chg=excluded.pct_chg, source=excluded.source;"
            )
        price = (bench_closes.get(code) or {}).get(as_of)
        prev = (bench_closes.get(code) or {}).get(nav_date)
        if price is None or prev is None:
            warnings.append(f"{code}: 缺 {as_of} 快照，跳过")
            continue
        add(
            "INSERT INTO quote_snapshot (instrument_code, price, prev_close, pct_chg, quote_ts, trade_date, source) "
            f"VALUES ({bs.q(code)}, {bs.fnum(price)}, {bs.fnum(prev)}, {bs.fnum(price / prev - 1.0)}, "
            f"{bs.q(snapshot_ts)}, {bs.q(as_of)}, 'eastmoney') "
            "ON CONFLICT(instrument_code) DO UPDATE SET price=excluded.price, "
            "prev_close=excluded.prev_close, pct_chg=excluded.pct_chg, quote_ts=excluded.quote_ts, "
            "trade_date=excluded.trade_date, source=excluded.source;"
        )
    add("")

    add("-- 6b. 新增个股的日线与盘中快照")
    for icode, closes in sorted(stock_closes.items()):
        days = sorted(d for d in closes if d < as_of)
        for day in days[-2:]:
            i = days.index(day)
            if i == 0:
                continue
            prev = closes[days[i - 1]]
            close = closes[day]
            if prev <= 0:
                continue
            add(
                "INSERT INTO quote_daily (instrument_code, trade_date, close, pct_chg, source) "
                f"VALUES ({bs.q(icode)}, {bs.q(day)}, {bs.fnum(close)}, {bs.fnum(close / prev - 1.0)}, "
                "'eastmoney') ON CONFLICT(instrument_code, trade_date) DO UPDATE SET "
                "close=excluded.close, pct_chg=excluded.pct_chg, source=excluded.source;"
            )
        price = closes.get(as_of)
        prev = closes.get(nav_date)
        if price is None or prev is None:
            warnings.append(f"{icode}: 缺 {as_of} 盘中价，跳过个股快照")
            continue
        add(
            "INSERT INTO quote_snapshot (instrument_code, price, prev_close, pct_chg, quote_ts, trade_date, source) "
            f"VALUES ({bs.q(icode)}, {bs.fnum(price)}, {bs.fnum(prev)}, {bs.fnum(price / prev - 1.0)}, "
            f"{bs.q(snapshot_ts)}, {bs.q(as_of)}, 'eastmoney') "
            "ON CONFLICT(instrument_code) DO UPDATE SET price=excluded.price, "
            "prev_close=excluded.prev_close, pct_chg=excluded.pct_chg, quote_ts=excluded.quote_ts, "
            "trade_date=excluded.trade_date, source=excluded.source;"
        )

    add("-- 6c. QDII 的 T-1 美股基准与汇率（快照 pct 即上一夜变动）")
    qdii_quotes: list[str] = sorted({
        m.get("benchmark")
        for m in spec["new_funds"].values()
        if m.get("category") == "qdii_index" and m.get("benchmark")
    } | {"FX.USDCNH"})
    for code in qdii_quotes:
        meta = instruments.get(code)
        if not meta:
            warnings.append(f"{code}: QDII 行情标的未定义，跳过")
            continue
        try:
            _, closes = bs.klines(meta["secid"], kline_beg, kline_end)
        except Exception as err:  # noqa: BLE001
            warnings.append(f"{code}: T-1 行情抓取失败 {err}")
            continue
        days = sorted(d for d in closes if d < as_of)
        if len(days) < 2:
            warnings.append(f"{code}: T-1 行情不足两日，跳过")
            continue
        prev_day, last_day = days[-2], days[-1]
        prev, last = closes[prev_day], closes[last_day]
        if prev <= 0:
            warnings.append(f"{code}: T-1 前收非法，跳过")
            continue
        pct = last / prev - 1.0
        source = "eastmoney_us_t1" if code.startswith("US.") else "eastmoney_fx"
        for day in days[-2:]:
            i = days.index(day)
            if i == 0:
                continue
            add(
                "INSERT INTO quote_daily (instrument_code, trade_date, close, pct_chg, source) "
                f"VALUES ({bs.q(code)}, {bs.q(day)}, {bs.fnum(closes[day])}, "
                f"{bs.fnum(closes[day] / closes[days[i - 1]] - 1.0)}, {bs.q(source)}) "
                "ON CONFLICT(instrument_code, trade_date) DO UPDATE SET close=excluded.close, "
                "pct_chg=excluded.pct_chg, source=excluded.source;"
            )
        add(
            "INSERT INTO quote_snapshot (instrument_code, price, prev_close, pct_chg, quote_ts, trade_date, source) "
            f"VALUES ({bs.q(code)}, {bs.fnum(last)}, {bs.fnum(prev)}, {bs.fnum(pct)}, "
            f"{bs.q(snapshot_ts)}, {bs.q(as_of)}, {bs.q(source)}) "
            "ON CONFLICT(instrument_code) DO UPDATE SET price=excluded.price, "
            "prev_close=excluded.prev_close, pct_chg=excluded.pct_chg, quote_ts=excluded.quote_ts, "
            "trade_date=excluded.trade_date, source=excluded.source;"
        )
    add("")

    add("-- 7. 当前持仓（按基金汇总；成本未知为 NULL）")
    for code, agg in sorted(by_fund.items()):
        weight = agg["market_value"] / total_value
        cost = bs.fnum(agg["cost"]) if agg["cost_known"] else "NULL"
        if not agg["cost_known"]:
            source = "unknown"
        elif agg["sources"] == {"screenshot"}:
            source = "screenshot"
        else:
            source = "derived_pnl"
        note = f"handoff {as_of}: 市值反推份额"
        add(
            "INSERT INTO position (fund_code, shares, cost_basis, market_value, weight, cost_source, note) "
            f"VALUES ({bs.q(code)}, {bs.fnum(agg['shares'])}, {cost}, {bs.fnum(agg['market_value'])}, "
            f"{bs.fnum(weight)}, {bs.q(source)}, {bs.q(note)}) "
            "ON CONFLICT(fund_code) DO UPDATE SET shares=excluded.shares, cost_basis=excluded.cost_basis, "
            "market_value=excluded.market_value, weight=excluded.weight, "
            "cost_source=excluded.cost_source, note=excluded.note, "
            "updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now');"
        )
    add("")

    add("-- 8. 期初交易流水（一行 = 文档一笔；后续可追加真实流水）")
    # 重新生成时清掉上一版 handoff 派生流水，避免旧仓位/旧平台拆分残留；
    # 真实流水（source != 'handoff'）不受影响。
    add(
        "DELETE FROM trade WHERE source='handoff' AND trade_date="
        f"{bs.q(nav_date)};"
    )
    for idx, pos in enumerate(positions, start=1):
        external = f"handoff-{as_of}-{pos['code']}-{idx}"
        amount = pos["cost"] if pos["cost"] is not None else pos["market_value"]
        note = pos["note"] or ""
        if pos["cost"] is None:
            note = (note + "；成本未知，amount 按市值记").strip("；")
        add(
            "INSERT INTO trade (external_id, fund_code, trade_date, kind, shares, nav, amount, fee, source, note) "
            f"VALUES ({bs.q(external)}, {bs.q(pos['code'])}, {bs.q(nav_date)}, 'buy', "
            f"{bs.fnum(pos['shares'])}, {bs.fnum(pos['nav'])}, {bs.fnum(amount)}, 0, 'handoff', {bs.q(note)}) "
            "ON CONFLICT(external_id) DO UPDATE SET shares=excluded.shares, nav=excluded.nav, "
            "amount=excluded.amount, note=excluded.note;"
        )
    add("")

    add("-- 9. A/C 类 family")
    for fam in spec.get("families", []):
        add(
            "INSERT INTO fund_family (code, name, note) "
            f"VALUES ({bs.q(fam['code'])}, {bs.q(fam['name'])}, {bs.q(fam.get('note'))}) "
            "ON CONFLICT(code) DO UPDATE SET name=excluded.name, note=excluded.note;"
        )
        for member, share_class in fam["members"].items():
            add(
                "UPDATE fund SET family_id=(SELECT id FROM fund_family WHERE code="
                f"{bs.q(fam['code'])}), share_class={bs.q(share_class)} WHERE code={bs.q(member)};"
            )
    add("")

    add("-- 10. 资源桶成员")
    # 重新生成时清掉上一版 handoff 派生成员，避免基金换桶后旧行残留。
    add("DELETE FROM bucket_member WHERE note LIKE 'handoff_%';")
    for bucket, members in spec.get("buckets", {}).items():
        for member in members:
            add(
                "INSERT INTO bucket_member (bucket_id, fund_code, role, note) "
                f"SELECT id, {bs.q(member)}, 'member', 'handoff_{as_of}' FROM bucket "
                f"WHERE code={bs.q(bucket)} ON CONFLICT(bucket_id, fund_code) DO NOTHING;"
            )
    add("")

    add("-- 11. 组合快照")
    add(
        "INSERT INTO portfolio_snapshot (as_of_date, total_value, cash, total_cost, source, note) "
        f"VALUES ({bs.q(as_of)}, {bs.fnum(total_value)}, {bs.fnum(pending_cash)}, {bs.fnum(total_cost)}, 'handoff', "
        f"'已列出的 {len(by_fund)} 只基金；在途申购 {bs.fnum(pending_cash)} 元；成本未知的未计入 total_cost') "
        "ON CONFLICT(as_of_date) DO UPDATE SET total_value=excluded.total_value, "
        "cash=excluded.cash, total_cost=excluded.total_cost, source=excluded.source, note=excluded.note;"
    )
    add("")
    add("COMMIT;")
    add("")

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text("\n".join(L), encoding="utf-8")

    print(f"\nwrote {out_path} ({len(L)} lines)")
    print(f"funds={len(by_fund)} positions={len(positions)} total_value={total_value:,.0f} "
          f"total_cost_known={total_cost:,.0f}")
    unknown = [c for c, a in by_fund.items() if not a["cost_known"]]
    print(f"cost unknown: {len(unknown)} funds -> {', '.join(sorted(unknown))}")
    if warnings:
        print("\n[WARN]")
        for warn in warnings:
            print("  -", warn)
    return 0


if __name__ == "__main__":
    sys.exit(main())
