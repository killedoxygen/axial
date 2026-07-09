# Design Guardrails
This project is a desktop Minecraft launcher, not a marketing site. Keep UI work restrained, workflow-first, and consistent with the existing launcher primitives.

## Product Shape
- Design for desktop launcher windows and compact desktop windows.
- Do not optimize or smoke-test phone/mobile layouts unless mobile becomes a product requirement.
- Build the usable workflow first. Avoid landing-page, hero, or explanatory UI patterns inside the app.
- Prefer dense but readable operational surfaces over decorative layouts.

## Visual Direction
- Use existing Axial primitives before inventing new ones:
  - `Button`, `IconButton`, `Input`, `Pill`, `Segmented`, `Card`, `SectionHeading`, dialogs, context menus, resource/log layouts.
- Use the Modrinth App as a product reference where a launcher workflow needs precedent, but do not copy its code, Vue components, or exact layout.
- Depth model: elevation does hierarchy, accent does action. A deep chassis (`--bg-deep`) holds the content panel (`--bg`); cards are solid raised surfaces (`--surface`, `--shadow-raised`, no border); controls on cards sit at `--surface-2`; hover is `--surface-3`. Recessed wells (search fields, segmented tracks) use `color-mix(in oklab, var(--bg) 55%, var(--surface))`.
- Neutrals are not gray: the whole surface stack carries a low-chroma tint of the accent hue, rebuilt at runtime by `applyCssVars()` and `buildNeutrals(dark, hue)`. Never hardcode a neutral with a hue that fights the accent.
- Borders are reserved for genuine separators (row dividers, table heads), dashed empty states, selection states, and floating overlays. Never use a border to frame a card or a button.
- Accent is rationed: primary action, active nav, selection. Everything else is raised neutral or ghost. Row-level Play buttons are secondary; only the featured action is primary.
- Typography is one voice: Manrope everywhere (vendored in `frontend/static/fonts/`). Monospace only for actual log output. Use tabular figures (`font-variant-numeric`) for aligned numbers, not a different font.
- Radii are generous and friendly: `--r-xs` 8 / `--r-sm` 12 / `--r-md` 16 / `--r-lg` 20 / `--r-xl` 28. Cards use `--r-lg`, buttons and inputs `--r-sm`. Do not hardcode radii; consume the vars.
- Home is workflow-first: a full-width featured banner for the last-played instance (`.cp-feature`) over a fluid cover-card library grid (`.cp-cover-grid` / `InstanceCard`), no vanity stat cards. Instances uses the same cover grid as its default view; the table is the secondary mode. Layouts must fill wide viewports with fluid `auto-fill` grids rather than capping into empty margins.
- Instance chrome uses deterministic square identity tiles (`ui/InstanceVisual.tsx`): the surface stack carries a low-chroma tint of the instance hue (`art_seed` -> `--cp-tile-h`, mixed via `color-mix` so it tracks the theme) with a dim loader mark, using the creeper face for vanilla. Banners are never generated imagery: the home hero is a raised surface panel with a quiet accent glow (`.cp-feature-glow`); the instance detail page's atmosphere is the aurora stage derived from the tile hue. No screenshots, world images, or version text in UI chrome, no procedural noise art, no saturated gradient avatars, no monograms.
- Keep controls familiar:
  - icons for small actions;
  - segmented controls for small mode sets;
  - dropdowns for larger option sets: always `ui/Select.tsx` (`SelectField`), never a native `<select>`;
  - native disclosures for secondary or developer-only detail;
  - context menus for secondary row actions.
- Modals use `ui/Modal.tsx` (`Modal`/`ModalContent`/`ModalHeader`/`ModalFooter`/`ModalTitle`/`ModalDescription`/`ModalClose`): portal rendering, scrim + Escape dismiss, focus trap and focus restore. Panels style themselves via `className`; pass `showCloseButton={false}` when the panel carries its own close.
- Primitive policy: shadcn/ui is the **design and API reference** (component decomposition, `data-slot` conventions, behavior contract), but its Radix runtime does not render reliably under preact/compat. The dialog mounted only its overlay. Behavioral primitives are therefore implemented directly in Preact inside `ui/`, matching the shadcn contract, styled with `cp-*` classes. Do not add `@radix-ui/*` dependencies without smoke-testing the rendered output in the app first.
- Text inputs and select triggers focus with a neutral ring (stronger hairline + text-tinted halo), never accent. Accent rings are for interactive focus-visible on buttons only.
- "Already installed" on version rows is the `download` icon (OpenAI icon set), not a colored status dot.
- Do not use `cp-section-eyebrow`.
- Do not add broad card-heavy layouts. Avoid nested cards.
- Cards are acceptable for repeated items, existing framed tools, and current surfaces that already use them. Do not introduce cards as a default spacing device.

