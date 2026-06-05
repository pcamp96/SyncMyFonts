# SyncMyFonts

SyncMyFonts is a FOSS-first MVP for syncing installed fonts across macOS and
Windows machines. The current build provides:

- A self-hostable Rust sync server.
- SQLite metadata storage.
- Content-addressed local font blob storage.
- A cross-platform Rust CLI agent for macOS and Windows.
- Docker/Compose deployment for the server.

## MVP Behavior

The sync engine treats font files as immutable blobs identified by SHA-256. If
two devices have the same font bytes, they resolve to the same library item even
if the filename differs.

Local font deletion does not delete the global library. This is deliberate:
removing a font from one computer should not surprise-remove it everywhere.

## System Font Exclusion Policy

SyncMyFonts only syncs fonts installed in the current user's font directory. It
does not scan, upload, copy, install, delete, or manage operating-system font
directories.
When installing a synced font, SyncMyFonts also skips it if the sanitized file
name conflicts with a file already present in a known system font directory.

Excluded system locations include:

- macOS: `/System/Library/Fonts`, `/Library/Fonts`, and `/Network/Library/Fonts`
- Windows: `%WINDIR%\Fonts` and machine-wide registry font entries under `HKLM`

This is both a licensing and safety rule. System fonts may have OS-specific
licenses, and mutating them can require administrator privileges or destabilize
applications. SyncMyFonts is intended for fonts the user intentionally installed
for their own design/workshop workflow.

## Run the Server

```bash
docker compose up --build
```

The server listens on `http://localhost:7368`.

Set an API key before exposing it beyond localhost:

```bash
SYNCMYFONTS_API_KEY=change-me docker compose up --build
```

## Pull the Published Container

The GitHub Actions workflow publishes multi-architecture images to GHCR:

```bash
docker pull ghcr.io/pcamp96/syncmyfonts:latest
```

Run it directly:

```bash
docker run -d \
  --name syncmyfonts \
  -p 7368:7368 \
  -e SYNCMYFONTS_API_KEY=change-me \
  -v syncmyfonts-data:/data \
  ghcr.io/pcamp96/syncmyfonts:latest
```

## Build Locally

```bash
cargo build
```

## Run the Native Desktop App

The agent includes a native cross-platform desktop GUI:

```bash
cargo run -p syncmyfonts-agent -- gui
```

The GUI can scan fonts, discover sharing LAN peers, test a LAN peer, preview
missing fonts from a peer, install missing fonts, save LAN peers, sync all saved
peers, start/stop LAN sharing, show the copyable LAN URL for this device, verify
managed font installs, and produce a redacted diagnostics report.
When SyncMyFonts installs a font, it records that install in a local managed
font manifest so future tooling can distinguish SyncMyFonts-managed fonts from
other user-installed fonts.

The browser control surface is still available for development and future
self-hosted/server-adjacent workflows:

```bash
cargo run -p syncmyfonts-agent -- app
```

## Client Commands

Scan local user fonts:

```bash
cargo run -p syncmyfonts-agent -- scan
```

Push local fonts to the server:

```bash
cargo run -p syncmyfonts-agent -- push --server http://localhost:7368
```

Sync missing server fonts onto the current machine:

```bash
cargo run -p syncmyfonts-agent -- sync --server http://localhost:7368
```

Verify SyncMyFonts-managed font installs:

```bash
cargo run -p syncmyfonts-agent -- verify-managed
```

With auth:

```bash
SYNCMYFONTS_API_KEY=change-me cargo run -p syncmyfonts-agent -- sync --server http://localhost:7368
```

## LAN Peer Sync

LAN sync lets two installs exchange user-installed fonts directly without the
Docker sync server. It is pull-only for the MVP: one device temporarily serves
its local user-font manifest and blobs, and the other device pulls anything it
is missing.

On the device that has the fonts:

```bash
SYNCMYFONTS_LAN_KEY=choose-a-shared-key \
  cargo run -p syncmyfonts-agent -- lan-serve --listen 0.0.0.0:7370
```

If you omit `SYNCMYFONTS_LAN_KEY`, `lan-serve` generates a private token and
prints an 8-digit pairing code. In the native GUI, leaving `Shared Key` blank
does the same thing and shows the pairing code in the result panel.

On the device that needs the fonts:

```bash
SYNCMYFONTS_LAN_KEY=choose-a-shared-key \
  cargo run -p syncmyfonts-agent -- lan-sync --peer http://<peer-lan-ip>:7370
```

To find peers sharing on the default LAN port:

```bash
cargo run -p syncmyfonts-agent -- lan-discover
```

For a dry run:

```bash
cargo run -p syncmyfonts-agent -- lan-sync \
  --peer http://<peer-lan-ip>:7370 \
  --lan-key choose-a-shared-key \
  --dry-run
```

The first LAN MVP has lightweight UDP peer discovery, manual peer URLs, and an
8-digit app pairing code that saves the generated LAN token for future syncs.
Bonjour/mDNS discovery, QR-code pairing, and tray/menu background sync are
planned next-layer app features.

Save a peer for repeated sync:

```bash
cargo run -p syncmyfonts-agent -- lan-add-peer \
  --name "Workshop PC" \
  --url http://<peer-lan-ip>:7370 \
  --lan-key choose-a-shared-key
```

List saved peers:

```bash
cargo run -p syncmyfonts-agent -- lan-peers
```

Pull from every saved peer:

```bash
cargo run -p syncmyfonts-agent -- lan-sync-all
```

To sync both directions today, run `lan-sync` once from each device while the
other device is serving. For example:

1. Mac runs `lan-serve`; Windows runs `lan-sync --peer http://<mac-ip>:7370`.
2. Windows runs `lan-serve`; Mac runs `lan-sync --peer http://<windows-ip>:7370`.

No port forwarding is required. The peer URL should be a LAN address reachable
inside the same local network. Windows or macOS may still ask for local network
or firewall permission when a device is serving fonts.

## App Architecture

The current MVP is one cross-platform agent binary with CLI commands, a native
desktop GUI, and a browser control surface kept for development and future
self-hosted/server-adjacent workflows. The GUI calls the same sync logic as the
CLI instead of reimplementing sync behavior:

- "Share fonts on this network" -> `syncmyfonts-agent lan-serve`
- "Pull fonts from another device" -> `syncmyfonts-agent lan-sync`
- "Preview what would install" -> `syncmyfonts-agent lan-sync --dry-run`
- "Sync through my server" -> `syncmyfonts-agent sync`
- "Open native GUI" -> `syncmyfonts-agent gui`
- "Open browser control surface" -> `syncmyfonts-agent app`

See the platform app notes in:

- `docs/macos-lan-app.md`
- `docs/windows-lan-app.md`
- `docs/release.md`

## Platform Install Paths

macOS installs synced fonts into:

```text
~/Library/Fonts/SyncMyFonts
```

Windows installs synced fonts into:

```text
%LOCALAPPDATA%\Microsoft\Windows\Fonts
```

The Windows MVP also writes the current-user registry entry through `reg.exe`.

## Roadmap

- Add a local manifest and ownership tracking.
- Add font name parsing from OpenType tables.
- Add R2/S3 blob storage adapter.
- Add Postgres metadata adapter.
- Add tray/background agents for macOS and Windows.
- Add UI for conflict review and library archive/delete.
