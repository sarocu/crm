//! Rule-based classification: which vertical a company is in, which role
//! family a job posting belongs to, and what kind of signal a headline is.
//!
//! Deliberately plain keyword rules. They are cheap, deterministic and easy
//! to reason about when an agent asks "why is this company tagged as
//! logistics?", which matters more here than squeezing out recall.

use crm_core::market::{MarketConfig, Vertical, normalize};
use crm_core::model::{Company, SignalKind};

/// Vertical slugs a company belongs to.
///
/// Structured evidence first (SIC, NAICS prefix, Wikidata class), then the
/// vertical's name and aliases against the company's industry labels, then
/// keywords. Keywords in the name, description or industry labels count on
/// one hit; the site body is noisier — a restaurant's menu page mentions
/// "brewery" — so it takes two distinct keywords.
pub fn verticals_for(c: &Company, market: &MarketConfig) -> Vec<String> {
    let headline = normalize(&format!(
        "{} {} {}",
        c.name,
        c.description,
        c.industries.join(" ")
    ));
    let body = normalize(&c.body);
    let industries: Vec<String> = c.industries.iter().map(|i| normalize(i)).collect();

    market
        .verticals
        .iter()
        .filter(|v| matches(v, c, &headline, &body, &industries))
        .map(|v| v.slug.clone())
        .collect()
}

fn matches(v: &Vertical, c: &Company, headline: &str, body: &str, industries: &[String]) -> bool {
    if let Some(sic) = &c.sic
        && v.sic.iter().any(|s| s == sic)
    {
        return true;
    }
    if c.naics.iter().any(|code| {
        v.naics
            .iter()
            .any(|prefix| code.starts_with(prefix.as_str()))
    }) {
        return true;
    }
    if c.industry_qids.iter().any(|q| v.wikidata.contains(q)) {
        return true;
    }
    let names: Vec<String> = std::iter::once(&v.name)
        .chain(v.aliases.iter())
        .map(|n| normalize(n))
        .collect();
    if industries.iter().any(|i| names.contains(i)) {
        return true;
    }
    if v.keywords
        .iter()
        .any(|k| contains_phrase(headline, &normalize(k)))
    {
        return true;
    }
    v.keywords
        .iter()
        .filter(|k| contains_phrase(body, &normalize(k)))
        .count()
        >= 2
}

/// Whole-word phrase match over already-normalised text.
pub fn contains_phrase(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = start == 0 || bytes[start - 1] == b' ';
        let after_ok = end == haystack.len() || bytes[end] == b' ';
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
        while from < haystack.len() && !haystack.is_char_boundary(from) {
            from += 1;
        }
    }
    false
}

/// Tools and systems worth knowing a prospect runs, looked for on their
/// site and in their job postings. Profile keywords are added to this.
pub const TECH: &[&str] = &[
    "netsuite",
    "quickbooks",
    "sage intacct",
    "sap",
    "oracle",
    "microsoft dynamics",
    "salesforce",
    "hubspot",
    "zoho",
    "shopify",
    "woocommerce",
    "bigcommerce",
    "magento",
    "stripe",
    "square",
    "toast",
    "ekos",
    "orchestrated beer",
    "vinoshipper",
    "commerce7",
    "shipstation",
    "shipbob",
    "fishbowl",
    "cin7",
    "katana",
    "odoo",
    "workday",
    "gusto",
    "rippling",
    "adp",
    "zendesk",
    "intercom",
    "slack",
    "aws",
    "google cloud",
    "azure",
    "snowflake",
    "segment",
    "marketo",
    "pardot",
];

