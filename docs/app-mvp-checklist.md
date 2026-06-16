# App MVP Checklist

Scope: concrete MVP checklist for a macOS plus Windows local sync app built on
the existing Rust CLI agent and LAN engine.

This checklist tracks the native desktop app first. The browser control surface
is retained for development and future self-hosted/server-adjacent workflows,
but it is not the normal user-facing app.

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
- `syncmyfonts-agent lan-pair --name <name> --url <url> --pairing-code <code>`
- `syncmyfonts-agent lan-peers`
- `syncmyfonts-agent lan-sync-all`
- `syncmyfonts-agent doctor`
- `syncmyfonts-agent validation-report`
- `syncmyfonts-agent verify-managed`
- `syncmyfonts-agent repair-managed`
- `syncmyfonts-agent install-validation-font`
- `syncmyfonts-agent install-app-shortcuts`
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
- [x] `Test Connection` validates a peer by running a dry-run LAN sync.
- [x] `Preview From Peer` wraps
  `syncmyfonts-agent lan-sync --peer http://<peer-lan-ip>:7370 --dry-run`.
- [x] `Get Missing Fonts From Peer` wraps
  `syncmyfonts-agent lan-sync --peer http://<peer-lan-ip>:7370`.
- [x] `Save Peer` wraps
  `syncmyfonts-agent lan-add-peer --name <name> --url http://<peer-lan-ip>:7370`.
- [x] `Find LAN Peers` wraps `syncmyfonts-agent lan-discover`.
- [x] `Pair Peer` exchanges an 8-digit pairing code for a saved LAN token.
- [x] Saved-peer selector loads a chosen saved Mac or Windows PC instead of
  assuming the first saved peer.
- [x] `Forget Peer` removes the selected saved peer from the native GUI and
  clears stale connection fields after removal.
- [x] `Sync Saved Peers` wraps `syncmyfonts-agent lan-sync-all`.
- [x] `Diagnostics` wraps `syncmyfonts-agent diagnostics`.
- [x] `Validation Report` wraps `syncmyfonts-agent validation-report`.
- [x] `Copy Validation Plan` copies a concise Mac-to-Windows and
  Windows-to-Mac clean-machine proof checklist from the native GUI.
- [x] `Verify Managed Fonts` wraps `syncmyfonts-agent verify-managed`.
- [x] `Repair Managed Fonts` wraps `syncmyfonts-agent repair-managed`.
- [x] `Install Validation Font` wraps
  `syncmyfonts-agent install-validation-font`.
- [x] `Install App Shortcuts` wraps
  `syncmyfonts-agent install-app-shortcuts`.
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
- [x] Friendly device name for UI, logs, and diagnostics.
- [x] Copyable LAN URL is shown after sharing starts.

## Must-Have Artifacts

### Shared

- [ ] Signed or otherwise trusted app bundle/executable for each platform.
- [x] Unsigned portable app-style launcher for each platform.
- [x] Bundled `syncmyfonts-agent` binary compiled for the target platform.
- [x] Per-user config file with peer URL history, LAN listen address, and local
  auto-sync preference.
- [ ] Per-user config file with server URL and server sync mode preference.
- [x] Per-user log directory.
- [x] Diagnostics output that redacts API keys and LAN keys.
- [x] Readiness and diagnostics warn when saved LAN tokens are still stored in
  the portable per-user config instead of a platform secret store.
- [x] Readiness check for local paths, saved peers, and sign-in sync helper.
- [x] Validation report that bundles diagnostics, readiness, managed font
  verification, and manual clean-machine pass criteria.
- [x] Validation report includes a Mac-to-Windows and Windows-to-Mac evidence
  matrix for before/after clean-machine testing.
- [x] A short first-run setup path for manual peer URL entry.
- [x] Native GUI shows the current first-run phase: pairing, sharing, preview,
  or sync.
- [x] Native GUI shows and can copy a LAN readiness summary with sharing,
  pairing, saved-peer, and automation state.
- [x] Native GUI shows and can copy whether saved LAN tokens exist without
  exposing the token values.
- [x] Native GUI shows a role card with what this computer and the other
  computer should do next.
- [x] Native GUI gates peer actions so pairing, preview, and install buttons
  appear in the intended LAN setup order.
- [x] Native GUI explains the selected peer's pairing state after discovery so
  users know when to enter the sharing computer's 8-digit code.
- [x] Native GUI keeps peer actions disabled until the peer URL is an absolute
  `http://` or `https://` URL.
- [x] Native GUI keeps `Pair Peer` disabled until the pairing code normalizes
  to exactly 8 digits, while accepting pasted formats like `1234-5678`.
- [x] Manually saved LAN peers get a readable fallback name if the user leaves
  the name field blank.
