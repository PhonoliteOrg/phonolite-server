# Server performance profiling

Profiling is opt-in. Before starting the server yourself, set `PHONOLITE_PROFILE=1`
in its environment. In PowerShell:

```powershell
$env:PHONOLITE_PROFILE = '1'
```

Then use your normal server launch command. The flag is read once per process;
restart the server after changing it. `true` and `yes` are also accepted. Remove
the variable or set it to `0` to disable profiling on the next launch.

Events use the existing logger at INFO level with the `performance` target. They
appear in the configured log directory's `all.log` and `info.log`. To extract the
records after a run, substitute your configured log directory:

```powershell
Select-String -LiteralPath '.\logs\all.log' -Pattern 'performance:' |
    ForEach-Object { $_.Line } |
    Set-Content -LiteralPath '.\profile-server.log'
```

The existing rolling logs retain at most 10,000 lines and trim to 5,000 lines;
save results after each short scenario. Profiling adds logging overhead, so compare
runs with the same logging settings. New profiling events exclude tokens,
request bodies, URL query strings, private file paths and track metadata. Existing
server logs retain their existing content.

Successful console polls (`/logs/tail`, `/logs/issues`, and `/status/library`)
are omitted from routine HTTP request/response traces and profiling completion
events. Polling the logs must not itself append a record to those logs. Failed
polls, actions, and other requests remain visible. The log console retains its
bounded row tail and sends revision tokens so unchanged polls avoid log-file
reads, full response bodies, and DOM updates.

## Timing events

All `elapsed_ms` values use a monotonic clock. HTTP timings start at the profiling
middleware; QUIC timings start when the server handles an authorized command.
They can be compared as durations without synchronizing app and server clocks.

Metadata profiling uses the same flag and logger. `metadata_source_fetch` reports
the provider (`musicbrainz` or `theaudiodb`), kind (`artist`, `album`, or
`album_artists`), outcome, and elapsed time. It includes provider pacing, retries,
and JSON decoding. Different providers can overlap, so their durations must not
be added to calculate wall time.

`metadata_local_scan` reports `source=local_tags`, force/targeted/unchanged flags,
audio/parsed/reused file counts, and successful scan timings in microseconds:
`duration_us`, `lock_wait_us`, `inventory_us`, `tags_us`, `catalog_us`,
`writer_wait_us`, `enrichment_reuse_us`, and `commit_us`. An unchanged catalog
returns after inventory, so later phases/counts are omitted and `parsed_files=0`.
The reuse phase measures reconciling fetched fields with local source snapshots;
the commit phase includes delta comparison, source/cache persistence, and the
atomic live catalog commit. Only newly parsed tag results are serialized back
into the tag cache. File I/O failures remain retryable after another change or a
forced rescan rather than being cached as permanent failures.

`metadata_artwork` separates `cache`, `legacy_cache`, and `network` sources for
logos/banners, with outcome, elapsed time, and payload/file bytes when known.
Cached file sizes are not network traffic. Network timing includes cache checks,
download, and file writing; it measures completed image bodies rather than wire
overhead. The pipeline's artwork step measures the overlapping pair's wall time.

`metadata_pipeline_stage` groups artist/album enrichment by `operation_id`. Its
`stage_ms` measures the step since the previous checkpoint: `store_attempt`,
`fetch`, `artwork` (artists), and `store`. The store step includes the catalog
update, metadata event/activity recording, and retry-history update.
`metadata_pipeline_finished` reports total elapsed time, outcome, and whether
the catalog changed. Already complete/recently attempted entries are skipped
before starting a pipeline measurement. Source events are aggregate comparisons;
they contain no entity identifiers and cannot be joined to a specific pipeline.

For a useful comparison, keep library size, enabled sources, source order, scan
limit, and timeout settings fixed. Compare a cold scan, an unchanged scan, a scan
after adding one track, and a forced reindex separately. A forced reindex
intentionally discards enrichment and retry history; ordinary incremental scans
retain them for surviving entities whose local metadata source has not changed.
Changing an album's title/year/artist identity invalidates its old enrichment
and retry keys, even when the album folder ID stays the same. A changed track
count makes MusicBrainz credit resolution eligible again while retaining the
existing credits until the replacement lookup succeeds.
MusicBrainz candidate pruning avoids detail requests that cannot improve the
chosen match, and duplicate successful detail responses are reused within one
resolution. Local collaborator tags are only read after a remote release match.
Artist logo/banner downloads overlap and still check the local asset cache first.

