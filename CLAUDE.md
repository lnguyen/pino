# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A small async Rust (tokio + hyper/axum) HTTP reverse proxy in front of `api.anthropic.com`.
Clients point `ANTHROPIC_BASE_URL` at the proxy; it forwards everything to upstream, optionally
mutating `/v1/messages` requests to improve prompt caching and/or shrink the request body, and
optionally metering token savings into SQLite with a live dashboard.

This is a Rust port of an earlier Node implementation. The port exists because the Node version
ran gzip decode (`gunzipSync`) and `node:sqlite` writes **synchronously on the event loop** during
metering, which stalled the proxy under load ("stopped responding"). In Rust those run on a
dedicated OS thread, off the async reactor.

## Building & running

```bash
cargo build --release        # target/release/{pino-proxy, backfill, cache-stats}
cargo test                   # unit + integration + JS-parity tests

# pure pass-through on :8898
cargo run --release

# typical full dev invocation:
AUTO_CACHE=1 TRANSFORM=1 \
  DROP_TOOLS=NotebookEdit,CronCreate,CronDelete,CronList \
  LOG_BODIES=1 cargo run --release

# metering + live dashboard (persists savings to SQLite):
METRICS=1 DASHBOARD=1 AUTO_CACHE=1 cargo run --release   # dashboard at /__pino/ on :8898

# the two CLI tools:
cargo run --release --bin backfill        # seed the DB from existing logs/ ([LOG_DIR] [DB_PATH])
cargo run --release --bin cache-stats project   # terminal rollup (project|session|agent|model [since])
```

Default port is **8898**. Requires Rust >= 1.80. SQLite is bundled via `rusqlite` (no system lib).

### Run on a laptop (Docker Compose)

```bash
docker compose up -d --build     # http://localhost:8898/__pino/ , DB persists in ./data
```

The image is a multi-stage Rust build (`rust:1-slim` → `debian:slim`); the dashboard HTML is
embedded into the binary, so the runtime image is just the binary + CA certs. Kubernetes manifests
live in `deploy/k8s/pino.yaml` (single replica; PVC-backed SQLite; `DASHBOARD_TOKEN` gate).

## Layout

```
src/main.rs           # pino-proxy entry: tokio main, --healthcheck mode, build_app + serve
src/server.rs         # axum handler; mutation pipeline; streaming tee; spawn_meter_worker
src/config.rs         # load_config() from env; constants (UPSTREAM_HOST, BETA_FLAG, ceilings)
src/cache.rs          # inject_breakpoint_if_absent, apply_ttls, strip_* — see TTL note below
src/model.rs          # rewrite_system_model_refs — model id/name rewrites for MODEL_OVERRIDE
src/usage.rs          # parse_usage (regex, SSE/JSON), compute_cost, model_family, prices
src/identity.rs       # session/agent ids from headers; project mined from system-prompt cwd (memoized)
src/store.rs          # rusqlite metrics store; broadcast::Sender<Value> drives live SSE
src/dashboard.rs      # control plane: /__pino UI, /api/stats|series|requests, /api/stream (SSE), /healthz, /readyz
src/http_decode.rs    # decode_body (gzip/br/deflate/zstd) + model_from_response — shared by worker & backfill
src/transform.rs      # native env-driven transform (DROP_TOOLS, STRIP_ANSI, TRIM_BASH_GIT, restructure_v123)
src/logger.rs         # ts/file_ts/log, sanitize_headers, write_request_log, now_ms
src/public/dashboard.html   # the live dashboard (Chart.js via CDN, SSE) — embedded via include_str!
src/bin/backfill.rs   # populate metrics db from logs/ via the same usage/identity modules
src/bin/cache_stats.rs# print a savings rollup from the db (no server needed)
tests/                # usage.rs, identity.rs, store.rs (ported), parity.rs (diff vs JS)
parity/               # vendored JS reference (cache.js/config.js) + js_mutate.mjs for the parity test
test/fixtures/        # real captured SSE + request body (gitignored)
```

## Architecture

### src/server.rs — the proxy

A single axum fallback handler streams every request to `api.anthropic.com` over HTTPS (reqwest +
rustls, **no** auto-decompression so the client receives upstream bytes verbatim). Request bodies
are buffered (so they can be parsed and mutated); responses are streamed straight through and
optionally tee'd.

