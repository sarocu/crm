//! The MCP tool surface.
//!
//! Tool descriptions are the only documentation a model gets, so they carry
//! the operational detail: what a parameter does, what to call next, and
//! what to do when a guess does not resolve.

use std::sync::Arc;

use axum::http::request::Parts;
use crm_core::id::{company_id, root_domain};
use crm_core::index::{ACCOUNTS, ACTIVITIES, COMPANIES, SIGNALS};
use crm_core::model::{
    Account, AccountStatus, Activity, ActivityType, Company, Signal, SignalKind, now_ts,
};
use crm_core::request::{CompanyRequest, RequestStatus};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::Actor;
use crate::filter::Filter;
use crate::render;
use crate::score::{self, Fit};
use crate::state::{AppState, COMPANY_FIELDS, LIVE, MAX_HITS, Page};

/// Hard ceiling on `limit`, so one tool call cannot pull the whole index
/// into a model's context window.
pub const MAX_LIMIT: usize = 50;
pub const DEFAULT_LIMIT: usize = 10;
/// Candidates `find_prospects` scores before picking the best.
const PROSPECT_POOL: usize = 200;
/// Signals and activities shown in a dossier.
const DOSSIER_ITEMS: usize = 20;

#[derive(Clone)]
pub struct CrmServer {
    state: Arc<AppState>,
}

