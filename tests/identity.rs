// Ported from test/identity.test.js. All scenarios live in one test to avoid
// races on the process-global project memo cache.

use std::path::PathBuf;

use pino::identity::{clear_project_cache, identify, project_from_body};
use serde_json::{json, Value};

fn fixture_body() -> Option<Value> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/fixtures/req-body.json");
    if p.exists() {
        serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
    } else {
        eprintln!("skip — no fixture (run `node test/capture-fixtures.js`)");
        None
    }
}

#[test]
fn identity_extraction() {
    let Some(req_body) = fixture_body() else { return };

    // mines the cwd out of a real system prompt
    clear_project_cache();
    let project = project_from_body(&req_body, "sess-1");
    assert!(project.starts_with('/'), "an absolute path: {project}");
    assert!(!project.contains('"'));

    // a session's project memo is not poisoned by an early request lacking cwd
    clear_project_cache();
    assert_eq!(project_from_body(&json!({}), "sess-2"), "unknown");
    let resolved = project_from_body(&req_body, "sess-2");
    assert!(resolved.starts_with('/'));
    assert_eq!(project_from_body(&json!({}), "sess-2"), resolved); // now sticks

    // identify pulls session and agent ids from headers
    clear_project_cache();
    let id = identify(
        &json!({
            "x-claude-code-session-id": "abc",
            "x-claude-code-agent-id": "ag1",
            "x-claude-code-parent-agent-id": "ag0",
        }),
        &req_body,
    );
    assert_eq!(id.session_id, "abc");
    assert_eq!(id.agent_id, "ag1");
    assert_eq!(id.parent_agent_id, "ag0");
    assert!(id.project.starts_with('/'));
}