Mutation applies only when **all** hold: `POST`, path `/v1/messages` (or `…/count_tokens`),
`content-type: application/json`, non-empty body, and at least one of `AUTO_CACHE` / `TRANSFORM` /
`MODEL_OVERRIDE`. Otherwise bytes are forwarded untouched.

Order of operations on the parsed body (identical to the original Node pipeline):
1. `MODEL_OVERRIDE` — replaces `body.model` and rewrites model-name references in `body.system`
   via `rewrite_system_model_refs` (`src/model.rs`).
2. native `transform` (`src/transform.rs`) when `TRANSFORM=1`.
3. `strip_intermediate_message_breakpoints` then `inject_breakpoint_if_absent` — places up to four
   `cache_control` markers within the 4-breakpoint ceiling: strip small (<500 char) system
   breakpoints, then tools → system → `messages[0]` reminders (1h) and the rolling tail (`TAIL_TTL`).
4. `apply_ttls` — bumps every ephemeral breakpoint to 1h **except those in the last message**, which
   are forced to `TAIL_TTL` (the rolling tail moves every turn).
5. `ensure_beta_header` appends `extended-cache-ttl-2025-04-11` to `anthropic-beta`.

Headers are copied verbatim except `host`, `content-length`, and hop-by-hop headers.

### Metering — the off-reactor worker

When `METRICS=1` (implied by `DASHBOARD=1`), the response stream is tee'd into a **bounded** buffer
(`MAX_METER_BYTES`, 8 MB — beyond that, metering is skipped for that response and logged). On stream
end the buffer is sent over an `mpsc` channel to a dedicated OS thread (`spawn_meter_worker`) that
runs `decode_body → parse_usage → compute_cost → store.record_request`. This keeps decode + SQLite
writes off the tokio reactor entirely — the fix for the Node "stopped responding" hang.
`record_request` writes the row **and** publishes it on a `tokio::sync::broadcast` channel; the
dashboard's `/api/stream` SSE endpoint forwards every row live. The **Cache write tier** panel
surfaces `ephemeral_1h` vs `ephemeral_5m` write tokens.

### Key invariant — the reference-free TTL skip set (src/cache.rs)

The Node version protected the rolling-tail breakpoint from the blind 1h bump using a `Set` of
object *references*. `serde_json::Value` has no reference identity, so `apply_ttls` uses the
equivalent observation: the skip set is exactly *every ephemeral breakpoint in the last message*.
So it bumps everything outside the last message to 1h and forces every breakpoint inside the last
message to `TAIL_TTL` — the identical end state. `tests/parity.rs` proves this is byte-equivalent to
`cache.js` across representative bodies (run `cargo test --test parity`; needs `node`).

### src/transform.rs — the native body mutator

Env-driven, built once via `Transform::from_env()`:
- `DROP_TOOLS=Name1,Name2` — remove tools from `body.tools` **and** scrub their names out of any
  `<system-reminder>` block advertising deferred tools / ToolSearch (both must happen together).
- `STRIP_ANSI=0` to disable (default on) — strips ANSI escapes from message + tool-result content.
- `TRIM_BASH_GIT=1` — truncates the Bash tool description at `# Committing changes with git`.
- `restructure_v123` — hoists static core context (CLAUDE.md / skills / ToolSearch catalog) into
  `messages[0]` and drops stale `<system-reminder>` / `<command-name>` scaffolding from history.

`LOG_BODIES=1` writes `<reqId>.req.json` (post-mutation, auth redacted) and `<reqId>.resp.log`
(raw upstream bytes + header preamble) per request — the format `backfill` parses.

## Gotchas

- The proxy binds to `BIND_HOST` (default `127.0.0.1`; the Docker image sets `0.0.0.0`).
- Header passthrough is verbatim: `x-api-key` / `authorization` go upstream as-is (redacted only in logs).
- `serde_json` uses the `preserve_order` feature so object key order is stable across mutation.
- The metering buffer is capped (`MAX_METER_BYTES`); a response larger than the cap is still streamed
  to the client correctly but won't be metered.
- The transform on-switch is `TRANSFORM=1`; a non-empty `TRANSFORM_FILE` also enables it (back-compat),
  but the path is ignored — the transform is native Rust, not a loaded module.
- When changing mutation logic, keep `tests/parity.rs` green — it's the guard against drift from the
  original caching behavior. The vendored JS reference lives in `parity/`.
