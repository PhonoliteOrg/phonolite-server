# Phonolite server

Self-hosted music streaming server for Jellyfin-style music folders.

Indexing uses a local `redb` database (no external services).

Genre labels use consistent title case in the web library and API views, including
existing metadata: `alternative rock` becomes `Alternative Rock`, while acronyms
such as `R&B`, `EDM`, and `IDM` retain their customary casing. Stored tags,
shuffle matching keys, and listening history identities are unchanged; no rescan
is needed for the display correction.

The artist browser and search include main album and song artists and explicitly
featured guests by default. Supporting contributors remain available through
the optional contributor filter and detailed credits. Profiles separate
**Albums** for solo headline releases, **Collab albums** for joint album billing,
**Appears on** for billed song appearances and **Other credits** for supporting roles.
Each shared album retains one canonical ID and appears once per main artist with that artist's roles
and credited songs. **Download artist** and downloaded album removal selection
cover solo and joint headline albums; guest credits do not select unrelated
tracks from the shared album.

Repeated artist tags and explicit `ARTISTS` / `ALBUMARTISTS` fields establish
ordered individual credits. Clear list and featured-artist separators also
identify collaborations. Ambiguous joining punctuation requires independent
identity evidence, so names such as Earth, Wind & Fire and Tyler, The Creator
remain intact without that evidence. Complete known names are matched before
their punctuation is interpreted, including names inside a larger artist list.
MusicBrainz can repair an unstructured combined credit when every named artist
matches its structured artist IDs and the release's track titles match in order.
Duplicate identities are collapsed, explicit artist tags take precedence, and
verified repairs persist across scans. Album and song displays use the verified
credited names and joining words, while original source tags remain intact.
Existing stored resolutions receive this display correction when read. Explicit
local credits can also remove repeated names when the whole display resolves to
those identities in the same billing order: `Prince, Prince and the Revolution`
becomes `Prince and the Revolution` when Prince and The Revolution are separately
identified. A single explicit band name and unknown portions remain intact.
TheAudioDB's primary artist alone does
not establish collaboration membership. The next
normal server startup after upgrading repairs existing catalog organization and
reparses obsolete tag caches. Shared album and song IDs remain stable, and
existing download records remain readable.

MusicBrainz search tries the full title before a base title obtained by removing
a recognized trailing edition qualifier such as `(Deluxe Expanded Edition)`.
An edition-title difference is accepted only when the complete ordered tracklist
and every structured artist identity agree. A standard edition missing bonus
tracks cannot authenticate an expanded edition. Candidate rejection details are
available in the server logs.

Album metadata diagnostics in `info.log` distinguish empty provider searches,
missing identities, artist/title mismatches, ambiguous matches, invalid provider
IDs, and verified records missing fields. Search logs include the attempted title,
candidate counts, and up to three bounded rejected identity samples. TheAudioDB
warnings distinguish a failed identity match from a verified album without a
description. MusicBrainz logs state when a verified record has no genres; this
integration supplies genres, not descriptions. Provider request failures include
the request stage, and MusicBrainz retry warnings include attempt numbers, HTTP
status when available, and retry delays. Request errors omit credential URLs.
After an album write, storage diagnostics report the actual persisted description,
genre count, and remaining missing fields. The sweep ends with the persisted
complete/total/incomplete album counts; update and attempt counts are separate.
These diagnostics use normal info/warning logging and do not require debug mode.

If a matching expanded release is unavailable from a provider, local tags can
establish its artist credits independently. For the Prince example, use separate
`ALBUMARTISTS=Prince` and `ALBUMARTISTS=The Revolution` values, and separate
`ARTISTS` values for songs billed to both. `ALBUMARTIST` and `ARTIST` may carry the
display `Prince and the Revolution`. A lone `ALBUMARTISTS` value containing that
entire phrase instead declares a single artist identity; it is not split.

Automatic biography and artwork gathering targets main and featured artists.
Configured providers enrich those profiles when the returned identity matches;
missing or ambiguous results leave available local metadata in place. Repeated
song guests are never promoted to album artists without release-level billing.
Offline metadata includes the main and featured profiles alongside full credit
data. The next normal startup repairs older album associations while preserving
existing biographies, artwork, canonical media IDs and download records.

