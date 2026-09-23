# oxideav-hls

Minimal HLS VOD packet-source integration for the OxideAV framework.

The crate deliberately keeps HLS as a **source-layer concern**. It resolves a
master/media playlist, opens each segment through `oxideav-http`, and
owns the selected segment demuxer. Downstream OxideAV code sees one
continuous `PacketSource`; it does not need to know where HLS segment
boundaries occur.

## Current scope

The implementation targets completed VOD playlists:

- HTTP(S) master or media playlists;
- completed VOD/event playlists carrying `#EXT-X-ENDLIST`;
- one fixed rendition selected when the source opens;
- MPEG-TS segments and mapped fragmented MP4 segments;
- ID3-timestamped packed ADTS AAC, detected from content rather than URL suffix;
- discovery of external and embedded audio renditions and their variant groups;
- relative playlist/segment URI resolution;
- `#EXTINF`-indexed media-time seeking;
- one-segment successor readahead;
- segment and initialization-map byte ranges;
- cached initialization maps reused across segments and seeks;
- no encryption;
- no discontinuities;
- no I-frames-only playlists;
- no nested master playlists;
- no live playlist reload;
- no adaptive bitrate switching.

Unsupported shapes fail explicitly rather than being partially interpreted.

## URIs and registration

`register()` installs packet sources for:

```text
hls+http://...
hls+https://...
```

The extra `hls+` prefix makes HLS source selection explicit. Playlist and
segment transfers are delegated to `oxideav-http`, while MPEG-TS demuxing is
delegated to `oxideav-mpegts` and fMP4 demuxing to `oxideav-mp4`. Packed AAC
headers are parsed by `oxideav-aac::adts::AdtsHeader`. HLS handles the ID3 transport
anchor and packet timing; compressed ADTS frames (including their headers/CRC)
are passed unchanged to the decoder.

## Rendition selection

A master playlist chooses the highest-resolution non-I-frame rendition whose
height is at most 720 by default. Set `OXIDEAV_HLS_MAX_HEIGHT` to a positive
integer to change that ceiling. If the master contains no usable rendition at
or below the ceiling, the lowest-bandwidth playable rendition is used as a
conservative fallback.

Applications that want to present their own fixed-rendition selector can call
`inspect_hls()` first. A master inspection performs one bounded GET and returns
each non-I-frame variant with its absolute media-playlist URL, bandwidth,
resolution, frame rate, codecs and linked rendition name/group metadata, plus
the index that `open_hls()` would have chosen automatically. Opening one of the
returned media URLs then fetches that media playlist directly; it does not fetch
the master a second time. `inspect_hls()` does not open any media playlist or
segment itself.

This is fixed-rendition selection. Adaptive bitrate selection is not provided
by this crate.

### Audio rendition discovery

`HlsPlaylistInfo::Master::audio_renditions` contains every `TYPE=AUDIO`
`EXT-X-MEDIA` entry, in playlist order. Each `HlsAudioRendition` retains its
group/name, absolute optional URL, language and associated language, default and
autoselect flags, channels string and accessibility characteristics. A variant's
`audio_renditions(&audio_renditions)` iterator matches its `AUDIO` group ID.
Names are not globally unique: two groups may both have an "English" rendition.

An absent rendition URL means audio is embedded in the associated variant; it
does **not** mean an invalid or missing playlist. An external URL can be opened
with `open_hls("hls+https://...")` and routed alongside the video URL in an
OxideAV pipeline job. Discovery does not fetch rendition playlists and leaves
language/default/accessibility selection to the application. Opening the master
with `open_hls` still opens only the selected variant; it does not automatically
merge external audio. This API addition requires exhaustive `Master` patterns
to bind `audio_renditions` or include `..`.

### Segment format and packed-audio timing

The segment dispatcher probes the bytes after applying any byte range and
initialization map. MPEG-TS and mapped fMP4 use their existing demuxers. An ID3
prefix selects the packed-audio reader. Unknown formats, unmapped fMP4 and
packed audio with a map fail explicitly instead of falling through to MPEG-TS.

