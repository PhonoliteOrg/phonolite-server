# Listening statistics: counter protocol v2

New clients persist monthly counters, not a listening-event history. Each account
has a permanent monthly total per immutable track-metadata snapshot; each device
has one last accepted batch sequence and hash. Listening does not depend on daily
rollovers, upload frequency, or an always-connected device.

## Storage and accounting

The app keeps pending contributions keyed by UTC year/month and metadata hash,
one frozen upload per account/device, cached server monthly counters, and private
local counters excluded from upload. Metadata snapshots are shared rather than
copied into every interval. The recorder still checkpoints consumed PCM audio
every 15 seconds and at playback boundaries, but those messages are immediately
folded into aggregates. A bounded set of 16 recent playback states preserves
qualification and retry protection across a process restart. No new rows are
created for each listening checkpoint.

Plays qualify once at the smaller of 30 seconds and half the known track duration
(30 seconds for unknown duration). Pause, resume, and seeks keep the same play;
repeat starts another. Listening duration is divided across UTC month boundaries
with integer allocation that conserves milliseconds. A play belongs to the month
in which its playback session began, including a zero-duration contribution if
it qualifies in a later month. Yearly reports sum the twelve monthly counters.

Only the primary artist receives listening time. Canonical distinct genres share
the finite duration; stored `genre_ms` allocations preserve exact rounding when
combining intervals or migrating v1 data. Current catalog genre corrections are
separate reporting projections and never rewrite frozen upload metadata. Rule
version 3 identifies the aggregate protocol; the v1 play threshold is preserved.

## API and batch lifecycle

All endpoints require bearer authentication. The authenticated user owns all data;
requests cannot choose an account. `server_id` is durable. The local account key
combines that server identity with the authenticated user ID. Device identifiers
are random installation identifiers; never reuse one for independent databases.

- `GET /stats/state?device_id=...` returns server/user identity,
  `history_generation`, collection and device preferences, `preference_revision`,
  `checkpoint: {last_sequence,last_hash,upgraded}`, and
  `months: [{year,month,revision}]`.
- `POST /stats/activate` accepts `{device_id,history_generation}` after the
  installation has reconciled all its pending v1 event IDs. It fences future v1
  writes from that device and returns its checkpoint.
- `POST /stats/batch` accepts the payload below. Each batch contains one month,
  at most 200 contributions, and at most 256 KiB of canonical JSON. Metadata is
  included once for each referenced snapshot.
- `GET /stats/snapshot?device_id=...&year=2026&month=10&after=...&revision=...`
  returns a bounded page of contributions, their shared metadata, genre reporting
  projections, the month revision, the requesting device's checkpoint, and
  `next_key`/`has_more`. The revision and checkpoint are read in one transaction.
  Omit `revision`/`after` for the first page; use its revision for remaining pages.

```json
{
  "device_id": "installation-id",
  "history_generation": 1,
  "collection_revision": 7,
  "batch_sequence": 12,
  "rule_version": 3,
  "metadata": {
    "<sha256>": {
      "track_id": "track-id", "title": "Song",
      "artist_id": "artist-id", "artist_name": "Artist",
      "album_id": "album-id", "album_title": "Album",
      "genres": ["Rock"], "artists": [], "track_duration_ms": 180000
    }
  },
  "contributions": [{
    "year": 2026, "month": 10, "metadata_id": "<sha256>",
    "listened_ms": 60000, "plays": 1, "genre_ms": {"Rock": 60000}
  }]
}
```

Snapshot IDs and the last batch hash use SHA-256 of UTF-8 JSON with recursively
sorted object keys (Unicode scalar order), preserved array order, integer numbers,
and no whitespace. The hash covers the entire immutable batch, including its
generation, consent revision, and sequence. Rust and Dart tests share a golden
metadata hash. Servers validate referenced metadata, periods, genre conservation,
size, device preference, and generation before committing anything.

The app atomically removes selected contributions from pending, saves the frozen
payload, and moves those contributions to its outstanding bucket. New playback
continues updating pending separately. Retries send the identical frozen payload.
For a device's last accepted sequence `n`, the server applies these rules:

| Received sequence | Result |
| --- | --- |
| `n + 1` | Commit every counter change, month revisions, and checkpoint atomically |
| `n`, identical hash | Acknowledge without adding anything |
| `n`, different hash | HTTP 409; preserve the existing checkpoint |
| Less than `n` | Stale acknowledgment; add nothing |
| Greater than `n + 1` | HTTP 409; no changes |

