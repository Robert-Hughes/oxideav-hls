//! Minimal HLS VOD source for OxideAV.
//!
//! V1 deliberately targets the classic MPEG-TS HLS shape used by the
//! Real-world HLS VOD validation:
//! * HTTP(S) master or media playlists;
//! * VOD (`#EXT-X-ENDLIST`) only;
//! * one fixed rendition selected at open time;
//! * ordinary whole-file MPEG-TS segments;
//! * no encryption, byte ranges, init maps, discontinuities, or live reloads.
//!
//! `m3u8-rs` parses the playlists; `oxideav-http` performs every HTTP transfer.
//! The source exposes already-demuxed packets while internally owning one
//! MPEG-TS demuxer for the active media segment. `#EXTINF` durations provide a
//! media-time index, so seeking jumps directly to one segment instead of byte-
//! bisecting a virtual concatenation of every object in a long VOD.

use std::collections::{HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};

use m3u8_rs::{
    AlternativeMediaType, KeyMethod, MasterPlaylist, MediaPlaylist, Playlist, VariantStream,
};
use oxideav_core::{
    BytesSource, Demuxer, Error, NullCodecResolver, Packet, PacketSource, ReadSeek, Result,
    RuntimeContext, StreamInfo,
};
use url::Url;

const DEFAULT_MAX_HEIGHT: u64 = 720;
const MAX_PLAYLIST_BYTES: u64 = 4 * 1024 * 1024;

/// One non-I-frame rendition advertised by an HLS master playlist.
///
/// `url` is already resolved against the master URL, so callers that choose a
/// fixed rendition can pass it straight back to the HLS source without
/// fetching the master again. The remaining fields mirror useful
/// `#EXT-X-STREAM-INF` metadata while keeping `m3u8-rs` types out of the public
/// OxideAV API.
#[derive(Clone, Debug, PartialEq)]
pub struct HlsVariant {
    pub url: Url,
    pub bandwidth: u64,
    pub average_bandwidth: Option<u64>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub frame_rate: Option<f64>,
    pub codecs: Option<String>,
    pub video_group: Option<String>,
    pub audio_group: Option<String>,
    pub name: Option<String>,
}

/// Result of fetching and parsing one HLS playlist for discovery.
#[derive(Clone, Debug, PartialEq)]
pub enum HlsPlaylistInfo {
    /// The supplied URL was already a media playlist.
    Media { url: Url },
    /// The supplied URL was a master playlist. `preferred_variant` is the
    /// rendition selected by the same fixed-rendition policy used by
    /// [`open_hls`] when it is handed the master directly.
    Master {
        variants: Vec<HlsVariant>,
        preferred_variant: usize,
    },
}

/// Fetch and inspect an HLS master/media playlist without opening any media
/// rendition or segment.
///
/// For a master this performs exactly one bounded GET of the master playlist.
/// Returned variant URLs are absolute, allowing a consumer to choose one and
/// subsequently open that media playlist directly.
pub fn inspect_hls(uri: &str) -> Result<HlsPlaylistInfo> {
    let playlist_url = unwrap_hls_uri(uri)?;
    match fetch_playlist(&playlist_url)? {
        Playlist::MediaPlaylist(_) => {
            eprintln!("oxideav-hls: inspected media playlist: {playlist_url}");
            Ok(HlsPlaylistInfo::Media { url: playlist_url })
        }
        Playlist::MasterPlaylist(master) => {
            eprintln!(
                "oxideav-hls: inspected master playlist: {} variants: {playlist_url}",
                master
                    .variants
                    .iter()
                    .filter(|variant| !variant.is_i_frame)
                    .count()
            );
            inspect_master(&playlist_url, &master)
        }
    }
}

pub fn register(ctx: &mut RuntimeContext) {
    ctx.sources.register_packets("hls+http", open_hls);
    ctx.sources.register_packets("hls+https", open_hls);
}

oxideav_core::register!("source", register);

pub fn open_hls(uri: &str) -> Result<Box<dyn PacketSource>> {
    let playlist_url = unwrap_hls_uri(uri)?;
    let (media_url, media) = resolve_media_playlist(&playlist_url)?;
    validate_media_playlist(&media)?;
    HlsPacketSource::open(&media_url, &media)
        .map(|source| Box::new(source) as Box<dyn PacketSource>)
}

