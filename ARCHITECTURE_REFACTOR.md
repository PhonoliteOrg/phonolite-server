# Server architecture refactoring log

## Scope and baseline

This refactor follows the supplied architecture guideline: improve cohesion, encapsulation, and dependency direction while preserving observable behavior. The working tree already contains indexing, metadata, streaming, and console changes; those are the baseline and must be preserved. No applications, network listeners, or servers are started for verification.

The existing workspace tests pass before structural changes (`cargo test --offline --workspace`). Compilation is part of that run. Baseline Clippy is also recorded before implementation; existing warnings will be distinguished from introduced warnings.

## Findings and ordered plan (recorded before implementation)

| Order | Issue and affected files | Boundary / planned change | Risk and invariants |
| --- | --- | --- | --- |
| 1 | `server/utils.rs` mixes response transport, console presentation, asset discovery, formatting, and process restart. API authentication imports token parsing from `admin/mod.rs`. | Give HTTP responses/session headers a shared transport owner; move console rendering into admin presentation and process restart into process control. Update consumers to depend on the actual capability. | Low. Preserve response status/headers, cookie/token precedence, escaping, template lookup order, and restart behavior. |
| 2 | `library/lib.rs` combines redb schema, read queries, cross-index metadata transactions, normalization, catalog assembly, and filesystem indexing; `indexing.rs` reaches into its parent through a wildcard import. | Keep the public `Library` facade; extract private storage, queries, enrichment, normalization, and indexing/catalog modules with explicit imports. | Medium. Preserve schema/version, serialization layout, cursor and index keys, atomic writes, existing incremental-scan changes, and public signatures. Add characterization tests before moving code. |
| 3 | `api/library.rs` mixes search/SSE with offline metadata, checksum/range/file handling and download routes. `download_jobs.rs` persists an API-owned DTO; v2 routes depend on v1 route internals. | Give offline metadata a stable contract owner and file-download primitives their own module. Keep v1/v2 handlers at the transport boundary with narrow compatibility reexports where useful. | Medium. Preserve schema version, JSON fields, checksum invalidation, ETags, byte ranges, authentication, and handler paths. Characterize byte/serialization behavior first. |
| 4 | `admin/library.rs` mixes authenticated handlers with HTML composition; Activity/Settings reach through it for status presentation. Settings validation is embedded in a handler. | Separate Library presentation and shared status views from handlers; extract pure settings validation with explicit form/config inputs. | Low–medium. Preserve full-page/fragment parity, form names, validation order/messages, independent save behaviors, and scan triggers. |
| 5 | `external.rs` and `musicbrainz_album_artists.rs` duplicate MusicBrainz transport/retry/decoding. Admin metadata-source configuration depends on `scan.rs`. | Share a concrete MusicBrainz transport function with explicit client/user-agent/timeout inputs; move source-configuration parsing to its own capability. Keep provider-specific behavior separate. | Low–medium. Preserve retry pacing, source precedence, user-agent/timeout behavior, and scan ordering. Use pure/in-process tests, with no provider calls. |
| 6 | Source-contract tests assume implementation bodies remain in monolithic files. | Update source lists to the new owners without deleting assertions; retain runtime tests and add focused tests around extracted boundaries. Run workspace tests, build, formatting, and Clippy after integration. | Low. Architectural moves must not make tests silently stop checking a responsibility. |

## Boundaries to preserve

- `common` owns shared music concepts; `metadata` reads tags; `library` owns catalog persistence and indexing. Server transports use their capabilities rather than copying storage schemas.
- HTTP API and admin presentation are peers. Neither should own transport primitives or persisted download contracts needed by the other.
- Provider transport accepts explicit inputs. Application orchestration decides when enrichment runs; source configuration does not belong to the scan scheduler.
- Keep coherent cross-index transactions together. Introduce no repository/service traits merely to wrap existing concrete implementations.

## Intentionally deferred

QUIC connection state, transcoder framing/seek behavior, and scan scheduling are riskier stateful subsystems. They are reviewed but will not be rewritten merely to reduce file length. Shared `AppState`, global pacing/checksum/scan coordination, and the public database adapter remain compatibility constraints unless a specific safe boundary change is justified. Further splitting of cohesive transactions or small modules would add indirection without improving independence.

## Implementation and validation log

