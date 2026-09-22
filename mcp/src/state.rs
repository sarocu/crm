//! Shared server state, and every read and write the tools make.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use crm_core::id::{company_id, root_domain};
use crm_core::index::{ACCOUNTS, ACTIVITIES, COMPANIES, COMPANY_REQUESTS, SIGNALS};
use crm_core::market::MarketConfig;
use crm_core::meili::{self, Client};
use crm_core::model::{Account, AccountStatus, Activity, Company, Signal};
use crm_core::request::CompanyRequest;
use meilisearch_sdk::errors::{Error as MeiliError, ErrorCode};
use meilisearch_sdk::search::{SearchQuery, Selectors};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::filter::{Filter, quote};

/// Company fields returned by searches. `body` is left out: it is up to
/// 8,000 characters of site text per company and would swamp a result list.
pub const COMPANY_FIELDS: &[&str] = &[
    "id",
    "name",
    "name_source",
    "aliases",
    "domain",
    "website",
    "description",
    "verticals",
    "sic",
    "sic_description",
    "naics",
    "industries",
    "industry_qids",
    "employees",
    "employees_band",
    "revenue_usd",
    "founded",
    "hq_country",
    "hq_state",
    "hq_city",
    "ticker",
    "cik",
    "wikidata_id",
    "ats_provider",
    "ats_slug",
    "tech",
    "sources",
    "last_signal_at",
    "merged_into",
    "updated_at",
];

/// Meilisearch's default ceiling on `offset + limit` for a search.
pub const MAX_HITS: usize = 1000;

/// Every company query starts here.
pub const LIVE: &str = "merged_into NOT EXISTS";

#[derive(Debug)]
pub struct AppState {
    /// Reads every index. Search and document reads only.
    pub client: Client,
    /// Writes CRM state only: accounts, activities, company requests.
    writer: Client,
    pub market: MarketConfig,
}

pub struct Page<T> {
    pub hits: Vec<T>,
    pub estimated_total: usize,
}

impl AppState {
    pub fn new(client: Client, writer: Client, market: MarketConfig) -> Self {
        Self {
            client,
            writer,
            market,
        }
    }

    // ------------------------------------------------------------ generic

