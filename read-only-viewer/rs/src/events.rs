//! The change signal every subscriber gets, and nothing more.
//!
//! What crosses this channel is a SEQUENCE NUMBER, not a snapshot. `/api/sessions`
//! is ~1.7 MB, and pushing that to every open tab on every hook line would spend
//! megabytes to say "look again" — so the page refetches on its own, over the same
//! cached, single-flighted collect every other client uses.
//!
//! A subscriber is a channel plus the thread that owns its socket, and it is
//! reaped from BOTH ends, because each end sees a different failure:
//!
//! - the stream thread notices its socket is dead only when a write fails, so it
//!   returns, and its `Sub` guard removes itself on drop;
//! - `publish` notices a thread that has already gone by a send that fails.
//!
//! Either alone would leak in one direction. Together the count is exact, which is
//! why it is worth logging: "how many streams are open" is the one number that
//! says whether this leaks threads.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// One thread per subscriber is fine at this scale — a handful of tabs — but it is
/// not free, so there is a ceiling. Past it a subscriber is refused with a status
/// the client can act on, rather than accepted into an unbounded thread pile.
pub const MAX_SUBSCRIBERS: usize = 64;

pub struct Events {
    seq: AtomicU64,
    next_id: AtomicU64,
    subs: Mutex<Vec<(u64, Sender<u64>)>>,
}

/// A live subscription. Dropping it unsubscribes — so the stream thread cannot
/// return without freeing its slot, whatever path it returns by.
pub struct Sub {
    id: u64,
    events: Arc<Events>,
    pub rx: Receiver<u64>,
}

impl Drop for Sub {
    fn drop(&mut self) {
        if let Ok(mut subs) = self.events.subs.lock() {
            subs.retain(|(id, _)| *id != self.id);
        }
    }
}

impl Default for Events {
    fn default() -> Self {
        Self { seq: AtomicU64::new(0), next_id: AtomicU64::new(0), subs: Mutex::new(Vec::new()) }
    }
}

impl Events {
    /// `None` when the ceiling is reached; the caller answers 503.
    pub fn subscribe(self: &Arc<Self>) -> Option<Sub> {
        let mut subs = self.subs.lock().ok()?;
        if subs.len() >= MAX_SUBSCRIBERS {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        subs.push((id, tx));
        Some(Sub { id, events: Arc::clone(self), rx })
    }

    /// Bump the sequence and wake everyone. Returns the new sequence.
    ///
    /// The `retain` is the second line of defence: the guard's `Drop` is what
    /// normally frees a slot, and this catches a receiver that went away without
    /// it. Cheap, and the alternative is a broadcast into a channel nobody holds.
    pub fn publish(&self) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut subs) = self.subs.lock() {
            subs.retain(|(_, tx)| tx.send(seq).is_ok());
        }
        seq
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    pub fn subscribers(&self) -> usize {
        self.subs.lock().map(|s| s.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subscriber_sees_the_sequence_advance() {
        let events = Arc::new(Events::default());
        let sub = events.subscribe().unwrap();
        assert_eq!(events.publish(), 1);
        assert_eq!(events.publish(), 2);
        assert_eq!(sub.rx.recv().unwrap(), 1);
        assert_eq!(sub.rx.recv().unwrap(), 2);
        assert_eq!(events.subscribers(), 1);
    }

    #[test]
    fn dropping_the_subscription_frees_the_slot_immediately() {
        let events = Arc::new(Events::default());
        let sub = events.subscribe().unwrap();
        assert_eq!(events.subscribers(), 1);
        drop(sub);
        // No publish needed: the stream thread returning is itself the signal.
        assert_eq!(events.subscribers(), 0);
    }

    #[test]
    fn the_ceiling_refuses_rather_than_growing() {
        let events = Arc::new(Events::default());
        let held: Vec<_> = (0..MAX_SUBSCRIBERS).map(|_| events.subscribe().unwrap()).collect();
        assert!(events.subscribe().is_none());
        drop(held);
        assert!(events.subscribe().is_some());
    }
}
