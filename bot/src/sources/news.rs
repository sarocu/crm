//! Trade-press and newswire feeds, matched to the companies we track.
//!
//! Each run reads a slice of the configured RSS/Atom feeds. An item becomes
//! a signal for a company when it links to that company's domain, or when
//! the company's name appears in the headline or summary as whole words.
//! Items that match nothing are dropped: an unattributed headline is not
//! something a salesperson can act on.

use std::collections::BTreeSet;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use crm_core::id::root_domain;
use crm_core::index::COMPANIES;
use crm_core::market::normalize;
use crm_core::model::{Doc, Signal, collapse_ws, now_ts};
use meilisearch_sdk::search::{SearchQuery, Selectors};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Batch, Ctx, Source, cursor_usize};
use crate::classify;
use crate::state::fetch_where;

const FEEDS_PER_RUN: usize = 10;
const MAX_AGE_DAYS: i64 = 365;

/// Domains that appear in press releases without being about the company.
const NOISE_DOMAINS: &[&str] = &[
    "twitter.com",
    "x.com",
    "linkedin.com",
    "facebook.com",
    "instagram.com",
    "youtube.com",
    "google.com",
    "apple.com",
    "prnewswire.com",
    "businesswire.com",
    "globenewswire.com",
    "accesswire.com",
    "einpresswire.com",
    "feedburner.com",
    "wordpress.com",
    "medium.com",
];

/// Words that do not identify a company on their own.
const NAME_NOISE: &[&str] = &[
    "inc",
    "llc",
    "ltd",
    "co",
    "corp",
    "corporation",
    "company",
    "the",
    "group",
    "holdings",
];

static HREF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)href\s*=\s*["']([^"']+)["']"#).unwrap());

pub struct News;

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub url: Option<String>,
    pub published: Option<i64>,
    /// Every registrable domain the item links to.
    pub domains: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct NameRow {
    id: String,
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
}

#[async_trait]
impl Source for News {
    fn name(&self) -> &'static str {
        "news"
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(30 * 60)
    }

    async fn fetch(&self, ctx: &Ctx, cursor: Option<Value>) -> Result<Batch> {
        let mut feeds = ctx.market.feeds();
        feeds.extend(ctx.config.news_feeds.iter().map(|u| u.to_string()));
        feeds.sort();
        feeds.dedup();
        if feeds.is_empty() {
            tracing::debug!("no feeds configured; the news source has nothing to do");
            return Ok(Batch::empty());
        }
        let start = cursor_usize(&cursor, "pos") % feeds.len();
        let end = (start + FEEDS_PER_RUN).min(feeds.len());

        let cutoff = now_ts() - MAX_AGE_DAYS * 86_400;
        let mut docs = Vec::new();
        for feed_url in &feeds[start..end] {
            let body = match ctx.http.get_text(feed_url).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(feed = %feed_url, error = %e, "feed fetch failed");
                    continue;
                }
            };
            let items = match parse_feed(&body, feed_url) {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!(feed = %feed_url, error = %e, "feed did not parse");
                    continue;
                }
            };
            let items: Vec<Item> = items
                .into_iter()
                .filter(|i| i.published.is_none_or(|p| p >= cutoff))
                .collect();
            let mut matched = 0;
            for item in &items {
                for company in match_companies(ctx, item).await? {
                    let mut s = Signal::new(
                        &company,
                        classify::news_kind(&item.title, &item.summary),
                        "news",
                        format!("{}#{company}", item.id),
                        item.title.clone(),
                        item.published.unwrap_or_else(now_ts),
                    );
                    s.summary = item.summary.clone();
                    s.url = item.url.clone();
                    docs.push(Doc::Signal(s));
                    matched += 1;
                }
            }
            tracing::info!(feed = %feed_url, items = items.len(), matched, "feed read");
        }

        let batch = if end >= feeds.len() {
            Batch::new(docs, Some(json!({ "pos": 0 }))).swept()
        } else {
            Batch::new(docs, Some(json!({ "pos": end })))
        };
        Ok(batch)
    }
}

/// Parse any RSS/Atom/JSON feed into items.
pub fn parse_feed(body: &str, feed_url: &str) -> Result<Vec<Item>> {
    let feed = feed_rs::parser::parse(body.as_bytes())?;
    let own = root_domain(feed_url);
    Ok(feed
        .entries
        .into_iter()
        .filter_map(|e| {
            let title = collapse_ws(&e.title.as_ref()?.content);
            if title.is_empty() {
                return None;
            }
            let html = e
                .summary
                .as_ref()
                .map(|s| s.content.clone())
                .or_else(|| e.content.as_ref().and_then(|c| c.body.clone()))
                .unwrap_or_default();
            let url = e.links.first().map(|l| l.href.clone());
            let mut domains: Vec<String> = HREF
                .captures_iter(&html)
                .filter_map(|c| root_domain(&c[1]))
                .chain(e.links.iter().filter_map(|l| root_domain(&l.href)))
                .filter(|d| Some(d) != own.as_ref() && !NOISE_DOMAINS.contains(&d.as_str()))
                .collect();
            domains.sort();
            domains.dedup();
            Some(Item {
                id: if e.id.is_empty() {
                    url.clone().unwrap_or_else(|| title.clone())
                } else {
                    e.id
                },
                summary: crm_core::model::truncate_chars(&strip_tags(&html), 600),
                title,
                url,
                published: e.published.or(e.updated).map(|d| d.timestamp()),
                domains,
            })
        })
        .collect())
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    collapse_ws(
        &out.replace("&amp;", "&")
            .replace("&nbsp;", " ")
            .replace("&#39;", "'")
            .replace("&quot;", "\""),
    )
}

