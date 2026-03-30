# Plan 4: MSA Authentication (Online Mode)

**Priority**: Medium-Low — this is the feature that makes it a "real launcher" but has the highest maintenance burden and legal ambiguity.

**Goal**: Allow users to sign in with a Microsoft account to play on online-mode servers with a verified identity, proper UUID, and skin.

---

## Background

The current launcher sets `auth_access_token` to `"null"` and generates an offline UUID. This works for singleplayer and `online-mode=false` servers but blocks:
- Online-mode server multiplayer
- Realms
- Correct skin display
- Marketplace (not relevant for this launcher)

### The Authentication Chain

```
Microsoft Account (OAuth2 Device Code Flow)
    → Xbox Live Token
        → XSTS Token
            → Minecraft Access Token
                → Minecraft Profile (UUID, username, skin)
```

Each step is a separate HTTP request with its own token format and expiry.

---

## Design Decisions

### Device Code Flow (not browser redirect)
- Device code flow: show user a code + URL, they authenticate in their own browser
- No need to embed a browser or handle redirect URIs
- Works on headless systems, avoids WebView2 auth cookie issues
- Microsoft recommends this for "input-constrained" devices — a CLI/lightweight launcher qualifies

### Token Storage
- Store tokens encrypted on disk using OS credential store
- Windows: Windows Credential Manager (via `wincred`)
- Linux: `libsecret` / `gnome-keyring` / plaintext fallback with warning
- macOS: Keychain (via `security` CLI)
- Fallback: AES-256 encrypted file with key derived from machine ID

### Refresh Strategy
- MC access token expires in ~24 hours
- Xbox Live token expires in ~14 days
- MSA refresh token lasts ~90 days
- On launch: check token expiry, refresh silently if needed
- If refresh fails: prompt re-login

---

## Phase 1: Azure App Registration

