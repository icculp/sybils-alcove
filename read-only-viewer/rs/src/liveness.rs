//! What the stop events MEAN: state folded from the spool, not from file age.
//!
//! The viewer used to answer "is this subagent still working?" with "was its
//! transcript written in the last five minutes". That is wrong for minutes at a
//! time in the one direction that matters: a child that finished ten seconds ago
//! keeps rendering as running until its file ages out of the window.
//!
//! The hooks emit an authoritative transition instead — `subagent_stop` names the
//! child in `target`, `stop` ends a session's turn — so state here is a FOLD over
//! those transitions in timestamp order, latest wins. A stop is not a tombstone:
//! a later event for the same child means it resumed, and the fold says running
//! again.
//!
//! ## Three things this deliberately refuses to do
//!
//! - **Claim coverage it does not have.** A child that ran before the hooks were
//!   wired, or older than the spool window, has NO transition on record. The fold
//!   returns `None` and the caller falls back to the age window, labelled
//!   `inferred`. "I could not ask" and "the answer is no" must look different.
//! - **Believe a running state forever.** A harness killed mid-turn writes no
//!   stop event. So an activity-latest fold that has gone quiet for longer than
//!   the live window stops claiming "running" and hands the question back to
//!   inference, rather than pinning a dead child green until midnight.
//! - **Attribute a child's work to its parent.** A child's tool call carries the
//!   PARENT's `session_id` (verified — see `hooks/README.md`), and a background
//!   child outlives its parent's turn: the parent's `stop` lands while the child
//!   is still working. Child activity therefore never counts as session activity.
//!
//! ## The one clock subtlety
//!
//! Transcript timestamps and spool timestamps are both harness-written UTC, so
//! they are directly comparable — which is why this compares against the
//! transcript's own `last_ts` and never against its mtime. They are not, however,
//! written at the same instant: measured on a real subagent, the child's last
//! transcript event was `14:50:13.009Z` and its `SubagentStop` fired 55 ms later
//! at `14:50:13.064Z`. So "the transcript moved after the stop" needs a margin
//! wider than that jitter, or every stopped child would immediately re-read as
//! resumed.

use std::collections::HashMap;

use crate::spool::ToolCall;

/// How far past a stop a transcript write must land to count as a resumption.
/// Measured jitter between a child's last event and its stop hook is ~55 ms; one
/// second is two orders of margin and still far below any real resume.
const RESUME_MARGIN_MS: i64 = 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A tool call: this agent was doing something at this instant.
    Activity,
    /// `stop` / `subagent_stop`: the harness says this agent's turn ended.
    Stopped,
}

#[derive(Clone, Debug)]
pub struct Latest {
    pub kind: Kind,
    pub at: String,
    pub at_ms: i64,
}

/// The answer for one agent, when there is one.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    /// `"running"` or `"stopped"` — never a guess. `inferred` is always false
    /// here; the caller sets its own label when the fold declines to answer.
    pub state: &'static str,
    /// When the stop happened. Present only for `stopped`, because a running
    /// agent has no stop time and a zero would read as "stopped at the epoch".
    pub stopped_at: Option<String>,
    /// The latest thing seen for this agent, whatever it was.
    pub last_at: String,
}

