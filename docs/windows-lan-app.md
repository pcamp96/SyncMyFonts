# Windows LAN App MVP Guidance

Scope: Windows-specific local LAN sync app and agent guidance only. This document
does not define the macOS app, server storage internals, or source changes. It
translates the current Rust CLI behavior into an MVP that a normal Windows user
can run on the same local network as a macOS peer.

## MVP Shape

The Windows MVP should be a small peer app or tray agent wrapped around the
existing agent commands:

- `scan`: inventory current-user fonts.
- `push`: upload current-user fonts to the local sync server.
- `sync`: download missing fonts from the local sync server.

The app should assume a trusted home, studio, or workshop LAN. It should not
require admin rights for normal use, should not install system-wide fonts, and
should never modify `C:\Windows\Fonts` or `HKLM` font registry entries.

## Non-Goals

- Internet-facing discovery or sync.
- Enterprise deployment, domain policy integration, or MSI packaging.
- System-wide font installation.
- Automatic deletion of fonts.
- Conflict resolution UI beyond clear skip/report behavior.
- Editing the Rust source as part of this guidance pass.

## User-Facing Install Model

Windows synced fonts install for the signed-in user only:

```text
%LOCALAPPDATA%\Microsoft\Windows\Fonts
```

Registration should use the current-user registry font table:

```text
HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts
```

This is the correct MVP path because it avoids UAC prompts and keeps the app
inside the user's own profile. The app should explain this in plain language:

```text
SyncMyFonts installs fonts only for your Windows account. Other Windows users on
this PC will not see them unless they run SyncMyFonts too.
```

Some apps cache font lists. After a successful sync, the app should show:

```text
Fonts are installed. If an app does not show them yet, close and reopen that app.
```

## LAN Server Connection

For MVP, the Windows app should ask for:

- Server address, for example `http://192.168.0.50:7368`.
- Optional API key.
- Device name, defaulting to the Windows computer name.

Recommended first-run flow:

1. Show a single connection screen.
2. Let the user enter or paste the Mac/server LAN address.
3. Test `GET /healthz`.
4. If health passes, save the server URL and optional API key in the user's
   profile.
5. Run a dry sync preview.
6. Offer a clear `Sync Now` action.

Avoid making non-technical users choose push versus sync first. The MVP app can
label the actions as:

- `Send My Fonts`
- `Get Missing Fonts From Peer`
- `Sync Both Ways`

Internally, `Sync Both Ways` can run `push` followed by `sync`.

## Firewall Prompt Guidance

The Windows peer app should not need an inbound firewall exception when it only
acts as a client that connects to a Mac or local server. In that mode, Windows
Defender Firewall should not show a scary inbound prompt.

When the Windows app hosts LAN sharing or answers peer discovery, it may trigger
a Windows Security Alert. The app and docs should tell the user:

```text
Allow SyncMyFonts on Private networks only. Do not allow Public networks.
```

Expected prompt handling:

- Private networks: allowed only when this PC is intentionally hosting or
  advertising SyncMyFonts on the local LAN.
- Public networks: leave unchecked.
- Domain networks: leave unchecked unless the user is in a managed office
  environment and knows they need it.

Recommended app copy near hosted/discovery mode:

```text
Only enable LAN sharing on a trusted home or studio network. If Windows asks,
choose Private networks.
```

The Readiness Check also inspects Windows network profile categories. A Public
profile should be reported as a hosted-mode warning because other computers on
the LAN may not be able to reach the sharing PC.

The app should also show a simple warning when Windows reports the active
network profile as Public:

```text
This network is marked Public in Windows, so SyncMyFonts will not accept local
connections here.
```

## How A Non-Technical User Runs It

### First Computer

1. Install or start the SyncMyFonts server on the Mac or always-on computer.
2. Note the LAN address, such as `http://192.168.0.50:7368`.
3. Open SyncMyFonts on Windows.
4. Paste the server address.
5. Click `Test Connection`.
6. Click `Sync Both Ways`.
7. Reopen design apps if new fonts do not appear.

### Returning Use

The main window should have one obvious button:

```text
Sync Now
```

It should show the last result in normal language:

```text
Synced 12 fonts. 3 were already installed. 1 was skipped because Windows already
has a system font with that name.
```

The app should include a small diagnostics/details view for copyable support
output:

- Server URL.
- Windows device name.
- Last sync time.
- Counts for scanned, uploaded, installed, skipped, and warnings.
- Current user font directory path.
- App version and agent version.

## Command Shortcuts

Until there is a full Windows GUI, provide Start Menu shortcuts that call the
agent with saved settings.

Suggested shortcuts:

- `SyncMyFonts - Sync Now`
- `SyncMyFonts - Send My Fonts`
- `SyncMyFonts - Get Missing Fonts From Peer`
- `SyncMyFonts - Diagnostics`

The shortcuts should run without opening a long-lived terminal window. If the
MVP is CLI-only, use a small `.cmd` or PowerShell wrapper that pauses only on
failure so the user can read errors.

Suggested wrapper behavior:

```text
1. Load server URL and API key from the user config.
2. Run the requested agent command.
3. Write a log file under %LOCALAPPDATA%\SyncMyFonts\logs.
4. Show success or failure in a toast, tray menu, or short console message.
```

Do not put API keys directly into Start Menu shortcut arguments if avoidable.
Prefer a per-user config file with user-only permissions.

## Service And Startup Options

For the MVP, prefer a per-user startup app or tray agent over a Windows service.

Recommended order:

1. Manual app: lowest risk, easiest to explain.
2. Per-user startup tray agent: good for "sync when I sign in" without admin.
3. Scheduled Task at user logon: useful if there is no tray UI yet.
4. Windows service: defer until system-wide background sync is required.