## Shell
- The sidebar is a fixed 68px icon rail (`.cp-rail`): brand, search, Home/Instances/New, instance identity tiles, settings, player head. There is no expanded sidebar mode; labels live in tooltips and the command palette.
- Active instance tile: full-color tile plus raised shadow while siblings sit dimmed. Do not use rings or borders because they clip in the scroll container. Running instances get a status dot. Keep rail items 44px.
- Desktop window chrome is platform-owned where possible: Linux uses the base native-decorated Tauri window, macOS uses native decorations with the overlay traffic-light titlebar, and Windows keeps the custom frameless shell. The frontend consumes the shell-authored chrome mode from Tauri instead of sniffing browser user agents. Keep duplicated `app.windows` blocks in platform-specific Tauri configs in sync because platform config arrays replace the base array.

## Selection & Active States
- One selection language everywhere: solid `--accent-fill` background with `--accent-on` content (the onboarding pill pattern). No translucent accent washes, no accent borders for selection. Applies to rail nav, version rows, source tiles, runtime presets, icon-button active, settings rail, on/off pills.
- Mode switches (segmented controls, channel tabs, mini-seg) stay neutral: raised `--surface-2` thumb on a recessed track.

## Create Instance
- Creating an instance is a modal (`createOpen` signal in `ui-state.ts`), not a route: it pops over the current view with a scrim and closes on Esc/Cancel/success. Two steps in one card: Version (source tiles, channel tabs, searchable version list in a recessed well, plus the loader-build picker for modloader sources) then Details (identity preview, color, name, memory, window/profile/auto-optimize rows). The header keeps the thin Version–Details progress ruler.
- The card is a raised surface with an inset top light, a 1px hairline ring, and the deep `--shadow`; head/foot separators are edge-faded gradient hairlines, not full-bleed borders.
- The loader-build picker lives in the version step's footer row as a `SelectField` defaulting to the backend-authored "Automatic" option; pinned builds use `loader_build|<component>|<build_id>` selection ids from `/instances/create-view/loader-builds`.
- The Details step leads with identity: 84px tile with a hue-tinted ambient shadow, kicker (source pill + Minecraft version), and the name `Input` in place of a title (the name is never duplicated as both title and field). Below it sits the Color row: flat solid `oklch` hue dots (no borders, gradients, or separators; active = surface-gap halo ring) plus a shuffle button that regenerates the whole palette.
- The look guardian (`ui/look-guardian.ts`) prevents bad colors instead of warning about them: palettes are seed-decorrelated, gap-relaxed samples over seeded profiles inside the theme-harmonious hue domain (25–80° analogous offsets only; no complementary jumps, because the instance art and aurora sit next to the live accent). Shuffle regenerates the whole palette and the selected color is always one of the visible swatches. Existing instance seeds resolve through the same guard at render time so theme changes do not leave clashing art behind. Its warn-shaped verdict (`assessTileHue`) exists for surfaces where prevention is impossible.
- Auto-optimize renders as a Toggle row using the backend `optimize_option` view model (label, detail, default state); the frontend never authors that copy.

