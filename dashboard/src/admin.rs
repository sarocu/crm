//! The operator dashboard: a read-only window onto what the indexer has
//! found and what the BDR agent has done with it.
//!
//! Deliberately unauthenticated — the load balancer is expected to sit in
//! front of this port. It writes nothing, so the worst an intruder learns is
//! the pipeline.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use crm_core::model::{AccountStatus, Company};
use maud::{Markup, html};
use serde::Deserialize;

use crate::state::AppState;
use crate::store::{BotHealth, PAGE_SIZE, is_id};
use crate::views::{self, Nav, page, status_badge};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(overview))
        .route("/accounts", get(accounts))
        .route("/accounts/{id}", get(account_detail))
        .route("/companies", get(companies))
        .route("/requests", get(requests))
        .route("/healthz", get(crate::healthz))
        .with_state(state)
}

fn nav(current: &'static str) -> Nav {
    Nav {
        items: vec![
            ("/", "Overview"),
            ("/accounts", "Pipeline"),
            ("/companies", "Companies"),
            ("/requests", "Requests"),
        ],
        current,
    }
}

fn shell(state: &AppState, current: &'static str, title: &str, body: Markup) -> Markup {
    let brand = format!("{} · CRM", state.store.market.name);
    page(
        &brand,
        "/",
        Some(&nav(current)),
        title,
        html! { main.wrap { (body) } },
    )
}

// --------------------------------------------------------------- overview

async fn overview(State(state): State<Arc<AppState>>) -> Markup {
    let o = state.store.overview().await;
    let m = &state.store.market;

    shell(
        &state,
        "Overview",
        "Overview",
        html! {
            h1 { (m.name) }
            p.lede {
                (m.verticals.len()) " verticals, " (m.profiles.len()) " customer profiles. "
                "Market data is gathered by the indexer; accounts and activity are written by the BDR agent over MCP."
            }

            .grid {
                @for (index, n) in &o.documents {
                    .card {
                        .k { (index) }
                        .n { (n.map(|n| n.to_string()).unwrap_or_else(|| "?".into())) }
                        .sub {
                            @match o.freshest.get(index).copied().flatten() {
                                Some(t) => { "newest " (views::ago(t)) }
                                None => { "nothing yet" }
                            }
                        }
                    }
                }
                .card {
                    .k { "requests queued" }
                    a.n href="/requests" { (o.pending_requests) }
                    .sub { "from add_company" }
                }
            }

            h2 { "Pipeline" }
            .chips {
                @for (status, n) in &o.pipeline {
                    a href={ "/accounts?status=" (status) } { (status) " · " (n) }
                }
            }

            h2 { "Companies by vertical" }
            .chips {
                @for v in &m.verticals {
                    a href={ "/companies?vertical=" (v.slug) } {
                        (v.name) " · " (o.by_vertical.get(&v.slug).copied().unwrap_or(0))
                    }
                }
            }

            @if !o.signals_30d.is_empty() {
                h2 { "Signals in the last 30 days" }
                .chips {
                    @for (kind, n) in &o.signals_30d {
                        span."badge"."plain" { (kind) " · " (n) }
                    }
                }
            }

            h2 { "Indexer" }
            (indexer_panel(o.bot.as_ref()))
        },
    )
}

fn indexer_panel(bot: Option<&BotHealth>) -> Markup {
    let Some(bot) = bot else {
        return html! {
            .note.warn {
                strong { "The indexer is not answering. " }
                "Nothing new will be indexed until it is back. Check "
                span.mono { "docker compose logs bot" }
                " or the bot VM, and confirm BOT_HEALTH_URL points at it."
            }
        };
    };

    html! {
        @if bot.status != "ok" {
            .note.warn {
                strong { "The indexer reports " (bot.status) ". " }
                "Meilisearch is " (bot.meilisearch) "."
            }
        }
        @if bot.sources.is_empty() {
            .note {
                "The indexer is up but no source has completed a run yet. "
                "Sources that are disabled never report."
            }
        } @else {
            table {
                thead {
                    tr {
                        th { "Source" }
                        th.nowrap { "Last run" }
                        th { "Runs" }
                        th { "Written" }
                        th { "Unchanged" }
                        th { "Merged" }
                        th { "Dropped" }
                        th { "State" }
                    }
                }
                tbody {
                    @for (name, s) in &bot.sources {
                        tr {
                            td { strong { (name) } }
                            td.nowrap {
                                @match s.last_run_at {
                                    Some(t) => { (views::ago(t)) }
                                    None => { "—" }
                                }
                            }
                            td { (s.runs) }
                            td { (s.totals.written) }
                            td { (s.totals.unchanged) }
                            td { (s.totals.merged) }
                            td {
                                // Signals for companies we have no record of,
                                // and malformed records. A rising count here
                                // is usually a mapping bug.
                                (s.totals.orphaned + s.totals.invalid)
                            }
                            td {
                                @if let Some(err) = &s.last_error {
                                    span."badge"."warn" { "failing" }
                                    div.small.muted { (truncate(err, 140)) }
                                } @else if s.consecutive_errors > 0 {
                                    span."badge"."warn" { (views::plural(s.consecutive_errors as usize, "error")) }
                                } @else {
                                    span."badge"."ok" { "ok" }
                                }
                            }
                        }
                    }
                }
            }
        }
        p.small.muted {
            @if let Some(n) = bot.crawl_frontier_pending { (views::plural(n, "URL")) " queued for crawling. " }
            @if let Some(t) = bot.started_at { "Up since " (views::ts(t)) ". " }
            @if let Some(v) = &bot.version { "v" (v) "." }
        }
    }
}

