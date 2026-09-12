//! Lighting effects: frames computed here, written by the daemon.
//!
//! ## Why effects are frames
//!
//! The firmware has no animations of its own - no command type anyone has
//! published starts one in the EC - so every effect is the four zones
//! rewritten many times a second. `omen-rgb-keyboard` does the same from a
//! kernel timer. What that costs, and why the default is 30 fps, is in
//! `dev/FINDINGS.md` §"Lighting effects".
//!
//! ## Four zones, not keys
//!
//! The keyboard on the machine this was built on reports itself as a
//! standard, non-per-key layout, and the four zones are the resolution of
//! the hardware. So a "wave" here is a pulse crossing four sectors, and a
//! rainbow is four bands. Effects are written against [`ZONES`] positions
//! so that nothing here has to change if a finer device is ever driven.
//!
//! ## Two halves
//!
//! [`frame`] is a pure function of the effect and the time, so every
//! effect is tested without hardware. [`Animator`] is the thread that
//! paces it and hands each frame to a [`Sink`].

use std::f64::consts::TAU;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::color::Rgb;
use crate::dialect::DialectError;
use crate::ZONES;

/// How many colours an effect can be given. Enough for any cycle a person
/// would set up by hand, and a bound on what a request can make us store.
pub const MAX_COLORS: usize = 8;

pub const SPEED_MIN: u8 = 1;
pub const SPEED_MAX: u8 = 10;
pub const SPEED_DEFAULT: u8 = 5;

/// The frame rates offered. 30 is smooth on four zones and costs about 4 %
/// of a core; 60 costs 8 % and is hard to tell apart.
pub const FPS_MIN: u8 = 5;
pub const FPS_MAX: u8 = 60;
pub const FPS_DEFAULT: u8 = 30;

/// Consecutive failed writes before an animation gives up. One is a busy
/// EC (the worst single write measured was 13 ms, not an error); several in
/// a row is an interface that has gone away.
const FAILURES_BEFORE_STOPPING: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EffectKind {
    /// The colours, each zone its own, dimming to black and back.
    Breathing,
    /// Every zone the same hue, walking round the colour wheel.
    Spectrum,
    /// The colour wheel spread across the zones and moving.
    RainbowWave,
    /// A pulse of the first colour crossing the zones over the second.
    Wave,
    /// All zones fading from one colour of the list to the next. White and
    /// black is 255,255,255 to 0,0,0 and back.
    Fade,
}

impl EffectKind {
    pub const ALL: [Self; 5] = [
        Self::Breathing,
        Self::Spectrum,
        Self::RainbowWave,
        Self::Wave,
        Self::Fade,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Breathing => "breathing",
            Self::Spectrum => "spectrum",
            Self::RainbowWave => "rainbowWave",
            Self::Wave => "wave",
            Self::Fade => "fade",
        }
    }

    /// Whether the colours mean anything to it. The two rainbows make
    /// their own, and a UI should not offer a picker that does nothing.
    pub fn uses_colors(self) -> bool {
        !matches!(self, Self::Spectrum | Self::RainbowWave)
    }

    /// What it runs with when it was given no colours.
    fn default_colors(self) -> Vec<Rgb> {
        match self {
            Self::Breathing => vec![Rgb::new(0xff, 0xff, 0xff)],
            Self::Wave => vec![Rgb::new(0x00, 0x8c, 0xff), Rgb::BLACK],
            Self::Fade => vec![Rgb::new(0xff, 0xff, 0xff), Rgb::BLACK],
            Self::Spectrum | Self::RainbowWave => Vec::new(),
        }
    }

    /// One cycle at the middle speed, in seconds.
    fn base_period(self) -> f64 {
        match self {
            Self::Breathing => 4.0,
            Self::Spectrum => 8.0,
            Self::RainbowWave => 4.0,
            Self::Wave => 2.0,
            // Per colour, not per trip round the list.
            Self::Fade => 3.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    #[default]
    LeftToRight,
    RightToLeft,
}

/// One effect, as it is stored and sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Effect {
    pub kind: EffectKind,
    #[serde(default)]
    pub colors: Vec<Rgb>,
    #[serde(default = "default_speed")]
    pub speed: u8,
    #[serde(default)]
    pub direction: Direction,
}

fn default_speed() -> u8 {
    SPEED_DEFAULT
}

impl Effect {
    pub fn new(kind: EffectKind) -> Self {
        Self {
            kind,
            colors: Vec::new(),
            speed: SPEED_DEFAULT,
            direction: Direction::default(),
        }
    }

    /// Squared up to what [`frame`] can run: speed in range, a bounded
    /// colour list, and the effect's own colours where none were given.
    pub fn normalised(mut self) -> Self {
        self.speed = self.speed.clamp(SPEED_MIN, SPEED_MAX);
        self.colors.truncate(MAX_COLORS);
        if self.colors.is_empty() {
            self.colors = self.kind.default_colors();
        }
        self
    }

