//! Wikidata: companies whose industry or class is one a vertical names.
//!
//! One SPARQL query per configured QID, paged, asking for organisations
//! whose industry (P452) or instance-of (P31) is that item and which have an
//! official website (P856) — the website is what gives them a domain, and a
//! company we cannot put a domain on is one the crawler and the job boards
//! can do nothing with. Headcount (P1128), HQ (P159), country (P17),
//! founding date (P571) and SEC CIK (P5531) come along when present; the
//! CIK is what lets the pipeline fold an EDGAR-only record into this one.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use crm_core::id::{company_id, root_domain};
use crm_core::model::{Company, Doc};
use serde_json::{Value, json};

use super::{Batch, Ctx, Source, cursor_usize};

pub struct Wikidata;

#[async_trait]
impl Source for Wikidata {
    fn name(&self) -> &'static str {
        "wikidata"
    }

    fn default_interval(&self) -> Duration {
        // The query service is shared and slow; Wikidata changes slowly.
        Duration::from_secs(20 * 60)
    }

    async fn fetch(&self, ctx: &Ctx, cursor: Option<Value>) -> Result<Batch> {
        // Every (vertical, QID) pair, in config order.
        let targets: Vec<&str> = ctx
            .market
            .verticals
            .iter()
            .flat_map(|v| v.wikidata.iter().map(String::as_str))
            .collect();
        if targets.is_empty() {
            tracing::debug!(
                "no vertical names a Wikidata QID; the wikidata source has nothing to do"
            );
            return Ok(Batch::empty());
        }
        let t = cursor_usize(&cursor, "target") % targets.len();
        let offset = cursor_usize(&cursor, "offset");
        let limit = ctx.config.wikidata_page_size.max(1);
        let qid = targets[t];

        let query = sparql(qid, limit, offset);
        let url = url::Url::parse_with_params(
            &ctx.config.wikidata_sparql,
            &[("format", "json"), ("query", query.as_str())],
        )?;
        let raw: Value = ctx.http.get_json(url.as_str()).await?;
        let rows = bindings(&raw);
        let n = rows.len();
        let companies = to_companies(&rows);
        tracing::info!(
            qid,
            offset,
            rows = n,
            companies = companies.len(),
            "wikidata page"
        );

        let docs = companies.into_iter().map(Doc::Company).collect();
        // A short page means this QID is exhausted; move to the next one.
        let (next, swept) = if n < limit {
            let nt = t + 1;
            (
                json!({ "target": nt % targets.len(), "offset": 0 }),
                nt >= targets.len(),
            )
        } else {
            (json!({ "target": t, "offset": offset + limit }), false)
        };
        let batch = Batch::new(docs, Some(next));
        Ok(if swept { batch.swept() } else { batch })
    }
}

/// The query for one QID. `ORDER BY ?item` makes `OFFSET` paging stable.
pub fn sparql(qid: &str, limit: usize, offset: usize) -> String {
    // QIDs come from our own config, but keep them to their real shape so a
    // typo cannot inject SPARQL.
    let qid: String = qid.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    format!(
        r#"SELECT ?item ?itemLabel ?website ?employees ?countryCode ?hqLabel ?inception ?cik ?industry ?industryLabel ?description WHERE {{
  {{ ?item wdt:P452 wd:{qid} . }} UNION {{ ?item wdt:P31 wd:{qid} . }}
  ?item wdt:P856 ?website .
  OPTIONAL {{ ?item wdt:P1128 ?employees . }}
  OPTIONAL {{ ?item wdt:P17 ?country . ?country wdt:P297 ?countryCode . }}
  OPTIONAL {{ ?item wdt:P159 ?hq . }}
  OPTIONAL {{ ?item wdt:P571 ?inception . }}
  OPTIONAL {{ ?item wdt:P5531 ?cik . }}
  OPTIONAL {{ ?item wdt:P452 ?industry . }}
  OPTIONAL {{ ?item schema:description ?description . FILTER(LANG(?description) = "en") }}
  SERVICE wikibase:label {{ bd:serviceParam wikibase:language "en". }}
}}
ORDER BY ?item
LIMIT {limit} OFFSET {offset}"#
    )
}

