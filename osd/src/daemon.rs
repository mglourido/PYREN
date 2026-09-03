//! Talking to pyren-daemon from a GUI process: one thread that waits for
//! events, one that runs the calls the widget makes, and a channel into the
//! GTK main loop.
//!
//! No socket call happens on the main thread. A `power.setMode` runs
//! `powerprofilesctl` inside the daemon and takes a human-visible fraction
//! of a second, and a widget that freezes while the mode changes is a
//! widget that looks broken at the exact moment it is working.

use std::sync::mpsc;
use std::time::Duration;

use pyren_core::client;
use serde_json::{json, Value};

use crate::mode::Mode;

/// Something the widget needs to react to. Everything the GTK thread learns
/// about the daemon arrives as one of these.
#[derive(Debug, Clone)]
pub enum Message {
    /// The shortcut was pressed. Show the widget with this mode
    /// highlighted; nothing was changed, and the user picks from here.
    Show(Mode),
    /// The shortcut was pressed on a daemon old enough to still cycle the
    /// mode itself. Kept because the event is a released protocol and a
    /// widget that cannot read the previous daemon is a widget that breaks
    /// on the one upgrade nobody sequences.
    Pressed { mode: Mode, changed: bool, refusal: Option<String> },
    /// The mode is this now - from the daemon at startup, or after a click.
    Mode(Mode),
    /// A call the widget asked for was refused.
    Refused(String),
    /// The daemon could not be reached. Carried rather than logged because
    /// the widget shows it: a key press that does nothing needs a reason
    /// on screen, not in a journal the user is not reading.
    Unreachable(String),
    Reachable,
}

/// Something the widget wants done.
pub enum Command {
    SetMode(Mode),
}

/// Long enough that the daemon is not answering constantly, short enough
/// that a restarted daemon is noticed within one poll.
const POLL_MS: u64 = 25_000;

/// After the daemon goes away. Long enough not to spin on a socket that
/// will be gone for the rest of the session.
const RETRY: Duration = Duration::from_secs(2);

/// Starts both threads. Returns the sender the widget puts commands on.
pub fn start(events: async_channel::Sender<Message>) -> mpsc::Sender<Command> {
    let poller = events.clone();
    std::thread::Builder::new()
        .name("pyren-osd-events".into())
        .spawn(move || poll_forever(poller))
        .expect("the event thread must start");

    let (tx, rx) = mpsc::channel::<Command>();
    std::thread::Builder::new()
        .name("pyren-osd-commands".into())
        .spawn(move || {
            while let Ok(command) = rx.recv() {
                let message = match command {
                    Command::SetMode(mode) => set_mode(mode),
                };
                if events.send_blocking(message).is_err() {
                    return;
                }
            }
        })
        .expect("the command thread must start");

    tx
}

fn set_mode(mode: Mode) -> Message {
    match client::call("power", "setMode", json!({ "mode": mode.id() })) {
        Ok(_) => Message::Mode(mode),
        Err(e) => Message::Refused(e.to_string()),
    }
}

/// The one way this thread can stop, and the reason it says so out loud.
///
/// If it ever exits, the widget stays alive and simply never appears
/// again - the process answers, the window still opens when activated by
/// hand, and nothing is logged. That is a bad failure to diagnose from a
/// user's description, so it leaves a line behind.
fn poll_forever(events: async_channel::Sender<Message>) {
    poll_until_closed(&events);
    eprintln!(
        "pyren-osd: stopped listening for daemon events (the widget will no longer \
         appear on its own); restart pyren-osd"
    );
}

