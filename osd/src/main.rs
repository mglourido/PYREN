//! pyren-osd: the widget the laptop's performance key puts on the screen.
//!
//! Windows has one, drawn by the vendor's software over whatever is
//! running. Linux has neither the key nor the widget: the key needs root
//! to hear (it is `/dev/input`), and a window in the middle of the screen
//! over a fullscreen game needs a Wayland protocol most toolkits do not
//! reach for. So the work is split at the privilege boundary, which is
//! where it belongs:
//!
//! - **pyren-daemon**, as root, hears the key. That half works with
//!   nothing else installed and nobody logged in.
//! - **this**, in the user's session, draws the four modes so the user can
//!   pick one - and the press itself changes nothing. Pressing the
//!   shortcut again puts the widget away.
//!
//! Between them is the daemon's event stream (`core.nextEvent`), one long
//! poll on a socket. Nothing is polled on a timer and nothing is guessed:
//! the widget shows the mode the daemon says the machine ended up in,
//! including when that is the mode it started in because the change was
//! refused.
//!
//! Run it as a user service:
//!
//! ```sh
//! systemctl --user enable --now pyren-osd.service
//! ```

mod daemon;
mod icon;
mod mode;
mod ui;

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gio, glib, Application};

use daemon::Message;
use mode::Lang;
use ui::Ui;

const HELP: &str = "\
pyren-osd - the on-screen display for the performance key

USAGE
  pyren-osd [--show]

It waits for pyren-daemon to publish a key press and draws the four power
modes in the middle of the screen, over everything else. Nothing appears
until the key is pressed - or until a second copy is launched, which shows
the widget rather than starting a second process.

  --show        show the widget straight away, then wait as usual
  -h, --help
  -V, --version

The daemon has to be told which key that is first, once per machine:

  pyren-ctl hotkey learn

The socket is $PYREN_SOCKET, or /run/pyren/daemon.sock, or
/tmp/pyren-daemon.sock. Reaching a daemon running as root means being in
the 'pyren' group.
";

fn main() -> glib::ExitCode {
    let mut show_now = false;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--show" => show_now = true,
            "-h" | "--help" => {
                print!("{HELP}");
                return glib::ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("pyren-osd {}", env!("CARGO_PKG_VERSION"));
                return glib::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("pyren-osd: unknown argument '{other}' (try --help)");
                return glib::ExitCode::FAILURE;
            }
        }
    }

    let app = Application::builder()
        .application_id("dev.pyren.osd")
        // GTK must not parse the command line: the arguments above are
        // this program's, and an unknown one is its business to report.
        .flags(gio::ApplicationFlags::empty())
        .build();

    // One widget per session. Launching a second copy activates this one,
    // and the useful thing to do with that is show the modes - which makes
    // `pyren-osd` a perfectly good "open the mode switcher" command for a
    // compositor keybinding on a laptop whose own key never reaches Linux.
    let existing: Rc<RefCell<Option<Rc<Ui>>>> = Rc::new(RefCell::new(None));

    app.connect_activate(move |app| {
        if let Some(ui) = existing.borrow().as_ref() {
            ui.preview();
            return;
        }

        let (sender, receiver) = async_channel::unbounded::<Message>();
        let commands = daemon::start(sender);
        let ui = Ui::build(app, Lang::detect(), commands);
        *existing.borrow_mut() = Some(Rc::clone(&ui));

        if show_now {
            ui.preview();
        }

        // Everything from the daemon crosses into the GTK thread here, and
        // only here.
        glib::spawn_future_local(async move {
            while let Ok(message) = receiver.recv().await {
                match message {
                    Message::Show(mode) => ui.show(mode),
                    Message::Pressed { mode, changed, refusal } => {
                        ui.pressed(mode, changed, refusal)
                    }
                    Message::Mode(mode) => ui.mode_is(mode),
                    Message::Refused(why) => ui.refused(why),
                    Message::Unreachable(why) => ui.unreachable(why),
                    Message::Reachable => {}
                }
            }
        });
    });

    app.run_with_args::<&str>(&[])
}
