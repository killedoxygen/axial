# Linux AppImage build

## Current compositor-visible onboarding failure

2026-10-11, an active unlocked Wayland desktop provides actual compositor screenshots independently of the native capture helper. The recorded `716cd9f3` AppImage has live Name/Next onboarding accessibility, but its1100×720 compositor window capture is uniformly dark. A monitor capture shows the same dark Axial surface beside painted desktop/file-manager content. This establishes a compositor-visible rendering failure; it does not identify a driver or application cause.

The actually loaded bundled engine is WebKit2.54.0/GTK3. Separate supported process-local shared-memory frame-transfer and CPU-raster controls each retain the dark result; neither is shipped as a remedy. All three originals94068/46022/56344 close through the actual application control and join0, with exact group/listener/profile-holder absence. No host graphics, packages, sandbox or security settings change. The fresh export candidate, AppImage `b78aeb6f177f7731e0b89f6afe5a84f64245e31644cd3c15773ff01a62fa5eeb`/bundled executable `b66eb9e9ddf0b6d62304af77b771ea504d7d4d3f24d1976234bea191e1979d47`, also produces a uniformly dark onboarding capture in the ordinary desktop environment without renderer overrides; actual Close15612 joins0. Its authenticated software-X11 [native export journey](native-skins.md#current-linux-native-png-export) renders correctly and completes, qualifying the display boundary separately.

The same current compiled application outside AppImage, using host GTK/WebKit libraries, also captures uniformly dark; original62313 closes normally and joins0. A separate supported Mesa software-EGL control reaches the native parent and actual WebProcess but remains dark;53886 closes normally and joins0. No GPUProcess is observed, and the window's actual EGL renderer is unmeasured, so the control's environment alone does not prove a software-rendered Wayland surface. These outcomes rule out an AppImage-only explanation and provide no product repair.

A minimal ephemeral host GTK3/WebKit window then renders its native label, green/magenta Cairo strip and Close button, while its static cyan/yellow/red HTML remains dark after load finishes. This reproduces the drawing failure independently of Axial's frontend and embedding. Its application-owned Close joins70897/0 and leaves no child owners. The installed candidate's physical-desktop X11 control also captures black, separately with and without the software-GL variable; both actual Close originals17292/20936 join0. Thus an X11 switch alone is not a demonstrated remedy. These are diagnostic controls, not a shipped override or established driver cause.

The next independent WebKit snapshot control produces the correct cyan/yellow/red page in both the default native policy and an explicitly read-back `NEVER` policy: each PNG is1000×487/19,567 bytes, SHA256 `936ca14903e1978d06a1d7b56f7e051b6b4cf043463a2402e67dc63326617f4a`. This snapshot uses a separate rendering/readback path and is not the captured live framebuffer. Both compositor captures remain at their initial frame, including native GTK labels that accessibility reports as updated. A final GTK3-only window with no WebKit import also accepts one native update and changes its accessible label, yet retains the initial green/magenta capture and never executes the queued blue/yellow draw callback before Close. The failure therefore extends to ordinary native GTK redraw delivery; it is not confined to Axial or WebKit. All three actual Close originals24851/14223/7774 join0 with empty process groups. No product workaround or host change follows.

Evidence: `physical-onboarding-current/physical-proof.json`, individual capture facts, compositor window images and hash-bound owner/closure records; `physical-render-prep/{host-static-proof.json,host-static-compositor.png}` records the initial independent drawing control. `physical-render-current/proof.json`, paired snapshot/compositor images, native update logs and joined receipts record the changed boundary. Full-monitor diagnostics remain private. Further unchanged engine flags are deferred. Desktop frame/redraw delivery is now the next diagnostic boundary; its cause and remedy remain unresolved, and isolated X11 outcomes do not waive applicable desktop acceptance.

## Current installed offline journey

2026-10-11, source `e8b44c20b25a7f4b70c1c1a1839f8a8d00f38aef`. Fresh pinned Node24.13.1/Rust1.93.1 frontend and normal optimized x86_64 AppImage build complete. Frontend generation is `ff71ffab3ae2`; AppImage SHA256 is `118540d3b0de2596a81762b9466426c9a5748d5466be7fc9b75bd9529df23550`. Packaging retries with the actual Fedora GStreamer plugin/helper directories and media-framework bundling; original57796 joins0. The earlier generic bundle failure remains recorded, without an established complete cause. No updater artifacts, signing or publication are produced.

The actual AppImage renders on a temporary authenticated, TCP-disabled software X11 display. Fresh native offline onboarding creates LinuxParity; native Create installs Vanilla1.20.1 with2GiB, Vanilla mode and optimization off. Ready → native Launch opens visible Minecraft. Singleplayer creates LinuxSavedParity in Creative mode, visibly enters the world, creates an actual F2 screenshot, saves to title and quits the game cleanly. Axial lists the22-file/15,917,256-byte world and displays the game screenshot in its native lightbox. Real HTTP and install SSE respond200; media serves379,194 PNG bytes with SHA256 `ba677db7527e5b6b4802ab0049f291c449fb4673d3719a9de3157ca82cdc9d13`, exactly matching the game-created file.

Native window Close joins original3064/0; app/game/listener absence and exact save hashes pass. A normal same-profile cold reopen lists the exact world Ready. One native Launch opens a second game; its Singleplayer list identifies the saved world, and Play Selected World visibly reloads the saved scene. Save/Quit to title and Quit Game settle the second original session; both launch intents have durable terminal acknowledgements. Native Close joins original50600/0, both games/apps and both listeners are absent. Normal gameplay adds save files, so the final28-file/16,425,362-byte state is recorded separately from pre-relaunch exact cold equality.

Compact proof: `.rewrite-logs/linux-current/vanilla-journey-final.json`; exact cold manifest/proof: `world-before-cold.json`, `world-cold-exact.json`; visible outcomes: `world-entry-progress.png`, `native-saved-world-list.png`, `native-game-screenshot-lightbox.png`, `game-cold-world-selector.png`, `saved-world-reloaded.png`; transport/settlement: `native-install-sse.json`, `game-screenshot-http.json`, `game-first-settlement-2.json`, `second-game-settlement.json`. Physical Wayland captures remain blank despite live accessibility; their graphics cause is unresolved. Isolated X11 acceptance does not certify physical graphics, audible playback, other platforms, trusted updates, later source edits or full parity.

## Historical package qualification

2026-10-06, source `c471e96ddf12625376e671fb9433a972c43ebb1e`. A normal optimized x86_64 AppImage builds successfully in an isolated Horizon container. This is a diagnostic package and inspected payload, not installed-runtime, publisher-trust or full-parity acceptance. No production source/UI, host packages, application profiles, signing or publication changed.

## Inputs and isolation

Fresh rootless container `installed-linux-TbgISuLv`, ID `feee3cf195795da518368e2b37f34b46dd2660bc02cd9a8204f08f786c315f90`, uses official Rust1.93.1 Bookworm image `docker.io/library/rust@sha256:7c4ae649a84014c467d79319bbf17ce2632ae8b8be123ac2fb2ea5be46823f31`; resolved amd64 image ID `78a50754b62786434bc4ebfd874ef382348701b5f01b2c0d21b8e17cfad97969`. No mounts, GUI/DBus sockets, published ports, host networking, devices or privileged mode. Sources, tools and target belong to its writable layer; shared build verification remains serialized with six Cargo jobs. The older `parity-1f1dbc85` container is absent, not reused.

The tracked archive contains all seven workspace members through `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `core`, `apps`, `scripts` and `frontend`; no `legacy/`. This selected compilation input is not a tagged release-source receipt. The existing frontend generation is `fff9567f184b5b2a0b1a56b5fe9317da75900a9e2d06f8f1bb121a329a6bf411`; its unchanged verifier passes after transfer and after bundling. Frontend source matches its earlier `318e7ede` build. That build used Node24.19.0 against the requested24.13.1; this Linux verifier uses copied Linux OCI Node24.21.0. No fresh frontend rebuild or fully pinned release-toolchain claim follows.

| Input | SHA256 |
| --- | --- |
| `installed-linux-source.tar` | `5fe1a62a625d64d698e1bf838f04f5f474481a9a7c0861051ce2a94ad9818b10` |
| Initial generation archive | `062aa1564a677829a0d01a3a61a3ecd107aaa3dae04299f5888bed16df6645b9` |
| Metadata-free generation archive | `ea9f3c2300418c353e4188cad7a1aa147ba178f26ba997b68a092f0029b031f4` |
| Linux Node binary | `7fde7b8afa198da66257f42ee2001d874c7355631e6d1579a5fb5ef1f246df4c` |

The initial generation transfer twice refuses `generation_inventory_drift`: GNU extraction exposes89 AppleDouble extras alongside84 expected files, with none missing. Preserve that tree at `/work/frontend-dist-with-metadata`. Recreate only the transfer archive using `COPYFILE_DISABLE=1 tar --no-xattrs --no-acls`; the unchanged verifier then passes. No inventory waiver, manifest edit or product fix. An earlier container creation refuses because its workdir does not exist; that unused container is removed and recreated without the workdir. The unused Node extraction container is also removed after its copy. Both are recreatable from retained images; actual build inputs/evidence remain.

Container-only apt supplies the repository's Linux build dependencies plus DBus development and packaging tools: WebKit4.1 `2.50.6`, GTK3 `3.24.38`, DBus `1.14.10`; Cargo/Rust1.93.1. Tauri CLI2.11.2 installs with `--locked` into `/work/tauri-cli`. No host dependency installation follows. See [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/#linux).

## Build and output

Inside `/work/source`, with `/work/tauri-cli/bin` on PATH:

```sh
node scripts/cargo-target.mjs run -- cargo tauri build \
  --target x86_64-unknown-linux-gnu --bundles appimage --no-sign \
  --config '{"build":{"beforeBuildCommand":""},"bundle":{"createUpdaterArtifacts":false}}' \
  -- --locked
```

The existing target lease owns the build; the inline override skips only the already-verified frontend rebuild and updater-artifact generation. Normal application ID/product/capabilities remain unchanged, with no Inspector/debug feature. CLI installation and the real package command both join exit0; optimized application compilation takes3m19s and bundling finishes one AppImage. Compiler dead-code warnings remain recorded, not fixed by this packaging slice. Pinned bundler [extract-and-run support](https://raw.githubusercontent.com/tauri-apps/tauri/tauri-cli-v2.11.2/crates/tauri-bundler/src/bundle/linux/appimage/linuxdeploy.rs) needs no container FUSE or privilege change.

Output under `/work/source/target/x86_64-unknown-linux-gnu/release/`:

| Output | SHA256 |
| --- | --- |
| `axial-desktop` | `aa604c3b3e12ce9223e008c2e27ed34dd21fb1e3f1ac84cede5d44562fc4ccfe` |
| AppDir and extracted `usr/bin/axial-desktop` | `3015f4b9591914653fa9badac0995478bbfb6f1618288c326618da9f209f7119` |
| `bundle/appimage/Axial Rewrite_0.4.0-dev.5_amd64.AppImage` | `d6989cd530d0536a04c520d0d0dd267d675967896769312f41f2635d0f0e77ae` |

AppImage size is120,769,016 bytes. Its identical hash is verified after container→Horizon copy to `/var/tmp/installed-linux.TbgISuLv/candidate.AppImage`, then SCP to local `.rewrite-logs/installed-linux-candidate.AppImage`. Neither copy is launched. No release collector receipt is fabricated for this untagged diagnostic input.

## Payload inspection and limits

- Both main executables are unstripped ELF64/x86_64, with identical BuildID. Ordered needed libraries and `.text`, `.data`, `.data.rel.ro`, `.rela.dyn`, `.rela.plt`, `.eh_frame` match. Exactly three `.rodata` bytes change `UNK`→`APP`, explained by the pinned [package-marker owner](https://github.com/tauri-apps/tauri/blob/tauri-cli-v2.11.2/crates/tauri-bundler/src/bundle.rs#L18-L74), which restores the unpatched release binary after bundling. Dynamic inspection shows added `$ORIGIN/../lib` RUNPATH and relocated/enlarged string table. These measured packaging changes are not whole-binary equality or a semantic-equivalence certificate.
- AppImage extract-only inspection never starts AppRun/the launcher. Its first runtime extraction creates0700 directories, although the SquashFS lists0755. Container-only `squashfs-tools` inspection first exits2 for unsupported destination SELinux xattrs; retain that result. Fresh extraction with explicit `-no-xattrs` exits0: all404 entries match AppDir by kind, file size/hash, mode and symlink target. UID/time/xattr preservation is not certified.
- Actual payload contains `libwebkit2gtk-4.1.so.0`, both WebKit subprocesses, injected bundle, desktop entry and GTK hook. The hook selects `GDK_BACKEND=x11`; Horizon's active KDE Wayland session alone proves neither XWayland nor visible runtime acceptance. The overlay leaves media-framework bundling disabled, so audio portability remains open. See [AppImage guidance](https://v2.tauri.app/distribute/appimage/) and [GTK helper](https://raw.githubusercontent.com/tauri-apps/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh).
- The archive retains an absolute `.DirIcon` target under the container's build path and a root-owned0770 `AppRun.wrapped`. Other desktop/icon links are relative. Record these portability/runtime handoffs; do not infer successful icon resolution or launch outside the container, or silently patch the package.
- Post-build comparison with a fresh source-archive extraction finds every original file unchanged. Only build-generated desktop `gen`, frontend `dist`, frontend `._dist` metadata and `target` are additional. Generation verification does not itself inspect every embedded asset or prove a fresh source rebuild.

Prepared helper hashes are retained because several upstream download URLs are mutable. CLI pinning alone does not pin every packaging byte; linuxdeploy's recorded hash is after the pinned bundler's integration-marker patch. The downloaded output-plugin file is present; the quiet bundle log does not certify whether linuxdeploy selected it or its built-in fallback.

| Prepared helper | SHA256 |
| --- | --- |
| `AppRun-x86_64` | `f30140a43a0a59e46db21bdefdf749b9e9f2c6946e92afabbacf98b8ae73fb4f` |
| `linuxdeploy-x86_64.AppImage` | `20eebde3c18ae2e44279bd624fc72482503aece216d5d77f10932235342f71c1` |
| `linuxdeploy-plugin-gtk.sh` | `7804c9eef13e59bf2783aad9882ef9db8f3f3f9e8d631874b1d348d550a3693f` |
| `linuxdeploy-plugin-gstreamer.sh` | `c107b49d84edbffc6ab226ed1007e0626a4f7aa2c3a36b7782bef62351d49e94` |
| `linuxdeploy-plugin-appimage.AppImage` | `49d6a17160675a6bd1781699aae6bdf7692d98552e02a3671d2183d10547842e` |

## Evidence and remaining work

Logs are `.rewrite-logs/installed-linux-*`: identity/source/generation/setup/CLI/package, `payload-{inspection,detail,proof}`, `appimage-extract`, `squashfs-inspection`, `final-extraction`, `runtime-inputs`, `helper-hashes`, `host-copy`, `local-copy` and `cleanup`. The original research handoff remains at `.rewrite-logs/linux-package-research.md`.

After all commands/copies join, exact container inspection shows only PID1 `sleep`, no build children. `podman stop` escalates that idle PID1 to SIGKILL after10s; stopped container exit137 is not an application/build exit. Exact ID reports `running=false`; writable layer, tools, failed transfers, payload and outputs remain. Package command exit0 is independently joined, not inferred from cleanup or a file appearing.

Independent Standards, Spec and architecture reviews are clear; both two-axis reviewers independently hash the local AppImage. No new application tests or AGENTS.md rule are needed for this source-unchanged packaging/evidence slice.

Next: ordinary launch on Horizon from this hash-bound package, canonical isolated profile and supported native observation; verify runtime permissions/icon portability, WebView/API/SSE/media/opener/game lifecycle and persistence. Host WebKit absence is not automatically a blocker for bundled dependencies. No UI/runtime case in [installed acceptance](../../../acceptance/native/matrix.mjs), other architecture, publisher authenticity, signed updater or full-parity gate is closed by this build.

## Renderer failure and pending packaging remedy

This section records the pre-resume checkpoint; the verified continuation below supersedes its pending implementation/build items, not its retained failures or acceptance limits.

Subsequent ordinary execution of the original `d6989cd5` package reaches its local API (unauthenticated config read refuses401) but WebKit aborts with `EGL_BAD_PARAMETER`. Four fresh canonical `/var/tmp/renderer-loop.*` profiles retain baseline RED, DMABUF-disable RED, host-Wayland-client preload without that abort, then baseline RED again. The successful diagnostic control exposes missing `appsrc`, `autoaudiosink`, `giostreamsrc` and `decodebin`. Each launcher is deliberately terminated143; process survival is not visible UI, audio, ordinary Quit or persistence acceptance. KDE Wayland is active but locked, and supported computer use exposes no Horizon GUI. No host packages, graphics settings, passwords or existing profiles change.

[Tauri CLI2.12.0](https://tauri.app/release/tauri-cli/v2.12.0/) includes [bundler2.10.0](https://tauri.app/release/tauri-bundler/v2.10.0/), whose released packaging correction addresses this EGL boundary. The pending source changes only matching CLI pins in `toolchain.json`/release CI and `bundleMediaFramework` in the existing Linux overlay. Runtime Tauri remains2.11.2; no shipped preload/renderer override, alternate state owner or private bundler fork. The private primary-source handoff is `.rewrite-logs/linux-renderer-research.md`; the observed controls identify a library boundary, not a particular EGL vendor or complete cross-host root cause.

CLI2.12.0 installs successfully into `/work/tauri-cli-fixed`. The same existing target-lease build command then creates a renderer-only candidate, retaining the original bundle at `/work/original-appimage-bundle` and executable at `/work/original-release-desktop`. Its log reports a finished bundle, but interruption makes handle55943 unavailable; the final command exit is not recovered and must not be inferred from that line or the artifact. Read-only inspection finds only PID1 `sleep`, unchanged raw executable SHA256 `aa604c3b3e12ce9223e008c2e27ed34dd21fb1e3f1ac84cede5d44562fc4ccfe`, and new AppImage SHA256 `b4951892ad018bde8206f3ea08b13bef693a32c3938c7b66f96b8db8ca209956`. This build uses the original archived source and media-disabled override, not the three pending repository edits or a rebuilt frontend. Complete new payload/source/helper comparison and no-override renderer verification remain unperformed.

The pending-source delivery/dependency/generation contract selection passes60/0 in1.23s, and `git diff --check` passes. Separate Standards/Spec source and qualification reviews are clear; runtime evidence remains a handoff. Logs retain original renderer controls, CLI installation, interrupted package output and `interrupted-review` inspection/cleanup. The authoritative full-parity goal is paused; this scheduled review does not resume it, run another build or certify the remedy. Exact idle-container stop joins0 and confirms `running=false`; its PID1 requires SIGKILL/137, not an application/build result. The writable layer and every package/profile witness remain.

On authorized continuation: verify the renderer-only package without preload or rendering overrides; independently build the actual media-enabled overlay and inspect required plugin dependencies/playback. Confirm payload/helper hashes, source preservation, icon/mode portability and ordinary native behavior. All installed UI, other-architecture, signing/update and full-parity gates remain open.

## Released packager and media correction

Continuation verified 2026-10-06/07; full-parity goal active again.

The resumed goal integrates three packaging-owned changes over `93ae02b0`: CLI2.12.0 in the existing toolchain/CI pins, media-framework inclusion in the Linux overlay, and explicit base/good GStreamer packages in Linux release prerequisites. Runtime crates, Cargo.lock, product Rust, frontend generation and UI remain unchanged. No shipped preload, rendering override, custom bundler, codec implementation or new state owner.

The preserved `b4951892` renderer-only package passes the original EGL loop without overrides; the original `d6989cd5` package then reproduces that abort. Media remains missing in the renderer-only control. Horizon subsequently restarts: the old session612 disappears and its Xauthority path changes. Three renderer-only probes encounter Wayland protocol error71, including one with the refreshed session; this failure is still unexplained, not fixed by correcting the diagnostic environment. One intermediate fixture fails before launch because it clears the bus environment before querying it; only the private script is corrected. No acceptance follows from that refusal.

The actual media build uses the transferred Linux overlay plus only the already-verified frontend-build skip. Before bundling, the six required elements resolve and the retained MP3 decodes through the container's GStreamer1.22 tools. The existing target lease owns optimized compilation/bundling; handle41989 joins0 and the private runner records `package-command-exit=0`. Unlike the interrupted renderer-only build, this exit is certified. The original selected archive remains the compilation baseline with the three declared packaging inputs transferred; it is not a complete tagged release-source receipt. Final input comparison finds no other original-source differences, and the unchanged generation verifier passes. Existing Node/toolchain qualifications above remain.

| Output | SHA256 |
| --- | --- |
| Raw `axial-desktop` (unchanged) | `aa604c3b3e12ce9223e008c2e27ed34dd21fb1e3f1ac84cede5d44562fc4ccfe` |
| Staged/extracted packaged executable | `a76a16b9c2f5c08bb33ab05e869da6e1e62f3b02eb0345a0d8a15a5c5bd117c5` |
| Media-enabled AppImage, 186,280,440 bytes | `202727ea5b8c06bdf13764e8db095761879a6adaf997e2b8e5bd5c2f19a250fb` |
| Complete staged/extracted inventory | `0075045621f508432aa4c6d49f1274c7c0be79412e1032f0bcfe4d98cfcbb22a` |

Fresh `unsquashfs -no-xattrs` extraction matches all1345 staged entries by kind, bytes/size, mode and symlink target; UID/time/xattrs remain uncertified. The raw binary is byte-identical, but no whole-file equivalence of the packaged executable is asserted. Its RUNPATH is `$ORIGIN/../lib`. The actual payload excludes the Wayland client; GTK no longer forces X11. `.DirIcon` now resolves through relative `Axial Rewrite.png`, and `AppRun.wrapped` is0755. The prior absolute-link/wrapper-mode handoffs therefore do not persist in this artifact; visible icon presentation is still unobserved.

Prepared versioned linuxdeploy SHA256 is `bd9521cd5ff3ca351fecb78cd9b236a4e37b3621014ac05cac0c2d767ffa0474`; embedded GTK/GStreamer helper hashes are `ef6b9a980417243bc62e0241b51dc49876032afd1bab9b4762389f961b406d9b` / `2a15ce9da8de6e20159e1ab27861a7a5ef8758c81a6278ba4ab30cefa1d74c9f`. AppRun and optional output-helper hashes retain the earlier values. Mutable download/selected-output-plugin qualifications remain; a CLI pin is not every packaging byte's trust receipt.

On Horizon, the final AppImage passes bounded startup with ordinary environment, separately with Wayland tracing, and with a diagnostic X11 selection: live non-zombie WebProcess, no original EGL abort and no missing-element errors. Each generated-profile process joins diagnostic termination143, not normal Quit. Mesa/Zink/DRI and host GTK-module warnings remain; neither clean graphics nor the renderer-only protocol failure is diagnosed by these passes. No backend override is shipped.

From the hash-bound extracted payload, matched GStreamer1.22 diagnostic tools load its bundled core and resolve `appsrc`, `autoaudiosink`, `giostreamsrc`, `decodebin`, `mpg123audiodec` and `pulsesink` exclusively from its plugin directory. The same929,167-byte sound sprite (`2c06b318…abc39a1`) decodes through those plugins to a fakesink, exit0. This proves plugin resolution/decoding on Fedora, not WebView asset delivery, audible playback, controls or the audio device. No host packages are installed; only the owned build container gains diagnostic GStreamer tools. The source-generation MP3 is copied as a decoder fixture, not extracted from the executable.

The final focused delivery/dependency/generation selection passes60/0 in0.97s, and whitespace checks pass. Independent Standards and Spec source reviews are clear. Logs are `installed-linux-{renderer-fixed-runtime,renderer-original-resume,media-*,wayland-protocol-owner}`; failures, reboot/session evidence, all packages and generated profiles remain. Supported computer use discovers the existing Horizon RustDesk viewer, but its reconnect fails against its configured obsolete broker address; no viewer/server/security settings are altered. Visible native UI, HTTP/media/SSE journeys, ordinary Quit/reopen, audible playback, other architectures and signing/update/full-parity acceptance remain open.

The identical final AppImage hash survives container→Horizon→local `.rewrite-logs/installed-linux-media-candidate.AppImage` copies. After every build, probe and copy joins, native/WebKit process inventory is empty and the exact owned container has only PID1 `sleep`. Its stop joins0 and confirms `running=false`; idle PID1 again requires SIGKILL/137, not a build/application result. Retain the writable layer, original/renderer-only/final packages, failed probes and generated-profile evidence. No unrelated container, existing profile or user installation is changed.

## Fresh current-source build

2026-10-09 scheduled review records the root-owned continuation at committed `893559c02f124ab1989dc4157bd87589ded43c2b`. A fresh rootless Horizon container uses the repository-pinned CI image, without host mounts, GUI access or privilege changes. Its selected committed-source archive is18,524,160 bytes, SHA256 `fdffd71a36959cf1a98fc2e96b5e10008dd11b43bc121b4e48d706061b7544d3`. Tools and six compilation-cache directories are copied from the preserved stopped container; its executable and bundles are not the new build outputs. Historical artifacts and profiles remain unchanged; no cache deletion occurs.

Actual Node24.13.1/pnpm11.1.3/Rust-Cargo1.93.1/Tauri CLI2.12.0 are checked. The initial tool probe exits127 for missing Node before assertions; provisioning then succeeds. Task is unused and absent, so complete desktop tool-profile verification is not claimed. Fresh frontend build4233 joins0, publishes `4a0aa9e7a2c7`, and passes generation/budget verification. The package uses the committed media-enabled Linux overlay, skipping only that already-built frontend hook.

Original package95271 **joins1**: optimized Rust compilation finishes, but bundling reports `failed to run linuxdeploy`; post-package checks do not run. The proposed bundle-only diagnostic is refused by the existing target supervisor before execution. The supported verbose build12317 then joins1 with the exact cause: the GStreamer helper reports `patchelf not found` and exits2. Independent inventory confirms base/good plugins are already installed; the compile/test image lacks five existing release prerequisites, not a product media implementation. Root reconciles patchelf and the four missing development packages inside only the fresh container, using its existing signed Ubuntu snapshot20260719. Simulation reports32 new packages, zero upgrades/removals,6,376kB download and30.0MB installed growth. Index/provisioning commands join0; no host, image-source, application or UI change follows.

The unchanged original package17574 now **joins0**, verifies generation and reports one AppImage. Its retry warning about the missing unknown bundle marker is retained: actual release and packaged binaries each contain exactly one APP marker and zero UNK/DEB/RPM/NSS markers. This is consistent with the pinned bundler's error-before-restoration path; it is not updater acceptance or a pristine unpatched-executable claim.

| Current output | SHA256 |
| --- | --- |
| Release executable after failed-attempt marker | `97e46f84c24ddb48e876a284c1e9b5e8bc37f2ca96df0a01b1b2becdda46c9da` |
| Staged/extracted executable | `34ff3e6d1c4742b2813889e0e4079c3910005b6df13dbd661039c3752823bd3a` |
| AppImage,113,183,224 bytes | `5751ea099978d9ee025ac8e5db2a0ee4e9dba959f5541927fdaff0103df24848` |

Original verification6459 joins0. Fresh archive comparison preserves all892 original descendants by kind/mode/bytes/size/link; additions and root metadata are excluded, so this is not a complete release-source receipt. Fresh offset-bound `unsquashfs -no-xattrs` comparison matches all782 payload descendants, inventory SHA256 `c083c08d7fc108f292349f5af9bf27f5817566e50b4211d6dfc96eba59dfddfc`. UID/time/xattrs/root metadata remain uncertified. The corrected checker charges both source passes and independently pins the reused digest owner/dependency before execution. Its12,000-entry/3GiB declared-size/192MiB-file prechecks are not race-safe byte, enumeration or heap caps. Checks use quiescent owned fixtures. Package RUNPATH is `$ORIGIN/../lib`, `.DirIcon` is relative, and AppRun.wrapped is0755; whole-binary equivalence and visible icon behavior are not inferred.

After bundling, matched snapshot GStreamer1.24 diagnostic tools resolve all six retained elements exclusively from the extracted plugins; the unchanged929,167-byte source MP3 decodes to a fakesink, joined0. This is container plugin/decoder evidence, not Fedora runtime, embedded-asset delivery, audible playback or WebView controls. Prepared helpers retain the preceding recorded hashes and mutable-download qualifications. The identical AppImage hash/size survive both Horizon and local `.rewrite-logs/current-linux-candidate.AppImage` copies, whose original commands join0.

Logs are `.rewrite-logs/current-linux-*`, including failed original/verbose builds, the supervisor refusal, provisioning, verification, media and copies. A fresh supported RustDesk reconnect still fails against its configured obsolete broker; settings remain unchanged. After commands join, container inventory has only PID1 sleep and the idle frontend esbuild service. Exact generated-container stop21822 joins0 and confirms `running=false`; escalation137 is container cleanup, not a launcher/build exit. Its recoverable layer, package copies and historical evidence remain. Both source-review axes remain unchanged; independent evidence review clears the final qualifications. Visible UI, HTTP/media/SSE, ordinary Quit/reopen, real game, other architectures, signing/update and full-parity acceptance remain open; the full-parity goal stays active.

Later read-only continuation reaches `ssh horizon` with BatchMode and a5s connection bound, joins0 and receives hostname horizon. The retained current-candidate transfer record resolves its exact host path to `/var/tmp/current-linux.6l3fIPuh/candidate.AppImage`; a fresh remote SHA256 query joins0 and matches `5751ea09…df24848`. This changes the earlier SSH-unavailable observation, not native-viewing or runtime acceptance. No package is launched, host/viewer setting changed or profile touched. Mac native inventory is locked, and supported Horizon viewing remains unestablished. Logs: `horizon-current-{access,candidate}.log`. Next gate is ordinary visible startup/audio/Quit/cold reopen from this exact candidate and a fresh admitted profile; decoder/process survival cannot replace it.
