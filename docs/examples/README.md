# Private input examples

These files are **fictional templates**. They exist only to show the JSON shape
expected by `collector/*.py`.

- `inputs/funds_source.json` — fund pool, instruments, benchmarks, holdings.
- `inputs/portfolio_example.json` — positions, cost/return inputs, buckets.
- `AGENTS.local.example.md` — optional local agent status file.

They are not runnable against Eastmoney: `DEMO01`/`DEMO02` and `DEMO.*`
instruments are fake. To use the project with real data:

```bash
mkdir -p data/private/notes
chmod 700 data/private
cp docs/examples/inputs/funds_source.json data/private/funds_source.json
cp docs/examples/inputs/portfolio_example.json data/private/portfolio_<date>.json
# replace DEMO codes/names/benchmarks with real public fund and instrument codes
python3 collector/validate_inputs.py
```

Never commit `data/private/`, `seeds/`, databases, screenshots, or generated
reports. See `docs/onboarding.md` and `docs/private-data.md`.
