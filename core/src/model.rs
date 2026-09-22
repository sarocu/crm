//! The document schema.
//!
//! Two kinds of record, kept in separate indexes so one writer can never
//! clobber the other:
//!
//! - **Market data** — [`Company`] and [`Signal`] — is written only by the
//!   indexer, from public sources, and is rebuilt on every sweep.
//! - **CRM state** — [`Account`] and [`Activity`] — is written only through
//!   the MCP server, by the BDR agent, and is never touched by a sweep.
//!
//! An account shares its company's `id`, which is the join between the two.

use serde::{Deserialize, Serialize};

use crate::id;

/// Maximum number of characters kept in `body`.
pub const MAX_BODY_CHARS: usize = 8_000;

/// Maximum number of characters kept in `summary` / `description`.
pub const MAX_SUMMARY_CHARS: usize = 600;

// ------------------------------------------------------------------ company

/// A company, merged across every source that knows about it.
///
/// Identity is the registrable domain wherever one is known (see
/// [`crate::id::company_id`]), so EDGAR, Wikidata, the crawler and the job
/// boards all converge on one record. Optional fields are omitted when
/// empty, which is what lets a filter such as `employees NOT EXISTS` find
/// companies whose size we have not learned yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Company {
    pub id: String,
    pub name: String,
    /// Which source supplied `name`; see [`name_rank`].
    #[serde(default)]
    pub name_source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Registrable domain, lowercase, no `www.`: `acme.com`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(default)]
    pub description: String,
    /// Text from the company's own site, for keyword search and matching.
    #[serde(default)]
    pub body: String,

    /// Vertical slugs from the market config. Recomputed on every write.
    #[serde(default)]
    pub verticals: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sic_description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub naics: Vec<String>,
    /// Human-readable industry labels (Wikidata P452, EDGAR SIC text).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub industries: Vec<String>,
    /// Wikidata QIDs of the company's industry and class, for matching.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub industry_qids: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub employees: Option<u64>,
    /// Bucketed `employees`, for faceting: `1-10`, `11-50`, … `10001+`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub employees_band: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revenue_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub founded: Option<i32>,

    /// ISO 3166-1 alpha-2, uppercase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hq_country: Option<String>,
    /// State or province code where known (`CO`), else the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hq_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hq_city: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticker: Option<String>,
    /// SEC Central Index Key, zero-padded to 10 digits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cik: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wikidata_id: Option<String>,

    /// Applicant tracking system hosting the public job board.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ats_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ats_slug: Option<String>,
    /// Tech and need keywords seen on the company's site.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tech: Vec<String>,

    /// Every source that has contributed to this record.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Newest signal about this company, unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_signal_at: Option<i64>,
    /// Set on a record that was folded into another; points at the survivor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_into: Option<String>,

    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub indexed_at: i64,
    #[serde(default)]
    pub content_hash: String,
}

/// How much to trust a source's idea of a company's name. The legal name
/// from EDGAR ("ACME BREWING CO /CO/") is the least readable; the name a
/// company gives itself on its own site is the most.
pub fn name_rank(source: &str) -> u8 {
    match source {
        "crawl" => 4,
        "wikidata" => 3,
        "requests" => 2,
        "jobs" => 1,
        _ => 0,
    }
}

impl Company {
    /// A new, empty record from one source.
    pub fn new(id: impl Into<String>, name: impl Into<String>, source: &str) -> Self {
        let now = now_ts();
        Self {
            id: id.into(),
            name: name.into(),
            name_source: source.to_string(),
            sources: vec![source.to_string()],
            updated_at: now,
            indexed_at: now,
            ..Default::default()
        }
    }

