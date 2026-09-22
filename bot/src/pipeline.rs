//! The path every document takes into the index.
//!
//! Sources produce partial company records and signals. This merges each
//! company with what is already stored and with records other sources made
//! for the same company under a weaker key, classifies it into verticals,
//! and writes only what actually changed.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use crm_core::index::{ACCOUNTS, ACTIVITIES, COMPANIES, SIGNALS};
use crm_core::market::MarketConfig;
use crm_core::meili::{self, Client};
use crm_core::model::{Account, Activity, Company, Doc, Signal};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::classify;
use crate::state::{existing_hashes, fetch_by_ids, fetch_where};

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct IngestStats {
    pub received: usize,
    /// Dropped for missing identity or title.
    pub invalid: usize,
    /// Signals about a company we have no record of.
    pub orphaned: usize,
    /// Provisional records folded into their domain-keyed company.
    pub merged: usize,
    /// Already indexed with identical content, so not rewritten.
    pub unchanged: usize,
    pub written: usize,
}

impl IngestStats {
    pub fn merge(&mut self, other: IngestStats) {
        self.received += other.received;
        self.invalid += other.invalid;
        self.orphaned += other.orphaned;
        self.merged += other.merged;
        self.unchanged += other.unchanged;
        self.written += other.written;
    }
}

pub struct Pipeline {
    client: Client,
    market: MarketConfig,
    /// Company writes are read-merge-write, and every source runs on its
    /// own task. One lock around that cycle is what stops two sources from
    /// each merging into the same stale copy and losing the other's fields.
    company_lock: Mutex<()>,
}

/// The identity fields of a stored company, for cross-key matching.
#[derive(Debug, Deserialize)]
struct KeyRow {
    id: String,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    cik: Option<String>,
    #[serde(default)]
    wikidata_id: Option<String>,
    #[serde(default)]
    merged_into: Option<String>,
}

impl Pipeline {
    pub fn new(client: Client, market: MarketConfig) -> Self {
        Self {
            client,
            market,
            company_lock: Mutex::new(()),
        }
    }

    pub async fn ingest(&self, docs: Vec<Doc>) -> Result<IngestStats> {
        let mut stats = IngestStats {
            received: docs.len(),
            ..Default::default()
        };
        if docs.is_empty() {
            return Ok(stats);
        }

        let mut companies = Vec::new();
        let mut signals = Vec::new();
        for doc in docs {
            match doc {
                // A page that says nothing about whose it is still carries
                // facts; the domain stands in for a name until one turns up.
                Doc::Company(c)
                    if !c.id.is_empty() && (!c.name.trim().is_empty() || c.domain.is_some()) =>
                {
                    companies.push(c)
                }
                Doc::Signal(s) if !s.company_id.is_empty() && !s.title.trim().is_empty() => {
                    signals.push(s)
                }
                _ => stats.invalid += 1,
            }
        }

        let _guard = self.company_lock.lock().await;
        let redirects = if companies.is_empty() {
            HashMap::new()
        } else {
            self.ingest_companies(companies, &mut stats).await?
        };
        if !signals.is_empty() {
            self.ingest_signals(signals, &redirects, &mut stats).await?;
        }
        Ok(stats)
    }