impl CrmServer {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

// ---------------------------------------------------------------- arguments

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchCompaniesArgs {
    /// Free text matched against name, domain, description, industries and
    /// site text: "cold storage", "netsuite", "acme". Omit to browse by
    /// filters alone.
    #[serde(default)]
    pub query: Option<String>,
    /// Vertical slugs or names (see list_verticals). Any of them matches.
    #[serde(default)]
    pub verticals: Option<Vec<String>>,
    /// Apply a profile's hard filters (verticals, territory, size range) on
    /// top of the others. Use find_prospects instead to rank by fit.
    #[serde(default)]
    pub profile: Option<String>,
    /// HQ country codes, ISO 3166-1 alpha-2: ["US", "CA"].
    #[serde(default)]
    pub countries: Option<Vec<String>>,
    /// HQ state or province codes: ["CO", "UT"].
    #[serde(default)]
    pub states: Option<Vec<String>>,
    #[serde(default)]
    pub min_employees: Option<u64>,
    #[serde(default)]
    pub max_employees: Option<u64>,
    /// Only companies with a signal of these kinds since `signal_since`:
    /// hiring, funding, launch, leadership, filing, news. Pass [] or omit
    /// to not filter on signals.
    #[serde(default)]
    pub has_signals: Option<Vec<String>>,
    /// Lower bound for `has_signals` (ISO-8601, YYYY-MM-DD or unix
    /// seconds). Defaults to 90 days ago.
    #[serde(default)]
    pub signal_since: Option<String>,
    /// Only companies whose account is in one of these statuses. "none"
    /// matches companies with no account yet.
    #[serde(default)]
    pub statuses: Option<Vec<String>>,
    /// Leave out companies whose account is in one of these statuses.
    #[serde(default)]
    pub exclude_statuses: Option<Vec<String>>,
    /// Tech seen on their site: ["netsuite", "shopify"].
    #[serde(default)]
    pub tech: Option<Vec<String>>,
    /// Backed by any of these investors: ["Y Combinator"]. Portfolio slugs
    /// from describe_market work too.
    #[serde(default)]
    pub investors: Option<Vec<String>>,
    /// Accelerator cohort, e.g. ["Summer 2026"].
    #[serde(default)]
    pub cohorts: Option<Vec<String>>,
    /// "relevance" (default with a query), "recent_signal" (default
    /// without), "employees" or "name".
    #[serde(default)]
    pub sort: Option<String>,
    /// Max results, default 10, capped at 50.
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindProspectsArgs {
    /// Profile slug or name (see list_profiles).
    pub profile: String,
    /// Optional text to narrow the pool first: "cold storage".
    #[serde(default)]
    pub query: Option<String>,
    /// Max results, default 10, capped at 50.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Include companies already contacted, engaged, in a meeting,
    /// qualified or disqualified, and nurture accounts not yet due.
    /// Default false.
    #[serde(default)]
    pub include_worked: Option<bool>,
    /// Drop anything scoring below this, 0–100. Default 0.
    #[serde(default)]
    pub min_score: Option<u8>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetCompanyArgs {
    /// A company `id` from any result, a domain or URL ("acme.com"), or an
    /// exact company name.
    pub company: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchSignalsArgs {
    /// Free text over signal titles and summaries: "series b", "planner".
    #[serde(default)]
    pub query: Option<String>,
    /// hiring, funding, launch, leadership, filing, news.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
    /// Earliest `occurred_at` (ISO-8601, YYYY-MM-DD or unix seconds).
    /// Defaults to 30 days ago.
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    /// Vertical slugs or names.
    #[serde(default)]
    pub verticals: Option<Vec<String>>,
    /// Limit to one company (id, domain or exact name).
    #[serde(default)]
    pub company: Option<String>,
    /// Role families for hiring signals: sales, marketing, operations,
    /// supply chain, production, finance, engineering, …
    #[serde(default)]
    pub roles: Option<Vec<String>>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateAccountArgs {
    /// Company id, domain or exact name.
    pub company: String,
    /// new, researching, queued, contacted, engaged, meeting, qualified,
    /// disqualified, nurture.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    /// What happens next, in a sentence.
    #[serde(default)]
    pub next_step: Option<String>,
    /// When to touch next (ISO-8601, YYYY-MM-DD or unix seconds). Required
    /// in spirit for nurture: prospecting skips nurture accounts until then.
    #[serde(default)]
    pub next_touch_at: Option<String>,
    /// Required when setting status to disqualified.
    #[serde(default)]
    pub disqualify_reason: Option<String>,
    /// The profile this account was qualified against.
    #[serde(default)]
    pub fit_profile: Option<String>,
    /// Replaces the tag list.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Why, recorded on the status-change activity.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LogActivityArgs {
    /// Company id, domain or exact name.
    pub company: String,
    /// email, call, linkedin, meeting or note.
    #[serde(rename = "type")]
    pub activity_type: String,
    /// outbound or inbound. Defaults to outbound for email, call and
    /// linkedin.
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    /// What was said or sent, briefly.
    pub summary: String,
    /// e.g. sent, no_answer, voicemail, replied, bounced, booked,
    /// not_interested.
    #[serde(default)]
    pub outcome: Option<String>,
    /// When it happened. Defaults to now.
    #[serde(default)]
    pub occurred_at: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddCompanyArgs {
    /// The company's domain or website URL: "acme.com".
    pub domain: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Vertical slugs or names you believe apply.
    #[serde(default)]
    pub verticals: Option<Vec<String>>,
    /// Why it is being added.
    #[serde(default)]
    pub note: Option<String>,
}

// -------------------------------------------------------------------- tools

#[tool_router]
impl CrmServer {
    #[tool(
        name = "describe_market",
        description = "Overview of this CRM: the industry verticals and customer profiles it is configured for, how many companies, signals, accounts and activities are indexed, how fresh each index is, the pipeline broken down by status, and recent signal volume by kind. Call this first."
    )]
    async fn describe_market(&self) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let m = &st.market;
        let now = now_ts();

        let mut counts = serde_json::Map::new();
        for idx in [COMPANIES, SIGNALS, ACCOUNTS, ACTIVITIES] {
            let filter = if idx == COMPANIES { LIVE } else { "" };
            counts.insert(idx.into(), json!(st.count(idx, filter).await));
        }
        let mut fresh = serde_json::Map::new();
        for (idx, field) in [
            (COMPANIES, "updated_at"),
            (SIGNALS, "occurred_at"),
            (ACCOUNTS, "updated_at"),
            (ACTIVITIES, "occurred_at"),
        ] {
            fresh.insert(
                idx.into(),
                json!(render::fmt_ts_opt(st.newest(idx, field).await)),
            );
        }
        let by_vertical = st
            .facet(COMPANIES, "verticals", LIVE)
            .await
            .unwrap_or_default();
        let by_investor = st
            .facet(COMPANIES, "investors", LIVE)
            .await
            .unwrap_or_default();
        let pipeline = st.facet(ACCOUNTS, "status", "").await.unwrap_or_default();
        let recent = st
            .facet(
                SIGNALS,
                "kind",
                &format!("occurred_at >= {}", now - 90 * 86_400),
            )
            .await
            .unwrap_or_default();
        let sizes = st
            .facet(COMPANIES, "employees_band", LIVE)
            .await
            .unwrap_or_default();
        let countries = st
            .facet(COMPANIES, "hq_country", LIVE)
            .await
            .unwrap_or_default();

        let value = json!({
            "market": m.name,
            "slug": m.slug,
            "counts": counts,
            "last_updated": fresh,
            "verticals": m.verticals.iter().map(|v| json!({
                "slug": v.slug,
                "name": v.name,
                "companies": by_vertical.get(&v.slug).copied().unwrap_or(0),
            })).collect::<Vec<_>>(),
            "profiles": m.profiles.iter().map(|p| json!({
                "slug": p.slug,
                "name": p.name,
                "description": p.description,
            })).collect::<Vec<_>>(),
            "portfolios": m.portfolios.iter().map(|p| json!({
                "slug": p.slug,
                "investor": p.investor,
                "companies": by_investor.get(&p.investor).copied().unwrap_or(0),
            })).collect::<Vec<_>>(),
            "pipeline": pipeline,
            "signals_last_90_days": recent,
            "company_sizes": sizes,
            "hq_countries": countries,
            "account_statuses": AccountStatus::ALL.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            "signal_kinds": SignalKind::ALL.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        });
        Ok(respond(render::market(&value, m), value))
    }

    #[tool(
        name = "list_verticals",
        description = "List the industry verticals companies are classified into, with the SIC/NAICS codes and keywords that put a company in each and how many companies each has. Vertical slugs are what `verticals` arguments take."
    )]
    async fn list_verticals(&self) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let counts = st
            .facet(COMPANIES, "verticals", LIVE)
            .await
            .unwrap_or_default();
        let text: String = st
            .market
            .verticals
            .iter()
            .map(|v| render::vertical(v, counts.get(&v.slug).copied()))
            .collect::<Vec<_>>()
            .join("\n");
        let value = json!({
            "verticals": st.market.verticals.iter().map(|v| {
                let mut j = serde_json::to_value(v).unwrap_or_default();
                j["companies"] = json!(counts.get(&v.slug).copied().unwrap_or(0));
                j
            }).collect::<Vec<_>>(),
        });
        Ok(respond(text, value))
    }

