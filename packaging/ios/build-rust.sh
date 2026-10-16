#!/bin/sh
# Xcode build phase: builds apps/gridcraft-ios for the platform Xcode is building (an iPad or the
# Simulator) and puts the executable in the app bundle, before Xcode signs it.
#
# Xcode sets PLATFORM_NAME, ARCHS, CONFIGURATION, TARGET_BUILD_DIR and EXECUTABLE_PATH. Debug
# builds use Cargo's dev profile (optimised dependencies, see the workspace Cargo.toml), Release
# builds the release profile. GRIDCRAFT_IOS_TARGET_DIR overrides the Cargo target directory
# (default: target-ios at the repository root, apart from the desktop builds).
set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# Xcode runs scripts with a minimal PATH.
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
# SDKROOT points at the iOS SDK; Cargo's build scripts are built for the Mac and must not use it.
unset SDKROOT

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found; install Rust from https://rustup.rs" >&2
    exit 1
fi

case "${PLATFORM_NAME:-iphonesimulator}" in
    iphoneos)
        TARGETS="aarch64-apple-ios"
        ;;
    iphonesimulator)
        TARGETS=""
        for arch in ${ARCHS:-arm64}; do
            case "$arch" in
                arm64) TARGETS="$TARGETS aarch64-apple-ios-sim" ;;
                x86_64) TARGETS="$TARGETS x86_64-apple-ios" ;;
                *) echo "error: unsupported Simulator architecture $arch" >&2; exit 1 ;;
            esac
        done
        ;;
    *)
        echo "error: unsupported platform ${PLATFORM_NAME:-}" >&2
        exit 1
        ;;
esac

if [ "${CONFIGURATION:-Debug}" = "Release" ]; then
    PROFILE_DIR=release
    set -- --release
else
    PROFILE_DIR=debug
    set --
fi

TARGET_DIR="${GRIDCRAFT_IOS_TARGET_DIR:-$ROOT/target-ios}"
BINARIES=""
for target in $TARGETS; do
    if command -v rustup >/dev/null 2>&1; then
        rustup target add "$target" >/dev/null
    fi
    cargo build --manifest-path "$ROOT/Cargo.toml" -p gridcraft-ios --target "$target" --target-dir "$TARGET_DIR" "$@"
    BINARIES="$BINARIES $TARGET_DIR/$target/$PROFILE_DIR/gridcraft-ios"
done

OUT="${TARGET_BUILD_DIR:?run this from Xcode}/${EXECUTABLE_PATH:?run this from Xcode}"
mkdir -p "$(dirname "$OUT")"
# shellcheck disable=SC2086 # one path per target, none with spaces (target-ios at the root)
set -- $BINARIES
if [ "$#" -eq 1 ]; then
    cp -f "$1" "$OUT"
else
    lipo -create "$@" -output "$OUT"
fi
echo "GridCraft: $OUT ($TARGETS, $PROFILE_DIR)"
