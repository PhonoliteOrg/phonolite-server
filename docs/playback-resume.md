# Private playback resume checkpoints

The server advertises `playback_resume: "1"` from `/server/capabilities`.
All routes below are beneath `/api`, authenticated, and derive the owner from
the token. They provide resume suggestions; they never start, pause, or replace
another device's active player.

## Checkpoint contract

`GET /account/playback` returns:

```json
{
  "revision": 12,
  "generation": "server-generated-uuid",
  "checkpoint": {
    "track_id": "track-id",
    "source": {"kind":"playlist","id":"playlist-id","artist_ids":[],"genres":[]},
    "shuffle_mode": "current_playlist",
    "shuffle_context": {"kind":"playlist","id":"playlist-id","artist_ids":[],"genres":[]},
    "deck_id": "shuffle-session-id",
    "cursor": 42
  }
}
```

An empty account has revision zero, null generation, and null checkpoint.
Accounts with only the old last-track seed receive a standalone suggestion.
Source kinds are `standalone`, `library`, `liked`, `playlist`, `album`, `artist`,
and `custom`. Playlist/album/artist sources require `id`; other kinds omit it.
Custom sources require at least one artist or genre filter, with at most 128 of
each. Filters are normalized and deduplicated. No device file paths, auth tokens,
or library titles are stored in a checkpoint.

`source` retains the origin queue. `shuffle_context` records the effective deck
selection. `shuffle_mode` retains the chosen UI preference (`off`, `all`,
`artist`, `album`, `custom`, `liked`, `current_playlist`), which can differ from
the effective context. Active shuffle requires an owned deck and a context
matching that deck; shuffle off omits `deck_id`.

`POST /account/playback` accepts:

```json
{
  "expected_revision": 12,
  "session_id": "fresh-runtime-writer-nonce",
  "sequence": 5,
  "request_id": "stable-for-this-exact-request",
  "intent": "start",
  "client_id": "playback-client-id",
  "checkpoint": {"track_id":"track-id","source":{"kind":"library"},"shuffle_mode":"off","shuffle_context":{"kind":"standalone"},"cursor":0}
}
```

The full checkpoint is committed atomically with its revision and, for `start`,
the compatible old last-track seed. A successful start generates a fresh
generation. `update` additionally requires the current `generation` at the top
level and must retain the same current track, or the exact replacement selected
by authoritative restoration after a removal. The original revision/generation
still fence that repair; it does not report a new successful track start.
Use updates for paused shuffle/source changes. Starts must point at the actual track at the deck cursor. An update may
retain a current track outside a newly selected deck; cursor zero then identifies
the first next candidate, which restoration prepares paused.

Every accepted write advances the revision. An exact retry of the latest request
from that writer returns current state without advancing it, even if its source
has since disappeared or the catalogue is temporarily unavailable. Stale revisions,
generations, sequences, and conflicting reuse return **409** with the current
snapshot shape. Refresh and resolve conflicts; never replay a stale update as a
new start. Clocks and offline playback backlogs do not order account state.

The first checkpoint referencing a live shuffle deck must provide its original
`client_id`. The server verifies account/client ownership and imports the exact
immutable deck once; it does not accept a client-uploaded list of IDs. The
durable ID equals the live session ID. Later writes and other devices under the
same account can reference that durable ID without impersonating the old client.

## Durable pages and reconciliation

`GET /account/playback/decks/:id?offset=0&limit=10` returns the same shape as live
shuffle pages: `session_id`, `offset`, `next_offset`, `total`, `has_more`, `items`,
and `item_offsets`. Limits are 1–50. Offsets address the original immutable deck;
removed catalog entries or source members leave holes. Advance by `next_offset`,
including for an empty page, and use `item_offsets` for history/cursor positions.
Foreign and missing IDs both return 404. Durable pages survive runtime session
release, two-hour live-deck expiry, and server restart.

Restoration checks current source existence and effective membership. Active
shuffle uses `shuffle_context` membership; shuffle off uses `source` membership.
An existing origin queue does not restrict an all-library shuffle to that queue.
A deleted origin source detaches a surviving current track to standalone playback
with shuffle off. A deleted shuffle context disables shuffle while retaining an
existing origin source. A removed current track advances through surviving deck
entries in their existing order, wrapping once if only earlier entries survive.
Source edits remove future members from resumed pages; added tracks enter a newly
created deck rather than silently changing the old order.

Optional `reason` values include `source_missing`, `shuffle_source_missing`,
`track_removed`, `track_missing`, `source_empty`, `deck_missing`, and
`invalid_checkpoint`. If no playable candidate survives, checkpoint is null.
Empty liked/custom sources never become the entire library. Catalogue
unavailability returns **503**, preserving raw state. Reconciliation is read-only:
GET does not erase the checkpoint or advance its revision. Playback position is
not part of this contract; restoration is paused at position zero. Repeat mode
continues to use the existing account playback settings endpoint.

## Storage, privacy, lifecycle

Checkpoints, writer fences, immutable deck metadata, and 256-ID deck chunks live
in additive tables in local `user_data.redb`. All keys use authenticated account
IDs or their collision-safe account prefix. Metadata projection always uses the
requesting account's likes and playlist flags. Runtime candidate indexes contain
public catalog data and are reused through the existing bounded shuffle worker
and index admission controls; personal collections are not cached across owners.

Retention is bounded to four durable decks and two million deck IDs per account,
with one million IDs per deck and 64 recent checkpoint writers. The current deck
is protected during cleanup. Legacy start retry history retains 128 recent
writers. Active sessions that outlive old-deck eviction can create a fresh owned
deck; the current account checkpoint remains available.

Account deletion purges checkpoints, durable decks/chunks, last-track state,
retry records, and live shuffle decks. A permanent account-ID deletion fence is
committed with personal playback cleanup, blocking already authenticated delayed
writes from recreating rows after deletion. Account UUIDs are never reassigned.
`DELETE /account/playback?expected_revision=N` clears the resume suggestion and
durable decks with the same revision conflict behavior (200 snapshot response).
It keeps writer fences and does not control active devices. A legacy successful
start atomically replaces newer context with a standalone suggestion, so an old
client cannot attach a new track to an unrelated saved playlist or shuffle deck.

The account revision vector adds `playback`. Events are invalidation hints for
idle resume UI, not remote player commands. Logical per-account privacy does not
claim encryption against the server operator or access to raw database backups.

## Verification without applications or listening sockets

Normal default builds still include the server binary. To verify only the
listener-free fixtures without replacing a possibly running user executable:

```powershell
cargo test -p server --offline --no-default-features --test playback_resume_api_test --test account_collection_sync_test --test shuffle_sessions_api_test
cargo check -p server --tests --offline
```

The optional `server-binary` feature is enabled by default. Disabling defaults
only omits the application binary, leaving these production-handler/store tests
available. Tests cover restart, exact order, compare-and-swap, generations,
paused toggles, ownership, missing sources/tracks, empty collections, catalog
unavailability, stable offsets, wrapped recovery, chunk boundaries, retention,
deletion fences, legacy compatibility, and independent revision notifications.
