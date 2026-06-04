# Windows Client MVP

Scope: Windows client behavior only. The main agent will implement the Rust CLI; this document defines the expected MVP behavior for detecting user-installed fonts, hashing local metadata, and installing synced fonts safely on Windows.

## Goals

- Detect fonts installed for the current Windows user.
- Produce stable metadata hashes so the sync layer can compare local and remote state.
- Install synced font files without overwriting unrelated local fonts or requiring admin rights for the MVP path.
- Keep destructive behavior out of the MVP: no uninstall, replace, or global font installation unless a later design explicitly adds it.

## Non-Goals

- System-wide font inventory from `C:\Windows\Fonts`.
- Font activation without installation.
- Font conflict resolution across foundries, versions, or duplicate family names beyond deterministic skip/report behavior.
- Server API shape, auth, or database schema.

## Font Locations

For MVP, treat the user font directory as the writable source of truth:

```text
%LOCALAPPDATA%\Microsoft\Windows\Fonts
```

The client may inspect the per-user registry font table to map display names to files:

```text
HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts
```

Use system font locations only as read-only conflict context:

```text
%WINDIR%\Fonts
HKLM\Software\Microsoft\Windows NT\CurrentVersion\Fonts
```

The MVP should not write to `%WINDIR%\Fonts` or `HKLM`.

## Supported File Types

MVP supported extensions:

- `.ttf`
- `.otf`
- `.ttc`
- `.otc`

Ignore other files in the font directory. Extension matching should be case-insensitive.

## Local Detection

Detection should merge file-system and registry evidence:

1. Enumerate supported font files in `%LOCALAPPDATA%\Microsoft\Windows\Fonts`.
2. Read `HKCU\...\Fonts` values when available.
3. Resolve registry file values:
   - Absolute paths are used as-is.
   - Relative file names are resolved against the user font directory.
4. Keep entries whose resolved path exists and has a supported extension.
5. Include user font files even if no matching registry value exists, but mark registry state as missing.
6. Do not include system fonts in the synced set. If a synced candidate conflicts with a system-installed file name or font identity, report the conflict and skip install.

The detected model should preserve enough data for diagnostics:

```rust
struct LocalFont {
    path: PathBuf,
    file_name: String,
    registry_name: Option<String>,
    registry_present: bool,
    file_size: u64,
    modified_unix_ms: Option<i64>,
    content_sha256: String,
    metadata_hash: String,
}
```

## Hashing

Use two hashes with different purposes:

- `content_sha256`: SHA-256 of the full font file bytes.
- `metadata_hash`: SHA-256 of a canonical JSON document containing stable local metadata.

The MVP metadata hash should include:

```json
{
  "schema": 1,
  "file_name_lower": "example.ttf",
  "file_size": 123456,
  "content_sha256": "..."
}
```

Do not include absolute paths, Windows user names, machine names, registry display names, or modified timestamps in `metadata_hash`; those can vary between machines without changing the font identity. Keep `modified_unix_ms` as diagnostic metadata only.

Canonical JSON rules:

- UTF-8.
- Fixed key order as shown above.
- Lowercase hex SHA-256.
- Lowercase `file_name_lower` using ASCII case folding.
- No insignificant whitespace.

## Installing Synced Fonts

Install synced fonts into the per-user font directory only:

```text
%LOCALAPPDATA%\Microsoft\Windows\Fonts
```

Safe install flow:

1. Validate the incoming font payload has a supported extension and non-empty bytes.
2. Compute `content_sha256` before writing.
3. Choose a destination file name:
   - Prefer the remote file name after sanitizing path separators and reserved Windows characters.
   - If that file name exists with the same `content_sha256`, treat install as already complete.
   - If that file name exists with different content, write a deterministic suffixed name such as `Name.syncmyfonts-<hash8>.ttf`.
4. Write to a temporary file in the same directory.
5. Flush the file.
6. Atomically rename the temporary file to the final destination.
7. Register the font for the current user by adding/updating `HKCU\...\Fonts`.
8. Notify Windows font consumers with `WM_FONTCHANGE` via `SendMessageTimeoutW(HWND_BROADCAST, WM_FONTCHANGE, ...)`.
9. Re-run local detection and return the detected entry.

The registry value name can be derived from the file stem for MVP, for example:

```text
<file stem> (SyncMyFonts)
```

The registry value data should be the final file name when the file lives in the user font directory. Use an absolute path only if a future installer intentionally supports another per-user location.

## Conflict Rules

MVP behavior should be predictable and conservative:

- Same file name and same `content_sha256`: no-op success.
- Same file name and different `content_sha256`: install with a suffixed file name.
- Same registry display name and different file: keep existing entry, install with a SyncMyFonts-specific registry name.
- Conflict with system font file name or system registry identity: skip and report `SystemFontConflict`.
- Invalid or unreadable local font file: skip and include a warning in diagnostics.

Do not delete or overwrite a user font unless the file was previously installed by SyncMyFonts and a future manifest explicitly proves ownership. Ownership tracking is out of scope for MVP.

## CLI Behavior Draft

Suggested MVP commands:

```text
syncmyfonts scan --json
syncmyfonts install-font --source <path-to-font> --json
```

`scan --json` should return:

```json
{
  "platform": "windows",
  "schema": 1,
  "fonts": [],
  "warnings": []
}
```

`install-font --json` should return:

```json
{
  "installed": true,
  "already_present": false,
  "font": {},
  "warnings": []
}
```

Errors should be machine-readable with stable codes:

```json
{
  "error": {
    "code": "UnsupportedFontType",
    "message": "Only .ttf, .otf, .ttc, and .otc fonts are supported."
  }
}
```

Initial error codes:

- `UnsupportedFontType`
- `EmptyFontFile`
- `SourceReadFailed`
- `UserFontDirectoryUnavailable`
- `DestinationWriteFailed`
- `RegistryWriteFailed`
- `FontChangeNotificationFailed`
- `SystemFontConflict`

## Rust Implementation Notes

Recommended crates:

- `sha2` for SHA-256.
- `serde` and `serde_json` for JSON output.
- `windows` or `windows-sys` for registry access and `SendMessageTimeoutW`.
- `tempfile` is acceptable, but ensure the temporary file is created in the destination directory before rename.

Path and registry handling notes:

- Resolve `%LOCALAPPDATA%` via the Windows known-folder/environment APIs, not by hard-coding `C:\Users`.
- Use wide Windows APIs for paths and registry strings.
- Normalize only for comparison; preserve the actual destination path for file operations.
- Treat registry failures as install failures after the file write, but report the final file path so a later repair command can reconcile it.

## Test Matrix

Minimum local tests for the Windows implementer:

- Scan returns an empty list when the user font directory is empty or absent.
- Scan includes a `.ttf` present in the user font directory without a registry entry and marks `registry_present=false`.
- Scan resolves a relative registry value to the user font directory.
- Metadata hash is stable when only modified time changes.
- Installing an existing identical font is a no-op.
- Installing a same-name different-content font creates a suffixed file.
- Installing an unsupported extension returns `UnsupportedFontType`.
- A simulated `HKLM` conflict returns `SystemFontConflict`.
- `WM_FONTCHANGE` failure is reported without panicking.
