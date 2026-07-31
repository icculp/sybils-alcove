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

/// One reasoning-effort switch, the same shape as `ModelAt` and read the same
/// way. `effort` is the word BOTH harnesses already write, so it stays the word
/// here rather than acquiring a viewer-only synonym.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EffortAt {
    pub effort: String,
    pub at: String,
}

/// One harness-version change. Same shape and same idiom as `ModelAt`, because
/// it answers the same question one layer down: which BUILD served this turn.
///
/// A long-lived session crosses upgrades — `b3d712dd` spans six Claude CLI
/// versions (2.1.211 → .215 → .216 → .217 → .218 → .220) in a single file — and
/// "which version was running when this broke" is otherwise unanswerable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionAt {
    pub version: String,
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

/// Push only a KNOWN effort, and only when it differs from the one before it.
///
/// An event that records no effort must not enter the timeline. Most of this
/// corpus predates the field, so treating "absent" as a value would manufacture
/// a switch on every transcript that spans the version where it appeared —
/// inference rendering as fact, which is the failure this codebase keeps paying
/// for. Absent stays absent, and the timeline stays a record of switches that
/// actually happened.
pub fn push_effort(timeline: &mut Vec<EffortAt>, effort: &str, at: &str) {
    if effort.is_empty() {
        return;
    }
    if timeline.last().map(|e| e.effort.as_str()) != Some(effort) {
        timeline.push(EffortAt { effort: effort.to_string(), at: at.to_string() });
    }
}

/// Push only a KNOWN version, on the same terms as `push_effort`: an event that
/// records none is not a downgrade to "".
pub fn push_version(timeline: &mut Vec<VersionAt>, version: &str, at: &str) {
    if version.is_empty() {
        return;
    }
    if timeline.last().map(|v| v.version.as_str()) != Some(version) {
        timeline.push(VersionAt { version: version.to_string(), at: at.to_string() });
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

/// Claude stamps the reasoning effort on the assistant EVENT, not inside the
/// message.
///
/// Two shapes are named because two were believed. Measured over 260 transcripts
/// and 19,783 events carrying the field, it is a bare string every time
/// (`high` 10,571 / `xhigh` 8,565 / `max` 647) and the nested `{"level": …}`
/// form appears zero times — which is why reading only `effort.level` made the
/// session badge blank for every Claude session since the field landed. Keeping
/// both named costs nothing and makes a third shape a miss at a known point
/// rather than a silent "".
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Effort {
    Level(String),
    Nested { level: String },
}

impl Effort {
    pub fn level(&self) -> &str {
        match self {
            Effort::Level(s) => s,
            Effort::Nested { level } => level,
        }
    }
}

/// The effort this event was served at, or "" when the event does not say.
pub fn event_effort(event: &Value) -> String {
    let Some(raw) = event.get("effort") else {
        return String::new();
    };
    serde_json::from_value::<Effort>(raw.clone())
        .map(|e| e.level().to_string())
        .unwrap_or_default()
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
    /// The effort this turn was served at, "" when the transcript does not say.
    /// Empty is not a default value for "low" — it means the harness wrote no
    /// effort on this turn, and the UI must render it as nothing at all.
    pub effort: String,
    /// How much the model thought, in the only unit each harness records.
    ///
    /// They are NOT the same measurement and are therefore not one column:
    /// Claude persists thinking blocks with the text stripped, so the honest
    /// signal is how many of them there were; Codex persists a reasoning token
    /// count and no blocks. `None` means this harness does not record that unit
    /// (or nothing was seen), which is different from zero.
    pub thinking_blocks: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    /// The harness build that served this turn, "" when unknown.
    ///
    /// Always "" for Codex, deliberately. Codex writes a version only in
    /// `session_meta` (`cli_version`), which is the version of the process that
    /// wrote the FILE — and a resumed rollout replays turns served months and
    /// several releases earlier. Stamping those with the resuming build would
    /// invent an attribution, and because `turn` is INSERT OR IGNORE the first
    /// wrong answer would be the permanent one. `turn_context` carries
    /// `multi_agent_version: "v1"` and nothing else version-shaped; that is a
    /// feature-schema marker, not a CLI version. So Codex's version is a
    /// per-session fact and stays there.
    pub version: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn effort_reads_the_shape_that_is_actually_written() {
        assert_eq!(event_effort(&json!({"effort": "xhigh"})), "xhigh");
        // The shape the first reader assumed. Never observed, still accepted.
        assert_eq!(event_effort(&json!({"effort": {"level": "max"}})), "max");
        // Absent and unrecognised both mean "the event does not say".
        assert_eq!(event_effort(&json!({"type": "assistant"})), "");
        assert_eq!(event_effort(&json!({"effort": 3})), "");
    }

    #[test]
    fn absent_version_is_not_a_switch() {
        let mut tl = Vec::new();
        push_version(&mut tl, "2.1.218", "t1");
        push_version(&mut tl, "", "t2"); // an event that records nothing
        push_version(&mut tl, "2.1.218", "t3");
        push_version(&mut tl, "2.1.220", "t4");
        assert_eq!(
            tl,
            vec![
                VersionAt { version: "2.1.218".into(), at: "t1".into() },
                VersionAt { version: "2.1.220".into(), at: "t4".into() },
            ]
        );
    }

    #[test]
    fn absent_effort_is_not_a_switch() {
        let mut tl = Vec::new();
        push_effort(&mut tl, "high", "t1");
        push_effort(&mut tl, "", "t2"); // an event that records nothing
        push_effort(&mut tl, "high", "t3"); // still the same run
        push_effort(&mut tl, "max", "t4");
        assert_eq!(
            tl,
            vec![
                EffortAt { effort: "high".into(), at: "t1".into() },
                EffortAt { effort: "max".into(), at: "t4".into() },
            ]
        );
    }
}
