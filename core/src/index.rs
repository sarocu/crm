//! Index names and settings.
//!
//! `ensure_indexes` is idempotent and called on startup by every binary that
//! holds a key allowed to do it, so the schema is applied by whichever
//! service boots first and none can operate against a half-configured index.

use meilisearch_sdk::settings::{FacetingSettings, Settings};

use crate::error::Result;
use crate::market::MarketConfig;
use crate::meili::{Client, await_task};

/// Firmographics, merged per company. Written by the indexer.
pub const COMPANIES: &str = "companies";
/// Dated buying signals, one company each. Written by the indexer.
pub const SIGNALS: &str = "signals";
/// Pipeline state per company. Written through MCP.
pub const ACCOUNTS: &str = "accounts";
/// Append-only outreach log. Written through MCP.
pub const ACTIVITIES: &str = "activities";
/// `add_company` requests: written through MCP, consumed by the indexer.
pub const COMPANY_REQUESTS: &str = "company_requests";
/// Indexer bookkeeping: per-source cursors and the crawl frontier.
pub const BOT_STATE: &str = "bot_state";

/// Indexes the indexer writes and the MCP server only reads.
pub const MARKET_INDEXES: [&str; 2] = [COMPANIES, SIGNALS];
/// Indexes the MCP server writes.
pub const CRM_INDEXES: [&str; 3] = [ACCOUNTS, ACTIVITIES, COMPANY_REQUESTS];

/// Ceiling on how long a single search may run, in milliseconds.
const SEARCH_CUTOFF_MS: u64 = 1_500;

fn base(market: &MarketConfig) -> Settings {
    Settings::new()
        .with_stop_words(&market.vocabulary.stop_words)
        .with_synonyms(market.vocabulary.synonym_map())
        .with_search_cutoff(SEARCH_CUTOFF_MS)
        .with_faceting(FacetingSettings {
            max_values_per_facet: 200,
            sort_facet_values_by: None,
        })
}

pub fn company_settings(market: &MarketConfig) -> Settings {
    base(market)
        .with_searchable_attributes([
            "name",
            "aliases",
            "domain",
            "description",
            "industries",
            "verticals",
            "tech",
            "investors",
            "hq_city",
            "hq_state",
            "body",
        ])
        .with_filterable_attributes([
            "id",
            "domain",
            "verticals",
            "industries",
            "industry_qids",
            "sic",
            "naics",
            "employees",
            "employees_band",
            "founded",
            "hq_country",
            "hq_state",
            "hq_city",
            "ticker",
            "cik",
            "wikidata_id",
            "ats_provider",
            "tech",
            "investors",
            "cohort",
            "stage",
            "sources",
            "last_signal_at",
            "merged_into",
        ])
        .with_sortable_attributes([
            "employees",
            "last_signal_at",
            "updated_at",
            "founded",
            "name",
        ])
}

pub fn signal_settings(market: &MarketConfig) -> Settings {
    base(market)
        // `search_companies has_signals` facets on company_id to find which
        // companies had a signal; 200 values would silently cut that off.
        .with_faceting(FacetingSettings {
            max_values_per_facet: 1000,
            sort_facet_values_by: None,
        })
        .with_searchable_attributes(["title", "company_name", "roles", "summary", "location"])
        .with_filterable_attributes([
            "id",
            "company_id",
            "kind",
            "verticals",
            "roles",
            "source",
            "occurred_at",
        ])
        .with_sortable_attributes(["occurred_at", "updated_at"])
}

pub fn account_settings(market: &MarketConfig) -> Settings {
    base(market)
        .with_searchable_attributes(["company_name", "domain", "next_step", "tags", "owner"])
        .with_filterable_attributes([
            "id",
            "status",
            "owner",
            "tags",
            "fit_profile",
            "next_touch_at",
            "last_activity_at",
            "updated_at",
        ])
        .with_sortable_attributes([
            "updated_at",
            "created_at",
            "next_touch_at",
            "last_activity_at",
        ])
}

pub fn activity_settings(market: &MarketConfig) -> Settings {
    base(market)
        .with_searchable_attributes(["subject", "summary", "company_name", "outcome"])
        .with_filterable_attributes([
            "id",
            "account_id",
            "type",
            "actor",
            "outcome",
            "occurred_at",
        ])
        .with_sortable_attributes(["occurred_at", "created_at"])
}

