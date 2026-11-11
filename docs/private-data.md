# 私有数据边界（Personal data boundary）

FinLens 的代码、方法、阈值与演示数据可以进版本库；**你的持仓、金额、成本、盈亏、截图与日报不进。**

## 哪些是私有数据

| 路径 | 内容 | 为什么私有 |
|---|---|---|
| `data/private/funds_source.json` | 基金池 + 季报持仓 + 标的字典 | 持有名单本身就是信息 |
| `data/private/portfolio_*.json` | 市值、收益率、成本、平台拆分、备注 | 直接暴露资产规模与盈亏 |
| `data/private/dashboard-live.png` | 含真实金额的页面截图 | 同上 |
| `data/private/notes/` | 本机状态、Agent 基线、真实代码/权重的验证经验 | 含可识别的组合细节 |
| `data/private/backup/` | 数据库/日报/种子的本机备份 | 同上 |
| `seeds/*.sql` | 由上面 JSON 生成的 SQL，含 `position`/`trade`/`cost_basis` | 生成物包含个人数据 |
| `data/*.db` | SQLite 事实库 | 同上 |
| `data/daily/*.md` | 盘后对账日报（含估算与实际盈亏） | 同上 |
| `state/` | 预留的本地状态目录 | 可能含个人数据 |
| `AGENTS.local.md` | 本机基金数/持仓数/验证基线 | 同上 |

`.gitignore` 已覆盖以上路径，并额外忽略 `.env*`、密钥/证书、数据库、导出文件、根目录截图与常见 Agent 运行时目录。

## 目录约定

```text
data/private/                  # 私有，不入库（建议 0700）
├── funds_source.json          # 基金池 + 季报持仓（build_seed 的输入）
├── portfolio_<date>.json      # 市值/成本/平台（build_portfolio 的输入）
├── dashboard-live.png         # 真实数据截图（不放进 docs/）
├── notes/                     # 本机状态、基线、验证经验
│   ├── status.md
│   ├── agent-baselines.md
│   └── validation-lessons.md
└── backup/                    # 本机备份
    ├── finlens-before.db
    ├── seeds-before/
    └── daily-before/

seeds/                         # 生成的 SQL，含个人数据，整体忽略
data/finlens.db                # 运行时数据库，忽略
AGENTS.local.md                # 本机 Agent 状态，忽略
```

`collector/build_*.py` 的输入路径默认指向 `data/private/`，输出写 `seeds/`；
两者都在忽略范围内，所以**换机器/新克隆不能直接 seed，必须先用私有输入重新生成**，这是有意的。
接入方法见 [onboarding.md](onboarding.md)。

## 分离原则

1. **提交树只放引擎、方法、阈值、DEMO 数据与虚构样例。**
2. **任何能识别组合的事实都放本机**：金额、成本、盈亏、份额、当前权重、基金代码、
   真实持仓/行业权重、逐只验证误差、截图、数据库、日报。
3. **文档用占位符**：`<date>`、`<fund-code>`、`<total>`；不要用真实值举例。
4. **私有经验放 `data/private/notes/`**，不要写进 README/AGENTS/skill。
5. **不要 `git add -f`**；`.gitignore` 不是安全边界，索引才是。

## 本仓库的 Git 状态

仓库已初始化 Git，第一个提交只包含 `.gitignore`，用于先锁定私有数据边界。
首次提交前检查：

```bash
git check-ignore -v data/private/funds_source.json \
  data/private/notes/status.md \
  AGENTS.local.md seeds/funds.sql data/finlens.db

git ls-files -ci --exclude-standard
# 必须为空：没有“被忽略却仍被跟踪”的文件
```

`git status` 里出现任何 `data/private/`、`seeds/`、`*.db`、`AGENTS.local.md` 就要停下。

## 如果不小心提交了

1. 未推送：`git rm --cached -r data/private seeds data/finlens.db AGENTS.local.md`，提交，再确认 `.gitignore` 生效；
2. 已推送：仅 `rm --cached` 不够，历史里仍有。用 `git filter-repo --path data/private --invert-paths`
   （或 BFG）重写历史，强推后通知所有协作者重新克隆；
3. 财务数据没法“换密钥”，泄露即泄露——所以更稳妥的做法是**一开始就不 add**。

## 备份

只备份 `data/private/`（输入 + notes）与 `data/finlens.db`（或至少前者 + `seeds/`）。
数据库可以从 `data/private/*.json` + `collector/*.py` + `fin-lens seed` 完整重建；
重建后重跑 `validate` 恢复 `calibration` 与日报。

截图与东方财富缓存（`.cache/`）不必备份。

## 演示模式

对外展示用 DEMO 数据，不碰私有库：

```bash
FINLENS_SEED_DEMO=1 FINLENS_DB=/tmp/finlens-demo.db FINLENS_BIND=127.0.0.1:8788 cargo run
```

`docs/dashboard-demo.png` 由这个实例生成；README 只引用它。DEMO 只会在 `fund` 表为空时写入。

## 密钥类数据源（当前不需要）

现在所有数据源都是东方财富公开接口，只带 UA / Referer，**没有任何 API key**；
Rust 只读 `FINLENS_*` 配置（监听地址、阈值、数据库路径，均非密钥），也不加载 `.env`。
所以新克隆不需要任何密钥文件。

如果以后接需要 token 的数据源（Tushare、付费行情、券商接口）：

1. **只放环境变量**，不写进 JSON / SQL / SQLite / 日志。
   Rust 用 `std::env::var("FINLENS_XXX_TOKEN")`，缺失时启动即报错，不要静默降级；
   Python 用 `os.environ["FINLENS_XXX_TOKEN"]`，报错只报变量名，不回显值。
2. **本地存放**用 `.env` 或 shell `export`；仓库只提交 `.env.example`，值写 `your-token-here`
   这类占位符。`.env` 已在 `.gitignore` 里；也不要把真实值发给 Agent。
3. **轮换**：泄露或换机时先吊销旧 token，再把新 token 写回本机环境，重启服务；
   数据库不需要迁移（它不存密钥）。
4. **误提交**：先 `git rm --cached .env`，然后**立即吊销并更换 key**——历史里的 key 视为已泄露；
   是否 `git filter-repo` 只影响别人能否看到旧值，不影响密钥是否安全。
5. **日志**：`tracing` 不要打印请求头或带 token 的 URL；需要调试时只打印长度或前 4 位。
