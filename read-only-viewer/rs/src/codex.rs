//! Codex rollout transcripts. Port of `alcove/sources/codex.py`.
//!
//!     ~/.codex/sessions/<Y>/<M>/<D>/rollout-<ts>-<id>.jsonl
//!
//! A Codex subagent writes a full sibling transcript with its own thread id; the
//! link back is `parent_thread_id` in its `session_meta`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::model::{
    is_real_model, push_effort, push_model, Compaction, EffortAt, ModelAt, TurnRow, Usage,
};
use crate::cache::ScanCache;
use crate::transcripts::{chronological, file_size, head_events, tail_events};

#[derive(Clone)]
pub struct Scan {
    pub turn_rows: Vec<TurnRow>,
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
    pub effort_timeline: Vec<EffortAt>,
    /// `session_meta.cli_version` — the build that WROTE this rollout. See the
    /// note on `TurnRow::version`: Codex records no per-turn version, so this is
    /// a session fact and never a turn one.
    pub version: String,
    pub compactions: Vec<Compaction>,
    pub usage_since_compact: Option<Usage>,
    pub turns_since_compact: Option<i64>,
    pub path: PathBuf,
    pub size: u64,
    pub age_s: Option<f64>,
    pub live: bool,
    pub spawn_status: String,
    pub branch: String,
}

pub struct SubAgent {
    pub status: String,
    pub nickname: String,
    pub turn_rows: Vec<TurnRow>,
    pub effort: String,
    pub effort_timeline: Vec<EffortAt>,
    pub version: String,
    pub age_s: Option<f64>,
    pub live: bool,
    pub size: u64,
    pub id: String,
    pub label: String,
    pub model: String,
    pub role: String,
    pub turns: i64,
    pub usage: Usage,
    pub task: String,
}

