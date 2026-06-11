// Ported from test/usage.test.js — runs against the REAL captured SSE response
// (test/fixtures/stream.sse.txt), per project policy: no mocks.

use std::path::PathBuf;

use pino::usage::{compute_cost, model_family, parse_usage};

fn fixture() -> Option<String> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/fixtures/stream.sse.txt");
    if p.exists() {
        std::fs::read_to_string(p).ok()
    } else {
        eprintln!("skip — no fixture (run `node test/capture-fixtures.js`)");
        None
    }
}

#[test]
fn parse_usage_extracts_full_breakdown() {
    let Some(sse) = fixture() else { return };
    let u = parse_usage(&sse).expect("should find usage");
    assert_eq!(u.input_tokens, 1);
    assert_eq!(u.cache_create, 3984);
    assert_eq!(u.cache_read, 38748);
    assert_eq!(u.ephem_5m, 3984);
    assert_eq!(u.ephem_1h, 0);
    assert_eq!(u.output_tokens, 351); // final message_delta value, not 0
}

#[test]
fn parse_usage_returns_none_without_usage() {
    assert!(parse_usage(r#"{"type":"count_tokens","input_tokens":10}"#).is_none());
    assert!(parse_usage("").is_none());
}

#[test]
fn model_family_maps_ids() {
    assert_eq!(model_family("claude-opus-4-8"), "opus");
    assert_eq!(model_family("claude-haiku-4-5-20251001"), "haiku");
    assert_eq!(model_family("claude-sonnet-4-6"), "sonnet");
    assert_eq!(model_family("mystery-model"), "opus"); // safe fallback
}

#[test]
fn compute_cost_prices_caching_cheaper() {
    let Some(sse) = fixture() else { return };
    let u = parse_usage(&sse).unwrap();
    let c = compute_cost(&u, "claude-opus-4-8");
    assert_eq!(c.family, "opus");
    assert!(!c.estimate);
    assert!(c.cost_actual > 0.0);
    assert!(c.cost_uncached > c.cost_actual, "uncached must cost more");
    assert!(c.saved > 0.0);
    assert!(c.saved_pct > 50.0 && c.saved_pct <= 100.0);

    let expected_input = (1.0 + 38748.0 * 0.1 + 3984.0 * 1.25) * (5.0 / 1e6);
    let expected_out = 351.0 * (25.0 / 1e6);
    assert!((c.cost_actual - (expected_input + expected_out)).abs() < 1e-9);
}

#[test]
fn fable_is_its_own_family() {
    let Some(sse) = fixture() else { return };
    let c = compute_cost(&parse_usage(&sse).unwrap(), "claude-fable-5");
    assert_eq!(c.family, "fable");
    assert!(!c.estimate);
    let opus = compute_cost(&parse_usage(&sse).unwrap(), "claude-opus-4-8");
    assert!(c.cost_actual > opus.cost_actual); // Fable input is 2x Opus
}

#[test]
fn unknown_model_flagged_as_estimate() {
    let Some(sse) = fixture() else { return };
    let c = compute_cost(&parse_usage(&sse).unwrap(), "some-future-model");
    assert!(c.estimate);
}