### Prerequisites (Manual, One-Time)
1. Register an application in Azure AD (portal.azure.com)
2. Set "Supported account types" to "Personal Microsoft accounts only"
3. Enable "Allow public client flows" (for device code flow)
4. Add API permission: `XboxLive.signin` (delegated)
5. Note the **Client ID** — this is the only secret (it's public, not confidential)
6. No client secret needed for public client flow

The Client ID will be embedded in the binary. This is standard practice (Prism Launcher, MultiMC do the same).

---

## Phase 2: Authentication Flow Implementation

### New File: `internal/auth/msa.go`

```go
package auth

// DeviceCodeResponse is returned by the initial device code request.
type DeviceCodeResponse struct {
    DeviceCode      string `json:"device_code"`
    UserCode        string `json:"user_code"`
    VerificationURI string `json:"verification_uri"`
    ExpiresIn       int    `json:"expires_in"`
    Interval        int    `json:"interval"`
    Message         string `json:"message"`
}

// MSATokens holds the OAuth2 tokens from Microsoft.
type MSATokens struct {
    AccessToken  string `json:"access_token"`
    RefreshToken string `json:"refresh_token"`
    ExpiresAt    int64  `json:"expires_at"`
}

// RequestDeviceCode initiates the device code flow.
// POST https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode
// Body: client_id=<ID>&scope=XboxLive.signin%20offline_access
func RequestDeviceCode(clientID string) (*DeviceCodeResponse, error)

// PollForToken polls Microsoft until the user completes authentication.
// POST https://login.microsoftonline.com/consumers/oauth2/v2.0/token
// Polls every `interval` seconds until success, expiry, or denial.
func PollForToken(clientID, deviceCode string, interval int) (*MSATokens, error)

// RefreshMSAToken refreshes an expired MSA access token.
func RefreshMSAToken(clientID, refreshToken string) (*MSATokens, error)
```

### New File: `internal/auth/xbox.go`

```go
package auth

// XboxLiveToken represents an Xbox Live authentication token.
type XboxLiveToken struct {
    Token    string `json:"Token"`
    Uhs      string // User Hash from DisplayClaims
    ExpireAt int64
}

// XSTSToken represents an XSTS authorization token.
type XSTSToken struct {
    Token string `json:"Token"`
    Uhs   string
}

// AuthenticateXboxLive exchanges MSA token for Xbox Live token.
// POST https://user.auth.xboxlive.com/user/authenticate
func AuthenticateXboxLive(msaAccessToken string) (*XboxLiveToken, error)

// AuthenticateXSTS exchanges Xbox Live token for XSTS token.
// POST https://xsts.auth.xboxlive.com/xsts/authorize
func AuthenticateXSTS(xblToken string) (*XSTSToken, error)
```

### New File: `internal/auth/minecraft.go`

```go
package auth

// MinecraftToken represents a Minecraft access token.
type MinecraftToken struct {
    AccessToken string `json:"access_token"`
    ExpiresIn   int    `json:"expires_in"`
    ExpiresAt   int64
}

// MinecraftProfile represents the player's Minecraft profile.
type MinecraftProfile struct {
    ID   string `json:"id"`   // Real UUID (no dashes)
    Name string `json:"name"` // Gamertag
    Skins []Skin `json:"skins"`
    Capes []Cape `json:"capes"`
}

type Skin struct {
    ID      string `json:"id"`
    State   string `json:"state"`
    URL     string `json:"url"`
    Variant string `json:"variant"` // "CLASSIC" or "SLIM"
}

type Cape struct {
    ID    string `json:"id"`
    State string `json:"state"`
    URL   string `json:"url"`
}

// AuthenticateMinecraft exchanges XSTS token for Minecraft access token.
// POST https://api.minecraftservices.com/authentication/login_with_xbox
func AuthenticateMinecraft(xstsToken, uhs string) (*MinecraftToken, error)

// GetMinecraftProfile fetches the player's Minecraft profile.
// GET https://api.minecraftservices.com/minecraft/profile
// Header: Authorization: Bearer <mcAccessToken>
func GetMinecraftProfile(mcAccessToken string) (*MinecraftProfile, error)

// CheckGameOwnership verifies the account owns Minecraft.
// GET https://api.minecraftservices.com/entitlements/mcstore
func CheckGameOwnership(mcAccessToken string) (bool, error)
```

---

## Phase 3: Token Storage

### New File: `internal/auth/store.go`

```go
package auth

// AuthState holds all tokens for a logged-in user.
type AuthState struct {
    MSATokens     MSATokens        `json:"msa"`
    MCToken       MinecraftToken   `json:"mc"`
    Profile       MinecraftProfile `json:"profile"`
    LastRefreshed int64            `json:"lastRefreshed"`
}

// Store manages persistent token storage.
type Store interface {
    Save(state *AuthState) error
    Load() (*AuthState, error)
    Clear() error
}

// NewStore returns the best available store for the platform.
func NewStore(configDir string) Store
```

### Platform Implementations

**File: `internal/auth/store_windows.go`**
- Use Windows Credential Manager via `golang.org/x/sys/windows` or `github.com/danieljoos/wincred`
- Store as a generic credential with target name "croopor/minecraft-auth"
- Value: JSON-serialized `AuthState`

**File: `internal/auth/store_linux.go`**
- Try `libsecret` via D-Bus (gnome-keyring, KDE Wallet)
- Fallback: AES-256 encrypted file at `<configDir>/auth.enc`
- Key derived from: `PBKDF2(machine-id + username, salt, 100000, 32)`

**File: `internal/auth/store_darwin.go`**
- Use Keychain via `security` CLI tool
- `security add-generic-password -a croopor -s minecraft-auth -w <json>`

---

## Phase 4: Auth Orchestrator

### New File: `internal/auth/auth.go`

```go
package auth

// Authenticator manages the full authentication lifecycle.
type Authenticator struct {
    clientID string
    store    Store
}

// Status returns the current auth state.
type AuthStatus struct {
    LoggedIn    bool              `json:"loggedIn"`
    Username    string            `json:"username,omitempty"`
    UUID        string            `json:"uuid,omitempty"`
    SkinURL     string            `json:"skinUrl,omitempty"`
    TokenValid  bool              `json:"tokenValid"`
    ExpiresAt   int64             `json:"expiresAt,omitempty"`
}

func NewAuthenticator(clientID, configDir string) *Authenticator

// StartLogin initiates the device code flow. Returns the user code and URL.
// The caller should display these to the user and then call WaitForLogin.
func (a *Authenticator) StartLogin() (*DeviceCodeResponse, error)

// WaitForLogin polls until the user completes authentication.
// Performs the full chain: MSA → Xbox → XSTS → MC → Profile.
// Stores result in the credential store.
func (a *Authenticator) WaitForLogin(deviceCode string, interval int) (*AuthState, error)

// EnsureValid checks if the current token is valid. Refreshes if expired.
// Returns the current auth state or error if re-login is needed.
func (a *Authenticator) EnsureValid() (*AuthState, error)

// Logout clears all stored tokens.
func (a *Authenticator) Logout() error

// Status returns the current auth state without refreshing.
func (a *Authenticator) Status() *AuthStatus

// GetLaunchAuth returns the auth variables for BuildAndLaunch.
// If logged in: real token, UUID, username from profile.
// If not logged in: offline mode values (current behavior).
func (a *Authenticator) GetLaunchAuth() (accessToken, uuid, username, xuid, userType string)
```

---

## Phase 5: Integration with Launch System

### File: `internal/launcher/builder.go`

**Change `BuildAndLaunch()`**:

Currently at line ~105:
```go
AuthAccessToken: "null",
AuthUUID:        minecraft.OfflineUUID(opts.Username),
AuthXUID:        "",
UserType:        "msa",
```

Change to:
```go
// Get auth from authenticator
accessToken, uuid, username, xuid, userType := authenticator.GetLaunchAuth()
// If online mode, use real values; if offline, falls back to current behavior
```

### File: `internal/minecraft/arguments.go`

No structural changes needed — the `LaunchVars` struct already has all the right fields (`AuthAccessToken`, `AuthUUID`, `AuthXUID`, `UserType`). Just pass different values.

---

## Phase 6: API Endpoints

### File: `internal/server/api.go`

```
GET    /api/v1/auth/status          → Current auth state (logged in, username, skin URL)
POST   /api/v1/auth/login           → Start device code flow (returns code + URL)
GET    /api/v1/auth/login/poll      → SSE stream: polls until auth completes or times out
POST   /api/v1/auth/logout          → Clear tokens
POST   /api/v1/auth/refresh         → Force token refresh
GET    /api/v1/auth/profile         → Get full profile (UUID, skins, capes)
```

---

## Phase 7: Frontend

### File: `frontend/static/app.js`

**Account Section (topbar or settings)**:
1. **Logged out state**:
   - "Sign in with Microsoft" button
   - Click → calls `POST /api/v1/auth/login`
   - Modal shows: "Go to microsoft.com/link and enter code: ABCD-EFGH"
   - Auto-copies code to clipboard
   - Subscribes to SSE poll endpoint
   - On success: modal closes, topbar updates with username + skin head

2. **Logged in state**:
   - Topbar shows: player head (fetched from skin URL) + username
   - Username field becomes read-only (comes from Microsoft account)
   - Dropdown: "Logout", "Switch to Offline Mode"

3. **Offline mode toggle**:
   - Settings checkbox: "Play in Offline Mode"
   - When checked: uses current offline behavior (custom username, null token)
   - When unchecked + logged in: uses real auth tokens
   - This allows users to stay logged in but choose when to use online mode

4. **Token expiry warning**:
   - If token refresh fails silently, show a banner: "Session expired — sign in again to play online"
   - Non-blocking: user can still play offline

### Skin Head Rendering
- Fetch skin texture from profile's skin URL
- Extract the 8x8 head portion (pixels 8,8 to 16,16 in the skin texture)
- Render at 32x32 or 48x48 in the topbar
- Use canvas or pre-render server-side
- Fallback: default Steve/Alex head based on UUID

---

## Phase 8: Game Ownership Check

### Important
Before allowing online-mode launch, verify the account owns Minecraft:
1. Call `CheckGameOwnership(mcAccessToken)`
2. If not owned: show clear message "This Microsoft account does not own Minecraft Java Edition"
3. Allow offline mode but not online mode

This prevents confusion when users try to use a non-owning Microsoft account.

---

## Security Considerations

1. **Token storage**: Never log tokens. Encrypt at rest. Clear on logout.
2. **Client ID exposure**: The Azure client ID is public. This is by design for public client flows. It cannot be used to impersonate users without their consent.
3. **Token in memory**: Tokens exist in process memory during launch. This is unavoidable and standard.
4. **HTTPS only**: All auth endpoints are HTTPS. The launcher's local HTTP server (localhost) never receives or transmits auth tokens externally.
5. **Refresh token rotation**: Microsoft may rotate refresh tokens. Always store the latest one from any refresh response.

---

## Files Changed (Summary)

| File | Change |
|------|--------|
| `internal/auth/msa.go` | **New** — MSA OAuth2 device code flow |
| `internal/auth/xbox.go` | **New** — Xbox Live + XSTS token exchange |
| `internal/auth/minecraft.go` | **New** — MC token + profile + ownership |
| `internal/auth/auth.go` | **New** — Orchestrator (login, refresh, lifecycle) |
| `internal/auth/store.go` | **New** — Store interface |
| `internal/auth/store_windows.go` | **New** — Windows Credential Manager |
| `internal/auth/store_linux.go` | **New** — libsecret / encrypted file fallback |
| `internal/auth/store_darwin.go` | **New** — macOS Keychain |
| `internal/launcher/builder.go` | Use real auth when available |
| `internal/server/api.go` | Auth endpoints |
| `internal/server/server.go` | Add Authenticator to Server |
| `frontend/static/app.js` | Login UI, skin head, offline toggle |
| `go.mod` | Add `golang.org/x/sys`, possibly `wincred` |

---

## Risks & Mitigations

1. **Microsoft changes the auth flow**: The device code flow is an OAuth2 standard. Unlikely to break without notice. Community (wiki.vg) tracks changes.
2. **Azure app gets rate-limited or banned**: Very unlikely for a small user base. If it happens, users can register their own Azure app and configure the client ID.
3. **Legal**: Third-party launchers authenticating with Microsoft accounts operate in a gray area. Mojang has not actively blocked them (Prism, MultiMC, ATLauncher all do this). However, this could change.
4. **Account security**: The device code flow is secure — the launcher never sees the user's password. Tokens are scoped to `XboxLive.signin` only.
5. **Token refresh race**: If two instances launch simultaneously and both try to refresh, one may invalidate the other's token. Solution: mutex around refresh operations.

---

## Azure App Registration Requirements

The Client ID must be registered before development begins. Steps:
1. Go to https://portal.azure.com → Azure Active Directory → App registrations
2. New registration: name "Croopor Minecraft Launcher", personal accounts only
3. Authentication → Allow public client flows → Yes
4. API Permissions → Add → Microsoft Graph → XboxLive.signin
5. Copy Application (client) ID
6. Store as a const in `internal/auth/msa.go`