    #[tool(
        name = "list_profiles",
        description = "List the ideal customer profiles: for each, the verticals, territory, headcount range, hiring roles and keywords it looks for, and how much each criterion weighs in the fit score. Profile slugs are what find_prospects and search_companies take."
    )]
    async fn list_profiles(&self) -> Result<CallToolResult, ErrorData> {
        let m = &self.state.market;
        let text = m
            .profiles
            .iter()
            .map(render::profile)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(respond(text, json!({ "profiles": m.profiles })))
    }

    #[tool(
        name = "search_companies",
        description = "Search and filter companies by text, vertical, territory, headcount, tech, recent signals and pipeline status. Each result carries its id, firmographics, last signal date and account status. Use find_prospects to rank by fit to a profile; use get_company for one company's full dossier."
    )]
    async fn search_companies(
        &self,
        Parameters(args): Parameters<SearchCompaniesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let limit = clamp_limit(args.limit);
        let offset = args.offset.unwrap_or(0).min(MAX_HITS.saturating_sub(limit));
        let query = args.query.clone().unwrap_or_default();

        let mut f = Filter::new(LIVE);
        if let Some(v) = args.verticals.as_deref() {
            match self.verticals(v) {
                Ok(slugs) => f.any_of("verticals", &slugs),
                Err(e) => return Ok(user_error(e)),
            };
        }
        if let Some(p) = args.profile.as_deref() {
            let Some(profile) = st.market.resolve_profile(p) else {
                return Ok(user_error(self.no_profile(p)));
            };
            apply_profile(&mut f, profile);
        }
        if let Some(c) = &args.countries {
            let up: Vec<String> = c.iter().map(|x| x.trim().to_uppercase()).collect();
            f.any_of("hq_country", &up);
        }
        if let Some(s) = &args.states {
            f.any_of("hq_state", s);
        }
        if let Some(n) = args.min_employees {
            f.gte("employees", n as i64);
        }
        if let Some(n) = args.max_employees {
            f.lte("employees", n as i64);
        }
        if let Some(t) = &args.tech {
            let lower: Vec<String> = t.iter().map(|x| x.trim().to_lowercase()).collect();
            f.any_of("tech", &lower);
        }
        if let Some(i) = &args.investors {
            f.any_of("investors", &self.investors(i));
        }
        if let Some(c) = &args.cohorts {
            f.any_of("cohort", c);
        }
        if let Some(kinds) = args.has_signals.as_deref().filter(|k| !k.is_empty()) {
            let kinds = match parse_kinds(kinds) {
                Ok(k) => k,
                Err(e) => return Ok(user_error(e)),
            };
            let since = match parse_time(args.signal_since.as_deref()) {
                Ok(t) => t.unwrap_or_else(|| now_ts() - 90 * 86_400),
                Err(e) => return Ok(user_error(e)),
            };
            let mut sf = Filter::default();
            sf.any_of("kind", &kinds).gte("occurred_at", since);
            match st.companies_with_signals(&sf.build()).await {
                Ok(ids) if ids.is_empty() => {
                    return Ok(respond(
                        format!(
                            "No company has a {} signal since {}.",
                            kinds.join("/"),
                            render::fmt_ts(since)
                        ),
                        json!({ "results": [], "estimated_total": 0 }),
                    ));
                }
                Ok(ids) => {
                    f.any_of("id", &ids);
                }
                Err(e) => return Ok(backend_error("search_companies", e)),
            }
        }
        match self
            .status_filter(
                &mut f,
                args.statuses.as_deref(),
                args.exclude_statuses.as_deref(),
            )
            .await
        {
            Ok(Some(empty)) => return Ok(empty),
            Ok(None) => {}
            Err(e) => return Ok(user_error(e)),
        }

        let sort: Vec<&str> = match args.sort.as_deref().map(str::trim) {
            None | Some("") => {
                if query.is_empty() {
                    vec!["last_signal_at:desc"]
                } else {
                    vec![]
                }
            }
            Some("relevance") => vec![],
            Some("recent_signal") => vec!["last_signal_at:desc"],
            Some("employees") => vec!["employees:desc"],
            Some("name") => vec!["name:asc"],
            Some(other) => {
                return Ok(user_error(format!(
                    "unknown sort {other:?}; use relevance, recent_signal, employees or name"
                )));
            }
        };

        let page: Page<Company> = match st
            .search(
                COMPANIES,
                &query,
                &f.build(),
                &sort,
                Some(COMPANY_FIELDS),
                limit,
                offset,
            )
            .await
        {
            Ok(p) => p,
            Err(e) => return Ok(backend_error("search_companies", e)),
        };
        let ids: Vec<String> = page.hits.iter().map(|c| c.id.clone()).collect();
        let accounts = st.accounts_by_ids(&ids).await.unwrap_or_default();

        let mut text = header("company", page.hits.len(), page.estimated_total, offset);
        for (i, c) in page.hits.iter().enumerate() {
            text.push_str(&render::company_entry(
                i + offset,
                c,
                accounts.get(&c.id),
                None,
            ));
        }
        if page.hits.is_empty() {
            text.push_str(
                "Nothing matched. Loosen a filter, or call describe_market to see what is indexed.",
            );
        }
        let results: Vec<Value> = page
            .hits
            .iter()
            .map(|c| company_json(c, accounts.get(&c.id), None))
            .collect();
        Ok(respond(
            text,
            json!({ "estimated_total": page.estimated_total, "offset": offset, "results": results }),
        ))
    }

    #[tool(
        name = "find_prospects",
        description = "Rank companies by fit to a customer profile and return the best ones to work next, each with a 0-100 fit score and the reasons behind it (vertical, size, territory, recent hiring in target roles, news, keywords). Companies already being worked are left out unless include_worked is true. This is the starting point for prospecting: pick from here, read get_company, then reach out and log_activity."
    )]
    async fn find_prospects(
        &self,
        Parameters(args): Parameters<FindProspectsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let Some(profile) = st.market.resolve_profile(&args.profile) else {
            return Ok(user_error(self.no_profile(&args.profile)));
        };
        let limit = clamp_limit(args.limit);
        let now = now_ts();

        let mut f = Filter::new(LIVE);
        apply_profile(&mut f, profile);
        if !args.include_worked.unwrap_or(false) {
            match st.skip_ids(now).await {
                Ok(ids) => {
                    f.none_of("id", &ids);
                }
                Err(e) => return Ok(backend_error("find_prospects", e)),
            }
        }
        // Most recently active first, so the pool is where the signals are.
        let query = args.query.clone().unwrap_or_default();
        let sort: &[&str] = if query.is_empty() {
            &["last_signal_at:desc"]
        } else {
            &[]
        };
        let pool: Page<Company> = match st
            .search(COMPANIES, &query, &f.build(), sort, None, PROSPECT_POOL, 0)
            .await
        {
            Ok(p) => p,
            Err(e) => return Ok(backend_error("find_prospects", e)),
        };
        let ids: Vec<String> = pool.hits.iter().map(|c| c.id.clone()).collect();
        let since = now - i64::from(profile.signals.recency_days) * 86_400;
        let signals = match st.signals_for(&ids, Some(since), 25).await {
            Ok(s) => s,
            Err(e) => return Ok(backend_error("find_prospects", e)),
        };

        let min = args.min_score.unwrap_or(0);
        let mut ranked: Vec<(Fit, Company)> = pool
            .hits
            .into_iter()
            .map(|c| {
                let sig = signals.get(&c.id).map(Vec::as_slice).unwrap_or(&[]);
                (score::score(&c, sig, profile, now), c)
            })
            .filter(|(fit, _)| fit.score >= min)
            .collect();
        ranked.sort_by(|a, b| {
            b.0.points
                .total_cmp(&a.0.points)
                .then(b.1.last_signal_at.cmp(&a.1.last_signal_at))
        });
        let considered = ranked.len();
        ranked.truncate(limit);

        let ids: Vec<String> = ranked.iter().map(|(_, c)| c.id.clone()).collect();
        let accounts = st.accounts_by_ids(&ids).await.unwrap_or_default();
        let mut text = format!(
            "Top {} of {considered} candidate(s) for profile {} ({}), scored on {} of {} in the pool{}.\n\n",
            ranked.len(),
            profile.slug,
            profile.name,
            considered.min(PROSPECT_POOL),
            pool.estimated_total,
            if args.include_worked.unwrap_or(false) {
                ""
            } else {
                ", excluding accounts already being worked"
            }
        );
        for (i, (fit, c)) in ranked.iter().enumerate() {
            let mut c = c.clone();
            c.body.clear();
            text.push_str(&render::company_entry(
                i,
                &c,
                accounts.get(&c.id),
                Some(fit),
            ));
        }
        if ranked.is_empty() {
            text.push_str("No candidates. The profile's verticals may have no companies yet — check describe_market — or everything matching is already being worked.");
        }
        let results: Vec<Value> = ranked
            .iter()
            .map(|(fit, c)| company_json(c, accounts.get(&c.id), Some(fit)))
            .collect();
        Ok(respond(
            text,
            json!({ "profile": profile.slug, "pool": pool.estimated_total, "results": results }),
        ))
    }

    #[tool(
        name = "get_company",
        description = "Everything known about one company: firmographics, tech seen on their site, job board, our account status and next step, fit to every profile with reasons, the latest signals and the outreach activity log. Accepts an id, a domain or an exact name."
    )]
    async fn get_company(
        &self,
        Parameters(args): Parameters<GetCompanyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let c = match self.company(&args.company).await {
            Ok(c) => c,
            Err(r) => return Ok(r),
        };
        let now = now_ts();
        let (account, signals, activities) = tokio::join!(
            st.account(&c.id),
            st.signals_for(std::slice::from_ref(&c.id), None, DOSSIER_ITEMS),
            st.activities(&c.id, DOSSIER_ITEMS),
        );
        let account = account.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "account lookup failed");
            None
        });
        let signals: Vec<Signal> = signals
            .ok()
            .and_then(|mut m| m.remove(&c.id))
            .unwrap_or_default();
        let activities = activities.unwrap_or_default();
        let fits: Vec<Fit> = st
            .market
            .profiles
            .iter()
            .map(|p| score::score(&c, &signals, p, now))
            .collect();

        let mut shown = c.clone();
        shown.body.clear();
        let text = render::dossier(&shown, account.as_ref(), &signals, &activities, &fits);
        Ok(respond(
            text,
            json!({
                "company": company_json(&shown, account.as_ref(), None),
                "account": account,
                "fit": fits,
                "signals": signals.iter().map(signal_json).collect::<Vec<_>>(),
                "activities": activities,
            }),
        ))
    }

    #[tool(
        name = "search_signals",
        description = "Search buying signals — job postings, funding, launches, leadership changes, SEC filings and press — newest first. Filter by kind, date range, vertical, company and hiring role family. Use this to find timely reasons to reach out, e.g. every company that posted a supply-chain role this month."
    )]
    async fn search_signals(
        &self,
        Parameters(args): Parameters<SearchSignalsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let limit = clamp_limit(args.limit);
        let offset = args.offset.unwrap_or(0).min(MAX_HITS.saturating_sub(limit));
        let since = match parse_time(args.since.as_deref()) {
            Ok(t) => t.unwrap_or_else(|| now_ts() - 30 * 86_400),
            Err(e) => return Ok(user_error(e)),
        };
        let until = match parse_time(args.until.as_deref()) {
            Ok(t) => t,
            Err(e) => return Ok(user_error(e)),
        };

        let mut f = Filter::default();
        f.gte("occurred_at", since);
        if let Some(u) = until {
            f.lte("occurred_at", u);
        }
        if let Some(k) = args.kinds.as_deref() {
            match parse_kinds(k) {
                Ok(k) => f.any_of("kind", &k),
                Err(e) => return Ok(user_error(e)),
            };
        }
        if let Some(v) = args.verticals.as_deref() {
            match self.verticals(v) {
                Ok(slugs) => f.any_of("verticals", &slugs),
                Err(e) => return Ok(user_error(e)),
            };
        }
        if let Some(r) = &args.roles {
            let lower: Vec<String> = r.iter().map(|x| x.trim().to_lowercase()).collect();
            f.any_of("roles", &lower);
        }
        if let Some(key) = args.company.as_deref() {
            match self.company(key).await {
                Ok(c) => f.eq("company_id", &c.id),
                Err(r) => return Ok(r),
            };
        }

        let query = args.query.clone().unwrap_or_default();
        let page: Page<Signal> = match st
            .search(
                SIGNALS,
                &query,
                &f.build(),
                &["occurred_at:desc"],
                None,
                limit,
                offset,
            )
            .await
        {
            Ok(p) => p,
            Err(e) => return Ok(backend_error("search_signals", e)),
        };
        let mut text = header("signal", page.hits.len(), page.estimated_total, offset);
        for (i, s) in page.hits.iter().enumerate() {
            text.push_str(&format!(
                "{}. {}\n",
                i + 1 + offset,
                render::signal_line(s, true)
            ));
        }
        if page.hits.is_empty() {
            text.push_str("Nothing matched. Widen `since`, drop a filter, or check describe_market for signal volume.");
        }
        Ok(respond(
            text,
            json!({
                "estimated_total": page.estimated_total,
                "offset": offset,
                "results": page.hits.iter().map(signal_json).collect::<Vec<_>>(),
            }),
        ))
    }

    #[tool(
        name = "update_account",
        description = "Set our pipeline state for a company: status, owner, next step and when, tags, the profile it was qualified against, or a disqualification reason (required when disqualifying). Creates the account if there is none. A status change is recorded in the activity log with `note` as the reason. Only the fields you pass change."
    )]
    async fn update_account(
        &self,
        Parameters(args): Parameters<UpdateAccountArgs>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let actor = actor(&parts);
        let c = match self.company(&args.company).await {
            Ok(c) => c,
            Err(r) => return Ok(r),
        };
        let mut account = match st.account(&c.id).await {
            Ok(a) => a.unwrap_or_else(|| new_account(&c, &actor)),
            Err(e) => return Ok(backend_error("update_account", e)),
        };
        let before = account.status;

        if let Some(s) = args.status.as_deref() {
            match AccountStatus::parse(s) {
                Some(s) => account.status = s,
                None => {
                    return Ok(user_error(format!(
                        "unknown status {s:?}; use one of: {}",
                        status_names()
                    )));
                }
            }
        }
        if let Some(v) = args.owner {
            account.owner = Some(v).filter(|s| !s.trim().is_empty());
        }
        if let Some(v) = args.next_step {
            account.next_step = Some(v).filter(|s| !s.trim().is_empty());
        }
        if let Some(v) = args.next_touch_at.as_deref() {
            match parse_time(Some(v)) {
                Ok(t) => account.next_touch_at = t,
                Err(e) => return Ok(user_error(e)),
            }
        }
        if let Some(v) = args.disqualify_reason {
            account.disqualify_reason = Some(v).filter(|s| !s.trim().is_empty());
        }
        if let Some(p) = args.fit_profile.as_deref() {
            match st.market.resolve_profile(p) {
                Some(p) => account.fit_profile = Some(p.slug.clone()),
                None => return Ok(user_error(self.no_profile(p))),
            }
        }
        if let Some(t) = args.tags {
            account.tags = t
                .into_iter()
                .map(|x| x.trim().to_lowercase())
                .filter(|x| !x.is_empty())
                .collect();
            account.tags.dedup();
        }
        if account.status == AccountStatus::Disqualified && account.disqualify_reason.is_none() {
            return Ok(user_error(
                "disqualifying needs a `disqualify_reason`, so nobody re-prospects this company without knowing why".into(),
            ));
        }
        if account.status != AccountStatus::Disqualified {
            account.disqualify_reason = None;
        }

        let now = now_ts();
        account.updated_at = now;
        account.updated_by = actor.clone();
        let mut log = Vec::new();
        if account.status != before {
            log.push(status_change(
                &account,
                before,
                args.note.as_deref(),
                &actor,
                now,
            ));
        }
        if let Err(e) = st.write_account(&account).await {
            return Ok(backend_error("update_account", e));
        }
        if !log.is_empty()
            && let Err(e) = st.write_activities(&log).await
        {
            return Ok(backend_error("update_account", e));
        }

        let mut text = format!("{} — ", c.name);
        if account.status != before {
            text.push_str(&format!("status {before} → {}\n", account.status));
        } else {
            text.push_str("updated\n");
        }
        text.push_str(&render::account_block(&account));
        Ok(respond(text, json!({ "account": account })))
    }

    #[tool(
        name = "log_activity",
        description = "Record an outreach touch or note against a company: an email, call, LinkedIn message or meeting, or a research note. Creates the account if needed, and moves its status forward when the touch implies it: outbound email/call/linkedin → contacted, an inbound reply → engaged, a meeting → meeting. It never moves an account backwards or out of qualified/disqualified; use update_account for that."
    )]
    async fn log_activity(
        &self,
        Parameters(args): Parameters<LogActivityArgs>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let actor = actor(&parts);
        let Some(kind) =
            ActivityType::parse(&args.activity_type).filter(|t| *t != ActivityType::StatusChange)
        else {
            return Ok(user_error(format!(
                "unknown type {:?}; use email, call, linkedin, meeting or note",
                args.activity_type
            )));
        };
        let direction = match args.direction.as_deref().map(|d| d.trim().to_lowercase()) {
            Some(d) if d == "inbound" || d == "outbound" => Some(d),
            Some(d) if d.is_empty() => None,
            Some(d) => {
                return Ok(user_error(format!(
                    "direction {d:?} must be inbound or outbound"
                )));
            }
            None if matches!(
                kind,
                ActivityType::Email | ActivityType::Call | ActivityType::Linkedin
            ) =>
            {
                Some("outbound".to_string())
            }
            None => None,
        };
        if args.summary.trim().is_empty() {
            return Ok(user_error("`summary` must say what happened".into()));
        }
        let now = now_ts();
        let occurred_at = match parse_time(args.occurred_at.as_deref()) {
            Ok(t) => t.unwrap_or(now),
            Err(e) => return Ok(user_error(e)),
        };
        let c = match self.company(&args.company).await {
            Ok(c) => c,
            Err(r) => return Ok(r),
        };
        let (mut account, created) = match st.account(&c.id).await {
            Ok(Some(a)) => (a, false),
            Ok(None) => {
                let mut a = new_account(&c, &actor);
                a.status = AccountStatus::Researching;
                (a, true)
            }
            Err(e) => return Ok(backend_error("log_activity", e)),
        };
        let before = if created {
            AccountStatus::New
        } else {
            account.status
        };

        let activity = Activity {
            id: uuid::Uuid::new_v4().simple().to_string(),
            account_id: c.id.clone(),
            company_name: c.name.clone(),
            activity_type: kind,
            direction: direction.clone(),
            subject: args.subject.filter(|s| !s.trim().is_empty()),
            summary: args.summary.trim().to_string(),
            outcome: args
                .outcome
                .map(|o| o.trim().to_lowercase())
                .filter(|o| !o.is_empty()),
            occurred_at,
            actor: actor.clone(),
            created_at: now,
        };
        if let Some(next) = advance(account.status, kind, direction.as_deref()) {
            account.status = next;
        }
        account.last_activity_at = Some(account.last_activity_at.unwrap_or(0).max(occurred_at));
        account.updated_at = now;
        account.updated_by = actor.clone();

        let mut log = vec![activity.clone()];
        if account.status != before {
            log.push(status_change(
                &account,
                before,
                Some(&format!("after {kind}")),
                &actor,
                now,
            ));
        }
        if let Err(e) = st.write_account(&account).await {
            return Ok(backend_error("log_activity", e));
        }
        if let Err(e) = st.write_activities(&log).await {
            return Ok(backend_error("log_activity", e));
        }

        let mut text = format!(
            "Logged on {}: {}\n",
            c.name,
            render::activity_line(&activity)
        );
        if account.status != before {
            text.push_str(&format!("Status {before} → {}\n", account.status));
        }
        Ok(respond(
            text,
            json!({ "activity": activity, "account": account }),
        ))
    }

    #[tool(
        name = "add_company",
        description = "Ask the indexer to add a company by its domain. It is stubbed within a minute or two and its website crawled on the indexer's next crawl pass, after which get_company shows what was found. Returns the id the company will have. If the company is already indexed, says so and returns its id instead."
    )]
    async fn add_company(
        &self,
        Parameters(args): Parameters<AddCompanyArgs>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let actor = actor(&parts);
        let Some(domain) = root_domain(&args.domain) else {
            return Ok(user_error(format!(
                "{:?} is not a domain or website URL I can use; pass something like \"acme.com\"",
                args.domain
            )));
        };
        let id = company_id(Some(&domain), None, None).unwrap_or_default();
        match st.resolve_company(&id).await {
            Ok(Some(c)) => {
                return Ok(respond(
                    format!(
                        "{} ({domain}) is already indexed as id={}. Use get_company.",
                        c.name, c.id
                    ),
                    json!({ "status": "exists", "id": c.id }),
                ));
            }
            Ok(None) => {}
            Err(e) => return Ok(backend_error("add_company", e)),
        }
        match st.pending_request(&domain).await {
            Ok(Some(r)) => {
                return Ok(respond(
                    format!(
                        "{domain} was already requested by {} and is queued; it will be id={}.",
                        r.requested_by, r.company_id
                    ),
                    json!({ "status": "queued", "id": r.company_id }),
                ));
            }
            Ok(None) => {}
            Err(e) => return Ok(backend_error("add_company", e)),
        }
        let verticals = match args.verticals.as_deref() {
            Some(v) => match self.verticals(v) {
                Ok(s) => s,
                Err(e) => return Ok(user_error(e)),
            },
            None => Vec::new(),
        };
        let now = now_ts();
        let req = CompanyRequest {
            id: uuid::Uuid::new_v4().simple().to_string(),
            status: RequestStatus::Pending,
            domain: domain.clone(),
            company_id: id.clone(),
            name: args.name.filter(|n| !n.trim().is_empty()),
            verticals,
            note: args.note.filter(|n| !n.trim().is_empty()),
            requested_by: actor,
            requested_at: now,
            updated_at: now,
            apply_note: None,
        };
        if let Err(e) = st.write_request(&req).await {
            return Ok(backend_error("add_company", e));
        }
        Ok(respond(
            format!(
                "Queued {domain}. It will be id={id}; expect a stub within a couple of minutes and site details after the next crawl pass."
            ),
            json!({ "status": "queued", "id": id, "request": req }),
        ))
    }
}