/// Tech and profile keywords present in some text.
pub fn tech_in(text: &str, market: &MarketConfig) -> Vec<String> {
    let norm = normalize(text);
    let mut out: Vec<String> = TECH
        .iter()
        .map(|s| s.to_string())
        .chain(
            market
                .profiles
                .iter()
                .flat_map(|p| p.keywords.iter().cloned()),
        )
        .filter(|k| contains_phrase(&norm, &normalize(k)))
        .map(|k| k.to_lowercase())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Role families, each with the title fragments that put a posting in it.
const ROLE_FAMILIES: &[(&str, &[&str])] = &[
    (
        "sales",
        &[
            "sales",
            "account executive",
            "account manager",
            "business development",
            "sdr",
            "bdr",
            "partnerships",
        ],
    ),
    (
        "marketing",
        &[
            "marketing",
            "brand",
            "growth",
            "demand generation",
            "content",
        ],
    ),
    (
        "revenue operations",
        &[
            "revenue operations",
            "revops",
            "sales operations",
            "sales ops",
        ],
    ),
    (
        "customer success",
        &[
            "customer success",
            "customer support",
            "support",
            "customer service",
            "account management",
        ],
    ),
    (
        "engineering",
        &[
            "engineer",
            "engineering",
            "developer",
            "software",
            "devops",
            "sre",
        ],
    ),
    (
        "data",
        &["data", "analytics", "analyst", "machine learning"],
    ),
    ("product", &["product manager", "product owner", "product"]),
    ("design", &["designer", "design", "ux"]),
    (
        "operations",
        &["operations", "ops", "general manager", "plant manager"],
    ),
    (
        "supply chain",
        &[
            "supply chain",
            "logistics",
            "procurement",
            "purchasing",
            "buyer",
            "inventory",
            "planner",
            "warehouse",
            "distribution",
            "fulfillment",
            "shipping",
        ],
    ),
    (
        "production",
        &[
            "production",
            "manufacturing",
            "brewer",
            "cellar",
            "packaging",
            "distiller",
            "winemaker",
            "machine operator",
            "maintenance",
        ],
    ),
    ("quality", &["quality", "qa", "food safety", "compliance"]),
    (
        "finance",
        &[
            "finance",
            "controller",
            "accountant",
            "accounting",
            "cfo",
            "bookkeeper",
            "payroll",
            "fp a",
        ],
    ),
    (
        "people",
        &["recruiter", "talent", "human resources", "hr", "people"],
    ),
    (
        "it",
        &[
            "it",
            "information technology",
            "systems administrator",
            "helpdesk",
            "erp",
        ],
    ),
    (
        "leadership",
        &[
            "chief",
            "ceo",
            "coo",
            "cto",
            "vp",
            "vice president",
            "head of",
            "director",
        ],
    ),
];

/// The role families a job title belongs to.
pub fn role_families(title: &str) -> Vec<String> {
    let t = normalize(title);
    ROLE_FAMILIES
        .iter()
        .filter(|(_, frags)| frags.iter().any(|f| contains_phrase(&t, &normalize(f))))
        .map(|(family, _)| family.to_string())
        .collect()
}

/// What kind of signal a headline (plus optional summary) describes.
pub fn news_kind(title: &str, summary: &str) -> SignalKind {
    let t = normalize(&format!("{title} {summary}"));
    const FUNDING: &[&str] = &[
        "raises",
        "raised",
        "funding",
        "funding round",
        "seed round",
        "series a",
        "series b",
        "series c",
        "series d",
        "investment",
        "invests",
        "acquires",
        "acquired",
        "acquisition",
        "merger",
        "merges",
        "ipo",
    ];
    const LEADERSHIP: &[&str] = &[
        "appoints",
        "appointed",
        "names",
        "named",
        "hires",
        "promotes",
        "promoted",
        "joins as",
        "steps down",
        "retires",
        "new ceo",
        "chief executive",
        "chief operating",
        "chief financial",
    ];
    const LAUNCH: &[&str] = &[
        "launches",
        "launched",
        "unveils",
        "introduces",
        "opens",
        "grand opening",
        "expands",
        "expansion",
        "new facility",
        "new location",
        "partnership",
        "partners with",
    ];
    // Headline first: a funding story usually mentions who led the round,
    // which would otherwise read as leadership.
    let title_norm = normalize(title);
    for (kind, words) in [
        (SignalKind::Funding, FUNDING),
        (SignalKind::Leadership, LEADERSHIP),
        (SignalKind::Launch, LAUNCH),
    ] {
        if words.iter().any(|w| contains_phrase(&title_norm, w)) {
            return kind;
        }
    }
    for (kind, words) in [
        (SignalKind::Funding, FUNDING),
        (SignalKind::Leadership, LEADERSHIP),
        (SignalKind::Launch, LAUNCH),
    ] {
        if words.iter().any(|w| contains_phrase(&t, w)) {
            return kind;
        }
    }
    SignalKind::News
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market() -> MarketConfig {
        MarketConfig::load(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../market.example.toml"
        ))
        .unwrap()
    }

    #[test]
    fn structured_codes_classify() {
        let m = market();
        let mut c = Company::new("x", "Some Co", "edgar");
        c.sic = Some("2082".into());
        assert_eq!(verticals_for(&c, &m), vec!["craft-beverage"]);

        let mut c = Company::new("x", "Some Co", "edgar");
        c.naics = vec!["493110".into()];
        assert_eq!(verticals_for(&c, &m), vec!["logistics"]);

        let mut c = Company::new("x", "Some Co", "wikidata");
        c.industry_qids = vec!["Q131734".into()];
        assert_eq!(verticals_for(&c, &m), vec!["craft-beverage"]);
    }

    #[test]
    fn keywords_need_more_evidence_in_the_body_than_the_headline() {
        let m = market();
        let mut c = Company::new("x", "Mesa Taproom", "crawl");
        assert_eq!(verticals_for(&c, &m), vec!["craft-beverage"]);

        c.name = "Joe's Diner".into();
        c.body = "We serve beer from a local brewery.".into();
        assert!(verticals_for(&c, &m).is_empty());

        c.body = "Our brewery and taproom are open daily.".into();
        assert_eq!(verticals_for(&c, &m), vec!["craft-beverage"]);
    }

    #[test]
    fn industry_labels_match_vertical_names_and_aliases() {
        let m = market();
        let mut c = Company::new("x", "Acme", "requests");
        c.industries = vec!["Logistics & warehousing".into()];
        assert_eq!(verticals_for(&c, &m), vec!["logistics"]);
    }

    #[test]
    fn phrases_match_whole_words_only() {
        assert!(contains_phrase("we run netsuite daily", "netsuite"));
        assert!(contains_phrase("netsuite", "netsuite"));
        assert!(!contains_phrase("sapphire", "sap"));
        assert!(!contains_phrase("reopens", "opens"));
        assert!(contains_phrase("sap and oracle", "sap"));
    }

    #[test]
    fn tech_is_found_on_a_page() {
        let t = tech_in("We moved from QuickBooks to NetSuite last year.", &market());
        assert!(t.contains(&"netsuite".to_string()));
        assert!(t.contains(&"quickbooks".to_string()));
        assert!(!t.contains(&"sap".to_string()));
    }

    #[test]
    fn job_titles_map_to_role_families() {
        assert_eq!(role_families("Senior Account Executive"), vec!["sales"]);
        assert!(role_families("Supply Chain Planner").contains(&"supply chain".to_string()));
        assert!(role_families("VP, Operations").contains(&"operations".to_string()));
        assert!(role_families("VP, Operations").contains(&"leadership".to_string()));
        assert!(role_families("Head Brewer").contains(&"production".to_string()));
        assert!(role_families("Barista").is_empty());
    }

    #[test]
    fn headlines_classify_into_signal_kinds() {
        assert_eq!(
            news_kind("Acme raises $20M Series B", ""),
            SignalKind::Funding
        );
        assert_eq!(
            news_kind("Acme appoints Jane Doe as COO", ""),
            SignalKind::Leadership
        );
        assert_eq!(
            news_kind("Acme opens second taproom in Denver", ""),
            SignalKind::Launch
        );
        assert_eq!(news_kind("Acme wins award", ""), SignalKind::News);
        // Headline beats body.
        assert_eq!(
            news_kind("Acme raises $5M", "led by Beta, which named a new partner"),
            SignalKind::Funding
        );
    }
}
