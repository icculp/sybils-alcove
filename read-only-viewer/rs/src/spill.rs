//! Spillout: the recent event stream of one session. Port of `alcove/spill.py`.
//!
//! What it deliberately does NOT show is reasoning. Both harnesses persist a
//! reasoning record with the text stripped: Claude writes a `thinking` block
//! carrying only a `signature`, Codex a `reasoning` item carrying only
//! `encrypted_content`. Measured across this corpus, 22,669 Claude thinking
//! blocks and 22,428 Codex reasoning items contained text in exactly zero cases.
//! So the stream emits a `reasoning` marker with no body — the model thought
//! here, and the content is not on disk. Rendering nothing would imply it never
//! thought; inventing a summary would be a lie.

use std::path::Path;

use serde_json::{json, Map, Value};

use crate::model::event_effort;
use crate::transcripts::{chronological, tail_events};

/// Per-event text cap. Tool results run to megabytes; the browser wants a peek,
/// not the payload. Truncation is always flagged so a cut never reads as the end.
pub const MAX_TEXT: usize = 4000;
pub const MAX_ARG: usize = 600;

/// Truncate on CHARACTER boundaries, matching Python's slicing. Cutting a UTF-8
/// string by bytes would panic mid-codepoint on any non-ASCII transcript.
fn clip(text: &str, limit: usize) -> (String, bool) {
    let text = text.replace("\r\n", "\n");
    if text.chars().count() <= limit {
        return (text, false);
    }
    (text.chars().take(limit).collect(), true)
}

/// Truncate long strings inside tool arguments but keep the structure.
///
/// A Write call's `content` is the whole file; flattening the dict to a clipped
/// JSON string would hide which parameters were even passed. Keys survive,
/// values get cut.
fn shrink(value: &Value, depth: usize) -> Value {
    match value {
        Value::String(s) => {
            if s.chars().count() > MAX_ARG {
                let mut out: String = s.chars().take(MAX_ARG).collect();
                out.push('…');
                Value::String(out)
            } else {
                Value::String(s.clone())
            }
        }
        Value::Object(map) if depth < 4 => Value::Object(
            map.iter().take(40).map(|(k, v)| (k.clone(), shrink(v, depth + 1))).collect(),
        ),
        Value::Array(items) if depth < 4 => {
            Value::Array(items.iter().take(20).map(|v| shrink(v, depth + 1)).collect())
        }
        other => other.clone(),
    }
}

/// Spawn parameters worth pulling to the front of a row, and the ONLY keys
/// lifted out of a spawn's arguments. Same whitelist the hook spools into
/// `params` (see `hooks/README.md`); kept in step with it by hand, because the
/// two read different inputs — the hook reads the live payload, this reads the
/// transcript the harness wrote afterwards.
const SPAWN_PARAM_KEYS: [&str; 8] = [
    "model",
    "subagent_type",
    "agent_type",
    "effort",
    "reasoning_effort",
    "run_in_background",
    "isolation",
    "fork_context",
];

/// Is this tool call a spawn? `Agent`/`Task` on Claude, `spawn_agent` on Codex
/// — where the tool namespace is sometimes prefixed, hence the substring test.
fn is_spawn(name: &str) -> bool {
    name == "Agent" || name == "Task" || name.to_lowercase().contains("spawn_agent")
}

/// The whitelisted parameters of a spawn, or `None` for any other call.
///
/// Which model a subagent was given is the most governance-relevant fact about
/// a launch, and in a spawn's raw arguments it sits next to a prompt that is
/// three orders of magnitude longer — so it is lifted out and rendered up
/// front. `None` when the call is not a spawn or named none of these; the page
/// then renders nothing, never a guessed default. The arguments themselves are
/// still shown in full underneath, so this is a summary and not a replacement.
fn spawn_params(name: &str, input: Option<&Value>) -> Option<Value> {
    if !is_spawn(name) {
        return None;
    }
    let map = input?.as_object()?;
    let mut out = Map::new();
    for key in SPAWN_PARAM_KEYS {
        match map.get(key) {
            // A body can never arrive here — the keys are a whitelist — but an
            // object or array value would still be noise in a one-line summary.
            Some(v) if v.is_string() || v.is_boolean() || v.is_number() => {
                out.insert(key.to_string(), shrink(v, 3));
            }
            _ => {}
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(Value::Object(out))
    }
}

/// Seconds since the epoch from an ISO-8601 timestamp, or None.
///
/// Hand-rolled rather than pulling in a date crate: the only shape written by
/// either harness is `YYYY-MM-DDTHH:MM:SS(.fff)?Z`.
fn ts_epoch(ts: &str) -> Option<f64> {
    let b = ts.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |a: usize, z: usize| ts.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // Days from civil (Howard Hinnant), the inverse of the formatter in collect.
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + h * 3600 + mi * 60 + s) as f64)
}