Opt-in HTTP and QUIC performance measurements are documented in
[PERFORMANCE.md](PERFORMANCE.md). Set `PHONOLITE_PROFILE=1` before your normal launch.

For module responsibilities, dependency boundaries, and refactoring verification, see the [architecture log](ARCHITECTURE_REFACTOR.md). Console design and interaction contracts are documented in [web/README.md](web/README.md).

## Quickstart

## Setup After Cloning

1. Install Rust (stable) and ensure `cargo` is on `PATH`.
2. Initialize submodules (required for `vendor/opus`):

```bash
git submodule update --init --recursive
```

3. Install platform build prerequisites for the QUIC dependency:
   - Windows: install NASM and ensure `nasm.exe` is on `PATH`
   - Linux: install a working C/C++ toolchain for `quiche`/BoringSSL
   - If `config.yaml` lives outside the binary directory, set `PHONOLITE_WEB_ROOT` to the `web` folder location

From `phonolite-server/`:

```bash
cargo run --release
```

From the repo root:

```bash
cargo run --manifest-path phonolite-server/Cargo.toml --release
```

On first run the server creates `config.yaml` next to the server binary. Complete local
account enrollment as described below, then open `https://127.0.0.1:3002/` on the
server machine to set the music folder (indexing starts immediately). Bind address and index path changes
still require a restart.
Relative paths in the config are resolved against the config file directory (the binary folder).

Optional override for config path:

```powershell
$env:PHONOLITE_CONFIG="C:\path\to\config.yaml"
```

```bash
export PHONOLITE_CONFIG=/path/to/config.yaml
export PHONOLITE_WEB_ROOT=/path/to/phonolite-server/web
```

The server watches `music_root` by default and will debounce file events before rescanning.

## Build Dependencies

Windows: install NASM (Netwide Assembler) and ensure `nasm.exe` is on `PATH`. It is required
by the QUIC dependency (BoringSSL) when building `quiche`.

## Authentication

The network API now requires TLS 1.3. The Flutter app accepts a server IP/host,
port, username and password, with no certificate installation or fingerprint
comparison. The address step prepares the connection; sign-in verifies reachability
and the server identity together. Authentication and application traffic require
HTTPS. Plain HTTP navigation redirects to HTTPS on the same port; plain HTTP
login submissions and API writes are rejected.

On a new installation, create the initial account locally while the server is stopped:

```powershell
.\server.exe --setup-admin listener
```

The command prompts twice without echoing the password, generates the shared TLS
identity and OPAQUE setup, saves the enrollment, and exits without starting any
listeners or indexing. Set `PHONOLITE_CONFIG` first if using a custom config.
After you start the server yourself, enter its IP and HTTPS port (default `3000`)
in the app and sign in normally.

The browser admin console initially listens on the server's loopback interface:

```
https://127.0.0.1:3002/
```

`admin_port` defaults to `3002`. API, admin and QUIC ports must all be different,
even when QUIC is disabled; conflicts are rejected when saving or loading the
configuration. Change the admin port in
**Settings → Server → Admin port (HTTPS)**, save, and restart the server.

**Settings → Server → Admin bind IP** controls the listening interface, stored as
`admin_bind_addr` in `config.yaml`. Its default is `127.0.0.1`. Use a specific
server IP to bind that interface, `0.0.0.0` for all IPv4 interfaces, `::1` for IPv6
loopback, or `::` for all IPv6 interfaces. Connect using the server's IP address;
IPv6 URLs use brackets, such as `https://[fd00::10]:3002/`. `localhost` also works
with loopback or wildcard binding.

Bind address and port changes apply after restart. The current console address
keeps working until then; afterward, open the chosen server IP and admin port.
With the default binding, opening
`http://localhost:3002/` redirects to `https://localhost:3002/`, preserving the
path and query. Admin redirects also support the configured remote interface.
The API port also redirects HTTP navigation to its HTTPS address.
These HTTP connections only serve
redirects, never account forms, authentication or application data. Send credentials
and API writes directly over HTTPS; the redirect cannot protect a password or token
that a third-party client already sent in a plain HTTP request.

