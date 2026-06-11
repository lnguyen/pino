//! Control plane: the dashboard UI, its JSON API, a live SSE stream, and the
//! health probes. Served off the same listener as the proxy. try_handle returns
//! Some(response) when it owns the request, letting the proxy hot path fall
//! through on None. Mirrors src/dashboard.js.

use std::convert::Infallible;
use std::time::Duration;

use axum::body::Body;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, Method, StatusCode, Uri};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::config::Config;
use crate::logger::now_ms;
use crate::store::SharedStore;

const PREFIX: &str = "/__pino";
const DASHBOARD_HTML: &str = include_str!("public/dashboard.html");

fn no_store(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    resp
}

fn text(status: StatusCode, body: &str, content_type: &str) -> Response {
    no_store(
        Response::builder()
            .status(status)
            .header("content-type", content_type)
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
}

fn json_resp(status: StatusCode, value: &Value) -> Response {
    no_store(
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
    )
}

fn query_param(uri: &Uri, key: &str) -> Option<String> {
    let q = uri.query()?;
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        if k == key {
            return Some(it.next().unwrap_or("").to_string());
        }
    }
    None
}

fn authorized(uri: &Uri, headers: &HeaderMap, token: &str) -> bool {
    if token.is_empty() {
        return true;
    }
    if query_param(uri, "token").as_deref() == Some(token) {
        return true;
    }
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .map(|h| h == format!("Bearer {token}"))
        .unwrap_or(false)
}

/// Accept absolute epoch ms, or a relative window like "24h" / "7d" / "30m".
fn parse_since(raw: Option<&str>) -> i64 {
    let Some(raw) = raw else { return 0 };
    if raw.is_empty() {
        return 0;
    }
    let bytes = raw.as_bytes();
    let last = bytes[bytes.len() - 1];
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
    raw.parse::<i64>().unwrap_or(0)
}

fn build_stats(store: &SharedStore, group_by: &str, since: i64) -> Value {
    let totals = store.totals(since);
    let mut rows = store.query_rollup(group_by, since, 200);
    if group_by == "session" {
        let meta = store.session_meta(since);
        for r in rows.iter_mut() {
            if let Some(key) = r.get("key").and_then(|k| k.as_str()) {
                if let Some(m) = meta.get(key) {
                    r["project"] = m.get("project").cloned().unwrap_or(Value::Null);
                    r["models"] = m.get("models").cloned().unwrap_or_else(|| json!({}));
                }
            }
        }
    }
    json!({ "groupBy": group_by, "since": since, "totals": totals, "rows": rows })
}

fn sse_response(store: &SharedStore) -> Response {
    let rx: broadcast::Receiver<Value> = store.subscribe();
    let hello = store.totals(0);

    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(Event::default().event("hello").data(hello.to_string()));
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(row) => {
                    yield Ok(Event::default().event("request").data(row.to_string()));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("ping"))
        .into_response()
}

/// Returns Some(response) if this request belongs to the control plane.
pub fn try_handle(
    config: &Config,
    store: &Option<SharedStore>,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Option<Response> {
    let path = uri.path();

    // --- health probes (always open, no token) ---
    if path == "/healthz" {
        return Some(text(StatusCode::OK, "ok", "text/plain"));
    }
    if path == "/readyz" {
        let ok = store.as_ref().map(|s| s.ready).unwrap_or(false);
        return Some(text(
            if ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE },
            if ok { "ready" } else { "not-ready" },
            "text/plain",
        ));
    }

    if !config.dashboard {
        return None;
    }

    // Convenience: GET / → the dashboard.
    if method == Method::GET && (path == "/" || path == "/index.html") {
        return Some(no_store(
            Response::builder()
                .status(StatusCode::FOUND)
                .header("location", format!("{PREFIX}/"))
                .body(Body::empty())
                .unwrap(),
        ));
    }

    if path != PREFIX && !path.starts_with(&format!("{PREFIX}/")) {
        return None;
    }

    // --- token gate for everything under /__pino ---
    if !authorized(uri, headers, &config.dashboard_token) {
        return Some(json_resp(StatusCode::UNAUTHORIZED, &json!({ "error": "unauthorized" })));
    }

    // --- dashboard page ---
    if path == PREFIX || path == format!("{PREFIX}/") || path == format!("{PREFIX}/index.html") {
        return Some(text(StatusCode::OK, DASHBOARD_HTML, "text/html; charset=utf-8"));
    }

    let Some(store) = store.as_ref() else {
        return Some(json_resp(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({ "error": "metrics disabled (set METRICS=1)" }),
        ));
    };

    let since = parse_since(query_param(uri, "since").as_deref());

    if path == format!("{PREFIX}/api/stats") {
        let group_by = query_param(uri, "groupBy").unwrap_or_else(|| "project".to_string());
        return Some(json_resp(StatusCode::OK, &build_stats(store, &group_by, since)));
    }

    if path == format!("{PREFIX}/api/series") {
        let buckets = query_param(uri, "buckets")
            .and_then(|b| b.parse::<i64>().ok())
            .unwrap_or(80)
            .clamp(10, 400);
        let bucket_ms = query_param(uri, "bucketMs").and_then(|b| b.parse::<i64>().ok());
        let mut out = store.series(since, buckets, bucket_ms);
        out["since"] = json!(since);
        return Some(json_resp(StatusCode::OK, &out));
    }

    if path == format!("{PREFIX}/api/requests") {
        let limit = query_param(uri, "limit")
            .and_then(|b| b.parse::<i64>().ok())
            .unwrap_or(50)
            .min(500);
        let project = query_param(uri, "project");
        let session = query_param(uri, "session");
        let rows = store.recent_requests(limit, project.as_deref(), session.as_deref());
        return Some(json_resp(StatusCode::OK, &json!({ "rows": rows })));
    }

    if path == format!("{PREFIX}/api/stream") {
        return Some(sse_response(store));
    }

    Some(json_resp(StatusCode::NOT_FOUND, &json!({ "error": "not found" })))
}