#[tool_handler]
impl ServerHandler for CrmServer {
    fn get_info(&self) -> ServerConfig {
        let m = &self.state.market;
        let verticals: Vec<&str> = m.verticals.iter().map(|v| v.slug.as_str()).collect();
        let profiles: Vec<&str> = m.profiles.iter().map(|p| p.slug.as_str()).collect();
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                format!("crm-{}", m.slug),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "CRM for outbound prospecting ({name}). Companies are gathered from public \
                 sources — investor portfolios (Y Combinator and other VCs), company websites, \
                 public job boards, trade press, Wikidata and optionally SEC EDGAR — classified into industry verticals ({verticals}) and scored against \
                 customer profiles ({profiles}). Only companies are tracked, never individual \
                 people.\n\n\
                 A typical loop: `find_prospects` with a profile to get ranked companies with \
                 reasons; `get_company` for the dossier and recent signals to personalise \
                 outreach; after reaching out, `log_activity` (which moves the account to \
                 contacted); record decisions with `update_account` (next step, nurture date, \
                 disqualify with a reason). `search_signals` finds timely triggers such as new \
                 job postings or a company newly added to an investor's portfolio; \
                 `search_companies` filters by anything, including investor and cohort. If a company \
                 you need is missing, `add_company` with its domain. Call `describe_market` \
                 first to see what is indexed.",
                name = m.name,
                verticals = verticals.join(", "),
                profiles = profiles.join(", "),
            ))
    }
}