fn unwrap_hls_uri(uri: &str) -> Result<Url> {
    let raw = uri
        .strip_prefix("hls+https://")
        .map(|rest| format!("https://{rest}"))
        .or_else(|| {
            uri.strip_prefix("hls+http://")
                .map(|rest| format!("http://{rest}"))
        })
        .ok_or_else(|| Error::invalid(format!("hls: expected hls+http(s) URI, got {uri:?}")))?;
    let url =
        Url::parse(&raw).map_err(|e| Error::invalid(format!("hls: invalid playlist URL: {e}")))?;
    require_http(&url, "playlist")?;
    Ok(url)
}

fn fetch_playlist(url: &Url) -> Result<Playlist> {
    // Playlists are small metadata resources and some real HLS origins
    // (notably Twitch Usher) deliberately reject HEAD while accepting GET.
    // Use oxideav-http's bounded GET helper rather than its seekable
    // HEAD+Range media-source primitive.
    let bytes = oxideav_http::fetch_bytes(url.as_str(), MAX_PLAYLIST_BYTES)?;
    let (_, playlist) = m3u8_rs::parse_playlist(&bytes)
        .map_err(|e| Error::invalid(format!("hls: failed to parse playlist {url}: {e:?}")))?;
    Ok(playlist)
}

fn inspect_master(master_url: &Url, master: &MasterPlaylist) -> Result<HlsPlaylistInfo> {
    let preferred_uri = select_variant(&master.variants)?.uri.clone();
    let preferred_url = master_url.join(&preferred_uri).map_err(|error| {
        Error::invalid(format!(
            "hls: invalid preferred variant URI {preferred_uri:?}: {error}"
        ))
    })?;
    let variants = inspect_variants(master_url, master)?;
    let preferred_variant = variants
        .iter()
        .position(|variant| variant.url == preferred_url)
        .ok_or_else(|| {
            Error::invalid("hls: preferred variant was not present in inspected master")
        })?;
    Ok(HlsPlaylistInfo::Master {
        variants,
        preferred_variant,
    })
}

fn inspect_variants(master_url: &Url, master: &MasterPlaylist) -> Result<Vec<HlsVariant>> {
    let mut variants = Vec::new();
    for variant in master.variants.iter().filter(|variant| !variant.is_i_frame) {
        let url = master_url.join(&variant.uri).map_err(|error| {
            Error::invalid(format!(
                "hls: invalid variant URI {:?}: {error}",
                variant.uri
            ))
        })?;
        require_http(&url, "variant playlist")?;
        let name = variant.video.as_deref().and_then(|group_id| {
            master
                .alternatives
                .iter()
                .find(|alternative| {
                    alternative.media_type == AlternativeMediaType::Video
                        && alternative.group_id == group_id
                })
                .map(|alternative| alternative.name.clone())
        });
        variants.push(HlsVariant {
            url,
            bandwidth: variant.bandwidth,
            average_bandwidth: variant.average_bandwidth,
            width: variant.resolution.map(|resolution| resolution.width),
            height: variant.resolution.map(|resolution| resolution.height),
            frame_rate: variant.frame_rate,
            codecs: variant.codecs.clone(),
            video_group: variant.video.clone(),
            audio_group: variant.audio.clone(),
            name,
        });
    }
    if variants.is_empty() {
        return Err(Error::invalid(
            "hls: master playlist has no playable variants",
        ));
    }
    Ok(variants)
}

fn resolve_media_playlist(initial_url: &Url) -> Result<(Url, MediaPlaylist)> {
    match fetch_playlist(initial_url)? {
        Playlist::MediaPlaylist(media) => {
            eprintln!("oxideav-hls: media playlist: {initial_url}");
            Ok((initial_url.clone(), media))
        }
        Playlist::MasterPlaylist(master) => {
            let variant = select_variant(&master.variants)?;
            let media_url = initial_url.join(&variant.uri).map_err(|e| {
                Error::invalid(format!("hls: invalid variant URI {:?}: {e}", variant.uri))
            })?;
            require_http(&media_url, "variant playlist")?;
            let resolution = variant
                .resolution
                .map(|r| format!("{}x{}", r.width, r.height))
                .unwrap_or_else(|| "unknown resolution".to_string());
            eprintln!(
                "oxideav-hls: selected variant {resolution}, bandwidth {}: {media_url}",
                variant.bandwidth
            );
            match fetch_playlist(&media_url)? {
                Playlist::MediaPlaylist(media) => Ok((media_url, media)),
                Playlist::MasterPlaylist(_) => Err(Error::unsupported(
                    "hls: nested master playlists are not supported in V1",
                )),
            }
        }
    }
}

