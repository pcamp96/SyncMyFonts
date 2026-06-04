# App MVP Checklist

Scope: concrete MVP checklist for a macOS plus Windows local sync app built on
the existing Rust CLI agent and LAN engine. This checklist intentionally does
not require Rust source changes for the current documentation pass.

## MVP Goal

Ship a user-scoped app or thin wrapper that lets a normal macOS user and a
normal Windows user sync their own installed fonts across the same trusted LAN
without admin rights, cloud accounts, or system-font mutation.

The app MVP should reuse the current agent commands:

- `syncmyfonts-agent scan`
- `syncmyfonts-agent push --server <url>`
- `syncmyfonts-agent sync --server <url>`
- `syncmyfonts-agent sync --server <url> --dry-run`
- `syncmyfonts-agent lan-serve --listen 0.0.0.0:7370`
- `syncmyfonts-agent lan-sync --peer <url>`
- `syncmyfonts-agent lan-sync --peer <url> --dry-run`
- `syncmyfonts-agent lan-add-peer --name <name> --url <url>`
- `syncmyfonts-agent lan-peers`
- `syncmyfonts-agent lan-sync-all`

## Must-Have Product Behavior

- [ ] Sync only current-user fonts and SyncMyFonts-managed fonts.
- [ ] Never scan, copy, install, delete, or mutate system font directories.
- [ ] Install synced fonts without administrator privileges.
- [ ] Treat SHA-256 as the font identity and deduplicate identical bytes across
  macOS and Windows.
- [ ] Preserve pull-only LAN semantics: one device serves, the other pulls.
- [ ] Support bidirectional LAN sync by running pull once in each direction.
- [ ] Verify downloaded font bytes against the expected SHA-256 before final
  install.
- [ ] Skip unsupported font formats with a visible result.
- [ ] Avoid deletion propagation in the MVP.
- [ ] Show clear installed, skipped, dry-run, and failure counts.

## Must-Have Commands And Wrappers

### CLI Surface

- [ ] `Scan Local Fonts` wraps `syncmyfonts-agent scan`.
- [ ] `Preview From Peer` wraps
  `syncmyfonts-agent lan-sync --peer http://<peer-lan-ip>:7370 --dry-run`.
- [ ] `Get Missing Fonts From Peer` wraps
  `syncmyfonts-agent lan-sync --peer http://<peer-lan-ip>:7370`.
- [ ] `Save Peer` wraps
  `syncmyfonts-agent lan-add-peer --name <name> --url http://<peer-lan-ip>:7370`.
- [ ] `Sync Saved Peers` wraps `syncmyfonts-agent lan-sync-all`.
- [ ] `Share Fonts On This Network` starts
  `syncmyfonts-agent lan-serve --listen 0.0.0.0:7370`.
- [ ] `Stop Sharing` terminates the running `lan-serve` process cleanly.
- [ ] `Preview From Server` wraps
  `syncmyfonts-agent sync --server http://<server>:7368 --dry-run`.
- [ ] `Get Missing Fonts From Server` wraps
  `syncmyfonts-agent sync --server http://<server>:7368`.
- [ ] `Send My Fonts To Server` wraps
  `syncmyfonts-agent push --server http://<server>:7368`.
- [ ] `Sync Both Ways With Server` runs `push` first, then `sync`.

### Required Inputs

- [ ] Peer URL for LAN peer mode, for example `http://192.168.0.50:7370`.
- [ ] Shared LAN key when peer mode uses `SYNCMYFONTS_LAN_KEY`.
- [ ] Server URL for central server mode, for example `http://192.168.0.50:7368`.
- [ ] Optional server API key, passed through `SYNCMYFONTS_API_KEY` or a
  per-user secret store.
- [ ] Friendly device name for UI, logs, and diagnostics.

## Must-Have Artifacts

### Shared

- [ ] Signed or otherwise trusted app bundle/executable for each platform.
- [ ] Bundled `syncmyfonts-agent` binary compiled for the target platform.
- [ ] Per-user config file with server URL, peer URL history, sync mode, and
  startup preference.
- [ ] Per-user log directory.
- [ ] Diagnostics output that redacts API keys and LAN keys.
- [ ] A short first-run setup path for manual peer or server URL entry.
- [ ] A copyable support report with app version, agent version, platform,
  config paths, font paths, last command, and last result.

### macOS

- [ ] App bundle with Local Network usage copy if Bonjour, discovery, or bundled
  LAN access is used.
- [ ] Managed install folder:
  `~/Library/Fonts/SyncMyFonts`.
- [ ] App support folder:
  `~/Library/Application Support/SyncMyFonts`.
- [ ] Log folder:
  `~/Library/Logs/SyncMyFonts`.
- [ ] Optional user LaunchAgent in `~/Library/LaunchAgents` for scheduled sync.
- [ ] App action to open the managed font folder.

### Windows

- [ ] User install folder:
  `%LOCALAPPDATA%\Microsoft\Windows\Fonts`.
- [ ] Current-user registry registration under
  `HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts`.
- [ ] App config and logs under `%LOCALAPPDATA%\SyncMyFonts`.
- [ ] Start Menu shortcuts for `Sync Now`, `Send My Fonts`,
  `Get Missing Fonts`, and `Diagnostics` if there is no full tray UI yet.
- [ ] Per-user startup option through a tray app, Startup folder, `HKCU\Run`, or
  a current-user Scheduled Task.
- [ ] Plain guidance for firewall prompts when Windows is intentionally hosting:
  allow Private networks only.

## Verification Gates

### Repo And Build

