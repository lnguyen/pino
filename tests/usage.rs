// Ported from test/usage.test.js — runs against the REAL captured SSE response
// (test/fixtures/stream.sse.txt), per project policy: no mocks.

use std::path::PathBuf;

use pino::usage::{compute_cost, compute_marginal, model_family, parse_usage, Usage, FIVE_MIN_MS};

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

// ---- marginal: 1h cache vs Claude Code's free 5m default ----

fn usage_rw(cache_read: i64, ephem_1h: i64) -> Usage {
    Usage { cache_read, ephem_1h, ..Default::default() }
}

#[test]
fn marginal_within_5min_is_pure_write_premium_loss() {
    // Gap under 5min: the default 5m cache would have hit too, so the 1h write
    // premium bought nothing — net is the premium, paid for nothing.
    let u = usage_rw(10_000, 2_000);
    let m = compute_marginal(&u, "claude-opus-4-8", Some(FIVE_MIN_MS - 1));
    assert!(!m.extended_window);
    // opus in-price 5/M; premium = 2000 * (2.0-1.25) * 5e-6
    let premium = 2_000.0 * 0.75 * (5.0 / 1e6);
    assert!((m.write_premium - premium).abs() < 1e-12);
    assert!((m.saved - (-premium)).abs() < 1e-12, "should be a net loss");
}

#[test]
fn marginal_past_5min_credits_the_read() {
    // Gap >= 5min: the default 5m cache would have expired and missed, so the
    // read at 0.1x replaced a 1.25x re-write — that delta is the real saving.
    let u = usage_rw(10_000, 2_000);
    let m = compute_marginal(&u, "claude-opus-4-8", Some(FIVE_MIN_MS));
    assert!(m.extended_window);
    let read_benefit = 10_000.0 * (1.25 - 0.1) * (5.0 / 1e6);
    let premium = 2_000.0 * 0.75 * (5.0 / 1e6);
    assert!((m.saved - (read_benefit - premium)).abs() < 1e-12);
    assert!(m.saved > 0.0);
}

#[test]
fn marginal_unknown_gap_is_conservative() {
    // First request in a session / backfill with no predecessor: don't claim a
    // read benefit we can't justify.
    let m = compute_marginal(&usage_rw(10_000, 2_000), "claude-opus-4-8", None);
    assert!(!m.extended_window);
    assert!(m.saved <= 0.0);
}

#[test]
fn marginal_zero_when_nothing_cached() {
    let m = compute_marginal(&Usage::default(), "claude-opus-4-8", Some(FIVE_MIN_MS * 10));
    assert_eq!(m.saved, 0.0);
    assert_eq!(m.write_premium, 0.0);
}