    /// One search, with every knob Meilisearch has that the tools use.
    /// Positional on purpose: each call site reads as a single query.
    #[allow(clippy::too_many_arguments)]
    pub async fn search<T: DeserializeOwned + Send + Sync + 'static>(
        &self,
        index: &str,
        query: &str,
        filter: &str,
        sort: &[&str],
        fields: Option<&[&str]>,
        limit: usize,
        offset: usize,
    ) -> Result<Page<T>> {
        let idx = self.client.index(index);
        let mut q = SearchQuery::new(&idx);
        q.with_query(query).with_limit(limit).with_offset(offset);
        if !filter.is_empty() {
            q.with_filter(filter);
        }
        if !sort.is_empty() {
            q.with_sort(sort);
        }
        if let Some(f) = fields {
            q.with_attributes_to_retrieve(Selectors::Some(f));
        }
        let res = q
            .execute::<T>()
            .await
            .with_context(|| format!("searching {index}"))?;
        Ok(Page {
            estimated_total: res.estimated_total_hits.unwrap_or(res.hits.len()),
            hits: res.hits.into_iter().map(|h| h.result).collect(),
        })
    }

    async fn get<T: DeserializeOwned + Send + Sync + 'static>(
        &self,
        index: &str,
        id: &str,
    ) -> Result<Option<T>> {
        match self.client.index(index).get_document::<T>(id).await {
            Ok(d) => Ok(Some(d)),
            Err(MeiliError::Meilisearch(e))
                if matches!(
                    e.error_code,
                    ErrorCode::DocumentNotFound | ErrorCode::IndexNotFound
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e).with_context(|| format!("reading {index}/{id}")),
        }
    }

    // ---------------------------------------------------------- companies

    /// Resolve an id, a domain or URL, or a name to one live company,
    /// following any merge.
    pub async fn resolve_company(&self, key: &str) -> Result<Option<Company>> {
        let key = key.trim();
        if key.is_empty() {
            return Ok(None);
        }
        let mut found: Option<Company> = None;
        if key.len() == 16 && key.chars().all(|c| c.is_ascii_hexdigit()) {
            found = self.get(COMPANIES, key).await?;
        }
        if found.is_none()
            && let Some(domain) = root_domain(key)
            && let Some(id) = company_id(Some(&domain), None, None)
        {
            found = self.get(COMPANIES, &id).await?;
        }
        if found.is_none() {
            let page: Page<Company> = self.search(COMPANIES, key, LIVE, &[], None, 1, 0).await?;
            // Only an unambiguous name match: the text must contain the whole
            // query, or the agent gets a confidently wrong company.
            found = page.hits.into_iter().next().filter(|c| {
                let n = crm_core::market::normalize(&c.name);
                let q = crm_core::market::normalize(key);
                q.len() >= 3
                    && (n == q
                        || n.starts_with(&q)
                        || c.aliases
                            .iter()
                            .any(|a| crm_core::market::normalize(a) == q))
            });
        }
        // Follow at most a couple of merges.
        for _ in 0..3 {
            match found.as_ref().and_then(|c| c.merged_into.clone()) {
                Some(to) => found = self.get(COMPANIES, &to).await?,
                None => break,
            }
        }
        Ok(found)
    }

    // ------------------------------------------------------------ signals

    /// Newest signals for each company, one multi-search round trip.
    pub async fn signals_for(
        &self,
        company_ids: &[String],
        since: Option<i64>,
        per_company: usize,
    ) -> Result<HashMap<String, Vec<Signal>>> {
        let mut out: HashMap<String, Vec<Signal>> = HashMap::new();
        if company_ids.is_empty() {
            return Ok(out);
        }
        let idx = self.client.index(SIGNALS);
        let sort = ["occurred_at:desc"];
        for chunk in company_ids.chunks(100) {
            let filters: Vec<String> = chunk
                .iter()
                .map(|id| {
                    let mut f = Filter::default();
                    f.eq("company_id", id);
                    if let Some(s) = since {
                        f.gte("occurred_at", s);
                    }
                    f.build()
                })
                .collect();
            let queries: Vec<_> = filters
                .iter()
                .map(|f| {
                    let mut q = SearchQuery::new(&idx);
                    q.with_query("")
                        .with_filter(f)
                        .with_sort(&sort)
                        .with_limit(per_company);
                    q
                })
                .collect();
            let mut multi = self.client.multi_search();
            for q in &queries {
                multi.with_search_query(q.clone());
            }
            let res = multi.execute::<Signal>().await.context("reading signals")?;
            for (id, r) in chunk.iter().zip(res.results) {
                out.insert(id.clone(), r.hits.into_iter().map(|h| h.result).collect());
            }
        }
        Ok(out)
    }

    /// Company ids with any signal of these kinds since a time, via a facet
    /// over the signals index.
    pub async fn companies_with_signals(&self, filter: &str) -> Result<Vec<String>> {
        Ok(self
            .facet(SIGNALS, "company_id", filter)
            .await?
            .into_keys()
            .collect())
    }

    // ----------------------------------------------------------- accounts

    pub async fn account(&self, id: &str) -> Result<Option<Account>> {
        self.get(ACCOUNTS, id).await
    }

    pub async fn accounts_by_ids(&self, ids: &[String]) -> Result<HashMap<String, Account>> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(200) {
            let mut f = Filter::default();
            f.any_of("id", chunk);
            let page: Page<Account> = self
                .search(ACCOUNTS, "", &f.build(), &[], None, chunk.len(), 0)
                .await?;
            out.extend(page.hits.into_iter().map(|a| (a.id.clone(), a)));
        }
        Ok(out)
    }

    /// Ids of accounts matching a filter, up to the search ceiling.
    pub async fn account_ids(&self, filter: &str) -> Result<Vec<String>> {
        #[derive(serde::Deserialize)]
        struct Id {
            id: String,
        }
        let page: Page<Id> = self
            .search(ACCOUNTS, "", filter, &[], Some(&["id"]), MAX_HITS, 0)
            .await?;
        Ok(page.hits.into_iter().map(|i| i.id).collect())
    }

    /// Accounts prospecting should skip: already worked, or parked in
    /// nurture until a date that has not come yet.
    pub async fn skip_ids(&self, now: i64) -> Result<Vec<String>> {
        let worked: Vec<String> = AccountStatus::worked()
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        let mut f = Filter::default();
        f.any_of("status", &worked);
        let mut ids = self.account_ids(&f.build()).await?;
        ids.extend(
            self.account_ids(&format!("status = \"nurture\" AND next_touch_at > {now}"))
                .await?,
        );
        Ok(ids)
    }

    pub async fn activities(&self, account_id: &str, limit: usize) -> Result<Vec<Activity>> {
        let page: Page<Activity> = self
            .search(
                ACTIVITIES,
                "",
                &format!("account_id = {}", quote(account_id)),
                &["occurred_at:desc"],
                None,
                limit,
                0,
            )
            .await?;
        Ok(page.hits)
    }

    pub async fn write_account(&self, account: &Account) -> Result<()> {
        meili::upsert_chunked(&self.writer, ACCOUNTS, std::slice::from_ref(account))
            .await
            .context("writing the account")?;
        Ok(())
    }

    pub async fn write_activities(&self, activities: &[Activity]) -> Result<()> {
        meili::upsert_chunked(&self.writer, ACTIVITIES, activities)
            .await
            .context("writing the activity")?;
        Ok(())
    }

    // ----------------------------------------------------------- requests

    pub async fn pending_request(&self, domain: &str) -> Result<Option<CompanyRequest>> {
        let page: Page<CompanyRequest> = self
            .search(
                COMPANY_REQUESTS,
                "",
                &format!("domain = {} AND status = \"pending\"", quote(domain)),
                &[],
                None,
                1,
                0,
            )
            .await?;
        Ok(page.hits.into_iter().next())
    }

    pub async fn write_request(&self, req: &CompanyRequest) -> Result<()> {
        meili::upsert_chunked(&self.writer, COMPANY_REQUESTS, std::slice::from_ref(req))
            .await
            .context("writing the request")?;
        Ok(())
    }

    // -------------------------------------------------------------- stats

    pub async fn facet(
        &self,
        index: &str,
        field: &str,
        filter: &str,
    ) -> Result<BTreeMap<String, usize>> {
        let idx = self.client.index(index);
        let fields = [field];
        let mut q = SearchQuery::new(&idx);
        q.with_query("")
            .with_limit(0)
            .with_facets(Selectors::Some(&fields));
        if !filter.is_empty() {
            q.with_filter(filter);
        }
        let res = q.execute::<Value>().await?;
        Ok(res
            .facet_distribution
            .and_then(|mut d| d.remove(field))
            .unwrap_or_default()
            .into_iter()
            .collect())
    }

    /// Document count, or `None` when it cannot be read — never zero for an
    /// error, which would tell the caller every search will come back empty.
    pub async fn count(&self, index: &str, filter: &str) -> Option<usize> {
        let idx = self.client.index(index);
        let mut q = SearchQuery::new(&idx);
        q.with_query("").with_limit(0);
        if !filter.is_empty() {
            q.with_filter(filter);
        }
        match q.execute::<Value>().await {
            Ok(res) => res.estimated_total_hits,
            Err(MeiliError::Meilisearch(e)) if e.error_code == ErrorCode::IndexNotFound => Some(0),
            Err(e) => {
                tracing::warn!(index, error = %e, "could not count documents");
                None
            }
        }
    }

    /// The largest value of a sortable field, for freshness reporting.
    pub async fn newest(&self, index: &str, field: &str) -> Option<i64> {
        let sort = format!("{field}:desc");
        let fields = [field];
        let page: Page<Value> = self
            .search(index, "", "", &[sort.as_str()], Some(&fields), 1, 0)
            .await
            .ok()?;
        page.hits.first()?.get(field)?.as_i64()
    }
}
