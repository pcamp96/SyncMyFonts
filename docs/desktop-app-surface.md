# Desktop App Control Surface MVP

Scope: smallest macOS plus Windows desktop control surface that makes the
existing Rust agent usable by a non-technical person for local font sync. This
document tracks the native one-window GUI and future wrapper/tray guidance.

## MVP Decision

Build a thin desktop wrapper around the existing `syncmyfonts-agent` binary. The
MVP should be one normal app window on each platform, with optional menu-bar or
tray status later. It should not implement its own font scanner, installer,
network protocol, or conflict logic.

The MVP qualifies when a Mac and a Windows PC on the same trusted LAN can:

1. Open the app on the device that has fonts.
2. Turn on local sharing.
3. Copy the shown peer URL and shared key to the other device.
4. Preview missing fonts.
5. Install the missing fonts.
6. Repeat in the other direction when needed.

Server sync can exist as a secondary settings mode, but LAN peer sync is the
smallest default path because it avoids accounts, cloud setup, port forwarding,
and admin rights.

## Primary Window

The first MVP window has four fixed areas.

### Status Header

Shows:

- Device name.
- Platform: `macOS` or `Windows`.
- Sharing state: `Off`, `Starting`, `On`, `Stopping`, or `Failed`.
- Last sync result.
- Managed font folder shortcut.

Primary actions:

- `Sync Now`
- `Preview`
- `Share`
- `Stop`
- `Settings`
- `Diagnostics`

### Peer Setup

Fields:

- Peer name, for example `Workshop PC`.
- Peer URL, for example `http://192.168.0.50:7370`.
- Shared LAN key.

Actions:

- `Test Peer`
- `Save Peer`
- `Preview From Peer`
- `Get Missing Fonts From Peer`
- `Sync Saved Peers`

The app can hide this section after at least one peer is saved, but it must keep
manual URL entry available for the MVP.

### Results

Show the latest command result as counts plus details:

- Scanned.
- Uploaded.
- Installed.
- Already present.
- Unsupported or skipped.
- Failed.
- Dry run or real install.

For installed fonts, show:

```text
Fonts are installed. Reopen your design apps if they do not appear yet.
```

### Settings

Minimum settings:

- Device name.
- LAN listen address, default `0.0.0.0:7370`.
- Shared LAN key.
- Saved peers.
- Optional server URL, default blank.
- Optional server API key.
- Startup preference: `Manual only` for MVP default.

Secrets should not be shown after saving. The first pass may store the LAN key
and API key in the existing per-user config if needed, but the UI and
diagnostics must redact them.

## UI Action To Command Mapping

Use the bundled `syncmyfonts-agent` executable directly.

| UI action | Command |
| --- | --- |
| Scan Local Fonts | `syncmyfonts-agent scan` |
| Scan Including Managed Fonts | `syncmyfonts-agent scan --include-managed` |
| Verify Managed Fonts | `syncmyfonts-agent verify-managed` |
| Repair Managed Fonts | `syncmyfonts-agent repair-managed` |
| Share | `syncmyfonts-agent lan-serve --listen 0.0.0.0:7370 --lan-key <key>` |
| Stop | Terminate the running `lan-serve` child process cleanly |
| Test Peer | `GET <peer>/api/lan/v1/health` or `syncmyfonts-agent lan-sync --peer <peer> --lan-key <key> --dry-run` |
| Save Peer | `syncmyfonts-agent lan-add-peer --name <name> --url <peer> --lan-key <key>` |
| List Saved Peers | `syncmyfonts-agent lan-peers` |
| Preview From Peer | `syncmyfonts-agent lan-sync --peer <peer> --lan-key <key> --dry-run` |
| Get Missing Fonts From Peer | `syncmyfonts-agent lan-sync --peer <peer> --lan-key <key>` |
| Preview Saved Peers | `syncmyfonts-agent lan-sync-all --dry-run` |
| Sync Saved Peers | `syncmyfonts-agent lan-sync-all` |
| Preview From Server | `syncmyfonts-agent sync --server <url> --api-key <key> --dry-run` |
| Get Missing Fonts From Server | `syncmyfonts-agent sync --server <url> --api-key <key>` |
| Send My Fonts To Server | `syncmyfonts-agent push --server <url> --api-key <key>` |
| Sync Both Ways With Server | Run `push`, then run `sync` |
| Open Native GUI | `syncmyfonts-agent gui` |

If a secret is already stored in the user config, prefer passing it through the
environment variables already supported by the agent:

- `SYNCMYFONTS_LAN_KEY`
- `SYNCMYFONTS_API_KEY`
- `SYNCMYFONTS_SERVER`

Do not put secrets in shortcuts, launch agents, scheduled task arguments, logs,
or diagnostics.

## State Model

### App States

- `first-run`: no peers and no server URL are configured.
- `ready`: at least one peer or server URL is configured.
- `running-command`: a foreground command is active.
- `sharing`: `lan-serve` child process is running.
- `offline`: saved peer or server did not respond.
- `needs-attention`: last command failed or produced warnings.

### Sharing States

- `off`: no `lan-serve` process.
- `starting`: process launched but health is not confirmed.
- `on`: process is running and local URL is shown.
- `stopping`: stop was requested.
- `failed`: process exited unexpectedly or could not bind the port.

When sharing is `on`, show:

```text
Sharing on this trusted network at http://<this-device-lan-ip>:7370
```

