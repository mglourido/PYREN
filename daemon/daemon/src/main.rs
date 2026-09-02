//! omen-hub-daemon: the privileged host process. Loads every hardware
//! module and serves them over a Unix domain socket. Intended to run as
//! root via a systemd service in production; see docs/01-ipc-protocol.md
//! for the wire format the Tauri app speaks to reach it.

use std::sync::Arc;

use omen_hub_core::{serve_unix_socket, Audience, Registry};
use omen_hub_fan::FanModule;
use omen_hub_installer::InstallerModule;
use omen_hub_power::PowerModule;
use omen_hub_system::{Compatibility, SystemModule};

/// Production (systemd, running as root) should set `OMEN_HUB_SOCKET` to
/// `/run/omen-hub/daemon.sock`. This fallback keeps `cargo run` usable for
/// unprivileged local development without needing a real install.
fn socket_path() -> String {
    std::env::var("OMEN_HUB_SOCKET").unwrap_or_else(|_| "/tmp/omen-hub-daemon.sock".to_string())
}

/// Whether the socket being owner-only is a problem. Unprivileged
/// development is the case where it isn't: the app runs as the same user.
fn is_root() -> bool {
    std::fs::metadata("/proc/self").map(|m| {
        use std::os::unix::fs::MetadataExt;
        m.uid() == 0
    }).unwrap_or(false)
}

fn main() {
    let system = SystemModule::new();

    // Printing what we detected at startup is the fastest way to diagnose a
    // "nothing works on my machine" report - it is the first thing to ask
    // for, so make it appear without needing a debug flag.
    let identity = system.identity();
    println!("omen-hub-daemon: {}", identity.summary());
    if let Some(cpu) = &identity.cpu {
        println!("  cpu:    {cpu} ({} threads)", identity.cpu_cores);
    }
    for gpu in &identity.gpus {
        println!("  gpu:    {gpu}");
    }
    if let Some(kernel) = &identity.kernel {
        println!("  kernel: {kernel}");
    }
    if identity.compatibility != Compatibility::Supported {
        println!(
            "  note:   hardware control is expected to be unavailable here; \
             monitoring still works"
        );
    }

    let mut registry = Registry::new();
    registry.register(Box::new(system));
    registry.register(Box::new(PowerModule::new()));
    registry.register(Box::new(FanModule::new()));
    registry.register(Box::new(InstallerModule::new()));
    let registry = Arc::new(registry);

    for cap in registry.capabilities() {
        println!("  module '{}' supported={}", cap.id, cap.supported);
    }

    let socket_path = socket_path();

    let announce = |audience: &Audience| {
        println!("omen-hub-daemon: listening on {socket_path}, {}", audience.summary());
        // A root daemon nobody can reach looks exactly like a working one
        // until the app fails to connect, so name the fix here.
        if matches!(audience, Audience::OwnerOnly) && is_root() {
            println!(
                "  note:   no 'omen-hub' group on this system, so only root can connect.\n\
                 \x20         create it and add your desktop user:\n\
                 \x20           sudo groupadd -f omen-hub && sudo usermod -aG omen-hub $USER"
            );
        }
    };

    if let Err(e) = serve_unix_socket(&socket_path, registry, announce) {
        eprintln!("omen-hub-daemon: fatal: {e}");
        std::process::exit(1);
    }
}