- **Baseline:** all 109 workspace tests passed. `cargo clippy --offline --workspace --all-targets` completed with existing warnings (server: 86 binary / 83 test warnings). No listener, application, or provider request was started.
- **Characterization before structural moves:** six new catalog tests passed alongside the existing twelve. Seventeen new server tests passed alongside the existing sixty-six, covering HTTP negotiation/redirects, session precedence/cookies, rendering, settings validation, offline metadata serialization, checksum invalidation/ranges, library status, provider configuration, and error formatting. Settings validation was first isolated in its existing file to test its rules independently.
- **HTTP / console / process boundary:** `utils.rs` responsibilities moved to `http.rs`, `admin/presentation.rs`, and `process_control.rs`. HTTP authentication no longer needs an admin-owned parser; presentation keeps escaping and template composition together; process control owns restart and shutdown signals. Cookie, response, shell-rendering, and formatting tests move with their owners. All consumers now import these capability modules directly, and `utils.rs` is removed.
- **Settings validation:** `admin/settings/validation.rs` accepts a form and an immutable configuration snapshot, returning a validated copy or the existing error text. The authenticated handler keeps persistence and logging side effects. Tests cover validation order, limits, checkbox semantics, source-derived flags, unchanged non-form settings, and absence of input mutation.
- **Catalog ownership:** `library/storage.rs`, `queries.rs`, `enrichment.rs`, `normalization.rs`, and `indexing/catalog.rs` now own their respective responsibilities behind the existing `Library` facade. Indexing uses explicit imports; cross-index updates remain atomic. Schema version 10, key encoding, serialized records, and the existing incremental scan implementation are preserved. All 18 catalog tests pass after extraction; source-contract assertions now read their precise owning files.
- **Metadata sources and provider transport:** `metadata_sources.rs` owns credential parsing and settings-to-provider conversion, removing administration's configuration dependency on scan orchestration. `musicbrainz_http.rs` shares request retries and decoding between metadata enrichment and album-artist resolution, accepting the HTTP client, user agent, and timeout explicitly. Six attempts, backoff, global rate limiting, source order, and one-shot source testing remain unchanged. Characterization tests were moved with these responsibilities; additional tests cover retry classification/backoff and UTF-8 diagnostics. The one intentional correction prevents a panic when the 240-byte error excerpt cuts through a multibyte character; ASCII output remains identical.
- **Download contracts and transport:** `offline_metadata.rs` owns schema v4 and its builder; `library_views.rs` owns reusable artist/album/track projections. Persisted `download_jobs.rs` records no longer depend on an API route module. `download_files.rs` owns checksum caching, ETags, filename encoding, and range-error responses used by v1 and v2. `api/downloads.rs` keeps the v1 HTTP endpoints, with narrow reexports preserving router wiring. Personalization that uses application state lives in `api/views.rs`. Characterization covers JSON/bincode field order, exact range headers, checksum invalidation, and the intentionally different v1/v2 ETags.
- **Admin library composition:** `admin/library.rs` retains authenticated handlers and actions, while its private `library/view.rs` composes full pages and fragments. `admin/library_status.rs` supplies status presentation directly to Library, Settings, and Activity. Existing full-page/fragment/navigation tests moved with their owner; the new status test covers every library state and escaping. No route or template contract changed.
- **Review and contract coverage:** independent reviews compared moved HTTP/process/download functions and catalog projections with their original bodies, and checked settings assignments, validation order, cookie handling, and template paths. Source-contract tests retain their assertions and explicitly include the new owning modules; the MusicBrainz assertions now check the extracted attempt constant and backoff function. No source check was broadened into a repository-wide search that could conceal a missing owner.

## Resulting dependency boundaries

| Capability | Owner | Consumers |
| --- | --- | --- |
| Catalog schema, queries, enrichment, indexing | Private modules behind `library::Library` | Server features use the existing catalog API |
| HTTP responses and session headers | `server/http.rs` | API and admin as peers |
| Console templates and display formatting | `admin/presentation.rs` | Admin pages and assets |
| Validated settings update | `admin/settings/validation.rs` | Settings handler owns save/logging effects |
| Library page and status composition | `admin/library/view.rs`, `admin/library_status.rs` | Authenticated Library handler; Settings and Activity share status |
| Catalog projections and offline schema | `library_views.rs`, `offline_metadata.rs` | Download APIs and persisted job snapshots |
| Download file identity and checksums | `download_files.rs` | Both HTTP download versions |
| Metadata source configuration | `metadata_sources.rs` | Settings and enrichment orchestration |
| MusicBrainz retry/decoding policy | `musicbrainz_http.rs` | Provider enrichment and album-artist resolution |
| Restart and termination signals | `process_control.rs` | Startup wiring and authenticated administration |

## Remaining work and diminishing returns

`scan.rs` and `watch.rs` still depend on each other's lifecycle operations. A future change should characterize event batching and cancellation before introducing a narrow change-notification channel. QUIC playback state and transcoding have meaningful protocol boundaries, but should first gain byte-level framing, fragmented-message, seek, and backpressure tests. These are stronger next steps than merely moving their stateful loops.

`AppState` remains broad, some request DTOs still live in `state.rs`, and checksum/pacing/scan coordination retains process-global state. Per-capability inputs can be narrowed when those use cases next change; MusicBrainz pacing must remain shared. The checksum cache's retention policy and the public `Library::db()` escape hatch deserve separate compatibility decisions. Existing source-string tests are useful contract checks but should gradually be complemented by more in-process request and persistence tests.

Further splitting the atomic catalog/enrichment transactions, tightly related view composition, or small transport functions would mostly add navigation and indirection. No traits, service layers, or generic repositories were added just to reduce file length.

## Final verification

| Check | Result |
| --- | --- |
| `cargo test --offline --workspace` | 136 passed, 0 failed; up from the 109-test baseline with 27 added boundary tests |
| `node --test web/tests/*.test.mjs` | 11 passed, 0 failed |
| `cargo build --offline --workspace` | Passed |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --offline --workspace --all-targets` | Passed with existing warnings; no new warning messages versus baseline. Server binary remains at 86 warnings, server tests drop from 83 to 82. |
| `git diff --check` | Passed |

Verification used compilation, static analysis, and local/in-process tests only. No application, HTTP/QUIC server, background service, or live provider request was started. Browser/live-stream verification remains for a user-started instance. Existing working-tree console, indexing, metadata, streaming, and watcher changes were retained.
