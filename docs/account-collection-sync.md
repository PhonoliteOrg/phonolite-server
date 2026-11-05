# Account collection synchronization

The server advertises `account_collections_sync: "1"` and
`atomic_playlist_tracks: "1"` from `/api/server/capabilities`. Authentication
selects the account for every route below; caller-supplied account IDs are ignored.

## Establishing and recovering a subscription

1. Open authenticated `GET /api/account/collections/events`.
2. Wait for the `ready` event. The server has already subscribed at this point.
3. Fetch `GET /api/account/collections/snapshot` while buffering notifications.
4. Apply the snapshot atomically in the client. Fetch again if any observed
   revision is newer than the applied snapshot.

The SSE event name is `collections`; JSON `data` contains `kind` and `revision`.
Kinds are `ready`, `collections_changed`, and `revision`. The event ID is also
the revision. These notifications invalidate state; they are not a delta/replay
log, and a `Last-Event-ID` does not replace snapshot reconciliation.

Snapshot responses contain `user_id`, `revision`, `likes` (objects with
`track_id`, `liked`, and `updated_at`, including unlike tombstones), and
`playlists` (the usual playlist fields plus `revision`). All fields come from
one redb read transaction. Playlist revision zero represents an existing record
that predates revision tracking.

Every new stream, reconnect, foreground recovery, lag/overflow, or server
identity change requires a fresh snapshot. Do not persist an applied cursor
across accounts or assume a restored database has a greater revision than the
previous connection. Coalesce bursts and compare the highest revision observed;
committing writers may publish in a different order from their commit order.
Periodic `revision` events read the durable revision every 15 seconds, repairing
missed in-memory notifications without requiring an event log. The stream ends
on session invalidation or shutdown; session validity is checked on each emitted
event. Outgoing events do not count as incoming client presence activity.

## Safe playlist writes

`POST /api/library/playlists/:playlist_id/tracks` adds tracks against the current
server list. `DELETE` on that same route removes tracks. Both accept
`{"track_ids":["track-id"]}`. Repeated additions/removals are no-ops, and
simultaneous additions from different apps preserve both tracks.

Whole-list replacement/reordering through
`POST /api/library/playlists/:playlist_id` requires `expected_revision` whenever
`track_ids` is present. Missing preconditions return HTTP 428; stale versions
return HTTP 409 without making any partial changes. Refresh the playlist before
retrying a conflicting reorder. This requirement also applies to older clients;
unconditional replacement would allow them to overwrite newer clients' edits.
Field-specific name, description, and artwork operations retain their existing
behavior. Playlist responses include the current playlist revision.

The account revision and affected playlist revision are updated in the same
transaction as the data. Notifications publish only after successful commits,
even if the originating request is cancelled. Batch likes use a single transaction.
Revisions use separate tables so existing bincode records remain readable.

Collection updates do not control audio transport. Apps should update collection
views and playback flags without restarting audio; an existing play queue remains
the current listening session, and newly started playback uses current collections.

## Verification

Store and in-process HTTP/SSE tests need no listening socket or running server.
Use `cargo test -p server --test user_data_isolation_test` and focused binary unit
test filters `user_data::tests` and `api::account_sync::tests`. Real multi-device
playback checks are performed by the user with their own running apps/server.
