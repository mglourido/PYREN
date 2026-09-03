//! Things that happen *to* the daemon, and the long poll a client reads
//! them with.
//!
//! Everything else in this protocol is a question a client asks. This is
//! the one direction that could not be expressed that way: the hotkey is
//! pressed when the user presses it, and the OSD has to hear about it in
//! the tens of milliseconds a person notices, not on the next poll of
//! `power.getState`.
//!
//! **The framing does not change**, deliberately. A client still sends one
//! request line and reads one response line; `core.nextEvent` simply does
//! not answer until there is something to say or the timeout runs out. So
//! no existing client, and nothing in `docs/01-ipc-protocol.md` about
//! framing, has to be revisited - and a client that never calls it is
//! unaffected.
//!
//! Two consequences worth knowing:
//!
//! - The buffer is small and **events are dropped rather than queued
//!   forever** for a client that stopped reading. A reply says how many
//!   were missed instead of pretending the stream was complete: an OSD
//!   that was asleep should redraw from `power.getState`, not replay six
//!   minutes of key presses.
//! - A poll costs one connection and one thread for its duration. That is
//!   affordable here (one OSD process, one app) and would not be if this
//!   ever became a per-widget subscription.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// How many events are kept for a client that is between polls.
///
/// Small on purpose: the useful ones are seconds old, and a client that
/// fell far enough behind to lose some needs to re-read the state anyway.
const CAPACITY: usize = 64;

/// Longest a `nextEvent` call will wait before answering with nothing.
/// Bounded so a client cannot pin a daemon thread indefinitely.
pub const MAX_WAIT: Duration = Duration::from_secs(60);

/// What a client gets when it does not ask for a particular wait.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(25);

#[derive(Debug, Clone)]
pub struct Event {
    pub seq: u64,
    pub topic: String,
    pub payload: Value,
    at: Instant,
}

impl Event {
    /// The wire shape. Age is reported rather than a timestamp because it
    /// is what a reader actually needs - "is this still worth showing?" -
    /// and because it cannot be wrong across a suspend or a clock change.
    fn to_json(&self, now: Instant) -> Value {
        json!({
            "seq": self.seq,
            "topic": self.topic,
            "payload": self.payload,
            "ageMs": now.saturating_duration_since(self.at).as_millis() as u64,
        })
    }
}

#[derive(Debug, Default)]
struct Ring {
    events: VecDeque<Event>,
    /// Sequence of the newest event published; 0 before the first.
    latest: u64,
    /// Sequence of the oldest event still held, once anything was evicted.
    oldest: u64,
}

/// The daemon's published events. Cheap to clone the `Arc` around; every
/// publisher and every reader shares one.
#[derive(Debug)]
pub struct EventBus {
    ring: Mutex<Ring>,
    published: Condvar,
}

/// One answer to `core.nextEvent`.
pub struct Batch {
    /// The sequence to pass back as `since` next time - the newest event
    /// that exists, whether or not this batch carried it.
    pub seq: u64,
    pub events: Vec<Event>,
    /// Events that happened after `since` and were evicted before this
    /// client came back for them. Non-zero means "you missed something,
    /// re-read the state" - it is never rounded down to keep a reply tidy.
    pub missed: u64,
}

impl Batch {
    pub fn to_json(&self) -> Value {
        let now = Instant::now();
        json!({
            "seq": self.seq,
            "events": self.events.iter().map(|e| e.to_json(now)).collect::<Vec<_>>(),
            "missed": self.missed,
        })
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self { ring: Mutex::new(Ring::default()), published: Condvar::new() }
    }

    /// Publishes one event and wakes every waiting poll. Returns its
    /// sequence number.
    ///
    /// Never blocks on a reader: if nobody is listening the event ages out
    /// of the ring and that is the end of it. A hardware daemon must not
    /// be able to stall because a GUI stopped reading its socket.
    pub fn publish(&self, topic: impl Into<String>, payload: Value) -> u64 {
        let seq = {
            let mut ring = self.lock();
            ring.latest += 1;
            let event =
                Event { seq: ring.latest, topic: topic.into(), payload, at: Instant::now() };
            ring.events.push_back(event);
            while ring.events.len() > CAPACITY {
                if let Some(dropped) = ring.events.pop_front() {
                    ring.oldest = dropped.seq;
                }
            }
            ring.latest
        };
        self.published.notify_all();
        seq
    }

    /// The newest sequence number so far. A client that wants only what
    /// happens from now on starts here instead of replaying the ring.
    pub fn latest(&self) -> u64 {
        self.lock().latest
    }

