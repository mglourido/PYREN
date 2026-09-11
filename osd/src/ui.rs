//! The widget itself: four cards on a dark panel, in the middle of the
//! screen, over everything else.
//!
//! ## Why this is a layer surface and not a window
//!
//! An ordinary toplevel cannot do this job on Wayland. It cannot place
//! itself in the middle of the screen (the compositor decides), it cannot
//! stay above a fullscreen game, and showing it takes the keyboard away
//! from whatever the user was doing. `wlr-layer-shell` is the protocol for
//! exactly this: a surface on the overlay layer, centred by the compositor,
//! that takes the keyboard only when clicked.
//!
//! Where layer-shell is missing - X11, GNOME - the window is presented as
//! an ordinary always-on-top dialog instead. It is a worse OSD, and it is
//! not nothing.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{glib, Align, Application, ApplicationWindow, Orientation, PositionType, Scale};

use crate::daemon::{pwm_from_percent, Command};
use crate::icon;
use crate::mode::{self, FanMode, Lang, Mode};

/// How long the widget stays up after a press when the pointer never comes
/// near it. Matched to the vendor's: long enough to press the key again and
/// watch the highlight move, short enough not to sit on top of a game. Once
/// the pointer is on the widget it stays up for as long as the pointer is,
/// and these timers do not run - see `stay` and the motion controller.
const LINGER: Duration = Duration::from_millis(2500);

/// Longer when there is something to read - a refusal is a sentence, and
/// two and a half seconds is not enough to read a sentence and understand
/// that it was about the key you just pressed.
const LINGER_WITH_MESSAGE: Duration = Duration::from_millis(6000);

/// After the pointer leaves the widget. Short: the pointer leaving is the
/// clearest "I am done here" there is.
const HIDE_AFTER_LEAVE: Duration = Duration::from_millis(1000);

/// After a card is clicked, if the pointer then sits still on the widget.
/// A twitch inside cancels it - that reads as "still choosing" - but a
/// pick followed by nothing means get out of the way.
const HIDE_AFTER_CLICK: Duration = Duration::from_millis(2000);

const ICON: f64 = 34.0;

/// How long the manual slider waits after the last drag before the speed
/// goes to the daemon. Long enough that dragging across the track is one
/// call, not forty.
const MANUAL_PUSH_DELAY: Duration = Duration::from_millis(250);

const CSS: &str = "
window.osd { background: transparent; }

.panel {
  background: rgba(11, 11, 13, 0.94);
  border: 1px solid rgba(255, 255, 255, 0.07);
  border-radius: 4px;
  padding: 22px 26px;
}

.title {
  color: #7c7c86;
  font-size: 11px;
  font-weight: 700;
  letter-spacing: 2px;
}

.mode {
  background: #232326;
  border: 1px solid transparent;
  border-radius: 2px;
  padding: 14px 8px 0 8px;
  min-width: 132px;
  color: #7c7c86;
}

