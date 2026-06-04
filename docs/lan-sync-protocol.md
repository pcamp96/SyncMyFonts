# LAN Peer Sync Protocol MVP

## Purpose

This document defines a local LAN sync protocol for two SyncMyFonts installs to
sync font files directly without an external server. It is an MVP contract, not
a long-term distributed database design.

The LAN protocol should preserve the existing product safety rule:
SyncMyFonts only syncs user-installed fonts and fonts it manages itself. It must
not scan, advertise, copy, install, delete, or mutate system font directories.

## Goals

- Let two installs on the same trusted LAN exchange fonts directly.
- Keep font identity content-addressed by SHA-256.
- Avoid account setup, cloud infrastructure, and central coordination.
- Give users a simple explicit pairing flow before any peer can read manifests
  or blobs.
- Make conflicts deterministic and conservative enough for a first working
  version.
- Reuse the client safety rules from the macOS and Windows MVP documents.

## Non-Goals

- Internet relay, NAT traversal, or remote sync across networks.
- Multi-peer consensus, global revision history, or real-time collaboration.
- Automatic deletion propagation.
- System-wide font installation.
- Automatic conflict repair for fonts with the same name but different bytes.
- Long-term trust management, certificate rotation, or user accounts.

## Transport

Use HTTP over the local network for the MVP.

- Default bind address: `0.0.0.0:7370` while LAN sync is enabled.
- Default advertised URL: `http://<lan-ip>:7370`.
- All peer endpoints live under `/api/lan/v1`.
- JSON uses UTF-8 and `application/json`.
- Blob responses use `application/octet-stream`.
- Clients should set a short request timeout, for example 15 seconds for JSON
  calls and 120 seconds for blob downloads.

TLS is not required for the first LAN MVP because pairing is explicit, the
secret is high entropy, and the feature is scoped to trusted local networks.
The protocol should still be designed so HTTPS can replace HTTP later without
changing JSON bodies.

## Discovery

Discovery can be implemented in either of these MVP-compatible ways:

- Manual: user enters a peer URL shown by the other install.
- mDNS: advertise `_syncmyfonts._tcp.local` with TXT records:
  - `protocol=lan-v1`
  - `device_id=<stable-device-uuid>`
  - `device_name=<friendly-name>`
  - `platform=macos|windows`

Manual URL entry is acceptable for the first working version. mDNS is a
quality-of-life improvement, not a protocol blocker.

## Device Identity

Each install has a stable local `device_id` UUID stored in the user app support
directory. The device ID is not an account identity; it only distinguishes peers
inside manifests, logs, and pairing records.

Recommended local storage:

- macOS: `~/Library/Application Support/SyncMyFonts/lan-device.json`
- Windows: `%APPDATA%\SyncMyFonts\lan-device.json`

The public device descriptor is:

```json
{
  "device_id": "uuid",
  "device_name": "Patrick's MacBook",
  "platform": "macos",
  "protocol": "lan-v1"
}
```

## Pairing And Auth

The MVP uses explicit one-time pairing with a short human-entered code.

1. Device A enables LAN sync and chooses "pair new device."
2. Device A displays a random 8-digit numeric pairing code and keeps it valid
   for 10 minutes.
3. Device B enters Device A's URL and the pairing code.
4. Device B calls `POST /api/lan/v1/pair`.
5. Device A creates a peer record and returns a random 256-bit bearer token.
6. Device B stores the token for Device A.
7. Device B repeats the same flow in reverse if both devices should be able to
   initiate sync pulls.

All authenticated requests use:

```text
Authorization: Bearer <peer-token>
```

Tokens are scoped to one paired peer. Revoking a peer deletes that token locally.
For MVP, bearer tokens may be stored in the same local app support directory as
the manifest. A platform keychain is preferred later but not required for the
first working version.

Pairing endpoint:

`POST /api/lan/v1/pair`

Request:

```json
{
  "pairing_code": "12345678",
  "peer": {
    "device_id": "uuid",
    "device_name": "Workshop PC",
    "platform": "windows",
    "protocol": "lan-v1"
  }
}
```