/// The companies an item is about: by linked domain, then by name.
async fn match_companies(ctx: &Ctx, item: &Item) -> Result<Vec<String>> {
    let client = ctx.state.client();
    let fields: &[&str] = &["id", "name", "aliases"];
    let mut found: BTreeSet<String> = BTreeSet::new();

    if !item.domains.is_empty() {
        let rows: Vec<NameRow> =
            fetch_where(client, COMPANIES, "domain", &item.domains, Some(fields)).await?;
        for r in rows {
            found.insert(r.id);
        }
    }

    let idx = client.index(COMPANIES);
    let mut q = SearchQuery::new(&idx);
    q.with_query(&item.title)
        .with_filter("merged_into NOT EXISTS")
        .with_limit(5)
        .with_attributes_to_retrieve(Selectors::Some(fields));
    let text = normalize(&format!("{} {}", item.title, item.summary));
    for h in q.execute::<NameRow>().await?.hits {
        if name_appears(&h.result, &text) {
            found.insert(h.result.id);
        }
    }
    Ok(found.into_iter().collect())
}

fn name_appears(row: &NameRow, text: &str) -> bool {
    std::iter::once(&row.name)
        .chain(row.aliases.iter())
        .map(|n| distinctive(n))
        .any(|n| n.len() >= 5 && classify::contains_phrase(text, &n))
}

/// A name with its legal suffixes dropped: "Acme Brewing Co., Inc." →
/// "acme brewing". Returns empty when nothing distinctive is left.
pub fn distinctive(name: &str) -> String {
    let words: Vec<String> = normalize(name).split(' ').map(str::to_string).collect();
    let mut end = words.len();
    while end > 0 && NAME_NOISE.contains(&words[end - 1].as_str()) {
        end -= 1;
    }
    let kept = &words[..end];
    if kept.iter().all(|w| NAME_NOISE.contains(&w.as_str())) {
        return String::new();
    }
    kept.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?>
      <rss version="2.0"><channel><title>Trade Press</title><link>https://press.test</link>
        <item>
          <title>Acme Brewing raises $12M to expand canning</title>
          <link>https://press.test/acme-raises</link>
          <guid>press-1</guid>
          <pubDate>Tue, 01 Sep 2026 12:00:00 GMT</pubDate>
          <description><![CDATA[<p>Denver's <a href="https://www.acmebrewing.test/">Acme Brewing</a> closed a round.
            <a href="https://press.test/other">more</a> <a href="https://twitter.com/acme">tw</a></p>]]></description>
        </item>
        <item><title></title><link>https://press.test/empty</link></item>
      </channel></rss>"#;

    #[test]
    fn rss_items_parse_with_linked_domains() {
        let items = parse_feed(RSS, "https://press.test/feed").unwrap();
        assert_eq!(items.len(), 1, "an untitled item is dropped");
        let i = &items[0];
        assert_eq!(i.id, "press-1");
        assert_eq!(i.url.as_deref(), Some("https://press.test/acme-raises"));
        assert_eq!(
            i.domains,
            vec!["acmebrewing.test"],
            "own and noise domains are dropped"
        );
        assert!(
            i.summary
                .starts_with("Denver's Acme Brewing closed a round.")
        );
        assert!(i.published.is_some());
    }

    #[test]
    fn names_match_on_their_distinctive_part() {
        assert_eq!(distinctive("Acme Brewing Co., Inc."), "acme brewing");
        assert_eq!(distinctive("The Company"), "");
        let row = NameRow {
            id: "1".into(),
            name: "Acme Brewing Company".into(),
            aliases: vec![],
        };
        assert!(name_appears(&row, &normalize("Acme Brewing raises $12M")));
        assert!(!name_appears(&row, &normalize("Acme Brewingworks opens")));
        let short = NameRow {
            id: "2".into(),
            name: "Ace".into(),
            aliases: vec![],
        };
        assert!(
            !name_appears(&short, &normalize("Ace hardware news")),
            "short names are too ambiguous to match on text"
        );
    }

    #[test]
    fn tags_are_stripped_from_summaries() {
        assert_eq!(strip_tags("<p>A &amp; B</p><br/>C"), "A & B C");
    }
}
