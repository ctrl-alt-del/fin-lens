//! HTMX 服务端渲染的 Dashboard（One-pager + Reference）。
//!
//! 首屏只回答四个问题：今天亏多少、过去一年走得怎样、风险集中在哪里、
//! 现在最该做什么。基金逐只明细折叠进 `<details>`，不再抢首屏。
//!
//! 页面不做任何计算：所有数字来自 `service::history_view` / `overview_all`，
//! 与 API、`validate` 走同一条估值路径。

use std::collections::HashMap;
use std::f64::consts::PI;
use std::fmt::Write as _;

use axum::{
    extract::State,
    http::header,
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use chrono::Local;

use crate::{
    api::AppState,
    db::repo,
    domain::{risk::SeriesPoint, types::ValuationMode, valuation::Flag},
    error::AppError,
    service::{self, FundOverview, HealthStatus, LookthroughView},
};

const HTMX_JS: &str = include_str!("../static/htmx.min.js");

/// 由 `api::router()` 合并进主 Router。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/partials/overview", get(overview_partial))
        .route("/static/htmx.min.js", get(htmx_js))
        .route("/favicon.ico", get(favicon))
}

// ---------------------------------------------------------------------------
// 数据装载
// ---------------------------------------------------------------------------

struct PageData {
    funds: Vec<FundOverview>,
    summary: service::PortfolioSummary,
    skipped: Vec<(String, String)>,
    lookthrough: LookthroughView,
    /// 最近一次盘中快照 `(quote_ts, trade_date)`。
    snapshot: Option<(String, String)>,
    history: service::HistoryView,
}

async fn load(state: &AppState) -> Result<PageData, AppError> {
    let quotes: HashMap<String, f64> = repo::all_quotes(&state.db)
        .await?
        .into_iter()
        .map(|q| (q.instrument_code, q.pct_chg))
        .collect();
    let today = Local::now().date_naive();

    let (mut funds, skipped) = service::overview_all(&state.db, &state.cfg, today, &quotes).await?;
    // 跌得最多的排前面：Reference 里也先看最疼的。
    funds.sort_by(|a, b| a.estimated_pct.total_cmp(&b.estimated_pct));

    let positions = repo::positions(&state.db).await?;
    let mut summary = service::PortfolioSummary::build(&funds, &positions);
    if let Some(snap) = repo::latest_portfolio_snapshot(&state.db).await? {
        summary.pending_cash = snap.cash;
    }
    let lookthrough = service::lookthrough_view(&state.db, &state.cfg).await?;
    let history = service::history_view(&state.db, &state.cfg, today).await?;
    let snapshot = repo::latest_snapshot(&state.db).await?;

    Ok(PageData {
        funds,
        summary,
        skipped,
        lookthrough,
        snapshot,
        history,
    })
}

// ---------------------------------------------------------------------------
// 路由 handler
// ---------------------------------------------------------------------------

async fn dashboard(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let data = load(&state).await?;
    Ok(Html(render_page(&data)))
}

async fn overview_partial(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let data = load(&state).await?;
    Ok(Html(render_live(&data)))
}

async fn htmx_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        HTMX_JS,
    )
}

async fn favicon() -> impl IntoResponse {
    axum::http::StatusCode::NO_CONTENT
}

// ---------------------------------------------------------------------------
// 页面骨架
// ---------------------------------------------------------------------------

fn render_page(data: &PageData) -> String {
    let mut h = String::with_capacity(64 * 1024);
    let _ = write!(
        h,
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>FinLens · 基金穿透监控</title>
<link rel="icon" href="data:,">
<script src="/static/htmx.min.js" defer></script>
<style>{STYLE}</style>
</head>
<body>
<header>
  <h1>FIN<span class="accent">·</span>LENS</h1>
  <div class="sub">{n_funds} 只基金 · {n_pos} 只持仓 · 本地 SQLite · 东方财富数据</div>
  <div class="spacer"></div>
  <span class="clock">{now}</span>
  <a class="json" href="/api/lookthrough">JSON</a>
</header>
<main>
<div id="live" hx-get="/partials/overview" hx-trigger="every 30s" hx-swap="innerHTML">
{live}
</div>
{reference}
</main>
<footer>估值 ≠ 净值，仅供监控参考 · 数据源：东方财富公开接口 · 本机真实数据，非投资建议</footer>
</body>
</html>"#,
        STYLE = STYLE,
        n_funds = data.funds.len() + data.skipped.len(),
        n_pos = data.summary.positions,
        now = Local::now().format("%Y-%m-%d %H:%M:%S"),
        live = render_live(data),
        reference = render_reference(data),
    );
    h
}

fn render_live(data: &PageData) -> String {
    let mut h = String::with_capacity(48 * 1024);
    h.push_str(&render_hero(data));
    h.push_str(&render_headline(data));
    h.push_str(&flow_header("1", "今天", "跌了多少，谁拖的？"));
    h.push_str(&render_today(data));
    h.push_str(&flow_header("2", "风险", "钱压在哪，哪些数字不可信？"));
    h.push_str(&render_risk(data));
    h.push_str(&flow_header(
        "3",
        "是不是真的跑赢",
        "组合和穿透篮子，各跟自己的基准比",
    ));
    h.push_str(&render_compare(data));
    h.push_str(&flow_header(
        "4",
        "要盯的基金",
        "数据缺口、低置信度、今日大跌",
    ));
    h.push_str(&render_warnings(data));
    h
}

/// 章节标题：编号 + 标题 + 一句话说明，用来固定阅读顺序。
fn flow_header(num: &str, title: &str, question: &str) -> String {
    format!(
        r#"<div class="flow-h"><span class="num">{num}</span><h2>{title}</h2><span class="q">{question}</span></div>"#
    )
}

fn render_headline(data: &PageData) -> String {
    let hv = &data.history;
    if hv.headline.is_empty() {
        return String::new();
    }
    format!(
        r#"<div class="headline-bar"><span class="lead">一句话</span><span>{}</span></div>
<div class="read-hint">按 ①→④ 读：今天 → 风险 → 是不是真的跑赢 → 要盯的基金。细节都在底部 Reference。</div>"#,
        esc(&hv.headline)
    )
}

