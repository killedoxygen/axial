# Design Guardrails
This project is a desktop Minecraft launcher, not a marketing site. Keep UI work restrained, workflow-first, and consistent with the existing launcher primitives.

## Product Shape
- Design for desktop launcher windows and compact desktop windows.
- Do not optimize or smoke-test phone/mobile layouts unless mobile becomes a product requirement.
- Build the usable workflow first. Avoid landing-page, hero, or explanatory UI patterns inside the app.
- Prefer dense but readable operational surfaces over decorative layouts.

## Visual Direction
- Use existing Croopor primitives before inventing new ones:
  - `Button`, `IconButton`, `Input`, `Pill`, `Segmented`, `Card`, `SectionHeading`, dialogs, context menus, resource/log layouts.
- Use the Modrinth App as a product reference where a launcher workflow needs precedent, but do not copy its code, Vue components, or exact layout.
- Depth model: elevation does hierarchy, accent does action. A deep chassis (`--bg-deep`) holds the content panel (`--bg`); cards are solid raised surfaces (`--surface`, `--shadow-raised`, no border); controls on cards sit at `--surface-2`; hover is `--surface-3`. Recessed wells (search fields, segmented tracks) use `color-mix(in oklab, var(--bg) 55%, var(--surface))`.
- Neutrals are not gray: the whole surface stack carries a low-chroma tint of the accent hue, rebuilt at runtime by `applyCssVars()` and `buildNeutrals(dark, hue)`. Never hardcode a neutral with a hue that fights the accent.
- Borders are reserved for genuine separators (row dividers, table heads), dashed empty states, selection states, and floating overlays. Never use a border to frame a card or a button.
- Accent is rationed: primary action, active nav, selection. Everything else is raised neutral or ghost. Row-level Play buttons are secondary; only the featured action is primary.
- Typography is one voice: Manrope everywhere (vendored in `frontend/static/fonts/`). Monospace only for actual log output. Use tabular figures (`font-variant-numeric`) for aligned numbers, not a different font.
- Radii are generous and friendly: `--r-xs` 8 / `--r-sm` 12 / `--r-md` 16 / `--r-lg` 20 / `--r-xl` 28. Cards use `--r-lg`, buttons and inputs `--r-sm`. Do not hardcode radii; consume the vars.
- Home is workflow-first: a full-width featured banner for the last-played instance (`.cp-feature`) over a fluid cover-card library grid (`.cp-cover-grid` / `InstanceCard`), no vanity stat cards. Instances uses the same cover grid as its default view; the table is the secondary mode. Layouts must fill wide viewports with fluid `auto-fill` grids rather than capping into empty margins.
- Instance chrome uses deterministic square identity tiles (`ui/InstanceVisual.tsx`): the surface stack carries a low-chroma tint of the instance hue (`art_seed` -> `--cp-tile-h`, mixed via `color-mix` so it tracks the theme) with a dim loader mark, using the creeper face for vanilla. Banners are never generated imagery: the home hero is a raised surface panel with a quiet accent glow (`.cp-feature-glow`); the instance cover is the deep backdrop with its accent glow and vignette. No screenshots, world images, or version text in UI chrome, no procedural noise art, no saturated gradient avatars, no monograms.
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

## Selection & Active States
- One selection language everywhere: solid `--accent-fill` background with `--accent-on` content (the onboarding pill pattern). No translucent accent washes, no accent borders for selection. Applies to rail nav, version rows, source tiles, runtime presets, icon-button active, settings rail, on/off pills.
- Mode switches (segmented controls, channel tabs, mini-seg) stay neutral: raised `--surface-2` thumb on a recessed track.

## Create Instance
- Creating an instance is a modal (`createOpen` signal in `ui-state.ts`), not a route: it pops over the current view with a scrim and closes on Esc/Cancel/success. One screen, no steps: source tiles, channel tabs, searchable version list in a recessed well (left) and identity preview, name, memory, window/profile rows (right).

## Instance Tabs
- Every tab is "toolbar row (30px controls) + raised panel" so switching tabs never shifts layout (`.cp-resource-toolbar` + panel).
- Logs: the latest/current log renders by default at a fixed-height viewer (`.cp-logview`); past logs live behind a select. Log line colors derive from theme tokens, never hardcoded hues.
- Instance settings: one raised sheet (`.cp-iset`) of hairline-divided sections, no master-detail nav, decoupled from the logs UI.
- Overview first bento row (Worlds/Activity) is fixed-height so empty and populated states do not reflow.
- Context menus are expected on operational rows: worlds (tab and overview card), screenshots, mods, instance rows/cards.

