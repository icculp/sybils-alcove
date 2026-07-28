//! Codex rollout transcripts. Port of `alcove/sources/codex.py`.
//!
//!     ~/.codex/sessions/<Y>/<M>/<D>/rollout-<ts>-<id>.jsonl
//!
//! A Codex subagent writes a full sibling transcript with its own thread id; the
//! link back is `parent_thread_id` in its `session_meta`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::model::{is_real_model, push_model, Compaction, ModelAt, Usage};
use crate::cache::ScanCache;
use crate::transcripts::{chronological, file_size, head_events, tail_events};

#[derive(Clone)]
pub struct Scan {
    pub session_id: String,
    pub parent: String,
    pub role: String,
    pub nickname: String,
    pub timeline: Vec<ModelAt>,
    pub model: String,
    pub usage: Usage,
    pub turns: i64,
    pub last_ts: String,
    pub cwd: String,
    pub effort: String,
    pub compactions: Vec<Compaction>,
    pub usage_since_compact: Option<Usage>,
    pub turns_since_compact: Option<i64>,
    pub path: PathBuf,
    pub size: u64,
}

pub struct SubAgent {
    pub id: String,
    pub label: String,
    pub model: String,
    pub role: String,
    pub turns: i64,
    pub usage: Usage,
    pub task: String,
}

pub struct Session {
    pub session_id: String,
    pub label: String,
    pub project: String,
    pub cwd: String,
    pub effort: String,
    pub model: String,
    pub timeline: Vec<ModelAt>,
    pub usage: Usage,
    pub turns: i64,
    pub last_ts: String,
    pub compactions: Vec<Compaction>,
    pub subagents: Vec<SubAgent>,
    pub path: PathBuf,
}

pub fn scan(path: &Path) -> Scan {
    let mut timeline: Vec<ModelAt> = Vec::new();
    let mut usage = Usage::default();
    let (mut turns, mut ctx_turns) = (0i64, 0i64);
    let mut compactions: Vec<Compaction> = Vec::new();
    let mut usage_at_compact: Option<Usage> = None;
    let (mut last_ts, mut cwd, mut effort) = (String::new(), String::new(), String::new());
    let (mut role, mut nickname) = (String::new(), String::new());
    let (mut sid, mut parent) = (String::new(), String::new());

    // Identity from the head (line 1), activity from the tail. `payload.id` is
    // the thread's OWN id; `payload.session_id` on a spawned agent is the
    // PARENT's, so reading session_id first collapses children into parents.
    for event in head_events(path) {
        if event.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(payload) = event.get("payload").filter(|p| p.is_object()) else {
            continue;
        };
        let s = |k: &str| payload.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        sid = {
            let id = s("id");
            if id.is_empty() { s("session_id") } else { id }
        };
        cwd = s("cwd");
        role = s("agent_role");
        nickname = s("agent_nickname");
        parent = s("parent_thread_id");
        if parent.is_empty() {
            if let Some(spawn) = payload
                .get("source")
                .and_then(|src| src.get("subagent"))
                .and_then(|sa| sa.get("thread_spawn"))
            {
                let g = |k: &str| spawn.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                parent = g("parent_thread_id");
                if role.is_empty() {
                    role = g("agent_role");
                }
                if nickname.is_empty() {
                    nickname = g("agent_nickname");
                }
            }
        }
        break;
    }

    for event in chronological(tail_events(path), "timestamp") {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = event.get("payload");
        let ts = event.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
        let ptype = payload.and_then(|p| p.get("type")).and_then(Value::as_str).unwrap_or("");

        // Codex marks one compaction TWICE — a `compacted` record and an
        // `event_msg`/`context_compacted` — landing milliseconds apart, so
        // compare at second granularity. Its token totals are cumulative
        // snapshots, so the post-boundary figure is a subtraction, not a reset.
        if kind == "compacted" || ptype == "context_compacted" {
            let dup = compactions
                .last()
                .map(|c| c.at.chars().take(19).eq(ts.chars().take(19)))
                .unwrap_or(false);
            if !dup {
                compactions.push(Compaction {
                    at: ts.clone(),
                    trigger: String::new(),
                    pre_tokens: None,
                });
            }
            usage_at_compact = Some(usage);
            ctx_turns = 0;
            continue;
        }
        let Some(payload) = payload.filter(|p| p.is_object()) else {
            continue;
        };

        if kind == "turn_context" {
            // Written once per SESSION (and again on a model or effort change),
            // NOT once per turn. Counting it reported every Codex session and
            // subagent as having taken exactly one.
            let model = payload.get("model").and_then(Value::as_str);
            if let Some(e) = payload.get("effort").and_then(Value::as_str) {
                if !e.is_empty() {
                    effort = e.to_string();
                }
            }
            if is_real_model(model) {
                if !ts.is_empty() && last_ts.is_empty() {
                    last_ts = ts.clone();
                }
                push_model(&mut timeline, model.unwrap(), &ts);
            }
        } else if kind == "response_item"
            && ptype == "message"
            && payload.get("role").and_then(Value::as_str) == Some("assistant")
        {
            // The real per-turn signal; agrees with the count of
            // `event_msg`/`agent_message` events on the same transcript.
            turns += 1;
            ctx_turns += 1;
            if !ts.is_empty() {
                last_ts = ts.clone();
            }
        } else if kind == "event_msg" && ptype == "token_count" {
            let Some(info) = payload.get("info").filter(|i| i.is_object()) else {
                continue;
            };
            if let Some(total) = info.get("total_token_usage").filter(|t| t.is_object()) {
                let g = |k: &str| total.get(k).and_then(Value::as_i64).unwrap_or(0);
                // Cumulative snapshot: REPLACE, never accumulate.
                usage = Usage {
                    input: g("input_tokens"),
                    output: g("output_tokens"),
                    cache_read: g("cached_input_tokens"),
                    cache_write: g("cache_write_input_tokens"),
                    reasoning: g("reasoning_output_tokens"),
                };
            }
        }
    }

    let model = timeline.last().map(|t| t.model.clone()).unwrap_or_default();
    let has_compactions = !compactions.is_empty();
    let since = usage_at_compact.map(|at| Usage {
        input: (usage.input - at.input).max(0),
        output: (usage.output - at.output).max(0),
        cache_read: (usage.cache_read - at.cache_read).max(0),
        cache_write: (usage.cache_write - at.cache_write).max(0),
        reasoning: (usage.reasoning - at.reasoning).max(0),
    });
    Scan {
        session_id: sid,
        parent,
        role,
        nickname,
        timeline,
        model,
        usage,
        turns,
        last_ts,
        cwd,
        effort,
        compactions,
        usage_since_compact: since,
        turns_since_compact: if has_compactions { Some(ctx_turns) } else { None },
        size: file_size(path),
        path: path.to_path_buf(),
    }
}