    /// One cycle, in seconds, at this effect's speed. Speed 5 is the base
    /// period, 10 is twice as fast and 1 five times slower.
    fn period(&self) -> f64 {
        self.kind.base_period() * f64::from(SPEED_DEFAULT) / f64::from(self.speed.max(SPEED_MIN))
    }

    /// Where zone `z` sits along the direction of travel.
    fn position(&self, zone: usize) -> f64 {
        match self.direction {
            Direction::LeftToRight => zone as f64,
            Direction::RightToLeft => (ZONES - 1 - zone) as f64,
        }
    }
}

pub fn clamp_fps(value: i64) -> u8 {
    value.clamp(i64::from(FPS_MIN), i64::from(FPS_MAX)) as u8
}

/// The four zones at `t` seconds into `effect`. Pure: the same effect and
/// time always give the same frame.
///
/// Expects an effect that has been through [`Effect::normalised`]; an empty
/// colour list is read as black rather than panicking.
pub fn frame(effect: &Effect, t: f64) -> [Rgb; ZONES] {
    let period = effect.period();
    // The fractional part of the cycle, taken in f64 before anything else:
    // `t` grows for as long as the daemon runs, and a phase computed from
    // a large float in f32 starts to stutter after a few hours.
    let phase = (t / period).rem_euclid(1.0);
    let color = |i: usize| {
        if effect.colors.is_empty() {
            Rgb::BLACK
        } else {
            effect.colors[i % effect.colors.len()]
        }
    };

    let mut out = [Rgb::BLACK; ZONES];
    match effect.kind {
        EffectKind::Breathing => {
            // Starts bright, so switching to it does not open on a flash
            // of black.
            let level = (1.0 + (phase * TAU).cos()) / 2.0;
            for (z, zone) in out.iter_mut().enumerate() {
                *zone = dim(color(z), level);
            }
        }
        EffectKind::Spectrum => {
            out = [hue(phase); ZONES];
        }
        EffectKind::RainbowWave => {
            for (z, zone) in out.iter_mut().enumerate() {
                *zone = hue(phase - effect.position(z) / ZONES as f64);
            }
        }
        EffectKind::Wave => {
            // The pulse starts a little before the first zone and ends a
            // little past the last, so it enters and leaves rather than
            // popping into existence on one side.
            const MARGIN: f64 = 0.75;
            const WIDTH: f64 = 0.5;
            let span = (ZONES - 1) as f64 + 2.0 * MARGIN;
            let centre = phase * span - MARGIN;
            let (front, back) = (
                color(0),
                if effect.colors.len() > 1 {
                    color(1)
                } else {
                    Rgb::BLACK
                },
            );
            for (z, zone) in out.iter_mut().enumerate() {
                let d = effect.position(z) - centre;
                *zone = mix(back, front, (-(d * d) / WIDTH).exp());
            }
        }
        EffectKind::Fade => {
            let c = if effect.colors.len() <= 1 {
                // A single colour fades to black and back, which is the
                // only reading of "fade" that does something with one.
                dim(color(0), (1.0 + (phase * TAU).cos()) / 2.0)
            } else {
                // Through the whole list, one period per colour.
                let along = (t / period).rem_euclid(effect.colors.len() as f64);
                let from = along.floor() as usize;
                mix(color(from), color(from + 1), smooth(along - from as f64))
            };
            out = [c; ZONES];
        }
    }
    out
}

/// Ease in and out, so a fade does not start and stop with a jolt.
fn smooth(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn channel(v: f64) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

fn dim(c: Rgb, level: f64) -> Rgb {
    Rgb::new(
        channel(f64::from(c.r) * level),
        channel(f64::from(c.g) * level),
        channel(f64::from(c.b) * level),
    )
}

/// `a` at 0, `b` at 1.
fn mix(a: Rgb, b: Rgb, k: f64) -> Rgb {
    let at = |x: u8, y: u8| channel(f64::from(x) + (f64::from(y) - f64::from(x)) * k);
    Rgb::new(at(a.r, b.r), at(a.g, b.g), at(a.b, b.b))
}

/// Full saturation and value, hue in turns (0..1 is once round).
fn hue(turns: f64) -> Rgb {
    let h = turns.rem_euclid(1.0) * 6.0;
    let x = 1.0 - ((h % 2.0) - 1.0).abs();
    let (r, g, b) = match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    Rgb::new(channel(r * 255.0), channel(g * 255.0), channel(b * 255.0))
}

/// How far apart the zones start in a power sweep, as a fraction of it.
/// 0.2 with four zones: each zone takes 40 % of the sweep, the last one
/// starting when the first has finished.
const STAGGER: f64 = 0.2;

/// The two animations that are not effects: the lights coming on and
/// going off. They run once, over [`Transition::DURATION`], and end on
/// a colour rather than looping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Transition {
    /// Black to the lights, zone by zone from the left.
    PowerOn,
    /// The lights to black, zone by zone from the right - the way back
    /// out of [`Transition::PowerOn`].
    PowerOff,
}