    /// Fold what another source knows into this record.
    ///
    /// Scalars: the incoming value wins when it has one, because it is the
    /// fresher read. The name is the exception and follows [`name_rank`].
    /// Lists are unioned. `verticals` is left alone — the pipeline
    /// recomputes it from the merged record.
    pub fn merge_from(&mut self, other: &Company) {
        let incoming = name_rank(&other.name_source);
        if !other.name.trim().is_empty()
            && (self.name.trim().is_empty() || incoming >= name_rank(&self.name_source))
        {
            if !self.name.is_empty() && self.name != other.name {
                self.aliases.push(self.name.clone());
            }
            self.name = other.name.clone();
            self.name_source = other.name_source.clone();
        } else if !other.name.trim().is_empty() && other.name != self.name {
            self.aliases.push(other.name.clone());
        }

        fn take<T: Clone>(dst: &mut Option<T>, src: &Option<T>) {
            if src.is_some() {
                *dst = src.clone();
            }
        }
        fn union(dst: &mut Vec<String>, src: &[String]) {
            for s in src {
                if !dst.iter().any(|d| d.eq_ignore_ascii_case(s)) {
                    dst.push(s.clone());
                }
            }
        }

        take(&mut self.domain, &other.domain);
        take(&mut self.website, &other.website);
        if !other.description.is_empty()
            && (other.description.len() > 20 || self.description.is_empty())
        {
            self.description = other.description.clone();
        }
        if !other.body.is_empty() {
            self.body = other.body.clone();
        }
        take(&mut self.sic, &other.sic);
        take(&mut self.sic_description, &other.sic_description);
        union(&mut self.naics, &other.naics);
        union(&mut self.industries, &other.industries);
        union(&mut self.industry_qids, &other.industry_qids);
        take(&mut self.employees, &other.employees);
        take(&mut self.revenue_usd, &other.revenue_usd);
        take(&mut self.founded, &other.founded);
        take(&mut self.hq_country, &other.hq_country);
        take(&mut self.hq_state, &other.hq_state);
        take(&mut self.hq_city, &other.hq_city);
        take(&mut self.ticker, &other.ticker);
        take(&mut self.cik, &other.cik);
        take(&mut self.wikidata_id, &other.wikidata_id);
        if other.ats_provider.is_some() && other.ats_slug.is_some() {
            self.ats_provider = other.ats_provider.clone();
            self.ats_slug = other.ats_slug.clone();
        }
        union(&mut self.tech, &other.tech);
        union(&mut self.aliases, &other.aliases);
        union(&mut self.sources, &other.sources);
        self.last_signal_at = match (self.last_signal_at, other.last_signal_at) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }

    /// Normalise text fields, derive the size band and stamp `content_hash`.
    pub fn finalize(mut self) -> Self {
        self.name = collapse_ws(&self.name);
        self.description = truncate_chars(&collapse_ws(&self.description), MAX_SUMMARY_CHARS);
        self.body = truncate_chars(&collapse_ws(&self.body), MAX_BODY_CHARS);
        self.aliases = dedupe_keep_case(&self.aliases, &self.name);
        self.verticals = dedupe_lower(&self.verticals);
        self.industries = dedupe_lower(&self.industries);
        self.tech = dedupe_lower(&self.tech);
        self.sources = dedupe_lower(&self.sources);
        self.hq_country = self
            .hq_country
            .take()
            .map(|c| c.trim().to_uppercase())
            .filter(|c| !c.is_empty());
        self.employees_band = self.employees.map(|n| employees_band(n).to_string());
        self.indexed_at = now_ts();
        self.content_hash = id::content_hash(&self);
        self
    }
}

/// Bucket a headcount. Bands follow the ranges most firmographic tools
/// use, so an agent's "50-200 employees" maps onto one or two of them.
pub fn employees_band(n: u64) -> &'static str {
    match n {
        0..=10 => "1-10",
        11..=50 => "11-50",
        51..=200 => "51-200",
        201..=500 => "201-500",
        501..=1000 => "501-1000",
        1001..=5000 => "1001-5000",
        5001..=10000 => "5001-10000",
        _ => "10001+",
    }
}

// ------------------------------------------------------------------- signal

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalKind {
    /// An open job posting.
    Hiring,
    /// A raise, acquisition or other money event.
    Funding,
    /// A product or location launch.
    Launch,
    /// An executive hire or departure.
    Leadership,
    /// A regulatory filing.
    Filing,
    /// Press coverage that fits none of the above.
    News,
}

