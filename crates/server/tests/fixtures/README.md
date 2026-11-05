The 400 ms tone FLAC and MP3 fixtures are copied from the existing phonolite-app/packages/phonolite_local_audio/native/local_audio/tests/fixtures directory. They exercise real source decoding, Opus encoder delay/flush, cached seek preroll, and premature source EOF without a network listener or playback device.

The `formats/` fixtures are 48 kHz stereo conversions of `tone.flac`, generated
with FFmpeg 8.0 for real metadata, decoding, full-track, seek, and container-alias
tests. They are generated once and checked in; tests do not invoke FFmpeg.
Each file uses these tags where its container permits them: title `Format tone`,
artist `Format Artist`, album `Format Album`, track `1`.

Generation command (replace CODEC and OUTPUT using the table):

```sh
ffmpeg -i tone.flac -ar 48000 -c:a CODEC -metadata title="Format tone" \
  -metadata artist="Format Artist" -metadata album="Format Album" \
  -metadata track=1 formats/OUTPUT
```

| CODEC | OUTPUT |
| --- | --- |
| `aac` | `aac.m4a`, `aac.mp4`, `tone.aac` |
| `alac` | `alac.m4a`, `alac.mp4` |
| `libopus` | `opus.opus`, `opus.ogg`, `opus.oga` |
| `libvorbis` | `vorbis.ogg`, `vorbis.oga` |
| `pcm_s16le` | `tone.wav` |
| `pcm_s16be` | `tone.aif`, `tone.aiff` |

`long-opus.oga` and `long-vorbis.ogg` use the same source with `-stream_loop 3`
before `-i` and `-t 1.4` after it. These untagged 1.4-second files exercise
seeking from later Ogg pages with a newly initialized decoder.
