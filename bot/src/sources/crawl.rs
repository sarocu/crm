//! A polite crawler for company websites.
//!
//! Every company another source finds with a domain gets its homepage put
//! on the frontier. From there the crawl follows only links that look like
//! they lead somewhere a salesperson cares about — about, careers, news,
//! products — and reads each page for:
//!
//! - schema.org `Organization` JSON-LD (name, address, headcount, founding),
//! - the company's applicant tracking system, from careers links, which is
//!   what lets the `jobs` source poll its job board,
//! - tech and need keywords in the page text,
//! - dated press releases, which become signals.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use crm_core::id::{company_id, root_domain};
use crm_core::index::COMPANIES;
use crm_core::model::{Company, Doc, Signal, collapse_ws, now_ts};
use meilisearch_sdk::search::{SearchQuery, Selectors};
use regex::Regex;
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use super::{Batch, Ctx, Source, cursor_usize};
use crate::classify;
use crate::state::FrontierEntry;

/// File extensions that are never worth fetching as HTML.
const SKIP_EXTENSIONS: &[&str] = &[
    ".pdf", ".jpg", ".jpeg", ".png", ".gif", ".svg", ".webp", ".ico", ".css", ".js", ".json",
    ".xml", ".zip", ".gz", ".mp3", ".mp4", ".mov", ".avi", ".doc", ".docx", ".xls", ".xlsx",
    ".ppt", ".pptx", ".rss", ".woff", ".woff2", ".ttf",
];

/// Path segments worth following off a homepage.
const INTERESTING: &[&str] = &[
    "about",
    "company",
    "who-we-are",
    "our-story",
    "careers",
    "career",
    "jobs",
    "join",
    "work-with-us",
    "hiring",
    "news",
    "press",
    "newsroom",
    "media",
    "blog",
    "locations",
    "products",
    "solutions",
    "services",
    "customers",
    "partners",
];

/// Press releases older than this are history, not signals.
const NEWS_MAX_AGE_DAYS: i64 = 365;

static LD_JSON: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"script[type="application/ld+json"]"#).unwrap());
static LINKS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a[href], iframe[src]").unwrap());
static TITLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("title").unwrap());
static META: LazyLock<Selector> = LazyLock::new(|| Selector::parse("meta").unwrap());
static MAIN: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("main, article, [role=main]").unwrap());
static BODY: LazyLock<Selector> = LazyLock::new(|| Selector::parse("body").unwrap());

/// Public job-board URLs, with the board's slug captured.
static ATS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    vec![
        (
            "greenhouse",
            Regex::new(r"(?i)(?:boards|job-boards)(?:\.eu)?\.greenhouse\.io/(?:embed/job_board\?for=)?([a-z0-9_-]+)").unwrap(),
        ),
        (
            "greenhouse",
            Regex::new(r"(?i)boards-api\.greenhouse\.io/v1/boards/([a-z0-9_-]+)").unwrap(),
        ),
        ("lever", Regex::new(r"(?i)jobs\.lever\.co/([a-z0-9_.-]+)").unwrap()),
        (
            "ashby",
            Regex::new(r"(?i)jobs\.ashbyhq\.com/([a-z0-9_.%-]+)").unwrap(),
        ),
    ]
});

/// Slugs that are paths on the ATS host, not a company's board.
const ATS_NOT_SLUGS: &[&str] = &["embed", "v1", "api", "static", "assets", "jobs"];

pub struct Crawl;

