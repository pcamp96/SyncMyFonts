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