- [ ] `cargo build` succeeds on macOS.
- [ ] `cargo build` succeeds on Windows.
- [ ] `cargo run -p syncmyfonts-agent -- scan` returns JSON inventory on macOS.
- [ ] `cargo run -p syncmyfonts-agent -- scan` returns JSON inventory on
  Windows.
- [ ] The packaged app invokes the same agent binary or library path as the CLI.

### LAN Peer Sync

- [ ] macOS can run
  `SYNCMYFONTS_LAN_KEY=<key> syncmyfonts-agent lan-serve --listen 0.0.0.0:7370`.
- [ ] Windows can run
  `SYNCMYFONTS_LAN_KEY=<key> syncmyfonts-agent lan-sync --peer http://<mac-ip>:7370 --dry-run`.
- [ ] Windows can run the same `lan-sync` without `--dry-run` and install
  missing fonts for the current user.
- [ ] Windows can run `lan-serve` and macOS can run `lan-sync --dry-run` against
  the Windows peer.
- [ ] macOS can run the same `lan-sync` without `--dry-run` and install missing
  fonts under `~/Library/Fonts/SyncMyFonts`.
- [ ] Running the same LAN sync twice skips already-present fonts.
- [ ] Wrong LAN key fails without exposing font manifests or blobs.
- [ ] Offline peer or bad peer URL produces a visible failure and keeps local
  fonts untouched.

### Server Sync

- [ ] `docker compose up --build` starts the server on `http://localhost:7368`.
- [ ] `GET /healthz` succeeds from the app host.
- [ ] `push --server http://<server>:7368` uploads current-user fonts.
- [ ] `sync --server http://<server>:7368 --dry-run` previews missing fonts.
- [ ] `sync --server http://<server>:7368` installs missing fonts.
- [ ] Server auth works when `SYNCMYFONTS_API_KEY` is required.
- [ ] API key values are not written to logs, shortcuts, LaunchAgent plists, or
  diagnostics output.

### Platform Safety

- [ ] macOS scan excludes `/System/Library/Fonts`, `/Library/Fonts`, and
  `/Network/Library/Fonts`.
- [ ] macOS install does not request sudo and does not run font cache reset
  commands.
- [ ] Windows install never writes to `C:\Windows\Fonts`.
- [ ] Windows install never writes to `HKLM`.
- [ ] Windows install writes only current-user font registry entries.
- [ ] Unsupported `.woff`, `.woff2`, or unknown extensions are skipped.
- [ ] Same file name with different bytes creates a deterministic suffixed file
  or another documented non-overwrite outcome.
- [ ] Hash mismatch fails before final install.

### App UX

- [ ] First-run setup works with manual URL entry.
- [ ] `Test Connection` checks server health or peer health before sync.
- [ ] `Dry Run` result matches the following real sync result.
- [ ] The main app view shows last sync time, last result, and warning count.
- [ ] The app shows "reopen your design app" guidance after successful install.
- [ ] Denied macOS Local Network permission still allows manual URL fallback.
- [ ] Windows client-only mode does not request an inbound firewall exception.
- [ ] Hosted peer mode clearly says it is only for trusted local networks.

## Current Engine Coverage

Already present in the repo:

- CLI commands for `scan`, `push`, `sync`, `lan-serve`, and `lan-sync`.
- CLI commands for saved peers: `lan-add-peer`, `lan-peers`, and
  `lan-sync-all`.
- LAN peer HTTP endpoints for health, manifest, and blob download.
- Optional LAN bearer key through `SYNCMYFONTS_LAN_KEY`.
- Server API key support through `SYNCMYFONTS_API_KEY`.
- SHA-256 verification before installing downloaded fonts.
- macOS managed install path under `~/Library/Fonts/SyncMyFonts`.
- Windows per-user install path under
  `%LOCALAPPDATA%\Microsoft\Windows\Fonts`.
- Windows `HKCU` font registry registration through `reg.exe`.
- Deterministic same-name different-content suffixing.
- Docker Compose server startup.
- Per-user JSON config with a stable local device ID and saved LAN peers.
- macOS LaunchAgent templates and install/uninstall helpers.
- Windows current-user Scheduled Task and Startup shortcut helpers.

## Remaining Gaps

- [ ] App wrapper or tray/menu-bar UI for macOS.
- [ ] App wrapper or tray UI for Windows.
- [ ] Packaged binaries and installer/update flow for both platforms.
- [x] Stored per-user config with saved peer URLs and LAN keys.
- [ ] Keychain or Credential Manager integration for secrets.
- [ ] Stable machine-readable reports for every command and error.
- [ ] Dedicated diagnostics command or support-report artifact.
- [ ] Local ownership manifest for managed fonts.
- [ ] Explicit conflict review UI.
- [ ] Bonjour/mDNS discovery.
- [ ] Pairing-code flow from the protocol doc.
- [ ] Firewall/network-profile detection for hosted Windows peer mode.
- [ ] macOS bundled-app Local Network permission testing.
- [ ] Windows `WM_FONTCHANGE` notification after registry writes.
- [ ] Automated cross-platform LAN test fixture.

## MVP Acceptance

The local app MVP is ready when a non-technical user can:

1. Install the app on a Mac and a Windows PC.
2. Open `Share Fonts On This Network` on the computer that has fonts.
3. Enter the shown peer URL and shared key on the other computer.
4. Run `Preview From Peer`.
5. Run `Get Missing Fonts From Peer`.
6. Repeat the flow in the other direction if needed.
7. Reopen their design app and use the synced fonts.
8. Read a concise result showing installed, skipped, and failed items.

No part of that path should require administrator rights, editing environment
variables by hand, opening system font directories, or understanding the Windows
registry.
