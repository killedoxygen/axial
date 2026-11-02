#!/usr/bin/env bash
# Rebuilds the macOS disk image with the first-open guide beside the app. Tauri cannot add files to the
# image, so this reruns the create-dmg script Tauri writes next to its own image, then replaces that image.
# Usage: dmg.sh <bundle root>   (for example target/aarch64-apple-darwin/release/bundle)
set -euo pipefail

bundle=$(cd "$1" && pwd)
desktop=$(cd "$(dirname "$0")/../../apps/desktop" && pwd)
guide="If Axial won’t open.html"

apps=("$bundle"/macos/*.app)
images=("$bundle"/dmg/*.dmg)
if [[ ${#apps[@]} -ne 1 || ! -d ${apps[0]} || ${#images[@]} -ne 1 || ! -f ${images[0]} ]]; then
  echo "dmg.sh: expected one app in $bundle/macos and one disk image in $bundle/dmg" >&2
  exit 1
fi
app=$(basename "${apps[0]}")
image=${images[0]}

staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
cp "$desktop/platform/install-guide.html" "$staging/$guide"
rm "$image"

cd "$bundle/macos"
"$bundle/dmg/bundle_dmg.sh" \
  --volname "${app%.app}" \
  --volicon "$desktop/icons/macos/icon.icns" \
  --background "$desktop/platform/dmg-background.tiff" \
  --window-size 720 540 \
  --icon-size 128 \
  --text-size 16 \
  --icon "$app" 168 180 \
  --app-drop-link 552 180 \
  --hide-extension "$app" \
  --add-file "$guide" "$staging/$guide" 168 372 \
  --hide-extension "$guide" \
  "$image" "$app"