fn render_today(data: &PageData) -> String {
    let hv = &data.history;
    let mut chips = String::new();
    for h in &hv.horizons {
        let _ = write!(
            chips,
            r#"<div class="hchip"><div class="k">{label}</div><div class="v {cls}">{ret}</div><div class="s">{start} 起 · {n} 点</div></div>"#,
            label = horizon_label(h.days),
            cls = tone_class(h.ret),
            ret = pct(h.ret),
            start = h.start,
            n = h.observations,
        );
    }
    let mut losers = String::new();
    for f in data.funds.iter().take(3) {
        let _ = write!(
            losers,
            r#"<div class="loser"><div class="lname">{name}<span class="dim">{code}</span></div><div class="lval {cls}">{pct}</div></div>"#,
            name = esc(&f.name),
            code = f.code,
            cls = tone_class(f.estimated_pct),
            pct = pct(f.estimated_pct),
        );
    }
    format!(
        r#"<div class="two-col">
  <div class="panel"><div class="panel-h">区间表现 <span class="panel-sub">当前份额回溯 · 起点=100</span></div><div class="horizons">{chips}</div><div class="footline">短区间是波动，长区间才是配置结果；1 年不代表账户真实收益。</div></div>
  <div class="panel"><div class="panel-h">今日拖累 Top 3 <span class="panel-sub">按估算涨跌</span></div><div class="losers">{losers}</div><div class="footline">估算涨跌 ≠ 当日净值；偏差项启用后会逐步靠近。</div></div>
</div>"#
    )
}

fn render_risk(data: &PageData) -> String {
    format!(
        r#"<div class="two-col">
  <div class="stack">{health}{actions}</div>
  <div class="stack">{donut}{top}</div>
</div>"#,
        health = render_health(data),
        actions = render_actions(data),
        donut = render_donut(data),
        top = render_top_exposures(data),
    )
}

fn render_compare(data: &PageData) -> String {
    format!(
        r#"<div class="two-col">{portfolio}{basket}</div>{attr}"#,
        portfolio = render_trend(data),
        basket = render_basket_trend(data),
        attr = render_attribution(data),
    )
}

fn attribution_note(attr: &[service::BucketAttribution]) -> String {
    match (attr.first(), attr.last()) {
        (Some(top), Some(worst)) if top.label != worst.label => format!(
            "最大贡献 {} {:+.1}pp，最大拖累 {} {:+.1}pp。",
            top.label,
            top.contribution * 100.0,
            worst.label,
            worst.contribution * 100.0
        ),
        _ => String::new(),
    }
}

fn render_attribution(data: &PageData) -> String {
    let hv = &data.history;
    if hv.attribution.is_empty() {
        return r#"<div class="panel"><div class="panel-h">1 年收益归因</div><div class="empty">历史净值不足，无法拆归因。</div></div>"#.to_string();
    }
    let mut rows = String::new();
    for b in &hv.attribution {
        let _ = write!(
            rows,
            r#"<tr><td><span class="swatch" style="background:{color}"></span>{label}</td><td>{w}</td><td class="{rc}">{ret}</td><td class="{cc}">{contrib} pp</td></tr>"#,
            color = b.color,
            label = b.label,
            w = weight_pct(b.weight),
            rc = tone_class(b.ret),
            ret = pct(b.ret),
            cc = tone_class(b.contribution),
            contrib = format!("{:+.1}", b.contribution * 100.0),
        );
    }
    let total: f64 = hv.attribution.iter().map(|b| b.contribution).sum();
    let one_year = hv
        .horizons
        .iter()
        .find(|h| h.days == 365)
        .map(|h| pct(h.ret))
        .unwrap_or_else(|| "—".to_string());
    format!(
        r#"<div class="panel" style="margin-top:12px">
  <div class="panel-h">1 年收益归因 <span class="panel-sub">当前份额近似：桶盈亏 ÷ 组合期初市值</span></div>
  <table class="attr"><thead><tr><th>资源桶</th><th>当前权重</th><th>1 年收益</th><th>贡献</th></tr></thead><tbody>{rows}</tbody></table>
  <div class="footline">合计 {total} pp，组合 1 年 {one_year}（差异来自取整与各基金取最近可用净值）。{note}沪深300 不含黄金/有色，跨资产比较只作参照。</div>
</div>"#,
        total = format!("{:+.1}", total * 100.0),
        note = attribution_note(&hv.attribution),
    )
}

