//! pino-proxy entry point. Loads config, builds the app, and serves.

use pino::config::load_config;
use pino::logger::log;
use pino::server::build_app;

/// `pino-proxy --healthcheck`: probe the local /healthz endpoint and exit 0/1.
/// Used by the Docker HEALTHCHECK so the runtime image needs no curl/node.
fn healthcheck() -> ! {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let port = std::env::var("PORT").ok().and_then(|p| p.parse::<u16>().ok()).unwrap_or(8898);
    let addr = format!("127.0.0.1:{port}");
    let ok = (|| -> std::io::Result<bool> {
        let mut s = TcpStream::connect(&addr)?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        s.write_all(format!("GET /healthz HTTP/1.0\r\nHost: {addr}\r\n\r\n").as_bytes())?;
        let mut buf = String::new();
        s.read_to_string(&mut buf)?;
        Ok(buf.starts_with("HTTP/1.0 200") || buf.starts_with("HTTP/1.1 200"))
    })()
    .unwrap_or(false);
    std::process::exit(if ok { 0 } else { 1 });
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--healthcheck") {
        healthcheck();
    }

    let config = load_config();

    if config.log_bodies {
        let _ = std::fs::create_dir_all(&config.log_dir);
    }

    let summary = format!(
        "settings: AUTO_CACHE={} TAIL_TTL={} MODEL_OVERRIDE={} TRANSFORM={} LOG_BODIES={} METRICS={} DASHBOARD={}",
        config.auto_cache,
        config.tail_ttl,
        if config.model_override.is_empty() { "(none)" } else { &config.model_override },
        config.transform_enabled,
        config.log_bodies,
        config.metrics,
        config.dashboard,
    );
    let bind_host = config.bind_host.clone();
    let port = config.port;
    let dashboard = config.dashboard;

    let (router, bind) = build_app(config);

    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind {bind}: {e}");
            std::process::exit(1);
        }
    };

    log(&format!("pino-proxy listening on http://{bind_host}:{port}"));
    log(&summary);
    if dashboard {
        log(&format!("dashboard: http://{bind_host}:{port}/__pino/"));
    }
    log(&format!("export ANTHROPIC_BASE_URL=http://{bind_host}:{port}"));

    if let Err(e) = axum::serve(listener, router).await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