fn bindings(raw: &Value) -> Vec<BTreeMap<String, String>> {
    raw.pointer("/results/bindings")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_object()
                        .map(|o| {
                            o.iter()
                                .filter_map(|(k, v)| {
                                    Some((k.clone(), v.get("value")?.as_str()?.to_string()))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn qid_of(uri: &str) -> Option<String> {
    let q = uri.rsplit('/').next()?;
    (q.starts_with('Q') && q[1..].chars().all(|c| c.is_ascii_digit())).then(|| q.to_string())
}

/// Group rows by item — multi-valued properties repeat the row — and build
/// one company per item that has a usable domain.
pub fn to_companies(rows: &[BTreeMap<String, String>]) -> Vec<Company> {
    let mut by_item: BTreeMap<String, Company> = BTreeMap::new();
    for row in rows {
        let Some(qid) = row.get("item").and_then(|u| qid_of(u)) else {
            continue;
        };
        let Some(label) = row
            .get("itemLabel")
            .filter(|l| qid_of(&format!("x/{l}")).is_none())
        else {
            // No English label: the label service hands back the QID itself.
            continue;
        };
        let domain = row.get("website").and_then(|w| root_domain(w));
        let entry = by_item.entry(qid.clone()).or_insert_with(|| {
            let id = company_id(domain.as_deref(), None, Some(&qid)).unwrap_or_default();
            let mut c = Company::new(id, label.clone(), "wikidata");
            c.wikidata_id = Some(qid.clone());
            c.domain = domain.clone();
            c.website = row.get("website").cloned();
            c
        });
        if let Some(n) = row
            .get("employees")
            .and_then(|e| e.parse::<f64>().ok())
            .filter(|n| n.is_finite() && *n >= 1.0)
        {
            entry.employees = Some(entry.employees.unwrap_or(0).max(n as u64));
        }
        if let Some(cc) = row.get("countryCode") {
            entry.hq_country.get_or_insert_with(|| cc.clone());
        }
        if let Some(hq) = row
            .get("hqLabel")
            .filter(|h| qid_of(&format!("x/{h}")).is_none())
        {
            entry.hq_city.get_or_insert_with(|| hq.clone());
        }
        if let Some(year) = row
            .get("inception")
            .and_then(|d| d.get(..4))
            .and_then(|y| y.parse::<i32>().ok())
        {
            entry.founded.get_or_insert(year);
        }
        if let Some(cik) = row
            .get("cik")
            .filter(|c| c.chars().all(|d| d.is_ascii_digit()))
        {
            let padded = format!("{:0>10}", cik);
            entry.cik.get_or_insert(padded);
        }
        if let Some(ind) = row.get("industry").and_then(|u| qid_of(u))
            && !entry.industry_qids.contains(&ind)
        {
            entry.industry_qids.push(ind);
        }
        if let Some(l) = row.get("industryLabel")
            && qid_of(&format!("x/{l}")).is_none()
            && !entry.industries.contains(l)
        {
            entry.industries.push(l.clone());
        }
        if let Some(d) = row.get("description")
            && entry.description.is_empty()
        {
            entry.description = d.clone();
        }
    }
    by_item
        .into_values()
        .filter(|c| c.domain.is_some() && !c.id.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn repeated_rows_collapse_into_one_company() {
        let rows = vec![
            row(&[
                ("item", "http://www.wikidata.org/entity/Q42"),
                ("itemLabel", "Acme Brewing"),
                ("website", "https://www.acme.test/"),
                ("employees", "120"),
                ("countryCode", "US"),
                ("hqLabel", "Denver"),
                ("inception", "1994-01-01T00:00:00Z"),
                ("cik", "1234567"),
                ("industry", "http://www.wikidata.org/entity/Q869095"),
                ("industryLabel", "brewing"),
            ]),
            row(&[
                ("item", "http://www.wikidata.org/entity/Q42"),
                ("itemLabel", "Acme Brewing"),
                ("website", "https://www.acme.test/"),
                ("employees", "150"),
            ]),
            // No English label: skipped.
            row(&[
                ("item", "http://www.wikidata.org/entity/Q7"),
                ("itemLabel", "Q7"),
                ("website", "https://seven.test/"),
            ]),
        ];
        let cs = to_companies(&rows);
        assert_eq!(cs.len(), 1);
        let c = &cs[0];
        assert_eq!(c.name, "Acme Brewing");
        assert_eq!(c.domain.as_deref(), Some("acme.test"));
        assert_eq!(c.id, company_id(Some("acme.test"), None, None).unwrap());
        assert_eq!(c.employees, Some(150));
        assert_eq!(c.cik.as_deref(), Some("0001234567"));
        assert_eq!(c.founded, Some(1994));
        assert_eq!(c.industry_qids, vec!["Q869095"]);
        assert_eq!(c.industries, vec!["brewing"]);
        assert_eq!(c.wikidata_id.as_deref(), Some("Q42"));
    }

    #[test]
    fn the_query_is_paged_and_sanitised() {
        let q = sparql("Q131734\" } DROP", 50, 100);
        assert!(q.contains("wd:Q131734DROP"));
        assert!(!q.contains("\" }"));
        assert!(q.contains("LIMIT 50 OFFSET 100"));
        assert!(q.contains("ORDER BY ?item"));
    }

    #[test]
    fn a_bindings_payload_parses() {
        let raw = serde_json::json!({"results": {"bindings": [
            {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q1"},
             "itemLabel": {"type": "literal", "value": "One"}}
        ]}});
        let rows = bindings(&raw);
        assert_eq!(rows[0]["itemLabel"], "One");
        assert!(bindings(&serde_json::json!({})).is_empty());
    }
}
