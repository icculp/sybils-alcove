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

mod cache;
mod claude;
mod codex;
mod codex_state;
mod collect;
mod config;
mod events;
mod liveness;
mod model;
mod par;
mod process;
mod spill;
mod spool;
mod store;
mod transcripts;
mod watch;
mod web;

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
    // `--snapshot` emits the canonical form the equivalence gate diffs against;
    // with no arguments the binary serves.
    if std::env::args().any(|a| a == "--ingest-only") {
        std::process::exit(ingest_once());
    }
    if !std::env::args().any(|a| a == "--snapshot") {
        std::process::exit(web::serve(config::Config::from_env()));
    }
    let claude_root = env_path("ALCOVE_CLAUDE_ROOT", home().join(".claude/projects"));
    let codex_root = env_path("ALCOVE_CODEX_ROOT", home().join(".codex/sessions"));

    let mut out: Vec<CanonSession> = Vec::new();

    for s in claude::collect(&claude_root, &cache::ScanCache::default()) {
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

    for s in codex::collect(&codex_root, &cache::ScanCache::default()) {
        let mut subs: Vec<CanonSub> = s
            .subagents
            .into_iter()
            .map(|a| CanonSub {
                id: a.id,
                label: a.label,
                model: a.model,
                role: a.role,
                // From the spawn edge in Codex's own sqlite — the only place
                // open/closed is written down.
                status: a.status,
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
            branch: s.branch,
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

/// One ingest pass, for cron. Idempotent: re-running on overlapping data is a
/// no-op, which is the whole point of keying every fact on a natural id.
fn ingest_once() -> i32 {
    let cfg = config::Config::from_env();
    let snapshot = collect::Collector::new(cfg).collect();
    let mut conn = match store::connect(true) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("store: {e}");
            return 1;
        }
    };
    println!("store: {}", store::db_path().display());

    // The spool is a second, independent input: hooks record tool calls, which
    // no transcript reliably carries.
    let spooled = spool::read_all();
    match spooled.files {
        // "The hooks have not run" is a fact, not a failure — but it is also not
        // "there were no tool calls", so say which one it is.
        None => println!("spool: {} (absent)", spooled.dir.display()),
        Some(n) => println!(
            "spool: {} ({n} file{}, {} events, {} skipped)",
            spooled.dir.display(),
            if n == 1 { "" } else { "s" },
            spooled.calls.len(),
            spooled.skipped
        ),
    }
    for err in &spooled.errors {
        eprintln!("spool: could not read {err}");
    }

    // Two independent inputs, so they get two independent attempts: a snapshot
    // that will not ingest must not silently take the spool down with it. Found
    // the hard way — the live store predates `turn`'s (id, thread_id) key, so its
    // snapshot ingest fails outright, and a nested attempt would have discarded
    // every tool call for a reason that has nothing to do with the spool.
    let snapshot_result = store::ingest(&mut conn, &snapshot);
    let tool_result = store::ingest_tool_calls(&mut conn, &spooled.calls);

    let mut status = 0;
    let tool_call_new = match &tool_result {
        Ok(n) => n.to_string(),
        Err(e) => {
            eprintln!("tool_call ingest failed: {e}");
            status = 1;
            // Never "0": that reads as "there was nothing to write".
            "failed".to_string()
        }
    };
    match &snapshot_result {
        Ok(c) => println!(
            "  changed:  turn_new={}, selection_new={}, compaction_new={}, \
observation_new={}, session_seen={}, subagent_seen={}, tool_call_new={}, \
spool_skipped={}",
            c.turn_new, c.selection_new, c.compaction_new, c.observation_new,
            c.session_seen, c.subagent_seen, tool_call_new, spooled.skipped
        ),
        Err(e) => {
            eprintln!("ingest failed: {e}");
            status = 1;
            println!(
                "  changed:  snapshot ingest FAILED, tool_call_new={}, spool_skipped={}",
                tool_call_new, spooled.skipped
            );
        }
    }
    if status != 0 {
        return status;
    }
    if let Ok(t) = store::totals(&conn) {
        println!(
            "  lifetime: {} turns, {} output tokens, {} sessions",
            t["turns"], t["output"], t["sessions"]
        );
        println!("  span:     {} .. {}", t["first_ts"], t["last_ts"]);
    }
    0
}