impl Transition {
    /// Long enough to read as a sweep, short enough not to hold up a
    /// suspend or a shutdown noticeably.
    pub const DURATION: Duration = Duration::from_millis(1200);

    /// How lit zone `zone` is at `k` (0..1) through the transition.
    pub fn level(self, k: f64, zone: usize) -> f64 {
        let sweep = |position: f64| {
            let each = 1.0 - (ZONES - 1) as f64 * STAGGER;
            smooth((k - position * STAGGER) / each)
        };
        match self {
            Self::PowerOn => sweep(zone as f64),
            Self::PowerOff => 1.0 - sweep((ZONES - 1 - zone) as f64),
        }
    }

    /// `colors` as they look at `k` through the transition.
    pub fn apply(self, colors: &[Rgb; ZONES], k: f64) -> [Rgb; ZONES] {
        let mut out = *colors;
        for (zone, c) in out.iter_mut().enumerate() {
            *c = dim(*c, self.level(k, zone));
        }
        out
    }
}

/// Plays a finite animation, blocking until it is over: `frames` is given
/// the progress (0..1) and the seconds since it began, and the frame at
/// exactly 1 is always the last one written, however the timing fell.
///
/// Blocking on purpose. Its callers - a shutdown, a suspend hook - are
/// waiting for the lights to be in their final state before going on.
pub fn play<S: Sink>(
    sink: &mut S,
    brightness: u8,
    fps: u8,
    duration: Duration,
    mut frames: impl FnMut(f64, f64) -> [Rgb; ZONES],
) -> Result<(), DialectError> {
    // Every frame of this scales to black, so the thirty writes and the
    // second they take buy nothing. The one write still happens: it is
    // what puts the lights where the sweep would have left them.
    if brightness == 0 {
        return jump(sink, 0, duration, frames);
    }
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps.clamp(FPS_MIN, FPS_MAX)));
    let start = Instant::now();
    let mut next = start;
    loop {
        let t = start.elapsed().as_secs_f64();
        let k = (t / duration.as_secs_f64().max(f64::EPSILON)).min(1.0);
        sink.show(&frames(k, t), brightness)?;
        if k >= 1.0 {
            return Ok(());
        }
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
    }
}

/// Where a sweep would have ended, written once: [`play`] without the
/// frames in between.
///
/// The end of a sweep is not decoration - it is the lights being on or
/// off - so it is written whatever the machine thinks of animations. What
/// the machine gets a say in is the second of frames leading up to it,
/// and this is the answer when that second is not worth spending: the lid
/// is shut and nobody is looking, or the brightness is at zero and every
/// frame of it is black.
pub fn jump<S: Sink>(
    sink: &mut S,
    brightness: u8,
    duration: Duration,
    mut frames: impl FnMut(f64, f64) -> [Rgb; ZONES],
) -> Result<(), DialectError> {
    sink.show(&frames(1.0, duration.as_secs_f64()), brightness)
}

/// Where frames go. The daemon's is [`crate::dialect::FrameSink`]; the
/// tests' records them.
///
/// `colors` arrive unscaled with the brightness beside them, because one
/// dialect has a brightness field of its own - see
/// [`crate::dialect::Dialect::write_colors`].
pub trait Sink: Send + 'static {
    fn show(&mut self, colors: &[Rgb], brightness: u8) -> Result<(), DialectError>;
}

/// How often a paused animation looks to see whether it may carry on.
const PAUSED_POLL: Duration = Duration::from_millis(250);

/// Limits the machine puts on an animation, as opposed to what the user
/// asked for: fewer frames on battery, none with the lid shut. Shared with
/// the thread and read every frame, so a change takes effect at once and
/// the effect does not restart.
#[derive(Debug)]
struct Throttle {
    /// 0 is no limit.
    max_fps: AtomicU8,
    /// Nothing is written while set; the keyboard keeps the last frame.
    paused: AtomicBool,
}

/// A running animation: the thread that paces [`frame`] into a [`Sink`].
///
/// At most one runs. Starting another stops this one first, and dropping
/// the animator stops it - so a module that forgets its animator cannot
/// leave a thread writing to the lights.
pub struct Animator {
    running: Option<Running>,
    /// Outlives any one animation: the lid is still shut when the next
    /// effect starts.
    throttle: Arc<Throttle>,
}

