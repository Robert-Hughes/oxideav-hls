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
//! The segment resources are exposed as one seekable logical byte source so the
//! existing OxideAV MPEG-TS demuxer needs no HLS-specific knowledge. Segment byte
//! lengths are learned lazily as playback reaches them so multi-hour VODs do not
//! require thousands of HTTP metadata requests before the first frame.

use std::io::{self, Read, Seek, SeekFrom};

use m3u8_rs::{
    AlternativeMediaType, KeyMethod, MasterPlaylist, MediaPlaylist, Playlist, VariantStream,
};
use oxideav_core::{BytesSource, Error, Result, RuntimeContext};
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
    ctx.sources.register_bytes("hls+http", open_hls);
    ctx.sources.register_bytes("hls+https", open_hls);
}

oxideav_core::register!("source", register);

pub fn open_hls(uri: &str) -> Result<Box<dyn BytesSource>> {
    let playlist_url = unwrap_hls_uri(uri)?;
    let (media_url, media) = resolve_media_playlist(&playlist_url)?;
    validate_media_playlist(&media)?;

    let mut segment_urls = Vec::with_capacity(media.segments.len());
    for (idx, segment) in media.segments.iter().enumerate() {
        validate_segment(idx, segment)?;
        let segment_url = media_url.join(&segment.uri).map_err(|e| {
            Error::invalid(format!("hls: invalid segment URI {:?}: {e}", segment.uri))
        })?;
        require_http(&segment_url, "segment")?;
        segment_urls.push(segment_url);
    }

    eprintln!(
        "oxideav-hls: VOD ready: {} segments (lazy HTTP segment indexing)",
        segment_urls.len()
    );

    Ok(Box::new(HlsSource::new(segment_urls)))
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

struct HlsSource {
    /// Resolved segment URLs in playlist order. No segment HTTP source is
    /// opened merely by constructing the HLS source.
    segment_urls: Vec<Url>,
    /// Prefix byte offsets for segments whose lengths have been learned.
    /// `starts[0] == 0`; when `starts.len() == N + 1`, segment N-1 is the
    /// last indexed segment and `starts[N]` is the known-prefix byte length.
    starts: Vec<u64>,
    /// One live HTTP source, normally the segment currently being read.
    /// Keeping only one avoids thousands of idle HTTP-source objects on long
    /// Twitch VODs while still allowing an indexed segment to be reopened for
    /// a backward seek.
    current: Option<(usize, Box<dyn BytesSource>)>,
    /// Absolute byte position in the virtual concatenated MPEG-TS stream.
    pos: u64,
}

impl HlsSource {
    fn new(segment_urls: Vec<Url>) -> Self {
        Self {
            segment_urls,
            starts: vec![0],
            current: None,
            pos: 0,
        }
    }

    fn indexed_count(&self) -> usize {
        self.starts.len() - 1
    }

    fn known_end(&self) -> u64 {
        *self.starts.last().expect("HLS starts always contains zero")
    }

    fn fully_indexed(&self) -> bool {
        self.indexed_count() == self.segment_urls.len()
    }

    fn segment_for_known_pos(&self, pos: u64) -> Option<usize> {
        if pos >= self.known_end() {
            return None;
        }
        (0..self.indexed_count()).find(|&i| pos >= self.starts[i] && pos < self.starts[i + 1])
    }

    fn open_segment(&self, idx: usize) -> io::Result<Box<dyn BytesSource>> {
        oxideav_http::open_http(self.segment_urls[idx].as_str()).map_err(io::Error::other)
    }

    /// Learn one more segment's byte length. The returned source is rewound
    /// and can be retained by the caller when this is the segment it wants to
    /// read, avoiding a second HEAD/open operation.
    fn index_next(&mut self) -> io::Result<(usize, Box<dyn BytesSource>)> {
        let idx = self.indexed_count();
        if idx >= self.segment_urls.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "hls: no segment left to index",
            ));
        }
        let mut source = self.open_segment(idx)?;
        let len = source.seek(SeekFrom::End(0))?;
        source.seek(SeekFrom::Start(0))?;
        let end = self.known_end().checked_add(len).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "hls: concatenated segment length overflows u64",
            )
        })?;
        self.starts.push(end);
        Ok((idx, source))
    }

    /// Ensure the segment containing `self.pos` has a known byte range and a
    /// live HTTP source. Seeking far forward can therefore index intervening
    /// segments on demand, while normal sequential playback indexes exactly
    /// one new segment at each boundary.
    fn ensure_current_for_pos(&mut self) -> io::Result<Option<usize>> {
        loop {
            if let Some(idx) = self.segment_for_known_pos(self.pos) {
                if self.current.as_ref().map(|(i, _)| *i) != Some(idx) {
                    let source = self.open_segment(idx)?;
                    self.current = Some((idx, source));
                }
                return Ok(Some(idx));
            }

            if self.fully_indexed() {
                return Ok(None);
            }

            let (idx, source) = self.index_next()?;
            let seg_end = self.starts[idx + 1];
            if self.pos < seg_end {
                self.current = Some((idx, source));
                return Ok(Some(idx));
            }
            // Zero-length segment, or a forward seek beyond this newly
            // indexed segment: discard its source and continue indexing.
        }
    }
}

impl Read for HlsSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let Some(idx) = self.ensure_current_for_pos()? else {
            return Ok(0);
        };

        let segment_start = self.starts[idx];
        let segment_end = self.starts[idx + 1];
        let intra = self.pos - segment_start;
        let want = buf.len().min((segment_end - self.pos) as usize);
        let (_, part) = self
            .current
            .as_mut()
            .expect("ensure_current_for_pos installed source");
        part.seek(SeekFrom::Start(intra))?;
        let n = part.read(&mut buf[..want])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HlsSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(delta) => add_signed(self.pos, delta)?,
            SeekFrom::End(delta) => {
                if !self.fully_indexed() {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "hls: end-relative seek requires all segment lengths; VOD is lazily indexed",
                    ));
                }
                add_signed(self.known_end(), delta)?
            }
        };
        self.pos = target;
        Ok(target)
    }
}

fn add_signed(base: u64, delta: i64) -> io::Result<u64> {
    base.checked_add_signed(delta).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "hls: seek resolves before byte zero or overflows",
        )
    })
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
    fn lazy_source_rejects_end_seek_before_segment_lengths_are_known() {
        let urls = vec![
            Url::parse("https://example.test/0.ts").unwrap(),
            Url::parse("https://example.test/1.ts").unwrap(),
        ];
        let mut src = HlsSource::new(urls);
        assert_eq!(src.seek(SeekFrom::Start(123)).unwrap(), 123);
        assert_eq!(src.seek(SeekFrom::Current(-23)).unwrap(), 100);
        let err = src.seek(SeekFrom::End(0)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
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