fn select_variant(variants: &[VariantStream]) -> Result<&VariantStream> {
    let candidates: Vec<&VariantStream> = variants.iter().filter(|v| !v.is_i_frame).collect();
    if candidates.is_empty() {
        return Err(Error::invalid(
            "hls: master playlist has no playable variants",
        ));
    }

    let max_height = std::env::var("OXIDEAV_HLS_MAX_HEIGHT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_HEIGHT);

    if let Some(best) = candidates
        .iter()
        .copied()
        .filter(|v| v.resolution.is_some_and(|r| r.height <= max_height))
        .max_by_key(|v| {
            let r = v.resolution.expect("filtered to variants with resolution");
            (r.height, r.width, v.bandwidth)
        })
    {
        return Ok(best);
    }

    // No variant carried a usable <=max-height resolution. Prefer the lowest
    // bandwidth rendition as a conservative fallback rather than unexpectedly
    // selecting a very large stream.
    candidates
        .into_iter()
        .min_by_key(|v| v.bandwidth)
        .ok_or_else(|| Error::invalid("hls: master playlist has no playable variants"))
}

fn validate_media_playlist(media: &MediaPlaylist) -> Result<()> {
    if !media.end_list {
        return Err(Error::unsupported(
            "hls: live/event playlist reloads are not supported in V1 (missing #EXT-X-ENDLIST)",
        ));
    }
    if media.i_frames_only {
        return Err(Error::unsupported(
            "hls: I-frames-only playlists are not supported",
        ));
    }
    if media.segments.is_empty() {
        return Err(Error::invalid("hls: media playlist contains no segments"));
    }
    Ok(())
}

fn validate_segment(idx: usize, segment: &m3u8_rs::MediaSegment) -> Result<()> {
    if segment.byte_range.is_some() {
        return Err(Error::unsupported(format!(
            "hls: segment {idx} uses #EXT-X-BYTERANGE; V1 supports whole-file segments only"
        )));
    }
    if segment.discontinuity {
        return Err(Error::unsupported(format!(
            "hls: segment {idx} begins a discontinuity; V1 supports one continuous MPEG-TS timeline"
        )));
    }
    if segment.map.is_some() {
        return Err(Error::unsupported(format!(
            "hls: segment {idx} uses #EXT-X-MAP (typically fMP4); V1 supports MPEG-TS segments only"
        )));
    }
    if let Some(key) = &segment.key {
        if key.method != KeyMethod::None {
            return Err(Error::unsupported(format!(
                "hls: encrypted segment {idx} ({}) is not supported in V1",
                key.method
            )));
        }
    }
    Ok(())
}

fn require_http(url: &Url, what: &str) -> Result<()> {
    if matches!(url.scheme(), "http" | "https") {
        Ok(())
    } else {
        Err(Error::unsupported(format!(
            "hls: {what} URL uses unsupported scheme {:?}; V1 supports HTTP(S) only",
            url.scheme()
        )))
    }
}

#[derive(Clone, Debug)]
struct SegmentEntry {
    url: Url,
    start_seconds: f64,
    duration_seconds: f64,
}

/// `Box<dyn BytesSource>` cannot be directly coerced to `Box<dyn ReadSeek>`
/// even though both trait bundles are `Read + Seek + Send`. This tiny shim
/// bridges the two public OxideAV abstractions without copying segment bytes.
struct SegmentReader(Box<dyn BytesSource>);

impl Read for SegmentReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for SegmentReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

struct HlsPacketSource {
    segments: Vec<SegmentEntry>,
    total_duration_seconds: f64,
    streams: Vec<StreamInfo>,
    transport_origin_seconds: f64,
    current_segment: usize,
    current: Box<dyn Demuxer>,
    pending: VecDeque<Packet>,
}

impl HlsPacketSource {
    fn open(media_url: &Url, media: &MediaPlaylist) -> Result<Self> {
        let segments = build_segment_index(media_url, media)?;
        let total_duration_seconds = segments
            .last()
            .map(|segment| segment.start_seconds + segment.duration_seconds)
            .unwrap_or(0.0);
        let mut current = open_segment_demuxer(&segments[0].url)?;

        // MPEG-TS learns each stream's first PTS only as PES packets flow. Cache
        // a small initial prefix so the PacketSource can advertise trustworthy
        // start times before the pipeline snapshots StreamInfo, then replay that
        // prefix unchanged through next_packet().
        let stream_count = current.streams().len();
        let mut pending = VecDeque::new();
        let mut seen_timestamped = HashSet::new();
        const INITIAL_TIMESTAMP_PACKETS_MAX: usize = 256;
        while seen_timestamped.len() < stream_count && pending.len() < INITIAL_TIMESTAMP_PACKETS_MAX
        {
            match current.next_packet() {
                Ok(packet) => {
                    if packet.pts.is_some() {
                        seen_timestamped.insert(packet.stream_index);
                    }
                    pending.push_back(packet);
                }
                Err(Error::Eof) => break,
                Err(error) => return Err(error),
            }
        }

        let mut streams = current.streams().to_vec();
        let transport_origin_seconds = streams
            .iter()
            .filter_map(|stream| {
                stream
                    .start_time
                    .map(|pts| stream.time_base.seconds_of(pts))
            })
            .filter(|seconds| seconds.is_finite())
            .min_by(f64::total_cmp)
            .or_else(|| {
                pending
                    .iter()
                    .filter_map(|packet| packet.pts.map(|pts| packet.time_base.seconds_of(pts)))
                    .filter(|seconds| seconds.is_finite())
                    .min_by(f64::total_cmp)
            })
            .ok_or_else(|| {
                Error::invalid("hls: first MPEG-TS segment exposed no timestamped packets")
            })?;

        for stream in &mut streams {
            let tick_seconds = stream.time_base.as_rational().as_f64();
            if tick_seconds.is_finite() && tick_seconds > 0.0 {
                stream.duration = Some((total_duration_seconds / tick_seconds).round() as i64);
            }
        }

        eprintln!(
            "oxideav-hls: VOD ready: {} segments duration={:.3}s transport_origin={:.3}s",
            segments.len(),
            total_duration_seconds,
            transport_origin_seconds,
        );

        Ok(Self {
            segments,
            total_duration_seconds,
            streams,
            transport_origin_seconds,
            current_segment: 0,
            current,
            pending,
        })
    }

    fn open_segment(&self, index: usize) -> Result<Box<dyn Demuxer>> {
        open_segment_demuxer(&self.segments[index].url)
    }

    fn install_segment(&mut self, index: usize, demuxer: Box<dyn Demuxer>) -> Result<()> {
        validate_stream_layout(&self.streams, demuxer.streams(), index)?;
        self.current_segment = index;
        self.current = demuxer;
        self.pending.clear();
        Ok(())
    }

    fn segment_for_media_seconds(&self, seconds: f64) -> usize {
        segment_index_for_media_seconds(&self.segments, seconds)
    }

    fn seek_in_segment(
        &self,
        index: usize,
        stream_index: u32,
        pts: i64,
    ) -> Result<(Box<dyn Demuxer>, i64)> {
        let mut demuxer = self.open_segment(index)?;
        validate_stream_layout(&self.streams, demuxer.streams(), index)?;
        let landed = demuxer.seek_to(stream_index, pts)?;
        Ok((demuxer, landed))
    }
}

impl PacketSource for HlsPacketSource {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if let Some(packet) = self.pending.pop_front() {
            return Ok(packet);
        }
        loop {
            match self.current.next_packet() {
                Ok(packet) => return Ok(packet),
                Err(Error::Eof) => {
                    let next = self.current_segment + 1;
                    if next >= self.segments.len() {
                        return Err(Error::Eof);
                    }
                    let demuxer = self.open_segment(next)?;
                    self.install_segment(next, demuxer)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        let requested_stream = self
            .streams
            .iter()
            .find(|stream| stream.index == stream_index)
            .ok_or_else(|| Error::invalid(format!("hls: no stream with index {stream_index}")))?;
        let time_base = requested_stream.time_base;
        // For A/V HLS, landing must be video-decode-safe even when the
        // application addressed the audio route (sink-facing and source stream
        // indices need not have the same ordering). MPEG-TS uses the same
        // 90 kHz time base for every elementary stream, so the target PTS is
        // unchanged when we prefer the first video stream for access-point
        // selection. Audio-only renditions keep the requested stream.
        let seek_stream_index = self
            .streams
            .iter()
            .find(|stream| stream.params.media_type == oxideav_core::MediaType::Video)
            .map(|stream| stream.index)
            .unwrap_or(stream_index);
        let raw_seconds = time_base.seconds_of(pts);
        if !raw_seconds.is_finite() {
            return Err(Error::invalid("hls: seek target is not a finite timestamp"));
        }
        let media_seconds = (raw_seconds - self.transport_origin_seconds)
            .clamp(0.0, self.total_duration_seconds.max(0.0));
        let index = self.segment_for_media_seconds(media_seconds);

        eprintln!(
            "oxideav-hls: seek target_raw={raw_seconds:.3}s media={media_seconds:.3}s segment={index} segment_start={:.3}s",
            self.segments[index].start_seconds,
        );

        let (mut demuxer, mut landed) = self.seek_in_segment(index, seek_stream_index, pts)?;
        let mut landed_index = index;

        // EXTINF timing is nominal and can differ by a few ticks from the TS
        // access-point timeline. If the selected segment can only clamp upward
        // past the requested PTS, seek the preceding segment instead so the
        // public "nearest decode-safe point at or before target" contract holds.
        if landed > pts && index > 0 {
            let previous = index - 1;
            let result = self.seek_in_segment(previous, seek_stream_index, pts)?;
            demuxer = result.0;
            landed = result.1;
            landed_index = previous;
        }

        self.install_segment(landed_index, demuxer)?;
        eprintln!(
            "oxideav-hls: seek landed segment={} raw={:.3}s media={:.3}s",
            landed_index,
            time_base.seconds_of(landed),
            time_base.seconds_of(landed) - self.transport_origin_seconds,
        );
        Ok(landed)
    }

    fn duration_micros(&self) -> Option<i64> {
        Some((self.total_duration_seconds * 1_000_000.0).round() as i64)
    }
}

fn build_segment_index(media_url: &Url, media: &MediaPlaylist) -> Result<Vec<SegmentEntry>> {
    let mut segments = Vec::with_capacity(media.segments.len());
    let mut start_seconds = 0.0_f64;
    for (index, segment) in media.segments.iter().enumerate() {
        validate_segment(index, segment)?;
        let url = media_url.join(&segment.uri).map_err(|error| {
            Error::invalid(format!(
                "hls: invalid segment URI {:?}: {error}",
                segment.uri
            ))
        })?;
        require_http(&url, "segment")?;
        let duration_seconds = f64::from(segment.duration);
        if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
            return Err(Error::invalid(format!(
                "hls: segment {index} has invalid EXTINF duration {}",
                segment.duration
            )));
        }
        segments.push(SegmentEntry {
            url,
            start_seconds,
            duration_seconds,
        });
        start_seconds += duration_seconds;
    }
    Ok(segments)
}

fn segment_index_for_media_seconds(segments: &[SegmentEntry], seconds: f64) -> usize {
    debug_assert!(!segments.is_empty());
    let seconds = seconds.max(0.0);
    let insertion = segments.partition_point(|segment| segment.start_seconds <= seconds);
    insertion.saturating_sub(1).min(segments.len() - 1)
}

fn open_segment_demuxer(url: &Url) -> Result<Box<dyn Demuxer>> {
    let bytes = oxideav_http::open_http(url.as_str())?;
    let input: Box<dyn ReadSeek> = Box::new(SegmentReader(bytes));
    oxideav_mpegts::open_demuxer(input, &NullCodecResolver)
}

fn validate_stream_layout(
    expected: &[StreamInfo],
    actual: &[StreamInfo],
    segment: usize,
) -> Result<()> {
    if expected.len() != actual.len() {
        return Err(Error::invalid(format!(
            "hls: MPEG-TS stream count changed at segment {segment}: expected {}, got {}",
            expected.len(),
            actual.len()
        )));
    }
    for (expected, actual) in expected.iter().zip(actual) {
        if expected.index != actual.index
            || expected.params.media_type != actual.params.media_type
            || expected.params.codec_id != actual.params.codec_id
        {
            return Err(Error::invalid(format!(
                "hls: MPEG-TS stream layout changed at segment {segment}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use m3u8_rs::{AlternativeMedia, MasterPlaylist, Resolution};

    #[test]
    fn selects_highest_variant_at_or_below_default_height() {
        let variants = vec![
            VariantStream {
                uri: "480.m3u8".into(),
                bandwidth: 1_000,
                resolution: Some(Resolution {
                    width: 854,
                    height: 480,
                }),
                ..Default::default()
            },
            VariantStream {
                uri: "720.m3u8".into(),
                bandwidth: 2_000,
                resolution: Some(Resolution {
                    width: 1280,
                    height: 720,
                }),
                ..Default::default()
            },
            VariantStream {
                uri: "1080.m3u8".into(),
                bandwidth: 5_000,
                resolution: Some(Resolution {
                    width: 1920,
                    height: 1080,
                }),
                ..Default::default()
            },
        ];
        let master = MasterPlaylist {
            variants,
            ..Default::default()
        };
        std::env::remove_var("OXIDEAV_HLS_MAX_HEIGHT");
        assert_eq!(select_variant(&master.variants).unwrap().uri, "720.m3u8");
    }

    #[test]
    fn inspection_resolves_variant_urls_names_and_preferred_rendition() {
        let master_url = Url::parse("https://example.test/path/master.m3u8").unwrap();
        let master = MasterPlaylist {
            variants: vec![
                VariantStream {
                    uri: "1080.m3u8".into(),
                    bandwidth: 5_000_000,
                    codecs: Some("avc1.64002a,mp4a.40.2".into()),
                    resolution: Some(Resolution {
                        width: 1920,
                        height: 1080,
                    }),
                    frame_rate: Some(60.0),
                    video: Some("source".into()),
                    ..Default::default()
                },
                VariantStream {
                    uri: "720.m3u8".into(),
                    bandwidth: 3_000_000,
                    resolution: Some(Resolution {
                        width: 1280,
                        height: 720,
                    }),
                    frame_rate: Some(60.0),
                    video: Some("720p60".into()),
                    ..Default::default()
                },
                VariantStream {
                    uri: "iframe.m3u8".into(),
                    bandwidth: 500_000,
                    is_i_frame: true,
                    ..Default::default()
                },
            ],
            alternatives: vec![
                AlternativeMedia {
                    media_type: AlternativeMediaType::Video,
                    group_id: "source".into(),
                    name: "1080p60".into(),
                    ..Default::default()
                },
                AlternativeMedia {
                    media_type: AlternativeMediaType::Video,
                    group_id: "720p60".into(),
                    name: "720p60".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let HlsPlaylistInfo::Master {
            variants,
            preferred_variant,
        } = inspect_master(&master_url, &master).unwrap()
        else {
            panic!("expected master inspection");
        };

        assert_eq!(variants.len(), 2);
        assert_eq!(
            variants[0].url.as_str(),
            "https://example.test/path/1080.m3u8"
        );
        assert_eq!(variants[0].name.as_deref(), Some("1080p60"));
        assert_eq!(variants[0].width, Some(1920));
        assert_eq!(variants[0].height, Some(1080));
        assert_eq!(variants[0].frame_rate, Some(60.0));
        assert_eq!(variants[0].bandwidth, 5_000_000);
        assert_eq!(preferred_variant, 1);
    }

    #[test]
    fn extinf_index_maps_media_time_without_segment_byte_lengths() {
        let media_url = Url::parse("https://example.test/vod/index.m3u8").unwrap();
        let media = MediaPlaylist {
            end_list: true,
            segments: vec![
                m3u8_rs::MediaSegment {
                    uri: "0.ts".into(),
                    duration: 10.0,
                    ..Default::default()
                },
                m3u8_rs::MediaSegment {
                    uri: "1.ts".into(),
                    duration: 10.5,
                    ..Default::default()
                },
                m3u8_rs::MediaSegment {
                    uri: "2.ts".into(),
                    duration: 9.5,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let segments = build_segment_index(&media_url, &media).unwrap();
        assert_eq!(segments[0].start_seconds, 0.0);
        assert_eq!(segments[1].start_seconds, 10.0);
        assert_eq!(segments[2].start_seconds, 20.5);
        assert_eq!(segment_index_for_media_seconds(&segments, 0.0), 0);
        assert_eq!(segment_index_for_media_seconds(&segments, 9.999), 0);
        assert_eq!(segment_index_for_media_seconds(&segments, 10.0), 1);
        assert_eq!(segment_index_for_media_seconds(&segments, 20.49), 1);
        assert_eq!(segment_index_for_media_seconds(&segments, 29.9), 2);
        assert_eq!(segments[2].url.as_str(), "https://example.test/vod/2.ts");
    }

    #[test]
    fn hls_scheme_unwraps_to_http() {
        assert_eq!(
            unwrap_hls_uri("hls+https://example.test/master.m3u8")
                .unwrap()
                .as_str(),
            "https://example.test/master.m3u8"
        );
    }
}
