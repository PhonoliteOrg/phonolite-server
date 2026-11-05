# Symphonia Ogg 0.5.5 timing patch

Vendored from the published `symphonia-format-ogg` 0.5.5 crate, originally
<https://github.com/pdeljanov/Symphonia/tree/v0.5.5/symphonia-format-ogg>.
Upstream source notices are retained; see `LICENSE` (MPL 2.0) and
`README.upstream.md`.

Phonolite changes the Opus granule-to-timestamp conversion to subtract the
OpusHead pre-skip. The mapper exposes this known leading delay to the logical
stream so a short file's final padding cannot replace the pre-skip. A file whose
entire audio fits in one page gets its tail padding calculated from decoded
packet durations, and the first buffered page is stamped again once that
padding is known. The same single-page tail handling applies to Vorbis: its
final granule trims padding at the end instead of inventing a leading delay.
Buffered packets are stamped again after final-page probing for every mapper.

`src/opus_timing_tests.rs` reproduces single- and multiple-page streams, checks
their exact presentation duration and trim amounts, verifies seeking, and
checks decoding with gapless trimming disabled. The regression stream contains
21 20 ms packets (20160 decoded frames), a pre-skip of 312, and final granule
19512: its presentation duration must be 19200 frames, with 648 frames of tail
padding. The unpatched crate treats the tail padding as a leading delay and
reports the wrong timeline.

Run `cargo test -p symphonia-format-ogg` from the Phonolite workspace after
applying its Cargo patch. The server's `audio_formats_test` also exercises real
Opus file decoding and seeking with the bundled libopus decoder.