// -------------------------------------------------------------- accounts

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    vertical: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
}

async fn accounts(State(state): State<Arc<AppState>>, Query(query): Query<ListQuery>) -> Markup {
    let status = query.status.as_deref().and_then(AccountStatus::parse);
    let q = query.q.clone().unwrap_or_default();
    let offset = query.offset.unwrap_or(0);
    let (rows, total) = state
        .store
        .accounts(status, &q, offset)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "listing accounts failed");
            (Vec::new(), 0)
        });

    let link = |s: Option<AccountStatus>| match s {
        Some(s) => format!("/accounts?status={}", s.as_str()),
        None => "/accounts".to_string(),
    };

    shell(
        &state,
        "Pipeline",
        "Pipeline",
        html! {
            h1 { "Pipeline" }
            p.lede { "Every company the agent has touched, most recently updated first." }
            .chips {
                @if status.is_none() {
                    a href=(link(None)) aria-current="page" { "All" }
                } @else {
                    a href=(link(None)) { "All" }
                }
                @for s in AccountStatus::ALL {
                    @if status == Some(s) {
                        a href=(link(Some(s))) aria-current="page" { (s.as_str()) }
                    } @else {
                        a href=(link(Some(s))) { (s.as_str()) }
                    }
                }
            }
            (search_form("/accounts", "Search accounts…", &q, query.status.as_deref().map(|s| ("status", s))))
            @if rows.is_empty() {
                .note { "Nothing here yet." }
            } @else {
                table {
                    thead { tr {
                        th { "Company" } th { "Status" } th { "Next step" }
                        th.nowrap { "Next touch" } th { "Owner" } th.nowrap { "Updated" }
                    } }
                    tbody {
                        @for a in &rows {
                            tr {
                                td {
                                    a href={ "/accounts/" (a.id) } { (a.company_name) }
                                    @if let Some(d) = &a.domain { div.small.muted { (d) } }
                                }
                                td { (status_badge(a.status)) }
                                td { (a.next_step.clone().unwrap_or_default()) }
                                td.nowrap { (views::opt_ts(a.next_touch_at)) }
                                td { (a.owner.clone().unwrap_or_default()) }
                                td.nowrap { (views::ago(a.updated_at)) div.small.muted { (a.updated_by) } }
                            }
                        }
                    }
                }
                (pager("/accounts", &query, offset, rows.len(), total))
            }
        },
    )
}

