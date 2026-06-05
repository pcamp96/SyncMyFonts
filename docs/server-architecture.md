# SyncMyFonts Server Architecture

## MVP Goal

Provide a self-hostable backend that lets authenticated clients sync font metadata and font binary blobs across devices. The server should be simple to deploy, deterministic during conflicts, and usable on a single home server without external services.

## Recommended Stack

- HTTP API: Rust with Axum.
- Metadata store: SQLite for default self-hosting, with a schema that can move to Postgres later.
- Blob store: local filesystem by default, addressed by SHA-256 content hash.
- Database access: SQLx with checked migrations.
- Auth: bearer API tokens for MVP.
- Deployment: Docker image plus Compose file with bind-mounted data directories.

## Data Model

Use immutable blob records and mutable user font records. The blob identity is content-derived; the user font identity is a stable client-generated UUID.

### Tables

`users`

- `id`: UUID primary key.
- `email`: nullable text for future UI login.
- `created_at`: timestamp.

`api_tokens`

- `id`: UUID primary key.
- `user_id`: UUID foreign key.
- `token_hash`: text, Argon2id or SHA-256 HMAC hash of the token.
- `name`: text.
- `last_used_at`: nullable timestamp.
- `created_at`: timestamp.
- `revoked_at`: nullable timestamp.

`font_blobs`

- `sha256`: text primary key.
- `size_bytes`: integer.
- `mime_type`: text.
- `storage_path`: text.
- `created_at`: timestamp.

`fonts`

- `id`: UUID primary key. Client generated.
- `user_id`: UUID foreign key.
- `blob_sha256`: text foreign key to `font_blobs.sha256`.
- `family_name`: text.
- `style_name`: text.
- `postscript_name`: nullable text.
- `full_name`: nullable text.
- `version`: nullable text.
- `weight`: nullable integer.
- `width`: nullable integer.
- `italic`: boolean.
- `format`: text, for example `otf`, `ttf`, `woff`, or `woff2`.
- `source_device_id`: nullable text.
- `client_updated_at`: timestamp supplied by the client.
- `server_updated_at`: timestamp assigned by the server.
- `revision`: integer, incremented by the server on every accepted mutation.
- `deleted_at`: nullable timestamp for tombstones.

Recommended indexes:

- `fonts(user_id, server_updated_at)`
- `fonts(user_id, deleted_at)`
- `fonts(user_id, postscript_name)`
- `font_blobs(created_at)`

## Blob Storage

Store blobs under a content-addressed layout:

```text
/data/blobs/ab/cd/abcdef...sha256
```

The upload path should verify that the received bytes match the declared SHA-256 and size before committing the metadata row. If the blob already exists, reuse it and skip rewriting the file.

For MVP, local filesystem storage is enough. Keep the blob store behind a Rust trait so S3-compatible storage can be added later:

```rust
trait BlobStore {
    async fn put_verified(&self, sha256: &str, bytes: Bytes) -> Result<BlobInfo>;
    async fn get(&self, sha256: &str) -> Result<BlobStream>;
    async fn exists(&self, sha256: &str) -> Result<bool>;
}
```

## REST API

All endpoints are under `/api/v1`. Use `Authorization: Bearer <token>`.

### Health

`GET /healthz`

Returns process health and does not require auth.

```json
{ "ok": true, "api_version": "v1" }
```

### Current User

`GET /api/v1/me`

Returns the authenticated account and server capabilities.

```json
{
  "user_id": "uuid",
  "server_time": "2026-06-04T14:00:00Z",
  "capabilities": {
    "max_blob_bytes": 104857600,
    "conflict_policy": "revision_compare_and_swap"
  }
}
```

### List Changes

`GET /api/v1/fonts?since=<server_updated_at>&include_deleted=true`

Returns all font records changed after the supplied server timestamp. If `since` is omitted, return the full active library plus tombstones newer than the server retention window.

```json
{
  "items": [
    {
      "id": "uuid",
      "blob_sha256": "hex",
      "family_name": "Inter",
      "style_name": "Regular",
      "postscript_name": "Inter-Regular",
      "format": "ttf",
      "client_updated_at": "2026-06-04T13:59:00Z",
      "server_updated_at": "2026-06-04T14:00:00Z",
      "revision": 7,
      "deleted_at": null
    }
  ],
  "next_since": "2026-06-04T14:00:00Z"
}
```