MusicBrainz retains the shared one-request-per-second limiter required by its
[API policy](https://musicbrainz.org/doc/MusicBrainz_API). TheAudioDB requests stay
serial within each configured provider group, but its account-specific limits
still apply; see the [official API limits](https://www.theaudiodb.com/free_music_api).
No live provider speedup is assumed from the offline tests: use the timing logs
from your own server run to measure median and tail latency.

| Event | What it measures |
| --- | --- |
| `http_response_ready` | Time for the HTTP handler to return the response, with method, route template and status. `declared_response_bytes` is the Content-Length header when present; it is not a measured transfer count. Response-body streaming, SSE lifetime and network delivery are excluded. |
| `command_received` | Baseline for an `open`, `seek` or `advance` operation. |
| `stream_registered` | A new outgoing stream and its worker have been registered; its control response is about to be queued. It does not imply the source is ready. |
| `source_prepared` | The bounded reader task has its cached range cursor and is ready to read its header; source encoding may still be pending. |
| `source_prepare_failed` | Asynchronous source setup failed. No error contents are included in this event. |
| `first_audio_frame_observed` | The QUIC loop first drained a complete encoded audio frame from the producer. Includes time spent waiting in the producer queue and event loop. |
| `first_audio_frame_submitted` | The complete first encoded audio frame was accepted by `quiche::Connection::stream_send`, including any partial sends. Headers, end-of-stream records and seek-reset markers are excluded. This is a transport submission measurement, not client receipt or audible output. |
| `range_cache_hit` | The requested position has a reusable encoded prefix in the shared rendition-range cache. |
| `range_producer_reserved` | This request reserved the single producer for a missing or failed range. |
| `range_producer_shared` | This request attached to an existing range producer. |
| `producer_failed` | The QUIC loop received a producer failure. |

Use `session_id` and `operation_id` to group server events. Open and seek events include
the client's `request_id` when supplied; seek events also include `seek_id`. IDs are
scoped to the relevant process or connection, so do not combine separate app
connections solely by their numeric request IDs. Each seek starts a fresh timing
and byte-sample baseline, including a seek to position zero.

## Communication samples

Samples appear about every five seconds while the connection is established.

`quic_stream_sample` reports bytes accepted by the QUIC stream API, including raw
audio framing, metadata and reset markers; the interval byte count and rate; calls
blocked by the QUIC send buffer/flow control; producer bytes still queued on the
server; the latest client-reported buffer duration; and whether sending is paused
because the client's buffer target has been reached. A low submitted rate during
a pause can be intentional backpressure, rather than a slow network. These samples
are per operation and reset on a new open/seek.

`quic_transport_sample` reports cumulative connection counters from quiche:
`transport_sent_bytes`, `transport_received_bytes`, `transport_lost_bytes`,
`retransmitted_stream_bytes`, `sent_packets`, and `lost_packets`. It also reports
the active path's estimated `rtt_ms` and congestion window. These byte counters
include protocol overhead and control traffic, and describe quiche's transport
activity; they do not prove that a UDP write succeeded or data was acknowledged.
Use differences between successive samples for interval comparisons. The lost
packet and retransmitted byte counters help distinguish a loss-prone connection
from slow source preparation.

For startup and seek tests, pair server stage durations with the app's profiling
timeline. Record cold and warm starts separately, test short and long seeks on the
same track, and keep mode, quality, frame duration and buffer targets constant.
Repeat each case to compare median and tail latency. Client first-frame/decode/
playback measurements are required to assess user-visible recovery; the server
alone cannot measure audible output.

## Encoded range production

Streaming uses a fixed quality rendition for both `auto` and `fixed` requests. Each canonical range is 5 seconds, independently encoded with its own delay compensation and graceful EOS/FIN. `stream` announcements include `request_id`, optional `seek_id`, `position_ms`, `range_end_ms`, `end_of_track`, and a source-version hash. The client continues a range with a new generation, without reopening the output device.

The shared cache and ephemeral ranges used when caching is disabled share 64 memory reservations of 4 MiB each (256 MiB of encoded payload, plus object/index overhead). Exhausted pinned capacity returns a generation-scoped admission error for retry instead of allocating outside that budget. At most four source producers run together. The channel and outgoing application staging each cover at most 400 ms of frames; output/native buffers and QUIC flow-control buffers are measured separately. Missing cache frames wait for their producer, while producer failure remains an error and never generates a successful truncated EOS. No future-track audio is prefetched.

These bounds are implementation limits, not measured audible-latency guarantees. Playback-device and live-network measurements should be performed by the user after rebuilding the matching app and server.

Canonical 5-second ranges bound cold-target encoding work to less than 5 seconds of source audio before the requested packet. Source seeking requests an accurate packet position, compensates measured preroll, and caps decoded preroll/fallback at 30 seconds; sources that cannot seek efficiently beyond that return a scoped error. Format probing or index scans can still add source-dependent I/O latency. These limits bound work, and are not measured end-to-end latency guarantees.