// ------------------------------------------------------------------ helpers

impl CrmServer {
    /// Resolve a company or produce the error result to return.
    async fn company(&self, key: &str) -> Result<Company, CallToolResult> {
        match self.state.resolve_company(key).await {
            Ok(Some(c)) => Ok(c),
            Ok(None) => Err(user_error(format!(
                "no company matches {key:?}. Pass an id from a result, a domain, or an exact name; search_companies finds partial names, and add_company queues a missing one."
            ))),
            Err(e) => Err(backend_error("company lookup", e)),
        }
    }

    fn verticals(&self, names: &[String]) -> Result<Vec<String>, String> {
        let m = &self.state.market;
        names
            .iter()
            .filter(|n| !n.trim().is_empty())
            .map(|n| {
                m.resolve_vertical(n).map(|v| v.slug.clone()).ok_or_else(|| {
                    format!(
                        "unknown vertical {n:?}. Did you mean: {}? list_verticals shows them all.",
                        m.suggest_verticals(n, 3).join(", ")
                    )
                })
            })
            .collect()
    }

    /// Investor names, accepting a portfolio slug for its investor.
    fn investors(&self, names: &[String]) -> Vec<String> {
        names
            .iter()
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
            .map(|n| {
                self.state
                    .market
                    .portfolios
                    .iter()
                    .find(|p| p.slug == n || p.investor.eq_ignore_ascii_case(n))
                    .map(|p| p.investor.clone())
                    .unwrap_or_else(|| n.to_string())
            })
            .collect()
    }

