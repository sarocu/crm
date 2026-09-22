//! Investor portfolios: the companies VCs and accelerators have backed.
//!
//! Being newly backed is one of the earliest buying signals there is — a
//! company that just raised is about to hire and buy — and a portfolio is a
//! ready-made, curated list of companies in a stage and sector. Each
//! configured portfolio is read one of three ways:
//!
//! - `yc`: the Y Combinator directory as JSON. Structured: team size,
//!   location, industry tags, batch, status.
//! - `page`: any portfolio web page. Every outbound link to another domain
//!   is a portfolio company; with `detail_selector`, the investor's own
//!   per-company pages are visited for that link instead. The crawler then
//!   fills in the rest from the company's own site.
//! - `json`: any JSON document listing the portfolio, read with pointers.
//!
//! Every company is tagged with the investor. Once a portfolio has been
//! swept completely, a company that newly appears in it becomes a
//! `funding` signal — the first sweep only establishes the baseline, or the
//! whole back catalogue would read as news. YC companies get a signal per
//! recent batch instead.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use crm_core::id::{company_id, root_domain};
use crm_core::index::{COMPANIES, PORTFOLIOS};
use crm_core::market::{Portfolio, PortfolioKind};
use crm_core::model::{Company, Doc, Signal, SignalKind, collapse_ws, now_ts};
use crm_core::portfolio::{PortfolioStatus, RuntimePortfolio};
use meilisearch_sdk::search::SearchQuery;
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use super::{Batch, Ctx, Source};
use crate::state::fetch_by_ids;

/// The YC directory is ~10 MB; allow room for it to grow.
const MAX_JSON_BYTES: usize = 64 * 1024 * 1024;
/// A batch this recent still counts as "just raised".
const RECENT_BATCH_DAYS: i64 = 365;

/// Domains a portfolio page links to that are not portfolio companies.
const NOISE_DOMAINS: &[&str] = &[
    "twitter.com",
    "x.com",
    "linkedin.com",
    "facebook.com",
    "instagram.com",
    "youtube.com",
    "youtu.be",
    "tiktok.com",
    "threads.net",
    "medium.com",
    "substack.com",
    "github.com",
    "crunchbase.com",
    "pitchbook.com",
    "angel.co",
    "wellfound.com",
    "getro.com",
    "consider.co",
    "apple.com",
    "google.com",
    "goo.gl",
    "bit.ly",
    "spotify.com",
    "vimeo.com",
    "calendly.com",
    "typeform.com",
    "wikipedia.org",
    "techcrunch.com",
    "bloomberg.com",
    "forbes.com",
    "wsj.com",
    "nytimes.com",
    "businesswire.com",
    "prnewswire.com",
    "globenewswire.com",
    "ycombinator.com",
    "cloudflare.com",
    "framer.com",
    "framerusercontent.com",
    "gstatic.com",
    "googleapis.com",
    "webflow.io",
    "wixsite.com",
    "squarespace.com",
    // Investor back-office links: LP portals, forms, data rooms.
    "carta.com",
    "hsforms.com",
    "hs-sites.com",
    "hubspotpagebuilder.com",
    "docsend.com",
    "airtable.com",
    "jotform.com",
    "formstack.com",
    "junipersquare.com",
    "angellist.com",
];

/// Link texts that are calls to action, not company names.
const GENERIC_TEXT: &[&str] = &[
    "visit",
    "website",
    "visit website",
    "learn more",
    "read more",
    "more",
    "view",
    "view site",
    "site",
    "link",
    "open",
    "go",
];

pub struct Portfolios;

#[derive(Debug, Default, Deserialize, serde::Serialize)]
struct Cursor {
    #[serde(default)]
    pos: usize,
    /// Portfolios swept completely at least once; only these produce
    /// "added to portfolio" signals.
    #[serde(default)]
    swept: Vec<String>,
    /// Detail-page progress per `page` portfolio with a `detail_selector`.
    #[serde(default)]
    detail: BTreeMap<String, usize>,
}

/// What one portfolio read produced.
struct Read {
    companies: Vec<Company>,
    signals: Vec<Signal>,
    /// True when this read covered the whole portfolio.
    complete: bool,
}

