//! Assembling one snapshot across every source. Port of `alcove/collect.py`.
//!
//! The only place that decides session *state*, because that decision needs both
//! a transcript fact (was it written recently) and a process fact (does a pid own
//! it), and those come from different sources.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::cache::ScanCache;
use crate::config::Config;
use crate::{claude, codex, process};

/// State outranks file age: a running session whose transcript has been quiet
/// for a day still belongs above a finished one that wrote a minute ago.
fn rank(state: &str) -> u8 {
    match state {
        "running" => 0,
        "writing" => 1,
        "unknown" => 2,
        "ended" => 3,
        _ => 9,
    }
}

/// ISO-8601 UTC, matching what the Python wrote.
///
/// The store compares these against sqlite `date('now', ...)`, so a unix integer
/// here would silently make every date filter match nothing.
fn iso_now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // Civil date from days since epoch (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

fn age_s(path: &PathBuf) -> Option<f64> {
    let modified = path.metadata().ok()?.modified().ok()?;
    let now = SystemTime::now();
    now.duration_since(modified).ok().map(|d| d.as_secs_f64())
}

pub struct Collector {
    claude_cache: ScanCache<claude::Scan>,
    codex_cache: ScanCache<codex::Scan>,
    /// The pid lookup shells out to the `claude` CLI at ~560 ms. Once transcript
    /// scanning is cached that subprocess IS the refresh cost, so it gets its own
    /// longer TTL rather than running on every poll.
    procs: Mutex<Option<(Instant, std::collections::HashMap<String, process::Proc>, String)>>,
    /// (stored_at, value, how long the collect COST)
    snapshot: Mutex<Option<(Instant, Value, Duration)>>,
    /// Single-flight. Without it every waiting request starts its own collect:
    /// with a 2 s TTL, a 3 s poll and a 5 s collect, each poll missed the cache
    /// and launched another scan while the previous ones were still running.
    /// They contended, each got slower, and one request measured 59.7 s.
    refresh: Mutex<()>,
    /// (session_id, agent_id) -> (transcript path, harness, display meta).
    ///
    /// The client sends IDS, never paths. Resolving through a map the collector
    /// itself built means an unknown id is simply absent rather than a
    /// filesystem read, so no request can reach a file the collector did not
    /// already choose to open.
    paths: Mutex<std::collections::HashMap<(String, String), (PathBuf, String, Value)>>,
    cfg: Config,
}

impl Collector {
    pub fn new(cfg: Config) -> Self {
        Self {
            claude_cache: ScanCache::default(),
            codex_cache: ScanCache::default(),
            procs: Mutex::new(None),
            snapshot: Mutex::new(None),
            refresh: Mutex::new(()),
            paths: Mutex::new(std::collections::HashMap::new()),
            cfg,
        }
    }

    fn processes(&self) -> (std::collections::HashMap<String, process::Proc>, String) {
        let ttl = Duration::from_secs_f64(self.cfg.pid_ttl_s);
        if let Ok(guard) = self.procs.lock() {
            if let Some((at, map, status)) = guard.as_ref() {
                if at.elapsed() < ttl {
                    return (map.clone(), status.clone());
                }
            }
        }
        let (map, status) = process::running_pids();
        if let Ok(mut guard) = self.procs.lock() {
            *guard = Some((Instant::now(), map.clone(), status.clone()));
        }
        (map, status)
    }

    /// Serve the cache for at least as long as the last collect TOOK.
    ///
    /// A fixed TTL shorter than the collect means the scan never stops running:
    /// the result is stale before it is stored. Scaling the window to the
    /// observed cost keeps the duty cycle bounded however large the corpus gets.
    fn fresh(&self) -> Option<Value> {
        let floor = Duration::from_secs_f64(self.cfg.cache_ttl_s);
        let guard = self.snapshot.lock().ok()?;
        let (at, value, cost) = guard.as_ref()?;
        if at.elapsed() < floor.max(*cost) {
            Some(value.clone())
        } else {
            None
        }
    }

    pub fn cached(&self) -> Value {
        if let Some(value) = self.fresh() {
            return value;
        }
        // One collect at a time. Everyone else waits here and re-checks, so a
        // queue of waiting requests costs one scan rather than one scan each.
        let _flight = self.refresh.lock();
        if let Some(value) = self.fresh() {
            return value;
        }
        let started = Instant::now();
        let value = self.collect();
        let cost = started.elapsed();
        if let Ok(mut guard) = self.snapshot.lock() {
            *guard = Some((Instant::now(), value.clone(), cost));
        }
        value
    }

