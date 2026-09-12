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

use crate::mode::{FanMode, Mode};

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
    Pressed {
        mode: Mode,
        changed: bool,
        refusal: Option<String>,
    },
    /// The mode is this now - from the daemon at startup, or after a click.
    Mode(Mode),
    /// The fan mode and what this machine can do with it - from
    /// `fan.getStatus` on reconnect, and from the reply to a click. The
    /// widget needs `switch_mode`/`set_speed` to know which of the four
    /// cards to draw, so this carries more than `FanModeChanged`.
    FanState {
        mode: FanMode,
        manual_percent: u8,
        switch_mode: bool,
        set_speed: bool,
    },
    /// The fan mode moved - a `fan.mode` event, from the app, the CLI, or
    /// this widget. `manual_percent` rides along so the slider tracks a
    /// change made elsewhere.
    FanModeChanged {
        mode: FanMode,
        manual_percent: u8,
    },
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
    /// Pick a fan mode. The `u8` is the 0-255 manual speed, sent only for
    /// `manual` - the other three name a speed the firmware already knows.
    SetFanMode(FanMode, Option<u8>),
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
                    Command::SetFanMode(mode, pwm) => set_fan_mode(mode, pwm),
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

fn set_fan_mode(mode: FanMode, pwm: Option<u8>) -> Message {
    let mut params = json!({ "mode": mode.id() });
    if let Some(pwm) = pwm {
        params["pwm"] = json!(pwm);
    }
    match client::call("fan", "setMode", params) {
        // The reply is a full `fan.getStatus`, so the widget also learns
        // the manual speed the daemon clamped to and re-confirms the caps.
        Ok(status) => fan_state(&status).unwrap_or(Message::FanModeChanged {
            mode,
            manual_percent: pwm.map(percent_from_pwm).unwrap_or(0),
        }),
        Err(e) => Message::Refused(e.to_string()),
    }
}

/// The 0-255 the driver takes, from a 0-100 the slider shows. Never 0 for a
/// positive percentage - `pwm1 = 0` is the driver's "automatic" sentinel,
/// not "off" - matching `percentToPwm` in the app and `MIN_COMMANDED_PWM`
/// in the daemon.
pub fn pwm_from_percent(percent: u8) -> u8 {
    (((u16::from(percent.min(100)) * 255 + 50) / 100).max(1)) as u8
}

/// ...and back, for the slider's starting position.
pub fn percent_from_pwm(pwm: u8) -> u8 {
    ((u16::from(pwm) * 100 + 127) / 255) as u8
}

