//! GridCraft for iPadOS.
//!
//! Runs the same [`gridcraft_ui_egui::SheetApp`] as the desktop app through eframe's native
//! runner, which on iOS is winit's UIKit backend with wgpu on Metal. `main` is an ordinary Rust
//! entry point: winit calls `UIApplicationMain` itself, so no Objective-C or Swift is involved.
//! Xcode builds this crate and copies the executable into the app bundle; see
//! `packaging/ios/README.md`.
//!
//! Launch arguments (set them in the Xcode scheme): `--sample sales|budget|grades` opens a
//! sample; `--control <port>` (or `GRIDCRAFT_CONTROL_PORT`) starts the localhost control channel,
//! which in the Simulator is reachable from the Mac at `127.0.0.1:<port>`.
//!
//! Differences from the desktop app, for now: files live in the app's `Documents` folder (shared
//! with the Files app) instead of a document picker; no system clipboard (copy and paste stay
//! inside GridCraft); links don't open in Safari; no printing.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(target_os = "ios")]
mod app;
#[cfg(target_os = "ios")]
#[path = "../../gridcraft/src/control_server.rs"]
mod control_server;
#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
mod files;

#[cfg(target_os = "ios")]
fn main() -> eframe::Result<()> {
    app::run()
}

#[cfg(not(target_os = "ios"))]
fn main() {
    eprintln!("gridcraft-ios only runs on iPadOS: open packaging/ios in Xcode (see packaging/ios/README.md)");
}
