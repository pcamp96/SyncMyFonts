# App MVP Checklist

Scope: concrete MVP checklist for a macOS plus Windows local sync app built on
the existing Rust CLI agent and LAN engine.

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
- `syncmyfonts-agent verify-managed`
- `syncmyfonts-agent gui`

## Must-Have Product Behavior

- [x] Sync only current-user fonts and SyncMyFonts-managed fonts.
- [x] Never scan, copy, install, delete, or mutate system font directories.
- [x] Skip installs whose filename conflicts with known system font directories.
- [x] Install synced fonts without administrator privileges.
- [x] Treat SHA-256 as the font identity and deduplicate identical bytes across
  macOS and Windows.
- [x] Preserve pull-only LAN semantics: one device serves, the other pulls.
- [x] Support bidirectional LAN sync by running pull once in each direction.
- [x] Verify downloaded font bytes against the expected SHA-256 before final
  install.
- [x] Skip unsupported font formats with a visible result.
- [x] Avoid deletion propagation in the MVP.
- [x] Show clear installed, skipped, dry-run, and failure counts.

## Must-Have Commands And Wrappers

### CLI Surface

- [x] `Scan Local Fonts` wraps `syncmyfonts-agent scan`.
- [x] `Test Peer` validates a peer by running a dry-run LAN sync.
- [x] `Preview From Peer` wraps
  `syncmyfonts-agent lan-sync --peer http://<peer-lan-ip>:7370 --dry-run`.
- [x] `Get Missing Fonts From Peer` wraps
  `syncmyfonts-agent lan-sync --peer http://<peer-lan-ip>:7370`.
- [x] `Save Peer` wraps
  `syncmyfonts-agent lan-add-peer --name <name> --url http://<peer-lan-ip>:7370`.
- [x] `Find LAN Peers` wraps `syncmyfonts-agent lan-discover`.
- [x] `Pair Peer` exchanges an 8-digit pairing code for a saved LAN token.
- [x] `Sync Saved Peers` wraps `syncmyfonts-agent lan-sync-all`.
- [x] `Diagnostics` wraps `syncmyfonts-agent diagnostics`.
- [x] `Verify Managed Fonts` wraps `syncmyfonts-agent verify-managed`.
- [x] `Open Native GUI` wraps `syncmyfonts-agent gui`.
- [x] `Open Browser Control Surface` wraps `syncmyfonts-agent app`.
- [x] `Share Fonts On This Network` starts
  `syncmyfonts-agent lan-serve --listen 0.0.0.0:7370`.
- [x] `Stop Sharing` terminates the running `lan-serve` process cleanly.
- [ ] `Preview From Server` wraps
  `syncmyfonts-agent sync --server http://<server>:7368 --dry-run`.
- [ ] `Get Missing Fonts From Server` wraps
  `syncmyfonts-agent sync --server http://<server>:7368`.
- [ ] `Send My Fonts To Server` wraps
  `syncmyfonts-agent push --server http://<server>:7368`.
- [ ] `Sync Both Ways With Server` runs `push` first, then `sync`.

### Required Inputs

- [x] Peer URL for LAN peer mode, for example `http://192.168.0.50:7370`.
- [x] Shared LAN key when peer mode uses `SYNCMYFONTS_LAN_KEY`.
- [ ] Server URL for central server mode, for example `http://192.168.0.50:7368`.
- [ ] Optional server API key, passed through `SYNCMYFONTS_API_KEY` or a
  per-user secret store.
- [ ] Friendly device name for UI, logs, and diagnostics.
- [x] Copyable LAN URL is shown after sharing starts.

## Must-Have Artifacts

### Shared

- [ ] Signed or otherwise trusted app bundle/executable for each platform.
- [ ] Bundled `syncmyfonts-agent` binary compiled for the target platform.
- [ ] Per-user config file with server URL, peer URL history, sync mode, and
  startup preference.
- [ ] Per-user log directory.
- [x] Diagnostics output that redacts API keys and LAN keys.
- [x] A short first-run setup path for manual peer URL entry.
- [ ] A copyable support report with app version, agent version, platform,
  config paths, font paths, last command, and last result.

### macOS

- [ ] App bundle with Local Network usage copy if Bonjour, discovery, or bundled
  LAN access is used.
- [x] Managed install folder:
  `~/Library/Fonts/SyncMyFonts`.
- [x] App support folder:
  `~/Library/Application Support/SyncMyFonts`.
- [ ] Log folder:
  `~/Library/Logs/SyncMyFonts`.
- [ ] Optional user LaunchAgent in `~/Library/LaunchAgents` for scheduled sync.
- [x] App action to open the managed font folder.

### Windows

- [x] User install folder:
  `%LOCALAPPDATA%\Microsoft\Windows\Fonts`.
- [x] Current-user registry registration under
  `HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts`.