fn walk_jsonl(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, out);
        } else if path.extension().map(|x| x == "jsonl").unwrap_or(false) {
            out.push(path);
        }
    }
}

pub fn collect(root: &Path, cache: &ScanCache<Scan>) -> Vec<Session> {
    let mut sessions = Vec::new();
    if !root.is_dir() {
        return sessions;
    }
    let mut paths = Vec::new();
    walk_jsonl(root, &mut paths);
    // A Codex thread can span several rollout files (resume, rollback); merge by
    // thread id with the NEWEST file winning, so sort by mtime as Python does.
    paths.sort_by_key(|p| p.metadata().and_then(|m| m.modified()).ok());

    // Rollouts are independent files; scan them across cores, then merge
    // sequentially so the newest-file-wins ordering is unchanged.
    let scans = crate::par::pmap(paths, |p| cache.get_or_scan(p, || scan(p)));

    let mut merged: Vec<Scan> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for info in scans {
        if info.session_id.is_empty() {
            continue;
        }
        match index.get(&info.session_id) {
            None => {
                index.insert(info.session_id.clone(), merged.len());
                merged.push(info);
            }
            Some(&at) => {
                // NOTE: no thread in the corpus this was written against
                // actually spanned multiple files, so this merge path is
                // effectively untested — in Python too.
                let prior = &mut merged[at];
                prior.size += info.size;
                prior.turns += info.turns;
                for m in &info.timeline {
                    if prior.timeline.last().map(|t| &t.model) != Some(&m.model) {
                        prior.timeline.push(m.clone());
                    }
                }
                if info.usage.output >= prior.usage.output {
                    prior.usage = info.usage;
                }
                let known: Vec<String> =
                    prior.compactions.iter().map(|c| c.at.chars().take(19).collect()).collect();
                for c in &info.compactions {
                    let k: String = c.at.chars().take(19).collect();
                    if !known.contains(&k) {
                        prior.compactions.push(c.clone());
                    }
                }
                // This file is newer (sorted), so its state is current state.
                if !info.model.is_empty() {
                    prior.model = info.model.clone();
                }
                if !info.effort.is_empty() {
                    prior.effort = info.effort.clone();
                }
                if !info.cwd.is_empty() {
                    prior.cwd = info.cwd.clone();
                }
                if !info.role.is_empty() {
                    prior.role = info.role.clone();
                }
                if !info.nickname.is_empty() {
                    prior.nickname = info.nickname.clone();
                }
                if !info.parent.is_empty() {
                    prior.parent = info.parent.clone();
                }
                if !info.last_ts.is_empty() {
                    prior.last_ts = info.last_ts.clone();
                }
            }
        }
    }

    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, info) in merged.iter().enumerate() {
        if !info.parent.is_empty() {
            children.entry(info.parent.clone()).or_default().push(i);
        }
    }

    for (i, info) in merged.iter().enumerate() {
        if !info.parent.is_empty() {
            continue; // rendered under its parent
        }
        let mut subs: Vec<SubAgent> = Vec::new();
        for &ci in children.get(&info.session_id).unwrap_or(&Vec::new()) {
            let child = &merged[ci];
            subs.push(SubAgent {
                id: child.session_id.clone(),
                label: child.session_id.chars().take(12).collect(),
                model: child.model.clone(),
                role: child.role.clone(),
                turns: child.turns,
                usage: child.usage,
                task: child.nickname.clone(),
            });
        }
        let _ = i;
        sessions.push(Session {
            // Codex thread ids are time-ordered, so two sessions started in the
            // same window share an 8-char prefix and read as one duplicated row.
            label: info.session_id.chars().take(13).collect(),
            session_id: info.session_id.clone(),
            project: Path::new(&info.cwd)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .filter(|_| !info.cwd.is_empty())
                .unwrap_or_else(|| "unknown".to_string()),
            cwd: info.cwd.clone(),
            effort: info.effort.clone(),
            model: info.model.clone(),
            timeline: info.timeline.clone(),
            usage: info.usage,
            turns: info.turns,
            last_ts: info.last_ts.clone(),
            compactions: info.compactions.clone(),
            subagents: subs,
            path: info.path.clone(),
        });
    }
    sessions
}