/// Turns a `fan.getStatus` reply into the state the widget draws from.
fn fan_state(status: &Value) -> Option<Message> {
    let mode = FanMode::parse(status.get("mode")?.as_str()?)?;
    let manual_pwm = status.get("manualPwm").and_then(Value::as_u64).unwrap_or(0);
    let caps = status.get("capabilities");
    let cap = |name: &str| {
        caps.and_then(|c| c.get(name))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    Some(Message::FanState {
        mode,
        manual_percent: percent_from_pwm(manual_pwm.min(255) as u8),
        switch_mode: cap("switchMode"),
        set_speed: cap("setSpeed"),
    })
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

    // Push the current state before the first `nextEvent`, which on an idle
    // daemon does not answer for `POLL_MS`. The widget is most often shown
    // in the first seconds after login, and a blank one until something
    // else happens to move a mode would be the wrong first impression.
    if push_state(events).is_none() {
        return;
    }

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
                    if push_state(events).is_none() {
                        return;
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

                for event in reply
                    .get("events")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
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
                    && events
                        .send_blocking(Message::Unreachable(e.to_string()))
                        .is_err()
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

/// Best-effort push of the current power and fan state, so the widget is
/// right the moment it is first shown rather than only after the first
/// `nextEvent` returns (which is up to `POLL_MS` on an idle daemon). A
/// no-op when the daemon does not answer - the poll loop's own error path
/// is what reports that. `None` means the widget has gone and the thread
/// should stop.
fn push_state(events: &async_channel::Sender<Message>) -> Option<()> {
    if let Some(mode) = current_mode() {
        events.send_blocking(Message::Mode(mode)).ok()?;
    }
    if let Some(state) = current_fan_state() {
        events.send_blocking(state).ok()?;
    }
    Some(())
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
            let changed = payload
                .get("changed")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let refusal = payload
                .get("failed")
                .and_then(Value::as_array)
                .map(|failed| {
                    failed
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .filter(|failed| !failed.is_empty());
            Some(Message::Pressed {
                mode,
                changed,
                refusal,
            })
        }
        "power.mode" => Some(Message::Mode(Mode::parse(payload.get("mode")?.as_str()?)?)),
        "fan.mode" => {
            let mode = FanMode::parse(payload.get("mode")?.as_str()?)?;
            let manual_percent = payload
                .get("manualPwm")
                .and_then(Value::as_u64)
                .map(|pwm| percent_from_pwm(pwm.min(255) as u8))
                .unwrap_or(0);
            Some(Message::FanModeChanged {
                mode,
                manual_percent,
            })
        }
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

/// The fan mode the machine is in and what it can do with it, asked on
/// every reconnect for the same reason as `current_mode`: the app and
/// `pyren-ctl` can have moved it while this process was idle. `None` on a
/// machine with no fan control at all - the widget then never draws the
/// second row.
fn current_fan_state() -> Option<Message> {
    let status = client::call("fan", "getStatus", Value::Null).ok()?;
    fan_state(&status)
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
            Some(Message::Pressed {
                mode,
                changed,
                refusal,
            }) => {
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
            Some(Message::Pressed {
                changed, refusal, ..
            }) => {
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
        match interpret(&event(
            "power.mode",
            json!({ "mode": "unlimited", "source": "hotkey" }),
        )) {
            Some(Message::Mode(mode)) => assert_eq!(mode, Mode::Unlimited),
            other => panic!("expected a mode, got {other:?}"),
        }
    }

    /// A fan-mode change from the app or the CLI moves the second row's
    /// highlight, and the manual speed rides along for the slider.
    #[test]
    fn a_fan_mode_change_moves_the_second_row_and_carries_the_manual_speed() {
        match interpret(&event(
            "fan.mode",
            json!({ "mode": "manual", "manualPwm": 128 }),
        )) {
            Some(Message::FanModeChanged {
                mode,
                manual_percent,
            }) => {
                assert_eq!(mode, FanMode::Manual);
                assert_eq!(manual_percent, 50, "128/255 rounds to 50 %");
            }
            other => panic!("expected a fan mode change, got {other:?}"),
        }
    }

    #[test]
    fn a_fan_mode_event_with_a_junk_mode_is_ignored_rather_than_fatal() {
        assert!(interpret(&event("fan.mode", json!({ "mode": "turbo" }))).is_none());
        assert!(interpret(&event("fan.mode", json!({ "manualPwm": 10 }))).is_none());
    }

    /// The slider shows 0-100; the daemon takes 0-255, and never 0 for a
    /// positive percentage.
    #[test]
    fn the_manual_speed_survives_the_round_trip_through_pwm() {
        assert_eq!(
            pwm_from_percent(0),
            1,
            "0 % is still a commanded speed, not the auto sentinel"
        );
        assert_eq!(pwm_from_percent(100), 255);
        assert_eq!(percent_from_pwm(255), 100);
        assert_eq!(percent_from_pwm(0), 0);
        for percent in 0..=100u8 {
            let round_tripped = percent_from_pwm(pwm_from_percent(percent));
            assert!(
                round_tripped.abs_diff(percent) <= 1,
                "{percent}% -> {} -> {round_tripped}%",
                pwm_from_percent(percent)
            );
        }
    }
}
