// Mutation parity vs the original JS app. For each representative request body,
// run the AUTO_CACHE pipeline through both the Rust port and src/cache.js, then
// assert the mutated bodies are semantically identical. This is the proof that
// the reference-free TTL reformulation matches Node's object-identity skip set.

use std::io::Write;
use std::process::{Command, Stdio};

use pino::cache::{apply_ttls, inject_breakpoint_if_absent, strip_intermediate_message_breakpoints};
use serde_json::Value;

fn rust_pipeline(mut body: Value, tail_ttl: &str) -> Value {
    strip_intermediate_message_breakpoints(&mut body);
    inject_breakpoint_if_absent(&mut body, tail_ttl);
    apply_ttls(&mut body, tail_ttl);
    body
}

fn js_pipeline(body: &Value, tail_ttl: &str) -> Option<Value> {
    let mut child = Command::new("node")
        .arg("parity/js_mutate.mjs")
        .arg(tail_ttl)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.to_string().as_bytes())
        .ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn cases() -> Vec<(&'static str, Value)> {
    vec![
        (
            "string-system + tools + multi-message",
            serde_json::json!({
                "model": "claude-opus-4-8",
                "system": "You are a helpful assistant with a fairly long system prompt that comfortably exceeds the five-hundred character minimum so it is worth caching. ".repeat(5),
                "tools": [
                    {"name": "Read", "description": "read a file"},
                    {"name": "Bash", "description": "run a command"}
                ],
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "first message with reminders"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
                    {"role": "user", "content": [{"type": "text", "text": "latest turn"}]}
                ]
            }),
        ),
        (
            "array-system (multi-block) + single message",
            serde_json::json!({
                "model": "claude-opus-4-8",
                "system": [
                    {"type": "text", "text": "tiny"},
                    {"type": "text", "text": "a much longer second system block that is definitely beyond the minimum cache size threshold so it should receive a breakpoint on the last entry of the system array. ".repeat(4)}
                ],
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "only one message here"}]}
                ]
            }),
        ),
        (
            "client tail + intermediate breakpoints",
            serde_json::json!({
                "model": "claude-opus-4-8",
                "system": [{"type": "text", "text": "system text long enough to be cached ".repeat(20)}],
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "msg0 reminders"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "mid", "cache_control": {"type": "ephemeral"}}]},
                    {"role": "user", "content": [{"type": "text", "text": "another mid", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "tail block", "cache_control": {"type": "ephemeral"}}]}
                ]
            }),
        ),
        (
            "small system block with client cache_control",
            serde_json::json!({
                "model": "claude-opus-4-8",
                "system": [
                    {"type": "text", "text": "short", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
                    {"type": "text", "text": "a properly long system block well beyond the minimum threshold for caching to be worthwhile here today. ".repeat(5)}
                ],
                "messages": [
                    {"role": "user", "content": "msg one as a string"},
                    {"role": "user", "content": "the final turn, also a string, to test normalization"}
                ]
            }),
        ),
        (
            "string content in last message",
            serde_json::json!({
                "model": "claude-opus-4-8",
                "tools": [{"name": "Read", "description": "read"}],
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "block one"}]},
                    {"role": "assistant", "content": "a plain string tail message"}
                ]
            }),
        ),
    ]
}

#[test]
fn auto_cache_pipeline_matches_js() {
    // Skip cleanly if node or the JS source isn't available.
    if js_pipeline(&serde_json::json!({"messages": []}), "5m").is_none() {
        eprintln!("skip — node parity harness unavailable");
        return;
    }

    for tail_ttl in ["5m", "1h"] {
        for (name, body) in cases() {
            let rust_out = rust_pipeline(body.clone(), tail_ttl);
            let js_out = js_pipeline(&body, tail_ttl)
                .unwrap_or_else(|| panic!("js pipeline failed for case '{name}' @ {tail_ttl}"));
            assert_eq!(
                rust_out, js_out,
                "parity mismatch for case '{name}' @ tail_ttl={tail_ttl}\nRUST: {rust_out}\nJS:   {js_out}"
            );
        }
    }
}