pub struct Session {
    pub branch: String,
    pub nickname: String,
    pub turn_rows: Vec<TurnRow>,
    pub session_id: String,
    pub label: String,
    pub project: String,
    pub cwd: String,
    pub effort: String,
    pub effort_timeline: Vec<EffortAt>,
    pub version: String,
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
    let mut effort_timeline: Vec<EffortAt> = Vec::new();
    let mut turn_rows: Vec<TurnRow> = Vec::new();
    // Reasoning tokens accrue across a turn's several model round-trips and are
    // reported AFTER the assistant message, so they are bucketed between
    // `turn_context` boundaries and attached when the bucket closes. `None`
    // until a `last_token_usage` is actually seen: absent is not zero.
    let (mut bucket_reasoning, mut bucket_start) = (Option::<i64>::None, 0usize);
    let mut usage = Usage::default();
    let (mut turns, mut ctx_turns) = (0i64, 0i64);
    let mut compactions: Vec<Compaction> = Vec::new();
    let mut usage_at_compact: Option<Usage> = None;
    let (mut last_ts, mut cwd, mut effort) = (String::new(), String::new(), String::new());
    let (mut role, mut nickname) = (String::new(), String::new());
    let mut version = String::new();
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
        // The only version Codex writes down. `turn_context` carries
        // `multi_agent_version: "v1"` and nothing else version-shaped (678/678
        // lines measured across July) — a feature-schema marker, not a build, so
        // it is deliberately not read as one.
        version = s("cli_version");
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
            // NOT the turn signal, whatever the name says — counting it reported
            // every Codex session and subagent as having taken exactly one turn.
            // It is a turn BOUNDARY: written before a turn runs and again on a
            // model or effort change (153 of them against 126 `task_started` in
            // one measured rollout), which is what makes it the right place to
            // close a reasoning bucket and the wrong place to take a time from.
            //
            // **Its timestamp is not trustworthy.** On resume Codex replays the
            // whole history into the new rollout and restamps every replayed line
            // with the file-open time: in
            // `rollout-2026-07-31T01-26-37-019fb5c7…` 147 of 153 turn_context
            // lines share three seconds at the head of the file. Order survives
            // that; chronology does not. So the effort switch is recorded at the
            // timestamp of the TURN it governed, taken from the assistant message
            // below, and never from here.
            let model = payload.get("model").and_then(Value::as_str);
            if let Some(e) = payload.get("effort").and_then(Value::as_str) {
                if !e.is_empty() {
                    effort = e.to_string();
                }
            }
            // Close the previous turn's reasoning bucket onto the row it belongs
            // to. The trailing `token_count` lands after the assistant message,
            // so attaching on arrival would push it onto the following turn.
            if let (Some(n), true) = (bucket_reasoning, turn_rows.len() > bucket_start) {
                if let Some(row) = turn_rows.last_mut() {
                    row.reasoning_tokens = Some(n);
                }
            }
            bucket_reasoning = None;
            bucket_start = turn_rows.len();
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
            let pid = payload.get("id").and_then(Value::as_str).unwrap_or("");
            turn_rows.push(TurnRow {
                id: if pid.is_empty() {
                    format!("{}:{}", path.file_name().unwrap_or_default().to_string_lossy(), ts)
                } else {
                    pid.to_string()
                },
                ts: ts.clone(),
                model: timeline.last().map(|m| m.model.clone()).unwrap_or_default(),
                // Cumulative session snapshots, so no per-turn attribution.
                input: None,
                output: None,
                cache_read: None,
                cache_write: None,
                // The effort the preceding `turn_context` set, stamped on the
                // turn it actually governed.
                effort: effort.clone(),
                // Not the rollout's cli_version: a resumed rollout replays turns
                // that an older build served. See TurnRow::version.
                version: String::new(),
                // Codex writes no thinking blocks to count.
                thinking_blocks: None,
                // Filled when the bucket closes; see the `turn_context` arm.
                reasoning_tokens: None,
            });
            push_effort(&mut effort_timeline, &effort, &ts);
        } else if kind == "event_msg" && ptype == "token_count" {
            let Some(info) = payload.get("info").filter(|i| i.is_object()) else {
                continue;
            };
            // Per-request, unlike `total_token_usage` next to it — the one
            // per-turn "how much did it think" number Codex records, and the
            // analogue of Claude's thinking-block count.
            if let Some(last) = info.get("last_token_usage").filter(|t| t.is_object()) {
                if let Some(r) = last.get("reasoning_output_tokens").and_then(Value::as_i64) {
                    bucket_reasoning = Some(bucket_reasoning.unwrap_or(0) + r);
                }
            }
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

    // The last bucket has no closing `turn_context`; close it at end of file.
    if let (Some(n), true) = (bucket_reasoning, turn_rows.len() > bucket_start) {
        if let Some(row) = turn_rows.last_mut() {
            row.reasoning_tokens = Some(n);
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
        turn_rows,
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
        effort_timeline,
        version,
        compactions,
        usage_since_compact: since,
        turns_since_compact: if has_compactions { Some(ctx_turns) } else { None },
        size: file_size(path),
        age_s: None,
        live: false,
        spawn_status: String::new(),
        branch: String::new(),
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
    // Sort on (mtime_ns, path), not mtime alone — see the note in the Python
    // source: a resumed thread replays the same message ids into a new rollout,
    // and INSERT OR IGNORE keeps whichever is seen first.
    paths.sort_by(|a, b| {
        let key = |p: &PathBuf| {
            let ns = p
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            (ns, p.clone())
        };
        key(a).cmp(&key(b))
    });

    // Rollouts are independent files; scan them across cores, then merge
    // sequentially so the newest-file-wins ordering is unchanged.
    let live_window: f64 =
        std::env::var("ALCOVE_LIVE_WINDOW_S").ok().and_then(|v| v.parse().ok()).unwrap_or(300.0);
    let mut scans = crate::par::pmap(paths, |p| cache.get_or_scan(p, || scan(p)));
    // Age is wall-clock, so it is computed per collect rather than cached with
    // the scan — a cached `live` flag would freeze at whatever it was when the
    // file was last parsed.
    for info in scans.iter_mut() {
        info.age_s = info
            .path
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
            .map(|d| d.as_secs_f64());
        info.live = info.age_s.map(|a| a < live_window).unwrap_or(false);
    }

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
                prior.turn_rows.extend(info.turn_rows.clone());
                for m in &info.timeline {
                    if prior.timeline.last().map(|t| &t.model) != Some(&m.model) {
                        prior.timeline.push(m.clone());
                    }
                }
                for e in &info.effort_timeline {
                    if prior.effort_timeline.last().map(|t| &t.effort) != Some(&e.effort) {
                        prior.effort_timeline.push(e.clone());
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
                if !info.version.is_empty() {
                    prior.version = info.version.clone();
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

    // Codex's own sqlite, if readable. Transcript facts always win: the rollout
    // is what actually happened, and this is a convenience index over it. It
    // fills gaps — the open/closed status transcripts never record, a branch, an
    // effort. Absent or reshaped, every value below stays as it was.
    let state = crate::codex_state::read();
    for info in merged.iter_mut() {
        let meta = state.threads.get(&info.session_id);
        let edge = state.edges.get(&info.session_id);
        if info.role.is_empty() {
            info.role = meta.map(|m| m.role.clone()).unwrap_or_default();
        }
        if info.nickname.is_empty() {
            info.nickname = meta.map(|m| m.nickname.clone()).unwrap_or_default();
        }
        if info.parent.is_empty() {
            info.parent = edge.map(|e| e.parent.clone()).unwrap_or_default();
        }
        if info.effort.is_empty() {
            info.effort = meta.map(|m| m.effort.clone()).unwrap_or_default();
        }
        if info.model.is_empty() {
            info.model = meta.map(|m| m.model.clone()).unwrap_or_default();
        }
        info.spawn_status = edge.map(|e| e.status.clone()).unwrap_or_default();
        info.branch = meta.map(|m| m.branch.clone()).unwrap_or_default();
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
                // Codex records no completion in the transcript; the spawn edge is
                // the only place open/closed is written down.
                status: child.spawn_status.clone(),
                nickname: child.nickname.clone(),
                turn_rows: child.turn_rows.clone(),
                // A Codex subagent writes its own full rollout, so its effort
                // trace comes out of the same scan the parent's does.
                effort: child.effort.clone(),
                effort_timeline: child.effort_timeline.clone(),
                version: child.version.clone(),
                age_s: child.age_s,
                live: child.live,
                size: child.size,
                id: child.session_id.clone(),
                label: child.session_id.chars().take(12).collect(),
                model: child.model.clone(),
                role: child.role.clone(),
                turns: child.turns,
                usage: child.usage,
                task: child.nickname.clone(),
            });
        }
        // Running subagents first, then freshest — the same order the session
        // list uses, so the eye moves the same way at both levels. Python sorts
        // here too; omitting it made the store's INSERT OR IGNORE pick a
        // different winner for ids that appear in more than one thread.
        subs.sort_by(|a, b| {
            let key = |s: &SubAgent| (!s.live, s.age_s.unwrap_or(1e18));
            key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal)
        });
        let _ = i;
        sessions.push(Session {
            branch: info.branch.clone(),
            nickname: info.nickname.clone(),
            turn_rows: info.turn_rows.clone(),
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
            effort_timeline: info.effort_timeline.clone(),
            version: info.version.clone(),
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
