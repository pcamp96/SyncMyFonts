# macOS LAN Peer App MVP

Scope: macOS-specific guidance for turning the current CLI agent into a local
LAN sync peer app or background agent. This document does not change server API
contracts and should stay compatible with the Windows peer plan.

## MVP Shape

The MVP macOS peer should be a user-scoped helper that can:

- Discover a SyncMyFonts server or peer endpoint on the local LAN.
- Scan current-user fonts only.
- Push local user fonts to the configured sync endpoint.
- Pull missing library fonts and install them in the managed user font folder.
- Run on demand from a small app, menu-bar app, shortcut, or LaunchAgent.

Do not require administrator privileges for the MVP. The app should avoid
system font folders, privileged launch daemons, kernel extensions, login item
tricks, or automatic font cache resets.

## Bonjour and mDNS Expectations

For LAN-only discovery, advertise and browse a Bonjour service rather than
asking users to type IP addresses. Recommended service:

```text
_syncmyfonts._tcp.local.
```

Recommended TXT keys:

- `api=v1`
- `name=<human-readable server or peer name>`
- `auth=required|none`
- `tls=none|self-signed|trusted`
- `path=/api/v1`

MVP discovery behavior:

- Browse for `_syncmyfonts._tcp.local.` on app launch and before manual sync.
- Show only IPv4/IPv6 addresses that resolve on active local interfaces.
- Prefer previously trusted endpoints over newly discovered endpoints.
- Require explicit user confirmation before syncing to a newly discovered peer.
- Cache the confirmed endpoint in
  `~/Library/Application Support/SyncMyFonts/config.json`.
- Fall back to a manually entered `http://host:7368` URL when Bonjour is
  unavailable.

MVP advertising behavior depends on app architecture:

- If the macOS machine only runs a client, it should browse but not advertise.
- If it can host a local sync server, the server process should advertise the
  service while listening.
- If it exposes a peer-to-peer endpoint later, advertise only while that endpoint
  is reachable and authenticated.

Security expectation: Bonjour is discovery only. It is not authorization. Every
sync request still needs the configured API key, pairing token, or future auth
scheme.

## Local Network Permission

macOS prompts for Local Network access when a sandboxed or bundled app browses,
advertises, or connects to local network services. The MVP app should treat this
as a first-run setup requirement.

Add these keys to the bundled app's `Info.plist` when using Bonjour:

```xml
<key>NSLocalNetworkUsageDescription</key>
<string>SyncMyFonts uses your local network to discover and sync with your font server or trusted devices.</string>
<key>NSBonjourServices</key>
<array>
  <string>_syncmyfonts._tcp</string>
</array>
```

Expected UX:

- Prompt is triggered by a visible user action such as "Find Local Server" or
  "Sync Now".
- If permission is denied, keep manual server URL entry available.
- Surface a clear diagnostic: `local-network-permission-denied`.
- Tell users to re-enable access in System Settings > Privacy & Security >
  Local Network.

CLI note: a Terminal-run CLI may not behave exactly like a bundled app. The
packaged app should be tested separately because Local Network permission is
granted to the app bundle, not to an abstract command.

## Per-User Managed Font Install

The macOS peer must keep the current per-user install rule:

```text
~/Library/Fonts/SyncMyFonts
```

Rules:

- Scan `~/Library/Fonts` recursively for user-installed fonts.
- Exclude `~/Library/Fonts/SyncMyFonts` from normal push inventory unless the
  command explicitly includes managed fonts.
- Never write to `/System/Library/Fonts`, `/Library/Fonts`, or
  `/Network/Library/Fonts`.
- Install synced fonts by staging under
  `~/Library/Application Support/SyncMyFonts/tmp`, verifying SHA-256, then
  atomically renaming into the managed font folder.
- Keep a manifest at
  `~/Library/Application Support/SyncMyFonts/manifest.json` for fonts installed
  by SyncMyFonts.
- Refuse to overwrite a changed managed font unless a future explicit repair or
  force command is added.

After install, print or display:

```text
Installed. Some apps may need to be restarted before the font appears.
```

Do not automatically run `atsutil`, kill font services, request sudo, or mutate
system caches.

## App and Agent Options

The MVP can expose the same core agent through several macOS entry points.

### Command Shortcuts

Useful wrapper commands:

```bash
syncmyfonts scan --json
syncmyfonts push --server http://font-server.local:7368
syncmyfonts sync --server http://font-server.local:7368 --dry-run
syncmyfonts sync --server http://font-server.local:7368
```

Recommended app shortcuts:

