//! Backfill the metrics DB from existing logs/ captures. Reads each
//! <reqId>.resp.log (compressed SSE) + its matching <reqId>.req.json, runs them
//! through the same usage/identity/cost modules the live proxy uses, and inserts
//! a row per request (idempotent: INSERT OR REPLACE by req_id). Mirrors bin/backfill.js.
//!
//!   backfill [LOG_DIR] [DB_PATH]

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use pino::http_decode::{decode_body, model_from_response};
use pino::identity::identify;
use pino::store::{create_store, RowInput};
use pino::usage::{compute_cost, model_family, parse_usage};

fn arg_or_env(idx: usize, env_key: &str, default: &str) -> String {
    std::env::args()
        .nth(idx)
        .or_else(|| std::env::var(env_key).ok())
        .unwrap_or_else(|| default.to_string())
}

/// Split a .resp.log into its `# headers=...` JSON and the raw (encoded) body.
fn split_resp_log(buf: &[u8]) -> (Value, Vec<u8>) {
    let sep = buf.windows(2).position(|w| w == b"\n\n");
    let Some(sep) = sep else {
        return (json!({}), Vec::new());
    };
    let preamble = String::from_utf8_lossy(&buf[..sep]);
    let body = buf[sep + 2..].to_vec();
    let mut headers = json!({});
    for line in preamble.lines() {
        if let Some(rest) = line.strip_prefix("# headers=") {
            if let Ok(v) = serde_json::from_str::<Value>(rest) {
                headers = v;
            }
        }
    }
    (headers, body)
}

fn main() {
    let log_dir = PathBuf::from(arg_or_env(1, "LOG_DIR", "./logs"));
    let db_path = arg_or_env(2, "DB_PATH", "./data/metrics.db");

    if !log_dir.exists() {
        eprintln!("No log dir at {}", log_dir.display());
        std::process::exit(1);
    }

    let store = create_store(&db_path);

    let mut inserted = 0u64;
    let mut skipped = 0u64;

    let entries = fs::read_dir(&log_dir).expect("read log dir");
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".resp.log") {
            continue;
        }
        let req_id = name.trim_end_matches(".resp.log").to_string();
        match backfill_one(&log_dir, &req_id, &store) {
            Ok(true) => inserted += 1,
            Ok(false) => skipped += 1,
            Err(e) => {
                skipped += 1;
                eprintln!("skip {req_id}: {e}");
            }
        }
    }

    let t = store.totals(0);
    let saved = t["saved"].as_f64().unwrap_or(0.0);
    let uncached = t["cost_uncached"].as_f64().unwrap_or(0.0);
    let pct = t["pct"].as_f64().unwrap_or(0.0);
    let requests = t["requests"].as_i64().unwrap_or(0);
    println!(
        "\nBackfill complete → {db_path}\n  inserted {inserted}, skipped {skipped}\n  {requests} requests · saved ${saved:.2} of ${uncached:.2} ({pct:.1}%)"
    );
}

fn backfill_one(log_dir: &Path, req_id: &str, store: &pino::store::SharedStore) -> Result<bool, String> {
    let resp_path = log_dir.join(format!("{req_id}.resp.log"));
    let raw = fs::read(&resp_path).map_err(|e| e.to_string())?;
    let (headers, body) = split_resp_log(&raw);
    let enc = headers
        .get("content-encoding")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let text = decode_body(&body, enc);
    let Some(usage) = parse_usage(&text) else {
        return Ok(false);
    };

    // Pair with the request log for identity + model fallback.
    let mut req_headers = json!({});
    let mut req_body = json!({});
    let req_file = log_dir.join(format!("{req_id}.req.json"));
    if req_file.exists() {
        let j: Value = serde_json::from_str(&fs::read_to_string(&req_file).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        req_headers = j
            .get("meta")
            .and_then(|m| m.get("headers"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        req_body = j.get("body").cloned().unwrap_or_else(|| json!({}));
    }

    let identity = identify(&req_headers, &req_body);
    let fallback = req_body.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let model = model_from_response(&text, fallback);
    let cost = compute_cost(&usage, &model);
    let ts = fs::metadata(&resp_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let family = if cost.family.is_empty() {
        model_family(&model).to_string()
    } else {
        cost.family.to_string()
    };

    store.record_request(&RowInput {
        req_id: req_id.to_string(),
        ts,
        session_id: identity.session_id,
        agent_id: identity.agent_id,
        parent_agent: identity.parent_agent_id,
        project: identity.project,
        model,
        family,
        input_tokens: usage.input_tokens,
        cache_read: usage.cache_read,
        cache_create: usage.cache_create,
        ephem_5m: usage.ephem_5m,
        ephem_1h: usage.ephem_1h,
        output_tokens: usage.output_tokens,
        cost_actual: cost.cost_actual,
        cost_uncached: cost.cost_uncached,
        saved: cost.saved,
        estimate: cost.estimate,
    });
    Ok(true)
}