#[async_trait]
impl Source for Crawl {
    fn name(&self) -> &'static str {
        "crawl"
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(10 * 60)
    }

    async fn fetch(&self, ctx: &Ctx, cursor: Option<Value>) -> Result<Batch> {
        let next_cursor = self.seed(ctx, &cursor).await?;

        let claimed = ctx
            .state
            .frontier_claim(ctx.config.crawl_pages_per_run)
            .await?;
        if claimed.is_empty() {
            // Everything known has been visited; start the cycle again so
            // changed sites are eventually re-read.
            let n = ctx.state.frontier_reset_roots().await?;
            tracing::info!(
                homepages = n,
                "crawl frontier is empty; re-queued homepages for another pass"
            );
            return Ok(Batch::new(Vec::new(), Some(next_cursor)).swept());
        }

        let mut docs = Vec::new();
        for entry in &claimed {
            match self.visit(ctx, entry).await {
                Ok(mut produced) => {
                    docs.append(&mut produced);
                    let _ = ctx.state.frontier_finish(&entry.url, "done").await;
                }
                Err(e) => {
                    tracing::warn!(url = %entry.url, error = %e, "crawl page failed");
                    let _ = ctx.state.frontier_finish(&entry.url, "error").await;
                }
            }
        }

        let pending = ctx.state.frontier_pending().await;
        tracing::info!(
            visited = claimed.len(),
            produced = docs.len(),
            pending,
            "crawl cycle complete"
        );
        Ok(Batch::new(docs, Some(next_cursor)))
    }
}

#[derive(Debug, Deserialize)]
struct Homepage {
    id: String,
    #[serde(default)]
    website: Option<String>,
    #[serde(default)]
    domain: Option<String>,
}

impl Crawl {
    /// Queue the configured seeds, plus the next page of known companies'
    /// homepages. Rows already on the frontier are left alone, so this is
    /// safe to call on every run. Returns the cursor for the next page.
    async fn seed(&self, ctx: &Ctx, cursor: &Option<Value>) -> Result<Value> {
        let mut entries: Vec<FrontierEntry> = ctx
            .config
            .crawl_seeds
            .iter()
            .map(|u| FrontierEntry {
                url: u.to_string(),
                depth: 0,
                company_id: root_domain(u.as_str()).and_then(|d| company_id(Some(&d), None, None)),
            })
            .collect();

        let offset = cursor_usize(cursor, "offset");
        let idx = ctx.state.client().index(COMPANIES);
        let fields = ["id", "website", "domain"];
        let sort = ["name:asc"];
        let mut q = SearchQuery::new(&idx);
        q.with_query("")
            .with_filter("domain EXISTS AND merged_into NOT EXISTS")
            .with_sort(&sort)
            .with_limit(ctx.config.crawl_companies_per_run)
            .with_offset(offset)
            .with_attributes_to_retrieve(Selectors::Some(&fields));
        let page = q.execute::<Homepage>().await?;
        let got = page.hits.len();
        for h in page.hits {
            let c = h.result;
            let url = c
                .website
                .filter(|w| w.starts_with("http"))
                .or_else(|| c.domain.map(|d| format!("https://{d}/")));
            if let Some(url) = url {
                entries.push(FrontierEntry {
                    url,
                    depth: 0,
                    company_id: Some(c.id),
                });
            }
        }
        let added = ctx.state.frontier_add(&entries).await?;
        if added > 0 {
            tracing::info!(added, "queued company homepages");
        }
        // Meilisearch caps offset+limit at 1000 by default; wrap before it.
        let next = offset + got;
        let next = if got < ctx.config.crawl_companies_per_run || next >= 1000 {
            0
        } else {
            next
        };
        Ok(json!({ "offset": next }))
    }

    async fn visit(&self, ctx: &Ctx, entry: &FrontierEntry) -> Result<Vec<Doc>> {
        if !ctx.http.robots_allow(&entry.url).await {
            tracing::debug!(url = %entry.url, "robots.txt disallows this page");
            return Ok(Vec::new());
        }
        let (final_url, html) = ctx.http.get_page(&entry.url).await?;
        let base = Url::parse(&final_url)?;
        let page = read_page(&html, &base);

        if entry.depth < ctx.config.crawl_max_depth {
            let next: Vec<FrontierEntry> = page
                .links
                .iter()
                .filter(|l| is_interesting(l))
                .map(|url| FrontierEntry {
                    url: url.clone(),
                    depth: entry.depth + 1,
                    company_id: entry.company_id.clone(),
                })
                .collect();
            if let Err(e) = ctx.state.frontier_add(&next).await {
                tracing::warn!(error = %e, "could not extend the frontier");
            }
        }

        let Some(domain) = root_domain(base.as_str()) else {
            return Ok(Vec::new());
        };
        let id = entry
            .company_id
            .clone()
            .or_else(|| company_id(Some(&domain), None, None))
            .unwrap_or_default();
        Ok(extract(ctx, &page, &base, &domain, &id, entry.depth))
    }
}

