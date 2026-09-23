//! RFC 8216 section 3.4 packed audio. HLS owns the ID3 transport anchor;
//! ADTS framing supplies sample counts, while decoding remains downstream.
//! MP3/AC-3/E-AC-3 are deliberately rejected until framing is implemented.

mod id3;
#[cfg(test)]
pub(crate) mod tests;

use oxideav_aac::adts::{AdtsHeader, ADTS_HEADER_BYTES_NO_CRC, ADTS_HEADER_BYTES_WITH_CRC};
use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Packet, ReadSeek, Result, StreamInfo, TimeBase,
};
use std::io::SeekFrom;
use std::sync::Mutex;

const CLOCK: TimeBase = TimeBase::new(1, 90_000);
const WRAP: i64 = 1 << 33;

/// Use playlist position to unwrap the 33-bit transport clock. Anchoring once
/// makes reopening/seeking deterministic, even when readahead opens out of order.
#[derive(Default)]
pub(crate) struct Timeline(Mutex<Option<(i64, f64, AdtsConfig)>>);

impl Timeline {
    fn unwrap(&self, raw: i64, start_seconds: f64, config: AdtsConfig) -> Result<i64> {
        let mut anchor = self
            .0
            .lock()
            .map_err(|_| Error::other("hls: packed audio timeline poisoned"))?;
        let (origin, first_start, first_config) =
            *anchor.get_or_insert((raw, start_seconds, config));
        if config != first_config {
            return Err(Error::unsupported(
                "hls: ADTS configuration changed between segments",
            ));
        }
        let expected = origin as i128 + ((start_seconds - first_start) * 90_000.0).round() as i128;
        let turns = (expected - raw as i128 + (WRAP / 2) as i128).div_euclid(WRAP as i128);
        i64::try_from(raw as i128 + turns * WRAP as i128)
            .map_err(|_| Error::invalid("hls: packed audio timestamp overflow"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AdtsConfig {
    mpeg2: bool,
    profile: u8,
    sample_rate: u32,
    channels: u8,
}

/// Read an exact prefix, distinguishing clean EOF from a truncated frame/tag.
fn prefix(input: &mut dyn ReadSeek) -> Result<Option<[u8; 3]>> {
    let mut p = [0; 3];
    if input.read(&mut p[..1])? == 0 {
        return Ok(None);
    }
    input.read_exact(&mut p[1..])?;
    Ok(Some(p))
}

fn read_frame(input: &mut dyn ReadSeek, p: [u8; 3]) -> Result<(AdtsConfig, u64, Vec<u8>)> {
    if p[0] != 0xff || p[1] & 0xf6 != 0xf0 {
        return Err(Error::unsupported(
            "hls: packed audio payload is not ADTS AAC (MP3/AC-3/E-AC-3 are not implemented)",
        ));
    }
    let mut h = [0; ADTS_HEADER_BYTES_WITH_CRC];
    h[..3].copy_from_slice(&p);
    input.read_exact(&mut h[3..ADTS_HEADER_BYTES_NO_CRC])?;
    // The AAC parser requires the CRC bytes when protection_absent is false.
    // Read only the transport header here; field validation belongs to AAC.
    let header_len = if h[1] & 1 == 0 {
        input.read_exact(&mut h[ADTS_HEADER_BYTES_NO_CRC..])?;
        ADTS_HEADER_BYTES_WITH_CRC
    } else {
        ADTS_HEADER_BYTES_NO_CRC
    };
    let (header, _) = AdtsHeader::parse(&h[..header_len])
        .map_err(|e| Error::invalid(format!("hls: invalid ADTS header: {e}")))?;
    let config = AdtsConfig {
        mpeg2: header.mpeg_version_mpeg2,
        profile: header.profile,
        sample_rate: header.sample_rate(),
        channels: header.channel_configuration,
    };
    let length = usize::from(header.aac_frame_length);
    if length <= header_len {
        return Err(Error::invalid("hls: invalid ADTS frame length"));
    }
    let mut bytes = vec![0; length];
    bytes[..header_len].copy_from_slice(&h[..header_len]);
    input.read_exact(&mut bytes[header_len..])?;
    // Preserve ADTS (including CRC and multi-block framing) for the AAC decoder.
    Ok((
        config,
        1024 * u64::from(header.number_of_raw_data_blocks_in_frame),
        bytes,
    ))
}

pub(crate) fn open(
    mut input: Box<dyn ReadSeek>,
    timeline: &Timeline,
    start_seconds: f64,
) -> Result<Box<dyn Demuxer>> {
    let mut timestamp = None;
    let mut metadata_bytes = 0;
    let (audio_start, config) = loop {
        let position = input.stream_position()?;
        let p = prefix(&mut *input)?
            .ok_or_else(|| Error::invalid("hls: packed audio segment contains no audio"))?;
        if &p == b"ID3" {
            let tag = id3::read(&mut *input, &mut metadata_bytes)?;
            if let Some(pts) = tag {
                if timestamp.is_some_and(|previous| previous != pts) {
                    return Err(Error::invalid("hls: conflicting packed audio timestamps"));
                }
                timestamp = Some(pts);
            }
        } else {
            let (config, _, _) = read_frame(&mut *input, p)?;
            break (position, config);
        }
    };
    let timestamp = timestamp.ok_or_else(|| {
        Error::invalid("hls: packed audio is missing the ID3 transport timestamp")
    })?;
    let base_pts = timeline.unwrap(timestamp, start_seconds, config)?;
    input.seek(SeekFrom::Start(audio_start))?;
    let mut params = CodecParameters::audio(CodecId::new("aac"));
    params.sample_rate = Some(config.sample_rate);
    // Configuration 0 uses an in-band Program Config Element. Do not guess.
    params.channels = match config.channels {
        0 => None,
        7 => Some(8),
        n => Some(n.into()),
    };
    Ok(Box::new(PackedAac {
        input,
        audio_start,
        config,
        base_pts,
        samples: 0,
        metadata_bytes,
        streams: [StreamInfo {
            index: 0,
            time_base: CLOCK,
            duration: None,
            start_time: Some(base_pts),
            params,
        }],
    }))
}

struct PackedAac {
    input: Box<dyn ReadSeek>,
    audio_start: u64,
    config: AdtsConfig,
    base_pts: i64,
    samples: u64,
    metadata_bytes: usize,
    streams: [StreamInfo; 1],
}

impl PackedAac {
    fn pts(&self, samples: u64) -> Result<i64> {
        // Rescale cumulative samples, not each frame's duration: 44.1 kHz
        // otherwise accumulates rounding error on the 90 kHz transport clock.
        i64::try_from(
            self.base_pts as i128 + samples as i128 * 90_000 / self.config.sample_rate as i128,
        )
        .map_err(|_| Error::invalid("hls: packed audio timestamp overflow"))
    }
}

impl Demuxer for PackedAac {
    fn format_name(&self) -> &str {
        "hls-packed-aac"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn supports_seek(&self) -> bool {
        true
    }
    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            let p = prefix(&mut *self.input)?.ok_or(Error::Eof)?;
            if &p == b"ID3" {
                // Timed metadata is not AAC payload. The segment's initial
                // timestamp remains the anchor; frame sample counts advance it.
                id3::read(&mut *self.input, &mut self.metadata_bytes)?;
                continue;
            }
            let (config, samples, bytes) = read_frame(&mut *self.input, p)?;
            if config != self.config {
                return Err(Error::unsupported(
                    "hls: ADTS configuration changed within a segment",
                ));
            }
            let pts = self.pts(self.samples)?;
            self.samples = self
                .samples
                .checked_add(samples)
                .ok_or_else(|| Error::invalid("hls: AAC sample count overflow"))?;
            let end = self.pts(self.samples)?;
            return Ok(Packet::new(0, CLOCK, bytes)
                .with_pts(pts)
                .with_dts(pts)
                .with_duration(end - pts)
                .with_keyframe(true));
        }
    }

    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        if stream_index != 0 {
            return Err(Error::invalid("hls: no such packed audio stream"));
        }
        self.input.seek(SeekFrom::Start(self.audio_start))?;
        self.samples = 0;
        self.metadata_bytes = 0;
        let mut landing = (self.audio_start, 0, self.base_pts);
        loop {
            let position = self.input.stream_position()?;
            let samples = self.samples;
            match self.next_packet() {
                Ok(packet) => {
                    let at = packet.pts.expect("packed audio always has PTS");
                    if at > pts {
                        break;
                    }
                    landing = (position, samples, at);
                    if at == pts {
                        break;
                    }
                }
                Err(Error::Eof) => break,
                Err(error) => return Err(error),
            }
        }
        self.input.seek(SeekFrom::Start(landing.0))?;
        self.samples = landing.1;
        self.metadata_bytes = 0;
        Ok(landing.2)
    }
}
