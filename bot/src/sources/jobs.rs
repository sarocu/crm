//! Public job boards: who is hiring, and for what.
//!
//! For every company the crawler found an applicant tracking system for,
//! poll that ATS's public, unauthenticated board API and turn each open
//! posting into a `hiring` signal tagged with its role families. A company
//! hiring three supply-chain planners is telling us something.
//!
//! - Greenhouse: `boards-api.greenhouse.io/v1/boards/{slug}/jobs`
//! - Lever:      `api.lever.co/v0/postings/{slug}?mode=json`
//! - Ashby:      `api.ashbyhq.com/posting-api/job-board/{slug}`

use std::time::Duration;

use anyhow::{Result, bail};
use async_trait::async_trait;
use crm_core::index::COMPANIES;
use crm_core::model::{Doc, Signal, SignalKind, now_ts};
use meilisearch_sdk::search::{SearchQuery, Selectors};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Batch, Ctx, Source, cursor_usize};
use crate::classify;
use crate::sources::crawl::parse_schema_date;

/// Postings kept per company, newest first. A 2,000-role enterprise board
/// says "they hire a lot", which the first few hundred already say.
const MAX_POSTINGS: usize = 200;

pub struct Jobs;

#[derive(Debug, Deserialize)]
struct Board {
    id: String,
    #[serde(default)]
    ats_provider: Option<String>,
    #[serde(default)]
    ats_slug: Option<String>,
}

#[async_trait]
impl Source for Jobs {
    fn name(&self) -> &'static str {
        "jobs"
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }

    async fn fetch(&self, ctx: &Ctx, cursor: Option<Value>) -> Result<Batch> {
        let offset = cursor_usize(&cursor, "offset");
        let per_run = ctx.config.jobs_per_run.max(1);

        let idx = ctx.state.client().index(COMPANIES);
        let fields = ["id", "ats_provider", "ats_slug"];
        let sort = ["name:asc"];
        let mut q = SearchQuery::new(&idx);
        q.with_query("")
            .with_filter("ats_provider EXISTS AND merged_into NOT EXISTS")
            .with_sort(&sort)
            .with_limit(per_run)
            .with_offset(offset)
            .with_attributes_to_retrieve(Selectors::Some(&fields));
        let boards: Vec<Board> = q
            .execute::<Board>()
            .await?
            .hits
            .into_iter()
            .map(|h| h.result)
            .collect();
        let n = boards.len();

        let mut docs = Vec::new();
        for b in boards {
            let (Some(provider), Some(slug)) = (b.ats_provider.as_deref(), b.ats_slug.as_deref())
            else {
                continue;
            };
            match fetch_board(ctx, provider, slug).await {
                Ok(raw) => {
                    let postings = parse_postings(provider, &raw);
                    tracing::debug!(company = %b.id, provider, slug, n = postings.len(), "job board read");
                    docs.extend(
                        postings
                            .into_iter()
                            .take(MAX_POSTINGS)
                            .map(|p| Doc::Signal(p.into_signal(&b.id, provider, slug))),
                    );
                }
                Err(e) => {
                    tracing::warn!(company = %b.id, provider, slug, error = %e, "job board failed")
                }
            }
        }

        // Meilisearch caps offset+limit at 1000 by default; wrap before it.
        let next = offset + n;
        let done = n < per_run || next >= 1000;
        let batch = Batch::new(docs, Some(json!({ "offset": if done { 0 } else { next } })));
        Ok(if done { batch.swept() } else { batch })
    }
}

async fn fetch_board(ctx: &Ctx, provider: &str, slug: &str) -> Result<Value> {
    let slug: String = slug
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    let url = match provider {
        "greenhouse" => format!("https://boards-api.greenhouse.io/v1/boards/{slug}/jobs"),
        "lever" => format!("https://api.lever.co/v0/postings/{slug}?mode=json"),
        "ashby" => format!("https://api.ashbyhq.com/posting-api/job-board/{slug}"),
        other => bail!("unknown ATS provider {other:?}"),
    };
    ctx.http.get_json(&url).await
}

#[derive(Debug, Clone, PartialEq)]
pub struct Posting {
    pub id: String,
    pub title: String,
    pub location: Option<String>,
    pub department: Option<String>,
    pub url: Option<String>,
    pub posted_at: Option<i64>,
}

impl Posting {
    fn into_signal(self, company_id: &str, provider: &str, slug: &str) -> Signal {
        let mut roles = classify::role_families(&self.title);
        if let Some(d) = &self.department {
            roles.extend(classify::role_families(d));
        }
        let mut s = Signal::new(
            company_id,
            SignalKind::Hiring,
            "jobs",
            format!("{provider}:{slug}:{}", self.id),
            format!("Hiring: {}", self.title),
            self.posted_at.unwrap_or_else(now_ts),
        );
        s.roles = roles;
        s.location = self.location;
        s.url = self.url;
        s.summary = self
            .department
            .map(|d| format!("Department: {d}"))
            .unwrap_or_default();
        s
    }
}

