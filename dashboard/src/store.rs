//! Everything the dashboard reads. It writes nothing: the BDR agent owns
//! CRM state through MCP, and the indexer owns market data.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use crm_core::index::{
    ACCOUNTS, ACTIVITIES, BOT_STATE, COMPANIES, COMPANY_REQUESTS, PORTFOLIOS, SIGNALS,
};
use crm_core::market::MarketConfig;
use crm_core::meili::Client;
use crm_core::model::{Account, AccountStatus, Activity, Company, Signal};
use crm_core::portfolio::{PortfolioStatus, RuntimePortfolio};
use crm_core::request::CompanyRequest;
use meilisearch_sdk::search::{SearchQuery, Selectors};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Ceiling on a list page, so a runaway table cannot render forever.
pub const PAGE_SIZE: usize = 50;

pub struct Store {
    pub client: Client,
    pub market: MarketConfig,
    http: reqwest::Client,
    bot_health_url: String,
}

#[derive(Debug, Default, Clone)]
pub struct Overview {
    pub documents: BTreeMap<String, Option<usize>>,
    pub freshest: BTreeMap<String, Option<i64>>,
    pub pipeline: BTreeMap<String, usize>,
    pub by_vertical: BTreeMap<String, usize>,
    pub signals_30d: BTreeMap<String, usize>,
    pub pending_requests: usize,
    /// The indexer's `/healthz`, or `None` when it cannot be reached.
    pub bot: Option<BotHealth>,
}

/// The indexer's health payload.
///
/// Every field is optional or defaulted: this crosses a service boundary,
/// and a dashboard that panics because the indexer added a field is worse
/// than one that renders a blank cell.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct BotHealth {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub meilisearch: String,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub crawl_frontier_pending: Option<usize>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub sources: BTreeMap<String, SourceHealth>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct SourceHealth {
    #[serde(default)]
    pub runs: u64,
    #[serde(default)]
    pub last_run_at: Option<i64>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub consecutive_errors: u32,
    #[serde(default)]
    pub swept: u64,
    #[serde(default)]
    pub totals: SourceTotals,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct SourceTotals {
    #[serde(default)]
    pub received: usize,
    #[serde(default)]
    pub invalid: usize,
    #[serde(default)]
    pub orphaned: usize,
    #[serde(default)]
    pub merged: usize,
    #[serde(default)]
    pub unchanged: usize,
    #[serde(default)]
    pub written: usize,
}

pub struct Dossier {
    pub company: Company,
    pub account: Option<Account>,
    pub signals: Vec<Signal>,
    pub activities: Vec<Activity>,
}

impl Store {
    pub fn new(client: Client, market: MarketConfig, bot_health_url: String) -> Self {
        Self {
            client,
            market,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            bot_health_url,
        }
    }