#[async_trait]
impl Source for Portfolios {
    fn name(&self) -> &'static str {
        "portfolios"
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(30 * 60)
    }

    async fn fetch(&self, ctx: &Ctx, cursor: Option<Value>) -> Result<Batch> {
        let all = all_portfolios(ctx).await;
        if all.is_empty() {
            tracing::debug!(
                "no portfolios configured or added; the portfolios source has nothing to do"
            );
            return Ok(Batch::empty());
        }
        let mut cur: Cursor = cursor
            .and_then(|c| serde_json::from_value(c).ok())
            .unwrap_or_default();
        let slugs: Vec<String> = all.iter().map(|p| p.slug.clone()).collect();
        let read_before = ctx
            .state
            .portfolio_statuses(&slugs)
            .await
            .unwrap_or_default();
        let (order, next_pos, wrapped) =
            plan_run(&slugs, &read_before, cur.pos, ctx.config.portfolios_per_run);

        let mut docs = Vec::new();
        for idx in order {
            let pf = &all[idx];
            let mut status = PortfolioStatus {
                slug: pf.slug.clone(),
                last_read_at: now_ts(),
                ..Default::default()
            };
            match self.read(ctx, pf, &mut cur).await {
                Ok(read) => {
                    let first_sweep = !cur.swept.contains(&pf.slug);
                    let mut added = Vec::new();
                    if !first_sweep && pf.kind != PortfolioKind::Yc {
                        added = newly_added(ctx, pf, &read.companies).await?;
                    }
                    tracing::info!(
                        portfolio = %pf.slug,
                        investor = %pf.investor,
                        companies = read.companies.len(),
                        newly_added = added.len(),
                        baseline = first_sweep,
                        "portfolio read"
                    );
                    if read.complete && first_sweep {
                        cur.swept.push(pf.slug.clone());
                    }
                    status.companies = read.companies.len();
                    status.newly_added = added.len();
                    if read.companies.is_empty() {
                        status.error = Some(
                            "the page yielded no company links; it may render with JavaScript, \
                             or need a selector"
                                .into(),
                        );
                    }
                    docs.extend(read.companies.into_iter().map(Doc::Company));
                    docs.extend(read.signals.into_iter().map(Doc::Signal));
                    docs.extend(added.into_iter().map(Doc::Signal));
                }
                Err(e) => {
                    // One broken portfolio page must not stall the others.
                    let msg = format!("{e:#}");
                    tracing::warn!(portfolio = %pf.slug, error = %msg, "portfolio read failed");
                    status.error = Some(msg.chars().take(500).collect());
                }
            }
            status.baselined = cur.swept.contains(&pf.slug);
            if let Err(e) = ctx.state.set_portfolio_status(&status).await {
                tracing::warn!(portfolio = %pf.slug, error = %e, "could not record portfolio status");
            }
        }
        cur.pos = next_pos;
        let batch = Batch::new(docs, Some(serde_json::to_value(&cur)?));
        Ok(if wrapped { batch.swept() } else { batch })
    }
}

/// The configured portfolios, then every enabled one added at runtime whose
/// slug the config does not already use.
async fn all_portfolios(ctx: &Ctx) -> Vec<Portfolio> {
    let mut all = ctx.market.portfolios.clone();
    match runtime_portfolios(ctx).await {
        Ok(extra) => {
            for p in extra {
                if all.iter().any(|c| c.slug == p.slug) {
                    tracing::warn!(slug = %p.slug, "a runtime portfolio reuses a configured slug; ignoring it");
                } else {
                    all.push(p);
                }
            }
        }
        // Keep sweeping the configured ones if the runtime list is unreadable.
        Err(e) => tracing::warn!(error = %e, "could not read runtime portfolios"),
    }
    all
}

async fn runtime_portfolios(ctx: &Ctx) -> Result<Vec<Portfolio>> {
    let idx = ctx.state.client().index(PORTFOLIOS);
    let sort = ["added_at:asc"];
    let mut q = SearchQuery::new(&idx);
    q.with_query("")
        .with_filter("enabled = true")
        .with_sort(&sort)
        .with_limit(1000);
    Ok(q.execute::<RuntimePortfolio>()
        .await?
        .hits
        .into_iter()
        .map(|h| h.result.portfolio)
        .collect())
}

/// Which portfolios to read this run, the next rotation position, and
/// whether the rotation wrapped. Portfolios never read before go first, so
/// one the agent just added is swept on the next run rather than whenever
/// the rotation reaches it.
pub fn plan_run(
    slugs: &[String],
    read_before: &HashSet<String>,
    pos: usize,
    per_run: usize,
) -> (Vec<usize>, usize, bool) {
    let n = slugs.len();
    if n == 0 {
        return (Vec::new(), 0, false);
    }
    let per_run = per_run.clamp(1, n);
    let mut order: Vec<usize> = (0..n)
        .filter(|i| !read_before.contains(&slugs[*i]))
        .take(per_run)
        .collect();
    let start = pos % n;
    let mut steps = 0;
    let mut wrapped = false;
    while order.len() < per_run && steps < n {
        let idx = (start + steps) % n;
        steps += 1;
        wrapped |= idx + 1 == n;
        if !order.contains(&idx) {
            order.push(idx);
        }
    }
    (order, (start + steps) % n, wrapped)
}