fn is_interesting(url: &str) -> bool {
    let Ok(u) = Url::parse(url) else {
        return false;
    };
    let path = u.path().to_ascii_lowercase();
    path.split('/')
        .filter(|s| !s.is_empty())
        .take(2)
        .any(|seg| {
            INTERESTING
                .iter()
                .any(|i| seg == *i || seg.starts_with(&format!("{i}-")))
        })
}

// ------------------------------------------------------------- extraction

/// Everything worth having from one page, read out in a single pass.
///
/// `scraper::Html` is not `Send`, and this runs inside an async task, so the
/// document is parsed, drained and dropped before any `await` happens.
#[derive(Debug, Default)]
pub struct PageFacts {
    pub nodes: Vec<Value>,
    pub site_name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub text: String,
    /// Same-host links, absolute and de-fragmented.
    pub links: Vec<String>,
    /// ATS board found anywhere on the page: (provider, slug).
    pub ats: Option<(String, String)>,
}

pub fn read_page(html: &str, base: &Url) -> PageFacts {
    let doc = Html::parse_document(html);
    let mut nodes = Vec::new();
    for script in doc.select(&LD_JSON) {
        let raw: String = script.text().collect();
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            flatten_ld(value, &mut nodes);
        }
    }

    let mut seen = HashSet::new();
    let mut links = Vec::new();
    let mut ats = None;
    for el in doc.select(&LINKS) {
        let Some(href) = el.value().attr("href").or_else(|| el.value().attr("src")) else {
            continue;
        };
        let Ok(mut url) = base.join(href) else {
            continue;
        };
        if ats.is_none() {
            ats = detect_ats(url.as_str());
        }
        if !matches!(url.scheme(), "http" | "https") || url.host_str() != base.host_str() {
            continue;
        }
        url.set_fragment(None);
        let path = url.path().to_lowercase();
        if SKIP_EXTENSIONS.iter().any(|e| path.ends_with(e)) {
            continue;
        }
        let s = url.to_string();
        if seen.insert(s.clone()) {
            links.push(s);
        }
    }
    // Embedded boards are often loaded by script rather than linked.
    if ats.is_none() {
        ats = detect_ats(html);
    }

    PageFacts {
        nodes,
        site_name: meta_content(&doc, &["og:site_name", "application-name"]),
        title: doc
            .select(&TITLE)
            .next()
            .map(|t| collapse_ws(&t.text().collect::<String>()))
            .filter(|t| !t.is_empty()),
        description: meta_content(&doc, &["og:description", "description"]),
        text: page_text(&doc),
        links,
        ats,
    }
}

pub fn detect_ats(text: &str) -> Option<(String, String)> {
    for (provider, re) in ATS.iter() {
        for cap in re.captures_iter(text) {
            let slug = cap[1].trim_end_matches(['.', '-']).to_ascii_lowercase();
            if !slug.is_empty() && !ATS_NOT_SLUGS.contains(&slug.as_str()) {
                return Some((provider.to_string(), slug));
            }
        }
    }
    None
}

