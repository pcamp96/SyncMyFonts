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

## Run the Local App

The agent includes a small browser-based desktop control surface:

```bash
cargo run -p syncmyfonts-agent -- app
```

The command opens the local control surface in your browser. The app can scan
fonts, discover sharing LAN peers, test a LAN peer, preview missing fonts from
a peer, install missing fonts, save LAN peers, sync all saved peers, start/stop
LAN sharing, show the copyable LAN URL for this device, and produce a redacted
diagnostics report.
When SyncMyFonts installs a font, it records that install in a local managed
font manifest so future tooling can distinguish SyncMyFonts-managed fonts from
other user-installed fonts.

For scripts or headless runs, use `syncmyfonts-agent app --no-open` and open
the printed localhost URL manually.

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

The first LAN MVP has lightweight UDP peer discovery plus manual peer URLs.
Bonjour/mDNS discovery, QR-code pairing, tray apps, and background startup
wrappers are planned next-layer app features.

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

## App Wrapper Plan

The current MVP is a cross-platform agent with a local browser control surface.
Native tray/menu wrappers should call these same commands instead of
reimplementing sync logic:

- "Share fonts on this network" -> `syncmyfonts-agent lan-serve`
- "Pull fonts from another device" -> `syncmyfonts-agent lan-sync`
- "Preview what would install" -> `syncmyfonts-agent lan-sync --dry-run`
- "Sync through my server" -> `syncmyfonts-agent sync`
- "Open control surface" -> `syncmyfonts-agent app`

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