impl Portfolios {
    async fn read(&self, ctx: &Ctx, pf: &Portfolio, cur: &mut Cursor) -> Result<Read> {
        match pf.kind {
            PortfolioKind::Yc => {
                let raw = ctx.http.get_text_limited(&pf.url, MAX_JSON_BYTES).await?;
                let items: Vec<Value> =
                    serde_json::from_str(&raw).context("the YC directory is not a JSON array")?;
                let now = now_ts();
                let mut companies = Vec::new();
                let mut signals = Vec::new();
                for item in &items {
                    if let Some((c, s)) = yc_company(item, pf, now) {
                        companies.push(c);
                        signals.extend(s);
                    }
                }
                Ok(Read {
                    companies,
                    signals,
                    complete: true,
                })
            }
            PortfolioKind::Json => {
                let raw = ctx.http.get_text_limited(&pf.url, MAX_JSON_BYTES).await?;
                let doc: Value = serde_json::from_str(&raw).context("not JSON")?;
                Ok(Read {
                    companies: json_companies(&doc, pf)?,
                    signals: Vec::new(),
                    complete: true,
                })
            }
            PortfolioKind::Page => self.read_page(ctx, pf, cur).await,
        }
    }

    async fn read_page(&self, ctx: &Ctx, pf: &Portfolio, cur: &mut Cursor) -> Result<Read> {
        if !ctx.http.robots_allow(&pf.url).await {
            bail!("robots.txt disallows {}", pf.url);
        }
        let (final_url, html) = ctx.http.get_page(&pf.url).await?;
        let base = Url::parse(&final_url)?;

        let Some(detail_sel) = pf.detail_selector.as_deref() else {
            let found = page_companies(&html, &base, pf.selector.as_deref())?;
            return Ok(Read {
                companies: found.into_iter().map(|f| f.into_company(pf)).collect(),
                signals: Vec::new(),
                complete: true,
            });
        };

        // The grid links to the investor's own page per company: visit a
        // slice of those per run and take each one's outbound link.
        let details = detail_links(&html, &base, detail_sel)?;
        if details.is_empty() {
            bail!(
                "no links on {} match detail_selector {detail_sel:?}; the page may render with JavaScript",
                pf.url
            );
        }
        let offset = cur
            .detail
            .get(&pf.slug)
            .copied()
            .unwrap_or(0)
            .min(details.len());
        let end = (offset + ctx.config.portfolio_detail_per_run.max(1)).min(details.len());
        let mut companies = Vec::new();
        for url in &details[offset..end] {
            if !ctx.http.robots_allow(url).await {
                continue;
            }
            let (final_url, page) = match ctx.http.get_page(url).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(url, error = %e, "portfolio detail page failed");
                    continue;
                }
            };
            let Ok(page_url) = Url::parse(&final_url) else {
                continue;
            };
            if let Some(f) = detail_company(&page, &page_url) {
                companies.push(f.into_company(pf));
            }
        }
        let complete = end >= details.len();
        cur.detail
            .insert(pf.slug.clone(), if complete { 0 } else { end });
        Ok(Read {
            companies,
            signals: Vec::new(),
            complete,
        })
    }
}

/// Signals for companies this portfolio lists that were not already
/// recorded as backed by this investor.
async fn newly_added(ctx: &Ctx, pf: &Portfolio, companies: &[Company]) -> Result<Vec<Signal>> {
    #[derive(Deserialize)]
    struct Row {
        id: String,
        #[serde(default)]
        investors: Vec<String>,
    }
    let ids: Vec<String> = companies.iter().map(|c| c.id.clone()).collect();
    let known: HashMap<String, Vec<String>> = fetch_by_ids::<Row>(
        ctx.state.client(),
        COMPANIES,
        &ids,
        Some(&["id", "investors"]),
    )
    .await?
    .into_iter()
    .map(|r| (r.id, r.investors))
    .collect();
    let now = now_ts();
    Ok(companies
        .iter()
        .filter(|c| {
            known
                .get(&c.id)
                .is_none_or(|inv| !inv.iter().any(|i| i.eq_ignore_ascii_case(&pf.investor)))
        })
        .map(|c| {
            let mut s = Signal::new(
                &c.id,
                SignalKind::Funding,
                "portfolios",
                format!("{}:{}", pf.slug, c.domain.as_deref().unwrap_or(&c.id)),
                format!("Added to the {} portfolio", pf.investor),
                now,
            );
            s.url = Some(pf.url.clone());
            s.summary = format!(
                "{} newly appears in {}'s published portfolio, which usually follows a new investment.",
                c.name, pf.investor
            );
            s
        })
        .collect())
}

// ---------------------------------------------------------------- YC