fn poll_until_closed(events: &async_channel::Sender<Message>) {
    let mut since: Option<u64> = None;
    let mut connected = false;

    loop {
        let mut params = json!({ "timeoutMs": POLL_MS });
        if let Some(since) = since {
            params["since"] = json!(since);
        }

        match client::call("core", "nextEvent", params) {
            Ok(reply) => {
                if !connected {
                    connected = true;
                    if events.send_blocking(Message::Reachable).is_err() {
                        return;
                    }
                    if let Some(mode) = current_mode() {
                        if events.send_blocking(Message::Mode(mode)).is_err() {
                            return;
                        }
                    }
                }

                let seq = reply.get("seq").and_then(Value::as_u64);
                // A daemon that restarted counts from zero again. Holding
                // on to its predecessor's sequence would mean waiting for a
                // number the new one will not reach for hours.
                since = match (since, seq) {
                    (Some(previous), Some(seq)) if seq < previous => Some(seq),
                    (previous, seq) => seq.or(previous),
                };

                for event in reply.get("events").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(message) = interpret(event) {
                        if events.send_blocking(message).is_err() {
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                // Reported when the daemon goes away, and once at startup
                // if it was never there - not on every retry, which would
                // be a line every two seconds for as long as it is down.
                if (connected || since.is_none())
                    && events.send_blocking(Message::Unreachable(e.to_string())).is_err()
                {
                    return;
                }
                connected = false;
                // Start from "now" on reconnect: the key presses that
                // happened while the daemon was down are not worth
                // flashing a widget for.
                since = None;
                std::thread::sleep(RETRY);
            }
        }
    }
}

/// Turns one published event into something the widget can act on, or
/// `None` for a topic this build does not know - which is a newer daemon,
/// not an error.
fn interpret(event: &Value) -> Option<Message> {
    let payload = event.get("payload")?;
    match event.get("topic")?.as_str()? {
        "hotkey.pressed" => {
            let mode = Mode::parse(payload.get("mode")?.as_str()?)?;
            // `show` is what a current daemon sends: the key asks for the
            // widget and touches nothing, so there is no outcome to
            // report and nothing that can have been refused.
            if payload.get("action").and_then(Value::as_str) == Some("show") {
                return Some(Message::Show(mode));
            }
            let changed = payload.get("changed").and_then(Value::as_bool).unwrap_or(true);
            let refusal = payload
                .get("failed")
                .and_then(Value::as_array)
                .map(|failed| {
                    failed.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")
                })
                .filter(|failed| !failed.is_empty());
            Some(Message::Pressed { mode, changed, refusal })
        }
        "power.mode" => Some(Message::Mode(Mode::parse(payload.get("mode")?.as_str()?)?)),
        _ => None,
    }
}

/// What the machine is set to right now.
///
/// Asked on every reconnect rather than remembered, because the app, the
/// supervisor and `pyren-ctl` can all have moved the mode while this
/// process was doing nothing.
fn current_mode() -> Option<Mode> {
    let state = client::call("power", "getState", Value::Null).ok()?;
    Mode::parse(state.get("mode")?.as_str()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(topic: &str, payload: Value) -> Value {
        json!({ "seq": 1, "topic": topic, "payload": payload, "ageMs": 3 })
    }

    /// What a current daemon sends: the key asks for the widget and the
    /// machine is left alone. Nothing was attempted, so nothing may be
    /// reported as having succeeded or failed.
    #[test]
    fn a_press_shows_the_modes_without_claiming_anything_changed() {
        let message = interpret(&event(
            "hotkey.pressed",
            json!({ "action": "show", "mode": "performance", "device": "AT Translated Set 2 keyboard" }),
        ));

        match message {
            Some(Message::Show(mode)) => assert_eq!(mode, Mode::Performance),
            other => panic!("expected a show, got {other:?}"),
        }
    }

    #[test]
    fn a_key_press_becomes_a_widget_that_shows_the_new_mode() {
        let message = interpret(&event(
            "hotkey.pressed",
            json!({ "mode": "performance", "changed": true, "failed": [] }),
        ));

        match message {
            Some(Message::Pressed { mode, changed, refusal }) => {
                assert_eq!(mode, Mode::Performance);
                assert!(changed);
                assert_eq!(refusal, None);
            }
            other => panic!("expected a press, got {other:?}"),
        }
    }

    /// The case this machine is actually in: the key works, the daemon
    /// tried, and the mode did not move. The widget has to be able to say
    /// so, which means the reason has to survive the trip.
    #[test]
    fn a_press_that_changed_nothing_carries_the_reason_why() {
        let message = interpret(&event(
            "hotkey.pressed",
            json!({
                "mode": "eco",
                "changed": false,
                "failed": ["power-profiles-daemon: device or resource busy"],
            }),
        ));

        match message {
            Some(Message::Pressed { changed, refusal, .. }) => {
                assert!(!changed);
                assert!(refusal.unwrap().contains("busy"));
            }
            other => panic!("expected a press, got {other:?}"),
        }
    }

    #[test]
    fn a_topic_this_build_has_never_heard_of_is_ignored_rather_than_fatal() {
        assert!(interpret(&event("fan.curveApplied", json!({ "mode": "curve" }))).is_none());
        assert!(interpret(&event("power.mode", json!({ "mode": "nonsense" }))).is_none());
        assert!(interpret(&json!({ "seq": 1 })).is_none());
    }

    #[test]
    fn a_mode_change_from_anywhere_moves_the_highlight() {
        match interpret(&event("power.mode", json!({ "mode": "unlimited", "source": "hotkey" }))) {
            Some(Message::Mode(mode)) => assert_eq!(mode, Mode::Unlimited),
            other => panic!("expected a mode, got {other:?}"),
        }
    }
}
