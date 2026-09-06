# oxideav-hls

Minimal HLS VOD byte-source integration for the OxideAV framework.

The crate deliberately keeps HLS as a **source-layer concern**. It resolves a
master/media playlist and exposes its MPEG-TS segments as one logical seekable
byte stream; the existing `oxideav-mpegts` demuxer and codec stack remain
unaware of HLS.

## Current scope

The first implementation targets the classic VOD shape used by the
Real-world HLS VOD validation:

- HTTP(S) master or media playlists;
- completed VOD/event playlists carrying `#EXT-X-ENDLIST`;
- one fixed rendition selected when the source opens;
- whole-file MPEG-TS segments;
- relative playlist/segment URI resolution;
- no encryption;
- no byte ranges;
- no `#EXT-X-MAP` / fMP4;
- no discontinuities;
- no I-frames-only playlists;
- no nested master playlists;
- no live playlist reload or adaptive bitrate switching yet.

Unsupported shapes fail explicitly rather than being partially interpreted.

## URIs and registration

`register()` installs byte sources for:

```text
hls+http://...
hls+https://...
```

The extra `hls+` prefix makes the source selection explicit while the crate is
still a deliberately narrow HLS implementation. The underlying playlist and
segment transfers are delegated to `oxideav-http`.

## Rendition selection

A master playlist chooses the highest-resolution non-I-frame rendition whose
height is at most 720 by default. Set `OXIDEAV_HLS_MAX_HEIGHT` to a positive
integer to change that ceiling. If the master contains no usable rendition at
or below the ceiling, the lowest-bandwidth playable rendition is used as a
conservative fallback.

This is fixed-rendition selection, not ABR. A future adaptive controller should
be a separate policy layer rather than hidden inside the byte-source reader.

## Why playlist fetches use normal GET

`oxideav-http::open_http()` is a seekable media source and normally probes the
origin with HEAD/range semantics. Real HLS origins do not universally support
that shape: Twitch's signed Usher playlist URLs, for example, accepted GET while
returning 404 to HEAD during the validation that motivated this crate.

Playlists are therefore fetched through `oxideav_http::fetch_bytes()`: a normal
GET with a hard byte limit and the same redirect/content-encoding policy as the
HTTP driver. Playlist metadata is capped at 4 MiB.

## Lazy segment indexing

A multi-hour Twitch VOD can contain thousands of MPEG-TS segments. Opening every
segment merely to discover its byte length would turn startup into thousands of
HTTP requests before the first frame.

`HlsSource` therefore indexes segment lengths **lazily**:

```text
playlist URLs
    ↓
open/index segment 0 only when downstream reads reach it
    ↓
learn its byte length and append one prefix offset
    ↓
continue into segment 1, 2, ... as downstream demand advances
```

Only one HTTP segment source is normally kept open. Already indexed segments can
be reopened for backward seeks.

There is intentionally no separate HLS background prefetcher at this layer.
OxideAV's downstream packet/frame queues naturally pull data ahead of immediate
presentation, which in turn opens later HLS segments early. That keeps read-ahead
policy centralised instead of stacking independent buffering schemes at every
layer; explicit HLS prefetch should only be added if real network-jitter evidence
shows the shared pipeline buffering is insufficient.

## Seeking limitations

The current source models the media as a virtual concatenation of segment
**bytes**, not yet as an HLS media timeline. Consequently:

- byte seeking works within the prefix of segments whose lengths have been
  learned;
- `SeekFrom::End` requires every segment length and is refused until the source
  is fully indexed;
- player-relative time seeking should eventually use playlist segment durations
  rather than forcing the generic byte source to discover the whole VOD;
- MPEG-TS transport timestamps still need a media-relative timeline/rebasing
  layer for user-facing positions.

These are known follow-ups, not reasons to make startup eager again.

## Validation

The implementation has been exercised against:

- the public Big Buck Bunny HLS master/720p60 MPEG-TS stream; and
- a signed Twitch VOD playlist with 4,275 MPEG-TS segments and roughly 11 h 54 m
  duration.

The Twitch playlist is intentionally not committed: its signed URLs contain
short-lived tokens. Real segment media used for regression lives separately in
local integration fixtures.

## Future work

Likely next steps are media-timeline seeking, discontinuity handling, byte-range
segments, `#EXT-X-MAP`/fMP4, encryption/key handling, and live reload/ABR.

MIT.