/// Flatten a content list to text, dropping images.
///
/// A transcript image block is an inline base64 PNG — hundreds of kilobytes that
/// would dwarf every other event in the payload.
fn blocks_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => {
            let mut parts: Vec<String> = Vec::new();
            for block in items {
                let Some(obj) = block.as_object() else {
                    parts.push(block.to_string());
                    continue;
                };
                match obj.get("type").and_then(Value::as_str) {
                    Some("image") => parts.push("[image]".into()),
                    Some("tool_reference") => parts.push(format!(
                        "[tool: {}]",
                        obj.get("tool_name").and_then(Value::as_str).unwrap_or("")
                    )),
                    _ => {
                        if let Some(t) = obj.get("text").and_then(Value::as_str) {
                            parts.push(t.to_string());
                        }
                    }
                }
            }
            parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n")
        }
        _ => String::new(),
    }
}

fn event(kind: &str, ts: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), json!(kind));
    m.insert("ts".into(), json!(ts));
    m
}

fn spill_claude(path: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    for ev in chronological(tail_events(path), "timestamp") {
        let etype = ev.get("type").and_then(Value::as_str).unwrap_or("");
        let ts = ev.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
        if etype == "system" && ev.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        {
            out.push(Value::Object(event("compact", &ts)));
            continue;
        }
        let Some(message) = ev.get("message").filter(|m| m.is_object()) else {
            continue;
        };
        if etype != "user" && etype != "assistant" {
            continue;
        }
        let model = message.get("model").and_then(Value::as_str).unwrap_or("").to_string();
        // Stamped on the EVENT, so the stream can say which effort served each
        // line rather than only what the session is set to now. "" where the
        // transcript does not say, and the page renders that as nothing.
        let effort = event_effort(&ev);
        // The build that wrote the line. Codex has no per-event equivalent, so
        // its rows carry none — absent, not blank-as-a-value.
        let version = ev.get("version").and_then(Value::as_str).unwrap_or("").to_string();
        let owned;
        let blocks: &Vec<Value> = match message.get("content") {
            Some(Value::Array(a)) => a,
            other => {
                owned = vec![json!({"type": "text", "text": other.cloned().unwrap_or(Value::Null)})];
                &owned
            }
        };
        for block in blocks {
            let Some(obj) = block.as_object() else { continue };
            match obj.get("type").and_then(Value::as_str) {
                // Signature only; see the module docstring.
                Some("thinking") => {
                    let mut e = event("reasoning", &ts);
                    e.insert("model".into(), json!(model));
                    e.insert("effort".into(), json!(effort));
                    e.insert("version".into(), json!(version));
                    e.insert("version".into(), json!(version));
                    out.push(Value::Object(e));
                }
                Some("text") => {
                    let raw = obj.get("text").and_then(Value::as_str).unwrap_or("");
                    let (text, cut) = clip(raw, MAX_TEXT);
                    if !text.trim().is_empty() {
                        let mut e =
                            event(if etype == "assistant" { "assistant" } else { "user" }, &ts);
                        e.insert("text".into(), json!(text));
                        e.insert("truncated".into(), json!(cut));
                        e.insert("model".into(), json!(model));
                        e.insert("effort".into(), json!(effort));
                        e.insert("version".into(), json!(version));
                        out.push(Value::Object(e));
                    }
                }
                Some("tool_use") => {
                    let mut e = event("tool_use", &ts);
                    let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
                    e.insert("name".into(), json!(name));
                    e.insert("tool_id".into(), json!(obj.get("id").and_then(Value::as_str).unwrap_or("")));
                    if let Some(params) = spawn_params(name, obj.get("input")) {
                        e.insert("params".into(), params);
                    }
                    e.insert("args".into(), shrink(obj.get("input").unwrap_or(&Value::Null), 0));
                    e.insert("model".into(), json!(model));
                    e.insert("effort".into(), json!(effort));
                    e.insert("version".into(), json!(version));
                    out.push(Value::Object(e));
                }
                Some("tool_result") => {
                    let (text, cut) = clip(&blocks_text(obj.get("content")), MAX_TEXT);
                    let mut e = event("tool_result", &ts);
                    e.insert("text".into(), json!(text));
                    e.insert("truncated".into(), json!(cut));
                    e.insert("tool_id".into(), json!(obj.get("tool_use_id").and_then(Value::as_str).unwrap_or("")));
                    e.insert("error".into(), json!(obj.get("is_error").and_then(Value::as_bool).unwrap_or(false)));
                    out.push(Value::Object(e));
                }
                _ => {}
            }
        }
    }
    out
}

