#!/usr/bin/env python3
"""Build the fund/holding/nav/quote seed from public Eastmoney endpoints.

设计约束：
  * Python 只做「抓取 + 标准化 + 生成 SQL」，运行时不连接数据库；
  * Rust 侧用 `fin-lens seed <file.sql>` 把 SQL 灌进 SQLite；
  * 生成的 SQL 全部 `ON CONFLICT DO UPDATE`，可重复执行。

输入：`--private-dir` 下的 `funds_source.json`（私有，不入库）。
输出：默认 `seeds/funds.sql`，可用 `--out` 覆盖。

日期全部来自私有 JSON：
  * `validation_date` = 净值验证日；
  * `snapshot_date` = 盘中快照日。
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timedelta, timezone
from pathlib import Path

import config

ROOT = config.ROOT
CACHE_DIR = ROOT / ".cache" / "seed"
REFRESH = "--refresh" in sys.argv

UA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/120 Safari/537.36"
KLINE_URL = (
    "https://push2his.eastmoney.com/api/qt/stock/kline/get"
    "?secid={secid}&fields1=f1,f2,f3,f4,f5,f6&fields2=f51,f53"
    "&klt=101&fqt=1&beg={beg}&end={end}"
)
NAV_URL = "https://api.fund.eastmoney.com/f10/lsjz?fundCode={code}&pageIndex=1&pageSize=8"
ALLOC_URL = "https://fund.eastmoney.com/pingzhongdata/{code}.js"


def cached(key: str, producer):
    """把每次网络响应落盘，迭代脚本时不必反复打接口。"""
    path = CACHE_DIR / (re.sub(r"[^0-9A-Za-z_.-]", "_", key) + ".txt")
    if path.exists() and not REFRESH:
        return path.read_text(encoding="utf-8")
    value = producer()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(value, encoding="utf-8")
    return value


def fetch(url: str, referer: str | None = None, retries: int = 3, timeout: int = 15) -> str:
    headers = {"User-Agent": UA}
    if referer:
        headers["Referer"] = referer
    last_err: Exception | None = None
    for attempt in range(retries):
        try:
            req = urllib.request.Request(url, headers=headers)
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                return resp.read().decode("utf-8", "replace")
        except Exception as err:  # noqa: BLE001 - 网络错误统一重试
            last_err = err
            if attempt < retries - 1:
                time.sleep(1.5)
    raise RuntimeError(f"fetch failed: {url}: {last_err}")


def instrument_code(raw: str) -> str:
    """原始代码 → 内部统一代码。

    已带市场前缀的代码（`US.XOP` / `IDX.000300` / `ETF.512400`）原样返回；
    A 股 6 位 → `SH.`/`SZ.`；港股 5 位 → `HK.`。
    """
    raw = raw.strip()
    if "." in raw:
        return raw
    if len(raw) == 6:
        return ("SH." if raw[0] == "6" else "SZ.") + raw
    if len(raw) == 5:
        return "HK." + raw
    raise ValueError(f"无法识别的股票代码: {raw}")


def secid_for_stock(raw: str) -> str:
    if len(raw) == 6:
        return ("1." if raw[0] == "6" else "0.") + raw
    if len(raw) == 5:
        return "116." + raw
    raise ValueError(f"无法识别的股票代码: {raw}")


def exchange_of(code: str) -> str:
    if code.startswith("SH."):
        return "SSE"
    if code.startswith("SZ."):
        return "SZSE"
    if code.startswith("HK."):
        return "HKEX"
    if code.startswith("AU."):
        return "SGE"
    if code.startswith("US."):
        return "US"
    if code.startswith("FX."):
        return "FOREX"
    return ""


def currency_of(code: str) -> str:
    if code.startswith("HK."):
        return "HKD"
    if code.startswith("US."):
        return "USD"
    return "CNY"


# ETF 联接基金的日线基准必须用 **ETF 净值**（基金会计口径），而不是场内价格：
# 场内价格含折溢价，溢价波动日会让估值出现数百 bp 的假误差。
BENCHMARK_NAV_CODE = {"AU.GOLD": "518880"}


def benchmark_nav_code(code: str) -> str | None:
    """返回该标的应使用的 ETF 净值代码；非 ETF/金价代理返回 None。"""
    if code in BENCHMARK_NAV_CODE:
        return BENCHMARK_NAV_CODE[code]
    if code.startswith("ETF."):
        return code.split(".", 1)[1]
    return None


def klines(secid: str, beg: str, end: str) -> tuple[str, dict[str, float]]:
    key = f"kline_{secid}_{beg}_{end}"
    payload = json.loads(cached(key, lambda: fetch(
        KLINE_URL.format(secid=secid, beg=beg, end=end),
        referer="https://quote.eastmoney.com/")))
    data = payload.get("data") or {}
    closes: dict[str, float] = {}
    for line in data.get("klines") or []:
        day, close = line.split(",")
        closes[day] = float(close)
    return data.get("name") or "", closes


def kline_window(as_of: str, before: int = 8, after: int = 1) -> tuple[str, str]:
    """返回覆盖 as_of 前后若干天的 kline 起止日期（YYYYMMDD）。"""
    day = datetime.strptime(as_of, "%Y-%m-%d")
    return (
        (day - timedelta(days=before)).strftime("%Y%m%d"),
        (day + timedelta(days=after)).strftime("%Y%m%d"),
    )


def date_before(closes: dict[str, float], day: str) -> str | None:
    earlier = [d for d in closes if d < day]
    return max(earlier) if earlier else None


def close_before(closes: dict[str, float], day: str) -> float | None:
    earlier = [d for d in closes if d < day]
    return closes[max(earlier)] if earlier else None


def norm_nav(code: str) -> dict[str, dict]:
    payload = json.loads(cached(f"nav_{code}", lambda: fetch(
        NAV_URL.format(code=code), referer="https://fundf10.eastmoney.com/")))
    rows = ((payload.get("Data") or {}).get("LSJZList")) or []
    out: dict[str, dict] = {}
    for row in rows:
        day = row.get("FSRQ")
        if not day:
            continue
        pct = row.get("JZZZL")
        out[day] = {
            "nav": float(row["DWJZ"]),
            "pct": float(pct) / 100.0 if pct not in (None, "", "--") else None,
        }
    return out


def allocation(code: str) -> dict | None:
    js = cached(f"alloc_{code}", lambda: fetch(
        ALLOC_URL.format(code=code), referer="https://fund.eastmoney.com/"))
    match = re.search(r"var Data_assetAllocation\s*=\s*(\{.*?\});", js, re.S)
    if not match:
        return None
    obj = json.loads(match.group(1))
    cats = obj.get("categories") or []
    series = {s.get("name"): s.get("data") or [] for s in obj.get("series") or []}
    if not cats:
        return None
    idx = len(cats) - 1

    def pick(name: str) -> float | None:
        arr = series.get(name) or []
        if idx < len(arr) and arr[idx] is not None:
            return float(arr[idx]) / 100.0
        return None

    return {
        "report_date": cats[idx],
        "stock": pick("股票占净比"),
        "bond": pick("债券占净比"),
        "cash": pick("现金占净比"),
    }


def q(text: str | None) -> str:
    if text is None:
        return "NULL"
    return "'" + text.replace("'", "''") + "'"


def fnum(value: float | None) -> str:
    return "NULL" if value is None else repr(round(float(value), 10))


def main(argv: list[str] | None = None) -> int:
    global REFRESH

    parser = argparse.ArgumentParser(description="生成基金/持仓/净值/行情种子 SQL")
    parser.add_argument("--funds", help="funds_source.json 路径")
    config.add_path_args(parser)
    args = parser.parse_args(argv)
    REFRESH = args.refresh

    private = config.private_dir(args.private_dir)
    src_path = config.funds_source_path(private, args.funds)
    out_path = config.output_path(args.out, config.seed_dir() / "funds.sql")

    spec = json.loads(src_path.read_text(encoding="utf-8"))
    report_date = spec["report_date"]
    validation_date = spec["validation_date"]
    snapshot_date = spec["snapshot_date"]
    sector_map: dict[str, str] = spec["sector_map"]
    instruments: dict[str, dict] = dict(spec["instruments"])
    warnings: list[str] = []

    # 行情窗口由私有日期推导，避免把某一天写死在脚本里。
    kline_beg = (
        datetime.strptime(validation_date, "%Y-%m-%d") - timedelta(days=8)
    ).strftime("%Y%m%d")
    kline_end = (
        datetime.strptime(snapshot_date, "%Y-%m-%d") + timedelta(days=1)
    ).strftime("%Y%m%d")

    # 基金持仓里出现的新标的补进 instrument 字典
    for fund in spec["funds"]:
        for raw, name, _weight in fund.get("holdings", []):
            code = instrument_code(raw)
            if code not in instruments:
                if "." in raw:
                    warnings.append(f"{code}: 已带前缀但缺少 instruments 元数据，跳过")
                    continue
                instruments[code] = {
                    "name": name,
                    "kind": "stock",
                    "sector": sector_map.get(raw, "其他"),
                    "secid": secid_for_stock(raw),
                    "source_symbol": raw,
                }
            elif instruments[code].get("sector") is None:
                instruments[code]["sector"] = sector_map.get(raw, "其他")
            if code.startswith("HK."):
                instruments[code]["currency"] = "HKD"

    # ---- 抓取行情（并行；结果仍按 code 汇总，保证 SQL 可重复） ------------
    closes_by_code: dict[str, dict[str, float]] = {}
    bench_navs: dict[str, dict[str, dict]] = {}

    def load_instrument(code: str, meta: dict) -> tuple[str, dict[str, float], dict[str, dict], list[str]]:
        notes: list[str] = []
        secid = meta.get("secid")
        if not secid:
            return code, {}, {}, [f"{code}: 缺少 secid，跳过行情"]
        try:
            fetched_name, closes = klines(secid, kline_beg, kline_end)
        except Exception as err:  # noqa: BLE001
            return code, {}, {}, [f"{code}: 行情抓取失败 {err}"]
        if not closes:
            return code, {}, {}, [f"{code}: 无 K 线数据"]

        bench_nav: dict[str, dict] = {}
        nav_code = benchmark_nav_code(code)
        if nav_code:
            try:
                bench_nav = norm_nav(nav_code)
            except Exception as err:  # noqa: BLE001
                notes.append(f"{code}: ETF 净值抓取失败 {err}，回退场内价格")
            else:
                if validation_date not in bench_nav:
                    notes.append(f"{code}: ETF {nav_code} 缺少 {validation_date} 净值，回退场内价格")
                    bench_nav = {}

        if validation_date not in closes:
            notes.append(f"{code}: 缺少 {validation_date} 收盘价（停牌？）")
        if fetched_name and fetched_name != meta["name"]:
            notes.append(f"{code}: 名称不一致 seed={meta['name']!r} api={fetched_name!r}")
        return code, closes, bench_nav, notes

    total = len(instruments)
    done = 0
    with ThreadPoolExecutor(max_workers=6) as executor:
        futures = [executor.submit(load_instrument, code, meta)
                   for code, meta in instruments.items()]
        for future in as_completed(futures):
            code, closes, bench_nav, notes = future.result()
            done += 1
            if closes:
                closes_by_code[code] = closes
            if bench_nav:
                bench_navs[code] = bench_nav
            warnings.extend(notes)
            last = closes.get(validation_date)
            print(f"  [{done:>2}/{total}] {code:<13} {validation_date} close="
                  f"{'n/a' if last is None else f'{last:.3f}'}", flush=True)

    # ---- 抓取净值 / 资产配置（并行） ---------------------------------------
    navs: dict[str, dict[str, dict]] = {}
    allocs: dict[str, dict | None] = {}

    def load_fund_extras(fund: dict) -> tuple[str, dict, dict | None, list[str]]:
        code = fund["code"]
        notes: list[str] = []
        try:
            nav_rows = norm_nav(code)
        except Exception as err:  # noqa: BLE001
            nav_rows = {}
            notes.append(f"{code}: 净值抓取失败 {err}")
        alloc: dict | None = None
        if fund["mode"] == "semi_transparent":
            try:
                alloc = allocation(code)
            except Exception as err:  # noqa: BLE001
                notes.append(f"{code}: 资产配置抓取失败 {err}")
        return code, nav_rows, alloc, notes

    done = 0
    with ThreadPoolExecutor(max_workers=5) as executor:
        futures = [executor.submit(load_fund_extras, fund) for fund in spec["funds"]]
        for future in as_completed(futures):
            code, nav_rows, alloc, notes = future.result()
            navs[code] = nav_rows
            allocs[code] = alloc
            warnings.extend(notes)
            done += 1
            day = nav_rows.get(validation_date)
            print(f"  [{done:>2}/{len(spec['funds'])}] fund {code} nav="
                  f"{'n/a' if day is None else f'{day[chr(110)+chr(97)+chr(118)]:.4f}'} "
                  f"alloc={'yes' if alloc else 'no'}", flush=True)
    warnings.sort()

    # ---- 生成 SQL -----------------------------------------------------------
    now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    snapshot_ts = f"{snapshot_date}T14:30:00+08:00"
    lines: list[str] = []
    add = lines.append

    add("-- =============================================================================")
    add("-- fin-lens 基金/持仓/净值/行情种子（生成文件，不要手改）")
    add(f"-- 生成时间: {now}")
    add(f"-- 数据源:   Eastmoney 公开接口（kline / lsjz / pingzhongdata）；ETF 基准用 ETF 净值")
    add(f"-- 持仓期:   {report_date}")
    add(f"-- 验证日:   {validation_date}（净值 + 日线行情）")
    add(f"-- 快照日:   {snapshot_date}（盘中 quote_snapshot）")
    add(f"-- 用法:     fin-lens seed {out_path.name}")
    add("-- =============================================================================")
    add("")
    add("BEGIN;")
    add("")

    # 1. 标的字典
    add("-- 1. 标的字典")
    for code, meta in sorted(instruments.items()):
        source_symbol = meta.get("source_symbol") or meta.get("secid", "").split(".", 1)[-1]
        add(
            "INSERT INTO instrument (code, name, kind, exchange, currency, sector, source, source_symbol) "
            f"VALUES ({q(code)}, {q(meta['name'])}, {q(meta['kind'])}, {q(exchange_of(code))}, "
            f"{q(currency_of(code))}, {q(meta.get('sector'))}, 'eastmoney', {q(source_symbol)}) "
            "ON CONFLICT(code) DO UPDATE SET name=excluded.name, kind=excluded.kind, "
            "sector=excluded.sector, source=excluded.source, source_symbol=excluded.source_symbol, "
            "is_active=1;"
        )
    add("")

    # 2. 基金主档
    add("-- 2. 基金主档")
    for fund in spec["funds"]:
        bench = fund.get("benchmark")
        bench_name = instruments[bench]["name"] if bench else None
        etf = fund.get("etf_code")
        add(
            "INSERT INTO fund (code, name, mode, category, benchmark_code, benchmark_name, etf_code, "
            "tracking_beta, notes) "
            f"VALUES ({q(fund['code'])}, {q(fund['name'])}, {q(fund['mode'])}, {q(fund.get('category'))}, "
            f"{q(bench)}, {q(bench_name)}, {q(etf)}, {fnum(fund.get('tracking_beta'))}, {q(fund.get('notes'))}) "
            "ON CONFLICT(code) DO UPDATE SET name=excluded.name, mode=excluded.mode, "
            "category=excluded.category, benchmark_code=excluded.benchmark_code, "
            "benchmark_name=excluded.benchmark_name, etf_code=excluded.etf_code, "
            "tracking_beta=excluded.tracking_beta, notes=excluded.notes, is_active=1;"
        )
    add("")

    # 3. 季报持仓
    add(f"-- 3. 季报持仓（{report_date}）")
    for fund in spec["funds"]:
        holdings = fund.get("holdings") or []
        for rank, (raw, name, weight) in enumerate(holdings, start=1):
            add(
                "INSERT INTO holding (fund_code, report_date, instrument_code, weight, rank, source) "
                f"VALUES ({q(fund['code'])}, {q(report_date)}, {q(instrument_code(raw))}, "
                f"{fnum(weight)}, {rank}, 'eastmoney') "
                "ON CONFLICT(fund_code, report_date, instrument_code) DO UPDATE SET "
                "weight=excluded.weight, rank=excluded.rank, source=excluded.source;"
            )
    add("")

    # 4. 资产配置（权益占比决定半透明基金的残余仓位）
    add("-- 4. 资产配置（东财 pingzhongdata 最新报告期）")
    for fund in spec["funds"]:
        alloc = allocs.get(fund["code"])
        if not alloc:
            continue
        stock = alloc.get("stock") or 0.0
        bond = alloc.get("bond") or 0.0
        cash = alloc.get("cash") or 0.0
        other = max(0.0, 1.0 - stock - bond - cash)
        add(
            "INSERT INTO fund_allocation (fund_code, report_date, stock_weight, bond_weight, cash_weight, "
            "other_weight, source) "
            f"VALUES ({q(fund['code'])}, {q(alloc['report_date'])}, {fnum(stock)}, {fnum(bond)}, "
            f"{fnum(cash)}, {fnum(other)}, 'eastmoney') "
            "ON CONFLICT(fund_code, report_date) DO UPDATE SET stock_weight=excluded.stock_weight, "
            "bond_weight=excluded.bond_weight, cash_weight=excluded.cash_weight, "
            "other_weight=excluded.other_weight, source=excluded.source;"
        )
    add("")

    # 5. 净值（验证日 + 前一日）
    add(f"-- 5. 净值（{validation_date} 及前一交易日）")
    for fund in spec["funds"]:
        rows = navs.get(fund["code"]) or {}
        days = sorted(rows)[-2:]
        if validation_date not in rows:
            warnings.append(f"{fund['code']}: 无 {validation_date} 净值")
        for day in days:
            row = rows[day]
            add(
                "INSERT INTO nav (fund_code, trade_date, unit_nav, accum_nav, daily_pct, source) "
                f"VALUES ({q(fund['code'])}, {q(day)}, {fnum(row['nav'])}, NULL, {fnum(row['pct'])}, 'eastmoney') "
                "ON CONFLICT(fund_code, trade_date) DO UPDATE SET unit_nav=excluded.unit_nav, "
                "daily_pct=excluded.daily_pct, source=excluded.source;"
            )
    add("")

    # 6. 日线行情（验证日 + 前一日，pct_chg 为小数）
    # ETF / 金价代理用 ETF 净值口径；其余标的用收盘价。
    add(f"-- 6. 日线行情（{validation_date} 及前一日；ETF 基准取 ETF 净值而非场内价格）")
    for code, closes in sorted(closes_by_code.items()):
        if code in bench_navs:
            rows = bench_navs[code]
            prev_day = date_before(rows, validation_date)
            for day in [d for d in (prev_day, validation_date) if d]:
                row = rows.get(day)
                if row is None or row["pct"] is None:
                    continue
                add(
                    "INSERT INTO quote_daily (instrument_code, trade_date, close, pct_chg, source) "
                    f"VALUES ({q(code)}, {q(day)}, {fnum(row['nav'])}, {fnum(row['pct'])}, "
                    "'eastmoney_fund_nav') "
                    "ON CONFLICT(instrument_code, trade_date) DO UPDATE SET close=excluded.close, "
                    "pct_chg=excluded.pct_chg, source=excluded.source;"
                )
            continue
        prev_day = date_before(closes, validation_date)
        for day in [d for d in (prev_day, validation_date) if d]:
            close = closes.get(day)
            if close is None:
                continue
            prev = close_before(closes, day)
            pct = (close / prev - 1.0) if prev else None
            add(
                "INSERT INTO quote_daily (instrument_code, trade_date, close, pct_chg, source) "
                f"VALUES ({q(code)}, {q(day)}, {fnum(close)}, {fnum(pct)}, 'eastmoney') "
                "ON CONFLICT(instrument_code, trade_date) DO UPDATE SET close=excluded.close, "
                "pct_chg=excluded.pct_chg, source=excluded.source;"
            )
    add("")

    # 7. 盘中快照（供实时估值）
    add(f"-- 7. 盘中快照（{snapshot_date}）")
    add(f"-- quote_ts = 生成脚本运行时刻 {now}")
    for code, closes in sorted(closes_by_code.items()):
        price = closes.get(snapshot_date)
        prev = closes.get(validation_date)
        if price is None:
            # 停牌 / 港股休市：快照缺失，不制造 0% 假行情（估值内核会打 missing 标记）
            warnings.append(f"{code}: 缺少 {snapshot_date} 价格，快照跳过")
            continue
        if prev is None:
            warnings.append(f"{code}: 缺少 {validation_date} 价格，快照跳过")
            continue
        pct = price / prev - 1.0
        add(
            "INSERT INTO quote_snapshot (instrument_code, price, prev_close, pct_chg, quote_ts, trade_date, source) "
            f"VALUES ({q(code)}, {fnum(price)}, {fnum(prev)}, {fnum(pct)}, {q(snapshot_ts)}, {q(snapshot_date)}, 'eastmoney') "
            "ON CONFLICT(instrument_code) DO UPDATE SET price=excluded.price, "
            "prev_close=excluded.prev_close, pct_chg=excluded.pct_chg, quote_ts=excluded.quote_ts, "
            "trade_date=excluded.trade_date, source=excluded.source;"
        )
    add("")
    add("COMMIT;")
    add("")

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text("\n".join(lines), encoding="utf-8")

    # ---- 自检输出 -----------------------------------------------------------
    print(f"wrote {out_path} ({len(lines)} lines)")
    print(f"instruments={len(instruments)} funds={len(spec['funds'])} navs={sum(len(v) for v in navs.values())}")
    if warnings:
        print("\n[WARN]")
        for warn in warnings:
            print("  -", warn)
    else:
        print("no warnings")

    # 验证日实际涨跌 vs 私有 ground truth 交叉核对（可选）
    truth = spec.get("ground_truth_pct") or {}
    if not truth:
        print("\n[ground truth] 未配置 ground_truth_pct，跳过交叉核对")
        return 0
    print(f"\n[{validation_date} 净值 vs 私有 ground truth]")
    mismatches = 0
    for fund in spec["funds"]:
        row = (navs.get(fund["code"]) or {}).get(validation_date)
        if not row or row["pct"] is None:
            print(f"  {fund['code']} {fund['name']}: 无净值")
            mismatches += 1
            continue
        expected = truth.get(fund["code"])
        if expected is None:
            continue
        actual_bp = row["pct"] * 100
        diff = actual_bp - float(expected)
        flag = "" if abs(diff) < 0.005 else "  <-- MISMATCH"
        if flag:
            mismatches += 1
        print(f"  {fund['code']} {fund['name']:<24} api={actual_bp:+.2f}% truth={float(expected):+.2f}%{flag}")
    print(f"mismatches={mismatches}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
