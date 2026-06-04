# macOS Client MVP

This document defines the macOS-only behavior for the SyncMyFonts MVP. The Rust
CLI should implement these rules for local font detection, metadata hashing, and
safe installation of synced fonts. Server behavior is intentionally out of scope.

## Goals

- Detect fonts installed by the current macOS user.
- Produce stable metadata and content hashes so identical fonts deduplicate
  across devices.
- Install synced fonts without modifying system font directories.
- Avoid overwriting local user fonts unless the user explicitly asks for it.
- Surface clear CLI errors for unsupported, conflicting, or unsafe operations.

## Supported Font Locations

The MVP should only read and write user-scoped fonts:

- User fonts: `~/Library/Fonts`
- Synced fonts managed by this app: `~/Library/Fonts/SyncMyFonts`

The client may read system fonts for conflict diagnostics later, but the MVP
should not sync, copy, delete, overwrite, or install anything in:

- `/Library/Fonts`
- `/System/Library/Fonts`
- `/Network/Library/Fonts`

## Supported File Types

The MVP should accept these font file extensions, case-insensitively:

- `.otf`
- `.ttf`
- `.ttc`

The MVP should ignore these by default:

- macOS metadata files, including `.DS_Store`
- hidden files and directories
- partial downloads or temporary files
- unsupported font formats, including `.dfont`, `.woff`, and `.woff2`

Unsupported files should not fail an inventory scan. They should be reported only
when the user asks for verbose diagnostics.

## Font Detection

Inventory should scan `~/Library/Fonts` recursively, excluding
`~/Library/Fonts/SyncMyFonts` unless the command explicitly asks to inspect
managed synced fonts.

For every supported file, collect:

- Absolute path
- File name
- File extension normalized to lowercase
- File size in bytes
- Last modified timestamp
- SHA-256 content hash of the full file bytes
- Optional parsed font names, if the implementation includes a font parser

The content hash is the canonical identity for deduplication. File name, display
name, and modification time are metadata only.

## Metadata Hashing

Each inventory item should include two hashes:

- `content_hash`: SHA-256 over the exact font file bytes.
- `metadata_hash`: SHA-256 over canonical JSON metadata.

Canonical metadata JSON should include only stable fields:

```json
{
  "content_hash": "sha256 hex",
  "file_size": 12345,
  "font_family": "optional parsed family name",
  "font_full_name": "optional parsed full name",
  "postscript_name": "optional parsed PostScript name",
  "format": "otf|ttf|ttc"
}
```

Do not include local path, file name, modification time, machine ID, username, or
install state in `metadata_hash`. JSON keys should be serialized in sorted order,
with absent optional values encoded as `null` or omitted consistently across all
clients. Pick one convention and keep it stable.

## Local Manifest

The client should maintain a local manifest at:

`~/Library/Application Support/SyncMyFonts/manifest.json`

The manifest should record fonts installed by SyncMyFonts, not every user font.
Each record should include:

- `content_hash`
- `metadata_hash`
- installed file path
- original server or sync identifier, if provided by the main agent
- install timestamp
- last verified timestamp

The manifest is advisory. If a manifest entry points at a missing file, the
client should mark it missing and continue. If a managed file exists but its
content hash no longer matches, the client should treat it as locally modified
and refuse to overwrite it without an explicit force option.

## Safe Installation

Synced fonts should install into:

`~/Library/Fonts/SyncMyFonts`

Install steps:

1. Create `~/Library/Fonts/SyncMyFonts` if needed with user-only ownership.
2. Download or receive the font into a temporary file under
   `~/Library/Application Support/SyncMyFonts/tmp`.
3. Hash the temporary file and verify it matches the expected `content_hash`.
4. Validate that the extension is supported.
5. Choose a deterministic managed file name:
   `<safe-postscript-or-family-name>-<first-12-content-hash-chars>.<ext>`.
6. Write the final file by atomic rename from the temporary file.
7. Update `manifest.json` after the final file exists.

If no parsed font name is available, use:

`font-<first-12-content-hash-chars>.<ext>`

The safe name should contain only ASCII letters, digits, `.`, `_`, and `-`.
Collapse all other characters to `-`.

## Conflict Handling

Before installing a synced font, the client should check for conflicts in:

- `~/Library/Fonts`
- `~/Library/Fonts/SyncMyFonts`

Conflict rules:

- Same `content_hash` already installed in `SyncMyFonts`: no-op success.
- Same `content_hash` already present elsewhere in `~/Library/Fonts`: record as
  available locally and skip managed install unless the user asks to import it.
- Different `content_hash` but same parsed PostScript name: fail with a
  `name-conflict` error.
- Existing target path with different hash: fail with a `path-conflict` error.
- Existing managed file changed since manifest: fail with a `local-modified`
  error.

The MVP should not delete or disable conflicting local fonts. It should tell the
user which local path caused the conflict.

## macOS Font Cache Behavior

Installing into `~/Library/Fonts` normally makes fonts available without
administrator privileges. The MVP should not run `atsutil`, kill system font
services, request sudo, or modify system caches automatically.

After installation, the CLI should print a short message that some apps may need
to be restarted before seeing the new font.

## CLI Behavior Contract

The exact command names are up to the main agent, but macOS operations should
have these outcomes:

- Inventory: returns a JSON list of user-installed supported fonts.
- Verify: checks managed manifest entries and reports missing or modified files.
- Install: installs one or more synced fonts into the managed user font folder.
- Dry run: reports planned installs, skips, and conflicts without writing files.

All write operations should support a dry-run mode.

All write operations should fail closed if:

- The expected content hash is missing.
- The downloaded or staged file hash does not match the expected hash.
- The destination path escapes `~/Library/Fonts/SyncMyFonts`.
- A conflicting local font is detected.
- The manifest cannot be updated after the install.

## MVP Error Names

Use stable error names so UI or server-side orchestration can react to them:

- `unsupported-format`
- `hash-mismatch`
- `name-conflict`
- `path-conflict`
- `local-modified`
- `manifest-write-failed`
- `unsafe-path`
- `permission-denied`

Human-readable error text should include the affected path when applicable.

## Non-Goals

The MVP does not need to:

- Install system-wide fonts.
- Uninstall local user fonts that were not installed by SyncMyFonts.
- Resolve font name conflicts automatically.
- Parse every font table perfectly.
- Support web fonts.
- Manage font activation state outside macOS's normal user font folder behavior.
