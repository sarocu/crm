//! Indexer bookkeeping, persisted in Meilisearch.
//!
//! Per-source cursors and the crawl frontier live in the `bot_state` index.
//! Meilisearch is the only stateful service in the stack, so keeping state
//! here means the indexer needs no volume of its own and survives VM
//! replacement.
//!
//! It is not a queue, and it is not pretending to be one: a single indexer
//! owns the frontier, and claims are marked before work starts so a restart
//! does not replay the same page forever.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use crm_core::id::stable_id;
use crm_core::index::BOT_STATE;
use crm_core::meili::{self, Client};
use crm_core::model::now_ts;
use meilisearch_sdk::search::{SearchQuery, Selectors};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One row in `bot_state`. A single flat shape keeps the index settings
/// simple; `kind` separates the uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDoc {
    pub id: String,
    /// `cursor` or `frontier`.
    pub kind: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub value: Option<Value>,

    // frontier
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub priority: Option<i64>,
    /// The company whose site this page belongs to.
    #[serde(default)]
    pub company_id: Option<String>,
    #[serde(default)]
    pub discovered_at: Option<i64>,

    pub updated_at: i64,
}

impl StateDoc {
    fn empty(id: String, kind: &str) -> Self {
        Self {
            id,
            kind: kind.into(),
            source: String::new(),
            value: None,
            url: None,
            host: None,
            status: None,
            depth: None,
            priority: None,
            company_id: None,
            discovered_at: None,
            updated_at: now_ts(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FrontierEntry {
    pub url: String,
    pub depth: u32,
    pub company_id: Option<String>,
}

pub struct BotState {
    client: Client,
}

impl BotState {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// The Meilisearch client, for sources that read the market indexes.
    pub fn client(&self) -> &Client {
        &self.client
    }

    fn index(&self) -> meili::Index {
        self.client.index(BOT_STATE)
    }

    // ------------------------------------------------------------- cursors

    pub async fn get_cursor(&self, source: &str) -> Option<Value> {
        let id = format!("cursor-{source}");
        match self.index().get_document::<StateDoc>(&id).await {
            Ok(doc) => doc.value,
            Err(_) => None,
        }
    }

    pub async fn set_cursor(&self, source: &str, value: Option<Value>) -> Result<()> {
        let mut doc = StateDoc::empty(format!("cursor-{source}"), "cursor");
        doc.source = source.into();
        doc.value = value;
        meili::upsert_chunked(&self.client, BOT_STATE, &[doc]).await?;
        Ok(())
    }

    // ------------------------------------------------------------ frontier

    /// Add URLs that are not already known. Existing rows are left alone so
    /// a page already crawled is not silently reset to pending.
    pub async fn frontier_add(&self, entries: &[FrontierEntry]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }
        let ids: Vec<String> = entries.iter().map(|e| frontier_id(&e.url)).collect();
        let known = self.known_ids(&ids).await?;

        let now = now_ts();
        let mut fresh: HashMap<String, StateDoc> = HashMap::new();
        for (e, id) in entries.iter().zip(&ids) {
            if known.contains(id) || fresh.contains_key(id) {
                continue;
            }
            let mut doc = StateDoc::empty(id.clone(), "frontier");
            doc.source = "crawl".into();
            doc.url = Some(e.url.clone());
            doc.host = crate::http::host_of(&e.url).ok();
            doc.status = Some("pending".into());
            doc.depth = Some(e.depth);
            // Homepages first: they carry the JSON-LD and the links to the
            // about and careers pages everything else comes from.
            doc.priority = Some(e.depth as i64);
            doc.company_id = e.company_id.clone();
            doc.discovered_at = Some(now);
            fresh.insert(id.clone(), doc);
        }

        let fresh: Vec<StateDoc> = fresh.into_values().collect();
        let n = fresh.len();
        meili::upsert_chunked(&self.client, BOT_STATE, &fresh).await?;
        Ok(n)
    }

    /// Claim up to `limit` pending URLs, marking them in progress before
    /// returning so a crash cannot leave them to be fetched forever.
    pub async fn frontier_claim(&self, limit: usize) -> Result<Vec<FrontierEntry>> {
        let idx = self.index();
        let sort = ["priority:asc", "discovered_at:asc"];
        let mut q = SearchQuery::new(&idx);
        q.with_query("")
            .with_filter("kind = \"frontier\" AND status = \"pending\"")
            .with_sort(&sort)
            .with_limit(limit);
        let res = q.execute::<StateDoc>().await?;

        let entries: Vec<FrontierEntry> = res
            .hits
            .iter()
            .filter_map(|h| {
                Some(FrontierEntry {
                    url: h.result.url.clone()?,
                    depth: h.result.depth.unwrap_or(0),
                    company_id: h.result.company_id.clone(),
                })
            })
            .collect();

        let claimed: Vec<StateDoc> = res
            .hits
            .into_iter()
            .map(|h| StateDoc {
                status: Some("in_progress".into()),
                updated_at: now_ts(),
                ..h.result
            })
            .collect();
        meili::upsert_chunked(&self.client, BOT_STATE, &claimed).await?;

        Ok(entries)
    }

    pub async fn frontier_finish(&self, url: &str, status: &str) -> Result<()> {
        let id = frontier_id(url);
        let mut doc = match self.index().get_document::<StateDoc>(&id).await {
            Ok(d) => d,
            Err(_) => return Ok(()),
        };
        doc.status = Some(status.to_string());
        doc.updated_at = now_ts();
        meili::upsert_chunked(&self.client, BOT_STATE, &[doc]).await?;
        Ok(())
    }

    pub async fn frontier_pending(&self) -> usize {
        let idx = self.index();
        let mut q = SearchQuery::new(&idx);
        q.with_query("")
            .with_filter("kind = \"frontier\" AND status = \"pending\"")
            .with_limit(0);
        match q.execute::<StateDoc>().await {
            Ok(r) => r.estimated_total_hits.unwrap_or(0),
            Err(_) => 0,
        }
    }

    /// Put every visited homepage back in the pending pool, so a finished
    /// pass starts another and changed sites are eventually re-read.
    pub async fn frontier_reset_roots(&self) -> Result<usize> {
        self.set_status_where(
            "kind = \"frontier\" AND depth = 0 AND status != \"pending\" AND status != \"in_progress\"",
            "pending",
        )
        .await
    }

    /// Return anything claimed but never finished to the pending pool.
    /// Called once on startup, which is what makes a mid-crawl crash safe.
    pub async fn frontier_requeue_stale(&self) -> Result<usize> {
        let n = self
            .set_status_where(
                "kind = \"frontier\" AND status = \"in_progress\"",
                "pending",
            )
            .await?;
        if n > 0 {
            tracing::info!(n, "requeued crawl URLs left in progress by a previous run");
        }
        Ok(n)
    }

    async fn set_status_where(&self, filter: &str, status: &str) -> Result<usize> {
        let idx = self.index();
        let mut total = 0;
        loop {
            let mut q = SearchQuery::new(&idx);
            q.with_query("").with_filter(filter).with_limit(1000);
            let res = q.execute::<StateDoc>().await?;
            if res.hits.is_empty() {
                return Ok(total);
            }
            let batch: Vec<StateDoc> = res
                .hits
                .into_iter()
                .map(|h| StateDoc {
                    status: Some(status.into()),
                    updated_at: now_ts(),
                    ..h.result
                })
                .collect();
            total += batch.len();
            let full = batch.len() == 1000;
            meili::upsert_chunked(&self.client, BOT_STATE, &batch).await?;
            if !full {
                return Ok(total);
            }
        }
    }

    // --------------------------------------------------------------- utils

    async fn known_ids(&self, ids: &[String]) -> Result<HashSet<String>> {
        let rows: Vec<IdOnly> = fetch_by_ids(&self.client, BOT_STATE, ids, Some(&["id"])).await?;
        Ok(rows.into_iter().map(|r| r.id).collect())
    }
}

#[derive(Debug, Deserialize)]
struct IdOnly {
    id: String,
}

pub fn frontier_id(url: &str) -> String {
    format!("f{}", stable_id("frontier", url))
}

/// `field IN ["a", "b"]`, quoted and escaped.
pub fn in_filter(field: &str, values: &[String]) -> String {
    let quoted: Vec<String> = values
        .iter()
        .map(|v| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("{field} IN [{}]", quoted.join(", "))
}

/// Fetch documents by id from any index, 200 at a time.
pub async fn fetch_by_ids<T: DeserializeOwned + Send + Sync + 'static>(
    client: &Client,
    index: &str,
    ids: &[String],
    fields: Option<&[&str]>,
) -> Result<Vec<T>> {
    fetch_where(client, index, "id", ids, fields).await
}

/// Fetch documents whose `field` is any of `values`, 200 values at a time.
pub async fn fetch_where<T: DeserializeOwned + Send + Sync + 'static>(
    client: &Client,
    index: &str,
    field: &str,
    values: &[String],
    fields: Option<&[&str]>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    let idx = client.index(index);
    for chunk in values.chunks(200) {
        let filter = in_filter(field, chunk);
        let mut offset = 0;
        loop {
            let mut q = SearchQuery::new(&idx);
            q.with_query("")
                .with_filter(&filter)
                .with_limit(1000)
                .with_offset(offset);
            if let Some(f) = fields {
                q.with_attributes_to_retrieve(Selectors::Some(f));
            }
            let res = q.execute::<T>().await?;
            let n = res.hits.len();
            out.extend(res.hits.into_iter().map(|h| h.result));
            if n < 1000 {
                break;
            }
            offset += n;
        }
    }
    Ok(out)
}

/// Look up the `content_hash` already indexed for a batch of ids.
///
/// This is what lets the indexer run continuously without rewriting the
/// whole index every cycle: unchanged documents never reach Meilisearch.
pub async fn existing_hashes(
    client: &Client,
    index: &str,
    ids: &[String],
) -> Result<HashMap<String, String>> {
    let rows: Vec<HashRow> =
        fetch_by_ids(client, index, ids, Some(&["id", "content_hash"])).await?;
    Ok(rows.into_iter().map(|r| (r.id, r.content_hash)).collect())
}

#[derive(Debug, Deserialize)]
struct HashRow {
    id: String,
    #[serde(default)]
    content_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontier_ids_are_stable_and_url_safe() {
        let a = frontier_id("https://a.test/x");
        assert_eq!(a, frontier_id("https://a.test/x"));
        assert_ne!(a, frontier_id("https://a.test/y"));
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn in_filters_are_well_formed_and_escaped() {
        assert_eq!(
            in_filter("id", &["aa".into(), "bb".into()]),
            r#"id IN ["aa", "bb"]"#
        );
        assert_eq!(in_filter("cik", &["a\"b".into()]), r#"cik IN ["a\"b"]"#);
    }
}