fn request_settings() -> Settings {
    Settings::new()
        .with_searchable_attributes(["domain", "name"])
        .with_filterable_attributes(["id", "status", "domain", "company_id"])
        .with_sortable_attributes(["requested_at", "updated_at"])
}

/// `id` must be filterable here: the frontier is read back a batch of ids at
/// a time rather than one document per request.
fn state_settings() -> Settings {
    Settings::new()
        .with_searchable_attributes(["id"])
        .with_filterable_attributes(["id", "kind", "source", "status", "host", "depth"])
        .with_sortable_attributes(["priority", "discovered_at", "updated_at"])
}

/// Create any missing index with `id` as its primary key and apply settings.
///
/// Safe to call concurrently from several services: creating an index that
/// already exists is tolerated.
pub async fn ensure_indexes(client: &Client, market: &MarketConfig) -> Result<()> {
    ensure_one(client, COMPANIES, &company_settings(market)).await?;
    ensure_one(client, SIGNALS, &signal_settings(market)).await?;
    ensure_one(client, ACCOUNTS, &account_settings(market)).await?;
    ensure_one(client, ACTIVITIES, &activity_settings(market)).await?;
    ensure_one(client, COMPANY_REQUESTS, &request_settings()).await?;
    ensure_one(client, BOT_STATE, &state_settings()).await?;
    tracing::info!(
        market = %market.name,
        verticals = market.verticals.len(),
        profiles = market.profiles.len(),
        "index settings applied"
    );
    Ok(())
}

async fn ensure_one(client: &Client, uid: &str, settings: &Settings) -> Result<()> {
    if client.get_index(uid).await.is_err() {
        match client.create_index(uid, Some("id")).await {
            Ok(info) => {
                if let Err(e) = await_task(client, info).await {
                    tracing::debug!(uid, error = %e, "index creation did not complete cleanly");
                }
            }
            Err(e) => tracing::debug!(uid, error = %e, "index creation rejected"),
        }
    }
    let info = client.index(uid).set_settings(settings).await?;
    await_task(client, info).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::tests::example;

    fn filterable_names(s: &Settings) -> Vec<String> {
        s.filterable_attributes
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|a| match a {
                meilisearch_sdk::settings::FilterableAttribute::Attribute(n) => n,
                meilisearch_sdk::settings::FilterableAttribute::Settings(cfg) => {
                    format!("{cfg:?}")
                }
            })
            .collect()
    }

    #[test]
    fn id_is_filterable_everywhere_so_batches_can_be_looked_up() {
        let m = example();
        for s in [
            company_settings(&m),
            signal_settings(&m),
            account_settings(&m),
            activity_settings(&m),
            request_settings(),
            state_settings(),
        ] {
            assert!(filterable_names(&s).contains(&"id".to_string()));
        }
    }

    #[test]
    fn companies_filter_on_what_profiles_ask_for() {
        let f = filterable_names(&company_settings(&example()));
        for attr in [
            "verticals",
            "hq_country",
            "hq_state",
            "employees",
            "ats_provider",
            "cik",
            "merged_into",
        ] {
            assert!(f.contains(&attr.to_string()), "{attr} must be filterable");
        }
    }

    #[test]
    fn signals_join_to_companies_and_sort_by_time() {
        let s = signal_settings(&example());
        let f = filterable_names(&s);
        assert!(f.contains(&"company_id".to_string()));
        assert!(f.contains(&"kind".to_string()));
        assert!(
            s.sortable_attributes
                .unwrap()
                .contains(&"occurred_at".to_string())
        );
    }

    #[test]
    fn crm_indexes_filter_on_their_join_keys() {
        let m = example();
        assert!(filterable_names(&account_settings(&m)).contains(&"status".to_string()));
        assert!(filterable_names(&activity_settings(&m)).contains(&"account_id".to_string()));
    }

    #[test]
    fn the_vocabulary_is_applied() {
        let s = company_settings(&example());
        assert!(!s.synonyms.unwrap().is_empty());
        assert!(!s.stop_words.unwrap().is_empty());
    }
}