## Layout Rules
- Preserve existing grid and bento alignment unless a planned visual pass explicitly changes it.
- Do not add policy/configuration blocks inside overview cards when that breaks card height balance. Instance overview cards should summarize; settings surfaces should edit.
- Keep text within controls and compact panels. Prefer tighter copy to larger containers.
- Do not use viewport-scaled font sizes or nonzero letter spacing.
- Avoid new decorative gradients, blobs, bokeh, or one-note palettes.
- Use theme variables and color-mix patterns already present in the app. Do not hardcode state colors when theme-derived colors are expected.

## Sensitive Surfaces
Frontend surfaces render backend-authored policy. Do not add UI code that decides readiness, classifies exits, parses raw JVM args, infers install repair state, decides performance health, or chooses Guardian/Healing precedence. Use backend notices, actions, operation states, and view models as the display contract.

### InstanceDetail
- Do not reintroduce a persistent Guardian preflight card in the overview without a planned design.
- Do not put performance policy controls inside the overview Performance card.
- Keep the Performance card close to the original bento role: plan summary, runtime/readiness facts, and memory scanability.
- Preserve Worlds, Screenshots, Logs, and Settings as operational tabs using existing resource/log primitives.

### CreateView
- The create modal is a compact two-step card, not a full-page create route: version/source first, then name and launch defaults.
- Source picker stays a straight-line row of icon-plus-label tiles, not a card grid.

### Accounts & Skins
- The sticky 3D stage on the left is a recessed display case (`.cp-skinstage` uses the recessed-well mix on `--r-xl`), holding a transparent canvas over a soft dark contact shadow, with exactly one action cluster and an optional one-line caption under the model. No pills, badges, hashes, or segmented controls on the stage.
- The model always plays a gentle ambient walk (limb swing plus bob, paused while dragging) and faces slightly toward the content column; tile snapshots face slightly back toward the stage. Do not gate the canvas idle animation on reduced motion.
- The stage nametag is the in-game-style dark tag; when the offline identity is active it is an editable button that opens the rename prompt.
- The right column is two headed sections (`.cp-skin-section__head`, no disclosure chrome): "Library" (count chip, dashed Add tile, current-profile tile, saved tiles), a compact "Default skins" strip (`.cp-skin-strip`) of the nine bundled Mojang defaults (`src/default-skins.ts`), and the cape picker when the active Minecraft account exposes capes.
- Default skins never duplicate into the Library: their backend texture keys come from `/skins/normalize` lazily (`defaultSkinTextureKey`, `defaultSkinTextureKeys`), matching records are hidden from the saved grid, and the default tile itself carries selection plus the equipped/queued chip. Applying a default reuses its existing record instead of saving a copy.
- Saved skins render as large tiles (`.cp-skin-grid`) using static 3D bust snapshots from the shared offscreen renderer (`skin-snapshot.ts`, one WebGL rig, module-level cache keyed by texture/variant/cape). Selection is an accent inset border plus tint; equipped/queued is a small corner chip; names appear only on hover/selection overlays.
- Player heads everywhere (`PlayerHeadPreview`) render real skin textures: the effective account head comes from `accountSkinSrc` (`src/player-skin.ts`), using the active Minecraft profile texture for online mode and the account-keyed local wardrobe selection for offline identities, falling back to Steve. There is no generated avatar art and no separate offline/online skin-selection flow.
- One interaction model: clicking anything (saved tile, default tile, profile tile, username search result) previews it on the stage; the stage's single primary action commits, saving into the local library first when the source is not saved yet. Tiles never trigger side effects on click.
- Sources stay first-class and minimal: the username lookup row on top (`/skin/lookup` + `/skins/from-username`, result previews directly on the stage with Dismiss/Save/Apply), the dashed Add skin tile/drop zone, and the current-profile tile with a context menu for resets.
- Feedback discipline: successes are transient toasts; only contextual errors render inline. When online apply is unavailable, Apply is replaced by Save plus a one-line sign-in hint instead of disabled buttons.
- Upload and edit happen in modals (`.cp-skinedit-modal`): live 3D preview left, name/model/cape/texture fields right, Save / Save & apply footer. No inline edit panels on the page.
- Save before apply: fetched/uploaded skins are stored locally first, then queued via the deferred apply flow with visible queued state and cancel.
- Account switching lives in a header chip plus modal (`AccountSwitcher`): backend-listed Microsoft accounts (native Microsoft sign-in window, refresh, select, per-account sign out/remove) and local offline identities with create/rename/remove; switching writes `launch_auth_mode` and the selected offline username.
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
