#!/bin/sh
# Generates GridCraft.xcodeproj from project.yml (XcodeGen), with the app icon catalog made from
# assets/app-icon/gridcraft-1024.png. Run it on a Mac with Xcode, then open the project:
#
#   packaging/ios/xcode.sh && open packaging/ios/GridCraft.xcodeproj
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

if ! command -v xcodegen >/dev/null 2>&1; then
    echo "xcodegen not found: brew install xcodegen" >&2
    exit 1
fi

ASSETS="$HERE/build/Assets.xcassets"
ICON="$ASSETS/AppIcon.appiconset"
rm -rf "$ASSETS"
mkdir -p "$ICON"
cat > "$ASSETS/Contents.json" <<'JSON'
{ "info" : { "author" : "xcode", "version" : 1 } }
JSON
cp -f "$ROOT/assets/app-icon/gridcraft-1024.png" "$ICON/gridcraft-1024.png"
cat > "$ICON/Contents.json" <<'JSON'
{
  "images" : [
    { "filename" : "gridcraft-1024.png", "idiom" : "universal", "platform" : "ios", "size" : "1024x1024" }
  ],
  "info" : { "author" : "xcode", "version" : 1 }
}
JSON

chmod +x "$HERE/build-rust.sh"
xcodegen generate --spec "$HERE/project.yml" --project "$HERE"
echo "Open $HERE/GridCraft.xcodeproj"
