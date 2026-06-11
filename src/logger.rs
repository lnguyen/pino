//! Timestamps, header redaction, and LOG_BODIES file dumps. Mirrors src/logger.js.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};
use time::OffsetDateTime;

/// "YYYY-MM-DD HH:MM:SS" in UTC (matches new Date().toISOString().slice(0,19)).
pub fn ts() -> String {
    let d = OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        d.year(),
        d.month() as u8,
        d.day(),
        d.hour(),
        d.minute(),
        d.second(),
    )
}

/// ISO timestamp with `:` and `.` replaced by `-` (filesystem-safe), e.g.
/// "2026-06-11T15-18-38-672Z". Matches Node's fileTs().
pub fn file_ts() -> String {
    let d = OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}-{:02}-{:02}-{:03}Z",
        d.year(),
        d.month() as u8,
        d.day(),
        d.hour(),
        d.minute(),
        d.second(),
        d.millisecond(),
    )
}

pub fn log(msg: &str) {
    println!("[{}] {}", ts(), msg);
}

/// Current time as epoch milliseconds (Date.now() equivalent).
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A short random suffix for request ids: 6 base36 chars, like
/// Math.random().toString(36).slice(2,8).
pub fn rand_suffix() -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    (0..6)
        .map(|_| ALPHABET[fastrand::usize(..ALPHABET.len())] as char)
        .collect()
}

/// Redact secret-bearing headers for logging. Accepts a JSON object of headers.
pub fn sanitize_headers(headers: &Value) -> Value {
    let mut out = headers.clone();
    if let Some(obj) = out.as_object_mut() {
        for (k, v) in obj.iter_mut() {
            let lk = k.to_lowercase();
            if lk.contains("authorization") || lk.contains("x-api-key") {
                *v = Value::String("[redacted]".to_string());
            }
        }
    }
    out
}

fn safe_json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

/// Write `<reqId>.req.json` with sanitized headers + parsed body.
pub fn write_request_log(log_dir: &Path, req_id: &str, meta: Value, body: &[u8]) {
    let req_file = log_dir.join(format!("{req_id}.req.json"));
    let body_text = String::from_utf8_lossy(body);
    let payload = json!({ "meta": meta, "body": safe_json(&body_text) });
    if let Err(e) = fs::write(&req_file, serde_json::to_vec_pretty(&payload).unwrap_or_default()) {
        log(&format!("WARN req log failed: {e}"));
    }
}

/// Build the `# status=... \n# headers=...` preamble that `backfill` parses,
/// followed by the raw (still-encoded) response bytes.
pub fn response_log_preamble(status: u16, headers: &Value) -> Vec<u8> {
    format!("# status={status}\n# headers={}\n\n", headers).into_bytes()
}
