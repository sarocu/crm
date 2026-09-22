//! SEC EDGAR: public companies in our verticals, and their filings.
//!
//! Walks the SEC's ticker list one slice per run and reads each company's
//! submissions record, which carries its SIC code, business address and
//! recent filings. Only companies whose SIC code belongs to a configured
//! vertical are kept. Recent 8-Ks, 10-Ks and registration statements become
//! signals; an 8-K's item numbers say what kind (5.02 is an executive
//! change, 2.01 an acquisition).
//!
//! The SEC asks for at most ten requests a second and a User-Agent with a
//! contact address. The Fetcher's per-host interval and `CONTACT_EMAIL`
//! take care of both.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use crm_core::id::{company_id, root_domain};
use crm_core::model::{Company, Doc, Signal, SignalKind, now_ts};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{Batch, Ctx, Source, cursor_usize};

/// How long the ticker list is reused before it is fetched again.
const TICKERS_TTL: Duration = Duration::from_secs(24 * 3600);
/// Filings per company turned into signals, newest first.
const MAX_FILINGS: usize = 10;
/// Forms worth telling a salesperson about.
const FORMS: &[&str] = &["8-K", "10-K", "S-1", "S-4", "425", "D"];

#[derive(Debug, Clone)]
pub struct Listed {
    pub cik: String,
    pub ticker: String,
}

#[derive(Default)]
pub struct Edgar {
    tickers: Mutex<Option<(Instant, Arc<Vec<Listed>>)>>,
}

#[async_trait]
impl Source for Edgar {
    fn name(&self) -> &'static str {
        "edgar"
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(30 * 60)
    }

    async fn fetch(&self, ctx: &Ctx, cursor: Option<Value>) -> Result<Batch> {
        let wanted: Vec<&str> = ctx
            .market
            .verticals
            .iter()
            .flat_map(|v| v.sic.iter().map(String::as_str))
            .collect();
        if wanted.is_empty() {
            tracing::debug!("no vertical names a SIC code; the edgar source has nothing to do");
            return Ok(Batch::empty());
        }

        let listed = self.tickers(ctx).await?;
        if listed.is_empty() {
            return Ok(Batch::empty());
        }
        let start = cursor_usize(&cursor, "pos").min(listed.len());
        let end = (start + ctx.config.edgar_per_run).min(listed.len());

        let mut docs = Vec::new();
        let mut matched = 0;
        for l in &listed[start..end] {
            let url = format!("{}/CIK{}.json", ctx.config.edgar_submissions_url, l.cik);
            let sub: Submissions = match ctx.http.get_json(&url).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(cik = %l.cik, error = %e, "no submissions record");
                    continue;
                }
            };
            if !sub.sic.as_deref().is_some_and(|s| wanted.contains(&s)) {
                continue;
            }
            matched += 1;
            let since = now_ts() - ctx.config.edgar_filing_days * 86_400;
            let (company, signals) = to_docs(&l.cik, Some(&l.ticker), &sub, since);
            docs.push(Doc::Company(company));
            docs.extend(signals.into_iter().map(Doc::Signal));
        }

        tracing::info!(
            from = start,
            to = end,
            of = listed.len(),
            matched,
            "edgar slice complete"
        );
        let batch = if end >= listed.len() {
            Batch::new(docs, Some(json!({ "pos": 0 }))).swept()
        } else {
            Batch::new(docs, Some(json!({ "pos": end })))
        };
        Ok(batch)
    }
}

impl Edgar {
    async fn tickers(&self, ctx: &Ctx) -> Result<Arc<Vec<Listed>>> {
        let mut cache = self.tickers.lock().await;
        if let Some((at, list)) = cache.as_ref()
            && at.elapsed() < TICKERS_TTL
        {
            return Ok(list.clone());
        }
        let raw: Value = ctx.http.get_json(&ctx.config.edgar_tickers_url).await?;
        let list = Arc::new(parse_tickers(&raw));
        tracing::info!(n = list.len(), "loaded the SEC ticker list");
        *cache = Some((Instant::now(), list.clone()));
        Ok(list)
    }
}