Packed AAC follows [RFC 8216 section 3.4](https://www.rfc-editor.org/rfc/rfc8216#section-3.4):
each segment must begin with an ID3 PRIV transport timestamp identifying its
first sample. ID3v2.3 and ID3v2.4 sizes, extended headers, unsynchronisation and
v2.4 footers are handled. Unrelated metadata is skipped, including ID3 tags
between AAC frames; missing/invalid timestamps and compressed/encrypted PRIV
frames fail explicitly. Metadata reads are bounded to 1 MiB per scan.

Packets use the shared 90 kHz transport time base. Each PTS is calculated from
the ID3 anchor plus cumulative samples at the ADTS core sample rate, avoiding
per-frame rounding drift (including 44.1 kHz). The playlist's segment position
resolves 33-bit timestamp wraparound against the first segment; seeking and
out-of-order readahead therefore use the same clock epoch. Seeking scans only
the chosen segment and lands on an AAC frame at or before the target, clamping
to its first frame when necessary. ADTS configuration changes within a segment
or between packed-audio segments are rejected because the source advertises a
fixed stream description. Channel configuration zero remains unknown until the
decoder reads the in-band Program Config Element.

Other HLS packed-audio codecs (MP3, AC-3, E-AC-3) and WebVTT segments are not
implemented. Their framing/timing adapters can be added at the segment dispatch
boundary without changes to playlist traversal or the playback pipeline.

### Validation

`cargo test` covers playlist discovery, content-based dispatch, metadata parsing,
sample timing, transport wraparound, frame seeks, and a local HTTP VOD with
byte-ranged segments and successor readahead. `cargo clippy --all-targets -- -D
warnings` checks the library and tests.

An optional real-source smoke test reads 600 packets, decodes the first 20 with
the existing AAC decoder, seeks to 60 seconds, and opens the video rendition:

```text
# Set OXIDEAV_TEST_HLS_MASTER to a fresh hls+https:// master URI with external AAC.
cargo test --test live_audio -- --ignored --nocapture
```

It produces no audible output and does not save media or signed URLs. Origins
that answer range probes with full HTTP 200 responses require the corresponding
`oxideav-http` full-response fallback. Local validation used that existing
implementation (`25e6869`) through Cargo path overrides.

## Why playlist fetches use normal GET

`oxideav-http::open_http()` is a seekable media source and normally probes the
origin with HEAD/range semantics. Real HLS origins do not universally support
that shape: Twitch's signed Usher playlist URLs, for example, accepted GET while
returning 404 to HEAD during the validation that motivated this crate.

Playlists are therefore fetched through `oxideav_http::fetch_bytes()`: a normal
GET with a hard byte limit and the same redirect/content-encoding policy as the
HTTP driver. Playlist metadata is capped at 4 MiB.

## Packet source and segment progression

Opening a media playlist builds a lightweight in-memory segment index from its
resolved segment URLs, optional byte ranges and initialization maps, and
`#EXTINF` durations. Segments are not modelled as one long byte stream.

`HlsPacketSource` owns:

- the active demuxer for the current segment;
- the stream metadata exposed to downstream consumers;
- the playlist-duration/media-time segment index;
- a small queue of packets primed during startup or successor preparation; and
- one prepared successor segment.

The first segment is opened immediately. During startup, a bounded packet prefix
is read until timestamped packets establish trustworthy stream start times.
Those packets are retained and replayed unchanged through `next_packet()`.

When the active segment reaches EOF, the next prepared segment demuxer is
installed and packet delivery continues across the HLS boundary without exposing
that boundary downstream.

For `#EXT-X-MAP`, the source fetches the referenced initialization section
(honouring its optional `BYTERANGE`) and caches it by URL and byte range.
It presents that section followed by one media segment as a seekable view to
the MP4 demuxer. The same map is reused for later segments and seeks; a new
map switches the initialization data. The cache holds at most four maps of up
to 8 MiB each. Segment `#EXT-X-BYTERANGE` is also
honoured, including implicit offsets following a range on the same resource.

## Successor readahead

Exactly one successor segment is prepared in the background.

While segment N is active, a worker for segment N+1:

1. opens the segment's HTTP resource;
2. detects the segment format and creates its demuxer;
3. validates that its stream layout matches the current rendition; and
4. primes the first packet.

At current-segment EOF, that prepared state is installed directly. This overlaps
HTTP setup and initial demux work with playback of the current segment without
increasing decoded video or PCM buffering.

A readahead failure is carried by the prepared slot and becomes an ordinary
source error only when that successor is actually required. If the worker cannot
be started, the source falls back to opening the successor synchronously when it
is needed.

Seeking invalidates the old successor state; after the landed segment is
installed, readahead begins again for that segment's successor.

## Media-time seeking

`#EXTINF` durations define the HLS media timeline. The source records each
segment's media-relative start time and the total VOD duration when the playlist
is opened.

Segment timestamps are transport timestamps rather than zero-based media time.
During first-segment priming, `HlsPacketSource` derives a transport-time origin
from the earliest trustworthy stream/packet timestamp. A requested raw PTS is
therefore mapped to media time as:

```text
media time = transport PTS time - transport origin
```

The media-relative target selects a segment directly from the `#EXTINF` index.
That segment is opened and its demuxer performs the actual timestamp
seek. For A/V renditions, the video stream is preferred for access-point
selection so the landing is video-decode-safe even when the caller addressed
the audio route.

`#EXTINF` timing is nominal and can differ slightly from access-point
timing. If the chosen segment can only land after the requested PTS, the source
retries the preceding segment so the seek contract remains the nearest
decode-safe point at or before the target.

The seek result remains in the original transport PTS domain. Applications can
therefore preserve their normal timestamp handling while using the HLS media
timeline only to choose the correct segment efficiently.

## Validation

The implementation has been exercised against:

- the public Big Buck Bunny HLS master/720p60 MPEG-TS stream; and
- signed Twitch VOD playlists containing thousands of MPEG-TS segments; and
- a YouTube VOD using mapped fMP4 H.264 segments, including a seek to 389 s.

The Twitch playlists are intentionally not committed: their signed URLs contain
short-lived tokens. Real segment media used for regressions lives separately in
local integration fixtures.

## Future work

Remaining HLS format work includes discontinuity handling, encryption/key
handling, separate audio renditions, and live playlist reload.

MIT.
