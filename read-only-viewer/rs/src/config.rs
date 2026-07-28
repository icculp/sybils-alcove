//! Environment configuration. Port of `alcove/config.py`.
//!
//! Every value has a working default except ALCOVE_TOKEN, which is mandatory
//! for any non-loopback bind.

use std::path::PathBuf;

pub const COOKIE: &str = "alcove_token";

#[derive(Clone)]
pub struct Config {
    pub claude_root: PathBuf,
    pub codex_root: PathBuf,
    pub port: u16,
    pub bind: String,
    pub token: String,
    pub live_window_s: f64,
    pub cache_ttl_s: f64,
    pub pid_ttl_s: f64,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root"))
}

/// `~/...` is routine in an env file, and a literal "~" path fails to exist —
/// the server would report "no transcripts" and exit.
fn expand(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(raw)
}

fn env_path(key: &str, default: PathBuf) -> PathBuf {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => expand(&v),
        _ => default,
    }
}

fn env_num(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            claude_root: env_path("ALCOVE_CLAUDE_ROOT", home().join(".claude/projects")),
            codex_root: env_path("ALCOVE_CODEX_ROOT", home().join(".codex/sessions")),
            port: env_num("ALCOVE_PORT", 8899.0) as u16,
            bind: std::env::var("ALCOVE_BIND").unwrap_or_else(|_| "127.0.0.1".into()),
            token: std::env::var("ALCOVE_TOKEN").unwrap_or_default(),
            live_window_s: env_num("ALCOVE_LIVE_WINDOW_S", 300.0),
            // The transcript scan is cached per file now, so this exists only to
            // stop a burst of clients each triggering a fresh assemble.
            cache_ttl_s: env_num("ALCOVE_CACHE_TTL_S", 2.0),
            // The pid lookup is a ~560 ms subprocess; polling it every 2 s was
            // the dominant cost once scanning became incremental.
            pid_ttl_s: env_num("ALCOVE_PID_TTL_S", 15.0),
        }
    }

    pub fn is_local_bind(&self) -> bool {
        self.bind == "127.0.0.1" || self.bind == "localhost"
    }
}
