#!/usr/bin/env node
import { copyFileSync, existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const rootDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const iconsDir = path.join(rootDir, "apps", "desktop", "icons");
const sourceDir = path.join(iconsDir, "source");
const macosDir = path.join(iconsDir, "macos");
const devMacosDir = path.join(iconsDir, "dev", "macos");
const iconSizes = [16, 32, 128, 256, 512];
const designGeneration = 26;
const composerGlyphScale = 2;
const macosIcnsContentScale = 840 / 1024;
const rimErodeScale = 17 / 1024;
const rimDarkMultiplier = 0.28;
const rimMaskStrength = 0.72;

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: rootDir,
    stdio: "inherit",
    ...options,
  });

  if (result.error) {
    throw result.error;
  }

  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed with status ${result.status}`);
  }
}

function findIctool() {
  const candidates = [
    process.env.ICTOOL_PATH,
    "/Applications/Icon Composer.app/Contents/Executables/ictool",
    "/Applications/Xcode.app/Contents/Applications/Icon Composer.app/Contents/Executables/ictool",
    "/Applications/Xcode-beta.app/Contents/Applications/Icon Composer.app/Contents/Executables/ictool",
  ].filter(Boolean);

  const found = candidates.find((candidate) => existsSync(candidate));

  if (!found) {
    throw new Error(
      "Icon Composer ictool was not found. Install Icon Composer or set ICTOOL_PATH.",
    );
  }

  return found;
}

function srgb(hex) {
  const normalized = hex.replace("#", "");
  const values = [0, 2, 4].map((offset) => {
    const channel = Number.parseInt(normalized.slice(offset, offset + 2), 16) / 255;
    return channel.toFixed(5);
  });

  return `srgb:${values.join(",")},1.00000`;
}

function solidFill(hex) {
  return {
    solid: srgb(hex),
  };
}

function writeIconComposerBundle({ bundlePath, glyphPath, fill }) {
  rmSync(bundlePath, { recursive: true, force: true });
  mkdirSync(path.join(bundlePath, "Assets"), { recursive: true });
  copyFileSync(glyphPath, path.join(bundlePath, "Assets", "glyph.svg"));
  writeFileSync(
    path.join(bundlePath, "icon.json"),
    `${JSON.stringify(
      {
        "color-space-for-untagged-svg-colors": "display-p3",
        groups: [
          {
            layers: [
              {
                "image-name": "glyph.svg",
                name: "glyph",
                glass: true,
                position: {
                  scale: composerGlyphScale,
                  "translation-in-points": [0, 0],
                },
              },
            ],
            name: "Foreground",
            specular: true,
            shadow: {
              kind: "neutral",
              opacity: 0.5,
            },
            "blur-material": null,
            translucency: {
              enabled: true,
              value: 0.4,
            },
          },
        ],
        "supported-platforms": {
          squares: "shared",
        },
        fill,
      },
      null,
      2,
    )}\n`,
  );
}

function renderComposerPng({ ictool, bundlePath, outputPath }) {
  run(ictool, [
    bundlePath,
    "--export-image",
    "--output-file",
    outputPath,
    "--platform",
    "macOS",
    "--rendition",
    "Default",
    "--width",
    "1024",
    "--height",
    "1024",
    "--scale",
    "1",
    "--design-generation",
    String(designGeneration),
  ]);
}

function sanitizeAlphaEdges(imagePath) {
  run("magick", [
    imagePath,
    "-channel",
    "RGB",
    "-fx",
    "u*a",
    "+channel",
    "-depth",
    "8",
    `PNG32:${imagePath}`,
  ]);
}

function darkenOuterRim(imagePath, size) {
  const erodeRadius = Math.max(1, Math.round(size * rimErodeScale));
  const alphaPath = `${imagePath}.alpha.png`;
  const innerPath = `${imagePath}.inner.png`;
  const ringPath = `${imagePath}.ring.png`;
  const strengthPath = `${imagePath}.ring-strength.png`;
  const outputPath = `${imagePath}.rim.png`;

  run("magick", [imagePath, "-alpha", "extract", "-colorspace", "Gray", "+profile", "*", alphaPath]);
  run("magick", [alphaPath, "-morphology", "Erode", `Square:${erodeRadius}`, innerPath]);
  run("magick", [alphaPath, innerPath, "-compose", "minus_src", "-composite", ringPath]);
  run("magick", [ringPath, "-evaluate", "Multiply", String(rimMaskStrength), strengthPath]);
  run("magick", [
    imagePath,
    "(",
    imagePath,
    "-channel",
    "RGB",
    "-evaluate",
    "Multiply",
    String(rimDarkMultiplier),
    "+channel",
    "(",
    strengthPath,
    ")",
    "-compose",
    "CopyOpacity",
    "-composite",
    ")",
    "-compose",
    "Over",
    "-composite",
    "-depth",
    "8",
    `PNG32:${outputPath}`,
  ]);

  copyFileSync(outputPath, imagePath);
  rmSync(alphaPath, { force: true });
  rmSync(innerPath, { force: true });
  rmSync(ringPath, { force: true });
  rmSync(strengthPath, { force: true });
  rmSync(outputPath, { force: true });
}

function generateIcns(sourcePng, outputIcns) {
  const iconsetDir = `${outputIcns}.iconset`;
  rmSync(iconsetDir, { recursive: true, force: true });
  mkdirSync(iconsetDir, { recursive: true });

  for (const size of iconSizes) {
    const contentSize = Math.round(size * macosIcnsContentScale);
    const iconPath = path.join(iconsetDir, `icon_${size}x${size}.png`);
    run("magick", [
      sourcePng,
      "-resize",
      `${contentSize}x${contentSize}`,
      "-background",
      "none",
      "-gravity",
      "center",
      "-extent",
      `${size}x${size}`,
      "-channel",
      "RGB",
      "-fx",
      "u*a",
      "+channel",
      "-depth",
      "8",
      `PNG32:${iconPath}`,
    ]);
    darkenOuterRim(iconPath, size);
    sanitizeAlphaEdges(iconPath);

    const retinaSize = size * 2;
    const retinaContentSize = Math.round(retinaSize * macosIcnsContentScale);
    const retinaIconPath = path.join(iconsetDir, `icon_${size}x${size}@2x.png`);
    run("magick", [
      sourcePng,
      "-resize",
      `${retinaContentSize}x${retinaContentSize}`,
      "-background",
      "none",
      "-gravity",
      "center",
      "-extent",
      `${retinaSize}x${retinaSize}`,
      "-channel",
      "RGB",
      "-fx",
      "u*a",
      "+channel",
      "-depth",
      "8",
      `PNG32:${retinaIconPath}`,
    ]);
    darkenOuterRim(retinaIconPath, retinaSize);
    sanitizeAlphaEdges(retinaIconPath);
  }

  run("iconutil", ["-c", "icns", iconsetDir, "-o", outputIcns]);
  rmSync(iconsetDir, { recursive: true, force: true });
}

function generateRasterIcons({ svg, png, ico }) {
  run("magick", ["-background", "none", svg, "-resize", "512x512", "-alpha", "on", "-depth", "8", `PNG32:${png}`]);
  run("magick", [png, "-depth", "8", "-define", "icon:auto-resize=256,64,48,32,24,16", ico]);
}

function generateMacIcon({ glyph, outputDir, fill }) {
  const ictool = findIctool();
  const bundlePath = path.join(outputDir, "icon.icon");
  const renderPath = path.join(outputDir, ".icon-render.png");
  const icnsPath = path.join(outputDir, "icon.icns");

  mkdirSync(outputDir, { recursive: true });
  writeIconComposerBundle({ bundlePath, glyphPath: glyph, fill });
  renderComposerPng({ ictool, bundlePath, outputPath: renderPath });
  sanitizeAlphaEdges(renderPath);
  generateIcns(renderPath, icnsPath);
  rmSync(renderPath, { force: true });
}

generateRasterIcons({
  svg: path.join(sourceDir, "croopor-flat.svg"),
  png: path.join(iconsDir, "icon.png"),
  ico: path.join(iconsDir, "icon.ico"),
});

copyFileSync(path.join(sourceDir, "croopor-dev-flat.svg"), path.join(iconsDir, "dev", "icon.svg"));
generateRasterIcons({
  svg: path.join(iconsDir, "dev", "icon.svg"),
  png: path.join(iconsDir, "dev", "icon.png"),
  ico: path.join(iconsDir, "dev", "icon.ico"),
});

generateMacIcon({
  glyph: path.join(sourceDir, "croopor-glyph.svg"),
  outputDir: macosDir,
  fill: solidFill("#151515"),
});

generateMacIcon({
  glyph: path.join(sourceDir, "croopor-dev-glyph.svg"),
  outputDir: devMacosDir,
  fill: solidFill("#151515"),
});

rmSync(path.join(iconsDir, "icon.icns"), { force: true });
rmSync(path.join(iconsDir, "dev", "icon.icns"), { force: true });
