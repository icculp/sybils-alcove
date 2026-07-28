//! Live process state. Port of `alcove/sources/process.py`.
//!
//! A file timestamp says a transcript was written recently. Only a process says
//! a session is alive. These are different facts and the UI keeps them separate.
//!
//! This layer does NOT get faster by being in Rust: it shells out to the
//! `claude` CLI, which costs ~560 ms regardless of caller. Once parsing is
//! incremental that subprocess dominates a refresh entirely, so it is cached on
//! a longer TTL than the transcript scan.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

#[derive(Debug, Default, Clone)]
pub struct Proc {
    pub pids: Vec<i64>,
    pub name: String,
    pub kind: String,
}

/// Portable liveness probe: signal 0.
///
/// `/proc` exists only on Linux — checking it on macOS silently drops every pid
/// and `running` never appears, with the lookup still reporting ok. EPERM means
/// the process exists but belongs to someone else, which is still alive.
pub fn pid_alive(pid: i64) -> bool {
    // SAFETY: kill(pid, 0) performs no signal delivery; it only probes.
    let rc = unsafe { libc_kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    // errno: 1 = EPERM (exists, not ours), 3 = ESRCH (gone)
    std::io::Error::last_os_error().raw_os_error() == Some(1)
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// Absolute path to the `claude` CLI.
///
/// A bare "claude" resolves fine in a login shell and not at all under systemd,
/// whose PATH has no nvm directory. In the Python that failure was swallowed by
/// a bare except for the entire life of the pid column: every session reported
/// no process, so liveness silently degraded to "was this file written
/// recently", which reports a busy session as idle and a dead one as present.
pub fn claude_bin() -> String {
    if let Ok(explicit) = std::env::var("ALCOVE_CLAUDE_BIN") {
        if !explicit.is_empty() {
            return explicit;
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = PathBuf::from(dir).join("claude");
            if candidate.is_file() {
                return candidate.to_string_lossy().to_string();
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    // Newest nvm version first, matching the Python's reverse sort.
    let nvm = PathBuf::from(&home).join(".nvm/versions/node");
    if let Ok(entries) = std::fs::read_dir(&nvm) {
        let mut versions: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        versions.sort();
        for version in versions.into_iter().rev() {
            let candidate = version.join("bin/claude");
            if candidate.is_file() {
                return candidate.to_string_lossy().to_string();
            }
        }
    }
    for fixed in ["/usr/local/bin/claude", "/usr/bin/claude"] {
        if PathBuf::from(fixed).is_file() {
            return fixed.to_string();
        }
    }
    String::new()
}

/// (sessionId -> Proc, status).
///
/// Status is reported to the page rather than discarded: "no process" and "I
/// could not ask" must not look the same, or a broken lookup reads as every
/// session having ended.
pub fn running_pids() -> (HashMap<String, Proc>, String) {
    let exe = claude_bin();
    if exe.is_empty() {
        return (HashMap::new(), "unavailable: claude CLI not found".into());
    }
    let output = match Command::new(&exe).args(["agents", "--json", "--all"]).output() {
        Ok(o) => o,
        Err(e) => return (HashMap::new(), format!("unavailable: {}", e.kind() as i32)),
    };
    if !output.status.success() {
        return (
            HashMap::new(),
            format!("unavailable: exit {}", output.status.code().unwrap_or(-1)),
        );
    }
    let parsed: Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => return (HashMap::new(), "unavailable: unparseable output".into()),
    };
    let mut out: HashMap<String, Proc> = HashMap::new();
    for row in parsed.as_array().unwrap_or(&Vec::new()) {
        let sid = row.get("sessionId").and_then(Value::as_str).unwrap_or("");
        let pid = row.get("pid").and_then(Value::as_i64);
        // The CLI can list an entry whose process is already gone.
        let Some(pid) = pid else { continue };
        if sid.is_empty() || !pid_alive(pid) {
            continue;
        }
        let entry = out.entry(sid.to_string()).or_default();
        entry.pids.push(pid);
        if entry.name.is_empty() {
            entry.name = row.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        }
        if entry.kind.is_empty() {
            entry.kind = row.get("kind").and_then(Value::as_str).unwrap_or("").to_string();
        }
    }
    (out, "ok".into())
}

/// How many `codex` processes are running, or None if /proc is unreadable.
///
/// Deliberately a count and not a mapping: Codex puts no thread id in its argv
/// and holds no transcript fd open, so there is no honest way to attribute a
/// process to a session.
pub fn codex_process_count() -> Option<i64> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut total = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let argv0 = cmdline.split(|&b| b == 0).next().unwrap_or(&[]);
        let argv0 = String::from_utf8_lossy(argv0);
        // Basename, so the `node` wrapper fronting each one is not double-counted.
        if argv0.rsplit('/').next() == Some("codex") {
            total += 1;
        }
    }
    Some(total)
}
