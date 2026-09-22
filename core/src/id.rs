//! Deterministic identity and change detection.

use serde::Serialize;

/// A stable primary key derived from the source and its own identifier.
///
/// Deterministic by design: re-running a source upserts the same documents
/// instead of duplicating them. The output is hex, which satisfies
/// Meilisearch's primary-key charset (`[a-zA-Z0-9_-]`) for any input.
pub fn stable_id(source: &str, source_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(source.as_bytes());
    hasher.update(b"\x00");
    hasher.update(source_id.as_bytes());
    hasher.finalize().to_hex()[..16].to_string()
}

/// The strongest identity a source has for a company.
///
/// Domain first, because every source can eventually learn it and it is
/// what the crawler, the job boards and the news matcher all key on. A
/// source that only knows a CIK or a Wikidata item gets a provisional id
/// that the pipeline folds into the domain record once one turns up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompanyKey {
    Domain(String),
    Cik(String),
    Wikidata(String),
}

impl CompanyKey {
    pub fn id(&self) -> String {
        match self {
            CompanyKey::Domain(d) => stable_id("company", d),
            CompanyKey::Cik(c) => stable_id("company", &format!("cik:{c}")),
            CompanyKey::Wikidata(q) => stable_id("company", &format!("wd:{q}")),
        }
    }
}

/// The company id for whichever identifiers are known, strongest first.
pub fn company_id(
    domain: Option<&str>,
    cik: Option<&str>,
    wikidata: Option<&str>,
) -> Option<String> {
    domain
        .map(|d| CompanyKey::Domain(d.to_string()))
        .or_else(|| cik.map(|c| CompanyKey::Cik(c.to_string())))
        .or_else(|| wikidata.map(|q| CompanyKey::Wikidata(q.to_string())))
        .map(|k| k.id())
}

/// The registrable domain behind a URL or bare host: `https://shop.acme.co.uk/x`
/// → `acme.co.uk`. Lowercased, `www.` dropped. `None` for IPs, localhost
/// and anything without a public suffix.
pub fn root_domain(url_or_host: &str) -> Option<String> {
    let raw = url_or_host.trim();
    if raw.is_empty() {
        return None;
    }
    // A bare host, maybe with a port or path stuck on, parses as a URL
    // once it has a scheme.
    let parsed = if raw.contains("://") {
        url::Url::parse(raw).ok()?
    } else {
        url::Url::parse(&format!("http://{raw}")).ok()?
    };
    let host = parsed
        .host_str()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host.parse::<std::net::IpAddr>().is_ok() || !host.contains('.') {
        return None;
    }
    let domain = psl::domain_str(&host)?;
    Some(domain.to_string())
}

/// Fields excluded from the content hash because they change on every run
/// even when nothing meaningful did.
const VOLATILE: [&str; 4] = ["indexed_at", "updated_at", "content_hash", "last_signal_at"];

/// Hash of a document's meaningful content.
///
/// The pipeline compares this against what is already indexed and skips the
/// write when they match, so write volume tracks actual change rather than
/// crawl volume.
pub fn content_hash<T: Serialize>(doc: &T) -> String {
    let mut value = match serde_json::to_value(doc) {
        Ok(v) => v,
        Err(_) => return String::from("unhashable"),
    };
    if let Some(obj) = value.as_object_mut() {
        for key in VOLATILE {
            obj.remove(key);
        }
    }
    // serde_json's default map is a BTreeMap, so this rendering is stable
    // across runs regardless of struct field order.
    let canonical = value.to_string();
    blake3::hash(canonical.as_bytes()).to_hex()[..32].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Company;

    #[test]
    fn ids_are_stable_and_distinct() {
        assert_eq!(stable_id("jobs", "1"), stable_id("jobs", "1"));
        assert_ne!(stable_id("jobs", "1"), stable_id("jobs", "2"));
        assert_ne!(stable_id("a", "bc"), stable_id("ab", "c"));
        assert_eq!(stable_id("x", "y").len(), 16);
    }

    #[test]
    fn company_ids_prefer_the_domain() {
        let by_domain = company_id(Some("acme.com"), Some("1"), None).unwrap();
        assert_eq!(by_domain, CompanyKey::Domain("acme.com".into()).id());
        let by_cik = company_id(None, Some("1"), Some("Q1")).unwrap();
        assert_eq!(by_cik, CompanyKey::Cik("1".into()).id());
        assert_ne!(by_domain, by_cik);
        assert!(company_id(None, None, None).is_none());
    }

    #[test]
    fn root_domains_strip_subdomains_and_respect_public_suffixes() {
        assert_eq!(
            root_domain("https://www.Acme.com/about").as_deref(),
            Some("acme.com")
        );
        assert_eq!(
            root_domain("shop.acme.co.uk").as_deref(),
            Some("acme.co.uk")
        );
        assert_eq!(root_domain("acme.com:8443/x").as_deref(), Some("acme.com"));
        assert_eq!(
            root_domain("https://boards.greenhouse.io/acme").as_deref(),
            Some("greenhouse.io")
        );
        assert_eq!(root_domain("http://127.0.0.1/"), None);
        assert_eq!(root_domain("localhost"), None);
        assert_eq!(root_domain(""), None);
    }

    #[test]
    fn content_hash_ignores_bookkeeping() {
        let mut a = Company::new("x", "Same", "crawl");
        let mut b = a.clone();
        a.indexed_at = 1;
        b.indexed_at = 2;
        a.updated_at = 1;
        b.updated_at = 2;
        a.last_signal_at = Some(1);
        assert_eq!(content_hash(&a), content_hash(&b));
        b.name = "Different".into();
        assert_ne!(content_hash(&a), content_hash(&b));
    }
}
