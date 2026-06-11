//! Prompt-cache breakpoint injection and TTL rewriting. Mirrors src/cache.js.
//!
//! Porting note — the reference-identity `skip` set: the Node version protects
//! the rolling-tail breakpoint from the blind 1h bump using a `Set` of object
//! *references*. serde_json::Value has no reference identity, so we use the
//! equivalent observation: the skip set is exactly every ephemeral breakpoint in
//! the *last message*. So `apply_ttls` bumps everything outside the last message
//! to 1h and forces every breakpoint inside the last message to TAIL_TTL — the
//! identical end state.

use serde_json::{json, Value};

use crate::config::{BREAKPOINT_CEILING, MIN_SYSTEM_CACHE_CHARS};

fn is_ephemeral(node: &Value) -> bool {
    node.get("cache_control")
        .and_then(|c| c.get("type"))
        .and_then(|t| t.as_str())
        == Some("ephemeral")
}

fn set_cc(node: &mut Value, ttl: &str) {
    if let Some(obj) = node.as_object_mut() {
        obj.insert(
            "cache_control".to_string(),
            json!({ "type": "ephemeral", "ttl": ttl }),
        );
    }
}

/// Recursively count ephemeral breakpoints anywhere in the body.
pub fn count_cache_breakpoints(node: &Value) -> usize {
    match node {
        Value::Array(a) => a.iter().map(count_cache_breakpoints).sum(),
        Value::Object(o) => {
            let here = if is_ephemeral(node) { 1 } else { 0 };
            here + o.values().map(count_cache_breakpoints).sum::<usize>()
        }
        _ => 0,
    }
}

/// Bump every ephemeral breakpoint in a subtree to 1h. Skips recursing into the
/// `cache_control` key itself (it holds no nested breakpoints).
fn bump_to_1h(node: &mut Value, rewritten: &mut usize, already: &mut usize) {
    match node {
        Value::Array(a) => {
            for it in a.iter_mut() {
                bump_to_1h(it, rewritten, already);
            }
        }
        Value::Object(o) => {
            if o.get("cache_control").map(|c| c.get("type").and_then(|t| t.as_str()) == Some("ephemeral")).unwrap_or(false) {
                if let Some(cc) = o.get_mut("cache_control").and_then(|c| c.as_object_mut()) {
                    let was_1h = cc.get("ttl").and_then(|t| t.as_str()) == Some("1h");
                    cc.insert("ttl".to_string(), Value::String("1h".to_string()));
                    if was_1h {
                        *already += 1;
                    } else {
                        *rewritten += 1;
                    }
                }
            }
            for (k, v) in o.iter_mut() {
                if k != "cache_control" {
                    bump_to_1h(v, rewritten, already);
                }
            }
        }
        _ => {}
    }
}

/// Force every ephemeral breakpoint in a subtree to `ttl` (the tail tier).
fn force_ttl(node: &mut Value, ttl: &str, count: &mut usize) {
    match node {
        Value::Array(a) => {
            for it in a.iter_mut() {
                force_ttl(it, ttl, count);
            }
        }
        Value::Object(o) => {
            if o.get("cache_control").map(|c| c.get("type").and_then(|t| t.as_str()) == Some("ephemeral")).unwrap_or(false) {
                if let Some(cc) = o.get_mut("cache_control").and_then(|c| c.as_object_mut()) {
                    cc.insert("ttl".to_string(), Value::String(ttl.to_string()));
                    *count += 1;
                }
            }
            for (k, v) in o.iter_mut() {
                if k != "cache_control" {
                    force_ttl(v, ttl, count);
                }
            }
        }
        _ => {}
    }
}

/// Counters surfaced in the per-request log note.
#[derive(Default)]
pub struct TtlCounter {
    pub rewritten: usize,
    pub already: usize,
    pub skipped: usize,
}

/// Bump every breakpoint to 1h, except those in the last message which are
/// forced to `tail_ttl`. Equivalent to Node's rewriteCacheControl + the skip set.
pub fn apply_ttls(body: &mut Value, tail_ttl: &str) -> TtlCounter {
    let mut c = TtlCounter::default();

    if let Some(tools) = body.get_mut("tools") {
        bump_to_1h(tools, &mut c.rewritten, &mut c.already);
    }
    if let Some(system) = body.get_mut("system") {
        bump_to_1h(system, &mut c.rewritten, &mut c.already);
    }
    if let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        let n = msgs.len();
        if n > 0 {
            for m in msgs.iter_mut().take(n - 1) {
                bump_to_1h(m, &mut c.rewritten, &mut c.already);
            }
            force_ttl(&mut msgs[n - 1], tail_ttl, &mut c.skipped);
        }
    }
    c
}

fn has_breakpoint(arr: &Value) -> bool {
    arr.as_array()
        .map(|a| a.iter().any(is_ephemeral))
        .unwrap_or(false)
}

