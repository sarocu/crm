//! How well a company fits a customer profile, and why.
//!
//! A pure function of the company, its recent signals and the profile, so
//! the ranking `find_prospects` returns can be explained line by line and
//! tested without an index. Every criterion contributes up to its weight;
//! unknowns (no headcount, no HQ) earn half, so a sparse record is neither
//! rewarded nor thrown away.

use crm_core::market::{Profile, normalize};
use crm_core::model::{Company, Signal, SignalKind};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Reason {
    pub criterion: &'static str,
    pub points: f32,
    pub max: f32,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Fit {
    pub profile: String,
    /// 0–100: points earned as a share of points available.
    pub score: u8,
    pub points: f32,
    pub max: f32,
    pub reasons: Vec<Reason>,
}

/// Score one company against one profile. `signals` may include anything;
/// only those inside the profile's recency window count.
pub fn score(c: &Company, signals: &[Signal], p: &Profile, now: i64) -> Fit {
    let w = p.weights;
    let mut reasons = Vec::new();

    // Vertical.
    let hit: Vec<&String> = c
        .verticals
        .iter()
        .filter(|v| p.verticals.contains(v))
        .collect();
    reasons.push(if hit.is_empty() {
        reason(
            "vertical",
            0.0,
            w.vertical,
            format!("not in {}", p.verticals.join(", ")),
        )
    } else {
        reason(
            "vertical",
            w.vertical,
            w.vertical,
            format!("in {}", join(&hit)),
        )
    });

    // Size.
    let r = p.employees;
    reasons.push(match c.employees {
        _ if r.is_open() => reason("size", w.size, w.size, "any size fits".into()),
        Some(n) if r.contains(n) => reason(
            "size",
            w.size,
            w.size,
            format!("{n} employees, in {}", range(r)),
        ),
        Some(n) => reason(
            "size",
            0.0,
            w.size,
            format!("{n} employees, outside {}", range(r)),
        ),
        None => reason("size", w.size / 2.0, w.size, "headcount unknown".into()),
    });

    // Geography.
    reasons.push(geo(c, p, w.geo));

    // Hiring.
    let since = now - i64::from(p.signals.recency_days) * 86_400;
    let recent: Vec<&Signal> = signals.iter().filter(|s| s.occurred_at >= since).collect();
    let hiring: Vec<&Signal> = recent
        .iter()
        .copied()
        .filter(|s| s.kind == SignalKind::Hiring)
        .collect();
    let wanted: Vec<String> = p
        .signals
        .hiring_roles
        .iter()
        .map(|r| normalize(r))
        .collect();
    let (matching, roles): (Vec<&Signal>, Vec<String>) = if wanted.is_empty() {
        (hiring.clone(), Vec::new())
    } else {
        let mut roles = Vec::new();
        let m = hiring
            .iter()
            .copied()
            .filter(|s| {
                let text = normalize(&format!("{} {}", s.title, s.roles.join(" ")));
                let found: Vec<&String> = wanted.iter().filter(|r| phrase(&text, r)).collect();
                for f in &found {
                    if !roles.contains(*f) {
                        roles.push((*f).clone());
                    }
                }
                !found.is_empty()
            })
            .collect();
        (m, roles)
    };
    reasons.push(if matching.is_empty() {
        let detail = if hiring.is_empty() {
            format!("no open roles in the last {} days", p.signals.recency_days)
        } else {
            format!("{} open role(s), none in target functions", hiring.len())
        };
        reason("hiring", 0.0, w.hiring, detail)
    } else {
        let pts = w.hiring * (matching.len() as f32 / 3.0).min(1.0);
        let example = matching[0].title.trim_start_matches("Hiring: ");
        let what = if roles.is_empty() {
            String::new()
        } else {
            format!(" in {}", roles.join(", "))
        };
        reason(
            "hiring",
            pts,
            w.hiring,
            format!("{} open role(s){what}, e.g. \"{example}\"", matching.len()),
        )
    });

    // News and filings.
    let mut news: Vec<&Signal> = recent
        .iter()
        .copied()
        .filter(|s| s.kind != SignalKind::Hiring)
        .collect();
    news.sort_by_key(|s| std::cmp::Reverse(s.occurred_at));
    reasons.push(if news.is_empty() {
        reason(
            "news",
            0.0,
            w.news,
            format!("no news in the last {} days", p.signals.recency_days),
        )
    } else {
        let pts = w.news * (news.len() as f32 / 2.0).min(1.0);
        reason(
            "news",
            pts,
            w.news,
            format!(
                "{} recent item(s), latest {}: \"{}\"",
                news.len(),
                news[0].kind,
                news[0].title
            ),
        )
    });

    // Keywords.
    if !p.keywords.is_empty() {
        let text = normalize(&format!(
            "{} {} {} {}",
            c.description,
            c.body,
            c.tech.join(" "),
            hiring
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        ));
        let found: Vec<&String> = p
            .keywords
            .iter()
            .filter(|k| phrase(&text, &normalize(k)))
            .collect();
        reasons.push(if found.is_empty() {
            reason(
                "keywords",
                0.0,
                w.keywords,
                "none of the profile's keywords seen".into(),
            )
        } else {
            let pts = w.keywords * (found.len() as f32 / 2.0).min(1.0);
            reason(
                "keywords",
                pts,
                w.keywords,
                format!("mentions {}", join(&found)),
            )
        });
    }

    let points: f32 = reasons.iter().map(|r| r.points).sum();
    let max: f32 = reasons.iter().map(|r| r.max).sum();
    let score = if max > 0.0 {
        ((points / max) * 100.0).round().clamp(0.0, 100.0) as u8
    } else {
        0
    };
    Fit {
        profile: p.slug.clone(),
        score,
        points: round2(points),
        max: round2(max),
        reasons,
    }
}

fn geo(c: &Company, p: &Profile, weight: f32) -> Reason {
    if p.countries.is_empty() && p.states.is_empty() {
        return reason("geo", weight, weight, "anywhere fits".into());
    }
    let country_ok = if p.countries.is_empty() {
        Some(true)
    } else {
        c.hq_country
            .as_ref()
            .map(|cc| p.countries.iter().any(|w| w.eq_ignore_ascii_case(cc)))
    };
    let state_ok = if p.states.is_empty() {
        Some(true)
    } else {
        c.hq_state
            .as_ref()
            .map(|st| p.states.iter().any(|w| normalize(w) == normalize(st)))
    };
    let where_ = [
        c.hq_city.as_deref(),
        c.hq_state.as_deref(),
        c.hq_country.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");
    match (country_ok, state_ok) {
        (Some(true), Some(true)) => reason("geo", weight, weight, format!("HQ {where_}")),
        (Some(false), _) | (_, Some(false)) => reason(
            "geo",
            0.0,
            weight,
            format!("HQ {where_} is outside the territory"),
        ),
        _ => reason("geo", weight / 2.0, weight, "HQ location unknown".into()),
    }
}

fn reason(criterion: &'static str, points: f32, max: f32, detail: String) -> Reason {
    Reason {
        criterion,
        points: round2(points),
        max: round2(max),
        detail,
    }
}

fn round2(x: f32) -> f32 {
    (x * 100.0).round() / 100.0
}

fn join(v: &[&String]) -> String {
    v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
}

fn range(r: crm_core::market::Range) -> String {
    match (r.min, r.max) {
        (Some(a), Some(b)) => format!("{a}–{b}"),
        (Some(a), None) => format!("{a}+"),
        (None, Some(b)) => format!("up to {b}"),
        (None, None) => "any".into(),
    }
}

/// Whole-word phrase match over already-normalised text.
pub fn phrase(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let padded = format!(" {haystack} ");
    padded.contains(&format!(" {needle} "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crm_core::market::MarketConfig;

    const NOW: i64 = 1_800_000_000;

    fn profile() -> Profile {
        let m = MarketConfig::load(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../market.example.toml"
        ))
        .unwrap();
        m.profile("mid-market-ops").unwrap().clone()
    }

    fn company() -> Company {
        let mut c = Company::new("c1", "Acme Brewing", "crawl");
        c.verticals = vec!["craft-beverage".into()];
        c.employees = Some(120);
        c.hq_country = Some("US".into());
        c.hq_state = Some("CO".into());
        c.tech = vec!["quickbooks".into()];
        c
    }

    fn hiring(title: &str, roles: &[&str], days_ago: i64) -> Signal {
        let mut s = Signal::new(
            "c1",
            SignalKind::Hiring,
            "jobs",
            title,
            format!("Hiring: {title}"),
            NOW - days_ago * 86_400,
        );
        s.roles = roles.iter().map(|r| r.to_string()).collect();
        s
    }

    #[test]
    fn a_strong_fit_scores_high_with_reasons_for_each_criterion() {
        let signals = vec![
            hiring("Supply Chain Planner", &["supply chain"], 5),
            hiring("Operations Manager", &["operations"], 10),
            hiring("Inventory Analyst", &["supply chain"], 20),
            Signal::new(
                "c1",
                SignalKind::Funding,
                "news",
                "n1",
                "Acme raises $10M",
                NOW - 86_400,
            ),
        ];
        let fit = score(&company(), &signals, &profile(), NOW);
        assert!(fit.score >= 85, "{fit:#?}");
        let crits: Vec<&str> = fit.reasons.iter().map(|r| r.criterion).collect();
        assert_eq!(
            crits,
            ["vertical", "size", "geo", "hiring", "news", "keywords"]
        );
        let hiring = &fit.reasons[3];
        assert_eq!(hiring.points, hiring.max);
        assert!(hiring.detail.contains("supply chain"), "{}", hiring.detail);
    }

    #[test]
    fn stale_or_off_target_signals_do_not_count() {
        let signals = vec![
            hiring("Supply Chain Planner", &["supply chain"], 400),
            hiring("Barista", &[], 3),
        ];
        let fit = score(&company(), &signals, &profile(), NOW);
        let h = &fit.reasons[3];
        assert_eq!(h.points, 0.0);
        assert!(
            h.detail.contains("none in target functions"),
            "{}",
            h.detail
        );
    }

    #[test]
    fn out_of_range_size_and_territory_score_zero_but_unknowns_score_half() {
        let p = profile();
        let mut c = company();
        c.employees = Some(10_000);
        c.hq_country = Some("DE".into());
        let fit = score(&c, &[], &p, NOW);
        assert_eq!(fit.reasons[1].points, 0.0);
        assert_eq!(fit.reasons[2].points, 0.0);

        c.employees = None;
        c.hq_country = None;
        let fit = score(&c, &[], &p, NOW);
        assert_eq!(fit.reasons[1].points, p.weights.size / 2.0);
        assert_eq!(fit.reasons[2].points, p.weights.geo / 2.0);
    }

    #[test]
    fn wrong_vertical_is_visible_in_the_reasons() {
        let mut c = company();
        c.verticals = vec!["vertical-saas".into()];
        let fit = score(&c, &[], &profile(), NOW);
        assert_eq!(fit.reasons[0].points, 0.0);
        assert!(fit.reasons[0].detail.starts_with("not in"));
    }

    #[test]
    fn phrases_need_word_boundaries() {
        assert!(phrase("supply chain planner", "supply chain"));
        assert!(!phrase("operationsx", "operations"));
    }
}
