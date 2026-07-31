//! The push side: a filesystem watch that turns an append into a change signal.
//!
//! Everything this viewer knows arrives as bytes appended to a file — a transcript
//! event, a hook spool line, a Codex sqlite write. So the trigger is the
//! filesystem, and the poll loop in the browser becomes a fallback rather than the
//! mechanism.
//!
//! `notify` rather than raw inotify: this is a public, generic tool, and a
//! Linux-only syscall in the middle of it would make the macOS story "port it
//! first". On Linux `notify` IS inotify, so nothing is paid for the portability
//! beyond one dependency.
//!
//! ## What this bound is, precisely
//!
//! **Process death emits no filesystem event.** A session whose `claude` process
//! exits changes no file, so nothing here fires; `state` flips to `ended` on the
//! pid cache's own 15 s cadence. That is a documented bound, not a bug — and it is
//! the only state transition the push path cannot see.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::event::{EventKind, ModifyKind};
use notify::{RecursiveMode, Watcher};

use crate::collect::Collector;
use crate::config::Config;
use crate::events::Events;

/// Appends arrive in bursts — one turn writes the transcript, the tool-result
/// file, and two spool lines within a few milliseconds. Coalescing for 200 ms
/// turns that into one collect while keeping the visible latency well inside the
/// one-second target.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// A floor on how often a watch-driven collect may run, so a machine with six
/// busy sessions cannot spin the collector continuously. A warm collect is ~139
/// ms; at this floor the watcher costs at most ~1/3 of one core in the worst case
/// and normally nothing.
const MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Does this KIND of event mean the data changed?
///
/// This one is not a nicety, it is the difference between a watcher and a spin
/// loop. `notify`'s inotify backend subscribes to the full event set, which
/// includes `IN_OPEN` and `IN_ACCESS` — and this process READS the directories it
/// watches. Measured: one appended spool line produced a collect, the collect
/// opened the two transcript roots and both spool files, each open arrived back
/// as `Access(Open(Any))`, and the watcher collected again, forever, at exactly
/// the rate limiter's floor. The event stream never went idle again.
///
/// So: a read is this process's own footprint and is never a reason to collect.
/// Only a write is. `Modify(Metadata)` goes with it — an atime bump is not news.
fn triggers(kind: &EventKind) -> bool {
    !matches!(kind, EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_)))
}

/// Is this path worth a collect?
///
/// Transcripts and spool lines are `.jsonl`, Codex state is `.sqlite` (plus its
/// `-wal`/`-shm`), and the subagent `.meta.json` files appear when a child is
/// spawned. Everything else under these roots — lock files, editor droppings — is
/// noise, and a noisy trigger is a collect that publishes a change with nothing
/// changed.
fn interesting(path: &Path) -> bool {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        // A directory event with no name still means something moved; err toward
        // collecting rather than missing a new session directory.
        None => return true,
    };
    name.ends_with(".jsonl")
        || name.ends_with(".json")
        || name.contains(".sqlite")
        || !name.contains('.') // a new session/project directory
}

/// Start watching. The returned handle owns the watcher and must be kept alive:
/// dropping it stops the watch, and the page silently goes back to polling.
pub struct Watch {
    _thread: std::thread::JoinHandle<()>,
}

pub fn spawn(cfg: &Config, collector: Arc<Collector>, events: Arc<Events>) -> Option<Watch> {
    let (tx, rx) = channel::<()>();
    // Why a change fired is otherwise invisible, and a watch that retriggers
    // itself looks identical to a busy machine.
    let debug = std::env::var("ALCOVE_WATCH_DEBUG").is_ok();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        // The watcher callback runs on notify's own thread and must not do work:
        // it decides whether the path matters and hands off. A blocking collect
        // here would stall the event queue and drop later events.
        if let Ok(event) = res {
            if debug {
                eprintln!("watch: {:?} {:?}", event.kind, event.paths);
            }
            if triggers(&event.kind)
                && (event.paths.is_empty() || event.paths.iter().any(|p| interesting(p)))
            {
                let _ = tx.send(());
            }
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("watch: unavailable ({e}); the page will poll instead");
            return None;
        }
    };

    // The spool directory may not exist yet — the hooks may never have run. Watch
    // its parent in that case so the directory's creation is itself an event,
    // rather than silently never watching the spool at all. The viewer does not
    // create it: it never writes.
    let spool = crate::spool::spool_dir();
    let spool_target =
        if spool.is_dir() { Some(spool.clone()) } else { spool.parent().map(PathBuf::from) };

    let mut watching = 0usize;
    for root in [Some(cfg.claude_root.clone()), Some(cfg.codex_root.clone()), spool_target]
        .into_iter()
        .flatten()
    {
        if !root.is_dir() {
            continue;
        }
        match watcher.watch(&root, RecursiveMode::Recursive) {
            Ok(()) => {
                watching += 1;
                println!("  watch:  {}", root.display());
            }
            Err(e) => eprintln!("watch: {} not watched ({e})", root.display()),
        }
    }
    if watching == 0 {
        eprintln!("watch: nothing watchable; the page will poll instead");
        return None;
    }

    let thread = std::thread::spawn(move || {
        // `watcher` is moved in so it lives exactly as long as this loop.
        let _watcher = watcher;
        let mut last = Instant::now() - MIN_INTERVAL;
        loop {
            // Block until something moves. A closed channel means every sender is
            // gone, which cannot happen while `_watcher` is alive — so this exits
            // only when the watcher itself is dropped.
            if rx.recv().is_err() {
                return;
            }
            // Coalesce the burst: drain for DEBOUNCE, then collect once.
            let deadline = Instant::now() + DEBOUNCE;
            loop {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                match rx.recv_timeout(remaining) {
                    Ok(()) => continue,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            if let Some(wait) = MIN_INTERVAL.checked_sub(last.elapsed()) {
                std::thread::sleep(wait);
            }
            last = Instant::now();
            collector.refresh();
            events.publish();
        }
    });
    Some(Watch { _thread: thread })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, RemoveKind};

    #[test]
    fn a_read_is_never_a_reason_to_collect() {
        // The loop that shipped for ten minutes: the collector opens what it
        // watches, so its own opens came back as events.
        assert!(!triggers(&EventKind::Access(AccessKind::Open(AccessMode::Any))));
        assert!(!triggers(&EventKind::Access(AccessKind::Read)));
        assert!(!triggers(&EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))));
        // A write is.
        assert!(triggers(&EventKind::Modify(ModifyKind::Data(DataChange::Any))));
        assert!(triggers(&EventKind::Create(CreateKind::File)));
        assert!(triggers(&EventKind::Remove(RemoveKind::File)));
    }

    #[test]
    fn only_the_files_that_carry_state_trigger_a_collect() {
        assert!(interesting(Path::new("/root/.claude/projects/-root/abc.jsonl")));
        assert!(interesting(Path::new("/x/agent-a1.meta.json")));
        assert!(interesting(Path::new("/x/state_3.sqlite")));
        assert!(interesting(Path::new("/x/state_3.sqlite-wal")));
        assert!(interesting(Path::new("/x/claude-20260730.jsonl")));
        // A new session directory has no extension, and its creation matters.
        assert!(interesting(Path::new("/root/.claude/projects/-root/9c5e83d0-0c96")));
        assert!(!interesting(Path::new("/x/.transcript.jsonl.swp")));
        assert!(!interesting(Path::new("/x/notes.md")));
    }
}