/// One YC directory entry → a company, plus a funding signal when its
/// batch is recent. `None` for companies filtered out or without a site.
pub fn yc_company(item: &Value, pf: &Portfolio, now: i64) -> Option<(Company, Option<Signal>)> {
    let s = |k: &str| {
        item.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let status = s("status").unwrap_or("Active");
    let keep: Vec<&str> = if pf.statuses.is_empty() {
        vec!["Active", "Public"]
    } else {
        pf.statuses.iter().map(String::as_str).collect()
    };
    if !keep.iter().any(|k| k.eq_ignore_ascii_case(status)) {
        return None;
    }
    let batch = s("batch");
    let batch_start = batch.and_then(batch_start);
    if let (Some(min), Some(start)) = (pf.since_year, batch_start)
        && year_of(start) < min
    {
        return None;
    }
    let website = s("website")?;
    let domain = root_domain(website)?;
    let id = company_id(Some(&domain), None, None)?;

    let mut c = Company::new(&id, s("name")?, "portfolios");
    c.domain = Some(domain);
    c.website = Some(website.to_string());
    c.description = [s("one_liner"), s("long_description")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(". ");
    c.employees = item
        .get("team_size")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0);
    for key in ["industries", "tags"] {
        if let Some(a) = item.get(key).and_then(Value::as_array) {
            c.industries
                .extend(a.iter().filter_map(Value::as_str).map(str::to_string));
        }
    }
    if let Some(loc) = s("all_locations") {
        let (city, state, country) = parse_location(loc);
        c.hq_city = city;
        c.hq_state = state;
        c.hq_country = country;
    }
    c.cohort = batch.map(str::to_string);
    c.stage = s("stage").map(str::to_string);
    c.investors = vec![pf.investor.clone()];

    let signal = match (batch, batch_start) {
        (Some(b), Some(start)) if now - start <= RECENT_BATCH_DAYS * 86_400 && start <= now => {
            let mut sig = Signal::new(
                &id,
                SignalKind::Funding,
                "portfolios",
                format!("{}:{}:{b}", pf.slug, s("slug").unwrap_or(&id)),
                format!("Backed by {} ({b})", pf.investor),
                start,
            );
            sig.url = s("url").map(str::to_string);
            sig.summary = s("one_liner").unwrap_or_default().to_string();
            Some(sig)
        }
        _ => None,
    };
    Some((c, signal))
}

/// When a YC batch starts: "Winter 2025" → 2025-01-01, "Spring" → April,
/// "Summer" → June, "Fall" → September.
pub fn batch_start(batch: &str) -> Option<i64> {
    let mut parts = batch.split_whitespace();
    let season = parts.next()?.to_ascii_lowercase();
    let year: i32 = parts.next()?.parse().ok()?;
    let month = match season.as_str() {
        "winter" => 1,
        "spring" => 4,
        "summer" => 6,
        "fall" | "autumn" => 9,
        _ => return None,
    };
    chrono::NaiveDate::from_ymd_opt(year, month, 1)?
        .and_hms_opt(0, 0, 0)
        .map(|d| d.and_utc().timestamp())
}

fn year_of(ts: i64) -> i32 {
    use chrono::Datelike;
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.year())
        .unwrap_or(0)
}

/// "San Francisco, CA, USA; New York, NY, USA" → the first location, as
/// (city, state, ISO country).
pub fn parse_location(raw: &str) -> (Option<String>, Option<String>, Option<String>) {
    let first = raw.split(';').next().unwrap_or("").trim();
    let parts: Vec<&str> = first
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let own = |s: &str| Some(s.to_string());
    match parts.as_slice() {
        [] => (None, None, None),
        [country] => (None, None, Some(country_code(country))),
        [city, country] => (own(city), None, Some(country_code(country))),
        [city, state, .., country] => (own(city), own(state), Some(country_code(country))),
    }
}

fn country_code(name: &str) -> String {
    let n = name.trim();
    let code = match n.to_ascii_lowercase().as_str() {
        "usa" | "us" | "united states" | "united states of america" => "US",
        "canada" => "CA",
        "united kingdom" | "uk" | "england" | "scotland" | "wales" => "GB",
        "india" => "IN",
        "germany" => "DE",
        "france" => "FR",
        "spain" => "ES",
        "netherlands" => "NL",
        "mexico" => "MX",
        "brazil" => "BR",
        "colombia" => "CO",
        "argentina" => "AR",
        "chile" => "CL",
        "nigeria" => "NG",
        "kenya" => "KE",
        "singapore" => "SG",
        "israel" => "IL",
        "australia" => "AU",
        "japan" => "JP",
        "south korea" => "KR",
        "indonesia" => "ID",
        "pakistan" => "PK",
        "sweden" => "SE",
        "switzerland" => "CH",
        "ireland" => "IE",
        _ if n.len() == 2 => return n.to_ascii_uppercase(),
        _ => return n.to_string(),
    };
    code.to_string()
}