- [x] Saving or pairing the same normalized LAN URL updates the existing saved
  peer instead of creating duplicate repeat-sync entries.
- [x] Native GUI keeps `Get Missing Fonts From Peer` disabled until the current
  peer URL has completed a successful preview in this app session.
- [x] Native GUI relocks `Get Missing Fonts From Peer` if the peer URL or LAN
  key changes after preview.
- [x] Native GUI labels shared keys as optional so pairing codes read as the
  default first-run path.
- [x] Native GUI distinguishes receiving fonts from hosting, so Windows
  client-only sync does not look like it needs inbound firewall setup.
- [x] Native GUI keeps `Share Fonts On This Network` disabled until the listen
  address is a valid socket address and explains the expected format.
- [x] Native GUI enables Share and Stop Sharing controls according to the
  current sharing state.
- [x] Native GUI disables saved-peer loading until at least one LAN peer is
  saved.
- [x] Native GUI disables saved-peer automation setup until at least one LAN
  peer is saved, while still allowing a previously enabled auto-sync preference
  to be turned off.
- [x] Native GUI shows a concise sync receipt with installed, already-present,
  skipped, failed-peer, and checked-peer counts while keeping detailed JSON
  copyable for support.
- [x] Native GUI can copy a readable install review that explains installed
  fonts, already-present fonts, unsupported formats, system-font conflicts, and
  failed peers without requiring users to read JSON.
- [x] A copyable support report with app version, agent version, platform,
  config paths, font paths, last command, and last result.
- [x] Native GUI copy buttons for the current result and latest redacted
  support report.
- [x] Native GUI can copy the clean-machine validation plan without opening
  docs or a browser.
- [x] Native GUI can copy a LAN setup packet that bundles role, readiness,
  first-sync steps, and validation proof guidance.
- [x] Native GUI reloads the last saved action result, warning count, and
  next-step guidance after relaunch.
- [x] Native GUI copy actions for the LAN URL and pairing code leave visible,
  remembered receipts for the two-computer handoff.
- [x] Packaged release instructions and clean-machine evidence helpers use the
  exact native GUI button names for previewing and installing from a peer.
- [x] Copyable pairing instructions tell the other computer to pair, preview,
  and use `Get Missing Fonts From Peer` without copying shared-key secrets.
- [x] The visible `Copy Pairing Instructions` receipt repeats the full
  pair-preview-install path.

### macOS

- [x] App bundle with Local Network usage copy if Bonjour, discovery, or bundled
  LAN access is used.
- [x] Managed install folder:
  `~/Library/Fonts/SyncMyFonts`.
- [x] App support folder:
  `~/Library/Application Support/SyncMyFonts`.
- [x] Log folder:
  `~/Library/Logs/SyncMyFonts`.
- [x] Optional user LaunchAgent in `~/Library/LaunchAgents` for saved-peer
  sync at sign-in.
- [x] App action to open the managed font folder.
- [x] App action to open the per-user log folder.
- [x] App action to open the app support/config folder.

### Windows

- [x] User install folder:
  `%LOCALAPPDATA%\Microsoft\Windows\Fonts`.
- [x] Current-user registry registration under
  `HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts`.
- [x] App config and logs under `%LOCALAPPDATA%\SyncMyFonts`.
- [x] Start Menu shortcuts for opening SyncMyFonts, saved-peer sync, saved-peer
  preview, diagnostics, and readiness check if there is no full tray UI yet.
- [x] Optional GUI auto-sync for saved LAN peers while the app is open.
- [x] Per-user startup option through a tray app, Startup folder, `HKCU\Run`, or
  a current-user Scheduled Task.
- [x] Plain guidance for firewall prompts when Windows is intentionally hosting:
  allow Private networks only.
- [x] Readiness check detects Windows network profile state and warns when a
  hosted Windows peer appears to be on a Public network.

## Verification Gates

### Repo And Build

- [x] `cargo build` succeeds on macOS.
- [x] `cargo build` succeeds on Windows.
- [x] GitHub Actions builds/tests on macOS and Windows and produces portable
  release archives.
- [x] GitHub Actions smoke-tests the packaged agent inside each portable
  archive.
- [x] GitHub Actions verifies the packaged GUI launcher/app wrapper is present.
- [x] GitHub Actions smoke-tests packaged LAN serve/sync with isolated font
  roots, pairing code setup, and saved-peer sync.
- [x] GitHub Actions Windows package smoke uses a generated valid TrueType font
  and verifies current-user font registration after sync.
- [x] GitHub Actions smoke-tests native GUI state initialization from the
  packaged macOS and Windows GUI launchers.
- [x] GitHub Actions verifies packaged launch/readiness evidence helpers are
  present for macOS and Windows.