async fn account_detail(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if !is_id(&id) {
        return (StatusCode::NOT_FOUND, "no such company").into_response();
    }
    let Some(d) = state.store.dossier(&id).await else {
        return (StatusCode::NOT_FOUND, "no such company").into_response();
    };
    let c = &d.company;
    shell(
        &state,
        "Pipeline",
        &c.name,
        html! {
            h1 { (c.name) }
            p.lede { (c.description) }
            (company_facts(c))

            h2 { "Account" }
            @match &d.account {
                None => { .note { "No account yet — the agent has not worked this company." } }
                Some(a) => {
                    dl.facts {
                        dt { "Status" } dd { (status_badge(a.status)) }
                        @if let Some(o) = &a.owner { dt { "Owner" } dd { (o) } }
                        @if let Some(n) = &a.next_step { dt { "Next step" } dd { (n) } }
                        @if let Some(t) = a.next_touch_at { dt { "Next touch" } dd { (views::ts(t)) } }
                        @if let Some(r) = &a.disqualify_reason { dt { "Disqualified" } dd { (r) } }
                        @if let Some(p) = &a.fit_profile { dt { "Profile" } dd { (p) } }
                        @if !a.tags.is_empty() { dt { "Tags" } dd { (a.tags.join(", ")) } }
                        dt { "Updated" } dd { (views::ts(a.updated_at)) " by " (a.updated_by) }
                    }
                }
            }

            h2 { "Activity" }
            @if d.activities.is_empty() {
                p.muted { "None." }
            } @else {
                table {
                    thead { tr { th.nowrap { "When" } th { "Type" } th { "What" } th { "Outcome" } th { "By" } } }
                    tbody {
                        @for a in &d.activities {
                            tr {
                                td.nowrap { (views::ts(a.occurred_at)) }
                                td.nowrap {
                                    (a.activity_type.as_str())
                                    @if let Some(dir) = &a.direction { div.small.muted { (dir) } }
                                }
                                td {
                                    @if let Some(s) = &a.subject { strong { (s) } br; }
                                    (a.summary)
                                }
                                td { (a.outcome.clone().unwrap_or_default()) }
                                td { (a.actor) }
                            }
                        }
                    }
                }
            }

            h2 { "Signals" }
            @if d.signals.is_empty() {
                p.muted { "None found yet." }
            } @else {
                table {
                    thead { tr { th.nowrap { "When" } th { "Kind" } th { "Signal" } th { "Source" } } }
                    tbody {
                        @for s in &d.signals {
                            tr {
                                td.nowrap { (views::ts(s.occurred_at)) }
                                td { span."badge"."plain" { (s.kind.as_str()) } }
                                td {
                                    @match &s.url {
                                        Some(u) => { a href=(u) rel="noopener noreferrer" { (s.title) } }
                                        None => { (s.title) }
                                    }
                                    @if !s.roles.is_empty() { div.small.muted { (s.roles.join(", ")) } }
                                }
                                td { (s.source) }
                            }
                        }
                    }
                }
            }
        },
    )
    .into_response()
}

fn company_facts(c: &Company) -> Markup {
    let hq: Vec<&str> = [
        c.hq_city.as_deref(),
        c.hq_state.as_deref(),
        c.hq_country.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    html! {
        dl.facts {
            @if let Some(w) = c.website.as_ref().or(c.domain.as_ref()) {
                dt { "Website" } dd { a href=(website_href(w)) rel="noopener noreferrer" { (w) } }
            }
            @if !c.verticals.is_empty() { dt { "Verticals" } dd { (c.verticals.join(", ")) } }
            @if !c.industries.is_empty() { dt { "Industries" } dd { (c.industries.join(", ")) } }
            @if let Some(n) = c.employees { dt { "Employees" } dd { (n) } }
            @if !hq.is_empty() { dt { "HQ" } dd { (hq.join(", ")) } }
            @if let Some(y) = c.founded { dt { "Founded" } dd { (y) } }
            @if let Some(t) = &c.ticker { dt { "Ticker" } dd { (t) } }
            @if !c.investors.is_empty() { dt { "Investors" } dd { (c.investors.join(", ")) } }
            @if let Some(b) = &c.cohort { dt { "Cohort" } dd { (b) } }
            @if !c.tech.is_empty() { dt { "Tech" } dd { (c.tech.join(", ")) } }
            @if let (Some(p), Some(s)) = (&c.ats_provider, &c.ats_slug) { dt { "Job board" } dd { (p) "/" (s) } }
            dt { "Sources" } dd { (c.sources.join(", ")) }
            dt { "Id" } dd.mono { (c.id) }
        }
    }
}

fn website_href(w: &str) -> String {
    if w.starts_with("http://") || w.starts_with("https://") {
        w.to_string()
    } else {
        format!("https://{w}")
    }
}

// ------------------------------------------------------------- companies

async fn companies(State(state): State<Arc<AppState>>, Query(query): Query<ListQuery>) -> Markup {
    let q = query.q.clone().unwrap_or_default();
    let offset = query.offset.unwrap_or(0);
    let vertical = query.vertical.as_deref();
    let (rows, total) = state
        .store
        .companies(vertical, &q, offset)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "listing companies failed");
            (Vec::new(), 0)
        });
    let m = &state.store.market;

    shell(
        &state,
        "Companies",
        "Companies",
        html! {
            h1 { "Companies" }
            p.lede { "Everything the indexer has found, most recently active first." }
            .chips {
                @if vertical.is_none() {
                    a href="/companies" aria-current="page" { "All" }
                } @else {
                    a href="/companies" { "All" }
                }
                @for v in &m.verticals {
                    @if vertical == Some(v.slug.as_str()) {
                        a href={ "/companies?vertical=" (v.slug) } aria-current="page" { (v.name) }
                    } @else {
                        a href={ "/companies?vertical=" (v.slug) } { (v.name) }
                    }
                }
            }
            (search_form("/companies", "Search companies…", &q, vertical.map(|v| ("vertical", v))))
            @if rows.is_empty() {
                .note { "Nothing here yet." }
            } @else {
                table {
                    thead { tr {
                        th { "Company" } th { "Verticals" } th { "Employees" } th { "HQ" } th.nowrap { "Last signal" }
                    } }
                    tbody {
                        @for c in &rows {
                            tr {
                                td {
                                    a href={ "/accounts/" (c.id) } { (c.name) }
                                    @if let Some(d) = &c.domain { div.small.muted { (d) } }
                                }
                                td { (c.verticals.join(", ")) }
                                td { (c.employees.map(|n| n.to_string()).unwrap_or_default()) }
                                td { (c.hq_state.clone().or_else(|| c.hq_country.clone()).unwrap_or_default()) }
                                td.nowrap { (c.last_signal_at.map(views::ago).unwrap_or_default()) }
                            }
                        }
                    }
                }
                (pager("/companies", &query, offset, rows.len(), total))
            }
        },
    )
}