Windows-specific copy when hosting:

```text
If Windows asks, allow Private networks only.
```

### Command Result States

- `success`: command exited with status 0 and no warnings.
- `success-with-warnings`: command exited with status 0 and warnings or skipped
  items.
- `dry-run`: command exited with status 0 and did not install fonts.
- `failed`: command exited non-zero.
- `cancelled`: user stopped the command.

## Error Messages

Keep error messages short, actionable, and mapped to known command failures.

| Condition | User-facing message |
| --- | --- |
| Peer URL is empty or invalid | `Enter a peer URL like http://192.168.0.50:7370.` |
| Peer is offline | `That device did not respond. Make sure SyncMyFonts is open and sharing on the same network.` |
| Wrong LAN key or unauthorized peer | `The shared key did not match. Check the key shown on the other device and try again.` |
| Port already in use | `SyncMyFonts could not share fonts because port 7370 is already in use.` |
| Public Windows network while hosting | `This network is marked Public, so SyncMyFonts will not accept local connections here.` |
| Local Network permission denied on macOS | `Local Network access is off. Enable SyncMyFonts in System Settings, or enter the peer URL manually.` |
| Unsupported font format | `Some files were skipped because SyncMyFonts only installs desktop font files.` |
| Hash mismatch | `A downloaded font did not match its expected fingerprint, so it was not installed.` |
| Permission denied | `SyncMyFonts could not write to your user font folder. Check folder permissions and try again.` |
| Server URL is empty or invalid | `Enter a server URL like http://192.168.0.50:7368.` |
| Server auth failed | `The server API key was rejected. Check the saved key and try again.` |
| Server offline | `The server did not respond. Check the address and make sure the server is running.` |
| Unknown command failure | `SyncMyFonts could not finish this command. Open Diagnostics for details.` |

Diagnostics may include raw command stderr, but must redact API keys and LAN
keys.

## Platform Surface

### macOS

Required:

- App bundle with bundled `syncmyfonts-agent`.
- Managed folder shortcut to `~/Library/Fonts/SyncMyFonts`.
- App support path display for
  `~/Library/Application Support/SyncMyFonts`.
- Logs under `~/Library/Logs/SyncMyFonts`.
- Manual peer URL entry.
- If the app uses bundled local networking, include Local Network usage copy.

Do not request sudo, write to `/Library/Fonts`, write to
`/System/Library/Fonts`, or run font cache reset commands.

### Windows

Required:

- App executable with bundled `syncmyfonts-agent.exe`.
- Managed install path display for
  `%LOCALAPPDATA%\Microsoft\Windows\Fonts`.
- Config and logs under `%LOCALAPPDATA%\SyncMyFonts`.
- Manual peer URL entry.
- Clear Private-network-only copy when sharing.

Do not request UAC, write to `C:\Windows\Fonts`, write to `HKLM`, or require a
Windows service for the MVP.

## Acceptance Tests

### Static Wrapper Tests

- The packaged macOS app contains the macOS `syncmyfonts-agent` binary.
- The packaged Windows app contains the Windows `syncmyfonts-agent.exe` binary.
- The app can run `scan` and parse JSON on each platform.
- Diagnostics redact `SYNCMYFONTS_LAN_KEY` and `SYNCMYFONTS_API_KEY`.
- No shortcut, LaunchAgent, scheduled task, or log includes a secret in plain
  command arguments.

### First-Run LAN Tests

- Fresh install opens in `first-run` state.
- User can start `Share` on macOS and see a LAN URL plus key.
- Windows can save that Mac peer with `lan-add-peer`.
- Windows `Preview From Peer` runs `lan-sync --dry-run` and shows planned
  installs without writing fonts.
- Windows `Get Missing Fonts From Peer` runs `lan-sync` and installs missing fonts for
  the current user only.
- Running Windows `Get Missing Fonts From Peer` a second time reports already-present
  fonts instead of reinstalling them.
- Windows can start `Share`, macOS can preview from Windows, and macOS can
  install missing fonts into `~/Library/Fonts/SyncMyFonts`.

### Failure Tests

- Wrong LAN key fails without showing the peer manifest or installing fonts.
- Offline peer shows the offline message and keeps local fonts untouched.
- Invalid peer URL blocks sync before launching the agent command.
- Port `7370` already in use puts sharing into `failed` state.
- Unsupported `.woff` or `.woff2` files are skipped with a visible count.
- Hash mismatch fails before final install.
- Cancelling a running sync leaves already-installed fonts in place and reports
  the command as cancelled.

### Server Mode Tests

- User can enter `http://localhost:7368` or another LAN server URL.
- `Preview From Server` runs `sync --dry-run` and shows planned installs.
- `Send My Fonts To Server` runs `push` and reports scanned, uploaded, and
  skipped counts.
- `Sync Both Ways With Server` runs `push` before `sync`.
- Bad API key shows the server auth failure message and redacts the key in
  diagnostics.

## Smallest Recommended Build Order

1. Build the one-window manual LAN wrapper first.
2. Add saved-peer actions using `lan-add-peer`, `lan-peers`, and
   `lan-sync-all`.
3. Add diagnostics and secret redaction before wider testing.
4. Add server mode after LAN peer flow works on both platforms.
5. Defer Bonjour/mDNS discovery, tray/menu-bar background sync, conflict
   review, and auto-update until after the MVP path is boringly
   reliable.
