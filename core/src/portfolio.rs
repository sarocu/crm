//! Portfolios added at runtime, and how each portfolio's last sweep went.
//!
//! `market.toml` holds the portfolios a deployment ships with. The agent can
//! add more through MCP without a rebuild; those live in the `portfolios`
//! index, written by the MCP server and read by the indexer on every run.
//! Removing one disables it rather than deleting it, so the MCP key never
//! needs delete rights and the history of who added what survives.
//!
//! How a portfolio's last read went is the indexer's to record, in
//! `bot_state`, so the two writers never touch the same document.

use serde::{Deserialize, Serialize};

use crate::market::Portfolio;

/// A portfolio added through MCP. `id` is the slug.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePortfolio {
    pub id: String,
    #[serde(flatten)]
    pub portfolio: Portfolio,
    pub enabled: bool,
    pub added_by: String,
    pub added_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub note: Option<String>,
}

/// The outcome of the most recent read of one portfolio.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PortfolioStatus {
    pub slug: String,
    pub last_read_at: i64,
    /// Companies the last read produced.
    pub companies: usize,
    /// Companies newly added since the previous read.
    pub newly_added: usize,
    /// Whether the baseline sweep has completed.
    pub baselined: bool,
    #[serde(default)]
    pub error: Option<String>,
}

impl PortfolioStatus {
    /// The `bot_state` id this status is stored under.
    pub fn state_id(slug: &str) -> String {
        format!("pfstatus-{slug}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::PortfolioKind;

    #[test]
    fn runtime_portfolios_flatten_to_one_document() {
        let r = RuntimePortfolio {
            id: "denver-ventures".into(),
            portfolio: Portfolio {
                slug: "denver-ventures".into(),
                investor: "Denver Ventures".into(),
                kind: PortfolioKind::Page,
                url: "https://example.test/portfolio".into(),
                selector: None,
                detail_selector: None,
                items: None,
                name_field: None,
                website_field: None,
                statuses: vec![],
                since_year: None,
            },
            enabled: true,
            added_by: "bdr".into(),
            added_at: 1,
            updated_at: 1,
            note: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["investor"], "Denver Ventures");
        assert_eq!(v["kind"], "page");
        let back: RuntimePortfolio = serde_json::from_value(v).unwrap();
        assert_eq!(back.portfolio.slug, "denver-ventures");
        assert_eq!(PortfolioStatus::state_id("x"), "pfstatus-x");
    }
}
