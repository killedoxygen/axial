# Discord RPC Setup

Axial desktop builds can publish Discord Rich Presence through Discord's local IPC socket or named pipe. This is compile-time configured; a binary built without `AXIAL_DISCORD_APPLICATION_ID` does not start the Discord RPC worker.

## Developer Portal
1. Open `https://discord.com/developers/applications`.
2. Create or select the Axial application.
3. Copy the application id and set it as `AXIAL_DISCORD_APPLICATION_ID`.
4. Open Rich Presence art assets for that application.
5. Upload these asset keys:
   - `axial` for the large image.
   - `axial_idle` for launcher idle.
   - `axial_launching` for Minecraft startup.
   - `axial_minecraft` for active Minecraft.
   - `axial_multi` for multiple active sessions.

## Runtime Contract
- No bot token, client secret, OAuth redirect, scopes, or RPC origin are required.
- Discord desktop must be running for the local IPC connection to succeed.
- Connection failures are quiet diagnostics; the launcher UI does not show Discord connection errors.
- Presence can be disabled by the user through `discord_rpc_enabled`.
- The worker clears activity on explicit disable and during normal desktop shutdown.

## Privacy Boundary
Presence text is authored by `apps/api/src/state/presence.rs` and then mapped to Discord activity fields by `apps/desktop/src/discord_presence/activity.rs`.

The payload must not include instance names, account names, usernames, world names, server addresses, filesystem paths, command lines, Java paths, JVM args, mod lists, tokens, UUIDs, join secrets, buttons, or URLs.
