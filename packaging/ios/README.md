# GridCraft for iPadOS

An early port: the same engine and egui interface as the desktop app, running natively on iPad
(winit's UIKit backend, wgpu on Metal). The Rust crate is [`apps/gridcraft-ios`](../../apps/gridcraft-ios);
this folder holds the Xcode project spec and its build script.

## Run it

On a Mac with Xcode 15 or later:

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
brew install xcodegen
packaging/ios/xcode.sh                       # generates GridCraft.xcodeproj (and the icon catalog)
open packaging/ios/GridCraft.xcodeproj
```

Pick an iPad Simulator and press Run. The first build compiles the whole workspace for iOS and
takes a few minutes; later builds are incremental.

To run on a real iPad, choose your team under **Signing & Capabilities** (a free Apple ID works
for your own devices) and turn on Developer Mode on the iPad.

`GridCraft.xcodeproj` and `build/` are generated and ignored by git: change `project.yml`,
`Info.plist` or `build-rust.sh`, then run `xcode.sh` again.

## How the build works

The Xcode target has no Swift or Objective-C sources. Its build phase runs `build-rust.sh`, which
builds `gridcraft-ios` with Cargo for the selected platform (`aarch64-apple-ios` for an iPad,
`aarch64-apple-ios-sim` or `x86_64-apple-ios` for the Simulator) and copies the executable into
`GridCraft.app`. Xcode then copies the resources and signs the app. Debug uses Cargo's dev profile
and Release uses the release profile. The Cargo output goes to `target-ios/` at the repository root.

Winit starts UIKit from Rust's `main`, so the port needs no `unsafe` and no Objective-C entry
point. `cargo xtask ios` runs clippy on both iOS targets, and it works on Linux too.

## Testing

- **Scheme arguments** (Product › Scheme › Edit Scheme › Run › Arguments): `--sample sales` opens
  a sample (on by default). `--control 7979` starts the JSON-lines control channel. In the
  Simulator, the Mac can reach it at `127.0.0.1:7979`, the same as the desktop app
  ([`docs/control-protocol.md`](../../docs/control-protocol.md)).
- **Files**: the app's Documents folder shows in the Files app as *On My iPad › GridCraft*.
  Workbooks put there are listed under **File › Open › Recent**, and Save As and exports write
  there. In the Simulator, drag a `.xlsx` onto the Files app, or copy it into
  `$(xcrun simctl get_app_container booted ai.storyteller.gridcraft data)/Documents`.
- **Logs**: `log` records go to standard error, which shows in Xcode's console.

## Not done yet

- A system document picker (Open, Browse…) and *Open in GridCraft* from other apps.
- The system clipboard: copy and paste only work inside GridCraft for now.
- Opening links in Safari. Printing.
- Touch-first interaction: one-finger panning of the grid, pinch to zoom, selection handles,
  long-press menus, and larger ribbon targets. Taps, the on-screen keyboard and a hardware
  keyboard or trackpad work.
- An App Store-ready icon (iOS icons must be opaque) and release signing.

The first three need calls into UIKit. The workspace forbids `unsafe`, so they wait on a decision:
allow one small, separate platform crate, or use audited crates with safe APIs, as the macOS app
does with `fmv-macos-events`.
