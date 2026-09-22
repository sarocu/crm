//! The market: which industries we sell into, and who a good customer is.
//!
//! One `MarketConfig` is loaded from TOML at startup by every binary. It
//! replaces the old region gazetteer: instead of resolving town names to
//! coordinates, it resolves vertical and profile names to the criteria the
//! indexer classifies companies by and the MCP server scores them against.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const DEFAULT_MARKET_CONFIG: &str = "/etc/crm/market.toml";

/// An industry vertical. The indexer tags a company with a vertical when any
/// of its codes, Wikidata classes or keywords match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vertical {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// SEC Standard Industrial Classification codes, as 4-digit strings.
    #[serde(default)]
    pub sic: Vec<String>,
    /// NAICS codes. A code matches any company code it is a prefix of, so
    /// "3121" covers every beverage manufacturer.
    #[serde(default)]
    pub naics: Vec<String>,
    /// Wikidata items a company's industry (P452) or class (P31) may be.
    #[serde(default)]
    pub wikidata: Vec<String>,
    /// Words that, found in a company's description or site text, put it in
    /// this vertical.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Trade-press RSS/Atom feeds swept for news about companies here.
    #[serde(default)]
    pub feeds: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Range {
    #[serde(default)]
    pub min: Option<u64>,
    #[serde(default)]
    pub max: Option<u64>,
}

impl Range {
    pub fn contains(&self, n: u64) -> bool {
        self.min.is_none_or(|m| n >= m) && self.max.is_none_or(|m| n <= m)
    }

    pub fn is_open(&self) -> bool {
        self.min.is_none() && self.max.is_none()
    }
}

/// Which signals make a company in a profile worth contacting now.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalCriteria {
    /// Job titles (or fragments) whose postings suggest a need we serve.
    #[serde(default)]
    pub hiring_roles: Vec<String>,
    /// How far back a signal still counts.
    #[serde(default = "default_recency_days")]
    pub recency_days: u32,
}

impl Default for SignalCriteria {
    fn default() -> Self {
        Self {
            hiring_roles: Vec::new(),
            recency_days: default_recency_days(),
        }
    }
}

fn default_recency_days() -> u32 {
    90
}

/// How much each criterion contributes to a fit score.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Weights {
    #[serde(default = "w3")]
    pub vertical: f32,
    #[serde(default = "w2")]
    pub size: f32,
    #[serde(default = "w1")]
    pub geo: f32,
    #[serde(default = "w3")]
    pub hiring: f32,
    #[serde(default = "w1")]
    pub news: f32,
    #[serde(default = "w2")]
    pub keywords: f32,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            vertical: 3.0,
            size: 2.0,
            geo: 1.0,
            hiring: 3.0,
            news: 1.0,
            keywords: 2.0,
        }
    }
}

fn w1() -> f32 {
    1.0
}
fn w2() -> f32 {
    2.0
}
fn w3() -> f32 {
    3.0
}

/// An ideal customer profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Vertical slugs. A company must be in one of these to be a prospect.
    pub verticals: Vec<String>,
    /// ISO 3166-1 alpha-2 codes. Empty means anywhere.
    #[serde(default)]
    pub countries: Vec<String>,
    /// State/province codes or names. Empty means anywhere.
    #[serde(default)]
    pub states: Vec<String>,
    #[serde(default)]
    pub employees: Range,
    /// Need or tech hints looked for in site text and job postings.
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub signals: SignalCriteria,
    #[serde(default)]
    pub weights: Weights,
    /// Investor names ("Y Combinator"). When set, only companies backed by
    /// one of them are prospects.
    #[serde(default)]
    pub investors: Vec<String>,
}

/// How a portfolio is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortfolioKind {
    /// The Y Combinator company directory as JSON (the yc-oss mirror by
    /// default): structured, with team size, location, batch and status.
    Yc,
    /// Any investor's portfolio web page. Every outbound link to another
    /// domain is taken as a portfolio company, and the crawler fills in
    /// the rest from that company's own site.
    Page,
    /// A JSON document listing the portfolio — many portfolio sites load
    /// their grid from one. Fields are picked out with JSON pointers.
    Json,
}

