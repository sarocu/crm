//! Requests to add a company to the index.
//!
//! Lives in `core` because two services touch it: the MCP server's
//! `add_company` tool writes a request, and the indexer's `requests` source
//! turns it into a stub company and queues its site for crawling.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestStatus {
    /// Waiting for the indexer.
    Pending,
    /// Stub written and site queued. Terminal.
    Applied,
    /// Could not be acted on; `apply_note` says why. Terminal.
    Failed,
}

impl RequestStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestStatus::Pending => "pending",
            RequestStatus::Applied => "applied",
            RequestStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanyRequest {
    pub id: String,
    pub status: RequestStatus,
    /// Registrable domain, already normalised by the writer.
    pub domain: String,
    /// The id the company will have once indexed.
    pub company_id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Vertical slugs the requester believes apply. Only used when the
    /// crawl finds nothing better to classify by.
    #[serde(default)]
    pub verticals: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
    pub requested_by: String,
    pub requested_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub apply_note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_serialise_lowercase() {
        for s in [
            RequestStatus::Pending,
            RequestStatus::Applied,
            RequestStatus::Failed,
        ] {
            assert_eq!(
                serde_json::to_value(s).unwrap(),
                serde_json::Value::String(s.as_str().into())
            );
        }
    }
}