impl SignalKind {
    pub const ALL: [SignalKind; 6] = [
        SignalKind::Hiring,
        SignalKind::Funding,
        SignalKind::Launch,
        SignalKind::Leadership,
        SignalKind::Filing,
        SignalKind::News,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            SignalKind::Hiring => "hiring",
            SignalKind::Funding => "funding",
            SignalKind::Launch => "launch",
            SignalKind::Leadership => "leadership",
            SignalKind::Filing => "filing",
            SignalKind::News => "news",
        }
    }

    pub fn parse(s: &str) -> Option<SignalKind> {
        let s = s.trim().to_ascii_lowercase();
        let singular = s.strip_suffix('s').unwrap_or(&s);
        if matches!(singular, "job" | "posting" | "job posting") {
            return Some(SignalKind::Hiring);
        }
        SignalKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s || k.as_str() == singular)
    }
}

impl std::fmt::Display for SignalKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A dated event that suggests a company may be ready to buy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signal {
    pub id: String,
    pub company_id: String,
    /// Denormalised for display, so a signal list reads without a join.
    #[serde(default)]
    pub company_name: String,
    /// Copied from the company, so signals can be filtered by vertical.
    #[serde(default)]
    pub verticals: Vec<String>,
    pub kind: SignalKind,
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub source: String,
    pub source_id: String,
    /// When it happened upstream, unix seconds.
    pub occurred_at: i64,
    /// Normalised role families for a hiring signal: `sales`, `operations`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub indexed_at: i64,
    #[serde(default)]
    pub content_hash: String,
}

impl Signal {
    pub fn new(
        company_id: impl Into<String>,
        kind: SignalKind,
        source: &str,
        source_id: impl Into<String>,
        title: impl Into<String>,
        occurred_at: i64,
    ) -> Self {
        let source_id = source_id.into();
        let now = now_ts();
        Self {
            id: id::stable_id(source, &source_id),
            company_id: company_id.into(),
            company_name: String::new(),
            verticals: Vec::new(),
            kind,
            title: title.into(),
            summary: String::new(),
            url: None,
            source: source.to_string(),
            source_id,
            occurred_at,
            roles: Vec::new(),
            location: None,
            updated_at: now,
            indexed_at: now,
            content_hash: String::new(),
        }
    }

    pub fn finalize(mut self) -> Self {
        self.title = collapse_ws(&self.title);
        self.summary = truncate_chars(&collapse_ws(&self.summary), MAX_SUMMARY_CHARS);
        self.roles = dedupe_lower(&self.roles);
        self.indexed_at = now_ts();
        self.content_hash = id::content_hash(&self);
        self
    }
}

/// What a source hands the pipeline.
///
/// Built once and moved into a batch, never copied around in bulk, so the
/// size gap between the variants costs nothing worth a `Box`.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Doc {
    Company(Company),
    Signal(Signal),
}

// ------------------------------------------------------------------ account

/// Where a company is in our pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountStatus {
    /// Known, not yet looked at.
    New,
    /// Being qualified.
    Researching,
    /// Qualified and waiting for first touch.
    Queued,
    /// First outreach sent, no reply yet.
    Contacted,
    /// They replied.
    Engaged,
    /// A meeting is booked or held.
    Meeting,
    /// Handed to an AE.
    Qualified,
    /// Not a fit. Terminal, and needs a reason.
    Disqualified,
    /// Not now; revisit at `next_touch_at`.
    Nurture,
}

impl AccountStatus {
    pub const ALL: [AccountStatus; 9] = [
        AccountStatus::New,
        AccountStatus::Researching,
        AccountStatus::Queued,
        AccountStatus::Contacted,
        AccountStatus::Engaged,
        AccountStatus::Meeting,
        AccountStatus::Qualified,
        AccountStatus::Disqualified,
        AccountStatus::Nurture,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            AccountStatus::New => "new",
            AccountStatus::Researching => "researching",
            AccountStatus::Queued => "queued",
            AccountStatus::Contacted => "contacted",
            AccountStatus::Engaged => "engaged",
            AccountStatus::Meeting => "meeting",
            AccountStatus::Qualified => "qualified",
            AccountStatus::Disqualified => "disqualified",
            AccountStatus::Nurture => "nurture",
        }
    }

    pub fn parse(s: &str) -> Option<AccountStatus> {
        let s = s.trim().to_ascii_lowercase();
        AccountStatus::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// Someone has already reached out, so prospecting should skip it.
    pub fn is_worked(&self) -> bool {
        matches!(
            self,
            AccountStatus::Contacted
                | AccountStatus::Engaged
                | AccountStatus::Meeting
                | AccountStatus::Qualified
                | AccountStatus::Disqualified
        )
    }

    pub fn worked() -> Vec<AccountStatus> {
        AccountStatus::ALL
            .into_iter()
            .filter(AccountStatus::is_worked)
            .collect()
    }
}