fn strip_small_system_breakpoints(body: &mut Value) -> usize {
    let Some(system) = body.get_mut("system").and_then(|s| s.as_array_mut()) else {
        return 0;
    };
    let mut stripped = 0;
    for block in system.iter_mut() {
        if !is_ephemeral(block) {
            continue;
        }
        let len = block.get("text").and_then(|t| t.as_str()).map(|s| s.chars().count()).unwrap_or(0);
        if len < MIN_SYSTEM_CACHE_CHARS {
            if let Some(o) = block.as_object_mut() {
                o.remove("cache_control");
                stripped += 1;
            }
        }
    }
    stripped
}

/// Normalize a message's content (string -> array) and return the index of its
/// last cacheable block (text/tool_result/image), or None.
fn last_cacheable_block_idx(message: &mut Value) -> Option<usize> {
    let content = message.get("content")?;
    if content.is_array() {
        let arr = content.as_array().unwrap();
        for j in (0..arr.len()).rev() {
            let b = &arr[j];
            let t = b.get("type").and_then(|t| t.as_str());
            if matches!(t, Some("text") | Some("tool_result") | Some("image")) {
                return Some(j);
            }
        }
        None
    } else if let Some(s) = content.as_str() {
        if s.is_empty() {
            return None;
        }
        let text = s.to_string();
        message["content"] = json!([{ "type": "text", "text": text }]);
        Some(0)
    } else {
        None
    }
}

/// Place up to four breakpoints within the 4-breakpoint ceiling. Returns the
/// human-readable tag for the log note. TTL normalization is done by apply_ttls.
pub fn inject_breakpoint_if_absent(body: &mut Value, tail_ttl: &str) -> String {
    let mut tags: Vec<String> = Vec::new();

    let stripped = strip_small_system_breakpoints(body);
    if stripped > 0 {
        tags.push(format!("strip-sys:{stripped}"));
    }

    // Tools — last entry.
    if let Some(tools) = body.get("tools") {
        if tools.as_array().map(|a| !a.is_empty()).unwrap_or(false) && !has_breakpoint(tools) {
            let arr = body["tools"].as_array_mut().unwrap();
            let last = arr.len() - 1;
            if arr[last].is_object() {
                set_cc(&mut arr[last], "1h");
                tags.push("tools".to_string());
            }
        }
    }

    // System — last entry, or normalize a string into one cached block.
    match body.get("system") {
        Some(Value::Array(a)) if !a.is_empty() && !has_breakpoint(&body["system"]) => {
            let arr = body["system"].as_array_mut().unwrap();
            let last = arr.len() - 1;
            if arr[last].is_object() {
                set_cc(&mut arr[last], "1h");
                tags.push("system".to_string());
            }
        }
        Some(Value::String(s)) if !s.is_empty() => {
            let text = s.clone();
            body["system"] = json!([{ "type": "text", "text": text, "cache_control": { "type": "ephemeral", "ttl": "1h" } }]);
            tags.push("system-string".to_string());
        }
        _ => {}
    }

    // messages[0] static reminders — only when a distinct tail message follows.
    let msg_len = body.get("messages").and_then(|m| m.as_array()).map(|a| a.len()).unwrap_or(0);
    if msg_len > 1 && count_cache_breakpoints(body) < BREAKPOINT_CEILING {
        if let Some(first) = body.get_mut("messages").and_then(|m| m.as_array_mut()).and_then(|a| a.get_mut(0)) {
            if let Some(idx) = last_cacheable_block_idx(first) {
                let block = &mut first["content"][idx];
                if block.get("cache_control").is_none() {
                    set_cc(block, "1h");
                    tags.push("msg0".to_string());
                }
            }
        }
    }

    // Rolling tail — last cacheable block across messages, at tail_ttl.
    if count_cache_breakpoints(body) < BREAKPOINT_CEILING {
        if let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for i in (0..msgs.len()).rev() {
                if let Some(idx) = last_cacheable_block_idx(&mut msgs[i]) {
                    let block = &mut msgs[i]["content"][idx];
                    if block.get("cache_control").is_none() {
                        set_cc(block, tail_ttl);
                        tags.push(format!("tail:{tail_ttl}"));
                    }
                    break;
                }
            }
        }
    }

    if tags.is_empty() {
        "none".to_string()
    } else {
        tags.join("+")
    }
}

/// Remove client-sent cache_control from intermediate messages (everything
/// except the first and last). Returns the number stripped.
pub fn strip_intermediate_message_breakpoints(body: &mut Value) -> usize {
    let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return 0;
    };
    if msgs.len() <= 2 {
        return 0;
    }
    let mut stripped = 0;
    let n = msgs.len();
    for i in 1..n - 1 {
        if let Some(content) = msgs[i].get_mut("content").and_then(|c| c.as_array_mut()) {
            for block in content.iter_mut() {
                if let Some(o) = block.as_object_mut() {
                    if o.remove("cache_control").is_some() {
                        stripped += 1;
                    }
                }
            }
        }
    }
    stripped
}