## Instance Detail
- The page sits on the shared `.cp-view-page` scaffold (max-width 1480, centered) like every other view; no full-bleed gutters or fixed docks.
- There is no Overview tab. The view is: unboxed hero, tab row, then the active tab's content directly. Everything glanceable lives in the hero (identity, loader/version, last played/created, running state); everything operational lives in a tab.
- The page atmosphere is the aurora stage: the instance's own tile hue (`art_seed % 360` -> `--cp-aurora-h`) rendered as blurred, masked radial gradients behind the top of the page, breathing slowly while the instance runs. This full-bleed treatment is exclusive to the detail page; never reuse it inside modals or cards.
- The hero is unboxed (no card): 104px tile with a hue-tinted ambient shadow, kicker (loader pill + Minecraft version), 34px title, status meta line, and the launch cluster on the right. While running, the launch cluster becomes the session control (live elapsed timer + Stop). Launch is the only saturated action in the hero.
- The tab row is section navigation and uses the selection language: ghost icon+label buttons, hover `--surface-2`, active `--accent-fill`/`--accent-on`, no box-shadow. Labels collapse to icons below 740px; never a scrolling tab strip.
- Every tab is "toolbar row (30px controls) + raised panel" so switching tabs never shifts layout (`.cp-resource-toolbar` + panel).
- Logs: the latest/current log renders by default at a fixed-height viewer (`.cp-logview`); past logs live behind a select. Log line colors derive from theme tokens, never hardcoded hues.
- Instance settings: one raised sheet of hairline-divided rows using the shared settings primitives (`ui/SettingsSheet.tsx`: `SettingsSection`/`SettingRow`/`OverrideChip`, `.cp-sheet*`), no master-detail nav, decoupled from the logs UI. Global settings use the same sheet language — there is no card-per-setting layout. Every control auto-saves on commit (no Save button); instance rows that can inherit the global default show a neutral "Overridden" pill with a quiet Reset that writes the empty/zero sentinel. Persistent mode choices (performance, guardian, launch profile) use `ui/OptionList.tsx` rows with the accent-fill selection language. Backend performance-health warnings render as a `.cp-notice` above the sheet.
- Context menus are expected on operational rows: worlds, screenshots, mods, instance rows/cards.

## Layout Rules
- Preserve existing grid alignment unless a planned visual pass explicitly changes it.
- Keep text within controls and compact panels. Prefer tighter copy to larger containers.
- Do not use viewport-scaled font sizes or nonzero letter spacing.
- Avoid new decorative gradients, blobs, bokeh, or one-note palettes.
- Use theme variables and color-mix patterns already present in the app. Do not hardcode state colors when theme-derived colors are expected.

## Sensitive Surfaces
Frontend surfaces render backend-authored policy. Do not add UI code that decides readiness, classifies exits, parses raw JVM args, infers install repair state, decides performance health, or chooses Guardian/Healing precedence. Use backend notices, actions, operation states, and view models as the display contract.

### InstanceDetail
- Do not reintroduce an Overview tab, summary cards, or a persistent Guardian preflight card without a planned design; the hero plus content tabs is the whole surface.
- Performance health is backend-authored: the Settings tab displays the `/performance/health` view model as a notice when its tone is warn/err, and never derives health in the frontend.
- Preserve Worlds, Screenshots, Logs, and Settings as operational tabs using existing resource/log primitives.

### CreateView
- The create modal is a compact two-step card, not a full-page create route: version/source first, then name and launch defaults.
- Source picker stays a straight-line row of icon-plus-label tiles, not a card grid.
- Loader builds are backend-classified: the picker renders `/instances/create-view/loader-builds` labels (Automatic, Stable/Beta, Recommended, Installed, disabled reasons) verbatim and never parses build ids or provider payloads.
- The color guard (`ui/look-guardian.ts`) is presentation-layer only: it reasons about rendered theme tokens (hue distance to the live theme), never about backend policy, and stays a pure function of `(hue, Theme)` so any surface (create, settings) can reuse it and react to theme changes.

