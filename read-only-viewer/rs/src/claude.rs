//! Claude Code transcripts. Port of `alcove/sources/claude.py`.
//!
//!     ~/.claude/projects/<project>/<session-id>.jsonl          main thread
//!     ~/.claude/projects/<project>/<session-id>/subagents/
//!         agent-<agentId>.jsonl                                one per subagent
//!
//! Every child entry is `isSidechain: true`, so a parent's own totals must skip
//! sidechain events or it absorbs its subagents' work.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;

use crate::launch::{observe_claude, RawLaunch};
use crate::model::{
    event_effort, event_text, is_real_model, push_effort, push_model, push_selection,
    push_version, Compaction, EffortAt, ModelAt, Selection, TurnRow, Usage, VersionAt,
};
use crate::cache::ScanCache;
use crate::transcripts::{chronological, file_size, tail_events};

#[derive(Clone)]
pub struct Scan {
    pub turn_rows: Vec<TurnRow>,
    pub timeline: Vec<ModelAt>,
    pub model: String,
    pub selections: Vec<Selection>,
    pub selected_model: String,
    pub usage: Usage,
    pub turns: i64,
    pub last_ts: String,
    pub cwd: String,
    pub branch: String,
    pub effort: String,
    pub effort_timeline: Vec<EffortAt>,
    pub version: String,
    pub version_timeline: Vec<VersionAt>,
    pub compactions: Vec<Compaction>,
    pub usage_since_compact: Option<Usage>,
    pub turns_since_compact: Option<i64>,
    pub launches: Vec<RawLaunch>,
}

pub struct SubAgent {
    pub turn_rows: Vec<TurnRow>,
    pub effort: String,
    pub effort_timeline: Vec<EffortAt>,
    pub version: String,
    pub version_timeline: Vec<VersionAt>,
    pub id: String,
    pub label: String,
    pub model: String,
    pub record_model: String,
    pub role: String,
    pub status: String,
    pub turns: i64,
    pub usage: Usage,
    pub task: String,
    pub size: u64,
    pub no_transcript: bool,
    /// The child transcript's own last event timestamp. Harness-written UTC, so
    /// it is directly comparable with a spool `ts` — which mtime is not, and
    /// which is the whole reason it is carried out of the scan.
    pub last_ts: String,
    pub launches: Vec<RawLaunch>,
}

pub struct Session {
    pub turn_rows: Vec<TurnRow>,
    pub session_id: String,
    pub label: String,
    pub project: String,
    pub cwd: String,
    pub branch: String,
    pub effort: String,
    pub effort_timeline: Vec<EffortAt>,
    pub version: String,
    pub version_timeline: Vec<VersionAt>,
    pub model: String,
    pub selected_model: String,
    pub selections: Vec<Selection>,
    pub timeline: Vec<ModelAt>,
    pub usage: Usage,
    pub turns: i64,
    pub last_ts: String,
    pub compactions: Vec<Compaction>,
    pub subagents: Vec<SubAgent>,
    pub path: PathBuf,
    pub launches: Vec<RawLaunch>,
}

struct Res {
    model_set: Regex,
    model_args: Regex,
    ansi: Regex,
    model_tail: Regex,
}

impl Res {
    fn new() -> Self {
        Self {
            // The ONLY on-disk record of a switch that never served a turn.
            model_set: Regex::new(r"Set model to ([^<\n]+)").unwrap(),
            model_args: Regex::new(r"<command-args>([^<]*)</command-args>").unwrap(),
            ansi: Regex::new(r"\x1b\[[0-9;]*m").unwrap(),
            model_tail: Regex::new(r"\s+and saved as .*$").unwrap(),
        }
    }

    /// Two stdout shapes exist: a model id ("claude-opus-5[1m]") and a bolded
    /// display name with a trailing clause. Strip styling and the clause, or the
    /// model name comes out as a sentence.
    fn clean_model_name(&self, raw: &str) -> String {
        let no_ansi = self.ansi.replace_all(raw, "");
        self.model_tail.replace(&no_ansi, "").trim().to_string()
    }
}