    fn no_profile(&self, name: &str) -> String {
        let m = &self.state.market;
        format!(
            "unknown profile {name:?}. Did you mean: {}? list_profiles shows them all.",
            m.suggest_profiles(name, 3).join(", ")
        )
    }

    /// Turn status filters into id filters. `Ok(Some(result))` is an early,
    /// empty answer; `Err` is a bad status name.
    async fn status_filter(
        &self,
        f: &mut Filter,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
    ) -> Result<Option<CallToolResult>, String> {
        let st = &self.state;
        let parse = |list: &[String]| -> Result<(Vec<String>, bool), String> {
            let mut out = Vec::new();
            let mut none = false;
            for s in list {
                if s.trim().eq_ignore_ascii_case("none") {
                    none = true;
                } else {
                    let st = AccountStatus::parse(s).ok_or_else(|| {
                        format!(
                            "unknown status {s:?}; use one of: {}, or none",
                            status_names()
                        )
                    })?;
                    out.push(st.as_str().to_string());
                }
            }
            Ok((out, none))
        };
        if let Some(inc) = include.filter(|l| !l.is_empty()) {
            let (statuses, none) = parse(inc)?;
            let mut sf = Filter::default();
            sf.any_of("status", &statuses);
            if none {
                // "no account" plus any listed statuses: exclude everything
                // else rather than include a bounded list.
                let others: Vec<String> = AccountStatus::ALL
                    .iter()
                    .map(|s| s.as_str().to_string())
                    .filter(|s| !statuses.contains(s))
                    .collect();
                let mut of = Filter::default();
                of.any_of("status", &others);
                let ids = st
                    .account_ids(&of.build())
                    .await
                    .map_err(|e| e.to_string())?;
                f.none_of("id", &ids);
            } else {
                let ids = st
                    .account_ids(&sf.build())
                    .await
                    .map_err(|e| e.to_string())?;
                if ids.is_empty() {
                    return Ok(Some(respond(
                        format!("No account is in status {}.", statuses.join("/")),
                        json!({ "results": [], "estimated_total": 0 }),
                    )));
                }
                f.any_of("id", &ids);
            }
        }
        if let Some(exc) = exclude.filter(|l| !l.is_empty()) {
            let (statuses, _) = parse(exc)?;
            let mut sf = Filter::default();
            sf.any_of("status", &statuses);
            let ids = st
                .account_ids(&sf.build())
                .await
                .map_err(|e| e.to_string())?;
            f.none_of("id", &ids);
        }
        Ok(None)
    }
}

