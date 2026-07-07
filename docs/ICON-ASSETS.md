# Icon Assets

Use the designer-provided SVG as the source of truth for app icons when the `.ico` small-size frames are not visually approved.

The desktop/taskbar icon uses the full source SVG, including the black squircle container. Keep desktop ICOs high-resolution only: Windows taskbar rendering can select tiny embedded frames and make this logo look like a simplified pixel icon. In-app logo surfaces and frontend static logo/favicon assets use the same SVG with only the squircle container removed, so theme-driven logo color filtering remains owned by the UI.

Developer desktop builds use `apps/desktop/icons/dev/icon.svg` as their source and override only `bundle.icon` through `apps/desktop/tauri.dev.conf.json`. `apps/desktop/build.rs` applies that override for non-release builds so direct `cargo build -p croopor-desktop` debug builds and `task dev` use the same developer icon.

## Preferred workflow

Render desktop/taskbar frames directly from the source SVG at high resolution only. Render frontend logo/favicon frames from the no-squircle SVG.

```sh
mkdir -p tmp/icon-svg-render
cp "$SOURCE_SVG" tmp/icon-svg-render/taskbar-source.svg
cp "$SOURCE_SVG" tmp/icon-svg-render/logo-source.svg
perl -0pi -e 's/\n\s*<!-- App Icon Squircle Container -->\n\s*<rect width="500" height="500" rx="112\.5" fill="#000000" \/>//' tmp/icon-svg-render/logo-source.svg

for size in 128 256; do
  npx --yes @resvg/resvg-js-cli \
    --fit-width "$size" \
    tmp/icon-svg-render/taskbar-source.svg \
    "tmp/icon-svg-render/taskbar-icon-${size}.png"
done

for size in 32 96; do
  npx --yes @resvg/resvg-js-cli \
    --fit-width "$size" \
    tmp/icon-svg-render/logo-source.svg \
    "tmp/icon-svg-render/logo-${size}.png"
done
```

Pack those exact SVG renders into the app assets:

```sh
python3 - <<'PY'
from PIL import Image
from pathlib import Path

base = Path("tmp/icon-svg-render")
taskbar_sizes = [128, 256]
taskbar_frames = [Image.open(base / f"taskbar-icon-{size}.png").convert("RGBA") for size in taskbar_sizes]
logo_frames = {size: Image.open(base / f"logo-{size}.png").convert("RGBA") for size in [32, 96]}

taskbar_frames[taskbar_sizes.index(256)].save("apps/desktop/icons/icon.png")
logo_frames[96].save("frontend/static/logo.png")
logo_frames[32].save("frontend/static/favicon.png")

for target in ["apps/desktop/icons/icon.ico", "assets/icon.ico", "winres/icon.ico"]:
    taskbar_frames[-1].save(target, sizes=[(size, size) for size in taskbar_sizes], append_images=taskbar_frames[:-1])
PY
```

Verify the `.ico` sizes:

```sh
python3 - <<'PY'
from PIL import Image
for path in ["apps/desktop/icons/icon.ico", "assets/icon.ico", "winres/icon.ico"]:
    print(path, sorted(Image.open(path).ico.sizes()))
PY
```

Regenerate the developer icon:

```sh
for size in 128 256; do
  npx --yes @resvg/resvg-js-cli \
    --fit-width "$size" \
    apps/desktop/icons/dev/icon.svg \
    "tmp/icon-svg-render/dev-icon-${size}.png"
done

python3 - <<'PY'
from PIL import Image
from pathlib import Path

base = Path("tmp/icon-svg-render")
frames = [Image.open(base / f"dev-icon-{size}.png").convert("RGBA") for size in [128, 256]]
frames[-1].save("apps/desktop/icons/dev/icon.png")
frames[-1].save("apps/desktop/icons/dev/icon.ico", sizes=[(256, 256), (128, 128)], append_images=[frames[0]])
PY
```

## Alternate workflows

If the supplied `.ico` has approved embedded frames, it can be copied byte-for-byte into the `.ico` targets and used as the source for PNG frame extraction. Verify that the small taskbar frames, especially 16, 24, 32, and 48, are not simplified before using this path.

If the SVG and approved `.ico` are unavailable, the fallback used during icon cleanup was to upscale `frontend/static/logo.png` with nearest-neighbor sampling and regenerate `.ico` files from that PNG. This is only a recovery path; it loses direct SVG rendering and any hand-authored per-size frames.