.mode:hover { background: #2b2b2f; color: #b6b6bd; }

.mode.on {
  background: #141416;
  color: #ffffff;
}

.mode label { font-size: 14px; }

/* The OMEN signature gradient, on the selected card only. A bar rather
   than a border: GTK has no border-image, and a solid red outline is not
   the same mark. */
.bar {
  min-height: 3px;
  background-image: linear-gradient(90deg, #e5178c 0%, #f2374b 50%, #ff8a00 100%);
  opacity: 0;
}
.mode.on .bar { opacity: 1; }

.desc { color: #b6b6bd; font-size: 12px; }
.warn { color: #ff8a00; font-size: 12px; }

/* The fan row reads as its own section: a rule above its title. */
.section {
  border-top: 1px solid rgba(255, 255, 255, 0.07);
  padding-top: 14px;
}

/* The manual-speed slider. Dark to match the cards; the OMEN gradient on
   the filled part, so it belongs to the same widget as the selected card's
   bar. */
scale.manual trough {
  background: #232326;
  border: 1px solid rgba(255, 255, 255, 0.07);
  min-height: 6px;
}
scale.manual highlight {
  background-image: linear-gradient(90deg, #e5178c 0%, #f2374b 50%, #ff8a00 100%);
}
scale.manual slider {
  background: #d8d8de;
  min-height: 16px;
  min-width: 16px;
}
scale.manual value { color: #b6b6bd; font-size: 12px; }
";

struct Card {
    mode: Mode,
    button: gtk4::Button,
    glyph: gtk4::DrawingArea,
    selected: Rc<Cell<bool>>,
}

/// One card in the fan-mode row. The same shape as `Card`, over `FanMode`.
struct FanCard {
    mode: FanMode,
    button: gtk4::Button,
    glyph: gtk4::DrawingArea,
    selected: Rc<Cell<bool>>,
}

pub struct Ui {
    window: ApplicationWindow,
    cards: Vec<Card>,
    description: gtk4::Label,
    message: gtk4::Label,
    hide: RefCell<Option<glib::SourceId>>,
    current: Cell<Option<Mode>>,
    lang: Lang,
    commands: mpsc::Sender<Command>,

    /// Whether the pointer is on the widget right now. While it is, the
    /// widget stays up and no hide countdown runs - the one exception is
    /// the short post-click countdown `after_click` arms, and even that is
    /// dropped the moment the pointer moves.
    pointer_inside: Cell<bool>,

    /// The power-mode row. Hidden only when the app turns it off *and* the
    /// fan row is up to take its place.
    power_section: gtk4::Box,
    /// The fan-mode row. `fan_section` is the whole titled block, shown
    /// only when the app's setting is on and the machine can switch modes.
    fan_section: gtk4::Box,
    fan_cards: Vec<FanCard>,
    fan_description: gtk4::Label,
    fan_slider_row: gtk4::Box,
    fan_slider: Scale,
    current_fan: Cell<Option<FanMode>>,
    /// `(switch_mode, set_speed)` from the last `fan.getStatus`.
    fan_caps: Cell<(bool, bool)>,
    /// The manual speed the slider shows, 0-100.
    fan_manual_percent: Cell<u8>,
    /// True while the code is moving the slider, so its `value-changed`
    /// handler does not treat that as the user dragging it.
    updating_slider: Cell<bool>,
    /// The pending debounced push of the manual speed to the daemon.
    manual_push: RefCell<Option<glib::SourceId>>,
}

impl Ui {
    pub fn build(app: &Application, lang: Lang, commands: mpsc::Sender<Command>) -> Rc<Self> {
        let provider = gtk4::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let window = ApplicationWindow::builder()
            .application(app)
            .decorated(false)
            .resizable(false)
            .css_classes(["osd"])
            .build();

        let panel = gtk4::Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(16)
            .css_classes(["panel"])
            .build();

        // The power-mode row, in its own box so it can be hidden as a unit
        // - the app allows that, but only while the fan row is showing, so
        // the widget is never left empty (`refresh_rows` enforces it too).
        let power_section = gtk4::Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(16)
            .build();

        let title = gtk4::Label::builder()
            .label(lang.title())
            .halign(Align::Start)
            .css_classes(["title"])
            .build();
        power_section.append(&title);

        let row = gtk4::Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(10)
            .halign(Align::Center)
            .build();
        let mut cards = Vec::new();
        for mode in Mode::ALL {
            let card = Self::card(mode, lang);
            row.append(&card.button);
            cards.push(card);
        }
        power_section.append(&row);

        // Fixed width and two lines' worth of height, both requested
        // rather than left to the content: the four descriptions are
        // different lengths, and a widget that changes size under the
        // cursor every time the key is pressed is a widget that flinches.
        let description = gtk4::Label::builder()
            .label("")
            .halign(Align::Start)
            .valign(Align::Start)
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(62)
            .width_request(560)
            .height_request(34)
            .css_classes(["desc"])
            .build();
        power_section.append(&description);
        panel.append(&power_section);

        // The fan-mode row. Built whatever the setting says - it is one
        // `set_visible` away either way - and hidden until the daemon
        // answers `fan.getStatus` and the app's `widgetFanModes` is on.
        let fan_section = gtk4::Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(12)
            .css_classes(["section"])
            .visible(false)
            .build();

        let fan_title = gtk4::Label::builder()
            .label(lang.fan_title())
            .halign(Align::Start)
            .css_classes(["title"])
            .build();
        fan_section.append(&fan_title);

        let fan_row = gtk4::Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(10)
            .halign(Align::Center)
            .build();
        let mut fan_cards = Vec::new();
        for mode in FanMode::ALL {
            let card = Self::fan_card(mode, lang);
            fan_row.append(&card.button);
            fan_cards.push(card);
        }
        fan_section.append(&fan_row);

        let fan_description = gtk4::Label::builder()
            .label("")
            .halign(Align::Start)
            .valign(Align::Start)
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(62)
            .width_request(560)
            .height_request(34)
            .css_classes(["desc"])
            .build();
        fan_section.append(&fan_description);

        // The manual-speed slider, on its own row so it can be hidden
        // without the row above it moving. Shown only in `manual`.
        let fan_slider_row = gtk4::Box::builder()
            .orientation(Orientation::Horizontal)
            .visible(false)
            .build();
        let fan_slider = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0);
        fan_slider.set_hexpand(true);
        fan_slider.set_width_request(560);
        fan_slider.set_draw_value(true);
        fan_slider.set_value_pos(PositionType::Right);
        fan_slider.set_css_classes(&["manual"]);
        fan_slider.set_format_value_func(|_, value| format!("{value:.0} %"));
        fan_slider_row.append(&fan_slider);
        fan_section.append(&fan_slider_row);

        panel.append(&fan_section);

        let message = gtk4::Label::builder()
            .label("")
            .halign(Align::Start)
            .wrap(true)
            .max_width_chars(64)
            .visible(false)
            .css_classes(["warn"])
            .build();
        panel.append(&message);

        window.set_child(Some(&panel));

        let ui = Rc::new(Self {
            window,
            cards,
            description,
            message,
            hide: RefCell::new(None),
            current: Cell::new(None),
            lang,
            commands,
            pointer_inside: Cell::new(false),
            power_section,
            fan_section,
            fan_cards,
            fan_description,
            fan_slider_row,
            fan_slider,
            current_fan: Cell::new(None),
            fan_caps: Cell::new((false, false)),
            fan_manual_percent: Cell::new(0),
            updating_slider: Cell::new(false),
            manual_push: RefCell::new(None),
        });

        // Clicking a card picks that mode outright rather than stepping
        // towards it: the widget is on screen and the user is pointing at
        // what they want.
        for index in 0..ui.cards.len() {
            let weak = Rc::downgrade(&ui);
            ui.cards[index].button.connect_clicked(move |_| {
                let Some(ui) = weak.upgrade() else { return };
                let mode = ui.cards[index].mode;
                ui.select(mode);
                let _ = ui.commands.send(Command::SetMode(mode));
                ui.after_click();
            });
        }

        // Same for the fan row. `manual` also carries the slider's speed,
        // since picking it from the firmware's own curve has to land the
        // fans somewhere.
        for index in 0..ui.fan_cards.len() {
            let weak = Rc::downgrade(&ui);
            ui.fan_cards[index].button.connect_clicked(move |_| {
                let Some(ui) = weak.upgrade() else { return };
                let mode = ui.fan_cards[index].mode;
                ui.select_fan(mode);
                let pwm = mode.needs_speed().then(|| pwm_from_percent(ui.fan_manual_percent.get()));
                let _ = ui.commands.send(Command::SetFanMode(mode, pwm));
                ui.after_click();
            });
        }

        // Dragging the slider: debounce, so a sweep is one call. The guard
        // keeps a programmatic `set_value` (when the daemon reports a
        // speed) from bouncing straight back.
        {
            let weak = Rc::downgrade(&ui);
            ui.fan_slider.connect_value_changed(move |scale| {
                let Some(ui) = weak.upgrade() else { return };
                if ui.updating_slider.get() {
                    return;
                }
                ui.fan_manual_percent.set(scale.value().round().clamp(0.0, 100.0) as u8);
                ui.schedule_manual_push();
                ui.stay(LINGER);
            });
        }

        // The pointer decides when the widget goes away: while it is on the
        // panel the widget stays, and leaving starts a one-second
        // countdown. Put on `panel` rather than `window` so the transparent
        // margin around it does not count as "inside".
        {
            let motion = gtk4::EventControllerMotion::new();
            let weak = Rc::downgrade(&ui);
            motion.connect_enter(move |_, _, _| {
                if let Some(ui) = weak.upgrade() {
                    ui.pointer_inside.set(true);
                    ui.cancel_hide();
                }
            });
            let weak = Rc::downgrade(&ui);
            motion.connect_leave(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.pointer_inside.set(false);
                    ui.arm_hide(HIDE_AFTER_LEAVE);
                }
            });
            let weak = Rc::downgrade(&ui);
            motion.connect_motion(move |_, _, _| {
                if let Some(ui) = weak.upgrade() {
                    ui.pointer_inside.set(true);
                    // Motion on the widget means it is in use: no countdown
                    // runs while the pointer is here - not the key press's
                    // fallback linger, and not a click's settle countdown
                    // (a move straight after a click reads as "still
                    // choosing").
                    ui.cancel_hide();
                }
            });
            panel.add_controller(motion);
        }

        ui.place_on_screen();
        ui
    }

    fn fan_card(mode: FanMode, lang: Lang) -> FanCard {
        let selected = Rc::new(Cell::new(false));

        let glyph = gtk4::DrawingArea::builder()
            .content_width(ICON as i32)
            .content_height(ICON as i32)
            .halign(Align::Center)
            .build();
        let is_selected = Rc::clone(&selected);
        glyph.set_draw_func(move |_, cr, width, height| {
            if is_selected.get() {
                cr.set_source_rgb(1.0, 1.0, 1.0);
            } else {
                cr.set_source_rgb(0.486, 0.486, 0.525); // #7c7c86
            }
            cr.set_line_width(1.4 * ICON / icon::VIEWBOX);
            cr.set_line_cap(gtk4::cairo::LineCap::Round);
            cr.set_line_join(gtk4::cairo::LineJoin::Round);
            cr.translate((f64::from(width) - ICON) / 2.0, (f64::from(height) - ICON) / 2.0);
            icon::draw(cr, mode.icon(), ICON);
        });

        let label = gtk4::Label::builder().label(mode.label(lang)).build();
        let bar = gtk4::Box::builder()
            .orientation(Orientation::Horizontal)
            .css_classes(["bar"])
            .build();

        let content = gtk4::Box::builder().orientation(Orientation::Vertical).spacing(10).build();
        content.append(&glyph);
        content.append(&label);
        content.append(&bar);

        let button =
            gtk4::Button::builder().child(&content).css_classes(["mode"]).can_focus(false).build();

        FanCard { mode, button, glyph, selected }
    }

    fn card(mode: Mode, lang: Lang) -> Card {
        let selected = Rc::new(Cell::new(false));

        let glyph = gtk4::DrawingArea::builder()
            .content_width(ICON as i32)
            .content_height(ICON as i32)
            .halign(Align::Center)
            .build();
        let is_selected = Rc::clone(&selected);
        glyph.set_draw_func(move |_, cr, width, height| {
            // The card's own colours, drawn rather than themed: a
            // DrawingArea has no text to inherit a CSS colour through.
            if is_selected.get() {
                cr.set_source_rgb(1.0, 1.0, 1.0);
            } else {
                cr.set_source_rgb(0.486, 0.486, 0.525); // #7c7c86
            }
            cr.set_line_width(1.4 * ICON / icon::VIEWBOX);
            cr.set_line_cap(gtk4::cairo::LineCap::Round);
            cr.set_line_join(gtk4::cairo::LineJoin::Round);
            cr.translate(
                (f64::from(width) - ICON) / 2.0,
                (f64::from(height) - ICON) / 2.0,
            );
            icon::draw(cr, mode.icon(), ICON);
        });

        let label = gtk4::Label::builder().label(mode.label(lang)).build();
        let bar = gtk4::Box::builder()
            .orientation(Orientation::Horizontal)
            .css_classes(["bar"])
            .build();

        let content = gtk4::Box::builder().orientation(Orientation::Vertical).spacing(10).build();
        content.append(&glyph);
        content.append(&label);
        content.append(&bar);

        let button =
            gtk4::Button::builder().child(&content).css_classes(["mode"]).can_focus(false).build();

        Card { mode, button, glyph, selected }
    }

    /// Puts the window where an OSD belongs: the overlay layer, centred,
    /// and not stealing the keyboard until it is clicked.
    fn place_on_screen(&self) {
        if !gtk4_layer_shell::is_supported() {
            // X11, or a Wayland compositor without the protocol. An
            // always-on-top window is what is left; say so once, because
            // "it appears behind my game" needs an explanation.
            eprintln!(
                "pyren-osd: no wlr-layer-shell here, falling back to an ordinary window; \
                 it will not stay above a fullscreen game"
            );
            return;
        }

        use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
        self.window.init_layer_shell();
        self.window.set_layer(Layer::Overlay);
        self.window.set_namespace(Some("pyren-osd"));
        // With no anchors the compositor centres the surface, which is
        // where the vendor's widget appears and what was asked for.
        //
        // `OnDemand` rather than `Exclusive`: the widget appears over
        // whatever the user was doing, and taking their keyboard away
        // mid-game to show them a power mode would be worse than the
        // problem it solves. Clicking it still works.
        self.window.set_keyboard_mode(KeyboardMode::OnDemand);
    }

    /// One press of the shortcut: the modes, with the one in force
    /// highlighted, and no claim that anything happened.
    ///
    /// Deliberately not `pressed(mode, true, None)`, which would read as
    /// "the change succeeded" - there was no change. The user picks from
    /// here by clicking, or lets it fade.
    ///
    /// **Pressing it again puts the widget away.** One key, both
    /// directions: the shortcut opens a thing that covers the middle of
    /// the screen, and the way to get rid of something you opened is the
    /// key you opened it with - not waiting out a timer.
    ///
    /// This is safe against the double event a bare vendor key produces
    /// (the same scancode on press *and* release, with nothing to tell
    /// them apart) only because the daemon coalesces those first, in
    /// `HotkeyConfig::repeat_guard_ms`. Without that guard one physical
    /// press would open and close the widget, and the key would look
    /// dead.
    pub fn show(self: &Rc<Self>, mode: Mode) {
        if self.window.is_visible() {
            self.dismiss();
            return;
        }
        self.select(mode);
        self.say(None);
        self.refresh_rows();
        self.stay(LINGER);
    }

    /// Puts the widget away now, and cancels the countdown that would
    /// have done it later.
    fn dismiss(&self) {
        self.cancel_hide();
        self.pointer_inside.set(false);
        self.window.set_visible(false);
    }

    /// One press of the performance key.
    pub fn pressed(self: &Rc<Self>, mode: Mode, changed: bool, refusal: Option<String>) {
        self.select(mode);

        match (changed, refusal) {
            (true, _) => self.say(None),
            // The mode did not move and the daemon said why. This is the
            // whole reason the widget reads the event instead of assuming
            // the press worked.
            (false, Some(why)) => self.say(Some(format!("{} {why}", self.lang.refused()))),
            (false, None) => self.say(Some(self.lang.refused().to_string())),
        }

        self.refresh_rows();
        self.stay(if self.message.is_visible() { LINGER_WITH_MESSAGE } else { LINGER });
    }

    /// The mode changed somewhere else - the app's performance page, the
    /// CLI, the daemon's supervisor.
    ///
    /// The highlight follows immediately. Whether that is *visible* depends
    /// on where the widget already was, and both halves are deliberate:
    ///
    /// - **Hidden: it stays hidden.** Clicking a mode in the app window is
    ///   not a request for a widget on top of the app window.
    /// - **Open: it updates, and the countdown restarts.** Somebody
    ///   changing modes in the app with the widget up is watching the
    ///   widget, and having it fade out mid-change would be the one moment
    ///   it should not.
    pub fn mode_is(self: &Rc<Self>, mode: Mode) {
        let already_showing = self.window.is_visible();
        self.select(mode);
        if already_showing {
            // Not `present()` on its own: `stay` is what restarts the
            // countdown, and without it the widget would keep the timer of
            // the press that opened it.
            self.stay(LINGER);
        }
    }

    pub fn refused(self: &Rc<Self>, why: String) {
        self.say(Some(why));
        self.stay(LINGER_WITH_MESSAGE);
    }

    /// The daemon is not there. Only worth a line on screen if the widget
    /// is already up; otherwise it goes to the journal, because a widget
    /// that appears on its own to report its own plumbing is worse than
    /// one that waits to be asked.
    pub fn unreachable(&self, why: String) {
        eprintln!("pyren-osd: {why}");
        if self.window.is_visible() {
            self.say(Some(why));
        }
    }

    fn select(&self, mode: Mode) {
        self.current.set(Some(mode));
        for card in &self.cards {
            let on = card.mode == mode;
            card.selected.set(on);
            if on {
                card.button.add_css_class("on");
            } else {
                card.button.remove_css_class("on");
            }
            card.glyph.queue_draw();
        }
        self.description.set_label(mode.description(self.lang));
    }

    fn say(&self, message: Option<String>) {
        match message {
            Some(text) => {
                self.message.set_label(&text);
                self.message.set_visible(true);
            }
            None => {
                self.message.set_label("");
                self.message.set_visible(false);
            }
        }
    }

    /// Drops any pending hide countdown, without hiding the widget.
    fn cancel_hide(&self) {
        if let Some(pending) = self.hide.borrow_mut().take() {
            pending.remove();
        }
    }

    /// (Re)starts the hide countdown. Replaces any one already running.
    fn arm_hide(self: &Rc<Self>, after: Duration) {
        self.cancel_hide();
        // The handle is cleared by the timeout itself: a `SourceId` that
        // has already fired must not be removed a second time, and the
        // next press would do exactly that.
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(after, move || {
            if let Some(ui) = weak.upgrade() {
                ui.window.set_visible(false);
                ui.hide.replace(None);
                // A hidden widget cannot receive a `leave`, so drop the
                // flag now - the next show trusts a fresh `enter`/motion.
                ui.pointer_inside.set(false);
            }
        });
        *self.hide.borrow_mut() = Some(source);
    }

    /// Shows the widget and sets the fallback countdown - unless the
    /// pointer is on it, which holds it open until the pointer leaves (and
    /// then it is `HIDE_AFTER_LEAVE`, not `linger`). Pressing the key again
    /// while it is up extends the visit rather than starting a second one.
    fn stay(self: &Rc<Self>, linger: Duration) {
        self.window.present();
        if self.pointer_inside.get() {
            self.cancel_hide();
        } else {
            self.arm_hide(linger);
        }
    }

    /// A card was clicked: the change is on its way to the daemon. Give it
    /// `HIDE_AFTER_CLICK` and then get out of the way - even with the
    /// pointer still on the widget - unless the pointer moves first, which
    /// the motion handler reads as "still choosing" and holds it open.
    fn after_click(self: &Rc<Self>) {
        self.window.present();
        self.arm_hide(HIDE_AFTER_CLICK);
    }

    /// Shows the widget without a key press, for `--show` and for a second
    /// launch of an already-running one. Whatever mode the daemon last
    /// reported stays highlighted.
    pub fn preview(self: &Rc<Self>) {
        self.select(self.current.get().unwrap_or(Mode::Balanced));
        if let Some(mode) = self.current_fan.get() {
            self.select_fan(mode);
        }
        self.refresh_rows();
        self.stay(LINGER_WITH_MESSAGE);
    }

    // --- the fan-mode row -------------------------------------------------

    /// The fan mode and capabilities from `fan.getStatus` - on reconnect,
    /// and in the reply to a click. Follows the same rule as `mode_is`: the
    /// highlight moves always, the widget appears only if it was already up.
    pub fn fan_state(
        self: &Rc<Self>,
        mode: FanMode,
        manual_percent: u8,
        switch_mode: bool,
        set_speed: bool,
    ) {
        self.fan_caps.set((switch_mode, set_speed));
        self.set_manual_percent(manual_percent);
        self.select_fan(mode);
        self.refresh_rows();
        if self.window.is_visible() {
            self.stay(LINGER);
        }
    }

    /// A `fan.mode` event - the mode moved from the app, the CLI, or this
    /// widget. Same visibility rule again.
    pub fn fan_mode_is(self: &Rc<Self>, mode: FanMode, manual_percent: u8) {
        self.set_manual_percent(manual_percent);
        self.select_fan(mode);
        self.refresh_rows();
        if self.window.is_visible() {
            self.stay(LINGER);
        }
    }

    fn select_fan(&self, mode: FanMode) {
        self.current_fan.set(Some(mode));
        for card in &self.fan_cards {
            let on = card.mode == mode;
            card.selected.set(on);
            if on {
                card.button.add_css_class("on");
            } else {
                card.button.remove_css_class("on");
            }
            card.glyph.queue_draw();
        }
        self.fan_description.set_label(mode.description(self.lang));
        let (_, set_speed) = self.fan_caps.get();
        self.fan_slider_row.set_visible(mode == FanMode::Manual && set_speed);
    }

    /// Moves the slider without its handler treating it as a drag.
    fn set_manual_percent(&self, percent: u8) {
        let percent = percent.min(100);
        self.fan_manual_percent.set(percent);
        self.updating_slider.set(true);
        self.fan_slider.set_value(f64::from(percent));
        self.updating_slider.set(false);
    }

    /// Coalesces a slider drag into one `fan.setMode` after the motion
    /// stops, and keeps the mode on `manual` while doing it.
    fn schedule_manual_push(self: &Rc<Self>) {
        if let Some(pending) = self.manual_push.borrow_mut().take() {
            pending.remove();
        }
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(MANUAL_PUSH_DELAY, move || {
            if let Some(ui) = weak.upgrade() {
                ui.manual_push.replace(None);
                let pwm = pwm_from_percent(ui.fan_manual_percent.get());
                let _ = ui.commands.send(Command::SetFanMode(FanMode::Manual, Some(pwm)));
            }
        });
        *self.manual_push.borrow_mut() = Some(source);
    }

    /// Which of the two rows are drawn, and which of the fan cards. The
    /// settings are read here rather than once at startup, so a change in
    /// Settings takes hold the next time the widget opens - no restart.
    fn refresh_rows(&self) {
        let (switch_mode, set_speed) = self.fan_caps.get();
        let fan_show = switch_mode && mode::fan_modes_in_widget();

        // The power row goes only if the app turned it off *and* the fan
        // row is actually up to replace it - never leave the widget empty.
        let power_show = mode::power_modes_in_widget() || !fan_show;
        self.power_section.set_visible(power_show);

        self.fan_section.set_visible(fan_show);
        // The rule above the fan title only makes sense with the power row
        // above it; on its own the fan row is the whole widget.
        if power_show {
            self.fan_section.add_css_class("section");
        } else {
            self.fan_section.remove_css_class("section");
        }

        for card in &self.fan_cards {
            // `manual` and `curve` need a commandable speed; `auto` and
            // `max` go through a different firmware call and always work.
            card.button.set_visible(!card.mode.needs_speed_control() || set_speed);
        }
        let manual_now = self.current_fan.get() == Some(FanMode::Manual);
        self.fan_slider_row.set_visible(fan_show && set_speed && manual_now);
    }
}