fn render_warnings(data: &PageData) -> String {
    struct Row<'a> {
        fund: &'a service::FundOverview,
        reasons: Vec<(String, &'static str)>,
    }
    let mut rows: Vec<Row<'_>> = Vec::new();
    for f in &data.funds {
        let mut reasons: Vec<(String, &'static str)> = Vec::new();
        for flag in &f.flags {
            if matches!(flag, Flag::BiasInactive | Flag::QdiiLag) {
                continue;
            }
            let (label, tone) = flag_label(*flag);
            reasons.push((label.to_string(), tone));
        }
        if f.coverage < 0.5 && !f.flags.contains(&Flag::LowCoverage) {
            reasons.push((format!("覆盖 {:.0}%", f.coverage * 100.0), "warn"));
        }
        if f.estimated_pct <= -0.05 {
            reasons.push(("今日跌幅超 5%".to_string(), "warn"));
        }
        if !reasons.is_empty() {
            rows.push(Row { fund: f, reasons });
        }
    }
    rows.sort_by(|a, b| {
        b.reasons
            .len()
            .cmp(&a.reasons.len())
            .then_with(|| a.fund.estimated_pct.total_cmp(&b.fund.estimated_pct))
    });
    rows.truncate(8);
    if rows.is_empty() {
        return r#"<div class="panel"><div class="panel-h">要盯的基金 <span class="panel-sub">数据缺口 / 低置信度</span></div><div class="empty">今天没有数据或置信度预警。</div></div>"#.to_string();
    }
    let mut body = String::new();
    for row in &rows {
        let f = row.fund;
        let mut chips = String::new();
        for (label, tone) in &row.reasons {
            let _ = write!(
                chips,
                r#"<span class="flag {tone}">{label}</span>"#,
                tone = tone,
                label = esc(label)
            );
        }
        let _ = write!(
            body,
            r#"<tr><td>{name}<span class="dim mono"> {code}</span></td><td class="{cls}">{pct}</td><td>{cov}</td><td><div class="flags">{chips}</div></td></tr>"#,
            name = esc(&f.name),
            code = f.code,
            cls = tone_class(f.estimated_pct),
            pct = pct(f.estimated_pct),
            cov = weight_pct(f.coverage),
            chips = chips,
        );
    }
    format!(
        r#"<div class="panel">
  <div class="panel-h">要盯的基金 <span class="panel-sub">数据缺口 / 低置信度 / 今日大跌 · 不是买卖建议</span></div>
  <table class="exposures warn-table"><thead><tr><th>基金</th><th>今日估算</th><th>覆盖</th><th>为什么盯</th></tr></thead><tbody>{body}</tbody></table>
  <div class="footline">这类基金今天可以看，但别把它的估算当准数；优先补行情与持仓数据。</div>
</div>"#
    )
}

fn render_hero(data: &PageData) -> String {
    let s = &data.summary;
    let hv = &data.history;
    let pnl_cls = tone_class(s.today_pnl_pct);
    let one_year = hv.horizons.iter().find(|h| h.days == 365);
    let one_year_ret = one_year
        .map(|h| pct(h.ret))
        .unwrap_or_else(|| "—".to_string());
    let one_year_cls = one_year.map(|h| tone_class(h.ret)).unwrap_or("flat");
    let dd = one_year
        .map(|h| format!("{:.1}%", h.max_drawdown * 100.0))
        .unwrap_or_else(|| "—".to_string());
    let vol = one_year
        .and_then(|h| h.volatility)
        .map(|v| format!("{:.1}%", v * 100.0))
        .unwrap_or_else(|| "—".to_string());
    let pending_note = if s.pending_cash > 0.5 {
        format!(" · 另有在途申购 {}", cny(s.pending_cash))
    } else {
        String::new()
    };
    let cost_note = match (s.cost_basis, s.pnl_since_cost) {
        (Some(cost), Some(pnl)) => format!(
            "成本 {} · 累计 {}（成本已知 {} 只）",
            cny(cost),
            signed_cny(pnl),
            s.cost_known
        ),
        _ => "成本待补录".to_string(),
    };

    format!(
        r#"<section class="hero">
  <div class="hero-main">
    <div class="k">总市值（今日估算）</div>
    <div class="hero-value">{total}</div>
    <div class="hero-sub">今日 <span class="{cls}">{pnl}</span>（<span class="{cls}">{pnl_pct}</span>）· {n} 只持仓 · {snap}</div>
    <div class="hero-note">{cost_note} · {coverage}{pending_note}</div>
  </div>
  <div class="hero-side">
    <div class="hero-kpis">
      <div><div class="k">1 年</div><div class="v {ycls}">{yret}</div></div>
      <div><div class="k">最大回撤</div><div class="v">{dd}</div></div>
      <div><div class="k">年化波动</div><div class="v">{vol}</div></div>
    </div>
    <div class="hero-risk">对比基准：沪深300 / 创业板 · {note}</div>
  </div>
</section>"#,
        total = cny(s.total_value),
        cls = pnl_cls,
        pnl = signed_cny(s.today_pnl),
        pnl_pct = pct(s.today_pnl_pct),
        n = s.positions,
        snap = hero_snapshot(data),
        cost_note = esc(&cost_note),
        coverage = hero_coverage(s),
        yret = one_year_ret,
        ycls = one_year_cls,
        dd = dd,
        vol = vol,
        note = esc(&hv.note),
    )
}

fn hero_snapshot(data: &PageData) -> String {
    match data.snapshot.as_ref() {
        Some((ts, _)) => format!("快照 {}", local_ts(ts)),
        None => "无盘中快照".to_string(),
    }
}

fn hero_coverage(s: &service::PortfolioSummary) -> String {
    if s.covered == s.positions {
        format!("{} 只全部可估算", s.positions)
    } else {
        format!("{}/{} 只可估算", s.covered, s.positions)
    }
}

fn render_health(data: &PageData) -> String {
    let hv = &data.history;
    let mut rows = String::new();
    for item in &hv.health {
        let _ = write!(
            rows,
            r#"<div class="health-row"><span class="dot {cls}" title="{title}"></span><div><div class="health-label">{label}</div><div class="health-detail">{detail}</div></div></div>"#,
            cls = health_class(item.status),
            title = health_title(item.status),
            label = item.label,
            detail = esc(&item.detail),
        );
    }
    let bad: Vec<&str> = hv
        .health
        .iter()
        .filter(|i| i.status == HealthStatus::Bad)
        .map(|i| i.label)
        .collect();
    let warn: Vec<&str> = hv
        .health
        .iter()
        .filter(|i| i.status == HealthStatus::Warn)
        .map(|i| i.label)
        .collect();
    let verdict = if !bad.is_empty() {
        format!("红线：{}。先处理数据缺口，再谈调仓。", bad.join("、"))
    } else if !warn.is_empty() {
        format!("偏黄：{}。暂时不构成红线，但值得盯。", warn.join("、"))
    } else {
        "五项都正常；保持监控即可。".to_string()
    };

    format!(
        r#"<div class="panel">
  <div class="panel-h">组合体检 <span class="panel-sub">5 个信号 · 不是打分</span></div>
  <div class="health">{rows}</div>
  <div class="verdict">{verdict}</div>
</div>"#,
        rows = rows,
        verdict = esc(&verdict),
    )
}

fn render_actions(data: &PageData) -> String {
    let hv = &data.history;
    if hv.actions.is_empty() {
        return r#"<div class="panel"><div class="panel-h">Action items</div><div class="empty">暂无待办。</div></div>"#.to_string();
    }
    let mut rows = String::new();
    for action in &hv.actions {
        let _ = write!(
            rows,
            r#"<div class="action"><span class="sev {cls}">{sev}</span><div><div class="action-title">{title}</div><div class="action-detail">{detail}</div></div></div>"#,
            cls = health_class(action.severity),
            sev = action_severity(action.severity),
            title = esc(&action.title),
            detail = esc(&action.detail),
        );
    }
    format!(
        r#"<div class="panel">
  <div class="panel-h">Action items <span class="panel-sub">数据/暴露，不是投资建议</span></div>
  <div class="actions">{rows}</div>
</div>"#
    )
}

fn render_donut(data: &PageData) -> String {
    let buckets = &data.history.buckets;
    if buckets.is_empty() {
        return r#"<div class="panel"><div class="panel-h">资源桶</div><div class="empty">没有持仓市值。</div></div>"#.to_string();
    }
    let r = 64.0_f64;
    let c = 2.0 * PI * r;
    let mut offset = 0.0_f64;
    let mut segs = String::new();
    for b in buckets {
        let len = (b.weight * c).max(0.0);
        let dash_offset = -offset;
        let _ = write!(
            segs,
            r#"<circle cx="90" cy="90" r="{r:.1}" fill="none" stroke="{color}" stroke-width="20" stroke-dasharray="{len:.2} {rest:.2}" stroke-dashoffset="{dash_offset:.2}" transform="rotate(-90 90 90)"><title>{label} {w:.1}%</title></circle>"#,
            color = b.color,
            len = len,
            rest = (c - len).max(0.0),
            label = b.label,
            w = b.weight * 100.0,
        );
        offset += len;
    }
    let mut legend = String::new();
    for b in buckets {
        let _ = write!(
            legend,
            r#"<div class="legend-row"><span class="swatch" style="background:{color}"></span><span class="legend-label">{label}</span><span class="legend-val">{w}</span><span class="legend-cny">{v}</span></div>"#,
            color = b.color,
            label = b.label,
            w = weight_pct(b.weight),
            v = wan(b.value),
        );
    }
    format!(
        r#"<div class="panel">
  <div class="panel-h">资源桶 <span class="panel-sub">按前收市值 · 覆盖全部持仓</span></div>
  <div class="donut-wrap">
    <svg class="donut" viewBox="0 0 180 180" role="img" aria-label="资源桶占比">
      <circle cx="90" cy="90" r="{r:.1}" fill="none" stroke='#1b2430' stroke-width="20"></circle>
      {segs}
      <text x="90" y="86" text-anchor="middle" class="donut-total">{total}</text>
      <text x="90" y="104" text-anchor="middle" class="donut-sub">前收市值</text>
    </svg>
    <div class="legend">{legend}</div>
  </div>
  <div class="footline">{summary}</div>
</div>"#,
        r = r,
        segs = segs,
        total = cny(buckets.iter().map(|b| b.value).sum()),
        legend = legend,
        summary = esc(&donut_summary(buckets)),
    )
}

fn donut_summary(buckets: &[service::BucketSlice]) -> String {
    let gold: f64 = buckets
        .iter()
        .filter(|b| b.label == "黄金" || b.label == "有色")
        .map(|b| b.weight)
        .sum();
    format!(
        "黄金+有色 {:.1}%；这不是「分散」，是同一条美元/实际利率逻辑。",
        gold * 100.0
    )
}

fn render_top_exposures(data: &PageData) -> String {
    let report = &data.lookthrough.report;
    if report.exposures.is_empty() {
        return r#"<div class="panel"><div class="panel-h">穿透 Top 8</div><div class="empty">暂无持仓数据。</div></div>"#.to_string();
    }
    let max = report
        .exposures
        .iter()
        .map(|e| e.total_weight)
        .fold(0.0_f64, f64::max)
        .max(1e-9);
    let mut rows = String::new();
    for e in report.exposures.iter().take(8) {
        let width = (e.total_weight / max * 100.0).clamp(0.0, 100.0);
        let _ = write!(
            rows,
            r#"<div class="exp-row"><div class="exp-name">{name}<span class="dim">{sector}</span></div><div class="exp-bar"><i style="width:{width:.0}%"></i></div><div class="exp-val">{w}</div><div class="exp-n">{n} 只</div></div>"#,
            name = esc(&e.name),
            sector = esc(e.sector.as_deref().unwrap_or("")),
            width = width,
            w = weight_pct(e.total_weight),
            n = e.fund_count,
        );
    }
    format!(
        r#"<div class="panel">
  <div class="panel-h">穿透 Top 8 <span class="panel-sub">组合层面，不是基金内权重</span></div>
  <div class="exps">{rows}</div>
  <div class="footline">单票归因 {attributed} · hhi {hhi:.4} · <a href="/api/lookthrough">完整矩阵</a></div>
</div>"#,
        rows = rows,
        attributed = weight_pct(report.attributed_weight),
        hhi = report.hhi,
    )
}

fn render_trend(data: &PageData) -> String {
    let hv = &data.history;
    if !hv.available {
        return r#"<div class="panel"><div class="panel-h">组合趋势 <span class="panel-sub">当前份额回溯</span></div><div class="empty">历史净值不足，先跑 <code>python3 collector/build_history.py</code> 并 seed。</div></div>"#.to_string();
    }
    let series: Vec<(&str, &str, &[SeriesPoint])> = vec![
        ("组合", "#58a6ff", hv.portfolio.as_slice()),
        ("沪深300", "#8b949e", hv.hs300.as_slice()),
        ("创业板指", "#d2a8ff", hv.cyb.as_slice()),
    ];
    let verdict = match (
        hv.horizons.iter().find(|h| h.days == 365),
        hv.hs300_ret_1y,
        hv.attribution.first(),
    ) {
        (Some(h), Some(b), Some(top)) => format!(
            "1 年 {:+.2}% vs 沪深300 {:+.2}%（{:+.1}pp）；最大贡献来自 {} {:+.1}pp（见下表）。跨资产比较只作参照。",
            h.ret * 100.0,
            b * 100.0,
            (h.ret - b) * 100.0,
            top.label,
            top.contribution * 100.0
        ),
        (Some(h), Some(b), None) => format!(
            "1 年 {:+.2}% vs 沪深300 {:+.2}%（{:+.1}pp）；归因数据不足。",
            h.ret * 100.0,
            b * 100.0,
            (h.ret - b) * 100.0
        ),
        _ => "1 年对比数据不足。".to_string(),
    };
    format!(
        r#"<div class="panel">
  <div class="panel-h">组合趋势 <span class="panel-sub">当前份额回溯 · 起点=100</span></div>
  {chart}
  {legend}
  <div class="verdict">{verdict}</div>
  <div class="footline">起点 {start} · {n} 个交易日 · 不是账户真实历史收益</div>
</div>"#,
        chart = line_chart_svg(&series, 240.0),
        legend = chart_legend(&series),
        verdict = esc(&verdict),
        start = hv
            .start
            .map(|d| d.to_string())
            .unwrap_or_else(|| "—".to_string()),
        n = hv.portfolio.len(),
    )
}

fn render_basket_trend(data: &PageData) -> String {
    let hv = &data.history;
    if !hv.available || hv.basket.len() < 2 {
        return r#"<div class="panel"><div class="panel-h">穿透篮子趋势</div><div class="empty">历史数据不足。</div></div>"#.to_string();
    }
    let series: Vec<(&str, &str, &[SeriesPoint])> = vec![
        ("穿透篮子", "#ff7b72", hv.basket.as_slice()),
        ("沪深300", "#8b949e", hv.hs300.as_slice()),
    ];
    let verdict = match (hv.basket_ret_1y, hv.hs300_ret_1y) {
        (Some(b), Some(h)) => format!(
            "1 年 {:+.1}% vs 沪深300 {:+.1}%（{:+.1}pp）：不可交易、仅覆盖 {}、按当前披露持仓回放——这是暴露解释，不是择时能力。",
            b * 100.0,
            h * 100.0,
            (b - h) * 100.0,
            weight_pct(hv.attributed_weight)
        ),
        _ => "1 年对比数据不足。".to_string(),
    };
    format!(
        r#"<div class="panel">
  <div class="panel-h">穿透篮子趋势 <span class="panel-sub">当前穿透权重 · 单票归因 {attributed}</span></div>
  {chart}
  {legend}
  <div class="verdict">{verdict}</div>
  <div class="footline">权重来自季报持仓叠加，非实时调仓；覆盖越低，曲线越像噪声。</div>
</div>"#,
        attributed = weight_pct(hv.attributed_weight),
        chart = line_chart_svg(&series, 220.0),
        legend = chart_legend(&series),
        verdict = esc(&verdict),
    )
}

/// 简单多线图：所有序列共用坐标轴，起点都应为 100。
fn line_chart_svg(series: &[(&str, &str, &[SeriesPoint])], height: f64) -> String {
    let width = 640.0_f64;
    let mut min = f64::MAX;
    let mut max = f64::MIN;
    let mut n = 0usize;
    for (_, _, points) in series {
        n = n.max(points.len());
        for p in *points {
            if p.value.is_finite() {
                min = min.min(p.value);
                max = max.max(p.value);
            }
        }
    }
    if n < 2 || !min.is_finite() || !max.is_finite() || max <= min {
        return r#"<div class="empty">数据点不足。</div>"#.to_string();
    }
    let pad_y = (max - min) * 0.06 + 0.5;
    let lo = min - pad_y;
    let hi = max + pad_y;
    let x_of = |i: usize| 6.0 + i as f64 * (width - 12.0) / (n - 1) as f64;
    let y_of = |v: f64| height - 14.0 - (v - lo) / (hi - lo) * (height - 28.0);

    let mut out = format!(
        r#"<svg class="trend" viewBox="0 0 {width:.0} {height:.0}" preserveAspectRatio="none" role="img">"#
    );
    for frac in [0.0, 0.25, 0.5, 0.75, 1.0] {
        let y = y_of(lo + (hi - lo) * frac);
        let _ = write!(
            out,
            r#"<line x1="0" y1="{y:.1}" x2="{width:.0}" y2="{y:.1}" stroke='#1e2733' stroke-width="1"/>"#
        );
    }
    for (_, color, points) in series {
        if points.len() < 2 {
            continue;
        }
        let mut d = String::new();
        for (i, p) in points.iter().enumerate() {
            let _ = write!(
                d,
                "{}{:.1},{:.1}",
                if i == 0 { "M" } else { "L" },
                x_of(i),
                y_of(p.value)
            );
        }
        let _ = write!(
            out,
            r#"<path d="{d}" fill="none" stroke="{color}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>"#
        );
        if let Some(last) = points.last() {
            let _ = write!(
                out,
                r#"<circle cx="{x:.1}" cy="{y:.1}" r="2.6" fill="{color}"/>"#,
                x = x_of(points.len() - 1),
                y = y_of(last.value),
            );
        }
    }
    out.push_str("</svg>");
    out
}

fn chart_legend(series: &[(&str, &str, &[SeriesPoint])]) -> String {
    let mut legend = String::from(r#"<div class="chart-legend">"#);
    for (label, color, points) in series {
        let last = points.last().map(|p| p.value).unwrap_or(100.0);
        let _ = write!(
            legend,
            r#"<span class="legend-item"><i style="background:{color}"></i>{label}<b>{ret}</b></span>"#,
            color = color,
            label = label,
            ret = pct(last / 100.0 - 1.0),
        );
    }
    legend.push_str("</div>");
    legend
}

// ---------------------------------------------------------------------------
// Reference（折叠区）
// ---------------------------------------------------------------------------

fn render_reference(data: &PageData) -> String {
    let mut h = String::new();
    h.push_str(r#"<details class="reference"><summary>Reference · 基金估值明细 / 穿透全表 / 数据质量 <span class="dim">点击展开</span></summary><div class="ref-body">"#);
    h.push_str(&render_funds(data));
    h.push_str(&render_lookthrough(data));
    if !data.skipped.is_empty() {
        h.push_str(&render_skipped(data));
    }
    h.push_str(r#"<div class="section-title">口径说明 <span class="hint">为什么这些数字可信/不可信</span></div><section class="panel small">"#);
    let attributed = data.lookthrough.report.attributed_weight * 100.0;
    h.push_str(&format!(r#"<ul class="notes">
<li>今日估值：半透明基金 = 前十大权重×个股涨跌 + 其余仓位×基准；季报持仓有滞后，主动基本身覆盖 25%–75%。</li>
<li>趋势：用**当前份额**回放历史净值，只能回答「如果今天这套仓位拿到去年会怎样」，不是账户真实收益。</li>
<li>穿透：权重来自 position 市值；当前单票归因 {attributed:.1}% 是**下限**——ETF 联接基只展开目标 ETF 前十大，黄金按 AU.GOLD 代理。</li>
<li>健康度：5 条确定性规则（集中度、穿透集中、回撤、波动、数据完整性），不是模型打分。</li>
</ul></section>"#));
    h.push_str("</div></details>");
    h
}

fn render_funds(data: &PageData) -> String {
    let mut h = String::new();
    h.push_str(
        r#"<div class="section-title">基金估值 <span class="hint">按估算涨跌排序 · 负值在前 · 偏差项累计 3 个交易日后启用</span></div>"#,
    );

    if data.funds.is_empty() {
        h.push_str(r#"<div class="panel empty">还没有基金数据。先运行 <code>fin-lens seed seeds/funds.sql</code>。</div>"#);
        return h;
    }

    h.push_str(r#"<section class="grid">"#);
    for f in &data.funds {
        let cls = tone_class(f.estimated_pct);
        // 偏差未启用是全组合的阶段性状态，逐卡展示只会变成噪音。
        let mut flags = String::new();
        let visible: Vec<Flag> = f
            .flags
            .iter()
            .copied()
            .filter(|flag| *flag != Flag::BiasInactive)
            .collect();
        if visible.is_empty() {
            flags.push_str(r#"<span class="flag ok">无异常</span>"#);
        } else {
            for flag in visible {
                let (text, tone) = flag_label(flag);
                let _ = write!(flags, r#"<span class="flag {tone}">{text}</span>"#);
            }
        }
        let coverage = (f.coverage * 100.0).clamp(0.0, 100.0);
        let qdii_chip = f
            .qdii
            .as_ref()
            .map(|d| {
                let fx = d.fx_pct.map(pct).unwrap_or_else(|| "—".to_string());
                format!(
                    r#"<div class="qdii">美股T-1 <b>{us}</b> · 汇率 <b>{fx}</b> · β {beta:.2}</div>"#,
                    us = pct(d.us_pct),
                    fx = fx,
                    beta = d.beta,
                )
            })
            .unwrap_or_default();
        let _ = write!(
            h,
            r#"<article class="card">
  <div class="card-head">
    <div class="name" title="{name_full}">{name}</div>
    <span class="tag mode-{mode_cls}">{mode}</span>
  </div>
  <div class="code">{code} · 前值 {prev_nav:.4} <span class="dim">({prev_date})</span></div>
  <div class="big {cls}">{est_pct}</div>
  <div class="line">估算净值 <b>{est_nav:.4}</b> · 原始 {raw} · 偏差 {bias}</div>
  <div class="bar"><i class="bar-{bar_tone}" style="width:{coverage:.0}%"></i></div>
  <div class="bar-label">持仓覆盖 {coverage:.0}% · 置信度 {conf:.0}%</div>
  {qdii_chip}
  <div class="flags">{flags}</div>
</article>"#,
            name_full = esc(&f.name),
            name = esc(&f.name),
            mode = mode_label(f.mode),
            mode_cls = mode_class(f.mode),
            code = esc(&f.code),
            prev_nav = f.prev_nav,
            prev_date = f.prev_nav_date,
            cls = cls,
            est_pct = pct(f.estimated_pct),
            est_nav = f.estimated_nav,
            raw = pct(f.raw_pct),
            bias = if f.bias_pct.abs() < 5e-5 {
                "—".to_string()
            } else {
                pct(f.bias_pct)
            },
            bar_tone = if f.coverage >= 0.6 {
                "ok"
            } else if f.coverage >= 0.35 {
                "warn"
            } else {
                "bad"
            },
            coverage = coverage,
            conf = f.confidence * 100.0,
            qdii_chip = qdii_chip,
            flags = flags,
        );
    }
    h.push_str("</section>");
    h
}

fn render_lookthrough(data: &PageData) -> String {
    let view = &data.lookthrough;
    let report = &view.report;
    let mut h = String::new();

    h.push_str(
        r#"<div class="section-title">穿透重叠 <span class="hint">组合层面单标的暴露（黄金按 AU.GOLD 代理），不是基金内权重</span></div>
<section class="panel">"#,
    );

    if report.exposures.is_empty() {
        h.push_str(r#"<div class="empty">暂无持仓数据。</div>"#);
        h.push_str("</section>");
        return h;
    }

    let max = report
        .exposures
        .iter()
        .map(|e| e.total_weight)
        .fold(0.0_f64, f64::max)
        .max(1e-9);

    h.push_str(
        r#"<table class="exposures">
<thead><tr><th>标的</th><th>代码</th><th>穿透权重</th><th>持有基金</th><th>行业</th></tr></thead>
<tbody>"#,
    );
    for e in report.exposures.iter().take(16) {
        let width = (e.total_weight / max * 100.0).clamp(0.0, 100.0);
        let _ = write!(
            h,
            r#"<tr{class}>
  <td>{name}</td>
  <td class="mono dim">{code}</td>
  <td><div class="wbar"><i style="width:{width:.0}%"></i><span>{weight}</span></div></td>
  <td>{funds} 只</td>
  <td class="dim">{sector}</td>
</tr>"#,
            class = if e.is_alert {
                r#" class="alert-row""#
            } else {
                ""
            },
            name = esc(&e.name),
            code = esc(&e.instrument),
            width = width,
            weight = weight_pct(e.total_weight),
            funds = e.fund_count,
            sector = esc(e.sector.as_deref().unwrap_or("—")),
        );
    }
    h.push_str("</tbody></table>");

    h.push_str(r#"<div class="footline">"#);
    if view.weights_assumed_equal {
        let _ = write!(
            h,
            r#"<span class="flag warn">等权假设</span> 未录入真实持仓市值，穿透权重按 {n} 只有持仓数据的基金等权归一化。"#,
            n = view.fund_count
        );
    } else {
        h.push_str(r#"<span class="flag ok">真实仓位</span> 穿透权重来自 position 表。"#);
    }
    let _ = write!(
        h,
        r#" · 赫芬达尔指数 {hhi:.5} · 单票归因 {attributed:.1}%</div>"#,
        hhi = report.hhi,
        attributed = report.attributed_weight * 100.0,
    );

    if !report.sectors.is_empty() {
        h.push_str(r#"<div class="sectors"><span class="dim">行业暴露：</span>"#);
        for s in report.sectors.iter().take(8) {
            let _ = write!(
                h,
                r#"<span class="sector-chip">{name} {w}</span>"#,
                name = esc(&s.sector),
                w = weight_pct(s.weight),
            );
        }
        h.push_str("</div>");
    }

    h.push_str("</section>");
    h
}

fn render_skipped(data: &PageData) -> String {
    let mut h = String::new();
    h.push_str(
        r#"<div class="section-title">未能估值 <span class="hint">缺净值或行情，不参与汇总</span></div><section class="panel">"#,
    );
    for (code, reason) in &data.skipped {
        let _ = write!(
            h,
            r#"<div class="skip"><span class="mono dim">{code}</span> {reason}</div>"#,
            code = esc(code),
            reason = esc(reason),
        );
    }
    h.push_str("</section>");
    h
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 金额：`¥` + 千分位（四舍五入到元）。
fn cny(v: f64) -> String {
    let n = v.round() as i64;
    if n < 0 {
        format!("-¥{}", thousands(n))
    } else {
        format!("¥{}", thousands(n))
    }
}

/// 万元口径，用于环形图图例。
fn wan(v: f64) -> String {
    if v >= 10_000.0 {
        format!("{:.1} 万", v / 10_000.0)
    } else {
        format!("{:.0}", v)
    }
}

/// 带符号金额：正数前缀 `+`，负数前缀 `-`。
fn signed_cny(v: f64) -> String {
    let n = v.round() as i64;
    if n >= 0 {
        format!("+¥{}", thousands(n))
    } else {
        format!("-¥{}", thousands(n))
    }
}

fn pct(v: f64) -> String {
    format!("{:+.2}%", v * 100.0)
}

/// 权重/占比不带正负号，避免看起来像涨跌幅。
fn weight_pct(v: f64) -> String {
    format!("{:.2}%", v * 100.0)
}

fn horizon_label(days: i64) -> &'static str {
    match days {
        7 => "1 周",
        30 => "1 月",
        90 => "1 季",
        182 => "半年",
        365 => "1 年",
        _ => "区间",
    }
}

/// 快照时间统一按本地时区展示（库里存 UTC）。
fn local_ts(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| ts.chars().take(16).collect())
}

fn tone_class(v: f64) -> &'static str {
    if v > 5e-5 {
        "up"
    } else if v < -5e-5 {
        "down"
    } else {
        "flat"
    }
}

fn health_class(status: HealthStatus) -> &'static str {
    match status {
        HealthStatus::Good => "good",
        HealthStatus::Warn => "warn",
        HealthStatus::Bad => "bad",
    }
}

fn health_title(status: HealthStatus) -> &'static str {
    match status {
        HealthStatus::Good => "正常",
        HealthStatus::Warn => "关注",
        HealthStatus::Bad => "需要处理",
    }
}

fn action_severity(status: HealthStatus) -> &'static str {
    match status {
        HealthStatus::Good => "低",
        HealthStatus::Warn => "中",
        HealthStatus::Bad => "高",
    }
}

fn mode_label(mode: ValuationMode) -> &'static str {
    match mode {
        ValuationMode::Transparent => "透明",
        ValuationMode::SemiTransparent => "半透明",
        ValuationMode::Opaque => "不透明",
        ValuationMode::Qdii => "QDII",
    }
}

fn mode_class(mode: ValuationMode) -> &'static str {
    match mode {
        ValuationMode::Transparent => "t",
        ValuationMode::SemiTransparent => "s",
        ValuationMode::Opaque => "o",
        ValuationMode::Qdii => "q",
    }
}

fn flag_label(flag: Flag) -> (&'static str, &'static str) {
    match flag {
        Flag::MissingHoldingQuote => ("缺个股行情", "warn"),
        Flag::ResidualProxyMissing => ("残余无代理", "warn"),
        Flag::BetaMissing => ("β 缺失", "warn"),
        Flag::BetaClamped => ("β 夹紧", "muted"),
        Flag::BiasInactive => ("偏差待校准", "muted"),
        Flag::OpaqueFallback => ("不透明回退", "bad"),
        Flag::LowCoverage => ("覆盖不足", "bad"),
        Flag::QdiiLag => ("美股T-1", "muted"),
        Flag::FxMissing => ("缺汇率", "warn"),
    }
}

const STYLE: &str = r#"
:root {
  color-scheme: dark;
  --bg: #0b0f14;
  --panel: #111823;
  --line: #1e2733;
  --text: #e6edf3;
  --dim: #8b949e;
  --accent: #58a6ff;
  --up: #ff6b6b;
  --down: #2ecc71;
  --warn: #f0b429;
}
* { box-sizing: border-box; }
body {
  margin: 0;
  background: var(--bg);
  color: var(--text);
  font: 14px/1.55 -apple-system, BlinkMacSystemFont, "Segoe UI", "PingFang SC",
    "Hiragino Sans GB", "Microsoft YaHei", sans-serif;
}
a { color: var(--accent); text-decoration: none; }
header {
  position: sticky; top: 0; z-index: 5;
  display: flex; align-items: baseline; gap: 14px; flex-wrap: wrap;
  padding: 14px 28px;
  background: rgba(11, 15, 20, .92);
  backdrop-filter: blur(8px);
  border-bottom: 1px solid var(--line);
}
h1 { margin: 0; font-size: 19px; letter-spacing: .14em; font-weight: 700; }
.accent { color: var(--accent); }
.sub { color: var(--dim); font-size: 12px; }
.spacer { flex: 1; }
.clock { color: var(--dim); font-size: 12px; font-variant-numeric: tabular-nums; }
.json { font-size: 12px; border: 1px solid var(--line); padding: 3px 10px; border-radius: 999px; }
main { max-width: 1440px; margin: 0 auto; padding: 18px 28px 20px; }
footer { color: var(--dim); font-size: 12px; padding: 18px 28px 30px; border-top: 1px solid var(--line); }
.mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
.dim { color: var(--dim); }
.up { color: var(--up); }
.down { color: var(--down); }
.flat { color: var(--dim); }

/* Hero */
.hero { display: grid; grid-template-columns: minmax(280px, 1.05fr) 2fr; gap: 12px; }
.hero-main, .hero-side { background: var(--panel); border: 1px solid var(--line); border-radius: 12px; padding: 16px 18px; }
.hero .k { color: var(--dim); font-size: 12px; }
.hero-value { font-size: 34px; font-weight: 750; letter-spacing: -.02em; font-variant-numeric: tabular-nums; margin-top: 2px; }
.hero-sub { color: var(--dim); font-size: 13px; margin-top: 4px; }
.hero-sub b { color: var(--text); }
.hero-note { color: var(--dim); font-size: 12px; margin-top: 6px; }
.horizons { display: grid; grid-template-columns: repeat(5, 1fr); gap: 8px; }
.hchip { background: #0d131b; border: 1px solid var(--line); border-radius: 8px; padding: 8px 10px; }
.hchip .k { color: var(--dim); font-size: 11px; }
.hchip .v { font-size: 17px; font-weight: 650; font-variant-numeric: tabular-nums; }
.hchip .s { color: var(--dim); font-size: 10px; margin-top: 2px; }
.hero-risk { color: var(--dim); font-size: 12px; margin-top: 10px; }

/* Narrative flow */
.headline-bar { margin-top: 12px; background: linear-gradient(90deg, rgba(88,166,255,.13), rgba(88,166,255,.02)); border: 1px solid #1f3a5c; border-radius: 10px; padding: 11px 14px; font-size: 12.5px; line-height: 1.6; }
.headline-bar .lead { color: #58a6ff; font-weight: 700; margin-right: 8px; }
.read-hint { color: var(--dim); font-size: 11.5px; margin: 7px 4px 0; }
.flow-h { display: flex; align-items: baseline; gap: 10px; margin: 24px 4px 10px; }
.flow-h .num { width: 22px; height: 22px; border-radius: 50%; background: #1f6feb; color: #fff; font-size: 12px; font-weight: 700; display: inline-flex; align-items: center; justify-content: center; flex: none; transform: translateY(2px); }
.flow-h h2 { font-size: 15px; font-weight: 700; margin: 0; }
.flow-h .q { color: var(--dim); font-size: 12px; }
.two-col { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; align-items: start; }
.hero-kpis { display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; }
.hero-kpis .k { color: var(--dim); font-size: 11px; }
.hero-kpis .v { font-size: 19px; font-weight: 650; font-variant-numeric: tabular-nums; margin-top: 3px; }
.losers { display: flex; flex-direction: column; gap: 9px; }
.loser { display: flex; justify-content: space-between; align-items: center; gap: 10px; }
.loser .lname { font-size: 13px; }
.loser .lname .dim { margin-left: 6px; font-size: 11px; }
.loser .lval { font-size: 16px; font-weight: 650; font-variant-numeric: tabular-nums; }
table.attr { width: 100%; border-collapse: collapse; font-variant-numeric: tabular-nums; }
.attr th { text-align: left; color: var(--dim); font-weight: 500; font-size: 11.5px; padding: 4px 10px 8px 0; border-bottom: 1px solid var(--line); }
.attr td { padding: 7px 10px 7px 0; border-bottom: 1px solid rgba(30, 39, 51, .55); }
.attr tr:last-child td { border-bottom: none; }
.attr .swatch { display: inline-block; margin-right: 7px; vertical-align: 1px; }
.warn-table td { vertical-align: top; }

/* Layout */
.dash { display: grid; grid-template-columns: 330px minmax(0, 1.1fr) minmax(0, 1fr); gap: 12px; margin-top: 12px; align-items: start; }
.stack { display: flex; flex-direction: column; gap: 12px; }
.panel { background: var(--panel); border: 1px solid var(--line); border-radius: 12px; padding: 14px 16px; }
.panel-h { font-size: 13px; font-weight: 650; letter-spacing: .02em; margin-bottom: 10px; }
.panel-sub { color: var(--dim); font-weight: 400; font-size: 11px; margin-left: 8px; }
.empty { color: var(--dim); padding: 6px 0; font-size: 12px; }
.small { font-size: 12px; }

/* Health */
.health { display: flex; flex-direction: column; gap: 10px; }
.health-row { display: flex; gap: 9px; align-items: flex-start; }
.dot { width: 8px; height: 8px; border-radius: 50%; flex: none; margin-top: 6px; }
.dot.good { background: var(--down); }
.dot.warn { background: var(--warn); }
.dot.bad { background: #ff6b6b; }
.health-label { font-size: 12px; font-weight: 600; }
.health-detail { color: var(--dim); font-size: 11.5px; }
.verdict { margin-top: 12px; padding-top: 10px; border-top: 1px solid var(--line); color: var(--text); font-size: 12px; }

/* Actions */
.actions { display: flex; flex-direction: column; gap: 10px; }
.action { display: flex; gap: 9px; align-items: flex-start; }
.sev { flex: none; font-size: 10px; padding: 1px 6px; border-radius: 999px; margin-top: 2px; }
.sev.good { color: var(--down); border: 1px solid #1d3b2a; }
.sev.warn { color: var(--warn); border: 1px solid #4a3a12; }
.sev.bad { background: #5a2420; color: #ff8376; }
.action-title { font-size: 12px; font-weight: 600; }
.action-detail { color: var(--dim); font-size: 11.5px; }

/* Donut */
.donut-wrap { display: flex; gap: 16px; align-items: center; }
.donut { width: 168px; height: 168px; flex: none; }
.donut-total { fill: var(--text); font-size: 15px; font-weight: 700; }
.donut-sub { fill: var(--dim); font-size: 9px; }
.legend { display: flex; flex-direction: column; gap: 5px; flex: 1; }
.legend-row { display: grid; grid-template-columns: 10px 1fr 52px 58px; gap: 6px; align-items: center; font-size: 12px; }
.swatch { width: 9px; height: 9px; border-radius: 2px; }
.legend-label { color: var(--text); }
.legend-val { color: var(--text); font-variant-numeric: tabular-nums; text-align: right; }
.legend-cny { color: var(--dim); font-variant-numeric: tabular-nums; text-align: right; }
.footline { color: var(--dim); font-size: 11.5px; margin-top: 10px; }

/* Exposures */
.exps { display: flex; flex-direction: column; gap: 7px; }
.exp-row { display: grid; grid-template-columns: 150px 1fr 58px 34px; gap: 8px; align-items: center; font-size: 12px; }
.exp-name { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.exp-name .dim { margin-left: 6px; font-size: 10px; }
.exp-bar { height: 7px; background: #1b2430; border-radius: 999px; overflow: hidden; }
.exp-bar i { display: block; height: 100%; background: linear-gradient(90deg, #1f6feb, #58a6ff); border-radius: 999px; }
.exp-val { text-align: right; font-variant-numeric: tabular-nums; }
.exp-n { color: var(--dim); text-align: right; }

/* Charts */
.trend { width: 100%; height: auto; display: block; }
.chart-legend { display: flex; flex-wrap: wrap; gap: 12px; margin-top: 8px; font-size: 11.5px; color: var(--dim); }
.legend-item { display: flex; align-items: center; gap: 5px; }
.legend-item i { width: 8px; height: 8px; border-radius: 50%; display: inline-block; }
.legend-item b { color: var(--text); font-variant-numeric: tabular-nums; }

/* Reference */
.reference { margin-top: 22px; border-top: 1px solid var(--line); padding-top: 12px; }
.reference summary { cursor: pointer; color: var(--text); font-size: 13px; font-weight: 600; padding: 6px 0; }
.reference summary .dim { font-weight: 400; font-size: 11px; margin-left: 8px; }
.ref-body { margin-top: 8px; }
.notes { margin: 6px 0 0; padding-left: 18px; color: var(--dim); }
.notes li { margin: 4px 0; }
.section-title { margin: 22px 4px 10px; font-size: 13px; font-weight: 600; letter-spacing: .04em; }
.hint { color: var(--dim); font-weight: 400; font-size: 12px; margin-left: 8px; }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(250px, 1fr)); gap: 12px; }
.card { background: var(--panel); border: 1px solid var(--line); border-radius: 10px; padding: 13px 15px; }
.card-head { display: flex; align-items: flex-start; gap: 8px; justify-content: space-between; }
.name { font-weight: 600; font-size: 13px; line-height: 1.35; }
.code { color: var(--dim); font-size: 11.5px; margin-top: 4px; font-variant-numeric: tabular-nums; }
.tag { flex: none; font-size: 10px; padding: 1px 7px; border-radius: 999px; border: 1px solid var(--line); color: var(--dim); }
.mode-t { color: #58a6ff; border-color: #1f3a5c; }
.mode-s { color: #d2a8ff; border-color: #3d2a52; }
.mode-o { color: var(--warn); border-color: #4a3a12; }
.mode-q { color: #3fb950; border-color: #1d3b2a; }
.big { font-size: 26px; font-weight: 700; margin: 6px 0 2px; font-variant-numeric: tabular-nums; }
.line { color: var(--dim); font-size: 11.5px; }
.bar { height: 4px; background: #1b2430; border-radius: 999px; overflow: hidden; margin: 10px 0 4px; }
.bar i { display: block; height: 100%; border-radius: 999px; }
.bar-ok { background: var(--accent); }
.bar-warn { background: var(--warn); }
.bar-bad { background: #e5534b; }
.bar-label { color: var(--dim); font-size: 10.5px; }
.qdii { margin-top: 6px; font-size: 10.5px; color: var(--dim); font-variant-numeric: tabular-nums; }
.qdii b { color: var(--text); font-weight: 600; }
.flags { display: flex; flex-wrap: wrap; gap: 5px; margin-top: 8px; }
.flag { font-size: 10.5px; padding: 1px 7px; border-radius: 999px; border: 1px solid var(--line); color: var(--dim); }
.flag.ok { color: var(--down); border-color: #1d3b2a; }
.flag.warn { color: var(--warn); border-color: #4a3a12; }
.flag.bad { color: #ff8376; border-color: #5a2420; }
.flag.muted { color: var(--dim); }
table.exposures { width: 100%; border-collapse: collapse; font-variant-numeric: tabular-nums; }
.exposures th { text-align: left; color: var(--dim); font-weight: 500; font-size: 11.5px; padding: 4px 10px 8px 0; border-bottom: 1px solid var(--line); }
.exposures td { padding: 7px 10px 7px 0; border-bottom: 1px solid rgba(30, 39, 51, .55); vertical-align: middle; }
.exposures tr:last-child td { border-bottom: none; }
.alert-row td:first-child { color: #ff8376; }
.wbar { position: relative; height: 18px; min-width: 160px; }
.wbar i { position: absolute; left: 0; top: 4px; height: 10px; background: linear-gradient(90deg, #1f6feb, #58a6ff); border-radius: 3px; opacity: .55; }
.wbar span { position: relative; padding-left: 8px; font-size: 12px; }
.sectors { display: flex; flex-wrap: wrap; gap: 6px; margin-top: 12px; align-items: center; font-size: 12px; }
.sector-chip { border: 1px solid var(--line); border-radius: 999px; padding: 1px 8px; color: var(--text); }
.skip { font-size: 12px; padding: 4px 0; color: var(--dim); }
code { background: #1b2430; padding: 1px 6px; border-radius: 5px; font-size: 12px; }
@media (max-width: 1100px) {
  .two-col { grid-template-columns: 1fr; }
  .hero { grid-template-columns: 1fr; }
}
@media (max-width: 760px) {
  header, main, footer { padding-left: 16px; padding-right: 16px; }
  .dash { grid-template-columns: 1fr; }
  .horizons { grid-template-columns: repeat(3, 1fr); }
  .hero-value { font-size: 28px; }
  .donut-wrap { flex-direction: column; align-items: flex-start; }
  .exp-row { grid-template-columns: 110px 1fr 52px 30px; }
}
"#;
