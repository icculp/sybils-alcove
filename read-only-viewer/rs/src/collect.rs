//! Assembling one snapshot across every source. Port of `alcove/collect.py`.
//!
//! The only place that decides session *state*, because that decision needs both
//! a transcript fact (was it written recently) and a process fact (does a pid own
//! it), and those come from different sources.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::cache::ScanCache;
use crate::config::Config;
use crate::liveness::{self, Fold};
use crate::{claude, codex, process, spool};

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
    let rem = secs % 86400;
    let (y, m, d) = spool::utc_ymd(secs as i64);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// One subagent's state, and where the answer came from.
///
/// The fold answers or it does not. When it does not, the age window answers and
/// the third element says so — because the whole failure this replaces was a
/// guess that rendered exactly like a fact. Note what inference is NOT allowed to
/// say: `stopped`. A quiet transcript means "no recent write", which covers a
/// finished child and an abandoned one alike, so it renders `idle`. Only a
/// `subagent_stop` event ever produces `stopped`.
fn sub_state(
    verdict: Option<liveness::Verdict>,
    fresh_transcript: bool,
) -> (&'static str, Option<String>, bool) {
    match verdict {
        Some(v) => (v.state, v.stopped_at, false),
        None if fresh_transcript => ("running", None, true),
        None => ("idle", None, true),
    }
}

fn codex_sub_state(
    status: &str,
    verdict: Option<liveness::Verdict>,
    fresh_transcript: bool,
) -> (&'static str, Option<String>, bool) {
    if matches!(status, "closed" | "completed" | "failed") {
        return ("idle", None, false);
    }
    sub_state(verdict, fresh_transcript)
}

fn age_s(path: &PathBuf) -> Option<f64> {
    let modified = path.metadata().ok()?.modified().ok()?;
    let now = SystemTime::now();
    now.duration_since(modified).ok().map(|d| d.as_secs_f64())
}

/// How many UTC days of spool the state fold reads. Two, so a session that
/// started before midnight still has its start on record next to its stop.
const SPOOL_DAYS: i64 = 2;

/// The pid map, and the one rule about refreshing it: never on the path that a
/// browser is waiting on.
///
/// `claude agents --json --all` costs ~560 ms — more than every other part of a
/// warm collect put together. Measured on the push path: an append reached the
/// browser in 205 ms when the map was warm and **1,035 ms** when the TTL had just
/// expired, because the collect stopped to run a subprocess. Same cadence, wrong
/// thread.
///
/// So an expired map is SERVED while a refresh runs behind it: at most one
/// lookup in flight, and the answer is at most one TTL plus one lookup old. The
/// first call is still synchronous, because "no answer yet" would render as "no
/// pids", which reads as "nothing is running" — the exact failure this codebase
/// has already paid for once.
#[derive(Default)]
struct Procs {
    inner: Mutex<Option<(Instant, std::collections::HashMap<String, process::Proc>, String)>>,
    refreshing: std::sync::atomic::AtomicBool,
}

impl Procs {
    fn store(&self, map: std::collections::HashMap<String, process::Proc>, status: String) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some((Instant::now(), map, status));
        }
    }

    fn get(
        self: &Arc<Self>,
        ttl: Duration,
    ) -> (std::collections::HashMap<String, process::Proc>, String) {
        let cached = self.inner.lock().ok().and_then(|g| g.clone());
        match cached {
            Some((at, map, status)) => {
                if at.elapsed() >= ttl
                    && !self.refreshing.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    let procs = Arc::clone(self);
                    // Detached on purpose: nothing waits on this, and a lookup
                    // that hangs must not hold a collect open behind it.
                    let _ = std::thread::Builder::new()
                        .name("alcove-pids".into())
                        .spawn(move || {
                            let (map, status) = process::running_pids();
                            procs.store(map, status);
                            procs.refreshing.store(false, std::sync::atomic::Ordering::SeqCst);
                        });
                }
                (map, status)
            }
            // Cold: answer honestly, even though it costs the subprocess.
            None => {
                let (map, status) = process::running_pids();
                self.store(map.clone(), status.clone());
                (map, status)
            }
        }
    }
}

