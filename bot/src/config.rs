//! Indexer configuration, all from the environment.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use url::Url;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub user_agent: String,
    /// Run one cycle of every source and exit. Used by CI and by the
    /// verification steps in the README.
    pub once: bool,
    pub intervals: HashMap<String, Duration>,
    pub disabled_sources: Vec<String>,
    /// Extra homepages to crawl on top of the companies other sources find.
    pub crawl_seeds: Vec<Url>,
    pub crawl_max_depth: u32,
    pub crawl_pages_per_run: usize,
    /// Company homepages queued for crawling per run.
    pub crawl_companies_per_run: usize,
    /// Extra RSS/Atom feeds on top of the ones the market config names.
    pub news_feeds: Vec<Url>,
    pub edgar_tickers_url: String,
    pub edgar_submissions_url: String,
    pub edgar_per_run: usize,
    /// How far back a filing still counts as a signal.
    pub edgar_filing_days: i64,
    pub wikidata_sparql: String,
    pub wikidata_page_size: usize,
    /// Companies whose job boards are polled per run.
    pub jobs_per_run: usize,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let once = std::env::args().any(|a| a == "--once")
            || matches!(
                std::env::var("BOT_RUN_ONCE").as_deref(),
                Ok("1") | Ok("true")
            );

        let port: u16 = match std::env::var("PORT") {
            Ok(p) => p
                .parse()
                .with_context(|| format!("PORT={p:?} is not a port number"))?,
            Err(_) => 8081,
        };
        let host = std::env::var("BIND_HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let bind: SocketAddr = format!("{host}:{port}").parse()?;

        // The SEC and Wikimedia both require a real contact in the
        // User-Agent and block anonymous bulk traffic. Refusing to start is
        // friendlier than getting quietly banned an hour in.
        let contact_email = std::env::var("CONTACT_EMAIL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s.contains('@'))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "CONTACT_EMAIL must be set to a real address you monitor. SEC EDGAR and \
                     Wikimedia require it in the User-Agent and will block traffic without it."
                )
            })?;
        let user_agent = format!(
            "crm-indexer/{} (+{contact_email})",
            env!("CARGO_PKG_VERSION")
        );

        let intervals = parse_intervals(std::env::var("SOURCE_INTERVALS").ok().as_deref())?;
        let disabled_sources = split_csv(std::env::var("DISABLED_SOURCES").ok().as_deref());

        Ok(Self {
            bind,
            user_agent,
            once,
            intervals,
            disabled_sources,
            crawl_seeds: parse_urls("CRAWL_SEEDS", std::env::var("CRAWL_SEEDS").ok().as_deref())?,
            crawl_max_depth: env_num("CRAWL_MAX_DEPTH", 2)?,
            crawl_pages_per_run: env_num("CRAWL_PAGES_PER_RUN", 40)?,
            crawl_companies_per_run: env_num("CRAWL_COMPANIES_PER_RUN", 100)?,
            news_feeds: parse_urls("NEWS_FEEDS", std::env::var("NEWS_FEEDS").ok().as_deref())?,
            edgar_tickers_url: std::env::var("EDGAR_TICKERS_URL")
                .unwrap_or_else(|_| "https://www.sec.gov/files/company_tickers.json".into()),
            edgar_submissions_url: std::env::var("EDGAR_SUBMISSIONS_URL")
                .unwrap_or_else(|_| "https://data.sec.gov/submissions".into()),
            edgar_per_run: env_num("EDGAR_PER_RUN", 200)?,
            edgar_filing_days: env_num("EDGAR_FILING_DAYS", 180)?,
            wikidata_sparql: std::env::var("WIKIDATA_SPARQL")
                .unwrap_or_else(|_| "https://query.wikidata.org/sparql".into()),
            wikidata_page_size: env_num("WIKIDATA_PAGE_SIZE", 200)?,
            jobs_per_run: env_num("JOBS_PER_RUN", 25)?,
        })
    }

    pub fn interval_for(&self, source: &str, default: Duration) -> Duration {
        self.intervals.get(source).copied().unwrap_or(default)
    }

    pub fn is_enabled(&self, source: &str) -> bool {
        !self.disabled_sources.iter().any(|d| d == source)
    }
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(v) => v
            .trim()
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("{key}={v:?} is not a number: {e}")),
        Err(_) => Ok(default),
    }
}

fn split_csv(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse `edgar=6h,wikidata=1d,crawl=15m`.
fn parse_intervals(raw: Option<&str>) -> Result<HashMap<String, Duration>> {
    let mut out = HashMap::new();
    for entry in split_csv(raw) {
        let (name, value) = entry
            .split_once('=')
            .with_context(|| format!("SOURCE_INTERVALS entry {entry:?} is not name=duration"))?;
        out.insert(name.trim().to_string(), parse_duration(value.trim())?);
    }
    Ok(out)
}

/// Parse `45s`, `30m`, `6h`, `2d`. A bare number is seconds.
pub fn parse_duration(raw: &str) -> Result<Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty duration");
    }
    let (digits, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len()));
    let n: u64 = digits
        .parse()
        .with_context(|| format!("{raw:?} does not start with a number"))?;
    let secs = match unit.trim() {
        "" | "s" | "sec" | "secs" => n,
        "m" | "min" | "mins" => n * 60,
        "h" | "hr" | "hrs" => n * 3600,
        "d" | "day" | "days" => n * 86400,
        other => bail!("unknown duration unit {other:?} in {raw:?}; use s, m, h or d"),
    };
    if secs == 0 {
        bail!("duration {raw:?} is zero, which would spin the scheduler");
    }
    Ok(Duration::from_secs(secs))
}

fn parse_urls(var: &str, raw: Option<&str>) -> Result<Vec<Url>> {
    let mut out = Vec::new();
    for entry in split_csv(raw) {
        let url = Url::parse(&entry)
            .with_context(|| format!("{var} entry {entry:?} is not a valid URL"))?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("{var} entry {entry:?} must be http or https");
        }
        out.push(url);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_with_and_without_units() {
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("6h").unwrap(), Duration::from_secs(21600));
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172800));
        assert!(parse_duration("0m").is_err(), "a zero interval would spin");
        assert!(parse_duration("6 weeks").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn intervals_parse_as_a_map() {
        let m = parse_intervals(Some("edgar=6h, crawl=15m")).unwrap();
        assert_eq!(m["edgar"], Duration::from_secs(21600));
        assert_eq!(m["crawl"], Duration::from_secs(900));
        assert!(parse_intervals(Some("bogus")).is_err());
    }

    #[test]
    fn url_lists_reject_junk() {
        let s = parse_urls("X", Some("https://a.test/feed, https://b.test")).unwrap();
        assert_eq!(s.len(), 2);
        assert!(parse_urls("X", Some("ftp://a.test")).is_err());
        assert!(parse_urls("X", Some("not a url")).is_err());
    }
}