    async fn search<T: DeserializeOwned + Send + Sync + 'static>(
        &self,
        index: &str,
        query: &str,
        filter: &str,
        sort: &[&str],
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<T>, usize)> {
        let idx = self.client.index(index);
        let mut q = SearchQuery::new(&idx);
        q.with_query(query).with_limit(limit).with_offset(offset);
        if !filter.is_empty() {
            q.with_filter(filter);
        }
        if !sort.is_empty() {
            q.with_sort(sort);
        }
        let res = q
            .execute::<T>()
            .await
            .with_context(|| format!("searching {index}"))?;
        let total = res.estimated_total_hits.unwrap_or(res.hits.len());
        Ok((res.hits.into_iter().map(|h| h.result).collect(), total))
    }

    async fn facet(&self, index: &str, field: &str, filter: &str) -> BTreeMap<String, usize> {
        let idx = self.client.index(index);
        let facets = [field];
        let mut q = SearchQuery::new(&idx);
        q.with_query("")
            .with_limit(0)
            .with_facets(Selectors::Some(&facets));
        if !filter.is_empty() {
            q.with_filter(filter);
        }
        match q.execute::<Value>().await {
            Ok(r) => r
                .facet_distribution
                .and_then(|mut d| d.remove(field))
                .unwrap_or_default()
                .into_iter()
                .collect(),
            Err(e) => {
                tracing::warn!(index, field, error = %e, "facet lookup failed");
                Default::default()
            }
        }
    }

    // ------------------------------------------------------------- overview

    pub async fn overview(&self) -> Overview {
        let mut o = Overview::default();
        for (index, field) in [
            (COMPANIES, "updated_at"),
            (SIGNALS, "occurred_at"),
            (ACCOUNTS, "updated_at"),
            (ACTIVITIES, "occurred_at"),
        ] {
            let filter = if index == COMPANIES {
                "merged_into NOT EXISTS"
            } else {
                ""
            };
            let n = self
                .search::<Value>(index, "", filter, &[], 0, 0)
                .await
                .map(|(_, n)| n)
                .ok();
            o.documents.insert(index.to_string(), n);
            let sort = format!("{field}:desc");
            let newest = self
                .search::<Value>(index, "", "", &[sort.as_str()], 1, 0)
                .await
                .ok()
                .and_then(|(v, _)| v.first()?.get(field)?.as_i64());
            o.freshest.insert(index.to_string(), newest);
        }
        let pipeline = self.facet(ACCOUNTS, "status", "").await;
        // Always report every status, so "0 meeting" is visible rather than
        // an absent chip that reads as a broken page.
        for s in AccountStatus::ALL {
            o.pipeline.insert(
                s.as_str().to_string(),
                pipeline.get(s.as_str()).copied().unwrap_or(0),
            );
        }
        o.by_vertical = self
            .facet(COMPANIES, "verticals", "merged_into NOT EXISTS")
            .await;
        let since = crm_core::model::now_ts() - 30 * 86_400;
        o.signals_30d = self
            .facet(SIGNALS, "kind", &format!("occurred_at >= {since}"))
            .await;
        o.pending_requests = self
            .search::<Value>(COMPANY_REQUESTS, "", "status = \"pending\"", &[], 0, 0)
            .await
            .map(|(_, n)| n)
            .unwrap_or(0);
        o.bot = self.bot_health().await;
        o
    }

    async fn bot_health(&self) -> Option<BotHealth> {
        match self.http.get(&self.bot_health_url).send().await {
            Ok(r) if r.status().is_success() => match r.json::<BotHealth>().await {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!(error = %e, "indexer health did not parse");
                    None
                }
            },
            Ok(r) => {
                tracing::warn!(status = %r.status(), "indexer health returned an error status");
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, url = %self.bot_health_url, "indexer health unreachable");
                None
            }
        }
    }

    // ------------------------------------------------------------- accounts

    pub async fn accounts(
        &self,
        status: Option<AccountStatus>,
        query: &str,
        offset: usize,
    ) -> Result<(Vec<Account>, usize)> {
        let filter = status
            .map(|s| format!("status = \"{}\"", s.as_str()))
            .unwrap_or_default();
        self.search(
            ACCOUNTS,
            query,
            &filter,
            &["updated_at:desc"],
            PAGE_SIZE,
            offset,
        )
        .await
    }

    pub async fn dossier(&self, id: &str) -> Option<Dossier> {
        let company = self
            .client
            .index(COMPANIES)
            .get_document::<Company>(id)
            .await
            .ok()?;
        let account = self
            .client
            .index(ACCOUNTS)
            .get_document::<Account>(id)
            .await
            .ok();
        let q = format!("company_id = \"{id}\"");
        let signals = self
            .search(SIGNALS, "", &q, &["occurred_at:desc"], 30, 0)
            .await
            .map(|(v, _)| v)
            .unwrap_or_default();
        let q = format!("account_id = \"{id}\"");
        let activities = self
            .search(ACTIVITIES, "", &q, &["occurred_at:desc"], 100, 0)
            .await
            .map(|(v, _)| v)
            .unwrap_or_default();
        Some(Dossier {
            company,
            account,
            signals,
            activities,
        })
    }

    // ------------------------------------------------------------ companies

    pub async fn companies(
        &self,
        vertical: Option<&str>,
        query: &str,
        offset: usize,
    ) -> Result<(Vec<Company>, usize)> {
        let mut filter = "merged_into NOT EXISTS".to_string();
        if let Some(v) = vertical.and_then(|v| self.market.vertical(v)) {
            filter.push_str(&format!(" AND verticals = \"{}\"", v.slug));
        }
        let sort: &[&str] = if query.is_empty() {
            &["last_signal_at:desc"]
        } else {
            &[]
        };
        self.search(COMPANIES, query, &filter, sort, PAGE_SIZE, offset)
            .await
    }

    pub async fn requests(&self) -> Result<Vec<CompanyRequest>> {
        self.search(
            COMPANY_REQUESTS,
            "",
            "",
            &["requested_at:desc"],
            PAGE_SIZE,
            0,
        )
        .await
        .map(|(v, _)| v)
    }
}

/// One row of the portfolios page.
pub struct PortfolioRow {
    pub portfolio: crm_core::market::Portfolio,
    /// `config` or `runtime`.
    pub origin: &'static str,
    pub enabled: bool,
    pub added_by: Option<String>,
    pub companies: usize,
    pub status: Option<PortfolioStatus>,
}

impl Store {
    pub async fn portfolios(&self) -> Vec<PortfolioRow> {
        let mut rows: Vec<PortfolioRow> = self
            .market
            .portfolios
            .iter()
            .map(|p| PortfolioRow {
                portfolio: p.clone(),
                origin: "config",
                enabled: true,
                added_by: None,
                companies: 0,
                status: None,
            })
            .collect();
        let runtime: Vec<RuntimePortfolio> = self
            .search(PORTFOLIOS, "", "", &["added_at:asc"], 1000, 0)
            .await
            .map(|(v, _)| v)
            .unwrap_or_default();
        for r in runtime {
            if !rows.iter().any(|x| x.portfolio.slug == r.portfolio.slug) {
                rows.push(PortfolioRow {
                    portfolio: r.portfolio,
                    origin: "runtime",
                    enabled: r.enabled,
                    added_by: Some(r.added_by),
                    companies: 0,
                    status: None,
                });
            }
        }
        let counts = self
            .facet(COMPANIES, "investors", "merged_into NOT EXISTS")
            .await;
        for row in &mut rows {
            row.companies = counts.get(&row.portfolio.investor).copied().unwrap_or(0);
            row.status = self
                .client
                .index(BOT_STATE)
                .get_document::<Value>(&PortfolioStatus::state_id(&row.portfolio.slug))
                .await
                .ok()
                .and_then(|d| serde_json::from_value(d.get("value")?.clone()).ok());
        }
        rows
    }
}

/// Ids in filter expressions come from URL paths; only hex ids are real.
pub fn is_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_hex_ids_reach_a_filter() {
        assert!(is_id("63276ab7ed80a24f"));
        assert!(!is_id("x\" OR id = \"y"));
        assert!(!is_id(""));
    }

    #[test]
    fn statuses_are_safe_inside_a_filter() {
        for s in AccountStatus::ALL {
            assert!(s.as_str().chars().all(|c| c.is_ascii_lowercase()));
        }
    }
}
