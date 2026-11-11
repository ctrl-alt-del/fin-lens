"""Shared paths and CLI helpers for the collector scripts.

Only paths and argument parsing live here. No network, no database, no writes.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_PRIVATE_DIR = ROOT / "data" / "private"
DEFAULT_SEED_DIR = ROOT / "seeds"


def private_dir(value: str | None = None) -> Path:
    raw = value or os.environ.get("FINLENS_PRIVATE_DIR")
    return Path(raw).expanduser().resolve() if raw else DEFAULT_PRIVATE_DIR


def seed_dir(value: str | None = None) -> Path:
    raw = value or os.environ.get("FINLENS_SEED_DIR")
    return Path(raw).expanduser().resolve() if raw else DEFAULT_SEED_DIR


def funds_source_path(private: Path, explicit: str | None = None) -> Path:
    if explicit:
        return Path(explicit).expanduser().resolve()
    return private / "funds_source.json"


def portfolio_path(private: Path, explicit: str | None = None) -> Path:
    if explicit:
        return Path(explicit).expanduser().resolve()
    env = os.environ.get("FINLENS_PORTFOLIO_PATH")
    if env:
        return Path(env).expanduser().resolve()
    candidates = sorted(private.glob("portfolio_*.json"))
    if len(candidates) == 1:
        return candidates[0]
    if not candidates:
        raise SystemExit(
            f"未找到 {private}/portfolio_*.json；请用 --portfolio 指定，"
            "或设置 FINLENS_PORTFOLIO_PATH"
        )
    names = ", ".join(p.name for p in candidates)
    raise SystemExit(
        f"发现多个 portfolio_*.json（{names}）；请用 --portfolio 指定，"
        "或设置 FINLENS_PORTFOLIO_PATH"
    )


def output_path(explicit: str | None, default: Path) -> Path:
    return Path(explicit).expanduser().resolve() if explicit else default


def add_path_args(parser: argparse.ArgumentParser, *, refresh: bool = True) -> None:
    parser.add_argument("--private-dir", help="私有输入目录（默认 data/private）")
    parser.add_argument("--out", help="输出 SQL 路径")
    if refresh:
        parser.add_argument("--refresh", action="store_true", help="绕过 .cache 重新抓取")