// -------------------------------------------------------------- JSON

pub fn json_companies(doc: &Value, pf: &Portfolio) -> Result<Vec<Company>> {
    let items = match pf.items.as_deref().filter(|p| !p.is_empty()) {
        Some(ptr) => doc
            .pointer(ptr)
            .with_context(|| format!("no {ptr} in the portfolio JSON"))?,
        None => doc,
    };
    let items = items
        .as_array()
        .context("the portfolio JSON items are not an array")?;
    let name_ptr = pf.name_field.as_deref().unwrap_or("/name");
    let site_ptr = pf.website_field.as_deref().unwrap_or("/website");
    Ok(items
        .iter()
        .filter_map(|item| {
            let site = item.pointer(site_ptr)?.as_str()?;
            let name = item.pointer(name_ptr).and_then(Value::as_str);
            Found::new(name, site).map(|f| f.into_company(pf))
        })
        .collect())
}

// -------------------------------------------------------------- pages

/// A company found on a portfolio page.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub name: Option<String>,
    pub website: String,
    pub domain: String,
}

impl Found {
    fn new(name: Option<&str>, website: &str) -> Option<Self> {
        let domain = root_domain(website)?;
        if NOISE_DOMAINS.contains(&domain.as_str()) {
            return None;
        }
        Some(Self {
            name: name.and_then(clean_name),
            website: website.to_string(),
            domain,
        })
    }

    fn into_company(self, pf: &Portfolio) -> Company {
        let id = company_id(Some(&self.domain), None, None).unwrap_or_default();
        let mut c = Company::new(id, self.name.clone().unwrap_or_default(), "portfolios");
        if self.name.is_none() {
            // A logo link says whose it is only through the domain; let a
            // better source name it.
            c.name_source = String::new();
        }
        c.domain = Some(self.domain);
        c.website = Some(self.website);
        c.investors = vec![pf.investor.clone()];
        c
    }
}

/// Words in logo file names and alt texts that are not part of the name.
const FILLER: &[&str] = &[
    "logo", "logos", "img", "image", "icon", "photo", "png", "jpg", "jpeg", "svg", "webp", "copy",
    "final", "white", "black", "color", "colour", "dark", "light", "mark", "wordmark",
];

/// Design-tool export names: "Frame 111", "Group 42".
const EXPORT_WORDS: &[&str] = &[
    "frame",
    "group",
    "rectangle",
    "layer",
    "vector",
    "image",
    "artboard",
];