impl Default for Animator {
    fn default() -> Self {
        Self {
            running: None,
            throttle: Arc::new(Throttle {
                max_fps: AtomicU8::new(0),
                paused: AtomicBool::new(false),
            }),
        }
    }
}

struct Running {
    effect: Effect,
    started: Instant,
    stop: Sender<()>,
    handle: JoinHandle<()>,
    /// Read by the thread every frame, so a brightness slider moves the
    /// running effect instead of restarting it from the top.
    brightness: Arc<AtomicU8>,
}

impl Animator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `effect` until [`Animator::stop`], or until writes keep
    /// failing - in which case `on_failure` is told why, once, from the
    /// animation's thread.
    pub fn start<S, F>(
        &mut self,
        effect: Effect,
        brightness: u8,
        fps: u8,
        mut sink: S,
        on_failure: F,
    ) where
        S: Sink,
        F: FnOnce(DialectError) + Send + 'static,
    {
        self.stop();
        let effect = effect.normalised();
        let fps = fps.clamp(FPS_MIN, FPS_MAX);
        let throttle = Arc::clone(&self.throttle);
        let (stop, stopped) = mpsc::channel::<()>();
        let level = Arc::new(AtomicU8::new(brightness.min(100)));
        let shared = Arc::clone(&level);
        let started = Instant::now();
        let running = effect.clone();

        let handle = std::thread::Builder::new()
            .name("pyren-rgb-effect".into())
            .spawn(move || {
                let start = started;
                let mut next = start;
                let mut failures = 0;
                // Whether the one black frame that zero brightness needs
                // has been written. Cleared as soon as there is something
                // to show again, so the next zero writes it once more.
                let mut blanked = false;
                loop {
                    if throttle.paused.load(Ordering::Relaxed) {
                        match stopped.recv_timeout(PAUSED_POLL) {
                            Err(RecvTimeoutError::Timeout) => {
                                next = Instant::now();
                                continue;
                            }
                            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    let level = shared.load(Ordering::Relaxed);
                    // Nothing this effect can draw is visible at zero, so
                    // neither the frame nor the write is worth doing: one
                    // black frame puts the keyboard out, and then this
                    // costs four wake-ups a second until the brightness
                    // comes back.
                    //
                    // The test is the brightness *setting*, never the
                    // frame: breathing and fade are black at the bottom of
                    // every cycle, and an effect that stopped writing
                    // there would never come back up. The clock is not
                    // stopped either, so what resumes is where the effect
                    // would have been, not where it was left.
                    if level == 0 {
                        if !blanked {
                            match sink.show(&[Rgb::BLACK; ZONES], 0) {
                                Ok(()) => {
                                    failures = 0;
                                    blanked = true;
                                }
                                Err(e) => {
                                    failures += 1;
                                    if failures >= FAILURES_BEFORE_STOPPING {
                                        on_failure(e);
                                        return;
                                    }
                                }
                            }
                        }
                        match stopped.recv_timeout(PAUSED_POLL) {
                            Err(RecvTimeoutError::Timeout) => {
                                next = Instant::now();
                                continue;
                            }
                            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    blanked = false;
                    let limit = throttle.max_fps.load(Ordering::Relaxed);
                    let rate = if limit == 0 { fps } else { fps.min(limit) };
                    let interval = Duration::from_secs_f64(1.0 / f64::from(rate));
                    let t = start.elapsed().as_secs_f64();
                    match sink.show(&frame(&effect, t), level) {
                        Ok(()) => failures = 0,
                        Err(e) => {
                            failures += 1;
                            if failures >= FAILURES_BEFORE_STOPPING {
                                on_failure(e);
                                return;
                            }
                        }
                    }
                    next += interval;
                    let now = Instant::now();
                    // A frame that took longer than the interval is not
                    // made up for with a burst: the schedule moves on.
                    if next < now {
                        next = now;
                    }
                    match stopped.recv_timeout(next - now) {
                        Err(RecvTimeoutError::Timeout) => {}
                        // A stop, or the animator gone.
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            });

        match handle {
            Ok(handle) => {
                self.running = Some(Running {
                    effect: running,
                    started,
                    stop,
                    handle,
                    brightness: level,
                })
            }
            Err(e) => on_failure_spawn(e),
        }
    }

    /// Stops the animation and waits for its thread, so that when this
    /// returns no frame of it can still land on top of whatever is written
    /// next.
    pub fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = running.stop.send(());
            let _ = running.handle.join();
        }
    }

    /// Changes the brightness of the running effect from its next frame.
    /// Does nothing when nothing is running.
    pub fn set_brightness(&self, brightness: u8) {
        if let Some(running) = &self.running {
            running
                .brightness
                .store(brightness.min(100), Ordering::Relaxed);
        }
    }

    /// Caps the frame rate below what was asked for (`None` lifts the cap).
    /// Applies to the running effect from its next frame and to every
    /// effect started after.
    pub fn set_limit(&self, max_fps: Option<u8>) {
        let value = max_fps.map_or(0, |f| f.clamp(1, FPS_MAX));
        self.throttle.max_fps.store(value, Ordering::Relaxed);
    }

    /// Stops writing frames without stopping the animation: the keyboard
    /// keeps the last one, and the effect carries on from where its clock
    /// has got to when it resumes.
    pub fn set_paused(&self, paused: bool) {
        self.throttle.paused.store(paused, Ordering::Relaxed);
    }

    /// The effect that is running and how far into it, in seconds - what a
    /// transition needs to carry on drawing it while it fades.
    pub fn current(&self) -> Option<(Effect, f64)> {
        self.running
            .as_ref()
            .filter(|r| !r.handle.is_finished())
            .map(|r| (r.effect.clone(), r.started.elapsed().as_secs_f64()))
    }

    /// Whether a thread is still writing frames. False after a stop, and
    /// after an animation gave up on its own.
    pub fn is_running(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|r| !r.handle.is_finished())
    }
}

impl Drop for Animator {
    fn drop(&mut self) {
        self.stop();
    }
}

fn on_failure_spawn(e: std::io::Error) {
    pyren_core::log_warn!("could not start the lighting effect thread: {e}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn effect(kind: EffectKind) -> Effect {
        Effect::new(kind).normalised()
    }

    const WHITE: Rgb = Rgb {
        r: 255,
        g: 255,
        b: 255,
    };

    #[test]
    fn every_kind_round_trips_through_its_id() {
        for kind in EffectKind::ALL {
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json, kind.id());
            assert_eq!(serde_json::from_value::<EffectKind>(json).unwrap(), kind);
        }
    }

    #[test]
    fn a_request_with_only_a_kind_is_a_whole_effect() {
        let e: Effect = serde_json::from_value(serde_json::json!({ "kind": "wave" })).unwrap();
        assert_eq!(e.speed, SPEED_DEFAULT);
        assert_eq!(e.direction, Direction::LeftToRight);
        assert_eq!(
            e.normalised().colors.len(),
            2,
            "wave gets its default colours"
        );
    }

    #[test]
    fn normalising_bounds_what_a_request_can_ask_for() {
        let mut e = Effect::new(EffectKind::Fade);
        e.speed = 99;
        e.colors = vec![WHITE; 50];
        let e = e.normalised();
        assert_eq!(e.speed, SPEED_MAX);
        assert_eq!(e.colors.len(), MAX_COLORS);

        let mut slow = Effect::new(EffectKind::Fade);
        slow.speed = 0;
        assert_eq!(slow.normalised().speed, SPEED_MIN);
    }

    #[test]
    fn breathing_starts_bright_and_is_dark_half_a_cycle_later() {
        let e = effect(EffectKind::Breathing);
        assert_eq!(frame(&e, 0.0), [WHITE; ZONES]);
        let half = e.period() / 2.0;
        assert_eq!(frame(&e, half), [Rgb::BLACK; ZONES]);
        assert_eq!(frame(&e, e.period()), [WHITE; ZONES], "and it is a cycle");
    }

    #[test]
    fn fade_goes_from_white_to_black_and_back() {
        let e = effect(EffectKind::Fade);
        assert_eq!(frame(&e, 0.0), [WHITE; ZONES]);
        assert_eq!(
            frame(&e, e.period() * 0.999)[0].r,
            0,
            "black by the end of the first step"
        );
        let middle = frame(&e, e.period() * 0.5)[0];
        assert!(
            (120..=135).contains(&middle.r),
            "grey half way, got {middle:?}"
        );
        assert_eq!(
            frame(&e, e.period() * 2.0),
            [WHITE; ZONES],
            "back to white after both"
        );
    }

    #[test]
    fn a_fade_with_one_colour_still_moves() {
        let mut e = Effect::new(EffectKind::Fade);
        e.colors = vec![Rgb::new(255, 0, 0)];
        let e = e.normalised();
        assert_ne!(frame(&e, 0.0), frame(&e, e.period() / 2.0));
    }

    #[test]
    fn spectrum_is_one_colour_across_the_keyboard() {
        let e = effect(EffectKind::Spectrum);
        let f = frame(&e, 1.3);
        assert!(f.iter().all(|c| *c == f[0]));
        assert_ne!(
            frame(&e, 0.0),
            frame(&e, e.period() / 3.0),
            "and it changes"
        );
    }

    #[test]
    fn the_rainbow_wave_gives_each_zone_a_different_hue() {
        let f = frame(&effect(EffectKind::RainbowWave), 0.0);
        for a in 0..ZONES {
            for b in a + 1..ZONES {
                assert_ne!(f[a], f[b], "zones {a} and {b}");
            }
        }
    }

    #[test]
    fn the_rainbow_wave_moves_the_way_it_was_told() {
        let mut e = effect(EffectKind::RainbowWave);
        let step = e.period() / ZONES as f64;
        // Left to right: what zone 0 shows now, zone 1 shows one step later.
        assert_eq!(frame(&e, 0.0)[0], frame(&e, step)[1]);
        e.direction = Direction::RightToLeft;
        assert_eq!(frame(&e, 0.0)[3], frame(&e, step)[2]);
    }

    #[test]
    fn the_wave_crosses_from_one_side_to_the_other() {
        let e = effect(EffectKind::Wave);
        let brightest = |t: f64| {
            let f = frame(&e, t);
            (0..ZONES).max_by_key(|&z| u32::from(f[z].b)).unwrap()
        };
        let p = e.period();
        let first = brightest(p * 0.2);
        let last = brightest(p * 0.8);
        assert!(first < last, "left to right: zone {first} then zone {last}");

        let mut back = e.clone();
        back.direction = Direction::RightToLeft;
        let f = frame(&back, p * 0.2);
        assert!(f[3].b > f[0].b, "right to left starts on the right");
    }

    #[test]
    fn a_higher_speed_is_a_shorter_cycle() {
        let mut slow = effect(EffectKind::Breathing);
        slow.speed = SPEED_MIN;
        let mut fast = slow.clone();
        fast.speed = SPEED_MAX;
        assert!(fast.period() < slow.period());
        assert_eq!(fast.period(), EffectKind::Breathing.base_period() / 2.0);
    }

    #[test]
    fn frames_stay_smooth_after_the_daemon_has_run_for_a_long_time() {
        let e = effect(EffectKind::Spectrum);
        let a_month = 30.0 * 24.0 * 3600.0;
        // Same phase, a month apart: the frame must not have drifted.
        assert_eq!(frame(&e, 0.0), frame(&e, a_month - a_month % e.period()));
    }

    #[test]
    fn hue_walks_the_primaries() {
        assert_eq!(hue(0.0), Rgb::new(255, 0, 0));
        assert_eq!(hue(1.0 / 3.0), Rgb::new(0, 255, 0));
        assert_eq!(hue(2.0 / 3.0), Rgb::new(0, 0, 255));
        assert_eq!(hue(-1.0 / 3.0), hue(2.0 / 3.0));
    }

    #[test]
    fn power_on_sweeps_in_from_the_left() {
        let lit = [WHITE; ZONES];
        assert_eq!(Transition::PowerOn.apply(&lit, 0.0), [Rgb::BLACK; ZONES]);
        assert_eq!(Transition::PowerOn.apply(&lit, 1.0), lit);
        let early = Transition::PowerOn.apply(&lit, 0.3);
        assert!(
            early[0].r > early[3].r,
            "the left comes on first: {early:?}"
        );
    }

    #[test]
    fn power_off_sweeps_out_from_the_right() {
        let lit = [WHITE; ZONES];
        assert_eq!(Transition::PowerOff.apply(&lit, 0.0), lit);
        assert_eq!(Transition::PowerOff.apply(&lit, 1.0), [Rgb::BLACK; ZONES]);
        let early = Transition::PowerOff.apply(&lit, 0.3);
        assert!(early[3].r < early[0].r, "the right goes first: {early:?}");
    }

    #[test]
    fn every_zone_moves_monotonically_through_a_transition() {
        for transition in [Transition::PowerOn, Transition::PowerOff] {
            for zone in 0..ZONES {
                let levels: Vec<f64> = (0..=100)
                    .map(|i| transition.level(i as f64 / 100.0, zone))
                    .collect();
                let rising = transition == Transition::PowerOn;
                assert!(
                    levels
                        .windows(2)
                        .all(|w| if rising { w[1] >= w[0] } else { w[1] <= w[0] }),
                    "{transition:?} zone {zone}"
                );
            }
        }
    }

    #[test]
    fn playing_a_transition_ends_on_its_last_frame_and_returns() {
        let mut sink = Recorder::default();
        let lit = [WHITE; ZONES];
        let begun = Instant::now();
        play(
            &mut sink,
            90,
            FPS_MAX,
            Duration::from_millis(100),
            |k, _| Transition::PowerOn.apply(&lit, k),
        )
        .unwrap();
        assert!(begun.elapsed() >= Duration::from_millis(100));
        let frames = sink.frames.lock().unwrap();
        assert!(frames.len() >= 3);
        assert_eq!(frames.first().unwrap().0, vec![Rgb::BLACK; ZONES]);
        assert_eq!(
            frames.last().unwrap().0,
            lit.to_vec(),
            "always ends on k = 1"
        );
        assert!(frames.iter().all(|(_, b)| *b == 90));
    }

    #[test]
    fn the_running_effect_can_be_asked_for() {
        let mut animator = Animator::new();
        assert!(animator.current().is_none());
        animator.start(
            effect(EffectKind::Wave),
            100,
            FPS_MAX,
            Recorder::default(),
            |_| {},
        );
        std::thread::sleep(Duration::from_millis(30));
        let (running, t) = animator.current().expect("it is running");
        assert_eq!(running.kind, EffectKind::Wave);
        assert!(t > 0.0);
        animator.stop();
        assert!(animator.current().is_none());
    }

    /// Each frame's colours, and the brightness it came with.
    type Frames = Arc<Mutex<Vec<(Vec<Rgb>, u8)>>>;

    /// Records every frame, and fails on demand.
    #[derive(Clone, Default)]
    struct Recorder {
        frames: Frames,
        fail: Arc<Mutex<bool>>,
    }

    impl Sink for Recorder {
        fn show(&mut self, colors: &[Rgb], brightness: u8) -> Result<(), DialectError> {
            if *self.fail.lock().unwrap() {
                return Err(DialectError::Io("gone".into()));
            }
            self.frames
                .lock()
                .unwrap()
                .push((colors.to_vec(), brightness));
            Ok(())
        }
    }

    #[test]
    fn an_animation_writes_frames_until_it_is_stopped() {
        let sink = Recorder::default();
        let mut animator = Animator::new();
        animator.start(
            effect(EffectKind::Spectrum),
            70,
            FPS_MAX,
            sink.clone(),
            |_| {},
        );
        std::thread::sleep(Duration::from_millis(150));
        assert!(animator.is_running());
        animator.stop();
        assert!(!animator.is_running());

        let written = sink.frames.lock().unwrap().len();
        assert!(written >= 3, "{written} frames in 150 ms at 60 fps");
        assert!(
            sink.frames.lock().unwrap().iter().all(|(_, b)| *b == 70),
            "brightness is passed through"
        );

        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            sink.frames.lock().unwrap().len(),
            written,
            "nothing lands after stop returns"
        );
    }

    #[test]
    fn brightness_moves_a_running_effect_without_restarting_it() {
        let sink = Recorder::default();
        let mut animator = Animator::new();
        animator.start(
            effect(EffectKind::Spectrum),
            100,
            FPS_MAX,
            sink.clone(),
            |_| {},
        );
        std::thread::sleep(Duration::from_millis(60));
        animator.set_brightness(20);
        std::thread::sleep(Duration::from_millis(60));
        animator.stop();
        let frames = sink.frames.lock().unwrap();
        assert_eq!(frames.first().unwrap().1, 100);
        assert_eq!(frames.last().unwrap().1, 20);
    }

    #[test]
    fn a_paused_animation_writes_nothing_until_it_is_let_go() {
        let sink = Recorder::default();
        let mut animator = Animator::new();
        animator.set_paused(true);
        animator.start(
            effect(EffectKind::Spectrum),
            100,
            FPS_MAX,
            sink.clone(),
            |_| {},
        );
        std::thread::sleep(Duration::from_millis(80));
        assert!(sink.frames.lock().unwrap().is_empty());
        assert!(animator.is_running(), "paused is not stopped");
        animator.set_paused(false);
        std::thread::sleep(Duration::from_millis(400));
        assert!(!sink.frames.lock().unwrap().is_empty());
    }

    #[test]
    fn a_frame_rate_limit_slows_a_running_effect() {
        let sink = Recorder::default();
        let mut animator = Animator::new();
        animator.set_limit(Some(10));
        animator.start(
            effect(EffectKind::Spectrum),
            100,
            FPS_MAX,
            sink.clone(),
            |_| {},
        );
        std::thread::sleep(Duration::from_millis(500));
        animator.stop();
        let written = sink.frames.lock().unwrap().len();
        assert!(
            (3..=8).contains(&written),
            "{written} frames in 500 ms capped at 10 fps"
        );
    }

    #[test]
    fn starting_another_effect_replaces_the_first() {
        let first = Recorder::default();
        let second = Recorder::default();
        let mut animator = Animator::new();
        animator.start(
            effect(EffectKind::Spectrum),
            100,
            FPS_MAX,
            first.clone(),
            |_| {},
        );
        std::thread::sleep(Duration::from_millis(50));
        animator.start(
            effect(EffectKind::Wave),
            100,
            FPS_MAX,
            second.clone(),
            |_| {},
        );
        let before = first.frames.lock().unwrap().len();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            first.frames.lock().unwrap().len(),
            before,
            "the first one is stopped"
        );
        assert!(!second.frames.lock().unwrap().is_empty());
    }

    /// The saving that zero brightness is for: at 0 % every frame of every
    /// effect scales to black, so computing and writing 60 of them a
    /// second is 60 ACPI transactions to keep a keyboard dark.
    #[test]
    fn zero_brightness_writes_one_black_frame_and_then_nothing() {
        let sink = Recorder::default();
        let mut animator = Animator::new();
        animator.start(
            effect(EffectKind::RainbowWave),
            0,
            FPS_MAX,
            sink.clone(),
            |_| {},
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(animator.is_running(), "idle is not stopped");
        {
            let frames = sink.frames.lock().unwrap();
            assert_eq!(
                frames.len(),
                1,
                "200 ms at 60 fps would otherwise be a dozen"
            );
            assert_eq!(
                frames[0],
                (vec![Rgb::BLACK; ZONES], 0),
                "the lights are actually put out"
            );
        }

        // And it comes straight back, without the effect restarting: the
        // clock kept running under the idle.
        animator.set_brightness(80);
        std::thread::sleep(Duration::from_millis(300));
        animator.stop();
        let frames = sink.frames.lock().unwrap();
        assert!(
            frames.len() > 3,
            "{} frames once the brightness is back",
            frames.len()
        );
        assert_eq!(frames.last().unwrap().1, 80);
    }

    /// The trap in the optimisation above. Breathing and fade are black at
    /// the bottom of every cycle; if "the frame is black" were what
    /// stopped the writes, they would go out at the first trough and never
    /// come back.
    #[test]
    fn an_effect_that_passes_through_black_keeps_being_drawn() {
        let sink = Recorder::default();
        let mut animator = Animator::new();
        // Speed 10 is the shortest cycle: two seconds, so the trough is
        // at one and this runs well past it.
        let mut breathing = effect(EffectKind::Breathing);
        breathing.speed = SPEED_MAX;
        animator.start(breathing, 100, FPS_MAX, sink.clone(), |_| {});
        std::thread::sleep(Duration::from_millis(1300));
        animator.stop();
        let frames = sink.frames.lock().unwrap();
        assert!(
            frames.iter().all(|(_, b)| *b == 100),
            "the brightness never moved"
        );
        let dark = frames
            .iter()
            .position(|(c, _)| c.iter().all(|z| *z == Rgb::BLACK))
            .expect("the cycle does reach black");
        assert!(
            frames[dark..]
                .iter()
                .any(|(c, _)| c.iter().any(|z| *z != Rgb::BLACK)),
            "and comes back up out of it"
        );
    }

    #[test]
    fn a_sweep_nobody_can_see_is_its_last_frame_and_nothing_else() {
        let mut sink = Recorder::default();
        let lit = [WHITE; ZONES];
        let begun = Instant::now();
        jump(&mut sink, 75, Duration::from_millis(200), |k, _| {
            Transition::PowerOff.apply(&lit, k)
        })
        .unwrap();
        assert!(
            begun.elapsed() < Duration::from_millis(100),
            "it does not wait out the sweep"
        );
        let frames = sink.frames.lock().unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0],
            (vec![Rgb::BLACK; ZONES], 75),
            "the lights are out, which was the point"
        );
    }

    /// The same saving on the power sweep: a sweep whose every frame is
    /// black is one write, not a second of them.
    #[test]
    fn a_sweep_at_zero_brightness_is_a_single_write() {
        let mut sink = Recorder::default();
        let lit = [WHITE; ZONES];
        let begun = Instant::now();
        play(&mut sink, 0, FPS_MAX, Duration::from_millis(200), |k, _| {
            Transition::PowerOn.apply(&lit, k)
        })
        .unwrap();
        assert!(
            begun.elapsed() < Duration::from_millis(100),
            "it does not wait out the sweep"
        );
        let frames = sink.frames.lock().unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].0,
            lit.to_vec(),
            "the frame the sweep would have ended on"
        );
        assert_eq!(
            frames[0].1, 0,
            "scaled to black by the brightness beside it"
        );
    }

    #[test]
    fn an_animation_whose_writes_keep_failing_stops_and_says_why() {
        let sink = Recorder::default();
        *sink.fail.lock().unwrap() = true;
        let (tell, told) = mpsc::channel();
        let mut animator = Animator::new();
        animator.start(
            effect(EffectKind::Breathing),
            100,
            FPS_MAX,
            sink,
            move |e| {
                let _ = tell.send(e.to_string());
            },
        );
        let why = told
            .recv_timeout(Duration::from_secs(2))
            .expect("the failure is reported");
        assert!(why.contains("gone"));
        std::thread::sleep(Duration::from_millis(20));
        assert!(!animator.is_running());
    }
}
