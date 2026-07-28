//! Sybil's Alcove — Rust core.
//!
//! Phase 1 is the NORMALISER only: the part that turns two undocumented,
//! shifting transcript formats into one vocabulary. It emits a canonical
//! snapshot and nothing else, so it can be diffed against the Python
//! implementation over the same corpus.
//!
//! The canonical snapshot deliberately excludes everything volatile — wall
//! clock, file ages, liveness, pids, process state — because those differ
//! between two runs of the SAME implementation and would make the gate useless.
//! What remains is exactly the parsing facts, which is what a port can get
//! wrong.

mod claude;
mod codex;
mod model;
mod par;
mod transcripts;

use std::path::PathBuf;

use serde::Serialize;


use model::{Compaction, ModelAt, Selection, Usage};

#[derive(Serialize)]
struct CanonSub {
    id: String,
    label: String,
    model: String,
    role: String,
    status: String,
    turns: i64,
    usage: Usage,
    task: String,
}

#[derive(Serialize)]
struct CanonSession {
    harness: String,
    session_id: String,
    label: String,
    project: String,
    cwd: String,
    branch: String,
    effort: String,
    model: String,
    selected_model: String,
    turns: i64,
    last_ts: String,
    usage: Usage,
    timeline: Vec<ModelAt>,
    selections: Vec<Selection>,
    compactions: Vec<Compaction>,
    subagents: Vec<CanonSub>,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root"))
}

fn env_path(key: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(key).map(PathBuf::from).unwrap_or(default)
}

fn main() {
    let claude_root = env_path("ALCOVE_CLAUDE_ROOT", home().join(".claude/projects"));
    let codex_root = env_path("ALCOVE_CODEX_ROOT", home().join(".codex/sessions"));

    let mut out: Vec<CanonSession> = Vec::new();

    for s in claude::collect(&claude_root) {
        let mut subs: Vec<CanonSub> = s
            .subagents
            .into_iter()
            .map(|a| CanonSub {
                id: a.id,
                label: a.label,
                model: a.model,
                role: a.role,
                status: a.status,
                turns: a.turns,
                usage: a.usage,
                task: a.task,
            })
            .collect();
        // Sorted by id, NOT by the live-first order the UI uses: that order
        // depends on file mtimes and would differ between two runs seconds
        // apart, which is noise rather than a parsing difference.
        subs.sort_by(|a, b| a.id.cmp(&b.id));
        out.push(CanonSession {
            harness: "claude".into(),
            session_id: s.session_id,
            label: s.label,
            project: s.project,
            cwd: s.cwd,
            branch: s.branch,
            effort: s.effort,
            model: s.model,
            selected_model: s.selected_model,
            turns: s.turns,
            last_ts: s.last_ts,
            usage: s.usage,
            timeline: s.timeline,
            selections: s.selections,
            compactions: s.compactions,
            subagents: subs,
        });
    }

    for s in codex::collect(&codex_root) {
        let mut subs: Vec<CanonSub> = s
            .subagents
            .into_iter()
            .map(|a| CanonSub {
                id: a.id,
                label: a.label,
                model: a.model,
                role: a.role,
                // Codex status comes from its private sqlite, which phase 1
                // does not read; the Python side is run with that enrichment
                // disabled so the two are compared like for like.
                status: String::new(),
                turns: a.turns,
                usage: a.usage,
                task: a.task,
            })
            .collect();
        subs.sort_by(|a, b| a.id.cmp(&b.id));
        out.push(CanonSession {
            harness: "codex".into(),
            session_id: s.session_id,
            label: s.label,
            project: s.project,
            cwd: s.cwd,
            branch: String::new(),
            effort: s.effort,
            model: s.model,
            selected_model: String::new(),
            turns: s.turns,
            last_ts: s.last_ts,
            usage: s.usage,
            timeline: s.timeline,
            selections: Vec::new(),
            compactions: s.compactions,
            subagents: subs,
        });
    }

    out.sort_by(|a, b| {
        (a.harness.as_str(), a.session_id.as_str()).cmp(&(b.harness.as_str(), b.session_id.as_str()))
    });

    // Serialise the struct DIRECTLY. Going through `json!` turns it into a
    // serde_json::Value, whose map is a BTreeMap, which alphabetises the keys —
    // producing output identical in content but ordered differently from
    // Python's insertion-ordered dicts, and a 4,649-line diff of pure noise.
    #[derive(Serialize)]
    struct Doc {
        sessions: Vec<CanonSession>,
    }
    println!("{}", serde_json::to_string_pretty(&Doc { sessions: out }).unwrap());
}
