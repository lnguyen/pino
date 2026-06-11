//! Print a savings rollup from the metrics DB, no server needed. Mirrors
//! bin/cache-stats.js.
//!
//!   cache-stats [groupBy] [since]
//!   cache-stats agent 24h

use pino::logger::now_ms;
use pino::store::create_store;

fn parse_since(raw: &str) -> i64 {
    let bytes = raw.as_bytes();
    if let Some(&last) = bytes.last() {
        if matches!(last, b'm' | b'h' | b'd') {
            if let Ok(n) = raw[..raw.len() - 1].parse::<i64>() {
                let unit = match last {
                    b'm' => 60_000,
                    b'h' => 3_600_000,
                    _ => 86_400_000,
                };
                return now_ms() - n * unit;
            }
        }
    }
    raw.parse::<i64>().unwrap_or(0)
}

fn usd(v: f64) -> String {
    format!("${v:.2}")
}

fn tok(v: i64) -> String {
    if v >= 1_000_000 {
        format!("{:.1}M", v as f64 / 1e6)
    } else if v >= 1000 {
        format!("{:.0}k", v as f64 / 1e3)
    } else {
        v.to_string()
    }
}

fn short(key: &str, group_by: &str) -> String {
    match group_by {
        "project" => {
            if key == "unknown" {
                "unknown".to_string()
            } else {
                key.rsplit('/').next().unwrap_or(key).to_string()
            }
        }
        "session" | "agent" => {
            if key.is_empty() {
                "(none)".to_string()
            } else {
                key.chars().take(16).collect()
            }
        }
        _ => key.to_string(),
    }
}

fn pad(s: &str, n: usize) -> String {
    format!("{s:<n$}")
}
fn pad_l(s: &str, n: usize) -> String {
    format!("{s:>n$}")
}

fn main() {
    let group_by = std::env::args().nth(1).unwrap_or_else(|| "project".to_string());
    let since_arg = std::env::args().nth(2).unwrap_or_else(|| "0".to_string());
    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "./data/metrics.db".to_string());

    let store = create_store(&db_path);
    let since = parse_since(&since_arg);
    let rows = store.query_rollup(&group_by, since, 100);
    let t = store.totals(since);

    let since_label = if since != 0 {
        format!(" · since {since_arg}")
    } else {
        String::new()
    };
    println!("\n  pino savings · by {group_by}{since_label}\n");
    println!(
        "  {}{}{}{}{}{}",
        pad("name", 22),
        pad_l("reqs", 6),
        pad_l("cache rd", 11),
        pad_l("billed", 10),
        pad_l("saved", 10),
        pad_l("%", 7),
    );
    println!("  {}", "─".repeat(64));

    let getf = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
    let geti = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);

    for r in &rows {
        let key = r.get("key").and_then(|k| k.as_str()).unwrap_or("");
        println!(
            "  {}{}{}{}{}{}",
            pad(&short(key, &group_by), 22),
            pad_l(&geti(r, "requests").to_string(), 6),
            pad_l(&tok(geti(r, "cache_read")), 11),
            pad_l(&usd(getf(r, "cost_actual")), 10),
            pad_l(&usd(getf(r, "saved")), 10),
            pad_l(&format!("{:.0}%", getf(r, "pct")), 7),
        );
    }
    println!("  {}", "─".repeat(64));
    println!(
        "  {}{}{}{}{}{}",
        pad("TOTAL", 22),
        pad_l(&geti(&t, "requests").to_string(), 6),
        pad_l(&tok(geti(&t, "cache_read")), 11),
        pad_l(&usd(getf(&t, "cost_actual")), 10),
        pad_l(&usd(getf(&t, "saved")), 10),
        pad_l(&format!("{:.0}%", getf(&t, "pct")), 7),
    );
    println!(
        "\n  saved {} of {} list-price input cost\n",
        usd(getf(&t, "saved")),
        usd(getf(&t, "cost_uncached"))
    );
}
