//! Model-name rewrites for MODEL_OVERRIDE. Mirrors src/model.js.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

static SOURCE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"claude-opus-4-7(?:-\d{8})?").unwrap());
static SOURCE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Opus 4\.7").unwrap());
static TRAILING_DATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-\d{8}$").unwrap());

fn friendly_name(base: &str) -> &str {
    match base {
        "claude-opus-4-6" => "Opus 4.6",
        "claude-opus-4-5" => "Opus 4.5",
        "claude-sonnet-4-6" => "Sonnet 4.6",
        "claude-sonnet-4-5" => "Sonnet 4.5",
        "claude-haiku-4-5" => "Haiku 4.5",
        other => other,
    }
}

/// Rewrite Opus-4.7 id/name references in `body.system` to the override model.
/// Returns the number of system blocks (or the single string) that changed.
pub fn rewrite_system_model_refs(body: &mut Value, override_model: &str) -> usize {
    if override_model.is_empty() {
        return 0;
    }
    let base = TRAILING_DATE.replace(override_model, "").into_owned();
    let friendly = friendly_name(&base).to_string();

    let rewrite = |text: &str| -> String {
        let step1 = SOURCE_ID.replace_all(text, override_model);
        SOURCE_NAME.replace_all(&step1, friendly.as_str()).into_owned()
    };

    let mut count = 0;
    match body.get_mut("system") {
        Some(Value::String(s)) => {
            let next = rewrite(s);
            if &next != s {
                count += 1;
            }
            *s = next;
        }
        Some(Value::Array(blocks)) => {
            for blk in blocks.iter_mut() {
                if let Some(Value::String(t)) = blk.get_mut("text") {
                    let next = rewrite(t);
                    if &next != t {
                        count += 1;
                    }
                    *t = next;
                }
            }
        }
        _ => {}
    }
    count
}