/// A profile's hard constraints: its verticals, its territory (or unknown),
/// and its size range (or unknown). Soft preferences live in the score.
fn apply_profile(f: &mut Filter, p: &crm_core::market::Profile) {
    f.any_of("verticals", &p.verticals);
    f.any_of("investors", &p.investors);
    let countries: Vec<String> = p.countries.iter().map(|c| c.to_uppercase()).collect();
    f.any_of_or_missing("hq_country", &countries);
    f.any_of_or_missing("hq_state", &p.states);
    match (p.employees.min, p.employees.max) {
        (None, None) => {}
        (min, max) => {
            let mut range = Vec::new();
            if let Some(m) = min {
                range.push(format!("employees >= {m}"));
            }
            if let Some(m) = max {
                range.push(format!("employees <= {m}"));
            }
            f.and(format!(
                "(employees NOT EXISTS OR ({}))",
                range.join(" AND ")
            ));
        }
    }
}

/// How an account moves when an activity is logged. Only ever forward, and
/// never out of a decision someone made on purpose.
pub fn advance(
    current: AccountStatus,
    kind: ActivityType,
    direction: Option<&str>,
) -> Option<AccountStatus> {
    use AccountStatus::*;
    let rank = |s: AccountStatus| match s {
        New | Nurture => 0,
        Researching => 1,
        Queued => 2,
        Contacted => 3,
        Engaged => 4,
        Meeting => 5,
        Qualified | Disqualified => 99,
    };
    let target = match (kind, direction) {
        (ActivityType::Meeting, _) => Meeting,
        (ActivityType::Email | ActivityType::Call | ActivityType::Linkedin, Some("inbound")) => {
            Engaged
        }
        (ActivityType::Email | ActivityType::Call | ActivityType::Linkedin, _) => Contacted,
        _ => return None,
    };
    (rank(target) > rank(current)).then_some(target)
}

fn new_account(c: &Company, actor: &str) -> Account {
    let mut a = Account::new(&c.id, &c.name, actor);
    a.domain = c.domain.clone();
    a
}

fn status_change(
    a: &Account,
    before: AccountStatus,
    note: Option<&str>,
    actor: &str,
    now: i64,
) -> Activity {
    let mut summary = format!("{before} → {}", a.status);
    if let Some(n) = note.filter(|n| !n.trim().is_empty()) {
        summary.push_str(&format!(": {}", n.trim()));
    }
    if let Some(r) = &a.disqualify_reason
        && a.status == AccountStatus::Disqualified
    {
        summary.push_str(&format!(" (reason: {r})"));
    }
    Activity {
        id: uuid::Uuid::new_v4().simple().to_string(),
        account_id: a.id.clone(),
        company_name: a.company_name.clone(),
        activity_type: ActivityType::StatusChange,
        direction: None,
        subject: None,
        summary,
        outcome: None,
        occurred_at: now,
        actor: actor.to_string(),
        created_at: now,
    }
}

fn actor(parts: &Parts) -> String {
    parts
        .extensions
        .get::<Actor>()
        .map(|a| a.0.clone())
        .unwrap_or_else(|| Actor::ANONYMOUS.to_string())
}