    /// Resolve a session/agent id pair to a transcript, for spillout.
    pub fn resolve(&self, session: &str, agent: &str) -> Option<(PathBuf, String, Value)> {
        // Ensure the index exists even if nothing has hit /api/sessions yet.
        if self.paths.lock().map(|g| g.is_empty()).unwrap_or(true) {
            let _ = self.cached();
        }
        self.paths.lock().ok()?.get(&(session.to_string(), agent.to_string())).cloned()
    }

    pub fn collect(&self) -> Value {
        let (pids, pid_source) = self.processes();
        let claude_sessions = claude::collect(&self.cfg.claude_root, &self.claude_cache);
        let codex_sessions = codex::collect(&self.cfg.codex_root, &self.codex_cache);

        let mut live_paths: Vec<PathBuf> = Vec::new();
        let _ = &live_paths;
        let mut index: std::collections::HashMap<(String, String), (PathBuf, String, Value)> =
            std::collections::HashMap::new();
        let mut out: Vec<Value> = Vec::new();

        for s in &claude_sessions {
            live_paths.push(s.path.clone());
            index.insert(
                (s.session_id.clone(), String::new()),
                (s.path.clone(), "claude".into(), json!({
                    "session_id": s.session_id, "agent_id": "", "label": s.label,
                    "model": s.model, "cwd": s.cwd, "project": s.project,
                })),
            );
            let age = age_s(&s.path);
            let live = age.map(|a| a < self.cfg.live_window_s).unwrap_or(false);
            let proc = pids.get(&s.session_id);
            let pid_list: Vec<i64> = proc.map(|p| p.pids.clone()).unwrap_or_default();
            // Four distinct facts, never collapsed into one "live" flag.
            let state = if !pid_list.is_empty() {
                "running"
            } else if pid_source != "ok" {
                // A failed lookup proves nothing about absence.
                "unknown"
            } else if live {
                "writing"
            } else {
                "ended"
            };
            let mut subs: Vec<Value> = s
                .subagents
                .iter()
                .map(|a| {
                    let sub_path = self
                        .cfg
                        .claude_root
                        .join(&s.project)
                        .join(&s.session_id)
                        .join("subagents")
                        .join(format!("agent-{}.jsonl", a.id));
                    let sub_age = if a.no_transcript { None } else { age_s(&sub_path) };
                    if !a.no_transcript {
                        index.insert(
                            (s.session_id.clone(), a.id.clone()),
                            (sub_path.clone(), "claude".into(), json!({
                                "session_id": s.session_id, "agent_id": a.id,
                                "label": a.label, "model": a.model, "cwd": s.cwd,
                                "project": s.project, "role": a.role, "task": a.task,
                                "state": if sub_age.map(|x| x < self.cfg.live_window_s)
                                    .unwrap_or(false) { "running" } else { "" },
                            })),
                        );
                        live_paths.push(sub_path);
                    }
                    json!({
                        "id": a.id, "label": a.label, "model": a.model,
                        "record_model": a.record_model, "role": a.role,
                        "status": a.status, "turns": a.turns, "usage": a.usage,
                        "reported_tokens": Value::Null, "tool_uses": Value::Null,
                        "task": a.task, "size": a.size,
                        "age_s": sub_age,
                        "live": sub_age.map(|x| x < self.cfg.live_window_s).unwrap_or(false),
                        "no_transcript": a.no_transcript,
                        "timeline": Vec::<Value>::new(),
                        "turn_rows": a.turn_rows,
                    })
                })
                .collect();
            // Running subagents first, then freshest (Python's live_first,
            // claude.py:207). The Codex source sorts before collect because it
            // owns age/live; for Claude both are only known here.
            subs.sort_by(|a, b| {
                let key = |v: &Value| {
                    (!v["live"].as_bool().unwrap_or(false), v["age_s"].as_f64().unwrap_or(1e18))
                };
                key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal)
            });
            out.push(json!({
                "harness": "claude", "session_id": s.session_id, "label": s.label,
                "project": s.project, "cwd": s.cwd, "branch": s.branch,
                "effort": s.effort, "model": s.model,
                "selected_model": s.selected_model, "selections": s.selections,
                "timeline": s.timeline, "usage": s.usage, "turns": s.turns,
                "last_ts": s.last_ts, "age_s": age, "live": live,
                "compactions": s.compactions,
                "usage_since_compact": Value::Null, "turns_since_compact": Value::Null,
                "subagents": subs, "path": s.path.to_string_lossy(),
                "pids": pid_list,
                "agent_name": proc.map(|p| p.name.clone()).unwrap_or_default(),
                "kind": proc.map(|p| p.kind.clone()).unwrap_or_default(),
                "switches": (s.timeline.len().max(1) - 1),
                "turn_rows": s.turn_rows,
                "state": state, "state_inferred": false,
                // A process can own a session for a day without the model
                // writing a word: "running" means the window is open, not that
                // work is happening.
                "quiet": state == "running" && !live,
            }));
        }