- [x] App config under `%LOCALAPPDATA%\SyncMyFonts`.
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
- [x] GitHub Actions proves macOS and Windows build/test/release packaging.
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
- [x] Wrong LAN key fails without exposing font manifests or blobs.
- [ ] Offline peer or bad peer URL produces a visible failure and keeps local
  fonts untouched.

### Server Sync

- [ ] `docker compose up --build` starts the server on `http://localhost:7368`.
- [x] `GET /healthz` succeeds from the app host.
- [ ] `push --server http://<server>:7368` uploads current-user fonts.
- [ ] `sync --server http://<server>:7368 --dry-run` previews missing fonts.
- [ ] `sync --server http://<server>:7368` installs missing fonts.
- [ ] Server auth works when `SYNCMYFONTS_API_KEY` is required.
- [ ] API key values are not written to logs, shortcuts, LaunchAgent plists, or
  diagnostics output.

### Platform Safety

- [x] SyncMyFonts writes a local managed-font manifest for fonts it installs.

- [ ] macOS scan excludes `/System/Library/Fonts`, `/Library/Fonts`, and
  `/Network/Library/Fonts`.
- [ ] macOS install does not request sudo and does not run font cache reset
  commands.
- [ ] Windows install never writes to `C:\Windows\Fonts`.
- [ ] Windows install never writes to `HKLM`.
- [ ] Windows install writes only current-user font registry entries.
- [x] Unsupported `.woff`, `.woff2`, or unknown extensions are skipped.
- [x] Same file name with different bytes creates a deterministic suffixed file
  or another documented non-overwrite outcome.
- [x] Hash mismatch fails before final install.
- [x] Unit tests cover filename sanitization, stable hash IDs, peer URL
  normalization, and diagnostics secret redaction.

### App UX

- [x] First-run setup works with manual URL entry.
- [x] `Test Connection` checks peer health before sync.
- [ ] `Dry Run` result matches the following real sync result.
- [x] The main app view shows last sync time, last result, and warning count.
- [x] The app shows "reopen your design app" guidance after successful install.
- [ ] Denied macOS Local Network permission still allows manual URL fallback.
- [ ] Windows client-only mode does not request an inbound firewall exception.
- [x] Hosted peer mode clearly says it is only for trusted local networks.

## Current Engine Coverage

Already present in the repo:

- CLI commands for `scan`, `push`, `sync`, `lan-serve`, and `lan-sync`.
- CLI commands for saved peers: `lan-add-peer`, `lan-peers`, and
  `lan-sync-all`.
- CLI/app command for redacted diagnostics: `diagnostics`.
- Native desktop GUI command: `gui`.
- Local browser control surface command: `app`.
- LAN peer HTTP endpoints for health, manifest, and blob download.
- Optional LAN bearer key through `SYNCMYFONTS_LAN_KEY`.
- Server API key support through `SYNCMYFONTS_API_KEY`.
- SHA-256 verification before installing downloaded fonts.
- macOS managed install path under `~/Library/Fonts/SyncMyFonts`.
- Windows per-user install path under
  `%LOCALAPPDATA%\Microsoft\Windows\Fonts`.
- Windows `HKCU` font registry registration through `reg.exe`.
- Windows `WM_FONTCHANGE` broadcast after user font registry registration.
- Deterministic same-name different-content suffixing.
- Docker Compose server startup.
- Per-user JSON config with a stable local device ID and saved LAN peers.
- macOS LaunchAgent templates and install/uninstall helpers.
- Windows current-user Scheduled Task and Startup shortcut helpers.
- macOS release helper that packages the agent, docs, and launcher helpers.
- Windows release helper script for PowerShell-based packaging.
- GitHub Actions workflow for macOS and Windows build/test/package checks.
- Unit tests for core helper behavior used by the local app and LAN sync flow.
- Integration test for saved LAN peer sync using two isolated user font roots.

## Remaining Gaps

- [x] Native one-window GUI for macOS and Windows.
- [x] Local browser app/control surface for development and server-adjacent workflows.
- [ ] Native tray/menu-bar UI for macOS.
- [ ] Native tray UI for Windows.
- [x] Basic release archive scripts for macOS and Windows.
- [x] Stored per-user config with saved peer URLs and LAN keys.
- [ ] Keychain or Credential Manager integration for secrets.
- [ ] Stable machine-readable reports for every command and error.
- [x] Dedicated diagnostics command or support-report artifact.
- [x] Local ownership manifest for managed fonts.
- [ ] Explicit conflict review UI.
- [x] Lightweight UDP LAN peer discovery.
- [ ] Bonjour/mDNS discovery.
- [x] First pairing-code flow for exchanging a short code for a saved LAN token.
- [ ] Firewall/network-profile detection for hosted Windows peer mode.
- [ ] macOS bundled-app Local Network permission testing.
- [x] Windows `WM_FONTCHANGE` notification after registry writes.
- [x] Automated LAN test fixture for saved-peer pull/install behavior.

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