pub fn scan(path: &Path, main_thread_only: bool) -> Scan {
    let res = Res::new();
    let mut timeline: Vec<ModelAt> = Vec::new();
    let mut effort_timeline: Vec<EffortAt> = Vec::new();
    let mut version_timeline: Vec<VersionAt> = Vec::new();
    let mut turn_rows: Vec<TurnRow> = Vec::new();
    // message.id -> the row it produced, so the thinking blocks spread across a
    // turn's several events land on ONE row rather than on several.
    let mut row_of_msg: HashMap<String, usize> = HashMap::new();
    let mut selections: Vec<Selection> = Vec::new();
    let mut pending_args = String::new();
    let mut usage = Usage::default();
    let mut ctx_usage = Usage::default();
    let mut seen_msgs: HashSet<String> = HashSet::new();
    let (mut turns, mut ctx_turns) = (0i64, 0i64);
    let mut compactions: Vec<Compaction> = Vec::new();
    let mut launches: Vec<RawLaunch> = Vec::new();
    let (mut last_ts, mut cwd, mut branch, mut effort) =
        (String::new(), String::new(), String::new(), String::new());
    let mut version = String::new();

    for event in chronological(tail_events(path), "timestamp") {
        observe_claude(&event, &mut launches);
        if cwd.is_empty() {
            if let Some(v) = event.get("cwd").and_then(Value::as_str) {
                cwd = v.to_string();
            }
        }
        if branch.is_empty() {
            if let Some(v) = event.get("gitBranch").and_then(Value::as_str) {
                branch = v.to_string();
            }
        }
        let ts = event.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();

        // A compact boundary means everything before it has left the context
        // window, so a second set of counters resets here.
        if event.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
            let meta = event.get("compactMetadata");
            compactions.push(Compaction {
                at: ts.clone(),
                trigger: meta
                    .and_then(|m| m.get("trigger"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                pre_tokens: meta.and_then(|m| m.get("preTokens")).and_then(Value::as_i64),
            });
            ctx_usage = Usage::default();
            ctx_turns = 0;
            continue;
        }

        let etype = event.get("type").and_then(Value::as_str).unwrap_or("");
        if etype == "user" {
            // The command and its resolved output are two events sharing a
            // timestamp, so remember the requested alias until the resolved
            // name arrives on the next one.
            let text = event_text(&event);
            if text.contains("/model") {
                if let Some(c) = res.model_args.captures(&text) {
                    pending_args = c[1].trim().to_string();
                }
            }
            if let Some(c) = res.model_set.captures(&text) {
                let name = res.clean_model_name(&c[1]);
                if !name.is_empty() {
                    push_selection(&mut selections, &name, &ts, &pending_args);
                }
                pending_args.clear();
            }
            continue;
        }
        if etype != "assistant" {
            continue;
        }
        // A parent's own totals must not absorb its subagents' turns.
        if main_thread_only && event.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(message) = event.get("message").filter(|m| m.is_object()) else {
            continue;
        };
        if !ts.is_empty() {
            last_ts = ts.clone();
        }
        // Per EVENT, which is what makes the per-turn trace possible at all: this
        // is not a session setting read once, it is stamped on every assistant
        // event the harness writes.
        let at_effort = event_effort(&event);
        if !at_effort.is_empty() {
            effort = at_effort.clone();
        }
        // The CLI build that served this event. Top-level, on every line the
        // harness writes, and it MOVES: a session left open across upgrades
        // records each one in place, so this is a per-turn fact rather than a
        // session header.
        let at_version =
            event.get("version").and_then(Value::as_str).unwrap_or("").to_string();
        if !at_version.is_empty() {
            version = at_version.clone();
        }
        // Thinking blocks carry a signature and no text (0 of 22,669 measured),
        // so the honest signal is that the model thought and how many times —
        // never a body. Counted per EVENT and summed onto the turn's row below,
        // because one turn is written as several events.
        let thinking = message
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("thinking"))
                    .count() as i64
            })
            .unwrap_or(0);

        // One logical turn writes SEVERAL assistant events (one per content
        // block), each repeating the same message.id AND the same usage dict.
        // Counting per event overstated one session 1757 vs 761 turns and its
        // output 2.18M vs 0.82M. An event without an id still counts once.
        let msg_id = message.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let first = !seen_msgs.contains(&msg_id);
        if !msg_id.is_empty() {
            seen_msgs.insert(msg_id.clone());
        }
        if first {
            usage.add_anthropic(message.get("usage"));
            ctx_usage.add_anthropic(message.get("usage"));
        }
        let model = message.get("model").and_then(Value::as_str);
        if !is_real_model(model) {
            continue;
        }
        let model = model.unwrap();
        if first {
            turns += 1;
            ctx_turns += 1;
            // Keyed by message.id so a re-scan of overlapping windows is a
            // no-op; with no id, file+timestamp is still stable across scans.
            let u = message.get("usage");
            let g = |k: &str| u.and_then(|x| x.get(k)).and_then(Value::as_i64).unwrap_or(0);
            if !msg_id.is_empty() {
                row_of_msg.insert(msg_id.clone(), turn_rows.len());
            }
            turn_rows.push(TurnRow {
                id: if msg_id.is_empty() {
                    format!("{}:{}", path.file_name().unwrap_or_default().to_string_lossy(), ts)
                } else {
                    msg_id.clone()
                },
                ts: ts.clone(),
                model: model.to_string(),
                input: Some(g("input_tokens")),
                output: Some(g("output_tokens")),
                cache_read: Some(g("cache_read_input_tokens")),
                cache_write: Some(g("cache_creation_input_tokens")),
                effort: at_effort.clone(),
                version: at_version.clone(),
                // Countable on every Claude event, so Some(0) is a fact: this
                // turn thought nothing. Codex has no blocks to count at all.
                thinking_blocks: Some(thinking),
                reasoning_tokens: None,
            });
        } else if let Some(&at) = row_of_msg.get(&msg_id) {
            // A later event of the SAME turn. Its blocks belong to the row the
            // first event opened.
            let prior = turn_rows[at].thinking_blocks.unwrap_or(0);
            turn_rows[at].thinking_blocks = Some(prior + thinking);
        }
        push_model(&mut timeline, model, &ts);
        push_effort(&mut effort_timeline, &at_effort, &ts);
        push_version(&mut version_timeline, &at_version, &ts);
    }

    let model = timeline.last().map(|t| t.model.clone()).unwrap_or_default();
    let selected_model = selections.last().map(|s| s.model.clone()).unwrap_or_default();
    let has_compactions = !compactions.is_empty();
    Scan {
        turn_rows,
        timeline,
        model,
        selections,
        selected_model,
        usage,
        turns,
        last_ts,
        cwd,
        branch,
        effort,
        effort_timeline,
        version,
        version_timeline,
        compactions,
        usage_since_compact: if has_compactions { Some(ctx_usage) } else { None },
        turns_since_compact: if has_compactions { Some(ctx_turns) } else { None },
        launches,
    }
}