- [x] GitHub Actions verifies packaged GUI first-run setup includes validation
  font, LAN sharing, manual URL fallback, and preview guidance.
- [x] GitHub Actions verifies packaged GUI self-test starts with peer pairing,
  preview, and install actions locked until a LAN peer is selected and previewed.
- [x] GitHub Actions verifies packaged GUI self-test keeps shared-key labels
  optional for pairing-code-first setup.
- [x] GitHub Actions verifies packaged release instructions use the native
  `Get Missing Fonts From Peer` button name.
- [x] GitHub Actions verifies packaged GUI self-test exposes the full
  `Copy Pairing Instructions` receipt.
- [x] GitHub Actions verifies the Windows release archive uses the resolved
  Cargo package version instead of the workspace-inheritance marker.
- [x] GitHub Actions parses packaged `scan` output as JSON inventory on macOS
  and Windows after installing the OFL validation font.
- [x] GitHub Actions validation reports include managed-font registration
  health signals.
- [x] GitHub Actions verifies packaged validation reports include both
  Mac-to-Windows and Windows-to-Mac evidence rows.
- [x] GitHub Actions verifies packaged GUI self-test exposes a copyable
  clean-machine validation checklist.
- [x] macOS managed verification checks CoreText loadability for intact managed
  font files.
- [x] Manual clean-machine validation checklist exists for real macOS-to-Windows
  and Windows-to-macOS app testing.
- [ ] Clean-machine smoke tests prove the portable archives launch the native
  app on macOS and Windows.
- [x] Packaged `syncmyfonts-agent scan` returns JSON inventory on macOS.
- [x] Packaged `syncmyfonts-agent scan` returns JSON inventory on Windows.
- [x] The packaged app invokes the same agent binary or library path as the CLI.

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
- [x] Running the same LAN sync twice skips already-present fonts.
- [x] Wrong LAN key fails without exposing font manifests or blobs.
- [x] Offline peer or bad peer URL produces a visible failure and keeps local
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
- [x] Default scans exclude SyncMyFonts-managed fonts using manifest ownership.

- [x] macOS scan excludes `/System/Library/Fonts`, `/Library/Fonts`, and
  `/Network/Library/Fonts`.
- [x] macOS install does not request sudo and does not run font cache reset
  commands.
- [x] Windows install never writes to `C:\Windows\Fonts`.
- [x] Windows install never writes to `HKLM`.
- [x] Windows install writes only current-user font registry entries.
- [x] Unsupported `.woff`, `.woff2`, or unknown extensions are skipped.
- [x] Same file name with different bytes creates a deterministic suffixed file
  or another documented non-overwrite outcome.
- [x] Hash mismatch fails before final install.
- [x] Unit tests cover filename sanitization, stable hash IDs, peer URL
  normalization, and diagnostics secret redaction.

### App UX

- [x] First-run setup works with manual URL entry.
- [x] `Test Connection` checks peer health before sync.
- [x] `Dry Run` previews missing fonts without writing local font files.
- [x] The main app view shows last sync time, last result, and warning count.
- [x] The app shows "reopen your design app" guidance after successful install.
- [x] Denied macOS Local Network permission still has manual URL fallback
  guidance in the native app.
- [x] Windows client-only mode does not ask users to open an inbound firewall
  path; firewall guidance is scoped to hosted peer mode.
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
- [x] Visible secret-storage readiness warning until Keychain or Credential
  Manager integration lands.
- [x] Stable machine-readable reports for every command and error.
- [x] Dedicated diagnostics command or support-report artifact.
- [x] Local ownership manifest for managed fonts.
- [x] Explicit conflict and skipped-font review UI.
- [x] Lightweight UDP LAN peer discovery.
- [ ] Bonjour/mDNS discovery.
- [x] First pairing-code flow for exchanging a short code for a saved LAN token.
- [x] Firewall/network-profile detection for hosted Windows peer mode.
- [ ] macOS bundled-app Local Network permission testing.
- [x] Windows `WM_FONTCHANGE` notification after registry writes.
- [x] Automated LAN test fixture for saved-peer pull/install behavior.

## MVP Acceptance

The local app MVP is ready when a non-technical user can:

1. Install the app on a Mac and a Windows PC.
2. Open `Share Fonts On This Network` on the computer that has fonts.
3. Enter the shown pairing code on the other computer.
4. Run `Preview From Peer`.
5. Run `Get Missing Fonts From Peer`.
6. Repeat the flow in the other direction if needed.
7. Optionally click `Enable Sign-In Sync` after peers are saved.
8. Reopen their design app and use the synced fonts.
9. Read a concise result showing installed, skipped, and failed items.

No part of that path should require administrator rights, editing environment
variables by hand, opening system font directories, or understanding the Windows
registry.