/// Turn one page into a company patch and any press-release signals.
fn extract(
    ctx: &Ctx,
    page: &PageFacts,
    base: &Url,
    domain: &str,
    id: &str,
    depth: u32,
) -> Vec<Doc> {
    let mut out = Vec::new();
    let org = page.nodes.iter().find(|n| is_org(&ld_types(n)));

    let is_home = depth == 0 || matches!(base.path(), "" | "/");
    let is_about = base.path().to_ascii_lowercase().contains("about");
    let name = org
        .and_then(|n| ld_str(n, "name"))
        .or_else(|| page.site_name.clone())
        .or_else(|| {
            is_home
                .then(|| page.title.as_deref().map(clean_title))
                .flatten()
        })
        .filter(|n| !n.is_empty());

    let mut c = Company::new(id, name.clone().unwrap_or_default(), "crawl");
    if name.is_none() {
        // Contribute facts, but never a name, from pages that do not say
        // whose they are.
        c.name_source = String::new();
    }
    c.domain = Some(domain.to_string());
    if is_home {
        c.website = Some(format!(
            "{}://{}/",
            base.scheme(),
            base.host_str().unwrap_or(domain)
        ));
    }
    if let Some(n) = org {
        c.description = ld_str(n, "description").unwrap_or_default();
        if let Some(addr) = n
            .get("address")
            .or_else(|| n.get("location").and_then(|l| l.get("address")))
        {
            c.hq_city = ld_str(addr, "addressLocality");
            c.hq_state = ld_str(addr, "addressRegion");
            c.hq_country = ld_str(addr, "addressCountry").map(|cc| country_code(&cc));
        }
        c.employees = employees(n.get("numberOfEmployees"));
        c.founded = ld_str(n, "foundingDate").and_then(|d| d.get(..4)?.parse().ok());
        if let Some(naics) = ld_str(n, "naics") {
            c.naics = vec![naics];
        }
    }
    if c.description.is_empty() && (is_home || is_about) {
        c.description = page.description.clone().unwrap_or_default();
    }
    if is_home || is_about {
        c.body = page.text.clone();
    }
    if let Some((provider, slug)) = &page.ats {
        c.ats_provider = Some(provider.clone());
        c.ats_slug = Some(slug.clone());
    }
    c.tech = classify::tech_in(&page.text, &ctx.market);
    out.push(Doc::Company(c));

    // Press releases, from pages that say they are news.
    let path = base.path().to_ascii_lowercase();
    if ["news", "press", "newsroom", "media", "blog"]
        .iter()
        .any(|seg| path.contains(seg))
    {
        let cutoff = now_ts() - NEWS_MAX_AGE_DAYS * 86_400;
        for n in &page.nodes {
            if !is_article(&ld_types(n)) {
                continue;
            }
            let Some(title) = ld_str(n, "headline").or_else(|| ld_str(n, "name")) else {
                continue;
            };
            let Some(published) = ld_str(n, "datePublished").and_then(|d| parse_schema_date(&d))
            else {
                continue;
            };
            if published < cutoff {
                continue;
            }
            let summary = ld_str(n, "description").unwrap_or_default();
            let url = ld_str(n, "url").unwrap_or_else(|| base.to_string());
            let mut s = Signal::new(
                id,
                classify::news_kind(&title, &summary),
                "crawl",
                url.clone(),
                title,
                published,
            );
            s.summary = summary;
            s.url = Some(url);
            out.push(Doc::Signal(s));
        }
    }
    out
}

/// "Acme Brewing | Home" → "Acme Brewing".
fn clean_title(t: &str) -> String {
    t.split(['|', '–', '—', '·'])
        .next()
        .unwrap_or(t)
        .split(" - ")
        .next()
        .unwrap_or(t)
        .trim()
        .to_string()
}

fn country_code(raw: &str) -> String {
    let t = raw.trim();
    match t.to_ascii_lowercase().as_str() {
        "united states" | "united states of america" | "usa" | "us" => "US".into(),
        "canada" | "ca" => "CA".into(),
        "united kingdom" | "uk" | "gb" | "great britain" => "GB".into(),
        _ if t.len() == 2 => t.to_ascii_uppercase(),
        _ => t.to_string(),
    }
}