    /// Merge and write companies. Returns provisional id → surviving id for
    /// every record that was folded into another, so signals in the same
    /// batch can follow.
    async fn ingest_companies(
        &self,
        patches: Vec<Company>,
        stats: &mut IngestStats,
    ) -> Result<HashMap<String, String>> {
        // 1. Collapse duplicates inside the batch.
        let mut by_id: HashMap<String, Company> = HashMap::new();
        for c in patches {
            match by_id.get_mut(&c.id) {
                Some(existing) => existing.merge_from(&c),
                None => {
                    by_id.insert(c.id.clone(), c);
                }
            }
        }

        // 2. Find stored records for the same company under another key: a
        //    CIK- or Wikidata-keyed record for a company we now have a domain
        //    for, or a domain-keyed one for a patch that only knows its CIK.
        let ciks: Vec<String> = by_id.values().filter_map(|c| c.cik.clone()).collect();
        let qids: Vec<String> = by_id
            .values()
            .filter_map(|c| c.wikidata_id.clone())
            .collect();
        let fields: &[&str] = &["id", "domain", "cik", "wikidata_id", "merged_into"];
        let mut related: Vec<KeyRow> = Vec::new();
        if !ciks.is_empty() {
            related.extend(fetch_where(&self.client, COMPANIES, "cik", &ciks, Some(fields)).await?);
        }
        if !qids.is_empty() {
            related.extend(
                fetch_where(&self.client, COMPANIES, "wikidata_id", &qids, Some(fields)).await?,
            );
        }
        let related: Vec<KeyRow> = related
            .into_iter()
            .filter(|r| r.merged_into.is_none())
            .collect();

        let mut redirects: HashMap<String, String> = HashMap::new();
        let mut absorb: HashMap<String, Vec<String>> = HashMap::new();
        let mut final_patches: HashMap<String, Company> = HashMap::new();
        for (_, mut patch) in by_id {
            let same = |r: &&KeyRow| {
                (patch.cik.is_some() && r.cik == patch.cik)
                    || (patch.wikidata_id.is_some() && r.wikidata_id == patch.wikidata_id)
            };
            if patch.domain.is_none() {
                // Weak key: join the domain-keyed record if there is one.
                if let Some(target) = related.iter().filter(same).find(|r| r.domain.is_some()) {
                    redirects.insert(patch.id.clone(), target.id.clone());
                    patch.id = target.id.clone();
                }
            } else {
                // Strong key: pull in any weak-keyed record for the same company.
                for r in related.iter().filter(same) {
                    if r.id != patch.id && r.domain.is_none() {
                        absorb
                            .entry(patch.id.clone())
                            .or_default()
                            .push(r.id.clone());
                        redirects.insert(r.id.clone(), patch.id.clone());
                    }
                }
            }
            match final_patches.get_mut(&patch.id) {
                Some(existing) => existing.merge_from(&patch),
                None => {
                    final_patches.insert(patch.id.clone(), patch);
                }
            }
        }

        // 3. Merge onto what is stored.
        let mut want: Vec<String> = final_patches.keys().cloned().collect();
        want.extend(absorb.values().flatten().cloned());
        let stored: HashMap<String, Company> =
            fetch_by_ids::<Company>(&self.client, COMPANIES, &want, None)
                .await?
                .into_iter()
                .map(|c| (c.id.clone(), c))
                .collect();

        let mut to_write: Vec<Company> = Vec::new();
        let mut tombstones: Vec<Company> = Vec::new();
        for (id, patch) in final_patches {
            let previous = stored.get(&id);
            let mut merged = previous.cloned().unwrap_or_else(|| Company {
                id: id.clone(),
                ..Default::default()
            });
            for old in absorb.get(&id).into_iter().flatten() {
                if let Some(old_doc) = stored.get(old) {
                    merged.merge_from(old_doc);
                    let mut tomb = old_doc.clone();
                    tomb.merged_into = Some(id.clone());
                    tombstones.push(tomb.finalize());
                    stats.merged += 1;
                }
            }
            merged.merge_from(&patch);
            merged.id = id.clone();
            if merged.name.trim().is_empty() {
                merged.name = merged.domain.clone().unwrap_or_else(|| id.clone());
            }
            merged.verticals = classify::verticals_for(&merged, &self.market);
            if let Some(p) = previous {
                merged.updated_at = p.updated_at;
            }
            let mut merged = merged.finalize();
            match previous {
                Some(p) if p.content_hash == merged.content_hash => {
                    stats.unchanged += 1;
                }
                _ => {
                    merged.updated_at = crm_core::model::now_ts();
                    to_write.push(merged);
                }
            }
        }

        if !to_write.is_empty() {
            stats.written += meili::upsert_chunked(&self.client, COMPANIES, &to_write).await?;
            tracing::info!(index = COMPANIES, written = to_write.len(), "indexed");
        }
        if !tombstones.is_empty() {
            meili::upsert_chunked(&self.client, COMPANIES, &tombstones).await?;
        }
        for (old, new) in &redirects {
            if absorb.values().flatten().any(|a| a == old)
                && let Err(e) = self.rehome(old, new).await
            {
                tracing::warn!(old, new, error = %e, "could not move records to the merged company");
            }
        }
        Ok(redirects)
    }