impl std::fmt::Display for AccountStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Our relationship with one company. `id` is the company's id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    #[serde(default)]
    pub company_name: String,
    #[serde(default)]
    pub domain: Option<String>,
    pub status: AccountStatus,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub next_step: Option<String>,
    #[serde(default)]
    pub next_touch_at: Option<i64>,
    #[serde(default)]
    pub disqualify_reason: Option<String>,
    /// The profile this account was qualified against.
    #[serde(default)]
    pub fit_profile: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub last_activity_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub updated_by: String,
}

impl Account {
    pub fn new(company_id: &str, company_name: &str, actor: &str) -> Self {
        let now = now_ts();
        Self {
            id: company_id.to_string(),
            company_name: company_name.to_string(),
            domain: None,
            status: AccountStatus::New,
            owner: None,
            next_step: None,
            next_touch_at: None,
            disqualify_reason: None,
            fit_profile: None,
            tags: Vec::new(),
            last_activity_at: None,
            created_at: now,
            updated_at: now,
            updated_by: actor.to_string(),
        }
    }
}

// ----------------------------------------------------------------- activity

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityType {
    Email,
    Call,
    Linkedin,
    Meeting,
    Note,
    /// Written automatically when `update_account` changes the status.
    StatusChange,
}

impl ActivityType {
    pub const ALL: [ActivityType; 6] = [
        ActivityType::Email,
        ActivityType::Call,
        ActivityType::Linkedin,
        ActivityType::Meeting,
        ActivityType::Note,
        ActivityType::StatusChange,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            ActivityType::Email => "email",
            ActivityType::Call => "call",
            ActivityType::Linkedin => "linkedin",
            ActivityType::Meeting => "meeting",
            ActivityType::Note => "note",
            ActivityType::StatusChange => "status_change",
        }
    }

    pub fn parse(s: &str) -> Option<ActivityType> {
        let s = s.trim().to_ascii_lowercase().replace([' ', '-'], "_");
        ActivityType::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

impl std::fmt::Display for ActivityType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One touch, note or status change on an account. Append-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub id: String,
    pub account_id: String,
    #[serde(default)]
    pub company_name: String,
    #[serde(rename = "type")]
    pub activity_type: ActivityType,
    /// `outbound` or `inbound`.
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub summary: String,
    /// e.g. `no_answer`, `replied`, `bounced`, `booked`.
    #[serde(default)]
    pub outcome: Option<String>,
    pub occurred_at: i64,
    pub actor: String,
    pub created_at: i64,
}

// ------------------------------------------------------------------ helpers

pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Collapse all runs of whitespace to a single space and trim.
pub fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    out
}

/// Truncate to `max` characters on a char boundary, appending an ellipsis
/// when anything was actually cut.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    if let Some(sp) = out.rfind(' ')
        && sp > max.saturating_sub(1) * 4 / 5
    {
        out.truncate(sp);
    }
    out.push('…');
    out
}

fn dedupe_lower(v: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(v.len());
    for item in v {
        let c = collapse_ws(item).to_lowercase();
        if !c.is_empty() && seen.insert(c.clone()) {
            out.push(c);
        }
    }
    out
}