### Get One Font

`GET /api/v1/fonts/{font_id}`

Returns one font metadata record, including tombstones.

### Upsert Font Metadata

`PUT /api/v1/fonts/{font_id}`

Creates or updates a font metadata record. The client must send the last revision it observed.

```json
{
  "expected_revision": 6,
  "blob_sha256": "hex",
  "family_name": "Inter",
  "style_name": "Regular",
  "postscript_name": "Inter-Regular",
  "format": "ttf",
  "client_updated_at": "2026-06-04T13:59:00Z"
}
```

Returns `200 OK` with the new record on success. If `expected_revision` does not match, return `409 Conflict` with the current server record.

### Delete Font

`DELETE /api/v1/fonts/{font_id}`

Soft deletes the record by writing `deleted_at`, incrementing `revision`, and keeping the tombstone. Accept `expected_revision` as a query parameter or JSON body.

### Upload Blob

`POST /api/v1/blobs`

Use a raw binary body with headers:

- `Content-Type: font/ttf`, `font/otf`, or `application/octet-stream`.
- `X-Blob-Sha256: <hex>`
- `X-Blob-Size: <bytes>`

Returns:

```json
{
  "sha256": "hex",
  "size_bytes": 123456,
  "already_existed": false
}
```

### Download Blob

`GET /api/v1/blobs/{sha256}`

Streams the blob if the authenticated user has at least one non-deleted font record pointing at it. Return `404` if the blob does not exist or the user is not authorized for it.

## Sync Flow

1. Client calls `GET /api/v1/fonts?since=<last_sync_cursor>&include_deleted=true`.
2. Client downloads missing blobs via `GET /api/v1/blobs/{sha256}`.
3. Client uploads new local blobs via `POST /api/v1/blobs`.
4. Client upserts metadata with `PUT /api/v1/fonts/{font_id}` and `expected_revision`.
5. Client stores `next_since` after all local changes are accepted.

## Conflict Semantics

Use server revisions as the primary conflict mechanism. Wall-clock timestamps are metadata only and must not decide write acceptance.

- Create: accepted when `font_id` does not exist and `expected_revision` is `0` or omitted with an explicit `create=true`.
- Update: accepted only when `expected_revision` equals the current server `revision`.
- Delete: accepted only when `expected_revision` equals the current server `revision`.
- Conflict: return `409 Conflict` with the current server record and no mutation.
- Tombstones: deletes win only when their revision is accepted. A later accepted update may undelete by setting `deleted_at = null`.

This keeps the server deterministic and lets clients decide whether to overwrite, duplicate, or merge after a conflict.

## Docker Deployment

The MVP should run as one container with two mounted data paths:

```yaml
services:
  syncmyfonts:
    image: syncmyfonts/server:latest
    ports:
      - "8080:8080"
    environment:
      SYNC_FONT_BIND: "0.0.0.0:8080"
      SYNC_FONT_DATABASE_URL: "sqlite:///data/db/syncmyfonts.sqlite"
      SYNC_FONT_BLOB_DIR: "/data/blobs"
      SYNC_FONT_TOKEN_BOOTSTRAP: "change-me-on-first-run"
      SYNC_FONT_MAX_BLOB_BYTES: "104857600"
    volumes:
      - ./data/db:/data/db
      - ./data/blobs:/data/blobs
```

Startup should run database migrations automatically. The bootstrap token should create the first API token only when no users exist, then log a warning until the operator rotates it.

## Implementation Notes

- Validate font blobs enough to reject obviously unsupported formats, but avoid deep parsing in the first server milestone.
- Keep all writes transactional: blob metadata insert and font metadata upsert should not leave dangling records.
- Use atomic file writes for blob uploads: write to a temp file, verify hash, then rename into place.
- Enforce per-request body limits before reading the upload body into memory.
- Return stable error envelopes:

```json
{
  "error": {
    "code": "revision_conflict",
    "message": "Font record has changed on the server."
  }
}
```

## MVP Milestones

1. Axum server, health route, config loading, Docker image.
2. SQLite migrations for users, tokens, blobs, and fonts.
3. Bearer token auth and bootstrap-token first-user flow.
4. Blob upload/download with SHA-256 verification.
5. Font metadata CRUD with revision conflict handling.
6. Incremental sync endpoint with tombstone support.
7. Compose deployment doc and basic integration tests.
