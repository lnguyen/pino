//! The proxy. Buffers each request body so it can be parsed/mutated, streams the
//! upstream response straight back to the client while tee-ing a bounded copy to
//! the metering worker. Mirrors src/server.js — but decode + SQLite run on a
//! dedicated OS thread, never the async reactor (the Node hang fix).

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::StreamExt;
use http::header::{HeaderMap, HeaderValue};
use http::{Method, StatusCode};
use serde_json::{json, Value};

use crate::cache::{
    apply_ttls, inject_breakpoint_if_absent, strip_intermediate_message_breakpoints,
};
use crate::config::{Config, BETA_FLAG, MAX_METER_BYTES, UPSTREAM_HOST};
use crate::http_decode::{decode_body, model_from_response};
use crate::identity::{identify, Identity};
use crate::logger::{file_ts, log, now_ms, rand_suffix, response_log_preamble, sanitize_headers, write_request_log};
use crate::model::rewrite_system_model_refs;
use crate::store::{create_store, RowInput, SharedStore};
use crate::transform::Transform;
use crate::usage::{compute_cost, model_family, parse_usage};

const MAX_BODY: usize = 256 * 1024 * 1024;

/// Work handed to the metering thread once a response finishes streaming.
pub struct MeterJob {
    req_id: String,
    ts: i64,
    identity: Identity,
    fallback_model: String,
    buf: Vec<u8>,
    content_encoding: String,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Option<SharedStore>,
    pub transform: Option<Arc<Transform>>,
    pub client: reqwest::Client,
    pub meter_tx: Option<tokio::sync::mpsc::UnboundedSender<MeterJob>>,
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn is_messages_path(path: &str) -> bool {
    path == "/v1/messages" || path == "/v1/messages/count_tokens"
}

fn is_json_request(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("application/json"))
        .unwrap_or(false)
}

/// Lowercased header map as a JSON object (first value per name) — the shape
/// identity/logging expect, matching Node's already-lowercased req.headers.
fn headers_to_json(headers: &HeaderMap) -> Value {
    let mut obj = serde_json::Map::new();
    for (name, value) in headers.iter() {
        let k = name.as_str().to_lowercase();
        if !obj.contains_key(&k) {
            obj.insert(k, json!(value.to_str().unwrap_or("")));
        }
    }
    Value::Object(obj)
}

/// Append the 1h-TTL beta flag to anthropic-beta. Returns added|present|appended.
fn ensure_beta_header(headers: &mut HeaderMap) -> &'static str {
    match headers.get("anthropic-beta") {
        None => {
            headers.insert("anthropic-beta", HeaderValue::from_static(BETA_FLAG));
            "added"
        }
        Some(v) => {
            let existing = v.to_str().unwrap_or("").to_string();
            if existing.split(',').map(|s| s.trim()).any(|s| s == BETA_FLAG) {
                "present"
            } else {
                let combined = format!("{existing},{BETA_FLAG}");
                if let Ok(hv) = HeaderValue::from_str(&combined) {
                    headers.insert("anthropic-beta", hv);
                }
                "appended"
            }
        }
    }
}

/// Spawn the dedicated metering worker on its own OS thread. Decode + parse +
/// price + SQLite write all happen here, off the tokio reactor.
pub fn spawn_meter_worker(store: SharedStore) -> tokio::sync::mpsc::UnboundedSender<MeterJob> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<MeterJob>();
    std::thread::spawn(move || {
        while let Some(job) = rx.blocking_recv() {
            let text = decode_body(&job.buf, &job.content_encoding);
            if text.is_empty() {
                continue;
            }
            let Some(usage) = parse_usage(&text) else { continue };
            let model = model_from_response(&text, &job.fallback_model);
            let cost = compute_cost(&usage, &model);
            let family = if cost.family.is_empty() {
                model_family(&model).to_string()
            } else {
                cost.family.to_string()
            };
            let row = RowInput {
                req_id: job.req_id,
                ts: job.ts,
                session_id: job.identity.session_id,
                agent_id: job.identity.agent_id,
                parent_agent: job.identity.parent_agent_id,
                project: job.identity.project,
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
            };
            store.record_request(&row);
        }
    });
    tx
}

struct MeterCtx {
    tx: tokio::sync::mpsc::UnboundedSender<MeterJob>,
    identity: Identity,
    req_id: String,
    ts: i64,
    fallback_model: String,
    content_encoding: String,
}