/// Parse `2026-07-30T14:50:13.064Z` (and the second-precision variant the
/// transcripts sometimes carry) to epoch milliseconds.
///
/// Hand-rolled rather than pulling in a date crate: both producers write UTC with
/// a literal `Z`, so there is no zone arithmetic to get wrong, and the alternative
/// is a dependency for eleven lines.
pub fn ts_ms(iso: &str) -> Option<i64> {
    let bytes = iso.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let num = |a: usize, b: usize| -> Option<i64> { iso.get(a..b)?.parse().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // Days from civil (Howard Hinnant), the inverse of spool::utc_ymd.
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let mut ms = ((days * 86400) + h * 3600 + mi * 60 + s) * 1000;
    if bytes.len() > 20 && bytes[19] == b'.' {
        let frac: String = iso[20..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if !frac.is_empty() {
            // Pad or clip to milliseconds: `.5Z` is 500 ms, `.123456Z` is 123.
            let mut millis = frac.clone();
            millis.truncate(3);
            while millis.len() < 3 {
                millis.push('0');
            }
            ms += millis.parse::<i64>().unwrap_or(0);
        }
    }
    Some(ms)
}

/// Every transition the spool window holds, keyed by who it is about.
#[derive(Default, Debug)]
pub struct Fold {
    sessions: HashMap<String, Latest>,
    children: HashMap<String, Latest>,
    /// `agent_id` -> `agent_type`, for a child whose parent spawn record is
    /// missing. The transcript is the better source when it exists.
    types: HashMap<String, String>,
    pub events: usize,
}

fn keep(slot: &mut HashMap<String, Latest>, key: &str, latest: Latest) {
    match slot.get(key) {
        // Ties go to the later insertion, which is file order — the spool is
        // append-ordered, so within one millisecond the last line written wins.
        Some(prev) if prev.at_ms > latest.at_ms => {}
        _ => {
            slot.insert(key.to_string(), latest);
        }
    }
}

impl Fold {
    pub fn new(calls: &[ToolCall]) -> Self {
        let mut fold = Fold::default();
        for call in calls {
            let Some(at_ms) = ts_ms(&call.ts) else { continue };
            let latest = |kind| Latest { kind, at: call.ts.clone(), at_ms };
            if let Some(t) = call.agent_type.as_deref() {
                if let Some(id) = call.agent_id.as_deref().or(call.target.as_deref()) {
                    if !id.is_empty() && !t.is_empty() {
                        fold.types.insert(id.to_string(), t.to_string());
                    }
                }
            }
            match call.event.as_str() {
                "pre" | "post" => match call.agent_id.as_deref() {
                    // A child's own tool call. Its `session_id` is the parent's,
                    // so this must NOT count as session activity.
                    Some(child) if !child.is_empty() => {
                        keep(&mut fold.children, child, latest(Kind::Activity))
                    }
                    _ => keep(&mut fold.sessions, &call.session_id, latest(Kind::Activity)),
                },
                // The child is named in `target` per the frozen contract; the
                // additive `agent_id` is accepted as a fallback so a future
                // producer that only sets that field still folds.
                "subagent_stop" => {
                    let child = call
                        .target
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .or(call.agent_id.as_deref())
                        .unwrap_or("");
                    if !child.is_empty() {
                        keep(&mut fold.children, child, latest(Kind::Stopped));
                    }
                }
                "stop" => keep(&mut fold.sessions, &call.session_id, latest(Kind::Stopped)),
                _ => {}
            }
            fold.events += 1;
        }
        fold
    }

    pub fn agent_type(&self, agent_id: &str) -> Option<&str> {
        self.types.get(agent_id).map(|s| s.as_str())
    }

    pub fn sessions_covered(&self) -> usize {
        self.sessions.len()
    }

    pub fn children_covered(&self) -> usize {
        self.children.len()
    }

    /// The state of one child, or `None` when the spool cannot say.
    ///
    /// `transcript_last_ts` is the child transcript's own last event timestamp —
    /// the only thing that can prove a resumption before the resumed child gets
    /// as far as its first tool call.
    pub fn child(
        &self,
        agent_id: &str,
        transcript_last_ts: Option<&str>,
        now_ms: i64,
        live_window_ms: i64,
    ) -> Option<Verdict> {
        self.verdict(self.children.get(agent_id)?, transcript_last_ts, now_ms, live_window_ms)
    }

    /// The state of one session's TURN. Not its liveness: a process can own an
    /// idle session for a day, and only the pid can speak to that.
    pub fn session(
        &self,
        session_id: &str,
        transcript_last_ts: Option<&str>,
        now_ms: i64,
        live_window_ms: i64,
    ) -> Option<Verdict> {
        self.verdict(self.sessions.get(session_id)?, transcript_last_ts, now_ms, live_window_ms)
    }

    fn verdict(
        &self,
        latest: &Latest,
        transcript_last_ts: Option<&str>,
        now_ms: i64,
        live_window_ms: i64,
    ) -> Option<Verdict> {
        let wrote_ms = transcript_last_ts.and_then(ts_ms);
        match latest.kind {
            Kind::Stopped => {
                // A stop is a transition. If the transcript moved AFTER it — by
                // more than the hook's own jitter — the agent resumed.
                if let Some(wrote) = wrote_ms {
                    if wrote > latest.at_ms + RESUME_MARGIN_MS {
                        return Some(Verdict {
                            state: "running",
                            stopped_at: None,
                            last_at: transcript_last_ts.unwrap_or_default().to_string(),
                        });
                    }
                }
                Some(Verdict {
                    state: "stopped",
                    stopped_at: Some(latest.at.clone()),
                    last_at: latest.at.clone(),
                })
            }
            Kind::Activity => {
                // No stop on record. That is only evidence of "still running"
                // while the agent is recently ACTIVE — a harness killed mid-turn
                // never writes its stop, and pinning it running until midnight
                // would be the same lie the age window told, just louder.
                let seen = wrote_ms.unwrap_or(latest.at_ms).max(latest.at_ms);
                if now_ms - seen > live_window_ms {
                    return None;
                }
                Some(Verdict { state: "running", stopped_at: None, last_at: latest.at.clone() })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(event: &str, ts: &str, session: &str, agent: Option<&str>) -> ToolCall {
        ToolCall {
            v: 1,
            ts: ts.into(),
            harness: "claude".into(),
            event: event.into(),
            session_id: session.into(),
            tool: if event.starts_with("s") { String::new() } else { "Bash".into() },
            cwd: None,
            target: if event == "subagent_stop" { agent.map(|a| a.into()) } else { None },
            arg: None,
            ok: None,
            tool_use_id: None,
            agent_id: if event == "subagent_stop" { None } else { agent.map(|a| a.into()) },
            agent_type: agent.map(|_| "Explore".into()),
            agent_launchers: Vec::new(),
            params: None,
        }
    }

    const NOW: &str = "2026-07-30T15:00:00.000Z";
    fn now_ms() -> i64 {
        ts_ms(NOW).unwrap()
    }
    const WINDOW: i64 = 300_000;

    #[test]
    fn iso_parses_to_the_same_instant_python_reports() {
        // 2026-07-30T14:50:13.064Z, checked against `date -d ... +%s`.
        assert_eq!(ts_ms("2026-07-30T14:50:13.064Z"), Some(1785423013064));
        assert_eq!(ts_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(ts_ms("2026-07-30T14:50:13.5Z"), Some(1785423013500));
        assert_eq!(ts_ms("2026-07-30T14:50:13.123456Z"), Some(1785423013123));
        assert_eq!(ts_ms("nonsense"), None);
        assert_eq!(ts_ms(""), None);
    }

    #[test]
    fn a_stop_stops_the_child_and_names_the_time() {
        let calls = vec![
            call("pre", "2026-07-30T14:59:00.000Z", "sess", Some("child1")),
            call("subagent_stop", "2026-07-30T14:59:30.000Z", "sess", Some("child1")),
        ];
        let fold = Fold::new(&calls);
        let v = fold.child("child1", None, now_ms(), WINDOW).unwrap();
        assert_eq!(v.state, "stopped");
        assert_eq!(v.stopped_at.as_deref(), Some("2026-07-30T14:59:30.000Z"));
        assert_eq!(fold.agent_type("child1"), Some("Explore"));
    }

    #[test]
    fn activity_after_a_stop_is_a_resumption() {
        let calls = vec![
            call("subagent_stop", "2026-07-30T14:59:00.000Z", "sess", Some("child1")),
            call("pre", "2026-07-30T14:59:30.000Z", "sess", Some("child1")),
        ];
        let v = Fold::new(&calls).child("child1", None, now_ms(), WINDOW).unwrap();
        assert_eq!(v.state, "running");
        assert!(v.stopped_at.is_none(), "a running agent has no stop time");
    }

    #[test]
    fn a_transcript_write_after_the_stop_also_counts_as_resumed() {
        let calls = vec![call("subagent_stop", "2026-07-30T14:59:00.000Z", "sess", Some("c"))];
        let fold = Fold::new(&calls);
        // Inside the hook's own jitter: still stopped.
        let v = fold.child("c", Some("2026-07-30T14:59:00.055Z"), now_ms(), WINDOW).unwrap();
        assert_eq!(v.state, "stopped", "55 ms is the stop hook's own lag, not a resume");
        // Well past it: resumed.
        let v = fold.child("c", Some("2026-07-30T14:59:30.000Z"), now_ms(), WINDOW).unwrap();
        assert_eq!(v.state, "running");
    }

    #[test]
    fn a_stale_activity_fold_declines_to_answer() {
        // Last seen 20 minutes ago with no stop: the harness may have been killed.
        let calls = vec![call("pre", "2026-07-30T14:40:00.000Z", "sess", Some("c"))];
        assert!(
            Fold::new(&calls).child("c", None, now_ms(), WINDOW).is_none(),
            "no stop event plus no recent activity is not evidence of running"
        );
    }

    #[test]
    fn an_unknown_child_gets_no_verdict() {
        let fold = Fold::new(&[]);
        assert!(fold.child("never-seen", None, now_ms(), WINDOW).is_none());
        assert_eq!(fold.children_covered(), 0);
    }

    #[test]
    fn a_childs_work_is_not_its_parents_turn() {
        // The exact live shape: a background child works on after the parent's
        // turn has stopped. The session must read stopped, the child running.
        let calls = vec![
            call("stop", "2026-07-30T14:59:00.000Z", "sess", None),
            call("pre", "2026-07-30T14:59:40.000Z", "sess", Some("child1")),
        ];
        let fold = Fold::new(&calls);
        let s = fold.session("sess", None, now_ms(), WINDOW).unwrap();
        assert_eq!(s.state, "stopped", "the child's tool call is not the parent's work");
        assert_eq!(fold.child("child1", None, now_ms(), WINDOW).unwrap().state, "running");
    }

    #[test]
    fn out_of_order_lines_still_fold_to_the_latest() {
        // Concurrent sessions interleave in one file, so ordering is not given.
        let calls = vec![
            call("subagent_stop", "2026-07-30T14:59:30.000Z", "sess", Some("c")),
            call("pre", "2026-07-30T14:59:00.000Z", "sess", Some("c")),
        ];
        let v = Fold::new(&calls).child("c", None, now_ms(), WINDOW).unwrap();
        assert_eq!(v.state, "stopped", "latest by timestamp wins, not latest by line");
    }
}