/// `numberOfEmployees` is a number, a string, or a `QuantitativeValue` with
/// `value` or a `minValue`/`maxValue` range, whose midpoint we take.
fn employees(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    let n = match v {
        Value::Object(o) => {
            if let Some(x) = num(o.get("value")) {
                x
            } else {
                match (num(o.get("minValue")), num(o.get("maxValue"))) {
                    (Some(a), Some(b)) => (a + b) / 2.0,
                    (Some(a), None) | (None, Some(a)) => a,
                    _ => return None,
                }
            }
        }
        other => num(Some(other))?,
    };
    (n.is_finite() && n >= 1.0).then_some(n as u64)
}

fn is_org(types: &[String]) -> bool {
    const ORG: &[&str] = &[
        "organization",
        "corporation",
        "localbusiness",
        "brewery",
        "winery",
        "distillery",
        "foodestablishment",
        "store",
        "onlinebusiness",
        "onlinestore",
        "professionalservice",
        "manufacturer",
        "ngo",
    ];
    types.iter().any(|t| ORG.contains(&t.as_str()))
}

fn is_article(types: &[String]) -> bool {
    types
        .iter()
        .any(|t| t.contains("article") || t == "blogposting" || t == "pressrelease")
}

fn flatten_ld(value: Value, out: &mut Vec<Value>) {
    match value {
        Value::Array(items) => {
            for item in items {
                flatten_ld(item, out);
            }
        }
        Value::Object(ref map) => {
            if let Some(graph) = map.get("@graph").cloned() {
                flatten_ld(graph, out);
            }
            out.push(value);
        }
        _ => {}
    }
}

/// `@type` can be a string or a list; normalise to lowercase strings.
fn ld_types(node: &Value) -> Vec<String> {
    match node.get("@type") {
        Some(Value::String(s)) => vec![s.to_lowercase()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_lowercase))
            .collect(),
        _ => Vec::new(),
    }
}

fn ld_str(node: &Value, key: &str) -> Option<String> {
    match node.get(key)? {
        Value::String(s) => Some(collapse_ws(s)).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(a) => a.first().and_then(|v| v.as_str()).map(collapse_ws),
        Value::Object(o) => o
            .get("name")
            .or_else(|| o.get("@value"))
            .and_then(|v| v.as_str())
            .map(collapse_ws),
        _ => None,
    }
}

/// schema.org numbers arrive as numbers or as strings, depending on the CMS.
fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().replace(',', "").parse().ok(),
        _ => None,
    }
}

/// Parse a schema.org date: full RFC 3339, or a bare `YYYY-MM-DD`.
pub fn parse_schema_date(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.timestamp());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(raw.get(..10).unwrap_or(raw), "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp());
    }
    None
}

fn meta_content(doc: &Html, keys: &[&str]) -> Option<String> {
    for el in doc.select(&META) {
        let v = el.value();
        let name = v.attr("property").or_else(|| v.attr("name")).unwrap_or("");
        if keys.iter().any(|k| k.eq_ignore_ascii_case(name))
            && let Some(content) = v.attr("content")
        {
            let c = collapse_ws(content);
            if !c.is_empty() {
                return Some(c);
            }
        }
    }
    None
}

