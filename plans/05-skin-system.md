# Plan 5: Skin System

**Priority**: Lowest — nice-to-have, partially depends on MSA Auth (Plan 4) for full value. Can be partially implemented standalone.

**Goal**: Let users see and manage their skin from the launcher, and improve skin visibility on servers.

---

## Background

As discussed, skin visibility in Minecraft is fundamentally a two-party problem:
- **Your own view**: The client can show any skin locally
- **Others' view**: Depends on the viewer's client fetching from a skin server

Without patching the client or requiring all players to use the same skin system, we're limited to:
1. Showing the user their current skin in the launcher UI
2. Helping users set their skin on SkinRestorer servers
3. Auto-configuring CustomSkinLoader for Fabric instances

---

## Scope Tiers

### Tier 1: Skin Preview in Launcher (no dependencies)
- Display the user's skin in the launcher UI
- For online mode (Plan 4): fetch from Mojang profile
- For offline mode: show Steve/Alex based on UUID, or a user-uploaded custom skin

### Tier 2: SkinRestorer Command Helper (no dependencies)
- Help users set their skin on SkinRestorer-enabled servers
- Provide a "Copy Skin Command" button that copies `/skin set <username>` to clipboard

### Tier 3: CustomSkinLoader Auto-Config (depends on Plan 2 + Plan 3 infrastructure)
- For Fabric instances: auto-install CustomSkinLoader mod
- Configure it to use a custom skin server or resolve from the user's preferred username

---

## Tier 1: Skin Preview

### New File: `internal/skin/skin.go`

```go
package skin

// SkinData represents a player's skin information.
type SkinData struct {
    TextureURL string `json:"textureUrl"` // Full skin texture URL
    HeadURL    string `json:"headUrl"`    // Pre-cropped head image URL
    Variant    string `json:"variant"`    // "classic" or "slim" (Alex)
    Source     string `json:"source"`     // "mojang", "custom", "default"
}

// GetSkinForOnlineUser fetches skin data from Mojang's session server.
// GET https://sessionserver.mojang.com/session/minecraft/profile/<uuid>
// Decodes the base64 textures property to get the skin URL.
func GetSkinForOnlineUser(uuid string) (*SkinData, error)

// GetDefaultSkin returns Steve or Alex based on UUID hash.
// Alex is used when (uuid hashCode % 2) == 1.
func GetDefaultSkin(uuid string) *SkinData

// HeadFromSkinURL fetches a skin texture and extracts the 8x8 face.
// Returns PNG bytes of the head at the requested size (e.g., 64x64).
func HeadFromSkinURL(skinURL string, size int) ([]byte, error)
```

### API Endpoints

```
GET /api/v1/skin/head?uuid=<uuid>&size=64    → Returns head PNG
GET /api/v1/skin/profile?uuid=<uuid>          → Returns SkinData JSON
```

For offline users: `uuid` is the offline UUID. `GetSkinForOnlineUser` will return nothing, so fall back to `GetDefaultSkin`.

For online users (Plan 4 present): use the real UUID from the Minecraft profile.

### Frontend Changes

**File: `frontend/static/app.js`**

- **Topbar**: Replace the plain username text with a skin head image + username
  - Fetch from `/api/v1/skin/head?uuid=<uuid>&size=48`
  - Update on username change (offline) or login (online)

- **Instance detail**: Show the skin head next to the instance name

- **Player head rendering**:
  - Use an `<img>` tag pointing to the head endpoint
  - Add a subtle border/shadow matching the current theme
  - Cache aggressively (skin changes are infrequent)

---

## Tier 2: SkinRestorer Helper

### Frontend-Only Feature

No backend changes needed. Pure UX convenience.

**In Instance Detail or Settings**:
- "Server Skin" section
- Input: "Skin username" — the Mojang account whose skin to use
- Button: "Copy Skin Command" → copies `/skin set <username>` to clipboard
- Tooltip: "Run this command on any server with SkinRestorer to set your skin"

**Optional enhancement**:
- Also offer `/skin url <url>` format for custom skin URLs
- Link to NameMC for skin browsing

---

## Tier 3: CustomSkinLoader Auto-Config

### Depends On
- Plan 2 (Instance Isolation) — need per-instance mods folder
- Plan 3 infrastructure (Modrinth client) — for downloading the mod

### Implementation

**File: `internal/performance/bundle.go`** (or new file)

Add CustomSkinLoader as an optional mod in the performance bundle, or as a standalone mod install:

```go
var SkinMod = Mod{
    Slug:        "customskinloader",
    Name:        "CustomSkinLoader",
    Description: "Load skins from custom servers",
    Required:    false,
    MinVersion:  "1.14",
}
```

### CustomSkinLoader Configuration

After installing the mod, write its config file to the instance:

**File**: `<instance>/minecraft/CustomSkinLoader/CustomSkinLoader.json`

```json
{
    "version": "14.16",
    "loadlist": [
        {
            "name": "Mojang",
            "type": "MojangAPI"
        },
        {
            "name": "Crafatar",
            "type": "CustomSkinAPI",
            "root": "https://crafatar.com/"
        }
    ]
}
```

This tells CustomSkinLoader to first try Mojang's API, then fall back to Crafatar (which resolves skins by username).

### Frontend

In Instance Detail → Skin section:
- Toggle: "Enable custom skin loading" → installs/removes CustomSkinLoader mod
- If enabled: show the skin resolution chain
- Advanced: let user add custom skin server URLs to the loadlist

---

## Implementation Strategy

| Tier | Depends On | Effort | Value |
|------|-----------|--------|-------|
| Tier 1: Preview | Nothing | Low | Medium (visual polish) |
| Tier 2: Helper | Nothing | Trivial | Low (UX convenience) |
| Tier 3: Auto-Config | Plans 2+3 | Medium | Medium (only helps Fabric users on CustomSkinLoader-aware servers) |

**Recommended**: Implement Tier 1 + Tier 2 as part of any plan (they're small). Defer Tier 3 until Plans 2 and 3 are complete.

---

## Files Changed (Summary)

| File | Change |
|------|--------|
| `internal/skin/skin.go` | **New** — Skin fetching, head extraction |
| `internal/server/api.go` | Skin endpoints (head, profile) |
| `frontend/static/app.js` | Skin head in topbar, SkinRestorer helper |

---

## Notes

- **Image processing**: Go's `image/png` and `image` packages can handle skin texture cropping natively. No external dependencies needed.
- **Crafatar fallback**: https://crafatar.com/ provides rendered heads/avatars from usernames. Could use as a simpler alternative to parsing skin textures ourselves, but adds external dependency.
- **Caching**: Cache skin textures for 1 hour. Skins rarely change.
- **Offline skin upload**: Future enhancement — let users upload a PNG and set it as their local skin. Only visible to themselves in singleplayer (vanilla limitation). Would require resource pack injection.