struct SpawnRecord {
    agent_type: String,
    resolved_model: String,
    status: String,
    task: String,
}

/// Per-subagent records the parent wrote, keyed by agentId. Enriches the child
/// transcript reading; never gates whether a subagent is shown.
fn spawn_records(transcript: &Path) -> HashMap<String, SpawnRecord> {
    let mut out = HashMap::new();
    for event in tail_events(transcript) {
        let Some(result) = event.get("toolUseResult").filter(|r| r.is_object()) else {
            continue;
        };
        let Some(agent_id) = result.get("agentId").and_then(Value::as_str) else {
            continue;
        };
        let s = |k: &str| result.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let mut task = s("prompt");
        // Match Python's [:240] slice, which counts CHARACTERS not bytes.
        if task.chars().count() > 240 {
            task = task.chars().take(240).collect();
        }
        out.insert(
            agent_id.to_string(),
            SpawnRecord {
                agent_type: s("agentType"),
                resolved_model: s("resolvedModel"),
                status: s("status"),
                task,
            },
        );
    }
    out
}

pub fn collect(root: &Path, cache: &ScanCache<Scan>) -> Vec<Session> {
    let mut sessions = Vec::new();
    if !root.is_dir() {
        return sessions;
    }
    let mut projects: Vec<PathBuf> = match std::fs::read_dir(root) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect(),
        Err(_) => return sessions,
    };
    projects.sort();

    for project in projects {
        let mut transcripts: Vec<PathBuf> = match std::fs::read_dir(&project) {
            Ok(rd) => rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
                .collect(),
            Err(_) => continue,
        };
        transcripts.sort();

        // Scan the project's transcripts across cores; pmap preserves input
        // order, so the result is identical to the sequential version.
        let scans = crate::par::pmap(transcripts, |t| {
            (t.clone(), cache.get_or_scan(t, || scan(t, true)))
        });

        for (transcript, info) in scans {
            let sid = transcript.file_stem().unwrap_or_default().to_string_lossy().to_string();
            let sub_dir = project.join(&sid).join("subagents");
            let records =
                if sub_dir.is_dir() { spawn_records(&transcript) } else { HashMap::new() };

            let mut subs: Vec<SubAgent> = Vec::new();
            if sub_dir.is_dir() {
                let mut children: Vec<PathBuf> = match std::fs::read_dir(&sub_dir) {
                    Ok(rd) => rd
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| {
                            p.file_name()
                                .map(|n| n.to_string_lossy().starts_with("agent-"))
                                .unwrap_or(false)
                                && p.extension().map(|x| x == "jsonl").unwrap_or(false)
                        })
                        .collect(),
                    Err(_) => Vec::new(),
                };
                children.sort();
                let child_scans = crate::par::pmap(children, |c| {
                    (c.clone(), cache.get_or_scan(c, || scan(c, false)))
                });
                for (child, child_info) in child_scans {
                    let stem = child.file_stem().unwrap_or_default().to_string_lossy().to_string();
                    let agent_id = stem.trim_start_matches("agent-").to_string();
                    let record = records.get(&agent_id);
                    subs.push(SubAgent {
                        turn_rows: child_info.turn_rows.clone(),
                        // A child transcript is a full transcript, so the effort
                        // trace comes out of the same scan with no extra work.
                        effort: child_info.effort.clone(),
                        effort_timeline: child_info.effort_timeline.clone(),
                        version: child_info.version.clone(),
                        version_timeline: child_info.version_timeline.clone(),
                        label: agent_id.chars().take(12).collect(),
                        // Child transcript wins: written from the first event, so
                        // a running subagent reports its model before any record.
                        model: if child_info.model.is_empty() {
                            record.map(|r| r.resolved_model.clone()).unwrap_or_default()
                        } else {
                            child_info.model.clone()
                        },
                        record_model: record.map(|r| r.resolved_model.clone()).unwrap_or_default(),
                        role: record.map(|r| r.agent_type.clone()).unwrap_or_default(),
                        status: record.map(|r| r.status.clone()).unwrap_or_default(),
                        turns: child_info.turns,
                        usage: child_info.usage,
                        task: record.map(|r| r.task.clone()).unwrap_or_default(),
                        size: file_size(&child),
                        no_transcript: false,
                        last_ts: child_info.last_ts.clone(),
                        launches: child_info.launches.clone(),
                        id: agent_id,
                    });
                }
            }
            // A record with no transcript is still a spawn that happened.
            let seen: HashSet<String> = subs.iter().map(|s| s.id.clone()).collect();
            let mut orphans: Vec<(&String, &SpawnRecord)> =
                records.iter().filter(|(k, _)| !seen.contains(*k)).collect();
            orphans.sort_by(|a, b| a.0.cmp(b.0));
            for (agent_id, record) in orphans {
                subs.push(SubAgent {
                    turn_rows: Vec::new(),
                    // No transcript, so nothing observed the effort. Not "".
                    effort: String::new(),
                    effort_timeline: Vec::new(),
                    version: String::new(),
                    version_timeline: Vec::new(),
                    id: agent_id.clone(),
                    label: agent_id.chars().take(12).collect(),
                    model: record.resolved_model.clone(),
                    record_model: record.resolved_model.clone(),
                    role: record.agent_type.clone(),
                    status: record.status.clone(),
                    turns: 0,
                    usage: Usage::default(),
                    task: record.task.clone(),
                    size: 0,
                    no_transcript: true,
                    last_ts: String::new(),
                    launches: Vec::new(),
                });
            }

            sessions.push(Session {
                turn_rows: info.turn_rows.clone(),
                label: sid.chars().take(8).collect(),
                session_id: sid,
                project: project.file_name().unwrap_or_default().to_string_lossy().to_string(),
                cwd: info.cwd,
                branch: info.branch,
                effort: info.effort,
                effort_timeline: info.effort_timeline,
                version: info.version,
                version_timeline: info.version_timeline,
                model: info.model,
                selected_model: info.selected_model,
                selections: info.selections,
                timeline: info.timeline,
                usage: info.usage,
                turns: info.turns,
                last_ts: info.last_ts,
                compactions: info.compactions,
                subagents: subs,
                path: transcript,
                launches: info.launches,
            });
        }
    }
    sessions
}