Response:

```json
{
  "peer": {
    "device_id": "uuid",
    "device_name": "Patrick's MacBook",
    "platform": "macos",
    "protocol": "lan-v1"
  },
  "token": "base64url-encoded-32-random-bytes",
  "capabilities": {
    "max_blob_bytes": 104857600,
    "supports_push": false,
    "supports_delete": false
  }
}
```

Pairing requests should be rate-limited per remote IP. Five failed attempts per
10 minutes is enough for MVP.

## Endpoints

### Health

`GET /api/lan/v1/health`

Unauthenticated. Returns only basic protocol status.

```json
{
  "ok": true,
  "protocol": "lan-v1"
}
```

### Peer Info

`GET /api/lan/v1/info`

Authenticated. Returns the local device descriptor and capabilities.

```json
{
  "device": {
    "device_id": "uuid",
    "device_name": "Patrick's MacBook",
    "platform": "macos",
    "protocol": "lan-v1"
  },
  "capabilities": {
    "max_blob_bytes": 104857600,
    "supported_formats": ["otf", "ttf", "ttc", "otc"],
    "supports_push": false,
    "supports_delete": false
  }
}
```

### Manifest

`GET /api/lan/v1/manifest`

Authenticated. Returns the current local user-font inventory plus managed synced
fonts. It must exclude system fonts.

Query parameters:

- `include_managed=true|false`, default `true`

Response:

```json
{
  "schema": 1,
  "device_id": "uuid",
  "generated_at": "2026-06-04T15:00:00Z",
  "fonts": [
    {
      "content_sha256": "64-char-lowercase-hex",
      "size_bytes": 123456,
      "file_name": "Inter-Regular.ttf",
      "format": "ttf",
      "metadata_hash": "64-char-lowercase-hex",
      "family_name": "Inter",
      "style_name": "Regular",
      "postscript_name": "Inter-Regular",
      "full_name": "Inter Regular",
      "source": "user|managed",
      "install_state": "available"
    }
  ],
  "warnings": []
}
```

### Has Blob

`HEAD /api/lan/v1/blobs/{content_sha256}`

Authenticated. Returns `200 OK` if the blob is available to this peer, otherwise
`404 Not Found`.

### Download Blob

`GET /api/lan/v1/blobs/{content_sha256}`

Authenticated. Streams the exact font bytes for a hash advertised in the
manifest. The sender must verify the requested blob belongs to an advertised
user or managed font. The receiver must hash the bytes before installing.

Response headers:

- `Content-Type: application/octet-stream`
- `X-Content-Sha256: <hash>`
- `X-Content-Length: <bytes>`
- `X-SyncMyFonts-File-Name: <original-file-name>`

### Pull Plan Preview

`POST /api/lan/v1/preview-pull`

Authenticated. Optional but recommended. Lets one peer ask another peer how it
would classify a set of local hashes before downloading blobs.

Request:

```json
{
  "local_hashes": ["64-char-lowercase-hex"]
}
```

Response:

```json
{
  "missing_hashes": ["64-char-lowercase-hex"],
  "already_present_hashes": ["64-char-lowercase-hex"]
}
```

The first working version can skip this endpoint and compute the pull plan from
`GET /manifest` on the initiating device.

## Manifest Semantics

The manifest is a point-in-time inventory, not an append-only changelog.

Font identity:

- `content_sha256` is the canonical identity for deduplication.
- `metadata_hash` is advisory comparison data.
- File names and parsed font names are metadata only.

Required font fields:

- `content_sha256`
- `size_bytes`
- `file_name`
- `format`
- `metadata_hash`
- `source`
- `install_state`

Optional parsed name fields:

- `family_name`
- `style_name`
- `postscript_name`
- `full_name`

`source` values:

- `user`: a supported font in the user's font directory.
- `managed`: a font previously installed by SyncMyFonts.

`install_state` values:

- `available`: the blob can be downloaded.
- `missing`: a manifest entry exists locally but the file is missing.
- `modified`: a managed file differs from its local manifest record.

Peers must not advertise fonts from system locations:

- macOS: `/System/Library/Fonts`, `/Library/Fonts`,
  `/Network/Library/Fonts`
- Windows: `%WINDIR%\Fonts` and machine-wide `HKLM` font entries

If a platform implementation reads system font metadata for diagnostics, that
metadata must stay local and must not appear in the peer manifest.

## Pull Sync Flow

1. User pairs both devices or selects an already paired device.
2. Initiating device calls `GET /api/lan/v1/info`.
3. Initiating device calls `GET /api/lan/v1/manifest`.
4. Initiating device scans its own user and managed font locations.
5. Initiating device compares by `content_sha256`.
6. For each remote font not present locally, it checks local platform conflicts.
7. For installable, non-conflicting fonts, it downloads
   `GET /api/lan/v1/blobs/{content_sha256}`.
8. It verifies hash and size.
9. It installs using the platform-specific managed install flow.
10. It updates the local managed manifest.

The MVP should be pull-only. A "sync with peer" command pulls missing fonts from
the selected peer. Bidirectional sync is two pull operations, one initiated from
each device or one command that performs pull from A to B and then pull from B to
A using separately stored peer tokens.

## Conflict Behavior

The LAN MVP never overwrites or deletes unrelated local fonts.

Content matches:

- Same `content_sha256` already present locally: skip as already present.
- Same `content_sha256` present outside the managed folder: record as available
  locally and skip managed install.

Name conflicts:

- Different `content_sha256` with the same parsed PostScript name: skip and
  report `name-conflict`.
- Existing target path with different bytes: choose a deterministic suffixed
  destination if the platform rules allow it; otherwise skip and report
  `path-conflict`.
- Conflict with a system font identity or system font file name: skip and report
  `system-font-conflict`.

Managed-file conflicts:

- Existing managed file with same hash: no-op success.
- Existing managed file changed since the local manifest: skip and report
  `local-modified`.

Deletion behavior:

- Local deletion does not delete the font from the peer.
- Peer deletion is not represented in the LAN MVP manifest.
- A later version may add tombstones, but the first version should avoid delete
  propagation entirely.

## Acceptable First Working Version

The first working version is acceptable if it does all of the following:

- Supports manual peer URL entry.
- Supports explicit pairing with an expiring code and bearer token.
- Exposes `GET /health`, `POST /pair`, `GET /info`,
  `GET /manifest`, and `GET /blobs/{hash}` under `/api/lan/v1`.
- Pulls fonts from one paired peer to the local managed user font folder.
- Deduplicates by `content_sha256`.
- Verifies every downloaded blob before install.
- Excludes all system font locations from scans, manifests, downloads, and
  writes.
- Skips conflicts rather than overwriting.
- Prints or returns a summary of installed, skipped, and conflicted fonts.

The first working version may defer:

- mDNS discovery.
- HTTPS.
- Platform keychain storage.
- Push endpoints.
- Delete propagation.
- Incremental cursors.
- Multi-peer conflict reconciliation.
- Deep OpenType parsing beyond the parsed names already available locally.

## Error Codes

Use stable lowercase error codes in CLI summaries and JSON diagnostics:

- `unauthorized`
- `pairing-code-invalid`
- `pairing-code-expired`
- `peer-not-paired`
- `unsupported-format`
- `hash-mismatch`
- `blob-not-found`
- `name-conflict`
- `path-conflict`
- `system-font-conflict`
- `local-modified`
- `manifest-read-failed`
- `manifest-write-failed`
- `network-timeout`

## Implementation Recommendations

- Keep the LAN listener disabled by default and enable it only while the user is
  pairing or actively syncing.
- Prefer pull-only implementation until the install manifests are mature enough
  to prove ownership.
- Store peer records separately from the central-server API key config so LAN
  auth does not get confused with hosted/self-hosted auth.
- Make "exclude system fonts" an invariant at the scan layer, not just at the
  manifest serialization layer.
- Treat the peer manifest as untrusted input: validate hashes, sizes, formats,
  file names, and install paths before every write.
- Log remote peer device IDs and hashes transferred, but do not log bearer
  tokens or pairing codes.