The browser will show a warning for the generated self-signed certificate. This
browser step is unnecessary for app sign-in. Use the admin console to add users,
or `/settings` to configure the library. Host headers must match the active admin
binding and port; forms require the matching HTTPS origin. Arbitrary DNS names
are rejected to prevent DNS rebinding.
Admin UI templates are loaded from the `web` folder next to the server binary.

### New authentication schema

This is a breaking authentication implementation for new installations. Start with
a fresh index database and create new accounts. Databases containing previous
authentication tables are rejected without modifying them. Password hashes,
accounts and sessions from older versions are not imported or converted. The old
`/auth/login` endpoint does not exist. The app reads only the new authenticated
session format in OS secure storage; it does not read, rewrite or delete old
`auth.json` files. There is no account enrollment/migration command.

Create the first account with `--setup-admin` as shown above. Create additional
accounts and reset their passwords through the local admin console. Use the new
app and server together.

The TLS private key and OPAQUE server setup are persistent server identities.
Back up the stopped database and key together after account changes. Missing
security material causes startup to fail instead of silently replacing an enrolled
server identity. A changed key is rejected by clients. Restore the original backup;
do not delete keys to repair a connection failure. To renew the self-signed
certificate while keeping the same public-key pin, stop the server and run:

```powershell
.\server.exe --renew-certificate
```

This command also exits without starting the server. Replacing an expired public
certificate with another certificate using the same private key preserves app trust.

### Protocol and security properties

The fixed `phonolite-opaque-v1` profile uses `opaque-ke` 4.0.1, Ristretto255,
3DH/SHA-512 and Argon2id (64 MiB, three passes, one lane). Two bounded JSON proof
messages go over the same TLS connection. The OPAQUE context includes the normalized
username and that connection's RFC 9266 TLS exporter, derived independently at both
ends. Before authentication, requests carry only the username and OPAQUE handshake;
the app password never
appears in a remote HTTP request. Local account creation/reset supplies a password
to the trusted server process for enrollment; it is not retained as a password hash.

After successful mutual authentication, the app pins both the TLS SubjectPublicKeyInfo
SHA-256 digest and the OPAQUE server key. API calls, SSE, artwork and downloads require
pinned HTTPS; QUIC verifies the same TLS key before sending its bearer token or any
application data. Redirects cannot move authenticated requests to another origin.
TLS 1.3/QUIC provide authenticated encryption and forward secrecy, using their native
AES-GCM/ChaCha20-Poly1305 implementations and hardware acceleration where available.
Argon2 runs only during enrollment/sign-in, away from the app UI thread; media is not
encrypted with password hashing or per-chunk public-key operations.

OPAQUE attempts expire after 90 seconds, are single-use and limited to 20 per minute
per IP and username, plus a global limit and bounded pending state. Unknown users
receive dummy OPAQUE responses. Password changes and account updates revoke sessions
atomically; active QUIC sessions recheck revocation/expiry at least once per second.
Bearer tokens have 256 bits of cryptographic randomness and only their SHA-256 digests
are stored in the server database. The app keeps tokens and pins in OS secure storage.

This requires direct TLS termination in Phonolite. A TCP/UDP forwarding proxy can
pass the encrypted connection through; a reverse proxy that terminates TLS cannot
preserve the exporter binding. Browser clients and legacy third-party clients need
their own compatible OPAQUE implementation. Strong passwords and the rate limits
still matter: OPAQUE cannot eliminate online guessing or offline guessing after a
complete server credential/setup compromise. Traffic timing/size and peer addresses
remain visible. Streaming throughput has not been benchmarked by these code checks.

