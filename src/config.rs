//! Environment parsing + constants. Mirrors src/config.js.

use std::env;
use std::path::PathBuf;

pub const UPSTREAM_HOST: &str = "api.anthropic.com";
pub const BETA_FLAG: &str = "extended-cache-ttl-2025-04-11";

/// Client-sent breakpoints on system blocks smaller than this waste a slot.
pub const MIN_SYSTEM_CACHE_CHARS: usize = 500;

pub const BREAKPOINT_CEILING: usize = 4;

/// Hard cap on the per-response tee buffer used for metering. Beyond this we
/// drop metering for that response rather than grow memory unbounded — the
/// latent risk that, combined with synchronous decode, hung the Node version.
pub const MAX_METER_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub bind_host: String,
    pub auto_cache: bool,
    pub log_bodies: bool,
    pub log_dir: PathBuf,
    pub tail_ttl: String,
    pub model_override: String,
    pub metrics: bool,
    pub dashboard: bool,
    pub dashboard_token: String,
    pub db_path: String,
    /// Whether the native body transform runs. Enabled by `TRANSFORM=1`; also
    /// honors a non-empty `TRANSFORM_FILE` for back-compat with old invocations
    /// (the path is ignored — the transform is native Rust now).
    pub transform_enabled: bool,
}

fn flag(key: &str) -> bool {
    env::var(key).ok().as_deref() == Some("1")
}

fn non_empty(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// TAIL_TTL must be 5m|1h; anything else warns and falls back to 5m.
pub fn parse_tail_ttl(raw: Option<String>) -> String {
    match raw.map(|s| s.trim().to_lowercase()) {
        None => "5m".to_string(),
        Some(v) if v.is_empty() => "5m".to_string(),
        Some(v) if v == "5m" || v == "1h" => v,
        Some(other) => {
            eprintln!("TAIL_TTL must be one of 5m|1h (got \"{other}\"), falling back to 5m");
            "5m".to_string()
        }
    }
}

fn resolve(path: &str) -> String {
    if path == ":memory:" {
        return path.to_string();
    }
    match std::fs::canonicalize(path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        // canonicalize fails when the file doesn't exist yet (first run); fall
        // back to an absolute join against cwd so behavior matches Node's
        // path.resolve (which never touches the filesystem).
        Err(_) => {
            let p = PathBuf::from(path);
            if p.is_absolute() {
                p.to_string_lossy().into_owned()
            } else {
                env::current_dir()
                    .map(|c| c.join(&p).to_string_lossy().into_owned())
                    .unwrap_or_else(|_| path.to_string())
            }
        }
    }
}

pub fn load_config() -> Config {
    let dashboard = flag("DASHBOARD");
    Config {
        port: env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8898),
        bind_host: non_empty("BIND_HOST").unwrap_or_else(|| "127.0.0.1".to_string()),
        auto_cache: flag("AUTO_CACHE"),
        log_bodies: flag("LOG_BODIES"),
        log_dir: PathBuf::from(resolve(&non_empty("LOG_DIR").unwrap_or_else(|| "./logs".to_string()))),
        tail_ttl: parse_tail_ttl(env::var("TAIL_TTL").ok()),
        model_override: env::var("MODEL_OVERRIDE").unwrap_or_default(),
        // Dashboard implies metering (nothing to show otherwise).
        metrics: flag("METRICS") || dashboard,
        dashboard,
        dashboard_token: env::var("DASHBOARD_TOKEN").unwrap_or_default(),
        db_path: resolve(&non_empty("DB_PATH").unwrap_or_else(|| "./data/metrics.db".to_string())),
        transform_enabled: flag("TRANSFORM") || non_empty("TRANSFORM_FILE").is_some(),
    }
}
