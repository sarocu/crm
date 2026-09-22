//! Companies the agent asked for by name.
//!
//! `add_company` on the MCP server writes a request; this source turns each
//! pending one into a stub company record and puts its homepage on the crawl
//! frontier, so the crawler fills in the rest on its next pass.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use crm_core::index::COMPANY_REQUESTS;
use crm_core::meili;
use crm_core::model::{Company, Doc, now_ts};
use crm_core::request::{CompanyRequest, RequestStatus};
use meilisearch_sdk::search::SearchQuery;
use serde_json::Value;

use super::{Batch, Ctx, Source};
use crate::state::FrontierEntry;

/// Requests handled per pass.
const PER_RUN: usize = 50;

pub struct Requests;

#[async_trait]
impl Source for Requests {
    fn name(&self) -> &'static str {
        "requests"
    }

    fn default_interval(&self) -> Duration {
        // Short: an agent that just asked for a company will look for it.
        Duration::from_secs(60)
    }

    async fn fetch(&self, ctx: &Ctx, _cursor: Option<Value>) -> Result<Batch> {
        let idx = ctx.state.client().index(COMPANY_REQUESTS);
        let sort = ["requested_at:asc"];
        let mut q = SearchQuery::new(&idx);
        q.with_query("")
            .with_filter("status = \"pending\"")
            .with_sort(&sort)
            .with_limit(PER_RUN);
        let pending: Vec<CompanyRequest> = q
            .execute::<CompanyRequest>()
            .await?
            .hits
            .into_iter()
            .map(|h| h.result)
            .collect();
        if pending.is_empty() {
            return Ok(Batch::empty());
        }
        tracing::info!(n = pending.len(), "applying company requests");

        let mut docs = Vec::new();
        let mut done = Vec::new();
        for mut req in pending {
            let homepage = format!("https://{}/", req.domain);
            docs.push(Doc::Company(stub(ctx, &req, &homepage)));
            let queued = ctx
                .state
                .frontier_add(&[FrontierEntry {
                    url: homepage.clone(),
                    depth: 0,
                    company_id: Some(req.company_id.clone()),
                }])
                .await;
            match queued {
                Ok(_) => {
                    req.status = RequestStatus::Applied;
                    req.apply_note = Some(format!(
                        "Indexed as a stub; {homepage} is queued for crawling."
                    ));
                }
                Err(e) => {
                    req.status = RequestStatus::Failed;
                    req.apply_note = Some(format!("Could not queue {homepage} for crawling: {e}"));
                }
            }
            req.updated_at = now_ts();
            done.push(req);
        }
        // Written before the stubs are ingested: a request marked applied
        // whose stub then fails to write is re-created by the next crawl of
        // its homepage anyway, while the reverse would loop forever.
        meili::upsert_chunked(ctx.state.client(), COMPANY_REQUESTS, &done).await?;
        Ok(Batch::new(docs, None))
    }
}

fn stub(ctx: &Ctx, req: &CompanyRequest, homepage: &str) -> Company {
    let name = req
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| req.domain.clone());
    let mut c = Company::new(&req.company_id, name, "requests");
    c.domain = Some(req.domain.clone());
    c.website = Some(homepage.to_string());
    // The requester's verticals become industry labels, which classify on
    // the vertical's name until the crawl finds something better.
    c.industries = req
        .verticals
        .iter()
        .filter_map(|v| ctx.market.vertical(v))
        .map(|v| v.name.clone())
        .collect();
    c
}