/// An investor whose portfolio companies are swept in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Portfolio {
    pub slug: String,
    /// The investor's name, recorded on every company it brings in.
    pub investor: String,
    pub kind: PortfolioKind,
    pub url: String,
    /// `page` only: a CSS selector scoping which links count, e.g.
    /// `.portfolio-grid a`. Without it every outbound link on the page does.
    #[serde(default)]
    pub selector: Option<String>,
    /// `page` only: when the grid links to the investor's own page per
    /// company rather than to the company, a CSS selector for those links.
    /// Each detail page is then visited for the company's outbound link.
    #[serde(default)]
    pub detail_selector: Option<String>,
    /// `json` only: JSON pointer to the array of companies. Default: the
    /// document root.
    #[serde(default)]
    pub items: Option<String>,
    /// `json` only: pointer, within one item, to the company name.
    /// Default `/name`.
    #[serde(default)]
    pub name_field: Option<String>,
    /// `json` only: pointer, within one item, to the website. Default
    /// `/website`.
    #[serde(default)]
    pub website_field: Option<String>,
    /// `yc` only: which company statuses to keep. Default Active and Public.
    #[serde(default)]
    pub statuses: Vec<String>,
    /// `yc` only: skip batches before this year.
    #[serde(default)]
    pub since_year: Option<i32>,
}

/// Language-dependent search settings, applied to every searchable index.
/// A value given here **replaces** the built-in default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vocabulary {
    #[serde(default = "default_stop_words")]
    pub stop_words: Vec<String>,
    /// Groups of interchangeable terms, made synonyms in both directions.
    #[serde(default = "default_synonyms")]
    pub synonyms: Vec<Vec<String>>,
}

impl Default for Vocabulary {
    fn default() -> Self {
        Self {
            stop_words: default_stop_words(),
            synonyms: default_synonyms(),
        }
    }
}

impl Vocabulary {
    /// Expand the groups into the one-directional map Meilisearch wants.
    pub fn synonym_map(&self) -> std::collections::HashMap<String, Vec<String>> {
        let mut map: std::collections::HashMap<String, Vec<String>> = Default::default();
        for group in &self.synonyms {
            let terms: Vec<String> = group
                .iter()
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                .collect();
            for term in &terms {
                let others: Vec<String> = terms.iter().filter(|o| *o != term).cloned().collect();
                map.entry(term.clone()).or_default().extend(others);
            }
        }
        for (term, list) in map.iter_mut() {
            list.sort();
            list.dedup();
            list.retain(|o| o != term);
        }
        map
    }
}