pub struct Collector {
    claude_cache: ScanCache<claude::Scan>,
    codex_cache: ScanCache<codex::Scan>,
    /// The hook spool, stat-cached like the transcripts. Separate from them
    /// because it is a different producer with a different growth pattern: one
    /// file per harness per day, appended to constantly.
    spool_cache: spool::SpoolCache,
    /// The pid lookup shells out to the `claude` CLI at ~560 ms. Once transcript
    /// scanning is cached that subprocess IS the refresh cost, so it gets its own
    /// longer TTL rather than running on every poll — and, once a push path
    /// exists, its own thread rather than the caller's.
    procs: Arc<Procs>,
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
            spool_cache: spool::SpoolCache::default(),
            procs: Arc::default(),
            snapshot: Mutex::new(None),
            refresh: Mutex::new(()),
            paths: Mutex::new(std::collections::HashMap::new()),
            cfg,
        }
    }

    fn processes(&self) -> (std::collections::HashMap<String, process::Proc>, String) {
        self.procs.get(Duration::from_secs_f64(self.cfg.pid_ttl_s))
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
        self.store_collect()
    }

    /// Collect NOW, whatever the cache says, and store the result.
    ///
    /// This is the push path's entry point: a file changed, so the cached
    /// snapshot is known stale and serving it would defeat the whole point of
    /// having watched the file. Still single-flighted — a burst of watch events
    /// must not stack collects — and it stores its result, so the refetch that
    /// follows the change signal is served from cache rather than starting a
    /// second scan.
    pub fn refresh(&self) -> Value {
        let _flight = self.refresh.lock();
        self.store_collect()
    }

    fn store_collect(&self) -> Value {
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

        // State from the hooks: authoritative transitions, folded latest-wins.
        // Where it has nothing to say the age window still answers, LABELLED as
        // inference — the two must never render the same.
        let spooled = spool::read_window(&spool::spool_dir(), SPOOL_DAYS, &self.spool_cache);
        let fold = Fold::new(&spooled.calls);
        let now = now_ms();
        let window_ms = (self.cfg.live_window_s * 1000.0) as i64;

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
            // The TURN, which is not the session: `stop` says the harness finished
            // answering, and says nothing about whether the process is still there
            // waiting for the next prompt. That is what makes it the honest source
            // for `quiet`, where the five-minute window was previously guessing.
            let turn = fold.session(&s.session_id, Some(&s.last_ts), now, window_ms);
            let quiet = match &turn {
                Some(v) => state == "running" && v.state == "stopped",
                None => state == "running" && !live,
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
                    let fresh = sub_age.map(|x| x < self.cfg.live_window_s).unwrap_or(false);
                    // Authoritative first, age window second, and the JSON says
                    // which one answered.
                    let last = if a.last_ts.is_empty() { None } else { Some(a.last_ts.as_str()) };
                    let (sub_state, stopped_at, inferred) =
                        sub_state(fold.child(&a.id, last, now, window_ms), fresh);
                    let running = sub_state == "running";
                    // A child with no spawn record has no `role`; the spool saw its
                    // `agent_type` first-hand, so use that rather than render blank.
                    let role = if a.role.is_empty() {
                        fold.agent_type(&a.id).unwrap_or_default().to_string()
                    } else {
                        a.role.clone()
                    };
                    if !a.no_transcript {
                        index.insert(
                            (s.session_id.clone(), a.id.clone()),
                            (sub_path.clone(), "claude".into(), json!({
                                "session_id": s.session_id, "agent_id": a.id,
                                "label": a.label, "model": a.model, "cwd": s.cwd,
                                "project": s.project, "role": role, "task": a.task,
                                "state": if running { "running" } else { "" },
                            })),
                        );
                        live_paths.push(sub_path);
                    }
                    json!({
                        "id": a.id, "label": a.label, "model": a.model,
                        // A child transcript is a full transcript, so its effort
                        // is observed, not inherited from the parent.
                        "effort": a.effort, "effort_timeline": a.effort_timeline,
                        "version": a.version, "version_timeline": a.version_timeline,
                        "record_model": a.record_model, "role": role,
                        "status": a.status, "turns": a.turns, "usage": a.usage,
                        "reported_tokens": Value::Null, "tool_uses": Value::Null,
                        "task": a.task, "size": a.size,
                        "age_s": sub_age,
                        "live": running,
                        "state": sub_state, "stopped_at": stopped_at,
                        "inferred": inferred,
                        "no_transcript": a.no_transcript,
                        "timeline": Vec::<Value>::new(),
                        "turn_rows": a.turn_rows,
                    })
                })
                .collect();
            // Running subagents first, then freshest (Python's live_first,
            // claude.py:207). The Codex source sorts before collect because it
            // owns age/live; for Claude both are only known here.
            //
            // `live` is now the FOLDED state, not a file-age test, so this sorts
            // on what the harness said: a child that stopped ten seconds ago sinks
            // immediately instead of holding the top of the table for five
            // minutes. Age still breaks ties, and still comes from the file.
            subs.sort_by(|a, b| {
                let key = |v: &Value| {
                    (!v["live"].as_bool().unwrap_or(false), v["age_s"].as_f64().unwrap_or(1e18))
                };
                key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal)
            });
            out.push(json!({
                "harness": "claude", "session_id": s.session_id, "label": s.label,
                "project": s.project, "cwd": s.cwd, "branch": s.branch,
                "effort": s.effort, "effort_timeline": s.effort_timeline,
                "version": s.version, "version_timeline": s.version_timeline,
                "model": s.model,
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
                "quiet": quiet,
                // Whether that came from a `stop` event or from file age.
                "quiet_inferred": turn.is_none(),
                "turn_state": turn.as_ref().map(|v| v.state),
                "turn_stopped_at": turn.as_ref().and_then(|v| v.stopped_at.clone()),
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
            let turn = fold.session(&s.session_id, Some(&s.last_ts), now, window_ms);
            let subs: Vec<Value> = s
                .subagents
                .iter()
                .map(|a| {
                    // The same fold, on the same terms: Codex's hooks are wired
                    // but unacknowledged, so today this always declines and the
                    // age window answers — labelled inferred. It starts telling
                    // the truth the moment someone trusts the hooks in the TUI,
                    // with no code change here.
                    let (sub_state, stopped_at, inferred) = codex_sub_state(
                        &a.status,
                        fold.child(&a.id, None, now, window_ms),
                        a.live,
                    );
                    json!({
                        "id": a.id, "label": a.label, "model": a.model,
                        "effort": a.effort, "effort_timeline": a.effort_timeline,
                        "version": a.version, "version_timeline": Vec::<Value>::new(),
                        "record_model": "", "role": a.role, "status": a.status,
                        "nickname": a.nickname,
                        "turns": a.turns, "usage": a.usage,
                        "reported_tokens": if a.usage.output > 0 {
                            json!(a.usage.output)
                        } else {
                            Value::Null
                        },
                        "tool_uses": Value::Null, "task": a.task,
                        "size": a.size, "age_s": a.age_s,
                        "live": sub_state == "running",
                        "state": sub_state, "stopped_at": stopped_at,
                        "inferred": inferred,
                        "no_transcript": false, "timeline": Vec::<Value>::new(),
                        "turn_rows": a.turn_rows,
                    })
                })
                .collect();
            out.push(json!({
                "harness": "codex", "session_id": s.session_id, "label": s.label,
                "project": s.project, "cwd": s.cwd, "branch": s.branch,
                "nickname": s.nickname,
                "effort": s.effort, "effort_timeline": s.effort_timeline,
                // Codex has one version per rollout and no per-turn record, so
                // the trace is EMPTY rather than a one-entry timeline pretending
                // to be one. See codex.rs on `multi_agent_version`.
                "version": s.version, "version_timeline": Vec::<Value>::new(),
                "model": s.model,
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
                "quiet": false, "quiet_inferred": turn.is_none(),
                "turn_state": turn.as_ref().map(|v| v.state),
                "turn_stopped_at": turn.as_ref().and_then(|v| v.stopped_at.clone()),
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
        let (sh, sm) = self.spool_cache.stats();
        json!({
            "generated_at": iso_now(),
            "live_window_s": self.cfg.live_window_s,
            "tail_lines": crate::transcripts::TAIL_LINES,
            "pid_source": pid_source,
            "claude_bin": process::claude_bin(),
            "codex_processes": process::codex_process_count(),
            "scan_cache": {"hits": ch + xh, "misses": cm + xm},
            // How much the page is allowed to trust its own state labels. An
            // absent spool is a real answer ("the hooks have not run"), and is
            // not the same as one that would not open.
            "spool": {
                "dir": spooled.dir.to_string_lossy(),
                "files": spooled.files,
                "days": SPOOL_DAYS,
                "events": spooled.calls.len(),
                "skipped": spooled.skipped,
                "errors": spooled.errors,
                "sessions_covered": fold.sessions_covered(),
                "subagents_covered": fold.children_covered(),
                "cache": {"hits": sh, "misses": sm},
            },
            "sessions": out,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::codex_sub_state;

    #[test]
    fn terminal_codex_status_outranks_freshness() {
        assert_eq!(codex_sub_state("closed", None, true), ("idle", None, false));
        assert_eq!(codex_sub_state("failed", None, true), ("idle", None, false));
    }

    #[test]
    fn open_codex_status_still_uses_freshness() {
        assert_eq!(codex_sub_state("open", None, true), ("running", None, true));
    }
}
