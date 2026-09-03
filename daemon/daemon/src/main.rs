//! pyren-daemon: the privileged host process. Loads every hardware
//! module and serves them over a Unix domain socket. Intended to run as
//! root via a systemd service in production; see docs/01-ipc-protocol.md
//! for the wire format the Tauri app speaks to reach it.

use std::sync::Arc;

use pyren_core::{serve_unix_socket, Audience, Module, Registry};
use pyren_fan::FanModule;
use pyren_installer::{
    execute, plan, Action, Environment, ExecuteContext, InstallerModule, PlanOptions,
};
use pyren_overclock::OverclockModule;
use pyren_power::PowerModule;
use pyren_rgb::RgbModule;
use pyren_system::{Compatibility, Controls, SystemModule};

/// Production (systemd, running as root) should set `PYREN_SOCKET` to
/// `/run/pyren/daemon.sock`. This fallback keeps `cargo run` usable for
/// unprivileged local development without needing a real install.
fn socket_path() -> String {
    std::env::var("PYREN_SOCKET").unwrap_or_else(|_| "/tmp/pyren-daemon.sock".to_string())
}

/// Whether the socket being owner-only is a problem. Unprivileged
/// development is the case where it isn't: the app runs as the same user.
fn is_root() -> bool {
    std::fs::metadata("/proc/self").map(|m| {
        use std::os::unix::fs::MetadataExt;
        m.uid() == 0
    }).unwrap_or(false)
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
    };
    let report = execute(&plan, &env, &context, false);

    for result in &report.results {
        println!("  [{:?}] {} - {}", result.status, result.description, result.detail);
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
        \x20 PYREN_SOCKET_GROUP  group allowed to connect (default 'pyren')\n"
    );
    std::process::exit(0);
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
    let controls = Controls {
        fan_mode: fan.capabilities().switch_mode,
        fan_speed: fan.capabilities().set_speed,
        power_mode: power.is_supported(),
        lightbar: rgb.probe().lightbar.present,
    };

    let system = SystemModule::new(controls);

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

    // Saying which lighting was found - and, when none was, which of the
    // three reasons applies - is the difference between "no lighting page"
    // and "no lighting page because acpi_call is not installed".
    println!("  lights: {}", rgb.probe().lightbar.detail);
    if rgb.probe().per_key.present {
        println!("  note:   {}", rgb.probe().per_key.detail);
    }

    // One line per card, because "the overclocking page is empty" is
    // answered by *which* of the mechanisms this machine has, and that
    // differs between the two GPUs in the same laptop.
    let gpu_tuning = overclock.probe();
    println!("  gpu oc: {}", gpu_tuning.detail);
    for gpu in &gpu_tuning.gpus {
        println!("    {}: {}", gpu.name, gpu.detail);
    }

    let mut registry = Registry::new();
    registry.register(Box::new(system));
    registry.register(Box::new(power));
    registry.register(Box::new(fan));
    registry.register(Box::new(rgb));
    registry.register(Box::new(overclock));
    registry.register(Box::new(InstallerModule::new()));
    let registry = Arc::new(registry);

    for cap in registry.capabilities() {
        println!("  module '{}' supported={}", cap.id, cap.supported);
    }

    let socket_path = socket_path();

    let announce = |audience: &Audience| {
        println!("pyren-daemon: listening on {socket_path}, {}", audience.summary());
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
        eprintln!("pyren-daemon: fatal: {e}");
        std::process::exit(1);
    }
}
