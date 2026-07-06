# Icon Assets

Use the designer-provided SVG as the source of truth for app icons when the `.ico` small-size frames are not visually approved.

## Preferred workflow

Render the SVG at every exact icon size. Do not render one large PNG and resize it down.

```sh
mkdir -p tmp/icon-svg-render
for size in 16 20 24 30 32 36 40 48 60 64 72 80 96 128 256; do
  npx --yes @resvg/resvg-js-cli \
    --shape-rendering 1 \
    --fit-width "$size" \
    "$SOURCE_SVG" \
    "tmp/icon-svg-render/icon-${size}.png"
done
```

Pack those exact SVG renders into the app assets:

```sh
python3 - <<'PY'
from PIL import Image
from pathlib import Path

sizes = [16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 128, 256]
frames = [Image.open(Path("tmp/icon-svg-render") / f"icon-{size}.png").convert("RGBA") for size in sizes]

frames[sizes.index(256)].save("apps/desktop/icons/icon.png")
frames[sizes.index(96)].save("frontend/static/logo.png")
frames[sizes.index(32)].save("frontend/static/favicon.png")

for target in ["apps/desktop/icons/icon.ico", "assets/icon.ico", "winres/icon.ico"]:
    frames[-1].save(target, sizes=[(size, size) for size in sizes], append_images=frames[:-1])
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

## Alternate workflows

If the supplied `.ico` has approved embedded frames, it can be copied byte-for-byte into the `.ico` targets and used as the source for PNG frame extraction. Verify that the small taskbar frames, especially 16, 24, 32, and 48, are not simplified before using this path.

If the SVG and approved `.ico` are unavailable, the fallback used during icon cleanup was to upscale `frontend/static/logo.png` with nearest-neighbor sampling and regenerate `.ico` files from that PNG. This is only a recovery path; it loses direct SVG rendering and any hand-authored per-size frames.
