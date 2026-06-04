# App Install MVP

SyncMyFonts is currently a CLI-first MVP. The launcher artifacts in
`packaging/macos` and `packaging/windows` make it behave like a small local app
without changing the Rust agent.

## Build the Agent

```sh
cargo build --release -p syncmyfonts-agent
```

The launcher helpers expect the built binary:

- macOS: `target/release/syncmyfonts-agent`
- Windows: `target\release\syncmyfonts-agent.exe`

## macOS

Use a per-user LaunchAgent. It runs in the signed-in user's session, can access
that user's font folders, and does not require sudo.

Serve fonts on the LAN:

```sh
packaging/macos/install-launchagent.sh serve \
  --agent-path "$PWD/target/release/syncmyfonts-agent" \
  --lan-key "choose-a-shared-key"
```

Pull fonts from another LAN peer at sign-in and every 4 hours:

```sh
packaging/macos/install-launchagent.sh sync \
  --agent-path "$PWD/target/release/syncmyfonts-agent" \
  --lan-key "choose-a-shared-key" \
  --peer "http://192.168.1.50:7370" \
  --interval 14400
```

Logs are written to `~/Library/Logs/SyncMyFonts`.

## Windows

Prefer a current-user Scheduled Task or Startup folder shortcut. Do not use a
Windows service for the MVP because services run outside the user's normal font
install context.

Scheduled sync:

```powershell
.\packaging\windows\install-startup-task.ps1 `
  -Mode Sync `
  -AgentPath "$PWD\target\release\syncmyfonts-agent.exe" `
  -LanKey "choose-a-shared-key" `
  -Peer "http://192.168.1.50:7370" `
  -RepeatHours 4
```

Startup shortcut:

```powershell
.\packaging\windows\create-startup-shortcut.ps1 `
  -Mode Sync `
  -AgentPath "$PWD\target\release\syncmyfonts-agent.exe" `
  -LanKey "choose-a-shared-key" `
  -Peer "http://192.168.1.50:7370"
```

Generated wrappers and logs live under `%LOCALAPPDATA%\SyncMyFonts`.

## Recommendations

- Treat `lan-serve` as an explicit trusted-network action. It opens a local LAN
  listener on `0.0.0.0:7370` by default.
- Keep sync pull-only for the MVP and run both directions manually or through
  separate launchers if both devices should exchange fonts.
- Move LAN keys into Keychain on macOS and Windows Credential Manager before
  shipping a public installer.
- Add a tray/menu-bar settings UI later to manage peer URL, startup mode, last
  sync result, and logs.
