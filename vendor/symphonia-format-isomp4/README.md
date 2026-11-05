# Symphonia MP4 0.5.5 presentation timing patch

Vendored from the published `symphonia-format-isomp4` 0.5.5 crate at
<https://github.com/pdeljanov/Symphonia/tree/v0.5.5/symphonia-format-isomp4>.
Upstream source notices are retained; see `LICENSE` (MPL 2.0) and
`README.upstream.md`.

The patch interprets a single, unit-rate AAC-LC or ALAC edit as the audible
presentation interval. It publishes its duration, encoder delay and padding,
trims packet timestamps into presentation time, and maps seeks back into media
time. A seek to presentation zero retains the compressed primer. AAC-LC access
units contain 1024 decoded frames even when the final sample-table entry names
only their non-padding prefix. The Phonolite decode adapters apply AAC/ALAC
packet trim because these Symphonia 0.5 decoders do not apply it internally.

Movie-clock durations are rounded to the nearest media sample; rounding within
one movie tick at the physical end is clamped to that end. Empty, repeated,
rate-changing, out-of-range or otherwise unsupported audio edit patterns fail
explicitly. Edit parsing is bounded to 64 entries; unrelated video edit lists
are not interpreted. Files without edits retain upstream raw decoding.

App and server builds carry identical copies of this patch so source-preserving
playback, streaming Opus ranges, metadata probing and durable Opus files agree.
Regression tests compare AAC against independently packaged ADTS PCM trimmed to
the declared media interval, check lossless ALAC edits, and exercise fresh and
post-EOF seeks across 12-second files with a fractional-millisecond final tail.