/// Dedupe case-insensitively but keep the first spelling, dropping any
/// alias that is just the primary name again.
fn dedupe_keep_case(v: &[String], primary: &str) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    seen.insert(primary.to_lowercase());
    let mut out = Vec::new();
    for item in v {
        let c = collapse_ws(item);
        if !c.is_empty() && seen.insert(c.to_lowercase()) {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn company(source: &str, name: &str) -> Company {
        Company::new("c1", name, source)
    }

    #[test]
    fn finalize_normalises_and_bands() {
        let mut c = company("crawl", "  Acme   Brewing ");
        c.employees = Some(120);
        c.hq_country = Some(" us ".into());
        c.aliases = vec!["acme brewing".into(), "Acme Beer".into()];
        let c = c.finalize();
        assert_eq!(c.name, "Acme Brewing");
        assert_eq!(c.employees_band.as_deref(), Some("51-200"));
        assert_eq!(c.hq_country.as_deref(), Some("US"));
        assert_eq!(c.aliases, vec!["Acme Beer"]);
        assert!(!c.content_hash.is_empty());
    }

    #[test]
    fn merge_prefers_better_names_and_fresher_scalars() {
        let mut base = company("edgar", "ACME BREWING CO /CO/");
        base.cik = Some("0000000001".into());
        base.employees = Some(40);
        base.sources = vec!["edgar".into()];

        let mut site = company("crawl", "Acme Brewing");
        site.domain = Some("acme.test".into());
        site.employees = Some(55);
        site.tech = vec!["netsuite".into()];

        base.merge_from(&site);
        assert_eq!(base.name, "Acme Brewing");
        assert!(base.aliases.contains(&"ACME BREWING CO /CO/".to_string()));
        assert_eq!(base.cik.as_deref(), Some("0000000001"));
        assert_eq!(base.domain.as_deref(), Some("acme.test"));
        assert_eq!(base.employees, Some(55));
        assert_eq!(base.sources, vec!["edgar", "crawl"]);

        // A lower-ranked name does not displace a better one.
        base.merge_from(&company("edgar", "ACME BREWING CO"));
        assert_eq!(base.name, "Acme Brewing");
    }

    #[test]
    fn optional_fields_are_omitted_so_exists_filters_work() {
        let v = serde_json::to_value(company("crawl", "X").finalize()).unwrap();
        assert!(v.get("employees").is_none());
        assert!(v.get("hq_country").is_none());
        assert!(v.get("verticals").is_some());
    }

    #[test]
    fn bands_cover_the_range() {
        assert_eq!(employees_band(1), "1-10");
        assert_eq!(employees_band(50), "11-50");
        assert_eq!(employees_band(51), "51-200");
        assert_eq!(employees_band(1_000_000), "10001+");
    }

    #[test]
    fn enums_round_trip_through_their_wire_form() {
        for k in SignalKind::ALL {
            assert_eq!(SignalKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(SignalKind::parse("Jobs"), Some(SignalKind::Hiring));
        assert_eq!(SignalKind::parse("filings"), Some(SignalKind::Filing));
        for s in AccountStatus::ALL {
            assert_eq!(AccountStatus::parse(s.as_str()), Some(s));
            assert_eq!(
                serde_json::to_value(s).unwrap(),
                serde_json::Value::String(s.as_str().into())
            );
        }
        for t in ActivityType::ALL {
            assert_eq!(ActivityType::parse(t.as_str()), Some(t));
        }
        assert_eq!(
            ActivityType::parse("status change"),
            Some(ActivityType::StatusChange)
        );
    }

    #[test]
    fn worked_statuses_exclude_the_top_of_the_funnel() {
        assert!(!AccountStatus::New.is_worked());
        assert!(!AccountStatus::Queued.is_worked());
        assert!(!AccountStatus::Nurture.is_worked());
        assert!(AccountStatus::Contacted.is_worked());
        assert!(AccountStatus::Disqualified.is_worked());
    }

    #[test]
    fn signal_ids_are_stable_per_source() {
        let a = Signal::new("c", SignalKind::Hiring, "jobs", "gh:1", "SDR", 0);
        let b = Signal::new("c", SignalKind::Hiring, "jobs", "gh:1", "SDR", 5);
        assert_eq!(a.id, b.id);
    }

    #[test]
    fn truncate_breaks_on_a_word_boundary_and_marks_the_cut() {
        let s = "alpha beta gamma delta epsilon zeta";
        let out = truncate_chars(s, 20);
        assert!(out.chars().count() <= 20);
        assert!(out.ends_with('…'));
        assert_eq!(truncate_chars("short", 20), "short");
    }
}
