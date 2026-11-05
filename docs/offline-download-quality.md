# Offline download quality

Download quality is independent of the live streaming presets. Clients can ask
the server to prepare a complete Ogg Opus file, then save and play that file
without a server connection. Original downloads retain the source bytes.

| Profile ID | Encoding |
| --- | --- |
| `original` | Unchanged source file |
| `opus_64` | Opus VBR, 64,000 bit/s target |
| `opus_96` | Opus VBR, 96,000 bit/s target |
| `opus_128` | Opus VBR, 128,000 bit/s target |
| `opus_192` | Opus VBR, 192,000 bit/s target |

The compressed profiles produce `.opus` files, with an Ogg container, 48 kHz
timing, mono/stereo audio, encoder pre-skip and exact end trimming. Encoding
uses the bundled libopus and source decoder/resampler; FFmpeg is not required.
The target bitrate is an average encoding target, not a fixed file-size cap.

## Negotiation and job creation

`GET /api/server/capabilities` includes `download_profiles`, an array containing
the supported profile IDs. Clients must negotiate compressed profiles; a failed
compressed request must never fall through to an Original download.

`POST /api/download/jobs` accepts an optional `profile` alongside the existing
`client_id`, `client_request_id` and `scope`. Omitted profiles default to
`original`. The response includes the immutable job profile and the full track
item list with offline metadata. Compressed items initially have status
`queued`; conversion happens outside the HTTP request. Poll the existing job
GET endpoint for progress, and transfer ready items incrementally.

Jobs are deduplicated by account, client, scope and profile. Reusing a request ID
with a different profile returns 409. Existing tagged JSON and positional
binary records remain readable and acquire the `original` default. Original
scope index keys are preserved so upgrades retain queued jobs.

The legacy batch and source-file endpoints remain Original-only. New compressed
downloads use the job protocol rather than extending the legacy contracts.

## Ready files and delivery

A ready compressed item contains `profile`, `rendition_id`, `download_url`,
`byte_length`, `content_type`, `etag` and `sha256`. Its authenticated URL is:

```text
/api/download/jobs/{job_id}/tracks/{track_id}/file?rendition_id={rendition_id}
```

The route checks job ownership and playlist access. Byte ranges, If-Range,
attachment filenames and checksum headers follow the existing download file
contract. It also sends `x-phonolite-download-profile` and
`x-phonolite-rendition-id`. Cache identity includes the source path, size,
modification time, profile and encoding contract version. Bump the version when
changing encoding decisions that can change the file bytes.

Incomplete files have no completion marker and cannot be served. Publication
normally uses rename; Windows filesystems which deny rename use a flushed copy
followed by the same completion marker. Canceled or interrupted temporary files
are removed, and remaining `.part` files are cleaned before the next encode.

Expired cached files return 410. Refreshing a job resets its evicted ready items
to `queued` and schedules regeneration of the same profile. Clients should
refresh and retry that rendition, preserving completed local copies.

## Resource limits and recovery

Prepared audio is stored in `download_renditions/` beside the server config.
The server admits up to 32 preparing jobs and two worker permits; cache writing
is serialized to keep completed and temporary audio within the disk budget.
Oldest cached audio is removed when space is needed. Audio preparation checks
pause/cancel/delete and server shutdown between decoding/encoding operations.

The server config exposes positive MiB limits:

```yaml
download_cache_max_mib: 1024
download_file_max_mib: 256
```

The file limit must fit within the cache limit. Larger tracks fail with an
explicit file-size-limit error; raise both limits when storing longer media.
These are server preparation limits and do not reserve space on the client.
Disk-full and encoding errors remain visible job-item failures.

Jobs and their selected profiles are durable. After restart, client job GET/list
requests resume queued preparation. A paused job remains paused until resumed.
The app keeps download preferences separate from the server job profile.

The `presentation-v2` encoder policy applies MP4 AAC/ALAC edit-list timing and
Vorbis tail trimming. Rendition identities and completion markers include the
policy, so files prepared under the previous timing rules are regenerated when
requested. Existing completed files on the client remain available. Original
downloads continue to return the unchanged source file.

## Verification

Headless tests cover all four output profiles, real source formats, 44.1/96 kHz
conversion, exact output frame counts, cancellation, migration, profile-specific
deduplication, authenticated in-process router delivery, SHA-256, ranges and
request-ID conflicts. Producer fixtures in `target/offline-download-fixtures/`
are also decoded and sought by the app's native file decoder for cross-stack
verification. No Phonolite app, network listener or server process is needed.