fn default_stop_words() -> Vec<String> {
    [
        "a", "an", "and", "at", "for", "in", "is", "it", "of", "on", "or", "the", "to", "with",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_synonyms() -> Vec<Vec<String>> {
    [
        &[
            "inc",
            "incorporated",
            "corp",
            "corporation",
            "co",
            "company",
        ][..],
        &["llc", "ltd", "limited"][..],
        &["saas", "software as a service", "cloud software"][..],
        &["erp", "enterprise resource planning"][..],
        &["crm", "customer relationship management"][..],
        &["ops", "operations"][..],
    ]
    .iter()
    .map(|g| g.iter().map(|t| t.to_string()).collect())
    .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketConfig {
    pub name: String,
    pub slug: String,
    #[serde(default)]
    pub verticals: Vec<Vertical>,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// Investors whose portfolios are swept for companies.
    #[serde(default)]
    pub portfolios: Vec<Portfolio>,
    #[serde(default)]
    pub vocabulary: Vocabulary,
}

impl MarketConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .map_err(|e| Error::Market(format!("cannot read {}: {e}", path.display())))?;
        Self::parse(&raw).map_err(|e| Error::Market(format!("{}: {e}", path.display())))
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let cfg: MarketConfig =
            toml::from_str(raw).map_err(|e| Error::Market(format!("cannot parse: {e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load from `MARKET_CONFIG`, falling back to the baked-in image path.
    pub fn from_env() -> Result<Self> {
        let path = std::env::var("MARKET_CONFIG").unwrap_or_else(|_| DEFAULT_MARKET_CONFIG.into());
        Self::load(path)
    }

    fn validate(&self) -> Result<()> {
        let mut seen_pf = std::collections::HashSet::new();
        for p in &self.portfolios {
            if !is_slug(&p.slug) || !seen_pf.insert(p.slug.as_str()) {
                return Err(Error::Market(format!(
                    "portfolio slug {:?} must be unique lowercase letters, digits and dashes",
                    p.slug
                )));
            }
            for sel in [&p.selector, &p.detail_selector].into_iter().flatten() {
                if sel.trim().is_empty() {
                    return Err(Error::Market(format!(
                        "portfolio {:?} has an empty selector",
                        p.slug
                    )));
                }
            }
            if p.investor.trim().is_empty() {
                return Err(Error::Market(format!(
                    "portfolio {:?} names no investor",
                    p.slug
                )));
            }
            let ok = url::Url::parse(&p.url)
                .map(|u| matches!(u.scheme(), "http" | "https"))
                .unwrap_or(false);
            if !ok {
                return Err(Error::Market(format!(
                    "portfolio {:?} url {:?} is not an http(s) URL",
                    p.slug, p.url
                )));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for v in &self.verticals {
            if !is_slug(&v.slug) {
                return Err(Error::Market(format!(
                    "vertical slug {:?} must be lowercase letters, digits and dashes",
                    v.slug
                )));
            }
            if !seen.insert(v.slug.as_str()) {
                return Err(Error::Market(format!("duplicate vertical {:?}", v.slug)));
            }
            for f in &v.feeds {
                let ok = url::Url::parse(f)
                    .map(|u| matches!(u.scheme(), "http" | "https"))
                    .unwrap_or(false);
                if !ok {
                    return Err(Error::Market(format!(
                        "vertical {:?} feed {f:?} is not an http(s) URL",
                        v.slug
                    )));
                }
            }
        }
        let mut seen_p = std::collections::HashSet::new();
        for p in &self.profiles {
            if !is_slug(&p.slug) {
                return Err(Error::Market(format!(
                    "profile slug {:?} must be lowercase letters, digits and dashes",
                    p.slug
                )));
            }
            if !seen_p.insert(p.slug.as_str()) {
                return Err(Error::Market(format!("duplicate profile {:?}", p.slug)));
            }
            if p.verticals.is_empty() {
                return Err(Error::Market(format!(
                    "profile {:?} names no verticals",
                    p.slug
                )));
            }
            for v in &p.verticals {
                if self.vertical(v).is_none() {
                    return Err(Error::Market(format!(
                        "profile {:?} names unknown vertical {v:?}",
                        p.slug
                    )));
                }
            }
            if let (Some(min), Some(max)) = (p.employees.min, p.employees.max)
                && min > max
            {
                return Err(Error::Market(format!(
                    "profile {:?} has employees.min {min} above employees.max {max}",
                    p.slug
                )));
            }
        }
        Ok(())
    }

    pub fn vertical(&self, slug: &str) -> Option<&Vertical> {
        self.verticals.iter().find(|v| v.slug == slug)
    }

    pub fn profile(&self, slug: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.slug == slug)
    }

    /// Resolve a human name ("craft beer", "Craft Beverage", "brewery") to a
    /// vertical, by slug, name or alias, with typo tolerance.
    pub fn resolve_vertical(&self, query: &str) -> Option<&Vertical> {
        resolve(&self.verticals, query, |v| {
            std::iter::once(v.slug.as_str())
                .chain(std::iter::once(v.name.as_str()))
                .chain(v.aliases.iter().map(String::as_str))
                .collect()
        })
    }

    pub fn resolve_profile(&self, query: &str) -> Option<&Profile> {
        resolve(&self.profiles, query, |p| {
            std::iter::once(p.slug.as_str())
                .chain(std::iter::once(p.name.as_str()))
                .chain(p.aliases.iter().map(String::as_str))
                .collect()
        })
    }

    /// The closest vertical slugs to a query, for "did you mean" errors.
    pub fn suggest_verticals(&self, query: &str, n: usize) -> Vec<String> {
        suggest(&self.verticals, query, n, |v| (&v.slug, &v.name))
    }

    pub fn suggest_profiles(&self, query: &str, n: usize) -> Vec<String> {
        suggest(&self.profiles, query, n, |p| (&p.slug, &p.name))
    }

    /// Every feed URL across all verticals, each once.
    pub fn feeds(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .verticals
            .iter()
            .flat_map(|v| v.feeds.iter().cloned())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Exact match on any name, then prefix, then a bounded edit distance.
/// Returns `None` rather than guessing wildly.
fn resolve<'a, T>(items: &'a [T], query: &str, names: impl Fn(&T) -> Vec<&str>) -> Option<&'a T> {
    let q = normalize(query);
    if q.is_empty() {
        return None;
    }
    if let Some(t) = items
        .iter()
        .find(|t| names(t).iter().any(|n| normalize(n) == q))
    {
        return Some(t);
    }
    if let Some((_, t)) = items
        .iter()
        .filter_map(|t| {
            names(t)
                .iter()
                .map(|n| normalize(n))
                .filter(|n| n.len() >= 4 && (q.starts_with(n.as_str()) || n.starts_with(&q)))
                .map(|n| n.len())
                .max()
                .map(|len| (len, t))
        })
        .max_by_key(|(len, _)| *len)
    {
        return Some(t);
    }
    let budget = if q.len() >= 8 {
        2
    } else if q.len() >= 5 {
        1
    } else {
        return None;
    };
    items
        .iter()
        .filter_map(|t| {
            let d = names(t)
                .iter()
                .map(|n| edit_distance(&q, &normalize(n)))
                .min()
                .unwrap_or(usize::MAX);
            (d <= budget).then_some((d, t))
        })
        .min_by_key(|(d, _)| *d)
        .map(|(_, t)| t)
}

fn suggest<T>(
    items: &[T],
    query: &str,
    n: usize,
    key: impl Fn(&T) -> (&String, &String),
) -> Vec<String> {
    let q = normalize(query);
    let mut scored: Vec<(usize, &str)> = items
        .iter()
        .map(|t| {
            let (slug, name) = key(t);
            let d = edit_distance(&q, &normalize(slug)).min(edit_distance(&q, &normalize(name)));
            (d, slug.as_str())
        })
        .collect();
    scored.sort_by_key(|(d, s)| (*d, *s));
    scored
        .into_iter()
        .take(n)
        .map(|(_, s)| s.to_string())
        .collect()
}

/// Lowercase, drop punctuation and diacritics, collapse whitespace, so
/// "Craft-Beverage", "craft beverage" and "Cräft  Beverage" compare equal.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.chars() {
        let ch = fold_diacritic(ch);
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

fn fold_diacritic(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'Á' | 'À' | 'Â' | 'Ä' | 'Ã' | 'Å' => 'a',
        'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => 'i',
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Õ' => 'o',
        'ú' | 'ù' | 'û' | 'ü' | 'Ú' | 'Ù' | 'Û' | 'Ü' => 'u',
        'ñ' | 'Ñ' => 'n',
        'ç' | 'Ç' => 'c',
        other => other,
    }
}

/// Levenshtein distance over chars, two-row variant.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn example() -> MarketConfig {
        MarketConfig::load(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../market.example.toml"
        ))
        .expect("the shipped example market must load and validate")
    }

    #[test]
    fn shipped_config_is_valid() {
        let m = example();
        assert!(m.verticals.len() >= 3);
        assert!(m.profiles.len() >= 2);
        assert!(m.portfolios.iter().any(|p| p.kind == PortfolioKind::Yc));
        for p in &m.profiles {
            for v in &p.verticals {
                assert!(m.vertical(v).is_some());
            }
        }
    }

    #[test]
    fn verticals_resolve_by_slug_name_alias_and_typo() {
        let m = example();
        let slug = &m.verticals[0].slug;
        assert_eq!(&m.resolve_vertical(slug).unwrap().slug, slug);
        assert_eq!(
            &m.resolve_vertical(&m.verticals[0].name.to_uppercase())
                .unwrap()
                .slug,
            slug
        );
        assert_eq!(
            m.resolve_vertical("brewery").unwrap().slug,
            "craft-beverage"
        );
        assert_eq!(
            m.resolve_vertical("craft beverge").unwrap().slug,
            "craft-beverage"
        );
        assert!(m.resolve_vertical("zzzz").is_none());
        assert!(m.resolve_vertical("").is_none());
    }

    #[test]
    fn profiles_resolve_and_suggest() {
        let m = example();
        let p = &m.profiles[0];
        assert_eq!(&m.resolve_profile(&p.name).unwrap().slug, &p.slug);
        assert!(!m.suggest_profiles("nothing like it", 2).is_empty());
    }

    #[test]
    fn validation_rejects_bad_configs() {
        let unknown = r#"
            name = "x"
            slug = "x"
            [[verticals]]
            slug = "a"
            name = "A"
            [[profiles]]
            slug = "p"
            name = "P"
            verticals = ["b"]
        "#;
        assert!(MarketConfig::parse(unknown).is_err());

        let inverted = r#"
            name = "x"
            slug = "x"
            [[verticals]]
            slug = "a"
            name = "A"
            [[profiles]]
            slug = "p"
            name = "P"
            verticals = ["a"]
            employees = { min = 500, max = 10 }
        "#;
        assert!(MarketConfig::parse(inverted).is_err());

        let dup = r#"
            name = "x"
            slug = "x"
            [[verticals]]
            slug = "a"
            name = "A"
            [[verticals]]
            slug = "a"
            name = "B"
        "#;
        assert!(MarketConfig::parse(dup).is_err());

        let bad_portfolio = r#"
            name = "x"
            slug = "x"
            [[portfolios]]
            slug = "yc"
            investor = "Y Combinator"
            kind = "yc"
            url = "ftp://nope"
        "#;
        assert!(MarketConfig::parse(bad_portfolio).is_err());

        let bad_feed = r#"
            name = "x"
            slug = "x"
            [[verticals]]
            slug = "a"
            name = "A"
            feeds = ["not a url"]
        "#;
        assert!(MarketConfig::parse(bad_feed).is_err());
    }

    #[test]
    fn ranges_treat_missing_bounds_as_open() {
        let r = Range {
            min: Some(50),
            max: None,
        };
        assert!(r.contains(50));
        assert!(r.contains(1_000_000));
        assert!(!r.contains(49));
        assert!(Range::default().is_open());
    }

    #[test]
    fn synonyms_are_bidirectional() {
        let syn = Vocabulary::default().synonym_map();
        assert!(syn["corp"].contains(&"inc".to_string()));
        assert!(syn["inc"].contains(&"corp".to_string()));
        assert!(!syn["inc"].contains(&"inc".to_string()));
    }
}