### Per-User Startup

Use the user's Startup folder or `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
This should launch the tray/app process, not a system service.

Startup behavior:

- Wait until network is available.
- Test server health.
- Run `sync` or `push` plus `sync` depending on the user's selected mode.
- Back off quietly if the server is offline.
- Show a notification only when fonts changed or an error needs attention.

### Scheduled Task

A Scheduled Task can run at user logon without admin rights when registered for
the current user.

Suggested triggers:

- At logon.
- Optional repeat every 4 hours while signed in.

Suggested settings:

- Run only when user is logged on.
- Stop if running longer than 15 minutes.
- Do not start on battery by default only if the user opts into that behavior.

### Windows Service

Do not make a Windows service the default MVP path. A service introduces admin
installation, account isolation, access to the wrong user's font directory, and
more complex secrets storage. If added later, it should coordinate per-user
agents instead of installing fonts directly from a machine account.

## Suggested User Config

Store config per user:

```text
%LOCALAPPDATA%\SyncMyFonts\config.json
```

Suggested fields:

```json
{
  "server_url": "http://192.168.0.50:7368",
  "api_key_source": "user-config",
  "device_name": "SHOP-PC",
  "sync_on_startup": true,
  "sync_mode": "push-then-sync",
  "last_successful_sync": null
}
```

If an API key is stored locally, prefer Windows Credential Manager. If the MVP
uses a config file first, restrict it to the current user and make the settings
screen able to clear it.

## Logging

Write per-user logs here:

```text
%LOCALAPPDATA%\SyncMyFonts\logs
```

The normal app should show concise outcomes, not raw stack traces. Diagnostics
can include raw agent JSON for support.

Minimum useful log events:

- App start and version.
- Server URL tested, without printing the API key.
- Command run: scan, push, sync, dry-run.
- Counts from command output.
- Font install path.
- Registry write failure.
- Unsupported font type.
- Firewall/discovery mode state, if hosting or discovery is added.

## Failure Messages

Use plain, actionable text:

- Server offline:
  `SyncMyFonts could not reach the server at http://192.168.0.50:7368. Make sure both computers are on the same Wi-Fi or Ethernet network.`
- Public network profile:
  `Windows says this is a Public network. Switch it to Private before allowing LAN sharing.`
- Font app cache:
  `The font was installed, but your design app may need to be reopened.`
- Permission problem:
  `SyncMyFonts could not write to your Windows user font folder. Try signing out and back in, then run Sync Now again.`
- Registry problem:
  `The font file was copied, but Windows did not register it. Run Diagnostics and send the log.`

## Test Checklist

### Connection And Firewall

- Windows app can connect to a server by LAN IP and port.
- Windows app can connect by local DNS/hostname when available.
- Bad server URL shows a clear message.
- Server offline shows a clear message and does not clear saved settings.
- Client-only mode does not request an inbound firewall exception.
- Hosted/discovery mode, if added, asks for Private network access only.
- Public network profile blocks hosted/discovery mode or clearly warns the user.

### Per-User Font Install

- `sync` installs fonts into `%LOCALAPPDATA%\Microsoft\Windows\Fonts`.
- Install writes only `HKCU` font registry entries.
- Install never writes to `C:\Windows\Fonts`.
- Install never writes to `HKLM`.
- New fonts appear after reopening common apps.
- Existing identical font is skipped as already installed.
- Same file name with different content creates a deterministic suffixed file.
- Unsupported extensions are skipped with a stable error.
- System font conflicts are skipped and reported.

### App And Shortcut Flow

- A non-admin Windows user can run `Sync Now` successfully.
- First-run setup works with only server URL and optional API key.
- Saved settings survive app restart.
- Start Menu shortcut runs the expected command.
- Shortcut failure leaves a readable log.
- `Sync Both Ways` runs push before sync.
- Diagnostics view redacts API keys.

### Startup Agent

- Per-user startup agent runs after sign-in.
- Startup agent waits for network availability.
- Offline server does not create repeated noisy prompts.
- Successful background sync shows at most one concise notification.
- Repeated startup runs do not duplicate existing fonts.

### Cross-Platform MVP

- A font pushed from Windows appears in the server manifest.
- A macOS peer can sync the Windows-pushed font.
- A font pushed from macOS installs for the current Windows user.
- Identical font bytes deduplicate by SHA-256 across Windows and macOS.
- Dry-run output matches the planned install/skip behavior before real sync.

## Rust Change Proposals

Do not edit Rust source as part of this document-only pass. Recommended follow-up
changes for the agent/app implementer:

- Add a `--json` or guaranteed machine-readable mode for every command outcome,
  including errors.
- Add a `sync-both-ways` command or wrapper that runs `push` then `sync` with one
  combined report.
- Add a `config` command group for saving server URL, API key reference, device
  name, startup mode, and sync mode.
- Add `diagnostics` output that reports user font path, platform, server health,
  installed counts, skipped counts, and redacted config.
- Send `WM_FONTCHANGE` after Windows registry writes so running apps can refresh
  when possible.
- Consider Windows Credential Manager for API key storage instead of environment
  variables or shortcut arguments.
- Add explicit network-profile detection if Windows hosting or LAN discovery is
  implemented.
- Add a per-user startup helper or Scheduled Task installer command.

## MVP Acceptance

The Windows LAN app is acceptable when a non-technical user can:

1. Open the app.
2. Enter a local server address.
3. Click `Test Connection`.
4. Click `Sync Now`.
5. See clear counts and skipped items.
6. Reopen their design app and use the synced fonts.

No part of that path should require administrator rights, editing environment
variables, opening PowerShell, or understanding Windows font registry locations.