/// `company_tickers.json` is an object keyed "0", "1", … of
/// `{cik_str, ticker, title}`. Share classes repeat a CIK; keep the first.
pub fn parse_tickers(raw: &Value) -> Vec<Listed> {
    let Some(obj) = raw.as_object() else {
        return Vec::new();
    };
    let mut rows: Vec<(usize, Listed)> = obj
        .iter()
        .filter_map(|(k, v)| {
            let cik = v.get("cik_str")?.as_u64()?;
            Some((
                k.parse().unwrap_or(usize::MAX),
                Listed {
                    cik: format!("{cik:010}"),
                    ticker: v.get("ticker")?.as_str()?.to_string(),
                },
            ))
        })
        .collect();
    // Keys are the SEC's own ordering (roughly by market cap); keep it so
    // the cursor walks a stable list across refreshes.
    rows.sort_by_key(|(k, _)| *k);
    let mut seen = std::collections::HashSet::new();
    rows.into_iter()
        .map(|(_, l)| l)
        .filter(|l| seen.insert(l.cik.clone()))
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct Submissions {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub sic: Option<String>,
    #[serde(default, rename = "sicDescription")]
    pub sic_description: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub addresses: Option<Addresses>,
    #[serde(default)]
    pub filings: Option<Filings>,
}

#[derive(Debug, Deserialize)]
pub struct Addresses {
    #[serde(default)]
    pub business: Option<Address>,
}

#[derive(Debug, Deserialize)]
pub struct Address {
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default, rename = "stateOrCountry")]
    pub state_or_country: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Filings {
    pub recent: Recent,
}

/// Column-oriented: element `i` of every vector is one filing.
#[derive(Debug, Deserialize)]
pub struct Recent {
    #[serde(default, rename = "accessionNumber")]
    pub accession_number: Vec<String>,
    #[serde(default, rename = "filingDate")]
    pub filing_date: Vec<String>,
    #[serde(default)]
    pub form: Vec<String>,
    #[serde(default, rename = "primaryDocument")]
    pub primary_document: Vec<String>,
    #[serde(default, rename = "primaryDocDescription")]
    pub primary_doc_description: Vec<String>,
    #[serde(default)]
    pub items: Vec<String>,
}

const US_STATES: &[&str] = &[
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "DC", "FL", "GA", "HI", "ID", "IL", "IN", "IA",
    "KS", "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM",
    "NY", "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA",
    "WV", "WI", "WY", "PR",
];

pub fn to_docs(
    cik: &str,
    ticker: Option<&str>,
    sub: &Submissions,
    since: i64,
) -> (Company, Vec<Signal>) {
    let domain = sub.website.as_deref().and_then(root_domain);
    let id = company_id(domain.as_deref(), Some(cik), None).expect("a CIK is always present");
    let mut c = Company::new(&id, title_case(&sub.name), "edgar");
    c.cik = Some(cik.to_string());
    c.ticker = ticker.map(str::to_string);
    c.domain = domain;
    c.website = sub.website.clone().filter(|w| !w.trim().is_empty());
    c.sic = sub.sic.clone();
    c.sic_description = sub.sic_description.clone();
    c.industries = sub.sic_description.iter().cloned().collect();
    if let Some(addr) = sub.addresses.as_ref().and_then(|a| a.business.as_ref()) {
        c.hq_city = addr.city.as_deref().map(title_case);
        if let Some(code) = addr.state_or_country.as_deref()
            && US_STATES.contains(&code)
        {
            c.hq_country = Some("US".into());
            c.hq_state = Some(code.to_string());
        }
    }

    let mut signals = Vec::new();
    if let Some(recent) = sub.filings.as_ref().map(|f| &f.recent) {
        let cik_num = cik.trim_start_matches('0');
        for i in 0..recent.form.len() {
            if signals.len() >= MAX_FILINGS {
                break;
            }
            let form = recent.form[i].as_str();
            if !FORMS.contains(&form) {
                continue;
            }
            let Some(date) = recent.filing_date.get(i).and_then(|d| parse_date(d)) else {
                continue;
            };
            if date < since {
                // Newest first, so everything after this is older still.
                break;
            }
            let Some(accession) = recent.accession_number.get(i) else {
                continue;
            };
            let items = recent.items.get(i).map(String::as_str).unwrap_or("");
            let desc = recent
                .primary_doc_description
                .get(i)
                .map(String::as_str)
                .filter(|d| !d.trim().is_empty())
                .unwrap_or(form);
            let mut s = Signal::new(
                &id,
                filing_kind(form, items),
                "edgar",
                accession.clone(),
                format!("{form} filed: {}", describe(desc, items)),
                date,
            );
            let doc = recent
                .primary_document
                .get(i)
                .map(String::as_str)
                .unwrap_or("");
            s.url = Some(format!(
                "https://www.sec.gov/Archives/edgar/data/{cik_num}/{}/{doc}",
                accession.replace('-', "")
            ));
            if !items.is_empty() {
                s.summary = format!("8-K items {items}");
            }
            signals.push(s);
        }
    }
    (c, signals)
}

/// What an 8-K is about, from its item numbers.
pub fn filing_kind(form: &str, items: &str) -> SignalKind {
    if form == "8-K" {
        if items.contains("5.02") {
            return SignalKind::Leadership;
        }
        if items.contains("2.01") || items.contains("1.01") || items.contains("3.02") {
            return SignalKind::Funding;
        }
    }
    if matches!(form, "S-1" | "S-4" | "425" | "D") {
        return SignalKind::Funding;
    }
    SignalKind::Filing
}

