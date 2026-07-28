//! The shared vocabulary both harnesses map into. Port of `alcove/model.py`.
//!
//! This is where the Rust port earns its keep. The Python reads content blocks
//! with `.get()` chains that return "" when a shape surprises them — which is
//! precisely how several real bugs hid. Here the varying shapes are NAMED, so an
//! unnamed third shape is a parse failure at a known point rather than a silent
//! empty string.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
}

impl Usage {
    /// Anthropic usage block. Absent counters are 0, never an error: a message
    /// with no cache fields is normal, not malformed.
    pub fn add_anthropic(&mut self, usage: Option<&Value>) {
        let Some(u) = usage.and_then(Value::as_object) else {
            return;
        };
        let get = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
        self.input += get("input_tokens");
        self.output += get("output_tokens");
        self.cache_read += get("cache_read_input_tokens");
        self.cache_write += get("cache_creation_input_tokens");
    }
}

/// `<synthetic>` marks a harness-injected message, not a served model.
/// Counting it manufactures phantom switch pairs.
pub fn is_real_model(value: Option<&str>) -> bool {
    matches!(value, Some(v) if !v.is_empty() && !v.starts_with('<'))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelAt {
    pub model: String,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Selection {
    pub model: String,
    pub at: String,
    pub requested: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Compaction {
    pub at: String,
    pub trigger: String,
    pub pre_tokens: Option<i64>,
}

pub fn push_model(timeline: &mut Vec<ModelAt>, model: &str, at: &str) {
    if timeline.last().map(|t| t.model.as_str()) != Some(model) {
        timeline.push(ModelAt { model: model.to_string(), at: at.to_string() });
    }
}

pub fn push_selection(out: &mut Vec<Selection>, model: &str, at: &str, asked: &str) {
    if out.last().map(|s| s.model.as_str()) != Some(model) {
        out.push(Selection {
            model: model.to_string(),
            at: at.to_string(),
            requested: asked.to_string(),
        });
    }
}

/// A message's `content` is a string on some events and a list of blocks on
/// others.
///
/// In Python this was an `isinstance` ladder whose fall-through returned "". As
/// an untagged enum both shapes must be named, and a third shape fails loudly.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<Value>),
}

impl Content {
    /// Flatten to text the way `event_text` does.
    ///
    /// Every OBJECT block contributes, using "" when it has no `text` field —
    /// so `[tool_use, text]` flattens to " hello", with the leading space. That
    /// looks like a bug and is load-bearing: the `/model` detection downstream
    /// matches on substrings, and dropping empty blocks shifts the result.
    /// Non-object blocks are excluded entirely.
    pub fn to_text(&self) -> String {
        match self {
            Content::Text(s) => s.clone(),
            Content::Blocks(blocks) => blocks
                .iter()
                .filter(|b| b.is_object())
                .map(|b| b.get("text").and_then(Value::as_str).unwrap_or(""))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

pub fn event_text(event: &Value) -> String {
    let Some(content) = event.get("message").and_then(|m| m.get("content")) else {
        return String::new();
    };
    match serde_json::from_value::<Content>(content.clone()) {
        Ok(c) => c.to_text(),
        Err(_) => String::new(),
    }
}

/// One assistant turn, for the store.
///
/// `id` is a NATURAL key — Claude's `message.id`, Codex's `payload.id`, falling
/// back to `file:timestamp`. That is what makes ingestion idempotent, and
/// therefore what makes re-scanning overlapping windows free.
#[derive(Debug, Clone, Serialize)]
pub struct TurnRow {
    pub id: String,
    pub ts: String,
    pub model: String,
    /// Null for Codex: its token totals are cumulative session snapshots, so
    /// there is no per-turn attribution and inventing one would be a guess.
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub cache_read: Option<i64>,
    pub cache_write: Option<i64>,
}
