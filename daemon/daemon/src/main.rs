//! pyren-daemon: the privileged host process. Loads every hardware
//! module and serves them over a Unix domain socket. Intended to run as
//! root via a systemd service in production; see docs/01-ipc-protocol.md
//! for the wire format the Tauri app speaks to reach it.

use std::sync::Arc;

use serde_json::json;

use pyren_core::{log_error, log_info};
use pyren_core::{serve_unix_socket, Audience, EventBus, Module, Registry};
use pyren_fan::FanModule;
use pyren_gpu::GpuModule;
use pyren_hotkey::{HotkeyModule, KeyPress};
use pyren_installer::{
    execute, plan, Action, Environment, ExecuteContext, InstallerModule, PlanOptions,
};
use pyren_keymap::KeymapModule;
use pyren_network::NetworkModule;
use pyren_overclock::OverclockModule;
use pyren_power::{PowerModule, PowerSupplyState};
use pyren_rgb::{Conditions, RgbModule};
use pyren_system::{Compatibility, Controls, SystemModule};
use pyren_users::UsersModule;

/// Production (systemd, running as root) should set `PYREN_SOCKET` to
/// `/run/pyren/daemon.sock`. This fallback keeps `cargo run` usable for
/// unprivileged local development without needing a real install.
fn socket_path() -> String {
    std::env::var("PYREN_SOCKET").unwrap_or_else(|_| "/tmp/pyren-daemon.sock".to_string())
}

/// Whether the socket being owner-only is a problem. Unprivileged
/// development is the case where it isn't: the app runs as the same user.
fn is_root() -> bool {
    std::fs::metadata("/proc/self")
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.uid() == 0
        })
        .unwrap_or(false)
}

/// Installing the systemd unit is the one privileged action that cannot go
/// through the daemon, because it is what *makes* the daemon privileged in
/// the first place - a chicken and egg the IPC path cannot break. So the
/// binary can also be asked to do it directly, which is what the app runs
/// under `pkexec`.
///
/// This is not a second installer: it drives the same
/// `installer::{plan, execute}` the IPC method does. The only difference is
/// who is calling.
fn run_service_action(action: Action) -> ! {
    if !is_root() {
        eprintln!("pyren-daemon: this needs root (try: sudo pyren-daemon --install-service)");
        std::process::exit(1);
    }

    let env = Environment::detect();
    let plan = plan(&env, action, PlanOptions::default());
    if !plan.is_runnable() {
        for blocker in &plan.blockers {
            eprintln!("pyren-daemon: cannot continue: {}", blocker.message);
        }
        std::process::exit(1);
    }

    let context = ExecuteContext {
        max_rpm: Default::default(),
        experimental_board: None,
        daemon_binary: std::env::current_exe().ok(),
        skip_steps: Vec::new(),
    };
    let report = execute(&plan, &env, &context, false);

    for result in &report.results {
        println!(
            "  [{:?}] {} - {}",
            result.status, result.description, result.detail
        );
    }
    std::process::exit(if report.succeeded { 0 } else { 1 });
}

fn usage() -> ! {
    println!(
        "pyren-daemon - the privileged host process\n\n\
         With no arguments it serves the hardware modules over a Unix socket.\n\n\
         OPTIONS\n\
        \x20 --install-service   write and enable the systemd unit, then exit (needs root)\n\
        \x20 --remove-service    disable and delete it, then exit (needs root)\n\
        \x20 --help              this text\n\n\
         ENVIRONMENT\n\
        \x20 PYREN_SOCKET        where to listen (default /tmp/pyren-daemon.sock)\n\
        \x20 PYREN_SOCKET_GROUP  group allowed to connect (default 'pyren')\n\
        \x20 PYREN_LOG           how much to say: off, error, warn, info\n\
        \x20                     (the default) or debug. The startup report\n\
        \x20                     and --check are output, not logging, and\n\
        \x20                     are printed whatever this says\n"
    );
    std::process::exit(0);
}

/// Set for the daemon that replaces this one when the active user changes,
/// so it knows it was started to hand the hardware over and not by a boot.
const HANDOVER_ENV: &str = "PYREN_HANDOVER";