fn describe(desc: &str, items: &str) -> String {
    let mut out = Vec::new();
    for (code, label) in [
        ("1.01", "material agreement"),
        ("2.01", "acquisition or disposition"),
        ("2.02", "results of operations"),
        ("3.02", "unregistered sale of equity"),
        ("5.02", "executive or director change"),
        ("7.01", "Regulation FD disclosure"),
        ("8.01", "other events"),
    ] {
        if items.contains(code) {
            out.push(label);
        }
    }
    if out.is_empty() {
        desc.to_string()
    } else {
        out.join(", ")
    }
}

fn parse_date(raw: &str) -> Option<i64> {
    chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
        .map(|d| d.and_utc().timestamp())
}

/// EDGAR names are upper case ("ACME BREWING CO /CO/"). Make them readable,
/// and drop the state-of-incorporation suffix.
pub fn title_case(raw: &str) -> String {
    let trimmed = match raw.find(" /") {
        Some(i) => &raw[..i],
        None => raw,
    };
    trimmed
        .split_whitespace()
        .map(|w| {
            let upper = w.to_uppercase();
            if matches!(
                upper.as_str(),
                "LLC" | "LP" | "LLP" | "USA" | "US" | "PLC" | "NV" | "SA"
            ) {
                return upper;
            }
            let mut chars = w.chars();
            match chars.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBMISSIONS: &str = r#"{
      "cik": "1234567", "name": "ACME BREWING CO /CO/", "sic": "2082",
      "sicDescription": "Malt Beverages", "website": "https://www.acmebrewing.test",
      "addresses": {"business": {"city": "DENVER", "stateOrCountry": "CO"}},
      "filings": {"recent": {
        "accessionNumber": ["0001-24-000003", "0001-24-000002", "0001-24-000001", "0001-20-000001"],
        "filingDate": ["2026-09-01", "2026-08-15", "2026-08-01", "2020-01-01"],
        "form": ["8-K", "10-Q", "8-K", "10-K"],
        "primaryDocument": ["a.htm", "b.htm", "c.htm", "d.htm"],
        "primaryDocDescription": ["8-K", "10-Q", "8-K", "10-K"],
        "items": ["5.02,9.01", "", "2.01", ""]
      }}
    }"#;

    #[test]
    fn a_submissions_record_becomes_a_company_and_filing_signals() {
        let sub: Submissions = serde_json::from_str(SUBMISSIONS).unwrap();
        let since = parse_date("2026-01-01").unwrap();
        let (c, signals) = to_docs("0001234567", Some("ACME"), &sub, since);

        assert_eq!(c.name, "Acme Brewing Co");
        assert_eq!(c.domain.as_deref(), Some("acmebrewing.test"));
        assert_eq!(
            c.id,
            company_id(Some("acmebrewing.test"), None, None).unwrap()
        );
        assert_eq!(c.cik.as_deref(), Some("0001234567"));
        assert_eq!(c.hq_state.as_deref(), Some("CO"));
        assert_eq!(c.hq_country.as_deref(), Some("US"));
        assert_eq!(c.hq_city.as_deref(), Some("Denver"));

        // The 10-Q is not a form we track, and the 2020 10-K is too old.
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].kind, SignalKind::Leadership);
        assert!(signals[0].title.contains("executive or director change"));
        assert_eq!(
            signals[0].url.as_deref(),
            Some("https://www.sec.gov/Archives/edgar/data/1234567/000124000003/a.htm")
        );
        assert_eq!(signals[1].kind, SignalKind::Funding);
        assert!(signals.iter().all(|s| s.company_id == c.id));
    }

    #[test]
    fn without_a_website_the_cik_is_the_key() {
        let mut sub: Submissions = serde_json::from_str(SUBMISSIONS).unwrap();
        sub.website = Some(String::new());
        let (c, _) = to_docs("0001234567", None, &sub, 0);
        assert_eq!(c.id, company_id(None, Some("0001234567"), None).unwrap());
        assert!(c.domain.is_none());
    }

    #[test]
    fn the_ticker_list_parses_in_order_with_one_row_per_cik() {
        let raw = serde_json::json!({
            "1": {"cik_str": 2, "ticker": "B", "title": "B"},
            "0": {"cik_str": 1, "ticker": "A", "title": "A"},
            "2": {"cik_str": 1, "ticker": "A2", "title": "A"}
        });
        let l = parse_tickers(&raw);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].cik, "0000000001");
        assert_eq!(l[0].ticker, "A");
        assert_eq!(l[1].cik, "0000000002");
    }

    #[test]
    fn edgar_names_are_made_readable() {
        assert_eq!(title_case("ACME BREWING CO /CO/"), "Acme Brewing Co");
        assert_eq!(title_case("BIG FREIGHT LLC"), "Big Freight LLC");
    }
}