/// Postings from any provider's board payload, newest first.
pub fn parse_postings(provider: &str, raw: &Value) -> Vec<Posting> {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let mut out: Vec<Posting> = match provider {
        "greenhouse" => raw
            .get("jobs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|j| {
                Some(Posting {
                    id: j.get("id")?.to_string(),
                    title: s(j, "title")?,
                    location: j
                        .pointer("/location/name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    department: j
                        .pointer("/departments/0/name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    url: s(j, "absolute_url"),
                    posted_at: s(j, "first_published")
                        .or_else(|| s(j, "updated_at"))
                        .and_then(|d| parse_schema_date(&d)),
                })
            })
            .collect(),
        "lever" => raw
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|j| {
                Some(Posting {
                    id: s(j, "id")?,
                    title: s(j, "text")?,
                    location: j
                        .pointer("/categories/location")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    department: j
                        .pointer("/categories/team")
                        .or_else(|| j.pointer("/categories/department"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    url: s(j, "hostedUrl"),
                    // Lever reports milliseconds.
                    posted_at: j
                        .get("createdAt")
                        .and_then(Value::as_i64)
                        .map(|ms| ms / 1000),
                })
            })
            .collect(),
        "ashby" => raw
            .get("jobs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|j| j.get("isListed").and_then(Value::as_bool).unwrap_or(true))
            .filter_map(|j| {
                Some(Posting {
                    id: s(j, "id")?,
                    title: s(j, "title")?,
                    location: s(j, "location"),
                    department: s(j, "department").or_else(|| s(j, "team")),
                    url: s(j, "jobUrl"),
                    posted_at: s(j, "publishedAt").and_then(|d| parse_schema_date(&d)),
                })
            })
            .collect(),
        _ => Vec::new(),
    };
    out.sort_by_key(|p| std::cmp::Reverse(p.posted_at.unwrap_or(0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greenhouse_boards_parse() {
        let raw = json!({"jobs": [
            {"id": 11, "title": "Supply Chain Planner", "absolute_url": "https://x/11",
             "location": {"name": "Denver, CO"}, "updated_at": "2026-09-01T10:00:00-04:00",
             "departments": [{"name": "Operations"}]},
            {"id": 12, "title": "Account Executive", "first_published": "2026-09-10T00:00:00Z"}
        ]});
        let p = parse_postings("greenhouse", &raw);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].title, "Account Executive", "newest first");
        assert_eq!(p[1].location.as_deref(), Some("Denver, CO"));
        assert_eq!(p[1].department.as_deref(), Some("Operations"));
        assert_eq!(p[1].id, "11");
    }

    #[test]
    fn lever_boards_parse() {
        let raw = json!([
            {"id": "abc", "text": "Head Brewer", "hostedUrl": "https://jobs.lever.co/x/abc",
             "createdAt": 1_780_000_000_000i64, "categories": {"location": "Boulder", "team": "Production"}}
        ]);
        let p = parse_postings("lever", &raw);
        assert_eq!(p[0].posted_at, Some(1_780_000_000));
        assert_eq!(p[0].department.as_deref(), Some("Production"));
    }

    #[test]
    fn ashby_boards_parse_and_skip_unlisted() {
        let raw = json!({"jobs": [
            {"id": "1", "title": "SDR", "location": "Remote", "publishedAt": "2026-09-01T00:00:00.000+00:00", "jobUrl": "https://j/1"},
            {"id": "2", "title": "Secret", "isListed": false}
        ]});
        let p = parse_postings("ashby", &raw);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].title, "SDR");
    }

    #[test]
    fn postings_become_hiring_signals_with_roles() {
        let p = Posting {
            id: "9".into(),
            title: "Inventory Planner".into(),
            location: None,
            department: Some("Finance".into()),
            url: None,
            posted_at: Some(5),
        };
        let s = p.into_signal("c1", "greenhouse", "acme");
        assert_eq!(s.kind, SignalKind::Hiring);
        assert_eq!(s.occurred_at, 5);
        assert!(s.roles.contains(&"supply chain".to_string()));
        assert!(s.roles.contains(&"finance".to_string()));
        assert_eq!(s.source_id, "greenhouse:acme:9");
    }

    #[test]
    fn unknown_providers_yield_nothing() {
        assert!(parse_postings("workday", &json!({"jobs": [{"id": 1, "title": "x"}]})).is_empty());
    }
}
