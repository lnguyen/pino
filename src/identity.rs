//! Extract the dimensions we slice savings by — session, agent, project — from
//! request headers and body. session/agent come from headers; project is mined
//! once per session from the cwd embedded in the system prompt, then memoized.
//! Mirrors src/identity.js.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use regex::Regex;
use serde_json::Value;

static PROJECT_CACHE: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static RE_CWD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:cwd|[Ww]orking directory)["\s:]*?(/[^"\\,\n]{3,160})"#).unwrap()
});

#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub session_id: String,
    pub agent_id: String,
    pub parent_agent_id: String,
    pub project: String,
}

fn header<'a>(headers: &'a Value, name: &str) -> &'a str {
    headers.get(name).and_then(|v| v.as_str()).unwrap_or("")
}

pub fn session_id(headers: &Value) -> String {
    header(headers, "x-claude-code-session-id").to_string()
}

pub fn agent_ids(headers: &Value) -> (String, String) {
    (
        header(headers, "x-claude-code-agent-id").to_string(),
        header(headers, "x-claude-code-parent-agent-id").to_string(),
    )
}

/// Find the cwd inside the system prompt. Only memoizes a real hit — caching
/// "unknown" would poison the whole session if the first request lacks the line.
pub fn project_from_body(body: &Value, sid: &str) -> String {
    if !sid.is_empty() {
        if let Some(hit) = PROJECT_CACHE.lock().unwrap().get(sid) {
            return hit.clone();
        }
    }

    let mut project = "unknown".to_string();
    if let Some(sys) = body.get("system") {
        let hay = match sys {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if let Some(c) = RE_CWD.captures(&hay) {
            if let Some(m) = c.get(1) {
                project = m.as_str().trim().to_string();
            }
        }
    }

    if !sid.is_empty() && project != "unknown" {
        PROJECT_CACHE
            .lock()
            .unwrap()
            .insert(sid.to_string(), project.clone());
    }
    project
}

/// Everything we tag a request row with.
pub fn identify(headers: &Value, body: &Value) -> Identity {
    let sid = session_id(headers);
    let (agent_id, parent_agent_id) = agent_ids(headers);
    let project = project_from_body(body, &sid);
    Identity {
        session_id: sid,
        agent_id,
        parent_agent_id,
        project,
    }
}

/// Exposed for tests / long-running processes that want to reset memoization.
pub fn clear_project_cache() {
    PROJECT_CACHE.lock().unwrap().clear();
}
