#!/usr/bin/env python3
"""回填 1 年历史：基金净值（nav）+ 个股/指数日线（quote_daily）。

只产出 SQL，不写数据库。数据来源：
- 基金净值：fund.eastmoney.com/pingzhongdata/<code>.js 的 Data_netWorthTrend（一次拿全量）
- 个股/指数：push2his.eastmoney.com kline（一年一次取完）

用法：
    python3 collector/build_history.py [--refresh]

产物：默认 `seeds/history_<as_of>.sql`，用 `fin-lens seed` 应用。
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta
from pathlib import Path

import config

CACHE_DIR = config.ROOT / ".cache" / "history"
REFRESH = "--refresh" in sys.argv
AS_OF = ""
START = ""

KLINE_URL = (
    "https://push2his.eastmoney.com/api/qt/stock/kline/get"
    "?secid={secid}&fields1=f1,f2,f3,f4,f5,f6"
    "&fields2=f51,f53&klt=101&fqt=1&beg={beg}&end={end}"
)
FUND_JS_URL = "https://fund.eastmoney.com/pingzhongdata/{code}.js"


def fetch(url: str, referer: str | None = None, retries: int = 3, timeout: int = 20) -> str:
    headers = {"User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)"}
    if referer:
        headers["Referer"] = referer
    last_err: Exception | None = None
    for attempt in range(retries):
        try:
            req = urllib.request.Request(url, headers=headers)
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                return resp.read().decode("utf-8", "replace")
        except Exception as err:  # noqa: BLE001
            last_err = err
            if attempt < retries - 1:
                import time
                time.sleep(1.2 * (attempt + 1))
    raise RuntimeError(f"fetch failed: {url}: {last_err}")


def cached(key: str, producer):
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    path = CACHE_DIR / (re.sub(r"[^0-9A-Za-z_.-]", "_", key) + ".txt")
    if path.exists() and not REFRESH:
        return path.read_text(encoding="utf-8")
    text = producer()
    path.write_text(text, encoding="utf-8")
    return text


def fund_nav(code: str) -> list[tuple[str, float, float | None]]:
    text = cached(f"nav_{code}", lambda: fetch(
        FUND_JS_URL.format(code=code), referer="https://fund.eastmoney.com/"))
    match = re.search(r"var Data_netWorthTrend\s*=\s*(\[.*?\]);", text, re.S)
    if not match:
        raise RuntimeError(f"{code}: 未找到 Data_netWorthTrend")
    rows = json.loads(match.group(1))
    out: list[tuple[str, float, float | None]] = []
    for row in rows:
        # x 是北京时间当天 00:00 的毫秒时间戳，+8h 后再取 UTC 日期
        day = datetime.utcfromtimestamp(row["x"] / 1000 + 8 * 3600).strftime("%Y-%m-%d")
        if not (START <= day <= AS_OF):
            continue
        pct = row.get("equityReturn")
        out.append((day, float(row["y"]), None if pct is None else float(pct) / 100.0))
    return out


def klines_1y(secid: str) -> list[tuple[str, float, float | None]]:
    end = (datetime.strptime(AS_OF, "%Y-%m-%d") + timedelta(days=2)).strftime("%Y%m%d")
    payload = json.loads(cached(f"kline_{secid}", lambda: fetch(
        KLINE_URL.format(secid=secid, beg=START.replace("-", ""), end=end),
        referer="https://quote.eastmoney.com/")))
    data = payload.get("data") or {}
    rows: list[tuple[str, float, float | None]] = []
    prev: float | None = None
    for line in data.get("klines") or []:
        day, close = line.split(",")
        close_f = float(close)
        if not (START <= day <= AS_OF):
            prev = close_f
            continue
        pct = None if prev is None else close_f / prev - 1.0
        rows.append((day, close_f, pct))
        prev = close_f
    return rows


def instrument_code(raw: str) -> str:
    raw = raw.strip()
    if "." in raw:
        return raw
    if len(raw) == 6:
        return ("SH." if raw[0] == "6" else "SZ.") + raw
    if len(raw) == 5:
        return "HK." + raw
    raise ValueError(raw)


def secid_for_stock(code: str) -> str:
    prefix, sym = code.split(".", 1)
    if prefix == "SH":
        return "1." + sym
    if prefix == "SZ":
        return "0." + sym
    if prefix == "HK":
        return "116." + sym
    if prefix == "IDX":
        return ("0." if sym.startswith("399") else "1.") + sym
    raise ValueError(code)


def q(value: float | None) -> str:
    if value is None:
        return "NULL"
    return f"{value:.10g}"


def batched(rows: list, size: int = 200):
    for i in range(0, len(rows), size):
        yield rows[i:i + size]


def main(argv: list[str] | None = None) -> int:
    global REFRESH, AS_OF, START

    parser = argparse.ArgumentParser(description="回填一年基金净值与个股/指数日线")
    parser.add_argument("--portfolio", help="portfolio_*.json 路径")
    parser.add_argument("--funds", help="funds_source.json 路径")
    parser.add_argument("--as-of", help="截止日期（默认取 funds_source.validation_date）")
    parser.add_argument("--start", help="起始日期（默认 as_of 前一年）")
    config.add_path_args(parser)
    args = parser.parse_args(argv)
    REFRESH = args.refresh

    private = config.private_dir(args.private_dir)
    funds_path = config.funds_source_path(private, args.funds)
    portfolio_path = config.portfolio_path(private, args.portfolio)

    fs = json.loads(funds_path.read_text(encoding="utf-8"))
    pf = json.loads(portfolio_path.read_text(encoding="utf-8"))

    AS_OF = args.as_of or fs["validation_date"]
    # 含首尾共 365 天：as_of - 364 .. as_of。
    START = args.start or (
        datetime.strptime(AS_OF, "%Y-%m-%d") - timedelta(days=364)
    ).strftime("%Y-%m-%d")
    file_as_of = args.as_of or pf.get("as_of") or AS_OF
    out_path = config.output_path(
        args.out, config.seed_dir() / f"history_{file_as_of}.sql"
    )

    funds = [f["code"] for f in fs["funds"]]
    funds += [code for code in pf["new_funds"].keys() if code not in funds]

    # 私有 instruments 里的 secid 是唯一来源，不在脚本里写死任何美股代码。
    secid_by_code: dict[str, str] = {}
    for source in (fs.get("instruments", {}), pf.get("instruments", {})):
        for code, meta in source.items():
            if meta.get("secid"):
                secid_by_code[code] = meta["secid"]

    # 需要日线的标的：持仓个股 + 3 个指数。ETF/黄金/汇率不参与「组合回溯」，
    # 因为组合本身用基金净值回测，基准用指数。
    stocks: dict[str, str] = {}  # code -> secid
    for fund in fs["funds"]:
        for holding in fund.get("holdings", []):
            item_code = instrument_code(str(holding[0]))
            secid = secid_by_code.get(item_code)
            if secid is None:
                if item_code.startswith(("SH.", "SZ.", "HK.", "IDX.")):
                    secid = secid_for_stock(item_code)
                else:
                    print(f"WARN {item_code}: 缺少 instruments.secid，跳过", file=sys.stderr)
                    continue
            stocks.setdefault(item_code, secid)
    for code, fund in pf["new_funds"].items():
        for holding in fund.get("holdings", []):
            item_code = instrument_code(str(holding[0]))
            secid = secid_by_code.get(item_code)
            if secid is None:
                print(f"WARN {item_code}: 缺少 instruments.secid，跳过", file=sys.stderr)
                continue
            stocks.setdefault(item_code, secid)
    for index in ("IDX.000300", "IDX.399006", "IDX.000688"):
        stocks.setdefault(index, secid_for_stock(index))

    navs: dict[str, list[tuple[str, float, float | None]]] = {}
    quotes: dict[str, list[tuple[str, float, float | None]]] = {}

    def load_nav(code: str):
        try:
            navs[code] = fund_nav(code)
        except Exception as err:  # noqa: BLE001
            print(f"WARN nav {code}: {err}", file=sys.stderr)

    def load_quote(code: str, secid: str):
        try:
            quotes[code] = klines_1y(secid)
        except Exception as err:  # noqa: BLE001
            print(f"WARN quote {code} ({secid}): {err}", file=sys.stderr)

    with ThreadPoolExecutor(max_workers=8) as pool:
        list(pool.map(load_nav, funds))
        list(pool.map(lambda item: load_quote(*item), stocks.items()))

    lines: list[str] = [
        "-- 自动生成：collector/build_history.py，回填 1 年净值/日线",
        f"-- 区间 {START} .. {AS_OF}，来源东方财富；全部 ON CONFLICT DO NOTHING（保留已有精确口径）",
        "",
    ]

    nav_rows = 0
    for code, rows in navs.items():
        for chunk in batched(rows):
            values = ",".join(
                f"('{code}','{day}',{q(nav)},{q(pct)})" for day, nav, pct in chunk)
            lines.append(
                "INSERT INTO nav (fund_code, trade_date, unit_nav, daily_pct) VALUES "
                + values
                + " ON CONFLICT(fund_code, trade_date) DO NOTHING;")
            nav_rows += len(chunk)
    lines.append("")

    quote_rows = 0
    for code, rows in quotes.items():
        for chunk in batched(rows):
            values = ",".join(
                f"('{code}','{day}',{q(close)},{q(pct)},'eastmoney_kline')"
                for day, close, pct in chunk)
            lines.append(
                "INSERT INTO quote_daily (instrument_code, trade_date, close, pct_chg, source) VALUES "
                + values
                + " ON CONFLICT(instrument_code, trade_date) DO NOTHING;")
            quote_rows += len(chunk)
    lines.append("")

    out_path.write_text("\n".join(lines), encoding="utf-8")
    print(f"wrote {out_path} ({out_path.stat().st_size} bytes)")
    print(f"funds={len(navs)} nav_rows={nav_rows} instruments={len(quotes)} quote_rows={quote_rows}")
    missing = [code for code in funds if code not in navs]
    if missing:
        print("missing nav:", ", ".join(missing), file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
