export const APPLICATION_ID = "com.mateoltd.axial.rewrite";

export const PLATFORMS = Object.freeze({
  "linux-amd64": { os: "linux", arch: "x64", target: "x86_64-unknown-linux-gnu", updater: "linux-x86_64", bundles: "appimage" },
  "windows-amd64": { os: "win32", arch: "x64", target: "x86_64-pc-windows-msvc", updater: "windows-x86_64", bundles: "nsis" },
  "macos-amd64": { os: "darwin", arch: "x64", target: "x86_64-apple-darwin", updater: "darwin-x86_64", bundles: "app,dmg" },
  "macos-arm64": { os: "darwin", arch: "arm64", target: "aarch64-apple-darwin", updater: "darwin-aarch64", bundles: "app,dmg" },
});

const VERSION = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(dev|alpha|beta|rc)\.([1-9]\d*))?$/;

export function releaseIdentity(tag) {
  if (typeof tag !== "string" || !tag.startsWith("v") || !VERSION.test(tag.slice(1))) {
    throw new Error("invalid_release_tag");
  }
  return { tag, version: tag.slice(1) };
}

export function platformInfo(platform) {
  if (!Object.hasOwn(PLATFORMS, platform)) throw new Error("unsupported_artifact_platform");
  return PLATFORMS[platform];
}

export function payloads(platform, tag) {
  platformInfo(platform);
  const { version } = releaseIdentity(tag);
  const prefix = `axial-rewrite-${platform}-${version}`;
  if (platform.startsWith("linux")) return [
    { name: prefix, role: "portable" },
    { name: `${prefix}.tar.gz`, role: "portable-archive" },
    { name: `${prefix}.AppImage`, role: "updater" },
  ];
  if (platform.startsWith("windows")) return [
    { name: `${prefix}.exe`, role: "portable" },
    { name: `${prefix}.zip`, role: "portable-archive" },
    { name: `${prefix}-setup.exe`, role: "updater" },
  ];
  return [
    { name: `${prefix}.tar.gz`, role: "portable-archive" },
    { name: `${prefix}.dmg`, role: "installer" },
    { name: `${prefix}.app.tar.gz`, role: "updater" },
  ];
}

export function hostPlatform() {
  return Object.keys(PLATFORMS).find((key) => {
    const { os, arch } = PLATFORMS[key];
    return os === process.platform && arch === process.arch;
  }) ?? null;
}
