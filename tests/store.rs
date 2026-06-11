// Ported from test/store.test.js — each test uses its own :memory: store.

use pino::store::{create_store, RowInput, SharedStore};

fn seed(store: &SharedStore) {
    store.record_request(&RowInput {
        req_id: "r1".into(), ts: 1000, session_id: "s1".into(), agent_id: "a1".into(),
        project: "/x/pino".into(), model: "claude-opus-4-8".into(), family: "opus".into(),
        input_tokens: 5, cache_read: 1000, cache_create: 100, ephem_5m: 100, ephem_1h: 0,
        output_tokens: 50, cost_actual: 0.02, cost_uncached: 0.10, saved: 0.08, ..Default::default()
    });
    store.record_request(&RowInput {
        req_id: "r2".into(), ts: 2000, session_id: "s1".into(), agent_id: "a2".into(),
        project: "/x/pino".into(), model: "claude-haiku-4-5".into(), family: "haiku".into(),
        input_tokens: 1, cache_read: 2000, cache_create: 50, ephem_5m: 50, ephem_1h: 0,
        output_tokens: 20, cost_actual: 0.001, cost_uncached: 0.011, saved: 0.01, ..Default::default()
    });
    store.record_request(&RowInput {
        req_id: "r3".into(), ts: 3000, session_id: "s2".into(), agent_id: "a3".into(),
        project: "/x/mulch".into(), model: "claude-opus-4-8".into(), family: "opus".into(),
        input_tokens: 2, cache_read: 500, cache_create: 0, ephem_5m: 0, ephem_1h: 0,
        output_tokens: 10, cost_actual: 0.005, cost_uncached: 0.02, saved: 0.015, ..Default::default()
    });
}

#[test]
fn query_rollup_groups_and_sorts_by_saved_desc() {
    let store = create_store(":memory:");
    seed(&store);
    let by_project = store.query_rollup("project", 0, 100);
    assert_eq!(by_project.len(), 2);
    assert_eq!(by_project[0]["key"].as_str().unwrap(), "/x/pino"); // 0.09 saved
    assert_eq!(by_project[0]["requests"].as_i64().unwrap(), 2);
    assert!((by_project[0]["saved"].as_f64().unwrap() - 0.09).abs() < 1e-9);
    assert!(by_project[0]["pct"].as_f64().unwrap() > 0.0);
}

#[test]
fn model_grouping_uses_family() {
    let store = create_store(":memory:");
    seed(&store);
    let by_model = store.query_rollup("model", 0, 100);
    let mut keys: Vec<String> = by_model
        .iter()
        .map(|r| r["key"].as_str().unwrap().to_string())
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["haiku".to_string(), "opus".to_string()]);
}

#[test]
fn totals_aggregate_whole_window() {
    let store = create_store(":memory:");
    seed(&store);
    let t = store.totals(0);
    assert_eq!(t["requests"].as_i64().unwrap(), 3);
    assert!((t["saved"].as_f64().unwrap() - 0.105).abs() < 1e-9);
}

#[test]
fn record_request_emits_live_event() {
    let store = create_store(":memory:");
    let mut rx = store.subscribe();
    seed(&store);
    // Three rows were published; the last is r3.
    let mut last = None;
    while let Ok(row) = rx.try_recv() {
        last = Some(row);
    }
    let got = last.expect("an event");
    assert_eq!(got["req_id"].as_str().unwrap(), "r3");
    assert!((got["saved"].as_f64().unwrap() - 0.015).abs() < 1e-9);
}

#[test]
fn session_meta_reports_project_and_model_mix() {
    let store = create_store(":memory:");
    seed(&store);
    let meta = store.session_meta(0);
    assert_eq!(meta["s1"]["project"].as_str().unwrap(), "/x/pino");
    let mut models: Vec<String> = meta["s1"]["models"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    models.sort();
    assert_eq!(models, vec!["haiku".to_string(), "opus".to_string()]);
}
