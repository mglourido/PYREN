//! omen-hub-daemon: the privileged host process. Loads every hardware
//! module and serves them over a Unix domain socket. Intended to run as
//! root via a systemd service in production; see docs/01-ipc-protocol.md
//! for the wire format the Tauri app speaks to reach it.

use std::sync::Arc;

use omen_hub_core::{serve_unix_socket, Registry};
use omen_hub_fan::FanModule;

/// Production (systemd, running as root) should set `OMEN_HUB_SOCKET` to
/// `/run/omen-hub/daemon.sock`. This fallback keeps `cargo run` usable for
/// unprivileged local development without needing a real install.
fn socket_path() -> String {
    std::env::var("OMEN_HUB_SOCKET").unwrap_or_else(|_| "/tmp/omen-hub-daemon.sock".to_string())
}

fn main() {
    let mut registry = Registry::new();
    registry.register(Box::new(FanModule::new()));
    let registry = Arc::new(registry);

    let socket_path = socket_path();
    println!("omen-hub-daemon: listening on {socket_path}");
    for cap in registry.capabilities() {
        println!("  module '{}' supported={}", cap.id, cap.supported);
    }

    if let Err(e) = serve_unix_socket(&socket_path, registry) {
        eprintln!("omen-hub-daemon: fatal: {e}");
        std::process::exit(1);
    }
}