fn status_names() -> String {
    AccountStatus::ALL
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn company_json(c: &Company, account: Option<&Account>, fit: Option<&Fit>) -> Value {
    let mut v = serde_json::to_value(c).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        for k in [
            "body",
            "content_hash",
            "indexed_at",
            "name_source",
            "merged_into",
        ] {
            o.remove(k);
        }
        o.insert(
            "last_signal".into(),
            json!(render::fmt_ts_opt(c.last_signal_at)),
        );
        o.insert(
            "account_status".into(),
            json!(account.map(|a| a.status.as_str())),
        );
        if let Some(f) = fit {
            o.insert("fit".into(), json!(f));
        }
    }
    v
}

fn signal_json(s: &Signal) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        for k in ["content_hash", "indexed_at", "updated_at", "source_id"] {
            o.remove(k);
        }
        o.insert("date".into(), json!(render::fmt_ts(s.occurred_at)));
    }
    v
}

fn header(noun: &str, shown: usize, total: usize, offset: usize) -> String {
    let mut s = format!("{shown} {noun}(s)");
    if total > shown + offset {
        s.push_str(&format!(
            " of about {total}; pass offset={} for more",
            offset + shown
        ));
    }
    s.push_str("\n\n");
    s
}

fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn parse_kinds(raw: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for k in raw {
        match SignalKind::parse(k) {
            Some(kind) => {
                let s = kind.as_str().to_string();
                if !out.contains(&s) {
                    out.push(s);
                }
            }
            None => {
                return Err(format!(
                    "unknown signal kind {k:?}; use any of: hiring, funding, launch, leadership, filing, news"
                ));
            }
        }
    }
    Ok(out)
}

/// Accept either an ISO-8601 datetime, a bare date, or a unix timestamp,
/// because models reliably produce all three.
fn parse_time(raw: Option<&str>) -> Result<Option<i64>, String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if let Ok(ts) = raw.parse::<i64>() {
        return Ok(Some(ts));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(Some(dt.timestamp()));
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Ok(Some(d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp()));
    }
    Err(format!(
        "could not read {raw:?} as a time; use ISO-8601 (2026-07-04T00:00:00Z), a date (2026-07-04), or a unix timestamp"
    ))
}

/// A result the caller can act on: readable text plus the structured
/// equivalent, so both kinds of MCP client get something useful.
fn respond(text: String, value: Value) -> CallToolResult {
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(text)];
    result
}

/// The caller asked for something we cannot do. Returned as a tool-level
/// error so the message actually reaches them.
fn user_error(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

fn backend_error(tool: &str, e: anyhow::Error) -> CallToolResult {
    tracing::error!(tool, error = %e, "backend error");
    CallToolResult::error(vec![ContentBlock::text(format!(
        "the backend failed while handling {tool}: {e:#}"
    ))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crm_core::market::MarketConfig;

    #[test]
    fn limit_is_clamped_to_a_sane_window() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(5)), 5);
        assert_eq!(clamp_limit(Some(10_000)), MAX_LIMIT);
    }

    #[test]
    fn times_parse_from_every_shape_a_model_produces() {
        assert_eq!(parse_time(None).unwrap(), None);
        assert_eq!(parse_time(Some("  ")).unwrap(), None);
        assert_eq!(parse_time(Some("0")).unwrap(), Some(0));
        assert_eq!(
            parse_time(Some("1970-01-02T00:00:00Z")).unwrap(),
            Some(86_400)
        );
        assert_eq!(parse_time(Some("1970-01-02")).unwrap(), Some(86_400));
        assert!(parse_time(Some("next tuesday")).is_err());
    }

    #[test]
    fn signal_kinds_parse_and_dedupe() {
        assert_eq!(
            parse_kinds(&["jobs".into(), "hiring".into(), "Funding".into()]).unwrap(),
            vec!["hiring", "funding"]
        );
        assert!(parse_kinds(&["gossip".into()]).is_err());
    }

    #[test]
    fn activities_only_move_accounts_forward() {
        use AccountStatus::*;
        let email = ActivityType::Email;
        assert_eq!(advance(New, email, Some("outbound")), Some(Contacted));
        assert_eq!(advance(Researching, email, None), Some(Contacted));
        assert_eq!(advance(Contacted, email, Some("outbound")), None);
        assert_eq!(advance(Contacted, email, Some("inbound")), Some(Engaged));
        assert_eq!(advance(Engaged, ActivityType::Meeting, None), Some(Meeting));
        assert_eq!(advance(Meeting, email, Some("inbound")), None);
        assert_eq!(advance(Disqualified, ActivityType::Meeting, None), None);
        assert_eq!(advance(Qualified, email, None), None);
        assert_eq!(advance(Nurture, email, Some("outbound")), Some(Contacted));
        assert_eq!(advance(New, ActivityType::Note, None), None);
    }

    #[test]
    fn profiles_become_hard_filters_that_keep_unknowns() {
        let m = MarketConfig::load(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../market.example.toml"
        ))
        .unwrap();
        let mut f = Filter::new(LIVE);
        apply_profile(&mut f, m.profile("mid-market-ops").unwrap());
        assert_eq!(
            f.build(),
            r#"merged_into NOT EXISTS AND verticals IN ["craft-beverage", "food-manufacturing", "logistics"] AND (hq_country IN ["US"] OR hq_country NOT EXISTS) AND (employees NOT EXISTS OR (employees >= 50 AND employees <= 1000))"#
        );
    }

    #[test]
    fn status_changes_explain_themselves() {
        let mut a = Account::new("c", "Acme", "bdr");
        a.status = AccountStatus::Disqualified;
        a.disqualify_reason = Some("too small".into());
        let act = status_change(&a, AccountStatus::Contacted, Some("checked"), "bdr", 5);
        assert_eq!(act.activity_type, ActivityType::StatusChange);
        assert_eq!(
            act.summary,
            "contacted → disqualified: checked (reason: too small)"
        );
    }
}