- `Scan Local Fonts`
- `Find Local Server`
- `Sync Now`
- `Dry Run Sync`
- `Open Managed Font Folder`
- `Open Logs`

The app should call the same Rust code paths as the CLI instead of maintaining a
separate sync implementation.

### LaunchAgent

A user LaunchAgent is acceptable for scheduled sync because it runs in the user
session and does not need admin rights.

Example plist shape:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.syncmyfonts.agent</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Applications/SyncMyFonts.app/Contents/MacOS/syncmyfonts</string>
    <string>sync</string>
    <string>--server</string>
    <string>http://font-server.local:7368</string>
  </array>
  <key>StartInterval</key>
  <integer>3600</integer>
  <key>RunAtLoad</key>
  <true/>
  <key>StandardOutPath</key>
  <string>~/Library/Logs/SyncMyFonts/agent.log</string>
  <key>StandardErrorPath</key>
  <string>~/Library/Logs/SyncMyFonts/agent.err.log</string>
</dict>
</plist>
```

Implementation notes:

- Install the plist under `~/Library/LaunchAgents`.
- Use `launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.syncmyfonts.agent.plist`.
- Store secrets outside the plist. Prefer Keychain or a config file with
  user-only permissions.
- Avoid `KeepAlive` for MVP unless a long-running peer listener is intentionally
  added.
- Prefer `StartInterval` plus manual `Sync Now` for predictable behavior.

### Menu-Bar or Full App

A menu-bar MVP is enough if it provides:

- Current endpoint and discovery status.
- Last sync time.
- Last sync result and warning count.
- Buttons for `Sync Now`, `Dry Run`, and `Open Managed Font Folder`.
- A settings view for server URL, API key, and whether scheduled sync is enabled.

The full app can add font inventory and conflict review later, but the MVP
should prioritize a reliable sync path over a large UI.

## Proposed Rust Changes

Do not edit these yet unless the macOS app work starts. Recommended future
changes:

- Add `discover` command that browses `_syncmyfonts._tcp.local.` and returns
  machine-readable JSON.
- Add `serve-lan` or server-side advertisement support when a local server is
  running on macOS.
- Add stable macOS error codes:
  `local-network-permission-denied`, `bonjour-discovery-failed`,
  `endpoint-not-trusted`, `managed-manifest-missing`,
  `managed-font-modified`.
- Move scan/install logic behind reusable library functions so the CLI, app,
  and LaunchAgent call the same code.
- Add Keychain-backed API key lookup for bundled app usage, while keeping
  `SYNCMYFONTS_API_KEY` for CLI and CI.
- Add a config file reader for
  `~/Library/Application Support/SyncMyFonts/config.json`.
- Emit structured log events to `~/Library/Logs/SyncMyFonts`.

Potential crates or platform APIs:

- `mdns-sd` or `bonjour-service` for Bonjour/mDNS.
- `security-framework` for Keychain.
- `directories` for user-scoped Application Support and font paths.
- `tracing` plus a file subscriber for app diagnostics.

## Test Checklist

Local macOS tests:

- Fresh user with no `~/Library/Fonts` returns an empty scan without error.
- User-installed `.ttf`, `.otf`, `.ttc`, and `.otc` fonts are detected.
- System fonts in `/System/Library/Fonts` and `/Library/Fonts` are not synced.
- Managed fonts in `~/Library/Fonts/SyncMyFonts` are excluded from push by
  default and included only when explicitly requested.
- Sync installs into `~/Library/Fonts/SyncMyFonts` without sudo.
- Hash mismatch fails before writing the final font.
- Existing identical managed font is a no-op.
- Existing same-name different-content font produces a conflict or deterministic
  suffixed destination according to the active client contract.
- A modified managed font is detected from the manifest and is not overwritten.
- Apps launched before install may require restart; no font cache reset runs.

LAN and app tests:

- Bonjour discovery finds a LAN server advertising `_syncmyfonts._tcp.local.`.
- Discovery works across IPv4 and IPv6 local addresses.
- Denied Local Network permission produces a clear diagnostic and manual URL
  fallback still works.
- A newly discovered endpoint requires explicit confirmation before sync.
- Cached trusted endpoint survives app relaunch.
- API key is not written into a LaunchAgent plist or log file.
- LaunchAgent runs as the current user and syncs on `RunAtLoad`.
- Scheduled sync writes stdout/stderr to `~/Library/Logs/SyncMyFonts`.
- Offline server produces a visible failure without deleting local fonts.
- Windows and macOS syncing the same font bytes deduplicate by SHA-256.