The server retains no batch payloads, batch history, accepted event IDs, or change
feed for v2. A lost reply is resolved by retrying the same sequence or reading its
checkpoint. The client checks the last hash when it matches its frozen sequence.

An acknowledgment alone does not remove the outstanding local contribution.
The app stages snapshot pages and replaces a month's cache only when all pages
are complete. A revision change returns HTTP 409 and the next attempt restarts
from the first page. Cache replacement and removal of an outstanding batch
covered by the snapshot commit in one SQLite transaction. Reports therefore use
cached account totals + outstanding contributions not yet in that cache + pending
contributions + private local totals. Partial downloads never change reports.

## Consent and deletion

`POST /stats/preferences` remains a monotonic device preference API. The app
persists a consent-intent epoch and the epoch acknowledged for each account, so
an offline off/on cycle still causes a fresh server revision even if the final
enabled value is unchanged. Every v2 batch must match the current server
`collection_revision`. Re-enabling never resets the batch sequence.

Opt-out immediately stops collection and moves unsent deltas to private local
totals. An outstanding immutable batch is marked canceled because its request
may already have committed. After a server preference barrier, the app reads the
checkpoint: an accepted batch stays outstanding until a covering snapshot; an
unaccepted batch becomes private and its sequence can safely be reused. Requests
with the old consent revision cannot commit after that barrier.
Permanently invalid or oversized batches are also retained privately after a
fresh consent fence and checkpoint reconciliation, so they cannot block later
listening. A batch with a matching accepted checkpoint is always resolved through
a covering snapshot instead.

`POST /stats/reset` still atomically increments the account's history generation
and removes its counters, metadata, legacy history, and checkpoints. Device
preferences and identities remain. Old-generation uploads cannot restore deleted
history; disconnected clients clear their old caches and queues on reconnect.
Anonymous history and its reset remain separate. Explicit anonymous import moves
aggregates and applicable genre corrections into the selected account atomically;
it does not copy listening into multiple accounts.

## Upgrade and storage growth

Deploy the server before the new app. An older server leaves new listening saved
locally and displays the existing server-update message. Server initialization
atomically converts its v1 events and qualified sessions into monthly counters.
The migration marker prevents replay on restart. During transition, newly accepted
v1 events also update v2 counters in the same transaction.
During a mixed-client transition, an old event-feed client sees only v1
contributions; `GET /stats` includes both protocols. Update every app installation
for complete cross-device reports through the new snapshots.

The app migrates its existing database into aggregate views but keeps pending v1
event IDs until the old endpoint acknowledges or rejects them. This avoids double
counting an old upload whose reply was lost. New listening uses only counters
while that queue drains. Activation then fences the old writer and lets the app
delete its raw v1 rows. Anonymous raw rows can be compacted immediately.

The server compacts an account's old events, change feed, sessions, and derived
v1 periods once every device represented in its old ledger has activated. An old
offline device can postpone that final cleanup. The account owner can deliberately
retire a lost old installation using the activation endpoint with its device ID;
that installation must be upgraded before submitting more legacy history. After
compaction, v1 reads/writes return HTTP 410 with an update message. The original
aggregate-only `stats` table is preserved as a separate baseline for `GET /stats`;
it is never uploaded or merged into the app's replicated counters.

Steady-state server rows grow with distinct account/month/metadata combinations
and devices, not listening intervals or synchronization attempts. Local pending
state grows with distinct unsynchronized month/metadata combinations, plus one
outstanding batch and bounded playback state. A test accepts 1,000 uploads for
one track/month and verifies one counter row, one shared snapshot, one checkpoint,
and no event/change rows. Deleting old rows frees database pages for reuse; it does
not promise an immediate reduction in the database file's physical size.

## Legacy v1 reference (transition only)

Older clients recorded consumed playback in a durable local ledger. Their
transition protocol used immutable events, never transport position reports.
QUIC playback messages remain accepted but do not collect statistics. This avoids
counting online listening twice and allows an installation to stop all collection.

All endpoints below require the existing account bearer authentication. The server
selects the owner from that authentication; event payloads cannot select a user.

## Synchronization

