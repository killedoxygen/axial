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
- Borders are reserved for genuine separators (row dividers, table heads), dashed empty states, selection states, and floating overlays. Never use a border to frame a card or a button.
- Accent is rationed: primary action, active nav, selection. Everything else is raised neutral or ghost. Row-level Play buttons are secondary; only the featured action is primary.
- Typography is one voice: Geist everywhere (vendored in `frontend/static/fonts/`). Monospace only for actual log output. Use tabular figures (`font-variant-numeric`) for aligned numbers, not a different font.
- Radii are generous and friendly: `--r-xs` 8 / `--r-sm` 12 / `--r-md` 16 / `--r-lg` 20 / `--r-xl` 28. Cards use `--r-lg`, buttons and inputs `--r-sm`. Do not hardcode radii; consume the vars.
- Home is workflow-first: continue strip plus recent rows, no greeting hero or vanity stat cards.
- Keep controls familiar:
  - icons for small actions;
  - segmented controls for small mode sets;
  - dropdown/select controls for larger option sets;
  - native disclosures for secondary or developer-only detail;
  - context menus for secondary row actions.
- Do not use `cp-section-eyebrow`.
- Do not add broad card-heavy layouts. Avoid nested cards.
- Cards are acceptable for repeated items, existing framed tools, and current surfaces that already use them. Do not introduce cards as a default spacing device.

## Shell
- The sidebar is a fixed 68px icon rail (`.cp-rail`): brand, search, Home/Instances/New, instance art tiles, settings, player head. There is no expanded sidebar mode; labels live in tooltips and the command palette.
- The active instance tile gets an accent ring; running instances get a status dot. Keep rail items 44px.

## Layout Rules
- Preserve existing grid and bento alignment unless a planned visual pass explicitly changes it.
- Do not add policy/configuration blocks inside overview cards when that breaks card height balance. Instance overview cards should summarize; settings surfaces should edit.
- Keep text within controls and compact panels. Prefer tighter copy to larger containers.
- Do not use viewport-scaled font sizes or nonzero letter spacing.
- Avoid new decorative gradients, blobs, bokeh, or one-note palettes.
- Use theme variables and color-mix patterns already present in the app. Do not hardcode state colors when theme-derived colors are expected.

## Sensitive Surfaces
### InstanceDetail
- Do not reintroduce a persistent Guardian preflight card in the overview without a planned design.
- Do not put performance policy controls inside the overview Performance card.
- Keep the Performance card close to the original bento role: plan summary, runtime/readiness facts, and memory scanability.
- Preserve Worlds, Screenshots, Logs, and Settings as operational tabs using existing resource/log primitives.

### CreateView
- Treat user-owned `cp-cr-channels`, `cp-cr-subline`, source rail wrapping, tooltip/hover spacing, and adjacent create-flow styling as protected unless explicit direction says otherwise.
- The source picker direction is a straight-line rail with icon plus label, not a two-row card grid.
- Avoid broad CreateView redesigns while local CreateView files are dirty.

### Accounts & Skins
- Uploaded skins are v1 scope.
- Use the planned selected-preview plus saved-library model from `tmp/launcher-quality/PHASE3-SKINS-PLAN.md`.
- Prefer workflow additions over page redesign:
  - save locally;
  - preview selected skin;
  - apply explicitly;
  - edit metadata;
  - inspect layers/front/back;
  - use context menus for secondary row actions.
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