    /// Point everything that referenced a folded-in company at its survivor.
    async fn rehome(&self, old: &str, new: &str) -> Result<()> {
        let old_v = vec![old.to_string()];

        let mut signals: Vec<Signal> =
            fetch_where(&self.client, SIGNALS, "company_id", &old_v, None).await?;
        for s in &mut signals {
            s.company_id = new.to_string();
        }
        meili::upsert_chunked(&self.client, SIGNALS, &signals).await?;

        let mut activities: Vec<Activity> =
            fetch_where(&self.client, ACTIVITIES, "account_id", &old_v, None).await?;
        for a in &mut activities {
            a.account_id = new.to_string();
        }
        meili::upsert_chunked(&self.client, ACTIVITIES, &activities).await?;

        // An account moves only when the survivor has none of its own; the
        // agent's work on the survivor is the one to keep.
        let olds: Vec<Account> = fetch_by_ids(&self.client, ACCOUNTS, &old_v, None).await?;
        if let Some(mut acct) = olds.into_iter().next() {
            let has_new: Vec<Account> =
                fetch_by_ids(&self.client, ACCOUNTS, &[new.to_string()], None).await?;
            if has_new.is_empty() {
                acct.id = new.to_string();
                meili::upsert_chunked(&self.client, ACCOUNTS, &[acct]).await?;
            }
            meili::delete_chunked(&self.client, ACCOUNTS, &old_v).await?;
        }
        tracing::info!(old, new, "merged a provisional company record");
        Ok(())
    }

    async fn ingest_signals(
        &self,
        signals: Vec<Signal>,
        redirects: &HashMap<String, String>,
        stats: &mut IngestStats,
    ) -> Result<()> {
        // Follow merges made in this batch, then any made earlier.
        let mut signals: Vec<Signal> = signals
            .into_iter()
            .map(|mut s| {
                if let Some(to) = redirects.get(&s.company_id) {
                    s.company_id = to.clone();
                }
                s
            })
            .collect();
        let ids: Vec<String> = signals
            .iter()
            .map(|s| s.company_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let mut companies: HashMap<String, Company> =
            fetch_by_ids::<Company>(&self.client, COMPANIES, &ids, None)
                .await?
                .into_iter()
                .map(|c| (c.id.clone(), c))
                .collect();
        let forwarded: Vec<String> = companies
            .values()
            .filter_map(|c| c.merged_into.clone())
            .collect();
        if !forwarded.is_empty() {
            for c in fetch_by_ids::<Company>(&self.client, COMPANIES, &forwarded, None).await? {
                companies.insert(c.id.clone(), c);
            }
        }

        let mut kept: HashMap<String, Signal> = HashMap::new();
        for mut s in signals.drain(..) {
            let mut company = companies.get(&s.company_id);
            if let Some(to) = company.and_then(|c| c.merged_into.clone()) {
                s.company_id = to;
                company = companies.get(&s.company_id);
            }
            let Some(c) = company else {
                stats.orphaned += 1;
                continue;
            };
            s.company_name = c.name.clone();
            s.verticals = c.verticals.clone();
            let s = s.finalize();
            kept.insert(s.id.clone(), s);
        }

        let ids: Vec<String> = kept.keys().cloned().collect();
        let existing = existing_hashes(&self.client, SIGNALS, &ids)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "could not read existing signal hashes; rewriting");
                HashMap::new()
            });
        let changed: Vec<Signal> = kept
            .into_values()
            .filter(|s| existing.get(&s.id) != Some(&s.content_hash))
            .collect();
        stats.unchanged += ids.len() - changed.len();
        if changed.is_empty() {
            return Ok(());
        }
        stats.written += meili::upsert_chunked(&self.client, SIGNALS, &changed).await?;
        tracing::info!(index = SIGNALS, written = changed.len(), "indexed");

        // Keep each company's `last_signal_at` current, so "who has been
        // active lately" is one sort on the companies index.
        let mut newest: HashMap<&str, i64> = HashMap::new();
        for s in &changed {
            let e = newest.entry(s.company_id.as_str()).or_insert(s.occurred_at);
            *e = (*e).max(s.occurred_at);
        }
        let bumped: Vec<Company> = newest
            .into_iter()
            .filter_map(|(id, ts)| {
                let c = companies.get(id)?;
                (c.last_signal_at.is_none_or(|prev| ts > prev)).then(|| Company {
                    last_signal_at: Some(ts),
                    ..c.clone()
                })
            })
            .collect();
        if !bumped.is_empty() {
            meili::upsert_chunked(&self.client, COMPANIES, &bumped).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_merge_additively() {
        let mut a = IngestStats {
            received: 1,
            written: 1,
            ..Default::default()
        };
        a.merge(IngestStats {
            received: 2,
            orphaned: 2,
            ..Default::default()
        });
        assert_eq!(a.received, 3);
        assert_eq!(a.written, 1);
        assert_eq!(a.orphaned, 2);
    }
}