/// Replaces this process with a fresh daemon, which is how a change of
/// user takes effect - see `pyren_users`' own doc comment for why it is a
/// restart and not a reload. The hardware has already been let go of.
///
/// `exec` rather than exiting and leaving it to systemd: the unit restarts
/// on *failure*, five seconds later, and a switch between users is neither
/// a failure nor worth five seconds of nobody driving the fans. Everything
/// this process holds - the instance lock, the socket and its lock - is
/// close-on-exec, so the fresh daemon finds them free.
fn replace_self() -> ! {
    use std::os::unix::process::CommandExt;

    // The binary on disk when it is there, so an upgrade installed since
    // this daemon started is picked up; the image this process is running
    // when it is not (replaced mid-flight, and `current_exe` then names a
    // path that ends in " (deleted)").
    let binary = std::env::current_exe()
        .ok()
        .filter(|path| path.exists())
        .unwrap_or_else(|| "/proc/self/exe".into());
    // The new image inherits this thread's signal mask, and it must be
    // stoppable while it waits for one of Pyren's users to come back.
    pyren_core::signals::unblock_termination();
    let error = std::process::Command::new(binary)
        .env(HANDOVER_ENV, "1")
        .exec();
    // Still here, so it did not happen. Exiting non-zero is what gets
    // systemd to start a daemon in this one's place.
    log_error!("could not restart for the new active user: {error}");
    std::process::exit(1);
}