// -------------------------------------------------------------- requests

async fn requests(State(state): State<Arc<AppState>>) -> Markup {
    let rows = state.store.requests().await.unwrap_or_else(|e| {
        tracing::error!(error = %e, "listing requests failed");
        Vec::new()
    });
    shell(
        &state,
        "Requests",
        "Requests",
        html! {
            h1 { "Company requests" }
            p.lede { "Companies the agent asked to add with add_company. The indexer stubs each one and queues its site for crawling." }
            @if rows.is_empty() {
                .note { "No requests yet." }
            } @else {
                table {
                    thead { tr { th { "Domain" } th { "Status" } th { "By" } th.nowrap { "When" } th { "Note" } } }
                    tbody {
                        @for r in &rows {
                            tr {
                                td { a href={ "/accounts/" (r.company_id) } { (r.domain) } }
                                td { span class={ "badge " (views::request_class(r.status)) } { (r.status.as_str()) } }
                                td { (r.requested_by) }
                                td.nowrap { (views::ago(r.requested_at)) }
                                td {
                                    @if let Some(n) = &r.note { (n) }
                                    @if let Some(n) = &r.apply_note { div.small.muted { (n) } }
                                }
                            }
                        }
                    }
                }
            }
        },
    )
}

// ---------------------------------------------------------------- helpers

fn search_form(action: &str, placeholder: &str, q: &str, keep: Option<(&str, &str)>) -> Markup {
    html! {
        form method="get" action=(action) style="margin-bottom:18px" {
            @if let Some((k, v)) = keep { input type="hidden" name=(k) value=(v); }
            .actions {
                input type="text" name="q" value=(q) placeholder=(placeholder) style="max-width:24rem";
                button.secondary type="submit" { "Search" }
            }
        }
    }
}

fn pager(path: &str, q: &ListQuery, offset: usize, shown: usize, total: usize) -> Markup {
    let base = |off: usize| {
        let mut s = format!("{path}?offset={off}");
        for (k, v) in [
            ("status", &q.status),
            ("vertical", &q.vertical),
            ("q", &q.q),
        ] {
            if let Some(v) = v {
                s.push_str(&format!("&{k}={}", urlencode(v)));
            }
        }
        s
    };
    html! {
        p.small.muted style="margin-top:14px" {
            "Showing " (offset + 1) "–" (offset + shown) " of about " (total) ". "
            @if offset > 0 {
                a href=(base(offset.saturating_sub(PAGE_SIZE))) { "← previous" }
                " "
            }
            @if offset + shown < total && offset + PAGE_SIZE < 1000 {
                a href=(base(offset + PAGE_SIZE)) { "next →" }
            }
        }
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_errors_are_truncated_on_a_char_boundary() {
        assert_eq!(truncate("héllo", 3), "hél…");
        assert_eq!(truncate("ok", 5), "ok");
    }

    #[test]
    fn websites_link_absolutely() {
        assert_eq!(website_href("acme.test"), "https://acme.test");
        assert_eq!(website_href("http://acme.test/"), "http://acme.test/");
    }

    #[test]
    fn pager_links_escape_the_query() {
        let q = ListQuery {
            status: None,
            vertical: None,
            q: Some("a&b".into()),
            offset: None,
        };
        let out = pager("/companies", &q, 0, 50, 200).into_string();
        assert!(out.contains("q=a%26b"), "{out}");
    }
}