struct LogCtx {
    log_dir: PathBuf,
    req_id: String,
    status: u16,
    headers_json: Value,
}

async fn handle(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();

    // Control plane (dashboard, API, health probes) short-circuits the proxy.
    if let Some(resp) = crate::dashboard::try_handle(
        &state.config,
        &state.store,
        &parts.method,
        &parts.uri,
        &parts.headers,
    ) {
        return resp;
    }

    let raw = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "could not read request body").into_response();
        }
    };

    let cfg = &state.config;
    let method = parts.method.clone();
    let path = parts.uri.path().to_string();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| path.clone());

    let is_messages = method == Method::POST
        && is_messages_path(&path)
        && is_json_request(&parts.headers)
        && !raw.is_empty();

    let mutate =
        is_messages && (cfg.auto_cache || cfg.transform_enabled || !cfg.model_override.is_empty());
    let meter = cfg.metrics
        && state.store.is_some()
        && is_messages
        && path != "/v1/messages/count_tokens";

    let req_id = format!("{}-{}", file_ts(), rand_suffix());
    let started_at = now_ms();

    // Parse once if we're going to mutate.
    let mut parsed: Option<Value> = None;
    if mutate {
        match serde_json::from_slice::<Value>(&raw) {
            Ok(v) => parsed = Some(v),
            Err(e) => log(&format!("WARN parse failed, forwarding original body: {e}")),
        }
    }

    let headers_json = headers_to_json(&parts.headers);

    // Identity for the dashboard (needs the body for project extraction).
    let identity = if meter {
        let id_body = match &parsed {
            Some(v) => v.clone(),
            None => serde_json::from_slice::<Value>(&raw).unwrap_or_else(|_| json!({})),
        };
        Some(identify(&headers_json, &id_body))
    } else {
        None
    };

    let mut notes: Vec<String> = Vec::new();

    if let Some(body) = parsed.as_mut() {
        if !cfg.model_override.is_empty() {
            let prev = body
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            body["model"] = json!(cfg.model_override);
            let hits = rewrite_system_model_refs(body, &cfg.model_override);
            notes.push(format!(
                "model={prev}->{},sys-rewrites={hits}",
                cfg.model_override
            ));
        }

        if let Some(tf) = &state.transform {
            tf.apply(body);
            notes.push("transform=ok".to_string());
        }

        if cfg.auto_cache {
            let stripped_mid = strip_intermediate_message_breakpoints(body);
            let tag = inject_breakpoint_if_absent(body, &cfg.tail_ttl);
            let c = apply_ttls(body, &cfg.tail_ttl);
            notes.push(format!(
                "cache=rewrote:{},already:{},skipped:{},inject:{},mid-stripped:{},tail-ttl:{}",
                c.rewritten, c.already, c.skipped, tag, stripped_mid, cfg.tail_ttl
            ));
        }
    }

    let fallback_model = parsed
        .as_ref()
        .and_then(|p| p.get("model"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    let out_body: Vec<u8> = match &parsed {
        Some(v) => serde_json::to_vec(v).unwrap_or_else(|_| raw.to_vec()),
        None => raw.to_vec(),
    };

    // Build upstream headers: verbatim except host/content-length/hop-by-hop.
    let mut out_headers = HeaderMap::new();
    for (name, value) in parts.headers.iter() {
        let n = name.as_str().to_lowercase();
        if n == "host" || n == "content-length" || is_hop_by_hop(&n) {
            continue;
        }
        out_headers.insert(name.clone(), value.clone());
    }
    let beta_status = if cfg.auto_cache && parsed.is_some() {
        ensure_beta_header(&mut out_headers)
    } else {
        "skipped"
    };
    notes.push(format!("beta={beta_status}"));

    if cfg.log_bodies {
        let meta = json!({
            "method": method.as_str(),
            "url": path_and_query,
            "headers": sanitize_headers(&headers_to_json(&out_headers)),
            "mutated": parsed.is_some(),
        });
        write_request_log(&cfg.log_dir, &req_id, meta, &out_body);
    }

    // --- forward to upstream ---
    let url = format!("https://{UPSTREAM_HOST}{path_and_query}");
    let upstream = match state
        .client
        .request(method.clone(), &url)
        .headers(out_headers)
        .body(out_body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            log(&format!("ERR upstream: {e}"));
            return (
                StatusCode::BAD_GATEWAY,
                format!("proxy upstream error: {e}"),
            )
                .into_response();
        }
    };

    let status = upstream.status();
    let up_headers = upstream.headers().clone();
    let content_encoding = up_headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    log(&format!(
        "{} {} -> {} [id={} {}{}]",
        method,
        path_and_query,
        status.as_u16(),
        req_id,
        if notes.is_empty() {
            "pass-through".to_string()
        } else {
            notes.join(" ")
        },
        if meter { " metered" } else { "" },
    ));

    // Metering + body-logging context, moved into the tee stream.
    let meter_ctx = if meter {
        Some(MeterCtx {
            tx: state.meter_tx.clone().unwrap(),
            identity: identity.unwrap_or_default(),
            req_id: req_id.clone(),
            ts: started_at,
            fallback_model,
            content_encoding,
        })
    } else {
        None
    };
    let log_ctx = if cfg.log_bodies {
        Some(LogCtx {
            log_dir: cfg.log_dir.clone(),
            req_id: req_id.clone(),
            status: status.as_u16(),
            headers_json: headers_to_json(&up_headers),
        })
    } else {
        None
    };

    let body_stream = async_stream::stream! {
        let mut meter_buf: Option<Vec<u8>> = if meter_ctx.is_some() { Some(Vec::new()) } else { None };
        let mut log_buf: Option<Vec<u8>> = if log_ctx.is_some() { Some(Vec::new()) } else { None };
        let mut over = false;
        let mut s = upstream.bytes_stream();
        while let Some(item) = s.next().await {
            match item {
                Ok(chunk) => {
                    if let Some(b) = meter_buf.as_mut() {
                        if !over {
                            b.extend_from_slice(&chunk);
                            if b.len() > MAX_METER_BYTES {
                                over = true;
                                meter_buf = None;
                                log("WARN meter buffer cap exceeded, skipping metering for this response");
                            }
                        }
                    }
                    if let Some(lb) = log_buf.as_mut() {
                        lb.extend_from_slice(&chunk);
                    }
                    yield Ok::<bytes::Bytes, std::io::Error>(chunk);
                }
                Err(e) => {
                    yield Err(std::io::Error::new(std::io::ErrorKind::Other, e));
                    break;
                }
            }
        }
        if let (Some(buf), Some(ctx)) = (meter_buf.take(), meter_ctx) {
            let _ = ctx.tx.send(MeterJob {
                req_id: ctx.req_id,
                ts: ctx.ts,
                identity: ctx.identity,
                fallback_model: ctx.fallback_model,
                buf,
                content_encoding: ctx.content_encoding,
            });
        }
        if let (Some(buf), Some(ctx)) = (log_buf.take(), log_ctx) {
            let resp_file = ctx.log_dir.join(format!("{}.resp.log", ctx.req_id));
            let mut bytes = response_log_preamble(ctx.status, &ctx.headers_json);
            bytes.extend_from_slice(&buf);
            let _ = std::fs::write(resp_file, bytes);
        }
    };

    // Downstream response: upstream status + headers verbatim (minus hop-by-hop).
    let mut builder = Response::builder().status(status);
    for (name, value) in up_headers.iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }
    match builder.body(Body::from_stream(body_stream)) {
        Ok(resp) => resp,
        Err(e) => {
            log(&format!("ERR building response: {e}"));
            (StatusCode::BAD_GATEWAY, "proxy response error").into_response()
        }
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new().fallback(any(handle)).with_state(state)
}

/// Build everything and return (router, bind_addr_string). Wiring lives here so
/// main.rs stays a thin shell.
pub fn build_app(config: Config) -> (Router, String) {
    let bind = format!("{}:{}", config.bind_host, config.port);

    let store: Option<SharedStore> = if config.metrics {
        let s = create_store(&config.db_path);
        log(&format!("metrics store: {}", config.db_path));
        Some(s)
    } else {
        None
    };

    let meter_tx = match &store {
        Some(s) if config.metrics => Some(spawn_meter_worker(s.clone())),
        _ => None,
    };

    let transform = if config.transform_enabled {
        Some(Arc::new(Transform::from_env()))
    } else {
        None
    };

    let client = reqwest::Client::builder()
        .build()
        .expect("build reqwest client");

    let state = AppState {
        config: Arc::new(config),
        store,
        transform,
        client,
        meter_tx,
    };

    (build_router(state), bind)
}