fn spill_codex(path: &Path) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    // Codex records the effort once per turn, on `turn_context`, and not on the
    // events it governs — so it is carried forward in stream ORDER. The
    // turn_context timestamp is not usable (a resume restamps every replayed
    // line with the file-open time; see codex.rs) but its position is, and
    // position is all this needs.
    let mut effort = String::new();
    for ev in chronological(tail_events(path), "timestamp") {
        let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
        let ts = ev.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
        let Some(payload) = ev.get("payload").filter(|p| p.is_object()) else {
            continue;
        };
        let ptype = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "turn_context" {
            if let Some(e) = payload.get("effort").and_then(Value::as_str) {
                if !e.is_empty() {
                    effort = e.to_string();
                }
            }
            continue;
        }

        if kind == "compacted" || ptype == "context_compacted" {
            // One compaction is written twice, milliseconds apart. Compare at
            // second granularity: two real compactions in one second is not a thing.
            let dup = out.last().map(|e| {
                e.get("kind").and_then(Value::as_str) == Some("compact")
                    && e.get("ts").and_then(Value::as_str).unwrap_or("").chars().take(19)
                        .eq(ts.chars().take(19))
            }).unwrap_or(false);
            if !dup {
                out.push(Value::Object(event("compact", &ts)));
            }
            continue;
        }
        if kind != "response_item" {
            continue;
        }
        match ptype {
            "reasoning" => {
                let mut e = event("reasoning", &ts);
                e.insert("effort".into(), json!(effort));
                out.push(Value::Object(e));
            }
            "message" => {
                let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                // `developer` is the injected system preamble, re-sent every
                // turn. It is not commentary and would swamp the stream.
                if role != "assistant" && role != "user" {
                    continue;
                }
                let (text, cut) = clip(&blocks_text(payload.get("content")), MAX_TEXT);
                if !text.trim().is_empty() {
                    let mut e = event(role, &ts);
                    e.insert("text".into(), json!(text));
                    e.insert("truncated".into(), json!(cut));
                    if role == "assistant" {
                        e.insert("effort".into(), json!(effort));
                    }
                    out.push(Value::Object(e));
                }
            }
            "function_call" | "custom_tool_call" | "local_shell_call" => {
                // Codex serialises arguments as a JSON *string*; parse it so the
                // viewer can show fields, falling back to the raw text if it is
                // not the JSON it claims to be.
                let args = match payload.get("arguments") {
                    Some(Value::String(s)) => serde_json::from_str::<Value>(s)
                        .unwrap_or_else(|_| json!({ "arguments": s })),
                    Some(other) => other.clone(),
                    None => Value::Null,
                };
                let mut e = event("tool_use", &ts);
                let name = payload.get("name").and_then(Value::as_str).unwrap_or(ptype);
                e.insert("name".into(), json!(name));
                e.insert("tool_id".into(), json!(payload.get("call_id").and_then(Value::as_str).unwrap_or("")));
                if let Some(params) = spawn_params(name, Some(&args)) {
                    e.insert("params".into(), params);
                }
                e.insert("args".into(), shrink(&args, 0));
                out.push(Value::Object(e));
            }
            "function_call_output" | "custom_tool_call_output" => {
                let body = match payload.get("output") {
                    Some(Value::Object(o)) => o
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| Value::Object(o.clone()).to_string()),
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                let (text, cut) = clip(&body, MAX_TEXT);
                let mut e = event("tool_result", &ts);
                e.insert("text".into(), json!(text));
                e.insert("truncated".into(), json!(cut));
                e.insert("tool_id".into(), json!(payload.get("call_id").and_then(Value::as_str).unwrap_or("")));
                e.insert("error".into(), json!(false));
                out.push(Value::Object(e));
            }
            "tool_search_call" => {
                let mut e = event("tool_use", &ts);
                e.insert("name".into(), json!("tool_search"));
                e.insert("tool_id".into(), json!(""));
                let q = payload.get("queries").or_else(|| payload.get("query"));
                e.insert("args".into(), shrink(q.unwrap_or(&Value::Null), 0));
                out.push(Value::Object(e));
            }
            _ => {}
        }
    }
    out
}