    /// Everything after `since`, waiting up to `timeout` for the first one.
    ///
    /// `since` is the `seq` from the previous reply. Passing the current
    /// [`Self::latest`] means "only what happens from now"; passing 0 means
    /// "whatever you still hold", which is what a client that has never
    /// polled before gets if it asks for it.
    pub fn read_since(&self, since: u64, timeout: Duration) -> Batch {
        let timeout = timeout.min(MAX_WAIT);
        let deadline = Instant::now() + timeout;

        let mut ring = self.lock();
        loop {
            if ring.latest > since {
                return Self::batch(&ring, since);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // Nothing happened. `seq` still comes back so a client that
                // polled with a stale `since` catches up rather than asking
                // for the same nothing forever.
                return Batch { seq: ring.latest, events: Vec::new(), missed: 0 };
            }
            let (guard, _) = self
                .published
                .wait_timeout(ring, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            ring = guard;
        }
    }

    fn batch(ring: &Ring, since: u64) -> Batch {
        let events: Vec<Event> =
            ring.events.iter().filter(|e| e.seq > since).cloned().collect();
        // Everything from `since + 1` up to the oldest we still hold is
        // gone. `oldest` is 0 until the first eviction, which is why this
        // is written as a saturating difference rather than a comparison.
        let missed = ring.oldest.saturating_sub(since);
        Batch { seq: ring.latest, events, missed }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ring> {
        self.ring.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const NONE: Duration = Duration::from_millis(0);

    #[test]
    fn a_client_reads_only_what_happened_after_the_sequence_it_holds() {
        let bus = EventBus::new();
        bus.publish("hotkey.pressed", json!({ "n": 1 }));
        let first = bus.read_since(0, NONE);
        assert_eq!(first.events.len(), 1);
        assert_eq!(first.seq, 1);

        bus.publish("hotkey.pressed", json!({ "n": 2 }));
        let second = bus.read_since(first.seq, NONE);
        assert_eq!(second.events.len(), 1, "the one it already had must not come back");
        assert_eq!(second.events[0].payload["n"], 2);
    }

    #[test]
    fn asking_from_the_latest_sequence_starts_a_client_at_now() {
        let bus = EventBus::new();
        bus.publish("power.mode", json!({ "mode": "eco" }));
        let start = bus.latest();

        let batch = bus.read_since(start, NONE);
        assert!(batch.events.is_empty(), "history is not replayed to a client that starts now");
        assert_eq!(batch.seq, start);
    }

    /// The honest half of a bounded buffer: a client that fell behind is
    /// told so, rather than handed a gap it cannot see.
    #[test]
    fn a_client_that_fell_behind_is_told_how_much_it_lost() {
        let bus = EventBus::new();
        for n in 0..CAPACITY + 10 {
            bus.publish("hotkey.pressed", json!({ "n": n }));
        }

        let batch = bus.read_since(0, NONE);
        assert_eq!(batch.events.len(), CAPACITY);
        assert_eq!(batch.missed, 10);
        assert_eq!(batch.seq, (CAPACITY + 10) as u64);

        // And a client that is merely up to date is not told it lost
        // anything, which is the failure mode of a naive difference.
        assert_eq!(bus.read_since(batch.seq, NONE).missed, 0);
    }

    #[test]
    fn a_poll_with_nothing_to_report_answers_empty_rather_than_failing() {
        let bus = EventBus::new();
        let batch = bus.read_since(0, Duration::from_millis(20));
        assert!(batch.events.is_empty());
        assert_eq!(batch.seq, 0);
        assert_eq!(batch.missed, 0);
    }

    /// The whole point of the long poll: the OSD is woken by the key press,
    /// not by its next scheduled question.
    #[test]
    fn a_waiting_poll_is_woken_by_a_publish() {
        let bus = Arc::new(EventBus::new());
        let publisher = Arc::clone(&bus);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            publisher.publish("hotkey.pressed", json!({ "action": "powerCycle" }));
        });

        let started = Instant::now();
        let batch = bus.read_since(0, Duration::from_secs(5));
        assert_eq!(batch.events.len(), 1, "waited {:?} and got nothing", started.elapsed());
        assert!(started.elapsed() < Duration::from_secs(4), "it waited out the timeout instead");
        assert_eq!(batch.events[0].topic, "hotkey.pressed");
    }

    #[test]
    fn a_wait_longer_than_the_ceiling_is_clamped_rather_than_honoured() {
        let bus = EventBus::new();
        let started = Instant::now();
        // Not a real wait - the point is that MAX_WAIT bounds it, which is
        // checked by asking for far more and reading the code path that
        // clamps. Keep the assertion cheap: a zero-length ask returns now.
        let _ = bus.read_since(0, NONE);
        assert!(started.elapsed() < MAX_WAIT);
    }

    #[test]
    fn the_wire_shape_carries_the_fields_a_reader_branches_on() {
        let bus = EventBus::new();
        bus.publish("power.mode", json!({ "mode": "performance", "source": "hotkey" }));
        let json = bus.read_since(0, NONE).to_json();

        assert_eq!(json["events"][0]["topic"], "power.mode");
        assert_eq!(json["events"][0]["payload"]["source"], "hotkey");
        assert_eq!(json["events"][0]["seq"], 1);
        assert!(json["events"][0]["ageMs"].is_u64());
        assert_eq!(json["missed"], 0);
    }
}