fn page_text(doc: &Html) -> String {
    let region = doc
        .select(&MAIN)
        .next()
        .or_else(|| doc.select(&BODY).next());
    match region {
        Some(el) => collapse_ws(&el.text().collect::<Vec<_>>().join(" ")),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = r#"<html><head><title>Acme Brewing | Home</title>
      <meta property="og:site_name" content="Acme Brewing">
      <meta name="description" content="Independent brewery in Denver.">
      <script type="application/ld+json">
      {"@context":"https://schema.org","@graph":[
        {"@type":"Brewery","name":"Acme Brewing Co.","description":"Small-batch beer since 2011.",
         "foundingDate":"2011-05-01",
         "address":{"@type":"PostalAddress","addressLocality":"Denver","addressRegion":"CO","addressCountry":"United States"},
         "numberOfEmployees":{"@type":"QuantitativeValue","minValue":50,"maxValue":100}},
        {"@type":"WebSite","name":"Acme"}]}
      </script></head>
      <body><main>We brew on a 30bbl system and track every keg in NetSuite.
        <a href="/about-us">About</a> <a href="/careers">Careers</a>
        <a href="/beer/ipa">IPA</a> <a href="https://other.test/x">x</a>
        <a href="https://boards.greenhouse.io/acmebrewing/jobs/123">Open roles</a>
      </main></body></html>"#;

    #[test]
    fn a_homepage_yields_links_ats_and_org_facts() {
        let base = Url::parse("https://www.acme.test/").unwrap();
        let p = read_page(HOME, &base);
        assert_eq!(p.site_name.as_deref(), Some("Acme Brewing"));
        assert_eq!(p.ats, Some(("greenhouse".into(), "acmebrewing".into())));
        assert!(
            p.links
                .contains(&"https://www.acme.test/careers".to_string())
        );
        assert!(!p.links.iter().any(|l| l.contains("other.test")));

        let org = p.nodes.iter().find(|n| is_org(&ld_types(n))).unwrap();
        assert_eq!(ld_str(org, "name").as_deref(), Some("Acme Brewing Co."));
        assert_eq!(employees(org.get("numberOfEmployees")), Some(75));
    }

    #[test]
    fn only_sales_relevant_links_are_followed() {
        assert!(is_interesting("https://a.test/about-us"));
        assert!(is_interesting("https://a.test/careers"));
        assert!(is_interesting("https://a.test/news/2026/launch"));
        assert!(is_interesting("https://a.test/company/team"));
        assert!(!is_interesting("https://a.test/beer/ipa"));
        assert!(!is_interesting("https://a.test/cart"));
    }

    #[test]
    fn ats_boards_are_detected_across_providers() {
        assert_eq!(
            detect_ats("https://jobs.lever.co/acme/abc-123"),
            Some(("lever".into(), "acme".into()))
        );
        assert_eq!(
            detect_ats(
                r#"<script src="https://boards.greenhouse.io/embed/job_board?for=acmeco"></script>"#
            ),
            Some(("greenhouse".into(), "acmeco".into()))
        );
        assert_eq!(
            detect_ats("https://jobs.ashbyhq.com/Acme"),
            Some(("ashby".into(), "acme".into()))
        );
        assert_eq!(detect_ats("https://example.test/careers"), None);
    }

    #[test]
    fn employee_counts_take_every_shape() {
        assert_eq!(employees(Some(&json!(120))), Some(120));
        assert_eq!(employees(Some(&json!("1,200"))), Some(1200));
        assert_eq!(employees(Some(&json!({"value": 40}))), Some(40));
        assert_eq!(employees(Some(&json!({"minValue": 10}))), Some(10));
        assert_eq!(employees(Some(&json!("lots"))), None);
    }

    #[test]
    fn titles_and_countries_are_cleaned() {
        assert_eq!(clean_title("Acme Brewing | Home"), "Acme Brewing");
        assert_eq!(clean_title("Acme - Craft Beer"), "Acme");
        assert_eq!(country_code("United States"), "US");
        assert_eq!(country_code("ca"), "CA");
    }

    #[test]
    fn schema_dates_parse_in_every_common_shape() {
        assert_eq!(parse_schema_date("1970-01-02"), Some(86_400));
        assert_eq!(parse_schema_date("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(parse_schema_date("1970-01-02T00:00:00"), Some(86_400));
        assert_eq!(
            parse_schema_date("1970-01-02T00:00:00.000-00:00"),
            Some(86_400)
        );
        assert_eq!(parse_schema_date("sometime next August"), None);
    }

    #[test]
    fn malformed_json_ld_does_not_break_the_page() {
        let html = r#"<script type="application/ld+json">{not json</script>
                      <script type="application/ld+json">{"@type":"Organization","name":"OK"}</script>"#;
        let p = read_page(html, &Url::parse("https://a.test/").unwrap());
        assert_eq!(p.nodes.len(), 1);
        assert_eq!(ld_str(&p.nodes[0], "name").as_deref(), Some("OK"));
    }
}
