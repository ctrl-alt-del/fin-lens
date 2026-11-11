#!/usr/bin/env python3
"""Validate local private inputs before running the seed generators.

No network, no database, no writes. Prints only structural errors/warnings and
counts; it never prints amounts, costs, or P&L.

Usage:
    python3 collector/validate_inputs.py
    python3 collector/validate_inputs.py --private-dir docs/examples/inputs --allow-demo
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from datetime import date
from pathlib import Path

import config

FUND_CODE_RE = re.compile(r"^\d{6}$")
DEMO_CODE_RE = re.compile(r"^DEMO[0-9A-Za-z_-]*$")
MODES = {"transparent", "semi_transparent", "opaque"}
BUCKETS = {
    "gold",
    "nonferrous",
    "broad_index",
    "oil",
    "semiconductor",
    "medical",
    "optical",
    "compute",
}


def is_date(value) -> bool:
    if not isinstance(value, str):
        return False
    try:
        date.fromisoformat(value)
        return True
    except ValueError:
        return False


def is_num(value) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="校验 FinLens 私有输入 JSON")
    parser.add_argument("--funds", help="funds_source.json 路径")
    parser.add_argument("--portfolio", help="portfolio_*.json 路径")
    parser.add_argument("--private-dir", help="私有输入目录（默认 data/private）")
    parser.add_argument("--allow-demo", action="store_true", help="允许 DEMO 前缀的示例基金代码")
    args = parser.parse_args(argv)

    private = config.private_dir(args.private_dir)
    funds_path = config.funds_source_path(private, args.funds)
    portfolio_path = config.portfolio_path(private, args.portfolio)

    errors: list[str] = []
    warnings: list[str] = []

    try:
        fs = json.loads(funds_path.read_text(encoding="utf-8"))
    except Exception as err:  # noqa: BLE001
        print(f"ERROR 读取 {funds_path}: {err}", file=sys.stderr)
        return 1
    try:
        pf = json.loads(portfolio_path.read_text(encoding="utf-8"))
    except Exception as err:  # noqa: BLE001
        print(f"ERROR 读取 {portfolio_path}: {err}", file=sys.stderr)
        return 1

    def code_ok(code: str) -> bool:
        return bool(FUND_CODE_RE.match(code) or (args.allow_demo and DEMO_CODE_RE.match(code)))

    for key in ("report_date", "validation_date", "snapshot_date"):
        if not is_date(fs.get(key)):
            errors.append(f"funds_source.{key}: 需要 YYYY-MM-DD")
    for key in ("sector_map", "instruments", "funds"):
        if key not in fs:
            errors.append(f"funds_source: 缺少 {key}")

    instruments = fs.get("instruments") or {}
    funds = fs.get("funds") or []
    if not isinstance(instruments, dict):
        errors.append("funds_source.instruments: 必须是对象")
        instruments = {}
    if not isinstance(funds, list) or not funds:
        errors.append("funds_source.funds: 必须是非空数组")
        funds = []

    fund_codes: set[str] = set()
    base_instruments = set(instruments)
    for i, fund in enumerate(funds):
        where = f"funds[{i}]"
        if not isinstance(fund, dict):
            errors.append(f"{where}: 必须是对象")
            continue
        code = str(fund.get("code") or "")
        if not code_ok(code):
            errors.append(f"{where}.code: 需要 6 位基金代码（示例可用 DEMO 前缀）")
        if code in fund_codes:
            errors.append(f"{where}.code: 重复 {code}")
        fund_codes.add(code)
        for key in ("name", "mode", "benchmark"):
            if not fund.get(key):
                errors.append(f"{where}.{key}: 必填")
        if fund.get("mode") not in MODES:
            errors.append(f"{where}.mode: 必须是 {sorted(MODES)}")
        bench = fund.get("benchmark")
        if bench and bench not in instruments and not str(bench).startswith(
            ("AU.", "US.", "FX.", "IDX.", "ETF.")
        ):
            errors.append(f"{where}.benchmark: instruments 中没有 {bench}")
        holdings = fund.get("holdings") or []
        if not isinstance(holdings, list):
            errors.append(f"{where}.holdings: 必须是数组")
            continue
        total = 0.0
        for j, row in enumerate(holdings):
            if not isinstance(row, list) or len(row) != 3:
                errors.append(f"{where}.holdings[{j}]: 需要 [原始代码, 名称, 权重]")
                continue
            raw, _name, weight = row
            if not str(raw).strip():
                errors.append(f"{where}.holdings[{j}]: 原始代码为空")
            if not is_num(weight) or not (0.0 <= float(weight) <= 1.0):
                errors.append(f"{where}.holdings[{j}].weight: 必须在 [0,1]")
            else:
                total += float(weight)
        if total > 1.0001:
            errors.append(f"{where}.holdings: 权重合计 {total:.4f} > 1")

    truth = fs.get("ground_truth_pct")
    if truth is not None:
        if not isinstance(truth, dict):
            errors.append("funds_source.ground_truth_pct: 必须是对象")
        else:
            for code, pct in truth.items():
                if code not in fund_codes:
                    warnings.append(f"ground_truth_pct: {code} 不在 funds 中")
                if not is_num(pct):
                    errors.append(f"ground_truth_pct.{code}: 必须是数字")

    for key in ("as_of", "nav_date"):
        if not is_date(pf.get(key)):
            errors.append(f"portfolio.{key}: 需要 YYYY-MM-DD")
    if "report_date" in pf and not is_date(pf.get("report_date")):
        errors.append("portfolio.report_date: 需要 YYYY-MM-DD")

    positions = pf.get("positions")
    if not isinstance(positions, list) or not positions:
        errors.append("portfolio.positions: 必须是非空数组")
        positions = []

    new_funds = pf.get("new_funds") or {}
    if not isinstance(new_funds, dict):
        errors.append("portfolio.new_funds: 必须是对象")
        new_funds = {}
    all_codes = set(fund_codes) | set(new_funds)

    for i, pos in enumerate(positions):
        where = f"positions[{i}]"
        if not isinstance(pos, dict):
            errors.append(f"{where}: 必须是对象")
            continue
        code = str(pos.get("code") or "")
        if not code_ok(code):
            errors.append(f"{where}.code: 需要 6 位基金代码（示例可用 DEMO 前缀）")
        if code and code not in all_codes:
            warnings.append(f"{where}.code: {code} 不在 funds/new_funds 中")
        mv = pos.get("market_value")
        if not is_num(mv) or float(mv) <= 0:
            errors.append(f"{where}.market_value: 必须 > 0")
        cost = pos.get("cost_basis")
        if cost is not None and (not is_num(cost) or float(cost) < 0):
            errors.append(f"{where}.cost_basis: 必须 >= 0 或 null")
        pnl = pos.get("pnl_pct")
        if pnl is not None and (not is_num(pnl) or float(pnl) <= -1.0):
            errors.append(f"{where}.pnl_pct: 必须 > -1 或 null")
        pending = pos.get("pending_amount")
        if pending is not None and (not is_num(pending) or float(pending) < 0):
            errors.append(f"{where}.pending_amount: 必须 >= 0")

    fetch = pf.get("fetch_holdings") or []
    if not isinstance(fetch, list):
        errors.append("portfolio.fetch_holdings: 必须是数组")
    else:
        for code in fetch:
            if code not in new_funds:
                warnings.append(f"fetch_holdings: {code} 不在 new_funds 中")

    buckets = pf.get("buckets") or {}
    if not isinstance(buckets, dict):
        errors.append("portfolio.buckets: 必须是对象")
    else:
        for bucket, members in buckets.items():
            if bucket not in BUCKETS:
                warnings.append(f"buckets: 未知桶 {bucket}")
            if not isinstance(members, list):
                errors.append(f"buckets.{bucket}: 必须是数组")
                continue
            for code in members:
                if code not in all_codes:
                    warnings.append(f"buckets.{bucket}: {code} 不在 funds/new_funds 中")

    families = pf.get("families") or []
    if not isinstance(families, list):
        errors.append("portfolio.families: 必须是数组")

    for code, meta in new_funds.items():
        if not isinstance(meta, dict):
            errors.append(f"new_funds.{code}: 必须是对象")
            continue
        if not meta.get("name"):
            errors.append(f"new_funds.{code}.name: 必填")
        if meta.get("mode") and meta["mode"] not in MODES:
            errors.append(f"new_funds.{code}.mode: 必须是 {sorted(MODES)}")

    print(f"funds_source: {funds_path}（funds={len(funds)} instruments={len(instruments)}）")
    print(f"portfolio:    {portfolio_path}（positions={len(positions)} new_funds={len(new_funds)}）")
    if warnings:
        print("\nWARN")
        for w in warnings:
            print("  -", w)
    if errors:
        print("\nERROR", file=sys.stderr)
        for e in errors:
            print("  -", e, file=sys.stderr)
        return 1
    print("\nOK：结构校验通过")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