Protocol references: [OPAQUE (RFC 9807)](https://www.rfc-editor.org/rfc/rfc9807.html),
[TLS exporter channel binding (RFC 9266)](https://www.rfc-editor.org/rfc/rfc9266.html).

Album metadata first resolves a MusicBrainz release using bounded title and artist
queries, then verifies its programme against locally indexed tracks. Recognized
release labels, typography, and classical catalogue/movement titles can vary;
live, remix, acoustic, and different-work distinctions remain meaningful. A
numbered incomplete programme may establish group metadata only with high,
unambiguous track coverage; it cannot authorize a credit repair. Distinct matching
release groups are rejected rather than selected by search rank or genre availability.
Verified release-group IDs are cached against the local source/track fingerprint
and used for TheAudioDB descriptions and fresh MusicBrainz genre lookups. This
allows provider billing/title differences without renaming the local edition or
artist. Missing source descriptions remain incomplete and are logged separately
from search failures. Without verified group evidence, literal artist/title
matching remains the fallback. The primary
album artist from local tags stays first, except when a verified combined display
credit is repaired into its individual artists. Track-level collaborator credits require
matching track titles and order.

Individual tracks keep their tagged artists even on compilations. Additional
MusicBrainz artist credits need independent support in local tags; a composer
or shared work title alone cannot introduce a performer. MusicBrainz supplies
recognized genres through verified detail lookups, while TheAudioDB supplies
descriptions. General MusicBrainz tags and disambiguation notes are not used as
genres or biographies.

The optional identity cache uses a separate JSON table, preserves existing media
records, and requires no index reset. Manual metadata refresh can populate it on
the existing library. Metadata identity evidence never relaxes the stricter,
complete ordered-track checks for artist credit changes.

Cold identity discovery has bounded search/detail/browse stages and follows the
shared MusicBrainz pacing and retry policy. Its first refresh may take longer
than a literal search; later refreshes reuse verified identities. Request-count
bounds are not elapsed-time deadlines because providers can require retries.

Catalogs with an obsolete credit-identity policy are rebuilt from local tags and
sidecars on their next startup or scan. Old unverified downloaded enrichment and
artist associations are removed once, then eligible metadata is fetched with the
stricter matcher. Audio files are unchanged; a full reindex is not required.
Older tag-reader caches are discarded so date, repeated-credit, and empty-tag
corrections also apply to media whose files have not changed.

Compatible clients exchange OPAQUE proofs at `/api/v1/auth/opaque/start` and
`/api/v1/auth/opaque/finish` over one TLS connection. They must authenticate its TLS
exporter before accepting the returned bearer token and server pin. Password
login endpoints are not provided.

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
- GET /browse/artists?search=&limit=&offset=&include_contributors=
- GET /browse/artists/{artist_id}
- GET /browse/artists/{artist_id}/albums
- GET /browse/artists/{artist_id}/releases
- GET /browse/albums/{album_id}/tracks
- GET /browse/tracks/{track_id}
- GET /browse/playlists/{playlist_id}/tracks
- GET /browse/likes
- GET /stats
- GET /stats/sync
- GET /stats/v2/state
- GET /stats/v2/snapshot
- POST /stats/v2/batch
- POST /stats/v2/activate
- POST /stats/events
- POST /stats/preferences
- GET /player/settings
- POST /player/settings
- POST /auth/opaque/start
- POST /auth/opaque/finish
- POST /auth/logout

Listening statistics use monthly counters, one retry checkpoint per device, and
consent per installation. Legacy event ingestion remains during device migration.
See [LISTENING_STATS.md](LISTENING_STATS.md) for synchronization, reporting rules,
and migration of older server-only totals.

## QUIC Streaming

Audio streaming now uses raw QUIC (no HTTP/3). By default the QUIC listener
binds to `port + 1`. HTTPS and QUIC share the persistent TLS identity generated on
initial setup. The app verifies its OPAQUE-authenticated public-key pin before
sending the streaming session token; no plaintext streaming fallback exists.

Config keys:

- `quic_enabled` (bool)
- `quic_port` (number)
- `quic_cert_path` (string)
- `quic_key_path` (string)
- `quic_self_signed` (bool)

## Covers

```bash
curl --cacert quic_cert.pem -H "Authorization: Bearer <OPAQUE session token>" https://localhost:3000/api/v1/library/albums/<album_id>/cover --output cover.jpg
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
