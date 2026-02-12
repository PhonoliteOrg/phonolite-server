# Phonolite server

Self-hosted music streaming server for Jellyfin-style music folders.

Indexing uses a local `redb` database (no external services).

## Quickstart

## Setup After Cloning

1. Install Rust (stable) and ensure `cargo` is on `PATH`.
2. Initialize submodules (required for `vendor/opus`):

```powershell
git submodule update --init --recursive
```

3. On Windows, install NASM and ensure `nasm.exe` is on `PATH` (see Build Dependencies).

From `server/`:

```powershell
cargo run -p server --release
```

From the repo root:

```powershell
cargo run --manifest-path server\Cargo.toml -p server --release
```

On first run the server creates `config.yaml` next to the server binary. Open the admin console
at `/` to set the music folder (indexing starts immediately). Bind address and index path changes
still require a restart.
Relative paths in the config are resolved against the config file directory (the binary folder).

Optional override for config path:

```powershell
$env:PHONOLITE_CONFIG="C:\path\to\config.yaml"
```

The server watches `music_root` by default and will debounce file events before rescanning.

## Build Dependencies

Windows: install NASM (Netwide Assembler) and ensure `nasm.exe` is on `PATH`. It is required
by the QUIC dependency (BoringSSL) when building `quiche`.

## Authentication

On first run, open the admin console and create the initial admin user:

```
http://localhost:3000/
```

Use the admin console to add additional users.
Use `/settings` to update config and reindex the library.
Admin UI templates are loaded from the `web` folder next to the server binary.

Clients should log in via the API:

- POST `/api/v1/auth/login` with `{ "username": "...", "password": "..." }`
- Use the returned token as `Authorization: Bearer <token>` for all API calls

## Endpoints

Base path: `/api/v1`

- GET /health
- GET /library/albums/{album_id}
- GET /library/albums/{album_id}/cover
- GET /library/artists/{artist_id}/cover
- GET /library/search?query=&limit=
- GET /library/shuffle?mode=
- GET /library/playlists
- POST /library/playlists
- POST /library/playlists/{playlist_id}
- DELETE /library/playlists/{playlist_id}
- POST /library/likes/{track_id}
- DELETE /library/likes/{track_id}
- GET /browse/artists?search=&limit=&offset=
- GET /browse/artists/{artist_id}
- GET /browse/artists/{artist_id}/albums
- GET /browse/albums/{album_id}/tracks
- GET /browse/tracks/{track_id}
- GET /browse/playlists/{playlist_id}/tracks
- GET /browse/likes
- GET /stats
- GET /player/settings
- POST /player/settings
- POST /auth/login
- POST /auth/logout

## QUIC Streaming

Audio streaming now uses raw QUIC (no HTTP/3). By default the QUIC listener
binds to `port + 1` and auto-generates a self-signed TLS cert if missing.

Config keys:

- `quic_enabled` (bool)
- `quic_port` (number)
- `quic_cert_path` (string)
- `quic_key_path` (string)
- `quic_self_signed` (bool)

## Covers

```bash
curl http://localhost:3000/api/v1/library/albums/<album_id>/cover --output cover.jpg
```

## FFI codecs

The `codecs_ffi` crate provides feature-gated FFI hooks.

- `ffi-opus` (off): expects libopus sources under `vendor/opus`

Static musl build example:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build -p server --release --target x86_64-unknown-linux-musl
```

## Optional import scan tool

From `server/`:

```bash
cargo run -p tools --bin import_scan -- /path/to/music /path/to/library.redb
```