`GET /stats/sync?after=0&limit=200&device_id=<installation-id>` returns:

```json
{
  "server_id": "persistent-server-uuid",
  "user_id": "authenticated-user-id",
  "collection_enabled": true,
  "history_generation": 1,
  "device_enabled": true,
  "preference_revision": 0,
  "events": [{ "cursor": 1, "event": { "event_id": "..." } }],
  "genre_metadata": { "track-id": ["Acid Jazz", "Electronic", "Funk"] },
  "next_cursor": 1,
  "has_more": false
}
```

The event in this abbreviated example is the full event submitted by its origin
client. The installation preference fields appear when `device_id` is supplied.
Results are limited to 200 events in commit order. Persist downloaded records and
the next cursor atomically. The additive `genre_metadata` map contains current
catalog-derived genre labels for exact tracks included in this page, where
available. Persist those reporting overrides with the events and cursor; an empty
genre list means unclassified. The raw event payload remains its original snapshot.
Replayed events keep their IDs and must not enter a
client's outbound queue. Reading history and identity works while collection is
disabled. A cursor ahead of the account history returns HTTP 400.

`POST /stats/events` accepts `{device_id, history_generation, events: [...]}`.
At most 200 records may be submitted in a batch. Each event has:

- `event_id`, `session_id`, `device_id`, `track_id` (stable identifiers).
- `title`, `artist_id`, `artist_name`, `album_id`, `album_title`.
- `genres` (strings), and optionally `artists` (objects with `id` and `name`).
- `track_duration_ms`, `session_started_at_ms`, `started_at_ms`, `ended_at_ms`,
  `listened_ms`, and `rule_version` (currently `1`).
- Optional `end_reason`: `finished`, `skipped`, `stopped`, or `interrupted`.

Times are UTC milliseconds since the Unix epoch; listened duration is actual
consumed audio, excluding seeking, pauses, buffering, and loading. A zero-duration
end marker is permitted. Validation bounds identifiers and metadata, limits an
event to one hour of listening and a 24-hour interval, and allows up to five seconds
of consumed-duration tolerance for audio output drain accounting. Axum's existing
request body limit also bounds the serialized batch.

The response is `{accepted: [event_id], rejected: [{event_id, reason}]}`. Rejected
records do not prevent valid records in the same batch from being committed.
An existing identical event is acknowledged again; the same event ID with changed
contents is rejected. An event's device must match the request installation, and
all checkpoints for a session must agree on device, track, start, track duration,
and metadata snapshot.
IDs are scoped to the authenticated account.

Unique events, session counters, period aggregates, metadata, and the account
cursor commit in one durable redb transaction before acknowledgment. The change
feed stores event references rather than a second copy of each record. Period
summaries are decoded and written once per affected period per upload batch.

## Consent

`POST /stats/preferences` accepts and returns `{device_id, enabled, revision}`.
New installations default to enabled. Each installation maintains an increasing
revision; a stale enable cannot overwrite a newer disable. Stale or identical
revisions return the currently stored preference. Preferences can be updated even
when the server administrator has disabled recording.

An installation must stop recording locally immediately when switched off and
cancel queued uploads. Submit the current preference before any subsequent upload,
including after reconnecting. Disabled installations receive HTTP 403 for event
ingestion. Other installations on the same account retain their own preferences.
The server-wide `stats_collection_enabled` option independently gates all uploads.
Previously committed history remains readable. Disabling collection is not deletion.

## History reset

`POST /stats/reset` accepts `{history_generation}` and returns
`{server_id, user_id, history_generation}`. The server selects the account from
bearer authentication and atomically deletes its events, sessions, change feed,
metadata projections, period summaries, and legacy aggregates. It advances and
retains the account generation, resets the cursor to zero, and preserves server
identity and device collection preferences. Deletion is allowed even when
collection is disabled.

Send the generation obtained from the identity read immediately before reset.
Repeating the same request after its generation has advanced returns the current
generation without deleting newer history. A generation ahead of the account is
rejected with HTTP 409; zero is invalid. Serialize reset with synchronization and
ignore responses after an account/session change. Restart active listening in a
new session after reset while preserving the collection setting.

Clients read identity/generation before uploading and delete their old local rows
and pending queue when the generation advances. Delayed recording and replication
from an older generation are ignored, and stale uploads receive HTTP 409. Devices
that are offline learn the reset on reconnect; unsynced records from the previous
generation are discarded. Disabled clients still check reset metadata without
uploading listening records.