/// A company name from link text, an image's alt or a label — or `None`
/// when what is there is a call to action or a file name rather than a
/// name. Logos are often uploaded as `DV-Portfolio-Acme-Co.png` or with a
/// hash for a name, and those alt texts must not become company names.
fn clean_name(raw: &str) -> Option<String> {
    let mut n = collapse_ws(raw);
    // "DV-Portfolio-Caliber-Mind" → "Caliber Mind"; "acme_logo" → "acme".
    let lower_raw = n.to_lowercase();
    let file_like = !n.contains(' ')
        || ["portfolio-", "portfolio_", "logo-", "logo_"]
            .iter()
            .any(|m| lower_raw.contains(m));
    if file_like && (n.contains('-') || n.contains('_')) {
        n = n
            .split(['-', '_'])
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
    }
    let mut words: Vec<&str> = n.split_whitespace().collect();
    if let Some(i) = words
        .iter()
        .position(|w| w.eq_ignore_ascii_case("portfolio"))
    {
        words.drain(..=i);
    }
    words.retain(|w| !FILLER.contains(&w.to_lowercase().trim_matches('.')));
    let n = words.join(" ");

    let lower = n.to_lowercase();
    let trimmed = lower.trim_matches(|c: char| !c.is_alphanumeric());
    let count = n.chars().count();
    if !(2..=80).contains(&count) || GENERIC_TEXT.contains(&trimmed) {
        return None;
    }
    // Ids: long digit runs, hashes and UUIDs.
    let longest_digits = n
        .split(|c: char| !c.is_ascii_digit())
        .map(str::len)
        .max()
        .unwrap_or(0);
    let hexish = n.len() >= 16 && n.chars().all(|c| c.is_ascii_hexdigit() || c == ' ');
    if longest_digits >= 5 || hexish {
        return None;
    }
    if words.len() == 2
        && EXPORT_WORDS.contains(&words[0].to_lowercase().as_str())
        && words[1].chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    // "peak energy" → "Peak Energy"; leave deliberate casing alone.
    if n.chars().all(|c| !c.is_uppercase()) {
        return Some(
            n.split(' ')
                .map(|w| {
                    let mut c = w.chars();
                    c.next()
                        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    Some(n)
}

fn selector(css: &str) -> Result<Selector> {
    Selector::parse(css).map_err(|e| anyhow::anyhow!("bad CSS selector {css:?}: {e}"))
}

/// Outbound company links on a portfolio page, one per domain.
pub fn page_companies(html: &str, base: &Url, scope: Option<&str>) -> Result<Vec<Found>> {
    let doc = Html::parse_document(html);
    let sel = selector(scope.unwrap_or("a[href]"))?;
    let own = root_domain(base.as_str());
    let mut by_domain: BTreeMap<String, Found> = BTreeMap::new();
    for el in doc.select(&sel) {
        // A scope selector may match a card rather than the link itself.
        let links: Vec<ElementRef> = if el.value().name() == "a" {
            vec![el]
        } else {
            el.select(&selector("a[href]")?).collect()
        };
        for a in links {
            let Some(href) = a.value().attr("href") else {
                continue;
            };
            let Ok(url) = base.join(href) else { continue };
            if !matches!(url.scheme(), "http" | "https") {
                continue;
            }
            let name = link_name(&a).or_else(|| (el != a).then(|| link_name(&el)).flatten());
            let Some(f) = Found::new(name.as_deref(), url.as_str()) else {
                continue;
            };
            if Some(&f.domain) == own.as_ref() {
                continue;
            }
            match by_domain.get_mut(&f.domain) {
                Some(existing) if existing.name.is_none() => existing.name = f.name,
                Some(_) => {}
                None => {
                    by_domain.insert(f.domain.clone(), f);
                }
            }
        }
    }
    Ok(by_domain.into_values().collect())
}

/// The best name a link offers: its text, an image's alt, or its labels.
fn link_name(el: &ElementRef) -> Option<String> {
    let text: String = el.text().collect::<Vec<_>>().join(" ");
    if let Some(n) = clean_name(&text) {
        return Some(n);
    }
    let img = Selector::parse("img[alt]").ok()?;
    if let Some(alt) = el
        .select(&img)
        .filter_map(|i| i.value().attr("alt"))
        .find_map(|a| clean_name(a.trim_end_matches(" logo").trim_end_matches(" Logo")))
    {
        return Some(alt);
    }
    ["aria-label", "title"]
        .iter()
        .find_map(|k| el.value().attr(k).and_then(clean_name))
}

/// The investor's own per-company pages, in page order, deduplicated.
pub fn detail_links(html: &str, base: &Url, css: &str) -> Result<Vec<String>> {
    let doc = Html::parse_document(html);
    let sel = selector(css)?;
    let a_sel = selector("a[href]")?;
    let own = root_domain(base.as_str());
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for el in doc.select(&sel) {
        let links: Vec<ElementRef> = if el.value().name() == "a" {
            vec![el]
        } else {
            el.select(&a_sel).collect()
        };
        for a in links {
            let Some(href) = a.value().attr("href") else {
                continue;
            };
            let Ok(mut url) = base.join(href) else {
                continue;
            };
            url.set_fragment(None);
            if root_domain(url.as_str()) == own && seen.insert(url.to_string()) {
                out.push(url.to_string());
            }
        }
    }
    Ok(out)
}

/// A company from the investor's page about it: the first outbound,
/// non-noise link in the main content, named by the page's heading.
pub fn detail_company(html: &str, page: &Url) -> Option<Found> {
    let doc = Html::parse_document(html);
    let own = root_domain(page.as_str());
    // The heading, else the page title minus the investor's suffix:
    // "Airbnb | Sequoia Capital" → "Airbnb".
    let name = Selector::parse("h1")
        .ok()
        .and_then(|h| doc.select(&h).next())
        .map(|h| h.text().collect::<String>())
        .filter(|n| clean_name(n).is_some())
        .or_else(|| {
            let t = Selector::parse("title").ok()?;
            let title: String = doc.select(&t).next()?.text().collect();
            let first = title
                .split(['|', '–', '—', '·'])
                .next()?
                .split(" - ")
                .next()?;
            Some(first.trim().to_string())
        });
    for scope in ["main a[href]", "article a[href]", "a[href]"] {
        let sel = Selector::parse(scope).ok()?;
        for a in doc.select(&sel) {
            let Some(href) = a.value().attr("href") else {
                continue;
            };
            let Ok(url) = page.join(href) else { continue };
            if !matches!(url.scheme(), "http" | "https") {
                continue;
            }
            if let Some(f) = Found::new(name.as_deref(), url.as_str())
                && Some(&f.domain) != own.as_ref()
            {
                return Some(f);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pf(kind: PortfolioKind) -> Portfolio {
        Portfolio {
            slug: "t".into(),
            investor: "Test Ventures".into(),
            kind,
            url: "https://testvc.example/portfolio".into(),
            selector: None,
            detail_selector: None,
            items: None,
            name_field: None,
            website_field: None,
            statuses: vec![],
            since_year: None,
        }
    }

    fn yc_item(batch: &str, status: &str) -> Value {
        json!({
            "name": "CircuitHub", "slug": "circuithub", "website": "https://www.circuithub.com",
            "all_locations": "San Francisco, CA, USA; London, England, United Kingdom",
            "one_liner": "On-demand electronics manufacturing",
            "long_description": "Robotic factory.", "team_size": 58,
            "industries": ["Industrials", "Manufacturing and Robotics"], "tags": ["Hardware"],
            "batch": batch, "status": status, "stage": "Early",
            "url": "https://www.ycombinator.com/companies/circuithub"
        })
    }

    #[test]
    fn yc_entries_become_companies_with_investor_and_cohort() {
        let now = batch_start("Summer 2026").unwrap() + 30 * 86_400;
        let (c, sig) = yc_company(
            &yc_item("Summer 2026", "Active"),
            &pf(PortfolioKind::Yc),
            now,
        )
        .unwrap();
        assert_eq!(c.domain.as_deref(), Some("circuithub.com"));
        assert_eq!(
            c.id,
            company_id(Some("circuithub.com"), None, None).unwrap()
        );
        assert_eq!(c.employees, Some(58));
        assert_eq!(c.hq_city.as_deref(), Some("San Francisco"));
        assert_eq!(c.hq_state.as_deref(), Some("CA"));
        assert_eq!(c.hq_country.as_deref(), Some("US"));
        assert_eq!(c.investors, vec!["Test Ventures"]);
        assert_eq!(c.cohort.as_deref(), Some("Summer 2026"));
        assert!(c.industries.contains(&"Hardware".to_string()));
        assert!(
            c.description
                .starts_with("On-demand electronics manufacturing. Robotic")
        );
        let sig = sig.expect("a recent batch is a funding signal");
        assert_eq!(sig.kind, SignalKind::Funding);
        assert_eq!(sig.occurred_at, batch_start("Summer 2026").unwrap());
    }

    #[test]
    fn yc_filters_on_status_and_year_and_old_batches_are_not_news() {
        let now = batch_start("Summer 2026").unwrap();
        let p = pf(PortfolioKind::Yc);
        assert!(yc_company(&yc_item("Winter 2012", "Inactive"), &p, now).is_none());
        let (_, sig) = yc_company(&yc_item("Winter 2012", "Active"), &p, now).unwrap();
        assert!(sig.is_none());
        let mut recent_only = pf(PortfolioKind::Yc);
        recent_only.since_year = Some(2020);
        assert!(yc_company(&yc_item("Winter 2012", "Active"), &recent_only, now).is_none());
        let mut no_site = yc_item("Winter 2012", "Active");
        no_site["website"] = json!("");
        assert!(yc_company(&no_site, &p, now).is_none());
    }

    #[test]
    fn unread_portfolios_jump_the_rotation() {
        let slugs: Vec<String> = ["a", "b", "c", "d", "new"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let read: HashSet<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let (order, next, wrapped) = plan_run(&slugs, &read, 1, 3);
        assert_eq!(
            order,
            vec![4, 1, 2],
            "the unread one first, then the rotation"
        );
        assert_eq!(next, 3);
        assert!(!wrapped);

        let all: HashSet<String> = slugs.iter().cloned().collect();
        let (order, next, wrapped) = plan_run(&slugs, &all, 4, 3);
        assert_eq!(order, vec![4, 0, 1]);
        assert_eq!(next, 2);
        assert!(wrapped);

        assert_eq!(plan_run(&slugs[..1], &all, 7, 3).0, vec![0]);
        assert!(plan_run(&[], &all, 0, 3).0.is_empty());
    }

    #[test]
    fn batches_and_locations_parse() {
        assert!(batch_start("Fall 2024").unwrap() > batch_start("Summer 2024").unwrap());
        assert!(batch_start("IK12").is_none());
        assert_eq!(
            parse_location("Berlin, Germany"),
            (Some("Berlin".into()), None, Some("DE".into()))
        );
        assert_eq!(parse_location(""), (None, None, None));
    }

    const PAGE: &str = r#"<html><body>
      <nav><a href="https://twitter.com/testvc">Twitter</a><a href="/about">About</a></nav>
      <div class="portfolio">
        <a href="https://www.acme.test/?ref=testvc">Acme</a>
        <a href="https://acme.test/careers">Visit website</a>
        <div class="card"><a href="https://beta.test"><img src="b.png" alt="Beta Robotics logo"></a></div>
        <a href="https://gamma.test" aria-label="Gamma"></a>
        <a href="https://linkedin.com/company/acme">in</a>
      </div>
      <footer><a href="https://other.test">Our fund admin</a></footer>
    </body></html>"#;

    #[test]
    fn a_portfolio_page_yields_one_named_company_per_domain() {
        let base = Url::parse("https://testvc.example/portfolio").unwrap();
        let found = page_companies(PAGE, &base, Some(".portfolio a")).unwrap();
        let names: Vec<(String, Option<String>)> = found
            .iter()
            .map(|f| (f.domain.clone(), f.name.clone()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("acme.test".into(), Some("Acme".into())),
                ("beta.test".into(), Some("Beta Robotics".into())),
                ("gamma.test".into(), Some("Gamma".into())),
            ]
        );
        // Unscoped, the footer link counts too, but social links never do.
        let all = page_companies(PAGE, &base, None).unwrap();
        assert!(all.iter().any(|f| f.domain == "other.test"));
        assert!(!all.iter().any(|f| f.domain == "twitter.com"));
    }

    #[test]
    fn detail_pages_are_followed_for_the_company_link() {
        let base = Url::parse("https://testvc.example/portfolio").unwrap();
        let grid = r#"<div class="grid"><a href="/companies/acme">Acme</a><a href="/companies/beta">Beta</a>
                      <a href="/companies/acme#x">dup</a></div>"#;
        let links = detail_links(grid, &base, ".grid a").unwrap();
        assert_eq!(
            links,
            vec![
                "https://testvc.example/companies/acme",
                "https://testvc.example/companies/beta"
            ]
        );
        let titled = r#"<html><head><title>Beta | Test Ventures</title></head>
            <body><a href="https://beta.test">site</a></body></html>"#;
        let f = detail_company(titled, &Url::parse(&links[1]).unwrap()).unwrap();
        assert_eq!(f.name.as_deref(), Some("Beta"));
        let page = r#"<html><body><header><a href="https://twitter.com/x">t</a></header>
            <main><h1>Acme Inc</h1><p>Invested 2025.</p><a href="https://acme.test">acme.test</a></main></body></html>"#;
        let f = detail_company(page, &Url::parse(&links[0]).unwrap()).unwrap();
        assert_eq!(f.domain, "acme.test");
        assert_eq!(f.name.as_deref(), Some("Acme Inc"));
    }

    #[test]
    fn json_portfolios_read_with_pointers() {
        let mut p = pf(PortfolioKind::Json);
        p.items = Some("/data/companies".into());
        p.name_field = Some("/title".into());
        p.website_field = Some("/links/site".into());
        let doc = json!({"data": {"companies": [
            {"title": "Acme", "links": {"site": "https://acme.test"}},
            {"title": "No site"}
        ]}});
        let cs = json_companies(&doc, &p).unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].name, "Acme");
        assert_eq!(cs[0].investors, vec!["Test Ventures"]);
        p.items = Some("/missing".into());
        assert!(json_companies(&doc, &p).is_err());
    }

    #[test]
    fn logo_file_names_are_cleaned_or_rejected() {
        assert_eq!(
            clean_name("DV-Portfolio-Caliber-Mind").as_deref(),
            Some("Caliber Mind")
        );
        assert_eq!(clean_name("DV-Portfolio-BOOM").as_deref(), Some("BOOM"));
        assert_eq!(
            clean_name("DV-Portfolio-Crop Diagnostix").as_deref(),
            Some("Crop Diagnostix")
        );
        assert_eq!(clean_name("Coca-Cola").as_deref(), Some("Coca Cola"));
        assert_eq!(
            clean_name("Rolls-Royce Holdings").as_deref(),
            Some("Rolls-Royce Holdings")
        );
        assert_eq!(clean_name("acme_logo_white").as_deref(), Some("Acme"));
        assert_eq!(clean_name("peak energy").as_deref(), Some("Peak Energy"));
        assert_eq!(clean_name("Eleven labs").as_deref(), Some("Eleven labs"));
        assert_eq!(clean_name("22efa393-87a3-4632-830f-e6d89b6f2337"), None);
        assert_eq!(clean_name("5321458703472988073"), None);
        assert_eq!(clean_name("photo_5422780761553106618_y"), None);
        assert_eq!(clean_name("Frame 111"), None);
        assert_eq!(clean_name("Learn more"), None);
    }

    #[test]
    fn nameless_logo_links_leave_the_name_to_others() {
        let f = Found::new(None, "https://acme.test").unwrap();
        let c = f.into_company(&pf(PortfolioKind::Page));
        assert!(c.name.is_empty());
        assert_eq!(c.name_source, "");
    }
}
