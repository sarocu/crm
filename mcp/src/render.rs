//! Rendering results as text.
//!
//! The structured payload is the machine-readable answer; this is the part
//! a model actually reads. It is compact on purpose — every wasted line is
//! context that could have held another result.

use crm_core::market::{MarketConfig, Profile, Vertical};
use crm_core::model::{Account, Activity, Company, Signal};
use serde_json::Value;

use crate::score::Fit;

pub fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

pub fn fmt_ts_opt(ts: Option<i64>) -> Option<String> {
    ts.map(fmt_ts)
}

/// "Denver, CO, US".
pub fn hq(c: &Company) -> Option<String> {
    let parts: Vec<&str> = [
        c.hq_city.as_deref(),
        c.hq_state.as_deref(),
        c.hq_country.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// One company as a list entry: name line, then a line of facts.
pub fn company_entry(
    i: usize,
    c: &Company,
    account: Option<&Account>,
    fit: Option<&Fit>,
) -> String {
    let mut out = format!("{}. {}", i + 1, c.name);
    if let Some(d) = &c.domain {
        out.push_str(&format!(" ({d})"));
    }
    if let Some(f) = fit {
        out.push_str(&format!(" — fit {}/100", f.score));
    }
    out.push_str(&format!("  id={}\n", c.id));

    let mut facts = Vec::new();
    if !c.verticals.is_empty() {
        facts.push(c.verticals.join(", "));
    }
    if let Some(n) = c.employees {
        facts.push(format!("{n} employees"));
    }
    if let Some(h) = hq(c) {
        facts.push(format!("HQ {h}"));
    }
    if let Some(t) = &c.ticker {
        facts.push(format!("ticker {t}"));
    }
    if let Some(ts) = c.last_signal_at {
        facts.push(format!("last signal {}", fmt_ts(ts)));
    }
    match account {
        Some(a) => facts.push(format!("status: {}", a.status)),
        None => facts.push("status: not yet an account".into()),
    }
    out.push_str(&format!("   {}\n", facts.join(" · ")));

    if let Some(f) = fit {
        for r in f.reasons.iter().filter(|r| r.points > 0.0) {
            let mark = if r.points >= r.max { '+' } else { '~' };
            out.push_str(&format!("   {mark} {}: {}\n", r.criterion, r.detail));
        }
        for r in f.reasons.iter().filter(|r| r.points == 0.0 && r.max > 0.0) {
            out.push_str(&format!("   − {}: {}\n", r.criterion, r.detail));
        }
    } else if !c.description.is_empty() {
        out.push_str(&format!(
            "   {}\n",
            crm_core::model::truncate_chars(&c.description, 200)
        ));
    }
    out
}

pub fn signal_line(s: &Signal, with_company: bool) -> String {
    let mut out = format!("{} [{}] {}", fmt_ts(s.occurred_at), s.kind, s.title);
    if with_company && !s.company_name.is_empty() {
        out.push_str(&format!(
            " — {} (company_id={})",
            s.company_name, s.company_id
        ));
    }
    if !s.roles.is_empty() {
        out.push_str(&format!(" · roles: {}", s.roles.join(", ")));
    }
    if let Some(l) = &s.location {
        out.push_str(&format!(" · {l}"));
    }
    if let Some(u) = &s.url {
        out.push_str(&format!("\n   {u}"));
    }
    out
}

pub fn activity_line(a: &Activity) -> String {
    let mut out = format!("{} [{}", fmt_ts(a.occurred_at), a.activity_type);
    if let Some(d) = &a.direction {
        out.push_str(&format!(", {d}"));
    }
    out.push_str(&format!("] by {}", a.actor));
    if let Some(s) = &a.subject {
        out.push_str(&format!(": {s}"));
    }
    if !a.summary.is_empty() {
        out.push_str(&format!(" — {}", a.summary));
    }
    if let Some(o) = &a.outcome {
        out.push_str(&format!(" (outcome: {o})"));
    }
    out
}

pub fn account_block(a: &Account) -> String {
    let mut out = format!("Account: {}", a.status);
    if let Some(o) = &a.owner {
        out.push_str(&format!(" · owner {o}"));
    }
    if let Some(p) = &a.fit_profile {
        out.push_str(&format!(" · profile {p}"));
    }
    out.push('\n');
    if let Some(n) = &a.next_step {
        out.push_str(&format!("   next step: {n}"));
        if let Some(t) = a.next_touch_at {
            out.push_str(&format!(" (by {})", fmt_ts(t)));
        }
        out.push('\n');
    } else if let Some(t) = a.next_touch_at {
        out.push_str(&format!("   next touch: {}\n", fmt_ts(t)));
    }
    if let Some(r) = &a.disqualify_reason {
        out.push_str(&format!("   disqualified: {r}\n"));
    }
    if !a.tags.is_empty() {
        out.push_str(&format!("   tags: {}\n", a.tags.join(", ")));
    }
    out.push_str(&format!(
        "   updated {} by {}\n",
        fmt_ts(a.updated_at),
        a.updated_by
    ));
    out
}

pub fn dossier(
    c: &Company,
    account: Option<&Account>,
    signals: &[Signal],
    activities: &[Activity],
    fits: &[Fit],
) -> String {
    let mut out = format!("{}  id={}\n", c.name, c.id);
    let mut facts = Vec::new();
    if let Some(w) = c.website.as_ref().or(c.domain.as_ref()) {
        facts.push(w.clone());
    }
    if let Some(h) = hq(c) {
        facts.push(format!("HQ {h}"));
    }
    if let Some(n) = c.employees {
        facts.push(format!("{n} employees"));
    }
    if let Some(y) = c.founded {
        facts.push(format!("founded {y}"));
    }
    if let Some(t) = &c.ticker {
        facts.push(format!("ticker {t}"));
    }
    if !facts.is_empty() {
        out.push_str(&format!("{}\n", facts.join(" · ")));
    }
    if !c.verticals.is_empty() {
        out.push_str(&format!("Verticals: {}\n", c.verticals.join(", ")));
    }
    if !c.industries.is_empty() {
        out.push_str(&format!("Industries: {}\n", c.industries.join(", ")));
    }
    if !c.tech.is_empty() {
        out.push_str(&format!("Seen on their site: {}\n", c.tech.join(", ")));
    }
    if let (Some(p), Some(s)) = (&c.ats_provider, &c.ats_slug) {
        out.push_str(&format!("Job board: {p}/{s}\n"));
    }
    if !c.description.is_empty() {
        out.push_str(&format!("\n{}\n", c.description));
    }

    out.push('\n');
    match account {
        Some(a) => out.push_str(&account_block(a)),
        None => out.push_str("Account: none yet — log_activity or update_account creates one.\n"),
    }

    if !fits.is_empty() {
        out.push_str("\nFit by profile:\n");
        for f in fits {
            out.push_str(&format!("  {} — {}/100\n", f.profile, f.score));
            for r in &f.reasons {
                let mark = if r.points >= r.max && r.max > 0.0 {
                    '+'
                } else if r.points > 0.0 {
                    '~'
                } else {
                    '−'
                };
                out.push_str(&format!("    {mark} {}: {}\n", r.criterion, r.detail));
            }
        }
    }

    out.push_str(&format!("\nRecent signals ({}):\n", signals.len()));
    if signals.is_empty() {
        out.push_str("  none\n");
    }
    for s in signals {
        out.push_str(&format!(
            "  {}\n",
            signal_line(s, false).replace('\n', "\n  ")
        ));
    }

    out.push_str(&format!("\nActivity ({}):\n", activities.len()));
    if activities.is_empty() {
        out.push_str("  none\n");
    }
    for a in activities {
        out.push_str(&format!("  {}\n", activity_line(a)));
    }
    out
}

pub fn vertical(v: &Vertical, count: Option<usize>) -> String {
    let mut out = format!("{} — {}", v.slug, v.name);
    if let Some(n) = count {
        out.push_str(&format!(" ({n} companies)"));
    }
    out.push('\n');
    if !v.aliases.is_empty() {
        out.push_str(&format!("   aka {}\n", v.aliases.join(", ")));
    }
    if !v.sic.is_empty() {
        out.push_str(&format!("   SIC {}\n", v.sic.join(", ")));
    }
    if !v.naics.is_empty() {
        out.push_str(&format!("   NAICS {}\n", v.naics.join(", ")));
    }
    if !v.keywords.is_empty() {
        out.push_str(&format!("   keywords: {}\n", v.keywords.join(", ")));
    }
    out
}

pub fn profile(p: &Profile) -> String {
    let mut out = format!("{} — {}\n", p.slug, p.name);
    if !p.description.is_empty() {
        out.push_str(&format!("   {}\n", p.description));
    }
    out.push_str(&format!("   verticals: {}\n", p.verticals.join(", ")));
    if !p.countries.is_empty() || !p.states.is_empty() {
        let mut t = p.countries.clone();
        t.extend(p.states.iter().cloned());
        out.push_str(&format!("   territory: {}\n", t.join(", ")));
    }
    match (p.employees.min, p.employees.max) {
        (None, None) => {}
        (a, b) => out.push_str(&format!(
            "   employees: {}–{}\n",
            a.map(|v| v.to_string()).unwrap_or_default(),
            b.map(|v| v.to_string()).unwrap_or_default()
        )),
    }
    if !p.signals.hiring_roles.is_empty() {
        out.push_str(&format!(
            "   hiring for: {} (last {} days)\n",
            p.signals.hiring_roles.join(", "),
            p.signals.recency_days
        ));
    }
    if !p.keywords.is_empty() {
        out.push_str(&format!("   keywords: {}\n", p.keywords.join(", ")));
    }
    let w = p.weights;
    out.push_str(&format!(
        "   weights: vertical {} · size {} · geo {} · hiring {} · news {} · keywords {}\n",
        w.vertical, w.size, w.geo, w.hiring, w.news, w.keywords
    ));
    out
}

/// `describe_market`'s text from its own structured payload, so the two
/// can never disagree.
pub fn market(v: &Value, m: &MarketConfig) -> String {
    let mut out = format!("{}\n\n", v["market"].as_str().unwrap_or_default());
    if let Some(c) = v["counts"].as_object() {
        let parts: Vec<String> = c
            .iter()
            .map(|(k, n)| {
                format!(
                    "{k}: {}",
                    n.as_u64().map(|x| x.to_string()).unwrap_or("?".into())
                )
            })
            .collect();
        out.push_str(&format!("Indexed — {}\n", parts.join(" · ")));
    }
    if let Some(f) = v["last_updated"].as_object() {
        let parts: Vec<String> = f
            .iter()
            .map(|(k, t)| format!("{k} {}", t.as_str().unwrap_or("never")))
            .collect();
        out.push_str(&format!("Freshness — {}\n", parts.join(" · ")));
    }

    out.push_str("\nVerticals:\n");
    for vert in &m.verticals {
        let n = v["verticals"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["slug"] == vert.slug.as_str()))
            .and_then(|x| x["companies"].as_u64());
        out.push_str(&format!(
            "  {} — {} ({} companies)\n",
            vert.slug,
            vert.name,
            n.map(|x| x.to_string()).unwrap_or("?".into())
        ));
    }
    out.push_str("\nProfiles:\n");
    for p in &m.profiles {
        out.push_str(&format!("  {} — {}\n", p.slug, p.name));
    }
    for (key, label) in [
        ("pipeline", "Pipeline"),
        ("signals_last_90_days", "Signals, last 90 days"),
        ("company_sizes", "Company sizes"),
    ] {
        if let Some(o) = v[key].as_object()
            && !o.is_empty()
        {
            let parts: Vec<String> = o.iter().map(|(k, n)| format!("{k} {n}")).collect();
            out.push_str(&format!("\n{label}: {}\n", parts.join(" · ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crm_core::model::{AccountStatus, SignalKind};

    fn company() -> Company {
        let mut c = Company::new("abc", "Acme Brewing", "crawl");
        c.domain = Some("acme.test".into());
        c.verticals = vec!["craft-beverage".into()];
        c.employees = Some(80);
        c.hq_city = Some("Denver".into());
        c.hq_state = Some("CO".into());
        c
    }

    #[test]
    fn list_entries_carry_the_id_and_status() {
        let s = company_entry(0, &company(), None, None);
        assert!(s.starts_with("1. Acme Brewing (acme.test)"));
        assert!(s.contains("id=abc"));
        assert!(s.contains("status: not yet an account"));
        assert!(s.contains("HQ Denver, CO"));
    }

    #[test]
    fn a_dossier_shows_account_signals_and_activity() {
        let mut a = Account::new("abc", "Acme Brewing", "bdr");
        a.status = AccountStatus::Contacted;
        a.next_step = Some("Follow up".into());
        let s = Signal::new("abc", SignalKind::Hiring, "jobs", "1", "Hiring: Planner", 0);
        let act = Activity {
            id: "x".into(),
            account_id: "abc".into(),
            company_name: "Acme Brewing".into(),
            activity_type: crm_core::model::ActivityType::Email,
            direction: Some("outbound".into()),
            subject: Some("Hello".into()),
            summary: "Intro".into(),
            outcome: None,
            occurred_at: 0,
            actor: "bdr".into(),
            created_at: 0,
        };
        let d = dossier(&company(), Some(&a), &[s], &[act], &[]);
        assert!(d.contains("Account: contacted"));
        assert!(d.contains("next step: Follow up"));
        assert!(d.contains("[hiring] Hiring: Planner"));
        assert!(d.contains("[email, outbound] by bdr: Hello — Intro"));
    }
}
