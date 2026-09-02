fn main() {
    // `tauri_build` only re-runs the build when `tauri.conf.json` changes, not
    // when the icon files it embeds do. Without this, editing the logo leaves a
    // stale window icon baked into the binary until something else forces a
    // rebuild of this crate.
    println!("cargo:rerun-if-changed=icons");

    tauri_build::build()
}