/// Claims the machine, allowing a daemon that has just replaced another
/// a moment for the old one's claim to go.
///
/// The claim is a descriptor, and `exec` closes it - but a child the old
/// daemon had forked and not yet `exec`ed (every `nvidia-smi` and
/// `loginctl` goes through that instant) holds a copy until it does. Seen
/// from here that is "another daemon controls this system" for a few
/// milliseconds, and giving up on it would turn a change of user into a
/// failed service.
fn claim_the_machine(replaced: bool) -> std::io::Result<pyren_core::DaemonInstanceLock> {
    let patience = if replaced {
        std::time::Duration::from_secs(2)
    } else {
        std::time::Duration::ZERO
    };
    let deadline = std::time::Instant::now() + patience;
    loop {
        match pyren_core::acquire_daemon_instance() {
            Err(e)
                if e.kind() == std::io::ErrorKind::AddrInUse
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

/// Takes the one right to end this process, or says somebody already has.
///
/// Two things end it - a termination signal and a change of active user -
/// on two threads, and each lets go of the hardware and then either exits
/// or `exec`s. Whichever is first keeps this for good (the guard is
/// forgotten, never dropped): were it handed on, the other would be woken
/// to race an `exit` against an `exec`.
fn try_leave(leaving: &std::sync::Mutex<()>) -> bool {
    match leaving.try_lock() {
        Ok(guard) => {
            std::mem::forget(guard);
            true
        }
        Err(std::sync::TryLockError::Poisoned(poisoned)) => {
            std::mem::forget(poisoned.into_inner());
            true
        }
        Err(std::sync::TryLockError::WouldBlock) => false,
    }
}

/// One press of the performance key: put the modes on screen, and change
/// nothing.
///
/// The key used to step the mode itself, the way Fn+P does under Windows.
/// It does not any more, and the reason is what the key is *for* here: on
/// this hardware the vendor key never reaches Linux, so the shortcut is
/// one the user chose, and a chosen shortcut that silently moves the
/// machine to the next profile every time you glance at it is a worse
/// deal than one that opens a picker. The widget already lets you click a
/// mode, so nothing is lost - the choice simply becomes deliberate.
///
/// [`PowerModule::cycle`] is still there, tested, for whoever wants that
/// behaviour back behind a setting.
fn show_power_modes(power: &PowerModule, events: &EventBus, press: &KeyPress) {
    let mode = power.mode();

    // No `changed`, `applied` or `failed`: nothing was attempted, and a
    // report of an action nobody took is what a widget would misread.
    events.publish(
        "hotkey.pressed",
        json!({
            "action": "show",
            "device": press.device,
            "mode": mode,
        }),
    );

    log_info!("hotkey: showing the modes ({mode:?})");
}

/// What to print at startup about the performance key. Every branch names
/// the next thing to do, because "hotkey: no" sends people to the issue
/// tracker and "no key bound yet" sends them to `pyren-ctl hotkey learn`.
fn hotkey_summary(hotkey: &HotkeyModule, watching: bool) -> String {
    let status = match hotkey.call("getStatus", serde_json::Value::Null) {
        Ok(status) => status,
        Err(e) => return format!("unavailable: {e}"),
    };
    let detail = status["detail"]
        .as_str()
        .unwrap_or("unavailable")
        .to_string();
    if !watching {
        return detail;
    }
    let bound = status["triggers"].as_array().is_some_and(|t| !t.is_empty());
    if bound {
        detail
    } else {
        format!("{detail} (pyren-ctl hotkey learn)")
    }
}

/// Whether the lid is shut, from the ACPI button. A machine without one
/// reads as open, which is what it is for the lights.
fn lid_closed() -> bool {
    let Ok(entries) = std::fs::read_dir("/proc/acpi/button/lid") else {
        return false;
    };
    entries.filter_map(Result::ok).any(|lid| {
        std::fs::read_to_string(lid.path().join("state")).is_ok_and(|s| s.contains("closed"))
    })
}

/// Tells the lighting module about the charger and the lid, every two
/// seconds. Here rather than in either crate: the charger is the power
/// module's to read and the lights are the rgb module's to throttle, and
/// a module never calls another one directly.
///
/// A poll, not an event: both are one small sysfs read, the rgb side does
/// nothing unless one of them changed, and a lid shut for two seconds
/// before an effect pauses costs nobody anything.
fn watch_conditions(rgb: RgbModule) {
    let spawned = std::thread::Builder::new()
        .name("pyren-rgb-conditions".into())
        .spawn(move || loop {
            rgb.set_conditions(Conditions {
                on_battery: PowerSupplyState::read().on_battery.unwrap_or(false),
                lid_closed: lid_closed(),
            });
            std::thread::sleep(std::time::Duration::from_secs(2));
        });
    if let Err(e) = spawned {
        pyren_core::log_warn!("could not start the lighting conditions watcher: {e}");
    }
}

fn main() {
    // Arguments are handled before anything is probed: a machine that
    // cannot be detected properly should still be able to install a unit.
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("--install-service") => run_service_action(Action::InstallService),
        Some("--remove-service") => run_service_action(Action::RemoveService),
        Some("--help" | "-h") => usage(),
        Some(other) => {
            eprintln!("pyren-daemon: unknown argument '{other}' (try --help)");
            std::process::exit(1);
        }
    }

    // Claim the hardware control domain before any module can probe, restore,
    // or start a worker. The IPC socket pathname is intentionally irrelevant.
    let replaced = std::env::var_os(HANDOVER_ENV).is_some();
    std::env::remove_var(HANDOVER_ENV);
    let _instance_lock = claim_the_machine(replaced).unwrap_or_else(|e| {
        eprintln!("pyren-daemon: cannot acquire system control: {e}");
        std::process::exit(1);
    });

    // Whose settings these are comes before anything reads them, and so
    // does waiting, if the person at the machine is not one of Pyren's and
    // the daemon was told to stand down for them: no module exists yet, so
    // nothing is driving the hardware while it waits. Before the signals
    // are blocked, too - there is nothing to tidy up yet, and a daemon that
    // is only waiting has to die on SIGTERM like any other process.
    let users = UsersModule::new();
    if users.settle() || replaced {
        pyren_core::handover::mark();
    }

    // Before any module starts a thread: a thread inherits the signal mask
    // it was started with, and the handler below only works if SIGTERM is
    // blocked in every one of them (see `pyren_core::signals`).
    pyren_core::signals::block_termination();

    pyren_core::debuglog::init(pyren_core::debuglog::daemon_root());

    // The hardware modules come first, because what this machine can be
    // told to do is something only they can answer - `system` used to
    // answer it from a copied list of DMI board ids, which said "supported"
    // about a machine whose fans cannot be set. Probe, then report.
    let fan = FanModule::new();
    let power = PowerModule::new();
    // The lighting probe belongs here rather than beside the registry: two
    // unrelated hardware paths, and which one a laptop has is not decided
    // by its model name, so the lightbar is one of the things `controls`
    // has to have been told about before the verdict is computed.
    let rgb = RgbModule::new();
    // Probed here for the same reason as the lighting: whether a GPU can be
    // tuned depends on the driver and the session, not on the model, so it
    // is a question that has to be put to the machine before anything can
    // be said about it.
    let overclock = OverclockModule::new();
    let gpu = GpuModule::new();
    let network = NetworkModule::new();
    let controls = Controls {
        fan_mode: fan.capabilities().switch_mode,
        fan_speed: fan.capabilities().set_speed,
        power_mode: power.is_supported(),
        lightbar: rgb.probe().lighting.present,
        gpu_mux: gpu.is_supported(),
        network_qos: network.is_supported(),
    };

    // Built here, wired to the power module further down: what a key does
    // is coordination between two modules, and a module never calls
    // another one directly.
    let hotkey = HotkeyModule::new();
    // Off until a mapping is set and turned on: grabbing a keyboard here
    // silences `hotkey` on it (see `pyren_keymap`'s own doc comment), so
    // this must never come up watching on its own the way `hotkey` does.
    let keymap = KeymapModule::new();

    let system = SystemModule::new(controls);
    let debug = pyren_core::DebugModule::new();

    // Printing what we detected at startup is the fastest way to diagnose a
    // "nothing works on my machine" report - it is the first thing to ask
    // for, so make it appear without needing a debug flag.
    let identity = system.identity();
    println!("pyren-daemon: {}", identity.summary());
    if let Some(cpu) = &identity.cpu {
        println!("  cpu:    {cpu} ({} threads)", identity.cpu_cores);
    }
    for gpu in &identity.gpus {
        println!("  gpu:    {gpu}");
    }
    if let Some(kernel) = &identity.kernel {
        println!("  kernel: {kernel}");
    }
    if identity.compatibility != Compatibility::Controllable {
        println!("  note:   {}", identity.reason);
    }
    // Unprivileged, the Intel PMU stays shut and the iGPU reports no usage.
    // That looks exactly like a broken card unless someone says otherwise,
    // and this is the first place anyone looks.
    let privileges = system.privileges();
    if !privileges.perf_events {
        println!(
            "  note:   integrated-GPU utilisation is unavailable{}",
            if privileges.root {
                " (no Intel GPU, or its perf PMU is absent)"
            } else {
                "; it needs CAP_PERFMON, which the systemd unit gets by running as root"
            }
        );
    }

    pyren_core::debuglog::record_if_changed(
        pyren_core::debuglog::Category::DriverKernel,
        serde_json::json!({
            "vendor": identity.vendor,
            "model": identity.model,
            "boardName": identity.board_name,
            "boardVendor": identity.board_vendor,
            "biosVersion": identity.bios_version,
            "kernel": identity.kernel,
            "cpu": identity.cpu,
            "gpus": identity.gpus,
            "driverInstalled": fan.is_supported(),
        }),
    );

    // Saying which lighting was found - and, when none was, which of the
    // three reasons applies - is the difference between "no lighting page"
    // and "no lighting page because acpi_call is not installed".
    let lighting = rgb.probe();
    println!("  lights: {}", lighting.lighting.detail);
    // One line per dialect, for the same reason the GPUs get one each:
    // "the lighting page is empty" is answered by *which* of the three
    // ways of talking to these lights this machine refused, and that is
    // three different next steps.
    for dialect in &lighting.lighting.dialects {
        println!("    {}: {}", dialect.id, dialect.detail);
    }
    if lighting.per_key.present {
        println!("  note:   {}", lighting.per_key.detail);
    }

    // One line per card, because "the overclocking page is empty" is
    // answered by *which* of the mechanisms this machine has, and that
    // differs between the two GPUs in the same laptop.
    let gpu_tuning = overclock.probe();
    println!("  gpu oc: {}", gpu_tuning.detail);
    for gpu in &gpu_tuning.gpus {
        println!("    {}: {}", gpu.name, gpu.detail);
    }
    println!("  users:  {}", users.summary());

    let mut registry = Registry::new();
    let events = Arc::clone(registry.events());
    debug.publish_to(Arc::clone(&events));
    users.publish_to(Arc::clone(&events));
    // Everything that moves the power mode - the key, the app, the CLI,
    // the supervisor - is announced on this, so an open UI never sits
    // showing a mode the machine has already left.
    power.publish_to(Arc::clone(&events));
    // The fan module publishes too: `fan.mode` when the fan mode changes
    // (the same job `power.mode` does for the power mode - the widget and
    // the fan page follow it), and `fan.floorRaised` when its stall watch
    // has nudged the fans' minimum speed up because they kept giving out at
    // it. Nothing in-process listens to the latter yet; it is for a future
    // app notification, and until then `fan.getStatus` carries the record.
    fan.publish_to(Arc::clone(&events));

    // ...and the fan module listens to that announcement, so the curve
    // drawn for a profile is the one that runs while the machine is in it.
    //
    // Through the bus rather than a call, and the wiring is *here* rather
    // than in either crate: `pyren-power` does not know a fan module
    // exists, `pyren-fan` does not know what a power profile is (the name
    // is an opaque key to it - see `FanConfig::profile_curves`), and this
    // binary is the one place entitled to know both. Doing it the other way
    // is how "changing the power mode must not write to the fans" would
    // have quietly become two modules calling each other.
    {
        let fan = fan.clone();
        events.subscribe(move |topic, payload| {
            if topic != "power.mode" {
                return;
            }
            if let (Some(mode), Some(generation)) = (
                payload.get("mode").and_then(|m| m.as_str()),
                payload.get("generation").and_then(|g| g.as_u64()),
            ) {
                fan.set_active_profile_versioned(mode, generation);
            }
        });
    }
    events.subscribe(|topic, payload| {
        if matches!(topic, "power.mode" | "fan.mode" | "fan.floorRaised") {
            pyren_core::debuglog::record(
                pyren_core::debuglog::Category::Performance,
                serde_json::json!({ "topic": topic, "payload": payload }),
            );
        }
    });
    // The first announcement only comes with the first *change*, so without
    // this a daemon that starts in Eco and is left alone would follow the
    // shared curve until something moved the mode.
    let (initial_mode, initial_generation) = power.mode_snapshot();
    fan.set_active_profile_versioned(initial_mode.as_str(), initial_generation);
    // The fan safety checker's idea of "hot" is the power supervisor's
    // "hot at" / "cooled below", read from its file rather than asked of the
    // module, for the same reason as above: neither crate knows the other.
    {
        let power_config = power.config_path();
        fan.set_heat_source(Box::new(move || {
            let text = std::fs::read_to_string(&power_config).ok()?;
            let file: serde_json::Value = serde_json::from_str(&text).ok()?;
            let auto = file.get("auto")?;
            Some((
                auto.get("tempHighC")?.as_f64()?,
                auto.get("tempLowC")?.as_f64()?,
            ))
        }));
    }
    let fan_at_exit = fan.clone();

    registry.register(Box::new(system));
    registry.register(Box::new(power.clone()));
    registry.register(Box::new(fan.clone()));
    registry.register(Box::new(rgb.clone()));
    watch_conditions(rgb.clone());
    let overclock_at_exit = overclock.clone();
    registry.register(Box::new(overclock));
    registry.register(Box::new(gpu));
    registry.register(Box::new(network));
    registry.register(Box::new(hotkey.clone()));
    registry.register(Box::new(keymap));
    registry.register(Box::new(debug));
    registry.register(Box::new(users.clone()));
    // Installing the driver reloads hp-wmi, which renumbers the hwmon
    // directory the fan module found at startup. Handing it a way to look
    // again is what makes an install take effect without anyone being told
    // to restart the daemon afterwards.
    registry.register(Box::new(
        InstallerModule::new()
            .publish_to(Arc::clone(&events))
            .on_driver_changed(Box::new(move || {
                fan.rediscover();
                // Not the full snapshot the startup call above logs: `system`
                // (and the `identity` reference borrowed from it) is already
                // moved into the registry by this point, so a full re-read of
                // vendor/board/kernel/etc is not reachable here without
                // restructuring what this closure captures. `driverInstalled`
                // is: it is exactly what `fan.rediscover()` just refreshed,
                // so this re-logs the one fact this hook exists to observe -
                // that an install changed whether the driver is now seen as
                // present, without anyone restarting the daemon.
                pyren_core::debuglog::record_if_changed(
                    pyren_core::debuglog::Category::DriverKernel,
                    serde_json::json!({ "driverInstalled": fan.is_supported() }),
                );
            })),
    ));
    let registry = Arc::new(registry);

    // What the fan and power modules are doing is not always what their
    // files say (a mode is only written down when something asks for it to
    // outlast a restart), and both the daemon leaving and a user's
    // settings being put aside need the files to say it.
    let remember: Arc<dyn Fn() + Send + Sync> = {
        let fan = fan_at_exit.clone();
        let power = power.clone();
        Arc::new(move || {
            fan.remember_mode();
            power.remember_mode();
        })
    };
    users.before_setting_aside({
        let remember = Arc::clone(&remember);
        Box::new(move || remember())
    });

    // The one thing that has to happen on the way out: a lighting effect
    // is a thread rewriting the keyboard, and killing it mid-frame leaves
    // that frame on the keys - and a shutdown is when the power-off sweep
    // plays.
    let power_at_exit = power.clone();
    let release = move |machine_stopping: bool| {
        remember();
        rgb.on_exit(machine_stopping);
        // No destructor runs after this: a curve's low speed or a
        // calibration's near-stall floor would stay on the fans otherwise.
        fan_at_exit.on_exit();
        // auto-cpufreq keeps pyren's override in its own state otherwise.
        power_at_exit.on_exit();
    };
    let release: Arc<dyn Fn(bool) + Send + Sync> = Arc::new(release);
    let leaving = Arc::new(std::sync::Mutex::new(()));

    // Whether this is a shutdown or only the service stopping is asked of
    // systemd, because SIGTERM is the same signal either way.
    let release_on_signal = Arc::clone(&release);
    let leaving_on_signal = Arc::clone(&leaving);
    pyren_core::signals::on_termination(move |signal| {
        if !try_leave(&leaving_on_signal) {
            // The daemon is in the middle of replacing itself for another
            // user, and this thread has just swallowed the request to
            // stop. Sent again it stays pending - nobody is waiting for it
            // now - and kills the process the moment the hand-over
            // unblocks it, which is after the hardware has been let go of
            // and before a fresh daemon has taken it up again.
            pyren_core::signals::raise(signal);
            loop {
                std::thread::park();
            }
        }
        let stopping = pyren_core::signals::system_is_stopping();
        log_info!(
            "{}: leaving{}",
            pyren_core::signals::name(signal),
            if stopping {
                " (the machine is shutting down)"
            } else {
                ""
            }
        );
        pyren_core::debuglog::record(
            pyren_core::debuglog::Category::Daemon,
            serde_json::json!({
                "event": "shutdown",
                "signal": pyren_core::signals::name(signal),
                "systemStopping": stopping,
            }),
        );
        release_on_signal(stopping);
    });

    // From here the settings follow whoever is at the machine. A user
    // with none of their own is dealt with where the watcher stands;
    // anyone else's arrival ends this process, and the next one starts
    // with their settings - or does not start, for somebody who is not
    // one of Pyren's users.
    users.watch(Box::new(move |reason| {
        if !try_leave(&leaving) {
            // Already stopping: the signal thread has the hardware.
            return;
        }
        log_info!("{reason}");
        pyren_core::debuglog::record(
            pyren_core::debuglog::Category::Daemon,
            serde_json::json!({ "event": "handover", "reason": reason }),
        );
        release(false);
        // Not part of leaving on a signal: an offset on the card outlives
        // the daemon on purpose there. Here the next person at the machine
        // is somebody else.
        overclock_at_exit.on_handover();
        replace_self();
    }));

    // The shortcut, once somebody has taught the daemon which key it is.
    // One event comes out of a press - `hotkey.pressed`, which the
    // on-screen display waits on - and the machine is left alone: the
    // press asks for the widget, and the mode changes only if the user
    // then clicks one.
    let watching = hotkey.watch(Arc::new({
        let power = power.clone();
        let events = Arc::clone(&events);
        move |press: &KeyPress| show_power_modes(&power, &events, press)
    }));
    println!("  hotkey: {}", hotkey_summary(&hotkey, watching));

    for cap in registry.capabilities() {
        println!("  module '{}' supported={}", cap.id, cap.supported);
    }

    pyren_core::debuglog::record(
        pyren_core::debuglog::Category::Daemon,
        serde_json::json!({
            "event": "startup",
            "version": env!("CARGO_PKG_VERSION"),
            "privileged": is_root(),
        }),
    );

    let socket_path = socket_path();

    let announce = |audience: &Audience| {
        println!(
            "pyren-daemon: listening on {socket_path}, {}",
            audience.summary()
        );
        // A root daemon nobody can reach looks exactly like a working one
        // until the app fails to connect, so name the fix here.
        if matches!(audience, Audience::OwnerOnly) && is_root() {
            println!(
                "  note:   no 'pyren' group on this system, so only root can connect.\n\
                 \x20         create it and add your desktop user:\n\
                 \x20           sudo groupadd -f pyren && sudo usermod -aG pyren $USER"
            );
        }
    };

    if let Err(e) = serve_unix_socket(&socket_path, registry, announce) {
        log_error!("fatal: {e}");
        std::process::exit(1);
    }
}
