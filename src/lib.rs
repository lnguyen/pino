//! pino — a local reverse proxy in front of api.anthropic.com.
//!
//! Faithful Rust port of the original Node implementation. The proxy buffers
//! each request body so it can be parsed and mutated (prompt-cache breakpoint
//! injection, model override, body transforms), then streams the upstream
//! response straight back to the client while tee-ing a bounded copy off to a
//! dedicated metering worker — keeping gzip decode and SQLite writes off the
//! request path entirely (the bug that made the Node version hang under load).

pub mod cache;
pub mod config;
pub mod dashboard;
pub mod http_decode;
pub mod identity;
pub mod logger;
pub mod model;
pub mod server;
pub mod store;
pub mod transform;
pub mod usage;
