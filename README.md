# oxideav-hls

Minimal HLS VOD packet-source integration for the OxideAV framework.

The crate deliberately keeps HLS as a **source-layer concern**. It resolves a
master/media playlist, opens each MPEG-TS segment through `oxideav-http`, and
owns the active `oxideav-mpegts` demuxer. Downstream OxideAV code sees one
continuous `PacketSource`; it does not need to know where HLS segment
boundaries occur.

## Current scope

The implementation targets the classic VOD shape used by the
Real-world HLS VOD validation:

- HTTP(S) master or media playlists;
- completed VOD/event playlists carrying `#EXT-X-ENDLIST`;
- one fixed rendition selected when the source opens;
- whole-file MPEG-TS segments;
- relative playlist/segment URI resolution;
- `#EXTINF`-indexed media-time seeking;
- one-segment successor readahead;
- no encryption;
- no byte ranges;
- no `#EXT-X-MAP` / fMP4;
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
delegated to `oxideav-mpegts`.

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
resolved segment URLs and `#EXTINF` durations. Segment byte lengths are not
needed and segments are not modelled as one concatenated byte stream.

`HlsPacketSource` owns:

- the active MPEG-TS demuxer for the current segment;
- the stream metadata exposed to downstream consumers;
- the playlist-duration/media-time segment index;
- a small queue of packets primed during startup or successor preparation; and
- one prepared successor segment.

The first segment is opened immediately. During startup, a bounded packet prefix
is read until timestamped packets establish trustworthy stream start times.
Those packets are retained and replayed unchanged through `next_packet()`.

When the active segment reaches EOF, the next prepared MPEG-TS demuxer is
installed and packet delivery continues across the HLS boundary without exposing
that boundary downstream.

## Successor readahead

Exactly one successor segment is prepared in the background.

While segment N is active, a worker for segment N+1:

1. opens the segment's HTTP resource;
2. creates its MPEG-TS demuxer;
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

MPEG-TS timestamps are transport timestamps rather than zero-based media time.
During first-segment priming, `HlsPacketSource` derives a transport-time origin
from the earliest trustworthy stream/packet timestamp. A requested raw PTS is
therefore mapped to media time as:

```text
media time = transport PTS time - transport origin
```

The media-relative target selects a segment directly from the `#EXTINF` index.
That segment is opened and its MPEG-TS demuxer performs the actual timestamp
seek. For A/V renditions, the video stream is preferred for access-point
selection so the landing is video-decode-safe even when the caller addressed
the audio route.

`#EXTINF` timing is nominal and can differ slightly from MPEG-TS access-point
timing. If the chosen segment can only land after the requested PTS, the source
retries the preceding segment so the seek contract remains the nearest
decode-safe point at or before the target.

The seek result remains in the original transport PTS domain. Applications can
therefore preserve their normal timestamp handling while using the HLS media
timeline only to choose the correct segment efficiently.

## Validation

The implementation has been exercised against:

- the public Big Buck Bunny HLS master/720p60 MPEG-TS stream; and
- signed Twitch VOD playlists containing thousands of MPEG-TS segments.

The Twitch playlists are intentionally not committed: their signed URLs contain
short-lived tokens. Real segment media used for regressions lives separately in
local integration fixtures.

## Future work

Remaining HLS format work includes discontinuity handling, byte-range segments,
`#EXT-X-MAP`/fMP4, encryption/key handling and live playlist reload.

MIT.
