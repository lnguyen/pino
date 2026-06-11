//! Native body transform — port of src/transforms/default.js. Env-driven:
//!   DROP_TOOLS=Name1,Name2  remove tools + scrub them from deferred-tool reminders
//!   STRIP_ANSI=0            disable ANSI stripping (default on)
//!   TRIM_BASH_GIT=1         truncate the Bash tool description at the git section
//! plus restructureV123 (extract static core context to msg0, drop stale scaffolding).

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Value};

static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap());
static REMINDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<system-reminder>(.*?)</system-reminder>").unwrap());
static DEFERRED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)deferred tools|ToolSearch").unwrap());

pub struct Transform {
    drop_tools: HashSet<String>,
    strip_ansi: bool,
    trim_bash_git: bool,
}

impl Transform {
    pub fn from_env() -> Self {
        let drop_tools: HashSet<String> = std::env::var("DROP_TOOLS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let strip_ansi = std::env::var("STRIP_ANSI").ok().as_deref() != Some("0");
        let trim_bash_git = std::env::var("TRIM_BASH_GIT").ok().as_deref() == Some("1");

        if drop_tools.is_empty() {
            println!("[transform] DROP_TOOLS=(none)");
        } else {
            let mut names: Vec<&String> = drop_tools.iter().collect();
            names.sort();
            println!(
                "[transform] DROP_TOOLS={}",
                names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",")
            );
        }
        println!("[transform] STRIP_ANSI={strip_ansi} TRIM_BASH_GIT={trim_bash_git}");

        Transform {
            drop_tools,
            strip_ansi,
            trim_bash_git,
        }
    }

    pub fn apply(&self, body: &mut Value) {
        self.trim_tools(body);
        self.trim_reminders(body);
        // trim_system is a no-op placeholder, like the JS.
        self.restructure_v123(body);
        self.strip_ansi_from_messages(body);
    }

    fn trim_tools(&self, body: &mut Value) {
        let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) else {
            return;
        };
        if !self.drop_tools.is_empty() {
            tools.retain(|t| {
                let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
                !self.drop_tools.contains(name)
            });
        }
        if self.trim_bash_git {
            for t in tools.iter_mut() {
                if t.get("name").and_then(|n| n.as_str()) == Some("Bash") {
                    if let Some(desc) = t.get("description").and_then(|d| d.as_str()) {
                        if let Some(idx) = desc.find("# Committing changes with git") {
                            let trimmed = desc[..idx].trim_end().to_string();
                            t["description"] = Value::String(trimmed);
                        }
                    }
                }
            }
        }
    }

    fn strip_dropped_tools_from_reminder(&self, text: &str) -> String {
        if self.drop_tools.is_empty() {
            return text.to_string();
        }
        REMINDER_RE
            .replace_all(text, |caps: &regex::Captures| {
                let inner = &caps[1];
                if !DEFERRED_RE.is_match(inner) {
                    return caps[0].to_string();
                }
                let cleaned = inner
                    .split('\n')
                    .filter(|line| !self.drop_tools.contains(line.trim()))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("<system-reminder>{cleaned}</system-reminder>")
            })
            .into_owned()
    }

    fn trim_reminders(&self, body: &mut Value) {
        if self.drop_tools.is_empty() {
            return;
        }
        let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
            return;
        };
        for msg in messages.iter_mut() {
            match msg.get_mut("content") {
                Some(Value::String(s)) => {
                    *s = self.strip_dropped_tools_from_reminder(s);
                }
                Some(Value::Array(blocks)) => {
                    for block in blocks.iter_mut() {
                        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                            let next = self.strip_dropped_tools_from_reminder(t);
                            block["text"] = Value::String(next);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn strip_ansi_from_messages(&self, body: &mut Value) {
        if !self.strip_ansi {
            return;
        }
        let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
            return;
        };
        let strip = |s: &str| ANSI_RE.replace_all(s, "").into_owned();
        for msg in messages.iter_mut() {
            match msg.get_mut("content") {
                Some(Value::String(s)) => *s = strip(s),
                Some(Value::Array(blocks)) => {
                    for b in blocks.iter_mut() {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            let next = strip(t);
                            b["text"] = Value::String(next);
                        }
                        match b.get_mut("content") {
                            Some(Value::String(c)) => *c = strip(c),
                            Some(Value::Array(inner)) => {
                                for rc in inner.iter_mut() {
                                    if let Some(t) = rc.get("text").and_then(|t| t.as_str()) {
                                        let next = strip(t);
                                        rc["text"] = Value::String(next);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn restructure_v123(&self, body: &mut Value) {
        let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
            return;
        };
        if messages.len() < 2 {
            return;
        }

        // Normalize all message contents to arrays.
        for msg in messages.iter_mut() {
            if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
                let t = s.to_string();
                msg["content"] = json!([{ "type": "text", "text": t }]);
            }
        }

        let n = messages.len();
        let mut core: Vec<Value> = Vec::new();

        // Extract core context and drop stale scaffolding from every message.
        for i in 0..n {
            let is_tail = i == n - 1;
            let blocks = match messages[i]["content"].take() {
                Value::Array(a) => a,
                other => {
                    messages[i]["content"] = other;
                    continue;
                }
            };
            let mut new_content = Vec::new();
            for block in blocks {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    let text = text.to_string();
                    if is_core_context(&text) {
                        core.push(block);
                        continue;
                    }
                    if !is_tail && is_stale_removable(&text) {
                        continue;
                    }
                }
                new_content.push(block);
            }
            messages[i]["content"] = Value::Array(new_content);
        }

        // Assemble msg0 with unique core context blocks prepended.
        if !core.is_empty() {
            let mut unique = Vec::new();
            let mut seen = HashSet::new();
            for b in core {
                let t = b.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                if seen.insert(t) {
                    unique.push(b);
                }
            }
            let existing = match messages[0]["content"].take() {
                Value::Array(a) => a,
                _ => Vec::new(),
            };
            let mut combined = unique;
            combined.extend(existing);
            let count = combined.len();
            messages[0]["content"] = Value::Array(combined);
            messages[0]["role"] = Value::String("user".to_string());
            let _ = count;
        }

        // Drop any now-empty messages.
        messages.retain(|m| {
            m.get("content")
                .and_then(|c| c.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        });
    }
}

fn is_core_context(t: &str) -> bool {
    if t.contains("<local-command-stdout>") || t.contains("<local-command-caveat>") {
        return false;
    }
    t.contains("ToolSearch")
        || t.contains("claudeMd")
        || t.contains(".claude/projects")
        || t.contains(".claude/plans")
}

fn is_stale_removable(t: &str) -> bool {
    t.starts_with("<system-reminder>")
        || t.starts_with("<local-command-stdout>")
        || t.starts_with("<local-command-caveat>")
        || t.starts_with("<command-name>")
}
