# App Install MVP

SyncMyFonts is currently a native-GUI MVP backed by the same Rust agent that
also exposes CLI commands for scripting and diagnostics.

## Start The Local App

Run the native GUI from the release folder:

```text
macOS:   SyncMyFonts.app
Windows: bin\syncmyfonts-gui.exe
```

Or use the fallback command from a local build:

```sh
syncmyfonts-agent gui
```

The local app can start LAN sharing, show an 8-digit pairing code when no
shared key is provided, find sharing peers on the LAN, pair with a peer, test a
peer, preview missing fonts, install missing fonts, save peers, and run
diagnostics. It can also verify that SyncMyFonts-managed installed font files
still match the local manifest and re-run platform registration for intact
managed fonts if they stop appearing correctly.
On macOS, managed verification also checks that each managed font can be parsed
through CoreText, which is a stronger signal than only confirming the file path
and hash.
When sharing is on, use `Copy Pairing Instructions` to move the LAN URL,
pairing code, and next action to the other computer without retyping. `Copy URL`
and `Copy Code` are still available when you only need one value.
Turn on `Auto Sync Saved Peers` if this computer should keep checking saved LAN
peers on an interval while the app stays open. The app saves that preference in
the per-user config so it survives relaunches.
Use `Diagnostics` for a copyable support report, `Open Managed Folder` to see
fonts installed by SyncMyFonts, and `Open Logs` to open the per-user action
history folder.
Use `Open App Support` to inspect the per-user config, saved peers,
preferences, and managed-font manifest location.
After running `Diagnostics`, use `Copy Support Report` to copy the redacted
support text without selecting JSON by hand.
Use `Readiness Check` before live two-machine testing to confirm local app
paths, saved peers, and sign-in sync helper status.
Use `Validation Report` before and after a real Mac-to-Windows test to save
diagnostics, readiness, managed-font verification, and the manual pass criteria
as one redacted JSON bundle in the log folder.
Use `Enable Sign-In Sync` after pairing peers if this computer should pull
missing fonts from saved peers whenever the user signs in.
On Windows, use `Install App Shortcuts` to create current-user Start Menu
launchers for the native app, saved-peer sync, dry-run preview, diagnostics,
and readiness check.
Set `Device Name` in the app header before pairing if the default computer name
is unclear. This name appears in LAN discovery, pairing, diagnostics, and
support reports.
If you change the `Listen Address` for a custom LAN port, SyncMyFonts saves the
valid address after sharing starts successfully and uses it the next time the
app opens.

The browser control surface is kept as an explicit development and future
self-hosted/server-adjacent command:

```sh
syncmyfonts-agent app
```

Installed fonts are tracked in a local managed-font manifest next to the app
config. This record only includes fonts installed by SyncMyFonts and keeps
system fonts outside the sync ownership model.

For real Mac-to-Windows validation, use
`docs/manual-clean-machine-validation.md`. CI proves the packaged pairing flow
on isolated macOS and Windows runners, but a clean-machine pass is still the
proof that local firewall prompts, macOS Local Network behavior, and real font
visibility match the expected user experience.

## Build the Agent

```sh
cargo build --release -p syncmyfonts-agent --bins
```

The launcher helpers expect the built binary:

- Portable macOS archive: `bin/syncmyfonts-agent`
- Portable Windows archive: `bin\syncmyfonts-agent.exe`
- Source checkout on macOS: `target/release/syncmyfonts-agent`
- Source checkout on Windows: `target\release\syncmyfonts-agent.exe`

The portable GUI launchers are:

- Portable macOS archive: `SyncMyFonts.app` or `bin/syncmyfonts-gui`
- Portable Windows archive: `bin\syncmyfonts-gui.exe`
- Source checkout on macOS: `target/release/syncmyfonts-gui`
- Source checkout on Windows: `target\release\syncmyfonts-gui.exe`

## macOS

Use a per-user LaunchAgent. It runs in the signed-in user's session, can access
that user's font folders, and does not require sudo.
After pairing peers in the GUI, prefer the native app's `Enable Sign-In Sync`
button. The LaunchAgent scripts below are advanced/manual-key helpers.

Serve fonts on the LAN:

```sh
packaging/macos/install-launchagent.sh serve \
  --agent-path "$PWD/bin/syncmyfonts-agent" \
  --lan-key "choose-a-shared-key"
```

Pull fonts from another LAN peer at sign-in and every 4 hours:

```sh
packaging/macos/install-launchagent.sh sync \
  --agent-path "$PWD/bin/syncmyfonts-agent" \
  --lan-key "choose-a-shared-key" \
  --peer "http://192.168.1.50:7370" \
  --interval 14400
```

Logs are written to `~/Library/Logs/SyncMyFonts`. App support data is stored in
`~/Library/Application Support/SyncMyFonts`. The native app's `Open Logs` and
`Open App Support` buttons open those folders.

## Windows

Prefer a current-user Scheduled Task or Startup folder shortcut. Do not use a
Windows service for the MVP because services run outside the user's normal font
install context.

The app's `Enable Sign-In Sync` button writes a current-user Startup folder
helper that runs `lan-sync-all` against saved peers. The PowerShell helpers
below remain available for scheduled repeat sync or explicit serve mode with a
manual LAN key.

Scheduled sync:

```powershell
.\packaging\windows\install-startup-task.ps1 `
  -Mode Sync `
  -AgentPath "$PWD\bin\syncmyfonts-agent.exe" `
  -LanKey "choose-a-shared-key" `
  -Peer "http://192.168.1.50:7370" `
  -RepeatHours 4
```

Startup shortcut:

```powershell
.\packaging\windows\create-startup-shortcut.ps1 `
  -Mode Sync `
  -AgentPath "$PWD\bin\syncmyfonts-agent.exe" `
  -LanKey "choose-a-shared-key" `
  -Peer "http://192.168.1.50:7370"
```

Generated wrappers, config, and logs live under `%LOCALAPPDATA%\SyncMyFonts`.
The native app's `Open Logs` and `Open App Support` buttons open those folders.

## Recommendations

- Treat `lan-serve` as an explicit trusted-network action. It opens a local LAN
  listener on `0.0.0.0:7370` by default.
- If Windows asks for firewall access while sharing fonts, allow Private
  networks only. Client-only sync should not need an inbound firewall prompt.
- Keep sync pull-only for the MVP and run both directions manually or through
  separate launchers if both devices should exchange fonts.
- Move LAN keys into Keychain on macOS and Windows Credential Manager before
  shipping a public installer.
- Add a tray/menu-bar settings UI later to manage peer URL, startup mode, last
  sync result, and logs.