        for s in &codex_sessions {
            live_paths.push(s.path.clone());
            index.insert(
                (s.session_id.clone(), String::new()),
                (s.path.clone(), "codex".into(), json!({
                    "session_id": s.session_id, "agent_id": "", "label": s.label,
                    "model": s.model, "cwd": s.cwd, "project": s.project,
                })),
            );
            let age = age_s(&s.path);
            let live = age.map(|a| a < self.cfg.live_window_s).unwrap_or(false);
            // Codex has no per-session pid, so transcript freshness is the only
            // signal available — marked inferred rather than implying certainty.
            let state = if live { "writing" } else { "ended" };
            let subs: Vec<Value> = s
                .subagents
                .iter()
                .map(|a| {
                    json!({
                        "id": a.id, "label": a.label, "model": a.model,
                        "record_model": "", "role": a.role, "status": a.status,
                        "nickname": a.nickname,
                        "turns": a.turns, "usage": a.usage,
                        "reported_tokens": if a.usage.output > 0 {
                            json!(a.usage.output)
                        } else {
                            Value::Null
                        },
                        "tool_uses": Value::Null, "task": a.task,
                        "size": a.size, "age_s": a.age_s, "live": a.live,
                        "no_transcript": false, "timeline": Vec::<Value>::new(),
                        "turn_rows": a.turn_rows,
                    })
                })
                .collect();
            out.push(json!({
                "harness": "codex", "session_id": s.session_id, "label": s.label,
                "project": s.project, "cwd": s.cwd, "branch": s.branch,
                "nickname": s.nickname,
                "effort": s.effort, "model": s.model,
                "selected_model": "", "selections": Vec::<Value>::new(),
                "timeline": s.timeline, "usage": s.usage, "turns": s.turns,
                "last_ts": s.last_ts, "age_s": age, "live": live,
                "compactions": s.compactions,
                "usage_since_compact": Value::Null, "turns_since_compact": Value::Null,
                "subagents": subs, "path": s.path.to_string_lossy(),
                "pids": Vec::<i64>::new(), "agent_name": "", "kind": "",
                "switches": (s.timeline.len().max(1) - 1),
                "turn_rows": s.turn_rows,
                "state": state, "state_inferred": true,
                "quiet": false,
            }));
        }

        out.sort_by(|a, b| {
            let ka = rank(a["state"].as_str().unwrap_or(""));
            let kb = rank(b["state"].as_str().unwrap_or(""));
            let aa = a["age_s"].as_f64().unwrap_or(1e18);
            let ab = b["age_s"].as_f64().unwrap_or(1e18);
            ka.cmp(&kb).then(aa.partial_cmp(&ab).unwrap_or(std::cmp::Ordering::Equal))
        });

        // Drop cache entries for transcripts that no longer exist.
        if let Ok(mut guard) = self.paths.lock() {
            *guard = index;
        }
        self.claude_cache.evict_missing();
        self.codex_cache.evict_missing();

        let (ch, cm) = self.claude_cache.stats();
        let (xh, xm) = self.codex_cache.stats();
        let now = iso_now();
        json!({
            "generated_at": now,
            "live_window_s": self.cfg.live_window_s,
            "tail_lines": crate::transcripts::TAIL_LINES,
            "pid_source": pid_source,
            "claude_bin": process::claude_bin(),
            "codex_processes": process::codex_process_count(),
            "scan_cache": {"hits": ch + xh, "misses": cm + xm},
            "sessions": out,
        })
    }
}