The app separately offers device-only reset, which advances its anonymous local
generation and deletes only history recorded while signed out. It works offline,
keeps all account copies and server history, and prevents late local segments from
restoring deleted records. Anonymous history is not removed by account reset and
can still be imported only with explicit consent.

## Reporting and migration

Duration is split proportionally across UTC month/year boundaries, preserving each
event's exact total. Upload time does not affect attribution. A session qualifies
once after `min(30 seconds, half the track duration)` of actual listening; unknown
duration requires 30 seconds. Qualified plays belong to the session-start period,
independent of checkpoint arrival order. Concurrent devices contribute their
consumed durations independently. Artist rankings attribute each event's duration
to its primary `artist_id`, using the matching saved artist credit for its display
name. Band members and other contributors do not receive additional copies of
that time. Without a primary ID, the primary `artist_name` is used; only records
missing both primary fields fall back to one saved credit. Genre durations divide
each event's finite listening time evenly among its distinct tags. Cumulative
integer shares conserve milliseconds across
UTC boundaries and match client daily summaries. Untagged listening remains
unclassified, so genre durations can sum to less than overall listening time.

Statistics genre snapshots prefer the matching album's genre metadata, then track
tags, then performing artist tags. The additive `listening_genres` track projection
keeps normal `genres` and shuffle filtering unchanged. Clients reading older server
responses use available cached/offline metadata at session start. Server
projections use the same selection for online and downloaded tracks.
Production credits do not supply genre tags. Empty tags are ignored and duplicate
tags are collapsed without guessing genres from audio. Existing sessions retain
their original raw metadata, including old broad tags or sessions recorded without
genre tags. Reporting can reclassify those sessions using current genre metadata
for the exact recorded track; this projection is persisted separately. Statistics
reads reconcile known history with the current library, and changed projections
atomically rebuild only that account's derived genre totals in bounded batches.
Removed tracks retain their last known reporting labels or their original tags
when no projection exists. A one-time startup migration rebuilds only derived
genre durations; events, other totals, preferences and replication cursors remain
unchanged. Legacy
server aggregates without raw events keep their historical accounting.

A separate one-time startup migration rebuilds only derived primary artist
durations and their display names from the saved event snapshots. It commits
atomically with its policy marker and uses bounded batches. Raw events, sessions,
other totals, preferences, account generations and replication cursors remain
unchanged. Artist names unrelated to the event-based rankings and aggregate-only
legacy history remain available.

Artist and genre ranking items include exact `listening_ms` alongside the existing
whole-minute `minutes` field. Charts use the exact durations so short listens stay
visible; older responses can still fall back to `minutes`. Reports also include
`other_genre_listening_ms` for labels outside the top five and
`unclassified_listening_ms` for listening without known genres. Together with the
top-five genre durations these account for the full event-based listening total.
Clients display seconds for the total as well as ranking durations; whole-second
display rounding can leave a difference of a few seconds between visible labels.
Unique tracks include brief listens, while total plays include only sessions that
reach the qualification threshold described above.

Opening or refreshing client statistics repairs at most 50 historical track
identities, with at most four concurrent metadata requests. Corrected projections
persist locally and do not require another lookup on every playback checkpoint.
An in-memory cursor advances beyond missing tracks and wraps for retries. Account,
generation, and connection guards discard stale results after an identity change.
An exact saved album identity can supply genres when its track has been removed;
unresolvable records retain saved labels. Explicit local-history import preserves
local genre corrections unless the destination already has its own projection.

Outside an explicit reset, raw events, deduplication IDs, snapshots, and the
persistent server identity are retained. Track and artist names remain available
after a library item is removed.
The identity belongs to the stats database: copies/restores of that database retain
the identity. Account generations start at 1; mismatched upload generations return
HTTP 409. Account reset deletes historical rows while retaining the newer
generation marker so disconnected clients cannot restore them.

`GET /stats` preserves the existing response shape and merges the original
aggregate table as a legacy baseline with new canonical totals. Those historical
aggregates cannot be reconstructed into deduplicated events and are never replayed
through `/stats/sync`. Consequently, new event-based client reports begin with the
new ledger; earlier server-only totals remain available through the legacy endpoint.
