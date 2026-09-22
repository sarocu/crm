//! Print every index's settings as JSON, keyed by index uid.
//!
//!   cargo run -q -p crm-core --example print_settings -- market.example.toml
//!
//! For applying the schema to a Meilisearch no service has booted against
//! yet — `PATCH /indexes/<uid>/settings` with each value — without handing
//! the master key to a process that does not otherwise need it.

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "market.example.toml".into());
    let market = crm_core::MarketConfig::load(&path).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let map: serde_json::Map<String, serde_json::Value> = crm_core::index::all_settings(&market)
        .into_iter()
        .map(|(uid, s)| {
            (
                uid.to_string(),
                serde_json::to_value(s).expect("settings serialize"),
            )
        })
        .collect();
    println!("{}", serde_json::Value::Object(map));
}