### Accounts & Skins
- The page is the "skin hall" (`.cp-skinhall`): a two-column full-height split with no `.cp-view-page` scaffold. The left column is an unboxed, sticky, viewport-height 3D stage (`.cp-skinhall__stage`); the right column (`.cp-skinhall__work`) holds the page header, finder, and sections, and fills wide viewports with fluid grids instead of capping into empty margins.
- The stage is not a card: a transparent full-height canvas over a quiet accent-derived floor glow (`.cp-skinhall__backdrop`), separated from the content column by an edge-faded gradient hairline. The foot cluster overlays the bottom on a soft bottom scrim: a two-line caption (name in `--text`, status in `--text-dim`, no text separators like middle dots), exactly one action cluster (one primary, one secondary, one overflow menu that never reshuffles), and the drag-to-rotate hint. No pills, hashes, or segmented controls on the stage.
- The model always plays a gentle ambient walk (limb swing plus bob, paused while dragging) and faces slightly toward the content column; tile snapshots face slightly back toward the stage. Do not gate the canvas idle animation on reduced motion, but the render loop must pause while the document is hidden or the canvas is offscreen.
- The stage nametag is the in-game-style dark tag; when the offline identity is active it is an editable button that opens the rename prompt.
- The right column order: page header (title plus account chip), the full-width player finder (`.cp-skinfinder`: the recessed `Input` at hero weight, `/skin/lookup` + `/skins/from-username`, Enter affordance, result previews on the stage with Dismiss/Save/Apply), then "Library" (count chip, dashed Add tile/drop zone, current-profile tile with reset context menu, saved tiles), a compact "Default skins" strip (`.cp-skin-strip`) of the nine bundled Mojang defaults (`src/default-skins.ts`), and the cape picker when the active Minecraft account exposes capes.
- Default skins never duplicate into the Library: their backend texture keys come from `/skins/normalize` lazily (`defaultSkinTextureKey`, `defaultSkinTextureKeys`), matching records are hidden from the saved grid, and the default tile itself carries selection plus the equipped/queued chip. Applying a default reuses its existing record instead of saving a copy.
- Saved skins render as portrait tiles (`.cp-skin-grid`) using static waist-up 3D snapshots from the shared offscreen renderer (`skin-snapshot.ts`, one WebGL rig, module-level cache keyed by texture/variant/cape, IndexedDB-persisted, version-keyed; bump `SNAPSHOT_RENDER_VERSION` whenever framing changes). The bust is contained (never cover-cropped), bottom-anchored with its cut edge dissolved by a mask fade, over a quiet accent glow; saved/profile tiles carry an in-flow centered name plate below the figure (no gradient scrim), while the compact default strip keeps hover-only overlay labels. Full-body tile snapshots were rejected; portraits are the language. Selection is an accent inset border plus tint; equipped is the solid accent corner chip; queued is the hollow accent-ring chip. State chips derive from theme tokens, never fixed warn/info hues.
- Player heads everywhere (`PlayerHeadPreview`) render real skin textures: the effective account head comes from `accountSkinSrc` (`src/player-skin.ts`), using the active Minecraft profile texture for online mode and the account-keyed local wardrobe selection for offline identities, falling back to Steve. There is no generated avatar art and no separate offline/online skin-selection flow.
- One interaction model: clicking anything (saved tile, default tile, profile tile, username search result) previews it on the stage; the stage's single primary action commits, saving into the local library first when the source is not saved yet. Tiles never trigger side effects on click.
- Feedback discipline: successes are transient toasts; only contextual errors render inline. When online apply is unavailable, Apply is replaced by Save plus a one-line sign-in hint instead of disabled buttons.
- Upload and edit happen in modals (`.cp-skinedit-modal`): live 3D preview left, name/model/cape/texture fields right, Save / Save & apply footer. No inline edit panels on the page.
- Save before apply: fetched/uploaded skins are stored locally first, then queued via the deferred apply flow with visible queued state and cancel.
- State plumbing is machine-owned: `machines/accounts.ts` is the single shared accounts/auth snapshot plus serialized account ops; `machines/skin-wardrobe.ts` owns saved-skin records, the single stage-selection union, the serialized wardrobe op, and the error notice. Components read the signals; staged-file concerns (upload/edit/lookup forms, drag-drop) stay in hooks that call machine actions. No parallel busy flags, no duplicated fetches.
- Skin selection is per-account state: when the wardrobe context's account key changes, the stage selection resets and is rebuilt from the new identity's stored preference (`selectedSkinForAccount`); a selection must never survive an account switch just because the saved-skin library is global.
- Account switching is one DRY surface (`AccountSwitcherPanel` rendered by the global host): a popover anchored to whatever triggered it (the accounts-page chip) with an expand action into the centered modal, or the centered modal directly when opened without an anchor (rail user menu). Content: active-identity header (head, name, backend detail, actions menu), "Switch to" rows, quiet add rows for Microsoft sign-in and offline identities; switching writes `launch_auth_mode` and the selected offline username. The rail prefetches the host chunk on hover so opening never lags.
- NameMC skin discovery remains deferred until a stable allowed API boundary is verified.

### Settings Performance
- Keep normal settings focused on launch behavior and rule readiness.
- Keep proof, benchmark, and developer-only detail behind Advanced/dev disclosures.
- Use compact controls rather than tile grids or explanatory cards for every choice.

## Review Before UI Edits
Before changing UI layout, answer these in the plan or worker prompt:

- Which existing primitive or local layout is being reused?
- Is this a workflow feature, a bug fix, or a visual redesign?
- Does it touch a user-sensitive surface listed above?
- Does it add cards, nested cards, decorative styling, or large explanatory copy?
- Does it need a desktop smoke pass, or are type/build checks enough?

If the answer is uncertain, keep the change smaller or stop for a roadmap checkpoint.
