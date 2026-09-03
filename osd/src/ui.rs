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
use gtk4::{glib, Align, Application, ApplicationWindow, Orientation};

use crate::daemon::Command;
use crate::icon;
use crate::mode::{Lang, Mode};

/// How long the widget stays up after the last press. Matched to the
/// vendor's: long enough to press the key again and watch the highlight
/// move, short enough not to sit on top of a game.
const LINGER: Duration = Duration::from_millis(2500);

/// Longer when there is something to read - a refusal is a sentence, and
/// two and a half seconds is not enough to read a sentence and understand
/// that it was about the key you just pressed.
const LINGER_WITH_MESSAGE: Duration = Duration::from_millis(6000);

const ICON: f64 = 34.0;

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
";

struct Card {
    mode: Mode,
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

        let title = gtk4::Label::builder()
            .label(lang.title())
            .halign(Align::Start)
            .css_classes(["title"])
            .build();
        panel.append(&title);

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
        panel.append(&row);

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
        panel.append(&description);

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
                ui.stay(LINGER);
            });
        }

        ui.place_on_screen();
        ui
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
        self.stay(LINGER);
    }

    /// Puts the widget away now, and cancels the countdown that would
    /// have done it later.
    ///
    /// Dropping the timer matters: left running, it fires on a window
    /// that is already hidden and, worse, a *later* press would find a
    /// `SourceId` that has already fired sitting in `hide`.
    fn dismiss(&self) {
        if let Some(pending) = self.hide.borrow_mut().take() {
            pending.remove();
        }
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

    /// Shows the widget and restarts the countdown. Pressing the key again
    /// while it is up extends the visit rather than starting a second one.
    fn stay(self: &Rc<Self>, linger: Duration) {
        if let Some(pending) = self.hide.borrow_mut().take() {
            pending.remove();
        }
        self.window.present();

        // The handle is cleared by the timeout itself: a `SourceId` that
        // has already fired must not be removed a second time, and the
        // next press would do exactly that.
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(linger, move || {
            if let Some(ui) = weak.upgrade() {
                ui.window.set_visible(false);
                ui.hide.replace(None);
            }
        });
        *self.hide.borrow_mut() = Some(source);
    }

    /// Shows the widget without a key press, for `--show` and for a second
    /// launch of an already-running one. Whatever mode the daemon last
    /// reported stays highlighted.
    pub fn preview(self: &Rc<Self>) {
        self.select(self.current.get().unwrap_or(Mode::Balanced));
        self.stay(LINGER_WITH_MESSAGE);
    }
}