/// Recent events for one session or subagent.
///
/// `target` is resolved by the caller from the collected snapshot — the client
/// sends ids, never paths, so an unknown id is simply absent rather than a
/// filesystem read.
pub fn spill(
    target: Option<(std::path::PathBuf, String, Value)>,
    minutes: i64,
    limit: usize,
) -> Value {
    let Some((path, harness, meta)) = target else {
        return json!({"error": "unknown session", "events": []});
    };
    let mut events =
        if harness == "claude" { spill_claude(&path) } else { spill_codex(&path) };

    let mut window = Value::Null;
    if minutes > 0 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let cutoff = now - (minutes as f64) * 60.0;
        window = json!(minutes);
        // An event with an unparseable timestamp is KEPT: dropping it would
        // silently hide activity, and a missing timestamp is not evidence of age.
        events.retain(|e| {
            ts_epoch(e.get("ts").and_then(Value::as_str).unwrap_or("")).unwrap_or(1e18) >= cutoff
        });
    }
    let matched = events.len();
    if events.len() > limit {
        events = events.split_off(events.len() - limit);
    }
    json!({
        "session_id": meta.get("session_id").cloned().unwrap_or(Value::Null),
        "agent_id": meta.get("agent_id").cloned().unwrap_or(Value::Null),
        "harness": harness,
        "label": meta.get("label").cloned().unwrap_or(Value::Null),
        "model": meta.get("model").cloned().unwrap_or(Value::Null),
        "cwd": meta.get("cwd").cloned().unwrap_or(Value::Null),
        "project": meta.get("project").cloned().unwrap_or(Value::Null),
        "state": meta.get("state").cloned().unwrap_or(Value::Null),
        "role": meta.get("role").cloned().unwrap_or(Value::Null),
        "task": meta.get("task").cloned().unwrap_or(Value::Null),
        "events": events,
        "shown": matched.min(limit),
        "matched": matched,
        "window_minutes": window,
        // The tail window bounds this view exactly as it bounds the live one:
        // these are the last events in the file, not the whole session.
        "tail_bounded": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Claude shape is verbatim from a captured PreToolUse payload; the
    /// Codex one from a real `spawn_agent` function_call in a rollout. Both had
    /// their prompt/message bodies elided, which is the point of the whitelist.
    #[test]
    fn a_spawn_lifts_its_parameters_and_never_its_prompt() {
        let claude = json!({
            "description": "List files in hooks/ directory",
            "prompt": "a task body thousands of characters long",
            "subagent_type": "Explore", "model": "haiku", "run_in_background": false
        });
        let got = spawn_params("Agent", Some(&claude)).expect("a spawn has params");
        assert_eq!(got["model"], "haiku");
        assert_eq!(got["subagent_type"], "Explore");
        assert_eq!(got["run_in_background"], false);
        assert!(got.get("prompt").is_none() && got.get("description").is_none());

        let codex = json!({
            "agent_type": "default", "fork_context": false, "model": "gpt-5.5",
            "reasoning_effort": "xhigh", "service_tier": "priority",
            "message": "a task body"
        });
        let got = spawn_params("spawn_agent", Some(&codex)).unwrap();
        assert_eq!(got["model"], "gpt-5.5");
        assert_eq!(got["reasoning_effort"], "xhigh");
        assert!(got.get("message").is_none());
    }

    #[test]
    fn a_call_that_is_not_a_spawn_has_none() {
        // `model` on some other tool is not a spawn parameter, and an agent tool
        // that operates on an existing child is not a spawn either.
        assert!(spawn_params("Bash", Some(&json!({"command": "ls", "model": "haiku"}))).is_none());
        assert!(spawn_params("multi_agent_v1wait_agent", Some(&json!({"agent_id": "a"}))).is_none());
        // A spawn that named nothing renders as absent, never as a default.
        assert!(spawn_params("Task", Some(&json!({"prompt": "p"}))).is_none());
        assert!(spawn_params("Agent", None).is_none());
    }
}
